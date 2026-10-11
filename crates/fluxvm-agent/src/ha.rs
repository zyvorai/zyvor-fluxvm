// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! High availability for `fluxvm-agent central`: several centrals share one store (Redis); exactly one holds the
//! leader lock and serves `/fleet/*`, the others answer `503` naming the leader until the lock lapses and one of them
//! takes over.
//!
//! - **Leader lock.** A key holding the leader's id (its advertised URL) with a TTL. The leader renews it every
//!   third of the TTL; if it dies or is partitioned from the store, the lock expires and the first standby to try
//!   gets it.
//! - **Fenced state.** The leader mirrors its state (nodes and tenancy) into the store, and every write checks, in the
//!   same atomic step, that the writer still holds the lock. A leader that was paused past its TTL and wakes up after
//!   another took over cannot overwrite the new leader's state.
//! - **Promotion.** A standby that becomes leader loads the state from the store before it serves anything.
//!
//! The store is Redis (`--ha-redis redis://…`). A Redis outage stops leadership renewal, so the leader steps down
//! after its TTL rather than serving without fencing. [`HaStore::memory`] is an in-process store with the same
//! semantics, used by the tests to run several centrals against one store.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Atomically: renew the lock if we hold it, take it if nobody does, and return whoever holds it now.
const ACQUIRE_LUA: &str = r#"
local cur = redis.call('GET', KEYS[1])
if cur == ARGV[1] then
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
  return cur
end
if not cur then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
  return ARGV[1]
end
return cur
"#;

/// Atomically: write KEYS[2] only while KEYS[1] (the lock) still names ARGV[1]. Returns 1 if written.
const FENCED_SET_LUA: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  redis.call('SET', KEYS[2], ARGV[2])
  return 1
end
return 0
"#;

/// Atomically: drop the lock if we hold it (clean shutdown hands over at once instead of after the TTL).
const RELEASE_LUA: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
"#;

#[derive(Default)]
struct Memory {
    lock: Option<(String, Instant)>,
    kv: HashMap<String, String>,
}

#[derive(Clone)]
enum Backend {
    Memory(Arc<Mutex<Memory>>),
    Redis(Box<redis::aio::ConnectionManager>),
}

/// Where the lock and the mirrored state live. Cloning shares the same store.
#[derive(Clone)]
pub struct HaStore {
    backend: Backend,
    prefix: String,
}

impl HaStore {
    /// An in-process store; every clone sees the same lock and state.
    pub fn memory(cluster: &str) -> Self {
        Self {
            backend: Backend::Memory(Arc::new(Mutex::new(Memory::default()))),
            prefix: format!("fluxvm:fleet:{cluster}"),
        }
    }

    pub async fn redis(url: &str, cluster: &str) -> Result<Self> {
        let client = redis::Client::open(url).context("parsing the HA Redis URL")?;
        let mgr = redis::aio::ConnectionManager::new(client)
            .await
            .context("connecting to the HA Redis")?;
        Ok(Self {
            backend: Backend::Redis(Box::new(mgr)),
            prefix: format!("fluxvm:fleet:{cluster}"),
        })
    }

    fn lock_key(&self) -> String {
        format!("{}:leader", self.prefix)
    }

    fn state_key(&self, name: &str) -> String {
        format!("{}:state:{name}", self.prefix)
    }

