// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl host-compat check`, `fluxctl place advise` and `fluxctl bench-report check|compare`: offline planning
//! reports computed from JSON files. They need no daemon, VM, `/dev/kvm` or privileges and write nothing.
//! Rules live in `fluxvm_core::{host_compat, placement, bench_report}`.

use anyhow::{Context, Result, bail};
use fluxvm_core::bench_report::{self, BenchReport, Comparison, DeltaVerdict};
use fluxvm_core::config_lint::{Finding, Severity};
use fluxvm_core::host_compat::{self, CompatReport, HostDescription, Verdict, VmNeeds};
use fluxvm_core::placement::{self, PlacementAdvice};
use fluxvm_core::topology::HostTopology;
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Reads a file but never more than `cap` bytes: a hostile or wrong file must not exhaust memory.
fn read_capped(path: &Path, cap: u64) -> Result<String> {
    let f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buf = String::new();
    f.take(cap + 1)
        .read_to_string(&mut buf)
        .with_context(|| format!("reading {} (is it a UTF-8 text file?)", path.display()))?;
    if buf.len() as u64 > cap {
        bail!(
            "{} is larger than {} bytes; refusing to read it",
            path.display(),
            cap
        );
    }
    Ok(buf)
}

fn counts(findings: &[Finding]) -> (usize, usize, usize) {
    let c = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    (c(Severity::Error), c(Severity::Warning), c(Severity::Info))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn render_finding(f: &Finding) -> String {
    let mut s = format!("{:<8}{}  {}\n", f.severity.label(), f.key, f.message);
    if let Some(fix) = &f.fix {
        s.push_str(&format!("        fix: {fix}\n"));
    }
    s
}

fn summary(findings: &[Finding]) -> String {
    let (e, w, n) = counts(findings);
    format!(
        "{e} error{}, {w} warning{}, {n} note{}",
        plural(e),
        plural(w),
        plural(n)
    )
}

// ---- host-compat ------------------------------------------------------------------------------------------------

#[derive(clap::Subcommand, Debug, Clone)]
pub enum HostCompatCommand {
    /// Check whether a VM can move from one host to another (live migration or snapshot restore): CPU vendor and
    /// flags, hypervisor versions, passthrough devices, huge pages, NUMA nodes. Compares two JSON host
    /// descriptions; starts and touches nothing. Exits 1 when incompatible (and on a warning with `--strict`).
    Check(HostCompatArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct HostCompatArgs {
    /// Host description (JSON) of the host the VM runs on.
    pub source: PathBuf,
    /// Host description (JSON) of the destination.
    pub destination: PathBuf,
    /// A VM spec (the JSON of `create --spec`): narrows the check to that VM's backend, CPU template, memory,
    /// huge pages, NUMA binding and passthrough devices.
    #[arg(long)]
    pub spec: Option<PathBuf>,
    /// Exit 1 on warnings too.
    #[arg(long)]
    pub strict: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn render_host_compat(args: &HostCompatArgs) -> Result<(String, i32)> {
    let src = host_compat::parse_host(&read_capped(&args.source, host_compat::MAX_DOC_BYTES)?)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.source.display()))?;
    let dst = host_compat::parse_host(&read_capped(&args.destination, host_compat::MAX_DOC_BYTES)?)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.destination.display()))?;
    let vm = match &args.spec {
        Some(p) => Some(
            VmNeeds::from_spec_str(&read_capped(p, host_compat::MAX_DOC_BYTES)?)
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?,
        ),
        None => None,
    };
    let report = host_compat::check(&src, &dst, vm.as_ref());
    let code = compat_code(&report, args.strict);
    if args.json {
        return Ok((serde_json::to_string_pretty(&report)? + "\n", code));
    }
    Ok((compat_text(&report), code))
}

fn compat_code(r: &CompatReport, strict: bool) -> i32 {
    i32::from(
        r.verdict == Verdict::Incompatible
            || (strict && r.verdict == Verdict::CompatibleWithWarnings),
    )
}

