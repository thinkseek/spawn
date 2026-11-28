//! Closed workspace storage backend selector shared between `spawn.toml`
//! (`[storage].workspace_backend`) and the runtime `SPAWN_WORKSPACE_BACKEND` boundary.

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};

/// Closed workspace storage backend selector shared between `spawn.toml`
/// (`[storage].workspace_backend`) and the runtime `SPAWN_WORKSPACE_BACKEND` boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceBackend {
    /// The cheapest backend the root supports that the loop serves
    /// (`spawn_host::WorkspaceBackend::auto_with`), chosen once at start.
    Auto,
    /// An explicit concrete host workspace backend (`git`, `cow`, `overlay`,
    /// `d6-shadow`, or `d6`).
    Named(spawn_host::WorkspaceBackend),
}

impl WorkspaceBackend {
    /// Canonical lowercase backend identifier (`"git"`, `"cow"`, `"overlay"`,
    /// `"d6-shadow"`, `"d6"`, or `"auto"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Named(host) => host.name(),
        }
    }

    /// Parse an explicit backend name (`"auto"` or any concrete [`spawn_host::WorkspaceBackend`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            other => spawn_host::WorkspaceBackend::parse(other).map(Self::Named),
        }
    }

    /// Convert [`WorkspaceBackend`] to [`spawn_host::WorkspaceBackend`], or `None` for [`Self::Auto`].
    #[must_use]
    pub const fn to_host(self) -> Option<spawn_host::WorkspaceBackend> {
        match self {
            Self::Auto => None,
            Self::Named(host) => Some(host),
        }
    }
}

impl Serialize for WorkspaceBackend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for WorkspaceBackend {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            de::Error::custom(format!(
                "[storage].workspace_backend = {raw:?} is invalid: expected \"auto\", \"cow\", \"git\", \"overlay\", \"d6\", or \"d6-shadow\""
            ))
        })
    }
}
