// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"fmt"
	"sort"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

type clientDataSource struct {
	client *fleet.Client
}

func (d *clientDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	if req.ProviderData == nil {
		return
	}
	c, ok := req.ProviderData.(*fleet.Client)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", fmt.Sprintf("expected *fleet.Client, got %T", req.ProviderData))
		return
	}
	d.client = c
}

func (d *clientDataSource) ready(diags *diag.Diagnostics) bool {
	if d.client == nil {
		diags.AddError("Provider not configured", "The fluxvm provider has no endpoint yet; check the provider block.")
		return false
	}
	return true
}

// ---- fluxvm_images ----

type imagesDataSource struct{ clientDataSource }

func newImagesDataSource() datasource.DataSource { return &imagesDataSource{} }

type imageModel struct {
	Name        types.String `tfsdk:"name"`
	DisplayName types.String `tfsdk:"display_name"`
	Description types.String `tfsdk:"description"`
	GuestOS     types.String `tfsdk:"guest_os"`
	Image       types.String `tfsdk:"image"`
	VCPUs       types.Int64  `tfsdk:"vcpus"`
	MemoryMiB   types.Int64  `tfsdk:"memory_mib"`
	DiskSizeGiB types.Int64  `tfsdk:"disk_size_gib"`
	Source      types.String `tfsdk:"source"`
}

type imagesModel struct {
	GuestOS types.String `tfsdk:"guest_os"`
	Names   types.List   `tfsdk:"names"`
	Images  []imageModel `tfsdk:"images"`
}

func (d *imagesDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_images"
}

func (d *imagesDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "The image catalog: the disk images the operator has put on the Macs.",
		Attributes: map[string]schema.Attribute{
			"guest_os": schema.StringAttribute{Optional: true, Description: "Only `macos` or only `linux` images."},
			"names":    schema.ListAttribute{ElementType: types.StringType, Computed: true, Description: "The images' names, sorted."},
			"images": schema.ListNestedAttribute{
				Computed: true,
				NestedObject: schema.NestedAttributeObject{Attributes: map[string]schema.Attribute{
					"name":          schema.StringAttribute{Computed: true},
					"display_name":  schema.StringAttribute{Computed: true},
					"description":   schema.StringAttribute{Computed: true},
					"guest_os":      schema.StringAttribute{Computed: true},
					"image":         schema.StringAttribute{Computed: true, Description: "The disk's path on each Mac."},
					"vcpus":         schema.Int64Attribute{Computed: true},
					"memory_mib":    schema.Int64Attribute{Computed: true},
					"disk_size_gib": schema.Int64Attribute{Computed: true},
					"source":        schema.StringAttribute{Computed: true},
				}},
			},
		},
	}
}

func (d *imagesDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m imagesModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() || !d.ready(&resp.Diagnostics) {
		return
	}
	list, err := d.client.ListImages(ctx)
	if err != nil {
		apiError(&resp.Diagnostics, "listing images", err)
		return
	}
	sort.Slice(list, func(i, j int) bool { return list[i].Name < list[j].Name })
	m.Images = []imageModel{}
	names := []string{}
	for _, i := range list {
		if known(m.GuestOS) && i.GuestOS != m.GuestOS.ValueString() {
			continue
		}
		names = append(names, i.Name)
		m.Images = append(m.Images, imageModel{
			Name: types.StringValue(i.Name), DisplayName: strOrNull(i.DisplayName), Description: strOrNull(i.Description),
			GuestOS: types.StringValue(i.GuestOS), Image: types.StringValue(i.Image), VCPUs: types.Int64Value(int64(i.VCPUs)),
			MemoryMiB: types.Int64Value(int64(i.MemoryMiB)), DiskSizeGiB: int64OrNull(i.DiskSizeGiB), Source: strOrNull(i.Source),
		})
	}
	m.Names = listOf(names)
	resp.Diagnostics.Append(resp.State.Set(ctx, &m)...)
}

// ---- fluxvm_nodes ----

type nodesDataSource struct{ clientDataSource }

func newNodesDataSource() datasource.DataSource { return &nodesDataSource{} }

