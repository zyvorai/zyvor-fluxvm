# Image supply chain: SBOM, signatures, vulnerability scan and policy

Four checks for **OCI container images** (the images `fluxctl sandbox run IMAGE` and `fluxctl oci pull` use). They work with no
set-up, they are in Studio (Images, Security tab) as well as `fluxctl`, and everything except fetching layers, asking a registry
and `scan --update` runs offline.

What already existed and is reused, not rebuilt: Ed25519 image signing, the trust store and the signature-as-OCI-referrer layout
of `fluxctl mac` ([macos-signing.md](macos-signing.md), `fluxctl mac keys`), `fluxctl image attach|referrers` (OCI 1.1
referrers), the blob store `fluxctl oci pull` fills, and the daemon audit log. A key you trust with `fluxctl mac keys trust` is
trusted here too. What is new: reading package databases out of layers, advisory matching, the policy file, and the
verdicts.

These commands are about container images. A `fluxctl mac` image is a macOS VM disk with no package database inside, so it has no
SBOM or scan; `fluxctl mac verify` is its signature check.

| Command | What it does |
|---|---|
| `fluxctl image sbom IMAGE [--sbom-format spdx\|cyclonedx] [--out FILE]` | SPDX 2.3 or CycloneDX 1.5 JSON from the layers |
| `fluxctl image sign IMAGE` | sign the manifest digest and attach the signature to the registry |
| `fluxctl image verify IMAGE [--offline]` | signature plus a re-hash of the local layers; one plain verdict; exits 1 unless TRUSTED |
| `fluxctl image scan [IMAGE] [--update --source FILE_OR_URL] [--fail-on SEV]` | match the SBOM against the cached advisory data |
| `fluxctl image policy [show\|init\|check IMAGE\|overrides]` | the allow/deny file |
| `fluxctl sandbox run IMAGE --policy-override` | start an image against the policy (loud) |

IMAGE is a reference, a digest, or a 12+ character digest prefix. An image that is not known on this machine is pulled once
(network); `--offline` refuses instead (`IMAGE_NOT_CACHED`), and `--refresh` pulls again (for a tag that moved).

## SBOM

Layers are read in order, whiteouts are honoured, and the package databases that remain are listed:

- Debian and Ubuntu: `var/lib/dpkg/status` and `status.d/` (distroless images)
- Alpine: `lib/apk/db/installed`
- Python: `*.dist-info/METADATA`, `*.egg-info/PKG-INFO` under `site-packages` or `dist-packages`
- Node: `node_modules/NAME/package.json` (scoped names too)

Each package carries a package URL (`pkg:deb/debian/openssl@3.0.11-1?arch=arm64`). The SBOM is cached by manifest digest in
`<state_dir>/oci/sbom/`, so a second call (and every policy check) costs nothing.

**Not read:** RPM databases (sqlite and Berkeley DB), Go and Rust binaries, Java jars, anything installed outside a package
manager. If an RPM database is present the SBOM says so in `notes` instead of presenting an empty list as a finding. A scratch or
static image has no database and the SBOM says that too. Licences are `NOASSERTION`.

## Verify

`image verify` answers one question in one word:

| Verdict | Meaning |
|---|---|
| `TRUSTED` | a signature on the digest verifies against a key in your trust store |
| `UNSIGNED` | no signature (`IMAGE_UNSIGNED`) |
| `SIGNED, BUT NOT BY A KEY YOU TRUST` | valid signature, key not in the trust store (`IMAGE_SIGNER_UNTRUSTED`) |
| `INVALID` | a signature does not match, a layer on this machine does not match its digest, or the tag now points somewhere else (`IMAGE_SIGNATURE_INVALID`) |

Layers are re-hashed against the manifest, so a damaged or modified blob store is caught even with a valid signature. For a
multi-platform image the signature on the index covers each member. The result is saved to `<state_dir>/oci/verify/` and is what
the policy reads. `--offline` uses the signatures the last online check saved.

This is not cosign: there is no keyless flow, transparency log or certificate chain ([macos-signing.md](macos-signing.md)).
Third-party images signed with cosign are `UNSIGNED` here.

## Scan

Advisory data is a local file, `<state_dir>/oci/advisories.json`. **FluxVM does not choose a vulnerability feed for you:**
`scan --update --source FILE_OR_URL` (or `FLUXVM_ADVISORY_SOURCE`) replaces it from a source you name, and accepts OSV records
(an object, an array or `{"vulns": [...]}`; ranges become introduced/fixed pairs) or FluxVM's own JSON
(`{"format": "fluxvm-advisories-v1", "updated_unix": N, "source": "...", "advisories": [{"id", "package", "ecosystem", "severity", "introduced", "fixed", "summary"}]}`).
A bad download never replaces good data.

The report always says what it was matched against: "advisory data is 12 days old (4021 advisories)". With no data it says
"nothing was checked" and `--fail-on` exits 2; it never reports an image as clean. Matching is by package name and version
(dpkg order for deb and apk; an approximation of semver and PEP 440 for npm and pypi). Debian security data keyed by *source*
package name can be missed because the SBOM lists binary packages, and OSV `versions` lists and `last_affected` are not used.
Severity comes from the record (`database_specific.severity`); CVSS vectors are not scored, so such records are `unknown`.

## Policy

`fluxctl image policy init` writes `<state_dir>/oci/policy.toml`:

```toml
block_unsigned = true         # refuse images `image verify` has not found TRUSTED
block_severity = "critical"   # refuse findings at this severity or worse; "none" turns it off
allow = []                    # reference patterns (*) that skip the two rules above
deny = []                     # reference patterns that are never started
max_advisory_age_days = 30    # older advisory data earns a warning
```

No file, no policy: nothing is blocked. The daemon checks it every time a container sandbox starts (the `vz` OCI runtime and the
native Linux runtime), after the pull and before anything runs; a refusal is HTTP 403 with `IMAGE_POLICY_DENIED`,
`IMAGE_POLICY_UNSIGNED` or `IMAGE_POLICY_VULNERABLE` and the sentence says what to run. A typo in the file is an error
(`IMAGE_POLICY_INVALID`), not a silent "no policy". `block_unsigned` is judged from the saved `image verify` result for that exact
digest, so run `image verify` once per image (the daemon does not fetch signatures itself). With no advisory data,
`block_severity` cannot fire and every start logs a warning that the image was not checked.

`fluxctl image policy check IMAGE` runs the same decision the daemon will.

### The override

`fluxctl sandbox run IMAGE --policy-override`, or `"policy_override": true` in the sandbox request's `oci` object, starts the
image anyway. It is deliberately loud: a WARN on the `fluxvm_audit` log target (`POLICY OVERRIDE: ... was started against the
image policy`, with the actor token and every violation), a line in `<state_dir>/oci/policy-overrides.log` (read it with
`fluxctl image policy overrides`), and a red banner in Studio. An override with no policy in force is silent, since nothing was
overridden.

## Studio

Images, **Security**: one row per cached image with a badge (Looks good, Known problems, Not verified, Not scanned, Blocked by
policy, Could not check), the signature state, the findings by severity, package count, a **Download SBOM** button and Copy as
fluxctl for the verify, scan and SBOM commands; above them the policy state, how old the advisory data is, and any recent
overrides. The page only reads (`GET /v1/oci/security`, `GET /v1/oci/images/{digest}/sbom?format=spdx|cyclonedx`); the checks
that change something are the commands above. The list covers images cached by `fluxctl oci pull` (macOS `vz` hosts keep a
rootfs entry); images only pulled by a Linux runtime appear after `image verify|scan|sbom` has seen them on that host.
