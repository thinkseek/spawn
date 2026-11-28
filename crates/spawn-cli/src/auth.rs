//! Unified 3-layer decoupled authentication and credential subsystem.
//!
//! Decouples three orthogonal concerns using strict Sum / Product ADTs:
//!
//! ```text
//! Engine (claude | codex | agy | opencode)
//!   != AuthProfile (relay | native_login | cloud_iam | local)
//!   != ModelTier (L1 | L2 | L3)
//! ```
//!
//! Security & ADT guarantees:
//! - Illegal combinations (`agy` under `relay`/`local`/`cloud_iam`, cross-cloud IAM fields)
//!   are unrepresentable in Rust types and rejected fail-closed at parse boundaries.
//! - Endpoint URLs strictly forbid `?` queries, `#` fragments, `;` path parameters,
//!   backslashes, and `@` userinfo (zero secret leakage in config, JSON, or errors).
//! - Validators never echo rejected credential arguments, URLs, or tokens back to callers.
//! - `native_login` directories are strictly contained beneath the controlled `profiles_root`
//!   with zero symlink traversal and zero `chmod` on pre-existing directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};

use crate::config;
use crate::config_file::{SecretRef, SecretString};
use crate::toml_parser;

/// CLI usage contract for `spawn auth`.
pub(crate) const USAGE: &str = "\
usage: spawn auth list [--config <path>] [--json]
       spawn auth add-relay <name> --url <url> (--key-env <VAR> | --key-file <PATH>) [--engines claude,codex] [--tiers L1,L2,L3] [--config <path>] [--json]
       spawn auth login <agy|claude|codex> [--profile <name>] [--config <path>]
       spawn auth check [--config <path>] [--json]";

const PLAINTEXT_ERR: &str = "plaintext API keys/tokens are forbidden in auth configuration and CLI flags; \
     use SecretRef { from_env = \"VAR\" } or { from_file = \"/abs/path\" } \
     (CLI: --key-env <VAR> or --key-file <PATH>)";

const FORBIDDEN_PLAINTEXT_FIELDS: &[&str] = &[
    "key",
    "raw_key",
    "token",
    "access_token",
    "refresh_token",
    "bearer_token",
    "secret",
    "secret_key",
    "access_key",
    "client_secret",
    "password",
    "credentials_json",
];

const FORBIDDEN_PLAINTEXT_FLAGS: &[&str] = &[
    "key",
    "api-key",
    "apikey",
    "token",
    "secret",
    "password",
    "raw-key",
    "auth-token",
];

const FORBIDDEN_BARE_ROOTS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/opt",
    "/private",
    "/private/tmp",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/var",
    "/var/tmp",
];

const FORBIDDEN_HOME_SUBDIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".config",
    ".gnupg",
    ".kube",
    ".azure",
    ".claude",
    ".gemini",
    ".codex",
    ".antigravity",
    ".agy",
];

/// Supported coding-agent execution engines (layer 1: Engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Engine {
    Claude,
    Codex,
    Agy,
    OpenCode,
}

impl Engine {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
            Self::OpenCode => "opencode",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Closed 3-variant engine subset valid for `relay` and `local` endpoints (`Agy` unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RelayEngine {
    Claude,
    Codex,
    OpenCode,
}

impl RelayEngine {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    pub(crate) const fn into_engine(self) -> Engine {
        match self {
            Self::Claude => Engine::Claude,
            Self::Codex => Engine::Codex,
            Self::OpenCode => Engine::OpenCode,
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "agy" | "antigravity" => Err(
                "engine `agy` only supports `native_login` (not allowed in `relay`, `local`, or `cloud_iam`)"
                    .to_string(),
            ),
            other => Err(format!(
                "unknown relay/local engine `{other}`: expected claude, codex, or opencode (`agy` only supports `native_login`)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for RelayEngine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

/// Closed 2-variant engine subset valid for Azure Cloud IAM (`Codex | OpenCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AzureEngine {
    Codex,
    OpenCode,
}

impl AzureEngine {
    pub(crate) const fn into_engine(self) -> Engine {
        match self {
            Self::Codex => Engine::Codex,
            Self::OpenCode => Engine::OpenCode,
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "agy" | "antigravity" => {
                Err("engine `agy` only supports `native_login` (not `cloud_iam`)".to_string())
            }
            other => Err(format!(
                "cloud_iam provider `azure` only permits [codex,opencode], got `{other}`"
            )),
        }
    }
}

/// Logical capability/cost model tier (layer 3: ModelTier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum ModelTier {
    L1,
    L2,
    L3,
}

impl ModelTier {
    pub(crate) const ALL: [Self; 3] = [Self::L1, Self::L2, Self::L3];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "L1" => Ok(Self::L1),
            "L2" => Ok(Self::L2),
            "L3" => Ok(Self::L3),
            other => Err(format!(
                "unknown model tier `{other}`: expected one of L1, L2, L3"
            )),
        }
    }
}

/// Non-destructive readiness state of an auth profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileReadiness {
    Ready,
    Missing,
    ReauthRequired,
}

impl ProfileReadiness {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::ReauthRequired => "reauth_required",
        }
    }
}

/// Closed Sum Type for native OAuth / QR login engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeLoginProvider {
    Agy,
    ClaudeOauth,
    CodexOauth,
}

impl NativeLoginProvider {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Agy => "agy",
            Self::ClaudeOauth => "claude_oauth",
            Self::CodexOauth => "codex_oauth",
        }
    }

    pub(crate) const fn engine(self) -> Engine {
        match self {
            Self::Agy => Engine::Agy,
            Self::ClaudeOauth => Engine::Claude,
            Self::CodexOauth => Engine::Codex,
        }
    }

    pub(crate) const fn default_profile_name(self) -> &'static str {
        match self {
            Self::Agy => "agy-native",
            Self::ClaudeOauth => "claude-native",
            Self::CodexOauth => "codex-native",
        }
    }

    pub(crate) fn parse_provider(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "agy" | "antigravity" | "agy_oauth" => Ok(Self::Agy),
            "claude" | "claude_oauth" => Ok(Self::ClaudeOauth),
            "codex" | "codex_oauth" => Ok(Self::CodexOauth),
            other => Err(format!(
                "native_login supports `agy`, `claude` (`claude_oauth`), or `codex` (`codex_oauth`), got `{other}`"
            )),
        }
    }

    fn credential_candidates(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeOauth => &[
                ".credentials.json",
                "credentials.json",
                "oauth_credentials.json",
                ".claude/.credentials.json",
            ],
            Self::CodexOauth => &["auth.json", "credentials.json", ".codex/auth.json"],
            Self::Agy => &[
                "oauth_creds.json",
                "credentials.json",
                "session.json",
                ".gemini/oauth_creds.json",
                ".agy/session.json",
            ],
        }
    }
}

/// Passwordless local OpenAI-compatible inference backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalProvider {
    Ollama,
    Vllm,
    Sglang,
}

impl LocalProvider {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
        }
    }

    pub(crate) const fn default_base_url(self) -> &'static str {
        match self {
            Self::Ollama => "http://127.0.0.1:11434/v1",
            Self::Vllm => "http://127.0.0.1:8000/v1",
            Self::Sglang => "http://127.0.0.1:30000/v1",
        }
    }
}

/// Relay / API gateway profile backed strictly by a non-plaintext [`SecretRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayProfile {
    pub(crate) base_url: String,
    pub(crate) api_key: SecretRef,
    pub(crate) engines: BTreeSet<RelayEngine>,
}

/// Isolated native OAuth / QR login profile with strict containment under `profiles_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLoginProfile {
    pub(crate) provider: NativeLoginProvider,
    pub(crate) profile_dir: PathBuf,
}

/// Strict 3-variant Sum Type for passwordless cloud IAM providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudIamProfile {
    Vertex {
        region: String,
        project: Option<String>,
        service_account: Option<String>,
        endpoint: Option<String>,
    },
    Bedrock {
        region: String,
        aws_profile: Option<String>,
        endpoint: Option<String>,
    },
    Azure {
        region: String,
        resource: Option<String>,
        tenant_id: Option<String>,
        endpoint: Option<String>,
        engines: BTreeSet<AzureEngine>,
    },
}

impl CloudIamProfile {
    pub(crate) const fn provider_str(&self) -> &'static str {
        match self {
            Self::Vertex { .. } => "vertex",
            Self::Bedrock { .. } => "bedrock",
            Self::Azure { .. } => "azure",
        }
    }

    pub(crate) const fn region(&self) -> &str {
        match self {
            Self::Vertex { region, .. }
            | Self::Bedrock { region, .. }
            | Self::Azure { region, .. } => region.as_str(),
        }
    }

    pub(crate) fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Vertex { endpoint, .. }
            | Self::Bedrock { endpoint, .. }
            | Self::Azure { endpoint, .. } => endpoint.as_deref(),
        }
    }

    pub(crate) fn engines(&self) -> Vec<Engine> {
        match self {
            Self::Vertex { .. } | Self::Bedrock { .. } => vec![Engine::Claude],
            Self::Azure { engines, .. } => engines.iter().map(|e| e.into_engine()).collect(),
        }
    }
}

/// Passwordless local OpenAI-compatible inference endpoint (`auth = "none"` enforced on parse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalProfile {
    pub(crate) provider: LocalProvider,
    pub(crate) base_url: String,
    pub(crate) engines: BTreeSet<RelayEngine>,
}

/// Concrete authentication configuration variant (layer 2: AuthProfile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthProfile {
    Relay(RelayProfile),
    NativeLogin(NativeLoginProfile),
    CloudIam(CloudIamProfile),
    Local(LocalProfile),
}

impl AuthProfile {
    pub(crate) const fn mode_str(&self) -> &'static str {
        match self {
            Self::Relay(_) => "relay",
            Self::NativeLogin(_) => "native_login",
            Self::CloudIam(_) => "cloud_iam",
            Self::Local(_) => "local",
        }
    }

    pub(crate) fn engines(&self) -> Vec<Engine> {
        match self {
            Self::Relay(relay) => relay.engines.iter().map(|e| e.into_engine()).collect(),
            Self::NativeLogin(native) => vec![native.provider.engine()],
            Self::CloudIam(cloud) => cloud.engines(),
            Self::Local(local) => local.engines.iter().map(|e| e.into_engine()).collect(),
        }
    }
}

/// Origin of a loaded profile record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileSource {
    Configured,
    Default,
}

/// Named profile combining layer 2 (`AuthProfile`) and compatible layer 3 (`ModelTier`) values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedAuthProfile {
    pub(crate) name: String,
    pub(crate) source: ProfileSource,
    pub(crate) tiers: Vec<ModelTier>,
    pub(crate) profile: AuthProfile,
}

/// Isolated subprocess environment delta produced for one engine execution.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct EnvPlan {
    pub(crate) set: BTreeMap<String, String>,
    pub(crate) unset: Vec<String>,
    secret_keys: BTreeSet<String>,
}

impl EnvPlan {
    fn set_public(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.set.insert(key.into(), value.into());
    }

    fn set_secret(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        self.secret_keys.insert(key.clone());
        self.set.insert(key, value.into());
    }

    fn push_unset(&mut self, key: impl Into<String>) {
        let key = key.into();
        if !self.unset.contains(&key) {
            self.unset.push(key);
        }
    }

    /// Return a map safe for logging/JSON with all sensitive values masked as `<redacted>`.
    pub(crate) fn masked_summary(&self) -> BTreeMap<String, String> {
        self.set
            .iter()
            .map(|(key, value)| {
                let upper = key.to_ascii_uppercase();
                let sensitive = self.secret_keys.contains(key)
                    || upper.contains("KEY")
                    || upper.contains("TOKEN")
                    || upper.contains("SECRET");
                (
                    key.clone(),
                    if sensitive && value != "not-required" {
                        "<redacted>".to_string()
                    } else {
                        value.clone()
                    },
                )
            })
            .collect()
    }
}

impl fmt::Debug for EnvPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvPlan")
            .field("set", &self.masked_summary())
            .field("unset", &self.unset)
            .field("secret_keys", &self.secret_keys)
            .finish()
    }
}

/// Subprocess invocation plan for `spawn auth login <agy|claude|codex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginCommandPlan {
    pub(crate) profile: String,
    pub(crate) engine: Engine,
    pub(crate) profile_dir: PathBuf,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: EnvPlan,
}

