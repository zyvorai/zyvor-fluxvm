"""Live check of Session.watch_events against a real daemon. Skips unless FLUXVM_LIVE_URL is set.

The daemon needs a session template named "fc" (a flux-vm spec.json under sandbox.templates_dir).
    FLUXVM_LIVE_URL=http://127.0.0.1:7788 python3 tests/live_events.py
"""
import os
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from fluxvm import FluxVM  # noqa: E402
from fluxvm.sessions import Session  # noqa: E402


def main() -> int:
    url = os.environ.get("FLUXVM_LIVE_URL")
    if not url:
        print("FLUXVM_LIVE_URL not set; skipping")
        return 0
    fx = FluxVM(url, token=os.environ.get("FLUXVM_TOKEN"), timeout=120)
    info = fx._json("POST", "/v1/agents/sessions",
                    {"agent": "custom", "runtime": {"template": "fc"}}, timeout=240)
    s = Session(fx, info)
    got = []
    t = threading.Thread(target=lambda: got.extend(s.watch_events()), daemon=True)
    t.start()
    try:
        for _ in range(30):  # the guest agent needs a moment after create
            try:
                s.exec("echo live")
                break
            except Exception:
                time.sleep(1)
    finally:
        s.delete()
    t.join(30)
    kinds = [(e["type"], e["data"].get("kind") or e["data"].get("event")) for e in got]
    print(kinds)
    ok = not t.is_alive() and got and got[-1]["type"] == "error" and ("ledger", "exec") in kinds
    print("OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
