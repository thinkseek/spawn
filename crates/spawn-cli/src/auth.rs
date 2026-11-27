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
//! Illegal combinations (such as `agy` under `relay`, `local`, or `cloud_iam`,
//! or cross-cloud IAM fields across Vertex / Bedrock / Azure) are unrepresentable
//! in the type system and rejected fail-closed at parse boundaries.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};

use crate::config;
use crate::config_file::SecretRef;
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

/// Isolated native OAuth / QR login profile with host directory isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLoginProfile {
    pub(crate) provider: NativeLoginProvider,
    pub(crate) profile_dir: PathBuf,
}

/// Strict 3-variant Sum Type for passwordless cloud IAM providers (zero cross-cloud field bloat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudIamProfile {
    Vertex {
        region: String,
        project: Option<String>,
        service_account: Option<String>,
        endpoint: Option<String>,
        credential_chain: Vec<String>,
    },
    Bedrock {
        region: String,
        aws_profile: Option<String>,
        endpoint: Option<String>,
        credential_chain: Vec<String>,
    },
    Azure {
        region: String,
        resource: Option<String>,
        tenant_id: Option<String>,
        endpoint: Option<String>,
        credential_chain: Vec<String>,
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

    pub(crate) const fn engines(&self) -> &'static [Engine] {
        match self {
            Self::Vertex { .. } | Self::Bedrock { .. } => &[Engine::Claude],
            Self::Azure { .. } => &[Engine::Codex, Engine::OpenCode],
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
            Self::CloudIam(cloud) => cloud.engines().to_vec(),
            Self::Local(local) => local.engines.iter().map(|e| e.into_engine()).collect(),
        }
    }

    pub(crate) fn isolated_dir(&self) -> Option<&Path> {
        match self {
            Self::NativeLogin(native) => Some(native.profile_dir.as_path()),
            Self::Relay(_) | Self::CloudIam(_) | Self::Local(_) => None,
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

/// Subprocess invocation plan for `spawn auth login <engine>`.
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
                ..
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
                ..
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

/// Validate an environment variable identifier (`[A-Za-z_][A-Za-z0-9_]*`).
pub(crate) fn validate_env_var_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return Err("environment variable name must not be empty".to_string());
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "environment variable `{trimmed}` must start with an ASCII letter or `_` (never pass a secret value)"
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(format!(
            "environment variable `{trimmed}` contains invalid characters; expected [A-Za-z_][A-Za-z0-9_]*"
        ));
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

/// Validate an `http://` or `https://` endpoint URL, rejecting embedded credentials.
pub(crate) fn validate_endpoint_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) || trimmed.contains(' ') {
        return Err("endpoint URL must be a single valid http:// or https:// URL".to_string());
    }
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| format!("endpoint URL `{trimmed}` must start with https:// or http://"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(format!("endpoint URL `{trimmed}` is missing a host"));
    }
    if authority.contains('@') {
        return Err(
            "endpoint URL must not embed userinfo or credentials before `@`; use SecretRef"
                .to_string(),
        );
    }
    if let Some((_, query_plus)) = rest[authority_end..].split_once('?') {
        let query = query_plus.split('#').next().unwrap_or(query_plus);
        for pair in query.split('&') {
            let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "key" | "api_key" | "apikey" | "token" | "access_token" | "secret" | "password"
            ) {
                return Err(format!(
                    "endpoint URL must not embed credential query parameter `{key}=`; use SecretRef"
                ));
            }
        }
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn normalize_lexical(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {}", path.display()));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push(Component::RootDir),
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                return Err(format!(
                    "path must not contain `..` traversal: {}",
                    path.display()
                ));
            }
            Component::Prefix(_) => {
                return Err(format!("unsupported path prefix: {}", path.display()));
            }
        }
    }
    Ok(normalized)
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

fn forbidden_host_dirs(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.to_path_buf()];
    if let Ok(canon) = fs::canonicalize(home)
        && canon != home
    {
        roots.push(canon);
    }
    let mut candidates = Vec::new();
    for root in roots {
        candidates.push(root.clone());
        for rel in [
            ".claude",
            ".gemini",
            ".codex",
            ".antigravity",
            ".agy",
            ".config/claude",
            ".config/codex",
            ".config/agy",
            ".config/antigravity",
        ] {
            candidates.push(root.join(rel));
        }
    }
    candidates
}

fn path_touches_forbidden_dir(candidate: &Path, blocked: &Path, home: &Path) -> bool {
    candidate == home || candidate == blocked || (blocked != home && candidate.starts_with(blocked))
}