impl RelayProfile {
    /// Translate a resolved relay secret into isolated environment variables for `engine`.
    pub(crate) fn translate_env(
        &self,
        engine: RelayEngine,
        secret: &str,
    ) -> Result<EnvPlan, String> {
        if !self.engines.contains(&engine) {
            return Err(format!(
                "relay profile does not permit engine `{}`",
                engine.as_str()
            ));
        }
        if secret.trim().is_empty() {
            return Err("resolved relay secret must not be empty".to_string());
        }
        if secret.contains(['\n', '\r', '\0']) {
            return Err("resolved relay secret contains invalid newline or NUL byte".to_string());
        }

        let mut plan = EnvPlan::default();
        match engine {
            RelayEngine::Claude => {
                plan.set_public("ANTHROPIC_BASE_URL", &self.base_url);
                plan.set_secret("ANTHROPIC_API_KEY", secret);
                plan.push_unset("CLAUDE_CODE_USE_BEDROCK");
                plan.push_unset("CLAUDE_CODE_USE_VERTEX");
                plan.push_unset("CLAUDE_CODE_USE_FOUNDRY");
                plan.push_unset("ANTHROPIC_AUTH_TOKEN");
            }
            RelayEngine::Codex => {
                plan.set_public("OPENAI_BASE_URL", &self.base_url);
                plan.set_secret("OPENAI_API_KEY", secret);
            }
            RelayEngine::OpenCode => {
                plan.set_public("OPENAI_BASE_URL", &self.base_url);
                plan.set_secret("OPENAI_API_KEY", secret);
                plan.set_public("ANTHROPIC_BASE_URL", &self.base_url);
                plan.set_secret("ANTHROPIC_API_KEY", secret);
            }
        }
        Ok(plan)
    }
}

impl NativeLoginProfile {
    /// Build the isolated environment delta for this native login directory.
    pub(crate) fn translate_env(&self, profile_name: &str) -> EnvPlan {
        let mut env = EnvPlan::default();
        env.set_public(
            "XDG_CONFIG_HOME",
            self.profile_dir.join("xdg-config").display().to_string(),
        );
        env.set_public(
            "XDG_DATA_HOME",
            self.profile_dir.join("xdg-data").display().to_string(),
        );
        env.set_public("SPAWN_AUTH_PROFILE", profile_name);
        env.set_public(
            "SPAWN_AUTH_PROFILE_DIR",
            self.profile_dir.display().to_string(),
        );
        match self.provider {
            NativeLoginProvider::ClaudeOauth => {
                env.set_public("CLAUDE_CONFIG_DIR", self.profile_dir.display().to_string());
                env.push_unset("ANTHROPIC_API_KEY");
                env.push_unset("ANTHROPIC_BASE_URL");
                env.push_unset("ANTHROPIC_AUTH_TOKEN");
            }
            NativeLoginProvider::CodexOauth => {
                env.set_public("CODEX_HOME", self.profile_dir.display().to_string());
                env.push_unset("OPENAI_API_KEY");
                env.push_unset("OPENAI_BASE_URL");
            }
            NativeLoginProvider::Agy => {
                env.set_public("AGY_HOME", self.profile_dir.display().to_string());
                env.set_public("GEMINI_CLI_HOME", self.profile_dir.display().to_string());
                env.push_unset("GEMINI_API_KEY");
                env.push_unset("GOOGLE_API_KEY");
            }
        }
        env
    }
}

impl CloudIamProfile {
    /// Translate passwordless cloud IAM metadata into isolated environment variables.
    pub(crate) fn translate_env(&self, engine: Engine) -> Result<EnvPlan, String> {
        if engine == Engine::Agy {
            return Err("engine `agy` only supports `native_login` (not `cloud_iam`)".to_string());
        }
        if !self.engines().contains(&engine) {
            return Err(format!(
                "cloud_iam provider `{}` does not permit engine `{engine}`",
                self.provider_str()
            ));
        }

        let mut plan = EnvPlan::default();
        match self {
            Self::Vertex {
                region,
                project,
                service_account,
                endpoint,
            } => {
                plan.set_public("CLAUDE_CODE_USE_VERTEX", "1");
                plan.set_public("CLOUD_ML_REGION", region);
                plan.set_public("GOOGLE_CLOUD_REGION", region);
                if let Some(proj) = project {
                    plan.set_public("ANTHROPIC_VERTEX_PROJECT_ID", proj);
                    plan.set_public("GOOGLE_CLOUD_PROJECT", proj);
                }
                if let Some(sa) = service_account {
                    plan.set_public("GOOGLE_IMPERSONATE_SERVICE_ACCOUNT", sa);
                }
                if let Some(url) = endpoint {
                    plan.set_public("ANTHROPIC_VERTEX_BASE_URL", url);
                }
                plan.push_unset("ANTHROPIC_API_KEY");
                plan.push_unset("ANTHROPIC_AUTH_TOKEN");
            }
            Self::Bedrock {
                region,
                aws_profile,
                endpoint,
            } => {
                plan.set_public("CLAUDE_CODE_USE_BEDROCK", "1");
                plan.set_public("AWS_REGION", region);
                plan.set_public("AWS_DEFAULT_REGION", region);
                if let Some(profile) = aws_profile {
                    plan.set_public("AWS_PROFILE", profile);
                }
                if let Some(url) = endpoint {
                    plan.set_public("ANTHROPIC_BEDROCK_BASE_URL", url);
                }
                plan.push_unset("ANTHROPIC_API_KEY");
                plan.push_unset("ANTHROPIC_AUTH_TOKEN");
            }
            Self::Azure {
                region,
                resource,
                tenant_id,
                endpoint,
                ..
            } => {
                plan.set_public("AZURE_REGION", region);
                if let Some(res) = resource {
                    plan.set_public("AZURE_OPENAI_RESOURCE", res);
                }
                if let Some(tenant) = tenant_id {
                    plan.set_public("AZURE_TENANT_ID", tenant);
                }
                if let Some(url) = endpoint {
                    plan.set_public("AZURE_OPENAI_ENDPOINT", url);
                    plan.set_public("OPENAI_BASE_URL", url);
                }
                plan.push_unset("OPENAI_API_KEY");
                plan.push_unset("ANTHROPIC_API_KEY");
            }
        }
        Ok(plan)
    }
}

impl LocalProfile {
    /// Translate passwordless local inference settings into isolated environment variables.
    pub(crate) fn translate_env(&self, engine: RelayEngine) -> Result<EnvPlan, String> {
        if !self.engines.contains(&engine) {
            return Err(format!(
                "local profile ({}) does not permit engine `{}`",
                self.provider.as_str(),
                engine.as_str()
            ));
        }
        let mut plan = EnvPlan::default();
        plan.set_public("SPAWN_LOCAL_PROVIDER", self.provider.as_str());
        match engine {
            RelayEngine::Claude => {
                plan.set_public("ANTHROPIC_BASE_URL", &self.base_url);
                plan.set_public("ANTHROPIC_API_KEY", "not-required");
                plan.push_unset("CLAUDE_CODE_USE_BEDROCK");
                plan.push_unset("CLAUDE_CODE_USE_VERTEX");
            }
            RelayEngine::Codex | RelayEngine::OpenCode => {
                plan.set_public("OPENAI_BASE_URL", &self.base_url);
                plan.set_public("OPENAI_API_KEY", "not-required");
            }
        }
        Ok(plan)
    }
}

fn format_engine_list(engines: &[Engine]) -> String {
    engines
        .iter()
        .map(|engine| engine.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Validate a profile name (`[A-Za-z0-9][A-Za-z0-9_-]{0,63}`).
pub(crate) fn validate_profile_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(format!(
            "profile name `{trimmed}` must be 1..=64 characters long"
        ));
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return Err("profile name must not be empty".to_string());
    };
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "profile name `{trimmed}` must start with an ASCII letter or digit"
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
        return Err(format!(
            "profile name `{trimmed}` may only contain ASCII letters, digits, `-`, and `_`"
        ));
    }
    Ok(trimmed.to_string())
}

/// Validate an environment variable identifier without echoing invalid secret-like inputs.
pub(crate) fn validate_env_var_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    let valid_first = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    if !valid_first || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(
            "invalid environment variable name: expected [A-Za-z_][A-Za-z0-9_]* (never pass a raw key to --key-env or from_env)"
                .to_string(),
        );
    }
    Ok(trimmed.to_string())
}

fn validate_safe_identifier(field: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@' | ':'))
    {
        return Err(format!(
            "`{field}` contains invalid characters or whitespace"
        ));
    }
    Ok(trimmed.to_string())
}

/// Strictly validate an `http://` or `https://` endpoint URL without echoing the raw URL.
///
/// Rejects case-insensitively non-http(s) schemes, `@` userinfo, `?`/`&` query syntax,
/// `#` fragments, `;` path parameters, percent-encoded query/fragment/separator/userinfo
/// sequences, backslashes, non-ASCII/zero-width characters, and whitespace/control chars.
pub(crate) fn validate_endpoint_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("endpoint URL must be a single valid http:// or https:// URL".to_string());
    }
    let Some((raw_scheme, rest)) = trimmed.split_once("://") else {
        return Err("endpoint URL must start with https:// or http://".to_string());
    };
    let scheme = raw_scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("endpoint URL must use https:// or http:// scheme".to_string());
    }
    if rest.contains('\\') {
        return Err("endpoint URL must not contain backslashes".to_string());
    }
    if rest.contains('?') {
        return Err(
            "endpoint URL must not contain query parameters (`?`); pass credentials via SecretRef"
                .to_string(),
        );
    }
    if rest.contains('&') {
        return Err(
            "endpoint URL must not contain query separators (`&`); pass credentials via SecretRef"
                .to_string(),
        );
    }
    if rest.contains('#') {
        return Err(
            "endpoint URL must not contain a fragment (`#`); pass credentials via SecretRef"
                .to_string(),
        );
    }
    if rest.contains(';') {
        return Err("endpoint URL must not contain path parameters (`;`)".to_string());
    }
    let rest_lower = rest.to_ascii_lowercase();
    if ["%3f", "%23", "%40", "%26", "%3b", "%5c"]
        .iter()
        .any(|enc| rest_lower.contains(enc))
    {
        return Err(
            "endpoint URL must not contain percent-encoded query, fragment, separator, or userinfo characters; pass credentials via SecretRef"
                .to_string(),
        );
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err("endpoint URL is missing a host".to_string());
    }
    if authority.contains('@') {
        return Err(
            "endpoint URL must not embed userinfo or credentials (`@`); use SecretRef".to_string(),
        );
    }
    Ok(format!("{scheme}://{}", rest.trim_end_matches('/')))
}

fn normalize_lexical(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("path must be absolute".to_string());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push(Component::RootDir),
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => return Err("path must not contain `..` traversal".to_string()),
            Component::Prefix(_) => return Err("unsupported path prefix".to_string()),
        }
    }
    Ok(normalized)
}

fn normalize_secret_file_path(path: &Path) -> Result<PathBuf, String> {
    normalize_lexical(path).map_err(|_| {
        "secret file path must be an absolute path without `..` traversal (never pass a raw key to --key-file or from_file)"
            .to_string()
    })
}

fn resolve_through_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let mut cur = path;
    let mut tail = Vec::new();
    while !cur.exists() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        let Some(parent) = cur.parent() else {
            break;
        };
        cur = parent;
    }
    let mut resolved = fs::canonicalize(cur).unwrap_or_else(|_| cur.to_path_buf());
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    resolved
}

fn is_forbidden_bare_root(path: &Path) -> bool {
    FORBIDDEN_BARE_ROOTS
        .iter()
        .any(|blocked| path == Path::new(blocked))
}

fn touches_forbidden_home_path(candidate: &Path, home: Option<&Path>) -> bool {
    let Some(home_dir) = home else {
        return false;
    };
    let mut home_roots = vec![home_dir.to_path_buf()];
    if let Ok(canon) = fs::canonicalize(home_dir)
        && canon != home_dir
    {
        home_roots.push(canon);
    }
    for root in home_roots {
        if candidate == root {
            return true;
        }
        for sub in FORBIDDEN_HOME_SUBDIRS {
            let blocked = root.join(sub);
            if candidate == blocked || candidate.starts_with(&blocked) {
                return true;
            }
        }
    }
    false
}

/// Validate that the controlled `profiles_root` itself is not a bare system/shared
/// directory, `$HOME`, a sensitive host directory (`~/.ssh`, `~/.aws`, etc.), or symlink.
pub(crate) fn validate_profiles_root_dir(
    root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let normalized = normalize_lexical(root)?;
    let canonical = resolve_through_existing_ancestor(&normalized);
    if fs::symlink_metadata(&normalized)
        .ok()
        .is_some_and(|meta| meta.file_type().is_symlink())
    {
        return Err(format!(
            "auth profiles root {} must not be a symlink",
            root.display()
        ));
    }
    if is_forbidden_bare_root(&normalized)
        || is_forbidden_bare_root(&canonical)
        || touches_forbidden_home_path(&normalized, home)
        || touches_forbidden_home_path(&canonical, home)
    {
        return Err(format!(
            "auth profiles root {} refuses shared/system/sensitive directory {}",
            root.display(),
            canonical.display()
        ));
    }
    Ok(normalized)
}

fn ensure_no_symlink_tail(root_norm: &Path, dir_norm: &Path) -> Result<(), String> {
    let Ok(rel) = dir_norm.strip_prefix(root_norm) else {
        return Err(format!(
            "native_login profile_dir {} must live strictly inside controlled profiles root {}",
            dir_norm.display(),
            root_norm.display()
        ));
    };
    let mut cur = root_norm.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        if let Ok(meta) = fs::symlink_metadata(&cur)
            && meta.file_type().is_symlink()
        {
            return Err(format!(
                "native_login profile_dir {} contains symlink component {}",
                dir_norm.display(),
                cur.display()
            ));
        }
    }
    Ok(())
}

