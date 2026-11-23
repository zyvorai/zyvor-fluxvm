// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What `fluxctl kube certs` and `fluxctl kube secrets ls` show, and what Studio's Secrets and Certificates pages show, worked out
//! once. Pure functions over the objects a cluster returns, so the CLI (kubectl in the server VM) and the daemon (the Kubernetes
//! API) give the same answer and tests need no cluster.
//!
//! Secret VALUES never pass through here: a secret is reduced to its name, type and key names, and a TLS secret to the expiry of its
//! certificate (public by design). The private key is never read.

use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// A certificate this close to its end (or closer) is reported as expiring.
pub const WARN_DAYS: i64 = 30;

/// One Secret, without a byte of its data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretRow {
    pub namespace: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// The names of its keys, sorted. Never the values.
    pub keys: Vec<String>,
    pub created: Option<String>,
}

fn s<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

fn items(list: &Value) -> impl Iterator<Item = &Value> {
    list["items"].as_array().into_iter().flatten()
}

/// Secrets from a list object (`kubectl get secrets -o json` or the API). Only names, types and key names are kept. Helm's release
/// bookkeeping secrets are left out, since nobody sets or reads them by hand.
pub fn secrets(list: &Value) -> Vec<SecretRow> {
    let mut rows: Vec<SecretRow> = items(list)
        .filter_map(|o| {
            let kind = s(o, &["type"]).unwrap_or("Opaque");
            if kind == "helm.sh/release.v1" {
                return None;
            }
            let mut keys: Vec<String> = o["data"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            Some(SecretRow {
                namespace: s(o, &["metadata", "namespace"])?.to_owned(),
                name: s(o, &["metadata", "name"])?.to_owned(),
                kind: kind.to_owned(),
                keys,
                created: s(o, &["metadata", "creationTimestamp"]).map(str::to_owned),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    rows
}

/// A TLS Secret reduced to what the expiry check needs: the certificate (base64 of PEM, as the API stores it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSecret {
    pub namespace: String,
    pub name: String,
    pub crt_b64: String,
}

/// TLS Secrets from a list object. Only the public certificate is copied out; the private key is never touched.
pub fn tls_secrets(list: &Value) -> Vec<TlsSecret> {
    items(list)
        .filter(|o| s(o, &["type"]) == Some("kubernetes.io/tls"))
        .filter_map(|o| {
            Some(TlsSecret {
                namespace: s(o, &["metadata", "namespace"])?.to_owned(),
                name: s(o, &["metadata", "name"])?.to_owned(),
                crt_b64: s(o, &["data", "tls.crt"])?.to_owned(),
            })
        })
        .collect()
}

/// The end of the first (leaf) certificate in a Secret's `tls.crt` (base64 of a PEM bundle), as Unix seconds.
pub fn not_after(crt_b64: &str) -> Result<i64, &'static str> {
    let pem = base64::engine::general_purpose::STANDARD
        .decode(crt_b64.trim())
        .map_err(|_| "its certificate is not valid base64")?;
    let (_, p) =
        x509_parser::pem::parse_x509_pem(&pem).map_err(|_| "its certificate is not PEM")?;
    let cert = p
        .parse_x509()
        .map_err(|_| "its certificate cannot be read")?;
    Ok(cert.validity().not_after.timestamp())
}

/// One certificate a cluster serves, and what is wrong with it if anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CertRow {
    pub namespace: String,
    pub secret: String,
    /// `ingress web`, `gateway edge`: what serves it.
    pub used_by: Vec<String>,
    pub hosts: Vec<String>,
    /// Unix seconds; `None` when the certificate cannot be read.
    pub not_after: Option<i64>,
    pub days_left: Option<i64>,
    /// `ok`, `expiring`, `expired`, `missing` or `invalid`.
    pub state: &'static str,
    pub summary: String,
    /// The one command that fixes it (empty when `ok`).
    pub fix: String,
}

fn severity(state: &str) -> u8 {
    match state {
        "expired" | "missing" | "invalid" => 0,
        "expiring" => 1,
        _ => 2,
    }
}

/// Every `(namespace, secret)` an Ingress or Gateway names for TLS, with who names it and for which hosts.
fn references(
    ingresses: &Value,
    gateways: &Value,
) -> BTreeMap<(String, String), (Vec<String>, Vec<String>)> {
    let mut out: BTreeMap<(String, String), (Vec<String>, Vec<String>)> = BTreeMap::new();
    let mut add = |ns: &str, secret: &str, by: String, hosts: Vec<String>| {
        let e = out.entry((ns.to_owned(), secret.to_owned())).or_default();
        if !e.0.contains(&by) {
            e.0.push(by);
        }
        for h in hosts {
            if !e.1.contains(&h) {
                e.1.push(h);
            }
        }
    };
    for i in items(ingresses) {
        let ns = s(i, &["metadata", "namespace"]).unwrap_or("default");
        let name = s(i, &["metadata", "name"]).unwrap_or("?");
        for t in i["spec"]["tls"].as_array().into_iter().flatten() {
            let Some(secret) = s(t, &["secretName"]) else {
                continue;
            };
            let hosts = t["hosts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|h| h.as_str().map(str::to_owned))
                .collect();
            add(ns, secret, format!("ingress {name}"), hosts);
        }
    }
    for g in items(gateways) {
        let gns = s(g, &["metadata", "namespace"]).unwrap_or("default");
        let name = s(g, &["metadata", "name"]).unwrap_or("?");
        for l in g["spec"]["listeners"].as_array().into_iter().flatten() {
            for r in l["tls"]["certificateRefs"].as_array().into_iter().flatten() {
                if s(r, &["kind"]).is_some_and(|k| k != "Secret") {
                    continue;
                }
                let Some(secret) = s(r, &["name"]) else {
                    continue;
                };
                let ns = s(r, &["namespace"]).unwrap_or(gns);
                let hosts = s(l, &["hostname"])
                    .map(|h| vec![h.to_owned()])
                    .unwrap_or_default();
                add(ns, secret, format!("gateway {name}"), hosts);
            }
        }
    }
    out
}

fn fix_for(state: &str, ns: &str, secret: &str) -> String {
    match state {
        "ok" => String::new(),
        "missing" => format!(
            "kubectl -n {ns} create secret tls {secret} --cert=CERT.pem --key=KEY.pem   (or let cert-manager issue it)"
        ),
        "invalid" => format!(
            "kubectl -n {ns} create secret tls {secret} --cert=CERT.pem --key=KEY.pem --dry-run=client -o yaml | kubectl apply -f -"
        ),
        // expiring, expired
        _ => format!(
            "kubectl -n {ns} create secret tls {secret} --cert=NEW.pem --key=KEY.pem --dry-run=client -o yaml | kubectl apply -f -   (with cert-manager: cmctl renew -n {ns} CERTIFICATE)"
        ),
    }
}

/// The certificates the cluster's Ingresses and Gateways point at, worst first. `now` is Unix seconds; certificates ending within
/// `warn_days` are `expiring`.
pub fn certs(
    ingresses: &Value,
    gateways: &Value,
    tls: &[TlsSecret],
    now: i64,
    warn_days: i64,
) -> Vec<CertRow> {
    let mut rows: Vec<CertRow> = references(ingresses, gateways)
        .into_iter()
        .map(|((ns, secret), (used_by, hosts))| {
            let found = tls.iter().find(|t| t.namespace == ns && t.name == secret);
            let (end, state, summary) = match found.map(|t| not_after(&t.crt_b64)) {
                None => (
                    None,
                    "missing",
                    format!(
                        "the secret {secret} does not exist, so {} cannot serve TLS",
                        used_by.join(" and ")
                    ),
                ),
                Some(Err(why)) => (None, "invalid", format!("{secret}: {why}")),
                Some(Ok(end)) => {
                    let days = (end - now).div_euclid(86_400);
                    if end < now {
                        (
                            Some(end),
                            "expired",
                            format!(
                                "expired {} day{} ago",
                                -days,
                                if days == -1 { "" } else { "s" }
                            ),
                        )
                    } else if days <= warn_days {
                        (
                            Some(end),
                            "expiring",
                            format!("expires in {days} day{}", if days == 1 { "" } else { "s" }),
                        )
                    } else {
                        (Some(end), "ok", format!("valid for {days} more days"))
                    }
                }
            };
            CertRow {
                fix: fix_for(state, &ns, &secret),
                days_left: end.map(|e| (e - now).div_euclid(86_400)),
                namespace: ns,
                secret,
                used_by,
                hosts,
                not_after: end,
                state,
                summary,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        (
            severity(a.state),
            a.days_left.unwrap_or(i64::MAX),
            &a.namespace,
            &a.secret,
        )
            .cmp(&(
                severity(b.state),
                b.days_left.unwrap_or(i64::MAX),
                &b.namespace,
                &b.secret,
            ))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A self-signed certificate that ends `days` from `now`, as the base64 a Secret stores.
    fn crt(days: i64, now: i64) -> String {
        let mut p = rcgen::CertificateParams::new(vec!["app.example.test".into()]).unwrap();
        let at = |t: i64| time::OffsetDateTime::from_unix_timestamp(t).unwrap();
        p.not_before = at(now - 86_400 * 100);
        p.not_after = at(now + 86_400 * days);
        let key = rcgen::KeyPair::generate().unwrap();
        let pem = p.self_signed(&key).unwrap().pem();
        base64::engine::general_purpose::STANDARD.encode(pem)
    }

    const NOW: i64 = 1_800_000_000;

    fn ingress(secret: &str) -> Value {
        json!({"items": [{"metadata": {"namespace": "shop", "name": "web"},
            "spec": {"tls": [{"hosts": ["shop.example.test"], "secretName": secret}]}}]})
    }

    fn tls(name: &str, days: i64) -> TlsSecret {
        TlsSecret {
            namespace: "shop".into(),
            name: name.into(),
            crt_b64: crt(days, NOW),
        }
    }

    #[test]
    fn secrets_keep_names_and_never_values() {
        let list = json!({"items": [
            {"metadata": {"namespace": "b", "name": "z", "creationTimestamp": "t"}, "type": "Opaque", "data": {"token": "c2VjcmV0", "a": "eA=="}},
            {"metadata": {"namespace": "a", "name": "y"}, "data": {}},
            {"metadata": {"namespace": "a", "name": "sh.helm.release.v1.x.v1"}, "type": "helm.sh/release.v1", "data": {"release": "H4sI"}},
        ]});
        let rows = secrets(&list);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["y", "z"]
        );
        assert_eq!(rows[1].keys, ["a", "token"]);
        let text = serde_json::to_string(&rows).unwrap();
        assert!(
            !text.contains("c2VjcmV0") && !text.contains("eA=="),
            "{text}"
        );
    }

    #[test]
    fn tls_secrets_copy_only_the_certificate() {
        let list = json!({"items": [
            {"metadata": {"namespace": "n", "name": "t"}, "type": "kubernetes.io/tls", "data": {"tls.crt": "Q1JU", "tls.key": "S0VZ"}},
            {"metadata": {"namespace": "n", "name": "o"}, "type": "Opaque", "data": {"tls.crt": "Q1JU"}},
        ]});
        let got = tls_secrets(&list);
        assert_eq!(
            got,
            vec![TlsSecret {
                namespace: "n".into(),
                name: "t".into(),
                crt_b64: "Q1JU".into()
            }]
        );
    }

    #[test]
    fn a_healthy_certificate_is_ok_and_has_no_fix() {
        let rows = certs(
            &ingress("web-tls"),
            &json!({}),
            &[tls("web-tls", 90)],
            NOW,
            WARN_DAYS,
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!((r.state, r.days_left), ("ok", Some(90)));
        assert_eq!(r.used_by, ["ingress web"]);
        assert_eq!(r.hosts, ["shop.example.test"]);
        assert!(r.fix.is_empty());
    }

    #[test]
    fn expiring_expired_missing_and_invalid_are_told_apart_worst_first() {
        let ing = json!({"items": [{"metadata": {"namespace": "shop", "name": "web"}, "spec": {"tls": [
            {"hosts": ["a.test"], "secretName": "soon"},
            {"hosts": ["b.test"], "secretName": "gone"},
            {"hosts": ["c.test"], "secretName": "fine"},
            {"hosts": ["d.test"], "secretName": "ghost"},
            {"hosts": ["e.test"], "secretName": "junk"},
        ]}}]});
        let junk = TlsSecret {
            namespace: "shop".into(),
            name: "junk".into(),
            crt_b64: "bm90IGEgY2VydA==".into(),
        };
        let all = [tls("soon", 10), tls("gone", -3), tls("fine", 200), junk];
        let rows = certs(&ing, &json!({}), &all, NOW, WARN_DAYS);
        let order: Vec<(&str, &str)> = rows.iter().map(|r| (r.secret.as_str(), r.state)).collect();
        assert_eq!(
            order,
            [
                ("gone", "expired"),
                ("ghost", "missing"),
                ("junk", "invalid"),
                ("soon", "expiring"),
                ("fine", "ok")
            ]
        );
        assert!(
            rows[0].summary.contains("expired 3 days ago"),
            "{}",
            rows[0].summary
        );
        assert!(
            rows[3].summary.contains("expires in 10 days"),
            "{}",
            rows[3].summary
        );
        assert!(
            rows[1].fix.contains("create secret tls ghost"),
            "{}",
            rows[1].fix
        );
        assert!(rows[0].fix.contains("kubectl apply"), "{}", rows[0].fix);
    }

    #[test]
    fn gateways_and_ingresses_naming_one_secret_share_a_row() {
        let gw = json!({"items": [{"metadata": {"namespace": "edge", "name": "main"}, "spec": {"listeners": [
            {"name": "https", "hostname": "shop.example.test", "tls": {"certificateRefs": [{"name": "web-tls", "namespace": "shop"}]}},
            {"name": "other", "tls": {"certificateRefs": [{"kind": "ConfigMap", "name": "ignored"}]}},
        ]}}]});
        let rows = certs(
            &ingress("web-tls"),
            &gw,
            &[tls("web-tls", 60)],
            NOW,
            WARN_DAYS,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].used_by, ["ingress web", "gateway main"]);
        assert_eq!(rows[0].hosts, ["shop.example.test"]);
    }

    #[test]
    fn the_warning_window_is_a_parameter() {
        let rows = certs(
            &ingress("web-tls"),
            &json!({}),
            &[tls("web-tls", 45)],
            NOW,
            60,
        );
        assert_eq!(rows[0].state, "expiring");
        let rows = certs(
            &ingress("web-tls"),
            &json!({}),
            &[tls("web-tls", 45)],
            NOW,
            30,
        );
        assert_eq!(rows[0].state, "ok");
    }
}
