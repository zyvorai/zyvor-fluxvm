// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use fluxvm_intelligence::{
    exec_policy,
    guard::{self, DEFAULT_GUARD_PIN_ROOT, DEFAULT_GUARD_STATE_ROOT, GuardMode},
    rule_pack,
};
use std::{env, fs, path::PathBuf};
use uuid::Uuid;

fn usage() -> &'static str {
    "fluxvm-guard commands:\n\
     apply <uuid> <pid> [--mode audit|enforce] [--allow-exec] [--allow-wx]\n\
           [--no-device-allowlist] [--restrict-writes]\n\
           [--allow-device PATH]... [--allow-file PATH]...\n\
           [--exec-policy FILE]... [--rule-pack FILE]...\n\
           [--trusted-key B64]... [--unsigned]\n\
     pack verify FILE... [--trusted-key B64]... [--unsigned]\n\
     sign-rule-pack FILE PRIVATE_KEY_FILE\n\
     learn-exec [--dir PATH]... [--path PATH]... [--pid PID] [--depth N] [--out FILE]\n\
     sign-exec-policy FILE PRIVATE_KEY_FILE\n\
     status <uuid>\n\
     remove <uuid>\n\
     events <uuid> [seconds] [limit]\n\
     --rule-pack applies signed virtual-patch deny rules (expired packs are refused);\n\
     they travel with the policy, so to change packs re-run apply.\n\
     --exec-policy enables the positive exec allow-list (implies --allow-exec, since\n\
     the deny-all default would override it). A signed policy needs FILE.sig and a\n\
     trusted key; --unsigned skips verification (lab use only).\n\
     env: FLUXVM_GUARD_PIN_ROOT, FLUXVM_GUARD_STATE_ROOT,\n\
          FLUXVM_GUARD_TRUSTED_KEYS (comma-separated base64 public keys)"
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let pin_root = env::var("FLUXVM_GUARD_PIN_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| DEFAULT_GUARD_PIN_ROOT.into());
    let state_root = env::var("FLUXVM_GUARD_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| DEFAULT_GUARD_STATE_ROOT.into());
    match args.get(1).map(String::as_str) {
        Some("apply") => apply(&args, &pin_root, &state_root),
        Some("learn-exec") => learn_exec(&args),
        Some("pack") => pack_cmd(&args),
        Some("sign-rule-pack") => {
            let file = PathBuf::from(args.get(2).context(usage())?);
            let key = fs::read_to_string(args.get(3).context(usage())?)
                .context("reading private key file")?;
            println!("{}", rule_pack::sign_file(&file, &key)?.display());
            Ok(())
        }
        Some("sign-exec-policy") => {
            let file = PathBuf::from(args.get(2).context(usage())?);
            let key = fs::read_to_string(args.get(3).context(usage())?)
                .context("reading private key file")?;
            let sig = exec_policy::sign_file(&file, &key)?;
            println!("{}", sig.display());
            Ok(())
        }
        Some("status") => {
            let id: Uuid = args.get(2).context(usage())?.parse()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&guard::status(id, &pin_root, &state_root)?)?
            );
            Ok(())
        }
        Some("remove") => {
            let id: Uuid = args.get(2).context(usage())?.parse()?;
            guard::remove_policy(id, &pin_root, &state_root)?;
            println!("{{\"removed\":true,\"vm_id\":\"{id}\"}}");
            Ok(())
        }
        Some("events") => {
            let id: Uuid = args.get(2).context(usage())?.parse()?;
            let seconds = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(5);
            let limit = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(128);
            guard::stream_events(id, &pin_root, seconds, limit)
        }
        _ => bail!(usage()),
    }
}

