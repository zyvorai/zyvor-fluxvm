// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The API error codes in one table, for `fluxctl explain`.
//!
//! Every error body is `{"error": "<message>", "code": "<CODE>"}` and clients branch on `code`. The codes themselves
//! come from `fluxvm-api` (`error_code(status)` plus two special codes). This table says what each one means, what
//! usually causes it and what to do. `crates/fluxvm-api` has a test that every code the API can send is listed here,
//! so the two cannot drift.

/// One documented error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode {
    /// The value of `code` in the error body.
    pub code: &'static str,
    /// The HTTP status it rides on.
    pub status: u16,
    /// One line: what it means.
    pub summary: &'static str,
    /// What usually causes it.
    pub causes: &'static [&'static str],
    /// What to do about it.
    pub fixes: &'static [&'static str],
}

/// Every code the API sends.
pub const TABLE: &[ErrorCode] = &[
    ErrorCode {
        code: "BAD_REQUEST",
        status: 400,
        summary: "The request itself is malformed.",
        causes: &[
            "the JSON body does not parse, or a field has the wrong type",
            "a query parameter or an Idempotency-Key header is malformed",
            "a value fails validation (for example numa memory that does not add up to memory_mib)",
        ],
        fixes: &[
            "read the `error` text: it names the field",
            "lint a VM spec offline first: `fluxctl vm-spec check spec.json`",
        ],
    },
    ErrorCode {
        code: "UNAUTHORIZED",
        status: 401,
        summary: "No valid credential was sent.",
        causes: &[
            "no `Authorization: Bearer` header, or the token is wrong or expired",
            "with mTLS: the client certificate has no CN, or the CA does not match",
        ],
        fixes: &[
            "pass `--server-token` (or set FLUXVM_TOKEN) to fluxctl",
            "check the host config offline: `fluxctl config check`",
        ],
    },
    ErrorCode {
        code: "STEP_UP_REQUIRED",
        status: 401,
        summary: "This route needs a fresh TOTP code on top of the token.",
        causes: &["step-up authentication is on and the X-Fluxvm-Totp header was missing or wrong"],
        fixes: &[
            "send a current TOTP code in the X-Fluxvm-Totp header (fluxctl: `--totp` or FLUXVM_TOTP) and retry",
        ],
    },
    ErrorCode {
        code: "FORBIDDEN",
        status: 403,
        summary: "The credential is valid but its role may not use this route.",
        causes: &[
            "a read-only role calling a write route",
            "a session token calling a route outside its own session",
            "the feature is disabled in the host config",
        ],
        fixes: &[
            "use a token with a role that allows the route",
            "ask the operator to enable the feature in the host config",
        ],
    },
    ErrorCode {
        code: "NOT_FOUND",
        status: 404,
        summary: "No such VM, session or object visible to you.",
        causes: &[
            "a wrong or already deleted id",
            "the object belongs to another tenant (404, never 403, so ids do not leak)",
        ],
        fixes: &[
            "list what exists: `fluxctl list`",
            "use the token of the tenant that owns it",
        ],
    },
    ErrorCode {
        code: "CONFLICT",
        status: 409,
        summary: "The object is in a state that does not allow this.",
        causes: &[
            "the VM is not running (or is already running)",
            "an identical request is still in flight",
            "a changeset whose files moved since it was computed",
        ],
        fixes: &[
            "check the state with `fluxctl status <id>` and retry when it fits",
            "recompute the changeset",
        ],
    },
    ErrorCode {
        code: "PRECONDITION_FAILED",
        status: 412,
        summary: "The If-Match value no longer matches the object.",
        causes: &["someone else changed the object after you read it"],
        fixes: &["read it again, reapply your change, send the new ETag in If-Match"],
    },
    ErrorCode {
        code: "PAYLOAD_TOO_LARGE",
        status: 413,
        summary: "The request body is over the size limit.",
        causes: &["an upload, a cloud-init user-data or a policy larger than the limit"],
        fixes: &[
            "send a smaller body, or put big files in the VM another way (a disk, a shared folder)",
        ],
    },
    ErrorCode {
        code: "UNPROCESSABLE",
        status: 422,
        summary: "The same Idempotency-Key was reused with a different request.",
        causes: &["a retry that changed the body but kept the key"],
        fixes: &[
            "use a new Idempotency-Key for a different request; keep the key only for exact retries",
        ],
    },
    ErrorCode {
        code: "PRECONDITION_REQUIRED",
        status: 428,
        summary: "The request needs a condition first.",
        causes: &[
            "an operator approval is required before the action runs",
            "the route requires an If-Match header",
        ],
        fixes: &["approve the pending request, or send If-Match with the current ETag"],
    },
    ErrorCode {
        code: "APPROVAL_REQUIRED",
        status: 428,
        summary: "An operator has to approve this action; the body carries `approval_id`.",
        causes: &["an agent session's policy asks a human before running the command or action"],
        fixes: &[
            "approve or deny the request with its `approval_id` (`fluxctl agent approvals <session>`), then retry",
        ],
    },
    ErrorCode {
        code: "RATE_LIMITED",
        status: 429,
        summary: "Too many requests, or a quota is used up.",
        causes: &["a per-token request rate, or a per-token VM or memory quota"],
        fixes: &["wait for the `Retry-After` seconds; check usage with `fluxctl quota`"],
    },
    ErrorCode {
        code: "NOT_IMPLEMENTED",
        status: 501,
        summary: "The backend does not support this route.",
        causes: &["for example a QEMU-only feature asked of Firecracker or the vz backend"],
        fixes: &["use a backend that has the feature (the route's docs say which)"],
    },
    ErrorCode {
        code: "UNAVAILABLE",
        status: 503,
        summary: "The host cannot do this right now.",
        causes: &["no free capacity (for example no free GPU), or a needed component is down"],
        fixes: &[
            "retry later",
            "check the host: `fluxctl doctor`, `fluxctl readyz`",
        ],
    },
    ErrorCode {
        code: "INTERNAL",
        status: 500,
        summary: "A server fault (any other 5xx).",
        causes: &["a bug, or a host problem the daemon did not map to a more specific code"],
        fixes: &[
            "look in the daemon log for the same time",
            "collect an incident bundle: `fluxctl incident <id>`",
        ],
    },
];

