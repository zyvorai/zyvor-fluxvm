// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

// Package fleettest is an in-memory fake of FluxVM central's fleet API for
// tests: tenants and their tokens, API keys, nodes, leases, VMs, networks, the
// image catalog, webhooks, prices, billing, budgets, the activity log, volumes
// and pools. It answers with the same JSON shapes and status codes as central
// for the paths it covers, enforces tenant scoping, and can inject failures.
//
// It is a test double, not a second implementation: placement, quotas, metering
// and the 24-hour lease rule are simplified.
package fleettest

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sort"
	"strings"
	"sync"
	"time"
)

// OperatorToken is the fleet token the fake accepts as the operator.
const OperatorToken = "fleet-operator-token"

// Server is a running fake central.
type Server struct {
	*httptest.Server
	mu sync.Mutex
	// Requests records "METHOD /path?query" of every request, in order.
	Requests []string

	tenants  map[string]map[string]interface{}
	tokens   map[string]string // token -> tenant
	keys     map[string][]map[string]interface{}
	keyTok   map[string][2]string // token -> tenant, key id
	nodes    map[string]map[string]interface{}
	leases   []map[string]interface{}
	vms      []map[string]interface{}
	overlays map[string]map[string]interface{}
	images   map[string]map[string]interface{}
	hooks    []map[string]interface{}
	prices   map[string]interface{}
	events   []map[string]interface{}
	volumes  map[string]map[string]interface{}
	pools    map[string]map[string]interface{}
	faults   []fault
	seq      int
}

type fault struct {
	method, path string
	status       int
	left         int
}

// NewServer starts a fake central with two healthy Macs, mac-1 (12 vCPUs, 32 GiB)
// and mac-2 (24 vCPUs, 64 GiB, nested virtualization). Close it when done.
func NewServer() *Server {
	s := &Server{
		tenants:  map[string]map[string]interface{}{},
		tokens:   map[string]string{},
		keys:     map[string][]map[string]interface{}{},
		keyTok:   map[string][2]string{},
		nodes:    map[string]map[string]interface{}{},
		overlays: map[string]map[string]interface{}{},
		images:   map[string]map[string]interface{}{},
		volumes:  map[string]map[string]interface{}{},
		pools:    map[string]map[string]interface{}{},
		prices: map[string]interface{}{
			"currency": "USD", "mac_classes": []interface{}{}, "mac_per_hour": 0.0, "vm_per_hour": 0.0,
			"vcpu_per_hour": 0.0, "memory_gib_per_hour": 0.0, "macos_vm_per_hour": 0.0, "charge_vms_on_leased_macs": false,
		},
	}
	s.AddNode("mac-1", 12, 32768, false)
	s.AddNode("mac-2", 24, 65536, true)
	s.Server = httptest.NewServer(http.HandlerFunc(s.serve))
	return s
}

// AddNode registers a healthy Mac.
func (s *Server) AddNode(name string, vcpus int, memMiB int, nested bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.nodes[name] = map[string]interface{}{
		"name": name, "fluxvm_url": "http://" + name + ":8080", "vcpus_total": vcpus, "memory_mib_total": memMiB,
		"vm_count": 0, "last_seen": now(), "healthy": true, "cordoned": false, "labels": map[string]string{},
		"free_vcpus": vcpus, "free_memory_mib": memMiB, "security": map[string]interface{}{}, "density": nil,
		"apple": map[string]interface{}{"nested_virtualization": nested, "max_macos_vms": 2},
	}
}

// AddTenant creates a tenant directly and returns its primary token.
func (s *Server) AddTenant(id string) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	tok := "fvt_" + id + "_" + randHex(8)
	s.tenants[id] = map[string]interface{}{"id": id, "display_name": nil, "quota": map[string]interface{}{},
		"disabled": false, "created_at": now(), "monthly_budget": nil}
	s.tokens[tok] = id
	return tok
}

// PutImage adds a catalog entry directly.
func (s *Server) PutImage(name, guestOS, path string, vcpus, memMiB int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.images[name] = map[string]interface{}{"name": name, "display_name": nil, "description": nil, "guest_os": guestOS,
		"image": path, "vcpus": vcpus, "memory_mib": memMiB, "disk_size_gib": nil, "source": nil, "updated_at": now()}
}

// Fail makes the next n requests matching method and path (exact, without the query) answer status.
func (s *Server) Fail(method, path string, status, n int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.faults = append(s.faults, fault{method, path, status, n})
}

// DeleteVMBehindTheScenes removes a VM as if its node dropped it (TTL, direct delete), for drift tests.
func (s *Server) DeleteVMBehindTheScenes(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := s.vms[:0]
	for _, v := range s.vms {
		if v["id"] != id {
			out = append(out, v)
		}
	}
	s.vms = out
}

// Count returns how many requests matched "METHOD /path" (query ignored).
func (s *Server) Count(methodPath string) int {
	s.mu.Lock()
	defer s.mu.Unlock()
	n := 0
	for _, r := range s.Requests {
		if strings.SplitN(r, "?", 2)[0] == methodPath {
			n++
		}
	}
	return n
}

func now() string { return time.Now().UTC().Format(time.RFC3339Nano) }

func randHex(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}

type caller struct {
	tenant   string // "" = operator
	key      string // named key id, "" = primary
	readOnly bool
}