fn compat_text(r: &CompatReport) -> String {
    let mut out = String::new();
    for f in &r.findings {
        out.push_str(&render_finding(f));
    }
    let v = match r.verdict {
        Verdict::Compatible => "no known blocker",
        Verdict::CompatibleWithWarnings => "no certain blocker, but read the warnings",
        Verdict::Incompatible => "INCOMPATIBLE",
    };
    out.push_str(&format!(
        "{} -> {}: {v} ({})\n",
        r.source,
        r.destination,
        summary(&r.findings)
    ));
    out
}

// ---- place ------------------------------------------------------------------------------------------------------

#[derive(clap::Subcommand, Debug, Clone)]
pub enum PlaceCommand {
    /// Advise where one VM fits on a host: the NUMA node (memory, CPUs, huge pages, passthrough-device locality), a
    /// CPU set of whole cores, huge-page and IOMMU-group findings. Reads a saved host graph (`GET /v1/host/topology`,
    /// or a host description with a `topology` key) and a VM spec; starts and changes nothing. Exits 1 when the VM
    /// does not fit.
    Advise(PlaceArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct PlaceArgs {
    /// Host graph JSON (the topology, or a host description that has one).
    #[arg(long)]
    pub host: PathBuf,
    /// VM spec JSON.
    #[arg(long)]
    pub spec: PathBuf,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

fn load_topology(raw: &str) -> Result<HostTopology, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("not valid JSON: {e}"))?;
    if v.get("numaNodes").is_some() {
        return serde_json::from_value(v).map_err(|e| format!("not a host topology: {e}"));
    }
    if v.get("topology").is_some() {
        let h: HostDescription =
            serde_json::from_value(v).map_err(|e| format!("not a host description: {e}"))?;
        return h
            .topology
            .ok_or_else(|| "the host description has no topology".to_string());
    }
    Err("expected a host topology (`numaNodes`) or a host description with a `topology` key".into())
}

pub fn render_place(args: &PlaceArgs) -> Result<(String, i32)> {
    let topo = load_topology(&read_capped(&args.host, host_compat::MAX_DOC_BYTES)?)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.host.display()))?;
    let vm = VmNeeds::from_spec_str(&read_capped(&args.spec, host_compat::MAX_DOC_BYTES)?)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.spec.display()))?;
    let advice = placement::advise(&topo, &vm);
    let code = i32::from(
        advice
            .findings
            .iter()
            .any(|f| f.severity == Severity::Error),
    );
    if args.json {
        return Ok((serde_json::to_string_pretty(&advice)? + "\n", code));
    }
    Ok((place_text(&advice), code))
}

fn place_text(a: &PlacementAdvice) -> String {
    let mut out = String::new();
    out.push_str("node  fits  cpus  free MiB  free hugepage MiB\n");
    for n in &a.nodes {
        out.push_str(&format!(
            "{:<5} {:<5} {:<5} {:<9} {:<9}{}\n",
            n.node,
            if n.fits { "yes" } else { "no" },
            n.cpus,
            n.free_memory_mib,
            n.free_hugepage_mib,
            if n.reasons.is_empty() {
                String::new()
            } else {
                format!("  ({})", n.reasons.join("; "))
            }
        ));
    }
    match a.recommended_node {
        Some(n) => {
            out.push_str(&format!("recommended: numa_node {n}"));
            if let Some(c) = &a.suggested.cpuset {
                out.push_str(&format!(", cpuset \"{c}\""));
            }
            if a.suggested.hugepages == Some(true) {
                out.push_str(", hugepages true");
            }
            out.push('\n');
        }
        None if !a.suggested.span_nodes.is_empty() => {
            out.push_str(&format!(
                "no single node fits; span nodes {:?} (describe them with `numa`)\n",
                a.suggested.span_nodes
            ));
        }
        None => out.push_str("no placement\n"),
    }
    for f in &a.findings {
        out.push_str(&render_finding(f));
    }
    out.push_str(&format!("{}\n", summary(&a.findings)));
    out
}

