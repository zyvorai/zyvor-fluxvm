// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The `templates run|down` flow for VM environments against a fake daemon and guest.

use super::*;
use crate::run::VmApi;
use crate::stack::StackVm;
use fluxvm_core::up::Kvm;
use fluxvm_scheduler::oci_sandbox::SandboxLogs;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

#[derive(Default)]
struct Daemon {
    vms: RefCell<Vec<Value>>,
    snapshots: RefCell<BTreeSet<(Uuid, String)>>,
    calls: RefCell<Vec<String>>,
    fail_create: bool,
}

impl Daemon {
    fn add(&self, name: &str, status: &str, request: Value) -> Uuid {
        let id = Uuid::new_v4();
        self.vms.borrow_mut().push(
            json!({"id": id, "name": name, "status": status, "request": request, "labels": {}}),
        );
        id
    }
    fn set(&self, id: Uuid, status: &str) {
        for v in self.vms.borrow_mut().iter_mut() {
            if v["id"] == id.to_string().as_str() {
                v["status"] = json!(status);
            }
        }
    }
    fn log(&self, s: &str) {
        self.calls.borrow_mut().push(s.to_owned());
    }
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut self.calls.borrow_mut())
    }
}

impl VmApi for Daemon {
    async fn create(&self, spec: Value) -> Result<Uuid> {
        self.log("create");
        if self.fail_create {
            bail!("no space left on device")
        }
        let name = spec["name"].as_str().unwrap().to_owned();
        Ok(self.add(&name, "running", spec))
    }
    async fn guest_ip(&self, _: Uuid) -> Result<Option<String>> {
        Ok(Some("192.168.64.9".into()))
    }
    async fn delete(&self, id: Uuid) -> Result<()> {
        self.log("delete");
        self.vms
            .borrow_mut()
            .retain(|v| v["id"] != id.to_string().as_str());
        Ok(())
    }
    async fn find(&self, _: &str) -> Result<Option<(Uuid, String)>> {
        unimplemented!()
    }
    async fn snapshot(&self, id: Uuid, tag: &str) -> Result<()> {
        self.log("snapshot");
        self.snapshots.borrow_mut().insert((id, tag.into()));
        Ok(())
    }
    async fn has_snapshot(&self, id: Uuid, tag: &str) -> Result<bool> {
        Ok(self.snapshots.borrow().contains(&(id, tag.into())))
    }
    async fn list_snapshots(&self, _: Uuid) -> Result<Vec<Value>> {
        unimplemented!()
    }
    async fn delete_snapshot(&self, _: Uuid, _: &str) -> Result<()> {
        unimplemented!()
    }
    async fn stop(&self, id: Uuid) -> Result<()> {
        self.log("stop");
        self.set(id, "stopped");
        Ok(())
    }
    async fn restore(&self, id: Uuid, _: &str) -> Result<()> {
        self.log("restore");
        self.set(id, "running");
        Ok(())
    }
    async fn start(&self, id: Uuid) -> Result<()> {
        self.log("start");
        self.set(id, "running");
        Ok(())
    }
    async fn set_labels(&self, _: Uuid, _: BTreeMap<String, String>) -> Result<()> {
        self.log("labels");
        Ok(())
    }
    async fn list_stack(&self, _: &str) -> Result<Vec<StackVm>> {
        unimplemented!()
    }
    async fn create_sandbox(&self, _: Value) -> Result<Uuid> {
        unimplemented!()
    }
    async fn sandbox_logs(&self, _: Uuid) -> Result<SandboxLogs> {
        unimplemented!()
    }
    async fn vznets(&self) -> Result<Value> {
        unimplemented!()
    }
}

impl EnvApi for Daemon {
    async fn records(&self) -> Result<Vec<Value>> {
        Ok(self.vms.borrow().clone())
    }
    async fn resume_vm(&self, id: Uuid) -> Result<()> {
        self.log("resume");
        self.set(id, "running");
        Ok(())
    }
}

