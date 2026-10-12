// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package fleet_test

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/zyvorai/fluxvm/go/fleet"
	"github.com/zyvorai/fluxvm/go/fleet/fleettest"
)

func fast(token string) []fleet.Option {
	return []fleet.Option{fleet.WithToken(token), fleet.WithRetryDelay(time.Millisecond, 5*time.Millisecond)}
}

func TestTenantLifecycleAndKeys(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	ctx := context.Background()
	op := fleet.NewClient(srv.URL, fast(fleettest.OperatorToken)...)

	created, err := op.CreateTenant(ctx, fleet.CreateTenantRequest{ID: "acme", DisplayName: fleet.String("Acme"),
		Quota: fleet.Quota{MaxVMs: fleet.Uint32(5)}})
	if err != nil {
		t.Fatal(err)
	}
	if created.Token == "" || created.Tenant.ID != "acme" || *created.Tenant.Quota.MaxVMs != 5 {
		t.Fatalf("created = %+v", created)
	}
	if _, err := op.CreateTenant(ctx, fleet.CreateTenantRequest{ID: "acme"}); !errors.Is(err, fleet.ErrConflict) {
		t.Fatalf("duplicate tenant: %v", err)
	}

	acme := fleet.NewClient(srv.URL, fast(created.Token)...)
	me, err := acme.Me(ctx)
	if err != nil || me.ID != "acme" || me.InUse == nil {
		t.Fatalf("me = %+v, %v", me, err)
	}
	// A tenant cannot reach operator routes; the error carries central's status and message.
	_, err = acme.ListTenants(ctx)
	var ae *fleet.APIError
	if !errors.As(err, &ae) || ae.Status != 403 || !strings.Contains(ae.Message, "may not call") {
		t.Fatalf("tenant listing tenants: %v", err)
	}

	k, err := acme.CreateKey(ctx, fleet.Me, fleet.CreateKeyRequest{Name: "ci", ExpiresInDays: fleet.Int(30)})
	if err != nil || k.Token == "" || k.Key.ExpiresAt == nil {
		t.Fatalf("key = %+v, %v", k, err)
	}
	ro, err := acme.CreateKey(ctx, fleet.Me, fleet.CreateKeyRequest{Name: "grafana", ReadOnly: true})
	if err != nil {
		t.Fatal(err)
	}
	reader := fleet.NewClient(srv.URL, fast(ro.Token)...)
	if _, err := reader.ListVMs(ctx); err != nil {
		t.Fatalf("read-only key reading: %v", err)
	}
	if _, err := reader.CreateLease(ctx, fleet.CreateLeaseRequest{}); !errors.Is(err, fleet.ErrForbidden) {
		t.Fatalf("read-only key writing: %v", err)
	}
	got, err := acme.GetKey(ctx, fleet.Me, "ci")
	if err != nil || got.ID != k.Key.ID {
		t.Fatalf("GetKey = %+v, %v", got, err)
	}
	if _, err := acme.RevokeKey(ctx, fleet.Me, "ci"); err != nil {
		t.Fatal(err)
	}
	if _, err := acme.GetKey(ctx, fleet.Me, "ci"); !fleet.IsNotFound(err) {
		t.Fatalf("revoked key still listed: %v", err)
	}

	upd, err := op.UpdateTenant(ctx, "acme", fleet.UpdateTenantRequest{Disabled: fleet.Bool(true)})
	if err != nil || !upd.Disabled {
		t.Fatalf("update = %+v, %v", upd, err)
	}
	if _, err := acme.Me(ctx); !errors.Is(err, fleet.ErrForbidden) {
		t.Fatalf("disabled tenant: %v", err)
	}
	if err := op.DeleteTenant(ctx, "acme"); err != nil {
		t.Fatal(err)
	}
	if _, err := op.GetTenant(ctx, "acme"); !fleet.IsNotFound(err) {
		t.Fatalf("deleted tenant: %v", err)
	}
}