// ---- bench-report -----------------------------------------------------------------------------------------------

#[derive(clap::Subcommand, Debug, Clone)]
pub enum BenchReportCommand {
    /// Validate a benchmark report (`fluxvm-bench/1`: raw samples plus the method that produced them) and print
    /// p50/p95/p99 per series. Exits 1 on an error.
    Check {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Compare two reports: refuses to compare different methods (clock start/stop, page cache, backend, guest
    /// image, density), and flags a series as a regression when p50 or (with 100+ samples) p99 got worse than the
    /// threshold. Exit 1 on a regression, 2 when the reports are not comparable.
    Compare {
        base: PathBuf,
        new: PathBuf,
        /// Change in percent that counts as a regression or improvement.
        #[arg(long, default_value_t = 10.0)]
        threshold_pct: f64,
        #[arg(long)]
        json: bool,
    },
}

fn load_bench(p: &Path) -> Result<BenchReport> {
    bench_report::parse(&read_capped(p, bench_report::MAX_DOC_BYTES)?)
        .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
}

pub fn render_bench(cmd: &BenchReportCommand) -> Result<(String, i32)> {
    match cmd {
        BenchReportCommand::Check { file, json } => {
            let r = load_bench(file)?;
            let findings = bench_report::validate(&r);
            let (e, _, _) = counts(&findings);
            let stats: Vec<_> = r
                .series
                .iter()
                .filter_map(|(k, s)| {
                    bench_report::stats(&s.samples).map(|st| (k.clone(), s.unit.clone(), st))
                })
                .collect();
            if *json {
                let v = json!({
                    "file": file.display().to_string(),
                    "errors": e,
                    "series": stats.iter().map(|(k, u, s)| json!({"series": k, "unit": u, "stats": s})).collect::<Vec<_>>(),
                    "findings": findings,
                });
                return Ok((serde_json::to_string_pretty(&v)? + "\n", i32::from(e > 0)));
            }
            let mut out = String::new();
            for (k, u, s) in &stats {
                out.push_str(&format!(
                    "{k}: n={} p50={:.2}{u} p95={:.2}{u} p99={:.2}{u} max={:.2}{u}\n",
                    s.n, s.p50, s.p95, s.p99, s.max
                ));
            }
            for f in &findings {
                out.push_str(&render_finding(f));
            }
            out.push_str(&format!("{}: {}\n", file.display(), summary(&findings)));
            Ok((out, i32::from(e > 0)))
        }
        BenchReportCommand::Compare {
            base,
            new,
            threshold_pct,
            json,
        } => {
            if !threshold_pct.is_finite() || *threshold_pct < 0.0 || *threshold_pct > 1000.0 {
                bail!("--threshold-pct must be between 0 and 1000");
            }
            let c = bench_report::compare(&load_bench(base)?, &load_bench(new)?, *threshold_pct);
            let code = if !c.comparable {
                2
            } else {
                i32::from(c.regressions() > 0)
            };
            if *json {
                return Ok((serde_json::to_string_pretty(&c)? + "\n", code));
            }
            Ok((compare_text(&c), code))
        }
    }
}

fn compare_text(c: &Comparison) -> String {
    let mut out = String::new();
    for d in &c.deltas {
        let tag = match d.verdict {
            DeltaVerdict::Regression => "REGRESSION",
            DeltaVerdict::Improvement => "improved",
            DeltaVerdict::Unchanged => "unchanged",
        };
        out.push_str(&format!(
            "{:<10} {}: p50 {:.2} -> {:.2}{u} ({:+.1}%), p99 {:.2} -> {:.2}{u} ({:+.1}%), n {} -> {}\n",
            tag,
            d.series,
            d.base.p50,
            d.new.p50,
            d.p50_change_pct,
            d.base.p99,
            d.new.p99,
            d.p99_change_pct,
            d.base.n,
            d.new.n,
            u = d.unit
        ));
    }
    for f in &c.findings {
        out.push_str(&render_finding(f));
    }
    out.push_str(&format!(
        "{}; {} regression{}; {}\n",
        if c.comparable {
            "comparable"
        } else {
            "NOT COMPARABLE"
        },
        c.regressions(),
        plural(c.regressions()),
        summary(&c.findings)
    ));
    out
}

pub fn run_host_compat(cmd: &HostCompatCommand) -> Result<i32> {
    let HostCompatCommand::Check(a) = cmd;
    let (out, code) = render_host_compat(a)?;
    print!("{out}");
    Ok(code)
}

pub fn run_place(cmd: &PlaceCommand) -> Result<i32> {
    let PlaceCommand::Advise(a) = cmd;
    let (out, code) = render_place(a)?;
    print!("{out}");
    Ok(code)
}

pub fn run_bench(cmd: &BenchReportCommand) -> Result<i32> {
    let (out, code) = render_bench(cmd)?;
    print!("{out}");
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxctl-hostplan-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn put(d: &Path, name: &str, body: &str) -> PathBuf {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }
    const A: &str = r#"{"name":"a","arch":"x86_64","cpu":{"vendor":"GenuineIntel","flags":["avx2","avx512f"]},"backends":{"qemu":"9.1.0"}}"#;
    const B: &str = r#"{"name":"b","arch":"x86_64","cpu":{"vendor":"GenuineIntel","flags":["avx2"]},"backends":{"qemu":"9.2.0"}}"#;
    const SPEC: &str = r#"{"name":"t","backend":"qemu","image":"/a.qcow2"}"#;

    #[test]
    fn compat_end_to_end_and_exit_codes() {
        let d = dir("compat");
        let (a, b, s) = (
            put(&d, "a.json", A),
            put(&d, "b.json", B),
            put(&d, "s.json", SPEC),
        );
        let args = |spec: Option<PathBuf>, strict, json| HostCompatArgs {
            source: a.clone(),
            destination: b.clone(),
            spec,
            strict,
            json,
        };
        let (out, code) = render_host_compat(&args(None, false, false)).unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("avx512f") && out.contains("a -> b"));
        assert_eq!(render_host_compat(&args(None, true, false)).unwrap().1, 1);
        let (out, code) = render_host_compat(&args(Some(s), false, true)).unwrap();
        assert_eq!(code, 1);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["verdict"], "incompatible");
    }

