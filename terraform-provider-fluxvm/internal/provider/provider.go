// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

// Package provider is the FluxVM Terraform provider: resources and data sources over central's fleet API.
package provider

import (
	"context"
	"os"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

// New returns the provider factory for a version string.
func New(version string) func() provider.Provider {
	return func() provider.Provider { return &fluxProvider{version: version} }
}

type fluxProvider struct {
	version string
}

type providerModel struct {
	Endpoint types.String `tfsdk:"endpoint"`
	Token    types.String `tfsdk:"token"`
	Retries  types.Int64  `tfsdk:"retries"`
}

func (p *fluxProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = "fluxvm"
	resp.Version = p.version
}

func (p *fluxProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "Manage a FluxVM Mac cloud through central's fleet API: Mac leases, VMs, private networks, " +
			"volumes, warm pools, API keys and webhooks, and (with the fleet token) tenants.",
		Attributes: map[string]schema.Attribute{
			"endpoint": schema.StringAttribute{
				Optional:    true,
				Description: "Central's URL, e.g. `https://fleet.example.com`. Defaults to `FLUXVM_ENDPOINT`.",
			},
			"token": schema.StringAttribute{
				Optional:  true,
				Sensitive: true,
				Description: "A bearer token: a tenant's primary token or API key, or the fleet (operator) token. " +
					"Defaults to `FLUXVM_TOKEN`.",
			},
			"retries": schema.Int64Attribute{
				Optional:    true,
				Description: "How often an idempotent call is retried after a 429 or 5xx (default 3).",
			},
		},
	}
}

func (p *fluxProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var cfg providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &cfg)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if cfg.Endpoint.IsUnknown() || cfg.Token.IsUnknown() {
		resp.Diagnostics.AddWarning("Provider configuration not known yet",
			"endpoint or token depends on a value known only after apply; FluxVM resources cannot be planned until then.")
		return
	}
	endpoint := os.Getenv("FLUXVM_ENDPOINT")
	if !cfg.Endpoint.IsNull() {
		endpoint = cfg.Endpoint.ValueString()
	}
	token := os.Getenv("FLUXVM_TOKEN")
	if !cfg.Token.IsNull() {
		token = cfg.Token.ValueString()
	}
	endpoint = strings.TrimSpace(endpoint)
	if endpoint == "" {
		resp.Diagnostics.AddAttributeError(path.Root("endpoint"), "Missing FluxVM endpoint",
			"Set endpoint in the provider block or FLUXVM_ENDPOINT, e.g. https://fleet.example.com.")
		return
	}
	if !strings.HasPrefix(endpoint, "http://") && !strings.HasPrefix(endpoint, "https://") {
		resp.Diagnostics.AddAttributeError(path.Root("endpoint"), "Invalid FluxVM endpoint",
			"endpoint must start with http:// or https://, got "+endpoint)
		return
	}
	opts := []fleet.Option{fleet.WithUserAgent("terraform-provider-fluxvm/" + p.version)}
	if token != "" {
		opts = append(opts, fleet.WithToken(token))
	}
	if !cfg.Retries.IsNull() && !cfg.Retries.IsUnknown() {
		opts = append(opts, fleet.WithRetries(int(cfg.Retries.ValueInt64())))
	}
	client := fleet.NewClient(endpoint, opts...)
	resp.ResourceData = client
	resp.DataSourceData = client
}

func (p *fluxProvider) Resources(context.Context) []func() resource.Resource {
	return []func() resource.Resource{
		newTenantResource,
		newAPIKeyResource,
		newLeaseResource,
		newVMResource,
		newNetworkResource,
		newWebhookResource,
		newVolumeResource,
		newPoolResource,
	}
}

func (p *fluxProvider) DataSources(context.Context) []func() datasource.DataSource {
	return []func() datasource.DataSource{
		newImagesDataSource,
		newNodesDataSource,
		newTenantDataSource,
	}
}