struct Machine {
    backend: Backend,
    arch: &'static str,
    server: Option<String>,
    checks: HostChecks,
    has_key: Cell<bool>,
    generated: Cell<bool>,
    answer: bool,
    asked: Cell<bool>,
    /// A script containing this fails.
    failing: Option<&'static str>,
    cloud_init: RefCell<Vec<i32>>,
    scripts: RefCell<Vec<String>>,
}

fn machine(backend: Backend) -> Machine {
    Machine {
        backend,
        arch: if backend == Backend::Vz {
            "aarch64"
        } else {
            "x86_64"
        },
        server: None,
        checks: HostChecks {
            apple: (backend == Backend::Vz).then(|| {
                Ok(fluxvm_apple::AppleHostCapabilities {
                    rosetta: "installed".into(),
                    nested_virtualization: true,
                    ..Default::default()
                })
            }),
            kvm: Kvm::Ready,
            nested_kvm: Some(true),
            missing_tools: vec![],
            virtiofsd: true,
            disk_free_gib: Some(200),
            mem_total_mib: Some(32768),
            image: ImageState::Cached,
            busy_ports: vec![],
            invalid: None,
        },
        has_key: Cell::new(true),
        generated: Cell::new(false),
        answer: false,
        asked: Cell::new(false),
        failing: None,
        cloud_init: RefCell::default(),
        scripts: RefCell::default(),
    }
}

impl World for Machine {
    fn platform(&self) -> (Backend, String) {
        (self.backend, self.arch.into())
    }
    fn user(&self) -> String {
        "dev".into()
    }
    fn ssh_key(&self, generate: bool) -> Result<Option<String>> {
        if generate && !self.has_key.get() {
            self.has_key.set(true);
            self.generated.set(true);
        }
        Ok(self.has_key.get().then(|| "ssh-ed25519 AAA".to_owned()))
    }
    async fn facts(&self, _: &Value, _: &EnvTemplate, _: bool) -> HostFacts {
        HostFacts {
            backend: self.backend,
            os: if self.backend == Backend::Vz {
                "macos".into()
            } else {
                "linux".into()
            },
            arch: self.arch.into(),
            remote: self.server.as_deref().is_some_and(|s| !is_loopback(s)),
            host: self.server.is_none().then(|| self.checks.clone()),
            server: self.server.clone(),
        }
    }
    async fn address<A: VmApi>(&self, api: &A, id: Uuid, _: &str, _: Duration) -> Result<String> {
        Ok(api.guest_ip(id).await?.unwrap())
    }
    async fn exec(&self, _: &str, _: &str, script: &str, _: Duration) -> Result<(i32, String)> {
        self.scripts.borrow_mut().push(script.to_owned());
        if script.contains("cloud-init status") {
            let mut q = self.cloud_init.borrow_mut();
            let code = if q.is_empty() { 0 } else { q.remove(0) };
            return Ok((code, "status: something".into()));
        }
        match self.failing {
            Some(f) if script.contains(f) => Ok((127, "not found".into())),
            _ => Ok((0, String::new())),
        }
    }
    async fn pause_between_tries(&self) {}
    fn confirm(&self) -> Result<bool> {
        self.asked.set(true);
        Ok(self.answer)
    }
}

fn job(template: &str) -> Job {
    Job {
        template: vm_env::load(template).unwrap().unwrap(),
        reference: template.into(),
        flags: VmFlags::default(),
        down: false,
        yes: false,
        json: false,
    }
}

fn with(template: &str, f: impl FnOnce(&mut Job)) -> Job {
    let mut j = job(template);
    f(&mut j);
    j
}

async fn go(d: &Daemon, m: &Machine, j: &Job) -> (i32, String) {
    let mut out = Vec::new();
    let code = run(d, m, j, Path::new("/tmp"), Path::new("/tmp"), &mut out)
        .await
        .unwrap();
    (code, String::from_utf8(out).unwrap())
}

