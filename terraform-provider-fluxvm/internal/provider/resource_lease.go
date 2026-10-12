// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"errors"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type leaseResource struct{ clientResource }

func newLeaseResource() resource.Resource { return &leaseResource{} }

var (
	_ resource.ResourceWithImportState    = (*leaseResource)(nil)
	_ resource.ResourceWithValidateConfig = (*leaseResource)(nil)
)

type leaseModel struct {
	ID                   types.String `tfsdk:"id"`
	Tenant               types.String `tfsdk:"tenant"`
	Node                 types.String `tfsdk:"node"`
	NodeSelector         types.Map    `tfsdk:"node_selector"`
	Hours                types.Int64  `tfsdk:"hours"`
	MinVCPUs             types.Int64  `tfsdk:"min_vcpus"`
	MinMemoryMiB         types.Int64  `tfsdk:"min_memory_mib"`
	NestedVirtualization types.Bool   `tfsdk:"nested_virtualization"`
	ForceRelease         types.Bool   `tfsdk:"force_release"`
	State                types.String `tfsdk:"state"`
	StartedAt            types.String `tfsdk:"started_at"`
	MinEnd               types.String `tfsdk:"min_end"`
	EndsAt               types.String `tfsdk:"ends_at"`
	PriceClass           types.String `tfsdk:"price_class"`
}

func (r *leaseResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_lease"
}

func (r *leaseResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	i64Replace := []planmodifier.Int64{int64planmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A whole Mac leased for at least 24 hours (the macOS licence's minimum). Destroying it releases " +
			"the lease: central deletes the tenant's VMs on that Mac and marks it for scrubbing. Central refuses a " +
			"release before the 24-hour minimum unless the operator sets `force_release` (still billed to the minimum).",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"tenant": schema.StringAttribute{
				Optional: true, Computed: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplaceIfConfigured(), stringplanmodifier.UseStateForUnknown()},
				Description:   "Operator only: the tenant to lease for. A tenant token always leases for itself.",
			},
			"node": schema.StringAttribute{
				Optional: true, Computed: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplaceIfConfigured(), stringplanmodifier.UseStateForUnknown()},
				Description:   "A specific Mac; otherwise central picks the smallest eligible one.",
			},
			"node_selector": schema.MapAttribute{
				ElementType: types.StringType, Optional: true,
				PlanModifiers: []planmodifier.Map{mapplanmodifier.RequiresReplace()},
				Description:   "Only Macs whose labels match all of these.",
			},
			"hours": schema.Int64Attribute{
				Optional: true, PlanModifiers: i64Replace,
				Description: "Lease length (at least 24); central ends the lease then. Unset runs until destroyed.",
			},
			"min_vcpus":      schema.Int64Attribute{Optional: true, PlanModifiers: i64Replace},
			"min_memory_mib": schema.Int64Attribute{Optional: true, PlanModifiers: i64Replace},
			"nested_virtualization": schema.BoolAttribute{
				Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()},
				Description:   "Only Macs with nested virtualization (M3 or later).",
			},
			"force_release": schema.BoolAttribute{
				Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				Description: "Operator only: release on destroy even before the 24-hour minimum.",
			},
			"state":       schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "`active` or `released`."},
			"started_at":  schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"min_end":     schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "The earliest time the lease can be released."},
			"ends_at":     schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"price_class": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *leaseResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m leaseModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.Hours) && m.Hours.ValueInt64() < 24 {
		resp.Diagnostics.AddAttributeError(path.Root("hours"), "Lease too short",
			"A lease is at least 24 hours: the macOS licence requires 24 consecutive hours of sole use.")
	}
}

func (m *leaseModel) fill(l *fleet.Lease) {
	m.ID = types.StringValue(l.ID)
	m.Tenant = types.StringValue(l.Tenant)
	m.Node = types.StringValue(l.Node)
	m.State = types.StringValue(l.State)
	m.StartedAt = timeValue(l.StartedAt)
	m.MinEnd = timeValue(l.MinEnd)
	m.EndsAt = timePtrValue(l.EndsAt)
	m.PriceClass = strOrEmptyNull(l.PriceClass)
}

func (r *leaseResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan leaseModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	body := fleet.CreateLeaseRequest{
		Tenant:               optString(plan.Tenant),
		Node:                 optString(plan.Node),
		NodeSelector:         stringMap(ctx, plan.NodeSelector, &resp.Diagnostics),
		NestedVirtualization: plan.NestedVirtualization.ValueBool(),
	}
	if known(plan.Hours) {
		body.Hours = fleet.Int64(plan.Hours.ValueInt64())
	}
	if known(plan.MinVCPUs) {
		body.MinVCPUs = uint32(plan.MinVCPUs.ValueInt64())
	}
	if known(plan.MinMemoryMiB) {
		body.MinMemoryMiB = uint64(plan.MinMemoryMiB.ValueInt64())
	}
	out, err := r.client.CreateLease(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "leasing a Mac", err)
		return
	}
	plan.fill(&out.Lease)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *leaseResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st leaseModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	l, err := r.client.GetLease(ctx, st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading lease", err)
		return
	}
	// A lease that ended (its hours ran out, or someone released it) is gone: plan a new one.
	if l.State != fleet.LeaseActive {
		resp.State.RemoveResource(ctx)
		return
	}
	st.fill(l)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

// Update only changes force_release, which lives in state alone.
func (r *leaseResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan leaseModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *leaseResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st leaseModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	_, err := r.client.ReleaseLease(ctx, st.ID.ValueString(), st.ForceRelease.ValueBool())
	switch {
	case err == nil, fleet.IsNotFound(err):
	case fleet.StatusOf(err) == 409 && isNotActive(err):
		// Already ended: nothing to release.
	default:
		apiError(&resp.Diagnostics, "releasing lease", err)
	}
}

func isNotActive(err error) bool {
	var ae *fleet.APIError
	return errors.As(err, &ae) && strings.Contains(ae.Message, "is not active")
}

func (r *leaseResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("nested_virtualization"), false)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("force_release"), false)...)
}
