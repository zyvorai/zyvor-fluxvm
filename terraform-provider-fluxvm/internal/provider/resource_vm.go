// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type vmResource struct{ clientResource }

func newVMResource() resource.Resource { return &vmResource{} }

var (
	_ resource.ResourceWithImportState    = (*vmResource)(nil)
	_ resource.ResourceWithValidateConfig = (*vmResource)(nil)
)

type vmModel struct {
	ID            types.String `tfsdk:"id"`
	VMID          types.String `tfsdk:"vm_id"`
	Name          types.String `tfsdk:"name"`
	CatalogImage  types.String `tfsdk:"catalog_image"`
	Image         types.String `tfsdk:"image"`
	GuestOS       types.String `tfsdk:"guest_os"`
	VCPUs         types.Int64  `tfsdk:"vcpus"`
	MemoryMiB     types.Int64  `tfsdk:"memory_mib"`
	DiskSizeGiB   types.Int64  `tfsdk:"disk_size_gib"`
	TTLSeconds    types.Int64  `tfsdk:"ttl_seconds"`
	Node          types.String `tfsdk:"node"`
	NodeSelector  types.Map    `tfsdk:"node_selector"`
	Networks      types.List   `tfsdk:"networks"`
	Volumes       types.List   `tfsdk:"volumes"`
	Reschedulable types.Bool   `tfsdk:"reschedulable"`
	Tenant        types.String `tfsdk:"tenant"`
	ExtraJSON     types.String `tfsdk:"extra_json"`
	PowerState    types.String `tfsdk:"power_state"`
	Status        types.String `tfsdk:"status"`
	CreatedAt     types.String `tfsdk:"created_at"`
}

// fixedIfConfigured: an optional+computed argument that keeps its value when unset and replaces the VM when changed.
func fixedIfConfigured() []planmodifier.String {
	return []planmodifier.String{stringplanmodifier.UseStateForUnknown(), stringplanmodifier.RequiresReplaceIfConfigured()}
}

func fixedIntIfConfigured() []planmodifier.Int64 {
	return []planmodifier.Int64{int64planmodifier.UseStateForUnknown(), int64planmodifier.RequiresReplaceIfConfigured()}
}

func (r *vmResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_vm"
}

