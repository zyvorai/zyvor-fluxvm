// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Lifecycle tests against a fake host that records the order of everything done to it.

use super::host::*;
use super::lifecycle::*;
use super::model::*;
use super::script::{self, HostKey, NodeInfo};
use anyhow::{Result, bail};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;

const TOKEN: &str = "K10deadbeef0123::server:s3cr3tvalue";
const BIN: &[u8] = b"BINARY";

struct FVm {
    id: String,
    name: String,
    status: String,
    labels: Option<(String, Role, String, String)>,
}

/// What a real server's `backup` streams: a gzip tar with the token and the datastore, so the verify that follows every
/// backup passes.
fn fake_backup_archive(ds: Datastore) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut add = |name: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o600);
        h.set_cksum();
        b.append_data(&mut h, name, data).unwrap();
    };
    add("token", b"K10fake::server:secret");
    match ds {
        Datastore::Sqlite => {
            let mut db = b"SQLite format 3\0".to_vec();
            db.extend([0u8; 512]);
            add("db/state.db", &db);
        }
        Datastore::Etcd => add("db/snapshots/fluxvm-1", b"snapshot"),
    }
    b.into_inner().unwrap().finish().unwrap()
}

struct Fake {
    root: PathBuf,
    log: RefCell<Vec<String>>,
    vms: RefCell<Vec<FVm>>,
    nodes: RefCell<Vec<NodeInfo>>,
    fail: RefCell<Vec<(String, String)>>,
    fail_wait: RefCell<Vec<String>>,
    fail_create_vm: Cell<bool>,
    fail_power_delete: RefCell<Vec<String>>,
    version: RefCell<String>,
    new_version: RefCell<String>,
    stdin_seen: RefCell<Vec<(String, String, Vec<u8>)>>,
    boot: Cell<u32>,
    canary: RefCell<String>,
    lose_canary: Cell<bool>,
    seq: Cell<u32>,
    stuck_after_swap: RefCell<Vec<String>>,
    /// Every script run, as `(node, script)`.
    scripts: RefCell<Vec<(String, String)>>,
    /// The `ExecStart` line each node's unit was installed with.
    exec: RefCell<std::collections::BTreeMap<String, String>>,
    /// Nodes that got a kube-vip manifest.
    vip: RefCell<Vec<String>>,
    etcd_members: RefCell<Vec<String>>,
    snapshot_config: RefCell<String>,
    newest_age: Cell<Option<u64>>,
    /// What `k3s token rotate` was given: `OLD\nNEW\n`.
    rotated: RefCell<Option<String>>,
    dir_secret: RefCell<String>,
    leak_stdin: Cell<bool>,
    member_log: Cell<bool>,
}

impl Fake {
    fn new(root: PathBuf) -> Fake {
        Fake {
            root,
            log: RefCell::default(),
            vms: RefCell::default(),
            nodes: RefCell::default(),
            fail: RefCell::default(),
            fail_wait: RefCell::default(),
            fail_create_vm: Cell::new(false),
            fail_power_delete: RefCell::default(),
            version: RefCell::new("v1.31.4+k3s1".into()),
            new_version: RefCell::new("v1.31.5+k3s1".into()),
            stdin_seen: RefCell::default(),
            boot: Cell::new(1),
            canary: RefCell::default(),
            lose_canary: Cell::new(false),
            seq: Cell::new(0),
            stuck_after_swap: RefCell::default(),
            scripts: RefCell::default(),
            exec: RefCell::default(),
            vip: RefCell::default(),
            etcd_members: RefCell::default(),
            snapshot_config: RefCell::default(),
            newest_age: Cell::new(Some(3600)),
            rotated: RefCell::default(),
            dir_secret: RefCell::new(TOKEN.into()),
            leak_stdin: Cell::new(false),
            member_log: Cell::new(false),
        }
    }
    fn log(&self, s: impl Into<String>) {
        self.log.borrow_mut().push(s.into());
    }
    fn lines(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
    fn clear(&self) {
        self.log.borrow_mut().clear();
    }
    fn fail_on(&self, node_tag: &str, msg: &str) {
        self.fail.borrow_mut().push((node_tag.into(), msg.into()));
    }
    fn add_vm(&self, name: &str, status: &str, labels: Option<(&str, Role, &str, &str)>) -> String {
        self.seq.set(self.seq.get() + 1);
        let id = format!("00000000-0000-0000-0000-{:012}", self.seq.get());
        self.vms.borrow_mut().push(FVm {
            id: id.clone(),
            name: name.into(),
            status: status.into(),
            labels: labels.map(|(c, r, n, o)| (c.into(), r, n.into(), o.into())),
        });
        id
    }
}

/// Scripts that begin with `: fluxvm-op NAME;` are classified by that name.
const OPS: [&str; 15] = [
    "write-dropin",
    "remove-dropin",
    "restart",
    "snapshot-config",
    "snapshot-age",
    "unit-probe",
    "rotate-server-secret",
    "write-server-secret",
    "read-server-dir-secret",
    "cordon",
    "etcd-members",
    "etcd-member-log",
    "disable-unit",
    "kube-vip-probe",
    "kube-vip",
];

fn tag(script: &str) -> &'static str {
    let s = script;
    for t in OPS {
        if s.starts_with(&format!(": fluxvm-op {t};")) {
            return t;
        }
    }
    if s.contains("uname -m") {
        "preflight"
    } else if s.contains("sha256sum -c") && s.contains("k3s.new") {
        "stage-new"
    } else if s.contains("sha256sum -c") {
        "stage"
    } else if s.contains("cat > /etc/rancher/k3s/agent-token") {
        "agent-token"
    } else if s.contains("cat > /etc/rancher/k3s/token") {
        "token"
    } else if s.contains("enable --now") {
        "unit"
    } else if s.contains("get nodes -o json") {
        "nodes"
    } else if s.contains("/readyz") {
        "readyz"
    } else if s.contains("k3s.yaml") {
        "kubeconfig"
    } else if s.contains("--version") {
        "version"
    } else if s.contains("sha256sum /usr/local/bin/k3s") {
        "sha"
    } else if s.contains("mv -f /usr/local/bin/k3s.new") {
        "swap"
    } else if s.contains("mv -f /usr/local/bin/k3s.prev") {
        "rollback"
    } else if s.contains("rm -f /usr/local/bin/k3s.new") {
        "clean-staged"
    } else if s.contains("tar -tzf") {
        "verify-archive"
    } else if s.contains("-xzf") {
        "restore"
    } else if s.contains("test -f") && s.contains("-czf") {
        "snapshot-pull"
    } else if s.contains("-czf") {
        "backup"
    } else if s.contains("cluster-reset") {
        "snapshot-restore"
    } else if s.contains("etcd-snapshot delete") {
        "snapshot-delete"
    } else if s.contains("etcd-snapshot prune") {
        "snapshot-prune"
    } else if s.contains("etcd-snapshot save") {
        "snapshot-save"
    } else if s.contains("etcd-snapshot ls") {
        "snapshot-ls"
    } else if s.contains("is-active") {
        "active"
    } else if s.contains("systemctl stop") && s.contains("sync") {
        "svc-stop"
    } else if s.contains("cat /etc/rancher/k3s/agent-token") || s.contains("server/token") {
        "read-token"
    } else if s.contains("sudo -n cat /usr/local/bin/k3s") {
        "read-binary"
    } else if s.contains("boot_id") {
        "boot-id"
    } else if s.contains("create configmap") {
        "canary-mark"
    } else if s.contains("get configmap") {
        "canary-read"
    } else if s.contains("kubectl delete node") {
        "delete-node"
    } else if s.contains("kubectl drain") {
        "drain"
    } else if s.contains("kubectl uncordon") {
        "uncordon"
    } else if s.contains("grep -o '10") {
        "private-ip"
    } else {
        "other"
    }
}

