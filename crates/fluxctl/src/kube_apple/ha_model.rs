// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Journal types and validation for the high-availability work of `kube apple`: shared server flags, the registration
//! endpoint, the scheduled-snapshot policy and the S3 credentials (which are never journalled).

use super::model::san_ok;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub fn one_server() -> u8 {
    1
}

/// Control-plane nodes: odd, because etcd needs a majority.
pub const MAX_SERVERS: u8 = 7;

/// Flags that must be identical on every server of an embedded-etcd cluster (docs.k3s.io, HA embedded etcd). They are kept
/// in the journal and put in every server's unit, so servers agree by construction; `doctor` compares the units again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFlags {
    #[serde(default)]
    pub cluster_cidr: Option<String>,
    #[serde(default)]
    pub service_cidr: Option<String>,
    #[serde(default)]
    pub cluster_dns: Option<String>,
    #[serde(default)]
    pub cluster_domain: Option<String>,
    /// Bundled components to disable besides Traefik, which is always disabled.
    #[serde(default)]
    pub disable: Vec<String>,
    #[serde(default)]
    pub secrets_encryption: bool,
}

pub const DISABLE_OK: [&str; 5] = [
    "coredns",
    "servicelb",
    "traefik",
    "local-storage",
    "metrics-server",
];

impl SharedFlags {
    pub fn validate(&self) -> Result<()> {
        for (n, v) in [
            ("--cluster-cidr", &self.cluster_cidr),
            ("--service-cidr", &self.service_cidr),
        ] {
            if let Some(v) = v
                && !cidr_list_ok(v)
            {
                bail!("{n} {v:?} must be a CIDR (or two, comma separated)");
            }
        }
        if let Some(v) = &self.cluster_dns
            && v.parse::<std::net::IpAddr>().is_err()
        {
            bail!("--cluster-dns {v:?} must be an IP address");
        }
        if let Some(v) = &self.cluster_domain
            && !(san_ok(v) && !v.contains(':'))
        {
            bail!("--cluster-domain {v:?} must be a DNS name");
        }
        for d in &self.disable {
            if !DISABLE_OK.contains(&d.as_str()) {
                bail!("--disable {d:?}: one of {}", DISABLE_OK.join(", "));
            }
        }
        Ok(())
    }

    /// The server arguments in one fixed order. `--disable traefik` is always first and never repeated.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["--disable".to_owned(), "traefik".to_owned()];
        let mut dis: Vec<&String> = self.disable.iter().filter(|d| *d != "traefik").collect();
        dis.sort();
        dis.dedup();
        for d in dis {
            a.extend(["--disable".into(), d.clone()]);
        }
        for (flag, v) in [
            ("--cluster-cidr", &self.cluster_cidr),
            ("--service-cidr", &self.service_cidr),
            ("--cluster-dns", &self.cluster_dns),
            ("--cluster-domain", &self.cluster_domain),
        ] {
            if let Some(v) = v {
                a.extend([flag.to_owned(), v.clone()]);
            }
        }
        if self.secrets_encryption {
            a.push("--secrets-encryption".into());
        }
        a
    }
}

