"""In-process stand-in for FluxVM central's fleet API (crates/fluxvm-agent/src/central*.rs), for FleetClient tests.

Covers the routes the tests use, with central's shapes and statuses: bearer auth (401), tenant scoping (403 for
operator routes, 404 for other tenants' things), 201 on creates, 204 on deletes, 409 conflicts, and injected
failures (``state.fail``) for retry tests.
"""

import json
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

OPERATOR = "fleet-operator"
NOW = "2026-10-12T10:00:00Z"


class FleetState:
    def __init__(self):
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        self.tokens = {}  # token -> tenant id
        self.tenants = {}
        self.keys = {}  # tenant -> [key]
        self.leases = []
        self.vms = []
        self.images = {}
        self.hooks = []
        self.volumes = {}
        self.pools = {}
        self.requests = []  # (method, path, headers, body)
        self.fail = []  # [method, path, status, count]
        self.nodes = {
            "mac-1": {"name": "mac-1", "vcpus_total": 12, "memory_mib_total": 32768, "healthy": True,
                      "apple": {"nested_virtualization": False}},
        }

    def add_tenant(self, tid):
        token = "fvt_" + uuid.uuid4().hex
        self.tenants[tid] = {"id": tid, "display_name": None, "quota": {}, "disabled": False,
                             "created_at": NOW, "monthly_budget": None}
        self.tokens[token] = tid
        return token


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state = None  # type: FleetState

    def log_message(self, *args):
        pass

    def _send(self, status, body=None, ctype="application/json", headers=None):
        raw = b"" if body is None else (body.encode() if isinstance(body, str) else json.dumps(body).encode())
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if status != 204:
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        if status != 204:
            self.wfile.write(raw)

    def _err(self, status, msg):
        self._send(status, {"error": msg})

    def _handle(self, method):
        st = self.state
        parts = urlsplit(self.path)
        path, q = parts.path, {k: v[0] for k, v in parse_qs(parts.query).items()}
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        with st.lock:
            st.requests.append((method, self.path, dict(self.headers.items()), raw))
            for f in st.fail:
                if f[0] == method and f[1] == path and f[3] > 0:
                    f[3] -= 1
                    return self._send(f[2], {"error": "injected failure"},
                                      headers={"Retry-After": "0"} if f[2] == 429 else None)
            if path == "/healthz":
                return self._send(200, {"ok": True})
            token = (self.headers.get("Authorization") or "")[len("Bearer "):]
            if token == OPERATOR:
                tenant = None
            elif token in st.tokens:
                tenant = st.tokens[token]
            else:
                return self._err(401, "missing or invalid bearer token")
            body = json.loads(raw) if raw else {}
            return self._route(method, path, q, tenant, body)

    def _owned(self, tenant, owner):
        return tenant is None or owner == tenant

    def _route(self, m, path, q, tenant, body):  # noqa: C901 - one switch, like central's router
        st = self.state
        seg = path.strip("/").split("/")
        admin = tenant is None
        if seg == ["fleet", "tenants"]:
            if not admin:
                return self._err(403, "tenant '{}' may not call {} {}".format(tenant, m, path))
            if m == "GET":
                return self._send(200, {"items": list(st.tenants.values())})
            if body.get("id") in st.tenants:
                return self._err(409, "tenant '{}' already exists".format(body["id"]))
            token = st.add_tenant(body["id"])
            st.tenants[body["id"]].update(quota=body.get("quota") or {}, display_name=body.get("display_name"))
            return self._send(201, {"tenant": st.tenants[body["id"]], "token": token, "note": "store this token now"})
        if seg == ["fleet", "tenants", "me"]:
            if admin:
                return self._err(400, "the operator token is not a tenant; use GET /fleet/tenants")
            view = dict(st.tenants[tenant], in_use={"vms": 0, "vcpus": 0, "memory_mib": 0, "leased_hosts": 0},
                        active_leases=[])
            return self._send(200, view)
        if len(seg) == 4 and seg[:2] == ["fleet", "tenants"] and seg[3] == "budget" and m == "PUT":
            tid = tenant if seg[2] == "me" else seg[2]
            if not admin and seg[2] != "me":
                return self._err(403, "operator only")
            st.tenants[tid]["monthly_budget"] = body.get("monthly") or None
            return self._send(200, st.tenants[tid])
        if len(seg) >= 4 and seg[:2] == ["fleet", "tenants"] and seg[3] == "keys":
            tid = tenant if seg[2] == "me" else seg[2]
            if not admin and seg[2] != "me":
                return self._err(403, "operator only")
            keys = st.keys.setdefault(tid, [])
            if len(seg) == 4 and m == "GET":
                return self._send(200, {"items": keys})
            if len(seg) == 4 and m == "POST":
                key = {"id": "key_" + uuid.uuid4().hex[:12], "name": body["name"], "created_at": NOW,
                       "expires_at": None, "read_only": bool(body.get("read_only")), "last_used_at": None}
                keys.append(key)
                return self._send(201, {"key": key, "token": "fvk_" + uuid.uuid4().hex, "note": "shown once"})
            if len(seg) == 5 and m == "DELETE":
                for key in keys:
                    if seg[4] in (key["id"], key["name"]):
                        keys.remove(key)
                        return self._send(200, {"revoked": key})
                return self._err(404, "no API key '{}'".format(seg[4]))
        if seg == ["fleet", "leases"] and m == "POST":
            node = body.get("node") or "mac-1"
            if any(l["node"] == node and l["state"] == "active" for l in st.leases):
                return self._err(503, "no free Mac can be leased right now")
            lease = {"id": "lease-" + uuid.uuid4().hex[:8], "tenant": tenant or body.get("tenant"), "node": node,
                     "started_at": NOW, "min_end": "2026-10-13T10:00:00Z", "state": "active"}
            st.leases.append(lease)
            return self._send(201, {"lease": lease, "node": st.nodes[node]})
        if seg == ["fleet", "leases"] and m == "GET":
            items = [l for l in st.leases if self._owned(tenant, l["tenant"])
                     and (not q.get("state") or l["state"] == q["state"])]
            return self._send(200, {"items": items})
        if len(seg) == 4 and seg[:2] == ["fleet", "leases"] and seg[3] == "release":
            for lease in st.leases:
                if lease["id"] == seg[2] and self._owned(tenant, lease["tenant"]):
                    if q.get("force") != "true":
                        return self._err(409, "lease cannot be released before {} (24-hour minimum)".format(lease["min_end"]))
                    lease.update(state="released", released_at=NOW)
                    return self._send(200, {"lease": lease, "evicted": [], "scrub_required": True})
            return self._err(404, "no lease '{}'".format(seg[2]))
        if seg == ["fleet", "images"]:
            return self._send(200, {"items": list(st.images.values())})
        if seg == ["fleet", "vms"] and m == "POST":
            body.pop("nodeSelector", None)
            node = body.pop("node", None) or "mac-1"
            if tenant:
                body["tenant"] = tenant
            vm = {"id": str(uuid.uuid4()), "name": body["name"], "backend": body.get("backend"), "status": "running",
                  "created_at": NOW, "expires_at": None, "error": None, "request": body, "node": node}
            st.vms.append(vm)
            return self._send(200, {"node": node, "vm": {k: v for k, v in vm.items() if k != "node"}})
        if seg == ["fleet", "vms"] and m == "GET":
            items = [v for v in st.vms if self._owned(tenant, v["request"].get("tenant"))]
            return self._send(200, {"items": items, "unreachable_nodes": [{"node": "mac-9", "reason": "did not answer"}]})
        if len(seg) >= 4 and seg[:2] == ["fleet", "vms"]:
            vm = next((v for v in st.vms if v["node"] == seg[2] and v["id"] == seg[3]
                       and self._owned(tenant, v["request"].get("tenant"))), None)
            if vm is None:
                return self._err(404, "no VM '{}' on node '{}'".format(seg[3], seg[2]))
            if len(seg) == 4 and m == "DELETE":
                st.vms.remove(vm)
                return self._send(204)
            if seg[4] == "exec":
                return self._send(200, {"exit_code": 0, "stdout": "ran " + body["command"] + "\n", "stderr": ""})
            if seg[4] == "logs":
                return self._send(200, "line 1\nline 2\n", ctype="text/plain; charset=utf-8")
            vm["status"] = {"stop": "stopped", "pause": "paused"}.get(seg[4], "running")
            return self._send(200, vm)
        if seg == ["fleet", "webhooks"] and m == "POST":
            if tenant and not body["url"].startswith("https://"):
                return self._err(400, "a tenant's webhook must use https")
            hook = {"id": "whk_" + uuid.uuid4().hex[:12], "owner": tenant, "url": body["url"],
                    "events": body.get("events", []), "created_at": NOW}
            st.hooks.append(hook)
            return self._send(201, {"webhook": hook, "secret": "whsec_" + uuid.uuid4().hex, "note": "shown once"})
        if seg == ["fleet", "webhooks"] and m == "GET":
            return self._send(200, {"items": [dict(h, deliveries=[]) for h in st.hooks if self._owned(tenant, h["owner"])],
                                    "kinds": ["vm.created", "lease.ending", "test"]})
        if len(seg) >= 3 and seg[:2] == ["fleet", "webhooks"]:
            hook = next((h for h in st.hooks if h["id"] == seg[2] and self._owned(tenant, h["owner"])), None)
            if hook is None:
                return self._err(404, "no webhook '{}'".format(seg[2]))
            if m == "DELETE":
                st.hooks.remove(hook)
                return self._send(204)
            return self._send(200, {"delivery": {"notification": "ntf_1", "type": "test", "time": NOW, "attempt": 1,
                                                 "status": 200, "ok": True}})
        if seg == ["fleet", "prices"]:
            if m == "PUT" and not admin:
                return self._err(403, "tenant '{}' may not call PUT /fleet/prices".format(tenant))
            return self._send(200, body if m == "PUT" else {"currency": "USD", "mac_classes": [], "mac_per_hour": 0.6})
        if seg == ["fleet", "billing"]:
            if q.get("format") == "csv":
                return self._send(200, "tenant,from,to,description\n", ctype="text/csv")
            sts = [{"tenant": tenant or "acme", "currency": "USD", "from": NOW, "to": NOW, "lines": [],
                    "total_micros": 0, "total": 0.0}]
            return self._send(200, {"from": NOW, "to": NOW, "currency": "USD", "prices_set": True, "statements": sts})
        if seg == ["fleet", "events"]:
            return self._send(200, {"items": [{"time": NOW, "actor": tenant or "operator", "method": "POST",
                                               "path": "/fleet/vms", "status": 200}][: int(q.get("limit", 200))]})
        if seg == ["fleet", "volumes"] and m == "POST":
            if body["name"] in st.volumes:
                return self._err(409, "a volume named '{}' already exists".format(body["name"]))
            vol = {"id": "vol-" + uuid.uuid4().hex[:8], "name": body["name"], "tenant": tenant,
                   "node": body.get("node", "mac-1"), "size_gib": body["size_gib"], "attached_to": None,
                   "created_at": NOW}
            st.volumes[body["name"]] = vol
            return self._send(201, vol)
        if len(seg) >= 3 and seg[:2] == ["fleet", "volumes"]:
            vol = st.volumes.get(seg[2])
            if vol is None or not self._owned(tenant, vol["tenant"]):
                return self._err(404, "no volume '{}'".format(seg[2]))
            if m == "DELETE":
                del st.volumes[seg[2]]
                return self._send(204)
            if len(seg) == 4:
                vol["size_gib"] = body["size_gib"]
            return self._send(200, vol)
        if seg == ["fleet", "pools"] and m == "POST":
            pool = dict(body, tenant=tenant, ready=0, total=0, created_at=NOW)
            st.pools[body["name"]] = pool
            return self._send(201, pool)
        if len(seg) == 3 and seg[:2] == ["fleet", "pools"]:
            pool = st.pools.get(seg[2])
            if pool is None or not self._owned(tenant, pool["tenant"]):
                return self._err(404, "no pool '{}'".format(seg[2]))
            if m == "PATCH":
                pool.update(body)
            if m == "DELETE":
                del st.pools[seg[2]]
                return self._send(204)
            return self._send(200, pool)
        return self._err(404, "no route {} {}".format(m, path))

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")

    def do_PUT(self):
        self._handle("PUT")

    def do_PATCH(self):
        self._handle("PATCH")

    def do_DELETE(self):
        self._handle("DELETE")


def start():
    state = FleetState()
    handler = type("BoundFleetHandler", (Handler,), {"state": state})
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, state, "http://127.0.0.1:{}".format(server.server_address[1])
