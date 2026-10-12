// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type apiKeyResource struct{ clientResource }

func newAPIKeyResource() resource.Resource { return &apiKeyResource{} }

var (
	_ resource.ResourceWithImportState    = (*apiKeyResource)(nil)
	_ resource.ResourceWithValidateConfig = (*apiKeyResource)(nil)
)

type apiKeyModel struct {
	ID            types.String `tfsdk:"id"`
	Tenant        types.String `tfsdk:"tenant"`
	Name          types.String `tfsdk:"name"`
	ReadOnly      types.Bool   `tfsdk:"read_only"`
	ExpiresInDays types.Int64  `tfsdk:"expires_in_days"`
	Token         types.String `tfsdk:"token"`
	CreatedAt     types.String `tfsdk:"created_at"`
	ExpiresAt     types.String `tfsdk:"expires_at"`
	LastUsedAt    types.String `tfsdk:"last_used_at"`
}

func (r *apiKeyResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_api_key"
}

func (r *apiKeyResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A tenant's named API key, for CI or a tool. Keys are made with the tenant's primary token (not " +
			"with another key) or by the operator. Every change replaces the key.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "`key_` and 12 hex characters."},
			"tenant": schema.StringAttribute{
				Optional: true, Computed: true, Default: stringdefault.StaticString(fleet.Me),
				PlanModifiers: replace(),
				Description:   "The tenant the key belongs to: `me` (the default) for the calling tenant; another id needs the fleet token.",
			},
			"name": schema.StringAttribute{Required: true, PlanModifiers: replace(),
				Description: "Unique within the tenant; shown in the activity log. May not look like a key id."},
			"read_only": schema.BoolAttribute{
				Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()},
				Description:   "A read-only key can only GET.",
			},
			"expires_in_days": schema.Int64Attribute{
				Optional: true, PlanModifiers: []planmodifier.Int64{int64planmodifier.RequiresReplace()},
				Description: "1 to 3650; unset never expires.",
			},
			"token":        schema.StringAttribute{Computed: true, Sensitive: true, PlanModifiers: keep(), Description: "The key's bearer token; null after an import."},
			"created_at":   schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"expires_at":   schema.StringAttribute{Computed: true, PlanModifiers: keep()},
			"last_used_at": schema.StringAttribute{Computed: true, PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()}},
		},
	}
}

func (r *apiKeyResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m apiKeyModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.ExpiresInDays) && (m.ExpiresInDays.ValueInt64() < 1 || m.ExpiresInDays.ValueInt64() > 3650) {
		resp.Diagnostics.AddAttributeError(path.Root("expires_in_days"), "Invalid expiry", "expires_in_days must be between 1 and 3650.")
	}
	if known(m.Name) && strings.TrimSpace(m.Name.ValueString()) == "" {
		resp.Diagnostics.AddAttributeError(path.Root("name"), "Invalid name", "A key needs a name.")
	}
}

func (m *apiKeyModel) fill(k *fleet.APIKey) {
	m.ID = types.StringValue(k.ID)
	m.Name = types.StringValue(k.Name)
	m.ReadOnly = types.BoolValue(k.ReadOnly)
	m.CreatedAt = timeValue(k.CreatedAt)
	m.ExpiresAt = timePtrValue(k.ExpiresAt)
	m.LastUsedAt = timePtrValue(k.LastUsedAt)
}

func (r *apiKeyResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan apiKeyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	out, err := r.client.CreateKey(ctx, plan.Tenant.ValueString(), fleet.CreateKeyRequest{
		Name: plan.Name.ValueString(), ExpiresInDays: intPtr(plan.ExpiresInDays), ReadOnly: plan.ReadOnly.ValueBool()})
	if err != nil {
		apiError(&resp.Diagnostics, "creating API key", err)
		return
	}
	plan.fill(&out.Key)
	plan.Token = types.StringValue(out.Token)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *apiKeyResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st apiKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	k, err := r.client.GetKey(ctx, st.Tenant.ValueString(), st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading API key", err)
		return
	}
	st.fill(k)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

// Update is never called with a change (every argument replaces), but copies the plan for completeness.
func (r *apiKeyResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan apiKeyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *apiKeyResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st apiKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if _, err := r.client.RevokeKey(ctx, st.Tenant.ValueString(), st.ID.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "revoking API key", err)
	}
}

// ImportState takes `key_…` (the calling tenant's key) or `<tenant>/key_…` (operator).
func (r *apiKeyResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	tenant, id := fleet.Me, req.ID
	if i := strings.Index(req.ID, "/"); i >= 0 {
		tenant, id = req.ID[:i], req.ID[i+1:]
	}
	if id == "" || tenant == "" {
		resp.Diagnostics.AddError("Invalid import id", "Use key_<12 hex> or <tenant>/key_<12 hex>.")
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("tenant"), tenant)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), id)...)
}
