//! `GET /v1/openapi.json`: OpenAPI 3.1 description of the VM lifecycle routes.

use serde_json::{Map, Value, json};

struct Op {
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    tag: &'static str,
    body: Option<&'static str>,
    response: Option<&'static str>,
    status: &'static str,
    query: &'static [(&'static str, &'static str)],
}

const fn op(
    method: &'static str,
    path: &'static str,
    summary: &'static str,
    tag: &'static str,
) -> Op {
    Op {
        method,
        path,
        summary,
        tag,
        body: None,
        response: None,
        status: "200",
        query: &[],
    }
}

impl Op {
    const fn body(mut self, schema: &'static str) -> Self {
        self.body = Some(schema);
        self
    }
    const fn returns(mut self, schema: &'static str) -> Self {
        self.response = Some(schema);
        self
    }
    const fn status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }
    const fn query(mut self, q: &'static [(&'static str, &'static str)]) -> Self {
        self.query = q;
        self
    }
}

const VMS: &str = "vms";
const DISKS: &str = "disks";
const SNAPSHOTS: &str = "snapshots";
const EVENTS: &str = "events";
const TEMPLATES: &str = "vm-templates";
const HOST: &str = "host";
const NETOBS: &str = "netobs";
const NETWORK: &str = "network";

fn ops() -> Vec<Op> {
    vec![
        op("get", "/healthz", "Liveness probe (no auth)", HOST),
        op("get", "/readyz", "Readiness probe (no auth)", HOST),
        op(
            "get",
            "/v1/quotas/me",
            "Caller token's quota limits and usage",
            HOST,
        ),
        op("get", "/v1/vms", "List VMs", VMS)
            .returns("VmList")
            .query(&[
                ("name", "Exact VM name"),
                ("tenant", "Tenant id"),
                ("label", "Label selector, e.g. env=prod,!tmp"),
            ]),
        op("post", "/v1/vms", "Create a VM", VMS)
            .body("CreateVmRequest")
            .returns("VmRecord")
            .status("201"),
        op("get", "/v1/vms/{id}", "Get a VM", VMS).returns("VmRecord"),
        op(
            "patch",
            "/v1/vms/{id}",
            "Rename and/or edit labels (null value removes)",
            VMS,
        )
        .body("VmPatch")
        .returns("VmRecord"),
        op("delete", "/v1/vms/{id}", "Delete a VM", VMS).status("204"),
        op("post", "/v1/vms/{id}/start", "Start a VM", VMS).returns("VmRecord"),
        op("post", "/v1/vms/{id}/stop", "Stop a VM", VMS).returns("VmRecord"),
        op(
            "post",
            "/v1/vms/{id}/hibernate",
            "Save RAM/CPU/device state to the hibernation directory and stop QEMU; the next start resumes",
            VMS,
        )
        .returns("VmRecord"),
        op(
            "delete",
            "/v1/vms/{id}/hibernation",
            "Discard a hibernated VM's saved state; the next start boots from disk",
            VMS,
        )
        .returns("VmRecord"),
        op(
            "post",
            "/v1/vms/import-hibernated",
            "Register a VM hibernated on another node from the shared hibernation directory",
            VMS,
        )
        .returns("VmRecord"),
        op(
            "post",
            "/v1/vms/{id}/diagnostics",
            "Start an incident bundle job: QMP state, cgroup and PSI, console log, network and eBPF state, optional pcap and guest memory dump",
            VMS,
        )
        .body("DiagnosticsRequest")
        .returns("DiagnosticsBundle")
        .status("202"),
        op("get", "/v1/vms/{id}/diagnostics", "Diagnostics bundles kept for a VM", VMS)
            .returns("DiagnosticsBundleList"),
        op(
            "get",
            "/v1/vms/{id}/diagnostics/{token}",
            "One diagnostics bundle job",
            VMS,
        )
        .returns("DiagnosticsBundle"),
        op(
            "delete",
            "/v1/vms/{id}/diagnostics/{token}",
            "Delete a finished bundle and its memory dump",
            VMS,
        )
        .status("204"),
        op(
            "get",
            "/v1/vms/{id}/diagnostics/{token}/bundle",
            "Download a finished bundle (tar.gz); 409 while it is collected",
            VMS,
        ),
        op(
            "get",
            "/v1/vms/{id}/diagnostics/{token}/memory-dump",
            "Download a bundle's guest memory dump (ELF)",
            VMS,
        ),
        op("post", "/v1/vms/{id}/restart", "Stop then start a VM", VMS).returns("VmRecord"),
        op("post", "/v1/vms/{id}/pause", "Pause vCPUs", VMS).returns("VmRecord"),
        op("post", "/v1/vms/{id}/resume", "Resume vCPUs", VMS).returns("VmRecord"),
        op("post", "/v1/vms/{id}/clone", "Clone a stopped VM", VMS)
            .body("CloneVmRequest")
            .returns("VmRecord")
            .status("201"),
        op(
            "post",
            "/v1/vms/{id}/fork",
            "Fork a running flux-vm VM into N running copies",
            VMS,
        )
        .body("ForkVmRequest")
        .returns("VmList")
        .status("201"),
        op(
            "post",
            "/v1/vms/{id}/backup",
            "Export the root disk to state_dir/backups",
            VMS,
        )
        .body("BackupVmRequest")
        .returns("BackupResult")
        .status("201"),
        op(
            "post",
            "/v1/vms/{id}/restore-backup",
            "Restore a backup into the stopped VM in place",
            VMS,
        )
        .body("RestoreBackupRequest")
        .returns("RestoreBackupResult"),
        op(
            "get",
            "/v1/backups",
            "Backups under state_dir/backups, newest first",
            VMS,
        )
        .returns("BackupList"),
        op("delete", "/v1/backups/{name}", "Delete a backup", VMS).status("204"),
        op(
            "get",
            "/v1/backups/{name}/root",
            "Download a backup's root qcow2",
            VMS,
        ),
        op(
            "post",
            "/v1/vms/{id}/cbt-backup",
            "Start a full or incremental (dirty-bitmap) backup to S3",
            VMS,
        )
        .body("CbtBackupRequest")
        .returns("CbtJob")
        .status("202"),
        op(
            "post",
            "/v1/vms/{id}/cbt-restore",
            "Restore a CBT chain point into the stopped VM",
            VMS,
        )
        .body("CbtRestoreRequest")
        .returns("CbtJob")
        .status("202"),
        op(
            "post",
            "/v1/cbt/synthesize",
            "Consolidate a CBT chain into a synthetic full",
            VMS,
        )
        .body("CbtSynthesizeRequest")
        .returns("CbtJob")
        .status("202"),
        op("post", "/v1/cbt/delete", "Delete CBT backups by manifest", VMS)
            .body("CbtDeleteRequest")
            .returns("CbtDeleteResult"),
        op("get", "/v1/cbt-jobs/{id}", "Poll a CBT backup, restore or synthesize job", VMS)
            .returns("CbtJob"),
        op(
            "get",
            "/v1/vms/{id}/logs",
            "Console log (text; ?follow=true streams)",
            VMS,
        ),
        op(
            "get",
            "/v1/vms/{id}/serial",
            "Serial console websocket (raw bytes)",
            VMS,
        ),
        op(
            "get",
            "/v1/vms/{id}/tunnel",
            "TCP tunnel websocket to a guest port (?port=N; binary messages; fleet tunnels)",
            VMS,
        ),
        op(
            "get",
            "/v1/vms/{id}/console",
            "Guest-agent PTY websocket",
            VMS,
        ),
        op(
            "post",
            "/v1/vms/{id}/snapshot",
            "Save an internal snapshot",
            SNAPSHOTS,
        )
        .body("SnapshotRequest"),
        op("get", "/v1/vms/{id}/snapshots", "List snapshots", SNAPSHOTS).returns("SnapshotList"),
        op(
            "get",
            "/v1/host/overview",
            "One-call dashboard: daemon, host memory/CPU, VM counts, per-VM runner usage, helper availability",
            HOST,
        )
        .returns("HostOverview"),
        op(
            "post",
            "/v1/vms/{id}/fork/plan",
            "What a fork would be refused for and cost (memory bounds, estimated time), without doing it",
            HOST,
        ),
        op(
            "post",
            "/v1/branches/{id}/start/plan",
            "What a start from a branch would be refused for and cost, without doing it",
            HOST,
        ),
        op(
            "get",
            "/v1/vms/{id}/usernet/policy",
            "The egress policy of an apple.usernet guest as stored",
            HOST,
        ),
        op(
            "put",
            "/v1/vms/{id}/usernet/policy",
            "Change a usernet guest's egress policy; applied to the running gateway without cutting open connections",
            HOST,
        ),
        op(
            "get",
            "/v1/host/storage",
            "Where disk space goes: volume free space and per-VM, per-branch and image-cache allocated bytes",
            HOST,
        ),
        op(
            "get",
            "/v1/host/images",
            "Built-in images that already have a ready copy on this host (the fleet scheduler's image-presence signal)",
            HOST,
        ),
        op(
            "get",
            "/v1/host/history",
            "Last hour of host memory and per-VM runner usage at 5 s spacing (in memory; empty after a daemon restart)",
            HOST,
        ),
        op(
            "get",
            "/v1/stats",
            "One sample of the host (CPU ticks, memory, disk free, VM counts) and every VM (CPU seconds, memory, disk and network byte counters) for the fleet metrics collector",
            HOST,
        ),
        op(
            "get",
            "/v1/host/memory",
            "Host memory now: MemTotal, MemAvailable and memory PSI some/full avg10",
            HOST,
        ),
        op(
            "get",
            "/v1/vms/{id}/guard",
            "VMM Guard policy applied to the VM: mode, exec allow-list, applied rule packs (id, CVEs, valid_until, signer) and expired_packs",
            VMS,
        ),
        op(
            "get",
            "/v1/host/pressure",
            "Whole-host PSI now from /proc/pressure: cpu_some, memory_some/full, io_some/full (avg10, avg60, avg300, total), same shape as /v1/vms/{id}/pressure",
            HOST,
        ),
        op(
            "get",
            "/v1/host/hibernation",
            "QEMU version, q35 machine type, CPU vendor and flags a hibernated state depends on",
            HOST,
        ),
        op(
            "post",
            "/v1/host/hibernation/check",
            "Whether a hibernation manifest can be resumed on this host, and why not",
            HOST,
        ),
        op(
            "get",
            "/v1/host/topology",
            "Host hardware graph: NUMA nodes (CPUs, memory, hugepages) and placeable PCI devices with NUMA node, PCIe root and IOMMU group",
            HOST,
        ),
        op(
            "get",
            "/v1/host/gpus/health",
            "Per-GPU health: config-space response, PCIe AER counters and link width, and nvidia-smi ECC, temperature, memory and MIG mode when installed",
            HOST,
        ),
        op(
            "get",
            "/v1/host/infer",
            "Host OpenAI-compatible model server for apple.host_gpu (reachable, /v1/models)",
            HOST,
        ),
        op(
            "get",
            "/v1/host/mdev",
            "Mediated device (vGPU) parents with their types, free instances, framebuffer and MIG geometry, and the instances that exist",
            HOST,
        ),
        op(
            "post",
            "/v1/host/mdev",
            "Create a mediated device of a type on a parent; idempotent for the same UUID, parent and type (admin)",
            HOST,
        ),
        op(
            "delete",
            "/v1/host/mdev/{uuid}",
            "Remove a mediated device no VM lists and no process holds; absent is success (admin)",
            HOST,
        ),
        op(
            "post",
            "/v1/host/pci/bind",
            "Bind a PCI function's whole IOMMU group to vfio-pci (admin)",
            HOST,
        ),
        op(
            "post",
            "/v1/host/pci/release",
            "Release a PCI function's IOMMU group, optionally restoring the host driver (admin)",
            HOST,
        ),
        op(
            "get",
            "/v1/vms/{id}/numa",
            "Live NUMA placement of a QEMU VM: guest nodes, vCPU thread affinity, memory backend binding",
            VMS,
        ),
        op(
            "get",
            "/v1/host/apple/capabilities",
            "The capability table judged against this Mac (available here, and why not)",
            HOST,
        )
        .returns("CapabilityReport"),
        op("get", "/v1/host/apple", "Apple host capabilities (runner probe)", HOST),
        op("get", "/v1/vznets", "Private vz networks and their members", HOST),
        op("get", "/v1/macos-pool", "Enrolled macOS warm-pool slots", VMS).returns("VmList"),
        op("post", "/v1/macos-pool/checkout", "Restore a free macOS pool slot (409 when none is free)", VMS).returns("VmRecord"),
        op("post", "/v1/macos-pool/{id}/enroll", "Make a running, set-up macOS guest a pool slot", VMS).returns("VmRecord"),
        op("post", "/v1/macos-pool/{id}/return", "Stop a leased pool slot", VMS).returns("VmRecord"),
        op("get", "/v1/vms/{id}/balloon", "Memory balloon status", VMS),
        op("post", "/v1/vms/{id}/balloon", "Set the memory balloon target", VMS),
        op(
            "post",
            "/v1/vms/{id}/branches",
            "Checkpoint a running vz guest as a named, durable branch",
            SNAPSHOTS,
        )
        .body("CreateBranchRequest")
        .returns("VmBranch")
        .status("201"),
        op("get", "/v1/vms/{id}/branches", "List a VM's branches", SNAPSHOTS)
            .returns("BranchList"),
        op("get", "/v1/branches", "List all branches", SNAPSHOTS).returns("BranchList"),
        op("get", "/v1/branches/{id}", "Get a branch", SNAPSHOTS).returns("VmBranch"),
        op("delete", "/v1/branches/{id}", "Delete a branch", SNAPSHOTS),
        op(
            "post",
            "/v1/branches/prune",
            "Remove branches by policy (dry run unless dry_run is false)",
            SNAPSHOTS,
        ),
        op(
            "get",
            "/v1/branches/{a}/diff/{b}",
            "Compare two branches: saved sizes, lineage, labels",
            SNAPSHOTS,
        ),
        op(
            "post",
            "/v1/branches/{id}/export",
            "Write a branch to a bundle directory on the host (admin)",
            SNAPSHOTS,
        ),
        op(
            "post",
            "/v1/branches/import",
            "Import an exported bundle as a new branch (admin)",
            SNAPSHOTS,
        )
        .status("201"),
        op(
            "post",
            "/v1/branches/{id}/start",
            "Start new VMs from a branch (the body is the fork spec)",
            SNAPSHOTS,
        )
        .body("ForkVmRequest")
        .returns("VmList")
        .status("201"),
        op(
            "delete",
            "/v1/vms/{id}/snapshots/{tag}",
            "Delete a snapshot",
            SNAPSHOTS,
        )
        .status("204"),
        op(
            "post",
            "/v1/vms/{id}/start-from-snapshot",
            "Start from a snapshot tag",
            SNAPSHOTS,
        )
        .body("SnapshotRequest")
        .returns("VmRecord"),
        op("get", "/v1/vms/{id}/disks", "Root and data disks", DISKS).returns("DiskList"),
        op(
            "post",
            "/v1/vms/{id}/disks",
            "Create a data disk, or attach an existing image/block device (hot-added if running)",
            DISKS,
        )
        .body("AttachDiskRequest")
        .returns("VmDiskInfo")
        .status("201"),
        op(
            "patch",
            "/v1/vms/{id}/disks/{name}",
            "Grow a disk (name `root` for the boot disk)",
            DISKS,
        )
        .body("ResizeDiskRequest")
        .returns("VmDiskInfo"),
        op(
            "delete",
            "/v1/vms/{id}/disks/{name}",
            "Detach a data disk (deletes it unless attached from an existing image)",
            DISKS,
        )
        .status("204"),
        op(
            "put",
            "/v1/vms/{id}/disks/{name}/throttle",
            "Set or clear a disk's IOPS/bandwidth limits (name `root` for the boot disk); live when running",
            DISKS,
        )
        .body("DiskThrottleRequest")
        .returns("DiskThrottleResult"),
        // ---- volumes ----
        op("get", "/v1/volumes", "Persistent volumes on this host (admin)", DISKS).returns("VolumeList"),
        op(
            "post",
            "/v1/volumes",
            "Create a persistent volume: a sparse raw file that outlives VMs (admin)",
            DISKS,
        )
        .body("CreateVolumeRequest")
        .returns("VolumeInfo")
        .status("201"),
        op("get", "/v1/volumes/{name}", "One volume and the VMs using it (admin)", DISKS).returns("VolumeInfo"),
        op(
            "delete",
            "/v1/volumes/{name}",
            "Delete a volume; 409 while a VM references it (admin)",
            DISKS,
        )
        .status("204"),
        op(
            "post",
            "/v1/volumes/{name}/resize",
            "Grow a detached volume (admin)",
            DISKS,
        )
        .body("ResizeDiskRequest")
        .returns("VolumeInfo"),
        op(
            "post",
            "/v1/vms/{id}/cdroms/{name}/eject",
            "Eject install media (live when running); the empty drive stays",
            DISKS,
        )
        .returns("VmRecord"),
        op("get", "/v1/vm-templates", "List VM templates", TEMPLATES).returns("TemplateList"),
        op("post", "/v1/vm-templates", "Save a VM template", TEMPLATES)
            .body("SaveTemplateRequest")
            .returns("VmTemplate")
            .status("201"),
        op(
            "get",
            "/v1/vm-templates/{name}",
            "Get a VM template",
            TEMPLATES,
        )
        .returns("VmTemplate"),
        op(
            "delete",
            "/v1/vm-templates/{name}",
            "Delete a VM template",
            TEMPLATES,
        )
        .status("204"),
        op(
            "post",
            "/v1/vm-templates/{name}/instantiate",
            "Create a VM from a template",
            TEMPLATES,
        )
        .body("InstantiateTemplateRequest")
        .returns("VmRecord")
        .status("201"),
        op(
            "post",
            "/v1/vms/{id}/vz/display",
            "Ask for a new preferred mode on a display of a running vz guest (admin; macOS 14+). The guest follows only when a KMS client switches its active mode; the reply says the request was made, not that the guest changed",
            VMS,
        )
        .body("VzDisplayResize"),
        op(
            "get",
            "/v1/vms/{id}/vz/attestation",
            "Signed boot manifest of the vz guest's last launch and the host public key (a host claim, not hardware attestation)",
            VMS,
        ),
        op("get", "/v1/events", "Lifecycle events", EVENTS)
            .returns("EventList")
            .query(&[
                ("vm", "VM id"),
                ("event", "Event-name prefix"),
                ("since", "RFC 3339 timestamp"),
                ("limit", "Newest N (default 500)"),
            ]),
        op(
            "get",
            "/v1/events/stream",
            "Lifecycle events as Server-Sent Events",
            EVENTS,
        ),
        op(
            "get",
            "/v1/netobs/summary",
            "Guest-network observability: counters, per-VM RED totals, top talkers and errors",
            NETOBS,
        ),
        op("get", "/v1/netobs/flows", "Live (and optionally closed) guest flows", NETOBS)
            .query(&[
                ("vm", "VM id"),
                ("proto", "tcp, udp, icmp, icmpv6"),
                ("port", "Either endpoint port"),
                ("host", "Either endpoint address"),
                ("verdict", "allowed, denied or dropped"),
                ("closed", "Include recently closed flows"),
                ("limit", "Newest N (default 200)"),
            ]),
        op(
            "get",
            "/v1/netobs/flows/stream",
            "Flow, DNS, TLS, HTTP and drop metadata as Server-Sent Events (resume with since or Last-Event-ID)",
            NETOBS,
        )
        .query(&[("vm", "VM id"), ("since", "Resume after this event id")]),
        op("get", "/v1/netobs/dns", "Recent DNS queries and responses", NETOBS)
            .query(&[("vm", "VM id"), ("limit", "Newest N (default 200)")]),
        op(
            "get",
            "/v1/network/vnets",
            "Virtual networks applied on this host (VNI, CIDR, local and remote endpoints)",
            NETWORK,
        ),
        op(
            "get",
            "/v1/network/vnets/{vni}",
            "One virtual network as this host routes it",
            NETWORK,
        ),
        op(
            "put",
            "/v1/network/vnets/{vni}",
            "Apply a virtual network: CIDR, gateway, remote endpoints, uplink, SNAT and floating IPs (admin)",
            NETWORK,
        ),
        op(
            "delete",
            "/v1/network/vnets/{vni}",
            "Remove a virtual network with no VMs attached on this host (admin)",
            NETWORK,
        ),
        op("get", "/v1/netobs/history", "Retained roll-up windows", NETOBS),
        op("get", "/v1/netobs/capture", "Capture sessions", NETOBS),
        op(
            "post",
            "/v1/netobs/capture",
            "Start a filtered, TTL-bound packet capture for a VM (admin)",
            NETOBS,
        ),
        op(
            "delete",
            "/v1/netobs/capture",
            "Stop and discard a VM's capture (admin)",
            NETOBS,
        )
        .query(&[("vm", "VM id")])
        .status("204"),
        op(
            "get",
            "/v1/netobs/capture.pcap",
            "Download a VM's capture ring as pcap (admin)",
            NETOBS,
        )
        .query(&[("vm", "VM id")]),
    ]
}