impl Host for Fake {
    fn user(&self) -> String {
        "mini".into()
    }
    fn ssh_pub(&self) -> String {
        "ssh-ed25519 USERKEY".into()
    }
    fn new_host_key(&self) -> Result<HostKey> {
        self.seq.set(self.seq.get() + 1);
        Ok(HostKey {
            private: "PRIV".into(),
            public: format!("ssh-ed25519 HOSTKEY{}", self.seq.get()),
        })
    }
    fn new_id(&self) -> String {
        self.seq.set(self.seq.get() + 1);
        format!("{:032x}", self.seq.get())
    }
    fn stamp(&self) -> String {
        "20261121-101500".into()
    }
    async fn pause(&self, _secs: u64) {}

    async fn create_vm(&self, spec: Value) -> Result<String> {
        let name = spec["name"].as_str().unwrap().to_owned();
        let cluster = spec["apple"]["networks"][0]["name"]
            .as_str()
            .unwrap()
            .to_owned();
        let journal = self.root.join(format!("{cluster}.json")).exists();
        self.log(format!(
            "create_vm {name} journal={}",
            if journal { "yes" } else { "NO" }
        ));
        if self.fail_create_vm.get() {
            bail!("daemon refused");
        }
        Ok(self.add_vm(&name, "running", None))
    }
    async fn label_vm(
        &self,
        id: &str,
        cluster: &str,
        role: Role,
        node: &str,
        owner: &str,
    ) -> Result<()> {
        self.log(format!("label {node}"));
        let mut vms = self.vms.borrow_mut();
        let vm = vms.iter_mut().find(|v| v.id == id).unwrap();
        vm.labels = Some((cluster.into(), role, node.into(), owner.into()));
        Ok(())
    }
    async fn find_vm(&self, vm_name: &str) -> Result<Option<VmInfo>> {
        Ok(self
            .vms
            .borrow()
            .iter()
            .find(|v| v.name == vm_name)
            .map(|v| VmInfo {
                id: v.id.clone(),
                status: v.status.clone(),
            }))
    }
    async fn list_labelled(&self, cluster: &str) -> Result<Vec<LabelledVm>> {
        Ok(self
            .vms
            .borrow()
            .iter()
            .filter_map(|v| {
                let (c, r, n, o) = v.labels.as_ref()?;
                (c == cluster).then(|| LabelledVm {
                    id: v.id.clone(),
                    name: v.name.clone(),
                    role: *r,
                    node: n.clone(),
                    owner: o.clone(),
                    status: v.status.clone(),
                })
            })
            .collect())
    }
    async fn power(&self, id: &str, op: Power) -> Result<()> {
        let name = self
            .vms
            .borrow()
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.name.clone())
            .unwrap_or_default();
        self.log(format!("power {op:?} {name}"));
        if op == Power::Delete && self.fail_power_delete.borrow().contains(&name) {
            bail!("delete refused");
        }
        let mut vms = self.vms.borrow_mut();
        match op {
            Power::Start => {
                vms.iter_mut().find(|v| v.id == id).unwrap().status = "running".into();
                self.boot.set(self.boot.get() + 1);
            }
            Power::Stop => vms.iter_mut().find(|v| v.id == id).unwrap().status = "stopped".into(),
            Power::Delete => vms.retain(|v| v.id != id),
        }
        Ok(())
    }
    async fn wait_ssh(&self, _st: &State, node: &str) -> Result<()> {
        self.log(format!("wait_ssh {node}"));
        if self.fail_wait.borrow().iter().any(|n| n == node) {
            bail!("{node} did not answer ssh");
        }
        Ok(())
    }
    async fn ssh(
        &self,
        st: &State,
        node: &str,
        script: &str,
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let t = tag(script);
        self.log(format!(
            "ssh {node} {t}{}",
            stdin
                .map(|s| format!(" (stdin {} bytes)", s.len()))
                .unwrap_or_default()
        ));
        if let Some(s) = stdin {
            self.stdin_seen
                .borrow_mut()
                .push((node.into(), t.into(), s.to_vec()));
        }
        self.scripts.borrow_mut().push((node.into(), script.into()));
        let key = format!("{node}:{t}");
        if let Some((_, m)) = self.fail.borrow().iter().find(|(k, _)| *k == key) {
            if self.leak_stdin.get() {
                // A transport that echoes what it was sent, to prove callers scrub secrets from errors.
                bail!("{m} {}", String::from_utf8_lossy(stdin.unwrap_or_default()));
            }
            bail!("{m}");
        }
        let ver = self.version.borrow().clone();
        let new = self.new_version.borrow().clone();
        let out: Vec<u8> = match t {
            "active" => {
                if self.nodes.borrow().iter().any(|n| n.name == node) {
                    b"active".to_vec()
                } else {
                    b"inactive".to_vec()
                }
            }
            "readyz" => b"ok".to_vec(),
            "nodes" => {
                let items: Vec<Value> = self
                    .nodes
                    .borrow()
                    .iter()
                    .map(|n| {
                        serde_json::json!({"metadata":{"name":n.name},"status":{
                            "conditions":[{"type":"Ready","status": if n.ready {"True"} else {"False"}}],
                            "nodeInfo":{"kubeletVersion":n.version}}})
                    })
                    .collect();
                serde_json::json!({"items": items}).to_string().into_bytes()
            }
            "kubeconfig" => {
                b"clusters:\n- cluster:\n    server: https://127.0.0.1:6443\n  name: default\n"
                    .to_vec()
            }
            "version" => {
                let v = if script.contains("k3s.new") { new } else { ver };
                format!("k3s version {v} (abc123)").into_bytes()
            }
            "sha" => "c".repeat(64).into_bytes(),
            "read-token" => TOKEN.as_bytes().to_vec(),
            "private-ip" => b"10.89.1.2".to_vec(),
            "read-binary" => BIN.to_vec(),
            "boot-id" => format!("boot-{}", self.boot.get()).into_bytes(),
            "canary-mark" => {
                let n = script
                    .split("echo ")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .to_owned();
                *self.canary.borrow_mut() = n;
                vec![]
            }
            "canary-read" => {
                let n = if self.lose_canary.get() {
                    String::new()
                } else {
                    self.canary.borrow().clone()
                };
                format!("disk={n}\ncm={n}\n").into_bytes()
            }
            "snapshot-ls" => b"fluxvm-1-dev-1700000000  1MiB\n".to_vec(),
            "backup" => fake_backup_archive(st.datastore),
            "snapshot-pull" => {
                let mut v = vec![0x1f, 0x8b];
                v.extend([0u8; 64]);
                v
            }
            "unit-probe" => {
                let e = self.exec.borrow().get(node).cloned().unwrap_or_default();
                format!("[Service]\nExecStart={e}\n").into_bytes()
            }
            "kube-vip" => {
                self.vip.borrow_mut().push(node.into());
                vec![]
            }
            "kube-vip-probe" => {
                if self.vip.borrow().iter().any(|n| n == node) {
                    b"name: address, value: \"10.89.0.250\"\n".to_vec()
                } else {
                    b"absent\n".to_vec()
                }
            }
            "etcd-members" => self
                .etcd_members
                .borrow()
                .iter()
                .map(|m| format!("node/{m}\n"))
                .collect::<String>()
                .into_bytes(),
            "etcd-member-log" => {
                if self.member_log.get() {
                    b"level=info msg=\"Removing member dev-s3 from etcd cluster\"\n".to_vec()
                } else {
                    vec![]
                }
            }
            "snapshot-config" => self.snapshot_config.borrow().clone().into_bytes(),
            "snapshot-age" => match self.newest_age.get() {
                Some(a) => format!("now=100000000\nnewest={}\n", 100000000 - a).into_bytes(),
                None => b"now=100000000\nnewest=\n".to_vec(),
            },
            "rotate-server-secret" => {
                let s = String::from_utf8_lossy(stdin.unwrap_or_default()).into_owned();
                if let Some(new) = s.lines().nth(1) {
                    *self.dir_secret.borrow_mut() = new.to_owned();
                }
                *self.rotated.borrow_mut() = Some(s);
                vec![]
            }
            "read-server-dir-secret" => self.dir_secret.borrow().clone().into_bytes(),
            "delete-node" => {
                let gone = script
                    .split("delete node ")
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .unwrap_or_default()
                    .to_owned();
                self.nodes.borrow_mut().retain(|n| n.name != gone);
                vec![]
            }
            "unit" => {
                if let Some(line) = script.lines().find_map(|l| l.strip_prefix("ExecStart=")) {
                    self.exec.borrow_mut().insert(node.into(), line.into());
                }
                let mut nodes = self.nodes.borrow_mut();
                if !nodes.iter().any(|n| n.name == node) {
                    nodes.push(NodeInfo {
                        name: node.into(),
                        ready: true,
                        version: ver,
                    });
                }
                vec![]
            }
            "swap" => {
                let stuck = self.stuck_after_swap.borrow().iter().any(|n| n == node);
                for n in self
                    .nodes
                    .borrow_mut()
                    .iter_mut()
                    .filter(|n| n.name == node)
                {
                    n.version = new.clone();
                    n.ready = !stuck;
                }
                vec![]
            }
            "rollback" => {
                for n in self
                    .nodes
                    .borrow_mut()
                    .iter_mut()
                    .filter(|n| n.name == node)
                {
                    n.version = ver.clone();
                    n.ready = true;
                }
                vec![]
            }
            _ => vec![],
        };
        Ok(out)
    }
    fn tunnel_up(&self, _st: &State) -> bool {
        false
    }
    async fn start_tunnel(&self, _st: &State) -> Result<u32> {
        self.log("tunnel start");
        Ok(1)
    }
    fn stop_tunnel(&self, _st: &State) -> bool {
        self.log("tunnel stop");
        false
    }
    fn write_kubeconfig(
        &self,
        st: &State,
        raw: &str,
        path: Option<&std::path::Path>,
    ) -> Result<PathBuf> {
        let p = path
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.join(format!("{}.kubeconfig", st.name)));
        write_private(
            &p,
            &crate::kube_host::rewrite_kubeconfig(raw, &format!("apple-{}", st.name), st.api_port),
        )?;
        self.log("kubeconfig written");
        Ok(p)
    }
    fn remove_kubeconfig(&self, st: &State) {
        self.log("kubeconfig removed");
        let _ = std::fs::remove_file(self.root.join(format!("{}.kubeconfig", st.name)));
    }
    fn kubeconfig_exists(&self, st: &State) -> bool {
        self.root.join(format!("{}.kubeconfig", st.name)).is_file()
    }
    async fn serve_tunnel(&self, _st: &State) -> Result<()> {
        self.log("serve tunnel");
        Ok(())
    }
    fn install_tunnel_agent(&self, st: &State) -> Result<PathBuf> {
        self.log("agent install");
        Ok(self.root.join(format!("{}.plist", st.name)))
    }
    fn remove_tunnel_agent(&self, _st: &State) -> Result<bool> {
        self.log("agent remove");
        Ok(true)
    }
    fn tunnel_agent_status(&self, _st: &State) -> Result<Value> {
        Ok(serde_json::json!({"installed": true, "loaded": true}))
    }
    async fn scan_host_key(&self, _st: &State, node: &str) -> Result<String> {
        self.log(format!("scan {node}"));
        Ok(format!("ssh-ed25519 SCANNED-{node}"))
    }
}

