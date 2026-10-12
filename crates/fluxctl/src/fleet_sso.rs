// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet members | member-add | member-rm | member-role`: the people of a tenant who sign in to the console
//! with SSO. The CLI itself keeps using tokens.

use super::*;

/// `/fleet/tenants/{tenant|me}/members`.
fn members_path(tenant: Option<&str>) -> String {
    format!("/fleet/tenants/{}/members", esc(tenant.unwrap_or("me")))
}

fn role_cell(role: &str) -> Cell {
    match role {
        "owner" | "admin" => ct(role, Tone::Ok),
        "viewer" => ct(role, Tone::Dim),
        _ => c(role),
    }
}

pub(super) async fn members(
    central: &str,
    token: Option<&str>,
    out: &Out,
    cmd: FleetCommand,
) -> Result<()> {
    match cmd {
        FleetCommand::Members { tenant } => {
            let v = get(central, token, &members_path(tenant.as_deref())).await?;
            if !out.human {
                return out.json(&v);
            }
            let sso = v.get("sso").and_then(Value::as_bool) != Some(false);
            let list = items(&v);
            if list.is_empty() {
                println!("No members yet: only tokens use this tenant.");
                out.next("fluxctl fleet member-add alex@example.com --role admin");
            } else {
                let mut t = Table::new(vec!["EMAIL", "ROLE", "LAST SIGN-IN", "ADDED BY"]);
                for m in &list {
                    t.row(vec![
                        c(s(m, "email")),
                        role_cell(s(m, "role")),
                        ct(
                            time(m, "last_sign_in")
                                .map(rel)
                                .unwrap_or_else(|| "not yet".into()),
                            Tone::Dim,
                        ),
                        ct(s(m, "added_by"), Tone::Dim),
                    ]);
                }
                t.print(out);
            }
            if !sso {
                out.warn(
                    "single sign-on is not set up on this central (fluxvm-agent central --oidc-issuer), so members cannot sign in yet",
                );
            }
            Ok(())
        }
        FleetCommand::MemberAdd {
            email,
            role,
            tenant,
        } => {
            let v = call(
                central,
                token,
                Method::POST,
                &members_path(tenant.as_deref()),
                Some(json!({"email": email, "role": role})),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let m = v.get("member").cloned().unwrap_or_default();
            out.done(format!(
                "Added {} as {}. They can sign in to the console with SSO now.",
                out.bold(s(&m, "email")),
                s(&m, "role")
            ));
            Ok(())
        }
        FleetCommand::MemberRole {
            email,
            role,
            tenant,
        } => {
            let path = format!("{}/{}", members_path(tenant.as_deref()), esc(&email));
            let v = call(
                central,
                token,
                Method::PATCH,
                &path,
                Some(json!({ "role": role })),
            )
            .await
            .map_err(|e| {
                if status_of(&e) == Some(404) {
                    e.context(format!(
                        "{email} is not a member; add them with fluxctl fleet member-add"
                    ))
                } else {
                    e
                }
            })?;
            if !out.human {
                return out.json(&v);
            }
            let m = v.get("member").cloned().unwrap_or_default();
            out.done(format!(
                "{} is now {}. It applies to their open sessions at once.",
                out.bold(s(&m, "email")),
                s(&m, "role")
            ));
            Ok(())
        }
        FleetCommand::MemberRm { email, tenant, yes } => {
            if !yes
                && out.human
                && !confirm(&format!(
                    "Remove {email}? They are signed out of the console at once."
                ))?
            {
                bail!("{email} was not removed");
            }
            let path = format!("{}/{}", members_path(tenant.as_deref()), esc(&email));
            let v = call(central, token, Method::DELETE, &path, None).await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!(
                "Removed {}: they can no longer sign in.",
                s(v.get("removed").unwrap_or(&Value::Null), "email")
            ));
            Ok(())
        }
        _ => bail!("not a members command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_paths_escape_the_email() {
        assert_eq!(members_path(None), "/fleet/tenants/me/members");
        assert_eq!(members_path(Some("acme")), "/fleet/tenants/acme/members");
        assert_eq!(
            format!("{}/{}", members_path(None), esc("a+b@example.com")),
            "/fleet/tenants/me/members/a%2Bb%40example.com"
        );
    }

    /// The verbs against a real central: the operator invites, changes and removes a tenant's member.
    #[tokio::test]
    async fn add_change_list_and_remove_a_member() {
        let dir = tempfile::tempdir().unwrap();
        let app = fluxvm_agent::central::router(fluxvm_agent::central::CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("op".into()),
            reconcile_interval: None,
            ha: None,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let central = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let tok = Some("op");
        let quiet = Out {
            human: false,
            wide: false,
            color: false,
        };
        let created = call(
            &central,
            tok,
            Method::POST,
            "/fleet/tenants",
            Some(json!({"id": "acme"})),
        )
        .await
        .unwrap();
        let tenant_token = created["token"].as_str().unwrap().to_owned();
        let acme = || Some("acme".to_owned());
        members(
            &central,
            tok,
            &quiet,
            FleetCommand::MemberAdd {
                email: "Alex@Example.com".into(),
                role: "admin".into(),
                tenant: acme(),
            },
        )
        .await
        .unwrap();
        members(
            &central,
            tok,
            &quiet,
            FleetCommand::MemberRole {
                email: "alex@example.com".into(),
                role: "viewer".into(),
                tenant: acme(),
            },
        )
        .await
        .unwrap();
        // The tenant's own token sees the same list as `me`.
        let v = get(&central, Some(tenant_token.as_str()), &members_path(None))
            .await
            .unwrap();
        let list = items(&v);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["email"], "alex@example.com");
        assert_eq!(list[0]["role"], "viewer");
        assert_eq!(v["sso"], false);
        // An unknown member's role change says how to add them.
        let err = members(
            &central,
            tok,
            &quiet,
            FleetCommand::MemberRole {
                email: "ghost@example.com".into(),
                role: "admin".into(),
                tenant: acme(),
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("member-add"), "{err:#}");
        members(
            &central,
            tok,
            &quiet,
            FleetCommand::MemberRm {
                email: "alex@example.com".into(),
                tenant: acme(),
                yes: true,
            },
        )
        .await
        .unwrap();
        let v = get(&central, tok, &members_path(Some("acme")))
            .await
            .unwrap();
        assert!(items(&v).is_empty());
    }
}