/// Enforce strict whitelist containment: `dir` must be a proper non-symlink descendant of
/// validated `profiles_root` (both lexically and canonically) and never touch host dirs.
pub(crate) fn validate_isolated_profile_dir(
    dir: &Path,
    profiles_root: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let root_norm = validate_profiles_root_dir(profiles_root, home)?;
    let dir_norm = normalize_lexical(dir)?;
    if dir_norm == root_norm || !dir_norm.starts_with(&root_norm) {
        return Err(format!(
            "native_login profile_dir {} must be a subdirectory strictly inside {}",
            dir.display(),
            root_norm.display()
        ));
    }
    ensure_no_symlink_tail(&root_norm, &dir_norm)?;

    let canon_root = resolve_through_existing_ancestor(&root_norm);
    let canon_dir = resolve_through_existing_ancestor(&dir_norm);
    if canon_dir == canon_root || !canon_dir.starts_with(&canon_root) {
        return Err(format!(
            "native_login profile_dir {} escapes controlled profiles root {}",
            dir.display(),
            root_norm.display()
        ));
    }
    if is_forbidden_bare_root(&dir_norm)
        || is_forbidden_bare_root(&canon_dir)
        || touches_forbidden_home_path(&dir_norm, home)
        || touches_forbidden_home_path(&canon_dir, home)
    {
        return Err(format!(
            "native_login profile_dir {} refuses forbidden host directory",
            dir.display()
        ));
    }
    Ok(dir_norm)
}

fn current_unix_secs() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

enum SecretResolveError {
    Missing(String),
    Invalid(String),
}

fn resolve_secret_safely(
    secret_ref: &SecretRef,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, Vec<String>), SecretResolveError> {
    match secret_ref {
        SecretRef::FromEnv { from_env } => {
            validate_env_var_name(from_env).map_err(SecretResolveError::Invalid)?;
            let value = secret_ref
                .resolve_secret_with(env_lookup)
                .map(SecretString::into_inner)
                .map_err(|_| {
                    SecretResolveError::Missing(format!(
                        "environment variable `{from_env}` is unset or empty"
                    ))
                })?;
            Ok((value, Vec::new()))
        }
        SecretRef::FromFile { from_file } => {
            let normalized =
                normalize_secret_file_path(from_file).map_err(SecretResolveError::Invalid)?;
            let meta = match fs::symlink_metadata(&normalized) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SecretResolveError::Missing(format!(
                        "secret file {} does not exist",
                        normalized.display()
                    )));
                }
                Err(error) => {
                    return Err(SecretResolveError::Invalid(format!(
                        "cannot stat secret file {}: {error}",
                        normalized.display()
                    )));
                }
            };
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(SecretResolveError::Invalid(format!(
                    "secret file {} must be a regular non-symlink file",
                    normalized.display()
                )));
            }
            let mut warnings = Vec::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    warnings.push(format!(
                        "secret file {} has permissions {mode:04o}; consider 0600",
                        normalized.display()
                    ));
                }
            }
            let value = secret_ref
                .resolve_secret_with(env_lookup)
                .map(SecretString::into_inner)
                .map_err(SecretResolveError::Invalid)?;
            Ok((value, warnings))
        }
    }
}

/// Non-destructive probe result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeOutcome {
    pub(crate) readiness: ProfileReadiness,
    pub(crate) detail: String,
    pub(crate) warnings: Vec<String>,
}

fn parse_expiry_timestamp(value: &Value) -> Option<i64> {
    let secs = match value {
        Value::Number(num) => num
            .as_i64()
            .or_else(|| num.as_u64().and_then(|u| i64::try_from(u).ok()))
            .or_else(|| {
                #[expect(clippy::cast_possible_truncation)]
                num.as_f64()
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .map(|f| f as i64)
            }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }?;
    Some(if secs > 10_000_000_000 {
        secs / 1000
    } else {
        secs
    })
}

fn inspect_oauth_json(doc: &Value, now_unix_secs: i64) -> Result<(), String> {
    let Some(obj) = doc.as_object() else {
        return Err("OAuth credential artifact must be a JSON object".to_string());
    };
    if obj
        .get("reauth_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || obj.get("expired").and_then(Value::as_bool).unwrap_or(false)
    {
        return Err("OAuth credential marker indicates reauth_required".to_string());
    }
    if let Some(status) = obj.get("status").and_then(Value::as_str)
        && matches!(
            status.trim().to_ascii_lowercase().as_str(),
            "expired" | "revoked" | "reauth_required"
        )
    {
        return Err(format!("OAuth session status is `{status}`"));
    }
    let containers = [
        Some(obj),
        obj.get("claudeAiOauth").and_then(Value::as_object),
        obj.get("oauth").and_then(Value::as_object),
        obj.get("tokens").and_then(Value::as_object),
    ];
    for map in containers.into_iter().flatten() {
        for field in ["expires_at", "expiresAt", "expiry_date", "expiry"] {
            if let Some(raw_exp) = map.get(field)
                && let Some(exp_secs) = parse_expiry_timestamp(raw_exp)
                && exp_secs <= now_unix_secs
            {
                return Err("OAuth session expired; run `spawn auth login`".to_string());
            }
        }
    }
    Ok(())
}

/// Non-destructively probe an isolated `native_login` directory without reading host files.
pub(crate) fn probe_native_login(
    dir: &Path,
    profiles_root: &Path,
    provider: NativeLoginProvider,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> ProbeOutcome {
    if let Err(reason) = validate_isolated_profile_dir(dir, profiles_root, home) {
        return ProbeOutcome {
            readiness: ProfileReadiness::ReauthRequired,
            detail: reason,
            warnings: Vec::new(),
        };
    }
    if !dir.is_dir() {
        return ProbeOutcome {
            readiness: ProfileReadiness::Missing,
            detail: format!("isolated profile dir {} does not exist yet", dir.display()),
            warnings: Vec::new(),
        };
    }
    for rel in provider.credential_candidates() {
        let candidate = dir.join(rel);
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return ProbeOutcome {
                readiness: ProfileReadiness::ReauthRequired,
                detail: format!(
                    "credential artifact {rel} in {} must be a regular file",
                    dir.display()
                ),
                warnings: Vec::new(),
            };
        }
        let mut warnings = Vec::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "{} has mode {mode:04o}; consider chmod 600",
                    candidate.display()
                ));
            }
        }
        let Ok(raw) = fs::read_to_string(&candidate) else {
            return ProbeOutcome {
                readiness: ProfileReadiness::ReauthRequired,
                detail: format!("credential artifact {rel} is unreadable or non-UTF-8"),
                warnings,
            };
        };
        if raw.trim().is_empty() {
            return ProbeOutcome {
                readiness: ProfileReadiness::ReauthRequired,
                detail: format!("credential artifact {rel} is empty"),
                warnings,
            };
        }
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            return ProbeOutcome {
                readiness: ProfileReadiness::ReauthRequired,
                detail: format!("credential artifact {rel} is malformed JSON"),
                warnings,
            };
        };
        if let Err(reason) = inspect_oauth_json(&parsed, now_unix_secs) {
            return ProbeOutcome {
                readiness: ProfileReadiness::ReauthRequired,
                detail: reason,
                warnings,
            };
        }
        return ProbeOutcome {
            readiness: ProfileReadiness::Ready,
            detail: format!("verified isolated {} session ({rel})", provider.as_str()),
            warnings,
        };
    }
    ProbeOutcome {
        readiness: ProfileReadiness::Missing,
        detail: format!(
            "no {} credentials found in isolated dir {}",
            provider.as_str(),
            dir.display()
        ),
        warnings: Vec::new(),
    }
}

fn probe_secret_ref_with(
    secret: &SecretRef,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> ProbeOutcome {
    match resolve_secret_safely(secret, env_lookup) {
        Ok((_resolved_and_dropped, warnings)) => {
            let detail = match secret {
                SecretRef::FromEnv { from_env } => {
                    format!("from_env:{from_env} is set (non-empty)")
                }
                SecretRef::FromFile { from_file } => {
                    format!("from_file:{} readable", from_file.display())
                }
            };
            ProbeOutcome {
                readiness: ProfileReadiness::Ready,
                detail,
                warnings,
            }
        }
        Err(SecretResolveError::Missing(detail)) => ProbeOutcome {
            readiness: ProfileReadiness::Missing,
            detail,
            warnings: Vec::new(),
        },
        Err(SecretResolveError::Invalid(detail)) => ProbeOutcome {
            readiness: ProfileReadiness::ReauthRequired,
            detail,
            warnings: Vec::new(),
        },
    }
}

fn ini_has_section(contents: &str, expected: &str) -> bool {
    contents.lines().any(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|hdr| hdr.trim() == expected)
    })
}

fn aws_bedrock_credential_evidence(
    aws_profile: Option<&str>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    file_reader: &dyn Fn(&Path) -> Option<String>,
    home: Option<&Path>,
) -> Option<&'static str> {
    let has_env = |name: &str| env_lookup(name).is_some_and(|v| !v.trim().is_empty());
    let env_file = |name: &str| {
        env_lookup(name)
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
    };

    if env_file("AWS_WEB_IDENTITY_TOKEN_FILE").is_some_and(|p| file_exists(&p)) {
        return Some("AWS_WEB_IDENTITY_TOKEN_FILE present");
    }
    if has_env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        || has_env("AWS_CONTAINER_CREDENTIALS_FULL_URI")
    {
        return Some("AWS container credentials endpoint detected");
    }
    if has_env("AWS_ACCESS_KEY_ID") && has_env("AWS_SECRET_ACCESS_KEY") {
        return Some("AWS access key env vars present");
    }

    let cred_path = env_file("AWS_SHARED_CREDENTIALS_FILE")
        .or_else(|| home.map(|h| h.join(".aws/credentials")));
    let cfg_path = env_file("AWS_CONFIG_FILE").or_else(|| home.map(|h| h.join(".aws/config")));
    let active_profile = aws_profile
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| env_lookup("AWS_PROFILE").filter(|s| !s.trim().is_empty()));

    if let Some(prof) = active_profile {
        if let Some(p) = &cred_path
            && file_reader(p).is_some_and(|raw| ini_has_section(&raw, &prof))
        {
            return Some("matching profile found in AWS credentials file");
        }
        let cfg_hdr = if prof == "default" {
            "default".to_string()
        } else {
            format!("profile {prof}")
        };
        if let Some(p) = &cfg_path
            && file_reader(p).is_some_and(|raw| ini_has_section(&raw, &cfg_hdr))
        {
            return Some("matching profile found in AWS config file");
        }
        return None;
    }

    if cred_path.is_some_and(|p| file_exists(&p)) || cfg_path.is_some_and(|p| file_exists(&p)) {
        return Some("local AWS shared config/credentials file present");
    }
    None
}

/// Non-destructively probe whether ambient passwordless Cloud IAM credentials exist.
pub(crate) fn probe_cloud_iam_with(
    cloud: &CloudIamProfile,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    file_reader: &dyn Fn(&Path) -> Option<String>,
    home: Option<&Path>,
) -> ProbeOutcome {
    let has_env = |name: &str| env_lookup(name).is_some_and(|v| !v.trim().is_empty());
    let env_file_exists = |name: &str| {
        env_lookup(name).is_some_and(|p| !p.trim().is_empty() && file_exists(Path::new(&p)))
    };

    let detected: Option<&str> = match cloud {
        CloudIamProfile::Vertex { .. } => {
            if env_file_exists("GOOGLE_APPLICATION_CREDENTIALS") {
                Some("GOOGLE_APPLICATION_CREDENTIALS file present")
            } else if has_env("CLOUDSDK_AUTH_ACCESS_TOKEN")
                || has_env("GCE_METADATA_HOST")
                || env_file_exists("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_FILE")
            {
                Some("ambient GCP workload identity / metadata env detected")
            } else if home.is_some_and(|h| {
                file_exists(&h.join(".config/gcloud/application_default_credentials.json"))
            }) {
                Some("gcloud application_default_credentials.json present")
            } else {
                None
            }
        }
        CloudIamProfile::Bedrock { aws_profile, .. } => aws_bedrock_credential_evidence(
            aws_profile.as_deref(),
            env_lookup,
            file_exists,
            file_reader,
            home,
        ),
        CloudIamProfile::Azure { .. } => {
            if env_file_exists("AZURE_FEDERATED_TOKEN_FILE") {
                Some("AZURE_FEDERATED_TOKEN_FILE present")
            } else if has_env("IDENTITY_ENDPOINT") || has_env("MSI_ENDPOINT") {
                Some("Azure managed identity endpoint detected")
            } else if home.is_some_and(|h| {
                file_exists(&h.join(".azure/msal_token_cache.json"))
                    || file_exists(&h.join(".azure/azureProfile.json"))
            }) {
                Some("Azure CLI MSAL cache present")
            } else {
                None
            }
        }
    };

    if let Some(evidence) = detected {
        ProbeOutcome {
            readiness: ProfileReadiness::Ready,
            detail: format!(
                "{} IAM chain ready (region={}, {evidence})",
                cloud.provider_str(),
                cloud.region()
            ),
            warnings: Vec::new(),
        }
    } else {
        ProbeOutcome {
            readiness: ProfileReadiness::Missing,
            detail: format!(
                "no ambient {} IAM credentials detected (region={})",
                cloud.provider_str(),
                cloud.region()
            ),
            warnings: Vec::new(),
        }
    }
}