fn setup() -> (tempfile::TempDir, Store, Fake) {
    let d = tempfile::tempdir().unwrap();
    let st = Store::new(d.path().to_path_buf());
    let f = Fake::new(d.path().to_path_buf());
    (d, st, f)
}

fn params() -> Params {
    Params {
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 30,
        image: "debian-13".into(),
    }
}

fn create_req(name: &str) -> CreateReq {
    CreateReq {
        name: name.into(),
        params: params(),
        api_port: 16443,
        datastore: Datastore::Sqlite,
        tls_sans: vec![],
        install: Install::Binary {
            data: BIN.to_vec(),
            sha256: sha256_hex(BIN),
        },
        kubeconfig: None,
        user: "mini".into(),
        servers: 1,
        endpoint: None,
        vip: None,
        flags: SharedFlags::default(),
    }
}

fn join_req(cluster: &str) -> JoinReq {
    JoinReq {
        cluster: cluster.into(),
        node: None,
        role: Role::Agent,
        install: Install::FromServer,
        external: None,
        params: params(),
        user: "mini".into(),
        keep_failed: false,
    }
}

fn pos(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not in {lines:#?}"))
}

async fn cluster_with_agent(f: &Fake, s: &Store) {
    create(f, s, &create_req("dev")).await.unwrap();
    join(f, s, &join_req("dev")).await.unwrap();
}

#[tokio::test]
async fn create_orders_journal_vm_labels_pin_install_ready_kubeconfig() {
    let (_d, s, f) = setup();
    let r = create(&f, &s, &create_req("dev")).await.unwrap();
    let l = f.lines();
    assert!(l[0].contains("create_vm k3s-dev journal=yes"), "{l:#?}");
    assert!(pos(&l, "label dev") > pos(&l, "create_vm"));
    assert!(pos(&l, "wait_ssh dev") > pos(&l, "label dev"));
    let order = [
        "preflight",
        "stage (stdin 6 bytes)",
        "agent-token (stdin 64 bytes)",
        "unit",
        "nodes",
        "version",
        "kubeconfig",
    ];
    let at: Vec<usize> = order
        .iter()
        .map(|o| pos(&l, &format!("ssh dev {o}")))
        .collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{l:#?}");
    let st = s.require("dev").unwrap();
    assert_eq!(st.phase, Phase::Ready);
    assert_eq!(st.k3s_version.as_deref(), Some("v1.31.4+k3s1"));
    assert_eq!(st.k3s_sha256, Some(sha256_hex(BIN)));
    assert!(st.nodes[0].host_key.starts_with("ssh-ed25519 HOSTKEY"));
    assert!(st.nodes[0].joined && st.nodes[0].id.is_some());
    assert!(!r.resumed && r.kubeconfig.exists());
    let kc = std::fs::read_to_string(&r.kubeconfig).unwrap();
    assert!(kc.contains("https://127.0.0.1:16443") && kc.contains("apple-dev"));
    assert_eq!(f.vms.borrow()[0].labels.as_ref().unwrap().3, st.owner);
}

#[tokio::test]
async fn create_refuses_an_existing_ready_cluster_and_bad_names() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    assert!(
        create(&f, &s, &create_req("dev"))
            .await
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    let mut bad = create_req("Dev");
    bad.name = "Dev".into();
    assert!(create(&f, &s, &bad).await.is_err());
    let mut san = create_req("x1");
    san.tls_sans = vec!["a b".into()];
    assert!(create(&f, &s, &san).await.is_err());
}

#[tokio::test]
async fn create_resumes_after_a_failed_bootstrap_without_a_second_vm() {
    let (_d, s, f) = setup();
    f.fail_on("dev:unit", "systemctl failed");
    let e = create(&f, &s, &create_req("dev")).await.unwrap_err();
    assert!(e.to_string().contains("systemctl failed"));
    let st = s.require("dev").unwrap();
    assert_eq!(st.phase, Phase::Bootstrapping);
    assert!(st.nodes[0].id.is_some());
    f.fail.borrow_mut().clear();
    f.clear();
    let r = create(&f, &s, &create_req("dev")).await.unwrap();
    assert!(r.resumed);
    assert!(
        !f.lines().iter().any(|l| l.contains("create_vm")),
        "{:#?}",
        f.lines()
    );
    assert_eq!(f.vms.borrow().len(), 1);
    assert_eq!(s.require("dev").unwrap().phase, Phase::Ready);
}

#[tokio::test]
async fn crash_after_vm_creation_is_adopted_by_owner_label_on_rerun() {
    let (_d, s, f) = setup();
    // The daemon made the VM and labelled it, but the journal never got the id.
    let mut st = State {
        schema: 1,
        name: "dev".into(),
        owner: "ownerA".into(),
        phase: Phase::Creating,
        user: "mini".into(),
        api_port: 16443,
        datastore: Datastore::Sqlite,
        external_url: None,
        tls_sans: vec![],
        k3s_sha256: None,
        k3s_version: None,
        params: params(),
        nodes: vec![],
        canary: None,
        servers_wanted: 1,
        flags: SharedFlags::default(),
        endpoint: None,
        snapshots: None,
        agent_secret_rotated: None,
        server_secret_rotated: None,
    };
    st.nodes.push(Node {
        name: "dev".into(),
        role: Role::Server,
        vm_name: "k3s-dev".into(),
        id: None,
        host_key: "ssh-ed25519 OLD".into(),
        joined: false,
        version: None,
    });
    s.save(&st).unwrap();
    f.add_vm(
        "k3s-dev",
        "running",
        Some(("dev", Role::Server, "dev", "ownerA")),
    );
    create(&f, &s, &create_req("dev")).await.unwrap();
    assert!(!f.lines().iter().any(|l| l.contains("create_vm")));
    assert_eq!(f.vms.borrow().len(), 1);
}

#[tokio::test]
async fn a_vm_with_the_name_but_another_owner_is_never_taken_over() {
    let (_d, s, f) = setup();
    f.add_vm(
        "k3s-dev",
        "running",
        Some(("dev", Role::Server, "dev", "someone-else")),
    );
    // A journal of ours exists with no id: the VM with that name is not ours.
    let e = create(&f, &s, &create_req("dev"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("not owned by this cluster"), "{e}");
    assert!(
        !f.lines()
            .iter()
            .any(|l| l.contains("ssh ") || l.contains("power"))
    );
}

#[tokio::test]
async fn preinstalled_installs_nothing_and_records_the_digest_from_the_guest() {
    let (_d, s, f) = setup();
    let mut req = create_req("dev");
    req.install = Install::Preinstalled;
    create(&f, &s, &req).await.unwrap();
    let l = f.lines();
    assert!(!l.iter().any(|x| x.contains("stage")));
    assert_eq!(s.require("dev").unwrap().k3s_sha256, Some("c".repeat(64)));
}

#[tokio::test]
async fn failed_vm_creation_leaves_a_resumable_journal_and_no_vm() {
    let (_d, s, f) = setup();
    f.fail_create_vm.set(true);
    assert!(create(&f, &s, &create_req("dev")).await.is_err());
    assert!(f.vms.borrow().is_empty());
    assert_eq!(s.require("dev").unwrap().phase, Phase::Creating);
}

#[tokio::test]
async fn join_sends_the_token_on_stdin_only_and_never_logs_it() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.clear();
    let r = join(&f, &s, &join_req("dev")).await.unwrap();
    assert_eq!(r.node, "dev-a1");
    let l = f.lines();
    let order = [
        "ssh dev private-ip",
        "ssh dev read-token",
        "create_vm k3s-dev-a1",
        "ssh dev-a1 preflight",
        "ssh dev-a1 stage",
        "ssh dev-a1 token (stdin",
        "ssh dev-a1 unit",
        "ssh dev nodes",
    ];
    let at: Vec<usize> = order.iter().map(|o| pos(&l, o)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{l:#?}");
    // The binary is copied from the server and verified against the recorded digest.
    assert!(pos(&l, "ssh dev read-binary") < pos(&l, "ssh dev-a1 stage"));
    let seen = f.stdin_seen.borrow();
    let tok = seen
        .iter()
        .find(|(n, t, _)| n == "dev-a1" && t == "token")
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&tok.2), TOKEN);
    assert!(!l.join("\n").contains("s3cr3t"));
    let journal = std::fs::read_to_string(s.path("dev")).unwrap();
    assert!(!journal.contains("s3cr3t"));
    let st = s.require("dev").unwrap();
    assert_eq!(st.nodes.len(), 2);
    assert!(st.node("dev-a1").unwrap().joined);
    assert_eq!(f.vms.borrow()[1].labels.as_ref().unwrap().1, Role::Agent);
}

#[tokio::test]
async fn failed_join_removes_the_node_again_and_redacts_the_token() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.fail_on("dev-a1:token", &format!("remote said {TOKEN} is wrong"));
    f.clear();
    let e = join(&f, &s, &join_req("dev")).await.unwrap_err();
    let msg = format!("{e:#}");
    assert!(!msg.contains("s3cr3t"), "{msg}");
    assert!(msg.contains("<redacted>") && msg.contains("removed again"));
    let l = f.lines();
    assert!(pos(&l, "ssh dev delete-node") < pos(&l, "power Delete k3s-dev-a1"));
    assert_eq!(s.require("dev").unwrap().nodes.len(), 1);
    assert_eq!(f.vms.borrow().len(), 1);
    assert!(!f.lines().join("\n").contains("s3cr3t"));
}

#[tokio::test]
async fn failed_join_keeps_the_node_with_keep_failed() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.fail_on("dev-a1:unit", "boom");
    let mut r = join_req("dev");
    r.keep_failed = true;
    assert!(join(&f, &s, &r).await.is_err());
    assert_eq!(f.vms.borrow().len(), 2);
    assert!(!s.require("dev").unwrap().node("dev-a1").unwrap().joined);
}