#[tokio::test]
async fn a_first_run_creates_checks_and_a_second_changes_nothing_on_both_backends() {
    for backend in [Backend::Vz, Backend::Qemu] {
        let (d, m) = (Daemon::default(), machine(backend));
        let (code, out) = go(&d, &m, &job("debian")).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(d.calls(), ["create", "labels"]);
        assert!(
            out.contains("[to do]   VM: create debian") && out.contains("debian is ready"),
            "{out}"
        );
        assert!(out.contains("[ok]   git") && out.contains("fluxctl ssh debian"));
        // The second run: the QEMU SSH forward and the marker must not look like a changed definition.
        let (code, out) = go(&d, &m, &job("debian")).await;
        assert_eq!(code, 0, "{out}");
        assert!(d.calls().is_empty(), "{backend:?}");
        assert!(
            out.contains("is running; nothing to change") && out.contains("Nothing was changed"),
            "{out}"
        );
        assert!(!out.contains("differs"), "{backend:?}: {out}");
        let request = d.vms.borrow()[0]["request"].clone();
        assert_eq!(
            request["backend"],
            if backend == Backend::Vz { "vz" } else { "qemu" }
        );
        assert_eq!(
            crate::run::loopback_ssh_endpoint(&request).is_some(),
            backend == Backend::Qemu
        );
    }
}

#[tokio::test]
async fn a_dry_run_shows_the_vm_and_touches_nothing_not_even_a_key() {
    let (d, m) = (Daemon::default(), machine(Backend::Vz));
    m.has_key.set(false);
    let j = with("build", |j| {
        j.flags.dry_run = true;
        j.flags.publish = vec!["8123:80".into()];
        j.flags.snapshot = true;
    });
    let (code, out) = go(&d, &m, &j).await;
    assert_eq!(code, 0);
    assert!(d.calls().is_empty() && !m.generated.get() && m.scripts.borrow().is_empty());
    for part in [
        "Dry run: nothing was changed",
        "4 CPUs, 4096 MiB memory, 40 GiB disk",
        "build-essential",
        "127.0.0.1:8123 -> 80",
        "Ready when SSH answers",
        "save memory and disk as `ready`",
    ] {
        assert!(out.contains(part), "{part}: {out}");
    }
}

#[tokio::test]
async fn json_is_one_versioned_document() {
    let (d, m) = (Daemon::default(), machine(Backend::Vz));
    let (code, out) = go(&d, &m, &with("debian", |j| j.json = true)).await;
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["changed"], true);
    assert_eq!(v["ready"], true);
    assert_eq!(v["plan"]["action"], "create");
    assert_eq!(v["address"], "192.168.64.9");
    let (_, out) = go(
        &d,
        &m,
        &with("debian", |j| {
            j.json = true;
            j.flags.dry_run = true;
        }),
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["plan"]["action"], "check");
    assert_eq!(v["changed"], false);
}

#[tokio::test]
async fn stopped_and_paused_vms_are_started_and_resumed() {
    for (status, call) in [
        ("stopped", "start"),
        ("paused", "resume"),
        ("failed", "start"),
    ] {
        let (d, m) = (Daemon::default(), machine(Backend::Vz));
        go(&d, &m, &job("debian")).await;
        d.calls();
        let id: Uuid = d.vms.borrow()[0]["id"].as_str().unwrap().parse().unwrap();
        d.set(id, status);
        let (code, out) = go(&d, &m, &job("debian")).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(d.calls(), [call], "{status}");
    }
}