fn schemas() -> Value {
    let obj =
        |desc: &str| json!({"type": "object", "description": desc, "additionalProperties": true});
    let list = |item: &str| json!({"type": "object", "properties": {"items": {"type": "array", "items": {"$ref": format!("#/components/schemas/{item}")}}}});
    let mut all = json!({
        "VmRecord": obj("A VM: id, name, status, backend, request, labels, workspace, guest_ip, ..."),
        "CreateVmRequest": obj("VM spec: name, backend, image, vcpus, memory_mib, network, cloud_init, ..."),
        "VmList": list("VmRecord"),
        "BranchList": list("VmBranch"),
        "HostOverview": {"type": "object", "properties": {"daemon": {"type": "object"}, "host": {"type": "object"}, "vms": {"type": "object"}, "branches": {"type": "integer"}, "usage": {"type": "array", "items": {"type": "object", "properties": {"vm_id": {"type": "string", "format": "uuid"}, "name": {"type": "string"}, "status": {"type": "string"}, "cpu_seconds_total": {"type": "number"}, "rss_bytes": {"type": "integer"}, "service_attributed": {"type": "boolean"}, "started_at": {"type": "integer", "nullable": true}}}}, "helpers": {"type": "object"}, "networking": {"type": "object"}}},
        "CapabilityReport": {"type": "object", "properties": {"os_version": {"type": "string", "nullable": true}, "probed": {"type": "boolean"}, "items": {"type": "array", "items": {"type": "object", "properties": {"feature": {"type": "string"}, "supported": {"type": "boolean"}, "note": {"type": "string"}, "min_macos": {"type": "integer"}, "available_here": {"type": "boolean", "nullable": true}, "reason": {"type": "string", "nullable": true}}}}}},
        "CreateBranchRequest": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}, "labels": {"type": "object", "additionalProperties": {"type": "string"}}}},
        "VmBranch": {"type": "object", "properties": {"id": {"type": "string", "format": "uuid"}, "name": {"type": "string"}, "source_vm": {"type": "string", "format": "uuid"}, "source_name": {"type": "string"}, "parent_branch": {"type": "string", "format": "uuid", "nullable": true}, "created_at": {"type": "string", "format": "date-time"}, "labels": {"type": "object", "additionalProperties": {"type": "string"}}, "vcpus": {"type": "integer"}, "memory_mib": {"type": "integer"}, "state_bytes": {"type": "integer"}}},
        "VzDisplayResize": {"type": "object", "required": ["width", "height"], "properties": {
            "width": {"type": "integer", "minimum": 800, "maximum": 5120},
            "height": {"type": "integer", "minimum": 600, "maximum": 2880},
            "display": {"type": "integer", "minimum": 0, "maximum": 7, "default": 0},
            "ppi": {"type": "integer", "minimum": 72, "maximum": 300, "description": "macOS guests only"}
        }},
        "VmPatch": {"type": "object", "properties": {
            "name": {"type": "string"},
            "labels": {"type": "object", "additionalProperties": {"type": ["string", "null"]}}
        }},
        "CloneVmRequest": {"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string"},
            "snapshot": {"type": "string", "description": "Clone from this snapshot's disk (vz); the source VM may then be running"}
        }},
        "ForkVmRequest": {"type": "object", "properties": {"count": {"type": "integer", "minimum": 1, "maximum": 32, "default": 1}, "namePrefix": {"type": "string"}, "identity": {"type": "string", "enum": ["preserve", "best-effort", "unique"], "default": "unique", "description": "unique fails the fork (children rolled back) unless the guest confirms hostname, machine-id, entropy, SSH host keys, cloud-init, DHCP and network state; best-effort logs a failed reset and keeps the child; preserve resets nothing"}, "network": {"type": "string", "enum": ["auto", "isolated", "none", "preserve"], "default": "auto"}, "parallelism": {"type": "integer", "minimum": 1, "maximum": 32, "description": "children restored at once (vz); default min(count, 8)"}, "ready": {"description": "running (default), agent, exec, {\"tcp\": port} or {\"http\": {\"port\": n, \"path\": \"/\"}}, probed inside each child"}, "hooks": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}, "description": "argv commands run in each child after its identity reset (needs the guest agent)"}, "ready_timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600, "default": 60}}},
        "BackupVmRequest": {"type": "object", "properties": {
            "name": {"type": "string", "description": "Backup name under state_dir/backups; default <vm>-<utc>"},
            "compress": {"type": "boolean"},
            "all_disks": {"type": "boolean", "description": "Also back up data disks into a directory"},
            "quiesce": {"type": "string", "enum": ["auto", "required", "never"], "default": "auto",
                "description": "Freeze guest filesystems through the guest agent around the snapshot; auto falls back to crash-consistent"}
        }},
        "RestoreBackupRequest": {"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string", "description": "Backup name from GET /v1/backups"}
        }},
        "RestoreBackupResult": {"type": "object", "properties": {
            "vm_id": {"type": "string", "format": "uuid"},
            "backup": {"type": "string"},
            "restored": {"type": "array", "items": {"type": "string"}},
            "skipped": {"type": "array", "items": {"type": "object"}}
        }},
        "BackupList": list("BackupResult"),
        "BackupResult": {"type": "object", "properties": {
            "vm_id": {"type": "string", "format": "uuid"},
            "path": {"type": "string"},
            "size_bytes": {"type": "integer"},
            "live": {"type": "boolean"},
            "disks": {"type": "array", "items": {"type": "object", "properties": {
                "name": {"type": "string"}, "path": {"type": "string"}, "size_bytes": {"type": "integer"}
            }}}
        }},
        "VmTemplate": {"type": "object", "properties": {
            "name": {"type": "string"},
            "description": {"type": ["string", "null"]},
            "created_at": {"type": "string", "format": "date-time"},
            "spec": {"$ref": "#/components/schemas/CreateVmRequest"}
        }},
        "TemplateList": list("VmTemplate"),
        "SaveTemplateRequest": {"type": "object", "required": ["name", "spec"], "properties": {
            "name": {"type": "string"},
            "description": {"type": "string"},
            "spec": {"$ref": "#/components/schemas/CreateVmRequest"},
            "replace": {"type": "boolean"}
        }},
        "InstantiateTemplateRequest": {"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string"},
            "labels": {"type": "object", "additionalProperties": {"type": "string"}}
        }},
        "SnapshotRequest": {"type": "object", "required": ["tag"], "properties": {
            "tag": {"type": "string"},
            "memory": {"type": "boolean", "default": true,
                "description": "false: a disk-only snapshot (vz), which a stopped VM can take too and which restores by booting cold"}
        }},
        "VmSnapshotInfo": {"type": "object", "properties": {
            "tag": {"type": "string"},
            "created_at": {"type": ["string", "null"], "format": "date-time"},
            "size_bytes": {"type": "integer"},
            "has_memory": {"type": "boolean", "description": "Saved memory and device state (resume) or disk only (cold boot); absent when unknown"}
        }},
        "SnapshotList": list("VmSnapshotInfo"),
        "VmDiskInfo": {"type": "object", "properties": {
            "name": {"type": "string"},
            "path": {"type": "string"},
            "bus": {"type": "string", "enum": ["virtio", "scsi", "virtio-blk", "vhost-user-blk"], "description": "virtio is the root disk"},
            "format": {"type": "string"},
            "size_bytes": {"type": "integer"},
            "allocated_bytes": {"type": "integer"},
            "throttle": {"$ref": "#/components/schemas/DiskThrottle"}
        }},
        "DiskList": list("VmDiskInfo"),
        "AttachDiskRequest": {"type": "object", "required": ["name"], "description": "Exactly one of size_gib, path or backing", "properties": {
            "name": {"type": "string", "pattern": "^[a-z0-9][a-z0-9-]{0,31}$"},
            "size_gib": {"type": "integer", "minimum": 1, "description": "Create a new qcow2 disk of this size"},
            "path": {"type": "string", "description": "Attach this existing qcow2/raw file or block device; detach leaves it in place"},
            "backing": {"type": "string", "description": "Create a qcow2 overlay on this qcow2/raw image, which is never written; detach deletes the overlay"},
            "io": {"$ref": "#/components/schemas/DiskIo"}
        }},
        "DiskIo": {"type": "object", "description": "QEMU disk I/O settings; every field is optional", "properties": {
            "bus": {"type": "string", "enum": ["scsi", "virtio-blk", "vhost-user-blk"], "description": "Data disks only; default scsi. vhost-user-blk is served by a per-disk qemu-storage-daemon"},
            "aio": {"type": "string", "enum": ["io_uring", "native", "threads"], "description": "native needs cache none or directsync"},
            "cache": {"type": "string", "enum": ["none", "writeback", "writethrough", "directsync", "unsafe"], "description": "Default writeback"},
            "iothread": {"type": "boolean", "description": "Dedicated iothread (virtio-blk, vhost-user-blk, root)"},
            "queues": {"type": "integer", "minimum": 1, "maximum": 64, "description": "virtio queues (virtio-blk, vhost-user-blk, root)"},
            "throttle": {"$ref": "#/components/schemas/DiskThrottle"}
        }},
        "ResizeDiskRequest": {"type": "object", "required": ["size_gib"], "properties": {
            "size_gib": {"type": "integer", "minimum": 1}
        }},
        "VmEvent": obj("ts, event, vm_id and event-specific fields"),
        "EventList": list("VmEvent"),
        "Error": {"type": "object", "properties": {"error": {"type": "string"}}}
    });
    if let (Some(all), Value::Object(t)) = (all.as_object_mut(), throttle_schemas()) {
        all.extend(t);
    }
    if let (Some(all), Value::Object(cbt)) = (all.as_object_mut(), cbt_schemas()) {
        all.extend(cbt);
    }
    if let (Some(all), Value::Object(vols)) = (all.as_object_mut(), volume_schemas()) {
        all.extend(vols);
    }
    all
}

