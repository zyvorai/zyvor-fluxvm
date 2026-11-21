// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Merging any k3s kubeconfig (for example `/etc/rancher/k3s/k3s.yaml`, whose cluster, user and context are all called
//! `default`) into `~/.kube/config` under a chosen name, optionally pointing it at a different API server address.
//! Pure text-in, text-out; [`run`] only reads and writes the files. This is separate from the merge `kube up` does
//! for the clusters it creates.

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};
use std::path::PathBuf;

/// `fluxvm-<vm>` or `<prefix>-<vm>`; lowercase letters, digits, `-`, `.` and `_` only.
pub fn context_name(prefix: Option<&str>, vm: &str) -> Result<String> {
    let name = format!("{}-{vm}", prefix.unwrap_or("fluxvm"));
    check_name(&name)?;
    Ok(name)
}

pub fn check_name(n: &str) -> Result<()> {
    if n.is_empty()
        || n.len() > 63
        || !n
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._".contains(c))
        || !n.starts_with(|c: char| c.is_ascii_alphanumeric())
    {
        bail!("context name {n:?} must be 1-63 characters: lowercase letters, digits, - . _");
    }
    Ok(())
}

fn seq<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key)
        .and_then(Value::as_sequence)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

fn named(name: &str, key: &str, body: Value) -> Value {
    let mut m = Mapping::new();
    m.insert("name".into(), name.into());
    m.insert(key.into(), body);
    Value::Mapping(m)
}

fn entry_name(e: &Value) -> Option<&str> {
    e.get("name").and_then(Value::as_str)
}

/// Replaces the entry called `name` in `list`, or appends it.
fn upsert(list: &mut Vec<Value>, entry: Value) {
    let n = entry_name(&entry).unwrap_or_default().to_string();
    match list.iter().position(|e| entry_name(e) == Some(&n)) {
        Some(i) => list[i] = entry,
        None => list.push(entry),
    }
}

fn list_of(root: &Value, key: &str) -> Vec<Value> {
    seq(root, key).into_iter().cloned().collect()
}

/// Takes the one cluster/user/context in `incoming`, renames all three to `name` (the context points at the renamed
/// cluster and user), optionally replaces the server URL, and merges them into `existing`. Entries of that name are
/// replaced; everything else is kept. `use_it` makes it the current context.
pub fn merge(
    existing: Option<&str>,
    incoming: &str,
    name: &str,
    server: Option<&str>,
    use_it: bool,
) -> Result<String> {
    check_name(name)?;
    let inc: Value =
        serde_yaml::from_str(incoming).context("the kubeconfig to merge is not YAML")?;
    let clusters = seq(&inc, "clusters");
    let users = seq(&inc, "users");
    if clusters.len() != 1 || users.len() != 1 {
        bail!(
            "expected a kubeconfig with exactly one cluster and one user, found {} and {}",
            clusters.len(),
            users.len()
        );
    }
    let mut cluster = clusters[0]
        .get("cluster")
        .cloned()
        .context("cluster entry has no `cluster` body")?;
    let user = users[0]
        .get("user")
        .cloned()
        .context("user entry has no `user` body")?;
    if let Some(s) = server {
        if !s.starts_with("https://") {
            bail!("server {s:?} must start with https://");
        }
        if let Value::Mapping(m) = &mut cluster {
            m.insert("server".into(), s.into());
        }
    }
    let mut ctx = Mapping::new();
    ctx.insert("cluster".into(), name.into());
    ctx.insert("user".into(), name.into());

    let mut root: Value = match existing {
        Some(t) if !t.trim().is_empty() => {
            serde_yaml::from_str(t).context("~/.kube/config is not YAML; not touching it")?
        }
        _ => Value::Mapping(Mapping::new()),
    };
    let (mut cl, mut us, mut cx) = (
        list_of(&root, "clusters"),
        list_of(&root, "users"),
        list_of(&root, "contexts"),
    );
    upsert(&mut cl, named(name, "cluster", cluster));
    upsert(&mut us, named(name, "user", user));
    upsert(&mut cx, named(name, "context", Value::Mapping(ctx)));
    let m = root
        .as_mapping_mut()
        .context("~/.kube/config must be a YAML mapping")?;
    m.entry("apiVersion".into()).or_insert("v1".into());
    m.entry("kind".into()).or_insert("Config".into());
    m.entry("preferences".into())
        .or_insert(Value::Mapping(Mapping::new()));
    m.insert("clusters".into(), Value::Sequence(cl));
    m.insert("users".into(), Value::Sequence(us));
    m.insert("contexts".into(), Value::Sequence(cx));
    if use_it || !m.contains_key("current-context") {
        m.insert("current-context".into(), name.into());
    }
    Ok(serde_yaml::to_string(&root)?)
}