fn probe_profile_with(
    profile: &AuthProfile,
    profiles_root: &Path,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    file_reader: &dyn Fn(&Path) -> Option<String>,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> ProbeOutcome {
    match profile {
        AuthProfile::Relay(relay) => probe_secret_ref_with(&relay.api_key, env_lookup),
        AuthProfile::NativeLogin(native) => probe_native_login(
            &native.profile_dir,
            profiles_root,
            native.provider,
            home,
            now_unix_secs,
        ),
        AuthProfile::CloudIam(cloud) => {
            probe_cloud_iam_with(cloud, env_lookup, file_exists, file_reader, home)
        }
        AuthProfile::Local(local) => ProbeOutcome {
            readiness: ProfileReadiness::Ready,
            detail: format!(
                "local {} endpoint {} (auth=none)",
                local.provider.as_str(),
                local.base_url
            ),
            warnings: Vec::new(),
        },
    }
}

/// Build an isolated command/environment plan to run `spawn auth login <agy|claude|codex>`.
pub(crate) fn plan_login_command(
    provider: NativeLoginProvider,
    profile_name: &str,
    profile_dir: &Path,
    profiles_root: &Path,
    home: Option<&Path>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<LoginCommandPlan, String> {
    let validated_name = validate_profile_name(profile_name)?;
    let validated_dir = validate_isolated_profile_dir(profile_dir, profiles_root, home)?;
    let native = NativeLoginProfile {
        provider,
        profile_dir: validated_dir.clone(),
    };
    let env = native.translate_env(&validated_name);
    let (env_var, default_bin, args) = match provider {
        NativeLoginProvider::ClaudeOauth => (
            "SPAWN_AUTH_CLAUDE_BIN",
            "claude",
            vec!["auth".to_string(), "login".to_string()],
        ),
        NativeLoginProvider::CodexOauth => {
            ("SPAWN_AUTH_CODEX_BIN", "codex", vec!["login".to_string()])
        }
        NativeLoginProvider::Agy => (
            "SPAWN_AUTH_AGY_BIN",
            "agy",
            vec!["auth".to_string(), "login".to_string()],
        ),
    };
    let program = env_lookup(env_var)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default_bin.to_string());

    Ok(LoginCommandPlan {
        profile: validated_name,
        engine: provider.engine(),
        profile_dir: validated_dir,
        program,
        args,
        env,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRelayProfile {
    mode: String,
    base_url: String,
    api_key: SecretRef,
    #[serde(default)]
    engines: Vec<RelayEngine>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNativeLoginProfile {
    mode: String,
    provider: String,
    profile_dir: Option<PathBuf>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVertexIam {
    mode: String,
    provider: String,
    region: String,
    project: Option<String>,
    service_account: Option<String>,
    endpoint: Option<String>,
    engines: Option<Vec<String>>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBedrockIam {
    mode: String,
    provider: String,
    region: String,
    aws_profile: Option<String>,
    endpoint: Option<String>,
    engines: Option<Vec<String>>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAzureIam {
    mode: String,
    provider: String,
    region: String,
    resource: Option<String>,
    tenant_id: Option<String>,
    endpoint: Option<String>,
    engines: Option<Vec<String>>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLocalProfile {
    mode: String,
    provider: LocalProvider,
    base_url: Option<String>,
    auth: Option<String>,
    #[serde(default)]
    engines: Vec<RelayEngine>,
    #[serde(default)]
    tiers: Vec<ModelTier>,
}

fn reject_plaintext_fields_in_json(profile_name: &str, value: &Value) -> Result<(), String> {
    let Some(map) = value.as_object() else {
        return Err(format!(
            "[auth.profiles.{profile_name}] must be a TOML table"
        ));
    };
    for (k, v) in map {
        let lower = k.to_ascii_lowercase();
        if FORBIDDEN_PLAINTEXT_FIELDS.contains(&lower.as_str()) {
            return Err(format!(
                "[auth.profiles.{profile_name}]: field `{k}` looks like an inline credential; {PLAINTEXT_ERR}"
            ));
        }
        if lower == "api_key" {
            let Some(sub) = v.as_object() else {
                return Err(format!(
                    "[auth.profiles.{profile_name}]: `api_key` must be a SecretRef table; {PLAINTEXT_ERR}"
                ));
            };
            // Pre-validate `from_env`/`from_file` cleanly before SecretRef deserialization
            // so mis-pasted plaintext secrets are never echoed in serde error text.
            if let Some(Value::String(env_name)) = sub.get("from_env") {
                validate_env_var_name(env_name)
                    .map_err(|error| format!("[auth.profiles.{profile_name}]: {error}"))?;
            }
            if let Some(Value::String(file_path)) = sub.get("from_file") {
                normalize_secret_file_path(Path::new(file_path))
                    .map_err(|error| format!("[auth.profiles.{profile_name}]: {error}"))?;
            }
        }
        if let Some(sub) = v.as_object() {
            for sub_key in sub.keys() {
                if FORBIDDEN_PLAINTEXT_FIELDS.contains(&sub_key.to_ascii_lowercase().as_str()) {
                    return Err(format!(
                        "[auth.profiles.{profile_name}.{k}]: field `{sub_key}` looks like an inline credential; {PLAINTEXT_ERR}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn normalize_tiers(tiers: Vec<ModelTier>) -> Vec<ModelTier> {
    let mut out = Vec::new();
    for t in tiers {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.is_empty() {
        ModelTier::ALL.to_vec()
    } else {
        out
    }
}

fn validate_claude_only_engines(
    profile_name: &str,
    provider: &str,
    configured: Option<&[String]>,
) -> Result<(), String> {
    let Some(list) = configured else {
        return Ok(());
    };
    if list.is_empty() {
        return Err(format!(
            "[auth.profiles.{profile_name}]: `engines` must not be empty when specified"
        ));
    }
    for raw in list {
        let norm = raw.trim().to_ascii_lowercase();
        if matches!(norm.as_str(), "agy" | "antigravity") {
            return Err(format!(
                "[auth.profiles.{profile_name}]: engine `agy` only supports `native_login` (not `cloud_iam`)"
            ));
        }
        if norm != "claude" {
            return Err(format!(
                "[auth.profiles.{profile_name}]: cloud_iam provider `{provider}` only permits [claude], got `{raw}`"
            ));
        }
    }
    Ok(())
}

fn parse_azure_engines(
    profile_name: &str,
    configured: Option<Vec<String>>,
) -> Result<BTreeSet<AzureEngine>, String> {
    let Some(list) = configured else {
        return Ok([AzureEngine::Codex, AzureEngine::OpenCode]
            .into_iter()
            .collect());
    };
    if list.is_empty() {
        return Err(format!(
            "[auth.profiles.{profile_name}]: `engines` must not be empty when specified"
        ));
    }
    list.into_iter()
        .map(|raw| {
            AzureEngine::parse(&raw)
                .map_err(|error| format!("[auth.profiles.{profile_name}]: {error}"))
        })
        .collect()
}

fn parse_single_profile(
    name: &str,
    value: Value,
    default_profiles_root: &Path,
    home: Option<&Path>,
) -> Result<NamedAuthProfile, String> {
    let valid_name = validate_profile_name(name)?;
    reject_plaintext_fields_in_json(&valid_name, &value)?;

    let mode = value
        .as_object()
        .and_then(|map| map.get("mode"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("[auth.profiles.{valid_name}]: `mode` is required (relay | native_login | cloud_iam | local)")
        })?
        .trim()
        .to_ascii_lowercase();

    match mode.as_str() {
        "relay" => parse_relay_table(valid_name, value),
        "native_login" => parse_native_login_table(valid_name, value, default_profiles_root, home),
        "cloud_iam" => parse_cloud_iam_table(valid_name, value),
        "local" => parse_local_table(valid_name, value),
        other => Err(format!(
            "[auth.profiles.{valid_name}]: unknown mode `{other}`; expected relay, native_login, cloud_iam, or local"
        )),
    }
}

fn parse_relay_table(name: String, value: Value) -> Result<NamedAuthProfile, String> {
    let raw: RawRelayProfile = serde_json::from_value(value)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    let _ = raw.mode;
    let base_url = validate_endpoint_url(&raw.base_url)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    if let SecretRef::FromEnv { ref from_env } = raw.api_key {
        validate_env_var_name(from_env)
            .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    } else if let SecretRef::FromFile { ref from_file } = raw.api_key {
        normalize_secret_file_path(from_file)
            .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    }
    let engines: BTreeSet<RelayEngine> = raw.engines.into_iter().collect();
    if engines.is_empty() {
        return Err(format!(
            "[auth.profiles.{name}]: `engines` must list at least one engine (claude, codex, opencode)"
        ));
    }
    Ok(NamedAuthProfile {
        name,
        source: ProfileSource::Configured,
        tiers: normalize_tiers(raw.tiers),
        profile: AuthProfile::Relay(RelayProfile {
            base_url,
            api_key: raw.api_key,
            engines,
        }),
    })
}

fn parse_native_login_table(
    name: String,
    value: Value,
    default_profiles_root: &Path,
    home: Option<&Path>,
) -> Result<NamedAuthProfile, String> {
    let raw: RawNativeLoginProfile = serde_json::from_value(value)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    let _ = raw.mode;
    let provider = NativeLoginProvider::parse_provider(&raw.provider)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    let candidate_dir = raw
        .profile_dir
        .unwrap_or_else(|| default_profiles_root.join(&name));
    let profile_dir = validate_isolated_profile_dir(&candidate_dir, default_profiles_root, home)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    Ok(NamedAuthProfile {
        name,
        source: ProfileSource::Configured,
        tiers: normalize_tiers(raw.tiers),
        profile: AuthProfile::NativeLogin(NativeLoginProfile {
            provider,
            profile_dir,
        }),
    })
}

fn opt_safe_id(name: &str, field: &str, val: Option<&str>) -> Result<Option<String>, String> {
    val.map(|v| validate_safe_identifier(field, v))
        .transpose()
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))
}

fn opt_endpoint(name: &str, val: Option<&str>) -> Result<Option<String>, String> {
    val.map(validate_endpoint_url)
        .transpose()
        .map_err(|error| format!("[auth.profiles.{name}]: endpoint: {error}"))
}

fn parse_cloud_iam_table(name: String, value: Value) -> Result<NamedAuthProfile, String> {
    let provider = value
        .as_object()
        .and_then(|map| map.get("provider"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "[auth.profiles.{name}]: cloud_iam requires `provider` (vertex | bedrock | azure)"
            )
        })?
        .trim()
        .to_ascii_lowercase();

    match provider.as_str() {
        "vertex" => {
            let raw: RawVertexIam = serde_json::from_value(value)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let _ = (raw.mode, raw.provider);
            validate_claude_only_engines(&name, "vertex", raw.engines.as_deref())?;
            let region = validate_safe_identifier("region", &raw.region)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let cloud = CloudIamProfile::Vertex {
                region,
                project: opt_safe_id(&name, "project", raw.project.as_deref())?,
                service_account: opt_safe_id(
                    &name,
                    "service_account",
                    raw.service_account.as_deref(),
                )?,
                endpoint: opt_endpoint(&name, raw.endpoint.as_deref())?,
            };
            Ok(NamedAuthProfile {
                name,
                source: ProfileSource::Configured,
                tiers: normalize_tiers(raw.tiers),
                profile: AuthProfile::CloudIam(cloud),
            })
        }
        "bedrock" => {
            let raw: RawBedrockIam = serde_json::from_value(value)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let _ = (raw.mode, raw.provider);
            validate_claude_only_engines(&name, "bedrock", raw.engines.as_deref())?;
            let region = validate_safe_identifier("region", &raw.region)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let cloud = CloudIamProfile::Bedrock {
                region,
                aws_profile: opt_safe_id(&name, "aws_profile", raw.aws_profile.as_deref())?,
                endpoint: opt_endpoint(&name, raw.endpoint.as_deref())?,
            };
            Ok(NamedAuthProfile {
                name,
                source: ProfileSource::Configured,
                tiers: normalize_tiers(raw.tiers),
                profile: AuthProfile::CloudIam(cloud),
            })
        }
        "azure" => {
            let raw: RawAzureIam = serde_json::from_value(value)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let _ = (raw.mode, raw.provider);
            let engines = parse_azure_engines(&name, raw.engines)?;
            let region = validate_safe_identifier("region", &raw.region)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let cloud = CloudIamProfile::Azure {
                region,
                resource: opt_safe_id(&name, "resource", raw.resource.as_deref())?,
                tenant_id: opt_safe_id(&name, "tenant_id", raw.tenant_id.as_deref())?,
                endpoint: opt_endpoint(&name, raw.endpoint.as_deref())?,
                engines,
            };
            Ok(NamedAuthProfile {
                name,
                source: ProfileSource::Configured,
                tiers: normalize_tiers(raw.tiers),
                profile: AuthProfile::CloudIam(cloud),
            })
        }
        other => Err(format!(
            "[auth.profiles.{name}]: unknown cloud_iam provider `{other}`; expected vertex, bedrock, or azure"
        )),
    }
}

fn parse_local_table(name: String, value: Value) -> Result<NamedAuthProfile, String> {
    let raw: RawLocalProfile = serde_json::from_value(value)
        .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    let _ = raw.mode;
    if let Some(auth_val) = &raw.auth
        && !auth_val.trim().eq_ignore_ascii_case("none")
    {
        // Never echo invalid `auth` value in case an operator pasted a raw token.
        return Err(format!(
            "[auth.profiles.{name}]: local profile `auth` must be \"none\" (use `mode = \"relay\"` with SecretRef for keyed gateways)"
        ));
    }
    let base_url = validate_endpoint_url(
        raw.base_url
            .as_deref()
            .unwrap_or_else(|| raw.provider.default_base_url()),
    )
    .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
    let mut engines: BTreeSet<RelayEngine> = raw.engines.into_iter().collect();
    if engines.is_empty() {
        engines.insert(RelayEngine::Codex);
        engines.insert(RelayEngine::OpenCode);
    }
    Ok(NamedAuthProfile {
        name,
        source: ProfileSource::Configured,
        tiers: normalize_tiers(raw.tiers),
        profile: AuthProfile::Local(LocalProfile {
            provider: raw.provider,
            base_url,
            engines,
        }),
    })
}

/// Parse `[auth.profiles.<name>]` entries from a TOML document (`auth.toml` or `spawn.toml`).
pub(crate) fn parse_auth_profiles_toml(
    toml_str: &str,
    default_profiles_root: &Path,
    home: Option<&Path>,
) -> Result<BTreeMap<String, NamedAuthProfile>, String> {
    if toml_str.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let doc =
        toml_parser::parse(toml_str).map_err(|error| format!("parse auth TOML failed: {error}"))?;
    let Some(auth_val) = doc.get("auth") else {
        return Ok(BTreeMap::new());
    };
    let Some(auth_obj) = auth_val.as_object() else {
        return Err("[auth] must be a TOML table".to_string());
    };
    let Some(profiles_val) = auth_obj.get("profiles") else {
        return Ok(BTreeMap::new());
    };
    let Some(profiles_obj) = profiles_val.as_object() else {
        return Err("[auth.profiles] must be a TOML table".to_string());
    };
    let mut out = BTreeMap::new();
    for (name, entry) in profiles_obj {
        let parsed = parse_single_profile(name, entry.clone(), default_profiles_root, home)?;
        out.insert(parsed.name.clone(), parsed);
    }
    Ok(out)
}

/// Merge configured profiles with built-in isolated `native_login` defaults.
pub(crate) fn merge_with_default_profiles(
    configured: BTreeMap<String, NamedAuthProfile>,
    default_profiles_root: &Path,
) -> Vec<NamedAuthProfile> {
    let mut merged = configured;
    for provider in [
        NativeLoginProvider::Agy,
        NativeLoginProvider::ClaudeOauth,
        NativeLoginProvider::CodexOauth,
    ] {
        let name = provider.default_profile_name().to_string();
        merged
            .entry(name.clone())
            .or_insert_with(|| NamedAuthProfile {
                profile: AuthProfile::NativeLogin(NativeLoginProfile {
                    provider,
                    profile_dir: default_profiles_root.join(&name),
                }),
                name,
                source: ProfileSource::Default,
                tiers: ModelTier::ALL.to_vec(),
            });
    }
    merged.into_values().collect()
}

fn toml_basic_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(&mut out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

enum TomlHeader<'a> {
    Table(&'a str),
    ArrayOfTables,
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut escaped = false;
    let mut in_single = false;
    for (idx, ch) in line.char_indices() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
        } else if in_single {
            if ch == '\'' {
                in_single = false;
            }
        } else {
            match ch {
                '#' => return &line[..idx],
                '"' => in_double = true,
                '\'' => in_single = true,
                _ => {}
            }
        }
    }
    line
}

fn parse_toml_header(trimmed_line: &str) -> Option<TomlHeader<'_>> {
    if !trimmed_line.starts_with('[') {
        return None;
    }
    let code = strip_toml_comment(trimmed_line).trim_end();
    if let Some(inner) = code.strip_prefix("[[").and_then(|s| s.strip_suffix("]]"))
        && !inner.trim().is_empty()
    {
        return Some(TomlHeader::ArrayOfTables);
    }
    code.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(TomlHeader::Table)
}

fn belongs_to_profile_section(section: &str, name: &str) -> bool {
    let target = format!("auth.profiles.{name}");
    section == target || section.starts_with(&format!("{target}."))
}

fn flush_pending_comments<'a>(kept: &mut Vec<&'a str>, pending: &mut Vec<&'a str>) {
    if kept.is_empty() {
        while pending.first().is_some_and(|l| l.trim().is_empty()) {
            pending.remove(0);
        }
    }
    kept.append(pending);
}

/// Upsert `[auth.profiles.<name>]` in `existing` TOML while preserving all unrelated
/// `[tables]`, `[[array.of.tables]]`, and comment blocks intact.
pub(crate) fn upsert_relay_profile_toml(
    existing: &str,
    name: &str,
    relay: &RelayProfile,
    tiers: &[ModelTier],
    profiles_root: &Path,
    home: Option<&Path>,
) -> Result<String, String> {
    let valid_name = validate_profile_name(name)?;
    let _ = validate_endpoint_url(&relay.base_url)?;
    if relay.engines.is_empty() {
        return Err("relay profile must target at least one engine".to_string());
    }

    let mut kept_lines: Vec<&str> = Vec::new();
    let mut pending_comments: Vec<&str> = Vec::new();
    let mut skip_section = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        if let Some(header) = parse_toml_header(trimmed) {
            let is_target = match header {
                TomlHeader::Table(section) => belongs_to_profile_section(section, &valid_name),
                TomlHeader::ArrayOfTables => false,
            };
            if is_target {
                pending_comments.clear();
                skip_section = true;
                continue;
            }
            if skip_section {
                skip_section = false;
                flush_pending_comments(&mut kept_lines, &mut pending_comments);
            }
            kept_lines.push(line);
            continue;
        }
        if skip_section && trimmed.starts_with('[') {
            skip_section = false;
            flush_pending_comments(&mut kept_lines, &mut pending_comments);
            kept_lines.push(line);
            continue;
        }
        if skip_section {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                pending_comments.push(line);
            } else {
                pending_comments.clear();
            }
        } else {
            kept_lines.push(line);
        }
    }
    if skip_section && pending_comments.iter().any(|l| !l.trim().is_empty()) {
        flush_pending_comments(&mut kept_lines, &mut pending_comments);
    }
    while kept_lines.last().is_some_and(|line| line.trim().is_empty()) {
        kept_lines.pop();
    }

    let engines = relay
        .engines
        .iter()
        .map(|e| toml_basic_string(e.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let tier_list = tiers
        .iter()
        .map(|t| toml_basic_string(t.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let secret_line = match &relay.api_key {
        SecretRef::FromEnv { from_env } => format!("from_env = {}", toml_basic_string(from_env)),
        SecretRef::FromFile { from_file } => {
            format!(
                "from_file = {}",
                toml_basic_string(&from_file.display().to_string())
            )
        }
    };

    let mut out = if kept_lines.is_empty() {
        String::new()
    } else {
        let mut s = kept_lines.join("\n");
        s.push_str("\n\n");
        s
    };
    let _ = write!(
        &mut out,
        "[auth.profiles.{valid_name}]\nmode = \"relay\"\nbase_url = {}\nengines = [{engines}]\ntiers = [{tier_list}]\n\n[auth.profiles.{valid_name}.api_key]\n{secret_line}\n",
        toml_basic_string(&relay.base_url)
    );

    parse_auth_profiles_toml(&out, profiles_root, home).map_err(|error| {
        format!("upsert `{valid_name}` conflicts with existing TOML structure: {error}")
    })?;
    Ok(out)
}

fn write_text_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create auth config dir {}: {error}", parent.display()))?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auth.toml");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{nanos}", std::process::id()));

    let write_res = (|| -> Result<(), String> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_path)
            .map_err(|error| format!("create temp auth config {}: {error}", tmp_path.display()))?;
        file.write_all(content.as_bytes())
            .map_err(|error| format!("write temp auth config {}: {error}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync temp auth config {}: {error}", tmp_path.display()))?;
        drop(file);
        fs::rename(&tmp_path, path).map_err(|error| {
            format!(
                "rename temp auth config {} -> {}: {error}",
                tmp_path.display(),
                path.display()
            )
        })
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

/// Closed serde-tagged secret reference view (never contains plaintext secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RedactedSecretSource {
    FromEnv { var: String },
    FromFile { path: String },
}

/// Closed serde-tagged Cloud IAM metadata view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub(crate) enum RedactedCloudMeta {
    Vertex {
        region: String,
        project: Option<String>,
        service_account: Option<String>,
    },
    Bedrock {
        region: String,
        aws_profile: Option<String>,
    },
    Azure {
        region: String,
        resource: Option<String>,
        tenant_id: Option<String>,
    },
}

/// Closed serde-tagged mode-specific payload replacing loose `Option` bags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum RedactedProfileDetails {
    Relay {
        base_url: String,
        secret_source: RedactedSecretSource,
    },
    NativeLogin {
        provider: NativeLoginProvider,
        isolated_dir: String,
    },
    CloudIam {
        base_url: Option<String>,
        cloud: RedactedCloudMeta,
    },
    Local {
        local_provider: LocalProvider,
        base_url: String,
        local_auth: &'static str,
    },
}

impl RedactedProfileDetails {
    const fn mode_str(&self) -> &'static str {
        match self {
            Self::Relay { .. } => "relay",
            Self::NativeLogin { .. } => "native_login",
            Self::CloudIam { .. } => "cloud_iam",
            Self::Local { .. } => "local",
        }
    }

    fn isolated_dir(&self) -> Option<&str> {
        match self {
            Self::NativeLogin { isolated_dir, .. } => Some(isolated_dir.as_str()),
            Self::Relay { .. } | Self::CloudIam { .. } | Self::Local { .. } => None,
        }
    }
}

/// Redacted metadata entry returned by `list` and `check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RedactedProfileView {
    pub(crate) name: String,
    pub(crate) source: ProfileSource,
    pub(crate) engines: Vec<Engine>,
    pub(crate) tiers: Vec<ModelTier>,
    pub(crate) readiness: ProfileReadiness,
    pub(crate) detail: String,
    pub(crate) warnings: Vec<String>,
    #[serde(flatten)]
    pub(crate) details: RedactedProfileDetails,
    pub(crate) env_preview: BTreeMap<&'static str, BTreeMap<String, String>>,
}

/// Summary counters for `list` and `check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AuthSummary {
    pub(crate) total: usize,
    pub(crate) configured: usize,
    pub(crate) ready: usize,
    pub(crate) missing: usize,
    pub(crate) reauth_required: usize,
    pub(crate) configured_ready: usize,
}

/// Full redacted inventory/preflight report for `spawn auth list` and `spawn auth check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AuthReport {
    pub(crate) schema_version: u32,
    pub(crate) config_path: String,
    pub(crate) config_exists: bool,
    pub(crate) profiles_root_dir: String,
    pub(crate) ok: bool,
    pub(crate) summary: AuthSummary,
    pub(crate) profiles: Vec<RedactedProfileView>,
}

fn secret_source_redacted(secret: &SecretRef) -> RedactedSecretSource {
    match secret {
        SecretRef::FromEnv { from_env } => RedactedSecretSource::FromEnv {
            var: from_env.clone(),
        },
        SecretRef::FromFile { from_file } => RedactedSecretSource::FromFile {
            path: from_file.display().to_string(),
        },
    }
}

fn redacted_env_preview(
    named: &NamedAuthProfile,
) -> BTreeMap<&'static str, BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    match &named.profile {
        AuthProfile::Relay(relay) => {
            for &engine in &relay.engines {
                if let Ok(plan) = relay.translate_env(engine, "<redacted>") {
                    map.insert(engine.as_str(), plan.masked_summary());
                }
            }
        }
        AuthProfile::NativeLogin(native) => {
            map.insert(
                native.provider.engine().as_str(),
                native.translate_env(&named.name).masked_summary(),
            );
        }
        AuthProfile::CloudIam(cloud) => {
            for &engine in &cloud.engines() {
                if let Ok(plan) = cloud.translate_env(engine) {
                    map.insert(engine.as_str(), plan.masked_summary());
                }
            }
        }
        AuthProfile::Local(local) => {
            for &engine in &local.engines {
                if let Ok(plan) = local.translate_env(engine) {
                    map.insert(engine.as_str(), plan.masked_summary());
                }
            }
        }
    }
    map
}

fn redacted_cloud_meta(cloud: &CloudIamProfile) -> RedactedCloudMeta {
    match cloud {
        CloudIamProfile::Vertex {
            region,
            project,
            service_account,
            ..
        } => RedactedCloudMeta::Vertex {
            region: region.clone(),
            project: project.clone(),
            service_account: service_account.clone(),
        },
        CloudIamProfile::Bedrock {
            region,
            aws_profile,
            ..
        } => RedactedCloudMeta::Bedrock {
            region: region.clone(),
            aws_profile: aws_profile.clone(),
        },
        CloudIamProfile::Azure {
            region,
            resource,
            tenant_id,
            ..
        } => RedactedCloudMeta::Azure {
            region: region.clone(),
            resource: resource.clone(),
            tenant_id: tenant_id.clone(),
        },
    }
}

fn redact_profile_view(named: &NamedAuthProfile, probe: ProbeOutcome) -> RedactedProfileView {
    let env_preview = redacted_env_preview(named);
    let details = match &named.profile {
        AuthProfile::Relay(relay) => RedactedProfileDetails::Relay {
            base_url: relay.base_url.clone(),
            secret_source: secret_source_redacted(&relay.api_key),
        },
        AuthProfile::NativeLogin(native) => RedactedProfileDetails::NativeLogin {
            provider: native.provider,
            isolated_dir: native.profile_dir.display().to_string(),
        },
        AuthProfile::CloudIam(cloud) => RedactedProfileDetails::CloudIam {
            base_url: cloud.endpoint().map(str::to_string),
            cloud: redacted_cloud_meta(cloud),
        },
        AuthProfile::Local(local) => RedactedProfileDetails::Local {
            local_provider: local.provider,
            base_url: local.base_url.clone(),
            local_auth: "none",
        },
    };

    RedactedProfileView {
        name: named.name.clone(),
        source: named.source,
        engines: named.profile.engines(),
        tiers: named.tiers.clone(),
        readiness: probe.readiness,
        detail: probe.detail,
        warnings: probe.warnings,
        details,
        env_preview,
    }
}

/// Evaluate all configured and default profiles into a redacted [`AuthReport`].
pub(crate) fn evaluate_profiles_report_with(
    config_path: &Path,
    profiles_root: &Path,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    file_reader: &dyn Fn(&Path) -> Option<String>,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> Result<AuthReport, String> {
    let validated_root = validate_profiles_root_dir(profiles_root, home)?;
    let config_exists = config_path.is_file();
    let configured = if config_exists {
        let raw = fs::read_to_string(config_path)
            .map_err(|error| format!("read auth config {}: {error}", config_path.display()))?;
        parse_auth_profiles_toml(&raw, &validated_root, home)?
    } else {
        BTreeMap::new()
    };

    let all_profiles = merge_with_default_profiles(configured, &validated_root);
    let mut views = Vec::with_capacity(all_profiles.len());
    let (mut ready, mut missing, mut reauth_required) = (0usize, 0usize, 0usize);
    let (mut configured_count, mut configured_ready) = (0usize, 0usize);

    for named in &all_profiles {
        let probe = probe_profile_with(
            &named.profile,
            &validated_root,
            env_lookup,
            file_exists,
            file_reader,
            home,
            now_unix_secs,
        );
        match probe.readiness {
            ProfileReadiness::Ready => {
                ready += 1;
                if named.source == ProfileSource::Configured {
                    configured_ready += 1;
                }
            }
            ProfileReadiness::Missing => missing += 1,
            ProfileReadiness::ReauthRequired => reauth_required += 1,
        }
        if named.source == ProfileSource::Configured {
            configured_count += 1;
        }
        views.push(redact_profile_view(named, probe));
    }

    let ok = if configured_count > 0 {
        configured_ready == configured_count && reauth_required == 0
    } else {
        ready > 0 && reauth_required == 0
    };

    Ok(AuthReport {
        schema_version: 1,
        config_path: config_path.display().to_string(),
        config_exists,
        profiles_root_dir: validated_root.display().to_string(),
        ok,
        summary: AuthSummary {
            total: views.len(),
            configured: configured_count,
            ready,
            missing,
            reauth_required,
            configured_ready,
        },
        profiles: views,
    })
}

fn format_report_text(report: &AuthReport, heading: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "{heading}: config={} (exists={}) profiles_root={}",
        report.config_path, report.config_exists, report.profiles_root_dir
    );
    let _ = writeln!(
        &mut out,
        "summary: total={} configured={} ready={} missing={} reauth_required={} ok={}",
        report.summary.total,
        report.summary.configured,
        report.summary.ready,
        report.summary.missing,
        report.summary.reauth_required,
        report.ok
    );
    for p in &report.profiles {
        let engines = format_engine_list(&p.engines);
        let tiers = p
            .tiers
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let isolated = p.details.isolated_dir().unwrap_or("-");
        let src = match p.source {
            ProfileSource::Configured => "configured",
            ProfileSource::Default => "default",
        };
        let _ = writeln!(
            &mut out,
            "- {} [{}|{}] mode={} engines=[{}] tiers=[{}] isolated_dir={} — {}",
            p.name,
            p.readiness.as_str(),
            src,
            p.details.mode_str(),
            engines,
            tiers,
            isolated,
            p.detail
        );
        for warning in &p.warnings {
            let _ = writeln!(&mut out, "  warning: {warning}");
        }
    }
    out
}

#[derive(Debug, Default)]
struct ParsedAuthArgs {
    positional: Option<String>,
    values: BTreeMap<String, String>,
    switches: BTreeSet<String>,
}

fn parse_auth_args(
    args: &[String],
    positional_label: Option<&str>,
    allowed_values: &[&str],
    allowed_switches: &[&str],
) -> Result<ParsedAuthArgs, String> {
    let mut parsed = ParsedAuthArgs::default();
    let mut iter = args.iter().peekable();
    while let Some(word) = iter.next() {
        if let Some(flag_body) = word.strip_prefix("--") {
            let (name, inline_val) = match flag_body.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (flag_body, None),
            };
            if FORBIDDEN_PLAINTEXT_FLAGS.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(format!("flag `--{name}` is forbidden: {PLAINTEXT_ERR}"));
            }
            if allowed_switches.contains(&name) {
                if inline_val.is_some() {
                    return Err(format!("--{name} takes no value"));
                }
                parsed.switches.insert(name.to_string());
                continue;
            }
            if !allowed_values.contains(&name) {
                return Err(format!("unknown flag --{name}"));
            }
            let val = if let Some(v) = inline_val {
                if v.is_empty() {
                    return Err(format!("--{name} needs a value"));
                }
                v.to_string()
            } else {
                let Some(next) = iter.next() else {
                    return Err(format!("--{name} needs a value"));
                };
                if next.starts_with("--") {
                    return Err(format!("--{name} needs a value, found {next}"));
                }
                next.clone()
            };
            if parsed.values.insert(name.to_string(), val).is_some() {
                return Err(format!("duplicate flag --{name}"));
            }
            continue;
        }
        if word.starts_with('-') {
            return Err(format!("unknown flag {word}"));
        }
        if let Some(label) = positional_label {
            if parsed.positional.is_some() {
                return Err(format!("unexpected extra argument `{word}`"));
            }
            parsed.positional = Some(word.clone());
            let _ = label;
        } else {
            return Err(format!("unexpected argument `{word}`"));
        }
    }
    if let Some(label) = positional_label
        && parsed.positional.is_none()
    {
        return Err(format!("missing required positional argument {label}"));
    }
    Ok(parsed)
}

