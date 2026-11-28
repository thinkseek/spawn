//! Closed workspace storage backend selector shared between `spawn.toml`
//! (`[storage].workspace_backend`) and the runtime `SPAWN_WORKSPACE_BACKEND` boundary.

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};

/// Closed workspace storage backend selector shared between `spawn.toml`
/// (`[storage].workspace_backend`) and the runtime `SPAWN_WORKSPACE_BACKEND` boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceBackend {
    /// Git working copies: the default when `SPAWN_WORKSPACE_BACKEND` is unset.
    Git,
    /// Copy-on-write clones of a per-commit baseline.
    Cow,
    /// An unprivileged overlay per workspace (Linux): by name only.
    Overlay,
    /// Dual-track oracle wrapping authoritative `git` beside shadow `d6`: opt-in only.
    D6Shadow,
    /// Pure-Rust CAS Merkle + ledger workspace store (`D6Workspace`): opt-in only.
    D6,
    /// The cheapest backend the root supports that the loop serves
    /// (`spawn_host::WorkspaceBackend::auto_with`), chosen once at start.
    Auto,
}

impl WorkspaceBackend {
    /// Canonical lowercase backend identifier (`"git"`, `"cow"`, `"overlay"`,
    /// `"d6-shadow"`, `"d6"`, or `"auto"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cow => "cow",
            Self::Overlay => "overlay",
            Self::D6Shadow => "d6-shadow",
            Self::D6 => "d6",
            Self::Auto => "auto",
        }
    }

    /// Parse an explicit backend name (`"auto"` or any concrete [`spawn_host::WorkspaceBackend`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            other => spawn_host::WorkspaceBackend::parse(other).map(Self::from_host),
        }
    }

    /// Convert a concrete host backend into [`WorkspaceBackend`].
    #[must_use]
    pub const fn from_host(backend: spawn_host::WorkspaceBackend) -> Self {
        match backend {
            spawn_host::WorkspaceBackend::Git => Self::Git,
            spawn_host::WorkspaceBackend::Cow => Self::Cow,
            spawn_host::WorkspaceBackend::Overlay => Self::Overlay,
            spawn_host::WorkspaceBackend::D6Shadow => Self::D6Shadow,
            spawn_host::WorkspaceBackend::D6 => Self::D6,
        }
    }

    /// Convert [`WorkspaceBackend`] to [`spawn_host::WorkspaceBackend`], or `None` for [`Self::Auto`].
    #[must_use]
    pub const fn to_host(self) -> Option<spawn_host::WorkspaceBackend> {
        match self {
            Self::Git => Some(spawn_host::WorkspaceBackend::Git),
            Self::Cow => Some(spawn_host::WorkspaceBackend::Cow),
            Self::Overlay => Some(spawn_host::WorkspaceBackend::Overlay),
            Self::D6Shadow => Some(spawn_host::WorkspaceBackend::D6Shadow),
            Self::D6 => Some(spawn_host::WorkspaceBackend::D6),
            Self::Auto => None,
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