/// Persistent volumes (`/v1/volumes`), kept apart for the same reason as [`cbt_schemas`].
fn volume_schemas() -> Value {
    json!({
        "VolumeInfo": {"type": "object", "properties": {
            "name": {"type": "string"},
            "path": {"type": "string", "description": "The raw file; attach it as an apple.extra_disks image"},
            "format": {"type": "string", "enum": ["raw"]},
            "size_gib": {"type": "integer"},
            "size_bytes": {"type": "integer"},
            "allocated_bytes": {"type": "integer"},
            "attached_to": {"type": "array", "items": {"type": "string"}}
        }},
        "VolumeList": {"type": "object", "properties": {"dir": {"type": "string"}, "items": {"type": "array", "items": {"$ref": "#/components/schemas/VolumeInfo"}}}},
        "CreateVolumeRequest": {"type": "object", "required": ["name", "size_gib"], "properties": {
            "name": {"type": "string", "pattern": "^[a-z0-9][a-z0-9-]{0,62}$"},
            "size_gib": {"type": "integer", "minimum": 1, "maximum": 16384}
        }}
    })
}

/// Apart from [`schemas`] for the same reason as [`cbt_schemas`].
fn throttle_schemas() -> Value {
    json!({
        "DiskThrottle": {"type": "object", "description": "IOPS and bytes/s limits (scsi, virtio-blk, root). A *_total limit excludes its *_read/*_write pair; *_total_max bursts for burst_seconds", "properties": {
            "iops_total": {"type": "integer", "minimum": 1},
            "iops_read": {"type": "integer", "minimum": 1},
            "iops_write": {"type": "integer", "minimum": 1},
            "bps_total": {"type": "integer", "minimum": 1},
            "bps_read": {"type": "integer", "minimum": 1},
            "bps_write": {"type": "integer", "minimum": 1},
            "iops_total_max": {"type": "integer", "minimum": 1},
            "bps_total_max": {"type": "integer", "minimum": 1},
            "burst_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
        }},
        "DiskThrottleRequest": {"type": "object", "properties": {
            "throttle": {"$ref": "#/components/schemas/DiskThrottle"}
        }, "description": "Omit or null throttle to lift every limit"},
        "DiskThrottleResult": {"type": "object", "properties": {
            "name": {"type": "string"},
            "throttle": {"$ref": "#/components/schemas/DiskThrottle"}
        }}
    })
}

