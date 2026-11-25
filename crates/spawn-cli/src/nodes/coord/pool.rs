//! The pooled coordinator sessions of a process that probes coord nodes
//! again and again (issue #96 P0): a probe through a coordinator used to
//! build its whole way each time (tcp, TLS, the WebSocket upgrade, a
//! signed hello, a lookup, then the sealed request: 1.5 to 2.4 s over a
//! Cloudflare `:443`); a pooled probe reuses the coordinator session and
//! the node's checked prekey, and pays only its sealed request.
//!
//! - A session is keyed by its coordinator URL, the digest of its
//!   `SPAWN_COORD_TOKEN` and its TLS configuration: a rotated coordinator
//!   token opens a session of its own, never borrows the old one's.
//! - A session is evicted when its connection closed (its pump says so at
//!   once), when it sat unasked for its idle limit (its own pump closes
//!   it: no reaper thread), or past its age limit; the next probe connects
//!   again. Never more than [`POOL_MAX`] sessions.
//! - A node's prekey (its `peer_info`) is kept [`PEER_TTL`] at most and
//!   checked again on every use (its signature, its node id, the key
//!   pinned at its join); it is forgotten on any failure through it (a
//!   node that rotated its prekey refuses an envelope sealed to the old
//!   one; a node gone, a punched path that gave nothing), and looked up
//!   afresh.
//! - Every request is still sealed on its own (`spawn-e2e-v1`: a fresh
//!   ephemeral X25519 key, a fresh nonce, the caller's own token and
//!   revocations): what is reused is the authenticated coordinator session
//!   and the checked key agreement's peer half, never a symmetric key, so
//!   reuse weakens neither forward secrecy nor replay protection.
//! - A probe (`inventory`, `beat`: idempotent) that fails on a reused
//!   session or prekey is asked once more on a fresh one; anything else
//!   never twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    CoordClient, CoordUrl, HELLO_TIMEOUT, ProbeContext, Probed, ask_through, connect_probe,
    unknown_node,
};
use crate::nodes::crypto::{self, PeerKeys};

/// How long a pooled session may sit unasked before its pump closes it.
pub(crate) const POOL_IDLE: Duration = Duration::from_mins(5);
/// The longest a pooled session is reused (connected again past it).
pub(crate) const POOL_MAX_AGE: Duration = Duration::from_mins(30);
/// The longest a node's checked prekey is reused before it is looked up
/// again (a node rotates its prekey each hour, and still opens the one
/// before).
pub(crate) const PEER_TTL: Duration = Duration::from_mins(5);
/// The most sessions a pool holds.
pub(crate) const POOL_MAX: usize = 64;
/// The most prekeys a pool holds.
const PEERS_MAX: usize = 4096;

/// One pooled session.
struct Pooled {
    client: Arc<CoordClient>,
    born: Instant,
    /// Held so its address, part of the key, is never another's.
    _tls: Option<Arc<rustls::ClientConfig>>,
}

/// One node's record, as its coordinator answered it.
struct Cached {
    info: Value,
    at: Instant,
}

/// Coordinator sessions and nodes' prekeys, reused across probes.
pub(crate) struct CoordPool {
    sessions: Mutex<HashMap<String, Pooled>>,
    peers: Mutex<HashMap<String, Cached>>,
    idle: Duration,
    max_age: Duration,
    peer_ttl: Duration,
    connects: AtomicU64,
    lookups: AtomicU64,
}

impl Default for CoordPool {
    fn default() -> Self {
        Self::with_limits(POOL_IDLE, POOL_MAX_AGE, PEER_TTL)
    }
}