/// The reverse: drops the cluster, user and context called `name`; clears `current-context` if it was that one.
pub fn remove(existing: &str, name: &str) -> Result<String> {
    let mut root: Value = serde_yaml::from_str(existing)?;
    let m = root
        .as_mapping_mut()
        .context("kubeconfig must be a mapping")?;
    for k in ["clusters", "users", "contexts"] {
        if let Some(Value::Sequence(s)) = m.get_mut(k) {
            s.retain(|e| entry_name(e) != Some(name));
        }
    }
    if m.get("current-context").and_then(Value::as_str) == Some(name) {
        m.insert("current-context".into(), "".into());
    }
    Ok(serde_yaml::to_string(&root)?)
}

#[derive(clap::Args, Debug)]
pub struct MergeArgs {
    /// The kubeconfig to take (default: ./k3s.yaml).
    #[arg(long, default_value = "k3s.yaml")]
    pub from: PathBuf,
    /// The context, cluster and user name to give it. Default: fluxvm-<--vm>.
    #[arg(long)]
    pub name: Option<String>,
    /// Build the default name from this VM or node name.
    #[arg(long, conflicts_with = "name")]
    pub vm: Option<String>,
    /// Prefix for the default name instead of `fluxvm`.
    #[arg(long, requires = "vm")]
    pub prefix: Option<String>,
    /// Replace the API server address (https://HOST:PORT), e.g. https://127.0.0.1:6443.
    #[arg(long)]
    pub server: Option<String>,
    /// Make it the current context.
    #[arg(long = "use")]
    pub use_it: bool,
    /// The file to merge into (default ~/.kube/config).
    #[arg(long)]
    pub into: Option<PathBuf>,
    /// Take the named context out again instead of merging.
    #[arg(long, conflicts_with_all = ["server", "use_it"])]
    pub remove: bool,
}

pub fn run(a: &MergeArgs) -> Result<String> {
    let name = match (&a.name, &a.vm) {
        (Some(n), _) => n.clone(),
        (None, Some(v)) => context_name(a.prefix.as_deref(), v)?,
        (None, None) => bail!("give --name NAME or --vm VM"),
    };
    let into = match &a.into {
        Some(p) => p.clone(),
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?)
            .join(".kube")
            .join("config"),
    };
    let existing = std::fs::read_to_string(&into).ok();
    let out = if a.remove {
        remove(
            existing
                .as_deref()
                .context("no kubeconfig to remove it from")?,
            &name,
        )?
    } else {
        let inc = std::fs::read_to_string(&a.from)
            .with_context(|| format!("reading {}", a.from.display()))?;
        merge(
            existing.as_deref(),
            &inc,
            &name,
            a.server.as_deref(),
            a.use_it,
        )?
    };
    if let Some(old) = &existing {
        let bak = into.with_extension("fluxvm-bak");
        if !bak.exists() {
            write_private(&bak, old)?;
        }
    }
    if let Some(d) = into.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_private(&into, &out)?;
    Ok(format!(
        "{} context {name:?} in {}",
        if a.remove { "removed" } else { "merged" },
        into.display()
    ))
}

