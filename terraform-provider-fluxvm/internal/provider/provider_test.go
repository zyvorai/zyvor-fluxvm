// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

package provider

import (
	"strings"
	"testing"

	"github.com/zyvorai/fluxvm/go/fleet/fleettest"
)

func contains2(list []string, s string) bool {
	for _, x := range list {
		if strings.Contains(x, s) {
			return true
		}
	}
	return false
}

// The examples/main.tf stack, as a tenant: lease a Mac, make a volume and a network, launch a macOS VM from the
// catalog with both, add a webhook; then change things in place and by replacement, import, and destroy.
func TestAccMacCIStack(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	srv.PutImage("macos-27-xcode", "macos", "/Users/fluxvm/vms/macos-27-xcode/disk.raw", 6, 16384)
	tok := srv.AddTenant("acme")
	h := newTF(t, cfg{"endpoint": srv.URL, "token": tok})

	leaseCfg := cfg{"hours": 48, "nested_virtualization": true}
	lease, _ := h.mustApply("fluxvm_lease", nil, leaseCfg)
	if lease.str(t, "node") != "mac-2" || lease.str(t, "state") != "active" || lease.str(t, "ends_at") == "" || lease.str(t, "tenant") != "acme" {
		t.Fatalf("lease = %s", lease.value)
	}
	h.noChanges(lease, leaseCfg)

	volCfg := cfg{"name": "cache", "size_gib": 50}
	vol, _ := h.mustApply("fluxvm_volume", nil, volCfg)
	if vol.str(t, "node") != "mac-2" || vol.str(t, "tenant") != "acme" {
		t.Fatalf("volume = %s", vol.value)
	}

	netCfg := cfg{"name": "build"}
	net, _ := h.mustApply("fluxvm_network", nil, netCfg)
	if got := net.get(t, "nodes").([]string); len(got) != 1 || got[0] != "mac-2" || net.str(t, "subnet") == "" {
		t.Fatalf("network = %s", net.value)
	}
	h.noChanges(net, netCfg)

	vmCfg := cfg{"name": "ci-1", "catalog_image": "macos-27-xcode", "networks": []string{"build"}, "volumes": []string{"cache"},
		"extra_json": `{"labels": {"job": "42"}}`}
	vm, _ := h.mustApply("fluxvm_vm", nil, vmCfg)
	if vm.str(t, "node") != "mac-2" || vm.str(t, "guest_os") != "macos" || vm.get(t, "vcpus") != int64(6) ||
		vm.get(t, "memory_mib") != int64(16384) || vm.str(t, "power_state") != "running" || vm.str(t, "tenant") != "acme" ||
		vm.str(t, "image") != "/Users/fluxvm/vms/macos-27-xcode/disk.raw" || !strings.HasPrefix(vm.str(t, "id"), "mac-2/") {
		t.Fatalf("vm = %s", vm.value)
	}
	vm, d := h.read(vm)
	failOnDiags(t, "read vm", d)
	h.noChanges(vm, vmCfg)
	if v, _ := h.read(vol); v.str(t, "attached_to") != vm.str(t, "vm_id") {
		t.Fatalf("volume not attached to the VM: %s", v.value)
	}

	hookCfg := cfg{"url": "https://hooks.example.com/fluxvm", "events": []string{"lease.ending", "budget.over"}}
	hook, _ := h.mustApply("fluxvm_webhook", nil, hookCfg)
	if !strings.HasPrefix(hook.str(t, "secret"), "whsec_") || hook.str(t, "owner") != "acme" {
		t.Fatalf("webhook = %s", hook.value)
	}
	hook, _ = h.read(hook)
	if !strings.HasPrefix(hook.str(t, "secret"), "whsec_") {
		t.Fatal("refresh lost the webhook secret")
	}
	h.noChanges(hook, hookCfg)

	// In place: stop the VM.
	stopped := cfg{}
	for k, v := range vmCfg {
		stopped[k] = v
	}
	stopped["power_state"] = "stopped"
	creates := srv.Count("POST /fleet/vms")
	vm2, p := h.mustApply("fluxvm_vm", vm, stopped)
	if len(p.replace) != 0 || vm2.str(t, "power_state") != "stopped" || vm2.str(t, "vm_id") != vm.str(t, "vm_id") {
		t.Fatalf("power change: replace=%v state=%s", p.replace, vm2.value)
	}
	if srv.Count("POST /fleet/vms") != creates || srv.Count("POST /fleet/vms/mac-2/"+vm.str(t, "vm_id")+"/stop") != 1 {
		t.Fatalf("power change made the wrong calls: %v", srv.Requests)
	}
	vm = vm2
	vmCfg = stopped

	// Replacement: a different size is a new VM.
	bigger := cfg{}
	for k, v := range vmCfg {
		bigger[k] = v
	}
	bigger["vcpus"] = 8
	p = h.plan("fluxvm_vm", vm, bigger)
	failOnDiags(t, "plan", p.diags)
	if !contains2(p.replace, "vcpus") {
		t.Fatalf("vcpus change should replace the VM, replace=%v", p.replace)
	}

	// The volume grows in place and is replaced to shrink.
	vol, _ = h.read(vol)
	vol2, p := h.mustApply("fluxvm_volume", vol, cfg{"name": "cache", "size_gib": 80})
	if len(p.replace) != 0 || vol2.get(t, "size_gib") != int64(80) || srv.Count("POST /fleet/volumes/cache/resize") != 1 {
		t.Fatalf("grow: replace=%v state=%s", p.replace, vol2.value)
	}
	vol = vol2
	if p := h.plan("fluxvm_volume", vol, cfg{"name": "cache", "size_gib": 10}); !contains2(p.replace, "size_gib") {
		t.Fatalf("shrinking should replace, replace=%v diags=%s", p.replace, diagText(p.diags))
	}

	// Imports read back to the same state the configuration describes.
	for _, c := range []struct {
		typ, id string
		cfg     cfg
	}{
		{"fluxvm_vm", vm.str(t, "id"), vmCfg},
		{"fluxvm_volume", "cache", cfg{"name": "cache", "size_gib": 80}},
		{"fluxvm_network", "build", netCfg},
		{"fluxvm_lease", lease.str(t, "id"), cfg{"nested_virtualization": false}},
		{"fluxvm_webhook", hook.str(t, "id"), hookCfg},
	} {
		got, d := h.importState(c.typ, c.id)
		failOnDiags(t, "import "+c.typ, d)
		if got == nil {
			t.Fatalf("import %s %s found nothing", c.typ, c.id)
		}
		if c.typ == "fluxvm_vm" {
			// catalog_image and extra_json are not stored by central; after an import they come from configuration.
			if p := h.plan(c.typ, got, cfg{"name": "ci-1", "image": got.str(t, "image"), "guest_os": "macos",
				"networks": nil, "power_state": "stopped"}); hasError(p.diags) || len(p.replace) != 0 {
				t.Fatalf("imported VM plan: replace=%v %s", p.replace, diagText(p.diags))
			}
			continue
		}
		if c.typ == "fluxvm_lease" {
			if p := h.plan(c.typ, got, c.cfg); hasError(p.diags) || len(p.replace) != 0 {
				t.Fatalf("imported lease plan: replace=%v %s", p.replace, diagText(p.diags))
			}
			continue
		}
		h.noChanges(got, c.cfg)
	}

	// Destroy in dependency order.
	for _, st := range []*state{hook, vm, vol, net, lease} {
		if out, p := h.apply(st.typeName, st, nil); out != nil || hasError(p.diags) {
			t.Fatalf("destroy %s: %s", st.typeName, diagText(p.diags))
		}
	}
	if left, _ := h.read(lease); left != nil {
		t.Fatalf("lease still active after destroy: %s", left.value)
	}
	if left, _ := h.read(vm); left != nil {
		t.Fatal("VM still there after destroy")
	}
}

