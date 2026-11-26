//! The key cache: `node_id → (Ed25519 key, boot id)` of the nodes whose
//! signature the gateway verified against the ledger's key, for 120 s, at
//! most 4096 of them. A hit verifies a request with no database call at
//! all (a bad signature is refused at once); the verbs that never touch
//! the ledger (artifacts, the registry manifest, a result's upload) need
//! one (`409 beat_first` on a miss). `revoked` and `unknown_node` evict.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use spawn_node_proto::PublicKey;

/// How long an entry is believed.
pub const KEY_TTL: Duration = Duration::from_mins(2);
/// The most entries held.
pub const MAX_KEYS: usize = 4096;

/// A node's verified key and the boot it was last verified for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedKey {
    /// Verified Ed25519 public key for the node.
    pub key: PublicKey,
    /// Boot identifier associated with the verified key.
    pub boot_id: String,
}

#[derive(Debug)]
struct Entry {
    cached: CachedKey,
    at: Instant,
}

/// The cache; shared by every worker.
#[derive(Debug)]
pub struct KeyCache {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    cap: usize,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self::new(KEY_TTL, MAX_KEYS)
    }
}

impl KeyCache {
    /// Creates an empty key cache with entry lifetime `ttl` and maximum capacity `cap`.
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            cap: cap.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The live entry of `node_id`; an expired one is dropped.
    pub fn get(&self, node_id: &str) -> Option<CachedKey> {
        let mut entries = self.lock();
        match entries.get(node_id) {
            Some(entry) if entry.at.elapsed() < self.ttl => Some(entry.cached.clone()),
            Some(_) => {
                entries.remove(node_id);
                None
            }
            None => None,
        }
    }

    /// Remember a key verified now; when full, expired entries go first,
    /// then the oldest.
    pub fn insert(&self, node_id: &str, key: PublicKey, boot_id: &str) {
        let mut entries = self.lock();
        if !entries.contains_key(node_id) && entries.len() >= self.cap {
            let ttl = self.ttl;
            entries.retain(|_, entry| entry.at.elapsed() < ttl);
            if entries.len() >= self.cap {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.at)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(
            node_id.to_string(),
            Entry {
                cached: CachedKey {
                    key,
                    boot_id: boot_id.to_string(),
                },
                at: Instant::now(),
            },
        );
    }

    /// Removes any cached public key for `node_id`.
    pub fn evict(&self, node_id: &str) {
        self.lock().remove(node_id);
    }

    /// Returns the number of entries currently in the cache (including unexpired and uncollected ones).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns `true` when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    #[test]
    fn entries_expire_evict_and_stay_under_the_cap() {
        let cache = KeyCache::new(Duration::from_mins(1), 2);
        cache.insert("a", key(1), "b1");
        cache.insert("b", key(2), "b2");
        cache.insert("c", key(3), "b3");
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_none(), "the oldest went");
        assert_eq!(cache.get("c").unwrap().key, key(3));
        cache.evict("c");
        assert!(cache.get("c").is_none());

        let short = KeyCache::new(Duration::ZERO, 8);
        short.insert("a", key(1), "b1");
        assert!(short.get("a").is_none(), "expired");
        assert!(short.is_empty());
    }
}