#[tokio::test]
async fn failed_join_whose_vm_cannot_be_deleted_stays_in_the_journal() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.fail_on("dev-a1:unit", "boom");
    f.fail_power_delete.borrow_mut().push("k3s-dev-a1".into());
    let e = join(&f, &s, &join_req("dev"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("left in the journal"), "{e}");
    assert!(s.require("dev").unwrap().node("dev-a1").is_some());
}

#[tokio::test]
async fn join_rejects_bad_requests_before_creating_anything() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let before = f.lines().len();
    let mut server = join_req("dev");
    server.role = Role::Server;
    assert!(
        join(&f, &s, &server)
            .await
            .unwrap_err()
            .to_string()
            .contains("embedded etcd")
    );
    let mut dup = join_req("dev");
    dup.node = Some("dev".into());
    assert!(join(&f, &s, &dup).await.is_err());
    let mut bad = join_req("dev");
    bad.node = Some("Bad Name".into());
    assert!(join(&f, &s, &bad).await.is_err());
    assert!(join(&f, &s, &join_req("nope")).await.is_err());
    assert!(!f.lines()[before..].iter().any(|l| l.contains("create_vm")));
}

#[tokio::test]
async fn join_stops_when_the_server_binary_no_longer_matches_the_recorded_digest() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let mut st = s.require("dev").unwrap();
    st.k3s_sha256 = Some(sha256_hex(b"something else"));
    s.save(&st).unwrap();
    let e = join(&f, &s, &join_req("dev")).await.unwrap_err();
    assert!(format!("{e:#}").contains("no longer matches"));
    assert_eq!(s.require("dev").unwrap().nodes.len(), 1);
}

