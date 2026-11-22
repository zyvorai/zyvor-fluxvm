// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Recipes that are text, not actions: a `hostPath` PersistentVolume and claim for a folder shared with the VM over
//! virtiofs, and guidance for running linux/amd64 images through Rosetta.

use anyhow::{Result, bail};
use serde_yaml::{Mapping, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvSpec {
    pub name: String,
    pub namespace: String,
    /// The path *inside the VM* (`fluxctl kube up --share HOST[:GUEST]` makes the two sides match by default).
    pub guest_path: String,
    pub size: String,
    pub access: String,
    pub create: bool,
}

pub fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn check_quantity(q: &str) -> bool {
    let digits = q.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && ["Ki", "Mi", "Gi", "Ti"].contains(&&q[digits..])
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Mapping::new();
    for (k, v) in pairs {
        m.insert(k.into(), v);
    }
    Value::Mapping(m)
}

/// A PV plus a PVC bound to it by name. Both use the class `fluxvm-virtiofs`, which no provisioner serves, so nothing
/// else can claim the volume. `size` is only a label for hostPath (nothing enforces it).
pub fn pv_yaml(s: &PvSpec) -> Result<String> {
    if !dns_label(&s.name) || !dns_label(&s.namespace) {
        bail!("name and namespace must be lowercase DNS labels (letters, digits, -)");
    }
    if !s.guest_path.starts_with('/') || s.guest_path.contains("..") || s.guest_path == "/" {
        bail!(
            "path {:?} must be an absolute folder inside the VM, not / and without ..",
            s.guest_path
        );
    }
    if !check_quantity(&s.size) {
        bail!("size {:?} must look like 10Gi", s.size);
    }
    if !["ReadWriteOnce", "ReadWriteMany", "ReadOnlyMany"].contains(&s.access.as_str()) {
        bail!(
            "access {:?} must be ReadWriteOnce, ReadWriteMany or ReadOnlyMany",
            s.access
        );
    }
    let class = "fluxvm-virtiofs";
    let access = || Value::Sequence(vec![s.access.clone().into()]);
    let cap = |k: &str| map(vec![(k, s.size.clone().into())]);
    let pv = map(vec![
        ("apiVersion", "v1".into()),
        ("kind", "PersistentVolume".into()),
        ("metadata", map(vec![("name", s.name.clone().into())])),
        (
            "spec",
            map(vec![
                ("capacity", cap("storage")),
                ("accessModes", access()),
                ("persistentVolumeReclaimPolicy", "Retain".into()),
                ("storageClassName", class.into()),
                (
                    "hostPath",
                    map(vec![
                        ("path", s.guest_path.clone().into()),
                        (
                            "type",
                            if s.create {
                                "DirectoryOrCreate"
                            } else {
                                "Directory"
                            }
                            .into(),
                        ),
                    ]),
                ),
            ]),
        ),
    ]);
    let pvc = map(vec![
        ("apiVersion", "v1".into()),
        ("kind", "PersistentVolumeClaim".into()),
        (
            "metadata",
            map(vec![
                ("name", s.name.clone().into()),
                ("namespace", s.namespace.clone().into()),
            ]),
        ),
        (
            "spec",
            map(vec![
                ("accessModes", access()),
                ("storageClassName", class.into()),
                ("volumeName", s.name.clone().into()),
                ("resources", map(vec![("requests", cap("storage"))])),
            ]),
        ),
    ]);
    Ok(format!(
        "# hostPath on a virtiofs share. Single-node only: the path must exist in the VM, so share it first:\n\
         #   fluxctl kube up --share {p}   (the guest path defaults to the host path)\n\
         # Fine for source trees, uploads and caches. Not for databases or etcd: virtiofs fsync and file-locking\n\
         # behaviour differs from a local disk, and the Mac user's file ownership shows through.\n\
         {}---\n{}",
        serde_yaml::to_string(&pv)?,
        serde_yaml::to_string(&pvc)?,
        p = s.guest_path
    ))
}

#[derive(clap::Args, Debug)]
pub struct PvArgs {
    /// Name for the volume and claim.
    pub name: String,
    /// Folder inside the VM (the shared path).
    #[arg(long)]
    pub path: String,
    #[arg(long, default_value = "default")]
    pub namespace: String,
    #[arg(long, default_value = "10Gi")]
    pub size: String,
    #[arg(long, default_value = "ReadWriteOnce")]
    pub access: String,
    /// Let the kubelet create the folder if it is missing (default: it must exist).
    #[arg(long)]
    pub create: bool,
}

impl PvArgs {
    pub fn to_spec(&self) -> Result<PvSpec> {
        Ok(PvSpec {
            name: self.name.clone(),
            namespace: self.namespace.clone(),
            guest_path: self.path.clone(),
            size: self.size.clone(),
            access: self.access.clone(),
            create: self.create,
        })
    }
}

/// How to run linux/amd64 images on an Apple-silicon k3s VM. Not applied by anything; read it, then do it.
pub fn rosetta_guidance() -> String {
    "\
Running linux/amd64 images on an Apple-silicon cluster

1. Give the VM Rosetta. At creation: `fluxctl kube up --rosetta`. It shares the Mac's Rosetta over virtiofs and
   registers it with binfmt_misc in the guest, so x86-64 binaries run. It cannot be added to a VM created without it.
2. Check it inside the VM:
     ls /proc/sys/fs/binfmt_misc/rosetta && mount | grep rosetta
3. Prefer a multi-arch or arm64 image: native is faster and uses less memory. Rosetta is for images that only
   publish amd64.
4. The node is still arm64, so containerd selects the arm64 manifest and fails with `no match for platform` for an
   amd64-only image. Pull the amd64 variant by hand, then tell Kubernetes not to pull:
     sudo k3s ctr images pull --platform linux/amd64 docker.io/library/IMAGE:TAG
   and in the pod spec use `imagePullPolicy: Never` (or `IfNotPresent` with the image already present).
5. Expect: slower start and CPU-bound work, no AVX/AVX2 (some images, such as recent databases built for x86-64-v3,
   crash with `illegal instruction`), and JIT-heavy runtimes behaving worse. Do not run a database that way.
6. If a pod crash-loops, `kubectl logs` and look for `Illegal instruction` or `exec format error` (the latter means
   Rosetta is not registered: repeat step 2).

Steps 4 and 5 describe the usual behaviour of containerd and Rosetta; they have not been checked against a live cluster here.
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::from_str;

    fn spec() -> PvSpec {
        PvSpec {
            name: "src".into(),
            namespace: "dev".into(),
            guest_path: "/Users/me/work".into(),
            size: "5Gi".into(),
            access: "ReadWriteOnce".into(),
            create: false,
        }
    }

    #[test]
    fn pv_golden() {
        let out = pv_yaml(&spec()).unwrap();
        let body = out.split_once("---\n").unwrap();
        assert!(body.0.contains("fluxctl kube up --share /Users/me/work"));
        assert!(body.0.ends_with(
            "apiVersion: v1\nkind: PersistentVolume\nmetadata:\n  name: src\nspec:\n  capacity:\n    storage: 5Gi\n  accessModes:\n  - ReadWriteOnce\n  persistentVolumeReclaimPolicy: Retain\n  storageClassName: fluxvm-virtiofs\n  hostPath:\n    path: /Users/me/work\n    type: Directory\n"
        ), "{}", body.0);
        assert_eq!(
            body.1,
            "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: src\n  namespace: dev\nspec:\n  accessModes:\n  - ReadWriteOnce\n  storageClassName: fluxvm-virtiofs\n  volumeName: src\n  resources:\n    requests:\n      storage: 5Gi\n"
        );
    }

    #[test]
    fn create_flag_changes_the_type() {
        let out = pv_yaml(&PvSpec {
            create: true,
            ..spec()
        })
        .unwrap();
        assert!(out.contains("type: DirectoryOrCreate"));
    }

    #[test]
    fn output_is_two_yaml_documents() {
        let out = pv_yaml(&spec()).unwrap();
        let docs: Vec<Value> = out.split("---\n").map(|d| from_str(d).unwrap()).collect();
        assert_eq!(docs[0]["kind"], "PersistentVolume");
        assert_eq!(docs[1]["spec"]["volumeName"], docs[0]["metadata"]["name"]);
    }

    #[test]
    fn invalid_specs() {
        let bad = |f: &dyn Fn(&mut PvSpec)| {
            let mut s = spec();
            f(&mut s);
            pv_yaml(&s).is_err()
        };
        assert!(bad(&|s| s.name = "Has Caps".into()));
        assert!(bad(&|s| s.namespace = "".into()));
        assert!(bad(&|s| s.guest_path = "relative".into()));
        assert!(bad(&|s| s.guest_path = "/".into()));
        assert!(bad(&|s| s.guest_path = "/a/../b".into()));
        assert!(bad(&|s| s.size = "10GB".into()));
        assert!(bad(&|s| s.size = "Gi".into()));
        assert!(bad(&|s| s.access = "Everyone".into()));
    }

    #[test]
    fn rosetta_guidance_mentions_the_load_bearing_steps() {
        let g = rosetta_guidance();
        for w in [
            "kube up --rosetta",
            "binfmt_misc/rosetta",
            "--platform linux/amd64",
            "imagePullPolicy: Never",
            "not been checked",
        ] {
            assert!(g.contains(w), "{w}");
        }
    }

    #[test]
    fn dns_labels() {
        assert!(dns_label("a-b1"));
        assert!(!dns_label("-a"));
        assert!(!dns_label("a_b"));
    }
}
