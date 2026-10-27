// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Build host C helpers and (on Linux) the freestanding netboot guest payload.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn must(cmd: &mut Command, what: &str) {
    let st = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}"));
    if !st.success() {
        panic!("{what} failed: {st}");
    }
}

/// The C compiler and archiver for the host helper. A cross build (HOST differs from TARGET) must not use the build
/// machine's `gcc`, whose object the cross linker rejects as the wrong format; `CC_<target>` and `AR_<target>` win,
/// then the Debian-style cross prefix for the target architecture.
fn target_tools() -> (String, String) {
    let target = env::var("TARGET").unwrap_or_default();
    let host = env::var("HOST").unwrap_or_default();
    let key = target.replace('-', "_");
    let from_env = |name: &str| {
        env::var(format!("{name}_{key}"))
            .ok()
            .filter(|v| !v.is_empty())
    };
    if target == host || target.is_empty() {
        return (
            from_env("CC").unwrap_or_else(|| "gcc".into()),
            from_env("AR").unwrap_or_else(|| "ar".into()),
        );
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let prefix = format!("{arch}-linux-gnu-");
    (
        from_env("CC").unwrap_or_else(|| format!("{prefix}gcc")),
        from_env("AR").unwrap_or_else(|| format!("{prefix}ar")),
    )
}

fn main() {
    println!("cargo:rerun-if-changed=c/host_if.c");
    println!("cargo:rerun-if-changed=guest/netboot.c");
    println!("cargo:rerun-if-changed=guest/netboot.ld");

    let out = env::var("OUT_DIR").unwrap();
    let outp = Path::new(&out);
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Always emit a guest blob path so `include_bytes!` resolves on every host.
    let guest_bin = outp.join("netboot.bin");

    if target_os != "linux" {
        println!("cargo:rustc-check-cfg=cfg(fluxvm_no_host_c)");
        println!("cargo:rustc-cfg=fluxvm_no_host_c");
        fs::write(&guest_bin, b"\0").expect("write placeholder netboot.bin");
        return;
    }
    println!("cargo:rustc-check-cfg=cfg(fluxvm_no_host_c)");

    let (cc, ar) = target_tools();
    must(
        Command::new(&cc)
            .args(["-O2", "-fPIC", "-c", "c/host_if.c", "-o"])
            .arg(outp.join("host_if.o")),
        "compile host_if.c",
    );
    must(
        Command::new(&ar)
            .args(["crus"])
            .arg(outp.join("libhost_if.a"))
            .arg(outp.join("host_if.o")),
        "ar libhost_if",
    );
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=host_if");

    let guest_o = outp.join("netboot.o");
    let guest_elf = outp.join("netboot.elf");
    must(
        Command::new("gcc")
            .args([
                "-ffreestanding",
                "-nostdlib",
                "-fno-pic",
                "-fno-stack-protector",
                "-m64",
                "-mno-sse",
                "-mno-sse2",
                "-mno-avx",
                "-mno-mmx",
                "-msoft-float",
                "-O2",
                "-c",
                "guest/netboot.c",
                "-o",
            ])
            .arg(&guest_o),
        "compile guest",
    );
    must(
        Command::new("ld")
            .args(["-T", "guest/netboot.ld", "-o"])
            .arg(&guest_elf)
            .arg(&guest_o),
        "link guest",
    );
    must(
        Command::new("objcopy")
            .args(["-O", "binary"])
            .arg(&guest_elf)
            .arg(&guest_bin),
        "objcopy guest",
    );
}
