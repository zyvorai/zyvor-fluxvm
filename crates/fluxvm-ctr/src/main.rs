// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

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
    let Some(spec_path) = flag("--spec") else {
        fail("usage: fluxvm-container-run run --spec FILE | exec --spec FILE --pid PID -- CMD...".into())
    };
    let spec: ContainerSpec = match std::fs::read(&spec_path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => fail(format!("reading {spec_path}: {e}")),
    };
    let code = match args.first().map(String::as_str) {
        Some("run") => fluxvm_ctr::launch::run(&spec),
        Some("exec") => {
            let pid: i32 = flag("--pid").and_then(|p| p.parse().ok()).unwrap_or_else(|| fail("exec needs --pid".into()));
            let cmd: Vec<String> = args.iter().skip_while(|a| *a != "--").skip(1).cloned().collect();
            fluxvm_ctr::launch::exec_in(&spec, pid, &cmd)
        }
        _ => fail("the first argument must be run or exec".into()),
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