/// Looks a code up by name. Case, `-` versus `_` and spaces do not matter (`not-found`, `Not Found`, `NOT_FOUND`).
pub fn lookup(name: &str) -> Option<&'static ErrorCode> {
    let want = normalize(name);
    TABLE.iter().find(|e| e.code == want)
}

/// The code the API sends for an HTTP status (`fluxvm-api`'s `error_code`): the first entry of the table at that
/// status, 500 and above falling back to `INTERNAL`.
pub fn for_status(status: u16) -> Option<&'static ErrorCode> {
    let exact = TABLE.iter().find(|e| {
        e.status == status && e.code != "STEP_UP_REQUIRED" && e.code != "APPROVAL_REQUIRED"
    });
    exact.or_else(|| {
        if status >= 500 {
            lookup("INTERNAL")
        } else {
            None
        }
    })
}

/// The closest code to a mistyped name, if one is near enough to be a likely typo.
pub fn suggest(name: &str) -> Option<&'static str> {
    let want = normalize(name);
    let (best, dist) = TABLE
        .iter()
        .map(|e| (e.code, edit_distance(&want, e.code)))
        .min_by_key(|(_, d)| *d)?;
    (dist <= 3.max(want.len() / 4)).then_some(best)
}

/// Codes whose name contains the text (after normalizing), for a partial word such as `rate`.
pub fn search(text: &str) -> Vec<&'static ErrorCode> {
    let want = normalize(text);
    if want.is_empty() {
        return Vec::new();
    }
    TABLE.iter().filter(|e| e.code.contains(&want)).collect()
}

fn normalize(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| {
            if c == '-' || c == ' ' {
                '_'
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_complete() {
        let mut names: Vec<&str> = TABLE.iter().map(|e| e.code).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate code in TABLE");
        for e in TABLE {
            assert!(!e.summary.is_empty() && !e.causes.is_empty() && !e.fixes.is_empty());
            assert!(e.code.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
            assert!((400..600).contains(&e.status), "{}", e.code);
        }
    }

    #[test]
    fn lookup_ignores_case_dashes_and_spaces() {
        for q in ["NOT_FOUND", "not_found", "not-found", " Not Found "] {
            assert_eq!(lookup(q).map(|e| e.status), Some(404), "{q}");
        }
        assert!(lookup("NOPE").is_none());
    }

    #[test]
    fn status_maps_like_the_api() {
        assert_eq!(for_status(404).unwrap().code, "NOT_FOUND");
        assert_eq!(for_status(428).unwrap().code, "PRECONDITION_REQUIRED");
        assert_eq!(for_status(401).unwrap().code, "UNAUTHORIZED");
        assert_eq!(for_status(502).unwrap().code, "INTERNAL");
        assert!(for_status(418).is_none());
    }

    #[test]
    fn suggests_near_misses_only() {
        assert_eq!(suggest("NOT_FOND"), Some("NOT_FOUND"));
        assert_eq!(suggest("ratelimited"), Some("RATE_LIMITED"));
        assert_eq!(suggest("completely unrelated words here"), None);
    }

    #[test]
    fn search_matches_partial_words() {
        let hits: Vec<&str> = search("required").iter().map(|e| e.code).collect();
        assert_eq!(
            hits,
            [
                "STEP_UP_REQUIRED",
                "PRECONDITION_REQUIRED",
                "APPROVAL_REQUIRED"
            ]
        );
        assert!(search("").is_empty());
    }
}
