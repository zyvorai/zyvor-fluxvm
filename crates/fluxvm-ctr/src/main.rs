// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxvm-container-run run --spec FILE` and `fluxvm-container-run exec --spec FILE --pid PID -- CMD…`.
//! Started by systemd (`systemd-run`) or by the FluxVM daemon; not meant to be typed by hand.

#[cfg(target_os = "linux")]
fn main() {
    use fluxvm_ctr::spec::ContainerSpec;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fail = |m: String| -> ! {
        eprintln!("fluxvm-container-run: {m}");
        std::process::exit(125);
    };
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    // read and write need no spec (they only join the container's namespaces); run and exec do.
    let spec: ContainerSpec = match flag("--spec") {
        None if matches!(args.first().map(String::as_str), Some("read" | "write")) => ContainerSpec::default(),
        None => fail("usage: fluxvm-container-run run --spec FILE | exec --spec FILE --pid PID -- CMD... | read|write --pid PID --path P".into()),
        Some(spec_path) => match std::fs::read(&spec_path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        {
            Ok(s) => s,
            Err(e) => fail(format!("reading {spec_path}: {e}")),
        },
    };
    let code = match args.first().map(String::as_str) {
        Some("run") => fluxvm_ctr::launch::run(&spec),
        Some("exec") => {
            let pid: i32 = flag("--pid").and_then(|p| p.parse().ok()).unwrap_or_else(|| fail("exec needs --pid".into()));
            let cmd: Vec<String> = args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();
            fluxvm_ctr::launch::exec_in(&spec, pid, &cmd)
        }
        Some("read") => {
            use std::io::Write;
            let pid: i32 = flag("--pid").and_then(|p| p.parse().ok()).unwrap_or_else(|| fail("read needs --pid".into()));
            let path = flag("--path").unwrap_or_else(|| fail("read needs --path".into()));
            let max: u64 = flag("--max").and_then(|p| p.parse().ok()).unwrap_or(64 << 20);
            fluxvm_ctr::launch::read_in(pid, &path, max).and_then(|(mode, data)| {
                let mut out = std::io::stdout().lock();
                writeln!(out, "MODE {mode:o}")?;
                out.write_all(&data)?;
                Ok(0)
            })
        }
        Some("write") => {
            use std::io::Read;
            let pid: i32 = flag("--pid").and_then(|p| p.parse().ok()).unwrap_or_else(|| fail("write needs --pid".into()));
            let path = flag("--path").unwrap_or_else(|| fail("write needs --path".into()));
            let mode = flag("--mode").and_then(|m| u32::from_str_radix(&m, 8).ok()).unwrap_or(0o644);
            let mut data = Vec::new();
            if let Err(e) = std::io::stdin().read_to_end(&mut data) {
                fail(format!("reading stdin: {e}"));
            }
            fluxvm_ctr::launch::write_in(pid, &path, mode, &data).map(|()| 0)
        }
        _ => fail("the first argument must be run, exec, read or write".into()),
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => fail(format!("{e:#}")),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("fluxvm-container-run is Linux only");
    std::process::exit(125);
}