    #[test]
    fn bad_and_huge_files_are_refused() {
        let d = dir("bad");
        let bad = put(&d, "bad.json", "[1,2]");
        let ok = put(&d, "ok.json", A);
        let e = render_host_compat(&HostCompatArgs {
            source: bad,
            destination: ok.clone(),
            spec: None,
            strict: false,
            json: false,
        })
        .unwrap_err();
        assert!(e.to_string().contains("bad.json"), "{e}");
        let e = render_host_compat(&HostCompatArgs {
            source: d.join("missing.json"),
            destination: ok.clone(),
            spec: None,
            strict: false,
            json: false,
        })
        .unwrap_err();
        assert!(e.to_string().contains("missing.json"));
        let big = d.join("big.json");
        std::fs::write(&big, vec![b' '; 4096]).unwrap();
        assert!(
            read_capped(&big, 1024)
                .unwrap_err()
                .to_string()
                .contains("larger")
        );
        let bin = put(&d, "bin.json", "");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_capped(&bin, 1024).is_err());
    }

    const TOPO: &str = r#"{"numaNodes":[{"id":0,"cpus":[0,1,2,3],"threadSiblings":[[0,1],[2,3]],"memoryMib":8192,"freeMemoryMib":4096,"hugepages":[]}],"devices":[]}"#;

    #[test]
    fn place_accepts_a_topology_or_a_host_description() {
        let d = dir("place");
        let s = put(
            &d,
            "s.json",
            r#"{"name":"t","backend":"qemu","image":"/a","vcpus":2,"memory_mib":1024}"#,
        );
        let h = put(&d, "h.json", TOPO);
        let (out, code) = render_place(&PlaceArgs {
            host: h,
            spec: s.clone(),
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("recommended: numa_node 0, cpuset \"0-1\""),
            "{out}"
        );
        let wrapped = put(
            &d,
            "w.json",
            &format!(r#"{{"name":"x","topology":{TOPO}}}"#),
        );
        assert_eq!(
            render_place(&PlaceArgs {
                host: wrapped,
                spec: s.clone(),
                json: true
            })
            .unwrap()
            .1,
            0
        );
        let none = put(&d, "n.json", r#"{"name":"x"}"#);
        assert!(
            render_place(&PlaceArgs {
                host: none,
                spec: s.clone(),
                json: false
            })
            .is_err()
        );
        let big = put(&d, "b.json", TOPO);
        let spec_big = put(
            &d,
            "sb.json",
            r#"{"name":"t","backend":"qemu","image":"/a","vcpus":2,"memory_mib":999999}"#,
        );
        assert_eq!(
            render_place(&PlaceArgs {
                host: big,
                spec: spec_big,
                json: false
            })
            .unwrap()
            .1,
            1
        );
    }

    fn bench(samples: &str, stop: &str) -> String {
        format!(
            r#"{{"schema":"fluxvm-bench/1","method":{{"clock_start":"api_create","clock_stop":"{stop}","page_cache":"warm","backend":"firecracker","guest_image":"u"}},"series":{{"boot_ms":{{"unit":"ms","samples":{samples}}}}}}}"#
        )
    }

    #[test]
    fn bench_check_and_compare_exit_codes() {
        let d = dir("bench");
        let fast = format!("[{}]", vec!["100.0"; 20].join(","));
        let slow = format!("[{}]", vec!["150.0"; 20].join(","));
        let a = put(&d, "a.json", &bench(&fast, "guest_ready"));
        let b = put(&d, "b.json", &bench(&slow, "guest_ready"));
        let c = put(&d, "c.json", &bench(&slow, "ssh_login"));
        let (out, code) = render_bench(&BenchReportCommand::Check {
            file: a.clone(),
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("p50=100.00ms"), "{out}");
        let cmp = |x: &PathBuf, y: &PathBuf, t| {
            render_bench(&BenchReportCommand::Compare {
                base: x.clone(),
                new: y.clone(),
                threshold_pct: t,
                json: false,
            })
            .unwrap()
        };
        let (out, code) = cmp(&a, &b, 10.0);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("REGRESSION"));
        assert_eq!(cmp(&a, &b, 60.0).1, 0);
        assert_eq!(cmp(&a, &c, 10.0).1, 2);
        assert!(
            render_bench(&BenchReportCommand::Compare {
                base: a.clone(),
                new: b,
                threshold_pct: f64::NAN,
                json: false
            })
            .is_err()
        );
        let bad = put(&d, "bad.json", r#"{"schema":"nope","series":{}}"#);
        assert_eq!(
            render_bench(&BenchReportCommand::Check {
                file: bad,
                json: true
            })
            .unwrap()
            .1,
            1
        );
    }

    fn example(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/planning")
            .join(name)
    }

    #[test]
    fn the_shipped_examples_give_the_documented_answers() {
        let (out, code) = render_host_compat(&HostCompatArgs {
            source: example("host-a.json"),
            destination: example("host-b.json"),
            spec: Some(example("vm-gpu.json")),
            strict: false,
            json: false,
        })
        .unwrap();
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("avx512bw, avx512f"), "{out}");
        assert!(
            out.contains("not a device on the destination") || out.contains("vfio_devices"),
            "{out}"
        );
        let (out, code) = render_place(&PlaceArgs {
            host: example("host-a.json"),
            spec: example("vm-gpu.json"),
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("recommended: numa_node 1, cpuset \"8-15\", hugepages true"),
            "{out}"
        );
        let (out, code) = render_bench(&BenchReportCommand::Compare {
            base: example("bench-base.json"),
            new: example("bench-new.json"),
            threshold_pct: 10.0,
            json: false,
        })
        .unwrap();
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("REGRESSION"), "{out}");
    }
}
