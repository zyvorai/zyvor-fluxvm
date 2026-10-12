// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Team members and console sessions: the people of a tenant who sign in with SSO, their roles, and the sessions
//! central issues after a sign-in. Pure state and rules (no HTTP), like the rest of [`super`].
//!
//! A member is invited by email; the first SSO sign-in with that (verified) email gets a session for the tenant. The
//! session stores only who signed in and for which tenant: the role is looked up again on every request, so changing
//! or removing a member takes effect at once.

use super::*;

/// Most members one tenant may have.
pub const MAX_MEMBERS: usize = 500;
/// Most live sessions kept in all; the oldest go first.
const MAX_SESSIONS: usize = 10_000;
/// Most live sessions per person; signing in again past this ends the oldest.
const MAX_SESSIONS_PER_EMAIL: usize = 20;
/// Longest email accepted (RFC 5321).
const MAX_EMAIL_LEN: usize = 254;

/// What a member may do, from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// Looks at everything, changes nothing.
    Viewer,
    /// Manages VMs, volumes, snapshots and networks; not leases, API keys, members, the budget or webhooks.
    Member,
    /// Every tenant action, including members, except anything about owners.
    Admin,
    /// Everything.
    Owner,
}

impl MemberRole {
    pub fn parse(s: &str) -> Option<MemberRole> {
        match s.trim().to_ascii_lowercase().as_str() {
            "viewer" => Some(MemberRole::Viewer),
            "member" => Some(MemberRole::Member),
            "admin" => Some(MemberRole::Admin),
            "owner" => Some(MemberRole::Owner),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MemberRole::Viewer => "viewer",
            MemberRole::Member => "member",
            MemberRole::Admin => "admin",
            MemberRole::Owner => "owner",
        }
    }
}

/// A person of a tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Lowercase; matched against the verified email of an SSO sign-in.
    pub email: String,
    pub role: MemberRole,
    pub added_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sign_in: Option<DateTime<Utc>>,
    /// Who added them (an email, `operator` or `token`), for the members list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
}

impl Member {
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "email": self.email,
            "role": self.role,
            "added_at": self.added_at,
            "last_sign_in": self.last_sign_in,
            "added_by": self.added_by,
        })
    }
}

/// A console session after an SSO sign-in. Keyed by the SHA-256 of its token in [`TenancyState::sessions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub email: String,
    /// The tenant it acts for; `None` is the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// `email` trimmed and lowercased, or `None` when it does not look like an address.
pub fn normalize_email(email: &str) -> Option<String> {
    let e = email.trim().to_ascii_lowercase();
    if e.is_empty() || e.len() > MAX_EMAIL_LEN {
        return None;
    }
    let (local, domain) = e.split_once('@')?;
    let ok_chars = e.chars().all(|c| {
        !c.is_whitespace()
            && !c.is_control()
            && !matches!(c, '<' | '>' | '"' | '\\' | ',' | ';' | '/')
    });
    let ok_domain = domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && !domain.contains('@');
    (ok_chars && !local.is_empty() && ok_domain).then_some(e)
}

/// Why a member change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberError {
    UnknownTenant(String),
    UnknownMember(String),
    /// Bad input (400).
    Invalid(String),
    /// Refused by the rules (409): already a member, the last owner, too many members.
    Conflict(String),
}

impl MemberError {
    /// The HTTP status central answers with.
    pub fn status(&self) -> u16 {
        match self {
            MemberError::UnknownTenant(_) | MemberError::UnknownMember(_) => 404,
            MemberError::Invalid(_) => 400,
            MemberError::Conflict(_) => 409,
        }
    }
}