fn write_private(p: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(p)?;
    // `mode` only applies to a new file; an existing 0644 config is tightened too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S: &str = "apiVersion: v1\nkind: Config\nclusters:\n- name: default\n  cluster:\n    server: https://127.0.0.1:6443\n    certificate-authority-data: Q0E=\nusers:\n- name: default\n  user:\n    client-certificate-data: Q0VSVA==\n    client-key-data: S0VZ\ncontexts:\n- name: default\n  context:\n    cluster: default\n    user: default\ncurrent-context: default\n";

    #[test]
    fn into_an_empty_config_golden() {
        let out = merge(None, K3S, "fluxvm-dev", None, false).unwrap();
        assert_eq!(
            out,
            "apiVersion: v1\nkind: Config\npreferences: {}\nclusters:\n- name: fluxvm-dev\n  cluster:\n    server: https://127.0.0.1:6443\n    certificate-authority-data: Q0E=\nusers:\n- name: fluxvm-dev\n  user:\n    client-certificate-data: Q0VSVA==\n    client-key-data: S0VZ\ncontexts:\n- name: fluxvm-dev\n  context:\n    cluster: fluxvm-dev\n    user: fluxvm-dev\ncurrent-context: fluxvm-dev\n"
        );
    }

    #[test]
    fn merging_keeps_other_clusters_and_current_context() {
        let existing = "apiVersion: v1\nkind: Config\nclusters:\n- name: prod\n  cluster:\n    server: https://prod\nusers:\n- name: prod\n  user: {}\ncontexts:\n- name: prod\n  context:\n    cluster: prod\n    user: prod\ncurrent-context: prod\n";
        let out = merge(
            Some(existing),
            K3S,
            "dev",
            Some("https://10.0.0.5:6443"),
            false,
        )
        .unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["current-context"], "prod");
        assert_eq!(v["clusters"].as_sequence().unwrap().len(), 2);
        assert_eq!(
            v["clusters"][1]["cluster"]["server"],
            "https://10.0.0.5:6443"
        );
        assert_eq!(v["contexts"][1]["context"]["user"], "dev");
        let used = merge(Some(existing), K3S, "dev", None, true).unwrap();
        assert_eq!(
            serde_yaml::from_str::<Value>(&used).unwrap()["current-context"],
            "dev"
        );
    }

    #[test]
    fn merging_twice_replaces_instead_of_duplicating() {
        let once = merge(None, K3S, "dev", None, false).unwrap();
        let twice = merge(Some(&once), K3S, "dev", Some("https://h:1"), false).unwrap();
        let v: Value = serde_yaml::from_str(&twice).unwrap();
        assert_eq!(v["clusters"].as_sequence().unwrap().len(), 1);
        assert_eq!(v["clusters"][0]["cluster"]["server"], "https://h:1");
    }

    #[test]
    fn remove_drops_entries_and_current_context() {
        let once = merge(None, K3S, "dev", None, true).unwrap();
        let gone = remove(&once, "dev").unwrap();
        let v: Value = serde_yaml::from_str(&gone).unwrap();
        assert!(v["contexts"].as_sequence().unwrap().is_empty());
        assert_eq!(v["current-context"], "");
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(merge(None, "not: [valid", "dev", None, false).is_err());
        assert!(merge(None, "clusters: []\nusers: []\n", "dev", None, false).is_err());
        assert!(merge(None, K3S, "Bad Name", None, false).is_err());
        assert!(merge(None, K3S, "dev", Some("http://insecure"), false).is_err());
        assert!(merge(Some("- a\n- b\n"), K3S, "dev", None, false).is_err());
    }

    #[test]
    fn context_names() {
        assert_eq!(context_name(None, "dev").unwrap(), "fluxvm-dev");
        assert_eq!(context_name(Some("lab"), "n1").unwrap(), "lab-n1");
        assert!(context_name(None, "UPPER").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("-x").is_err());
    }

    #[test]
    fn run_writes_a_private_file_and_backs_up_once() {
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("k3s.yaml");
        std::fs::write(&from, K3S).unwrap();
        let into = d.path().join("config");
        std::fs::write(&into, "apiVersion: v1\nkind: Config\n").unwrap();
        let a = MergeArgs {
            from,
            name: Some("dev".into()),
            vm: None,
            prefix: None,
            server: None,
            use_it: true,
            into: Some(into.clone()),
            remove: false,
        };
        assert!(run(&a).unwrap().contains("merged"));
        assert!(
            std::fs::read_to_string(&into)
                .unwrap()
                .contains("name: dev")
        );
        let bak = into.with_extension("fluxvm-bak");
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            "apiVersion: v1\nkind: Config\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&into).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
