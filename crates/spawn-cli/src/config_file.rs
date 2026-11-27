//! Structured `spawn.toml` configuration, secret resolution & redaction,
//! environment entry computation, and the `spawn config` / `spawn doctor`
//! subcommands.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use super::toml_parser;

/// Expected `schema_version` in `spawn.toml`.
pub const SCHEMA_VERSION: u32 = 1;

/// Minimum supported Git version (matches `config::MIN_GIT`).
pub const MIN_GIT: (u32, u32) = (2, 43);

/// Usage text for `spawn config` (referenced by `main.rs` help contract).
pub const CONFIG_USAGE: &str = "\
usage: spawn config check [--config <path>] [--json]
       spawn config effective [--config <path>] [--json]
       spawn config init --data-dir <dir> [--name <name>] [--output <file>] [--from-host]
       spawn config docker-plan [--config <path>] [--data-dir <dir>] [--image <image>] [--json]";

/// Usage text for `spawn doctor` (referenced by `main.rs` help contract).
pub const DOCTOR_USAGE: &str = "usage: spawn doctor [--config <path>] [--root <dir>] [--json]";

const PLAINTEXT_SECRET_ERROR: &str = "secrets must use { from_env = \"...\" } or { from_file = \"...\" }, never plaintext in spawn.toml";

const REDACTED: &str = "***";

const BARE_SYSTEM_DIRS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/media",
    "/mnt",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/usr/local",
    "/var",
    "/var/cache",
    "/var/lib",
    "/var/run",
    "/var/tmp",
];

/// Reference to a secret loaded either from an environment variable or a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SecretRef {
    FromEnv { from_env: String },
    FromFile { from_file: PathBuf },
}

impl SecretRef {
    /// Resolve the secret from the environment variable or file, trimming
    /// trailing `\r\n` for files and rejecting missing or empty values.
    #[allow(dead_code)]
    pub fn resolve(&self) -> Result<String, String> {
        self.resolve_with(&|name| std::env::var(name).ok())
    }

    /// Resolve the secret using a caller-supplied environment lookup function.
    pub fn resolve_with(
        &self,
        env_lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, String> {
        match self {
            Self::FromEnv { from_env } => {
                let value = env_lookup(from_env)
                    .ok_or_else(|| format!("secret environment variable {from_env} is not set"))?;
                if value.is_empty() {
                    return Err(format!("secret environment variable {from_env} is empty"));
                }
                Ok(value)
            }
            Self::FromFile { from_file } => {
                if !from_file.is_absolute() {
                    return Err(format!(
                        "secret file path must be absolute: {}",
                        from_file.display()
                    ));
                }
                let raw = fs::read_to_string(from_file).map_err(|error| {
                    format!("read secret file {}: {error}", from_file.display())
                })?;
                let trimmed = raw.trim_end_matches(['\r', '\n']).to_string();
                if trimmed.is_empty() {
                    return Err(format!("secret file {} is empty", from_file.display()));
                }
                Ok(trimmed)
            }
        }
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SecretRefVisitor;

        impl<'de> Visitor<'de> for SecretRefVisitor {
            type Value = SecretRef;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .write_str("a table { from_env = \"VAR\" } or { from_file = \"/abs/path\" }")
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(PLAINTEXT_SECRET_ERROR))
            }

            fn visit_string<E>(self, _value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(PLAINTEXT_SECRET_ERROR))
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(PLAINTEXT_SECRET_ERROR))
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(PLAINTEXT_SECRET_ERROR))
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Err(E::custom(PLAINTEXT_SECRET_ERROR))
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut from_env: Option<String> = None;
                let mut from_file: Option<PathBuf> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "from_env" => {
                            if from_env.is_some() {
                                return Err(de::Error::duplicate_field("from_env"));
                            }
                            from_env = Some(map.next_value()?);
                        }
                        "from_file" => {
                            if from_file.is_some() {
                                return Err(de::Error::duplicate_field("from_file"));
                            }
                            from_file = Some(map.next_value()?);
                        }
                        other => {
                            return Err(de::Error::custom(format!(
                                "unknown field `{other}` in secret reference; {PLAINTEXT_SECRET_ERROR}"
                            )));
                        }
                    }
                }
                match (from_env, from_file) {
                    (Some(_), Some(_)) => Err(de::Error::custom(
                        "secret reference must specify either `from_env` or `from_file`, not both",
                    )),
                    (Some(var), None) => {
                        if var.trim().is_empty() {
                            return Err(de::Error::custom(
                                "secret reference `from_env` must not be empty",
                            ));
                        }
                        Ok(SecretRef::FromEnv { from_env: var })
                    }
                    (None, Some(path)) => {
                        if path.as_os_str().is_empty() || !path.is_absolute() {
                            return Err(de::Error::custom(format!(
                                "secret reference `from_file` must be an absolute path, got {:?}",
                                path.display()
                            )));
                        }
                        Ok(SecretRef::FromFile { from_file: path })
                    }
                    (None, None) => Err(de::Error::custom(PLAINTEXT_SECRET_ERROR)),
                }
            }
        }

        deserializer.deserialize_any(SecretRefVisitor)
    }
}

/// Top-level `spawn.toml` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnConfig {
    pub schema_version: u32,
    pub instance: InstanceConfig,
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub engines: EnginesConfig,
    #[serde(default)]
    pub advanced: AdvancedConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    #[serde(default = "default_instance_name")]
    pub name: String,
    pub data_dir: PathBuf,
    #[serde(default)]
    pub default_repo: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: Option<SecretRef>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<SecretRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub token: Option<SecretRef>,
    #[serde(default)]
    pub web_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(default = "default_workspace_backend")]
    pub workspace_backend: Option<String>,
    #[serde(default = "default_canonical_mount")]
    pub canonical_mount: Option<String>,
    #[serde(default = "default_true_opt")]
    pub cas_dedup: Option<bool>,
    #[serde(default)]
    pub max_checkpoint_file_mb: Option<u64>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            workspace_backend: default_workspace_backend(),
            canonical_mount: default_canonical_mount(),
            cas_dedup: default_true_opt(),
            max_checkpoint_file_mb: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
    #[serde(default = "default_terminal_backend")]
    pub backend: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub shards: Option<usize>,
    #[serde(default)]
    pub toolchain_paths: Option<Vec<String>>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            backend: default_terminal_backend(),
            session: None,
            shards: None,
            toolchain_paths: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
