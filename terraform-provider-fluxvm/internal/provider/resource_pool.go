// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"regexp"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type poolResource struct{ clientResource }

// poolNameRe is central's rule for pool names (warm VMs are named `warm-<pool>-<8 hex>`).
var poolNameRe = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,38}[a-z0-9])?$`)

func newPoolResource() resource.Resource { return &poolResource{} }

var (
	_ resource.ResourceWithImportState    = (*poolResource)(nil)
	_ resource.ResourceWithValidateConfig = (*poolResource)(nil)
)

type poolModel struct {
	ID        types.String `tfsdk:"id"`
	Name      types.String `tfsdk:"name"`
	Image     types.String `tfsdk:"image"`
	VCPUs     types.Int64  `tfsdk:"vcpus"`
	MemoryMiB types.Int64  `tfsdk:"memory_mib"`
	MinReady  types.Int64  `tfsdk:"min_ready"`
	MaxTotal  types.Int64  `tfsdk:"max_total"`
	Nodes     types.List   `tfsdk:"nodes"`
	Tenant    types.String `tfsdk:"tenant"`
	Ready     types.Int64  `tfsdk:"ready"`
	Total     types.Int64  `tfsdk:"total"`
}

func (r *poolResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_pool"
}

func (r *poolResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A warm pool: central keeps `min_ready` VMs booted from `image` so a launch is instant. `min_ready`, " +
			"`max_total` and `nodes` change in place; a new name, image or size makes a new pool (the warm VMs are " +
			"already booted). Requires central with pools.",
		Attributes: map[string]schema.Attribute{
			"id":    schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "The pool's name."},
			"name":  schema.StringAttribute{Required: true, PlanModifiers: replace()},
			"image": schema.StringAttribute{Required: true, PlanModifiers: replace(), Description: "A catalog image name."},
			"vcpus": schema.Int64Attribute{Optional: true, PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()},
				Description: "Default: the catalog entry's."},
			"memory_mib": schema.Int64Attribute{Optional: true, PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()},
				Description: "Default: the catalog entry's."},
			"min_ready": schema.Int64Attribute{Required: true, Description: "VMs kept booted and unclaimed."},
			"max_total": schema.Int64Attribute{Optional: true, Description: "Most VMs the pool holds, claimed or not."},
			"nodes": schema.ListAttribute{
				ElementType: types.StringType, Optional: true,
				Description: "Macs the pool may use (default: any the tenant may place on).",
			},
			"tenant": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"ready":  schema.Int64Attribute{Computed: true, Description: "Booted, unclaimed VMs right now."},
			"total":  schema.Int64Attribute{Computed: true},
		},
	}
}

func (r *poolResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m poolModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.MinReady) && m.MinReady.ValueInt64() < 0 {
		resp.Diagnostics.AddAttributeError(path.Root("min_ready"), "Invalid min_ready", "min_ready cannot be negative.")
	}
	if known(m.MinReady) && known(m.MaxTotal) && m.MaxTotal.ValueInt64() < m.MinReady.ValueInt64() {
		resp.Diagnostics.AddAttributeError(path.Root("max_total"), "Invalid max_total", "max_total must be at least min_ready.")
	}
	if known(m.Name) && !poolNameRe.MatchString(m.Name.ValueString()) {
		resp.Diagnostics.AddAttributeError(path.Root("name"), "Invalid pool name", "Use 1-40 lowercase letters, digits and '-', starting and ending with a letter or digit.")
	}
}

// fill copies what central reports; arguments the configuration left unset stay null so defaults do not show as drift.
func (m *poolModel) fill(p *fleet.Pool) {
	m.ID = types.StringValue(p.Name)
	m.Name = types.StringValue(p.Name)
	m.Image = types.StringValue(p.Image)
	m.MinReady = types.Int64Value(int64(p.MinReady))
	if !m.VCPUs.IsNull() {
		m.VCPUs = int64OrNull(p.VCPUs)
	}
	if !m.MemoryMiB.IsNull() {
		m.MemoryMiB = int64OrNull(p.MemoryMiB)
	}
	if !m.MaxTotal.IsNull() {
		m.MaxTotal = int64OrNull(p.MaxTotal)
	}
	if !m.Nodes.IsNull() {
		m.Nodes = listOf(p.Nodes)
	}
	m.Tenant = strOrNull(p.Tenant)
	m.Ready = types.Int64Value(int64(p.Ready))
	m.Total = types.Int64Value(int64(p.Total))
}

func (r *poolResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan poolModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	p, err := r.client.CreatePool(ctx, fleet.CreatePoolRequest{
		Name: plan.Name.ValueString(), Image: plan.Image.ValueString(), VCPUs: intPtr(plan.VCPUs),
		MemoryMiB: u64Ptr(plan.MemoryMiB), MinReady: int(plan.MinReady.ValueInt64()), MaxTotal: intPtr(plan.MaxTotal),
		Nodes: stringList(ctx, plan.Nodes, &resp.Diagnostics)})
	if err != nil {
		apiError(&resp.Diagnostics, "creating pool (needs central with pools)", err)
		return
	}
	plan.fill(p)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *poolResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st poolModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	p, err := r.client.GetPool(ctx, st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading pool", err)
		return
	}
	st.fill(p)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

func (r *poolResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan poolModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	minReady := int(plan.MinReady.ValueInt64())
	// Image and size never change in place (they replace the pool), so only the counts and the Macs are sent.
	p, err := r.client.UpdatePool(ctx, plan.Name.ValueString(), fleet.UpdatePoolRequest{
		MinReady: &minReady, MaxTotal: intPtr(plan.MaxTotal), Nodes: stringList(ctx, plan.Nodes, &resp.Diagnostics)})
	if err != nil {
		apiError(&resp.Diagnostics, "updating pool", err)
		return
	}
	plan.fill(p)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *poolResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st poolModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if err := r.client.DeletePool(ctx, st.ID.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting pool", err)
	}
}

func (r *poolResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
