// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The plan of `fluxctl templates run|down` for a VM environment: from what exists and what the host can do, decide the
//! one action and say it in plain words. Nothing here touches anything, so `--dry-run` is this plan and no more.

use crate::vm_env::{Check, EnvTemplate, MARKER_PATH, owner};
use crate::vm_net::Backend;
use fluxvm_apple::AppleHostCapabilities;
use fluxvm_core::up::{Kvm, StepState};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// The snapshot `--snapshot` saves and `--reset` goes back to.
pub const READY_SNAPSHOT: &str = "ready";
/// A built-in cloud image takes about this much once downloaded; less free space than this stops a create.
const MIN_FREE_GIB: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    Cached,
    Download,
    /// Not a built-in name on this host: a path or a catalog name the daemon resolves.
    Other,
}

/// What this process can see of the machine that will run the VM.
#[derive(Debug, Clone)]
pub struct HostChecks {
    /// `vz`: what the runner says, or why it could not be asked.
    pub apple: Option<Result<AppleHostCapabilities, String>>,
    pub kvm: Kvm,
    /// QEMU: whether the KVM module lets guests run their own VMs.
    pub nested_kvm: Option<bool>,
    pub missing_tools: Vec<String>,
    pub virtiofsd: bool,
    pub disk_free_gib: Option<u64>,
    pub mem_total_mib: Option<u64>,
    pub image: ImageState,
    pub busy_ports: Vec<u16>,
    /// `vz`: why the same validation a create runs refuses this request.
    pub invalid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HostFacts {
    pub backend: Backend,
    pub os: String,
    pub arch: String,
    /// `--server` / the context in use.
    pub server: Option<String>,
    /// `server` is another machine.
    pub remote: bool,
    /// `None` when a daemon runs the VM: it judges its own host at create time.
    pub host: Option<HostChecks>,
}

/// A VM that already has the name this run would use.
#[derive(Debug, Clone)]
pub struct Existing {
    pub id: Uuid,
    /// `running`, `stopped`, `paused`, `creating` or `failed`.
    pub status: String,
    pub record: Value,
    pub has_ready_snapshot: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Wants {
    pub down: bool,
    pub delete: bool,
    pub recreate: bool,
    pub reset: bool,
    pub snapshot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Recreate,
    Start,
    Resume,
    /// Already running: only check readiness.
    Check,
    Reset,
    Stop,
    Delete,
    Nothing,
}

impl Action {
    pub fn destroys(self) -> bool {
        matches!(self, Action::Recreate | Action::Reset | Action::Delete)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub state: StepState,
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub template: String,
    pub vm: String,
    pub action: Action,
    pub blocked: bool,
    pub headline: String,
    /// The VM as it will be (create) or as it is (anything else), label and value.
    pub summary: Vec<(String, String)>,
    pub steps: Vec<Step>,
    /// What "ready" means, empty for `down`.
    pub checks: Vec<Check>,
}

fn step(state: StepState, title: &str, detail: impl Into<String>) -> Step {
    Step {
        state,
        title: title.into(),
        detail: detail.into(),
        fix: None,
    }
}

fn blocked(title: &str, detail: impl Into<String>, fix: impl Into<String>) -> Step {
    Step {
        fix: Some(fix.into()),
        ..step(StepState::Blocked, title, detail)
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// The user's forwards: everything but the loopback SSH forward a QEMU guest is given at create time (appended after
/// the others, with a host port in the range `vm_net` picks from).
fn published(request: &Value) -> Vec<String> {
    let forwards: &[Value] = request["network"]["forwards"]
        .as_array()
        .map_or(&[], Vec::as_slice);
    let automatic = |f: &Value| {
        request["backend"] == "qemu"
            && f["guest_port"] == 22
            && f["host_addr"] == "127.0.0.1"
            && f["host_port"]
                .as_u64()
                .is_some_and(|p| (22000..23000).contains(&p))
    };
    let own = match forwards.last() {
        Some(last) if automatic(last) => &forwards[..forwards.len() - 1],
        _ => forwards,
    };
    own.iter()
        .filter_map(|f| {
            let (h, g) = (f["host_port"].as_u64()?, f["guest_port"].as_u64()?);
            let addr = f["host_addr"].as_str().unwrap_or("127.0.0.1");
            Some(format!("{addr}:{h} -> {g}"))
        })
        .collect()
}

fn shares(request: &Value) -> Vec<String> {
    request["shared_folders"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            format!(
                "{} -> {}{}",
                s["host_path"].as_str().unwrap_or("?"),
                s["guest_path"].as_str().unwrap_or("?"),
                if s["read_only"] == true { " (ro)" } else { "" }
            )
        })
        .collect()
}

fn networks(request: &Value) -> Vec<String> {
    request["apple"]["networks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["name"].as_str().map(str::to_owned))
        .collect()
}

fn or_none(v: Vec<String>) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

/// The rows describing a VM request (the one to be sent, or the one a record holds).
pub fn summarize(request: &Value, image: &str) -> Vec<(String, String)> {
    let n = |k: &str| {
        request[k]
            .as_u64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    };
    let ci = &request["cloud_init"];
    let mut rows = vec![
        (
            "Backend".to_owned(),
            request["backend"].as_str().unwrap_or("?").to_owned(),
        ),
        ("Login".into(), ci["user"].as_str().unwrap_or("?").into()),
        ("Image".into(), image.to_owned()),
        (
            "Size".into(),
            format!(
                "{} CPUs, {} MiB memory, {} GiB disk (sparse)",
                n("vcpus"),
                n("memory_mib"),
                n("disk_size_gib")
            ),
        ),
        ("Ports".into(), or_none(published(request))),
        ("Shares".into(), or_none(shares(request))),
    ];
    let mut apple = Vec::new();
    if request["apple"]["rosetta"] == true {
        apple.push("Rosetta (x86-64 programs)".to_owned());
    }
    if request["apple"]["nested_virtualization"] == true {
        apple.push("nested virtualization".to_owned());
    }
    apple.extend(
        networks(request)
            .iter()
            .map(|n| format!("private network {n}")),
    );
    if !apple.is_empty() {
        rows.push(("Apple".into(), apple.join(", ")));
    }
    rows.push(("Packages".into(), or_none(strings(&ci["packages"]))));
    let run = strings(&ci["runcmd"]);
    if !run.is_empty() {
        rows.push(("First boot".into(), run.join("; ")));
    }
    let files: Vec<String> = ci["write_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["path"].as_str())
        .filter(|p| *p != MARKER_PATH)
        .map(str::to_owned)
        .collect();
    if !files.is_empty() {
        rows.push(("Files".into(), files.join(", ")));
    }
    rows
}

/// What differs between an existing VM and what this command line asks for, in words.
fn differences(have: &Value, want: &Value) -> Vec<String> {
    let mut d = Vec::new();
    for (key, label) in [
        ("vcpus", "CPUs"),
        ("memory_mib", "memory MiB"),
        ("disk_size_gib", "disk GiB"),
    ] {
        if have[key] != want[key] {
            d.push(format!("{label} {} -> {}", have[key], want[key]));
        }
    }
    if published(have) != published(want) {
        d.push(format!(
            "ports {} -> {}",
            or_none(published(have)),
            or_none(published(want))
        ));
    }
    if shares(have) != shares(want) {
        d.push(format!(
            "shares {} -> {}",
            or_none(shares(have)),
            or_none(shares(want))
        ));
    }
    if networks(have) != networks(want) {
        d.push(format!(
            "private networks {} -> {}",
            or_none(networks(have)),
            or_none(networks(want))
        ));
    }
    if strings(&have["cloud_init"]["packages"]) != strings(&want["cloud_init"]["packages"]) {
        d.push("packages".into());
    }
    if d.is_empty() {
        d.push("the image, first-boot commands, login or SSH key".into());
    }
    d
}

pub struct Input<'a> {
    pub template: &'a EnvTemplate,
    pub vm: &'a str,
    /// `vm_env::definition` for this command line.
    pub definition: &'a Value,
    /// Every VM with this name.
    pub existing: &'a [Existing],
    pub wants: Wants,
    pub facts: &'a HostFacts,
    pub checks: Vec<Check>,
}

fn platform_steps(i: &Input, creating: bool, out: &mut Vec<Step>) {
    let f = i.facts;
    let supported = match f.backend {
        Backend::Vz => f.arch == "aarch64",
        Backend::Qemu => f.os == "linux" && f.arch == "x86_64",
    };
    if !supported {
        out.push(blocked(
            "This machine",
            format!("{} {}: VM environments are not supported here", f.os, f.arch),
            "use an Apple silicon Mac (vz) or an x86-64 Linux host with KVM (QEMU); the QEMU backend builds an x86 q35 machine",
        ));
        return;
    }
    if f.remote {
        out.push(blocked(
            "Server",
            format!(
                "{} is another machine: readiness is checked over SSH to an address only the VM's own host can reach",
                f.server.as_deref().unwrap_or("the server")
            ),
            "run the command on that machine",
        ));
    }
    out.push(step(
        StepState::Done,
        "This machine",
        format!("{} {}, backend {}", f.os, f.arch, f.backend.name()),
    ));
    if let Some(why) = i.template.unsupported_on(f.backend, &f.arch)
        && creating
    {
        out.push(blocked(
            "Template",
            why,
            "pick another: `fluxctl templates ls`",
        ));
    }
}

/// Checks of the host itself. `creating` adds what only a new VM needs (space, memory, ports, image, validation).
fn host_steps(i: &Input, creating: bool, out: &mut Vec<Step>) {
    let Some(h) = &i.facts.host else {
        out.push(step(
            StepState::Done,
            "Daemon",
            format!(
                "{} runs the VM and checks its own host when it creates it",
                i.facts.server.as_deref().unwrap_or("the server")
            ),
        ));
        return;
    };
    let t = i.template;
    match i.facts.backend {
        Backend::Vz => match &h.apple {
            Some(Err(e)) => out.push(blocked(
                "Apple Virtualization",
                format!("the FluxVM runner could not be asked: {e}"),
                "`fluxctl vz doctor` names what is missing",
            )),
            Some(Ok(c)) => {
                if t.rosetta {
                    match c.rosetta.as_str() {
                        "installed" => out.push(step(StepState::Done, "Rosetta", "installed")),
                        "" => out.push(step(
                            StepState::Warn,
                            "Rosetta",
                            "this runner does not say whether Rosetta is installed",
                        )),
                        other => out.push(blocked(
                            "Rosetta",
                            format!("Rosetta for Linux is {other}"),
                            "`fluxvm-vz-runner install-rosetta`",
                        )),
                    }
                }
                if t.nested_virtualization {
                    if c.nested_virtualization {
                        out.push(step(StepState::Done, "Nested virtualization", "supported"));
                    } else {
                        out.push(blocked(
                            "Nested virtualization",
                            "this Mac does not support it",
                            "it needs an Apple M3 or later and macOS 15 or later",
                        ));
                    }
                }
            }
            None => {}
        },
        Backend::Qemu => {
            match h.kvm {
                Kvm::Missing => out.push(blocked(
                    "KVM",
                    "/dev/kvm is not there",
                    "turn on virtualization in the firmware; `fluxctl explain NO_KVM`",
                )),
                Kvm::NoAccess => out.push(blocked(
                    "KVM",
                    "this user cannot open /dev/kvm",
                    "join the kvm group, or use the daemon with --server http://127.0.0.1:7788",
                )),
                _ => out.push(step(StepState::Done, "KVM", "/dev/kvm is usable")),
            }
            if !h.missing_tools.is_empty() {
                out.push(blocked(
                    "Programs",
                    format!("not installed: {}", h.missing_tools.join(", ")),
                    "install QEMU, qemu-img and cloud-localds (package cloud-image-utils)",
                ));
            }
            if creating && !shares(i.definition).is_empty() && !h.virtiofsd {
                out.push(blocked(
                    "Shared folders",
                    "virtiofsd is not installed",
                    "install virtiofsd (or set virtiofsd_binary), or leave --share out",
                ));
            }
            if t.nested_virtualization && h.nested_kvm == Some(false) {
                out.push(blocked(
                    "Nested virtualization",
                    "the host's KVM module has nested turned off",
                    "turn on the `nested` option of kvm_intel or kvm_amd on the host",
                ));
            }
        }
    }
    if !creating {
        return;
    }
    if let Some(e) = &h.invalid {
        out.push(blocked(
            "Request",
            e.clone(),
            "change the template or options",
        ));
    }
    let disk = i.definition["disk_size_gib"].as_u64().unwrap_or(0);
    match h.disk_free_gib {
        Some(free) if free < MIN_FREE_GIB => out.push(blocked(
            "Disk space",
            format!("{free} GiB free; at least {MIN_FREE_GIB} GiB is needed"),
            "free some space: `fluxctl df`, `fluxctl prune`",
        )),
        Some(free) if free < disk => out.push(step(
            StepState::Warn,
            "Disk space",
            format!("{free} GiB free for a sparse {disk} GiB disk: it can fill the volume"),
        )),
        Some(free) => out.push(step(
            StepState::Done,
            "Disk space",
            format!("{free} GiB free"),
        )),
        None => {}
    }
    let mem = i.definition["memory_mib"].as_u64().unwrap_or(0);
    if let Some(total) = h.mem_total_mib {
        if mem > total {
            out.push(blocked(
                "Memory",
                format!("the VM asks for {mem} MiB and the host has {total} MiB"),
                "use --memory-mib",
            ));
        } else if mem > total / 2 {
            out.push(step(
                StepState::Warn,
                "Memory",
                format!(
                    "the VM takes {mem} MiB of the host's {total} MiB (--memory-mib lowers it)"
                ),
            ));
        }
    }
    if !h.busy_ports.is_empty() {
        let p: Vec<String> = h.busy_ports.iter().map(u16::to_string).collect();
        out.push(blocked(
            "Ports",
            format!("already in use on this machine: {}", p.join(", ")),
            "pick other host ports with -p HOST:GUEST",
        ));
    }
    let image = &i.template.image;
    out.push(match h.image {
        ImageState::Cached => step(StepState::Done, "Image", format!("{image} is cached")),
        ImageState::Download => step(
            StepState::Todo,
            "Image",
            format!("download {image} from its vendor and verify its checksum"),
        ),
        ImageState::Other => step(
            StepState::Warn,
            "Image",
            format!(
                "{image} is not a built-in image here: it must be a file or a name in your catalog"
            ),
        ),
    });
}

/// Judges one run. At most one VM may carry the name, and only one this environment made is ever touched.
pub fn assess(i: &Input) -> Plan {
    let (t, vm, w) = (i.template, i.vm, i.wants);
    let mut steps = Vec::new();
    let mine = match i.existing {
        [] => Ok(None),
        [e] => match owner(&e.record) {
            Some((name, hash)) if name == t.name => Ok(Some((e, hash))),
            Some((name, _)) => Err(format!(
                "VM {vm} was made from the template {name:?}, not {:?}",
                t.name
            )),
            None => Err(format!(
                "a VM named {vm} exists and was not made by `fluxctl templates`; it is left alone"
            )),
        },
        many => Err(format!("{} VMs are named {vm}", many.len())),
    };
    let existing = mine.as_ref().ok().and_then(|m| m.as_ref());
    let action = match existing {
        _ if w.down => match existing {
            Some(_) if w.delete => Action::Delete,
            Some((e, _)) if e.status == "running" || e.status == "paused" => Action::Stop,
            _ => Action::Nothing,
        },
        None => Action::Create,
        Some(_) if w.recreate => Action::Recreate,
        Some(_) if w.reset => Action::Reset,
        Some((e, _)) => match e.status.as_str() {
            "running" => Action::Check,
            "paused" => Action::Resume,
            _ => Action::Start,
        },
    };
    let creating = matches!(action, Action::Create | Action::Recreate) && mine.is_ok();
    if !w.down {
        platform_steps(i, creating, &mut steps);
        if !steps
            .iter()
            .any(|s| s.title == "This machine" && s.state == StepState::Blocked)
        {
            host_steps(i, creating, &mut steps);
        }
    }
    match &mine {
        Err(why) => steps.push(blocked(
            "VM",
            why.clone(),
            "use another name with --vm NAME",
        )),
        Ok(None) if w.down => {
            steps.push(step(StepState::Done, "VM", format!("{vm} does not exist")))
        }
        Ok(None) if w.reset => steps.push(blocked(
            "VM",
            format!("{vm} does not exist, so there is nothing to reset"),
            "run it without --reset",
        )),
        Ok(None) => steps.push(step(StepState::Todo, "VM", format!("create {vm}"))),
        Ok(Some((e, hash))) => {
            let status = e.status.as_str();
            if status == "creating" && !w.down {
                steps.push(blocked(
                    "VM",
                    format!("{vm} is still being created"),
                    format!("wait, or delete it: `fluxctl delete {vm}`"),
                ));
            }
            let (state, text) = match action {
                Action::Check => (
                    StepState::Done,
                    format!("{vm} is running; nothing to change"),
                ),
                Action::Start if status == "failed" => (
                    StepState::Warn,
                    format!("{vm} failed earlier; try to start it (--recreate starts over)"),
                ),
                Action::Start => (StepState::Todo, format!("start {vm} ({status})")),
                Action::Resume => (StepState::Todo, format!("resume {vm}")),
                Action::Recreate => (
                    StepState::Todo,
                    format!("delete {vm} AND ITS DISK, then create it again"),
                ),
                Action::Reset => (
                    StepState::Todo,
                    format!(
                        "put {vm} back to the snapshot `{READY_SNAPSHOT}`: everything written since is LOST"
                    ),
                ),
                Action::Stop => (StepState::Todo, format!("stop {vm}; its disk is kept")),
                Action::Delete => (StepState::Todo, format!("delete {vm} AND ITS DISK")),
                Action::Nothing => (StepState::Done, format!("{vm} is already {status}")),
                Action::Create => (StepState::Todo, format!("create {vm}")),
            };
            steps.push(step(state, "VM", text));
            if !w.down && !w.recreate && *hash != crate::stack::spec_hash(i.definition) {
                steps.push(step(
                    StepState::Warn,
                    "Definition",
                    format!(
                        "{vm} differs from this command ({}); it is kept as it is. --recreate rebuilds it and deletes its disk",
                        differences(&e.record["request"], i.definition).join("; ")
                    ),
                ));
            }
            if w.reset && !e.has_ready_snapshot {
                steps.push(blocked(
                    "Snapshot",
                    format!("{vm} has no snapshot `{READY_SNAPSHOT}`"),
                    "save one first with --snapshot",
                ));
            }
        }
    }
    if w.snapshot && !w.down && mine.is_ok() {
        let kept = !w.recreate && existing.is_some_and(|(e, _)| e.has_ready_snapshot);
        steps.push(if kept {
            step(
                StepState::Done,
                "Snapshot",
                format!("`{READY_SNAPSHOT}` exists and is kept"),
            )
        } else {
            step(
                StepState::Todo,
                "Snapshot",
                format!("once ready, save memory and disk as `{READY_SNAPSHOT}`"),
            )
        });
    }
    let summary = match existing {
        Some((e, _)) if !matches!(action, Action::Recreate) => {
            summarize(&e.record["request"], &t.image)
        }
        _ if mine.is_ok() && !w.down => summarize(i.definition, &t.image),
        _ => Vec::new(),
    };
    let is_blocked = steps.iter().any(|s| s.state == StepState::Blocked);
    let what = match action {
        _ if is_blocked => format!("cannot run VM {vm}"),
        Action::Create => format!("create VM {vm}"),
        Action::Recreate => format!("recreate VM {vm}"),
        Action::Start => format!("start VM {vm}"),
        Action::Resume => format!("resume VM {vm}"),
        Action::Check => format!("check VM {vm}, which is running"),
        Action::Reset => format!("reset VM {vm}"),
        Action::Stop => format!("stop VM {vm}"),
        Action::Delete => format!("delete VM {vm}"),
        Action::Nothing => format!("nothing to do for VM {vm}"),
    };
    Plan {
        template: t.name.clone(),
        vm: vm.to_owned(),
        action,
        blocked: is_blocked,
        headline: format!("{}: {what}", t.title),
        summary,
        steps,
        checks: if w.down { Vec::new() } else { i.checks.clone() },
    }
}

fn mark(s: StepState) -> &'static str {
    match s {
        StepState::Done => "[ok]     ",
        StepState::Todo => "[to do]  ",
        StepState::Warn => "[warn]   ",
        StepState::Blocked => "[blocked]",
    }
}

pub fn render(p: &Plan) -> String {
    let mut s = format!("{}\n", p.headline);
    if !p.summary.is_empty() {
        s.push('\n');
        for (k, v) in &p.summary {
            s.push_str(&format!("  {k:<11} {v}\n"));
        }
    }
    if !p.checks.is_empty() {
        s.push_str("\n  Ready when SSH answers, cloud-init has finished, and:\n");
        for c in &p.checks {
            s.push_str(&format!("    - {}\n", c.name));
        }
    }
    s.push('\n');
    for st in &p.steps {
        s.push_str(&format!(
            "  {} {}: {}\n",
            mark(st.state),
            st.title,
            st.detail
        ));
        if let Some(f) = &st.fix {
            s.push_str(&format!("            fix: {f}\n"));
        }
    }
    s
}