type nodeModel struct {
	Name                 types.String `tfsdk:"name"`
	Healthy              types.Bool   `tfsdk:"healthy"`
	Cordoned             types.Bool   `tfsdk:"cordoned"`
	VCPUsTotal           types.Int64  `tfsdk:"vcpus_total"`
	MemoryMiBTotal       types.Int64  `tfsdk:"memory_mib_total"`
	FreeVCPUs            types.Int64  `tfsdk:"free_vcpus"`
	FreeMemoryMiB        types.Int64  `tfsdk:"free_memory_mib"`
	VMCount              types.Int64  `tfsdk:"vm_count"`
	IsMac                types.Bool   `tfsdk:"is_mac"`
	NestedVirtualization types.Bool   `tfsdk:"nested_virtualization"`
	Labels               types.Map    `tfsdk:"labels"`
}

type nodesModel struct {
	Names types.List  `tfsdk:"names"`
	Nodes []nodeModel `tfsdk:"nodes"`
}

func (d *nodesDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_nodes"
}

func (d *nodesDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "Macs: with the fleet token every registered node; with a tenant token the Macs the tenant leases " +
			"(size, free capacity and Apple capabilities only).",
		Attributes: map[string]schema.Attribute{
			"names": schema.ListAttribute{ElementType: types.StringType, Computed: true},
			"nodes": schema.ListNestedAttribute{
				Computed: true,
				NestedObject: schema.NestedAttributeObject{Attributes: map[string]schema.Attribute{
					"name":                  schema.StringAttribute{Computed: true},
					"healthy":               schema.BoolAttribute{Computed: true},
					"cordoned":              schema.BoolAttribute{Computed: true},
					"vcpus_total":           schema.Int64Attribute{Computed: true},
					"memory_mib_total":      schema.Int64Attribute{Computed: true},
					"free_vcpus":            schema.Int64Attribute{Computed: true},
					"free_memory_mib":       schema.Int64Attribute{Computed: true},
					"vm_count":              schema.Int64Attribute{Computed: true},
					"is_mac":                schema.BoolAttribute{Computed: true},
					"nested_virtualization": schema.BoolAttribute{Computed: true},
					"labels":                schema.MapAttribute{ElementType: types.StringType, Computed: true, Description: "Operator only (empty for tenants)."},
				}},
			},
		},
	}
}

func (d *nodesDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	if !d.ready(&resp.Diagnostics) {
		return
	}
	nodes, err := d.client.ListNodes(ctx)
	if fleet.StatusOf(err) == 403 {
		// A tenant: the Macs it leases.
		nodes, err = nil, nil
		leases, lerr := d.client.ListLeases(ctx, fleet.ListLeasesOptions{State: fleet.LeaseActive})
		if lerr != nil {
			apiError(&resp.Diagnostics, "listing leases", lerr)
			return
		}
		for _, l := range leases {
			n, nerr := d.client.GetNode(ctx, l.Node)
			if fleet.IsNotFound(nerr) {
				continue
			}
			if nerr != nil {
				apiError(&resp.Diagnostics, "reading Mac "+l.Node, nerr)
				return
			}
			nodes = append(nodes, *n)
		}
	}
	if err != nil {
		apiError(&resp.Diagnostics, "listing nodes", err)
		return
	}
	sort.Slice(nodes, func(i, j int) bool { return nodes[i].Name < nodes[j].Name })
	m := nodesModel{Nodes: []nodeModel{}}
	names := []string{}
	for _, n := range nodes {
		names = append(names, n.Name)
		labels := n.Labels
		if labels == nil {
			labels = map[string]string{}
		}
		m.Nodes = append(m.Nodes, nodeModel{
			Name: types.StringValue(n.Name), Healthy: types.BoolValue(n.Healthy), Cordoned: types.BoolValue(n.Cordoned),
			VCPUsTotal: types.Int64Value(int64(n.VCPUsTotal)), MemoryMiBTotal: types.Int64Value(int64(n.MemoryMiBTotal)),
			FreeVCPUs: types.Int64Value(int64(n.FreeVCPUs)), FreeMemoryMiB: types.Int64Value(int64(n.FreeMemoryMiB)),
			VMCount: types.Int64Value(int64(n.VMCount)), IsMac: types.BoolValue(n.Apple != nil),
			NestedVirtualization: types.BoolValue(n.Apple != nil && n.Apple.NestedVirtualization), Labels: mapOf(labels),
		})
	}
	m.Names = listOf(names)
	resp.Diagnostics.Append(resp.State.Set(ctx, &m)...)
}

// ---- fluxvm_tenant ----