fn parse_comma_items<T>(
    raw: Option<&str>,
    parser: impl Fn(&str) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let Some(text) = raw else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for part in text.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err("empty item in comma-separated list".to_string());
        }
        items.push(parser(trimmed)?);
    }
    Ok(items)
}

fn effective_explicit_config(cli_config: Option<&str>, parsed: &ParsedAuthArgs) -> Option<PathBuf> {
    parsed
        .values
        .get("config")
        .map(PathBuf::from)
        .or_else(|| cli_config.map(PathBuf::from))
}

fn process_home() -> Option<PathBuf> {
    config::env_value("HOME").map(PathBuf::from)
}

fn read_optional_file(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Dispatch `spawn auth <verb> ...` without global `--config` override.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    command_with_config(None, rest)
}

/// Dispatch `spawn auth <verb> ...`, honoring an optional leading `spawn --config <path>`.
pub(crate) fn command_with_config(
    cli_config: Option<&str>,
    rest: &[String],
) -> Result<i32, String> {
    let Some((verb, tail)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    match verb.as_str() {
        "list" => run_list_or_check(cli_config, tail, false),
        "check" => run_list_or_check(cli_config, tail, true),
        "add-relay" => run_add_relay(cli_config, tail),
        "login" => run_login(cli_config, tail),
        other => Err(format!("unknown auth subcommand `{other}`\n{USAGE}")),
    }
}

fn run_list_or_check(
    cli_config: Option<&str>,
    args: &[String],
    fail_on_unready: bool,
) -> Result<i32, String> {
    let parsed = parse_auth_args(args, None, &["config"], &["json"])?;
    let explicit = effective_explicit_config(cli_config, &parsed);
    let config_path = config::auth_config_read_path(explicit.as_deref());
    let profiles_root = config::auth_profiles_root_dir()?;
    let home = process_home();
    let report = evaluate_profiles_report_with(
        &config_path,
        &profiles_root,
        &|name| config::env_value(name),
        &Path::exists,
        &read_optional_file,
        home.as_deref(),
        current_unix_secs(),
    )?;

    if parsed.switches.contains("json") {
        let serialized = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize auth JSON report: {error}"))?;
        println!("{serialized}");
    } else {
        let heading = if fail_on_unready {
            "spawn auth check"
        } else {
            "spawn auth list"
        };
        print!("{}", format_report_text(&report, heading));
    }
    Ok(i32::from(fail_on_unready && !report.ok))
}

fn run_add_relay(cli_config: Option<&str>, args: &[String]) -> Result<i32, String> {
    let parsed = parse_auth_args(
        args,
        Some("<name>"),
        &["url", "key-env", "key-file", "engines", "tiers", "config"],
        &["json"],
    )?;
    let name = validate_profile_name(parsed.positional.as_deref().unwrap_or_default())?;
    let url = parsed
        .values
        .get("url")
        .ok_or_else(|| "--url is required".to_string())?;
    let base_url = validate_endpoint_url(url)?;

    let api_key = match (parsed.values.get("key-env"), parsed.values.get("key-file")) {
        (Some(_), Some(_)) => {
            return Err("specify either --key-env or --key-file, not both".to_string());
        }
        (Some(var), None) => SecretRef::FromEnv {
            from_env: validate_env_var_name(var)?,
        },
        (None, Some(path)) => SecretRef::FromFile {
            from_file: normalize_secret_file_path(Path::new(path))?,
        },
        (None, None) => {
            return Err(format!(
                "either --key-env or --key-file is required; {PLAINTEXT_ERR}"
            ));
        }
    };

    let parsed_engines = parse_comma_items(
        parsed.values.get("engines").map(String::as_str),
        RelayEngine::parse,
    )?;
    let engines: BTreeSet<RelayEngine> = if parsed_engines.is_empty() {
        [RelayEngine::Claude, RelayEngine::Codex]
            .into_iter()
            .collect()
    } else {
        parsed_engines.into_iter().collect()
    };
    let tiers = normalize_tiers(parse_comma_items(
        parsed.values.get("tiers").map(String::as_str),
        ModelTier::parse,
    )?);

    let relay = RelayProfile {
        base_url,
        api_key: api_key.clone(),
        engines,
    };
    let explicit = effective_explicit_config(cli_config, &parsed);
    let target_path = config::auth_config_write_path(explicit.as_deref());
    let existing = if target_path.exists() {
        fs::read_to_string(&target_path).map_err(|error| {
            format!(
                "read existing auth config {}: {error}",
                target_path.display()
            )
        })?
    } else {
        String::new()
    };
    let profiles_root = config::auth_profiles_root_dir()?;
    let home = process_home();
    let updated = upsert_relay_profile_toml(
        &existing,
        &name,
        &relay,
        &tiers,
        &profiles_root,
        home.as_deref(),
    )?;
    write_text_atomic(&target_path, &updated)?;
    print_add_relay_output(
        &name,
        &target_path,
        &relay,
        &tiers,
        parsed.switches.contains("json"),
    )?;
    Ok(0)
}

fn print_add_relay_output(
    name: &str,
    target_path: &Path,
    relay: &RelayProfile,
    tiers: &[ModelTier],
    json_output: bool,
) -> Result<(), String> {
    if json_output {
        let payload = json!({
            "schema_version": 1,
            "action": "add-relay",
            "profile": name,
            "mode": "relay",
            "config_path": target_path.display().to_string(),
            "base_url": relay.base_url,
            "engines": relay.engines,
            "tiers": tiers,
            "secret_source": secret_source_redacted(&relay.api_key),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|error| format!("serialize add-relay output: {error}"))?
        );
    } else {
        let engine_names: Vec<&str> = relay.engines.iter().map(|e| e.as_str()).collect();
        let source = match &relay.api_key {
            SecretRef::FromEnv { from_env } => format!("from_env:{from_env}"),
            SecretRef::FromFile { from_file } => format!("from_file:{}", from_file.display()),
        };
        println!(
            "saved relay auth profile `{name}` in {} (url={}, engines=[{}], secret={source})",
            target_path.display(),
            relay.base_url,
            engine_names.join(",")
        );
    }
    Ok(())
}

fn create_private_dir_only_if_new(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(format!(
                    "isolated auth path {} exists as symlink or non-directory",
                    path.display()
                ));
            }
            // Never chmod pre-existing directories.
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => Ok(()),
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let meta = fs::symlink_metadata(path).map_err(|e| {
                        format!("inspect isolated auth path {}: {e}", path.display())
                    })?;
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        return Err(format!(
                            "isolated auth path {} exists as symlink or non-directory",
                            path.display()
                        ));
                    }
                    Ok(())
                }
                Err(create_err) => Err(format!(
                    "create isolated auth dir {}: {create_err}",
                    path.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "stat isolated auth path {}: {error}",
            path.display()
        )),
    }
}