func TestAccOperatorTenantAndKeys(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	h := newTF(t, cfg{"endpoint": srv.URL, "token": fleettest.OperatorToken})

	tc := cfg{"id": "acme", "display_name": "Acme Mobile", "max_vms": 20, "max_leased_hosts": 2}
	ten, _ := h.mustApply("fluxvm_tenant", nil, tc)
	tok := ten.str(t, "token")
	if !strings.HasPrefix(tok, "fvt_") || ten.get(t, "disabled") != false {
		t.Fatalf("tenant = %s", ten.value)
	}
	h.noChanges(ten, tc)
	tc["max_vms"] = 30
	ten2, p := h.mustApply("fluxvm_tenant", ten, tc)
	if len(p.replace) != 0 || ten2.get(t, "max_vms") != int64(30) || ten2.str(t, "token") != tok {
		t.Fatalf("quota update: replace=%v %s", p.replace, ten2.value)
	}
	if p := h.plan("fluxvm_tenant", ten2, cfg{"id": "acme-2"}); !contains2(p.replace, "id") {
		t.Fatalf("renaming should replace: %v", p.replace)
	}

	kc := cfg{"tenant": "acme", "name": "ci-github", "expires_in_days": 90}
	key, _ := h.mustApply("fluxvm_api_key", nil, kc)
	if !strings.HasPrefix(key.str(t, "id"), "key_") || key.str(t, "token") == "" || key.str(t, "expires_at") == "" {
		t.Fatalf("key = %s", key.value)
	}
	if p := h.plan("fluxvm_api_key", key, cfg{"tenant": "acme", "name": "ci-github", "expires_in_days": 90, "read_only": true}); !contains2(p.replace, "read_only") {
		t.Fatalf("read_only change should replace: %v", p.replace)
	}
	imp, d := h.importState("fluxvm_api_key", "acme/"+key.str(t, "id"))
	failOnDiags(t, "import key", d)
	if imp.str(t, "name") != "ci-github" || imp.get(t, "token") != nil {
		t.Fatalf("imported key = %s", imp.value)
	}

	impT, d := h.importState("fluxvm_tenant", "acme")
	failOnDiags(t, "import tenant", d)
	h.noChanges(impT, tc)

	// The tenant can use its token through the provider too.
	ht := newTF(t, cfg{"endpoint": srv.URL, "token": tok})
	me, d := ht.dataSource("fluxvm_tenant", cfg{})
	failOnDiags(t, "tenant data source", d)
	if me.str(t, "id") != "acme" || me.get(t, "max_vms") != int64(30) || me.str(t, "display_name") != "Acme Mobile" {
		t.Fatalf("me = %s", me.value)
	}
	if _, p := ht.apply("fluxvm_tenant", nil, cfg{"id": "evil"}); !strings.Contains(diagText(p.diags), "403") {
		t.Fatalf("a tenant creating tenants: %s", diagText(p.diags))
	}

	if _, p := h.apply("fluxvm_api_key", key, nil); hasError(p.diags) {
		t.Fatal(diagText(p.diags))
	}
	if _, p := h.apply("fluxvm_tenant", ten2, nil); hasError(p.diags) {
		t.Fatal(diagText(p.diags))
	}
	if gone, _ := h.read(ten2); gone != nil {
		t.Fatal("tenant still there after destroy")
	}
}

