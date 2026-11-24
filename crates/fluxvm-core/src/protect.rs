// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Deletion protection for VMs (`fluxctl protect NAME`, like `limactl protect`): a VM carrying the label
//! `fluxvm.io/protected=true` cannot be deleted, by hand or in bulk, until `fluxctl unprotect NAME` removes it. The
//! label is an ordinary VM label, so it survives restarts and shows in `fluxctl ps --json`. Stopping, snapshotting and
//! editing a protected VM still work: only removal is refused.

use std::collections::BTreeMap;

/// The label that marks a VM as protected.
pub const LABEL: &str = "fluxvm.io/protected";

/// True when `labels` mark the VM as protected (`fluxvm.io/protected=true`, any letter case).
pub fn is_protected(labels: &BTreeMap<String, String>) -> bool {
    labels
        .get(LABEL)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// A delete was refused because the VM is protected. The API answers it as `409 Conflict`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmProtected {
    pub name: String,
}

impl std::fmt::Display for VmProtected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VM {} is protected from deletion ({LABEL}=true). Remove the protection first with `fluxctl unprotect {}`, then delete it.\n  more: fluxctl explain VM_PROTECTED",
            self.name, self.name
        )
    }
}

impl std::error::Error for VmProtected {}

/// `Err` when a VM with these labels must not be deleted.
pub fn guard_delete(name: &str, labels: &BTreeMap<String, String>) -> Result<(), VmProtected> {
    if is_protected(labels) {
        Err(VmProtected { name: name.into() })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[(&str, &str)]) -> BTreeMap<String, String> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_the_true_value_protects() {
        assert!(is_protected(&l(&[(LABEL, "true")])));
        assert!(is_protected(&l(&[(LABEL, "TRUE")])));
        assert!(!is_protected(&l(&[(LABEL, "false")])));
        assert!(!is_protected(&l(&[(LABEL, "")])));
        assert!(!is_protected(&l(&[("protected", "true")])));
        assert!(!is_protected(&BTreeMap::new()));
    }

    #[test]
    fn the_refusal_names_the_vm_and_the_fix() {
        let e = guard_delete("db", &l(&[(LABEL, "true")])).unwrap_err();
        let s = e.to_string();
        assert!(
            s.contains("VM db is protected") && s.contains("fluxctl unprotect db"),
            "{s}"
        );
        assert!(s.contains("fluxctl explain VM_PROTECTED"));
        assert!(guard_delete("db", &BTreeMap::new()).is_ok());
    }
}