pub struct NetworkConfig {
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default)]
    pub https_proxy: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default = "default_sandbox_mode")]
    pub mode: Option<String>,
    #[serde(default = "default_home_isolation")]
    pub home_isolation: Option<String>,
    #[serde(default = "default_auto_approve")]
    pub auto_approve: Option<String>,
    #[serde(default)]
    pub writable_mounts: Option<Vec<MountSpec>>,
    #[serde(default)]
    pub inherit_host: Option<String>,
    #[serde(default)]
    pub inherit_host_paths: Option<Vec<PathBuf>>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            home_isolation: default_home_isolation(),
            auto_approve: default_auto_approve(),
            writable_mounts: None,
            inherit_host: None,
            inherit_host_paths: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountSpec {
    pub host: PathBuf,
    pub mode: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnginesConfig {
    #[serde(default)]
    pub agy: Option<AgyConfig>,
    #[serde(default)]
    pub claude: Option<ClaudeConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub auth_source: Option<PathBuf>,
    #[serde(default = "default_true_opt")]
    pub isolate_workers: Option<bool>,
    #[serde(default = "default_true_opt")]
    pub isolate_brain: Option<bool>,
}

impl Default for AgyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_model: None,
            auth_source: None,
            isolate_workers: default_true_opt(),
            isolate_brain: default_true_opt(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
    #[serde(default)]
    pub sandbox: Option<bool>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            config_dir: None,
            sandbox: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvancedConfig {
    #[serde(default)]
    pub loop_interval_ms: Option<u64>,
    #[serde(default)]
    pub screen_quiet_ms: Option<u64>,
    #[serde(default)]
    pub message_taken_secs: Option<u64>,
}

/// Directories derived from `[instance].data_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DerivedPaths {
    pub workspaces: PathBuf,
    pub cas: PathBuf,
    pub transcripts: PathBuf,
    pub agy_data_dir: PathBuf,
    pub claude_data_dir: PathBuf,
    pub tmp: PathBuf,
    pub herdr: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_cache: PathBuf,
}

impl DerivedPaths {
    fn all(&self) -> [&Path; 9] {
        [
            &self.workspaces,
            &self.cas,
            &self.transcripts,
            &self.agy_data_dir,
            &self.claude_data_dir,
            &self.tmp,
            &self.herdr,
            &self.xdg_config,
            &self.xdg_cache,
        ]
    }
}

fn default_instance_name() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

#[allow(clippy::unnecessary_wraps)]
fn default_true_opt() -> Option<bool> {
    Some(true)
}

#[allow(clippy::unnecessary_wraps)]
fn default_workspace_backend() -> Option<String> {
    Some("auto".to_string())
}

#[allow(clippy::unnecessary_wraps)]
fn default_canonical_mount() -> Option<String> {
    Some("/workspace".to_string())
}

#[allow(clippy::unnecessary_wraps)]
fn default_terminal_backend() -> Option<String> {
    Some("herdr".to_string())
}

#[allow(clippy::unnecessary_wraps)]
fn default_sandbox_mode() -> Option<String> {
    Some("required".to_string())
}

#[allow(clippy::unnecessary_wraps)]
fn default_home_isolation() -> Option<String> {
    Some("whitelist".to_string())
}

#[allow(clippy::unnecessary_wraps)]
fn default_auto_approve() -> Option<String> {
    Some("startup".to_string())
}

impl SpawnConfig {
    /// Parse and validate `spawn.toml` text.
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        let value = toml_parser::parse(input).map_err(|error| format!("parse TOML: {error}"))?;
        let config: Self = serde_json::from_value(value)
            .map_err(|error| format!("invalid spawn.toml: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Read, parse, and validate a `spawn.toml` file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("read config {}: {error}", path.display()))?;
        Self::from_toml_str(&text)
    }

    /// Compute derived directories under `[instance].data_dir`.
    pub fn derived_paths(&self) -> DerivedPaths {
        let base = normalize_lexically(&self.instance.data_dir);
        DerivedPaths {
            workspaces: base.join("workspaces"),
            cas: base.join("cas"),
            transcripts: base.join("transcripts"),
            agy_data_dir: base.join("engines").join("agy"),
            claude_data_dir: base.join("engines").join("claude"),
            tmp: base.join("tmp"),
            herdr: base.join("herdr"),
            xdg_config: base.join("xdg-config"),
            xdg_cache: base.join("xdg-cache"),
        }
    }

    /// Validate every field and cross-field constraint.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}: expected {SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        self.validate_instance()?;
        self.validate_database()?;
        self.validate_server()?;
        self.validate_storage()?;
        self.validate_terminal()?;
        self.validate_network()?;
        self.validate_sandbox()?;
        self.validate_engines()?;
        self.validate_advanced()?;
        Ok(())
    }

    fn validate_instance(&self) -> Result<(), String> {
        let name = &self.instance.name;
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
            || name == "."
            || name == ".."
        {
            return Err(format!(
                "[instance].name {name:?} must be non-empty and use only letters, digits, '-', '_', '.'"
            ));
        }
        validate_data_dir(&self.instance.data_dir)?;
        if let Some(repo) = &self.instance.default_repo
            && (repo.as_os_str().is_empty() || !repo.is_absolute() || has_parent_dir(repo))
        {
            return Err(format!(
                "[instance].default_repo must be an absolute path without '..', got {:?}",
                repo.display()
            ));
        }
        Ok(())
    }

    fn validate_database(&self) -> Result<(), String> {
        let Some(db) = &self.database else {
            return Ok(());
        };
        let has_structured = db.host.is_some()
            || db.port.is_some()
            || db.name.is_some()
            || db.user.is_some()
            || db.password.is_some();
        if db.url.is_some() && has_structured {
            return Err(
                "[database] must specify either `url` or structured fields (`host`, `port`, `name`, `user`, `password`), not both"
                    .to_string(),
            );
        }
        if db.url.is_none() {
            if !has_structured {
                return Err(
                    "[database] must specify either `url` or connection fields (`name`, `user`, etc.)"
                        .to_string(),
                );
            }
            if db.name.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err("[database].name is required when [database].url is unset".to_string());
            }
            if db.user.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err("[database].user is required when [database].url is unset".to_string());
            }
            if let Some(host) = &db.host
                && host.trim().is_empty()
            {
                return Err("[database].host must not be empty".to_string());
            }
            if db.port == Some(0) {
                return Err("[database].port must be > 0".to_string());
            }
        }
        Ok(())
    }

    fn validate_server(&self) -> Result<(), String> {
        if let Some(listen) = &self.server.listen
            && (listen.trim().is_empty() || listen.chars().any(char::is_whitespace))
        {
            return Err(format!(
                "[server].listen {listen:?} must be non-empty without whitespace"
            ));
        }
        if let Some(auth) = &self.server.auth {
            match auth.as_str() {
                "local-open" => {}
                "token" => {
                    if self.server.token.is_none() {
                        return Err("[server].auth = \"token\" requires [server].token".to_string());
                    }
                }
                other => {
                    return Err(format!(
                        "[server].auth = {other:?} is invalid: expected \"local-open\" or \"token\""
                    ));
                }
            }
        }
        if let Some(web_dir) = &self.server.web_dir
            && (web_dir.as_os_str().is_empty() || has_parent_dir(web_dir))
        {
            return Err(format!(
                "[server].web_dir {:?} must be non-empty without '..'",
                web_dir.display()
            ));
        }
        Ok(())
    }

    fn validate_storage(&self) -> Result<(), String> {
        if let Some(backend) = &self.storage.workspace_backend
            && !matches!(backend.as_str(), "auto" | "cow" | "git" | "overlay")
        {
            return Err(format!(
                "[storage].workspace_backend = {backend:?} is invalid: expected \"auto\", \"cow\", \"git\", or \"overlay\""
            ));
        }
        if let Some(mount) = &self.storage.canonical_mount
            && !mount.is_empty()
            && mount != "off"
            && (!mount.starts_with('/')
                || mount == "/"
                || mount.chars().any(char::is_whitespace)
                || has_parent_dir(Path::new(mount)))
        {
            return Err(format!(
                "[storage].canonical_mount = {mount:?} must be \"off\" or an absolute non-root path"
            ));
        }
        if self.storage.max_checkpoint_file_mb == Some(0) {
            return Err("[storage].max_checkpoint_file_mb must be > 0".to_string());
        }
        Ok(())
    }

    fn validate_terminal(&self) -> Result<(), String> {
        let backend = self.terminal.backend.as_deref().unwrap_or("herdr");
        if backend == "tmux" {
            return Err(
                "[terminal].backend = \"tmux\" is no longer supported: tmux has been removed; \
                 use \"herdr\""
                    .to_string(),
            );
        }
        if backend != "herdr" {
            return Err(format!(
                "[terminal].backend = {backend:?} is invalid: expected \"herdr\""
            ));
        }
        if let Some(session) = &self.terminal.session {
            if session.is_empty()
                || !session
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return Err(format!(
                    "[terminal].session {session:?} must use only letters, digits, '_' and '-'"
                ));
            }
            if backend == "herdr" && session == "default" {
                return Err(
                    "[terminal].session cannot be \"default\" for herdr (reserved for user's own session)"
                        .to_string(),
                );
            }
        }
        if let Some(shards) = self.terminal.shards
            && !(1..=64).contains(&shards)
        {
            return Err(format!(
                "[terminal].shards = {shards} is out of range (expected 1..=64)"
            ));
        }
        if let Some(paths) = &self.terminal.toolchain_paths {
            for entry in paths {
                if entry.is_empty()
                    || !Path::new(entry).is_absolute()
                    || entry.contains(':')
                    || entry.chars().any(char::is_control)
                {
                    return Err(format!(
                        "[terminal].toolchain_paths entry {entry:?} must be an absolute path without ':'"
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_network(&self) -> Result<(), String> {
        for (field, value) in [
            ("http_proxy", &self.network.http_proxy),
            ("https_proxy", &self.network.https_proxy),
        ] {
            if let Some(url) = value {
                if url.trim().is_empty()
                    || url.chars().any(|ch| ch.is_whitespace() || ch.is_control())
                {
                    return Err(format!(
                        "[network].{field} {url:?} must be non-empty without whitespace"
                    ));
                }
                if url_has_userinfo(url) {
                    return Err(format!(
                        "[network].{field} must not embed credentials: {PLAINTEXT_SECRET_ERROR}"
                    ));
                }
            }
        }
        if let Some(list) = &self.network.no_proxy {
            for entry in list {
                if entry.trim().is_empty()
                    || entry.contains(',')
                    || entry
                        .chars()
                        .any(|ch| ch.is_whitespace() || ch.is_control())
                {
                    return Err(format!(
                        "[network].no_proxy entry {entry:?} must be non-empty without commas or whitespace"
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_sandbox(&self) -> Result<(), String> {
        if let Some(mode) = &self.sandbox.mode
            && !matches!(mode.as_str(), "required" | "container" | "off")
        {
            return Err(format!(
                "[sandbox].mode = {mode:?} is invalid: expected \"required\", \"container\", or \"off\""
            ));
        }
        if let Some(iso) = &self.sandbox.home_isolation
            && !matches!(iso.as_str(), "whitelist" | "blacklist")
        {
            return Err(format!(
                "[sandbox].home_isolation = {iso:?} is invalid: expected \"whitelist\" or \"blacklist\""
            ));
        }
        if let Some(approve) = &self.sandbox.auto_approve
            && !matches!(approve.as_str(), "off" | "startup" | "all")
        {
            return Err(format!(
                "[sandbox].auto_approve = {approve:?} is invalid: expected \"off\", \"startup\", or \"all\""
            ));
        }
        if let Some(inherit) = &self.sandbox.inherit_host
            && !matches!(inherit.as_str(), "off" | "auto")
        {
            return Err(format!(
                "[sandbox].inherit_host = {inherit:?} is invalid: expected \"off\" or \"auto\""
            ));
        }
        if let Some(paths) = &self.sandbox.inherit_host_paths {
            for entry in paths {
                let raw = entry.to_string_lossy();
                if raw == "~" || raw == "/" || raw.contains(':') || raw.contains(',') {
                    return Err(format!(
                        "[sandbox].inherit_host_paths entry {raw:?} cannot be '/', '~', or contain ':' or ','"
                    ));
                }
                validate_tilde_or_abs(entry, "[sandbox].inherit_host_paths")?;
                if entry.is_absolute() && normalize_lexically(entry) == Path::new("/") {
                    return Err(
                        "[sandbox].inherit_host_paths entry cannot resolve to '/'".to_string()
                    );
                }
            }
        }
        if let Some(mounts) = &self.sandbox.writable_mounts {
            for spec in mounts {
                let host_str = spec.host.to_string_lossy();
                if spec.host.as_os_str().is_empty()
                    || !spec.host.is_absolute()
                    || normalize_lexically(&spec.host) == Path::new("/")
                    || has_parent_dir(&spec.host)
                    || host_str.contains(':')
                    || host_str.contains(',')
                {
                    return Err(format!(
                        "[sandbox].writable_mounts host {:?} must be a valid absolute non-root path without ':' or ','",
                        spec.host.display()
                    ));
                }
                if !matches!(spec.mode.as_str(), "rw" | "ro") {
                    return Err(format!(
                        "[sandbox].writable_mounts mode {:?} for {:?} is invalid: expected \"rw\" or \"ro\"",
                        spec.mode,
                        spec.host.display()
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_engines(&self) -> Result<(), String> {
        let claude_enabled = self.engines.claude.as_ref().is_none_or(|c| c.enabled);
        let agy_enabled = self.engines.agy.as_ref().is_some_and(|a| a.enabled);
        if !claude_enabled && !agy_enabled {
            return Err(
                "[engines] must enable at least one engine (`claude` or `agy`)".to_string(),
            );
        }
        if let Some(agy) = &self.engines.agy {
            if let Some(model) = &agy.default_model
                && (model.trim().is_empty() || model.chars().any(char::is_whitespace))
            {
                return Err(format!(
                    "[engines.agy].default_model {model:?} must be non-empty without whitespace"
                ));
            }
            if let Some(auth_source) = &agy.auth_source {
                validate_tilde_or_abs(auth_source, "[engines.agy].auth_source")?;
            }
        }
        if let Some(claude) = &self.engines.claude
            && let Some(dir) = &claude.config_dir
        {
            validate_tilde_or_abs(dir, "[engines.claude].config_dir")?;
        }
        Ok(())
    }

    fn validate_advanced(&self) -> Result<(), String> {
        if self.advanced.loop_interval_ms == Some(0) {
            return Err("[advanced].loop_interval_ms must be > 0".to_string());
        }
        Ok(())
    }

    fn herdr_session_name(&self) -> String {
        self.terminal
            .session
            .clone()
            .unwrap_or_else(|| format!("spawn-{}", self.instance.name))
    }

    fn enabled_engines(&self) -> Vec<&'static str> {
        let mut enabled = Vec::new();
        if self.engines.claude.as_ref().is_none_or(|c| c.enabled) {
            enabled.push("claude");
        }
        if self.engines.agy.as_ref().is_some_and(|a| a.enabled) {
            enabled.push("agy");
        }
        enabled
    }
}

fn validate_tilde_or_abs(path: &Path, field: &str) -> Result<(), String> {
    let raw = path.to_string_lossy();
    if raw.is_empty() || has_parent_dir(path) {
        return Err(format!(
            "{field} must be non-empty without '..', got {raw:?}"
        ));
    }
    if raw == "~" || (!raw.starts_with("~/") && !path.is_absolute()) {
        return Err(format!(
            "{field} must be an absolute path or start with '~/', got {raw:?}"
        ));
    }
    Ok(())
}

fn expand_tilde(
    path: &Path,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = env_lookup("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                format!("cannot expand {raw:?}: HOME environment variable is not set")
            })?;
        Ok(PathBuf::from(home).join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normal.components().next_back() {
                Some(Component::Normal(_)) => {
                    normal.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normal.push(".."),
            },
            other => normal.push(other),
        }
    }
    if normal.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normal
    }
}

fn validate_data_dir(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("[instance].data_dir must not be empty".to_string());
    }
    if !path.is_absolute() {
        return Err(format!(
            "[instance].data_dir must be an absolute path, not {:?}",
            path.display()
        ));
    }
    if has_parent_dir(path) {
        return Err(format!(
            "[instance].data_dir must not contain '..', got {:?}",
            path.display()
        ));
    }
    let normalized = normalize_lexically(path);
    let text = normalized.to_string_lossy();
    if BARE_SYSTEM_DIRS.contains(&text.as_ref()) {
        return Err(format!(
            "[instance].data_dir {text:?} is a bare system directory; configure a dedicated subdirectory such as {}/spawn",
            text.trim_end_matches('/')
        ));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && normalize_lexically(Path::new(&home)) == normalized
    {
        return Err(format!(
            "[instance].data_dir {text:?} cannot be $HOME directly; use a dedicated subdirectory"
        ));
    }
    Ok(())
}

fn percent_encode_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~');
        if safe {
            out.push(*byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn format_pg_host(host: &str) -> String {
    if host.starts_with('/') {
        percent_encode_userinfo(host)
    } else if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn resolve_database_url(
    db: &DatabaseConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    if let Some(url_ref) = &db.url {
        let url = url_ref.resolve_with(env_lookup)?;
        if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
            return Err(
                "[database].url must resolve to a postgres:// or postgresql:// URL".to_string(),
            );
        }
        return Ok(url);
    }
    let host = db.host.as_deref().unwrap_or("127.0.0.1");
    let port = db.port.unwrap_or(5432);
    let name = db
        .name
        .as_deref()
        .ok_or_else(|| "[database].name is required".to_string())?;
    let user = db
        .user
        .as_deref()
        .ok_or_else(|| "[database].user is required".to_string())?;
    let user_enc = percent_encode_userinfo(user);
    let host_fmt = format_pg_host(host);
    let name_enc = percent_encode_userinfo(name);
    match &db.password {
        Some(pw_ref) => {
            let pw = pw_ref.resolve_with(env_lookup)?;
            let pw_enc = percent_encode_userinfo(&pw);
            Ok(format!(
                "postgres://{user_enc}:{pw_enc}@{host_fmt}:{port}/{name_enc}"
            ))
        }
        None => Ok(format!(
            "postgres://{user_enc}@{host_fmt}:{port}/{name_enc}"
        )),
    }
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
}

fn find_program_in_paths(program: &str, dirs: &[String]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| is_executable_file(candidate))
}

/// Resolve all environment variables represented by [`SpawnConfig`].
pub fn resolve_env_entries(cfg: &SpawnConfig) -> Result<Vec<(String, String)>, String> {
    resolve_env_entries_with(cfg, &|name| std::env::var(name).ok())
}

/// Resolve all environment variables represented by [`SpawnConfig`] using a
/// caller-supplied environment lookup function.
pub fn resolve_env_entries_with(
    cfg: &SpawnConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    cfg.validate()?;
    let derived = cfg.derived_paths();
    let mut entries: BTreeMap<String, String> = BTreeMap::new();

    if let Some(db) = &cfg.database {
        entries.insert(
            "SPAWN_DATABASE_URL".to_string(),
            resolve_database_url(db, env_lookup)?,
        );
    }
    entries.insert(
        "SPAWN_WORKSPACE_ROOT".to_string(),
        derived.workspaces.display().to_string(),
    );
    if let Some(repo) = &cfg.instance.default_repo {
        entries.insert("SPAWN_GIT_REPO".to_string(), repo.display().to_string());
    }

    resolve_server_env(cfg, env_lookup, &mut entries)?;
    resolve_storage_env(cfg, &mut entries);
    resolve_terminal_env(cfg, env_lookup, &mut entries);
    resolve_engines_env(cfg, &derived, env_lookup, &mut entries)?;
    resolve_sandbox_and_network_env(cfg, &derived, env_lookup, &mut entries)?;

    Ok(entries.into_iter().collect())
}

fn resolve_server_env(
    cfg: &SpawnConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    entries: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if let Some(listen) = &cfg.server.listen {
        entries.insert("SPAWN_SERVE_ADDRESS".to_string(), listen.clone());
    }
    if cfg.server.auth.as_deref() == Some("local-open") {
        entries.insert("SPAWN_SERVE_LOCAL_OPEN".to_string(), "1".to_string());
    }
    if let Some(token_ref) = &cfg.server.token {
        entries.insert(
            "SPAWN_SERVE_TOKEN".to_string(),
            token_ref.resolve_with(env_lookup)?,
        );
    }
    if let Some(web_dir) = &cfg.server.web_dir {
        entries.insert("SPAWN_WEB_DIR".to_string(), web_dir.display().to_string());
    }
    Ok(())
}

fn resolve_storage_env(cfg: &SpawnConfig, entries: &mut BTreeMap<String, String>) {
    entries.insert(
        "SPAWN_WORKSPACE_BACKEND".to_string(),
        cfg.storage
            .workspace_backend
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
    );
    entries.insert(
        "SPAWN_CAS_DEDUP".to_string(),
        if cfg.storage.cas_dedup.unwrap_or(true) {
            "1".to_string()
        } else {
            "0".to_string()
        },
    );
    if let Some(max_mb) = cfg.storage.max_checkpoint_file_mb {
        entries.insert("SPAWN_MAX_FILE_MB".to_string(), max_mb.to_string());
    }
    if let Some(canonical) = &cfg.storage.canonical_mount
        && !canonical.is_empty()
    {
        entries.insert(
            "SPAWN_SANDBOX_CANONICAL_WORKSPACE".to_string(),
            canonical.clone(),
        );
    }
}

fn resolve_terminal_env(
    cfg: &SpawnConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    entries: &mut BTreeMap<String, String>,
) {
    entries.insert(
        "SPAWN_TERMINAL_BACKEND".to_string(),
        cfg.terminal
            .backend
            .clone()
            .unwrap_or_else(|| "herdr".to_string()),
    );
    entries.insert("SPAWN_HERDR_SESSION".to_string(), cfg.herdr_session_name());
    if let Some(shards) = cfg.terminal.shards {
        entries.insert("SPAWN_HERDR_SHARDS".to_string(), shards.to_string());
    }
    if let Some(toolchain) = &cfg.terminal.toolchain_paths
        && !toolchain.is_empty()
    {
        if let Some(herdr_bin) = find_program_in_paths("herdr", toolchain) {
            entries.insert(
                "SPAWN_HERDR_PROGRAM".to_string(),
                herdr_bin.display().to_string(),
            );
        }
        let mut combined = toolchain.join(":");
        if let Some(existing) = env_lookup("PATH")
            && !existing.is_empty()
        {
            combined.push(':');
            combined.push_str(&existing);
        }
        entries.insert("PATH".to_string(), combined);
    }
}

fn resolve_engines_env(
    cfg: &SpawnConfig,
    derived: &DerivedPaths,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    entries: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    entries.insert("SPAWN_ENGINES".to_string(), cfg.enabled_engines().join(","));

    let agy_disabled = cfg.engines.agy.as_ref().is_some_and(|a| !a.enabled);
    if !agy_disabled {
        entries.insert(
            "SPAWN_AGY_DATA_DIR".to_string(),
            derived.agy_data_dir.display().to_string(),
        );
        let isolate_workers = cfg
            .engines
            .agy
            .as_ref()
            .and_then(|a| a.isolate_workers)
            .unwrap_or(true);
        let isolate_brain = cfg
            .engines
            .agy
            .as_ref()
            .and_then(|a| a.isolate_brain)
            .unwrap_or(true);
        entries.insert(
            "SPAWN_AGY_ISOLATE_WORKERS".to_string(),
            if isolate_workers { "1" } else { "0" }.to_string(),
        );
        entries.insert(
            "SPAWN_AGY_ISOLATE_BRAIN".to_string(),
            if isolate_brain { "1" } else { "0" }.to_string(),
        );
    }
    if let Some(agy) = &cfg.engines.agy
        && agy.enabled
    {
        if let Some(auth_source) = &agy.auth_source {
            let expanded = expand_tilde(auth_source, env_lookup)?;
            entries.insert("SPAWN_AGY_HOME".to_string(), expanded.display().to_string());
        }
        if let Some(model) = &agy.default_model {
            entries.insert("SPAWN_AGY_MODEL".to_string(), model.clone());
        }
    }
    if cfg.engines.claude.as_ref().is_none_or(|c| c.enabled)
        && let Some(claude) = &cfg.engines.claude
    {
        if let Some(dir) = &claude.config_dir {
            let expanded = expand_tilde(dir, env_lookup)?;
            entries.insert(
                "SPAWN_CLAUDE_CONFIG_DIR".to_string(),
                expanded.display().to_string(),
            );
        }
        if claude.sandbox == Some(true) {
            entries.insert("SPAWN_CLAUDE_SANDBOX".to_string(), "1".to_string());
            entries.insert(
                "SPAWN_CLAUDE_DATA_DIR".to_string(),
                derived.claude_data_dir.display().to_string(),
            );
        }
    }
    Ok(())
}

fn resolve_sandbox_and_network_env(
    cfg: &SpawnConfig,
    derived: &DerivedPaths,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    entries: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    entries.insert(
        "SPAWN_ENGINE_VIEW".to_string(),
        cfg.sandbox
            .mode
            .clone()
            .unwrap_or_else(|| "required".to_string()),
    );
    if let Some(mounts) = &cfg.sandbox.writable_mounts
        && !mounts.is_empty()
    {
        let joined = mounts
            .iter()
            .map(|m| {
                if m.mode == "ro" {
                    format!("{}:ro", m.host.display())
                } else {
                    m.host.display().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        entries.insert("SPAWN_ENGINE_VIEW_WRITABLE".to_string(), joined);
    }
    if let Some(inherit) = &cfg.sandbox.inherit_host {
        entries.insert("SPAWN_SANDBOX_INHERIT_HOST".to_string(), inherit.clone());
    }
    if let Some(paths) = &cfg.sandbox.inherit_host_paths
        && !paths.is_empty()
    {
        let mut expanded = Vec::with_capacity(paths.len());
        for path in paths {
            expanded.push(expand_tilde(path, env_lookup)?.display().to_string());
        }
        entries.insert(
            "SPAWN_SANDBOX_INHERIT_HOST_PATHS".to_string(),
            expanded.join(","),
        );
    }
    entries.insert(
        "SPAWN_SANDBOX_HOME_ISOLATION".to_string(),
        cfg.sandbox
            .home_isolation
            .clone()
            .unwrap_or_else(|| "whitelist".to_string()),
    );
    entries.insert(
        "SPAWN_AUTO_APPROVE".to_string(),
        cfg.sandbox
            .auto_approve
            .clone()
            .unwrap_or_else(|| "startup".to_string()),
    );

    if let Some(ms) = cfg.advanced.loop_interval_ms {
        entries.insert("SPAWN_LOOP_INTERVAL_MS".to_string(), ms.to_string());
    }
    if let Some(ms) = cfg.advanced.screen_quiet_ms {
        entries.insert("SPAWN_SCREEN_QUIET_MS".to_string(), ms.to_string());
    }
    if let Some(secs) = cfg.advanced.message_taken_secs {
        entries.insert("SPAWN_MESSAGE_TAKEN_SECS".to_string(), secs.to_string());
    }

    entries.insert("TMPDIR".to_string(), derived.tmp.display().to_string());
    entries.insert(
        "XDG_CONFIG_HOME".to_string(),
        derived.xdg_config.display().to_string(),
    );
    entries.insert(
        "XDG_CACHE_HOME".to_string(),
        derived.xdg_cache.display().to_string(),
    );

    if let Some(proxy) = &cfg.network.http_proxy
        && !proxy.is_empty()
    {
        entries.insert("http_proxy".to_string(), proxy.clone());
        entries.insert("HTTP_PROXY".to_string(), proxy.clone());
    }
    if let Some(proxy) = &cfg.network.https_proxy
        && !proxy.is_empty()
    {
        entries.insert("https_proxy".to_string(), proxy.clone());
        entries.insert("HTTPS_PROXY".to_string(), proxy.clone());
    }
    if let Some(no_proxy) = &cfg.network.no_proxy
        && !no_proxy.is_empty()
    {
        let joined = no_proxy.join(",");
        entries.insert("no_proxy".to_string(), joined.clone());
        entries.insert("NO_PROXY".to_string(), joined);
    }
    Ok(())
}

fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("PASSWORD")
        || upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("KEY")
}

fn url_has_userinfo(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@')
}

fn redact_url_credentials(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let (before_query, query) = rest
        .split_once('?')
        .map_or((rest, None), |(a, q)| (a, Some(q)));
    let (authority, path) = before_query
        .split_once('/')
        .map_or((before_query, ""), |(a, p)| (a, p));

    let mut changed = false;
    let new_authority = if let Some((userinfo, hostport)) = authority.rsplit_once('@') {
        if userinfo.is_empty() {
            authority.to_string()
        } else {
            changed = true;
            if let Some((user, _pw)) = userinfo.split_once(':') {
                format!("{user}:{REDACTED}@{hostport}")
            } else {
                format!("{REDACTED}@{hostport}")
            }
        }
    } else {
        authority.to_string()
    };

    let new_query = query.map(|q| {
        q.split('&')
            .map(|pair| {
                if let Some((k, _)) = pair.split_once('=')
                    && (k.eq_ignore_ascii_case("password") || k.eq_ignore_ascii_case("token"))
                {
                    changed = true;
                    format!("{k}={REDACTED}")
                } else {
                    pair.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("&")
    });

    if !changed {
        return None;
    }

    let mut out = format!("{scheme}://{new_authority}");
    if !path.is_empty() || before_query.contains('/') {
        out.push('/');
        out.push_str(path);
    }
    if let Some(q) = new_query {
        out.push('?');
        out.push_str(&q);
    }
    Some(out)
}

/// Redact secrets in resolved environment entries (`"***"` or URL password masking).
pub fn redact_env_entries(
    cfg: &SpawnConfig,
    entries: Vec<(String, String)>,
) -> BTreeMap<String, String> {
    let db_has_secret_ref = cfg
        .database
        .as_ref()
        .is_some_and(|db| db.url.is_some() || db.password.is_some());
    let server_has_secret_ref = cfg.server.token.is_some();

    let mut out = BTreeMap::new();
    for (name, value) in entries {
        let redacted = if name == "SPAWN_DATABASE_URL" {
            if let Some(masked) = redact_url_credentials(&value) {
                masked
            } else if db_has_secret_ref {
                REDACTED.to_string()
            } else {
                value
            }
        } else if is_secret_env_name(&name)
            || (server_has_secret_ref && name == "SPAWN_SERVE_TOKEN")
        {
            REDACTED.to_string()
        } else if let Some(masked) = redact_url_credentials(&value) {
            masked
        } else {
            value
        };
        out.insert(name, redacted);
    }
    out
}

/// Produce the redacted JSON view of the effective configuration and environment.
pub fn effective_json(cfg: &SpawnConfig) -> Result<Value, String> {
    effective_json_with(cfg, &|name| std::env::var(name).ok())
}

pub fn redacted_config_for_display(cfg: &SpawnConfig) -> SpawnConfig {
    let mut out = cfg.clone();
    out.network.http_proxy = out
        .network
        .http_proxy
        .as_deref()
        .map(|u| redact_url_credentials(u).unwrap_or_else(|| u.to_string()));
    out.network.https_proxy = out
        .network
        .https_proxy
        .as_deref()
        .map(|u| redact_url_credentials(u).unwrap_or_else(|| u.to_string()));
    out
}

/// Produce the redacted JSON view of the effective configuration using a
/// caller-supplied environment lookup function.
pub fn effective_json_with(
    cfg: &SpawnConfig,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Value, String> {
    let raw_entries = resolve_env_entries_with(cfg, env_lookup)?;
    let env_map = redact_env_entries(cfg, raw_entries);
    let safe_cfg = redacted_config_for_display(cfg);
    Ok(json!({
        "schema_version": cfg.schema_version,
        "instance": {
            "name": cfg.instance.name,
            "data_dir": cfg.instance.data_dir,
            "default_repo": cfg.instance.default_repo,
            "derived_paths": cfg.derived_paths(),
        },
        "config": safe_cfg,
        "env": env_map,
    }))
}

#[derive(Debug, Clone, Serialize)]
struct CheckReport {
    ok: bool,
    config_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<Value>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FilesystemInfo {
    path: PathBuf,
    probed_path: PathBuf,
    filesystem: String,
    fs_type: String,
    mount_point: String,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    on_root_filesystem: bool,
}

fn df_filesystem_info(path: &Path) -> Result<FilesystemInfo, String> {
    let probed = nearest_existing_ancestor(path);
    let output = Command::new("df")
        .args(["-Pk", "-T", "--"])
        .arg(&probed)
        .output()
        .map_err(|error| format!("run df -Pk -T {}: {error}", probed.display()))?;
    if !output.status.success() {
        return Err(format!(
            "df {}: {}",
            probed.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df {}: missing output row", probed.display()))?;
    parse_df_t_line(line, path, &probed)
        .ok_or_else(|| format!("df {}: unexpected output row {line:?}", probed.display()))
}

fn parse_df_t_line(line: &str, requested: &Path, probed: &Path) -> Option<FilesystemInfo> {
    let mut parts = line.split_whitespace();
    let filesystem = parts.next()?.to_string();
    let fs_type = parts.next()?.to_string();
    let total_kib: u64 = parts.next()?.parse().ok()?;
    let used_kib: u64 = parts.next()?.parse().ok()?;
    let avail_kib: u64 = parts.next()?.parse().ok()?;
    let _capacity = parts.next()?;
    let mount_point = parts.collect::<Vec<_>>().join(" ");
    if mount_point.is_empty() {
        return None;
    }
    Some(FilesystemInfo {
        path: requested.to_path_buf(),
        probed_path: probed.to_path_buf(),
        filesystem,
        fs_type,
        mount_point: mount_point.clone(),
        total_bytes: total_kib.saturating_mul(1024),
        used_bytes: used_kib.saturating_mul(1024),
        available_bytes: avail_kib.saturating_mul(1024),
        on_root_filesystem: mount_point == "/",
    })
}

fn other_persistent_mounts() -> Vec<String> {
    let Ok(output) = Command::new("df").args(["-Pk", "-T"]).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut mounts = Vec::new();
    for line in text.lines().skip(1) {
        let Some(info) = parse_df_t_line(line, Path::new("/"), Path::new("/")) else {
            continue;
        };
        if info.mount_point == "/"
            || info.mount_point.starts_with("/boot")
            || info.mount_point.starts_with("/dev")
            || info.mount_point.starts_with("/proc")
            || info.mount_point.starts_with("/sys")
            || info.mount_point.starts_with("/run")
            || matches!(
                info.fs_type.as_str(),
                "tmpfs"
                    | "devtmpfs"
                    | "squashfs"
                    | "overlay"
                    | "proc"
                    | "sysfs"
                    | "cgroup"
                    | "cgroup2"
            )
        {
            continue;
        }
        if !mounts.contains(&info.mount_point) {
            mounts.push(format!("{} ({})", info.mount_point, info.fs_type));
        }
    }
    mounts
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = normalize_lexically(path);
    loop {
        if current.exists() {
            return current;
        }
        let Some(parent) = current.parent() else {
            return PathBuf::from("/");
        };
        if parent.as_os_str().is_empty() {
            return PathBuf::from("/");
        }
        current = parent.to_path_buf();
    }
}

fn path_writable(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
}

fn check_secret_file_permissions(secret: &SecretRef, warnings: &mut Vec<String>) {
    let SecretRef::FromFile { from_file } = secret else {
        return;
    };
    let Ok(meta) = fs::metadata(from_file) else {
        return;
    };
    let mode = meta.permissions().mode();
    if (mode & 0o077) != 0 {
        warnings.push(format!(
            "secret file {} has permissions {:04o}; consider restricting to 0600",
            from_file.display(),
            mode & 0o777
        ));
    }
}

fn collect_config_warnings(cfg: &SpawnConfig, warnings: &mut Vec<String>) {
    let data_dir = &cfg.instance.data_dir;
    if !data_dir.exists() || data_dir.is_dir() {
        let anchor = nearest_existing_ancestor(data_dir);
        if !path_writable(&anchor) {
            warnings.push(format!(
                "data_dir anchor {} does not appear writable by current user",
                anchor.display()
            ));
        }
        if let Ok(info) = df_filesystem_info(data_dir)
            && info.on_root_filesystem
        {
            let others = other_persistent_mounts();
            if !others.is_empty() {
                warnings.push(format!(
                    "data_dir {} sits on the '/' filesystem while dedicated mounts exist ({})",
                    data_dir.display(),
                    others.join(", ")
                ));
            }
        }
    }

    if let Some(repo) = &cfg.instance.default_repo
        && !repo.exists()
    {
        warnings.push(format!(
            "default_repo {} does not exist yet",
            repo.display()
        ));
    }
    if let Some(mounts) = &cfg.sandbox.writable_mounts {
        for spec in mounts {
            if !spec.host.exists() {
                warnings.push(format!(
                    "writable mount host path {} does not exist yet",
                    spec.host.display()
                ));
            }
        }
    }
    if cfg.storage.workspace_backend.as_deref() == Some("overlay") {
        warnings.push(
            "storage.workspace_backend = \"overlay\" is experimental and refused by `spawn loop`"
                .to_string(),
        );
    }
    if let Some(db) = &cfg.database {
        if let Some(url) = &db.url {
            check_secret_file_permissions(url, warnings);
        }
        if let Some(pw) = &db.password {
            check_secret_file_permissions(pw, warnings);
        }
    }
    if let Some(token) = &cfg.server.token {
        check_secret_file_permissions(token, warnings);
    }
}

fn evaluate_config_check(path: &Path) -> CheckReport {
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    let cfg = match SpawnConfig::from_file(path) {
        Ok(cfg) => cfg,
        Err(error) => {
            errors.push(error);
            return CheckReport {
                ok: false,
                config_path: path.to_path_buf(),
                schema_version: None,
                instance: None,
                warnings,
                errors,
            };
        }
    };

    if cfg.instance.data_dir.exists() && !cfg.instance.data_dir.is_dir() {
        errors.push(format!(
            "[instance].data_dir {} exists and is not a directory",
            cfg.instance.data_dir.display()
        ));
    } else {
        let anchor = nearest_existing_ancestor(&cfg.instance.data_dir);
        if !anchor.is_dir() {
            errors.push(format!(
                "nearest ancestor {} of data_dir {} is not a directory",
                anchor.display(),
                cfg.instance.data_dir.display()
            ));
        }
    }

    if let Err(error) = resolve_env_entries(&cfg) {
        errors.push(error);
    }

    collect_config_warnings(&cfg, &mut warnings);

    let derived = cfg.derived_paths();
    let ok = errors.is_empty();
    CheckReport {
        ok,
        config_path: path.to_path_buf(),
        schema_version: Some(cfg.schema_version),
        instance: Some(json!({
            "name": cfg.instance.name,
            "data_dir": cfg.instance.data_dir,
            "derived_paths": derived,
        })),
        warnings,
        errors,
    }
}

/// Ensure `<data_dir>` and all derived subdirectories exist before launching
/// a runtime/daemon command via `--config`.
pub fn ensure_data_dirs(cfg: &SpawnConfig) -> Result<(), String> {
    let derived = cfg.derived_paths();
    fs::create_dir_all(&cfg.instance.data_dir).map_err(|error| {
        format!(
            "create data_dir {}: {error}",
            cfg.instance.data_dir.display()
        )
    })?;
    for dir in derived.all() {
        fs::create_dir_all(dir)
            .map_err(|error| format!("create derived directory {}: {error}", dir.display()))?;
    }
    Ok(())
}

/// Re-exec the current binary with resolved environment from `config_path`,
/// keeping the same PID (`CommandExt::exec`, 100% safe Rust).
pub fn exec_with_config(
    config_path: &Path,
    command: &str,
    rest: &[String],
    applied_env_key: &str,
) -> Result<i32, String> {
    let cfg = SpawnConfig::from_file(config_path)?;
    let env_entries = resolve_env_entries(&cfg)?;
    if matches!(command, "loop" | "tick" | "serve" | "integrate" | "init-db") {
        ensure_data_dirs(&cfg)?;
    }

    let exe = std::env::current_exe()
        .map_err(|error| format!("locate current executable for --config re-exec: {error}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg(command).args(rest);
    cmd.envs(env_entries);
    cmd.env("SPAWN_CONFIG", config_path);
    cmd.env(applied_env_key, config_path);

    let error = cmd.exec();
    Err(format!(
        "re-exec with config {}: {error}",
        config_path.display()
    ))
}

struct ParsedCliFlags {
    config_path: Option<PathBuf>,
    switches: BTreeSet<String>,
    values: BTreeMap<String, String>,
}

fn parse_config_flag(
    args: &[String],
    global_config: Option<&str>,
    allowed_switches: &[&str],
    allowed_values: &[&str],
) -> Result<ParsedCliFlags, String> {
    let mut switches = BTreeSet::new();
    let mut values = BTreeMap::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--config" || arg == "-c" {
            let Some(val) = iter.next() else {
                return Err(format!("{arg} needs a value"));
            };
            if val.starts_with("--") {
                return Err(format!("{arg} needs a value, found {val}"));
            }
            values.insert("config".to_string(), val.clone());
            continue;
        }
        if let Some(val) = arg.strip_prefix("--config=") {
            if val.is_empty() {
                return Err("--config needs a value".to_string());
            }
            values.insert("config".to_string(), val.to_string());
            continue;
        }
        if let Some(val) = arg.strip_prefix("-c=") {
            if val.is_empty() {
                return Err("-c needs a value".to_string());
            }
            values.insert("config".to_string(), val.to_string());
            continue;
        }
        let Some(name) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg}"));
        };
        if allowed_switches.contains(&name) {
            if let Some(next) = iter.peek()
                && !next.starts_with('-')
            {
                return Err(format!("--{name} takes no value, found {next}"));
            }
            switches.insert(name.to_string());
            continue;
        }
        if allowed_values.contains(&name) {
            let Some(val) = iter.next() else {
                return Err(format!("--{name} needs a value"));
            };
            if val.starts_with("--") {
                return Err(format!("--{name} needs a value, found {val}"));
            }
            values.insert(name.to_string(), val.clone());
            continue;
        }
        return Err(format!("unknown flag --{name}"));
    }

    let config_path = values
        .get("config")
        .cloned()
        .or_else(|| global_config.map(str::to_string))
        .or_else(|| std::env::var("SPAWN_CONFIG").ok().filter(|v| !v.is_empty()))
        .map(PathBuf::from);
    Ok(ParsedCliFlags {
        config_path,
        switches,
        values,
    })
}

/// Entrypoint for `spawn config <check|effective|init|docker-plan>`.
pub fn config_command(global_config: Option<&str>, rest: &[String]) -> Result<i32, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Err(CONFIG_USAGE.to_string());
    };
    match verb.as_str() {
        "check" => cmd_config_check(global_config, args),
        "effective" => cmd_config_effective(global_config, args),
        "init" => cmd_config_init(args),
        "docker-plan" => cmd_config_docker_plan(global_config, args),
        other => Err(format!(
            "unknown `spawn config` verb {other:?}\n{CONFIG_USAGE}"
        )),
    }
}

fn cmd_config_check(global_config: Option<&str>, args: &[String]) -> Result<i32, String> {
    let parsed = parse_config_flag(args, global_config, &["json"], &[])?;
    let json_mode = parsed.switches.contains("json");
    let Some(path) = parsed.config_path else {
        return Err("missing config path: pass --config <path> or set SPAWN_CONFIG".to_string());
    };
    let report = evaluate_config_check(&path);
    if json_mode {
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize config check report: {error}"))?;
        println!("{rendered}");
        return Ok(i32::from(!report.ok));
    }
    if report.ok {
        println!("spawn config check: OK ({})", report.config_path.display());
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        Ok(0)
    } else {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        Err(report.errors.join("; "))
    }
}

fn cmd_config_effective(global_config: Option<&str>, args: &[String]) -> Result<i32, String> {
    let parsed = parse_config_flag(args, global_config, &["json"], &[])?;
    let json_mode = parsed.switches.contains("json");
    let Some(path) = parsed.config_path else {
        return Err("missing config path: pass --config <path> or set SPAWN_CONFIG".to_string());
    };
    let cfg = SpawnConfig::from_file(&path)?;
    let effective = effective_json(&cfg)?;
    if json_mode {
        let rendered = serde_json::to_string_pretty(&effective)
            .map_err(|error| format!("serialize effective config: {error}"))?;
        println!("{rendered}");
    } else {
        println!(
            "instance: {} ({})",
            cfg.instance.name,
            cfg.instance.data_dir.display()
        );
        if let Some(env_obj) = effective["env"].as_object() {
            for (key, value) in env_obj {
                println!("{key}={}", value.as_str().unwrap_or_default());
            }
        }
    }
    Ok(0)
}

fn cmd_config_init(args: &[String]) -> Result<i32, String> {
    let parsed = parse_config_flag(args, None, &["from-host"], &["data-dir", "name", "output"])?;
    let data_dir_str = parsed
        .values
        .get("data-dir")
        .ok_or_else(|| "--data-dir <dir> is required".to_string())?;
    let data_dir = PathBuf::from(data_dir_str);
    validate_data_dir(&data_dir)?;

    let name = parsed
        .values
        .get("name")
        .cloned()
        .unwrap_or_else(default_instance_name);
    let toml_text = if parsed.switches.contains("from-host") {
        render_from_host_toml(&name, &data_dir)
    } else {
        render_init_toml(&name, &data_dir)
    };
    SpawnConfig::from_toml_str(&toml_text)?;

    if let Some(output) = parsed.values.get("output") {
        let out_path = Path::new(output);
        if out_path.exists() {
            return Err(format!(
                "refusing to overwrite existing config file {}",
                out_path.display()
            ));
        }
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create parent {}: {error}", parent.display()))?;
        }
        fs::write(out_path, &toml_text)
            .map_err(|error| format!("write {}: {error}", out_path.display()))?;
        println!("wrote {}", out_path.display());
    } else {
        print!("{toml_text}");
    }
    Ok(0)
}

fn escape_toml_str(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_init_toml(name: &str, data_dir: &Path) -> String {
    let dir_escaped = escape_toml_str(&data_dir.display().to_string());
    let name_escaped = escape_toml_str(name);
    format!(
        r#"schema_version = 1

[instance]
name = "{name_escaped}"
data_dir = "{dir_escaped}"
# default_repo = "/path/to/repo.git"

# [database]
# host = "127.0.0.1"
# port = 5432
# name = "spawn"
# user = "spawn"
# password = {{ from_env = "SPAWN_PG_PASSWORD" }}
# # Or full URL from secret ref (never plaintext):
# # url = {{ from_file = "/etc/spawn/database.url" }}

[server]
listen = "127.0.0.1:8787"
auth = "local-open"
# token = {{ from_file = "/etc/spawn/serve.token" }}

[storage]
workspace_backend = "auto"
canonical_mount = "/workspace"
cas_dedup = true

[terminal]
backend = "herdr"
# session = "spawn-{name_escaped}"
toolchain_paths = []

[sandbox]
mode = "required"
home_isolation = "whitelist"
auto_approve = "startup"
writable_mounts = []

[engines.claude]
enabled = true

# [engines.agy]
# enabled = true
# default_model = "gemini-2.5-pro"
# auth_source = "~/.gemini/antigravity-cli"
# isolate_workers = true
# isolate_brain = true
"#
    )
}

const HOST_SAFE_INHERIT_DIRS: &[&str] = &[
    ".rustup",
    ".cargo/bin",
    ".nvm",
    ".pyenv",
    ".asdf",
    ".mise",
    ".local/opt",
    ".local/share/mise",
];

const HOST_SAFE_INHERIT_FILES: &[&str] = &[".cargo/config.toml", ".cargo/config"];

struct HostInheritScan {
    home: Option<PathBuf>,
    safe_dirs: Vec<(String, PathBuf)>,
    safe_files: Vec<(String, PathBuf)>,
    gitconfig: Option<PathBuf>,
    skipped: Vec<Value>,
}

fn scan_host_inherit_home(home: Option<&Path>) -> HostInheritScan {
    let mut scan = HostInheritScan {
        home: home.map(Path::to_path_buf),
        safe_dirs: Vec::new(),
        safe_files: Vec::new(),
        gitconfig: None,
        skipped: Vec::new(),
    };
    let Some(home_dir) = home.filter(|p| p.is_dir()) else {
        return scan;
    };
    for rel in HOST_SAFE_INHERIT_DIRS {
        let candidate = home_dir.join(rel);
        if candidate.is_dir() {
            scan.safe_dirs.push(((*rel).to_string(), candidate));
        }
    }
    for rel in HOST_SAFE_INHERIT_FILES {
        let candidate = home_dir.join(rel);
        if candidate.is_file() {
            scan.safe_files.push(((*rel).to_string(), candidate));
        }
    }
    let gitcfg = home_dir.join(".gitconfig");
    if gitcfg.is_file() {
        match fs::read_to_string(&gitcfg) {
            Ok(text) if spawn_host::engine_view::gitconfig_has_no_inline_secrets(&text) => {
                scan.gitconfig = Some(gitcfg);
            }
            Ok(_) => scan.skipped.push(json!({
                "path": gitcfg,
                "reason": "contains inline credentials or auth headers",
            })),
            Err(error) => scan.skipped.push(json!({
                "path": gitcfg,
                "reason": format!("unreadable: {error}"),
            })),
        }
    }
    scan
}

fn scan_host_inherit() -> HostInheritScan {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    scan_host_inherit_home(home.as_deref())
}

fn find_program_with_common_dirs(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(path_env) = std::env::var("PATH") {
        for part in path_env.split(':').filter(|s| !s.is_empty()) {
            dirs.push(part.to_string());
        }
    }
    for extra in ["/data00/spawn-toolchain/bin"] {
        if !dirs.iter().any(|d| d == extra) {
            dirs.push(extra.to_string());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        for rel in [".local/bin", ".cargo/bin"] {
            let p = PathBuf::from(&home).join(rel).display().to_string();
            if !dirs.iter().any(|d| d == &p) {
                dirs.push(p);
            }
        }
    }
    find_program_in_paths(name, &dirs)
}

fn render_from_host_toml(name: &str, data_dir: &Path) -> String {
    let dir_escaped = escape_toml_str(&data_dir.display().to_string());
    let name_escaped = escape_toml_str(name);
    let view_cfg = spawn_host::ViewConfig::from_env(&|k| std::env::var(k).ok());
    let bwrap_cap = spawn_host::probe_bwrap("bwrap", &view_cfg);
    let sandbox_mode = match bwrap_cap {
        spawn_host::BwrapCapability::BlockedInContainer(_) => "container",
        _ => "required",
    };

    let mut toolchain_dirs: Vec<String> = Vec::new();
    for tool in ["git", "herdr", "bwrap", "claude", "agy"] {
        if let Some(found) = find_program_with_common_dirs(tool)
            && let Some(parent) = found.parent()
        {
            let p = parent.display().to_string();
            if !matches!(
                p.as_str(),
                "/usr/bin" | "/bin" | "/usr/local/bin" | "/usr/sbin" | "/sbin"
            ) && !toolchain_dirs.contains(&p)
            {
                toolchain_dirs.push(p);
            }
        }
    }
    let toolchain_toml = toolchain_dirs
        .iter()
        .map(|d| format!("\"{}\"", escape_toml_str(d)))
        .collect::<Vec<_>>()
        .join(", ");

    let has_agy = find_program_with_common_dirs("agy").is_some()
        || std::env::var("HOME")
            .ok()
            .is_some_and(|h| Path::new(&h).join(".gemini/antigravity-cli").is_dir());
    let claude_sandbox =
        matches!(bwrap_cap, spawn_host::BwrapCapability::Ready) || sandbox_mode == "container";
    let agy_block = if has_agy {
        r#"
[engines.agy]
enabled = true
auth_source = "~/.gemini/antigravity-cli"
isolate_workers = true
isolate_brain = true
"#
    } else {
        r#"
# [engines.agy]
# enabled = true
# auth_source = "~/.gemini/antigravity-cli"
# isolate_workers = true
# isolate_brain = true
"#
    };

    format!(
        r#"schema_version = 1

[instance]
name = "{name_escaped}"
data_dir = "{dir_escaped}"

[server]
listen = "127.0.0.1:8787"
auth = "local-open"

[storage]
workspace_backend = "auto"
canonical_mount = "/workspace"
cas_dedup = true

[terminal]
backend = "herdr"
toolchain_paths = [{toolchain_toml}]

[sandbox]
mode = "{sandbox_mode}"
home_isolation = "whitelist"
inherit_host = "auto"
auto_approve = "startup"
writable_mounts = []

[engines.claude]
enabled = true
sandbox = {claude_sandbox}
{agy_block}"#
    )
}

fn shell_quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn collect_docker_readonly_mounts(
    scan: &HostInheritScan,
    config_path: Option<&Path>,
) -> Vec<(String, String)> {
    let container_home = Path::new("/home/spawn");
    let mut readonly_mounts: Vec<(String, String)> = Vec::new();
    for (rel, host_path) in &scan.safe_dirs {
        readonly_mounts.push((
            host_path.display().to_string(),
            container_home.join(rel).display().to_string(),
        ));
    }
    for (rel, host_path) in &scan.safe_files {
        readonly_mounts.push((
            host_path.display().to_string(),
            container_home.join(rel).display().to_string(),
        ));
    }
    if let Some(gitcfg) = &scan.gitconfig {
        readonly_mounts.push((
            gitcfg.display().to_string(),
            container_home.join(".gitconfig").display().to_string(),
        ));
    }
    if let Some(cfg_path) = config_path {
        readonly_mounts.push((
            cfg_path.display().to_string(),
            "/etc/spawn/spawn.toml".to_string(),
        ));
    }
    readonly_mounts
}

fn build_docker_plan_report(
    config_path: Option<&Path>,
    data_dir: &Path,
    repo: Option<&Path>,
    image: &str,
) -> Value {
    let scan = scan_host_inherit();
    let readonly_mounts = collect_docker_readonly_mounts(&scan, config_path);
    let mut writable_mounts = vec![(data_dir.display().to_string(), "/var/lib/spawn".to_string())];
    if let Some(repo_path) = repo {
        writable_mounts.push((repo_path.display().to_string(), "/repo".to_string()));
    }

    let mut env = BTreeMap::from([
        ("SPAWN_ENGINE_VIEW".to_string(), "container".to_string()),
        ("SPAWN_SANDBOX_INHERIT_HOST".to_string(), "auto".to_string()),
        ("SPAWN_TERMINAL_BACKEND".to_string(), "herdr".to_string()),
    ]);
    if config_path.is_some() {
        env.insert(
            "SPAWN_CONFIG".to_string(),
            "/etc/spawn/spawn.toml".to_string(),
        );
    }
    if repo.is_some() {
        env.insert("SPAWN_GIT_REPO".to_string(), "/repo".to_string());
    }
    for var in [
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
    ] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
            && !url_has_userinfo(&val)
        {
            env.insert(var.to_string(), val);
        }
    }

    let mut docker_tokens = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "-i".to_string(),
    ];
    for (k, v) in &env {
        docker_tokens.push("-e".to_string());
        docker_tokens.push(format!("{k}={v}"));
    }
    for (src, dst) in &writable_mounts {
        docker_tokens.push("-v".to_string());
        docker_tokens.push(format!("{src}:{dst}:rw"));
    }
    for (src, dst) in &readonly_mounts {
        docker_tokens.push("-v".to_string());
        docker_tokens.push(format!("{src}:{dst}:ro"));
    }
    docker_tokens.push(image.to_string());
    docker_tokens.extend(["doctor".to_string(), "--json".to_string()]);
    let docker_command = docker_tokens
        .iter()
        .map(|tok| shell_quote_arg(tok))
        .collect::<Vec<_>>()
        .join(" ");

    json!({
        "ok": true,
        "image": image,
        "data_dir": data_dir,
        "repo": repo,
        "env": env,
        "writable_mounts": writable_mounts.iter().map(|(s, d)| json!({ "host": s, "container": d, "mode": "rw" })).collect::<Vec<_>>(),
        "readonly_mounts": readonly_mounts.iter().map(|(s, d)| json!({ "host": s, "container": d, "mode": "ro" })).collect::<Vec<_>>(),
        "skipped_host_paths": scan.skipped,
        "docker_command": docker_command,
    })
}

fn cmd_config_docker_plan(global_config: Option<&str>, rest: &[String]) -> Result<i32, String> {
    let parsed = parse_config_flag(rest, global_config, &["json"], &["data-dir", "image"])?;
    let json_mode = parsed.switches.contains("json");
    let loaded = parsed
        .config_path
        .as_ref()
        .map(|p| SpawnConfig::from_file(p))
        .transpose()?;
    let data_dir = parsed
        .values
        .get("data-dir")
        .map(PathBuf::from)
        .or_else(|| loaded.as_ref().map(|cfg| cfg.instance.data_dir.clone()))
        .ok_or_else(|| "missing data_dir: pass --config <path> or --data-dir <dir>".to_string())?;
    validate_data_dir(&data_dir)?;
    let repo = loaded
        .as_ref()
        .and_then(|cfg| cfg.instance.default_repo.clone());
    let image = parsed
        .values
        .get("image")
        .cloned()
        .unwrap_or_else(|| "spawn:latest".to_string());

    let plan = build_docker_plan_report(
        parsed.config_path.as_deref(),
        &data_dir,
        repo.as_deref(),
        &image,
    );
    if json_mode {
        let rendered = serde_json::to_string_pretty(&plan)
            .map_err(|error| format!("serialize docker plan: {error}"))?;
        println!("{rendered}");
    } else {
        println!("{}", plan["docker_command"].as_str().unwrap_or_default());
    }
    Ok(0)
}

/// Entrypoint for `spawn doctor [--config <path>] [--root <dir>] [--json]`.
pub fn doctor_command(global_config: Option<&str>, rest: &[String]) -> Result<i32, String> {
    let parsed = parse_config_flag(rest, global_config, &["json"], &["root"])?;
    let json_mode = parsed.switches.contains("json");
    let config_path = parsed.config_path;

    let loaded_config = if let Some(path) = &config_path {
        Some((path.clone(), SpawnConfig::from_file(path)?))
    } else {
        None
    };

    let toolchain_paths: Vec<String> = loaded_config
        .as_ref()
        .and_then(|(_, cfg)| cfg.terminal.toolchain_paths.clone())
        .unwrap_or_default();

    let explicit_root = parsed.values.get("root").map(PathBuf::from);
    let data_dir = loaded_config
        .as_ref()
        .map(|(_, cfg)| cfg.instance.data_dir.clone());
    let workspaces_or_root = resolve_doctor_probe_root(
        explicit_root.as_deref(),
        loaded_config.as_ref().map(|(_, cfg)| cfg),
    );

    let kernel = probe_kernel();
    let git = probe_git(&toolchain_paths);
    let herdr = probe_herdr(&toolchain_paths);
    let bwrap = probe_simple_tool("bwrap", &toolchain_paths);
    let zstd = probe_simple_tool("zstd", &toolchain_paths);
    let container = spawn_host::detect_container();
    let bwrap_capability = probe_bwrap_capability(&bwrap);
    let host_inherit = probe_host_inherit();
    let archive_mask_tool = probe_archive_mask_tool();

    let root_fs = df_filesystem_info(Path::new("/")).ok();
    let data_fs_target = explicit_root
        .as_deref()
        .or(data_dir.as_deref())
        .unwrap_or_else(|| Path::new("/"));
    let data_fs = df_filesystem_info(data_fs_target).ok();
    let other_mounts = other_persistent_mounts();

    let mut warnings = collect_doctor_target_warnings(
        loaded_config.as_ref().map(|(_, cfg)| cfg),
        data_fs.as_ref(),
        data_fs_target,
        &other_mounts,
    );

    let storage_probes = probe_storage(&workspaces_or_root);
    let (shared_build_caches, shared_cache_warnings) = probe_shared_build_caches(
        explicit_root.as_deref(),
        loaded_config.as_ref().map(|(_, cfg)| cfg),
    );
    warnings.extend(shared_cache_warnings);
    let (model_endpoints, model_warnings) = probe_model_endpoints();
    warnings.extend(model_warnings);
    let (claude_config, claude_config_warnings) = probe_claude_config_with(
        &|name| std::env::var(name).ok(),
        loaded_config.as_ref().map(|(_, cfg)| cfg),
    );
    warnings.extend(claude_config_warnings);

    let report = json!({
        "ok": git["ok"].as_bool().unwrap_or(false) && kernel["ok"].as_bool().unwrap_or(false),
        "config_path": config_path,
        "probe_root": workspaces_or_root,
        "kernel": kernel,
        "tools": {
            "git": git,
            "herdr": herdr,
            "bwrap": bwrap,
            "zstd": zstd,
        },
        "container": {
            "in_container": container.in_container,
            "runtime": container.runtime,
            "signals": container.signals,
        },
        "bwrap_capability": bwrap_capability,
        "host_inherit": host_inherit,
        "archive_mask_tool": archive_mask_tool,
        "filesystems": {
            "root": root_fs,
            "target": data_fs,
            "other_mounts": other_mounts,
        },
        "storage_probes": storage_probes,
        "shared_build_caches": shared_build_caches,
        "model_endpoints": model_endpoints,
        "claude_config": claude_config,
        "warnings": warnings,
    });

    if json_mode {
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize doctor report: {error}"))?;
        println!("{rendered}");
    } else {
        print_doctor_human(&report);
    }
    Ok(0)
}

fn collect_doctor_target_warnings(
    loaded_config: Option<&SpawnConfig>,
    data_fs: Option<&FilesystemInfo>,
    data_fs_target: &Path,
    other_mounts: &[String],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(cfg) = loaded_config {
        collect_config_warnings(cfg, &mut warnings);
    } else if let Some(fs_info) = data_fs
        && fs_info.on_root_filesystem
        && !other_mounts.is_empty()
    {
        warnings.push(format!(
            "target path {} sits on the '/' filesystem while dedicated mounts exist ({})",
            data_fs_target.display(),
            other_mounts.join(", ")
        ));
    }
    warnings
}

fn probe_bwrap_capability(bwrap_tool: &Value) -> Value {
    let bwrap_prog = bwrap_tool["program"]
        .as_str()
        .filter(|_| bwrap_tool["installed"].as_bool().unwrap_or(false))
        .unwrap_or("bwrap");
    let cfg = spawn_host::ViewConfig::from_env(&|k| std::env::var(k).ok());
    match spawn_host::probe_bwrap(bwrap_prog, &cfg) {
        spawn_host::BwrapCapability::Ready => json!({ "status": "ready" }),
        spawn_host::BwrapCapability::BlockedInContainer(reason) => json!({
            "status": "blocked_in_container",
            "reason": reason,
        }),
        spawn_host::BwrapCapability::Unavailable(reason) => json!({
            "status": "unavailable",
            "reason": reason,
        }),
    }
}

fn probe_host_inherit() -> Value {
    let scan = scan_host_inherit();
    json!({
        "home": scan.home,
        "safe_dirs": scan.safe_dirs.iter().map(|(rel, p)| json!({ "rel": rel, "path": p })).collect::<Vec<_>>(),
        "safe_files": scan.safe_files.iter().map(|(rel, p)| json!({ "rel": rel, "path": p })).collect::<Vec<_>>(),
        "gitconfig": scan.gitconfig,
        "skipped": scan.skipped,
    })
}

fn probe_archive_mask_tool() -> Value {
    let mut candidates = vec![PathBuf::from(
        "/usr/local/lib/spawn/redact/emit-locations.py",
    )];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("../lib/spawn/redact/emit-locations.py"));
        for ancestor in dir.ancestors().take(5) {
            candidates.push(ancestor.join("scripts/redact/emit-locations.py"));
        }
    }
    let found = candidates.into_iter().find(|p| p.is_file());
    json!({
        "available": found.is_some(),
        "path": found,
    })
}

fn resolve_doctor_probe_root(explicit_root: Option<&Path>, cfg: Option<&SpawnConfig>) -> PathBuf {
    if let Some(root) = explicit_root {
        return nearest_existing_ancestor(root);
    }
    if let Some(cfg) = cfg {
        let ws = cfg.derived_paths().workspaces;
        if ws.is_dir() {
            return ws;
        }
        return nearest_existing_ancestor(&cfg.instance.data_dir);
    }
    if let Ok(ws_env) = std::env::var("SPAWN_WORKSPACE_ROOT")
        && !ws_env.is_empty()
    {
        return nearest_existing_ancestor(Path::new(&ws_env));
    }
    std::env::temp_dir()
}

fn find_tool(name: &str, toolchain_paths: &[String]) -> PathBuf {
    if let Some(found) = find_program_in_paths(name, toolchain_paths) {
        return found;
    }
    PathBuf::from(name)
}

fn run_version_cmd(program: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{}: {error}", program.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!("exit {}: {detail}", output.status));
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

fn probe_kernel() -> Value {
    match fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(text) => {
            let release = text.trim().to_string();
            json!({
                "ok": !release.is_empty(),
                "release": release,
            })
        }
        Err(error) => json!({
            "ok": false,
            "error": format!("read /proc/sys/kernel/osrelease: {error}"),
        }),
    }
}

fn parse_git_version(answer: &str) -> Option<(u32, u32)> {
    let version = answer.trim().strip_prefix("git version ")?;
    let mut parts = version.split(['.', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn probe_git(toolchain_paths: &[String]) -> Value {
    let program = find_tool("git", toolchain_paths);
    match run_version_cmd(&program, &["--version"]) {
        Ok(raw) => {
            let parsed = parse_git_version(&raw);
            let ok = parsed.is_some_and(|v| v >= MIN_GIT);
            let reason = if ok {
                None
            } else {
                Some(format!(
                    "requires git {}.{} or later (for --attr-source)",
                    MIN_GIT.0, MIN_GIT.1
                ))
            };
            json!({
                "installed": true,
                "ok": ok,
                "program": program,
                "version": raw,
                "min_required": format!("{}.{}", MIN_GIT.0, MIN_GIT.1),
                "reason": reason,
            })
        }
        Err(error) => json!({
            "installed": false,
            "ok": false,
            "program": program,
            "error": error,
        }),
    }
}

fn probe_herdr(toolchain_paths: &[String]) -> Value {
    let program = find_tool("herdr", toolchain_paths);
    match run_version_cmd(&program, &["--version"]) {
        Ok(raw) => json!({
            "installed": true,
            "ok": true,
            "program": program,
            "version": raw,
            "expected_protocol": spawn_host::herdr::PROTOCOL,
        }),
        Err(error) => json!({
            "installed": false,
            "ok": false,
            "program": program,
            "expected_protocol": spawn_host::herdr::PROTOCOL,
            "error": error,
        }),
    }
}

fn probe_simple_tool(name: &str, toolchain_paths: &[String]) -> Value {
    let program = find_tool(name, toolchain_paths);
    match run_version_cmd(&program, &["--version"]) {
        Ok(raw) => json!({
            "installed": true,
            "ok": true,
            "program": program,
            "version": raw,
        }),
        Err(error) => json!({
            "installed": false,
            "ok": false,
            "program": program,
            "error": error,
        }),
    }
}

fn probe_storage(probe_dir: &Path) -> Value {
    if !probe_dir.is_dir() {
        return json!({
            "probed": false,
            "path": probe_dir,
            "reason": "probe directory does not exist",
        });
    }
    if !path_writable(probe_dir) {
        return json!({
            "probed": false,
            "path": probe_dir,
            "reason": "probe directory is not writable by current user",
        });
    }
    let cow = spawn_host::cow_clone_supported(probe_dir);
    #[cfg(target_os = "linux")]
    let overlay = spawn_host::overlay_supported_in(probe_dir);
    #[cfg(not(target_os = "linux"))]
    let overlay: Result<(), String> = Err("overlayfs requires Linux".to_string());

    json!({
        "probed": true,
        "path": probe_dir,
        "cow_clone_supported": cow,
        "overlay_supported": overlay.is_ok(),
        "overlay_error": overlay.err(),
    })
}

fn print_doctor_shared_build_caches(caches: &Value) {
    let status = caches["status"].as_str().unwrap_or("disabled");
    let vars = caches["managed_env_vars"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    if caches["enabled"].as_bool().unwrap_or(false) {
        println!(
            "shared_build_caches: {status} (path={}, exists={}, writable={}, vars={vars})",
            caches["path"].as_str().unwrap_or("<none>"),
            caches["exists"].as_bool().unwrap_or(false),
            caches["writable"].as_bool().unwrap_or(false),
        );
    } else if status == "invalid" {
        println!(
            "shared_build_caches: invalid ({}, vars={vars})",
            caches["error"].as_str().unwrap_or("invalid configuration"),
        );
    } else {
        println!("shared_build_caches: disabled (vars={vars})");
    }
}

fn print_doctor_model_endpoints(endpoints: &Value) {
    println!(
        "model_endpoints: preflight={}",
        endpoints["preflight_mode"].as_str().unwrap_or("warn")
    );
    for slot in ["model_relay", "judge"] {
        let entry = &endpoints[slot];
        let status = entry["status"].as_str().unwrap_or("not_configured");
        match status {
            "not_configured" => println!("  {slot}: not_configured"),
            "ok" => println!(
                "  {slot}: ok (target={}, latency_ms={})",
                entry["target"].as_str().unwrap_or("unknown"),
                entry["latency_ms"]
                    .as_u64()
                    .map_or_else(|| "unknown".to_string(), |ms| ms.to_string()),
            ),
            _ => {
                println!(
                    "  {slot}: {status} (target={}, latency_ms={}, detail={})",
                    entry["target"].as_str().unwrap_or("<invalid-endpoint>"),
                    entry["latency_ms"]
                        .as_u64()
                        .map_or_else(|| "unknown".to_string(), |ms| ms.to_string()),
                    entry["detail"].as_str().unwrap_or("unreachable"),
                );
                if let Some(hint) = entry["remediation"].as_str() {
                    println!("    remediation: {hint}");
                }
            }
        }
    }
    let slots = &endpoints["model_slots"];
    println!(
        "  model_slots: default={}, cheap={}, small={}",
        slots["default"].as_str().unwrap_or("<unset>"),
        slots["cheap"].as_str().unwrap_or("<unset>"),
        slots["small"].as_str().unwrap_or("<unset>"),
    );
}

fn sanitize_shared_cache_error(raw: &str) -> String {
    let redacted = spawn_core::redact::redact(raw);
    let lower = redacted.to_ascii_lowercase();
    if redacted.contains("://")
        || redacted.contains('@')
        || lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("secret=")
        || lower.contains("password=")
    {
        return REDACTED.to_string();
    }
    redacted
}

fn probe_shared_build_caches(
    explicit_root: Option<&Path>,
    loaded_config: Option<&SpawnConfig>,
) -> (Value, Vec<String>) {
    let env_ws = std::env::var("SPAWN_WORKSPACE_ROOT")
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from);
    let derived_ws = loaded_config.map(|cfg| cfg.derived_paths().workspaces);
    let ws_root = explicit_root
        .map(Path::to_path_buf)
        .or(derived_ws)
        .or(env_ws);
    probe_shared_build_caches_with(ws_root.as_deref(), &|name| std::env::var(name).ok())
}

const DOCTOR_SHARED_BUILD_CACHE_VARS: &[&str] = &[
    "ZIG_GLOBAL_CACHE_DIR",
    "CCACHE_DIR",
    "UV_CACHE_DIR",
    "SCCACHE_DIR",
    "PIP_CACHE_DIR",
    "NPM_CONFIG_CACHE",
    "GOMODCACHE",
    "GOCACHE",
];

const DOCTOR_SHARED_CACHE_SENSITIVE_SUBTREES: &[&str] = &[
    "/etc",
    "/proc",
    "/sys",
    "/dev",
    "/boot",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/usr",
    "/run/keep",
    "/run/keep-test",
    "/var/run/keep",
    "/var/run/keep-test",
];

fn normalize_doctor_cache_path(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::CurDir) {
            normal.push(component.as_os_str());
        }
    }
    if normal.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normal
    }
}

fn reject_doctor_cache_path(path: &Path, var_name: &str, home: Option<&str>) -> Result<(), String> {
    let text = path.to_string_lossy();
    if BARE_SYSTEM_DIRS.contains(&text.as_ref()) {
        return Err(format!(
            "{var_name} cannot target bare system directory {text:?}"
        ));
    }
    for prefix in DOCTOR_SHARED_CACHE_SENSITIVE_SUBTREES {
        let prefix_path = Path::new(prefix);
        if path == prefix_path || path.starts_with(prefix_path) {
            return Err(format!(
                "{var_name} cannot target sensitive system directory {text:?}"
            ));
        }
    }
    if let Some(home_raw) = home.map(str::trim).filter(|raw| !raw.is_empty()) {
        let home_path = Path::new(home_raw);
        if home_path.is_absolute() {
            let home_norm = normalize_doctor_cache_path(home_path);
            if path == home_norm
                || fs::canonicalize(&home_norm)
                    .ok()
                    .is_some_and(|canon| path == canon)
            {
                return Err(format!(
                    "{var_name} cannot target $HOME directly ({text:?}); use a subdirectory"
                ));
            }
        }
    }
    Ok(())
}

fn validate_doctor_cache_dir(
    raw: &str,
    var_name: &str,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return Err(format!(
            "{var_name} must be a non-empty clean path, not {raw:?}"
        ));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err(format!(
            "{var_name} must be an absolute path, not {trimmed:?}"
        ));
    }
    if has_parent_dir(path) {
        return Err(format!(
            "{var_name} must not contain '..' segments, got {trimmed:?}"
        ));
    }
    let normalized = normalize_doctor_cache_path(path);
    reject_doctor_cache_path(&normalized, var_name, home)?;
    if let Ok(canonical) = fs::canonicalize(&normalized) {
        reject_doctor_cache_path(&canonical, var_name, home)?;
    }
    Ok(normalized)
}

fn resolve_doctor_shared_build_cache(
    lookup: &dyn Fn(&str) -> Option<String>,
    workspace_root: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let raw_toggle = lookup("SPAWN_SHARED_BUILD_CACHES");
    let toggle = match raw_toggle
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        None => None,
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => {
                return Err(format!(
                    "SPAWN_SHARED_BUILD_CACHES={value:?} must be 1|true|yes|on or 0|false|no|off"
                ));
            }
        },
    };
    if toggle == Some(false) {
        return Ok(None);
    }
    let home = lookup("HOME");
    if let Some(dir) = lookup("SPAWN_SHARED_BUILD_CACHE_DIR")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return validate_doctor_cache_dir(&dir, "SPAWN_SHARED_BUILD_CACHE_DIR", home.as_deref())
            .map(Some);
    }
    if toggle == Some(true) {
        let Some(ws) = workspace_root else {
            return Err(
                "SPAWN_SHARED_BUILD_CACHES=1 requires a workspace root (set --root / SPAWN_WORKSPACE_ROOT or SPAWN_SHARED_BUILD_CACHE_DIR)"
                    .to_string(),
            );
        };
        if !ws.is_absolute()
            || has_parent_dir(ws)
            || normalize_doctor_cache_path(ws) == Path::new("/")
        {
            return Err(format!(
                "SPAWN_SHARED_BUILD_CACHES requires a valid non-root workspace path, not {:?}",
                ws.display()
            ));
        }
        let default_dir = normalize_doctor_cache_path(ws).join(".caches");
        return validate_doctor_cache_dir(
            &default_dir.display().to_string(),
            "SPAWN_SHARED_BUILD_CACHES",
            home.as_deref(),
        )
        .map(Some);
    }
    Ok(None)
}

fn probe_shared_build_caches_with(
    workspace_root: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> (Value, Vec<String>) {
    let managed_env_vars = DOCTOR_SHARED_BUILD_CACHE_VARS;
    match resolve_doctor_shared_build_cache(lookup, workspace_root) {
        Ok(None) => (
            json!({
                "enabled": false,
                "status": "disabled",
                "path": Value::Null,
                "exists": false,
                "writable": false,
                "managed_env_vars": managed_env_vars,
                "error": Value::Null,
            }),
            Vec::new(),
        ),
        Ok(Some(path)) => {
            let exists = fs::symlink_metadata(&path).is_ok();
            let is_dir = path.is_dir();
            let writable = if is_dir {
                path_writable(&path)
            } else if exists {
                false
            } else {
                let anchor = nearest_existing_ancestor(&path);
                anchor.is_dir() && path_writable(&anchor)
            };
            let mut warnings = Vec::new();
            let status = if exists && !is_dir {
                warnings.push(format!(
                    "shared build cache path {} exists and is not a directory",
                    path.display()
                ));
                "not_a_directory"
            } else if writable {
                "ok"
            } else {
                warnings.push(format!(
                    "shared build cache path {} is not writable by current user",
                    path.display()
                ));
                "not_writable"
            };
            (
                json!({
                    "enabled": true,
                    "status": status,
                    "path": path.display().to_string(),
                    "exists": exists,
                    "writable": writable,
                    "managed_env_vars": managed_env_vars,
                    "error": Value::Null,
                }),
                warnings,
            )
        }
        Err(error) => {
            let sanitized = sanitize_shared_cache_error(&error);
            let warnings = vec![format!(
                "shared build caches configuration invalid: {sanitized}"
            )];
            (
                json!({
                    "enabled": false,
                    "status": "invalid",
                    "path": Value::Null,
                    "exists": false,
                    "writable": false,
                    "managed_env_vars": managed_env_vars,
                    "error": sanitized,
                }),
                warnings,
            )
        }
    }
}

fn print_doctor_human(report: &Value) {
    println!(
        "kernel: {}",
        report["kernel"]["release"].as_str().unwrap_or("unknown")
    );
    for tool in ["git", "herdr", "bwrap", "zstd"] {
        let entry = &report["tools"][tool];
        if entry["installed"].as_bool().unwrap_or(false) {
            let status = if entry["ok"].as_bool().unwrap_or(false) {
                "ok"
            } else {
                "warn"
            };
            println!(
                "{tool}: {status} ({})",
                entry["version"].as_str().unwrap_or("")
            );
        } else {
            println!(
                "{tool}: missing ({})",
                entry["error"].as_str().unwrap_or("")
            );
        }
    }
    let in_ctr = report["container"]["in_container"]
        .as_bool()
        .unwrap_or(false);
    let ctr_rt = report["container"]["runtime"].as_str().unwrap_or("none");
    println!("container: {in_ctr} (runtime={ctr_rt})");
    println!(
        "bwrap_capability: {}",
        report["bwrap_capability"]["status"]
            .as_str()
            .unwrap_or("unknown")
    );
    let inherit_dirs = report["host_inherit"]["safe_dirs"]
        .as_array()
        .map_or(0, Vec::len);
    let has_gitcfg = !report["host_inherit"]["gitconfig"].is_null();
    println!("host_inherit: safe_dirs={inherit_dirs}, gitconfig_safe={has_gitcfg}");
    println!(
        "archive_mask_tool: available={}",
        report["archive_mask_tool"]["available"]
            .as_bool()
            .unwrap_or(false)
    );
    println!(
        "storage ({}): cow={}, overlay={}",
        report["storage_probes"]["path"]
            .as_str()
            .unwrap_or("unknown"),
        report["storage_probes"]["cow_clone_supported"],
        report["storage_probes"]["overlay_supported"],
    );
    print_doctor_shared_build_caches(&report["shared_build_caches"]);
    print_doctor_model_endpoints(&report["model_endpoints"]);
    print_doctor_claude_config(&report["claude_config"]);
    if let Some(warnings) = report["warnings"].as_array() {
        for warning in warnings {
            if let Some(text) = warning.as_str() {
                eprintln!("warning: {text}");
            }
        }
    }
}

fn is_disabled_endpoint_value(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("stub")
}

fn sanitize_model_slot_name(raw: Option<String>) -> Option<String> {
    let trimmed = raw
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let lower = trimmed.to_ascii_lowercase();
    if trimmed.contains("://")
        || trimmed.contains('@')
        || lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("secret=")
        || lower.contains("password=")
    {
        return Some(REDACTED.to_string());
    }
    Some(trimmed)
}

fn probe_single_model_endpoint(
    slot: &str,
    env_var: &str,
    raw_url: Option<String>,
    probe: &dyn Fn(&str, std::time::Duration) -> spawn_jev::EndpointHealth,
) -> (Value, Option<String>) {
    let Some(url) = raw_url
        .map(|s| s.trim().to_string())
        .filter(|s| !is_disabled_endpoint_value(s))
    else {
        return (
            json!({
                "env_var": env_var,
                "status": "not_configured",
                "target": null,
                "http_status": null,
                "latency_ms": null,
                "detail": null,
                "remediation": null,
            }),
            None,
        );
    };

    let health = probe(&url, spawn_jev::DEFAULT_PROBE_TIMEOUT);
    if health.status.is_ok() {
        return (
            json!({
                "env_var": env_var,
                "status": "ok",
                "target": health.target,
                "http_status": health.http_status,
                "latency_ms": health.latency_ms,
                "detail": null,
                "remediation": null,
            }),
            None,
        );
    }

    let detail = health
        .detail
        .clone()
        .unwrap_or_else(|| "endpoint unreachable".to_string());
    let remediation = match slot {
        "model_relay" => format!(
            "Start spawn-model-relay on {} or set {env_var}=off if engines do not require the relay; \
             if SPAWN_CHEAP_MODEL uses this relay, startup preflight falls back to the direct/default path unless SPAWN_MODEL_PREFLIGHT=strict.",
            health.target
        ),
        _ => format!(
            "Start spawn-jev-relay on {} or set {env_var}=off to run the Observer with deterministic rules only.",
            health.target
        ),
    };
    let warning = format!(
        "model endpoint {slot} ({env_var}) at {} is unreachable: {detail}",
        health.target
    );
    (
        json!({
            "env_var": env_var,
            "status": "unreachable",
            "target": health.target,
            "http_status": health.http_status,
            "latency_ms": health.latency_ms,
            "detail": detail,
            "remediation": remediation,
        }),
        Some(warning),
    )
}

/// Which Claude Code directory the engines run with (issue #79), as far
/// as it is no secret: where it comes from (`SPAWN_CLAUDE_CONFIG_DIR`,
/// `[engines.claude].config_dir`, or the user's own `~/.claude`), its
/// path, and whether a `settings.json` is there. The file is never read:
/// it holds the gateway's token.
fn probe_claude_config_with(
    lookup: &dyn Fn(&str) -> Option<String>,
    cfg: Option<&SpawnConfig>,
) -> (Value, Vec<String>) {
    let mut warnings = Vec::new();
    let from_toml = cfg
        .and_then(|cfg| cfg.engines.claude.as_ref())
        .filter(|claude| claude.enabled)
        .and_then(|claude| claude.config_dir.as_ref());
    // The rule `spawn loop` applies (`runtime::claude_config_dir`): empty
    // reads as unset, anything but an absolute path stops the loop. This
    // file is also compiled alone by `tests/config_toml.rs`, so the rule is
    // restated here rather than called.
    let from_env = lookup("SPAWN_CLAUDE_CONFIG_DIR").filter(|dir| !dir.trim().is_empty());
    let (source, dir) = match from_env {
        Some(dir) if dir.starts_with('/') => {
            let trimmed = dir.trim_end_matches('/');
            let dir = if trimmed.is_empty() { "/" } else { trimmed };
            ("SPAWN_CLAUDE_CONFIG_DIR", Some(PathBuf::from(dir)))
        }
        Some(dir) => {
            warnings.push(format!(
                "SPAWN_CLAUDE_CONFIG_DIR must be an absolute path, not {dir:?}: `spawn loop` \
                 refuses to start"
            ));
            ("SPAWN_CLAUDE_CONFIG_DIR", None)
        }
        None => match from_toml {
            Some(dir) => match expand_tilde(dir, lookup) {
                Ok(dir) => ("[engines.claude].config_dir", Some(dir)),
                Err(error) => {
                    warnings.push(format!("[engines.claude].config_dir: {error}"));
                    ("[engines.claude].config_dir", None)
                }
            },
            None => (
                "default",
                lookup("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".claude")),
            ),
        },
    };
    let isolated = source != "default" && dir.is_some();
    let settings_json = dir
        .as_ref()
        .is_some_and(|dir| dir.join("settings.json").is_file());
    if isolated && !settings_json {
        warnings.push(format!(
            "engine Claude Code directory {} has no settings.json: engines there get no \
             gateway token",
            dir.as_deref().unwrap_or(Path::new("")).display()
        ));
    }
    (
        json!({
            "source": source,
            "path": dir,
            "isolated": isolated,
            "settings_json_present": settings_json,
        }),
        warnings,
    )
}

fn print_doctor_claude_config(report: &Value) {
    println!(
        "claude_config: source={}, path={}, isolated={}, settings_json={}",
        report["source"].as_str().unwrap_or("unknown"),
        report["path"].as_str().unwrap_or("unknown"),
        report["isolated"].as_bool().unwrap_or(false),
        if report["settings_json_present"].as_bool().unwrap_or(false) {
            "present"
        } else {
            "missing"
        },
    );
}

fn probe_model_endpoints() -> (Value, Vec<String>) {
    probe_model_endpoints_with(
        &|name| std::env::var(name).ok(),
        &spawn_jev::probe_http_endpoint,
    )
}

fn probe_model_endpoints_with(
    lookup: &dyn Fn(&str) -> Option<String>,
    probe: &dyn Fn(&str, std::time::Duration) -> spawn_jev::EndpointHealth,
) -> (Value, Vec<String>) {
    let raw_relay = lookup("SPAWN_MODEL_RELAY");
    let relay_configured = raw_relay
        .as_deref()
        .is_some_and(|raw| !is_disabled_endpoint_value(raw));
    let (relay_report, relay_warning) =
        probe_single_model_endpoint("model_relay", "SPAWN_MODEL_RELAY", raw_relay, probe);

    let raw_judge = lookup("SPAWN_JUDGE_URL");
    let (judge_report, judge_warning) =
        probe_single_model_endpoint("judge", "SPAWN_JUDGE_URL", raw_judge, probe);

    let mut warnings = Vec::new();
    if let Some(w) = relay_warning {
        warnings.push(w);
    }
    if let Some(w) = judge_warning {
        warnings.push(w);
    }

    let preflight_mode = lookup("SPAWN_MODEL_PREFLIGHT")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "warn".to_string());

    let default_slot = sanitize_model_slot_name(lookup("SPAWN_DEFAULT_MODEL"));
    let cheap_slot = sanitize_model_slot_name(lookup("SPAWN_CHEAP_MODEL"))
        .or_else(|| relay_configured.then(|| "deepseek-flash".to_string()));
    let small_slot = sanitize_model_slot_name(lookup("SPAWN_SMALL_MODEL"))
        .or_else(|| relay_configured.then(|| "deepseek-flash".to_string()));
    let judge_slot = sanitize_model_slot_name(lookup("SPAWN_JUDGE_MODEL"));

    let value = json!({
        "preflight_mode": preflight_mode,
        "model_relay": relay_report,
        "judge": judge_report,
        // Every slot Spawn's inline `--settings` names through the relay
        // (issue #79), so `~/.claude/settings.json` cannot change them.
        "pinned_slots": if relay_configured {
            spawn_core::bridge::MODEL_SLOTS.to_vec()
        } else {
            Vec::new()
        },
        "model_slots": {
            "default": default_slot,
            "cheap": cheap_slot,
            "small": small_slot,
            "judge": judge_slot,
        },
    });
    (value, warnings)
}

#[cfg(test)]
mod tests {
    use super::probe_model_endpoints_with;

    /// Issue #79: doctor shows which Claude Code directory the engines run
    /// with, the env first, then `spawn.toml`, else the user's own, and
    /// never reads what `settings.json` holds.
    #[test]
    fn doctor_reports_the_engines_claude_config_dir_without_its_contents() {
        use super::{SpawnConfig, probe_claude_config_with};

        let base = std::env::temp_dir().join(format!("spawn-doctor-claude-{}", std::process::id()));
        let fleet = base.join("fleet-claude");
        std::fs::create_dir_all(&fleet).expect("mkdir fleet");
        std::fs::write(
            fleet.join("settings.json"),
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"DoctorTok99"}}"#,
        )
        .expect("write settings");
        let fleet_str = fleet.display().to_string();
        let home = base.join("home").display().to_string();

        let lookup = |key: &str| match key {
            "SPAWN_CLAUDE_CONFIG_DIR" => Some(format!("{fleet_str}/")),
            "HOME" => Some(home.clone()),
            _ => None,
        };
        let (env, warnings) = probe_claude_config_with(&lookup, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(env["source"], "SPAWN_CLAUDE_CONFIG_DIR");
        assert_eq!(env["path"], fleet_str.as_str());
        assert_eq!(env["isolated"], true);
        assert_eq!(env["settings_json_present"], true);
        assert!(!env.to_string().contains("DoctorTok99"), "{env}");

        let toml = concat!(
            "schema_version = 1\n\n[instance]\ndata_dir = \"/srv/spawn\"\n\n",
            "[engines.claude]\nconfig_dir = \"~/fleet-claude\"\n"
        );
        let cfg = SpawnConfig::from_toml_str(toml).expect("config parses");
        let home_only = |key: &str| (key == "HOME").then(|| base.display().to_string());
        let (toml_report, warnings) = probe_claude_config_with(&home_only, Some(&cfg));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(toml_report["source"], "[engines.claude].config_dir");
        assert_eq!(toml_report["path"], fleet_str.as_str());
        assert_eq!(toml_report["isolated"], true);

        let (default, warnings) =
            probe_claude_config_with(&|key| (key == "HOME").then(|| home.clone()), None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(default["source"], "default");
        assert_eq!(default["path"], format!("{home}/.claude").as_str());
        assert_eq!(default["isolated"], false);

        let relative = |key: &str| (key == "SPAWN_CLAUDE_CONFIG_DIR").then(|| "rel".to_string());
        let (refused, warnings) = probe_claude_config_with(&relative, None);
        assert_eq!(refused["isolated"], false);
        assert!(refused["path"].is_null());
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        let empty = base.join("empty-claude");
        std::fs::create_dir_all(&empty).expect("mkdir empty");
        let empty_str = empty.display().to_string();
        let (no_settings, warnings) = probe_claude_config_with(
            &|key| (key == "SPAWN_CLAUDE_CONFIG_DIR").then(|| empty_str.clone()),
            None,
        );
        assert_eq!(no_settings["settings_json_present"], false);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn probe_model_endpoints_reports_not_configured_ok_and_redacted_unreachable() {
        let ok_probe = |url: &str, _timeout: std::time::Duration| spawn_jev::EndpointHealth {
            status: spawn_jev::EndpointStatus::Ok,
            target: spawn_jev::sanitize_endpoint_target(url),
            http_status: Some(200),
            latency_ms: 3,
            detail: None,
        };
        let (unconfigured, warnings) = probe_model_endpoints_with(&|_| None, &ok_probe);
        assert!(warnings.is_empty());
        assert_eq!(unconfigured["model_relay"]["status"], "not_configured");
        assert_eq!(unconfigured["judge"]["status"], "not_configured");

        let lookup_ok = |key: &str| match key {
            "SPAWN_MODEL_RELAY" => Some("http://127.0.0.1:8791".to_string()),
            "SPAWN_JUDGE_URL" => Some("http://127.0.0.1:8790".to_string()),
            "SPAWN_DEFAULT_MODEL" => Some("claude-opus-5-5".to_string()),
            "SPAWN_CHEAP_MODEL" => Some("deepseek-v4-pro".to_string()),
            _ => None,
        };
        let (healthy, warnings) = probe_model_endpoints_with(&lookup_ok, &ok_probe);
        assert!(warnings.is_empty());
        assert_eq!(healthy["model_relay"]["status"], "ok");
        assert_eq!(healthy["model_relay"]["latency_ms"], 3);
        assert_eq!(healthy["judge"]["status"], "ok");
        assert_eq!(healthy["model_slots"]["default"], "claude-opus-5-5");
        assert_eq!(healthy["model_slots"]["cheap"], "deepseek-v4-pro");
        assert_eq!(healthy["model_slots"]["small"], "deepseek-flash");
        assert_eq!(
            healthy["pinned_slots"],
            serde_json::json!(spawn_core::bridge::MODEL_SLOTS)
        );
        assert_eq!(unconfigured["pinned_slots"], serde_json::json!([]));

        let down_probe = |url: &str, _timeout: std::time::Duration| spawn_jev::EndpointHealth {
            status: spawn_jev::EndpointStatus::Unreachable,
            target: spawn_jev::sanitize_endpoint_target(url),
            http_status: None,
            latency_ms: 7,
            detail: Some(format!(
                "connect {}: connection refused",
                spawn_jev::sanitize_endpoint_target(url)
            )),
        };
        let lookup_down = |key: &str| match key {
            "SPAWN_MODEL_RELAY" => {
                Some("http://u:RelayPass55@127.0.0.1:8791?api_key=RelayTok66".to_string())
            }
            "SPAWN_JUDGE_URL" => {
                Some("http://j:JudgePass77@127.0.0.1:8790?token=JudgeTok88".to_string())
            }
            _ => None,
        };
        let (unreachable, warnings) = probe_model_endpoints_with(&lookup_down, &down_probe);
        assert_eq!(warnings.len(), 2);
        assert_eq!(unreachable["model_relay"]["status"], "unreachable");
        assert_eq!(unreachable["model_relay"]["target"], "127.0.0.1:8791");
        assert!(unreachable["model_relay"]["remediation"].is_string());
        assert_eq!(unreachable["judge"]["status"], "unreachable");
        let combined = format!("{unreachable}\n{}", warnings.join("\n"));
        for secret in ["RelayPass55", "RelayTok66", "JudgePass77", "JudgeTok88"] {
            assert!(!combined.contains(secret), "secret leaked: {combined}");
        }
    }

    #[test]
    fn probe_shared_build_caches_reports_disabled_ok_invalid_and_redacts_secrets() {
        use super::probe_shared_build_caches_with;

        let base = std::env::temp_dir().join(format!("spawn-doctor-cache-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir ws");

        let (disabled, warnings) = probe_shared_build_caches_with(Some(&ws), &|_| None);
        assert!(warnings.is_empty());
        assert_eq!(disabled["enabled"], false);
        assert_eq!(disabled["status"], "disabled");
        assert!(disabled["path"].is_null());
        assert_eq!(disabled["writable"], false);
        let vars = disabled["managed_env_vars"].as_array().expect("vars array");
        assert_eq!(vars.len(), 8);
        assert!(vars.iter().any(|v| v == "ZIG_GLOBAL_CACHE_DIR"));
        assert!(vars.iter().any(|v| v == "GOCACHE"));

        // Opt-in via SPAWN_SHARED_BUILD_CACHES=1 must stay read-only and never mkdir `.caches`.
        let (enabled_default, warnings) = probe_shared_build_caches_with(Some(&ws), &|key| {
            (key == "SPAWN_SHARED_BUILD_CACHES").then(|| "1".to_string())
        });
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(enabled_default["enabled"], true);
        assert_eq!(enabled_default["status"], "ok");
        assert_eq!(
            enabled_default["path"],
            ws.join(".caches").display().to_string()
        );
        assert_eq!(enabled_default["exists"], false);
        assert_eq!(enabled_default["writable"], true);
        assert!(
            !ws.join(".caches").exists(),
            "spawn doctor must remain strictly read-only"
        );

        // Existing custom cache dir vs file collision.
        let custom_dir = base.join("custom");
        std::fs::create_dir_all(&custom_dir).expect("mkdir custom");
        let (custom_ok, warnings) = probe_shared_build_caches_with(Some(&ws), &|key| {
            (key == "SPAWN_SHARED_BUILD_CACHE_DIR").then(|| custom_dir.display().to_string())
        });
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(custom_ok["enabled"], true);
        assert_eq!(custom_ok["status"], "ok");
        assert_eq!(custom_ok["exists"], true);
        assert_eq!(custom_ok["writable"], true);

        let blocking_file = base.join("not-a-dir");
        std::fs::write(&blocking_file, "file\n").expect("write file");
        let (not_dir, warnings) = probe_shared_build_caches_with(Some(&ws), &|key| {
            (key == "SPAWN_SHARED_BUILD_CACHE_DIR").then(|| blocking_file.display().to_string())
        });
        assert_eq!(not_dir["enabled"], true);
        assert_eq!(not_dir["status"], "not_a_directory");
        assert_eq!(not_dir["exists"], true);
        assert_eq!(not_dir["writable"], false);
        assert_eq!(warnings.len(), 1);

        // Invalid paths and secret-bearing values never leak secrets.
        let (invalid, warnings) = probe_shared_build_caches_with(Some(&ws), &|key| match key {
            "SPAWN_SHARED_BUILD_CACHE_DIR" => {
                Some("http://user:CacheSecret99@127.0.0.1/cache?token=CacheTok00".to_string())
            }
            "SPAWN_DATABASE_URL" => Some("postgres://u:DbSecret11@127.0.0.1:5432/db".to_string()),
            _ => None,
        });
        assert_eq!(invalid["enabled"], false);
        assert_eq!(invalid["status"], "invalid");
        assert!(invalid["path"].is_null());
        assert_eq!(warnings.len(), 1);
        let serialized = format!("{invalid}\n{}", warnings.join("\n"));
        for secret in ["CacheSecret99", "CacheTok00", "DbSecret11"] {
            assert!(
                !serialized.contains(secret),
                "secret {secret} leaked: {serialized}"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }
}
