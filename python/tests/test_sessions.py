import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from fluxvm import Conflict, FluxVM  # noqa: E402

INFO = {
    "id": "s1", "agent": "codex", "sandbox_id": "b1", "backend": "vz", "children": [],
    "state": "running", "capabilities": {"snapshot": True, "fork": False, "pty": False},
    "policy": {"preset": "offline", "offline": True, "allow_hosts": [], "unenforced": []},
    "workspace": {"path": "/workspace", "cloned": False, "baseline": True},
    "checkpoints": [],
}


class _Handler(BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *a):
        pass

    def _send(self, code, obj=None):
        data = b"" if obj is None else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _handle(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n)) if n else None
        _Handler.calls.append((self.command, self.path, body))
        p = self.path.split("?")[0]
        if self.command == "POST" and p == "/v1/agents/sessions":
            return self._send(201, INFO)
        if self.command == "GET" and p == "/v1/agents/sessions":
            return self._send(200, {"items": [INFO]})
        if self.command == "GET" and p == "/v1/agents/sessions/s1":
            return self._send(200, INFO)
        if p.endswith("/exec"):
            return self._send(200, {"result": "exec", "exit_code": 3, "stdout": "o", "stderr": "e"})
        if p.endswith("/checkpoint"):
            return self._send(200, {"tag": body.get("tag", "cp-1"), "memory": body.get("memory", True)})
        if p.endswith("/restore"):
            return self._send(200, {"restored": body["tag"]})
        if p.endswith("/fork"):
            return self._send(409, {"error": "fork on vz needs a session with no network card of its own"})
        if p.endswith("/events"):
            return self._send(200, {"ledger": [{"kind": "exec"}], "vm_events": []})
        if p.endswith("/artifacts"):
            return self._send(200, {"changes": None, "note": "no baseline"})
        if self.command == "DELETE":
            return self._send(204)
        return self._send(404, {"error": "no route"})

    do_GET = do_POST = do_DELETE = _handle


class SessionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(("127.0.0.1", 0), _Handler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.client = FluxVM("http://127.0.0.1:%d" % cls.server.server_address[1])

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        _Handler.calls.clear()

    def test_create_builds_the_documented_body(self):
        s = self.client.create_session(
            "codex", repo="https://github.com/octocat/Hello-World", policy="developer-safe",
            allow=["a.example"], image="agent-micro", cpus=2, ttl_seconds=600)
        body = _Handler.calls[0][2]
        self.assertEqual(body["workspace"], {"git": "https://github.com/octocat/Hello-World"})
        self.assertEqual(body["runtime"], {"image": "agent-micro", "cpus": 2})
        self.assertEqual(body["policy"], {"preset": "developer-safe",
                                          "network": {"default": "deny", "allow": ["a.example"]}})
        self.assertEqual(body["ttl_seconds"], 600)
        self.assertFalse(s.capabilities["fork"])
        self.assertEqual(s.backend, "vz")

    def test_minimal_create_sends_only_the_agent(self):
        self.client.create_session()
        self.assertEqual(_Handler.calls[0][2], {"agent": "custom"})

    def test_exec_checkpoint_restore_events_artifacts(self):
        s = self.client.get_session("s1")
        r = s.exec("false", timeout=10)
        self.assertEqual((r.exit_code, r.stdout, r.stderr), (3, "o", "e"))
        self.assertEqual(_Handler.calls[-1][2], {"command": "false", "timeout_seconds": 10})
        s.checkpoint(memory=False)
        self.assertEqual(_Handler.calls[-1][2], {"memory": False})
        s.checkpoint("t1")
        self.assertEqual(_Handler.calls[-1][2], {"tag": "t1"})
        self.assertEqual(s.restore("t1")["restored"], "t1")
        self.assertEqual(s.events(since="2026-10-13T00:00:00Z")["ledger"][0]["kind"], "exec")
        self.assertIn("since=2026-10-13T00%3A00%3A00Z", _Handler.calls[-1][1])
        self.assertEqual(s.artifacts()["note"], "no baseline")
        self.assertEqual(len(self.client.list_sessions()), 1)

    def test_fork_refusal_is_a_conflict(self):
        s = self.client.get_session("s1")
        with self.assertRaises(Conflict) as cm:
            s.fork(2)
        self.assertIn("no network card of its own", str(cm.exception))

    def test_context_manager_deletes(self):
        with self.client.get_session("s1"):
            pass
        self.assertEqual(_Handler.calls[-1][0], "DELETE")


if __name__ == "__main__":
    unittest.main()
