// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Friendly explanations for the failures people hit most on `fluxctl mac`, `mac-fleet` and `kube` on a Mac. `fluxctl explain`
//! looks here after the API and cluster-health tables. [`hint_for`] maps the text of a failure to the code that explains it.

/// One explained failure. Same shape as the other `explain` tables.
pub struct MacCode {
    pub code: &'static str,
    /// The HTTP status it rides on, when it has one.
    pub status: Option<u16>,
    pub summary: &'static str,
    pub causes: &'static [&'static str],
    pub fixes: &'static [&'static str],
    /// Other spellings `explain` accepts.
    pub aliases: &'static [&'static str],
    /// Lower-case text that identifies the failure in an error message.
    pub signatures: &'static [&'static str],
}

pub const CODES: &[MacCode] = &[
    MacCode {
        code: "MAC_SLOT_LIMIT",
        status: Some(409),
        summary: "Both macOS VM slots on this Mac are taken (Apple's licence allows 2 macOS guests at once).",
        causes: &[
            "two macOS instances are already running on this Mac (`fluxctl mac ps`)",
            "an earlier `mac run` was left behind after a crash, so its slot is still held",
        ],
        fixes: &[
            "see what holds the slots: `fluxctl mac ps -a`",
            "remove one you do not need: `fluxctl mac rm NAME`",
            "on a fleet, let the scheduler pick a free Mac: `fluxctl mac-fleet run -f hosts.yaml --template NAME:TAG -- CMD` (add `--queue-timeout 600` to wait for a slot)",
        ],
        aliases: &["slot-limit", "slots", "409"],
        signatures: &[
            "slot limit",
            "2 macos",
            "two macos",
            "already running 2",
            "macos guests at once",
            "409 conflict",
        ],
    },
    MacCode {
        code: "MAC_SESSION_LOCKED",
        status: Some(503),
        summary: "The Mac has no unlocked login session, so macOS VMs cannot start or restore.",
        causes: &[
            "the screen is locked, or nobody is logged in at the console (Virtualization.framework needs a GUI session)",
            "the Mac is a headless mini that rebooted and is sitting at the login window",
        ],
        fixes: &[
            "unlock the Mac, or log in at the console once",
            "for a headless Mac enable automatic login (System Settings > Users & Groups) and keep it awake: `fluxctl mac host keepawake install`",
            "check the state: `fluxctl mac doctor` (the login session row) or `fluxctl mac host headless-check`",
        ],
        aliases: &["session-locked", "locked", "503"],
        signatures: &[
            "screen is locked",
            "session is locked",
            "no unlocked",
            "cgssessionscreenislocked",
            "503 service unavailable",
        ],
    },
    MacCode {
        code: "SSH_HOSTKEY_MISMATCH",
        status: None,
        summary: "ssh refused a Mac because its host key is not the one in known_hosts.",
        causes: &[
            "the Mac was reinstalled or its ssh keys were regenerated, so the key changed",
            "the address now belongs to a different machine (DHCP), or someone is intercepting the connection",
            "first contact with a Mac whose key was never recorded (fleet commands refuse unknown keys by default)",
        ],
        fixes: &[
            "first contact, and you trust the network: add `--accept-new-hostkey` to the command",
            "verify the key on the Mac (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`), then compare with `ssh-keyscan -t ed25519 HOST | ssh-keygen -lf -`",
            "key legitimately changed: remove the old entry with `ssh-keygen -R HOST`, then connect again (a changed key is never accepted automatically)",
        ],
        aliases: &["hostkey", "host-key", "host-key-mismatch", "known-hosts"],
        signatures: &[
            "host key verification failed",
            "remote host identification has changed",
            "host key mismatch",
            "no matching host key",
        ],
    },
    MacCode {
        code: "MAC_TEMPLATE_MISSING",
        status: None,
        summary: "The image (template) you named is not in the local store, or a Mac of the fleet does not hold it.",
        causes: &[
            "a typo in the name or tag (`name:tag`), or it was never pulled or imported on this Mac",
            "the template is on one Mac of the fleet but not the one the job landed on",
            "the template directory is missing one of disk.raw, hardware.bin, auxiliary.bin",
        ],
        fixes: &[
            "list what exists: `fluxctl mac images`",
            "get it: `fluxctl mac pull REGISTRY/REPO:TAG` or `fluxctl mac import DIR NAME:TAG`",
            "fleet: put it on every Mac, digest-verified: `fluxctl mac-fleet templates distribute -f hosts.yaml --template REG/REPO:TAG@sha256:HEX`",
            "the guided flow does this for you: `fluxctl mac quickstart`",
        ],
        aliases: &[
            "template-missing",
            "no-image",
            "missing-template",
            "image-not-found",
        ],
        signatures: &[
            "no such image",
            "image not found",
            "not in the local store",
            "unknown image",
            "template not found",
            "is not in the store",
        ],
    },
    MacCode {
        code: "MAC_DISK_FULL",
        status: None,
        summary: "There is not enough free disk on the Mac for the macOS image, an instance clone or the k3s guest.",
        causes: &[
            "a macOS template is 20 to 60 GiB and every full copy of it counts; clones are free only on the same APFS volume",
            "the store (FLUXVM_MAC_STORE) is on a small or non-APFS volume, so clones become real copies",
            "old instances, images or build cache are still around",
        ],
        fixes: &[
            "see the numbers: `fluxctl mac doctor` (the free disk row)",
            "drop what you do not use: `fluxctl mac rm NAME`, `fluxctl mac rmi NAME:TAG`, `fluxctl mac cache prune`",
            "move the store to a bigger APFS volume: set FLUXVM_MAC_STORE to a directory on it",
            "k3s guest: give back freed space with `fluxctl kube trim`, or create it smaller with `kube up --disk-gib`",
        ],
        aliases: &["disk-full", "no-space", "enospc"],
        signatures: &[
            "no space left on device",
            "not enough free",
            "disk full",
            "enospc",
            "out of disk",
        ],
    },
];