func TestAccPool(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	h := newTF(t, cfg{"endpoint": srv.URL, "token": srv.AddTenant("acme")})
	srv.PutImage("macos-27-xcode", "macos", "/vms/xcode.raw", 6, 16384)
	pc := cfg{"name": "ci", "image": "macos-27-xcode", "min_ready": 1, "max_total": 4}
	pool, _ := h.mustApply("fluxvm_pool", nil, pc)
	if pool.str(t, "tenant") != "acme" || pool.get(t, "max_total") != int64(4) {
		t.Fatalf("pool = %s", pool.value)
	}
	h.noChanges(pool, pc)
	pc["min_ready"] = 2
	pool2, p := h.mustApply("fluxvm_pool", pool, pc)
	if len(p.replace) != 0 || pool2.get(t, "min_ready") != int64(2) || srv.Count("PATCH /fleet/pools/ci") != 1 {
		t.Fatalf("pool update: replace=%v %s", p.replace, pool2.value)
	}
	imp, d := h.importState("fluxvm_pool", "ci")
	failOnDiags(t, "import pool", d)
	if imp.get(t, "min_ready") != int64(2) {
		t.Fatalf("imported pool = %s", imp.value)
	}
	if p := h.plan("fluxvm_pool", pool2, cfg{"name": "ci", "image": "x", "min_ready": 3, "max_total": 2}); !hasError(p.diags) {
		t.Fatal("max_total below min_ready accepted")
	}
	if _, p := h.apply("fluxvm_pool", pool2, nil); hasError(p.diags) {
		t.Fatal(diagText(p.diags))
	}
}

func TestAccDataSources(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	srv.PutImage("macos-27-xcode", "macos", "/vms/xcode.raw", 6, 16384)
	srv.PutImage("ubuntu-24", "linux", "/vms/ubuntu.raw", 2, 4096)

	op := newTF(t, cfg{"endpoint": srv.URL, "token": fleettest.OperatorToken})
	nodes, d := op.dataSource("fluxvm_nodes", cfg{})
	failOnDiags(t, "nodes", d)
	if got := nodes.get(t, "names").([]string); len(got) != 2 {
		t.Fatalf("operator nodes = %v", got)
	}

	tok := srv.AddTenant("acme")
	ten := newTF(t, cfg{"endpoint": srv.URL, "token": tok})
	imgs, d := ten.dataSource("fluxvm_images", cfg{"guest_os": "macos"})
	failOnDiags(t, "images", d)
	if got := imgs.get(t, "names").([]string); len(got) != 1 || got[0] != "macos-27-xcode" {
		t.Fatalf("macOS images = %v", got)
	}
	all, _ := ten.dataSource("fluxvm_images", cfg{})
	if got := all.get(t, "names").([]string); len(got) != 2 {
		t.Fatalf("images = %v", got)
	}
	// A tenant sees only the Macs it leases.
	none, d := ten.dataSource("fluxvm_nodes", cfg{})
	failOnDiags(t, "tenant nodes", d)
	if got := none.get(t, "names").([]string); len(got) != 0 {
		t.Fatalf("tenant without a lease sees %v", got)
	}
	ten.mustApply("fluxvm_lease", nil, cfg{"node": "mac-1"})
	mine, _ := ten.dataSource("fluxvm_nodes", cfg{})
	if got := mine.get(t, "names").([]string); len(got) != 1 || got[0] != "mac-1" {
		t.Fatalf("tenant nodes = %v", got)
	}
	me, _ := ten.dataSource("fluxvm_tenant", cfg{})
	if me.get(t, "leased_hosts") != int64(1) || me.get(t, "leased_macs").([]string)[0] != "mac-1" {
		t.Fatalf("me = %s", me.value)
	}
}

