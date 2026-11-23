// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl explain` codes for the image supply-chain commands (`image sbom|sign|verify|scan|policy`) and for the image
//! policy the daemon applies when a container sandbox starts. Same shape as `mac_errors`; `explain` looks here after
//! the API, health and Mac tables.

use crate::mac_errors::MacCode;

pub const CODES: &[MacCode] = &[
    MacCode {
        code: "IMAGE_NOT_CACHED",
        status: None,
        summary: "The image is not on this machine, and the command was told not to use the network.",
        causes: &[
            "`--offline` was given for an image that was never pulled here",
            "the image was pruned (`fluxctl oci prune`) since it was last used",
        ],
        fixes: &[
            "drop `--offline` and the layers are pulled once (network needed)",
            "or pull it first: `fluxctl oci pull IMAGE`",
        ],
        aliases: &["not-cached"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_LAYERS_MISSING",
        status: None,
        summary: "Some of the image's layers are no longer in the local blob store, so its contents cannot be read.",
        causes: &["`fluxctl oci prune` removed blobs no cached image needed", "the state directory was cleaned or moved"],
        fixes: &["pull the image again: `fluxctl oci pull IMAGE`, or add `--refresh` to the command"],
        aliases: &["layers-missing"],
        signatures: &["pull the image first"],
    },
    MacCode {
        code: "IMAGE_REGISTRY_UNREACHABLE",
        status: None,
        summary: "The registry could not be asked about the image's signature.",
        causes: &[
            "no network, or the registry is down",
            "the registry needs a login (`FLUXVM_REGISTRY_USER` / `FLUXVM_REGISTRY_PASSWORD`)",
        ],
        fixes: &[
            "check the network and the credentials, then run it again",
            "`--offline` checks the signatures saved by the last successful verify instead (nothing newer than that is seen)",
        ],
        aliases: &["registry-unreachable"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_UNSIGNED",
        status: None,
        summary: "Nothing vouches for this image: it has no signature.",
        causes: &["the publisher never signed it", "the signature was attached to a different tag or digest"],
        fixes: &[
            "sign it if it is yours: `fluxctl image sign IMAGE` (needs a key: `fluxctl mac keys generate`)",
            "if the policy blocks unsigned images and you accept the risk, start it with `--policy-override` (logged)",
        ],
        aliases: &["unsigned"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_SIGNER_UNTRUSTED",
        status: None,
        summary: "The image is signed, but not by a key you have chosen to trust.",
        causes: &["the signer's public key is not in your trust store"],
        fixes: &["if you know and trust the signer, add their public key: `fluxctl mac keys trust NAME FILE`, then verify again"],
        aliases: &["signer-untrusted", "untrusted-signer"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_SIGNATURE_INVALID",
        status: None,
        summary: "A signature is present but it does not match this image, or the layers on this machine do not match their digests.",
        causes: &[
            "the tag now points at a different image than the one that was signed",
            "a layer file in the local blob store is damaged or was changed",
            "the signature was tampered with",
        ],
        fixes: &[
            "do not run it. Remove the local copy (`fluxctl oci rm IMAGE`) and pull it again",
            "if it persists, ask the publisher to sign the digest you intend to run (`IMAGE@sha256:...`)",
        ],
        aliases: &["signature-invalid", "invalid"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_ADVISORY_SOURCE_MISSING",
        status: None,
        summary: "There is no advisory data on this machine and no source was given to fetch it from.",
        causes: &["`image scan` never ran with `--update`", "FluxVM does not choose a vulnerability feed for you"],
        fixes: &[
            "load advisories from a file or URL: `fluxctl image scan --update --source FILE_OR_URL`",
            "or set `FLUXVM_ADVISORY_SOURCE` once. Accepted: OSV records (a JSON object, array or {\"vulns\": [...]}) or FluxVM's own advisory JSON",
            "until then a scan says it checked nothing; it never reports an image as clean without data",
        ],
        aliases: &["advisory-source", "no-advisories"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_ADVISORY_UNUSABLE",
        status: None,
        summary: "The advisory file could not be read.",
        causes: &["it is not JSON, or not one of the accepted shapes (OSV records or FluxVM's advisory JSON)", "the download was cut short"],
        fixes: &["fetch it again with `--update`", "remove `<state_dir>/oci/advisories.json` to start over"],
        aliases: &["advisory-unusable"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_POLICY_INVALID",
        status: None,
        summary: "The image policy file has a mistake, so it was not applied.",
        causes: &["a misspelt key, or a value that is not allowed (`block_severity` must be critical, high, medium, low or none)"],
        fixes: &[
            "see the message for the line, fix `<state_dir>/oci/policy.toml`, and run `fluxctl image policy` to confirm",
            "`fluxctl image policy init` writes a correct starter if the file does not exist",
        ],
        aliases: &["policy-invalid"],
        signatures: &["the image policy file does not parse"],
    },
    MacCode {
        code: "IMAGE_POLICY_DENIED",
        status: Some(403),
        summary: "The image policy lists this image on its deny list, so it was not started.",
        causes: &["its reference or digest matches a pattern in `deny` of `<state_dir>/oci/policy.toml`"],
        fixes: &[
            "use a different image, or remove the pattern from `deny`",
            "to start it once anyway: `--policy-override` (or `policy_override: true` in the API). It is written to the audit log and `policy-overrides.log`",
        ],
        aliases: &["policy-denied"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_POLICY_UNSIGNED",
        status: Some(403),
        summary: "The image policy blocks images that have not been verified as signed by a trusted key, and this one has not.",
        causes: &[
            "`block_unsigned = true` and `fluxctl image verify IMAGE` has not found a trusted signature for this exact digest",
            "the tag moved to a new digest since it was verified",
        ],
        fixes: &[
            "check it: `fluxctl image verify IMAGE` (the answer is saved and the next start passes if it is trusted)",
            "see why: `fluxctl image policy check IMAGE`",
            "to start it once anyway: `--policy-override` (logged loudly)",
        ],
        aliases: &["policy-unsigned"],
        signatures: &[],
    },
    MacCode {
        code: "IMAGE_POLICY_VULNERABLE",
        status: Some(403),
        summary: "The image policy blocks images with known vulnerabilities at or above a severity, and this one has some.",
        causes: &["`block_severity` is set and the advisory data lists a matching problem for a package in the image"],
        fixes: &[
            "see them: `fluxctl image scan IMAGE`; update the image to a fixed version",
            "if the advisory data is old, refresh it: `fluxctl image scan IMAGE --update --source FILE_OR_URL`",
            "to start it once anyway: `--policy-override` (logged loudly)",
        ],
        aliases: &["policy-vulnerable"],
        signatures: &[],
    },
];

fn norm(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace('-', "_")
}

pub fn find(query: &str) -> Vec<&'static MacCode> {
    let q = norm(query);
    if q.is_empty() {
        return vec![];
    }
    let exact: Vec<&MacCode> = CODES
        .iter()
        .filter(|c| c.code == q || c.aliases.iter().any(|a| norm(a) == q))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    if q.len() >= 4 {
        return CODES.iter().filter(|c| c.code.contains(q.as_str())).collect();
    }
    vec![]
}

/// An error that ends with the command that explains it.
pub fn coded(message: impl Into<String>, code: &str) -> anyhow::Error {
    debug_assert!(CODES.iter().any(|c| c.code == code), "unknown code {code}");
    anyhow::anyhow!("{}\n  more: fluxctl explain {code}", message.into())
}

pub fn card(e: &MacCode) -> String {
    let head = match e.status {
        Some(s) => format!("{} (HTTP {s}, image)", e.code),
        None => format!("{} (image)", e.code),
    };
    let mut s = format!("{head}\n  {}\n\ncauses:\n", e.summary);
    for c in e.causes {
        s.push_str(&format!("  - {c}\n"));
    }
    s.push_str("\nwhat to do:\n");
    for f in e.fixes {
        s.push_str(&format!("  - {f}\n"));
    }
    s
}

pub fn render(found: &[&MacCode], json: bool) -> anyhow::Result<String> {
    if json {
        let one = |e: &MacCode| serde_json::json!({"code": e.code, "status": e.status, "summary": e.summary, "causes": e.causes, "fixes": e.fixes});
        let v = if found.len() == 1 {
            one(found[0])
        } else {
            serde_json::Value::Array(found.iter().map(|e| one(e)).collect())
        };
        return Ok(serde_json::to_string_pretty(&v)? + "\n");
    }
    Ok(found.iter().map(|e| card(e)).collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_code_the_daemon_sends_is_explained() {
        for c in [
            fluxvm_image::supply::CODE_DENIED,
            fluxvm_image::supply::CODE_UNSIGNED,
            fluxvm_image::supply::CODE_VULNERABLE,
        ] {
            assert_eq!(find(c).len(), 1, "{c}");
        }
    }

    #[test]
    fn lookup_accepts_aliases_and_fragments() {
        assert_eq!(find("policy-unsigned")[0].code, "IMAGE_POLICY_UNSIGNED");
        assert!(find("advisory").len() >= 2);
        assert!(find("zzzz").is_empty());
        assert!(card(find("IMAGE_NOT_CACHED")[0]).contains("what to do"));
    }

    #[test]
    fn codes_are_unique_and_every_card_has_a_fix() {
        let mut seen = std::collections::HashSet::new();
        for c in CODES {
            assert!(seen.insert(c.code), "{}", c.code);
            assert!(!c.fixes.is_empty() && !c.causes.is_empty());
        }
    }
}