func TestLeaseLaunchExecAndDelete(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	srv.PutImage("macos-27-xcode", "macos", "/Users/fluxvm/vms/xcode/disk.raw", 6, 16384)
	ctx := context.Background()
	tok := srv.AddTenant("acme")
	c := fleet.NewClient(srv.URL, fast(tok)...)

	// macOS needs a lease first.
	_, err := c.LaunchVM(ctx, fleet.LaunchRequest{Name: "early", Image: "/x", Apple: &fleet.AppleOptions{GuestOS: "macos"}})
	if !errors.Is(err, fleet.ErrUnavailable) {
		t.Fatalf("launch without a lease: %v", err)
	}
	lease, err := c.CreateLease(ctx, fleet.CreateLeaseRequest{Hours: fleet.Int64(48), NestedVirtualization: true})
	if err != nil {
		t.Fatal(err)
	}
	if lease.Lease.Node != "mac-2" || lease.Lease.State != fleet.LeaseActive || lease.Lease.EndsAt == nil {
		t.Fatalf("lease = %+v", lease.Lease)
	}
	if n, err := c.GetNode(ctx, "mac-2"); err != nil || n.Apple == nil || !n.Apple.NestedVirtualization || n.FluxvmURL != "" {
		t.Fatalf("tenant view of its Mac = %+v, %v", n, err)
	}
	if _, err := c.GetNode(ctx, "mac-1"); !fleet.IsNotFound(err) {
		t.Fatalf("tenant view of another Mac: %v", err)
	}

	img, err := c.GetImage(ctx, "macos-27-xcode")
	if err != nil {
		t.Fatal(err)
	}
	vol, err := c.CreateVolume(ctx, fleet.CreateVolumeRequest{Name: "cache", SizeGiB: 50})
	if err != nil {
		t.Fatal(err)
	}
	res, err := c.LaunchVM(ctx, fleet.LaunchRequest{
		Name: "ci-1", Image: img.Image, VCPUs: img.VCPUs, MemoryMiB: img.MemoryMiB,
		Apple:   &fleet.AppleOptions{GuestOS: img.GuestOS},
		Volumes: []string{vol.Name},
		Extra:   map[string]interface{}{"labels": map[string]string{"job": "42"}, "name": "ignored"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if res.Node != "mac-2" || res.VM.Name != "ci-1" || res.VM.Backend != "vz" || res.VM.Request["tenant"] != "acme" {
		t.Fatalf("launch = %+v", res)
	}
	if res.VM.Request["labels"] == nil {
		t.Fatal("Extra was not merged into the request")
	}
	vm, err := c.GetVM(ctx, res.Node, res.VM.ID)
	if err != nil || vm.GuestOS() != "macos" || vm.VCPUs() != 6 || vm.MemoryMiB() != 16384 {
		t.Fatalf("GetVM = %+v, %v", vm, err)
	}
	if v, err := c.FindVM(ctx, "ci-1"); err != nil || v.ID != vm.ID {
		t.Fatalf("FindVM = %+v, %v", v, err)
	}
	out, err := c.Exec(ctx, vm.Node, vm.ID, fleet.ExecRequest{Command: "sw_vers"})
	if err != nil || out.ExitCode != 0 || !strings.Contains(out.Stdout, "sw_vers") || len(out.Raw) == 0 {
		t.Fatalf("exec = %+v, %v", out, err)
	}
	if v, err := c.PowerVM(ctx, vm.Node, vm.ID, fleet.PowerStop); err != nil || v.Status != "stopped" {
		t.Fatalf("stop = %+v, %v", v, err)
	}
	if _, err := c.PowerVM(ctx, vm.Node, vm.ID, "explode"); err == nil {
		t.Fatal("unknown power action accepted")
	}
	if logs, err := c.VMLogs(ctx, vm.Node, vm.ID, 10); err != nil || !strings.Contains(logs, "login:") {
		t.Fatalf("logs = %q, %v", logs, err)
	}
	if v, _ := c.GetVolume(ctx, "cache"); v == nil || v.AttachedTo == nil || *v.AttachedTo != vm.ID {
		t.Fatalf("volume not attached: %+v", v)
	}
	if err := c.DeleteVolume(ctx, "cache"); !errors.Is(err, fleet.ErrConflict) {
		t.Fatalf("deleting an attached volume: %v", err)
	}
	if err := c.DeleteVM(ctx, vm.Node, vm.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetVM(ctx, vm.Node, vm.ID); !fleet.IsNotFound(err) {
		t.Fatalf("deleted VM: %v", err)
	}
	if v, err := c.ResizeVolume(ctx, "cache", 80); err != nil || v.SizeGiB != 80 {
		t.Fatalf("resize = %+v, %v", v, err)
	}
	if err := c.DeleteVolume(ctx, "cache"); err != nil {
		t.Fatal(err)
	}

	rel, err := c.ReleaseLease(ctx, lease.Lease.ID, false)
	if err != nil || rel.Lease.State != fleet.LeaseReleased || !rel.ScrubRequired {
		t.Fatalf("release = %+v, %v", rel, err)
	}
	if ls, err := c.ListLeases(ctx, fleet.ListLeasesOptions{State: fleet.LeaseActive}); err != nil || len(ls) != 0 {
		t.Fatalf("active leases = %+v, %v", ls, err)
	}
	evs, err := c.Events(ctx, fleet.EventsOptions{Limit: 50})
	if err != nil || len(evs) == 0 || evs[0].Actor != "acme" {
		t.Fatalf("events = %+v, %v", evs, err)
	}
}

func TestNetworksWebhooksBillingPools(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	ctx := context.Background()
	tok := srv.AddTenant("acme")
	c := fleet.NewClient(srv.URL, fast(tok)...)
	if _, err := c.CreateLease(ctx, fleet.CreateLeaseRequest{}); err != nil {
		t.Fatal(err)
	}
	n, err := c.CreateNetwork(ctx, fleet.CreateNetworkRequest{Name: "build"})
	if err != nil || n.Subnet == "" || len(n.Members) != 1 || n.Tenant == nil || *n.Tenant != "acme" {
		t.Fatalf("network = %+v, %v", n, err)
	}
	if _, err := c.CreateNetwork(ctx, fleet.CreateNetworkRequest{Name: "Bad Name"}); StatusOf(err) != 400 {
		t.Fatalf("bad network name: %v", err)
	}
	if n, err = c.RotateNetworkKey(ctx, "build"); err != nil || n.KeyEpoch != 1 {
		t.Fatalf("rotate = %+v, %v", n, err)
	}
	if err := c.DeleteNetwork(ctx, "build"); err != nil {
		t.Fatal(err)
	}

	h, err := c.CreateWebhook(ctx, fleet.CreateWebhookRequest{URL: "https://hooks.example.com/x", Events: []string{"lease.ending"}})
	if err != nil || !strings.HasPrefix(h.Secret, "whsec_") || h.Webhook.ID == "" {
		t.Fatalf("webhook = %+v, %v", h, err)
	}
	if _, err := c.CreateWebhook(ctx, fleet.CreateWebhookRequest{URL: "http://10.0.0.1/"}); StatusOf(err) != 400 {
		t.Fatalf("tenant http webhook: %v", err)
	}
	if d, err := c.TestWebhook(ctx, h.Webhook.ID); err != nil || !d.OK {
		t.Fatalf("test = %+v, %v", d, err)
	}
	list, err := c.ListWebhooks(ctx)
	if err != nil || len(list.Items) != 1 || len(list.Kinds) == 0 {
		t.Fatalf("list = %+v, %v", list, err)
	}
	raw, _ := json.Marshal(list.Items[0])
	if strings.Contains(string(raw), "whsec_") {
		t.Fatal("the list leaked the secret")
	}
	if err := c.DeleteWebhook(ctx, h.Webhook.ID); err != nil {
		t.Fatal(err)
	}

	if tn, err := c.SetBudget(ctx, fleet.Me, fleet.Float64(500)); err != nil || tn.MonthlyBudget == nil || *tn.MonthlyBudget != 500 {
		t.Fatalf("budget = %+v, %v", tn, err)
	}
	b, err := c.Billing(ctx, fleet.BillingOptions{})
	if err != nil || len(b.Statements) != 1 || b.Statements[0].Budget == nil || b.Statements[0].Tenant != "acme" {
		t.Fatalf("billing = %+v, %v", b, err)
	}
	if csv, err := c.BillingCSV(ctx, fleet.BillingOptions{Month: "2026-10"}); err != nil || !strings.HasPrefix(csv, "tenant,") {
		t.Fatalf("csv = %q, %v", csv, err)
	}
	if p, err := c.Prices(ctx); err != nil || p.Currency != "USD" {
		t.Fatalf("prices = %+v, %v", p, err)
	}
	if _, err := c.SetPrices(ctx, fleet.PriceList{Currency: "EUR"}); !errors.Is(err, fleet.ErrForbidden) {
		t.Fatalf("tenant setting prices: %v", err)
	}

	srv.PutImage("macos-27-xcode", "macos", "/vms/xcode.raw", 6, 16384)
	pool, err := c.CreatePool(ctx, fleet.CreatePoolRequest{Name: "ci", Image: "macos-27-xcode", MinReady: 1, MaxTotal: fleet.Int(2)})
	if err != nil || pool.MinReady != 1 || pool.MaxTotal == nil || *pool.MaxTotal != 2 {
		t.Fatalf("pool = %+v, %v", pool, err)
	}
	// Central's shape: the size and the counts are nested; the client fills the flat fields from them.
	if pool.Size == nil || pool.VCPUs == nil || *pool.VCPUs != 6 || pool.MemoryMiB == nil || *pool.MemoryMiB != 16384 ||
		pool.Status == nil || pool.Owner != "acme" {
		t.Fatalf("pool shape = %+v", pool)
	}
	if _, err := c.UpdatePool(ctx, "ci", fleet.UpdatePoolRequest{VCPUs: fleet.Int(2)}); err == nil {
		t.Fatal("a pool's size changed")
	}
	if pool, err = c.UpdatePool(ctx, "ci", fleet.UpdatePoolRequest{MinReady: fleet.Int(2)}); err != nil || pool.MinReady != 2 {
		t.Fatalf("update pool = %+v, %v", pool, err)
	}
	if err := c.DeletePool(ctx, "ci"); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetPool(ctx, "ci"); !fleet.IsNotFound(err) {
		t.Fatalf("deleted pool: %v", err)
	}
}

func StatusOf(err error) int { return fleet.StatusOf(err) }

func TestRetriesIdempotentCallsOnly(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	ctx := context.Background()
	c := fleet.NewClient(srv.URL, fast(fleettest.OperatorToken)...)

	srv.Fail("GET", "/fleet/images", 503, 2)
	if _, err := c.ListImages(ctx); err != nil {
		t.Fatalf("GET after two 503s: %v", err)
	}
	if n := srv.Count("GET /fleet/images"); n != 3 {
		t.Fatalf("GET tried %d times, want 3", n)
	}
	srv.Fail("GET", "/fleet/capacity", 429, 10)
	if _, err := c.Capacity(ctx); !errors.Is(err, fleet.ErrQuota) {
		t.Fatalf("GET after persistent 429s: %v", err)
	}
	if n := srv.Count("GET /fleet/capacity"); n != 4 {
		t.Fatalf("GET tried %d times, want 4 (1 + 3 retries)", n)
	}
	// A POST is never retried: it may have taken effect.
	srv.Fail("POST", "/fleet/leases", 502, 1)
	if _, err := c.CreateLease(ctx, fleet.CreateLeaseRequest{Tenant: "x"}); fleet.StatusOf(err) != 502 {
		t.Fatalf("POST: %v", err)
	}
	if n := srv.Count("POST /fleet/leases"); n != 1 {
		t.Fatalf("POST tried %d times, want 1", n)
	}
	noRetry := fleet.NewClient(srv.URL, fleet.WithToken(fleettest.OperatorToken), fleet.WithRetries(0))
	srv.Fail("GET", "/fleet/policy", 500, 1)
	if _, err := noRetry.Policy(ctx); fleet.StatusOf(err) != 500 {
		t.Fatalf("WithRetries(0): %v", err)
	}
}

func TestAuthHeadersErrorsAndContext(t *testing.T) {
	var seen atomic.Value
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seen.Store(r.Header.Get("Authorization") + "|" + r.Header.Get("User-Agent"))
		switch r.URL.Path {
		case "/fleet/tenants/me":
			w.WriteHeader(401)
			_, _ = w.Write([]byte(`{"error":"missing or invalid bearer token"}`))
		case "/fleet/images":
			w.WriteHeader(500)
			_, _ = w.Write([]byte("plain text failure"))
		case "/fleet/capacity":
			time.Sleep(200 * time.Millisecond)
			_, _ = w.Write([]byte(`{}`))
		}
	}))
	defer ts.Close()
	c := fleet.NewClient(ts.URL+"/", fleet.WithToken("tok"), fleet.WithRetries(0), fleet.WithUserAgent("tf/1"))
	ctx := context.Background()
	_, err := c.Me(ctx)
	if !errors.Is(err, fleet.ErrUnauthorized) || !strings.Contains(err.Error(), "invalid bearer token") {
		t.Fatalf("401: %v", err)
	}
	if got := seen.Load().(string); got != "Bearer tok|tf/1" {
		t.Fatalf("headers = %q", got)
	}
	_, err = c.ListImages(ctx)
	var ae *fleet.APIError
	if !errors.As(err, &ae) || ae.Status != 500 || ae.Message != "plain text failure" {
		t.Fatalf("500: %v", err)
	}
	short, cancel := context.WithTimeout(ctx, 20*time.Millisecond)
	defer cancel()
	if _, err := c.Capacity(short); !errors.Is(err, fleet.ErrTimeout) && !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("deadline: %v", err)
	}
	if _, err := fleet.NewClient("").Capacity(ctx); err == nil {
		t.Fatal("empty URL accepted")
	}
}

func TestLaunchRequestJSON(t *testing.T) {
	b, err := json.Marshal(fleet.LaunchRequest{Name: "a", Image: "/d", Node: "mac-1", NodeSelector: map[string]string{"zone": "a"},
		Extra: map[string]interface{}{"ttl_seconds": 60, "image": "/other"}})
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]interface{}
	_ = json.Unmarshal(b, &m)
	if m["backend"] != "vz" || m["image"] != "/d" || m["ttl_seconds"] != float64(60) || m["node"] != "mac-1" ||
		m["nodeSelector"].(map[string]interface{})["zone"] != "a" {
		t.Fatalf("json = %s", b)
	}
	if _, has := m["volumes"]; has {
		t.Fatalf("empty volumes sent: %s", b)
	}
}
