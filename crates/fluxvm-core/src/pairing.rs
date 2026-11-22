// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Pairing: a machine asks to join a fleet, a person approves it, and the machine receives its node credential without
//! anyone typing a URL or a token. See docs/cluster-pairing.md for the protocol and the threat model.
//!
//! This module is pure (no sockets, no files, an explicit clock) so the state machine is tested with fakes. It holds both
//! halves: the machine's keys and proofs ([`NodeKeys`]) and the server's book of requests ([`PairBook`]). The credential
//! itself is issued by the *existing* enrollment code: [`PairBook::approve`] takes a closure that calls it.
//!
//! ```text
//! machine                                    central
//!   | -- POST /fleet/pair/request ---------->  name, ed25519 pub, x25519 pub          (nothing is granted)
//!   | <- id, central's one-time x25519 pub --
//!   |  both sides show CODE = H(keys, id)       the person compares the two and clicks Approve
//!   | -- POST /fleet/pair/claim (signed) --->  pending ... pending ... approved
//!   | <- credential sealed to the machine ---  X25519 + HKDF + AES-256-GCM, delivered once
//! ```

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use ring::{aead, agreement, hkdf, rand::SecureRandom, rand::SystemRandom};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};

/// Crockford base 32: no I, L, O or U, so a code read aloud or off a screen is not ambiguous.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Ten characters are 50 bits. The code is not a password (it unlocks nothing by itself); it is a fingerprint both
/// screens must agree on, so it is long enough that grinding a colliding key pair inside the expiry is not practical.
pub const CODE_LEN: usize = 10;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    SystemRandom::new()
        .fill(&mut b)
        .expect("the operating system has no randomness");
    b
}

/// `ABCDE-FGHJK` from the two public keys, the request id and the server's one-time key. Changing any of them changes the
/// code, so a relay that substitutes a key on either side makes the two screens disagree.
pub fn code_for(ed_pub: &[u8; 32], x_pub: &[u8; 32], id: &str, server_pub: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"fluxvm-pair-code-v1");
    h.update(ed_pub);
    h.update(x_pub);
    h.update(id.as_bytes());
    h.update(server_pub);
    let d = h.finalize();
    let mut bits = u64::from_be_bytes(d[..8].try_into().expect("8 bytes"));
    let mut s = String::with_capacity(CODE_LEN + 1);
    for i in 0..CODE_LEN {
        if i == CODE_LEN / 2 {
            s.push('-');
        }
        s.push(ALPHABET[(bits >> 59) as usize & 31] as char);
        bits <<= 5;
    }
    s
}

/// What a person typed, made comparable: upper case, no dashes or spaces, and the look-alikes mapped (O to 0, I and L to 1).
pub fn normalize_code(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// The id central knows this machine by. Derived from its public key, so it cannot be chosen.
pub fn node_id_for(ed_pub: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"fluxvm-node-id-v1");
    h.update(ed_pub);
    hex(&h.finalize())[..32].to_owned()
}

fn claim_message(id: &str, server_pub: &[u8; 32]) -> Vec<u8> {
    format!("fluxvm-pair-claim-v1|{id}|{}", hex(server_pub)).into_bytes()
}

struct Len32;
impl hkdf::KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn seal_key(shared: &[u8], id: &str) -> aead::LessSafeKey {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, id.as_bytes()).extract(shared);
    let okm = prk
        .expand(&[b"fluxvm-pair-seal-v1"], Len32)
        .expect("32 bytes");
    let mut key = [0u8; 32];
    okm.fill(&mut key).expect("32 bytes");
    aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM, &key).expect("32 byte key"))
}

/// One X25519 key pair for one attempt; the private half is consumed by the agreement, so it cannot be reused.
pub struct EphemeralX {
    private: agreement::EphemeralPrivateKey,
    pub public: [u8; 32],
}

impl EphemeralX {
    pub fn generate() -> Self {
        let rng = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
            .expect("the operating system has no randomness");
        let public: [u8; 32] = private
            .compute_public_key()
            .expect("x25519 public key")
            .as_ref()
            .try_into()
            .expect("32 bytes");
        EphemeralX { private, public }
    }