fn apply(args: &[String], pin_root: &std::path::Path, state_root: &std::path::Path) -> Result<()> {
    let id: Uuid = args.get(2).context(usage())?.parse()?;
    let pid: u32 = args.get(3).context(usage())?.parse()?;
    let mut mode = GuardMode::Audit;
    let mut deny_exec = true;
    let mut deny_wx = true;
    let mut restrict_devices = true;
    let mut restrict_writes = false;
    let mut allow_devices = Vec::new();
    let mut allow_files = Vec::new();
    let mut exec_policy_file: Option<PathBuf> = None;
    let mut trusted_keys: Vec<String> = env::var("FLUXVM_GUARD_TRUSTED_KEYS")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut unsigned = false;
    let mut rule_pack_files: Vec<PathBuf> = Vec::new();
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                mode = GuardMode::parse(args.get(i).context("--mode requires audit|enforce")?)?;
            }
            "--allow-exec" => deny_exec = false,
            "--allow-wx" => deny_wx = false,
            "--no-device-allowlist" => restrict_devices = false,
            "--restrict-writes" => restrict_writes = true,
            "--allow-device" => {
                i += 1;
                allow_devices.push(PathBuf::from(
                    args.get(i).context("--allow-device requires PATH")?,
                ));
            }
            "--allow-file" => {
                i += 1;
                allow_files.push(PathBuf::from(
                    args.get(i).context("--allow-file requires PATH")?,
                ));
            }
            "--exec-policy" => {
                i += 1;
                exec_policy_file = Some(PathBuf::from(
                    args.get(i).context("--exec-policy requires FILE")?,
                ));
            }
            "--trusted-key" => {
                i += 1;
                trusted_keys.push(
                    args.get(i)
                        .context("--trusted-key requires BASE64")?
                        .clone(),
                );
            }
            "--unsigned" => unsigned = true,
            "--rule-pack" => {
                i += 1;
                rule_pack_files.push(PathBuf::from(
                    args.get(i).context("--rule-pack requires FILE")?,
                ));
            }
            other => bail!("unknown option {other:?}\n{}", usage()),
        }
        i += 1;
    }
    if unsigned && exec_policy_file.is_none() && rule_pack_files.is_empty() {
        bail!("--unsigned only applies with --exec-policy or --rule-pack");
    }
    let patch_set = if rule_pack_files.is_empty() {
        None
    } else {
        Some(load_patch_set(&rule_pack_files, &trusted_keys, unsigned)?)
    };
    let allow_execs = match &exec_policy_file {
        Some(file) => {
            let policy = if unsigned {
                eprintln!("warning: exec policy loaded WITHOUT signature verification");
                exec_policy::load_unsigned(file)?
            } else {
                let trusted = exec_policy::trusted_from_keys(&trusted_keys)?;
                let (policy, signer) = exec_policy::load_verified(file, &trusted)?;
                eprintln!("exec policy verified (signer {signer})");
                policy
            };
            // The deny-all default would override the allow-list.
            deny_exec = false;
            Some(policy.paths)
        }
        None => None,
    };
    let state = guard::apply_policy(
        id,
        pid,
        mode,
        deny_exec,
        deny_wx,
        restrict_devices,
        restrict_writes,
        &allow_devices,
        &allow_files,
        allow_execs.as_deref(),
        patch_set.as_ref(),
        pin_root,
        state_root,
    )?;
    println!("{}", serde_json::to_string_pretty(&state)?);
    Ok(())
}

fn learn_exec(args: &[String]) -> Result<()> {
    let mut dirs = Vec::new();
    let mut paths = Vec::new();
    let mut depth = 3usize;
    let mut out: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                dirs.push(PathBuf::from(args.get(i).context("--dir requires PATH")?));
            }
            "--path" => {
                i += 1;
                paths.push(PathBuf::from(args.get(i).context("--path requires PATH")?));
            }
            "--pid" => {
                i += 1;
                let pid: u32 = args.get(i).context("--pid requires PID")?.parse()?;
                paths.push(PathBuf::from(format!("/proc/{pid}/exe")));
            }
            "--depth" => {
                i += 1;
                depth = args.get(i).context("--depth requires N")?.parse()?;
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(args.get(i).context("--out requires FILE")?));
            }
            other => bail!("unknown option {other:?}\n{}", usage()),
        }
        i += 1;
    }
    if dirs.is_empty() && paths.is_empty() {
        bail!(
            "learn-exec needs at least one --dir, --path or --pid\n{}",
            usage()
        );
    }
    let policy = exec_policy::learn(&dirs, &paths, depth);
    if policy.paths.is_empty() {
        bail!("no executables found; nothing to write");
    }
    let text = policy.render()?;
    match out {
        Some(f) => {
            fs::write(&f, &text)?;
            eprintln!("wrote {} paths to {}", policy.paths.len(), f.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Verifies every pack and merges them; fails on any bad signature, expired
/// pack or duplicate id so a partial set is never applied.
fn load_patch_set(
    files: &[PathBuf],
    trusted_keys: &[String],
    unsigned: bool,
) -> Result<rule_pack::PatchSet> {
    let trusted = exec_policy::trusted_from_keys(trusted_keys)?;
    let mut loaded = Vec::new();
    for f in files {
        let (pack, signer) = if unsigned {
            eprintln!(
                "warning: rule pack {} loaded WITHOUT signature verification",
                f.display()
            );
            rule_pack::load_unsigned(f)?
        } else {
            rule_pack::load_verified(f, &trusted)?
        };
        eprintln!("rule pack {} ok (signer {signer})", pack.id);
        loaded.push((pack, signer));
    }
    rule_pack::merge(&loaded, now_unix())
}

fn pack_cmd(args: &[String]) -> Result<()> {
    if args.get(2).map(String::as_str) != Some("verify") {
        bail!(usage());
    }
    let mut files = Vec::new();
    let mut keys: Vec<String> = env::var("FLUXVM_GUARD_TRUSTED_KEYS")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut unsigned = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--trusted-key" => {
                i += 1;
                keys.push(
                    args.get(i)
                        .context("--trusted-key requires BASE64")?
                        .clone(),
                );
            }
            "--unsigned" => unsigned = true,
            f => files.push(PathBuf::from(f)),
        }
        i += 1;
    }
    if files.is_empty() {
        bail!(usage());
    }
    let set = load_patch_set(&files, &keys, unsigned)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "valid": true,
            "packs": set.packs,
            "file_rules": set.files.len(),
            "device_rules": set.devices.len(),
        }))?
    );
    Ok(())
}
