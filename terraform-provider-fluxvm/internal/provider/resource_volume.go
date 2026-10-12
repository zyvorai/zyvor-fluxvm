// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type volumeResource struct{ clientResource }

func newVolumeResource() resource.Resource { return &volumeResource{} }

var (
	_ resource.ResourceWithImportState    = (*volumeResource)(nil)
	_ resource.ResourceWithValidateConfig = (*volumeResource)(nil)
)

type volumeModel struct {
	ID         types.String `tfsdk:"id"`
	Name       types.String `tfsdk:"name"`
	SizeGiB    types.Int64  `tfsdk:"size_gib"`
	Node       types.String `tfsdk:"node"`
	Tenant     types.String `tfsdk:"tenant"`
	AttachedTo types.String `tfsdk:"attached_to"`
	CreatedAt  types.String `tfsdk:"created_at"`
}

func (r *volumeResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_volume"
}

// shrinkReplaces: a volume only grows in place; a smaller size needs a new volume.
func shrinkReplaces() planmodifier.Int64 {
	return int64planmodifier.RequiresReplaceIf(func(_ context.Context, req planmodifier.Int64Request, resp *int64planmodifier.RequiresReplaceIfFuncResponse) {
		resp.RequiresReplace = known(req.StateValue) && known(req.PlanValue) && req.PlanValue.ValueInt64() < req.StateValue.ValueInt64()
	}, "Shrinking a volume replaces it (its data is lost).", "Shrinking a volume replaces it (its data is lost).")
}

func (r *volumeResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A block volume on one Mac, attached to a VM at launch (`fluxvm_vm.volumes`). Growing `size_gib` " +
			"resizes it in place; shrinking replaces it. Requires central with volumes.",
		Attributes: map[string]schema.Attribute{
			"id":       schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"name":     schema.StringAttribute{Required: true, PlanModifiers: replace(), Description: "Unique within the tenant; the import id."},
			"size_gib": schema.Int64Attribute{Required: true, PlanModifiers: []planmodifier.Int64{shrinkReplaces()}},
			"node": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "The Mac it lives on; otherwise central picks one (a Mac the tenant leases first).",
			},
			"tenant":      schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"attached_to": schema.StringAttribute{Computed: true, Description: "The VM id it is attached to, if any."},
			"created_at":  schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *volumeResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m volumeModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.SizeGiB) && (m.SizeGiB.ValueInt64() < 1 || m.SizeGiB.ValueInt64() > 2048) {
		resp.Diagnostics.AddAttributeError(path.Root("size_gib"), "Invalid size", "size_gib is 1-2048.")
	}
	if n := m.Name.ValueString(); known(m.Name) && (!poolNameRe.MatchString(n) || strings.HasPrefix(n, "vol-")) {
		resp.Diagnostics.AddAttributeError(path.Root("name"), "Invalid volume name",
			"Use 1-40 lowercase letters, digits and '-', starting and ending with a letter or digit, not starting with vol-.")
	}
}

func (m *volumeModel) fill(v *fleet.Volume) {
	m.ID = types.StringValue(v.ID)
	m.Name = types.StringValue(v.Name)
	m.SizeGiB = types.Int64Value(int64(v.SizeGiB))
	m.Node = types.StringValue(v.Node)
	m.Tenant = strOrNull(v.Tenant)
	m.AttachedTo = strOrNull(v.AttachedTo)
	m.CreatedAt = timeValue(v.CreatedAt)
}

func (r *volumeResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan volumeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	v, err := r.client.CreateVolume(ctx, fleet.CreateVolumeRequest{Name: plan.Name.ValueString(),
		SizeGiB: uint64(plan.SizeGiB.ValueInt64()), Node: optString(plan.Node)})
	if err != nil {
		apiError(&resp.Diagnostics, "creating volume (needs central with volumes)", err)
		return
	}
	plan.fill(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *volumeResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st volumeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	v, err := r.client.GetVolume(ctx, st.Name.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading volume", err)
		return
	}
	st.fill(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

func (r *volumeResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, prior volumeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	var v *fleet.Volume
	var err error
	if plan.SizeGiB.ValueInt64() > prior.SizeGiB.ValueInt64() {
		v, err = r.client.ResizeVolume(ctx, prior.Name.ValueString(), uint64(plan.SizeGiB.ValueInt64()))
	} else {
		v, err = r.client.GetVolume(ctx, prior.Name.ValueString())
	}
	if err != nil {
		apiError(&resp.Diagnostics, "resizing volume", err)
		return
	}
	plan.fill(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *volumeResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st volumeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if err := r.client.DeleteVolume(ctx, st.Name.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting volume (delete the VM it is attached to first)", err)
	}
}

func (r *volumeResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("name"), req, resp)
}