    /// Seals `plain` to `peer_pub`, bound to `id`. Output: 12-byte nonce, ciphertext, tag.
    pub fn seal(self, peer_pub: &[u8; 32], id: &str, plain: &[u8]) -> Option<Vec<u8>> {
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, peer_pub);
        let key = agreement::agree_ephemeral(self.private, &peer, |s| seal_key(s, id)).ok()?;
        let nonce: [u8; 12] = random();
        let mut buf = plain.to_vec();
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(id.as_bytes()),
            &mut buf,
        )
        .ok()?;
        let mut out = nonce.to_vec();
        out.extend(buf);
        Some(out)
    }

    /// The inverse of [`seal`](Self::seal); `None` for anything not sealed to this key and id.
    pub fn open(self, peer_pub: &[u8; 32], id: &str, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < 12 + 16 {
            return None;
        }
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, peer_pub);
        let key = agreement::agree_ephemeral(self.private, &peer, |s| seal_key(s, id)).ok()?;
        let nonce: [u8; 12] = sealed[..12].try_into().ok()?;
        let mut buf = sealed[12..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(id.as_bytes()),
                &mut buf,
            )
            .ok()?;
        Some(plain.to_vec())
    }
}

/// The machine's side: a long-lived signing key (its identity, kept 0600 by the caller) and a per-attempt X25519 key.
pub struct NodeKeys {
    signing: SigningKey,
    x: EphemeralX,
}

