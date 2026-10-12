// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"regexp"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

var tenantIDRe = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,62}$`)

type tenantResource struct{ clientResource }

func newTenantResource() resource.Resource { return &tenantResource{} }

var (
	_ resource.ResourceWithImportState    = (*tenantResource)(nil)
	_ resource.ResourceWithValidateConfig = (*tenantResource)(nil)
)

type tenantModel struct {
	ID             types.String  `tfsdk:"id"`
	DisplayName    types.String  `tfsdk:"display_name"`
	MaxVMs         types.Int64   `tfsdk:"max_vms"`
	MaxVCPUs       types.Int64   `tfsdk:"max_vcpus"`
	MaxMemoryMiB   types.Int64   `tfsdk:"max_memory_mib"`
	MaxLeasedHosts types.Int64   `tfsdk:"max_leased_hosts"`
	Disabled       types.Bool    `tfsdk:"disabled"`
	MonthlyBudget  types.Float64 `tfsdk:"monthly_budget"`
	Token          types.String  `tfsdk:"token"`
	CreatedAt      types.String  `tfsdk:"created_at"`
}

func (r *tenantResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_tenant"
}

func (r *tenantResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A tenant of the Mac cloud (operator only: needs the fleet token). Its primary token is in " +
			"`token`, known only to the Terraform run that created it (or rotated it outside Terraform).",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:      true,
				PlanModifiers: replace(),
				Description:   "1-63 of a-z, 0-9 and '-', starting with a letter or digit; not `me`.",
			},
			"display_name":     schema.StringAttribute{Optional: true, Description: "Shown in the console."},
			"max_vms":          schema.Int64Attribute{Optional: true, Description: "Quota: VMs at once (unset: unlimited)."},
			"max_vcpus":        schema.Int64Attribute{Optional: true, Description: "Quota: vCPUs across the tenant's VMs."},
			"max_memory_mib":   schema.Int64Attribute{Optional: true, Description: "Quota: memory across the tenant's VMs, MiB."},
			"max_leased_hosts": schema.Int64Attribute{Optional: true, Description: "Quota: Macs leased at once."},
			"disabled": schema.BoolAttribute{
				Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				Description: "A disabled tenant's token and keys are refused at the door.",
			},
			"monthly_budget": schema.Float64Attribute{
				Computed:    true,
				Description: "The tenant's own monthly budget (the tenant sets it; read-only here).",
			},
			"token": schema.StringAttribute{
				Computed: true, Sensitive: true, PlanModifiers: keep(),
				Description: "The tenant's primary token. Central stores only its hash; null after an import.",
			},
			"created_at": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *tenantResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m tenantModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.ID) && (!tenantIDRe.MatchString(m.ID.ValueString()) || m.ID.ValueString() == "me") {
		resp.Diagnostics.AddAttributeError(path.Root("id"), "Invalid tenant id",
			"Use 1-63 lowercase letters, digits and '-', starting with a letter or digit (and not \"me\").")
	}
	for name, v := range map[string]types.Int64{"max_vms": m.MaxVMs, "max_vcpus": m.MaxVCPUs,
		"max_memory_mib": m.MaxMemoryMiB, "max_leased_hosts": m.MaxLeasedHosts} {
		if known(v) && v.ValueInt64() < 0 {
			resp.Diagnostics.AddAttributeError(path.Root(name), "Invalid quota", name+" cannot be negative.")
		}
	}
}

func (m *tenantModel) quota() fleet.Quota {
	return fleet.Quota{MaxVMs: u32Ptr(m.MaxVMs), MaxVCPUs: u64Ptr(m.MaxVCPUs),
		MaxMemoryMiB: u64Ptr(m.MaxMemoryMiB), MaxLeasedHosts: u32Ptr(m.MaxLeasedHosts)}
}

func (m *tenantModel) fill(t *fleet.Tenant) {
	m.ID = types.StringValue(t.ID)
	m.DisplayName = strOrNull(t.DisplayName)
	if t.DisplayName != nil && *t.DisplayName == "" {
		m.DisplayName = types.StringNull()
	}
	m.MaxVMs = int64OrNull(t.Quota.MaxVMs)
	m.MaxVCPUs = int64OrNull(t.Quota.MaxVCPUs)
	m.MaxMemoryMiB = int64OrNull(t.Quota.MaxMemoryMiB)
	m.MaxLeasedHosts = int64OrNull(t.Quota.MaxLeasedHosts)
	m.Disabled = types.BoolValue(t.Disabled)
	if t.MonthlyBudget != nil {
		m.MonthlyBudget = types.Float64Value(*t.MonthlyBudget)
	} else {
		m.MonthlyBudget = types.Float64Null()
	}
	m.CreatedAt = timeValue(t.CreatedAt)
}

func (r *tenantResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan tenantModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	created, err := r.client.CreateTenant(ctx, fleet.CreateTenantRequest{ID: plan.ID.ValueString(),
		DisplayName: optStringPtr(plan.DisplayName), Quota: plan.quota()})
	if err != nil {
		apiError(&resp.Diagnostics, "creating tenant", err)
		return
	}
	t := &created.Tenant
	if plan.Disabled.ValueBool() {
		if t, err = r.client.UpdateTenant(ctx, t.ID, fleet.UpdateTenantRequest{Disabled: fleet.Bool(true)}); err != nil {
			apiError(&resp.Diagnostics, "disabling tenant", err)
			return
		}
	}
	plan.fill(t)
	plan.Token = types.StringValue(created.Token)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *tenantResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st tenantModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	t, err := r.client.GetTenant(ctx, st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading tenant", err)
		return
	}
	st.fill(t)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

func (r *tenantResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, prior tenantModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	q := plan.quota()
	upd := fleet.UpdateTenantRequest{Quota: &q, Disabled: fleet.Bool(plan.Disabled.ValueBool())}
	if known(plan.DisplayName) {
		upd.DisplayName = optStringPtr(plan.DisplayName)
	} else if known(prior.DisplayName) {
		// Central's PATCH leaves an omitted name alone, so clearing it sends an empty one.
		upd.DisplayName = fleet.String("")
	}
	t, err := r.client.UpdateTenant(ctx, plan.ID.ValueString(), upd)
	if err != nil {
		apiError(&resp.Diagnostics, "updating tenant", err)
		return
	}
	plan.fill(t)
	plan.Token = prior.Token
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *tenantResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st tenantModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if err := r.client.DeleteTenant(ctx, st.ID.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting tenant (release its leases and delete its VMs first)", err)
	}
}

func (r *tenantResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