#[tokio::test]
async fn a_vm_that_is_not_ours_is_never_touched() {
    let d = Daemon::default();
    d.add("debian", "stopped", json!({"vcpus": 1}));
    let m = machine(Backend::Vz);
    for j in [
        job("debian"),
        with("debian", |j| {
            j.flags.recreate = true;
            j.yes = true;
        }),
        with("debian", |j| {
            j.down = true;
            j.flags.delete = true;
            j.yes = true;
        }),
    ] {
        let (code, out) = go(&d, &m, &j).await;
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("was not made by `fluxctl templates`"), "{out}");
        assert!(d.calls().is_empty());
    }
    // Nor another template's, under the same name.
    let d = Daemon::default();
    go(&d, &m, &with("build", |j| j.flags.vm = Some("box".into()))).await;
    d.calls();
    let (code, out) = go(&d, &m, &with("debian", |j| j.flags.vm = Some("box".into()))).await;
    assert_eq!(code, 1);
    assert!(
        out.contains("from the template \"build\"") && d.calls().is_empty(),
        "{out}"
    );
}

#[tokio::test]
async fn changed_options_are_reported_and_only_recreate_deletes() {
    let (d, mut m) = (Daemon::default(), machine(Backend::Vz));
    go(&d, &m, &job("debian")).await;
    d.calls();
    let bigger = with("debian", |j| j.flags.memory_mib = Some(4096));
    let (code, out) = go(&d, &m, &bigger).await;
    assert_eq!(code, 0, "{out}");
    assert!(d.calls().is_empty());
    assert!(
        out.contains("memory MiB 2048 -> 4096") && out.contains("--recreate"),
        "{out}"
    );
    let recreate = with("debian", |j| {
        j.flags.memory_mib = Some(4096);
        j.flags.recreate = true;
    });
    let (code, out) = go(&d, &m, &recreate).await;
    assert!(
        m.asked.get() && code == 0 && out.contains("Nothing changed."),
        "{out}"
    );
    assert!(d.calls().is_empty());
    m.answer = true;
    let (code, _) = go(&d, &m, &recreate).await;
    assert_eq!(code, 0);
    assert_eq!(d.calls(), ["delete", "create", "labels"]);
    assert_eq!(d.vms.borrow()[0]["request"]["memory_mib"], 4096);
}

#[tokio::test]
async fn a_users_own_ssh_forward_is_shown_and_the_automatic_one_is_not() {
    let (d, m) = (Daemon::default(), machine(Backend::Qemu));
    let j = with("debian", |j| j.flags.publish = vec!["18999:22".into()]);
    go(&d, &m, &j).await;
    let (_, out) = go(&d, &m, &j).await;
    let ports = out.lines().find(|l| l.trim().starts_with("Ports")).unwrap();
    assert!(ports.trim().ends_with("127.0.0.1:18999 -> 22"), "{ports}");
    assert!(!out.contains("differs"), "{out}");
}

#[tokio::test]
async fn a_failing_check_means_not_ready_and_the_vm_is_kept() {
    let (d, mut m) = (Daemon::default(), machine(Backend::Vz));
    m.failing = Some("jq --version");
    let j = with("debian", |j| j.flags.snapshot = true);
    let (code, out) = go(&d, &m, &j).await;
    assert_eq!(code, 1);
    assert!(
        out.contains("[fail] curl and jq: exit 127: not found")
            && out.contains("TEMPLATE_VM_NOT_READY"),
        "{out}"
    );
    // No snapshot of a machine that is not ready, and nothing is deleted.
    assert_eq!(d.calls(), ["create", "labels"]);
    let tries = m
        .scripts
        .borrow()
        .iter()
        .filter(|s| s.contains("jq --version"))
        .count();
    assert_eq!(tries, CHECK_TRIES as usize);
}

#[tokio::test]
async fn cloud_init_errors_fail_warnings_pass_and_dropped_ssh_is_retried() {
    let (d, m) = (Daemon::default(), machine(Backend::Vz));
    *m.cloud_init.borrow_mut() = vec![255, 255, 2];
    let (code, out) = go(&d, &m, &job("debian")).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("finished: with warnings"), "{out}");
    *m.cloud_init.borrow_mut() = vec![1];
    let (code, out) = go(&d, &m, &job("debian")).await;
    assert_eq!(code, 1);
    assert!(out.contains("cloud-init reports an error"), "{out}");
}

