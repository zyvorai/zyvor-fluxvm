// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Is the Mac's console login session usable for restoring a saved VM state?
//!
//! Restoring a `vz` snapshot (warm sandbox slots, hibernated sandboxes, the macOS pool) needs a logged-in, unlocked session.
//! On a locked Mac the restore fails, and the callers then cold-boot or error without saying why. This is the one place that
//! reads the session, so `fluxctl doctor`, the density report and the pool errors all agree.

use serde::{Deserialize, Serialize};

/// What the console login session looks like to `ioreg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginSession {
    Unlocked,
    Locked,
    /// Nobody is logged in at the console.
    NoSession,
    /// Not readable: `ioreg` gave nothing usable, or this is not a Mac.
    Unknown,
}

/// What to do about a session that blocks restores.
pub const UNLOCK_HINT: &str = "unlock the Mac, or give it a dedicated auto-login user with the screen lock off (docs/agent-density.md#unattended-hosts)";

impl LoginSession {
    /// Parse the `IOConsoleUsers` line of `ioreg -n Root -d1`. `IOConsoleLocked` is not used: it can say `No` while the
    /// user's session reports `CGSSessionScreenIsLocked=Yes`.
    pub fn parse_ioreg(ioreg: &str) -> Self {
        if ioreg.trim().is_empty() {
            return Self::Unknown;
        }
        let Some(line) = ioreg.lines().find(|l| l.contains("\"IOConsoleUsers\"")) else {
            return Self::NoSession;
        };
        if !line.contains('{') {
            return Self::NoSession;
        }
        if line.contains("\"CGSSessionScreenIsLocked\"=Yes") {
            Self::Locked
        } else {
            Self::Unlocked
        }
    }

    /// Reads the live session. Off macOS there is no such thing: `Unknown`.
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            match std::process::Command::new("ioreg")
                .args(["-n", "Root", "-d1"])
                .output()
            {
                Ok(o) if o.status.success() => {
                    Self::parse_ioreg(&String::from_utf8_lossy(&o.stdout))
                }
                _ => Self::Unknown,
            }
        }
        #[cfg(not(target_os = "macos"))]
        Self::Unknown
    }

    /// A snapshot restore will fail in this state. `Unknown` does not block: it is not evidence of a problem.
    pub fn blocks_restore(self) -> bool {
        matches!(self, Self::Locked | Self::NoSession)
    }

    /// Why restores fail, with the fix; `None` when nothing is known to be wrong.
    pub fn restore_hint(self) -> Option<String> {
        match self {
            Self::Locked => Some(format!(
                "the Mac's screen is locked, so snapshot restores fail: {UNLOCK_HINT}"
            )),
            Self::NoSession => Some(format!(
                "nobody is logged in at the Mac's console, so snapshot restores fail: {UNLOCK_HINT}"
            )),
            Self::Unlocked | Self::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_read_from_the_user_dict_not_ioconsolelocked() {
        let locked = "      \"IOConsoleLocked\" = No\n      \"IOConsoleUsers\" = ({\"kCGSSessionOnConsoleKey\"=Yes,\"CGSSessionScreenIsLocked\"=Yes,\"kCGSSessionUserNameKey\"=\"mini\"})\n";
        assert_eq!(LoginSession::parse_ioreg(locked), LoginSession::Locked);
        let open = locked.replace("\"CGSSessionScreenIsLocked\"=Yes,", "");
        assert_eq!(LoginSession::parse_ioreg(&open), LoginSession::Unlocked);
        assert_eq!(
            LoginSession::parse_ioreg("      \"IOConsoleUsers\" = ()\n"),
            LoginSession::NoSession
        );
        assert_eq!(
            LoginSession::parse_ioreg("      \"IOConsoleLocked\" = No\n"),
            LoginSession::NoSession
        );
        assert_eq!(LoginSession::parse_ioreg(""), LoginSession::Unknown);
    }

    #[test]
    fn only_a_locked_or_missing_session_blocks_a_restore_and_has_a_hint() {
        for s in [LoginSession::Locked, LoginSession::NoSession] {
            assert!(s.blocks_restore());
            assert!(s.restore_hint().unwrap().contains("auto-login"));
        }
        for s in [LoginSession::Unlocked, LoginSession::Unknown] {
            assert!(!s.blocks_restore());
            assert!(s.restore_hint().is_none());
        }
        assert_eq!(
            serde_json::to_string(&LoginSession::NoSession).unwrap(),
            "\"no_session\""
        );
    }
}