#[tokio::test]
async fn external_join_reads_a_private_token_file_and_records_a_local_node() {
    use std::os::unix::fs::PermissionsExt;
    let (d, s, f) = setup();
    let tf = d.path().join("tok");
    std::fs::write(&tf, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut r = join_req("remote");
    r.external = Some(("https://cluster.example:6443".into(), tf.clone()));
    r.install = Install::Binary {
        data: BIN.to_vec(),
        sha256: sha256_hex(BIN),
    };
    let e = join(&f, &s, &r).await.unwrap_err().to_string();
    assert!(e.contains("readable by others"), "{e}");
    assert!(!e.contains("s3cr3t"));
    std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = join(&f, &s, &r).await.unwrap();
    assert_eq!(report.node, "remote-a1");
    let st = s.require("remote").unwrap();
    assert_eq!(
        st.external_url.as_deref(),
        Some("https://cluster.example:6443")
    );
    assert!(st.server().is_none());
    // Without a local server there is nothing to ask for readiness: the node is recorded as joined.
    assert!(!f.lines().iter().any(|l| l.contains("read-token")));
    let mut copy = r;
    copy.node = Some("remote-b".into());
    copy.install = Install::FromServer;
    assert!(join(&f, &s, &copy).await.is_err());
}

#[tokio::test]
async fn external_join_validates_the_url() {
    let (d, s, f) = setup();
    let tf = d.path().join("tok");
    write_private(&tf, TOKEN).unwrap();
    let mut r = join_req("remote");
    r.external = Some(("http://x:6443".into(), tf));
    assert!(join(&f, &s, &r).await.is_err());
    assert!(f.vms.borrow().is_empty());
}

#[tokio::test]
async fn status_reports_healthy_and_each_kind_of_problem() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    let ok = status(&f, &s, "dev").await.unwrap();
    assert!(ok.healthy, "{:?}", ok.problems);
    assert_eq!(ok.nodes.len(), 2);
    assert_eq!(ok.nodes[0].role, "server");
    assert_eq!(ok.nodes[1].ready, Some(true));
    // An agent that is not Ready.
    f.nodes.borrow_mut()[1].ready = false;
    let bad = status(&f, &s, "dev").await.unwrap();
    assert!(!bad.healthy && bad.problems.iter().any(|p| p.contains("not Ready")));
    // An agent newer than the server.
    f.nodes.borrow_mut()[1].ready = true;
    f.nodes.borrow_mut()[1].version = "v1.32.0+k3s1".into();
    let skew = status(&f, &s, "dev").await.unwrap();
    assert!(
        skew.problems
            .iter()
            .any(|p| p.contains("newer than the server"))
    );
    // A stopped VM.
    f.nodes.borrow_mut()[1].version = "v1.31.4+k3s1".into();
    f.vms.borrow_mut()[1].status = "stopped".into();
    let down = status(&f, &s, "dev").await.unwrap();
    assert!(down.problems.iter().any(|p| p.contains("VM is stopped")));
    // A missing VM.
    f.vms.borrow_mut().remove(1);
    let gone = status(&f, &s, "dev").await.unwrap();
    assert_eq!(gone.nodes[1].vm, "missing");
}

#[tokio::test]
async fn stop_goes_agents_first_and_start_goes_server_first() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.clear();
    let stopped = stop(&f, &s, "dev").await.unwrap();
    assert_eq!(stopped, ["dev-a1", "dev"]);
    let l = f.lines();
    let stops: Vec<&String> = l.iter().filter(|x| x.starts_with("power Stop")).collect();
    assert_eq!(stops, ["power Stop k3s-dev-a1", "power Stop k3s-dev"]);
    assert!(pos(&l, "ssh dev-a1 svc-stop") < pos(&l, "power Stop k3s-dev-a1"));
    assert!(pos(&l, "tunnel stop") < pos(&l, "ssh dev-a1 svc-stop"));
    // Stopping again does nothing.
    f.clear();
    assert!(stop(&f, &s, "dev").await.unwrap().is_empty());
    f.clear();
    start(&f, &s, "dev", true).await.unwrap();
    let l = f.lines();
    let starts: Vec<&String> = l.iter().filter(|x| x.starts_with("power Start")).collect();
    assert_eq!(starts, ["power Start k3s-dev", "power Start k3s-dev-a1"]);
    assert!(pos(&l, "wait_ssh dev") < pos(&l, "power Start k3s-dev-a1"));
    assert!(
        pos(&l, "ssh dev nodes") < pos(&l, "power Start k3s-dev-a1"),
        "server Ready before agents start"
    );
    assert_eq!(l.last().unwrap(), "tunnel start");
}

#[tokio::test]
async fn start_refuses_when_a_vm_is_gone() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    stop(&f, &s, "dev").await.unwrap();
    f.vms.borrow_mut().retain(|v| v.name != "k3s-dev-a1");
    let e = start(&f, &s, "dev", false).await.unwrap_err().to_string();
    assert!(e.contains("recover"), "{e}");
}

#[tokio::test]
async fn persistence_cycle_passes_and_catches_a_lost_canary_or_no_reboot() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.clear();
    let r = persist_cycle(&f, &s, "dev").await.unwrap();
    assert!(r.ok, "{r:?}");
    let l = f.lines();
    assert!(
        pos(&l, "canary-mark") < pos(&l, "power Stop")
            && pos(&l, "power Stop") < pos(&l, "power Start")
            && pos(&l, "power Start") < pos(&l, "canary-read")
    );
    // Data lost across the restart.
    f.lose_canary.set(true);
    let lost = persist_verify(&f, &s, "dev", false).await.unwrap();
    assert!(!lost.ok && lost.checks.iter().filter(|c| !c.ok).count() == 2);
    f.lose_canary.set(false);
    // Verify without a restart: same boot id.
    persist_mark(&f, &s, "dev").await.unwrap();
    let same = persist_verify(&f, &s, "dev", false).await.unwrap();
    assert!(!same.ok && same.checks[0].name == "rebooted");
    assert!(persist_verify(&f, &s, "dev", true).await.unwrap().ok);
}

#[tokio::test]
async fn verify_needs_a_mark_first() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    assert!(persist_verify(&f, &s, "dev", false).await.is_err());
}

fn upgrade_req(backup: bool) -> UpgradeReq {
    UpgradeReq {
        binary: b"NEWBINARY".to_vec(),
        sha256: sha256_hex(b"NEWBINARY"),
        allow_downgrade: false,
        backup,
        drain: false,
    }
}

#[tokio::test]
async fn upgrade_backs_up_then_upgrades_the_server_before_the_agents() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.clear();
    let r = upgrade(&f, &s, "dev", &upgrade_req(true)).await.unwrap();
    assert_eq!(
        (r.from.as_str(), r.to.as_str()),
        ("v1.31.4+k3s1", "v1.31.5+k3s1")
    );
    assert_eq!(r.upgraded, ["dev", "dev-a1"]);
    let l = f.lines();
    let order = [
        "ssh dev stage-new",
        "ssh dev version",
        "ssh dev backup",
        "ssh dev swap",
        "ssh dev-a1 stage-new",
        "ssh dev-a1 swap",
    ];
    let at: Vec<usize> = order.iter().map(|o| pos(&l, o)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{l:#?}");
    assert!(r.backup.unwrap().exists());
    let st = s.require("dev").unwrap();
    assert_eq!(st.k3s_version.as_deref(), Some("v1.31.5+k3s1"));
    assert_eq!(st.k3s_sha256, Some(sha256_hex(b"NEWBINARY")));
    assert_eq!(
        st.node("dev-a1").unwrap().version.as_deref(),
        Some("v1.31.5+k3s1")
    );
}

#[tokio::test]
async fn upgrade_rolls_back_the_node_that_does_not_come_back_and_stops() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.stuck_after_swap.borrow_mut().push("dev-a1".into());
    f.clear();
    let e = upgrade(&f, &s, "dev", &upgrade_req(false))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("dev-a1") && e.contains("rolled back") && e.contains("[dev]"),
        "{e}"
    );
    let l = f.lines();
    assert!(pos(&l, "ssh dev-a1 swap") < pos(&l, "ssh dev-a1 rollback"));
    assert!(!l.iter().any(|x| x.contains("ssh dev rollback")));
    // The staged binary is not left behind and the journal reflects the mix.
    assert!(l.iter().any(|x| x.contains("clean-staged")));
    let st = s.require("dev").unwrap();
    assert_eq!(
        st.node("dev").unwrap().version.as_deref(),
        Some("v1.31.5+k3s1")
    );
    assert_ne!(
        st.node("dev-a1").unwrap().version.as_deref(),
        Some("v1.31.5+k3s1")
    );
}