    /// Renews or takes the leader lock for `me`; returns the current leader.
    pub async fn acquire(&self, me: &str, ttl: Duration) -> Result<String> {
        match &self.backend {
            Backend::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                match &m.lock {
                    Some((holder, until)) if *until > now && holder != me => Ok(holder.clone()),
                    _ => {
                        m.lock = Some((me.to_owned(), now + ttl));
                        Ok(me.to_owned())
                    }
                }
            }
            Backend::Redis(mgr) => {
                let mut conn = (**mgr).clone();
                let holder: String = redis::cmd("EVAL")
                    .arg(ACQUIRE_LUA)
                    .arg(1)
                    .arg(self.lock_key())
                    .arg(me)
                    .arg(ttl.as_millis().max(1) as u64)
                    .query_async(&mut conn)
                    .await
                    .context("HA leader lock")?;
                Ok(holder)
            }
        }
    }

    /// Writes state `name` only if `me` still holds the lock. `Ok(false)` means we are no longer the leader.
    pub async fn put_fenced(&self, me: &str, name: &str, value: &str) -> Result<bool> {
        match &self.backend {
            Backend::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                let held =
                    matches!(&m.lock, Some((h, until)) if h == me && *until > Instant::now());
                if held {
                    m.kv.insert(self.state_key(name), value.to_owned());
                }
                Ok(held)
            }
            Backend::Redis(mgr) => {
                let mut conn = (**mgr).clone();
                let n: i64 = redis::cmd("EVAL")
                    .arg(FENCED_SET_LUA)
                    .arg(2)
                    .arg(self.lock_key())
                    .arg(self.state_key(name))
                    .arg(me)
                    .arg(value)
                    .query_async(&mut conn)
                    .await
                    .context("HA fenced write")?;
                Ok(n == 1)
            }
        }
    }

    pub async fn get(&self, name: &str) -> Result<Option<String>> {
        match &self.backend {
            Backend::Memory(m) => Ok(m
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kv
                .get(&self.state_key(name))
                .cloned()),
            Backend::Redis(mgr) => {
                let mut conn = (**mgr).clone();
                let v: Option<String> = redis::cmd("GET")
                    .arg(self.state_key(name))
                    .query_async(&mut conn)
                    .await
                    .context("HA state read")?;
                Ok(v)
            }
        }
    }

    pub async fn release(&self, me: &str) -> Result<()> {
        match &self.backend {
            Backend::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                if m.lock.as_ref().is_some_and(|(h, _)| h == me) {
                    m.lock = None;
                }
                Ok(())
            }
            Backend::Redis(mgr) => {
                let mut conn = (**mgr).clone();
                let _: i64 = redis::cmd("EVAL")
                    .arg(RELEASE_LUA)
                    .arg(1)
                    .arg(self.lock_key())
                    .arg(me)
                    .query_async(&mut conn)
                    .await
                    .context("HA lock release")?;
                Ok(())
            }
        }
    }
}

/// HA settings for one central.
#[derive(Clone)]
pub struct HaConfig {
    pub store: HaStore,
    /// This central's id and the URL clients should use for it when it leads.
    pub advertise_url: String,
    pub ttl: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_leader_at_a_time_and_takeover_after_the_ttl() {
        let store = HaStore::memory("t");
        let ttl = Duration::from_millis(200);
        assert_eq!(store.acquire("a", ttl).await.unwrap(), "a");
        assert_eq!(store.acquire("b", ttl).await.unwrap(), "a");
        // Renewal keeps it.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(store.acquire("a", ttl).await.unwrap(), "a");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(store.acquire("b", ttl).await.unwrap(), "a");
        // a stops renewing: b takes over once the TTL lapses.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(store.acquire("b", ttl).await.unwrap(), "b");
        assert_eq!(store.acquire("a", ttl).await.unwrap(), "b");
    }

    #[tokio::test]
    async fn writes_are_fenced_by_the_lock() {
        let store = HaStore::memory("t");
        let ttl = Duration::from_millis(100);
        store.acquire("a", ttl).await.unwrap();
        assert!(store.put_fenced("a", "tenancy", "v1").await.unwrap());
        assert!(!store.put_fenced("b", "tenancy", "evil").await.unwrap());
        // a is paused past its TTL, b takes over; a's late write is refused.
        tokio::time::sleep(Duration::from_millis(150)).await;
        store.acquire("b", ttl).await.unwrap();
        assert!(store.put_fenced("b", "tenancy", "v2").await.unwrap());
        assert!(!store.put_fenced("a", "tenancy", "stale").await.unwrap());
        assert_eq!(store.get("tenancy").await.unwrap().as_deref(), Some("v2"));
        assert_eq!(store.get("nodes").await.unwrap(), None);
    }

    #[tokio::test]
    async fn release_hands_over_at_once() {
        let store = HaStore::memory("t");
        let ttl = Duration::from_secs(60);
        store.acquire("a", ttl).await.unwrap();
        store.release("b").await.unwrap();
        assert_eq!(store.acquire("b", ttl).await.unwrap(), "a");
        store.release("a").await.unwrap();
        assert_eq!(store.acquire("b", ttl).await.unwrap(), "b");
    }
}