/// Create `profiles_root` -> `dir` -> `xdg-config`/`xdg-data` strictly inside `profiles_root`,
/// setting `0o700` ONLY on newly created directories (never `chmod`ing existing dirs).
pub(crate) fn ensure_isolated_dirs(
    dir: &Path,
    profiles_root: &Path,
    home: Option<&Path>,
) -> Result<(), String> {
    let valid_dir = validate_isolated_profile_dir(dir, profiles_root, home)?;
    let valid_root = validate_profiles_root_dir(profiles_root, home)?;
    if let Some(root_parent) = valid_root.parent()
        && !root_parent.as_os_str().is_empty()
        && !root_parent.exists()
    {
        fs::create_dir_all(root_parent).map_err(|error| {
            format!(
                "create parent of auth profiles root {}: {error}",
                root_parent.display()
            )
        })?;
    }
    create_private_dir_only_if_new(&valid_root)?;
    let rel = valid_dir.strip_prefix(&valid_root).map_err(|_| {
        format!(
            "profile_dir {} escapes {}",
            valid_dir.display(),
            valid_root.display()
        )
    })?;
    let mut cur = valid_root;
    for component in rel.components() {
        cur.push(component);
        create_private_dir_only_if_new(&cur)?;
    }
    create_private_dir_only_if_new(&valid_dir.join("xdg-config"))?;
    create_private_dir_only_if_new(&valid_dir.join("xdg-data"))?;
    Ok(())
}

