// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

// Exercise the real parser and process replacement without a Docker daemon.
#[test]
fn docker_plugin_receives_original_arguments_endpoint_and_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$DOCKER_HOST" "${DOCKER_CONTEXT-unset}" "${DOCKER_TLS_VERIFY-unset}" "$@"
exit 37
"#,
    )
    .unwrap();
    fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();
    let args = [
        "compose",
        "-f",
        "my project.yaml",
        "exec",
        "-T",
        "app",
        "--",
        "-o",
    ];
    let run = |prefix: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fluxctl"))
            .env("PATH", dir.path())
            .env("DOCKER_CONTEXT", "other")
            .env("DOCKER_HOST", "tcp://other:2376")
            .env("DOCKER_TLS_VERIFY", "1")
            .arg("docker")
            .args(prefix)
            .args(args)
            .output()
            .unwrap()
    };
    for (prefix, endpoint) in [
        (
            vec![],
            format!("unix:///tmp/fluxvm-run-{}/docker-engine.sock", unsafe {
                libc::getuid()
            }),
        ),
        (
            vec!["cli", "--socket", "/tmp/my engine.sock", "--"],
            String::from("unix:///tmp/my engine.sock"),
        ),
    ] {
        let result = run(&prefix);
        assert_eq!(
            result.status.code(),
            Some(37),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8(result.stdout).unwrap();
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            [&[endpoint.as_str(), "unset", "unset"][..], &args].concat()
        );
    }
}