#[tokio::test]
async fn a_failed_create_is_reported_without_checks() {
    let d = Daemon {
        fail_create: true,
        ..Default::default()
    };
    let m = machine(Backend::Vz);
    let (code, out) = go(&d, &m, &job("debian")).await;
    assert_eq!(code, 1);
    assert!(
        out.contains("Failed: creating debian: no space left") && m.scripts.borrow().is_empty(),
        "{out}"
    );
}

#[tokio::test]
async fn snapshot_is_saved_once_and_reset_asks_and_needs_it() {
    let (d, mut m) = (Daemon::default(), machine(Backend::Vz));
    let reset = with("debian", |j| j.flags.reset = true);
    let (code, out) = go(&d, &m, &reset).await;
    assert!(code == 1 && out.contains("nothing to reset"), "{out}");
    go(&d, &m, &job("debian")).await;
    d.calls();
    let (code, out) = go(&d, &m, &reset).await;
    assert!(
        code == 1 && out.contains("has no snapshot `ready`"),
        "{out}"
    );
    let snap = with("debian", |j| j.flags.snapshot = true);
    assert_eq!(go(&d, &m, &snap).await.0, 0);
    assert_eq!(d.calls(), ["snapshot"]);
    assert_eq!(go(&d, &m, &snap).await.0, 0);
    assert!(d.calls().is_empty(), "kept, not saved again");
    m.answer = true;
    let (code, out) = go(&d, &m, &reset).await;
    assert_eq!(code, 0, "{out}");
    assert_eq!(d.calls(), ["stop", "restore"]);
}

#[tokio::test]
async fn down_stops_and_delete_deletes_only_after_a_yes() {
    let (d, m) = (Daemon::default(), machine(Backend::Qemu));
    let down = with("debian", |j| j.down = true);
    let (code, out) = go(&d, &m, &down).await;
    assert!(
        code == 0 && out.contains("does not exist") && d.calls().is_empty(),
        "{out}"
    );
    go(&d, &m, &job("debian")).await;
    d.calls();
    assert_eq!(go(&d, &m, &down).await.0, 0);
    assert_eq!(d.calls(), ["stop"]);
    let (_, out) = go(&d, &m, &down).await;
    assert!(
        out.contains("already stopped") && d.calls().is_empty(),
        "{out}"
    );
    let delete = with("debian", |j| {
        j.down = true;
        j.flags.delete = true;
    });
    go(&d, &m, &delete).await;
    assert!(m.asked.get() && d.calls().is_empty());
    let yes = with("debian", |j| {
        j.down = true;
        j.flags.delete = true;
        j.yes = true;
    });
    assert_eq!(go(&d, &m, &yes).await.0, 0);
    assert_eq!(d.calls(), ["delete"]);
}

