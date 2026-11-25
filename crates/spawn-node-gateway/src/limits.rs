//! Pacing (section 2.6): token buckets per source address and per
//! `node_id`, 20 requests per 10 s for the API verbs and 200 per 10 s for
//! artifacts and the registry manifest (a job fetches several). Over it:
//! `429 busy`, signed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The bucket's window.
pub const WINDOW: Duration = Duration::from_secs(10);
/// Requests per window of the API verbs.
pub const API_PER_WINDOW: u32 = 20;
/// Requests per window of artifact and manifest GETs.
pub const ARTIFACT_PER_WINDOW: u32 = 200;
/// Buckets held before the full (idle) ones are dropped.
const PRUNE_AT: usize = 16 * 1024;
/// Buckets held at most; a new subject past it is refused.
const MAX_BUCKETS: usize = 64 * 1024;

/// Which allowance a request draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Api,
    Artifact,
}

impl Class {
    fn per_window(self) -> u32 {
        match self {
            Self::Api => API_PER_WINDOW,
            Self::Artifact => ARTIFACT_PER_WINDOW,
        }
    }
}

/// Who a bucket paces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    Source(IpAddr),
    Node(String),
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Every bucket; shared by the workers.
#[derive(Debug, Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<(Class, Subject), Bucket>>,
}

impl Limiter {
    /// Takes one token from the source's bucket and one from the node's;
    /// false (and nothing taken from the second) when either is empty.
    pub fn allow(&self, class: Class, source: IpAddr, node_id: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() >= PRUNE_AT {
            buckets.retain(|(class, _), bucket| refill(*class, *bucket, now) < f64::from(class.per_window()));
        }
        take(&mut buckets, class, Subject::Source(source), now)
            && take(&mut buckets, class, Subject::Node(node_id.to_string()), now)
    }
}

fn refill(class: Class, bucket: Bucket, now: Instant) -> f64 {
    let cap = f64::from(class.per_window());
    let rate = cap / WINDOW.as_secs_f64();
    (bucket.tokens + now.duration_since(bucket.at).as_secs_f64() * rate).min(cap)
}

fn take(
    buckets: &mut HashMap<(Class, Subject), Bucket>,
    class: Class,
    subject: Subject,
    now: Instant,
) -> bool {
    let key = (class, subject);
    let tokens = match buckets.get(&key) {
        Some(bucket) => refill(class, *bucket, now),
        None if buckets.len() >= MAX_BUCKETS => return false,
        None => f64::from(class.per_window()),
    };
    if tokens < 1.0 {
        buckets.insert(key, Bucket { tokens, at: now });
        return false;
    }
    buckets.insert(
        key,
        Bucket {
            tokens: tokens - 1.0,
            at: now,
        },
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twenty_api_requests_pass_then_the_twenty_first_waits() {
        let limiter = Limiter::default();
        let source: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..API_PER_WINDOW {
            assert!(limiter.allow(Class::Api, source, "n1"));
        }
        assert!(!limiter.allow(Class::Api, source, "n1"));
        // Another node from the same source is paced by the source.
        assert!(!limiter.allow(Class::Api, source, "n2"));
        // Artifacts draw on their own, larger allowance.
        for _ in 0..ARTIFACT_PER_WINDOW {
            assert!(limiter.allow(Class::Artifact, source, "n1"));
        }
        assert!(!limiter.allow(Class::Artifact, source, "n1"));
        // Another source and node is untouched.
        assert!(limiter.allow(Class::Api, "192.0.2.2".parse().unwrap(), "n3"));
    }
}
