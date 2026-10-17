import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from fluxvm import Conflict, FluxVM, NotFound  # noqa: E402
from fluxvm.sessions import Session, parse_sse  # noqa: E402

INFO = {"id": "s1", "agent": "x", "sandbox_id": "b1", "backend": "vz", "state": "running"}


def sse(event, ident, obj):
    return "event: {}\nid: {}\ndata: {}\n\n".format(event, ident, json.dumps(obj)).encode()


class _Handler(BaseHTTPRequestHandler):
    seen = []  # (path, Last-Event-ID)
    drop_first = False  # close the first stream after one event, to force a reconnect

    def log_message(self, *a):
        pass

    def do_GET(self):
        p = self.path.split("?")[0]
        _Handler.seen.append((self.path, self.headers.get("Last-Event-ID")))
        if p == "/v1/agents/sessions/missing/events/stream":
            body = json.dumps({"error": "session not found", "code": "NOT_FOUND"}).encode()
            self.send_response(404)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if p == "/v1/agents/sessions/busy/events/stream":
            body = json.dumps({"error": "busy", "code": "CONFLICT"}).encode()
            self.send_response(409)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        resume = self.headers.get("Last-Event-ID")
        if resume is None:
            self.wfile.write(sse("ledger", "1|", {"op": "create"}))
            if _Handler.drop_first:
                _Handler.drop_first = False
                return  # the connection closes: the client must resume
            self.wfile.write(sse("vm", "1|t", {"event": "vm.start"}))
            self.wfile.write(sse("ledger", "2|t", {"op": "exec"}))
        else:
            self.wfile.write(sse("ledger", "2|t", {"op": "exec"}))
        self.wfile.write(sse("error", "2|t", {"error": "session not found"}))
        self.wfile.flush()


class WatchEventsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(("127.0.0.1", 0), _Handler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.fx = FluxVM("http://127.0.0.1:%d" % cls.server.server_port, token="t")

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        _Handler.seen = []
        _Handler.drop_first = False

    def test_parse_sse(self):
        lines = [b": ping\n", b"event: vm\n", b"id: 5\n", b"data: {\"a\":1}\n", b"\n", b"data: x\n", b"\n"]
        self.assertEqual(list(parse_sse(lines)), [("vm", "5", '{"a":1}'), ("message", None, "x")])

    def test_replays_then_ends_on_error_event(self):
        got = list(Session(self.fx, INFO).watch_events())
        self.assertEqual([(e["type"], e["data"].get("op") or e["data"].get("event") or "gone") for e in got],
                         [("ledger", "create"), ("vm", "vm.start"), ("ledger", "exec"), ("error", "gone")])
        self.assertEqual(_Handler.seen[0][1], None)

    def test_since_goes_in_the_query(self):
        list(Session(self.fx, INFO).watch_events(since="2026-10-17T09:00:00Z"))
        self.assertIn("since=2026-10-17T09%3A00%3A00Z", _Handler.seen[0][0])

    def test_reconnects_with_last_event_id(self):
        _Handler.drop_first = True
        got = list(Session(self.fx, INFO).watch_events(reconnect_delay=0.01))
        self.assertEqual(len(_Handler.seen), 2)
        self.assertEqual(_Handler.seen[1][1], "1|")
        self.assertEqual([e["type"] for e in got], ["ledger", "ledger", "error"])

    def test_errors_carry_the_stable_code(self):
        with self.assertRaises(NotFound) as cm:
            list(Session(self.fx, dict(INFO, id="missing")).watch_events())
        self.assertEqual(cm.exception.code, "NOT_FOUND")
        with self.assertRaises(Conflict) as cm:
            list(Session(self.fx, dict(INFO, id="busy")).watch_events())
        self.assertEqual(cm.exception.code, "CONFLICT")


if __name__ == "__main__":
    unittest.main()