type tenantDataSource struct{ clientDataSource }

func newTenantDataSource() datasource.DataSource { return &tenantDataSource{} }

type tenantDataModel struct {
	ID             types.String  `tfsdk:"id"`
	DisplayName    types.String  `tfsdk:"display_name"`
	Disabled       types.Bool    `tfsdk:"disabled"`
	MaxVMs         types.Int64   `tfsdk:"max_vms"`
	MaxVCPUs       types.Int64   `tfsdk:"max_vcpus"`
	MaxMemoryMiB   types.Int64   `tfsdk:"max_memory_mib"`
	MaxLeasedHosts types.Int64   `tfsdk:"max_leased_hosts"`
	MonthlyBudget  types.Float64 `tfsdk:"monthly_budget"`
	VMs            types.Int64   `tfsdk:"vms"`
	VCPUs          types.Int64   `tfsdk:"vcpus"`
	MemoryMiB      types.Int64   `tfsdk:"memory_mib"`
	LeasedHosts    types.Int64   `tfsdk:"leased_hosts"`
	LeasedMacs     types.List    `tfsdk:"leased_macs"`
	CreatedAt      types.String  `tfsdk:"created_at"`
}

func (d *tenantDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_tenant"
}

func (d *tenantDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		Description: "A tenant's quotas and what it uses now. Without `id`, the calling tenant (`GET /fleet/tenants/me`); " +
			"with `id`, that tenant (fleet token).",
		Attributes: map[string]schema.Attribute{
			"id":               schema.StringAttribute{Optional: true, Computed: true},
			"display_name":     schema.StringAttribute{Computed: true},
			"disabled":         schema.BoolAttribute{Computed: true},
			"max_vms":          schema.Int64Attribute{Computed: true},
			"max_vcpus":        schema.Int64Attribute{Computed: true},
			"max_memory_mib":   schema.Int64Attribute{Computed: true},
			"max_leased_hosts": schema.Int64Attribute{Computed: true},
			"monthly_budget":   schema.Float64Attribute{Computed: true},
			"vms":              schema.Int64Attribute{Computed: true, Description: "VMs running now."},
			"vcpus":            schema.Int64Attribute{Computed: true},
			"memory_mib":       schema.Int64Attribute{Computed: true},
			"leased_hosts":     schema.Int64Attribute{Computed: true},
			"leased_macs":      schema.ListAttribute{ElementType: types.StringType, Computed: true},
			"created_at":       schema.StringAttribute{Computed: true},
		},
	}
}

func (d *tenantDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m tenantDataModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() || !d.ready(&resp.Diagnostics) {
		return
	}
	var t *fleet.Tenant
	var err error
	if known(m.ID) && m.ID.ValueString() != fleet.Me {
		t, err = d.client.GetTenant(ctx, m.ID.ValueString())
	} else {
		t, err = d.client.Me(ctx)
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading tenant", err)
		return
	}
	m.ID = types.StringValue(t.ID)
	m.DisplayName = strOrNull(t.DisplayName)
	m.Disabled = types.BoolValue(t.Disabled)
	m.MaxVMs = int64OrNull(t.Quota.MaxVMs)
	m.MaxVCPUs = int64OrNull(t.Quota.MaxVCPUs)
	m.MaxMemoryMiB = int64OrNull(t.Quota.MaxMemoryMiB)
	m.MaxLeasedHosts = int64OrNull(t.Quota.MaxLeasedHosts)
	m.MonthlyBudget = types.Float64Null()
	if t.MonthlyBudget != nil {
		m.MonthlyBudget = types.Float64Value(*t.MonthlyBudget)
	}
	var use fleet.InUse
	if t.InUse != nil {
		use = *t.InUse
	}
	m.VMs = types.Int64Value(int64(use.VMs))
	m.VCPUs = types.Int64Value(int64(use.VCPUs))
	m.MemoryMiB = types.Int64Value(int64(use.MemoryMiB))
	m.LeasedHosts = types.Int64Value(int64(use.LeasedHosts))
	macs := []string{}
	for _, l := range t.ActiveLeases {
		macs = append(macs, l.Node)
	}
	sort.Strings(macs)
	m.LeasedMacs = listOf(macs)
	m.CreatedAt = timeValue(t.CreatedAt)
	resp.Diagnostics.Append(resp.State.Set(ctx, &m)...)
}