func writeJSON(w http.ResponseWriter, status int, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func errJSON(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

func (s *Server) serve(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Requests = append(s.Requests, r.Method+" "+r.URL.RequestURI())
	for i := range s.faults {
		f := &s.faults[i]
		if f.left > 0 && f.method == r.Method && f.path == r.URL.Path {
			f.left--
			if f.status == http.StatusTooManyRequests {
				w.Header().Set("Retry-After", "0")
			}
			errJSON(w, f.status, "injected failure")
			return
		}
	}
	if r.URL.Path == "/healthz" {
		writeJSON(w, 200, map[string]bool{"ok": true})
		return
	}
	tok := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	var c caller
	switch {
	case tok == OperatorToken:
	case s.tokens[tok] != "":
		c.tenant = s.tokens[tok]
	case s.keyTok[tok][0] != "":
		c.tenant, c.key = s.keyTok[tok][0], s.keyTok[tok][1]
		for _, k := range s.keys[c.tenant] {
			if k["id"] == c.key {
				c.readOnly, _ = k["read_only"].(bool)
			}
		}
	default:
		errJSON(w, 401, "missing or invalid bearer token")
		return
	}
	if c.tenant != "" {
		if d, _ := s.tenants[c.tenant]["disabled"].(bool); d {
			errJSON(w, 403, fmt.Sprintf("tenant '%s' is disabled", c.tenant))
			return
		}
		if !tenantRouteAllowed(r.Method, r.URL.Path) {
			errJSON(w, 403, fmt.Sprintf("tenant '%s' may not call %s %s", c.tenant, r.Method, r.URL.Path))
			return
		}
	}
	if c.readOnly && r.Method != http.MethodGet {
		errJSON(w, 403, "API key is read-only")
		return
	}
	var body map[string]interface{}
	if r.Body != nil && (r.Method == http.MethodPost || r.Method == http.MethodPut || r.Method == http.MethodPatch) {
		dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20))
		if err := dec.Decode(&body); err != nil && err.Error() != "EOF" {
			errJSON(w, 400, "Failed to parse the request body as JSON: "+err.Error())
			return
		}
	}
	if body == nil {
		body = map[string]interface{}{}
	}
	seg := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	status := s.route(w, r, c, seg, body)
	if r.Method != http.MethodGet && status != 0 {
		actor := "operator"
		if c.tenant != "" {
			actor = c.tenant
		}
		ev := map[string]interface{}{"time": now(), "actor": actor, "method": r.Method, "path": r.URL.Path, "status": status}
		if c.tenant != "" {
			ev["tenant"] = c.tenant
		}
		if c.key != "" {
			ev["key"] = c.key
		}
		s.events = append(s.events, ev)
	}
}

// tenantRouteAllowed mirrors central's tenant_route_allowed, plus volumes and pools.
func tenantRouteAllowed(method, path string) bool {
	get, post, del, put := method == "GET", method == "POST", method == "DELETE", method == "PUT"
	s := strings.Split(strings.Trim(path, "/"), "/")
	is := func(p ...string) bool {
		if len(p) != len(s) {
			return false
		}
		for i := range p {
			if p[i] != "*" && p[i] != s[i] {
				return false
			}
		}
		return true
	}
	switch {
	case is("fleet", "capacity"), is("fleet", "usage"), is("fleet", "tenants", "me"):
		return get
	case is("fleet", "tenants", "me", "keys"):
		return get || post
	case is("fleet", "tenants", "me", "keys", "*"):
		return del
	case is("fleet", "prices"), is("fleet", "billing"), is("fleet", "notifications"):
		return get
	case is("fleet", "webhooks"):
		return get || post
	case is("fleet", "webhooks", "*"):
		return del
	case is("fleet", "webhooks", "*", "test"):
		return post
	case is("fleet", "tenants", "me", "budget"):
		return put
	case is("fleet", "vms"), is("fleet", "leases"):
		return get || post
	case is("fleet", "vms", "*", "*"):
		return del
	case is("fleet", "vms", "*", "*", "exec"), is("fleet", "vms", "*", "*", "start"), is("fleet", "vms", "*", "*", "stop"),
		is("fleet", "vms", "*", "*", "restart"), is("fleet", "vms", "*", "*", "pause"), is("fleet", "vms", "*", "*", "resume"):
		return post
	case is("fleet", "vms", "*", "*", "logs"), is("fleet", "events"):
		return get
	case is("fleet", "leases", "*"):
		return get
	case is("fleet", "leases", "*", "release"):
		return post
	case is("fleet", "images"), is("fleet", "nodes", "*"):
		return get
	case is("fleet", "overlays"):
		return get || post
	case is("fleet", "overlays", "*"):
		return get || del
	case is("fleet", "overlays", "*", "members"), is("fleet", "overlays", "*", "rotate"):
		return post
	case is("fleet", "overlays", "*", "members", "*"):
		return del
	case is("fleet", "volumes"), is("fleet", "pools"):
		return get || post
	case is("fleet", "volumes", "*"):
		return get || del
	case is("fleet", "volumes", "*", "resize"):
		return post
	case is("fleet", "pools", "*"):
		return get || del || method == "PATCH"
	}
	return false
}

func str(m map[string]interface{}, k string) string {
	v, _ := m[k].(string)
	return v
}

func num(m map[string]interface{}, k string, def float64) float64 {
	switch v := m[k].(type) {
	case float64:
		return v
	case int:
		return float64(v)
	case int64:
		return float64(v)
	case uint64:
		return float64(v)
	}
	return def
}

func (s *Server) next(prefix string) string {
	s.seq++
	return fmt.Sprintf("%s%04d%s", prefix, s.seq, randHex(4))
}

// owned reports whether caller c may see a resource whose tenant field is t.
func owned(c caller, t interface{}) bool {
	if c.tenant == "" {
		return true
	}
	ts, _ := t.(string)
	return ts == c.tenant
}