/// Kept apart from [`schemas`]: one `json!` that size exceeds the macro
/// recursion limit.
fn cbt_schemas() -> Value {
    json!({
        "CbtTarget": {"type": "object", "required": ["type"], "description": "Credentials are used for this request only and never stored", "properties": {
            "type": {"type": "string", "enum": ["s3", "dir"]},
            "endpoint": {"type": "string"}, "bucket": {"type": "string"}, "region": {"type": "string"},
            "accessKeyId": {"type": "string"}, "secretAccessKey": {"type": "string"},
            "sessionToken": {"type": "string"}, "pathStyle": {"type": "boolean", "default": true},
            "path": {"type": "string", "description": "dir targets only"}
        }},
        "CbtEncryption": {"type": "object", "required": ["keyId", "key"], "properties": {
            "keyId": {"type": "string"}, "key": {"type": "string", "description": "base64 of 32 bytes (AES-256-GCM)"}
        }},
        "CbtParent": {"type": "object", "required": ["backupId", "sequence", "manifestKey"], "properties": {
            "backupId": {"type": "string"}, "sequence": {"type": "integer"}, "manifestKey": {"type": "string"}
        }},
        "CbtBackupRequest": {"type": "object", "required": ["target", "prefix", "chainId", "sequence", "backupId", "mode"], "properties": {
            "target": {"$ref": "#/components/schemas/CbtTarget"},
            "prefix": {"type": "string"}, "chainId": {"type": "string"}, "sequence": {"type": "integer"},
            "backupId": {"type": "string"}, "mode": {"type": "string", "enum": ["full", "incremental"]},
            "parent": {"$ref": "#/components/schemas/CbtParent"},
            "quiesce": {"type": "string", "enum": ["auto", "required", "never"], "default": "auto"},
            "encryption": {"$ref": "#/components/schemas/CbtEncryption"},
            "retainUntil": {"type": "string", "format": "date-time", "description": "S3 Object Lock COMPLIANCE retention"}
        }},
        "CbtRestoreRequest": {"type": "object", "required": ["target", "manifestKey"], "properties": {
            "target": {"$ref": "#/components/schemas/CbtTarget"}, "manifestKey": {"type": "string"},
            "encryption": {"$ref": "#/components/schemas/CbtEncryption"},
            "syntheticBases": {"type": "array", "items": {"type": "string"}, "description": "Manifest keys of this chain's synthetic fulls; one consolidating a parent replaces the rest of the walk"}
        }},
        "CbtSynthesizeRequest": {"type": "object", "required": ["target", "prefix", "manifestKey", "backupId"], "properties": {
            "target": {"$ref": "#/components/schemas/CbtTarget"}, "prefix": {"type": "string"},
            "manifestKey": {"type": "string"}, "backupId": {"type": "string"},
            "encryption": {"$ref": "#/components/schemas/CbtEncryption"},
            "syntheticBases": {"type": "array", "items": {"type": "string"}},
            "retainUntil": {"type": "string", "format": "date-time"}
        }},
        "CbtDeleteRequest": {"type": "object", "required": ["target", "manifestKeys"], "properties": {
            "target": {"$ref": "#/components/schemas/CbtTarget"},
            "manifestKeys": {"type": "array", "items": {"type": "string"}}
        }},
        "CbtDeleteResult": {"type": "object", "properties": {
            "deleted": {"type": "array", "items": {"type": "string"}},
            "notFound": {"type": "array", "items": {"type": "string"}},
            "retentionHeld": {"type": "array", "items": {"type": "string"}}
        }},
        "DiagnosticsRequest": {"type": "object", "properties": {
            "pcap_seconds": {"type": "integer", "minimum": 0, "maximum": 30, "default": 0},
            "pcap_filter": {"type": "string"},
            "memory_dump": {"type": "boolean", "default": false, "description": "QEMU only; pauses the guest while the dump is written"},
            "redact": {"type": "boolean", "default": true},
            "log_lines": {"type": "integer", "minimum": 0, "maximum": 20000, "default": 2000}
        }},
        "DiagnosticsBundle": {"type": "object", "properties": {
            "token": {"type": "string"}, "vm_id": {"type": "string", "format": "uuid"},
            "state": {"type": "string", "enum": ["running", "done", "failed"]},
            "created_at": {"type": "string", "format": "date-time"},
            "finished_at": {"type": "string", "format": "date-time"},
            "request": {"$ref": "#/components/schemas/DiagnosticsRequest"},
            "error": {"type": "string"},
            "sections": {"type": "array", "items": {"type": "object", "description": "name, ok, error, bytes"}},
            "size_bytes": {"type": "integer"}, "sha256": {"type": "string"},
            "memory_dump": {"type": "object", "description": "path, format, size_bytes, sha256, paused_ms"}
        }},
        "DiagnosticsBundleList": {"type": "object", "properties": {"items": {"type": "array", "items": {"$ref": "#/components/schemas/DiagnosticsBundle"}}}},
        "CbtJob": {"type": "object", "properties": {
            "id": {"type": "string", "format": "uuid"},
            "kind": {"type": "string", "enum": ["backup", "restore", "synthesize"]},
            "state": {"type": "string", "enum": ["running", "succeeded", "failed"]},
            "phase": {"type": "string"}, "bytesDone": {"type": "integer"}, "bytesTotal": {"type": "integer"},
            "error": {"type": "string"},
            "fullRequired": {"type": "boolean", "description": "The incremental could not run; take a full next"},
            "manifestKey": {"type": "string"},
            "manifest": {"type": "object", "description": "chainId, sequence, kind, parent, disks[{name, key, virtualSize, storedSize, changedBytes, sha256}]"},
            "skipped": {"type": "array", "items": {"type": "object"}}
        }}
    })
}

