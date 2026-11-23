// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet` and the registry login saved by `fluxctl mac login`.
//!
//! `templates distribute` has always forwarded `FLUXVM_REGISTRY_USER` / `FLUXVM_REGISTRY_PASSWORD`. A Mac operator who ran
//! `fluxctl mac login ghcr.io` has no such variables, so the Macs pulled anonymously and failed on a private template. This
//! module finds the saved login for the template's registry and sends it to each Mac as a one-registry login file:
//!
//! * only the template's registry goes (other saved logins stay home);
//! * it travels on the ssh stdin pipe, never in an argument, so it is not in `ps`, in the dry-run output or in any log;
//! * on the Mac it is written to a fresh file with mode 0600, used for the one `mac pull` through
//!   `FLUXVM_MAC_REGISTRY_AUTH`, and deleted when the script ends, also on an error or a signal.
//!
//! The local login file is read with the same refusal as `mac pull` (group or other readable: not used).

use crate::mac_fleet_remote::{PRELUDE, Secret, sh_quote};
use crate::mac_fleet_run::RegistryAuth;
use crate::mac_fleet_sched::TemplateRef;
use std::path::Path;

/// The saved login for the registry of template `name`, ready to send, or `None` when there is none (the pull is then
/// anonymous, as before). A login file that cannot be used prints a warning with the fix instead of failing the run.
pub fn from_saved_login(template_name: &str) -> Option<RegistryAuth> {
    let path = crate::mac_login::default_path().ok()?;
    match saved_in(&path, template_name) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("warning: not forwarding the saved registry login: {e:#}");
            None
        }
    }
}

pub(crate) fn saved_in(path: &Path, template_name: &str) -> anyhow::Result<Option<RegistryAuth>> {
    let Some(registry) = crate::mac_login::registry_of(template_name) else {
        return Ok(None);
    };
    let Some(login) = crate::mac_login::stored_in(path, &registry)? else {
        return Ok(None);
    };
    let doc = serde_json::json!({"registries": {registry: {
        "username": login.username, "password": login.password}}});
    Ok(Some(RegistryAuth {
        user: Secret::new(login.username.into_bytes()),
        password: Secret::new(login.password.into_bytes()),
        login_file: Some(Secret::new(serde_json::to_vec(&doc)?)),
    }))
}

/// The pull on a Mac that is handed a login file on stdin: file 0600 from creation, removed on every exit path.
pub fn pull_script_login_file(template: &TemplateRef) -> String {
    format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac' >&2; exit 127; }}\n\
umask 077\n\
AUTHF=$(mktemp \"${{TMPDIR:-/tmp}}/fluxvm-auth.XXXXXX\") || exit 1\n\
trap 'rm -f \"$AUTHF\"' EXIT HUP INT TERM\n\
chmod 600 \"$AUTHF\"\n\
cat > \"$AUTHF\"\n\
FLUXVM_MAC_REGISTRY_AUTH=\"$AUTHF\" \"$FLUXCTL\" mac pull {}\n",
        sh_quote(&template.name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_text() -> String {
        ["hunter", "-", "two"].concat()
    }

    #[cfg(unix)]
    fn write_login(dir: &Path, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("auth.json");
        let doc = serde_json::json!({"registries": {
            "ghcr.io": {"username": "robot", "password": secret_text()},
            "other.example": {"username": "x", "password": "other-secret"}}});
        std::fs::write(&p, doc.to_string()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn only_the_templates_registry_is_forwarded() {
        let d = tempfile::tempdir().unwrap();
        let p = write_login(d.path(), 0o600);
        let a = saved_in(&p, "ghcr.io/acme/xcode:15").unwrap().unwrap();
        let sent = String::from_utf8(a.login_file.unwrap().expose().to_vec()).unwrap();
        assert!(sent.contains(&secret_text()) && sent.contains("ghcr.io"));
        assert!(!sent.contains("other-secret") && !sent.contains("other.example"));
        assert!(saved_in(&p, "quay.io/acme/x:1").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_login_file_is_not_forwarded() {
        let d = tempfile::tempdir().unwrap();
        let p = write_login(d.path(), 0o644);
        let e = saved_in(&p, "ghcr.io/acme/xcode:15").unwrap_err();
        assert!(format!("{e:#}").contains("chmod 600"), "{e:#}");
    }

    #[test]
    fn the_script_keeps_the_login_out_of_argv_and_cleans_up() {
        let t = TemplateRef::parse("ghcr.io/acme/xcode:15").unwrap();
        let s = pull_script_login_file(&t);
        assert!(s.contains("chmod 600") && s.contains("umask 077"));
        assert!(s.contains("trap 'rm -f \"$AUTHF\"' EXIT"));
        assert!(s.contains("cat > \"$AUTHF\"") && s.contains("FLUXVM_MAC_REGISTRY_AUTH="));
        assert!(!s.contains(&secret_text()));
    }
}