impl CoordPool {
    /// A pool whose sessions close after `idle` unasked, are made again
    /// past `max_age`, and whose prekeys are reused for `peer_ttl`.
    pub(crate) fn with_limits(idle: Duration, max_age: Duration, peer_ttl: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            idle,
            max_age,
            peer_ttl,
            connects: AtomicU64::new(0),
            lookups: AtomicU64::new(0),
        }
    }

    /// This process's pool.
    pub(crate) fn global() -> &'static Self {
        static POOL: OnceLock<CoordPool> = OnceLock::new();
        POOL.get_or_init(Self::default)
    }

    /// The coordinator sessions this pool connected (each one handshake:
    /// tcp, TLS, upgrade, hello).
    #[cfg(test)]
    pub(crate) fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    /// The lookups this pool asked.
    #[cfg(test)]
    pub(crate) fn lookups(&self) -> u64 {
        self.lookups.load(Ordering::Relaxed)
    }

    /// The sessions held and alive.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.lock_sessions()
            .values()
            .filter(|pooled| pooled.client.alive())
            .count()
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pooled>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_peers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cached>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A session's key: its URL, its coordinator token's digest, its TLS
    /// configuration.
    fn key(url: &CoordUrl, context: &ProbeContext) -> String {
        let token = context
            .coord_token
            .as_deref()
            .map_or_else(String::new, |token| {
                crypto::hex(&crypto::sha256(token.as_bytes()))
            });
        let tls = context
            .tls
            .as_ref()
            .map_or_else(String::new, |tls| format!("{:p}", Arc::as_ptr(tls)));
        format!("{url}\n{token}\n{tls}")
    }

    /// The live session of `key`, or a new one to `url` (by `timeout`):
    /// it, and whether it was reused.
    fn session(
        &self,
        key: &str,
        url: &CoordUrl,
        timeout: Duration,
        context: &ProbeContext,
    ) -> Result<(Arc<CoordClient>, bool), String> {
        // Dropped outside the lock: a client dropped stops its pump (one
        // still asked elsewhere lives on until its last ask is done).
        let mut gone = Vec::new();
        let found = {
            let mut sessions = self.lock_sessions();
            sessions.retain(|_, pooled| {
                let keep = pooled.client.alive() && pooled.born.elapsed() < self.max_age;
                if !keep {
                    gone.push(Arc::clone(&pooled.client));
                }
                keep
            });
            sessions.get(key).map(|pooled| Arc::clone(&pooled.client))
        };
        drop(gone);
        if let Some(client) = found {
            return Ok((client, true));
        }
        let client = Arc::new(connect_probe(url, timeout, context)?);
        self.connects.fetch_add(1, Ordering::Relaxed);
        client.close_when_idle(self.idle);
        let mut spare = Vec::new();
        let chosen = {
            let mut sessions = self.lock_sessions();
            match sessions.get(key) {
                // Another probe connected meanwhile: its session is kept.
                Some(pooled) if pooled.client.alive() => {
                    spare.push(Arc::clone(&client));
                    Arc::clone(&pooled.client)
                }
                _ => {
                    while sessions.len() >= POOL_MAX {
                        let Some(oldest) = sessions
                            .iter()
                            .min_by_key(|(_, pooled)| pooled.born)
                            .map(|(key, _)| key.clone())
                        else {
                            break;
                        };
                        if let Some(pooled) = sessions.remove(&oldest) {
                            spare.push(pooled.client);
                        }
                    }
                    sessions.insert(
                        key.to_string(),
                        Pooled {
                            client: Arc::clone(&client),
                            born: Instant::now(),
                            _tls: context.tls.clone(),
                        },
                    );
                    client
                }
            }
        };
        drop(spare);
        Ok((chosen, false))
    }

    /// Forget `client` as `key`'s session (its connection closed).
    fn evict(&self, key: &str, client: &Arc<CoordClient>) {
        let removed = {
            let mut sessions = self.lock_sessions();
            match sessions.get(key) {
                Some(pooled) if Arc::ptr_eq(&pooled.client, client) => sessions.remove(key),
                _ => None,
            }
        };
        drop(removed);
    }

    /// The record kept as `key`, when it is fresh.
    fn cached(&self, key: &str) -> Option<Value> {
        let peers = self.lock_peers();
        peers
            .get(key)
            .filter(|cached| cached.at.elapsed() < self.peer_ttl)
            .map(|cached| cached.info.clone())
    }

    fn remember(&self, key: &str, info: Value) {
        let mut peers = self.lock_peers();
        let ttl = self.peer_ttl;
        peers.retain(|_, cached| cached.at.elapsed() < ttl);
        if peers.len() >= PEERS_MAX {
            peers.clear();
        }
        peers.insert(
            key.to_string(),
            Cached {
                info,
                at: Instant::now(),
            },
        );
    }

    fn forget(&self, key: &str) {
        self.lock_peers().remove(key);
    }

    /// One ask on `client`: the node's record kept (checked again) or
    /// looked up, then [`ask_through`]; the record forgotten on a failure.
    /// Its outcome, and whether a kept record was used.
    #[allow(clippy::too_many_arguments)]
    fn ask_once(
        &self,
        client: &CoordClient,
        peer_key: &str,
        url: &CoordUrl,
        node_id: Option<&str>,
        name: Option<&str>,
        pinned: Option<&str>,
        payload: &Value,
        deadline: Instant,
        context: &ProbeContext,
    ) -> (Result<Probed, String>, bool) {
        let kept = self.cached(peer_key).and_then(|info| {
            PeerKeys::verify(&info, node_id, pinned)
                .ok()
                .map(|peer| (info, peer))
        });
        let reused = kept.is_some();
        let (info, peer) = if let Some(kept) = kept {
            kept
        } else {
            self.forget(peer_key);
            self.lookups.fetch_add(1, Ordering::Relaxed);
            let left = deadline.saturating_duration_since(Instant::now());
            let looked = client
                .lookup(node_id, name, left)
                .and_then(|info| info.ok_or_else(|| unknown_node(url)))
                .and_then(|info| PeerKeys::verify(&info, node_id, pinned).map(|peer| (info, peer)));
            match looked {
                Ok((info, peer)) => {
                    self.remember(peer_key, info.clone());
                    (info, peer)
                }
                Err(why) => return (Err(why), false),
            }
        };
        let through = ask_through(client, info, &peer, payload, deadline, context);
        if through.outcome.is_err() || through.punch_missed {
            self.forget(peer_key);
        }
        (through.outcome, reused)
    }
}

