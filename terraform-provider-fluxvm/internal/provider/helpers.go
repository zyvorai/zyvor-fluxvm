// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"context"
	"fmt"
	"sort"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/zyvorai/fluxvm/go/fleet"
)

// clientResource is embedded by every resource: it receives the configured client.
type clientResource struct {
	client *fleet.Client
}

func (r *clientResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	if req.ProviderData == nil {
		return
	}
	c, ok := req.ProviderData.(*fleet.Client)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", fmt.Sprintf("expected *fleet.Client, got %T", req.ProviderData))
		return
	}
	r.client = c
}

func (r *clientResource) ready(d *diag.Diagnostics) bool {
	if r.client == nil {
		d.AddError("Provider not configured", "The fluxvm provider has no endpoint yet; check the provider block.")
		return false
	}
	return true
}

func apiError(d *diag.Diagnostics, what string, err error) {
	d.AddError("FluxVM: "+what, err.Error())
}

func optString(s types.String) string {
	if s.IsNull() || s.IsUnknown() {
		return ""
	}
	return s.ValueString()
}

func optStringPtr(s types.String) *string {
	if s.IsNull() || s.IsUnknown() {
		return nil
	}
	v := s.ValueString()
	return &v
}

func known(v attr.Value) bool { return !v.IsNull() && !v.IsUnknown() }

func strOrNull(p *string) types.String {
	if p == nil {
		return types.StringNull()
	}
	return types.StringValue(*p)
}

func strOrEmptyNull(s string) types.String {
	if s == "" {
		return types.StringNull()
	}
	return types.StringValue(s)
}

func timeValue(t time.Time) types.String {
	if t.IsZero() {
		return types.StringNull()
	}
	return types.StringValue(t.UTC().Format(time.RFC3339))
}

func timePtrValue(t *time.Time) types.String {
	if t == nil {
		return types.StringNull()
	}
	return timeValue(*t)
}

func stringList(ctx context.Context, l types.List, d *diag.Diagnostics) []string {
	if !known(l) {
		return nil
	}
	var out []string
	d.Append(l.ElementsAs(ctx, &out, false)...)
	return out
}

func stringSet(ctx context.Context, s types.Set, d *diag.Diagnostics) []string {
	if !known(s) {
		return nil
	}
	var out []string
	d.Append(s.ElementsAs(ctx, &out, false)...)
	sort.Strings(out)
	return out
}

func stringMap(ctx context.Context, m types.Map, d *diag.Diagnostics) map[string]string {
	if !known(m) {
		return nil
	}
	out := map[string]string{}
	d.Append(m.ElementsAs(ctx, &out, false)...)
	return out
}

func listOf(vals []string) types.List {
	elems := make([]attr.Value, 0, len(vals))
	for _, v := range vals {
		elems = append(elems, types.StringValue(v))
	}
	l, _ := types.ListValue(types.StringType, elems)
	return l
}

func setOf(vals []string) types.Set {
	elems := make([]attr.Value, 0, len(vals))
	for _, v := range vals {
		elems = append(elems, types.StringValue(v))
	}
	s, _ := types.SetValue(types.StringType, elems)
	return s
}

func mapOf(vals map[string]string) types.Map {
	elems := map[string]attr.Value{}
	for k, v := range vals {
		elems[k] = types.StringValue(v)
	}
	m, _ := types.MapValue(types.StringType, elems)
	return m
}

func int64OrNull(p interface{}) types.Int64 {
	switch v := p.(type) {
	case *uint32:
		if v != nil {
			return types.Int64Value(int64(*v))
		}
	case *uint64:
		if v != nil {
			return types.Int64Value(int64(*v))
		}
	case *int:
		if v != nil {
			return types.Int64Value(int64(*v))
		}
	case *int64:
		if v != nil {
			return types.Int64Value(*v)
		}
	}
	return types.Int64Null()
}

func u32Ptr(v types.Int64) *uint32 {
	if !known(v) {
		return nil
	}
	x := uint32(v.ValueInt64())
	return &x
}

func u64Ptr(v types.Int64) *uint64 {
	if !known(v) {
		return nil
	}
	x := uint64(v.ValueInt64())
	return &x
}

func intPtr(v types.Int64) *int {
	if !known(v) {
		return nil
	}
	x := int(v.ValueInt64())
	return &x
}

// keep is the plan modifier list for computed IDs and timestamps that never change once set.
func keep() []planmodifier.String {
	return []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
}

// replace is the plan modifier list for string attributes that need a new resource when they change.
func replace() []planmodifier.String {
	return []planmodifier.String{stringplanmodifier.RequiresReplace()}
}
