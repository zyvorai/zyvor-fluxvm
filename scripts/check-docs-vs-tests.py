#!/usr/bin/env python3
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
"""Keep the vz docs honest: compare what docs/macos.md claims with what the repo can actually run.

Hard failures (exit 1):
  - a doc names a scripts/*.sh file that does not exist
  - an alert rule or dashboard uses a fluxvm_* metric the daemon does not export
Reported, never failing:
  - hardware-log rows that say "Verified" but cite no script (checked by hand once, not repeatable)
  - claims the docs themselves mark unverified / not re-verified
  - vz/macos live-test scripts no doc mentions

    python3 scripts/check-docs-vs-tests.py            # from the repo root
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = [ROOT / "docs" / "macos.md"] + sorted((ROOT / "docs").glob("macos-vz*.md"))
SCRIPT_RE = re.compile(r"scripts/[A-Za-z0-9_.-]+\.sh")
failures, notes = [], []

doc_text = {d: d.read_text() for d in DOCS if d.exists()}
mentioned = set()
for d, t in doc_text.items():
    for m in SCRIPT_RE.finditer(t):
        mentioned.add(m.group(0))
        if not (ROOT / m.group(0)).exists():
            line = t[: m.start()].count("\n") + 1
            failures.append(f"{d.relative_to(ROOT)}:{line}: names {m.group(0)}, which does not exist")

# Hardware log rows: "| item | **Verified...** ..." with no script cited.
macos = doc_text.get(ROOT / "docs" / "macos.md", "")
in_log = False
for n, line in enumerate(macos.splitlines(), 1):
    if line.startswith("## "):
        in_log = line.strip() == "## Hardware verification log"
    if in_log and line.startswith("|") and "**Verified" in line and not SCRIPT_RE.search(line):
        item = line.split("|")[1].strip()
        notes.append(("verified-by-hand", f"docs/macos.md:{n}: {item[:90]}"))

UNVERIFIED = re.compile(r"unverified|not yet re-verified|not verified|not been verified", re.I)
for d, t in doc_text.items():
    for n, line in enumerate(t.splitlines(), 1):
        if UNVERIFIED.search(line):
            notes.append(("unverified", f"{d.relative_to(ROOT)}:{n}: {line.strip()[:110]}"))

for s in sorted((ROOT / "scripts").glob("*live-test.sh")):
    if re.search(r"vz|macos", s.name) and f"scripts/{s.name}" not in mentioned:
        notes.append(("script-not-in-docs", f"scripts/{s.name}"))

# Metrics used by alerts/dashboards must exist in the daemon source.
src = "\n".join(p.read_text(errors="ignore") for p in (ROOT / "crates").rglob("*.rs"))
exported = set(re.findall(r"fluxvm_[a-z0-9_]+", src))
for f in [*(ROOT / "deploy" / "prometheus").glob("*vz*.y*ml"), *(ROOT / "deploy" / "grafana").glob("*.json")]:
    for metric in sorted(set(re.findall(r"fluxvm_[a-z0-9_]+", f.read_text()))):
        if metric not in exported:
            failures.append(f"{f.relative_to(ROOT)}: metric {metric} is not exported by any crate")

for kind in ("verified-by-hand", "unverified", "script-not-in-docs"):
    rows = [m for k, m in notes if k == kind]
    print(f"\n{kind}: {len(rows)}")
    for m in rows:
        print(f"  {m}")
print(f"\n{len(failures)} failure(s)")
for f in failures:
    print(f"  FAIL {f}")
sys.exit(1 if failures else 0)
