// Copyright 2026 Zyvor · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! CLI entry points for `fluxctl domains`.

use crate::domains::{list, set};

pub fn cmd_domains(args: &[String]) -> Result<(), String> {
    match args.first().map(|s| s.as_str()) {
        Some("list") | None => {
            let domains = list()?;
            if domains.is_empty() {
                println!("no domains");
            } else {
                for d in domains {
                    println!("{} http={:?} https={:?}", d.name, d.http, d.https);
                }
            }
            Ok(())
        }
        Some("set") => {
            let name = args.get(1).ok_or("usage: domains set NAME [--http N] [--https N]")?;
            set(name, None, None)
        }
        Some(other) => Err(format!("unknown domains subcommand: {other}")),
    }
}
