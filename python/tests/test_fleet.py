import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

import fleet_mock  # noqa: E402
from fluxvm import (  # noqa: E402
    AuthError, Conflict, ConnectionFailed, FleetClient, ForbiddenError, NotFound, RateLimited, ServiceUnavailable,
)


class FleetClientTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server, cls.state, cls.url = fleet_mock.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        with self.state.lock:
            self.state.reset()
            self.token = self.state.add_tenant("acme")
            self.state.images["macos-27-xcode"] = {
                "name": "macos-27-xcode", "display_name": "macOS 27 with Xcode", "guest_os": "macos",
                "image": "/Users/fluxvm/vms/xcode/disk.raw", "vcpus": 6, "memory_mib": 16384, "disk_size_gib": None}
        self.sleeps = []
        self.fleet = FleetClient(self.url, token=self.token, sleep=self.sleeps.append)
        self.op = FleetClient(self.url, token=fleet_mock.OPERATOR, sleep=self.sleeps.append)

    def last_request(self):
        method, path, headers, body = self.state.requests[-1]
        return method, path, headers, json.loads(body) if body else None

    def test_auth_header_and_errors(self):
        self.assertEqual(self.fleet.me()["id"], "acme")
        _, _, headers, _ = self.last_request()
        self.assertEqual(headers["Authorization"], "Bearer " + self.token)
        self.assertTrue(headers["User-Agent"].startswith("fluxvm-fleet-python/"))
        with self.assertRaises(AuthError) as ctx:
            FleetClient(self.url, token="nope").me()
        self.assertEqual(ctx.exception.status, 401)
        with self.assertRaises(ForbiddenError) as ctx:
            self.fleet.list_tenants()
        self.assertIn("may not call", ctx.exception.message)

    def test_tenants_and_keys(self):
        created = self.op.create_tenant("beta", display_name="Beta", quota={"max_vms": 5})
        self.assertTrue(created["token"].startswith("fvt_"))
        self.assertEqual(created["tenant"]["quota"], {"max_vms": 5})
        with self.assertRaises(Conflict):
            self.op.create_tenant("beta")
        key = self.fleet.create_key("ci", expires_in_days=30)
        self.assertTrue(key["key"]["id"].startswith("key_"))
        self.assertEqual(self.last_request()[3], {"name": "ci", "read_only": False, "expires_in_days": 30})
        self.assertEqual([k["name"] for k in self.fleet.list_keys()], ["ci"])
        self.assertEqual(self.fleet.revoke_key("ci")["id"], key["key"]["id"])
        self.assertEqual(self.fleet.set_budget(500)["monthly_budget"], 500)
        self.assertEqual(self.last_request()[:2], ("PUT", "/fleet/tenants/me/budget"))

    def test_lease_launch_exec_delete(self):
        lease = self.fleet.create_lease(hours=48, nested_virtualization=True)["lease"]
        self.assertEqual(self.last_request()[3], {"hours": 48, "nested_virtualization": True})
        vol = self.fleet.create_volume("cache", 50)
        res = self.fleet.launch_from_catalog("ci-1", "macos-27-xcode", networks=["build"], volumes=[vol["name"]],
                                             extra={"labels": {"job": "42"}})
        sent = self.last_request()[3]
        self.assertEqual(sent["image"], "/Users/fluxvm/vms/xcode/disk.raw")
        self.assertEqual(sent["apple"], {"guest_os": "macos", "networks": [{"name": "build"}]})
        self.assertEqual((sent["vcpus"], sent["memory_mib"], sent["backend"]), (6, 16384, "vz"))
        self.assertEqual(sent["volumes"], ["cache"])
        self.assertEqual(sent["labels"], {"job": "42"})
        node, vm_id = res["node"], res["vm"]["id"]
        self.assertEqual(self.fleet.get_vm(node, vm_id)["name"], "ci-1")
        self.assertEqual(self.fleet.find_vm("ci-1")["id"], vm_id)
        with self.assertRaises(ServiceUnavailable):
            self.fleet.get_vm("mac-9", "x")  # its Mac did not answer: not "gone"
        with self.assertRaises(NotFound):
            self.fleet.get_vm(node, "missing")
        self.assertEqual(self.fleet.exec_vm(node, vm_id, "sw_vers", timeout_seconds=30)["exit_code"], 0)
        self.assertEqual(self.fleet.power_vm(node, vm_id, "stop")["status"], "stopped")
        with self.assertRaises(ValueError):
            self.fleet.power_vm(node, vm_id, "explode")
        self.assertIn("line 2", self.fleet.vm_logs(node, vm_id, lines=2))
        self.assertTrue(self.last_request()[1].endswith("/logs?lines=2"))
        self.fleet.delete_vm(node, vm_id)
        with self.assertRaises(NotFound):
            self.fleet.get_vm(node, vm_id)
        with self.assertRaises(Conflict) as ctx:
            self.fleet.release_lease(lease["id"])
        self.assertIn("24-hour minimum", ctx.exception.message)
        self.assertEqual(self.op.release_lease(lease["id"], force=True)["lease"]["state"], "released")
        self.assertEqual(self.fleet.list_leases(state="active"), [])

    def test_webhooks_billing_volumes_pools(self):
        hook = self.fleet.create_webhook("https://hooks.example.com/x", events=["lease.ending"])
        self.assertTrue(hook["secret"].startswith("whsec_"))
        listed = self.fleet.list_webhooks()
        self.assertNotIn("secret", listed["items"][0])
        self.assertTrue(self.fleet.test_webhook(hook["webhook"]["id"])["ok"])
        self.fleet.delete_webhook(hook["webhook"]["id"])
        self.assertEqual(self.fleet.billing(month="2026-10")["statements"][0]["tenant"], "acme")
        self.assertTrue(self.fleet.billing_csv().startswith("tenant,"))
        with self.assertRaises(ForbiddenError):
            self.fleet.set_prices({"currency": "EUR"})
        self.assertEqual(self.fleet.events(limit=1)[0]["actor"], "acme")
        self.assertEqual(self.fleet.resize_volume(self.fleet.create_volume("data", 10)["name"], 20)["size_gib"], 20)
        self.fleet.delete_volume("data")
        pool = self.fleet.create_pool("ci", "macos-27-xcode", min_ready=1, max_total=3)
        self.assertEqual((pool["tenant"], pool["max_total"]), ("acme", 3))
        self.assertEqual(self.fleet.update_pool("ci", min_ready=2)["min_ready"], 2)
        self.assertEqual(self.last_request()[3], {"min_ready": 2})
        self.fleet.delete_pool("ci")
        with self.assertRaises(NotFound):
            self.fleet.get_pool("ci")

    def test_retries_idempotent_calls_only(self):
        with self.state.lock:
            self.state.fail.append(["GET", "/fleet/images", 503, 2])
        self.assertEqual(len(self.fleet.list_images()), 1)
        self.assertEqual(len(self.sleeps), 2)
        self.assertLess(self.sleeps[0], self.sleeps[1] + 1)
        with self.state.lock:
            self.state.fail.append(["GET", "/fleet/capacity", 429, 10])
        with self.assertRaises(RateLimited):
            self.fleet.capacity()
        n = sum(1 for r in self.state.requests if r[1] == "/fleet/capacity")
        self.assertEqual(n, 4)
        with self.state.lock:
            self.state.fail.append(["POST", "/fleet/leases", 502, 1])
        before = len(self.state.requests)
        with self.assertRaises(Exception) as ctx:
            self.fleet.create_lease()
        self.assertEqual(getattr(ctx.exception, "status", None), 502)
        self.assertEqual(len(self.state.requests) - before, 1)

    def test_connection_failure(self):
        dead = FleetClient("http://127.0.0.1:9", token="x", retries=1, sleep=lambda s: None)
        with self.assertRaises(ConnectionFailed):
            dead.capacity()


if __name__ == "__main__":
    unittest.main()
