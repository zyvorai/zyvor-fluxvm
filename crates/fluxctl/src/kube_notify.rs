//! macOS notifications from `kube supervise` when a pod starts failing (crash loop, out of memory, image that cannot be
//! pulled), so a problem shows up on the desktop instead of in a terminal nobody is looking at. The decisions are pure;
//! only `send` talks to the system.

use crate::kube_doctor::{PodRow, is_bad};
use std::collections::HashMap;

/// A failing pod must look bad on this many checks in a row before it is announced; pods that are merely still starting
/// pass through `Error` or `ImagePullBackOff` for a moment.
const BAD_CHECKS: u32 = 2;
/// And it can be announced again only after it has looked fine this many checks in a row.
const QUIET_CHECKS: u32 = 5;

#[derive(Debug, PartialEq, Eq)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

#[derive(Default)]
struct State {
    bad: u32,
    quiet: u32,
    announced: bool,
}

/// Remembers which pods have been announced so each problem is reported once, not on every check.
#[derive(Default)]
pub struct Watch {
    pods: HashMap<String, State>,
}

impl Watch {
    /// Feeds one look at the cluster's pods; returns the problems to announce now.
    pub fn update(&mut self, cluster: &str, rows: &[PodRow]) -> Vec<Alert> {
        let mut alerts = vec![];
        let mut present = std::collections::HashSet::new();
        for p in rows {
            let key = format!("{}/{}", p.namespace, p.name);
            present.insert(key.clone());
            let st = self.pods.entry(key.clone()).or_default();
            if is_bad(&p.status) {
                st.bad += 1;
                st.quiet = 0;
                if !st.announced && st.bad >= BAD_CHECKS {
                    st.announced = true;
                    alerts.push(Alert {
                        title: format!("{cluster}: a pod is {}", p.status),
                        body: format!("{key} (restarts {}). See: fluxctl kube doctor", p.restarts),
                    });
                }
            } else {
                st.bad = 0;
                st.quiet += 1;
            }
        }
        self.pods
            .retain(|k, st| present.contains(k) && st.quiet < QUIET_CHECKS);
        alerts
    }
}

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// The AppleScript that shows the notification. Everything from the cluster is escaped.
pub fn applescript(a: &Alert) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape(&a.body),
        escape(&a.title)
    )
}

/// Shows the notification. `FLUXVM_NOTIFY_LOG=<file>` appends `title<TAB>body` to a file instead, for tests and for
/// machines with no desktop session.
pub fn send(a: &Alert) {
    if let Some(path) = std::env::var_os("FLUXVM_NOTIFY_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}\t{}", a.title, a.body);
        }
        return;
    }
    // Not waited for here: with no desktop session, or a permission prompt nobody answers, osascript can hang, and this is
    // called from the supervise loop.
    let script = applescript(a);
    std::thread::spawn(move || {
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_doctor::parse_pods;

    fn rows(s: &str) -> Vec<PodRow> {
        parse_pods(s)
    }

    #[test]
    fn a_failing_pod_is_announced_once_after_two_bad_checks() {
        let mut w = Watch::default();
        let bad = rows("default api 0/1 CrashLoopBackOff 6 (10s ago) 9m\n");
        assert!(w.update("dev", &bad).is_empty(), "one look is not enough");
        let a = w.update("dev", &bad);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title, "dev: a pod is CrashLoopBackOff");
        assert!(a[0].body.starts_with("default/api (restarts 6)"));
        assert!(
            w.update("dev", &bad).is_empty(),
            "not again while it stays bad"
        );
    }

    #[test]
    fn a_pod_that_flaps_is_not_announced_every_time() {
        let mut w = Watch::default();
        let bad = rows("default api 0/1 Error 3 1m\n");
        let ok = rows("default api 1/1 Running 3 1m\n");
        w.update("dev", &bad);
        assert_eq!(w.update("dev", &bad).len(), 1);
        for _ in 0..3 {
            assert!(w.update("dev", &ok).is_empty());
            assert!(w.update("dev", &bad).is_empty(), "still the same problem");
        }
    }

    #[test]
    fn a_pod_that_recovered_can_be_announced_again() {
        let mut w = Watch::default();
        let bad = rows("default api 0/1 OOMKilled 1 1m\n");
        let ok = rows("default api 1/1 Running 1 9m\n");
        w.update("dev", &bad);
        assert_eq!(w.update("dev", &bad).len(), 1);
        for _ in 0..QUIET_CHECKS {
            w.update("dev", &ok);
        }
        w.update("dev", &bad);
        assert_eq!(w.update("dev", &bad).len(), 1);
    }

    #[test]
    fn a_deleted_pod_is_forgotten() {
        let mut w = Watch::default();
        let bad = rows("default api 0/1 ErrImagePull 0 1m\n");
        w.update("dev", &bad);
        w.update("dev", &bad);
        w.update("dev", &[]);
        assert!(w.pods.is_empty());
    }

    #[test]
    fn the_script_escapes_what_the_cluster_sends() {
        let a = Alert {
            title: "dev: a \"pod\"".into(),
            body: "x\\y\nz\" & do shell script \"id".into(),
        };
        let s = applescript(&a);
        assert_eq!(
            s,
            "display notification \"x\\\\y z\\\" & do shell script \\\"id\" with title \"dev: a \\\"pod\\\"\""
        );
    }
}