func (s *Server) tenantView(id string) map[string]interface{} {
	t := s.tenants[id]
	out := map[string]interface{}{}
	for k, v := range t {
		out[k] = v
	}
	vms, vcpus, mem := 0, 0.0, 0.0
	for _, v := range s.vms {
		if str(v, "tenant") == id {
			vms++
			req := v["request"].(map[string]interface{})
			vcpus += num(req, "vcpus", 0)
			mem += num(req, "memory_mib", 0)
		}
	}
	active := []interface{}{}
	for _, l := range s.leases {
		if l["tenant"] == id && l["state"] == "active" {
			active = append(active, l)
		}
	}
	out["in_use"] = map[string]interface{}{"vms": vms, "vcpus": vcpus, "memory_mib": mem, "leased_hosts": len(active)}
	out["active_leases"] = active
	return out
}

func (s *Server) keysOwner(w http.ResponseWriter, c caller, id string) (string, bool) {
	if id == "me" {
		if c.tenant == "" {
			errJSON(w, 400, "the operator token is not a tenant; use /fleet/tenants/{id}/keys")
			return "", false
		}
		return c.tenant, true
	}
	if c.tenant != "" {
		errJSON(w, 403, "operator only")
		return "", false
	}
	if s.tenants[id] == nil {
		errJSON(w, 404, fmt.Sprintf("no tenant '%s'", id))
		return "", false
	}
	return id, true
}

func validName(n string, max int) bool {
	if n == "" || len(n) > max || strings.HasPrefix(n, "-") || strings.HasSuffix(n, "-") {
		return false
	}
	for _, b := range []byte(n) {
		if !(b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '-') {
			return false
		}
	}
	return true
}

