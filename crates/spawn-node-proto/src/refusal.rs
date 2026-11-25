//! The refusals of section 2.4: status, `error`, and what the daemon does.

/// One row of section 2.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// 400: malformed target, header or body; over a size cap.
    BadRequest,
    /// 401: the signature does not verify.
    BadSignature,
    /// 401: not enrolled.
    UnknownNode,
    /// 401: revoked.
    Revoked,
    /// 409: `|ts_ms − server| > 120 000`.
    ClockSkew,
    /// 409: heartbeat `seq ≤` the stored one.
    Replay,
    /// 409: a higher boot counter has been seen for this key.
    StaleBoot,
    /// 409: the gateway does not hold this node's key or boot yet.
    BeatFirst,
    /// 409: a result for an epoch that moved.
    LeaseFenced,
    /// 413: log over 32 MiB or metadata over 256 KiB.
    TooLarge,
    /// 429 `busy`: rate limit.
    RateLimited,
    /// 503 `busy`: PostgreSQL unreachable.
    Unavailable,
}

/// What the daemon does on a (signed) refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Log; do not resend the same bytes.
    DropRequest,
    /// Full Jitter backoff.
    Backoff,
    /// Stop every job and exit 78, with no restart loop.
    ExitRevoked,
    /// `offset_ms = server_time_ms − local_now_ms`; retry at once, once.
    AdoptServerClock,
    /// `seq = max(seq, local) + 1`; retry.
    BumpSeq,
    /// Exit non-zero: another process owns the identity.
    ExitStaleBoot,
    /// Send a heartbeat now, then retry.
    BeatThenRetry,
    /// Keep `results/<job>.<epoch>/` until GC; stop advertising it.
    KeepUntilGc,
    /// Truncate per section 3.5 and resend.
    TruncateAndResend,
}

/// The exit status of a revoked daemon (`EX_CONFIG`).
pub const REVOKED_EXIT_CODE: i32 = 78;

impl Refusal {
    pub const ALL: [Self; 12] = [
        Self::BadRequest,
        Self::BadSignature,
        Self::UnknownNode,
        Self::Revoked,
        Self::ClockSkew,
        Self::Replay,
        Self::StaleBoot,
        Self::BeatFirst,
        Self::LeaseFenced,
        Self::TooLarge,
        Self::RateLimited,
        Self::Unavailable,
    ];

    pub fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::BadSignature | Self::UnknownNode | Self::Revoked => 401,
            Self::ClockSkew
            | Self::Replay
            | Self::StaleBoot
            | Self::BeatFirst
            | Self::LeaseFenced => 409,
            Self::TooLarge => 413,
            Self::RateLimited => 429,
            Self::Unavailable => 503,
        }
    }

    /// The `error` field of the body.
    pub fn error(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::BadSignature => "bad_signature",
            Self::UnknownNode => "unknown_node",
            Self::Revoked => "revoked",
            Self::ClockSkew => "clock_skew",
            Self::Replay => "replay",
            Self::StaleBoot => "stale_boot",
            Self::BeatFirst => "beat_first",
            Self::LeaseFenced => "lease_fenced",
            Self::TooLarge => "too_large",
            Self::RateLimited | Self::Unavailable => "busy",
        }
    }

    /// The refusal of a status and `error`; `None` for a pair v1 does not
    /// have.
    pub fn from_wire(status: u16, error: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|refusal| refusal.status() == status && refusal.error() == error)
    }

    pub fn reaction(self) -> Reaction {
        match self {
            Self::BadRequest => Reaction::DropRequest,
            Self::BadSignature | Self::UnknownNode | Self::RateLimited | Self::Unavailable => {
                Reaction::Backoff
            }
            Self::Revoked => Reaction::ExitRevoked,
            Self::ClockSkew => Reaction::AdoptServerClock,
            Self::Replay => Reaction::BumpSeq,
            Self::StaleBoot => Reaction::ExitStaleBoot,
            Self::BeatFirst => Reaction::BeatThenRetry,
            Self::LeaseFenced => Reaction::KeepUntilGc,
            Self::TooLarge => Reaction::TruncateAndResend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_round_trips_through_its_status_and_error() {
        for refusal in Refusal::ALL {
            assert_eq!(
                Refusal::from_wire(refusal.status(), refusal.error()),
                Some(refusal)
            );
        }
        assert_eq!(Refusal::from_wire(409, "bad_request"), None);
        assert_eq!(Refusal::from_wire(200, "busy"), None);
    }
}