#[tokio::test]
async fn hosts_that_cannot_run_a_template_say_why_and_change_nothing() {
    let cases: Vec<(&str, Machine, &str)> = vec![
        ("amd64", machine(Backend::Qemu), "use `build`"),
        (
            "kvm",
            {
                let mut m = machine(Backend::Qemu);
                m.checks.nested_kvm = Some(false);
                m
            },
            "nested turned off",
        ),
        (
            "kvm",
            {
                let mut m = machine(Backend::Vz);
                if let Some(Ok(c)) = &mut m.checks.apple {
                    c.nested_virtualization = false;
                }
                m
            },
            "M3 or later",
        ),
        (
            "amd64",
            {
                let mut m = machine(Backend::Vz);
                if let Some(Ok(c)) = &mut m.checks.apple {
                    c.rosetta = "not-installed".into();
                }
                m
            },
            "install-rosetta",
        ),
        (
            "debian",
            {
                let mut m = machine(Backend::Qemu);
                m.checks.kvm = Kvm::NoAccess;
                m
            },
            "cannot open /dev/kvm",
        ),
        (
            "debian",
            {
                let mut m = machine(Backend::Qemu);
                m.checks.missing_tools = vec!["cloud-localds".into()];
                m
            },
            "cloud-localds",
        ),
        (
            "debian",
            {
                let mut m = machine(Backend::Qemu);
                m.arch = "aarch64";
                m
            },
            "not supported here",
        ),
        (
            "debian",
            {
                let mut m = machine(Backend::Vz);
                m.checks.disk_free_gib = Some(2);
                m
            },
            "2 GiB free",
        ),
        (
            "debian",
            {
                let mut m = machine(Backend::Vz);
                m.server = Some("http://10.0.0.5:7788".into());
                m
            },
            "another machine",
        ),
    ];
    for (t, m, why) in cases {
        let d = Daemon::default();
        let (code, out) = go(&d, &m, &job(t)).await;
        assert_eq!(code, 1, "{why}: {out}");
        assert!(
            out.contains("[blocked]") && out.contains(why),
            "{why}: {out}"
        );
        assert!(d.calls().is_empty(), "{why}");
    }
    let mut m = machine(Backend::Qemu);
    m.checks.virtiofsd = false;
    m.checks.busy_ports = vec![8080];
    let j = with("debian", |j| {
        j.flags.share = vec![".:/src".into()];
        j.flags.publish = vec!["8080:80".into()];
    });
    let (code, out) = go(&Daemon::default(), &m, &j).await;
    assert!(
        code == 1
            && out.contains("virtiofsd")
            && out.contains("already in use on this machine: 8080"),
        "{out}"
    );
}

#[tokio::test]
async fn a_loopback_daemon_runs_it_and_the_commands_name_the_server() {
    let (d, mut m) = (Daemon::default(), machine(Backend::Qemu));
    m.server = Some("http://127.0.0.1:17788".into());
    let (code, out) = go(&d, &m, &job("debian")).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("fluxctl --server http://127.0.0.1:17788 ssh debian"),
        "{out}"
    );
}

#[tokio::test]
async fn shares_and_private_networks_become_checks() {
    let (d, m) = (Daemon::default(), machine(Backend::Vz));
    let j = with("debian", |j| {
        j.flags.share = vec![".:/src:ro".into()];
        j.flags.network = vec!["lab".into()];
    });
    let (code, out) = go(&d, &m, &j).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("private network lab") && out.contains("[ok]   shared folder /src is mounted")
    );
    // The address the daemon assigns is checked once it is in the record.
    let r = json!({"apple": {"networks": [{"name": "lab", "address": "10.89.0.10/24"}]}});
    let c = record_checks(&r);
    assert!(c[0].command.contains("10.89.0.10/24"));
    let linux = machine(Backend::Qemu);
    let mut out = Vec::new();
    let e = run(
        &d,
        &linux,
        &j,
        Path::new("/tmp"),
        Path::new("/tmp"),
        &mut out,
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("only the macOS (vz) backend"), "{e}");
}

#[test]
fn only_this_machine_counts_as_loopback() {
    for s in [
        "http://127.0.0.1:7788",
        "http://localhost:7788/",
        "https://[::1]:7788",
        "127.0.0.1:7788",
    ] {
        assert!(is_loopback(s), "{s}");
    }
    for s in [
        "http://10.0.0.1:7788",
        "http://mini:7788",
        "http://127.0.0.1.example.com",
        "http://localhost@evil:1",
    ] {
        assert!(!is_loopback(s), "{s}");
    }
}

#[test]
fn down_takes_only_its_own_flags() {
    let f = VmFlags {
        vm: Some("x".into()),
        delete: true,
        ..Default::default()
    };
    assert!(f.check_for(true).is_ok());
    assert!(f.check_for(false).is_err());
    let f = VmFlags {
        recreate: true,
        ..Default::default()
    };
    assert!(f.check_for(true).is_err());
}