/// Validate that a `native_login` `profile_dir` is strictly isolated from host
/// `~/.claude`, `~/.gemini`, `~/.codex`, and `$HOME` (even via symlinks).
pub(crate) fn validate_isolated_profile_dir(
    dir: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let normalized = normalize_lexical(dir)?;
    let canonical = resolve_through_existing_ancestor(&normalized);
    let link_target = fs::symlink_metadata(&normalized)
        .ok()
        .filter(|meta| meta.file_type().is_symlink())
        .and_then(|_| fs::read_link(&normalized).ok());

    if let Some(home_dir) = home {
        let home_norm = normalize_lexical(home_dir)?;
        for blocked in forbidden_host_dirs(&home_norm) {
            if path_touches_forbidden_dir(&normalized, &blocked, &home_norm)
                || path_touches_forbidden_dir(&canonical, &blocked, &home_norm)
            {
                return Err(format!(
                    "native_login profile_dir {} refuses host directory {} to prevent host credential pollution",
                    dir.display(),
                    blocked.display()
                ));
            }
            if let Some(target) = &link_target {
                let abs_target = if target.is_absolute() {
                    target.clone()
                } else {
                    normalized.parent().unwrap_or(home_dir).join(target)
                };
                if path_touches_forbidden_dir(&abs_target, &blocked, &home_norm) {
                    return Err(format!(
                        "native_login profile_dir {} symlinks into forbidden host directory {}",
                        dir.display(),
                        blocked.display()
                    ));
                }
            }
        }
    }
    Ok(normalized)
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
            let value = secret_ref.resolve_with(env_lookup).map_err(|_| {
                SecretResolveError::Missing(format!(
                    "environment variable `{from_env}` is unset or empty"
                ))
            })?;
            Ok((value, Vec::new()))
        }
        SecretRef::FromFile { from_file } => {
            let normalized = normalize_lexical(from_file).map_err(SecretResolveError::Invalid)?;
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
                .resolve_with(env_lookup)
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
    provider: NativeLoginProvider,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> ProbeOutcome {
    if let Err(reason) = validate_isolated_profile_dir(dir, home) {
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

/// Non-destructively probe whether ambient passwordless Cloud IAM credentials exist.
pub(crate) fn probe_cloud_iam_with(
    cloud: &CloudIamProfile,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
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
        CloudIamProfile::Bedrock { aws_profile, .. } => {
            if env_file_exists("AWS_WEB_IDENTITY_TOKEN_FILE") {
                Some("AWS_WEB_IDENTITY_TOKEN_FILE present")
            } else if has_env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                || has_env("AWS_CONTAINER_CREDENTIALS_FULL_URI")
                || has_env("AWS_ROLE_ARN")
                || has_env("AWS_PROFILE")
                || aws_profile.is_some()
            {
                Some("AWS IAM profile or container workload identity detected")
            } else if home.is_some_and(|h| {
                file_exists(&h.join(".aws/credentials")) || file_exists(&h.join(".aws/config"))
            }) {
                Some("local AWS shared config/credentials chain present")
            } else {
                None
            }
        }
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
                "no ambient {} IAM credential chain detected (region={})",
                cloud.provider_str(),
                cloud.region()
            ),
            warnings: Vec::new(),
        }
    }
}