func (r *vmResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	i64Replace := []planmodifier.Int64{int64planmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A VM placed by central. Virtualization.framework has no live reconfiguration, so changing what " +
			"the VM is (image, size, Mac, networks, volumes) replaces it; only `power_state` changes in place. A macOS " +
			"VM runs on a Mac the tenant leases: depend on a `fluxvm_lease`.",
		Attributes: map[string]schema.Attribute{
			"id":    schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "`<node>/<vm_id>`; the import id."},
			"vm_id": schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "The VM's UUID on its node."},
			"name":  schema.StringAttribute{Required: true, PlanModifiers: replace()},
			"catalog_image": schema.StringAttribute{
				Optional: true, PlanModifiers: replace(),
				Description: "An image catalog entry (`data.fluxvm_images`); fills `image`, `guest_os`, `vcpus`, `memory_mib` and `disk_size_gib` defaults.",
			},
			"image": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "The disk's path on the Mac. Set this or `catalog_image`.",
			},
			"guest_os": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "`macos` or `linux` (from the catalog entry when `catalog_image` is set).",
			},
			"vcpus":         schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: fixedIntIfConfigured()},
			"memory_mib":    schema.Int64Attribute{Optional: true, Computed: true, PlanModifiers: fixedIntIfConfigured()},
			"disk_size_gib": schema.Int64Attribute{Optional: true, PlanModifiers: i64Replace},
			"ttl_seconds": schema.Int64Attribute{
				Optional: true, PlanModifiers: i64Replace,
				Description: "The node deletes the VM after this long; Terraform then plans a new one.",
			},
			"node": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "Pin the VM to this Mac; otherwise central places it (and this shows where).",
			},
			"node_selector": schema.MapAttribute{
				ElementType: types.StringType, Optional: true,
				PlanModifiers: []planmodifier.Map{mapplanmodifier.RequiresReplace()},
			},
			"networks": schema.ListAttribute{
				ElementType: types.StringType, Optional: true,
				PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()},
				Description:   "Private networks (`fluxvm_network` names) the VM joins.",
			},
			"volumes": schema.ListAttribute{
				ElementType: types.StringType, Optional: true,
				PlanModifiers: []planmodifier.List{listplanmodifier.RequiresReplace()},
				Description:   "Volumes (`fluxvm_volume` names) attached at launch. Requires central with volumes.",
			},
			"reschedulable": schema.BoolAttribute{
				Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()},
				Description:   "Central keeps the spec so a drain can recreate the VM elsewhere. Leave off when `extra_json` carries secrets.",
			},
			"tenant": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "Operator only: create for this tenant. Shows the owning tenant.",
			},
			"extra_json": schema.StringAttribute{
				Optional: true, PlanModifiers: replace(),
				Description: "A JSON object of further create-request fields (cloud_init, agent, labels, apple options...). Named arguments win.",
			},
			"power_state": schema.StringAttribute{
				Optional: true, Computed: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
				Description:   "`running`, `stopped` or `paused`. Changing it starts, stops, pauses or resumes the VM in place.",
			},
			"status":     schema.StringAttribute{Computed: true, Description: "What the node reports: creating, running, paused, stopped or failed."},
			"created_at": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *vmResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m vmModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if m.CatalogImage.IsNull() && m.Image.IsNull() {
		resp.Diagnostics.AddAttributeError(path.Root("catalog_image"), "No image",
			"Set catalog_image (a catalog entry) or image (a disk path on the Mac) with guest_os.")
	}
	if m.CatalogImage.IsNull() && known(m.Image) && m.GuestOS.IsNull() {
		resp.Diagnostics.AddAttributeError(path.Root("guest_os"), "guest_os needed", "A VM from a disk path needs guest_os = \"macos\" or \"linux\".")
	}
	if known(m.GuestOS) && m.GuestOS.ValueString() != "macos" && m.GuestOS.ValueString() != "linux" {
		resp.Diagnostics.AddAttributeError(path.Root("guest_os"), "Invalid guest_os", "guest_os is \"macos\" or \"linux\".")
	}
	if known(m.PowerState) {
		switch m.PowerState.ValueString() {
		case "running", "stopped", "paused":
		default:
			resp.Diagnostics.AddAttributeError(path.Root("power_state"), "Invalid power_state", "power_state is running, stopped or paused.")
		}
	}
	if known(m.VCPUs) && (m.VCPUs.ValueInt64() < 1 || m.VCPUs.ValueInt64() > 255) {
		resp.Diagnostics.AddAttributeError(path.Root("vcpus"), "Invalid vcpus", "vcpus is 1-255.")
	}
	if known(m.MemoryMiB) && m.MemoryMiB.ValueInt64() < 512 {
		resp.Diagnostics.AddAttributeError(path.Root("memory_mib"), "Invalid memory_mib", "A VM needs at least 512 MiB.")
	}
	if known(m.ExtraJSON) {
		var obj map[string]interface{}
		if err := json.Unmarshal([]byte(m.ExtraJSON.ValueString()), &obj); err != nil {
			resp.Diagnostics.AddAttributeError(path.Root("extra_json"), "Invalid extra_json", "extra_json must be a JSON object: "+err.Error())
		}
	}
}

func powerOf(status string) string {
	switch status {
	case "running", "stopped", "paused":
		return status
	}
	return ""
}