fn cidr_list_ok(v: &str) -> bool {
    !v.is_empty()
        && v.split(',').count() <= 2
        && v.split(',').all(|c| {
            c.split_once('/').is_some_and(|(ip, bits)| {
                ip.parse::<std::net::IpAddr>().is_ok() && bits.parse::<u8>().is_ok_and(|b| b <= 128)
            })
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointKind {
    /// A TCP forwarder on this Mac (`127.0.0.1:API_PORT`) that fails over between the servers' tunnels.
    Proxy,
    /// kube-vip as a static pod on every server: one address on the private network that follows the leader.
    KubeVip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCfg {
    pub kind: EndpointKind,
    /// The kube-vip address (on the private network); also a `--tls-san`.
    #[serde(default)]
    pub vip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Target {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPolicy {
    pub cron: String,
    pub retention: u32,
    pub compress: bool,
    #[serde(default)]
    pub s3: Option<S3Target>,
}

impl SnapshotPolicy {
    pub fn validate(&self) -> Result<()> {
        if !cron_ok(&self.cron) {
            bail!(
                "--cron {:?} must be five cron fields (digits and * / , - only)",
                self.cron
            );
        }
        if self.retention == 0 || self.retention > 1000 {
            bail!("--retention must be between 1 and 1000");
        }
        if let Some(s) = &self.s3 {
            if !san_ok(&s.endpoint) {
                bail!("--s3-endpoint must be HOST[:PORT] of plain characters, without a scheme");
            }
            if !bucket_ok(&s.bucket) {
                bail!("--s3-bucket {:?} is not a bucket name", s.bucket);
            }
            for (n, v) in [("--s3-region", &s.region), ("--s3-folder", &s.folder)] {
                if let Some(v) = v
                    && !path_word_ok(v)
                {
                    bail!("{n} {v:?} must be plain characters (letters, digits, - _ . /)");
                }
            }
        }
        Ok(())
    }

    /// The k3s configuration drop-in for this policy. With credentials it holds the S3 keys, so it is only ever sent on
    /// stdin to a root-only file; without them it is safe to show.
    pub fn dropin(&self, secret: Option<&S3Secret>) -> Result<String> {
        use serde_yaml::{Mapping, Value};
        let mut m = Mapping::new();
        let mut put = |k: &str, v: Value| {
            m.insert(Value::String(k.into()), v);
        };
        put(
            "etcd-snapshot-schedule-cron",
            Value::String(self.cron.clone()),
        );
        put(
            "etcd-snapshot-retention",
            Value::Number(self.retention.into()),
        );
        put("etcd-snapshot-compress", Value::Bool(self.compress));
        if let Some(s) = &self.s3 {
            put("etcd-s3", Value::Bool(true));
            put("etcd-s3-endpoint", Value::String(s.endpoint.clone()));
            put("etcd-s3-bucket", Value::String(s.bucket.clone()));
            if let Some(r) = &s.region {
                put("etcd-s3-region", Value::String(r.clone()));
            }
            if let Some(f) = &s.folder {
                put("etcd-s3-folder", Value::String(f.clone()));
            }
            match secret {
                Some(c) => {
                    put("etcd-s3-access-key", Value::String(c.access_key.clone()));
                    put("etcd-s3-secret-key", Value::String(c.secret_key.clone()));
                }
                None => bail!("an S3 target needs credentials"),
            }
        }
        Ok(serde_yaml::to_string(&Value::Mapping(m))?)
    }
}

/// Five cron fields of digits, `*`, `/`, `,` and `-`.
pub fn cron_ok(c: &str) -> bool {
    let f: Vec<&str> = c.split_whitespace().collect();
    f.len() == 5
        && f.iter().all(|x| {
            !x.is_empty()
                && x.bytes()
                    .all(|b| b.is_ascii_digit() || b"*/,-".contains(&b))
        })
}

pub fn bucket_ok(b: &str) -> bool {
    (3..=63).contains(&b.len())
        && b.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
}

pub fn path_word_ok(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && !v.contains("..")
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_./".contains(&c))
}

/// S3 credentials. They come from a file or the environment on the Mac and travel to the servers on stdin; this type hides
/// them from `Debug` so a stray `{:?}` cannot print them.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Secret {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for S3Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S3Secret(<redacted>)")
    }
}

fn s3_key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 256
        && k.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/+=_.-".contains(&c))
}

impl S3Secret {
    pub fn new(access_key: &str, secret_key: &str) -> Result<S3Secret> {
        if !s3_key_ok(access_key) || !s3_key_ok(secret_key) {
            bail!(
                "the S3 access key or secret key is empty, too long, or has characters outside letters, digits and /+=_.-"
            );
        }
        Ok(S3Secret {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        })
    }

    /// `KEY=VALUE` lines; accepts `access_key`/`secret_key` and the AWS spellings, in any case.
    pub fn parse(text: &str) -> Result<S3Secret> {
        let (mut a, mut s) = (None, None);
        for l in text.lines() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let Some((k, v)) = l.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"').trim_matches('\'');
            match k.trim().to_ascii_lowercase().as_str() {
                "access_key" | "aws_access_key_id" => a = Some(v.to_owned()),
                "secret_key" | "aws_secret_access_key" => s = Some(v.to_owned()),
                _ => {}
            }
        }
        match (a, s) {
            (Some(a), Some(s)) => S3Secret::new(&a, &s),
            _ => bail!(
                "the credentials need access_key= and secret_key= (or AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY=) lines"
            ),
        }
    }

    /// From a file only this user can read (as ssh does for keys), else from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.
    pub fn load(file: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Result<S3Secret> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(f) = file {
            let mode = std::fs::metadata(f)
                .with_context(|| format!("reading {}", f.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                bail!(
                    "{} is readable by others (mode {:o}); run chmod 600 on it",
                    f.display(),
                    mode & 0o777
                );
            }
            return S3Secret::parse(&std::fs::read_to_string(f)?);
        }
        match (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
            (Some(a), Some(s)) => S3Secret::new(&a, &s),
            _ => bail!(
                "S3 credentials: give --s3-credentials-file FILE (mode 600) or set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY; they are never taken from an argument"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_flags_are_validated_and_ordered() {
        let f = SharedFlags {
            cluster_cidr: Some("10.42.0.0/16".into()),
            service_cidr: Some("10.43.0.0/16".into()),
            cluster_dns: Some("10.43.0.10".into()),
            cluster_domain: Some("cluster.local".into()),
            disable: vec![
                "servicelb".into(),
                "traefik".into(),
                "coredns".into(),
                "coredns".into(),
            ],
            secrets_encryption: true,
        };
        f.validate().unwrap();
        assert_eq!(
            f.args().join(" "),
            "--disable traefik --disable coredns --disable servicelb --cluster-cidr 10.42.0.0/16 \
             --service-cidr 10.43.0.0/16 --cluster-dns 10.43.0.10 --cluster-domain cluster.local --secrets-encryption"
        );
        assert_eq!(SharedFlags::default().args().join(" "), "--disable traefik");
        for bad in [
            SharedFlags {
                cluster_cidr: Some("10.0.0.0".into()),
                ..Default::default()
            },
            SharedFlags {
                cluster_cidr: Some("10.0.0.0/8;id".into()),
                ..Default::default()
            },
            SharedFlags {
                cluster_dns: Some("dns.local".into()),
                ..Default::default()
            },
            SharedFlags {
                cluster_domain: Some("a b".into()),
                ..Default::default()
            },
            SharedFlags {
                disable: vec!["x;y".into()],
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cron_and_buckets_are_plain() {
        assert!(cron_ok("0 */6 * * *"));
        assert!(cron_ok("15 3 * * 1-5"));
        assert!(!cron_ok("0 */6 * *"));
        assert!(!cron_ok("0 $(id) * * *"));
        assert!(bucket_ok("my-bucket.1"));
        assert!(!bucket_ok("UP"));
        assert!(!path_word_ok("../x"));
    }

    fn policy(s3: bool) -> SnapshotPolicy {
        SnapshotPolicy {
            cron: "0 */6 * * *".into(),
            retention: 10,
            compress: true,
            s3: s3.then(|| S3Target {
                endpoint: "s3.example:9000".into(),
                bucket: "k3s-snaps".into(),
                region: Some("eu-west-1".into()),
                folder: Some("dev".into()),
            }),
        }
    }

    #[test]
    fn dropin_holds_the_schedule_and_only_holds_keys_when_given_s3() {
        let local = policy(false).dropin(None).unwrap();
        assert!(local.contains("etcd-snapshot-retention: 10"));
        assert!(local.contains("etcd-snapshot-compress: true"));
        assert!(!local.contains("etcd-s3"));
        assert!(policy(true).dropin(None).is_err());
        let sec = S3Secret::new("AKIAEXAMPLE", "s3cr3t/key+value").unwrap();
        let s3 = policy(true).dropin(Some(&sec)).unwrap();
        assert!(s3.contains("etcd-s3-bucket: k3s-snaps"));
        assert!(s3.contains("etcd-s3-secret-key: s3cr3t/key+value"));
        assert!(!format!("{sec:?}").contains("s3cr3t"));
        policy(true).validate().unwrap();
        let mut bad = policy(true);
        bad.s3.as_mut().unwrap().endpoint = "https://x".into();
        assert!(bad.validate().is_err());
        bad = policy(false);
        bad.retention = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn credentials_come_from_a_private_file_or_the_environment() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("creds");
        std::fs::write(&f, "# c\naccess_key=AKIA1\nsecret_key = \"abc/def\"\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let none = |_: &str| None;
        assert!(
            S3Secret::load(Some(&f), &none)
                .unwrap_err()
                .to_string()
                .contains("readable by others")
        );
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let s = S3Secret::load(Some(&f), &none).unwrap();
        assert_eq!(
            (s.access_key.as_str(), s.secret_key.as_str()),
            ("AKIA1", "abc/def")
        );
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("AKIA2".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("zzz".to_owned()),
            _ => None,
        };
        assert_eq!(S3Secret::load(None, &env).unwrap().access_key, "AKIA2");
        assert!(S3Secret::load(None, &none).is_err());
        std::fs::write(&f, "access_key=only\n").unwrap();
        assert!(S3Secret::load(Some(&f), &none).is_err());
        assert!(S3Secret::new("a b", "c").is_err());
    }
}
