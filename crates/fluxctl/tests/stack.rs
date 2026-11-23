// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A debug `fluxctl` must start on the stack it is given. The clap tree and `main` are large in an unoptimized build; this
//! runs the debug test binary with the main-thread stack capped at 6 MiB (the old code needed over 7; macOS gives 8 and frames there are larger) and fails on a stack overflow (SIGSEGV or SIGABRT).

#![cfg(unix)]

use std::process::Command;

fn run(args: &[&str]) {
    let exe = env!("CARGO_BIN_EXE_fluxctl");
    // `cargo test` sets RUST_MIN_STACK (see .cargo/config.toml), which would hide an overflow in a spawned thread. Remove it.
    let out = Command::new("sh")
        .arg("-c")
        .arg("ulimit -s 6144; exec \"$0\" \"$@\"")
        .arg(exe)
        .args(args)
        .env_remove("RUST_MIN_STACK")
        .output()
        .expect("spawn fluxctl");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "fluxctl {args:?} failed ({:?}): {stderr}",
        out.status
    );
    assert!(!stderr.contains("overflowed its stack"), "{stderr}");
}

#[test]
fn help_does_not_overflow_the_stack() {
    run(&["--help"]);
}

#[test]
fn subcommand_help_does_not_overflow_the_stack() {
    run(&["kube", "--help"]);
}
