// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl vm guest …`: guest operations over the QEMU guest agent (REST: `/v1/vms/{id}/guest/…`).
//! Passwords are read from stdin or a file, never from an argument, and are never printed.

use crate::remote::Remote;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Subcommand)]
pub enum GuestCommand {
    /// OS, host name, clock, users, filesystems and vCPUs as the guest agent reports them.
    Info {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
    },
    /// Manage a guest user's SSH authorized keys.
    SshKey {
        #[command(subcommand)]
        command: SshKeyCommand,
    },
    /// Set a guest user's password. Reads it from stdin or a file; never from an argument.
    SetPassword {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
        /// Guest user name.
        #[arg(long, default_value = "root")]
        user: String,
        /// Read the password from stdin (one line; the trailing newline is dropped).
        #[arg(
            long,
            conflicts_with = "password_file",
            required_unless_present = "password_file"
        )]
        password_stdin: bool,
        /// Read the password from this file (the trailing newline is dropped).
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// The input is already a crypt(3) hash, not plaintext.
        #[arg(long)]
        crypted: bool,
    },
    /// Set the guest clock from this host (or, with `--rtc`, from the guest's hardware clock). Use after resume or restore.
    TimeSync {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
        /// Reload the guest clock from its RTC instead of the host clock.
        #[arg(long)]
        rtc: bool,
    },
}