func (m *vmModel) fill(v *fleet.VM) {
	m.ID = types.StringValue(v.Node + "/" + v.ID)
	m.VMID = types.StringValue(v.ID)
	m.Name = types.StringValue(v.Name)
	m.Node = types.StringValue(v.Node)
	if img := v.Image(); img != "" {
		m.Image = types.StringValue(img)
	}
	if g := v.GuestOS(); g != "" {
		m.GuestOS = types.StringValue(g)
	}
	if n := v.VCPUs(); n > 0 {
		m.VCPUs = types.Int64Value(n)
	}
	if n := v.MemoryMiB(); n > 0 {
		m.MemoryMiB = types.Int64Value(n)
	}
	if t, ok := v.Request["tenant"].(string); ok && t != "" {
		m.Tenant = types.StringValue(t)
	} else if m.Tenant.IsUnknown() {
		m.Tenant = types.StringNull()
	}
	m.Status = types.StringValue(v.Status)
	if p := powerOf(v.Status); p != "" {
		m.PowerState = types.StringValue(p)
	} else if m.PowerState.IsUnknown() {
		m.PowerState = types.StringNull()
	}
	m.CreatedAt = timeValue(v.CreatedAt)
}

func (r *vmResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan vmModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	body := fleet.LaunchRequest{
		Name:          plan.Name.ValueString(),
		Backend:       "vz",
		Image:         optString(plan.Image),
		Node:          optString(plan.Node),
		NodeSelector:  stringMap(ctx, plan.NodeSelector, &resp.Diagnostics),
		Volumes:       stringList(ctx, plan.Volumes, &resp.Diagnostics),
		Reschedulable: plan.Reschedulable.ValueBool(),
		Tenant:        optString(plan.Tenant),
		DiskSizeGiB:   u64Ptr(plan.DiskSizeGiB),
		TTLSeconds:    u64Ptr(plan.TTLSeconds),
	}
	guest := optString(plan.GuestOS)
	vcpus, mem := plan.VCPUs, plan.MemoryMiB
	if known(plan.CatalogImage) {
		img, err := r.client.GetImage(ctx, plan.CatalogImage.ValueString())
		if err != nil {
			apiError(&resp.Diagnostics, "looking up catalog image "+plan.CatalogImage.ValueString(), err)
			return
		}
		if body.Image == "" {
			body.Image = img.Image
		}
		if guest == "" {
			guest = img.GuestOS
		}
		if !known(vcpus) {
			vcpus = types.Int64Value(int64(img.VCPUs))
		}
		if !known(mem) {
			mem = types.Int64Value(int64(img.MemoryMiB))
		}
		if body.DiskSizeGiB == nil && img.DiskSizeGiB != nil {
			body.DiskSizeGiB = img.DiskSizeGiB
		}
	}
	if known(vcpus) {
		body.VCPUs = int(vcpus.ValueInt64())
	}
	if known(mem) {
		body.MemoryMiB = uint64(mem.ValueInt64())
	}
	body.Apple = &fleet.AppleOptions{GuestOS: guest}
	for _, n := range stringList(ctx, plan.Networks, &resp.Diagnostics) {
		body.Apple.Networks = append(body.Apple.Networks, fleet.NetworkRef{Name: n})
	}
	if known(plan.ExtraJSON) {
		if err := json.Unmarshal([]byte(plan.ExtraJSON.ValueString()), &body.Extra); err != nil {
			resp.Diagnostics.AddAttributeError(path.Root("extra_json"), "Invalid extra_json", err.Error())
		}
		// Apple options in extra_json are merged under the named ones rather than dropped.
		if a, ok := body.Extra["apple"].(map[string]interface{}); ok {
			delete(body.Extra, "apple")
			merged := map[string]interface{}{}
			for k, v := range a {
				merged[k] = v
			}
			merged["guest_os"] = guest
			if len(body.Apple.Networks) > 0 {
				merged["networks"] = body.Apple.Networks
			}
			body.Apple = nil
			body.Extra["apple"] = merged
		}
	}
	if resp.Diagnostics.HasError() {
		return
	}
	out, err := r.client.LaunchVM(ctx, body)
	if err != nil {
		msg := "launching VM"
		if guest == "macos" && (fleet.StatusOf(err) == 503 || fleet.StatusOf(err) == 403) {
			msg += " (macOS VMs run on Macs you lease, two per Mac: add a fluxvm_lease and depends_on it)"
		}
		apiError(&resp.Diagnostics, msg, err)
		return
	}
	vm := out.VM
	vm.Node = out.Node
	plan.fill(&vm)
	if want := optString(plan.PowerState); want != "" && powerOf(vm.Status) != "" && want != powerOf(vm.Status) {
		if got, err := r.power(ctx, &vm, want); err != nil {
			resp.Diagnostics.AddError("FluxVM: VM created but could not be made "+want, err.Error())
		} else {
			plan.fill(got)
		}
	}
	if plan.PowerState.IsUnknown() {
		plan.PowerState = types.StringNull()
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

// power moves the VM to the wanted state.
func (r *vmResource) power(ctx context.Context, vm *fleet.VM, want string) (*fleet.VM, error) {
	var action string
	switch {
	case want == "running" && vm.Status == "paused":
		action = fleet.PowerResume
	case want == "running":
		action = fleet.PowerStart
	case want == "stopped":
		action = fleet.PowerStop
	case want == "paused":
		action = fleet.PowerPause
	default:
		return nil, fmt.Errorf("unknown power_state %q", want)
	}
	out, err := r.client.PowerVM(ctx, vm.Node, vm.ID, action)
	if err != nil {
		return nil, err
	}
	if out.ID == "" {
		// A node that answers without the record: read it back.
		return r.client.GetVM(ctx, vm.Node, vm.ID)
	}
	out.Node = vm.Node
	return out, nil
}

func (r *vmResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st vmModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	node, id, ok := splitVMID(st.ID.ValueString())
	if !ok {
		resp.Diagnostics.AddError("Invalid VM id", "Expected <node>/<vm_id>, got "+st.ID.ValueString())
		return
	}
	vm, err := r.client.GetVM(ctx, node, id)
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if fleet.StatusOf(err) == 503 {
		// The Mac did not answer: keep the VM rather than plan a duplicate.
		resp.Diagnostics.AddWarning("FluxVM: Mac not answering", err.Error()+"; keeping the last known state.")
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading VM", err)
		return
	}
	st.fill(vm)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

func (r *vmResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, prior vmModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	node, id, _ := splitVMID(prior.ID.ValueString())
	vm := &fleet.VM{ID: id, Node: node, Status: prior.Status.ValueString()}
	if want := optString(plan.PowerState); want != "" && want != powerOf(vm.Status) {
		got, err := r.power(ctx, vm, want)
		if err != nil {
			apiError(&resp.Diagnostics, "changing power_state to "+want, err)
			return
		}
		vm = got
	} else if got, err := r.client.GetVM(ctx, node, id); err == nil {
		vm = got
	}
	if vm.Name == "" {
		vm.Name = prior.Name.ValueString()
	}
	plan.fill(vm)
	if plan.CreatedAt.IsNull() {
		plan.CreatedAt = prior.CreatedAt
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *vmResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st vmModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	node, id, _ := splitVMID(st.ID.ValueString())
	if err := r.client.DeleteVM(ctx, node, id); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting VM", err)
	}
}

func splitVMID(s string) (string, string, bool) {
	i := strings.Index(s, "/")
	if i <= 0 || i == len(s)-1 {
		return "", "", false
	}
	return s[:i], s[i+1:], true
}

// ImportState takes `<node>/<vm_id>`.
func (r *vmResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	if _, _, ok := splitVMID(req.ID); !ok {
		resp.Diagnostics.AddError("Invalid import id", "Import a VM as <node>/<vm_id>, e.g. mac-1/0b9c...")
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("reschedulable"), false)...)
}
