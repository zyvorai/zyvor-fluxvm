// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Opt-in sleep prevention that lasts only while a VM runs. `FLUXVM_KEEPAWAKE_WHILE_RUNNING=1` on the daemon makes it hold
//! `caffeinate -i -m -s -w <daemon pid>` while at least one `vz` VM is running and release it when the last one stops. The
//! `-w` makes the assertion end with the daemon even if it is killed. Off by default; nothing is changed on the host
//! (an assertion is not a setting), and `fluxctl mac host keepawake install` remains the always-on alternative.

use std::process::{Child, Command};
use std::sync::Mutex;

pub const ENV: &str = "FLUXVM_KEEPAWAKE_WHILE_RUNNING";

pub fn enabled_from(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

/// Whether the assertion should be held now.
pub fn want(enabled: bool, running_vz: usize) -> bool {
    enabled && running_vz > 0
}

#[derive(Default)]
pub struct KeepAwake {
    child: Option<Child>,
}

impl KeepAwake {
    pub fn held(&mut self) -> bool {
        match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => true,
            Some(_) => {
                // It exited by itself (killed from outside): forget it so the next sync starts a new one.
                self.child = None;
                false
            }
            None => false,
        }
    }

    /// Brings the assertion in line with `want`, starting a process with `spawn` when it must be held and is not.
    pub fn sync_with(&mut self, want: bool, spawn: impl FnOnce() -> Option<Child>) {
        let held = self.held();
        if want && !held {
            self.child = spawn();
        } else if !want && held {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

fn caffeinate() -> Option<Child> {
    Command::new("/usr/bin/caffeinate")
        .args(["-i", "-m", "-s", "-w", &std::process::id().to_string()])
        .spawn()
        .map_err(|e| tracing::warn!(error = %e, "could not start caffeinate for the running VMs"))
        .ok()
}

static GLOBAL: Mutex<Option<KeepAwake>> = Mutex::new(None);

/// Called from the daemon's periodic tick with the number of running `vz` VMs. A no-op unless opted in and on macOS.
pub fn sync_running(running_vz: usize) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let on = enabled_from(std::env::var(ENV).ok().as_deref());
    let mut g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() && !on {
        return;
    }
    g.get_or_insert_with(KeepAwake::default)
        .sync_with(want(on, running_vz), caffeinate);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeper() -> Option<Child> {
        Command::new("sleep").arg("30").spawn().ok()
    }

    #[test]
    fn opt_in_and_only_while_running() {
        assert!(!enabled_from(None) && !enabled_from(Some("0")));
        assert!(enabled_from(Some("1")) && enabled_from(Some("true")));
        assert!(!want(false, 3));
        assert!(!want(true, 0));
        assert!(want(true, 1));
    }

    #[test]
    fn held_from_first_vm_until_the_last_stops() {
        let mut k = KeepAwake::default();
        k.sync_with(false, || panic!("must not spawn with no VM"));
        assert!(!k.held());
        k.sync_with(true, sleeper);
        assert!(k.held());
        k.sync_with(true, || panic!("already held: must not spawn again"));
        k.sync_with(false, || panic!("no spawn on release"));
        assert!(!k.held());
    }
}