impl std::fmt::Display for MemberError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemberError::UnknownTenant(t) => write!(f, "no tenant '{t}'"),
            MemberError::UnknownMember(e) => write!(f, "{e} is not a member"),
            MemberError::Invalid(m) | MemberError::Conflict(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for MemberError {}

impl TenancyState {
    fn tenant_mut(&mut self, id: &str) -> Result<&mut Tenant, MemberError> {
        self.tenants
            .get_mut(id)
            .ok_or_else(|| MemberError::UnknownTenant(id.into()))
    }

    /// The member `email` of `tenant`, if any.
    pub fn member(&self, tenant: &str, email: &str) -> Option<&Member> {
        let email = email.to_ascii_lowercase();
        self.tenants
            .get(tenant)?
            .members
            .iter()
            .find(|m| m.email == email)
    }

    /// Every tenant `email` belongs to, with its role there, by tenant id.
    pub fn memberships(&self, email: &str) -> Vec<(String, MemberRole)> {
        let email = email.to_ascii_lowercase();
        self.tenants
            .values()
            .filter_map(|t| {
                t.members
                    .iter()
                    .find(|m| m.email == email)
                    .map(|m| (t.id.clone(), m.role))
            })
            .collect()
    }

    /// The SSO sign-in of `email`, who belongs to no tenant yet but signed one up (`owner_email`, self-service
    /// sign-up): makes them that tenant's owner and returns its id. `None` (nothing changed) otherwise. Only a tenant
    /// without members adopts: once it has a team, a removed sign-up email must not come back as an owner.
    pub fn adopt_signup_owner(&mut self, email: &str, now: DateTime<Utc>) -> Option<String> {
        if !self.memberships(email).is_empty() {
            return None;
        }
        let id = self
            .tenants
            .values()
            .find(|t| {
                t.members.is_empty()
                    && t.owner_email
                        .as_deref()
                        .is_some_and(|o| o.trim().eq_ignore_ascii_case(email.trim()))
            })?
            .id
            .clone();
        self.add_member(&id, email, MemberRole::Owner, Some("signup".into()), now)
            .ok()?;
        // Without email confirmation, sign-up did not verify the address, so whoever holds the sign-up token may not
        // be this person (anyone can sign up with someone else's email). Now that the verified owner is in, what that
        // token could have left behind goes: the token itself, named keys and webhooks. The owner mints new ones from
        // the console. A sign-up that confirmed its address by a mailed link already belongs to this person.
        let confirmed = self
            .tenants
            .get(&id)
            .is_some_and(|t| t.owner_email_verified);
        if !confirmed {
            let _ = self.rotate_token(&id);
            if let Some(t) = self.tenants.get_mut(&id) {
                t.keys.clear();
            }
            self.webhooks
                .retain(|w| w.owner.as_deref() != Some(id.as_str()));
        }
        Some(id)
    }

    /// Invites `email` to `tenant` with `role`: from now on that person can sign in with SSO.
    pub fn add_member(
        &mut self,
        tenant: &str,
        email: &str,
        role: MemberRole,
        added_by: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<Member, MemberError> {
        let email = normalize_email(email).ok_or_else(|| {
            MemberError::Invalid(format!("'{}' is not an email address", email.trim()))
        })?;
        let t = self.tenant_mut(tenant)?;
        if t.members.iter().any(|m| m.email == email) {
            return Err(MemberError::Conflict(format!(
                "{email} is already a member; change their role instead"
            )));
        }
        if t.members.len() >= MAX_MEMBERS {
            return Err(MemberError::Conflict(format!(
                "a tenant can have at most {MAX_MEMBERS} members; remove one first"
            )));
        }
        let m = Member {
            email,
            role,
            added_at: now,
            last_sign_in: None,
            added_by,
        };
        t.members.push(m.clone());
        Ok(m)
    }

    /// Changes a member's role and returns the member with the role they had before.
    pub fn set_member_role(
        &mut self,
        tenant: &str,
        email: &str,
        role: MemberRole,
    ) -> Result<(Member, MemberRole), MemberError> {
        let email = email.to_ascii_lowercase();
        let t = self.tenant_mut(tenant)?;
        let owners = t
            .members
            .iter()
            .filter(|m| m.role == MemberRole::Owner)
            .count();
        let m = t
            .members
            .iter_mut()
            .find(|m| m.email == email)
            .ok_or_else(|| MemberError::UnknownMember(email.clone()))?;
        let before = m.role;
        if before == MemberRole::Owner && role != MemberRole::Owner && owners == 1 {
            return Err(MemberError::Conflict(format!(
                "{email} is the only owner; make someone else an owner first"
            )));
        }
        m.role = role;
        Ok((m.clone(), before))
    }

    /// Removes a member and ends their sessions for this tenant.
    pub fn remove_member(&mut self, tenant: &str, email: &str) -> Result<Member, MemberError> {
        let email = email.to_ascii_lowercase();
        let t = self.tenant_mut(tenant)?;
        let i = t
            .members
            .iter()
            .position(|m| m.email == email)
            .ok_or_else(|| MemberError::UnknownMember(email.clone()))?;
        let owners = t
            .members
            .iter()
            .filter(|m| m.role == MemberRole::Owner)
            .count();
        if t.members[i].role == MemberRole::Owner && owners == 1 && t.members.len() > 1 {
            return Err(MemberError::Conflict(format!(
                "{email} is the only owner; make someone else an owner first"
            )));
        }
        let m = t.members.remove(i);
        self.sessions
            .retain(|_, s| !(s.email == email && s.tenant.as_deref() == Some(tenant)));
        Ok(m)
    }

    /// Starts a session for `email` acting for `tenant` (`None`: the operator) and returns its one-time token. Expired
    /// sessions are dropped first, and the oldest go when a person or the whole fleet holds too many.
    pub fn create_session(
        &mut self,
        email: &str,
        tenant: Option<&str>,
        ttl: Duration,
        now: DateTime<Utc>,
    ) -> String {
        self.sessions.retain(|_, s| s.expires_at > now);
        let email = email.to_ascii_lowercase();
        let mut mine: Vec<(DateTime<Utc>, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.email == email)
            .map(|(k, s)| (s.created_at, k.clone()))
            .collect();
        if mine.len() >= MAX_SESSIONS_PER_EMAIL {
            mine.sort();
            for (_, k) in mine.iter().take(mine.len() + 1 - MAX_SESSIONS_PER_EMAIL) {
                self.sessions.remove(k);
            }
        }
        while self.sessions.len() >= MAX_SESSIONS {
            let oldest = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.created_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.sessions.remove(&k);
                }
                None => break,
            }
        }
        let token = format!(
            "fvs_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.sessions.insert(
            hash_token(&token),
            Session {
                email: email.clone(),
                tenant: tenant.map(str::to_owned),
                created_at: now,
                expires_at: now + ttl,
            },
        );
        if let Some(t) = tenant
            && let Some(m) = self
                .tenants
                .get_mut(t)
                .and_then(|t| t.members.iter_mut().find(|m| m.email == email))
        {
            m.last_sign_in = Some(now);
        }
        token
    }

    /// The live session behind `token`, if it has not expired.
    pub fn session(&self, token: &str, now: DateTime<Utc>) -> Option<&Session> {
        let h = hash_token(token);
        self.sessions.get(&h).filter(|s| s.expires_at > now)
    }

    /// Ends the session behind `token`; true when there was one.
    pub fn end_session(&mut self, token: &str) -> bool {
        self.sessions.remove(&hash_token(token)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn state() -> TenancyState {
        let mut s = TenancyState::default();
        s.create_tenant("acme", None, Quota::default(), t0())
            .unwrap();
        s.create_tenant("globex", None, Quota::default(), t0())
            .unwrap();
        s
    }

    #[test]
    fn emails_are_normalized_and_checked() {
        assert_eq!(
            normalize_email("  Alice@Example.COM ").as_deref(),
            Some("alice@example.com")
        );
        for bad in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice@localhost",
            "a b@example.com",
            "alice@example..com",
            "<alice@example.com>",
            "a@b@example.com",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad}");
        }
        assert_eq!(
            normalize_email(&format!("{}@example.com", "a".repeat(250))),
            None
        );
    }

    #[test]
    fn a_sign_up_owner_becomes_the_owner_on_first_sso_sign_in() {
        let mut s = state();
        s.tenants.get_mut("acme").unwrap().owner_email = Some("Ops@Acme.io".into());
        let before_token = s.tenants["acme"].token_sha256.clone();
        s.webhooks.push(Webhook {
            id: "whk_x".into(),
            owner: Some("acme".into()),
            url: "https://example.com/h".into(),
            events: vec![],
            secret: "s".into(),
            created_at: t0(),
        });
        assert_eq!(s.adopt_signup_owner("bob@acme.io", t0()), None);
        assert_eq!(
            s.adopt_signup_owner("ops@acme.io", t0()).as_deref(),
            Some("acme")
        );
        assert_eq!(
            s.member("acme", "ops@acme.io").map(|m| m.role),
            Some(MemberRole::Owner)
        );
        // The unverified sign-up token (and anything it made) no longer works.
        assert_ne!(s.tenants["acme"].token_sha256, before_token);
        assert!(s.tenants["acme"].keys.is_empty());
        assert!(
            s.webhooks
                .iter()
                .all(|w| w.owner.as_deref() != Some("acme"))
        );
        // Already a member somewhere: nothing changes, their role stays what an admin made it.
        s.set_member_role("acme", "ops@acme.io", MemberRole::Owner)
            .unwrap();
        assert_eq!(s.adopt_signup_owner("ops@acme.io", t0()), None);
        // Once the team has other people, a removed sign-up email does not come back as an owner.
        s.add_member("acme", "bob@acme.io", MemberRole::Owner, None, t0())
            .unwrap();
        s.remove_member("acme", "ops@acme.io").unwrap();
        assert_eq!(s.adopt_signup_owner("ops@acme.io", t0()), None);
        assert!(s.member("acme", "ops@acme.io").is_none());
    }

    /// A sign-up that confirmed its address by a mailed link keeps its token, keys and webhooks when its owner first
    /// signs in with SSO: the person holding them is already that person.
    #[test]
    fn a_confirmed_sign_up_keeps_its_token_on_adoption() {
        let mut s = state();
        {
            let t = s.tenants.get_mut("acme").unwrap();
            t.owner_email = Some("ops@acme.io".into());
            t.owner_email_verified = true;
        }
        let before = s.tenants["acme"].token_sha256.clone();
        assert_eq!(
            s.adopt_signup_owner("ops@acme.io", t0()).as_deref(),
            Some("acme")
        );
        assert_eq!(s.tenants["acme"].token_sha256, before);
        assert_eq!(
            s.member("acme", "ops@acme.io").map(|m| m.role),
            Some(MemberRole::Owner)
        );
    }

    #[test]
    fn members_are_added_once_and_found_case_insensitively() {
        let mut s = state();
        let m = s
            .add_member("acme", "Alice@Example.com", MemberRole::Admin, None, t0())
            .unwrap();
        assert_eq!(m.email, "alice@example.com");
        assert!(
            s.add_member("acme", "alice@example.com", MemberRole::Viewer, None, t0())
                .is_err()
        );
        assert!(
            s.add_member("nope", "bob@example.com", MemberRole::Viewer, None, t0())
                .is_err()
        );
        assert!(
            s.add_member("acme", "not-an-email", MemberRole::Viewer, None, t0())
                .is_err()
        );
        assert_eq!(
            s.member("acme", "ALICE@example.com").map(|m| m.role),
            Some(MemberRole::Admin)
        );
        assert!(s.member("globex", "alice@example.com").is_none());
        s.add_member(
            "globex",
            "alice@example.com",
            MemberRole::Viewer,
            None,
            t0(),
        )
        .unwrap();
        assert_eq!(
            s.memberships("alice@example.com"),
            vec![
                ("acme".to_owned(), MemberRole::Admin),
                ("globex".to_owned(), MemberRole::Viewer)
            ]
        );
    }

    #[test]
    fn the_last_owner_stays() {
        let mut s = state();
        s.add_member("acme", "o@example.com", MemberRole::Owner, None, t0())
            .unwrap();
        s.add_member("acme", "m@example.com", MemberRole::Member, None, t0())
            .unwrap();
        assert!(
            s.set_member_role("acme", "o@example.com", MemberRole::Admin)
                .is_err()
        );
        assert!(s.remove_member("acme", "o@example.com").is_err());
        let (m, before) = s
            .set_member_role("acme", "m@example.com", MemberRole::Owner)
            .unwrap();
        assert_eq!((m.role, before), (MemberRole::Owner, MemberRole::Member));
        s.set_member_role("acme", "o@example.com", MemberRole::Viewer)
            .unwrap();
        s.remove_member("acme", "o@example.com").unwrap();
        assert!(s.member("acme", "o@example.com").is_none());
        assert!(s.remove_member("acme", "o@example.com").is_err());
    }

    #[test]
    fn sessions_expire_and_end() {
        let mut s = state();
        s.add_member("acme", "a@example.com", MemberRole::Member, None, t0())
            .unwrap();
        let tok = s.create_session("A@example.com", Some("acme"), Duration::hours(12), t0());
        assert!(tok.starts_with("fvs_"));
        // Only the hash is stored.
        assert!(!s.sessions.contains_key(&tok));
        let sess = s.session(&tok, t0() + Duration::hours(11)).unwrap();
        assert_eq!(sess.email, "a@example.com");
        assert_eq!(sess.tenant.as_deref(), Some("acme"));
        assert_eq!(
            s.member("acme", "a@example.com").unwrap().last_sign_in,
            Some(t0())
        );
        assert!(s.session(&tok, t0() + Duration::hours(12)).is_none());
        assert!(s.session("fvs_other", t0()).is_none());
        // A new sign-in drops expired sessions.
        let tok2 = s.create_session(
            "b@example.com",
            None,
            Duration::hours(12),
            t0() + Duration::hours(13),
        );
        assert_eq!(s.sessions.len(), 1);
        assert!(s.end_session(&tok2));
        assert!(!s.end_session(&tok2));
    }

    #[test]
    fn removing_a_member_ends_their_sessions_and_sessions_are_capped() {
        let mut s = state();
        s.add_member("acme", "a@example.com", MemberRole::Member, None, t0())
            .unwrap();
        let tok = s.create_session("a@example.com", Some("acme"), Duration::hours(12), t0());
        let op = s.create_session("a@example.com", None, Duration::hours(12), t0());
        s.remove_member("acme", "a@example.com").unwrap();
        assert!(s.session(&tok, t0()).is_none());
        assert!(
            s.session(&op, t0()).is_some(),
            "only the tenant's sessions end"
        );
        let first = s.create_session("c@example.com", None, Duration::hours(12), t0());
        for i in 1..=MAX_SESSIONS_PER_EMAIL as i64 {
            s.create_session(
                "c@example.com",
                None,
                Duration::hours(12),
                t0() + Duration::seconds(i),
            );
        }
        let mine = s
            .sessions
            .values()
            .filter(|x| x.email == "c@example.com")
            .count();
        assert_eq!(mine, MAX_SESSIONS_PER_EMAIL);
        assert!(s.session(&first, t0()).is_none(), "the oldest went first");
    }

    #[test]
    fn old_state_files_load_without_members_or_sessions() {
        let s: TenancyState =
            serde_json::from_str(r#"{"tenants":{"a":{"id":"a","token_sha256":"x","created_at":"2026-10-01T00:00:00Z"}}}"#)
                .unwrap();
        assert!(s.tenants["a"].members.is_empty());
        assert!(s.sessions.is_empty());
        assert!(s.policy.operator_emails.is_empty());
        // And nothing new is written for a fleet that does not use SSO.
        let raw = serde_json::to_string(&s).unwrap();
        assert!(
            !raw.contains("members")
                && !raw.contains("sessions")
                && !raw.contains("operator_emails")
        );
    }
}
