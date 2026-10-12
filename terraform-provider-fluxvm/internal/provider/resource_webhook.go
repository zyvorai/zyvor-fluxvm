// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"net/url"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/setplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type webhookResource struct{ clientResource }

func newWebhookResource() resource.Resource { return &webhookResource{} }

var (
	_ resource.ResourceWithImportState    = (*webhookResource)(nil)
	_ resource.ResourceWithValidateConfig = (*webhookResource)(nil)
)

type webhookModel struct {
	ID        types.String `tfsdk:"id"`
	URL       types.String `tfsdk:"url"`
	Events    types.Set    `tfsdk:"events"`
	Secret    types.String `tfsdk:"secret"`
	Owner     types.String `tfsdk:"owner"`
	CreatedAt types.String `tfsdk:"created_at"`
}

func (r *webhookResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_webhook"
}

func (r *webhookResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A URL that receives notifications (vm.created, lease.ending, budget.over, ...) as JSON signed " +
			"with `X-FluxVM-Signature: sha256=<hmac>`. A tenant's webhook must be https to a public address. Changes replace it.",
		Attributes: map[string]schema.Attribute{
			"id":  schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "`whk_` and 12 hex characters."},
			"url": schema.StringAttribute{Required: true, PlanModifiers: replace()},
			"events": schema.SetAttribute{
				ElementType: types.StringType, Optional: true,
				PlanModifiers: []planmodifier.Set{setplanmodifier.RequiresReplace()},
				Description:   "Event types to send; unset sends every event the owner may receive.",
			},
			"secret": schema.StringAttribute{
				Computed: true, Sensitive: true, PlanModifiers: keep(),
				Description: "The HMAC-SHA256 signing secret, shown by central only at creation (null after an import).",
			},
			"owner":      schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "The tenant; null for the operator's webhooks."},
			"created_at": schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *webhookResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m webhookModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.URL) {
		u, err := url.Parse(m.URL.ValueString())
		if err != nil || (u.Scheme != "https" && u.Scheme != "http") || u.Host == "" {
			resp.Diagnostics.AddAttributeError(path.Root("url"), "Invalid webhook URL", "Use an https:// URL.")
		}
	}
}

func (m *webhookModel) fill(h *fleet.Webhook, keepEvents bool) {
	m.ID = types.StringValue(h.ID)
	m.URL = types.StringValue(h.URL)
	m.Owner = strOrNull(h.Owner)
	m.CreatedAt = timeValue(h.CreatedAt)
	if !keepEvents || !m.Events.IsNull() {
		if len(h.Events) == 0 {
			m.Events = types.SetNull(types.StringType)
		} else {
			m.Events = setOf(h.Events)
		}
	}
}

func (r *webhookResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan webhookModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	out, err := r.client.CreateWebhook(ctx, fleet.CreateWebhookRequest{URL: plan.URL.ValueString(),
		Events: stringSet(ctx, plan.Events, &resp.Diagnostics)})
	if err != nil {
		apiError(&resp.Diagnostics, "creating webhook", err)
		return
	}
	plan.fill(&out.Webhook, true)
	plan.Secret = types.StringValue(out.Secret)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *webhookResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st webhookModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	h, err := r.client.GetWebhook(ctx, st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading webhook", err)
		return
	}
	st.fill(h, false)
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

func (r *webhookResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan webhookModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *webhookResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st webhookModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if err := r.client.DeleteWebhook(ctx, st.ID.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting webhook", err)
	}
}

func (r *webhookResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