#[tokio::test]
async fn upgrade_checks_before_it_changes_anything() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    // A wrong digest never reaches the guest.
    let mut wrong = upgrade_req(false);
    wrong.sha256 = sha256_hex(b"other");
    f.clear();
    assert!(upgrade(&f, &s, "dev", &wrong).await.is_err());
    assert!(f.lines().is_empty());
    // A downgrade, and a jump over a minor release, are refused and the staged file is removed.
    *f.new_version.borrow_mut() = "v1.30.0+k3s1".into();
    assert!(
        upgrade(&f, &s, "dev", &upgrade_req(false))
            .await
            .unwrap_err()
            .to_string()
            .contains("older")
    );
    *f.new_version.borrow_mut() = "v1.33.0+k3s1".into();
    assert!(
        upgrade(&f, &s, "dev", &upgrade_req(false))
            .await
            .unwrap_err()
            .to_string()
            .contains("minor")
    );
    assert!(
        f.lines()
            .iter()
            .all(|l| !l.contains("swap") && !l.contains("backup"))
    );
    assert!(f.lines().iter().any(|l| l.contains("clean-staged")));
    // The same version and digest is a no-op.
    *f.new_version.borrow_mut() = "v1.31.4+k3s1".into();
    let mut st = s.require("dev").unwrap();
    st.k3s_sha256 = Some(sha256_hex(b"NEWBINARY"));
    s.save(&st).unwrap();
    f.clear();
    let r = upgrade(&f, &s, "dev", &upgrade_req(true)).await.unwrap();
    assert!(r.upgraded.is_empty());
    assert!(
        !f.lines()
            .iter()
            .any(|l| l.contains("swap") || l.contains("backup"))
    );
}

#[tokio::test]
async fn upgrade_refuses_an_unhealthy_cluster() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.nodes.borrow_mut()[1].ready = false;
    f.clear();
    let e = upgrade(&f, &s, "dev", &upgrade_req(false))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("not healthy"));
    assert!(!f.lines().iter().any(|l| l.contains("stage-new")));
}

#[tokio::test]
async fn upgrade_stops_before_swapping_when_the_backup_fails() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.fail_on("dev:backup", "disk full");
    f.clear();
    let e = format!(
        "{:#}",
        upgrade(&f, &s, "dev", &upgrade_req(true))
            .await
            .unwrap_err()
    );
    assert!(e.contains("nothing was upgraded"), "{e}");
    assert!(!f.lines().iter().any(|l| l.contains("swap")));
}

#[tokio::test]
async fn drain_wraps_the_agent_swap() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    let mut r = upgrade_req(false);
    r.drain = true;
    f.clear();
    upgrade(&f, &s, "dev", &r).await.unwrap();
    let l = f.lines();
    assert!(
        pos(&l, "dev drain") < pos(&l, "ssh dev-a1 swap")
            && pos(&l, "ssh dev-a1 swap") < pos(&l, "dev uncordon")
    );
}