fn norm(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace('-', "_")
}

/// A code, one of its aliases (including the HTTP status), or a fragment of a code.
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
        return CODES
            .iter()
            .filter(|c| c.code.contains(q.as_str()))
            .collect();
    }
    vec![]
}

/// The code that explains an error message, matched on the text ssh, the daemon and fluxctl print.
pub fn hint_for(message: &str) -> Option<&'static MacCode> {
    let m = message.to_ascii_lowercase();
    CODES
        .iter()
        .find(|c| c.signatures.iter().any(|s| m.contains(s)))
}

/// `explain MAC_SLOT_LIMIT` card text.
pub fn card(e: &MacCode) -> String {
    let head = match e.status {
        Some(s) => format!("{} (HTTP {s}, mac)", e.code),
        None => format!("{} (mac)", e.code),
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

pub fn entry(e: &MacCode) -> serde_json::Value {
    serde_json::json!({"code": e.code, "status": e.status, "summary": e.summary, "causes": e.causes, "fixes": e.fixes})
}

/// A short hint to print under a failure: the cause and the first fix. `None` when the message is not recognised.
pub fn friendly(message: &str) -> Option<String> {
    let c = hint_for(message)?;
    Some(format!(
        "hint ({}): {}\n  try: {}\n  more: fluxctl explain {}",
        c.code,
        c.summary,
        c.fixes.first().copied().unwrap_or(""),
        c.code
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_cause_and_fix_and_is_found_by_name_and_aliases() {
        for c in CODES {
            assert!(
                !c.causes.is_empty() && !c.fixes.is_empty() && !c.summary.is_empty(),
                "{}",
                c.code
            );
            assert_eq!(find(c.code)[0].code, c.code);
            for a in c.aliases.iter().filter(|a| a.parse::<u16>().is_err()) {
                assert_eq!(find(a)[0].code, c.code, "{a}");
            }
        }
    }

    #[test]
    fn statuses_and_spellings() {
        assert_eq!(find("409")[0].code, "MAC_SLOT_LIMIT");
        assert_eq!(find("503")[0].code, "MAC_SESSION_LOCKED");
        assert_eq!(find("mac-disk-full")[0].code, "MAC_DISK_FULL");
        assert!(find("zzz").is_empty() && find("").is_empty());
    }

    #[test]
    fn messages_map_to_codes() {
        let cases = [
            ("Host key verification failed.", "SSH_HOSTKEY_MISMATCH"),
            (
                "@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @",
                "SSH_HOSTKEY_MISMATCH",
            ),
            ("write: No space left on device", "MAC_DISK_FULL"),
            ("Error: no such image macos-x:1", "MAC_TEMPLATE_MISSING"),
            (
                "the Mac's screen is locked, so snapshot restores fail",
                "MAC_SESSION_LOCKED",
            ),
            ("both macOS slots: slot limit reached", "MAC_SLOT_LIMIT"),
        ];
        for (m, want) in cases {
            assert_eq!(hint_for(m).map(|c| c.code), Some(want), "{m}");
        }
        assert!(hint_for("connection refused").is_none());
        let f = friendly("No space left on device").unwrap();
        assert!(f.contains("fluxctl explain MAC_DISK_FULL") && f.contains("try:"));
    }

    #[test]
    fn card_has_causes_and_fixes() {
        let s = card(&CODES[0]);
        assert!(s.starts_with("MAC_SLOT_LIMIT (HTTP 409, mac)") && s.contains("what to do:"));
    }
}