fn run_login(cli_config: Option<&str>, args: &[String]) -> Result<i32, String> {
    let parsed = parse_auth_args(args, Some("<engine>"), &["profile", "config"], &[])?;
    let provider =
        NativeLoginProvider::parse_provider(parsed.positional.as_deref().unwrap_or_default())?;
    let explicit = effective_explicit_config(cli_config, &parsed);
    let config_path = config::auth_config_read_path(explicit.as_deref());
    let profiles_root = config::auth_profiles_root_dir()?;
    let home = process_home();
    let configured = if config_path.is_file() {
        let raw = fs::read_to_string(&config_path)
            .map_err(|error| format!("read auth config {}: {error}", config_path.display()))?;
        parse_auth_profiles_toml(&raw, &profiles_root, home.as_deref())?
    } else {
        BTreeMap::new()
    };

    let (profile_name, profile_dir) = resolve_login_target(
        provider,
        parsed.values.get("profile").map(String::as_str),
        &configured,
        &profiles_root,
    )?;
    let plan = plan_login_command(
        provider,
        &profile_name,
        &profile_dir,
        &profiles_root,
        home.as_deref(),
        &|name| config::env_value(name),
    )?;
    ensure_isolated_dirs(&plan.profile_dir, &profiles_root, home.as_deref())?;

    println!(
        "launching isolated `{}` login for profile `{}` in {}",
        plan.engine,
        plan.profile,
        plan.profile_dir.display()
    );

    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    for key in &plan.env.unset {
        cmd.env_remove(key);
    }
    for (key, value) in &plan.env.set {
        cmd.env(key, value);
    }
    let status = cmd.status().map_err(|error| {
        format!(
            "failed to start `{}` for isolated login (`{}`): {error}",
            plan.program, plan.profile
        )
    })?;
    Ok(status.code().unwrap_or(1))
}

