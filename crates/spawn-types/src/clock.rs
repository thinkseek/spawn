use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Millisecond-precision UTC instant.
///
/// The numeric value is Unix epoch milliseconds. Timezones are applied only at
/// presentation; storage and comparisons use the absolute number.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Construct from Unix epoch milliseconds.
    #[must_use]
    pub fn from_unix_millis(value: i64) -> Self {
        Self(value)
    }

    /// Return Unix epoch milliseconds.
    #[must_use]
    pub fn unix_millis(self) -> i64 {
        self.0
    }
}

/// Time source port. Implementations are injectable for deterministic tests.
pub trait Clock: Send + Sync {
    /// Return the current instant.
    fn now(&self) -> Timestamp;
}

impl<T> Clock for std::sync::Arc<T>
where
    T: Clock + ?Sized,
{
    fn now(&self) -> Timestamp {
        (**self).now()
    }
}

/// System wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
            });
        Timestamp::from_unix_millis(millis)
    }
}

/// Mutable clock driven manually by tests.
#[derive(Debug, Default)]
pub struct DeterministicClock {
    millis: std::sync::atomic::AtomicI64,
}

impl DeterministicClock {
    /// Create a clock at Unix epoch zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a clock at a fixed millisecond instant.
    #[must_use]
    pub fn starting_at(millis: i64) -> Self {
        Self {
            millis: std::sync::atomic::AtomicI64::new(millis),
        }
    }

    /// Advance the clock by a millisecond delta and return the new instant.
    pub fn advance_millis(&self, delta: i64) -> Timestamp {
        Timestamp::from_unix_millis(
            self.millis
                .fetch_add(delta, std::sync::atomic::Ordering::SeqCst)
                + delta,
        )
    }
}

impl Clock for DeterministicClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.millis.load(std::sync::atomic::Ordering::SeqCst))
    }
}
