// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The operations layer of `fluxctl mac`: enforced healthchecks, `supervise` (restart policies), TTLs and the reaper, `run --verify`.
//! This file holds the clap surface and the dispatch; the logic is in `mac_health`, `mac_supervise`, `mac_reap`.

use anyhow::{Result, ensure};
use clap::Subcommand;

/// Extra `mac run` flags, flattened into `Run`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct RunExtra {
    /// After starting, block until the image's HEALTHCHECK passes (probed over SSH from here). Fails when the image has no
    /// HEALTHCHECK, when it goes unhealthy, or after --health-timeout.
    #[arg(long)]
    pub wait_healthy: bool,
    /// Seconds --wait-healthy waits (0 = forever).
    #[arg(long, default_value_t = 600, value_name = "SECS")]
    pub health_timeout: u64,
    /// Delete the instance after this long (`90s`, `30m`, `2h`, `1d`) even if nobody removes it: `mac reap` or `mac supervise`
    /// does the deleting. Counted from when the run started.
    #[arg(long, value_name = "DURATION")]
    pub ttl: Option<String>,
    /// Before anything starts, check that the local image carries a signature from a key you trust (`mac keys trust`), using the
    /// signatures kept at pull or push time. Refuses an unsigned, untrusted or tampered image. Also: FLUXVM_MAC_REQUIRE_SIGNED=1.
    /// Does not apply with `--fleet` (the remote Mac runs its own store).
    #[arg(long)]
    pub verify: bool,
}

impl RunExtra {
    pub fn check(&self, rm: bool) -> Result<()> {
        if let Some(t) = &self.ttl {
            crate::mac_reap::parse_ttl(t)?;
        }
        ensure!(
            !(self.wait_healthy && rm),
            "--wait-healthy and --rm cannot be combined: the instance is deleted when its command ends"
        );
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
pub enum MacOps {
    /// Run the image's HEALTHCHECK in an instance once, record the result (shown by `mac inspect`) and print it. Exit 1 unless healthy.
    Health {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Enforce `--restart` policies (and healthchecks, and TTLs): a loop that starts instances that should be running. The daemon never
    /// restarts a macOS VM by itself. `--install` runs it as a LaunchAgent. See docs/macos-containers.md.
    Supervise(crate::mac_supervise::SuperviseArgs),
    /// Delete the instances whose `--ttl` passed. Only yours unless `--all` (or `--owner NAME`). `mac supervise` does this on every pass.
    Reap {
        /// Say what would be removed and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Reap expired instances of every owner (and of none).
        #[arg(long, conflicts_with = "owner")]
        all: bool,
        /// Only this owner's.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Validate `FROM IPSW latest|PATH` and print the unattended install plan, marking what is implemented and what needs a Mac to
    /// verify. `mac build` does not run an IPSW install yet.
    Ipsw {
        /// `latest`, or an absolute path to a .ipsw file on this Mac.
        #[arg(default_value = "latest")]
        source: String,
        /// Size of the guest disk.
        #[arg(long, default_value_t = crate::mac_ipsw::DEFAULT_DISK_GIB)]
        disk_gib: u64,
        /// The admin account to provision.
        #[arg(long, default_value = "admin")]
        user: String,
    },
    /// Change an instance's TTL (`2h`, counted from now) or clear it (`none`).
    Ttl { name: String, ttl: String },
}

impl MacOps {
    pub fn is_client_side(&self) -> bool {
        match self {
            // The LaunchAgent flags never reach the daemon.
            MacOps::Supervise(a) => a.install || a.uninstall || a.status,
            MacOps::Ipsw { .. } => true,
            _ => false,
        }
    }
}

/// The parts that need no daemon.
pub fn run_client(cmd: MacOps) -> Result<()> {
    match cmd {
        MacOps::Supervise(a) => {
            if let Some(v) = crate::mac_supervise::agent_command(&a)? {
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
            Ok(())
        }
        MacOps::Ipsw {
            source,
            disk_gib,
            user,
        } => crate::mac_ipsw::run(&source, disk_gib, &user),
        _ => anyhow::bail!("not a client-side command"),
    }
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacOps) -> Result<()> {
    match cmd {
        MacOps::Health { name, json } => crate::mac_health::health_once(r, &name, json).await,
        MacOps::Supervise(a) => crate::mac_supervise::run(r, a).await,
        MacOps::Reap {
            dry_run,
            all,
            owner,
        } => crate::mac_reap::reap(r, dry_run, all, owner).await,
        MacOps::Ttl { name, ttl } => crate::mac_reap::set_ttl(r, &name, &ttl).await,
        MacOps::Ipsw { .. } => anyhow::bail!("internal: mac ipsw is client side"),
    }
}

/// Pre-flight for `mac run`: signature verification with the existing trust store. Runs before any VM is created.
pub fn preflight(image: &str, x: &RunExtra) -> Result<()> {
    let required = std::env::var("FLUXVM_MAC_REQUIRE_SIGNED").is_ok_and(|v| v == "1");
    let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
    if !(x.verify || required) {
        if store.resolve(image).is_ok() {
            crate::mac_imgstat::touch(&store, image, crate::mac_health::now_unix());
        }
        return Ok(());
    }
    let trust = crate::mac_trust::TrustStore::open(crate::mac_trust::trust_dir(store.root()))?;
    let (digest, signer) = crate::mac_plus::verify_local(&store, &trust, image)?;
    eprintln!("verified {image} ({digest}), signed by {signer}");
    crate::mac_imgstat::touch(&store, image, crate::mac_health::now_unix());
    Ok(())
}

/// What `mac run` does after the instance is up.
pub async fn after_run(
    r: &crate::remote::Remote,
    name: &str,
    started: u64,
    x: &RunExtra,
) -> Result<()> {
    // Ownership always; the expiry only with --ttl. The instance may already be gone (`--rm`): then there is nothing to stamp.
    if let Ok(vm) = crate::mac_cmd::find_instance(r, name).await {
        let ttl = x
            .ttl
            .as_deref()
            .map(crate::mac_reap::parse_ttl)
            .transpose()?;
        let me = crate::mac_reap::current_user();
        let patch = crate::mac_reap::stamp(Some(&me), ttl, started);
        r.call(
            reqwest::Method::PATCH,
            &format!("/v1/vms/{}", vm["id"].as_str().unwrap_or_default()),
            Some(patch),
        )
        .await?;
    }
    if x.wait_healthy {
        let waited = crate::mac_health::wait_instance_healthy(r, name, x.health_timeout).await?;
        eprintln!("{name} is healthy (after {}s)", waited.as_secs());
    }
    Ok(())
}