#[derive(Args)]
pub struct KeySource {
    /// An authorized_keys line (repeatable).
    #[arg(long = "key")]
    keys: Vec<String>,
    /// A file of authorized_keys lines (blank lines and `#` comments are skipped).
    #[arg(long)]
    key_file: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum SshKeyCommand {
    /// Add keys to a guest user's authorized_keys.
    Add {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
        #[arg(long, default_value = "root")]
        user: String,
        #[command(flatten)]
        keys: KeySource,
        /// Replace the user's existing keys instead of appending.
        #[arg(long)]
        reset: bool,
    },
    /// Remove keys from a guest user's authorized_keys.
    Rm {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
        #[arg(long, default_value = "root")]
        user: String,
        #[command(flatten)]
        keys: KeySource,
    },
    /// List a guest user's authorized keys.
    Ls {
        #[arg(value_parser = crate::output::parse_vm_ref)]
        id: Uuid,
        #[arg(long, default_value = "root")]
        user: String,
    },
}

/// Keys from `--key` and `--key-file`; at least one is required.
fn collect_keys(src: &KeySource) -> Result<Vec<String>> {
    let mut keys = src.keys.clone();
    if let Some(p) = &src.key_file {
        let text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
        keys.extend(parse_key_lines(&text));
    }
    if keys.is_empty() {
        bail!("pass --key <line> or --key-file <path>");
    }
    Ok(keys)
}

pub(crate) fn parse_key_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// A password from a reader: the first line, without its line terminator. Empty is an error.
pub(crate) fn read_password(mut r: impl Read) -> Result<String> {
    let mut buf = String::new();
    r.read_to_string(&mut buf).context("read password")?;
    let line = buf.split('\n').next().unwrap_or("");
    let pw = line.strip_suffix('\r').unwrap_or(line);
    if pw.is_empty() {
        bail!("password is empty");
    }
    Ok(pw.to_string())
}

fn pretty(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub async fn run(r: &Remote, cmd: GuestCommand) -> Result<()> {
    match cmd {
        GuestCommand::Info { id } => pretty(
            &r.call(Method::GET, &format!("/v1/vms/{id}/guest/info"), None)
                .await?,
        ),
        GuestCommand::SshKey { command } => match command {
            SshKeyCommand::Add {
                id,
                user,
                keys,
                reset,
            } => {
                let keys = collect_keys(&keys)?;
                r.call(
                    Method::POST,
                    &format!("/v1/vms/{id}/guest/ssh-keys"),
                    Some(json!({"username": user, "keys": keys, "reset": reset})),
                )
                .await?;
                println!("{{\"ok\":true}}");
                Ok(())
            }
            SshKeyCommand::Rm { id, user, keys } => {
                let keys = collect_keys(&keys)?;
                r.call(
                    Method::POST,
                    &format!("/v1/vms/{id}/guest/ssh-keys/remove"),
                    Some(json!({"username": user, "keys": keys})),
                )
                .await?;
                println!("{{\"ok\":true}}");
                Ok(())
            }
            SshKeyCommand::Ls { id, user } => {
                let v = r
                    .call(
                        Method::GET,
                        &format!(
                            "/v1/vms/{id}/guest/ssh-keys?username={}",
                            crate::remote::encode_query(&user)
                        ),
                        None,
                    )
                    .await?;
                pretty(&v)
            }
        },
        GuestCommand::SetPassword {
            id,
            user,
            password_file,
            crypted,
            ..
        } => {
            let password = match password_file {
                Some(p) => read_password(
                    std::fs::File::open(&p).with_context(|| format!("open {}", p.display()))?,
                )?,
                None => read_password(std::io::stdin())?,
            };
            // The body is built here and handed to the client; it is never printed or put in an error.
            r.call(
                Method::POST,
                &format!("/v1/vms/{id}/guest/password"),
                Some(json!({"username": user, "password": password, "crypted": crypted})),
            )
            .await?;
            println!("{{\"ok\":true}}");
            Ok(())
        }
        GuestCommand::TimeSync { id, rtc } => pretty(
            &r.call(
                Method::POST,
                &format!("/v1/vms/{id}/guest/time-sync"),
                Some(json!({"mode": if rtc { "rtc" } else { "host" }})),
            )
            .await?,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        c: GuestCommand,
    }
    fn parse(args: &[&str]) -> Result<GuestCommand, clap::Error> {
        let mut v = vec!["t"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v).map(|c| c.c)
    }
    const ID: &str = "00000000-0000-0000-0000-000000000000";

    #[test]
    fn password_reads_first_line_and_rejects_empty() {
        assert_eq!(read_password(&b"hunter2\n"[..]).unwrap(), "hunter2");
        assert_eq!(
            read_password(&b"hunter2\r\nignored"[..]).unwrap(),
            "hunter2"
        );
        assert_eq!(read_password(&b" sp ace "[..]).unwrap(), " sp ace ");
        assert!(read_password(&b"\n"[..]).is_err());
        assert!(read_password(&b""[..]).is_err());
    }

    #[test]
    fn key_lines_skip_blanks_and_comments() {
        let t = "# c\n\nssh-ed25519 AAAA a@b\n  ssh-rsa BBBB  \n";
        assert_eq!(
            parse_key_lines(t),
            vec!["ssh-ed25519 AAAA a@b", "ssh-rsa BBBB"]
        );
    }

    #[test]
    fn collect_keys_needs_a_source() {
        let none = KeySource {
            keys: vec![],
            key_file: None,
        };
        assert!(collect_keys(&none).is_err());
        let one = KeySource {
            keys: vec!["ssh-ed25519 AAAA".into()],
            key_file: None,
        };
        assert_eq!(collect_keys(&one).unwrap().len(), 1);
    }

    #[test]
    fn clap_wiring() {
        assert!(matches!(
            parse(&["info", ID]),
            Ok(GuestCommand::Info { .. })
        ));
        assert!(matches!(
            parse(&["ssh-key", "add", ID, "--key", "ssh-ed25519 AAAA x"]),
            Ok(GuestCommand::SshKey {
                command: SshKeyCommand::Add { .. }
            })
        ));
        assert!(matches!(
            parse(&["ssh-key", "rm", ID, "--user", "bob", "--key-file", "k"]),
            Ok(GuestCommand::SshKey {
                command: SshKeyCommand::Rm { .. }
            })
        ));
        assert!(matches!(
            parse(&["ssh-key", "ls", ID]),
            Ok(GuestCommand::SshKey {
                command: SshKeyCommand::Ls { .. }
            })
        ));
        assert!(matches!(
            parse(&["time-sync", ID, "--rtc"]),
            Ok(GuestCommand::TimeSync { rtc: true, .. })
        ));
    }

    #[test]
    fn set_password_has_no_inline_password_flag() {
        // A secret on argv shows in `ps` and shell history: there is no such flag.
        assert!(parse(&["set-password", ID, "--password", "x"]).is_err());
        // One source is required, and only one.
        assert!(parse(&["set-password", ID]).is_err());
        assert!(
            parse(&[
                "set-password",
                ID,
                "--password-stdin",
                "--password-file",
                "f"
            ])
            .is_err()
        );
        assert!(parse(&["set-password", ID, "--password-stdin"]).is_ok());
        assert!(parse(&["set-password", ID, "--password-file", "f", "--crypted"]).is_ok());
    }
}