fn path_params(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| {
            let schema = if name == "id" {
                json!({"type": "string", "format": "uuid"})
            } else {
                json!({"type": "string"})
            };
            json!({"name": name, "in": "path", "required": true, "schema": schema})
        })
        .collect()
}

pub fn spec() -> Value {
    let mut paths: Map<String, Value> = Map::new();
    for o in ops() {
        let mut params = path_params(o.path);
        params.extend(o.query.iter().map(|(name, desc)| {
            json!({"name": name, "in": "query", "required": false, "description": desc, "schema": {"type": "string"}})
        }));
        let mut ok = json!({"description": "OK"});
        if let Some(r) = o.response {
            ok["content"] = json!({"application/json": {"schema": {"$ref": format!("#/components/schemas/{r}")}}});
        }
        let err = json!({"description": "Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}});
        let mut operation = json!({
            "summary": o.summary,
            "tags": [o.tag],
            "parameters": params,
            "responses": {o.status: ok, "default": err},
        });
        if let Some(b) = o.body {
            operation["requestBody"] = json!({
                "required": true,
                "content": {"application/json": {"schema": {"$ref": format!("#/components/schemas/{b}")}}}
            });
        }
        if matches!(o.path, "/healthz" | "/readyz") {
            operation["security"] = json!([]);
        }
        paths
            .entry(o.path)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("path item is an object")
            .insert(o.method.into(), operation);
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Zyvor FluxVM API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "VM lifecycle, disks, snapshots and events. Other /v1 routes exist; this document covers the VM surface."
        },
        "servers": [{"url": "/"}],
        "security": [{"bearer": []}],
        "tags": [
            {"name": VMS}, {"name": SNAPSHOTS}, {"name": DISKS}, {"name": TEMPLATES},
            {"name": EVENTS}, {"name": HOST}, {"name": NETOBS}, {"name": NETWORK}
        ],
        "paths": paths,
        "components": {
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
            "schemas": schemas()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_consistent() {
        let s = spec();
        assert_eq!(s["openapi"], "3.1.0");
        let schemas = s["components"]["schemas"].as_object().unwrap();
        let paths = s["paths"].as_object().unwrap();
        for p in [
            "/v1/vms",
            "/v1/vms/{id}",
            "/v1/vms/{id}/disks/{name}",
            "/v1/events",
        ] {
            assert!(paths.contains_key(p), "{p}");
        }
        assert!(paths["/v1/vms/{id}"].get("patch").is_some());
        let text = s.to_string();
        for (i, _) in text.match_indices("#/components/schemas/") {
            let name: String = text[i + 21..]
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            assert!(schemas.contains_key(&name), "dangling $ref {name}");
        }
        for (path, item) in paths {
            let declared = path.matches('{').count();
            for (_, op) in item.as_object().unwrap() {
                let n = op["parameters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|p| p["in"] == "path")
                    .count();
                assert_eq!(n, declared, "{path}");
            }
        }
    }
}