// route answers one request and returns the status it wrote (0 when none was written).
func (s *Server) route(w http.ResponseWriter, r *http.Request, c caller, seg []string, body map[string]interface{}) int {
	m := r.Method
	p := func(parts ...string) bool {
		if len(parts) != len(seg) {
			return false
		}
		for i := range parts {
			if parts[i] != "*" && parts[i] != seg[i] {
				return false
			}
		}
		return true
	}
	ok := func(status int, v interface{}) int { writeJSON(w, status, v); return status }
	fail := func(status int, msg string) int { errJSON(w, status, msg); return status }
	admin := c.tenant == ""
	operatorOnly := func() bool {
		if !admin {
			errJSON(w, 403, "operator only")
		}
		return admin
	}

	switch {
	// ---- tenants ----
	case p("fleet", "tenants") && m == "GET":
		out := []interface{}{}
		ids := make([]string, 0, len(s.tenants))
		for id := range s.tenants {
			ids = append(ids, id)
		}
		sort.Strings(ids)
		for _, id := range ids {
			out = append(out, s.tenantView(id))
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "tenants") && m == "POST":
		id := str(body, "id")
		if !validName(id, 63) || id == "me" {
			return fail(400, fmt.Sprintf("invalid tenant id '%s'", id))
		}
		if s.tenants[id] != nil {
			return fail(409, fmt.Sprintf("tenant '%s' already exists", id))
		}
		tok := "fvt_" + randHex(16)
		quota, _ := body["quota"].(map[string]interface{})
		if quota == nil {
			quota = map[string]interface{}{}
		}
		s.tenants[id] = map[string]interface{}{"id": id, "display_name": body["display_name"], "quota": quota,
			"disabled": false, "created_at": now(), "monthly_budget": nil}
		s.tokens[tok] = id
		return ok(201, map[string]interface{}{"tenant": s.tenants[id], "token": tok, "note": "store this token now"})
	case p("fleet", "tenants", "me") && m == "GET":
		if admin {
			return fail(400, "the operator token is not a tenant; use GET /fleet/tenants")
		}
		return ok(200, s.tenantView(c.tenant))
	case p("fleet", "tenants", "*") && (m == "GET" || m == "PATCH" || m == "DELETE"):
		id := seg[2]
		t := s.tenants[id]
		if t == nil {
			return fail(404, fmt.Sprintf("no tenant '%s'", id))
		}
		switch m {
		case "GET":
			return ok(200, s.tenantView(id))
		case "PATCH":
			for _, k := range []string{"quota", "disabled", "display_name"} {
				if v, set := body[k]; set && v != nil {
					t[k] = v
				}
			}
			return ok(200, t)
		default:
			for _, l := range s.leases {
				if l["tenant"] == id && l["state"] == "active" {
					return fail(409, fmt.Sprintf("tenant '%s' still has active leases or VMs", id))
				}
			}
			delete(s.tenants, id)
			delete(s.keys, id)
			for k, v := range s.tokens {
				if v == id {
					delete(s.tokens, k)
				}
			}
			w.WriteHeader(204)
			return 204
		}
	case p("fleet", "tenants", "*", "rotate-token") && m == "POST":
		id := seg[2]
		if s.tenants[id] == nil {
			return fail(404, fmt.Sprintf("no tenant '%s'", id))
		}
		for k, v := range s.tokens {
			if v == id {
				delete(s.tokens, k)
			}
		}
		tok := "fvt_" + randHex(16)
		s.tokens[tok] = id
		return ok(200, map[string]interface{}{"tenant": id, "token": tok})
	case p("fleet", "tenants", "*", "budget") && m == "PUT":
		id, good := s.keysOwner(w, c, seg[2])
		if !good {
			return 400
		}
		mo, isNum := body["monthly"].(float64)
		switch {
		case body["monthly"] == nil || (isNum && mo == 0):
			s.tenants[id]["monthly_budget"] = nil
		case isNum && mo > 0:
			s.tenants[id]["monthly_budget"] = mo
		default:
			return fail(400, "a budget is a positive amount")
		}
		return ok(200, s.tenants[id])

	// ---- keys ----
	case p("fleet", "tenants", "*", "keys") && (m == "GET" || m == "POST"):
		id, good := s.keysOwner(w, c, seg[2])
		if !good {
			return 400
		}
		if m == "GET" {
			out := []interface{}{}
			for _, k := range s.keys[id] {
				out = append(out, k)
			}
			return ok(200, map[string]interface{}{"items": out})
		}
		if c.key != "" {
			return fail(403, "API keys are created and revoked with the tenant's primary token")
		}
		name := str(body, "name")
		if name == "" || strings.HasPrefix(name, "key_") {
			return fail(400, "a key needs a name that does not look like a key id")
		}
		for _, k := range s.keys[id] {
			if k["name"] == name {
				return fail(409, fmt.Sprintf("tenant '%s' already has a key named '%s'", id, name))
			}
		}
		var exp interface{}
		if d, set := body["expires_in_days"].(float64); set {
			if d < 1 || d > 3650 {
				return fail(400, "expires_in_days must be between 1 and 3650")
			}
			exp = time.Now().UTC().Add(time.Duration(d) * 24 * time.Hour).Format(time.RFC3339Nano)
		}
		ro, _ := body["read_only"].(bool)
		k := map[string]interface{}{"id": "key_" + randHex(6), "name": name, "created_at": now(), "expires_at": exp,
			"read_only": ro, "last_used_at": nil}
		s.keys[id] = append(s.keys[id], k)
		tok := "fvk_" + randHex(16)
		s.keyTok[tok] = [2]string{id, k["id"].(string)}
		return ok(201, map[string]interface{}{"key": k, "token": tok, "note": "store this token now"})
	case p("fleet", "tenants", "*", "keys", "*") && m == "DELETE":
		id, good := s.keysOwner(w, c, seg[2])
		if !good {
			return 400
		}
		if c.key != "" {
			return fail(403, "API keys are created and revoked with the tenant's primary token")
		}
		for i, k := range s.keys[id] {
			if k["id"] == seg[4] || k["name"] == seg[4] {
				s.keys[id] = append(s.keys[id][:i], s.keys[id][i+1:]...)
				for t, v := range s.keyTok {
					if v[1] == k["id"] {
						delete(s.keyTok, t)
					}
				}
				return ok(200, map[string]interface{}{"revoked": k})
			}
		}
		return fail(404, fmt.Sprintf("no API key '%s'", seg[4]))

	// ---- nodes ----
	case p("fleet", "nodes") && m == "GET":
		if !operatorOnly() {
			return 403
		}
		names := make([]string, 0, len(s.nodes))
		for n := range s.nodes {
			names = append(names, n)
		}
		sort.Strings(names)
		out := []interface{}{}
		for _, n := range names {
			out = append(out, s.nodes[n])
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "nodes", "*") && m == "GET":
		n := s.nodes[seg[2]]
		if n == nil {
			return fail(404, fmt.Sprintf("no registered node named '%s'", seg[2]))
		}
		if !admin {
			if s.leaseOf(seg[2]) != c.tenant {
				return fail(404, fmt.Sprintf("you do not lease a Mac named '%s'", seg[2]))
			}
			return ok(200, map[string]interface{}{"name": n["name"], "healthy": true, "vcpus_total": n["vcpus_total"],
				"memory_mib_total": n["memory_mib_total"], "free_vcpus": n["free_vcpus"], "free_memory_mib": n["free_memory_mib"],
				"vm_count": n["vm_count"], "last_seen": n["last_seen"], "apple": n["apple"]})
		}
		return ok(200, n)
	case p("fleet", "capacity") && m == "GET":
		return ok(200, map[string]interface{}{"nodes_total": len(s.nodes), "nodes_healthy": len(s.nodes), "nodes_cordoned": 0,
			"nodes_schedulable": len(s.nodes), "vm_count": len(s.vms), "vcpus_total": 36, "vcpus_used": 0, "vcpus_free": 36,
			"memory_mib_total": 98304, "memory_mib_used": 0, "memory_mib_free": 98304})

	// ---- leases ----
	case p("fleet", "leases") && m == "POST":
		tenant := c.tenant
		if admin {
			tenant = str(body, "tenant")
			if tenant == "" {
				return fail(400, "an operator lease needs \"tenant\"")
			}
		}
		node := str(body, "node")
		if node != "" {
			if s.nodes[node] == nil {
				return fail(404, fmt.Sprintf("no registered node named '%s'", node))
			}
			if s.leaseOf(node) != "" {
				return fail(409, fmt.Sprintf("node '%s' cannot be leased: node '%s' is leased to tenant '%s'", node, node, s.leaseOf(node)))
			}
		} else {
			names := make([]string, 0)
			for n := range s.nodes {
				names = append(names, n)
			}
			sort.Strings(names)
			for _, n := range names {
				nd := s.nodes[n]
				nested, _ := body["nested_virtualization"].(bool)
				apple := nd["apple"].(map[string]interface{})
				if s.leaseOf(n) == "" && num(nd, "vcpus_total", 0) >= num(body, "min_vcpus", 0) &&
					num(nd, "memory_mib_total", 0) >= num(body, "min_memory_mib", 0) && (!nested || apple["nested_virtualization"] == true) {
					node = n
					break
				}
			}
			if node == "" {
				return fail(503, "no free Mac can be leased right now (healthy, uncordoned, not leased, scrubbed, empty and large enough)")
			}
		}
		start := time.Now().UTC()
		l := map[string]interface{}{"id": s.next("lease-"), "tenant": tenant, "node": node, "started_at": start.Format(time.RFC3339Nano),
			"min_end": start.Add(24 * time.Hour).Format(time.RFC3339Nano), "state": "active"}
		if h, set := body["hours"].(float64); set {
			if h < 24 {
				return fail(400, fmt.Sprintf("a lease must be at least 24 hours (asked for %d)", int(h)))
			}
			l["ends_at"] = start.Add(time.Duration(h) * time.Hour).Format(time.RFC3339Nano)
		}
		s.leases = append(s.leases, l)
		return ok(201, map[string]interface{}{"lease": l, "node": s.nodes[node]})
	case p("fleet", "leases") && m == "GET":
		out := []interface{}{}
		for _, l := range s.leases {
			if owned(c, l["tenant"]) && (r.URL.Query().Get("state") == "" || l["state"] == r.URL.Query().Get("state")) &&
				(r.URL.Query().Get("tenant") == "" || l["tenant"] == r.URL.Query().Get("tenant")) {
				out = append(out, l)
			}
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "leases", "*") && m == "GET":
		for _, l := range s.leases {
			if l["id"] == seg[2] && owned(c, l["tenant"]) {
				return ok(200, l)
			}
		}
		return fail(404, fmt.Sprintf("no lease '%s'", seg[2]))
	case p("fleet", "leases", "*", "release") && m == "POST":
		for _, l := range s.leases {
			if l["id"] == seg[2] && owned(c, l["tenant"]) {
				if l["state"] != "active" {
					return fail(409, fmt.Sprintf("lease '%s' is not active", seg[2]))
				}
				if !(admin && r.URL.Query().Get("force") == "true") && !s.earlyReleaseAllowed() {
					return fail(409, fmt.Sprintf("lease cannot be released before %s (24-hour minimum)", l["min_end"]))
				}
				l["state"] = "released"
				l["released_at"] = now()
				return ok(200, map[string]interface{}{"lease": l, "evicted": []interface{}{}, "scrub_required": true})
			}
		}
		return fail(404, fmt.Sprintf("no lease '%s'", seg[2]))

	// ---- VMs ----
	case p("fleet", "vms") && m == "POST":
		name := str(body, "name")
		if name == "" {
			return fail(400, "Failed to deserialize the JSON body into the target type: missing field `name`")
		}
		node := str(body, "node")
		delete(body, "node")
		delete(body, "nodeSelector")
		delete(body, "reschedulable")
		tenant := c.tenant
		if admin {
			tenant = str(body, "tenant")
		}
		if tenant != "" {
			body["tenant"] = tenant
		}
		macos := false
		if a, isMap := body["apple"].(map[string]interface{}); isMap && a["guest_os"] == "macos" {
			macos = true
		}
		if node == "" {
			for _, l := range s.leases {
				if l["tenant"] == tenant && l["state"] == "active" {
					node = l["node"].(string)
					break
				}
			}
		}
		if node == "" && macos && !admin {
			return fail(503, "no node can take it; macOS guests run only on Macs the tenant leases: lease one with POST /fleet/leases")
		}
		if node == "" {
			node = "mac-1"
		}
		if s.nodes[node] == nil {
			return fail(400, fmt.Sprintf("no registered node named '%s'", node))
		}
		if !admin && s.leaseOf(node) != c.tenant && macos {
			return fail(403, fmt.Sprintf("node '%s' is not leased to tenant '%s'", node, c.tenant))
		}
		if vols, has := body["volumes"].([]interface{}); has {
			for _, v := range vols {
				vn, _ := v.(string)
				vol := s.volumes[vn]
				if vol == nil || !owned(c, vol["tenant"]) {
					return fail(404, fmt.Sprintf("no volume '%s'", vn))
				}
			}
		}
		id := fmt.Sprintf("%08x-0000-4000-8000-%012x", s.seq+1, s.seq+1)
		s.seq++
		vm := map[string]interface{}{"id": id, "name": name, "backend": body["backend"], "status": "running", "pid": 4242,
			"created_at": now(), "expires_at": nil, "error": nil, "request": body, "node": node}
		if vols, has := body["volumes"].([]interface{}); has {
			for _, v := range vols {
				s.volumes[v.(string)]["attached_to"] = id
			}
		}
		s.vms = append(s.vms, vm)
		rec := map[string]interface{}{}
		for k, v := range vm {
			if k != "node" {
				rec[k] = v
			}
		}
		return ok(200, map[string]interface{}{"node": node, "vm": rec})
	case p("fleet", "vms") && m == "GET":
		out := []interface{}{}
		for _, v := range s.vms {
			if owned(c, v["tenant"]) || owned(c, v["request"].(map[string]interface{})["tenant"]) {
				out = append(out, v)
			}
		}
		return ok(200, map[string]interface{}{"items": out, "unreachable_nodes": []interface{}{}})
	case p("fleet", "vms", "*", "*") && m == "DELETE":
		for i, v := range s.vms {
			if v["node"] == seg[2] && v["id"] == seg[3] && owned(c, v["request"].(map[string]interface{})["tenant"]) {
				s.vms = append(s.vms[:i], s.vms[i+1:]...)
				for _, vol := range s.volumes {
					if vol["attached_to"] == seg[3] {
						vol["attached_to"] = nil
					}
				}
				w.WriteHeader(204)
				return 204
			}
		}
		return fail(404, fmt.Sprintf("no VM '%s' on node '%s'", seg[3], seg[2]))
	case p("fleet", "vms", "*", "*", "*") && m == "POST":
		var vm map[string]interface{}
		for _, v := range s.vms {
			if v["node"] == seg[2] && v["id"] == seg[3] && owned(c, v["request"].(map[string]interface{})["tenant"]) {
				vm = v
			}
		}
		if vm == nil {
			return fail(404, fmt.Sprintf("no VM '%s' on node '%s'", seg[3], seg[2]))
		}
		switch seg[4] {
		case "exec":
			return ok(200, map[string]interface{}{"exit_code": 0, "stdout": "ran: " + str(body, "command") + "\n", "stderr": ""})
		case "start", "resume", "restart":
			vm["status"] = "running"
		case "stop":
			vm["status"] = "stopped"
		case "pause":
			vm["status"] = "paused"
		default:
			return fail(404, fmt.Sprintf("no VM action '%s'", seg[4]))
		}
		return ok(200, vm)
	case p("fleet", "vms", "*", "*", "logs") && m == "GET":
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.WriteHeader(200)
		_, _ = w.Write([]byte("boot ok\nlogin:\n"))
		return 200

	// ---- overlays ----
	case p("fleet", "overlays") && m == "POST":
		name := str(body, "name")
		if !validName(name, 40) {
			return fail(400, "network names are 1-40 of a-z, 0-9 and '-'")
		}
		if s.overlays[name] != nil {
			return fail(409, fmt.Sprintf("a network named '%s' already exists", name))
		}
		tenant := interface{}(nil)
		if !admin {
			tenant = c.tenant
		} else if t := str(body, "tenant"); t != "" {
			tenant = t
		}
		members := map[string]interface{}{}
		var nodes []string
		if ns, set := body["nodes"].([]interface{}); set {
			for _, n := range ns {
				nodes = append(nodes, n.(string))
			}
		} else if !admin {
			for _, l := range s.leases {
				if l["tenant"] == c.tenant && l["state"] == "active" {
					nodes = append(nodes, l["node"].(string))
				}
			}
		}
		hpn := int(num(body, "hosts_per_node", 30))
		for i, n := range nodes {
			if s.nodes[n] == nil {
				return fail(400, fmt.Sprintf("no registered node named '%s'", n))
			}
			members[n] = []int{2 + i*hpn, 1 + (i+1)*hpn}
		}
		net := map[string]interface{}{"name": name, "tenant": tenant, "subnet": fmt.Sprintf("10.89.%d.0/24", len(s.overlays)+1),
			"port": 47100 + len(s.overlays), "mtu": int(num(body, "mtu", 1400)), "members": members, "key_epoch": 0,
			"key_rotated_at": now(), "created_at": now(), "hosts_per_node": hpn}
		s.overlays[name] = net
		return ok(201, publicNet(net))
	case p("fleet", "overlays") && m == "GET":
		out := []interface{}{}
		for _, o := range s.overlays {
			if owned(c, o["tenant"]) {
				out = append(out, publicNet(o))
			}
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "overlays", "*") || p("fleet", "overlays", "*", "members") || p("fleet", "overlays", "*", "rotate") ||
		p("fleet", "overlays", "*", "members", "*"):
		o := s.overlays[seg[2]]
		if o == nil || !owned(c, o["tenant"]) {
			return fail(404, fmt.Sprintf("no network '%s'", seg[2]))
		}
		members := o["members"].(map[string]interface{})
		switch {
		case len(seg) == 3 && m == "GET":
			return ok(200, publicNet(o))
		case len(seg) == 3 && m == "DELETE":
			delete(s.overlays, seg[2])
			w.WriteHeader(204)
			return 204
		case len(seg) == 4 && seg[3] == "members" && m == "POST":
			n := str(body, "node")
			if s.nodes[n] == nil {
				return fail(400, fmt.Sprintf("no registered node named '%s'", n))
			}
			hpn := int(num(o, "hosts_per_node", 30))
			i := len(members)
			members[n] = []int{2 + i*hpn, 1 + (i+1)*hpn}
			return ok(200, publicNet(o))
		case len(seg) == 4 && seg[3] == "rotate" && m == "POST":
			o["key_epoch"] = int(num(o, "key_epoch", 0)) + 1
			o["key_rotated_at"] = now()
			return ok(200, publicNet(o))
		case len(seg) == 5 && m == "DELETE":
			if members[seg[4]] == nil {
				return fail(409, fmt.Sprintf("'%s' is not a member of '%s'", seg[4], seg[2]))
			}
			delete(members, seg[4])
			return ok(200, publicNet(o))
		}
		return fail(405, "method not allowed")

	// ---- images ----
	case p("fleet", "images") && m == "GET":
		names := make([]string, 0, len(s.images))
		for n := range s.images {
			names = append(names, n)
		}
		sort.Strings(names)
		out := []interface{}{}
		for _, n := range names {
			out = append(out, s.images[n])
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "images", "*") && (m == "PUT" || m == "DELETE"):
		if m == "DELETE" {
			if s.images[seg[2]] == nil {
				return fail(404, fmt.Sprintf("no image '%s'", seg[2]))
			}
			delete(s.images, seg[2])
			w.WriteHeader(204)
			return 204
		}
		if g := str(body, "guest_os"); g != "macos" && g != "linux" {
			return fail(400, "guest_os must be 'macos' or 'linux'")
		}
		body["name"] = seg[2]
		body["updated_at"] = now()
		if _, set := body["vcpus"]; !set {
			body["vcpus"] = 4
		}
		if _, set := body["memory_mib"]; !set {
			body["memory_mib"] = 8192
		}
		s.images[seg[2]] = body
		return ok(200, body)

	// ---- webhooks and notifications ----
	case p("fleet", "webhooks") && m == "GET":
		out := []interface{}{}
		for _, h := range s.hooks {
			if admin || h["owner"] == c.tenant {
				out = append(out, publicHook(h))
			}
		}
		return ok(200, map[string]interface{}{"items": out, "kinds": []string{"vm.created", "vm.deleted", "lease.created",
			"lease.released", "lease.ending", "lease.ended", "budget.near", "budget.over", "test"}})
	case p("fleet", "webhooks") && m == "POST":
		u := str(body, "url")
		if !strings.HasPrefix(u, "https://") && !admin {
			return fail(400, "a webhook URL must use https")
		}
		if !strings.HasPrefix(u, "https://") && !strings.HasPrefix(u, "http://") {
			return fail(400, "a webhook URL must be http(s)")
		}
		owner := interface{}(nil)
		if !admin {
			owner = c.tenant
		}
		events := []interface{}{}
		if e, set := body["events"].([]interface{}); set {
			events = e
		}
		h := map[string]interface{}{"id": "whk_" + randHex(6), "owner": owner, "url": u, "events": events,
			"secret": "whsec_" + randHex(16), "created_at": now()}
		s.hooks = append(s.hooks, h)
		return ok(201, map[string]interface{}{"webhook": publicHook(h), "secret": h["secret"], "note": "store this secret now"})
	case p("fleet", "webhooks", "*") && m == "DELETE":
		for i, h := range s.hooks {
			if h["id"] == seg[2] && (admin || h["owner"] == c.tenant) {
				s.hooks = append(s.hooks[:i], s.hooks[i+1:]...)
				w.WriteHeader(204)
				return 204
			}
		}
		return fail(404, fmt.Sprintf("no webhook '%s'", seg[2]))
	case p("fleet", "webhooks", "*", "test") && m == "POST":
		for _, h := range s.hooks {
			if h["id"] == seg[2] && (admin || h["owner"] == c.tenant) {
				return ok(200, map[string]interface{}{"delivery": map[string]interface{}{"notification": "ntf_" + randHex(6),
					"type": "test", "time": now(), "attempt": 1, "status": 200, "ok": true}})
			}
		}
		return fail(404, fmt.Sprintf("no webhook '%s'", seg[2]))
	case p("fleet", "notifications") && m == "GET":
		return ok(200, map[string]interface{}{"items": []interface{}{}})

	// ---- prices, billing, usage, events ----
	case p("fleet", "prices") && m == "GET":
		return ok(200, s.prices)
	case p("fleet", "prices") && m == "PUT":
		if !operatorOnly() {
			return 403
		}
		if body["currency"] == nil {
			body["currency"] = "USD"
		}
		s.prices = body
		return ok(200, s.prices)
	case p("fleet", "billing") && m == "GET":
		ids := []string{}
		if !admin {
			ids = append(ids, c.tenant)
		} else if t := r.URL.Query().Get("tenant"); t != "" {
			ids = append(ids, t)
		} else {
			for id := range s.tenants {
				ids = append(ids, id)
			}
			sort.Strings(ids)
		}
		from := time.Date(time.Now().UTC().Year(), time.Now().UTC().Month(), 1, 0, 0, 0, 0, time.UTC)
		to := from.AddDate(0, 1, 0)
		sts := []interface{}{}
		for _, id := range ids {
			if s.tenants[id] == nil {
				continue
			}
			st := map[string]interface{}{"tenant": id, "currency": s.prices["currency"], "from": from, "to": to,
				"lines": []interface{}{}, "total_micros": 0, "total": 0.0}
			if b, isNum := s.tenants[id]["monthly_budget"].(float64); isNum {
				st["budget"] = map[string]interface{}{"monthly": b, "used": 0.0, "state": "ok"}
			}
			sts = append(sts, st)
		}
		if r.URL.Query().Get("format") == "csv" {
			w.Header().Set("Content-Type", "text/csv; charset=utf-8")
			w.WriteHeader(200)
			_, _ = w.Write([]byte("tenant,from,to,description,quantity,unit,unit_price,amount,currency\n"))
			return 200
		}
		return ok(200, map[string]interface{}{"from": from, "to": to, "currency": s.prices["currency"], "prices_set": false, "statements": sts})
	case p("fleet", "usage") && m == "GET":
		return ok(200, map[string]interface{}{"from": time.Now().UTC().AddDate(0, 0, -30), "to": time.Now().UTC(), "tenants": []interface{}{}})
	case p("fleet", "events") && m == "GET":
		out := []interface{}{}
		for i := len(s.events) - 1; i >= 0; i-- {
			if owned(c, s.events[i]["tenant"]) {
				out = append(out, s.events[i])
			}
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "policy") && (m == "GET" || m == "PUT"):
		if !operatorOnly() {
			return 403
		}
		return ok(200, map[string]interface{}{"require_lease_for_macos": true, "exclusive_leased_hosts": true, "min_lease_hours": 24,
			"ledger_retention_days": 400, "isolate_tenant_networks": false, "tenant_subnet_prefix": "10.230", "overlay_key_rotation_days": 30})

	// ---- volumes ----
	case p("fleet", "volumes") && m == "POST":
		name := str(body, "name")
		if !validName(name, 40) || strings.HasPrefix(name, "vol-") {
			return fail(400, "volume names are 1-40 of a-z, 0-9 and '-', not starting with vol-")
		}
		if s.volumes[name] != nil {
			return fail(409, fmt.Sprintf("a volume named '%s' already exists", name))
		}
		size := num(body, "size_gib", 0)
		if size < 1 || size > 2048 {
			return fail(400, "size_gib must be 1-2048")
		}
		node := str(body, "node")
		if node == "" {
			node = "mac-1"
			for _, l := range s.leases {
				if l["tenant"] == c.tenant && l["state"] == "active" {
					node = l["node"].(string)
				}
			}
		}
		var tenant interface{}
		if !admin {
			tenant = c.tenant
		}
		v := map[string]interface{}{"id": s.next("vol-"), "name": name, "tenant": tenant, "node": node, "size_gib": size,
			"attached_to": nil, "created_at": now()}
		s.volumes[name] = v
		return ok(201, v)
	case p("fleet", "volumes") && m == "GET":
		out := []interface{}{}
		for _, v := range s.volumes {
			if owned(c, v["tenant"]) {
				out = append(out, v)
			}
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "volumes", "*") || p("fleet", "volumes", "*", "resize"):
		v := s.volumes[seg[2]]
		if v == nil || !owned(c, v["tenant"]) {
			return fail(404, fmt.Sprintf("no volume '%s'", seg[2]))
		}
		switch {
		case len(seg) == 3 && m == "GET":
			return ok(200, v)
		case len(seg) == 3 && m == "DELETE":
			if v["attached_to"] != nil {
				return fail(409, fmt.Sprintf("volume '%s' is attached to a VM; delete the VM first", seg[2]))
			}
			delete(s.volumes, seg[2])
			w.WriteHeader(204)
			return 204
		case len(seg) == 4 && m == "POST":
			size := num(body, "size_gib", 0)
			if size <= num(v, "size_gib", 0) {
				return fail(400, "a volume can only grow")
			}
			v["size_gib"] = size
			return ok(200, v)
		}
		return fail(405, "method not allowed")

	// ---- pools ---- (central's shape: `size` and `status` objects, names of 1-40, image and size fixed after create)
	case p("fleet", "pools") && m == "POST":
		name := str(body, "name")
		if !validName(name, 40) {
			return fail(400, "pool names are 1-40 of a-z, 0-9 and '-'")
		}
		if s.pools[name] != nil {
			return fail(409, fmt.Sprintf("a pool named '%s' already exists", name))
		}
		img := s.images[str(body, "image")]
		if img == nil {
			return fail(400, fmt.Sprintf("no image '%s' in the catalog", str(body, "image")))
		}
		minReady := num(body, "min_ready", 1)
		if minReady < 0 {
			return fail(400, "min_ready cannot be negative")
		}
		maxTotal := num(body, "max_total", minReady*2+2)
		if maxTotal < minReady {
			return fail(400, "max_total must be at least min_ready")
		}
		tenant := interface{}(nil)
		owner := "operator"
		if !admin {
			tenant, owner = c.tenant, c.tenant
		} else if t := str(body, "tenant"); t != "" {
			tenant, owner = t, t
		}
		nodes := body["nodes"]
		if nodes == nil {
			nodes = []interface{}{}
		}
		pl := map[string]interface{}{"name": name, "owner": owner, "tenant": tenant, "image": str(body, "image"),
			"size": map[string]interface{}{"vcpus": num(body, "vcpus", num(img, "vcpus", 4)),
				"memory_mib": num(body, "memory_mib", num(img, "memory_mib", 8192))},
			"min_ready": minReady, "max_total": maxTotal, "nodes": nodes, "node_selector": map[string]interface{}{},
			"created_at": now(),
			"status":     map[string]interface{}{"state": "filling", "ready": 0, "booting": 0, "claimed_running": 0, "claimed_24h": 0}}
		s.pools[name] = pl
		return ok(201, pl)
	case p("fleet", "pools") && m == "GET":
		out := []interface{}{}
		for _, v := range s.pools {
			if owned(c, v["tenant"]) {
				out = append(out, v)
			}
		}
		return ok(200, map[string]interface{}{"items": out})
	case p("fleet", "pools", "*"):
		pl := s.pools[seg[2]]
		if pl == nil || !owned(c, pl["tenant"]) {
			return fail(404, fmt.Sprintf("no pool '%s'", seg[2]))
		}
		switch m {
		case "GET":
			return ok(200, pl)
		case "PATCH":
			size := pl["size"].(map[string]interface{})
			if v, ok := body["image"]; ok && v != nil && v != pl["image"] {
				return fail(400, "a pool's image cannot change: make a new pool")
			}
			for _, k := range []string{"vcpus", "memory_mib"} {
				if v, ok := body[k]; ok && v != nil && v != size[k] {
					return fail(400, "a pool's size cannot change: make a new pool")
				}
			}
			for _, k := range []string{"min_ready", "max_total", "nodes", "node_selector"} {
				if v, ok := body[k]; ok && v != nil {
					pl[k] = v
				}
			}
			if num(pl, "max_total", 0) < num(pl, "min_ready", 0) {
				pl["max_total"] = pl["min_ready"]
			}
			return ok(200, pl)
		case "DELETE":
			delete(s.pools, seg[2])
			return ok(200, map[string]interface{}{"deleted": seg[2]})
		}
		return fail(405, "method not allowed")
	}
	return fail(404, "no route "+m+" "+r.URL.Path)
}

// earlyReleaseAllowed: the fake lets tests release leases at once (central refuses before 24 hours).
func (s *Server) earlyReleaseAllowed() bool { return true }

func (s *Server) leaseOf(node string) string {
	for _, l := range s.leases {
		if l["node"] == node && l["state"] == "active" {
			return l["tenant"].(string)
		}
	}
	return ""
}

func publicNet(o map[string]interface{}) map[string]interface{} {
	out := map[string]interface{}{}
	for k, v := range o {
		if k != "hosts_per_node" {
			out[k] = v
		}
	}
	return out
}

func publicHook(h map[string]interface{}) map[string]interface{} {
	out := map[string]interface{}{}
	for k, v := range h {
		if k != "secret" {
			out[k] = v
		}
	}
	out["deliveries"] = []interface{}{}
	return out
}