#[tokio::test]
async fn backup_is_private_checksummed_and_never_overwritten() {
    use std::os::unix::fs::PermissionsExt;
    let (d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let out = d.path().join("b").join("dev.tar.gz");
    let r = backup(&f, &s, "dev", Some(out.clone())).await.unwrap();
    assert!(r.contains_cluster_token);
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let side = std::fs::read_to_string(sidecar_path(&out)).unwrap();
    assert!(side.starts_with(&r.sha256) && side.contains("dev.tar.gz"));
    f.clear();
    assert!(backup(&f, &s, "dev", Some(out)).await.is_err());
    assert!(
        f.lines().is_empty(),
        "no downtime for a backup that cannot be written"
    );
    // Default location and not-gzip output.
    let def = backup(&f, &s, "dev", None).await.unwrap();
    assert!(
        def.file.starts_with(s.backups_dir("dev"))
            && def.file.to_string_lossy().contains("20261121-101500")
    );
}

#[tokio::test]
async fn backup_refuses_output_that_is_not_an_archive() {
    let (d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.fail_on("dev:backup", "tar: boom");
    let out = d.path().join("x.tar.gz");
    assert!(backup(&f, &s, "dev", Some(out.clone())).await.is_err());
    assert!(!out.exists());
}

#[tokio::test]
async fn restore_verifies_checksum_and_contents_before_stopping_anything() {
    let (d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let b = backup(&f, &s, "dev", Some(d.path().join("dev.tar.gz")))
        .await
        .unwrap();
    f.clear();
    // Needs --yes.
    assert!(restore(&f, &s, "dev", &b.file, None, false).await.is_err());
    // A wrong digest touches nothing.
    let e = restore(&f, &s, "dev", &b.file, Some(&"0".repeat(64)), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("does not match"), "{e}");
    assert!(f.lines().is_empty());
    // The archive is looked at before the restore script runs.
    let r = restore(&f, &s, "dev", &b.file, None, true).await.unwrap();
    assert_eq!(r.sha256, b.sha256);
    let l = f.lines();
    assert!(pos(&l, "verify-archive") < pos(&l, "ssh dev restore"));
    assert!(pos(&l, "ssh dev restore") < pos(&l, "ssh dev nodes"));
    // A tampered file is refused.
    let mut data = std::fs::read(&b.file).unwrap();
    data.push(1);
    let t = d.path().join("t.tar.gz");
    std::fs::write(&t, &data).unwrap();
    std::fs::copy(sidecar_path(&b.file), sidecar_path(&t)).unwrap();
    f.clear();
    assert!(restore(&f, &s, "dev", &t, None, true).await.is_err());
    assert!(f.lines().is_empty());
    // No sidecar and no --sha256.
    let bare = d.path().join("bare.tar.gz");
    std::fs::write(&bare, &data).unwrap();
    assert!(
        restore(&f, &s, "dev", &bare, None, true)
            .await
            .unwrap_err()
            .to_string()
            .contains(".sha256")
    );
}

#[tokio::test]
async fn restore_does_not_stop_the_service_when_the_archive_is_wrong() {
    let (d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let b = backup(&f, &s, "dev", Some(d.path().join("dev.tar.gz")))
        .await
        .unwrap();
    f.fail_on("dev:verify-archive", "archive has no db/state.db");
    f.clear();
    assert!(restore(&f, &s, "dev", &b.file, None, true).await.is_err());
    assert!(!f.lines().iter().any(|l| l.contains("ssh dev restore")));
}

fn crashed_state(owner: &str, nodes: Vec<(&str, Role, Option<&str>)>) -> State {
    let mut st = crate::kube_apple::model::sample_state("dev");
    st.owner = owner.into();
    st.nodes = nodes
        .into_iter()
        .map(|(n, r, id)| Node {
            name: n.into(),
            role: r,
            vm_name: vm_name(n),
            id: id.map(str::to_owned),
            host_key: "ssh-ed25519 K".into(),
            joined: false,
            version: None,
        })
        .collect();
    st
}

#[tokio::test]
async fn recover_adopts_by_owner_label_and_reports_everything_else() {
    let (_d, s, f) = setup();
    let id = f.add_vm(
        "k3s-dev-a1",
        "running",
        Some(("dev", Role::Agent, "dev-a1", "ownerA")),
    );
    let orphan = f.add_vm(
        "k3s-dev-a9",
        "running",
        Some(("dev", Role::Agent, "dev-a9", "ownerA")),
    );
    f.add_vm(
        "k3s-dev-x",
        "running",
        Some(("dev", Role::Agent, "dev-x", "ownerB")),
    );
    let srv = f.add_vm("k3s-dev", "running", None); // lost its labels
    s.save(&crashed_state(
        "ownerA",
        vec![
            ("dev", Role::Server, Some(&srv)),
            ("dev-a1", Role::Agent, None),
            ("dev-a2", Role::Agent, None),
        ],
    ))
    .unwrap();
    let r = recover(&f, &s, "dev", &RecoverOpts::default())
        .await
        .unwrap();
    assert_eq!(r.adopted, [format!("dev-a1 -> {id}")]);
    assert_eq!(r.relabelled, ["dev"]);
    assert_eq!(r.dropped_never_created, ["dev-a2"]);
    assert_eq!(r.orphans.len(), 1);
    assert!(r.orphans[0].contains(&orphan));
    assert_eq!(r.foreign.len(), 1);
    assert!(r.foreign[0].contains("dev-x"));
    let st = s.require("dev").unwrap();
    assert_eq!(st.node("dev-a1").unwrap().id.as_deref(), Some(id.as_str()));
    assert!(st.node("dev-a2").is_none());
    // Nothing was deleted and the foreign VM was not touched.
    assert!(!f.lines().iter().any(|l| l.contains("power")));
    assert_eq!(f.vms.borrow().len(), 4);
}

#[tokio::test]
async fn recover_deletes_only_our_orphans_when_asked() {
    let (_d, s, f) = setup();
    let srv = f.add_vm(
        "k3s-dev",
        "running",
        Some(("dev", Role::Server, "dev", "ownerA")),
    );
    f.add_vm(
        "k3s-dev-a9",
        "running",
        Some(("dev", Role::Agent, "dev-a9", "ownerA")),
    );
    f.add_vm(
        "k3s-dev-x",
        "running",
        Some(("dev", Role::Agent, "dev-x", "ownerB")),
    );
    s.save(&crashed_state(
        "ownerA",
        vec![("dev", Role::Server, Some(&srv))],
    ))
    .unwrap();
    let r = recover(
        &f,
        &s,
        "dev",
        &RecoverOpts {
            delete_orphans: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r.deleted_orphans.len(), 1);
    let names: Vec<String> = f.vms.borrow().iter().map(|v| v.name.clone()).collect();
    assert_eq!(names, ["k3s-dev", "k3s-dev-x"]);
    let l = f.lines();
    assert!(pos(&l, "power Stop k3s-dev-a9") < pos(&l, "power Delete k3s-dev-a9"));
}

#[tokio::test]
async fn recover_treats_an_unlabelled_vm_as_ambiguous_unless_told() {
    let (_d, s, f) = setup();
    let id = f.add_vm("k3s-dev", "running", None);
    s.save(&crashed_state("ownerA", vec![("dev", Role::Server, None)]))
        .unwrap();
    let r = recover(&f, &s, "dev", &RecoverOpts::default())
        .await
        .unwrap();
    assert_eq!(r.ambiguous.len(), 1);
    assert!(s.require("dev").unwrap().nodes[0].id.is_none());
    let r = recover(
        &f,
        &s,
        "dev",
        &RecoverOpts {
            adopt_unlabelled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(r.adopted.len(), 1);
    assert_eq!(
        s.require("dev").unwrap().nodes[0].id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(f.vms.borrow()[0].labels.as_ref().unwrap().3, "ownerA");
}

#[tokio::test]
async fn recover_never_adopts_a_vm_labelled_for_someone_else() {
    let (_d, s, f) = setup();
    f.add_vm(
        "k3s-dev",
        "running",
        Some(("dev", Role::Server, "dev", "ownerB")),
    );
    s.save(&crashed_state("ownerA", vec![("dev", Role::Server, None)]))
        .unwrap();
    let r = recover(
        &f,
        &s,
        "dev",
        &RecoverOpts {
            adopt_unlabelled: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(r.adopted.is_empty());
    assert_eq!(r.foreign.len(), 1);
}

#[tokio::test]
async fn recover_reports_a_journalled_vm_that_vanished() {
    let (_d, s, f) = setup();
    s.save(&crashed_state(
        "ownerA",
        vec![(
            "dev",
            Role::Server,
            Some("00000000-0000-0000-0000-000000000099"),
        )],
    ))
    .unwrap();
    let r = recover(&f, &s, "dev", &RecoverOpts::default())
        .await
        .unwrap();
    assert_eq!(r.missing.len(), 1);
}

#[tokio::test]
async fn delete_removes_agents_then_the_server_then_the_journal() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.clear();
    assert!(delete(&f, &s, "dev", false).await.is_err());
    assert!(f.lines().is_empty());
    let r = delete(&f, &s, "dev", true).await.unwrap();
    assert_eq!(r.deleted, ["dev-a1", "dev"]);
    assert!(r.journal_removed && s.load("dev").unwrap().is_none());
    let l = f.lines();
    let dels: Vec<&String> = l.iter().filter(|x| x.starts_with("power Delete")).collect();
    assert_eq!(dels, ["power Delete k3s-dev-a1", "power Delete k3s-dev"]);
    assert!(l.contains(&"kubeconfig removed".to_owned()));
    assert!(f.vms.borrow().is_empty());
}

#[tokio::test]
async fn delete_keeps_the_failed_node_in_the_journal_for_a_retry() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.fail_power_delete.borrow_mut().push("k3s-dev-a1".into());
    f.clear();
    let r = delete(&f, &s, "dev", true).await.unwrap();
    assert_eq!(r.deleted, ["dev"]);
    assert_eq!(r.failed.len(), 1);
    assert!(!r.journal_removed);
    assert!(!f.lines().contains(&"kubeconfig removed".to_owned()));
    let st = s.require("dev").unwrap();
    assert_eq!(st.nodes.len(), 1);
    assert_eq!(st.nodes[0].name, "dev-a1");
    // Retry once the daemon cooperates.
    f.fail_power_delete.borrow_mut().clear();
    let r = delete(&f, &s, "dev", true).await.unwrap();
    assert!(r.journal_removed && r.failed.is_empty());
}

#[tokio::test]
async fn delete_never_touches_a_vm_it_does_not_own() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    // The journal's VM id no longer matches the VM that carries its name.
    f.vms.borrow_mut()[0].id = "00000000-0000-0000-0000-0000000000ff".into();
    let r = delete(&f, &s, "dev", true).await.unwrap();
    assert_eq!(r.failed.len(), 1);
    assert_eq!(f.vms.borrow().len(), 1);
}

#[tokio::test]
async fn kubeconfig_and_logs_use_the_server() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let p = kubeconfig(&f, &s, "dev", None).await.unwrap();
    assert!(p.exists());
    assert!(logs(&f, &s, "dev", Some("nope"), 10).await.is_err());
    logs(&f, &s, "dev", None, 10).await.unwrap();
}

#[tokio::test]
async fn unknown_clusters_are_reported() {
    let (_d, s, f) = setup();
    assert!(
        status(&f, &s, "nope")
            .await
            .unwrap_err()
            .to_string()
            .contains("kube apple list")
    );
    assert!(stop(&f, &s, "nope").await.is_err());
}

#[test]
fn install_resolution_demands_a_digest_and_checks_it() {
    use super::{BinaryArgs, resolve_install};
    let d = tempfile::tempdir().unwrap();
    let bin = d.path().join("k3s-arm64");
    std::fs::write(&bin, BIN).unwrap();
    let args = |binary: Option<&std::path::Path>, sha: Option<String>, pre: bool| BinaryArgs {
        binary: binary.map(PathBuf::from),
        sha256: sha,
        sha256_file: None,
        preinstalled: pre,
    };
    assert!(matches!(
        resolve_install(&args(None, None, true), None).unwrap(),
        Install::Preinstalled
    ));
    assert!(
        resolve_install(&args(None, None, false), None).is_err(),
        "the first server must say where k3s comes from"
    );
    assert!(matches!(
        resolve_install(&args(None, None, false), Some(Install::FromServer)).unwrap(),
        Install::FromServer
    ));
    assert!(
        resolve_install(&args(Some(&bin), None, false), None)
            .unwrap_err()
            .to_string()
            .contains("requires its published")
    );
    assert!(
        resolve_install(&args(Some(&bin), Some("0".repeat(64)), false), None)
            .unwrap_err()
            .to_string()
            .contains("nothing was sent")
    );
    assert!(matches!(
        resolve_install(&args(Some(&bin), Some(sha256_hex(BIN)), false), None).unwrap(),
        Install::Binary { .. }
    ));
    assert!(
        resolve_install(
            &args(None, Some(sha256_hex(BIN)), false),
            Some(Install::FromServer)
        )
        .is_err()
    );
    // A digest from the release's sums file, matched by the binary's file name.
    let sums = d.path().join("sums.txt");
    std::fs::write(&sums, format!("{}  k3s-arm64\n", sha256_hex(BIN))).unwrap();
    let mut a = args(Some(&bin), None, false);
    a.sha256_file = Some(sums);
    assert!(matches!(
        resolve_install(&a, None).unwrap(),
        Install::Binary { .. }
    ));
}

#[test]
fn script_helpers_used_by_the_fake_stay_in_sync() {
    // The fake classifies scripts by their text; if a script changes shape the lifecycle tests would silently stop
    // exercising it, so pin the classification.
    let sans: Vec<String> = vec![];
    let unit = UnitOpts {
        role: Role::Server,
        node: "dev",
        tls_sans: &sans,
        cluster_init: false,
        join_url: None,
        token_file: false,
        agent_token_file: false,
        flags: None,
        private_net: true,
    };
    use script::*;
    assert_eq!(tag(&preflight_script()), "preflight");
    assert_eq!(
        tag(&stage_binary_script(&"a".repeat(64), K3S_BIN, false)),
        "stage"
    );
    assert_eq!(
        tag(&stage_binary_script(&"a".repeat(64), NEW_BIN, true)),
        "stage-new"
    );
    assert_eq!(tag(&write_token_script()), "token");
    assert_eq!(tag(&install_unit_script(&unit)), "unit");
    assert_eq!(tag(&nodes_json_script()), "nodes");
    assert_eq!(tag(&swap_script(Role::Server)), "swap");
    assert_eq!(tag(&rollback_script(Role::Server)), "rollback");
    assert_eq!(tag(&cleanup_staged_script()), "clean-staged");
    assert_eq!(tag(&backup_script(Datastore::Sqlite, "1")), "backup");
    assert_eq!(tag(&backup_script(Datastore::Etcd, "1")), "backup");
    assert_eq!(tag(&restore_script(Datastore::Sqlite, "1")), "restore");
    assert_eq!(
        tag(&verify_archive_script(Datastore::Sqlite)),
        "verify-archive"
    );
    assert_eq!(tag(&read_token_script(Role::Agent)), "read-token");
    assert_eq!(tag(&read_token_script(Role::Server)), "read-token");
    assert_eq!(tag(&read_binary_script()), "read-binary");
    assert_eq!(tag(&private_ip_script()), "private-ip");
    assert_eq!(tag(&canary_mark_script("ab12")), "canary-mark");
    assert_eq!(tag(&canary_read_script()), "canary-read");
    assert_eq!(tag(&boot_id_script()), "boot-id");
    assert_eq!(tag(&is_active_script(Role::Agent)), "active");
    assert_eq!(tag(&delete_node_script("dev-a1")), "delete-node");
    assert_eq!(tag(&sha_of_installed_script()), "sha");
    assert_eq!(tag(&version_script(K3S_BIN)), "version");
}

#[tokio::test]
async fn agents_join_with_their_own_token_that_is_never_the_server_token() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    let seen = f.stdin_seen.borrow();
    let at = seen
        .iter()
        .find(|(n, t, _)| n == "dev" && t == "agent-token")
        .unwrap();
    let tok = String::from_utf8_lossy(&at.2).into_owned();
    assert_eq!(tok.len(), 64);
    assert!(token_ok(&tok));
    // It is not in the VM request, the journal or the log; the server unit reads it from a file.
    let journal = std::fs::read_to_string(s.path("dev")).unwrap();
    assert!(!journal.contains(&tok));
    assert!(!f.lines().join("\n").contains(&tok));
    drop(seen);
    f.clear();
    join(&f, &s, &join_req("dev")).await.unwrap();
    let l = f.lines();
    // The joining agent reads the agent token, not the server's `token` file.
    assert!(l.iter().any(|x| x.contains("ssh dev read-token")));
    assert!(!script::read_token_script(Role::Agent).contains("server/token"));
    assert!(script::read_token_script(Role::Server).contains("server/token"));
}

async fn etcd_cluster(f: &Fake, s: &Store) {
    let mut r = create_req("dev");
    r.datastore = Datastore::Etcd;
    create(f, s, &r).await.unwrap();
}

#[tokio::test]
async fn snapshots_need_embedded_etcd() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    f.clear();
    let e = snapshot_save(&f, &s, "dev", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("--embedded-etcd") && e.contains("backup"), "{e}");
    assert!(snapshot_ls(&f, &s, "dev").await.is_err());
    assert!(f.lines().is_empty());
}

#[tokio::test]
async fn snapshot_commands_validate_names_and_run_on_the_server() {
    let (d, s, f) = setup();
    etcd_cluster(&f, &s).await;
    assert!(f.lines().iter().any(|l| l == "ssh dev unit"));
    f.clear();
    snapshot_save(&f, &s, "dev", Some("nightly")).await.unwrap();
    assert_eq!(
        snapshot_ls(&f, &s, "dev").await.unwrap().trim(),
        "fluxvm-1-dev-1700000000  1MiB"
    );
    snapshot_delete(&f, &s, "dev", "nightly-dev-1")
        .await
        .unwrap();
    snapshot_prune(&f, &s, "dev", "nightly", 3).await.unwrap();
    let l = f.lines();
    for t in [
        "snapshot-save",
        "snapshot-ls",
        "snapshot-delete",
        "snapshot-prune",
    ] {
        assert!(
            l.iter().any(|x| x == &format!("ssh dev {t}")),
            "{t}: {l:#?}"
        );
    }
    for bad in ["a;b", "-rf", "../x", "", "a b"] {
        assert!(
            snapshot_save(&f, &s, "dev", Some(bad)).await.is_err(),
            "{bad:?}"
        );
        assert!(
            snapshot_delete(&f, &s, "dev", bad).await.is_err(),
            "{bad:?}"
        );
    }
    assert!(snapshot_prune(&f, &s, "dev", "nightly", 0).await.is_err());
    // A default name is made from the time.
    snapshot_save(&f, &s, "dev", None).await.unwrap();
    // Pull: private, checksummed, never overwritten.
    let out = d.path().join("snap.tar.gz");
    let r = snapshot_pull(&f, &s, "dev", "nightly-dev-1", Some(out.clone()))
        .await
        .unwrap();
    assert!(r.contains_cluster_token && out.exists());
    assert!(
        snapshot_pull(&f, &s, "dev", "nightly-dev-1", Some(out))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn snapshot_restore_needs_yes_and_waits_for_the_server() {
    let (_d, s, f) = setup();
    etcd_cluster(&f, &s).await;
    f.clear();
    assert!(snapshot_restore(&f, &s, "dev", "x1", false).await.is_err());
    assert!(f.lines().is_empty());
    snapshot_restore(&f, &s, "dev", "x1", true).await.unwrap();
    let l = f.lines();
    assert!(pos(&l, "snapshot-restore") < pos(&l, "ssh dev nodes"));
}

#[tokio::test]
async fn upgrade_of_an_etcd_cluster_snapshots_first() {
    let (_d, s, f) = setup();
    etcd_cluster(&f, &s).await;
    f.clear();
    upgrade(&f, &s, "dev", &upgrade_req(true)).await.unwrap();
    let l = f.lines();
    assert!(pos(&l, "ssh dev backup") < pos(&l, "ssh dev swap"));
    assert!(script::backup_script(Datastore::Etcd, "1").contains("etcd-snapshot save"));
}

#[path = "tests_ha.rs"]
mod ha;