fn resolve_login_target(
    provider: NativeLoginProvider,
    requested_profile: Option<&str>,
    configured: &BTreeMap<String, NamedAuthProfile>,
    profiles_root: &Path,
) -> Result<(String, PathBuf), String> {
    if let Some(raw_name) = requested_profile {
        let valid_name = validate_profile_name(raw_name)?;
        if let Some(existing) = configured.get(&valid_name) {
            return match &existing.profile {
                AuthProfile::NativeLogin(native) if native.provider == provider => {
                    Ok((existing.name.clone(), native.profile_dir.clone()))
                }
                AuthProfile::NativeLogin(native) => Err(format!(
                    "profile `{valid_name}` is configured for `{}`, not `{}`",
                    native.provider.as_str(),
                    provider.as_str()
                )),
                other => Err(format!(
                    "profile `{valid_name}` uses mode `{}`; `spawn auth login` requires `native_login`",
                    other.mode_str()
                )),
            };
        }
        return Ok((valid_name.clone(), profiles_root.join(&valid_name)));
    }

    if let Some(matched) = configured.values().find(|entry| {
        matches!(&entry.profile, AuthProfile::NativeLogin(native) if native.provider == provider)
    }) && let AuthProfile::NativeLogin(native) = &matched.profile
    {
        return Ok((matched.name.clone(), native.profile_dir.clone()));
    }

    let default_name = provider.default_profile_name();
    Ok((default_name.to_string(), profiles_root.join(default_name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-auth-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn b1_rejects_all_url_queries_fragments_semicolons_and_userinfo_without_echo() {
        let canary = "sk-b1-canary-never-leak-7741";
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let bad_urls = [
            format!("https://gw.example/v1?api-key={canary}"),
            format!("https://gw.example/v1?x-api-key={canary}"),
            format!("https://gw.example/v1?sig={canary}"),
            format!("https://gw.example/v1?auth={canary}"),
            format!("https://gw.example/v1?api_key={canary}"),
            format!("https://gw.example/v1?benign=1&foo={canary}"),
            format!("https://gw.example/v1;key={canary}"),
            format!("https://gw.example/v1#token={canary}"),
            format!("HTTPS://user:{canary}@gw.example/v1"),
            format!("ftp://user:{canary}@gw.example/v1"),
            format!("https://gw.example\\path\\{canary}"),
            format!("https://gw.example/v1%3Fapi-key={canary}"),
            format!("https://gw.example/v1%3fapi-key={canary}"),
            format!("https://gw.example/v1&api-key={canary}"),
            format!("https://gw.example/v1%23token={canary}"),
            format!("https://user%3A{canary}%40gw.example/v1"),
            "https://gw.example/v1\u{200b}".to_string(),
            format!("https://gw.example/v1\u{200b}{canary}"),
        ];
        for bad in &bad_urls {
            let err = validate_endpoint_url(bad).unwrap_err();
            assert!(!err.contains(canary), "canary leaked in URL error: {err}");
            let cli_err = command(&args(&[
                "add-relay",
                "leaky",
                "--url",
                bad,
                "--key-env",
                "GW_KEY",
            ]))
            .unwrap_err();
            assert!(
                !cli_err.contains(canary),
                "canary leaked in CLI error: {cli_err}"
            );
        }
        assert_eq!(
            validate_endpoint_url("HTTPS://gw.example/v1/"),
            Ok("https://gw.example/v1".to_string())
        );
    }

    #[test]
    fn b2_upsert_preserves_array_of_tables_and_comments_following_replaced_profile() {
        let initial = r#"# top file comment
[auth.profiles.foo]
mode = "relay"
base_url = "https://old.example/v1"
engines = ["claude"]
[auth.profiles.foo.api_key]
from_env = "OLD_KEY"

# keep this comment above array of tables
[[hooks]]
cmd = "keep-me"

[[writable_mounts]]
source = "/srv/work"
target = "/work"

# trailing server comment
[server]
port = 8080
"#;
        let updated = upsert_relay_profile_toml(
            initial,
            "foo",
            &RelayProfile {
                base_url: "https://new.example/v1".to_string(),
                api_key: SecretRef::FromEnv {
                    from_env: "NEW_KEY".to_string(),
                },
                engines: [RelayEngine::Claude, RelayEngine::Codex]
                    .into_iter()
                    .collect(),
            },
            &ModelTier::ALL,
            Path::new("/var/lib/spawn/auth-profiles"),
            None,
        )
        .expect("upsert preserving hooks");

        assert!(updated.contains("# top file comment"), "{updated}");
        assert!(
            updated.contains("# keep this comment above array of tables"),
            "{updated}"
        );
        assert!(updated.contains("[[hooks]]"), "{updated}");
        assert!(updated.contains("cmd = \"keep-me\""), "{updated}");
        assert!(updated.contains("[[writable_mounts]]"), "{updated}");
        assert!(updated.contains("# trailing server comment"), "{updated}");
        assert!(updated.contains("[server]"), "{updated}");
        assert!(!updated.contains("OLD_KEY"), "{updated}");
        assert!(updated.contains("NEW_KEY"), "{updated}");

        let doc = toml_parser::parse(&updated).expect("valid TOML");
        assert_eq!(doc["hooks"][0]["cmd"], json!("keep-me"));
        assert_eq!(doc["writable_mounts"][0]["target"], json!("/work"));
        assert_eq!(doc["server"]["port"], json!(8080));
    }

    #[test]
    fn r1_upsert_honors_resolved_profiles_root_with_explicit_native_profile_dir() {
        let scratch = scratch_dir("r1-explicit-native-dir");
        let fake_home = scratch.join("home");
        let profiles_root = fake_home.join(".local/state/spawn/auth-profiles");
        fs::create_dir_all(&profiles_root).expect("profiles_root");
        let native_dir = profiles_root.join("claude-work");

        let existing = format!(
            r#"[auth.profiles.claude-work]
mode = "native_login"
provider = "claude"
profile_dir = "{pdir}"
"#,
            pdir = native_dir.display()
        );
        let relay = RelayProfile {
            base_url: "https://relay.example/v1".to_string(),
            api_key: SecretRef::FromEnv {
                from_env: "RELAY_KEY".to_string(),
            },
            engines: [RelayEngine::Claude, RelayEngine::Codex]
                .into_iter()
                .collect(),
        };

        let updated = upsert_relay_profile_toml(
            &existing,
            "r1",
            &relay,
            &ModelTier::ALL,
            &profiles_root,
            Some(&fake_home),
        )
        .expect("upsert with explicit native profile_dir inside profiles_root");

        let parsed =
            parse_auth_profiles_toml(&updated, &profiles_root, Some(&fake_home)).expect("valid");
        assert!(parsed.contains_key("claude-work"));
        assert!(parsed.contains_key("r1"));

        let escaping = r#"[auth.profiles.bad-native]
mode = "native_login"
provider = "claude"
profile_dir = "/tmp"
"#;
        assert!(
            upsert_relay_profile_toml(
                escaping,
                "r1",
                &relay,
                &ModelTier::ALL,
                &profiles_root,
                Some(&fake_home),
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn r2_upsert_preserves_quoted_hash_table_headers_and_fails_closed_on_malformed_headers() {
        let initial = r#"[auth.profiles.x]
mode = "relay"
base_url = "https://old.example/v1"
engines = ["claude"]
[auth.profiles.x.api_key]
from_env = "OLD_KEY"

# comment above quoted-hash table
[labels."a#b"]
keep_me = 1
also_keep = 2

[labels.'c#d'] # inline comment
stay = true

[[items."e#f"]]
name = "ok"
"#;
        let relay = RelayProfile {
            base_url: "https://new.example/v1".to_string(),
            api_key: SecretRef::FromEnv {
                from_env: "NEW_KEY".to_string(),
            },
            engines: [RelayEngine::Claude].into_iter().collect(),
        };
        let root = Path::new("/var/lib/spawn/auth-profiles");
        let updated = upsert_relay_profile_toml(initial, "x", &relay, &ModelTier::ALL, root, None)
            .expect("upsert preserving quoted-hash sections");

        assert!(
            updated.contains("# comment above quoted-hash table"),
            "{updated}"
        );
        assert!(updated.contains(r#"[labels."a#b"]"#), "{updated}");
        assert!(updated.contains("keep_me = 1"), "{updated}");
        assert!(updated.contains("also_keep = 2"), "{updated}");
        assert!(
            updated.contains(r"[labels.'c#d'] # inline comment"),
            "{updated}"
        );
        assert!(updated.contains("stay = true"), "{updated}");
        assert!(updated.contains(r#"[[items."e#f"]]"#), "{updated}");
        assert!(!updated.contains("OLD_KEY"), "{updated}");

        let doc = toml_parser::parse(&updated).expect("valid TOML");
        assert_eq!(doc["labels"]["a#b"]["keep_me"], json!(1));
        assert_eq!(doc["labels"]["a#b"]["also_keep"], json!(2));
        assert_eq!(doc["labels"]["c#d"]["stay"], json!(true));
        assert_eq!(doc["items"]["e#f"][0]["name"], json!("ok"));

        let malformed_following = r#"[auth.profiles.x]
mode = "relay"
base_url = "https://old.example/v1"
engines = ["claude"]
[auth.profiles.x.api_key]
from_env = "OLD_KEY"

[unclosed
keep_me = 1
"#;
        assert!(
            upsert_relay_profile_toml(
                malformed_following,
                "x",
                &relay,
                &ModelTier::ALL,
                root,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn b3_enforces_strict_containment_and_never_chmods_existing_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = scratch_dir("b3-isolation");
        let fake_home = scratch.join("home");
        let profiles_root = scratch.join("controlled/auth-profiles");
        fs::create_dir_all(fake_home.join(".ssh")).expect("ssh");
        fs::create_dir_all(fake_home.join(".aws")).expect("aws");
        fs::create_dir_all(fake_home.join(".config/gcloud")).expect("gcloud");
        fs::create_dir_all(fake_home.join(".claude")).expect("claude");
        fs::create_dir_all(&profiles_root).expect("root");

        let external_dir = scratch.join("external-0755");
        fs::create_dir_all(&external_dir).expect("external");
        fs::set_permissions(&external_dir, fs::Permissions::from_mode(0o755)).expect("chmod 755");

        for home in [Some(fake_home.as_path()), None] {
            for blocked in [
                PathBuf::from("/"),
                PathBuf::from("/etc"),
                PathBuf::from("/tmp"),
                PathBuf::from("/home"),
                external_dir.clone(),
                profiles_root.clone(),
                fake_home.join(".ssh"),
                fake_home.join(".aws"),
                fake_home.join(".config"),
                fake_home.join(".config/gcloud"),
                fake_home.join(".claude"),
                PathBuf::from("/home/other/.claude"),
                profiles_root.join("../escape"),
            ] {
                assert!(
                    validate_isolated_profile_dir(&blocked, &profiles_root, home).is_err(),
                    "must reject {} with home={home:?}",
                    blocked.display()
                );
            }
        }

        // Symlink inside profiles_root pointing to ~/.ssh or /tmp must fail closed.
        let escape_link = profiles_root.join("escape-ssh");
        std::os::unix::fs::symlink(fake_home.join(".ssh"), &escape_link).expect("symlink");
        assert!(
            validate_isolated_profile_dir(&escape_link, &profiles_root, Some(&fake_home)).is_err()
        );

        // ensure_isolated_dirs refuses external dir without touching its 0755 permissions.
        assert!(ensure_isolated_dirs(&external_dir, &profiles_root, Some(&fake_home)).is_err());
        let mode_after = fs::metadata(&external_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_after, 0o755);

        // Pre-existing dir under profiles_root keeps its existing permissions; new dir gets 0700.
        let pre_dir = profiles_root.join("pre-existing");
        fs::create_dir_all(&pre_dir).expect("pre_dir");
        fs::set_permissions(&pre_dir, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        ensure_isolated_dirs(&pre_dir, &profiles_root, Some(&fake_home)).expect("ok pre_dir");
        assert_eq!(
            fs::metadata(&pre_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let brand_new = profiles_root.join("fresh-profile");
        ensure_isolated_dirs(&brand_new, &profiles_root, Some(&fake_home)).expect("create fresh");
        assert_eq!(
            fs::metadata(&brand_new).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn n1_never_echoes_mispasted_secrets_in_cli_or_toml_errors() {
        let canary = "sk-n1-mispasted-secret-99881";
        let root = Path::new("/var/lib/spawn/auth-profiles");
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        for cli in [
            args(&[
                "add-relay",
                "r",
                "--url",
                "https://gw.example",
                "--key-env",
                canary,
            ]),
            args(&[
                "add-relay",
                "r",
                "--url",
                "https://gw.example",
                "--key-file",
                canary,
            ]),
            args(&[
                "add-relay",
                "r",
                "--url",
                &format!("HTTPS://u:{canary}@gw.example"),
                "--key-env",
                "K",
            ]),
            args(&[
                "add-relay",
                "r",
                "--url",
                &format!("https://gw.example?k={canary}"),
                "--key-env",
                "K",
            ]),
        ] {
            let err = command(&cli).unwrap_err();
            assert!(!err.contains(canary), "canary echoed in CLI error: {err}");
        }

        for toml in [
            format!(
                "[auth.profiles.r]\nmode=\"relay\"\nbase_url=\"https://gw.example\"\nengines=[\"claude\"]\napi_key={{from_env=\"{canary}\"}}"
            ),
            format!(
                "[auth.profiles.r]\nmode=\"relay\"\nbase_url=\"https://gw.example\"\nengines=[\"claude\"]\napi_key={{from_file=\"{canary}\"}}"
            ),
            format!("[auth.profiles.l]\nmode=\"local\"\nprovider=\"ollama\"\nauth=\"{canary}\""),
        ] {
            let err = parse_auth_profiles_toml(&toml, root, None).unwrap_err();
            assert!(!err.contains(canary), "canary echoed in TOML error: {err}");
        }
    }

    #[test]
    fn adt_forbids_agy_and_cross_cloud_fields_and_unused_credential_chain() {
        let root = Path::new("/var/lib/spawn/auth-profiles");
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        for bad_engines in ["agy", "claude,agy"] {
            let err = command(&args(&[
                "add-relay",
                "bad",
                "--url",
                "https://gw.example.com",
                "--key-env",
                "GW_KEY",
                "--engines",
                bad_engines,
            ]))
            .unwrap_err();
            assert!(err.contains("native_login"), "{bad_engines}: {err}");
        }

        for toml in [
            "[auth.profiles.p]\nmode=\"relay\"\nbase_url=\"https://gw.example\"\nengines=[\"agy\"]\napi_key={from_env=\"K\"}",
            "[auth.profiles.p]\nmode=\"local\"\nprovider=\"ollama\"\nengines=[\"agy\"]",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"vertex\"\nregion=\"us-central1\"\nengines=[\"agy\"]",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"vertex\"\nregion=\"us-central1\"\nengines=[\"codex\"]",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"azure\"\nregion=\"eastus\"\nengines=[\"claude\"]",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"vertex\"\nregion=\"us-central1\"\naws_profile=\"p\"",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"bedrock\"\nregion=\"us-east-1\"\nservice_account=\"sa@p\"",
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"azure\"\nregion=\"eastus\"\naws_profile=\"p\"",
            // N5: credential_chain removed & refused fail-closed.
            "[auth.profiles.p]\nmode=\"cloud_iam\"\nprovider=\"vertex\"\nregion=\"us-central1\"\ncredential_chain=[\"adc\"]",
        ] {
            assert!(
                parse_auth_profiles_toml(toml, root, None).is_err(),
                "expected rejection for:\n{toml}"
            );
        }
    }

    #[test]
    fn n2_n3_azure_engine_subset_and_bedrock_probe() {
        let scratch = scratch_dir("n2-n3");
        let root = scratch.join("controlled/auth-profiles");
        fs::create_dir_all(&root).expect("root");

        let cfg_path = scratch.join("auth.toml");
        fs::write(
            &cfg_path,
            r#"
[auth.profiles.az-codex-only]
mode = "cloud_iam"
provider = "azure"
region = "eastus"
resource = "aoai-prod"
engines = ["codex"]

[auth.profiles.aws-prod]
mode = "cloud_iam"
provider = "bedrock"
region = "us-east-1"
aws_profile = "prod"
"#,
        )
        .expect("write cfg");

        let r_missing = evaluate_profiles_report_with(
            &cfg_path,
            &root,
            &|_| None,
            &|_| false,
            &|_| None,
            Some(&scratch.join("home")),
            1_750_000_000,
        )
        .expect("report missing");

        let az = r_missing
            .profiles
            .iter()
            .find(|p| p.name == "az-codex-only")
            .unwrap();
        assert_eq!(az.engines, vec![Engine::Codex]);
        assert_eq!(
            az.env_preview.keys().copied().collect::<Vec<_>>(),
            vec!["codex"]
        );

        let bedrock = r_missing
            .profiles
            .iter()
            .find(|p| p.name == "aws-prod")
            .unwrap();
        assert_eq!(bedrock.readiness, ProfileReadiness::Missing);

        let aws_cfg = scratch.join("home/.aws/config");
        let r_ready = evaluate_profiles_report_with(
            &cfg_path,
            &root,
            &|_| None,
            &|p| p == aws_cfg,
            &|p| (p == aws_cfg).then(|| "[profile prod]\nregion = us-east-1\n".to_string()),
            Some(&scratch.join("home")),
            1_750_000_000,
        )
        .expect("report ready");
        assert_eq!(
            r_ready
                .profiles
                .iter()
                .find(|p| p.name == "aws-prod")
                .unwrap()
                .readiness,
            ProfileReadiness::Ready
        );
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn n4_tagged_json_view_omits_null_option_bags_and_redacts_canary() {
        let scratch = scratch_dir("n4-tagged");
        let root = scratch.join("controlled/auth-profiles");
        fs::create_dir_all(&root).expect("root");

        let cfg_path = scratch.join("auth.toml");
        fs::write(
            &cfg_path,
            format!(
                r#"
[auth.profiles.relay-env]
mode = "relay"
base_url = "https://gw.example/v1"
engines = ["claude"]
[auth.profiles.relay-env.api_key]
from_env = "RELAY_KEY"

[auth.profiles.claude-work]
mode = "native_login"
provider = "claude_oauth"
profile_dir = "{pdir}"

[auth.profiles.local-ollama]
mode = "local"
provider = "ollama"
auth = "none"
"#,
                pdir = root.join("claude-work").display()
            ),
        )
        .expect("write cfg");

        let report = evaluate_profiles_report_with(
            &cfg_path,
            &root,
            &|k| (k == "RELAY_KEY").then(|| "sk-canary-val-5512".to_string()),
            &|_| false,
            &|_| None,
            Some(&scratch.join("home")),
            1_750_000_000,
        )
        .expect("report");

        let json_val = serde_json::to_value(&report).unwrap();
        let profiles = json_val["profiles"].as_array().unwrap();
        let relay_obj = profiles
            .iter()
            .find(|v| v["name"] == "relay-env")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(relay_obj["mode"], json!("relay"));
        assert!(!relay_obj.contains_key("isolated_dir"));
        assert!(!relay_obj.contains_key("cloud"));
        assert!(!relay_obj.contains_key("local_provider"));

        let native_obj = profiles
            .iter()
            .find(|v| v["name"] == "claude-work")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(native_obj["mode"], json!("native_login"));
        assert!(!native_obj.contains_key("base_url"));
        assert!(!native_obj.contains_key("secret_source"));

        let local_obj = profiles
            .iter()
            .find(|v| v["name"] == "local-ollama")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(local_obj["mode"], json!("local"));
        assert_eq!(local_obj["local_auth"], json!("none"));
        assert!(!local_obj.contains_key("secret_source"));

        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("sk-canary-val-5512")
        );
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn probes_native_login_states_and_translates_all_mode_envs() {
        let scratch = scratch_dir("native-and-envs");
        let profiles_root = scratch.join("controlled/auth-profiles");
        let fake_home = scratch.join("home");
        fs::create_dir_all(&profiles_root).expect("root");

        let isolated = profiles_root.join("claude-work");
        let now = 1_750_000_000_i64;
        assert_eq!(
            probe_native_login(
                &isolated,
                &profiles_root,
                NativeLoginProvider::ClaudeOauth,
                Some(&fake_home),
                now
            )
            .readiness,
            ProfileReadiness::Missing
        );
        fs::create_dir_all(&isolated).expect("isolated");
        let cred = isolated.join(".credentials.json");
        fs::write(
            &cred,
            format!(
                r#"{{"claudeAiOauth":{{"expiresAt":{}}}}}"#,
                (now + 3600) * 1000
            ),
        )
        .expect("ready");
        assert_eq!(
            probe_native_login(
                &isolated,
                &profiles_root,
                NativeLoginProvider::ClaudeOauth,
                Some(&fake_home),
                now
            )
            .readiness,
            ProfileReadiness::Ready
        );
        fs::write(
            &cred,
            format!(
                r#"{{"claudeAiOauth":{{"expiresAt":{}}}}}"#,
                (now - 10) * 1000
            ),
        )
        .expect("expired");
        assert_eq!(
            probe_native_login(
                &isolated,
                &profiles_root,
                NativeLoginProvider::ClaudeOauth,
                Some(&fake_home),
                now
            )
            .readiness,
            ProfileReadiness::ReauthRequired
        );

        let plan = plan_login_command(
            NativeLoginProvider::Agy,
            "agy-work",
            &profiles_root.join("agy-work"),
            &profiles_root,
            Some(&fake_home),
            &|_| None,
        )
        .expect("plan agy login");
        assert_eq!(plan.engine, Engine::Agy);
        assert!(plan.env.set.contains_key("AGY_HOME"));

        let relay = RelayProfile {
            base_url: "https://gw.example/v1".to_string(),
            api_key: SecretRef::FromEnv {
                from_env: "GW_KEY".to_string(),
            },
            engines: [RelayEngine::Claude, RelayEngine::OpenCode]
                .into_iter()
                .collect(),
        };
        let canary = "sk-env-secret-redact-123";
        let env = relay.translate_env(RelayEngine::Claude, canary).unwrap();
        assert!(!format!("{env:?}").contains(canary));
        assert_eq!(
            env.masked_summary()
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("<redacted>")
        );

        let azure = CloudIamProfile::Azure {
            region: "eastus".to_string(),
            resource: None,
            tenant_id: None,
            endpoint: None,
            engines: [AzureEngine::Codex].into_iter().collect(),
        };
        assert!(azure.translate_env(Engine::Codex).is_ok());
        assert!(azure.translate_env(Engine::OpenCode).is_err());

        let _ = fs::remove_dir_all(&scratch);
    }
}