/// [`super::ask`] through `pool`: its session to `url` and its record of
/// the node reused when they are, a probe asked once more on fresh ones
/// when a reused one fails it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ask_pooled(
    pool: &CoordPool,
    url: &CoordUrl,
    node_id: Option<&str>,
    name: Option<&str>,
    pinned: Option<&str>,
    payload: &Value,
    timeout: Duration,
    context: &ProbeContext,
) -> Result<Probed, String> {
    let deadline = Instant::now() + timeout;
    let key = CoordPool::key(url, context);
    let target = match node_id {
        Some(node_id) => format!("id:{node_id}"),
        None => format!("name:{}", name.unwrap_or_default()),
    };
    let peer_key = format!("{key}\n{target}");
    let idempotent = matches!(payload["op"].as_str(), Some("inventory" | "beat"));
    let mut tries = if idempotent { 2 } else { 1 };
    loop {
        tries -= 1;
        let left = deadline.saturating_duration_since(Instant::now());
        let (client, reused_session) = pool.session(&key, url, left.min(HELLO_TIMEOUT), context)?;
        let (outcome, reused_peer) = pool.ask_once(
            &client, &peer_key, url, node_id, name, pinned, payload, deadline, context,
        );
        match outcome {
            Ok(probed) => return Ok(probed),
            Err(why) => {
                let closed = !client.alive();
                if closed {
                    pool.evict(&key, &client);
                }
                // Asked again only when something reused was to blame: a
                // session that closed under it, a prekey kept.
                let stale = (reused_session && closed) || reused_peer;
                let left = deadline.saturating_duration_since(Instant::now());
                if tries == 0 || !stale || left.is_zero() {
                    return Err(why);
                }
            }
        }
    }
}