func TestAccDriftAndRetries(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	h := newTF(t, cfg{"endpoint": srv.URL, "token": fleettest.OperatorToken})
	vc := cfg{"name": "lin-1", "image": "/vms/ubuntu.raw", "guest_os": "linux", "vcpus": 2, "memory_mib": 2048, "node": "mac-1"}
	vm, _ := h.mustApply("fluxvm_vm", nil, vc)

	// A flaky central: the refresh's GETs are retried.
	srv.Fail("GET", "/fleet/vms", 503, 2)
	again, d := h.read(vm)
	failOnDiags(t, "read through 503s", d)
	if again == nil {
		t.Fatal("VM lost on a flaky read")
	}

	// The VM vanished outside Terraform (TTL, deleted on the node): the next plan creates it again.
	srv.DeleteVMBehindTheScenes(vm.str(t, "vm_id"))
	gone, d := h.read(vm)
	failOnDiags(t, "read after drift", d)
	if gone != nil {
		t.Fatalf("drifted VM still in state: %s", gone.value)
	}
	if p := h.plan("fluxvm_vm", nil, vc); hasError(p.diags) || p.planned.IsNull() {
		t.Fatalf("re-create plan: %s", diagText(p.diags))
	}
}

func TestValidation(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	h := newTF(t, cfg{"endpoint": srv.URL, "token": fleettest.OperatorToken})
	for _, c := range []struct {
		typ  string
		cfg  cfg
		want string
	}{
		{"fluxvm_lease", cfg{"hours": 12}, "at least 24 hours"},
		{"fluxvm_vm", cfg{"name": "x"}, "Set catalog_image"},
		{"fluxvm_vm", cfg{"name": "x", "image": "/d"}, "guest_os"},
		{"fluxvm_vm", cfg{"name": "x", "catalog_image": "a", "power_state": "asleep"}, "power_state"},
		{"fluxvm_vm", cfg{"name": "x", "catalog_image": "a", "extra_json": "[1]"}, "JSON object"},
		{"fluxvm_tenant", cfg{"id": "Not Valid"}, "Invalid tenant id"},
		{"fluxvm_tenant", cfg{"id": "me"}, "Invalid tenant id"},
		{"fluxvm_api_key", cfg{"name": "k", "expires_in_days": 0}, "between 1 and 3650"},
		{"fluxvm_network", cfg{"name": "-bad"}, "Invalid network name"},
		{"fluxvm_webhook", cfg{"url": "ftp://x"}, "Invalid webhook URL"},
		{"fluxvm_volume", cfg{"name": "v", "size_gib": 0}, "Invalid size"},
	} {
		p := h.plan(c.typ, nil, c.cfg)
		if !strings.Contains(diagText(p.diags), c.want) {
			t.Errorf("%s %v: want %q, got %s", c.typ, c.cfg, c.want, diagText(p.diags))
		}
	}
	// Errors from central carry its message.
	_, p := h.apply("fluxvm_vm", nil, cfg{"name": "x", "catalog_image": "nope"})
	if !strings.Contains(diagText(p.diags), "no image 'nope'") {
		t.Errorf("missing catalog image: %s", diagText(p.diags))
	}
}

func TestProviderConfigFromEnvironment(t *testing.T) {
	srv := fleettest.NewServer()
	defer srv.Close()
	t.Setenv("FLUXVM_ENDPOINT", srv.URL)
	t.Setenv("FLUXVM_TOKEN", fleettest.OperatorToken)
	h := newTF(t, cfg{})
	if _, d := h.dataSource("fluxvm_nodes", cfg{}); hasError(d) {
		t.Fatal(diagText(d))
	}
}