impl NodeKeys {
    /// `seed` is the 32 bytes the caller persisted; `None` makes a new identity (persist [`NodeKeys::seed`]).
    pub fn new(seed: Option<[u8; 32]>) -> Self {
        NodeKeys {
            signing: SigningKey::from_bytes(&seed.unwrap_or_else(random)),
            x: EphemeralX::generate(),
        }
    }
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }
    pub fn ed_pub(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }
    pub fn x_pub(&self) -> [u8; 32] {
        self.x.public
    }
    pub fn node_id(&self) -> String {
        node_id_for(&self.ed_pub())
    }
    pub fn code(&self, id: &str, server_pub: &[u8; 32]) -> String {
        code_for(&self.ed_pub(), &self.x_pub(), id, server_pub)
    }
    /// Proof that the asker is the machine that made the request; hex.
    pub fn claim_signature(&self, id: &str, server_pub: &[u8; 32]) -> String {
        hex(&self.signing.sign(&claim_message(id, server_pub)).to_bytes())
    }
    /// Opens the credential central sealed to this attempt; consumes the keys.
    pub fn open_credential(self, id: &str, server_pub: &[u8; 32], sealed: &[u8]) -> Option<String> {
        String::from_utf8(self.x.open(server_pub, id, sealed)?).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The request is malformed (name, key sizes).
    Invalid(&'static str),
    /// Too many open requests or too many failures; retry after this many seconds.
    Busy(u64),
    NotFound,
    /// The request ran out of time.
    Expired,
    /// Already approved, denied or delivered.
    NotPending,
    /// The signature does not match the machine that asked.
    BadProof,
    /// Issuing the credential failed (for example the name is taken by another machine).
    Issue(String),
}

#[derive(Debug, Clone, Copy)]
pub struct PairConfig {
    pub ttl_secs: i64,
    pub max_open: usize,
    pub max_open_per_ip: usize,
    /// Failed claims (unknown id, bad signature) one address may make per minute.
    pub max_failures_per_min: usize,
    /// Minimum seconds between claims of one request.
    pub claim_interval_secs: i64,
}

impl Default for PairConfig {
    fn default() -> Self {
        PairConfig {
            ttl_secs: 600,
            max_open: 16,
            max_open_per_ip: 3,
            max_failures_per_min: 20,
            claim_interval_secs: 1,
        }
    }
}

enum State {
    Pending,
    Approved(Vec<u8>),
    Denied,
    Delivered,
}

struct Entry {
    name: String,
    ed_pub: [u8; 32],
    x_pub: [u8; 32],
    server: Option<EphemeralX>,
    server_pub: [u8; 32],
    code: String,
    ip: String,
    created: i64,
    last_claim: i64,
    state: State,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Offer {
    pub id: String,
    /// Central's one-time X25519 key, hex.
    pub server_pub: String,
    pub expires_in: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PendingView {
    pub id: String,
    pub name: String,
    pub code: String,
    pub address: String,
    pub age_secs: i64,
    pub expires_in: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Pending,
    Denied,
    /// The sealed credential, once.
    Approved(Vec<u8>),
}

/// The server's book of join requests. Memory only: a request does not outlive a restart of central, by design.
pub struct PairBook {
    cfg: PairConfig,
    entries: HashMap<String, Entry>,
    failures: HashMap<String, VecDeque<i64>>,
}

impl Default for PairBook {
    fn default() -> Self {
        Self::new(PairConfig::default())
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && !n.starts_with(['-', '.'])
        && n.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}

impl PairBook {
    pub fn new(cfg: PairConfig) -> Self {
        PairBook {
            cfg,
            entries: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    fn gc(&mut self, now: i64) {
        let ttl = self.cfg.ttl_secs;
        self.entries.retain(|_, e| now - e.created < ttl);
        self.failures.retain(|_, q| {
            while q.front().is_some_and(|t| now - t >= 60) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    fn fail(&mut self, ip: &str, now: i64) {
        self.failures
            .entry(ip.to_owned())
            .or_default()
            .push_back(now);
    }

    fn blocked(&self, ip: &str) -> bool {
        self.failures
            .get(ip)
            .is_some_and(|q| q.len() >= self.cfg.max_failures_per_min)
    }

    /// A machine asks to join. Grants nothing: it only creates something for a person to look at.
    pub fn request(
        &mut self,
        now: i64,
        ip: &str,
        name: &str,
        ed_pub_hex: &str,
        x_pub_hex: &str,
    ) -> Result<Offer, PairError> {
        self.gc(now);
        if !valid_name(name) {
            return Err(PairError::Invalid(
                "name must be lowercase letters, digits, dashes and dots (at most 63)",
            ));
        }
        let ed_pub: [u8; 32] = unhex(ed_pub_hex).ok_or(PairError::Invalid("bad public key"))?;
        let x_pub: [u8; 32] = unhex(x_pub_hex).ok_or(PairError::Invalid("bad public key"))?;
        VerifyingKey::from_bytes(&ed_pub).map_err(|_| PairError::Invalid("bad public key"))?;
        // The same machine asking again (a lost response) gets its own request back, not a second one.
        if let Some((id, e)) = self.entries.iter().find(|(_, e)| {
            e.ed_pub == ed_pub && e.x_pub == x_pub && matches!(e.state, State::Pending)
        }) {
            return Ok(Offer {
                id: id.clone(),
                server_pub: hex(&e.server_pub),
                expires_in: self.cfg.ttl_secs - (now - e.created),
            });
        }
        if self.entries.len() >= self.cfg.max_open
            || self.entries.values().filter(|e| e.ip == ip).count() >= self.cfg.max_open_per_ip
        {
            return Err(PairError::Busy(30));
        }
        let id = hex(&random::<16>());
        let server = EphemeralX::generate();
        let server_pub = server.public;
        let code = code_for(&ed_pub, &x_pub, &id, &server_pub);
        self.entries.insert(
            id.clone(),
            Entry {
                name: name.to_owned(),
                ed_pub,
                x_pub,
                server: Some(server),
                server_pub,
                code,
                ip: ip.to_owned(),
                created: now,
                last_claim: 0,
                state: State::Pending,
            },
        );
        Ok(Offer {
            id,
            server_pub: hex(&server_pub),
            expires_in: self.cfg.ttl_secs,
        })
    }

    /// Requests waiting for a person.
    pub fn pending(&mut self, now: i64) -> Vec<PendingView> {
        self.gc(now);
        let mut v: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| matches!(e.state, State::Pending))
            .map(|(id, e)| PendingView {
                id: id.clone(),
                name: e.name.clone(),
                code: e.code.clone(),
                address: e.ip.clone(),
                age_secs: now - e.created,
                expires_in: self.cfg.ttl_secs - (now - e.created),
            })
            .collect();
        v.sort_by_key(|p| p.age_secs);
        v
    }

    fn find(&self, key: &str) -> Option<String> {
        if self.entries.contains_key(key) {
            return Some(key.to_owned());
        }
        let want = normalize_code(key);
        if want.len() != CODE_LEN {
            return None;
        }
        self.entries
            .iter()
            .find(|(_, e)| normalize_code(&e.code) == want)
            .map(|(id, _)| id.clone())
    }

    /// A person approved the request named by id or by the code on the machine's screen. `issue(name, node_id)` returns
    /// the credential (central passes the existing enrollment). The credential is sealed to the machine's key and kept
    /// until the machine collects it, once.
    pub fn approve(
        &mut self,
        now: i64,
        key: &str,
        issue: impl FnOnce(&str, &str) -> Result<String, String>,
    ) -> Result<PendingView, PairError> {
        self.gc(now);
        let id = self.find(key).ok_or(PairError::NotFound)?;
        let e = self.entries.get_mut(&id).ok_or(PairError::NotFound)?;
        if !matches!(e.state, State::Pending) {
            return Err(PairError::NotPending);
        }
        let node_id = node_id_for(&e.ed_pub);
        let cred = issue(&e.name, &node_id).map_err(PairError::Issue)?;
        let server = e.server.take().ok_or(PairError::NotPending)?;
        let sealed = server
            .seal(&e.x_pub, &id, cred.as_bytes())
            .ok_or(PairError::Invalid("could not seal the credential"))?;
        e.state = State::Approved(sealed);
        Ok(PendingView {
            id,
            name: e.name.clone(),
            code: e.code.clone(),
            address: e.ip.clone(),
            age_secs: now - e.created,
            expires_in: self.cfg.ttl_secs - (now - e.created),
        })
    }

    pub fn deny(&mut self, now: i64, key: &str) -> Result<(), PairError> {
        self.gc(now);
        let id = self.find(key).ok_or(PairError::NotFound)?;
        let e = self.entries.get_mut(&id).ok_or(PairError::NotFound)?;
        if !matches!(e.state, State::Pending) {
            return Err(PairError::NotPending);
        }
        e.state = State::Denied;
        e.server = None;
        Ok(())
    }

    /// The machine asks what became of its request. `sig_hex` proves it holds the key it asked with.
    pub fn claim(
        &mut self,
        now: i64,
        ip: &str,
        id: &str,
        sig_hex: &str,
    ) -> Result<Claim, PairError> {
        self.gc(now);
        if self.blocked(ip) {
            return Err(PairError::Busy(60));
        }
        let Some(e) = self.entries.get_mut(id) else {
            self.fail(ip, now);
            return Err(PairError::NotFound);
        };
        let ok = unhex::<64>(sig_hex).is_some_and(|s| {
            VerifyingKey::from_bytes(&e.ed_pub)
                .and_then(|k| {
                    k.verify(
                        &claim_message(id, &e.server_pub),
                        &Signature::from_bytes(&s),
                    )
                })
                .is_ok()
        });
        if !ok {
            self.fail(ip, now);
            return Err(PairError::BadProof);
        }
        if now - e.last_claim < self.cfg.claim_interval_secs {
            return Err(PairError::Busy(self.cfg.claim_interval_secs as u64));
        }
        e.last_claim = now;
        match &e.state {
            State::Pending => Ok(Claim::Pending),
            State::Denied => Ok(Claim::Denied),
            State::Delivered => Err(PairError::NotPending),
            State::Approved(_) => {
                let State::Approved(sealed) = std::mem::replace(&mut e.state, State::Delivered)
                else {
                    unreachable!()
                };
                Ok(Claim::Approved(sealed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn ask(book: &mut PairBook, keys: &NodeKeys, ip: &str, name: &str) -> Result<Offer, PairError> {
        book.request(NOW, ip, name, &hex(&keys.ed_pub()), &hex(&keys.x_pub()))
    }

    #[test]
    fn the_whole_flow_delivers_the_credential_once_and_only_to_the_machine() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let sp: [u8; 32] = unhex(&offer.server_pub).unwrap();
        let sig = keys.claim_signature(&offer.id, &sp);
        assert_eq!(
            book.claim(NOW + 2, "10.0.0.7", &offer.id, &sig),
            Ok(Claim::Pending)
        );
        // Both screens show the same code.
        let shown = book.pending(NOW + 2)[0].clone();
        assert_eq!(shown.code, keys.code(&offer.id, &sp));
        assert_eq!(shown.name, "mini-1");
        // The person types the code from the machine's screen, in any case and spacing.
        let typed = shown.code.to_lowercase().replace('-', " ");
        let mut issued = None;
        book.approve(NOW + 3, &typed, |name, node_id| {
            issued = Some((name.to_owned(), node_id.to_owned()));
            Ok("fvnc_secret.1.mac".into())
        })
        .unwrap();
        assert_eq!(issued.unwrap(), ("mini-1".into(), keys.node_id()));
        let Ok(Claim::Approved(sealed)) = book.claim(NOW + 5, "10.0.0.7", &offer.id, &sig) else {
            panic!("expected approval");
        };
        assert!(
            !sealed.windows(5).any(|w| w == b"fvnc_"),
            "sealed, not plain"
        );
        assert_eq!(
            keys.open_credential(&offer.id, &sp, &sealed).as_deref(),
            Some("fvnc_secret.1.mac")
        );
        // Single use: a replay of the same claim gets nothing.
        assert_eq!(
            book.claim(NOW + 7, "10.0.0.7", &offer.id, &sig),
            Err(PairError::NotPending)
        );
    }

    #[test]
    fn another_machine_cannot_collect_or_open_the_credential() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let thief = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let sp: [u8; 32] = unhex(&offer.server_pub).unwrap();
        book.approve(NOW, &offer.id, |_, _| Ok("fvnc_x.1.y".into()))
            .unwrap();
        // A shoulder-surfed code or a sniffed id is useless without the key: the claim must be signed.
        let bad = thief.claim_signature(&offer.id, &sp);
        assert_eq!(
            book.claim(NOW + 2, "10.0.0.9", &offer.id, &bad),
            Err(PairError::BadProof)
        );
        // The real machine still gets it.
        let good = keys.claim_signature(&offer.id, &sp);
        let Ok(Claim::Approved(sealed)) = book.claim(NOW + 4, "10.0.0.7", &offer.id, &good) else {
            panic!()
        };
        // And a sniffer who captured the sealed bytes cannot open them with another key.
        assert!(thief.open_credential(&offer.id, &sp, &sealed).is_none());
    }

    #[test]
    fn a_substituted_key_changes_the_code() {
        let a = NodeKeys::new(None);
        let b = NodeKeys::new(None);
        let sp = [7u8; 32];
        assert_ne!(a.code("id", &sp), b.code("id", &sp));
        assert_ne!(a.code("id", &sp), a.code("id2", &sp));
        assert_ne!(a.code("id", &sp), a.code("id", &[8u8; 32]));
        let c = a.code("id", &sp);
        assert_eq!(c.len(), CODE_LEN + 1);
        assert!(
            c.chars()
                .all(|ch| ch == '-' || ALPHABET.contains(&(ch as u8)))
        );
    }

    #[test]
    fn the_same_seed_is_the_same_machine() {
        let a = NodeKeys::new(None);
        let b = NodeKeys::new(Some(a.seed()));
        assert_eq!(a.ed_pub(), b.ed_pub());
        assert_eq!(a.node_id(), b.node_id());
        assert_eq!(a.node_id().len(), 32);
    }

    #[test]
    fn requests_expire_and_cannot_be_approved_late() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let sp: [u8; 32] = unhex(&offer.server_pub).unwrap();
        let sig = keys.claim_signature(&offer.id, &sp);
        let late = NOW + 600;
        assert!(book.pending(late).is_empty());
        assert_eq!(
            book.approve(late, &offer.id, |_, _| Ok("x".into())),
            Err(PairError::NotFound)
        );
        assert_eq!(
            book.claim(late + 1, "10.0.0.7", &offer.id, &sig),
            Err(PairError::NotFound)
        );
    }

    #[test]
    fn a_denied_request_gets_no_credential() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let sp: [u8; 32] = unhex(&offer.server_pub).unwrap();
        let sig = keys.claim_signature(&offer.id, &sp);
        book.deny(NOW + 1, &offer.id).unwrap();
        assert_eq!(
            book.claim(NOW + 3, "10.0.0.7", &offer.id, &sig),
            Ok(Claim::Denied)
        );
        assert_eq!(
            book.approve(NOW + 4, &offer.id, |_, _| Ok("x".into())),
            Err(PairError::NotPending)
        );
    }

    #[test]
    fn a_failed_issue_leaves_the_request_pending() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let r = book.approve(NOW, &offer.id, |_, _| Err("name taken".into()));
        assert_eq!(r, Err(PairError::Issue("name taken".into())));
        assert!(
            book.approve(NOW, &offer.id, |_, _| Ok("fvnc_a.1.b".into()))
                .is_ok()
        );
    }

    #[test]
    fn open_requests_are_capped_overall_and_per_address() {
        let mut book = PairBook::new(PairConfig::default());
        for i in 0..3 {
            ask(
                &mut book,
                &NodeKeys::new(None),
                "10.0.0.7",
                &format!("n{i}"),
            )
            .unwrap();
        }
        assert_eq!(
            ask(&mut book, &NodeKeys::new(None), "10.0.0.7", "n9"),
            Err(PairError::Busy(30))
        );
        // Other addresses still work, up to the overall cap.
        for i in 0..13 {
            ask(&mut book, &NodeKeys::new(None), &format!("10.0.1.{i}"), "x").unwrap();
        }
        assert_eq!(
            ask(&mut book, &NodeKeys::new(None), "10.0.2.1", "y"),
            Err(PairError::Busy(30))
        );
    }

    #[test]
    fn asking_twice_with_the_same_keys_is_one_request() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let a = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let b = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(book.pending(NOW).len(), 1);
    }

    #[test]
    fn guessing_ids_is_throttled_per_address() {
        let mut book = PairBook::new(PairConfig::default());
        let sig = hex(&[0u8; 64]);
        for _ in 0..20 {
            assert_eq!(
                book.claim(NOW, "10.6.6.6", "nope", &sig),
                Err(PairError::NotFound)
            );
        }
        assert_eq!(
            book.claim(NOW, "10.6.6.6", "nope", &sig),
            Err(PairError::Busy(60))
        );
        // Another address is unaffected; the block lifts after a minute.
        assert_eq!(
            book.claim(NOW, "10.0.0.1", "nope", &sig),
            Err(PairError::NotFound)
        );
        assert_eq!(
            book.claim(NOW + 61, "10.6.6.6", "nope", &sig),
            Err(PairError::NotFound)
        );
    }

    #[test]
    fn claims_are_rate_limited_per_request() {
        let mut book = PairBook::new(PairConfig::default());
        let keys = NodeKeys::new(None);
        let offer = ask(&mut book, &keys, "10.0.0.7", "mini-1").unwrap();
        let sp: [u8; 32] = unhex(&offer.server_pub).unwrap();
        let sig = keys.claim_signature(&offer.id, &sp);
        assert_eq!(
            book.claim(NOW + 5, "a", &offer.id, &sig),
            Ok(Claim::Pending)
        );
        assert_eq!(
            book.claim(NOW + 5, "a", &offer.id, &sig),
            Err(PairError::Busy(1))
        );
    }

    #[test]
    fn bad_requests_are_refused() {
        let mut book = PairBook::new(PairConfig::default());
        let k = NodeKeys::new(None);
        let (e, x) = (hex(&k.ed_pub()), hex(&k.x_pub()));
        assert!(matches!(
            book.request(NOW, "a", "Bad Name", &e, &x),
            Err(PairError::Invalid(_))
        ));
        assert!(matches!(
            book.request(NOW, "a", "ok", "zz", &x),
            Err(PairError::Invalid(_))
        ));
        assert!(matches!(
            book.request(NOW, "a", "ok", &e, &x[2..]),
            Err(PairError::Invalid(_))
        ));
    }

    #[test]
    fn codes_are_normalised() {
        assert_eq!(normalize_code("ab0o1-il x"), "AB001".to_owned() + "11X");
        let mut book = PairBook::new(PairConfig::default());
        assert_eq!(
            book.approve(NOW, "ABCDE-FGHJK", |_, _| Ok("x".into())),
            Err(PairError::NotFound)
        );
        assert_eq!(
            book.approve(NOW, "short", |_, _| Ok("x".into())),
            Err(PairError::NotFound)
        );
    }

    #[test]
    fn hex_roundtrip() {
        let b = [1u8, 2, 254, 255];
        assert_eq!(unhex::<4>(&hex(&b)), Some(b));
        assert_eq!(unhex::<4>("zz"), None);
        assert_eq!(unhex::<2>("é"), None);
    }
}
