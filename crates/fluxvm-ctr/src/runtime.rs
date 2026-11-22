// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Which runtime a workload gets: `container` (FluxVM's own, shares the host kernel) or `vm` (a microVM, its own kernel).

use serde::{Deserialize, Serialize};

/// What the caller asks for. `auto` is resolved by [`resolve`] and never appears in a [`Decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    #[default]
    Auto,
    Container,
    Vm,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Auto => "auto",
            Runtime::Container => "container",
            Runtime::Vm => "vm",
        }
    }
}

impl std::str::FromStr for Runtime {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Runtime::Auto),
            "container" => Ok(Runtime::Container),
            "vm" => Ok(Runtime::Vm),
            other => Err(format!(
                "unknown runtime {other:?}: use auto, container or vm"
            )),
        }
    }
}

/// How much the workload is trusted. It drives `auto` and the refusal of `container` for hostile code.
/// The default is `untrusted`: a caller that says nothing gets the stronger boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Your own code on your own machine (a dev loop).
    Dev,
    /// Code you reviewed or built.
    Trusted,
    /// Code you did not write (third-party images, anything pulled for a task).
    #[default]
    Untrusted,
    /// Code an AI agent wrote or chose to run.
    Agent,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Dev => "dev",
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::Agent => "agent",
        }
    }
    pub fn is_untrusted(self) -> bool {
        matches!(self, Trust::Untrusted | Trust::Agent)
    }
}

impl std::str::FromStr for Trust {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" => Ok(Trust::Dev),
            "trusted" => Ok(Trust::Trusted),
            "untrusted" => Ok(Trust::Untrusted),
            "agent" => Ok(Trust::Agent),
            other => Err(format!(
                "unknown trust level {other:?}: use dev, trusted, untrusted or agent"
            )),
        }
    }
}

/// What this host can do, as one `Ok` or one plain reason per runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSupport {
    /// FluxVM's own container runtime.
    pub container: Result<(), String>,
    /// A microVM that runs a container image (one VM per container).
    pub vm_image: Result<(), String>,
    /// A microVM sandbox without an image (a Debian VM, a template).
    pub vm_plain: Result<(), String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub runtime: Runtime,
    pub trust: Trust,
    /// The workload is a container image (`oci`), not a plain VM sandbox.
    pub image: bool,
    /// `--allow-container-untrusted`: the caller accepts a shared-kernel boundary for code it does not trust.
    pub allow_container_untrusted: bool,
    /// Operator setting `sandbox.container.allow_untrusted`: lets callers pass the flag above. Off by default.
    pub operator_allows_untrusted: bool,
}

/// The outcome: which runtime, why, and the isolation it gives, in words fit for the output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub runtime: Runtime,
    pub reason: String,
    pub isolation: String,
    /// Set when the choice carries a risk the caller accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

pub const CONTAINER_ISOLATION: &str = "container: the workload shares the host kernel. Namespaces, cgroup v2, seccomp and dropped capabilities confine it, but a kernel bug reachable through an allowed syscall is an escape";
pub const VM_ISOLATION: &str = "vm: the workload runs in its own kernel behind hardware virtualization; an escape needs a hypervisor bug, not just a kernel bug";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// `container` for untrusted code without the explicit flag (or the operator switch).
    UntrustedContainer { trust: Trust, operator_allows: bool },
    /// The runtime asked for cannot run here.
    Unavailable { runtime: Runtime, why: String },
    /// `auto` found nothing that can run, or only a weaker one for untrusted code.
    NoRuntime { container: String, vm: String },
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::UntrustedContainer {
                trust,
                operator_allows,
            } => {
                write!(
                    f,
                    "refusing runtime=container for {t} code. {CONTAINER_ISOLATION}. {VM_ISOLATION}. ",
                    t = trust.as_str()
                )?;
                if *operator_allows {
                    write!(
                        f,
                        "To accept the weaker boundary anyway, pass --allow-container-untrusted (REST: allow_container_untrusted: true); or use --runtime vm"
                    )
                } else {
                    write!(
                        f,
                        "Use --runtime vm, or have the operator set sandbox.container.allow_untrusted = true and then pass --allow-container-untrusted"
                    )
                }
            }
            RuntimeError::Unavailable { runtime, why } => {
                write!(
                    f,
                    "runtime {} is not available on this host: {why}",
                    runtime.as_str()
                )
            }
            RuntimeError::NoRuntime { container, vm } => write!(
                f,
                "no runtime can run this workload here. container: {container}. vm: {vm}"
            ),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Pick the runtime. Rules:
/// - `container` for `untrusted`/`agent` code is refused unless the operator and the caller both opted in.
/// - `auto` gives `container` to `dev`/`trusted` workloads (fast, cheap) and `vm` to `untrusted`/`agent`.
/// - `auto` falls back from `container` to `vm` (stronger) but never from `vm` to `container`.
pub fn resolve(req: &Request, host: &HostSupport) -> Result<Decision, RuntimeError> {
    let vm_ok = if req.image {
        &host.vm_image
    } else {
        &host.vm_plain
    };
    let t = req.trust.as_str();
    let container = |reason: String, warning: Option<String>| Decision {
        runtime: Runtime::Container,
        reason,
        isolation: CONTAINER_ISOLATION.to_string(),
        warning,
    };
    let vm = |reason: String| Decision {
        runtime: Runtime::Vm,
        reason,
        isolation: VM_ISOLATION.to_string(),
        warning: None,
    };
    match req.runtime {
        Runtime::Container => {
            if !req.image {
                return Err(RuntimeError::Unavailable {
                    runtime: Runtime::Container,
                    why: "a container needs an image (oci)".into(),
                });
            }
            if req.trust.is_untrusted()
                && !(req.allow_container_untrusted && req.operator_allows_untrusted)
            {
                return Err(RuntimeError::UntrustedContainer {
                    trust: req.trust,
                    operator_allows: req.operator_allows_untrusted,
                });
            }
            if let Err(why) = &host.container {
                return Err(RuntimeError::Unavailable {
                    runtime: Runtime::Container,
                    why: why.clone(),
                });
            }
            let warning = req.trust.is_untrusted().then(|| {
                format!("running {t} code in a container on explicit request: the host kernel is the only boundary")
            });
            Ok(container(
                format!("requested explicitly (trust={t})"),
                warning,
            ))
        }
        Runtime::Vm => match vm_ok {
            Ok(()) => Ok(vm(format!("requested explicitly (trust={t})"))),
            Err(why) => Err(RuntimeError::Unavailable {
                runtime: Runtime::Vm,
                why: why.clone(),
            }),
        },
        Runtime::Auto => {
            if !req.image {
                return match vm_ok {
                    Ok(()) => Ok(vm(format!("auto: no image, so a VM sandbox (trust={t})"))),
                    Err(why) => Err(RuntimeError::Unavailable {
                        runtime: Runtime::Vm,
                        why: why.clone(),
                    }),
                };
            }
            if req.trust.is_untrusted() {
                return match vm_ok {
                    Ok(()) => Ok(vm(format!(
                        "auto: trust={t} gets a microVM (own kernel); containers share the host kernel"
                    ))),
                    Err(why) => Err(RuntimeError::NoRuntime {
                        container: format!(
                            "not used for {t} code unless you pass --runtime container --allow-container-untrusted (operator must allow it)"
                        ),
                        vm: why.clone(),
                    }),
                };
            }
            match (&host.container, vm_ok) {
                (Ok(()), _) => Ok(container(
                    format!(
                        "auto: trust={t} gets a native container (fast start, shares the host kernel)"
                    ),
                    None,
                )),
                (Err(why), Ok(())) => Ok(vm(format!(
                    "auto: trust={t} would get a container, but {why}; using a microVM (stronger)"
                ))),
                (Err(c), Err(v)) => Err(RuntimeError::NoRuntime {
                    container: c.clone(),
                    vm: v.clone(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(c: bool, v: bool) -> HostSupport {
        let r = |ok: bool, why: &str| if ok { Ok(()) } else { Err(why.to_string()) };
        HostSupport {
            container: r(c, "no userns"),
            vm_image: r(v, "no oci boot"),
            vm_plain: r(v, "no kvm"),
        }
    }
    fn req(runtime: Runtime, trust: Trust) -> Request {
        Request {
            runtime,
            trust,
            image: true,
            allow_container_untrusted: false,
            operator_allows_untrusted: false,
        }
    }

    #[test]
    fn auto_gives_dev_and_trusted_a_container() {
        for t in [Trust::Dev, Trust::Trusted] {
            let d = resolve(&req(Runtime::Auto, t), &host(true, true)).unwrap();
            assert_eq!(d.runtime, Runtime::Container);
            assert!(d.reason.contains(t.as_str()));
            assert!(d.warning.is_none());
        }
    }

    #[test]
    fn auto_gives_untrusted_and_agent_a_vm_and_never_downgrades() {
        for t in [Trust::Untrusted, Trust::Agent] {
            let d = resolve(&req(Runtime::Auto, t), &host(true, true)).unwrap();
            assert_eq!(d.runtime, Runtime::Vm);
            let e = resolve(&req(Runtime::Auto, t), &host(true, false)).unwrap_err();
            assert!(matches!(e, RuntimeError::NoRuntime { .. }), "{e}");
            assert!(e.to_string().contains("allow-container-untrusted"));
        }
    }

    #[test]
    fn auto_falls_back_from_container_to_vm_only() {
        let d = resolve(&req(Runtime::Auto, Trust::Dev), &host(false, true)).unwrap();
        assert_eq!(d.runtime, Runtime::Vm);
        assert!(d.reason.contains("no userns"));
        assert!(resolve(&req(Runtime::Auto, Trust::Dev), &host(false, false)).is_err());
    }

    #[test]
    fn explicit_container_for_untrusted_code_is_refused_by_default() {
        for t in [Trust::Untrusted, Trust::Agent] {
            let e = resolve(&req(Runtime::Container, t), &host(true, true)).unwrap_err();
            assert!(matches!(e, RuntimeError::UntrustedContainer { .. }));
            let s = e.to_string();
            assert!(
                s.contains("shares the host kernel") && s.contains("own kernel"),
                "{s}"
            );
            assert!(s.contains("sandbox.container.allow_untrusted"), "{s}");
        }
    }

    #[test]
    fn untrusted_container_needs_both_the_operator_and_the_flag() {
        let mut r = req(Runtime::Container, Trust::Untrusted);
        r.allow_container_untrusted = true;
        assert!(
            resolve(&r, &host(true, true)).is_err(),
            "flag alone is not enough"
        );
        r.operator_allows_untrusted = true;
        let d = resolve(&r, &host(true, true)).unwrap();
        assert_eq!(d.runtime, Runtime::Container);
        assert!(d.warning.unwrap().contains("only boundary"));
        r.allow_container_untrusted = false;
        let e = resolve(&r, &host(true, true)).unwrap_err().to_string();
        assert!(e.contains("--allow-container-untrusted"), "{e}");
    }

    #[test]
    fn explicit_runtimes_report_why_they_cannot_run() {
        let e = resolve(&req(Runtime::Container, Trust::Dev), &host(false, true)).unwrap_err();
        assert_eq!(
            e.to_string(),
            "runtime container is not available on this host: no userns"
        );
        let e = resolve(&req(Runtime::Vm, Trust::Dev), &host(true, false)).unwrap_err();
        assert!(e.to_string().contains("no oci boot"));
        let mut r = req(Runtime::Vm, Trust::Dev);
        r.image = false;
        assert_eq!(
            resolve(&r, &host(true, false)).unwrap_err().to_string(),
            "runtime vm is not available on this host: no kvm"
        );
    }

    #[test]
    fn a_plain_sandbox_is_a_vm_and_cannot_be_a_container() {
        let mut r = req(Runtime::Auto, Trust::Dev);
        r.image = false;
        assert_eq!(resolve(&r, &host(true, true)).unwrap().runtime, Runtime::Vm);
        r.runtime = Runtime::Container;
        assert!(resolve(&r, &host(true, true)).is_err());
    }

    #[test]
    fn parsing() {
        assert_eq!("VM".parse::<Runtime>().unwrap(), Runtime::Vm);
        assert!("docker".parse::<Runtime>().is_err());
        assert_eq!("agent".parse::<Trust>().unwrap(), Trust::Agent);
        assert_eq!(Trust::default(), Trust::Untrusted);
        assert!("x".parse::<Trust>().is_err());
    }
}
