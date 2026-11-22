// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Runs the real launcher against a tiny rootfs (a static busybox). It needs a Linux host that lets this user create
//! namespaces (root, or unprivileged user namespaces not blocked by AppArmor) and a static `busybox` on PATH; otherwise
//! the test says why and passes, so it never fails a host that cannot run containers.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

fn static_busybox() -> Option<PathBuf> {
    let p = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("busybox"))
        .find(|p| p.is_file())?;
    // A dynamic busybox would need its libraries inside the rootfs.
    let out = Command::new("file").arg("-L").arg(&p).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .contains("statically linked")
        .then_some(p)
}

#[test]
fn the_launcher_runs_a_container_with_the_documented_restrictions() {
    let Some(busybox) = static_busybox() else {
        eprintln!("skipped: no static busybox on PATH");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("rootfs");
    let state = dir.path().join("state");
    for d in ["bin", "proc", "dev", "sys", "tmp", "run", "etc"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::create_dir_all(&state).unwrap();
    std::fs::copy(&busybox, root.join("bin/busybox")).unwrap();
    for a in ["sh", "cat", "grep", "id", "hostname"] {
        std::os::unix::fs::symlink("busybox", root.join("bin").join(a)).unwrap();
    }
    let script = "echo pid=$$; echo host=$(hostname); (echo x > /rootfile) 2>/dev/null || echo readonly; \
                  grep -E 'NoNewPrivs|Seccomp:' /proc/self/status; exit 7";
    let spec = serde_json::json!({
        "name": "fx-test-live", "rootfs": root, "argv": ["/bin/sh", "-c", script], "env": ["PATH=/bin"],
        "state_dir": state, "log_path": state.join("console.log"),
    });
    let spec_path = dir.path().join("spec.json");
    std::fs::write(&spec_path, spec.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fluxvm-container-run"))
        .args(["run", "--spec"])
        .arg(&spec_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(125) {
        eprintln!("skipped: this host does not let the launcher create namespaces: {stderr}");
        return;
    }
    assert_eq!(
        out.status.code(),
        Some(7),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("pid=1"), "{stdout}");
    assert!(stdout.contains("host=fx-test-live"), "{stdout}");
    assert!(stdout.contains("readonly"), "{stdout}");
    assert!(
        stdout.contains("NoNewPrivs:\t1") && stdout.contains("Seccomp:\t2"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(state.join("exit")).unwrap(), "7");
}
