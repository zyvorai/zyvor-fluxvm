// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"fmt"
	"regexp"
	"sort"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64default"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/setplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

var networkNameRe = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,38}[a-z0-9])?$`)

type networkResource struct{ clientResource }

func newNetworkResource() resource.Resource { return &networkResource{} }

var (
	_ resource.ResourceWithImportState    = (*networkResource)(nil)
	_ resource.ResourceWithValidateConfig = (*networkResource)(nil)
)

type networkModel struct {
	ID           types.String `tfsdk:"id"`
	Name         types.String `tfsdk:"name"`
	Tenant       types.String `tfsdk:"tenant"`
	Nodes        types.Set    `tfsdk:"nodes"`
	HostsPerNode types.Int64  `tfsdk:"hosts_per_node"`
	MTU          types.Int64  `tfsdk:"mtu"`
	Subnet       types.String `tfsdk:"subnet"`
	Port         types.Int64  `tfsdk:"port"`
	HostRanges   types.Map    `tfsdk:"host_ranges"`
	KeyEpoch     types.Int64  `tfsdk:"key_epoch"`
	CreatedAt    types.String `tfsdk:"created_at"`
}

func (r *networkResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_network"
}

func (r *networkResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	i64Replace := []planmodifier.Int64{int64planmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		Description: "A private network spanning Macs (an overlay): one /24, with a slice of addresses per member Mac. " +
			"VMs join it with `fluxvm_vm.networks`. Members change in place; the key is never exposed.",
		Attributes: map[string]schema.Attribute{
			"id":   schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "The network's name."},
			"name": schema.StringAttribute{Required: true, PlanModifiers: replace(), Description: "1-40 of a-z, 0-9 and '-'."},
			"tenant": schema.StringAttribute{
				Optional: true, Computed: true, PlanModifiers: fixedIfConfigured(),
				Description: "Operator only: the tenant it belongs to.",
			},
			"nodes": schema.SetAttribute{
				ElementType: types.StringType, Optional: true, Computed: true,
				PlanModifiers: []planmodifier.Set{setplanmodifier.UseStateForUnknown()},
				Description:   "Member Macs. Unset: every Mac the tenant leases when the network is created.",
			},
			"hosts_per_node": schema.Int64Attribute{
				Optional: true, Computed: true, Default: int64default.StaticInt64(30), PlanModifiers: i64Replace,
				Description: "Addresses each member Mac gets (default 30).",
			},
			"mtu": schema.Int64Attribute{
				Optional: true, Computed: true, Default: int64default.StaticInt64(1400), PlanModifiers: i64Replace,
			},
			"subnet":      schema.StringAttribute{Computed: true, PlanModifiers: keep(), Description: "e.g. 10.89.3.0/24."},
			"port":        schema.Int64Attribute{Computed: true, PlanModifiers: []planmodifier.Int64{int64planmodifier.UseStateForUnknown()}},
			"host_ranges": schema.MapAttribute{ElementType: types.StringType, Computed: true, Description: "Member Mac to its host numbers, e.g. `2-31`."},
			"key_epoch":   schema.Int64Attribute{Computed: true, Description: "Bumped by every key rotation."},
			"created_at":  schema.StringAttribute{Computed: true, PlanModifiers: keep()},
		},
	}
}

func (r *networkResource) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var m networkModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if known(m.Name) && !networkNameRe.MatchString(m.Name.ValueString()) {
		resp.Diagnostics.AddAttributeError(path.Root("name"), "Invalid network name",
			"Use 1-40 lowercase letters, digits and '-', not starting or ending with '-'.")
	}
	if known(m.HostsPerNode) && (m.HostsPerNode.ValueInt64() < 1 || m.HostsPerNode.ValueInt64() > 253) {
		resp.Diagnostics.AddAttributeError(path.Root("hosts_per_node"), "Invalid hosts_per_node", "hosts_per_node is 1-253.")
	}
	if known(m.MTU) && (m.MTU.ValueInt64() < 576 || m.MTU.ValueInt64() > 9000) {
		resp.Diagnostics.AddAttributeError(path.Root("mtu"), "Invalid mtu", "mtu is 576-9000.")
	}
}

func (m *networkModel) fill(n *fleet.Network) {
	m.ID = types.StringValue(n.Name)
	m.Name = types.StringValue(n.Name)
	m.Tenant = strOrNull(n.Tenant)
	m.MTU = types.Int64Value(int64(n.MTU))
	m.Subnet = types.StringValue(n.Subnet)
	m.Port = types.Int64Value(int64(n.Port))
	m.KeyEpoch = types.Int64Value(int64(n.KeyEpoch))
	m.CreatedAt = timeValue(n.CreatedAt)
	nodes := make([]string, 0, len(n.Members))
	ranges := map[string]string{}
	for node, r := range n.Members {
		nodes = append(nodes, node)
		ranges[node] = fmt.Sprintf("%d-%d", r[0], r[1])
		if hpn := r[1] - r[0] + 1; hpn > 0 {
			m.HostsPerNode = types.Int64Value(int64(hpn))
		}
	}
	sort.Strings(nodes)
	m.Nodes = setOf(nodes)
	m.HostRanges = mapOf(ranges)
	if m.HostsPerNode.IsUnknown() || m.HostsPerNode.IsNull() {
		m.HostsPerNode = types.Int64Value(30)
	}
}

func (r *networkResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan networkModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	body := fleet.CreateNetworkRequest{
		Name:         plan.Name.ValueString(),
		Tenant:       optString(plan.Tenant),
		HostsPerNode: int(plan.HostsPerNode.ValueInt64()),
		MTU:          int(plan.MTU.ValueInt64()),
	}
	if known(plan.Nodes) {
		body.Nodes = stringSet(ctx, plan.Nodes, &resp.Diagnostics)
		if body.Nodes == nil {
			body.Nodes = []string{}
		}
	}
	n, err := r.client.CreateNetwork(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "creating network", err)
		return
	}
	hpn := plan.HostsPerNode
	plan.fill(n)
	plan.HostsPerNode = hpn
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *networkResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var st networkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	n, err := r.client.GetNetwork(ctx, st.ID.ValueString())
	if fleet.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading network", err)
		return
	}
	hpn := st.HostsPerNode
	st.fill(n)
	if known(hpn) {
		st.HostsPerNode = hpn
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &st)...)
}

// Update adds and removes member Macs.
func (r *networkResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, prior networkModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &prior)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	name := prior.ID.ValueString()
	var n *fleet.Network
	var err error
	if known(plan.Nodes) {
		want := map[string]bool{}
		for _, x := range stringSet(ctx, plan.Nodes, &resp.Diagnostics) {
			want[x] = true
		}
		have := map[string]bool{}
		for _, x := range stringSet(ctx, prior.Nodes, &resp.Diagnostics) {
			have[x] = true
		}
		for _, x := range sortedKeys(want) {
			if !have[x] {
				if n, err = r.client.AddNetworkMember(ctx, name, x); err != nil {
					apiError(&resp.Diagnostics, "adding "+x+" to network "+name, err)
					return
				}
			}
		}
		for _, x := range sortedKeys(have) {
			if !want[x] {
				if n, err = r.client.RemoveNetworkMember(ctx, name, x); err != nil {
					apiError(&resp.Diagnostics, "removing "+x+" from network "+name, err)
					return
				}
			}
		}
	}
	if n == nil {
		if n, err = r.client.GetNetwork(ctx, name); err != nil {
			apiError(&resp.Diagnostics, "reading network", err)
			return
		}
	}
	hpn := plan.HostsPerNode
	plan.fill(n)
	plan.HostsPerNode = hpn
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func sortedKeys(m map[string]bool) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

func (r *networkResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var st networkModel
	resp.Diagnostics.Append(req.State.Get(ctx, &st)...)
	if resp.Diagnostics.HasError() || !r.ready(&resp.Diagnostics) {
		return
	}
	if err := r.client.DeleteNetwork(ctx, st.ID.ValueString()); err != nil && !fleet.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting network", err)
	}
}

func (r *networkResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