fn probe_profile_with(
    profile: &AuthProfile,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> ProbeOutcome {
    match profile {
        AuthProfile::Relay(relay) => probe_secret_ref_with(&relay.api_key, env_lookup),
        AuthProfile::NativeLogin(native) => {
            probe_native_login(&native.profile_dir, native.provider, home, now_unix_secs)
        }
        AuthProfile::CloudIam(cloud) => probe_cloud_iam_with(cloud, env_lookup, file_exists, home),
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
    home: Option<&Path>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<LoginCommandPlan, String> {
    let validated_name = validate_profile_name(profile_name)?;
    let validated_dir = validate_isolated_profile_dir(profile_dir, home)?;
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
    #[serde(default)]
    credential_chain: Vec<String>,
    #[serde(default)]
    engines: Vec<String>,
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
    #[serde(default)]
    credential_chain: Vec<String>,
    #[serde(default)]
    engines: Vec<String>,
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
    #[serde(default)]
    credential_chain: Vec<String>,
    #[serde(default)]
    engines: Vec<String>,
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
        if lower == "api_key" && !v.is_object() {
            return Err(format!(
                "[auth.profiles.{profile_name}]: `api_key` must be a SecretRef table; {PLAINTEXT_ERR}"
            ));
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

fn validate_cloud_iam_engines(
    profile_name: &str,
    provider: &str,
    configured: &[String],
    allowed: &[Engine],
) -> Result<(), String> {
    for raw in configured {
        let norm = raw.trim().to_ascii_lowercase();
        if matches!(norm.as_str(), "agy" | "antigravity") {
            return Err(format!(
                "[auth.profiles.{profile_name}]: engine `agy` only supports `native_login` (not `cloud_iam`)"
            ));
        }
        if !allowed.iter().any(|e| e.as_str() == norm) {
            return Err(format!(
                "[auth.profiles.{profile_name}]: cloud_iam provider `{provider}` only permits [{}], got `{raw}`",
                format_engine_list(allowed)
            ));
        }
    }
    Ok(())
}

fn default_chain(raw: Vec<String>, fallback: &[&str]) -> Vec<String> {
    if raw.is_empty() {
        fallback.iter().map(|s| (*s).to_string()).collect()
    } else {
        raw
    }
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
        normalize_lexical(from_file)
            .map_err(|error| format!("[auth.profiles.{name}]: api_key.from_file {error}"))?;
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
    let profile_dir = validate_isolated_profile_dir(&candidate_dir, home)
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
            validate_cloud_iam_engines(&name, "vertex", &raw.engines, &[Engine::Claude])?;
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
                credential_chain: default_chain(
                    raw.credential_chain,
                    &["workload_identity", "application_default", "gce_metadata"],
                ),
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
            validate_cloud_iam_engines(&name, "bedrock", &raw.engines, &[Engine::Claude])?;
            let region = validate_safe_identifier("region", &raw.region)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let cloud = CloudIamProfile::Bedrock {
                region,
                aws_profile: opt_safe_id(&name, "aws_profile", raw.aws_profile.as_deref())?,
                endpoint: opt_endpoint(&name, raw.endpoint.as_deref())?,
                credential_chain: default_chain(
                    raw.credential_chain,
                    &[
                        "web_identity_token",
                        "ecs_container_credentials",
                        "iam_role_profile",
                    ],
                ),
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
            validate_cloud_iam_engines(
                &name,
                "azure",
                &raw.engines,
                &[Engine::Codex, Engine::OpenCode],
            )?;
            let region = validate_safe_identifier("region", &raw.region)
                .map_err(|error| format!("[auth.profiles.{name}]: {error}"))?;
            let cloud = CloudIamProfile::Azure {
                region,
                resource: opt_safe_id(&name, "resource", raw.resource.as_deref())?,
                tenant_id: opt_safe_id(&name, "tenant_id", raw.tenant_id.as_deref())?,
                endpoint: opt_endpoint(&name, raw.endpoint.as_deref())?,
                credential_chain: default_chain(
                    raw.credential_chain,
                    &["federated_token", "managed_identity", "azure_cli_cache"],
                ),
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
        return Err(format!(
            "[auth.profiles.{name}]: local profile `auth` must be \"none\", got `{auth_val}` (use `mode = \"relay\"` with SecretRef for keyed gateways)"
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

fn section_header_name(trimmed_line: &str) -> Option<&str> {
    let stripped = trimmed_line.strip_prefix('[')?.trim_start();
    if stripped.starts_with('[') {
        return None;
    }
    stripped
        .split('#')
        .next()
        .unwrap_or(stripped)
        .trim_end()
        .strip_suffix(']')
        .map(str::trim)
}

fn belongs_to_profile_section(section: &str, name: &str) -> bool {
    let target = format!("auth.profiles.{name}");
    section == target || section.starts_with(&format!("{target}."))
}

/// Upsert `[auth.profiles.<name>]` in `existing` TOML while preserving unrelated sections/comments.
pub(crate) fn upsert_relay_profile_toml(
    existing: &str,
    name: &str,
    relay: &RelayProfile,
    tiers: &[ModelTier],
) -> Result<String, String> {
    let valid_name = validate_profile_name(name)?;
    let _ = validate_endpoint_url(&relay.base_url)?;
    if relay.engines.is_empty() {
        return Err("relay profile must target at least one engine".to_string());
    }

    let mut kept_lines: Vec<&str> = Vec::new();
    let mut skip_section = false;
    for line in existing.lines() {
        if let Some(section) = section_header_name(line.trim()) {
            skip_section = belongs_to_profile_section(section, &valid_name);
            if skip_section {
                continue;
            }
        }
        if !skip_section {
            kept_lines.push(line);
        }
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

    parse_auth_profiles_toml(&out, Path::new("/tmp/spawn-auth-validate"), None).map_err(
        |error| format!("upsert `{valid_name}` conflicts with existing TOML structure: {error}"),
    )?;
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

/// Redacted metadata entry returned by `list` and `check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RedactedProfileView {
    pub(crate) name: String,
    pub(crate) source: ProfileSource,
    pub(crate) mode: &'static str,
    pub(crate) engines: Vec<Engine>,
    pub(crate) tiers: Vec<ModelTier>,
    pub(crate) readiness: ProfileReadiness,
    pub(crate) detail: String,
    pub(crate) warnings: Vec<String>,
    pub(crate) isolated_dir: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) secret_source: Option<Value>,
    pub(crate) cloud: Option<Value>,
    pub(crate) local_provider: Option<LocalProvider>,
    pub(crate) local_auth: Option<&'static str>,
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

fn secret_source_redacted(secret: &SecretRef) -> Value {
    match secret {
        SecretRef::FromEnv { from_env } => json!({ "kind": "from_env", "var": from_env }),
        SecretRef::FromFile { from_file } => {
            json!({ "kind": "from_file", "path": from_file.display().to_string() })
        }
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
            for &engine in cloud.engines() {
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

fn cloud_metadata_json(cloud: &CloudIamProfile) -> Value {
    match cloud {
        CloudIamProfile::Vertex {
            region,
            project,
            service_account,
            credential_chain,
            ..
        } => json!({
            "provider": "vertex",
            "region": region,
            "project": project,
            "service_account": service_account,
            "credential_chain": credential_chain,
        }),
        CloudIamProfile::Bedrock {
            region,
            aws_profile,
            credential_chain,
            ..
        } => json!({
            "provider": "bedrock",
            "region": region,
            "aws_profile": aws_profile,
            "credential_chain": credential_chain,
        }),
        CloudIamProfile::Azure {
            region,
            resource,
            tenant_id,
            credential_chain,
            ..
        } => json!({
            "provider": "azure",
            "region": region,
            "resource": resource,
            "tenant_id": tenant_id,
            "credential_chain": credential_chain,
        }),
    }
}

fn redact_profile_view(named: &NamedAuthProfile, probe: ProbeOutcome) -> RedactedProfileView {
    let isolated_dir = named
        .profile
        .isolated_dir()
        .map(|p| p.display().to_string());
    let env_preview = redacted_env_preview(named);
    let (base_url, secret_source, cloud, local_provider, local_auth) = match &named.profile {
        AuthProfile::Relay(relay) => (
            Some(relay.base_url.clone()),
            Some(secret_source_redacted(&relay.api_key)),
            None,
            None,
            None,
        ),
        AuthProfile::NativeLogin(_) => (None, None, None, None, None),
        AuthProfile::CloudIam(cloud) => (
            cloud.endpoint().map(str::to_string),
            None,
            Some(cloud_metadata_json(cloud)),
            None,
            None,
        ),
        AuthProfile::Local(local) => (
            Some(local.base_url.clone()),
            None,
            None,
            Some(local.provider),
            Some("none"),
        ),
    };

    RedactedProfileView {
        name: named.name.clone(),
        source: named.source,
        mode: named.profile.mode_str(),
        engines: named.profile.engines(),
        tiers: named.tiers.clone(),
        readiness: probe.readiness,
        detail: probe.detail,
        warnings: probe.warnings,
        isolated_dir,
        base_url,
        secret_source,
        cloud,
        local_provider,
        local_auth,
        env_preview,
    }
}

/// Evaluate all configured and default profiles into a redacted [`AuthReport`].
pub(crate) fn evaluate_profiles_report_with(
    config_path: &Path,
    profiles_root: &Path,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    file_exists: &dyn Fn(&Path) -> bool,
    home: Option<&Path>,
    now_unix_secs: i64,
) -> Result<AuthReport, String> {
    let config_exists = config_path.is_file();
    let configured = if config_exists {
        let raw = fs::read_to_string(config_path)
            .map_err(|error| format!("read auth config {}: {error}", config_path.display()))?;
        parse_auth_profiles_toml(&raw, profiles_root, home)?
    } else {
        BTreeMap::new()
    };

    let all_profiles = merge_with_default_profiles(configured, profiles_root);
    let mut views = Vec::with_capacity(all_profiles.len());
    let (mut ready, mut missing, mut reauth_required) = (0usize, 0usize, 0usize);
    let (mut configured_count, mut configured_ready) = (0usize, 0usize);

    for named in &all_profiles {
        let probe =
            probe_profile_with(&named.profile, env_lookup, file_exists, home, now_unix_secs);
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
        profiles_root_dir: profiles_root.display().to_string(),
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
        let isolated = p.isolated_dir.as_deref().unwrap_or("-");
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
            p.mode,
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
    let profiles_root = config::auth_profiles_root_dir();
    let home = process_home();
    let report = evaluate_profiles_report_with(
        &config_path,
        &profiles_root,
        &|name| config::env_value(name),
        &Path::exists,
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
            from_file: normalize_lexical(Path::new(path))?,
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
    let updated = upsert_relay_profile_toml(&existing, &name, &relay, &tiers)?;
    write_text_atomic(&target_path, &updated)?;

    if parsed.switches.contains("json") {
        let payload = json!({
            "schema_version": 1,
            "action": "add-relay",
            "profile": name,
            "mode": "relay",
            "config_path": target_path.display().to_string(),
            "base_url": relay.base_url,
            "engines": relay.engines,
            "tiers": tiers,
            "secret_source": secret_source_redacted(&api_key),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|error| format!("serialize add-relay output: {error}"))?
        );
    } else {
        let engine_names: Vec<&str> = relay.engines.iter().map(|e| e.as_str()).collect();
        let source = match &api_key {
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
    Ok(0)
}

fn ensure_isolated_dirs(dir: &Path) -> Result<(), String> {
    for sub in [dir, &dir.join("xdg-config"), &dir.join("xdg-data")] {
        fs::create_dir_all(sub)
            .map_err(|error| format!("create isolated profile dir {}: {error}", sub.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(sub, fs::Permissions::from_mode(0o700)).map_err(|error| {
                format!("set permissions on isolated dir {}: {error}", sub.display())
            })?;
        }
    }
    Ok(())
}

fn run_login(cli_config: Option<&str>, args: &[String]) -> Result<i32, String> {
    let parsed = parse_auth_args(args, Some("<engine>"), &["profile", "config"], &[])?;
    let provider =
        NativeLoginProvider::parse_provider(parsed.positional.as_deref().unwrap_or_default())?;
    let explicit = effective_explicit_config(cli_config, &parsed);
    let config_path = config::auth_config_read_path(explicit.as_deref());
    let profiles_root = config::auth_profiles_root_dir();
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
        home.as_deref(),
        &|name| config::env_value(name),
    )?;
    ensure_isolated_dirs(&plan.profile_dir)?;

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
    fn adt_forbids_agy_in_relay_local_and_cloud_iam() {
        let root = Path::new("/tmp/spawn-auth-validate");
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

        let relay_agy = r#"
[auth.profiles.r-agy]
mode = "relay"
base_url = "https://gw.example.com/v1"
engines = ["claude", "agy"]
api_key = { from_env = "GW_KEY" }
"#;
        assert!(
            parse_auth_profiles_toml(relay_agy, root, None)
                .unwrap_err()
                .contains("native_login")
        );

        let local_agy = r#"
[auth.profiles.l-agy]
mode = "local"
provider = "ollama"
engines = ["agy"]
"#;
        assert!(
            parse_auth_profiles_toml(local_agy, root, None)
                .unwrap_err()
                .contains("native_login")
        );

        let cloud_agy = r#"
[auth.profiles.v-agy]
mode = "cloud_iam"
provider = "vertex"
region = "us-central1"
engines = ["agy"]
"#;
        assert!(
            parse_auth_profiles_toml(cloud_agy, root, None)
                .unwrap_err()
                .contains("native_login")
        );

        let vertex_codex = r#"
[auth.profiles.v-codex]
mode = "cloud_iam"
provider = "vertex"
region = "us-central1"
engines = ["codex"]
"#;
        assert!(parse_auth_profiles_toml(vertex_codex, root, None).is_err());

        let azure_claude = r#"
[auth.profiles.az-claude]
mode = "cloud_iam"
provider = "azure"
region = "eastus"
engines = ["claude"]
"#;
        assert!(parse_auth_profiles_toml(azure_claude, root, None).is_err());
    }

    #[test]
    fn adt_rejects_cross_cloud_iam_fields() {
        let root = Path::new("/tmp/spawn-auth-validate");
        let bad_clouds = [
            r#"
[auth.profiles.bad_vertex]
mode = "cloud_iam"
provider = "vertex"
region = "us-central1"
aws_profile = "prod-aws"
"#,
            r#"
[auth.profiles.bad_vertex_tenant]
mode = "cloud_iam"
provider = "vertex"
region = "us-central1"
tenant_id = "0000-tenant"
"#,
            r#"
[auth.profiles.bad_bedrock]
mode = "cloud_iam"
provider = "bedrock"
region = "us-east-1"
service_account = "sa@proj.iam.gserviceaccount.com"
"#,
            r#"
[auth.profiles.bad_bedrock_tenant]
mode = "cloud_iam"
provider = "bedrock"
region = "us-east-1"
tenant_id = "0000-tenant"
"#,
            r#"
[auth.profiles.bad_azure]
mode = "cloud_iam"
provider = "azure"
region = "eastus"
aws_profile = "prod-aws"
"#,
            r#"
[auth.profiles.bad_azure_sa]
mode = "cloud_iam"
provider = "azure"
region = "eastus"
service_account = "sa@proj.iam.gserviceaccount.com"
"#,
        ];
        for toml in bad_clouds {
            assert!(
                parse_auth_profiles_toml(toml, root, None).is_err(),
                "expected cross-cloud rejection for:\n{toml}"
            );
        }
    }

    #[test]
    fn parses_all_four_modes_from_toml() {
        let root = scratch_dir("parse-all-four");
        let isolated_dir = root.join("profiles/claude-work");
        let toml = format!(
            r#"
[auth.profiles.openrouter]
mode = "relay"
base_url = "https://openrouter.ai/api/"
engines = ["claude", "codex", "opencode"]
tiers = ["L1", "L2"]
[auth.profiles.openrouter.api_key]
from_env = "OPENROUTER_API_KEY"

[auth.profiles.claude-work]
mode = "native_login"
provider = "claude_oauth"
profile_dir = "{dir}"

[auth.profiles.gcp-vertex]
mode = "cloud_iam"
provider = "vertex"
region = "us-central1"
project = "my-gcp-project"
service_account = "runner@my-gcp-project.iam.gserviceaccount.com"

[auth.profiles.aws-bedrock]
mode = "cloud_iam"
provider = "bedrock"
region = "us-east-1"
aws_profile = "bedrock-prod"

[auth.profiles.az-openai]
mode = "cloud_iam"
provider = "azure"
region = "eastus"
resource = "aoai-prod"
tenant_id = "tenant-123"
endpoint = "https://aoai-prod.openai.azure.com"

[auth.profiles.local-ollama]
mode = "local"
provider = "ollama"
auth = "none"
engines = ["codex", "opencode"]
"#,
            dir = isolated_dir.display()
        );

        let parsed = parse_auth_profiles_toml(
            &toml,
            &root.join("default-profiles"),
            Some(Path::new("/home/tester")),
        )
        .expect("parse all 4 modes");
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed["openrouter"].profile.mode_str(), "relay");
        assert_eq!(parsed["claude-work"].profile.mode_str(), "native_login");
        assert_eq!(parsed["gcp-vertex"].profile.mode_str(), "cloud_iam");
        assert_eq!(parsed["aws-bedrock"].profile.mode_str(), "cloud_iam");
        assert_eq!(parsed["az-openai"].profile.mode_str(), "cloud_iam");
        assert_eq!(parsed["local-ollama"].profile.mode_str(), "local");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn translates_relay_envs_with_secret_redaction() {
        let canary = "sk-canary-never-leak-debug-4291";
        let relay = RelayProfile {
            base_url: "https://openrouter.ai/api".to_string(),
            api_key: SecretRef::FromEnv {
                from_env: "OPENROUTER_API_KEY".to_string(),
            },
            engines: [
                RelayEngine::Claude,
                RelayEngine::Codex,
                RelayEngine::OpenCode,
            ]
            .into_iter()
            .collect(),
        };
        let claude_env = relay
            .translate_env(RelayEngine::Claude, canary)
            .expect("claude env");
        assert_eq!(
            claude_env.set.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(
            claude_env.set.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(canary)
        );
        assert!(!format!("{claude_env:?}").contains(canary));
        assert_eq!(
            claude_env
                .masked_summary()
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("<redacted>")
        );

        let codex_env = relay
            .translate_env(RelayEngine::Codex, canary)
            .expect("codex env");
        assert_eq!(
            codex_env.set.get("OPENAI_API_KEY").map(String::as_str),
            Some(canary)
        );
        let oc_env = relay
            .translate_env(RelayEngine::OpenCode, canary)
            .expect("opencode env");
        assert!(
            oc_env.set.contains_key("OPENAI_API_KEY")
                && oc_env.set.contains_key("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn translates_cloud_iam_and_local_envs_cleanly() {
        let vertex = CloudIamProfile::Vertex {
            region: "us-central1".to_string(),
            project: Some("my-gcp-project".to_string()),
            service_account: None,
            endpoint: None,
            credential_chain: vec!["workload_identity".to_string()],
        };
        assert_eq!(vertex.engines(), &[Engine::Claude]);
        let vertex_env = vertex.translate_env(Engine::Claude).expect("vertex env");
        assert_eq!(
            vertex_env
                .set
                .get("CLAUDE_CODE_USE_VERTEX")
                .map(String::as_str),
            Some("1")
        );
        assert!(
            vertex
                .translate_env(Engine::Agy)
                .unwrap_err()
                .contains("native_login")
        );
        assert!(vertex.translate_env(Engine::Codex).is_err());

        let bedrock = CloudIamProfile::Bedrock {
            region: "us-east-1".to_string(),
            aws_profile: Some("bedrock-prod".to_string()),
            endpoint: None,
            credential_chain: vec!["web_identity_token".to_string()],
        };
        let bedrock_env = bedrock.translate_env(Engine::Claude).expect("bedrock env");
        assert_eq!(
            bedrock_env.set.get("AWS_PROFILE").map(String::as_str),
            Some("bedrock-prod")
        );

        let azure = CloudIamProfile::Azure {
            region: "eastus".to_string(),
            resource: Some("aoai-prod".to_string()),
            tenant_id: Some("tenant-123".to_string()),
            endpoint: Some("https://aoai-prod.openai.azure.com".to_string()),
            credential_chain: vec!["federated_token".to_string()],
        };
        assert_eq!(azure.engines(), &[Engine::Codex, Engine::OpenCode]);
        let az_env = azure.translate_env(Engine::Codex).expect("azure env");
        assert_eq!(
            az_env.set.get("AZURE_TENANT_ID").map(String::as_str),
            Some("tenant-123")
        );
        assert!(azure.translate_env(Engine::Claude).is_err());

        let local = LocalProfile {
            provider: LocalProvider::Ollama,
            base_url: LocalProvider::Ollama.default_base_url().to_string(),
            engines: [RelayEngine::Codex, RelayEngine::OpenCode]
                .into_iter()
                .collect(),
        };
        let local_env = local.translate_env(RelayEngine::Codex).expect("local env");
        assert_eq!(
            local_env.set.get("OPENAI_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(
            local_env.set.get("OPENAI_API_KEY").map(String::as_str),
            Some("not-required")
        );
    }

    #[test]
    fn anti_plaintext_guards_reject_inline_keys_urls_and_local_tokens() {
        let root = Path::new("/tmp/spawn-auth-validate");
        let plaintext_toml = r#"
[auth.profiles.bad]
mode = "relay"
base_url = "https://gw.example.com/v1"
api_key = "sk-plaintext-must-fail"
engines = ["claude"]
"#;
        let err = parse_auth_profiles_toml(plaintext_toml, root, None).unwrap_err();
        assert!(!err.contains("sk-plaintext-must-fail"), "{err}");

        let smuggled_toml = r#"
[auth.profiles.bad]
mode = "relay"
base_url = "https://gw.example.com/v1"
key = "sk-plaintext"
engines = ["claude"]
[auth.profiles.bad.api_key]
from_env = "OK_VAR"
"#;
        assert!(parse_auth_profiles_toml(smuggled_toml, root, None).is_err());

        assert!(
            validate_endpoint_url("https://user:secret@gw.example.com/v1")
                .unwrap_err()
                .contains("userinfo")
        );
        assert!(
            validate_endpoint_url("https://gw.example.com/v1?api_key=sk-secret")
                .unwrap_err()
                .contains("query parameter")
        );

        let bad_local = r#"
[auth.profiles.badlocal]
mode = "local"
provider = "vllm"
auth = "sk-local-secret"
"#;
        assert!(parse_auth_profiles_toml(bad_local, root, None).is_err());
    }

    #[test]
    fn native_login_enforces_host_dir_isolation_and_probes_readiness_states() {
        let scratch = scratch_dir("native-isolation");
        let fake_home = scratch.join("home");
        fs::create_dir_all(fake_home.join(".claude")).expect("home claude");
        fs::create_dir_all(fake_home.join(".gemini")).expect("home gemini");
        fs::create_dir_all(fake_home.join(".codex")).expect("home codex");

        for blocked in [
            fake_home.clone(),
            fake_home.join(".claude"),
            fake_home.join(".claude/work"),
            fake_home.join(".gemini"),
            fake_home.join(".codex"),
        ] {
            let err = validate_isolated_profile_dir(&blocked, Some(&fake_home)).unwrap_err();
            assert!(err.contains("refuses host directory"), "{err}");
        }

        let escape_link = scratch.join("sneaky-link");
        std::os::unix::fs::symlink(fake_home.join(".claude"), &escape_link).expect("symlink");
        assert!(validate_isolated_profile_dir(&escape_link, Some(&fake_home)).is_err());
        assert!(validate_isolated_profile_dir(Path::new("rel/profile"), Some(&fake_home)).is_err());

        let isolated = scratch.join("isolated/claude-work");
        let now = 1_750_000_000_i64;
        assert_eq!(
            probe_native_login(
                &isolated,
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
        .expect("write ready cred");
        assert_eq!(
            probe_native_login(
                &isolated,
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
        .expect("write expired cred");
        assert_eq!(
            probe_native_login(
                &isolated,
                NativeLoginProvider::ClaudeOauth,
                Some(&fake_home),
                now
            )
            .readiness,
            ProfileReadiness::ReauthRequired
        );

        fs::write(&cred, "{not-valid-json").expect("write bad json");
        assert_eq!(
            probe_native_login(
                &isolated,
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
            &scratch.join("isolated/agy-work"),
            Some(&fake_home),
            &|_| None,
        )
        .expect("plan agy login");
        assert_eq!(plan.engine, Engine::Agy);
        assert!(plan.env.set.contains_key("AGY_HOME"));
        assert!(plan.env.unset.contains(&"GEMINI_API_KEY".to_string()));

        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn json_and_text_reports_redact_canary_secrets_completely() {
        let scratch = scratch_dir("redact-json");
        let env_canary = "sk-env-canary-987654321-never-print";
        let file_canary = "sk-file-canary-123456789-never-print";
        let secret_file = scratch.join("relay.key");
        fs::write(&secret_file, format!("{file_canary}\n")).expect("write secret file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).expect("chmod");
        }

        let cfg_path = scratch.join("auth.toml");
        fs::write(
            &cfg_path,
            format!(
                r#"
[auth.profiles.relay-env]
mode = "relay"
base_url = "https://relay-a.example.com/v1"
engines = ["claude", "codex"]
[auth.profiles.relay-env.api_key]
from_env = "CANARY_RELAY_KEY"

[auth.profiles.relay-file]
mode = "relay"
base_url = "https://relay-b.example.com/v1"
engines = ["codex"]
[auth.profiles.relay-file.api_key]
from_file = "{secret_file}"
"#,
                secret_file = secret_file.display()
            ),
        )
        .expect("write auth.toml");

        let report = evaluate_profiles_report_with(
            &cfg_path,
            &scratch.join("profiles"),
            &|name| (name == "CANARY_RELAY_KEY").then(|| env_canary.to_string()),
            &Path::exists,
            Some(Path::new("/home/tester")),
            1_750_000_000,
        )
        .expect("evaluate report");

        assert!(report.ok);
        assert_eq!(report.summary.configured, 2);
        assert_eq!(report.summary.configured_ready, 2);

        let json_out = serde_json::to_string_pretty(&report).expect("json");
        let text_out = format_report_text(&report, "spawn auth list");
        for output in [&json_out, &text_out] {
            assert!(!output.contains(env_canary), "env canary leaked: {output}");
            assert!(
                !output.contains(file_canary),
                "file canary leaked: {output}"
            );
        }
        assert!(json_out.contains("CANARY_RELAY_KEY"));
        assert!(json_out.contains("env_preview"));

        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn cli_arg_parsing_and_relay_toml_upsert_reject_boundaries_cleanly() {
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        let key_flag_err = command(&args(&[
            "add-relay",
            "myrelay",
            "--url",
            "https://gw.example",
            "--key",
            "sk-bad",
        ]))
        .unwrap_err();
        assert!(key_flag_err.contains("forbidden"), "{key_flag_err}");
        assert!(command(&args(&["login", "opencode"])).is_err());
        assert!(command(&[]).is_err());
        assert!(
            command(&args(&["unknown"]))
                .unwrap_err()
                .contains("unknown auth subcommand")
        );

        let initial = r#"# fleet auth config
[auth.profiles.keep-local]
mode = "local"
provider = "ollama"

[auth.profiles.openrouter]
mode = "relay"
base_url = "https://old.example/v1"
engines = ["claude"]
[auth.profiles.openrouter.api_key]
from_env = "OLD_KEY"
"#;
        let updated = upsert_relay_profile_toml(
            initial,
            "openrouter",
            &RelayProfile {
                base_url: "https://openrouter.ai/api".to_string(),
                api_key: SecretRef::FromEnv {
                    from_env: "NEW_OPENROUTER_KEY".to_string(),
                },
                engines: [RelayEngine::Claude, RelayEngine::Codex]
                    .into_iter()
                    .collect(),
            },
            &ModelTier::ALL,
        )
        .expect("upsert");

        assert!(updated.contains("[auth.profiles.keep-local]"));
        assert!(!updated.contains("OLD_KEY"));
        assert!(updated.contains("NEW_OPENROUTER_KEY"));
        let reparsed =
            parse_auth_profiles_toml(&updated, Path::new("/tmp/p"), None).expect("reparse");
        assert_eq!(reparsed.len(), 2);
    }
}
