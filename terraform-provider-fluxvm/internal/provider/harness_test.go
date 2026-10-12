// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

// A small stand-in for Terraform core: it drives the provider over the real plugin protocol (tfprotov6) the way
// `terraform plan/apply/import/refresh` do (validate, plan with a proposed new state, apply, read, import), so the
// tests run the provider end to end against a fake central without a Terraform binary.

import (
	"context"
	"fmt"
	"math/big"
	"sort"
	"strings"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

type tf struct {
	t         *testing.T
	ctx       context.Context
	srv       tfprotov6.ProviderServer
	resources map[string]*tfprotov6.Schema
	data      map[string]*tfprotov6.Schema
}

// cfg is a configuration: attribute name to a Go value (string, int, bool, float64, []string, map[string]string, nil).
type cfg map[string]interface{}

func newTF(t *testing.T, providerCfg cfg) *tf {
	t.Helper()
	ctx := context.Background()
	srv := providerserver.NewProtocol6(New("test")())()
	sch, err := srv.GetProviderSchema(ctx, &tfprotov6.GetProviderSchemaRequest{})
	if err != nil {
		t.Fatal(err)
	}
	failOnDiags(t, "schema", sch.Diagnostics)
	h := &tf{t: t, ctx: ctx, srv: srv, resources: sch.ResourceSchemas, data: sch.DataSourceSchemas}
	pv := h.object(sch.Provider, providerCfg)
	dv := dyn(t, sch.Provider.ValueType(), pv)
	vr, err := srv.ValidateProviderConfig(ctx, &tfprotov6.ValidateProviderConfigRequest{Config: dv})
	if err != nil {
		t.Fatal(err)
	}
	failOnDiags(t, "validate provider", vr.Diagnostics)
	cr, err := srv.ConfigureProvider(ctx, &tfprotov6.ConfigureProviderRequest{Config: dv, TerraformVersion: "1.9.0"})
	if err != nil {
		t.Fatal(err)
	}
	failOnDiags(t, "configure provider", cr.Diagnostics)
	return h
}

func dyn(t *testing.T, typ tftypes.Type, v tftypes.Value) *tfprotov6.DynamicValue {
	t.Helper()
	dv, err := tfprotov6.NewDynamicValue(typ, v)
	if err != nil {
		t.Fatal(err)
	}
	return &dv
}

func undyn(t *testing.T, typ tftypes.Type, dv *tfprotov6.DynamicValue) tftypes.Value {
	t.Helper()
	if dv == nil {
		return tftypes.NewValue(typ, nil)
	}
	v, err := dv.Unmarshal(typ)
	if err != nil {
		t.Fatal(err)
	}
	return v
}

func diagText(ds []*tfprotov6.Diagnostic) string {
	var b strings.Builder
	for _, d := range ds {
		fmt.Fprintf(&b, "[%v] %s: %s; ", d.Severity, d.Summary, d.Detail)
	}
	return b.String()
}

func hasError(ds []*tfprotov6.Diagnostic) bool {
	for _, d := range ds {
		if d.Severity == tfprotov6.DiagnosticSeverityError {
			return true
		}
	}
	return false
}

func failOnDiags(t *testing.T, what string, ds []*tfprotov6.Diagnostic) {
	t.Helper()
	if hasError(ds) {
		t.Fatalf("%s: %s", what, diagText(ds))
	}
}

// goValue converts a Go value to a tftypes value of typ.
func goValue(t *testing.T, typ tftypes.Type, v interface{}) tftypes.Value {
	t.Helper()
	if v == nil {
		return tftypes.NewValue(typ, nil)
	}
	switch x := v.(type) {
	case tftypes.Value:
		return x
	case string:
		return tftypes.NewValue(typ, x)
	case bool:
		return tftypes.NewValue(typ, x)
	case int:
		return tftypes.NewValue(typ, new(big.Float).SetInt64(int64(x)))
	case float64:
		return tftypes.NewValue(typ, big.NewFloat(x))
	case []string:
		var elem tftypes.Type
		switch c := typ.(type) {
		case tftypes.List:
			elem = c.ElementType
		case tftypes.Set:
			elem = c.ElementType
		default:
			t.Fatalf("[]string for %s", typ)
		}
		vals := make([]tftypes.Value, 0, len(x))
		for _, s := range x {
			vals = append(vals, tftypes.NewValue(elem, s))
		}
		return tftypes.NewValue(typ, vals)
	case map[string]string:
		elem := typ.(tftypes.Map).ElementType
		vals := map[string]tftypes.Value{}
		for k, s := range x {
			vals[k] = tftypes.NewValue(elem, s)
		}
		return tftypes.NewValue(typ, vals)
	}
	t.Fatalf("unsupported Go value %T", v)
	return tftypes.Value{}
}

// object builds a configuration object for a schema: unset attributes are null.
func (h *tf) object(s *tfprotov6.Schema, c cfg) tftypes.Value {
	h.t.Helper()
	typ := s.ValueType().(tftypes.Object)
	vals := map[string]tftypes.Value{}
	for name, at := range typ.AttributeTypes {
		vals[name] = goValue(h.t, at, c[name])
	}
	for name := range c {
		if _, ok := typ.AttributeTypes[name]; !ok {
			h.t.Fatalf("no attribute %q in the schema", name)
		}
	}
	return tftypes.NewValue(typ, vals)
}

func attrs(t *testing.T, v tftypes.Value) map[string]tftypes.Value {
	t.Helper()
	if v.IsNull() {
		return nil
	}
	m := map[string]tftypes.Value{}
	if err := v.As(&m); err != nil {
		t.Fatal(err)
	}
	return m
}

// proposed is Terraform's proposed new state: the configuration, with computed attributes the configuration leaves
// null taken from the prior state.
func (h *tf) proposed(s *tfprotov6.Schema, prior, config tftypes.Value) tftypes.Value {
	if config.IsNull() {
		return config
	}
	if prior.IsNull() {
		return config
	}
	p, c := attrs(h.t, prior), attrs(h.t, config)
	computed := map[string]bool{}
	for _, a := range s.Block.Attributes {
		computed[a.Name] = a.Computed
	}
	out := map[string]tftypes.Value{}
	for k, v := range c {
		if v.IsNull() && computed[k] {
			out[k] = p[k]
		} else {
			out[k] = v
		}
	}
	return tftypes.NewValue(config.Type(), out)
}

// state is a resource's state as the harness keeps it.
type state struct {
	typeName string
	value    tftypes.Value
	private  []byte
}

// get returns one attribute of the state as a Go value (string, int64, float64, bool, []string, map[string]string or nil).
func (s *state) get(t *testing.T, name string) interface{} {
	t.Helper()
	v, ok := attrs(t, s.value)[name]
	if !ok {
		t.Fatalf("%s has no attribute %q", s.typeName, name)
	}
	return plain(t, v)
}

func (s *state) str(t *testing.T, name string) string {
	t.Helper()
	v, _ := s.get(t, name).(string)
	return v
}

func plain(t *testing.T, v tftypes.Value) interface{} {
	t.Helper()
	if v.IsNull() || !v.IsKnown() {
		return nil
	}
	switch {
	case v.Type().Is(tftypes.String):
		var s string
		_ = v.As(&s)
		return s
	case v.Type().Is(tftypes.Bool):
		var b bool
		_ = v.As(&b)
		return b
	case v.Type().Is(tftypes.Number):
		var f big.Float
		_ = v.As(&f)
		if f.IsInt() {
			i, _ := f.Int64()
			return i
		}
		x, _ := f.Float64()
		return x
	case v.Type().Is(tftypes.List{}) || v.Type().Is(tftypes.Set{}):
		var elems []tftypes.Value
		_ = v.As(&elems)
		out := []string{}
		for _, e := range elems {
			if s, ok := plain(t, e).(string); ok {
				out = append(out, s)
			}
		}
		sort.Strings(out)
		return out
	case v.Type().Is(tftypes.Map{}):
		var elems map[string]tftypes.Value
		_ = v.As(&elems)
		out := map[string]string{}
		for k, e := range elems {
			s, _ := plain(t, e).(string)
			out[k] = s
		}
		return out
	}
	return v.String()
}

// planResult is what PlanResourceChange said.
type planResult struct {
	planned   tftypes.Value
	private   []byte
	replace   []string
	diags     []*tfprotov6.Diagnostic
	unchanged bool
}

func (h *tf) plan(typeName string, prior *state, c cfg) planResult {
	h.t.Helper()
	s := h.resources[typeName]
	if s == nil {
		h.t.Fatalf("no resource type %s", typeName)
	}
	typ := s.ValueType()
	config := tftypes.NewValue(typ, nil)
	if c != nil {
		config = h.object(s, c)
		vr, err := h.srv.ValidateResourceConfig(h.ctx, &tfprotov6.ValidateResourceConfigRequest{TypeName: typeName, Config: dyn(h.t, typ, config)})
		if err != nil {
			h.t.Fatal(err)
		}
		if hasError(vr.Diagnostics) {
			return planResult{diags: vr.Diagnostics}
		}
	}
	priorV := tftypes.NewValue(typ, nil)
	var priv []byte
	if prior != nil {
		priorV, priv = prior.value, prior.private
	}
	resp, err := h.srv.PlanResourceChange(h.ctx, &tfprotov6.PlanResourceChangeRequest{
		TypeName: typeName, PriorState: dyn(h.t, typ, priorV), ProposedNewState: dyn(h.t, typ, h.proposed(s, priorV, config)),
		Config: dyn(h.t, typ, config), PriorPrivate: priv,
	})
	if err != nil {
		h.t.Fatal(err)
	}
	out := planResult{diags: resp.Diagnostics, private: resp.PlannedPrivate}
	if hasError(resp.Diagnostics) {
		return out
	}
	out.planned = undyn(h.t, typ, resp.PlannedState)
	for _, p := range resp.RequiresReplace {
		out.replace = append(out.replace, p.String())
	}
	out.unchanged = out.planned.Equal(priorV)
	return out
}

// apply plans and applies c (nil destroys). With a replacement planned it destroys and creates, as Terraform does.
func (h *tf) apply(typeName string, prior *state, c cfg) (*state, planResult) {
	h.t.Helper()
	p := h.plan(typeName, prior, c)
	if hasError(p.diags) {
		return prior, p
	}
	if len(p.replace) > 0 && prior != nil && c != nil {
		if _, d := h.applyPlanned(typeName, prior, tftypes.NewValue(prior.value.Type(), nil), nil, nil); hasError(d) {
			p.diags = d
			return prior, p
		}
		p2 := h.plan(typeName, nil, c)
		if hasError(p2.diags) {
			return nil, p2
		}
		st, d := h.applyPlanned(typeName, nil, p2.planned, p2.private, c)
		p2.diags, p2.replace = d, p.replace
		return st, p2
	}
	if p.unchanged && prior != nil {
		return prior, p
	}
	st, d := h.applyPlanned(typeName, prior, p.planned, p.private, c)
	p.diags = append(p.diags, d...)
	return st, p
}

func (h *tf) applyPlanned(typeName string, prior *state, planned tftypes.Value, priv []byte, c cfg) (*state, []*tfprotov6.Diagnostic) {
	h.t.Helper()
	s := h.resources[typeName]
	typ := s.ValueType()
	priorV := tftypes.NewValue(typ, nil)
	if prior != nil {
		priorV = prior.value
	}
	config := tftypes.NewValue(typ, nil)
	if c != nil {
		config = h.object(s, c)
	}
	resp, err := h.srv.ApplyResourceChange(h.ctx, &tfprotov6.ApplyResourceChangeRequest{
		TypeName: typeName, PriorState: dyn(h.t, typ, priorV), PlannedState: dyn(h.t, typ, planned),
		Config: dyn(h.t, typ, config), PlannedPrivate: priv,
	})
	if err != nil {
		h.t.Fatal(err)
	}
	newV := undyn(h.t, typ, resp.NewState)
	if !hasError(resp.Diagnostics) && !newV.IsNull() {
		checkConsistent(h.t, typeName, planned, newV)
	}
	if newV.IsNull() {
		return nil, resp.Diagnostics
	}
	return &state{typeName: typeName, value: newV, private: resp.Private}, resp.Diagnostics
}

// checkConsistent is Terraform's "Provider produced inconsistent result after apply": every known planned value
// must come back unchanged.
func checkConsistent(t *testing.T, typeName string, planned, got tftypes.Value) {
	t.Helper()
	p, g := attrs(t, planned), attrs(t, got)
	for k, pv := range p {
		if !pv.IsFullyKnown() {
			if !g[k].IsFullyKnown() {
				t.Fatalf("%s.%s is still unknown after apply", typeName, k)
			}
			continue
		}
		if !pv.Equal(g[k]) {
			t.Fatalf("%s.%s: planned %s, applied %s (inconsistent result after apply)", typeName, k, pv, g[k])
		}
	}
}

func (h *tf) read(st *state) (*state, []*tfprotov6.Diagnostic) {
	h.t.Helper()
	typ := h.resources[st.typeName].ValueType()
	resp, err := h.srv.ReadResource(h.ctx, &tfprotov6.ReadResourceRequest{TypeName: st.typeName,
		CurrentState: dyn(h.t, typ, st.value), Private: st.private})
	if err != nil {
		h.t.Fatal(err)
	}
	v := undyn(h.t, typ, resp.NewState)
	if v.IsNull() {
		return nil, resp.Diagnostics
	}
	return &state{typeName: st.typeName, value: v, private: resp.Private}, resp.Diagnostics
}

func (h *tf) importState(typeName, id string) (*state, []*tfprotov6.Diagnostic) {
	h.t.Helper()
	resp, err := h.srv.ImportResourceState(h.ctx, &tfprotov6.ImportResourceStateRequest{TypeName: typeName, ID: id})
	if err != nil {
		h.t.Fatal(err)
	}
	if hasError(resp.Diagnostics) || len(resp.ImportedResources) != 1 {
		return nil, resp.Diagnostics
	}
	ir := resp.ImportedResources[0]
	st := &state{typeName: typeName, value: undyn(h.t, h.resources[typeName].ValueType(), ir.State), private: ir.Private}
	return h.read(st)
}

func (h *tf) dataSource(typeName string, c cfg) (*state, []*tfprotov6.Diagnostic) {
	h.t.Helper()
	s := h.data[typeName]
	if s == nil {
		h.t.Fatalf("no data source %s", typeName)
	}
	typ := s.ValueType()
	resp, err := h.srv.ReadDataSource(h.ctx, &tfprotov6.ReadDataSourceRequest{TypeName: typeName, Config: dyn(h.t, typ, h.object(s, c))})
	if err != nil {
		h.t.Fatal(err)
	}
	if hasError(resp.Diagnostics) {
		return nil, resp.Diagnostics
	}
	return &state{typeName: typeName, value: undyn(h.t, typ, resp.State)}, resp.Diagnostics
}

// mustApply applies and fails the test on an error.
func (h *tf) mustApply(typeName string, prior *state, c cfg) (*state, planResult) {
	h.t.Helper()
	st, p := h.apply(typeName, prior, c)
	failOnDiags(h.t, "apply "+typeName, p.diags)
	return st, p
}

// noChanges asserts that planning c against st proposes nothing.
func (h *tf) noChanges(st *state, c cfg) {
	h.t.Helper()
	p := h.plan(st.typeName, st, c)
	failOnDiags(h.t, "plan "+st.typeName, p.diags)
	if !p.unchanged {
		h.t.Fatalf("%s: expected no changes, plan differs:\nprior   %s\nplanned %s", st.typeName, st.value, p.planned)
	}
}
