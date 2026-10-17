#!/usr/bin/env python3
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
"""Writes a CycloneDX 1.5 SBOM of the Rust dependencies pinned in Cargo.lock.

    scripts/make-sbom.py --version 0.4.12 > sbom.cdx.json

It lists what the lock file resolves (every crate, with its source and checksum), not what ends up linked into one
binary, so it is a superset. Packages of this workspace are the "components" of type application/library with no source.
"""
import argparse, json, sys, tomllib, uuid, datetime

ap = argparse.ArgumentParser()
ap.add_argument("--version", required=True)
ap.add_argument("--lock", default="Cargo.lock")
a = ap.parse_args()
with open(a.lock, "rb") as f:
    lock = tomllib.load(f)

components = []
for p in sorted(lock.get("package", []), key=lambda p: (p["name"], p["version"])):
    src = p.get("source", "")
    c = {"type": "library", "name": p["name"], "version": p["version"],
         "purl": f"pkg:cargo/{p['name']}@{p['version']}", "bom-ref": f"{p['name']}@{p['version']}"}
    if src.startswith("registry+"):
        c["hashes"] = [{"alg": "SHA-256", "content": p["checksum"]}] if p.get("checksum") else []
    elif src:
        c["externalReferences"] = [{"type": "vcs", "url": src.split("+", 1)[-1]}]
    else:
        c["scope"] = "required"
        c["properties"] = [{"name": "fluxvm:workspace", "value": "true"}]
    components.append(c)

json.dump({
    "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
    "serialNumber": "urn:uuid:" + str(uuid.uuid4()),
    "metadata": {"timestamp": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                 "component": {"type": "application", "name": "fluxvm", "version": a.version}},
    "components": components,
}, sys.stdout, indent=1)
print()
