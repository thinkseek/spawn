use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use crate::clock::{Clock, SystemClock};
use crate::error::ValidationError;

/// Maximum byte length accepted for any label or identifier.
pub const MAX_LABEL_BYTES: usize = 256;

/// Validate an opaque external label: non-empty, bounded, no control bytes.
///
/// Opaque labels wrap identifiers owned by other systems (LayerFS commit ids,
/// tmux session names). Spawn does not assume their internal format.
pub fn validate_opaque_label(raw: &str) -> Result<(), ValidationError> {
    let length = raw.len();
    if length == 0 || length > MAX_LABEL_BYTES {
        return Err(ValidationError::InvalidLength(length));
    }
    if raw.chars().any(char::is_control) {
        return Err(ValidationError::ControlCharacter);
    }
    Ok(())
}

macro_rules! opaque_label {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Construct from an external string after validation.
            pub fn new(raw: impl Into<String>) -> Result<Self, ValidationError> {
                let raw = raw.into();
                validate_opaque_label(&raw)?;
                Ok(Self(raw))
            }

            /// Borrow the underlying string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_label!(
    /// A LayerFS branch name such as `agent_001`.
    BranchName
);
opaque_label!(
    /// A LayerFS commit identifier returned by the workspace API.
    CommitId
);
opaque_label!(
    /// A mounted workspace reference in `sandbox:path` form.
    MountId
);
opaque_label!(
    /// The name of a terminal session, for example a tmux session.
    TerminalName
);

/// Validate a Spawn-owned identifier with a fixed prefix.
fn validate_prefixed(raw: &str, prefix: &str) -> Result<(), ValidationError> {
    validate_opaque_label(raw)?;
    if !raw.starts_with(prefix) {
        return Err(ValidationError::InvalidLength(raw.len()));
    }
    Ok(())
}

macro_rules! spawn_identifier {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Construct from an already-prefixed identifier string.
            pub fn from_prefixed(raw: impl Into<String>) -> Result<Self, ValidationError> {
                let raw = raw.into();
                validate_prefixed(&raw, $prefix)?;
                Ok(Self(raw))
            }

            /// Borrow the wire representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

spawn_identifier!(
    /// Long-lived logical identity of an agent.
    AgentId,
    "agent_"
);
spawn_identifier!(
    /// A continuous logical history that survives native session changes.
    SessionId,
    "session_"
);
spawn_identifier!(
    /// One computation bound to one terminal and one mounted workspace.
    ExecutionId,
    "exec_"
);

/// Process-wide monotonic counter used by [`IdGenerator`].
static GENERATOR_COUNTER: AtomicU32 = AtomicU32::new(0);

const BASE32_ALPHABET: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

fn encode_base32(mut value: u128) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::with_capacity(26);
    while value > 0 {
        digits.push(BASE32_ALPHABET[(value & 0x1f) as usize]);
        value >>= 5;
    }
    digits.reverse();
    String::from_utf8(digits).expect("base32 alphabet is ASCII")
}

/// Creates Spawn-owned identifiers from time, process identity and a counter.
///
/// The injected clock makes generation deterministic in tests. The atomic
/// counter disambiguates identifiers created within the same millisecond.
pub struct IdGenerator<'a> {
    clock: &'a dyn Clock,
    process_tag: u32,
}

impl std::fmt::Debug for IdGenerator<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdGenerator")
            .field("process_tag", &self.process_tag)
            .finish()
    }
}

impl<'a> IdGenerator<'a> {
    /// Build a generator with a specific clock and process tag.
    #[must_use]
    pub fn new(clock: &'a dyn Clock, process_tag: u32) -> Self {
        Self { clock, process_tag }
    }

    fn next_token(&self) -> String {
        let millis = u128::try_from(self.clock.now().unix_millis()).unwrap_or(0);
        let counter = u128::from(GENERATOR_COUNTER.fetch_add(1, Ordering::Relaxed));
        let tag = u128::from(self.process_tag);
        let packed = (millis << 32) | (counter << 16) | (tag & 0xffff);
        encode_base32(packed)
    }

    fn build<T>(&self, prefix: &str, wrap: impl FnOnce(String) -> T) -> T {
        wrap(format!("{prefix}{}", self.next_token()))
    }

    /// Generate a new [`AgentId`].
    #[must_use]
    pub fn new_agent_id(&self) -> AgentId {
        self.build("agent_", |raw| {
            AgentId::from_prefixed(raw).expect("generated identifiers satisfy validation")
        })
    }

    /// Generate a new [`SessionId`].
    #[must_use]
    pub fn new_session_id(&self) -> SessionId {
        self.build("session_", |raw| {
            SessionId::from_prefixed(raw).expect("generated identifiers satisfy validation")
        })
    }

    /// Generate a new [`ExecutionId`].
    #[must_use]
    pub fn new_execution_id(&self) -> ExecutionId {
        self.build("exec_", |raw| {
            ExecutionId::from_prefixed(raw).expect("generated identifiers satisfy validation")
        })
    }
}

/// Identifier generator bound to [`SystemClock`].
#[derive(Debug)]
pub struct SystemIdGenerator {
    inner: IdGenerator<'static>,
}

impl SystemIdGenerator {
    /// Build a generator tagged with one process identity.
    #[must_use]
    pub fn new(process_tag: u32) -> Self {
        // SystemClock is a zero-sized stateless value with static lifetime.
        let clock: &'static SystemClock = &SystemClock;
        Self {
            inner: IdGenerator::new(clock, process_tag),
        }
    }

    /// Generate a new [`AgentId`].
    #[must_use]
    pub fn new_agent_id(&self) -> AgentId {
        self.inner.new_agent_id()
    }

    /// Generate a new [`SessionId`].
    #[must_use]
    pub fn new_session_id(&self) -> SessionId {
        self.inner.new_session_id()
    }

    /// Generate a new [`ExecutionId`].
    #[must_use]
    pub fn new_execution_id(&self) -> ExecutionId {
        self.inner.new_execution_id()
    }
}

/// Current process tag derived from the OS process identifier.
#[must_use]
pub fn current_process_tag() -> u32 {
    std::process::id()
}
