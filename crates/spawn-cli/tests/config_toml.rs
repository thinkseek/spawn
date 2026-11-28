//! Comprehensive tests for `toml_parser`, `config_file` (`SpawnConfig`,
//! `SecretRef`, env resolution, secret redaction), and the `spawn config` /
//! `spawn doctor` / `--config` CLI commands.

#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "../src/toml_parser.rs"]
mod toml_parser;

#[path = "../src/workspace_backend.rs"]
mod workspace_backend;

#[allow(dead_code)]
#[path = "../src/config_file.rs"]
mod config_file;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use config_file::{
    ConfigOrigin, SecretRef, SecretString, SpawnConfig, WorkspaceBackend, effective_json_with,
    resolve_env_entries_with,
};
use serde_json::{Value, json};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "spawn-cfg-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spawn_bin(args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let path_val = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(args)
        .env_clear()
        .env("PATH", path_val)
        .envs(extra_env.iter().copied())
        .output()
        .expect("run spawn binary")
}

fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn env_as_map(entries: Vec<(String, String)>) -> BTreeMap<String, String> {
    entries.into_iter().collect()
}

// =========================================================================
// 1. toml_parser tests
// =========================================================================

#[test]
fn toml_parser_handles_tables_arrays_inline_tables_escapes_and_comments() {
    let doc = r#"
# Top-level comment
title = "spawn\t\"v2\"\n\\ok\u0041" # trailing comment
raw = 'C:\no\escapes\#here'
count = 1_000_000
neg = -42
pos = +7
hex = 0xdead_beef
oct = 0o755
bin = 0b1010_0101
ratio = 12.75_25
exp = -2.5e-3
flag_t = true
flag_f = false

multi_arr = [
    # comment inside array
    "alpha",
    'beta', # trailing item comment
    { k = 1, v = "x" },
]

inline = { a = 1, b = "x", nested = { ok = true } }
empty_inline = {}

[section]
key = "val"
dotted.inner = 99

[section.sub]
enabled = true

[parent.child]
x = 1

# Defining parent after child header is valid in TOML 1.0
[parent]
y = 2

[[items]]
name = "first"
tags = ["a", "b"]

[items.meta]
weight = 10

[[items]]
name = "second"

[[items.sub]]
id = 101
"#;

    let val = toml_parser::parse(doc).expect("valid TOML");
    assert_eq!(val["title"], json!("spawn\t\"v2\"\n\\okA"));
    assert_eq!(val["raw"], json!("C:\\no\\escapes\\#here"));
    assert_eq!(val["count"], json!(1_000_000));
    assert_eq!(val["neg"], json!(-42));
    assert_eq!(val["pos"], json!(7));
    assert_eq!(val["hex"], json!(0xdead_beef_i64));
    assert_eq!(val["oct"], json!(0o755));
    assert_eq!(val["bin"], json!(0b1010_0101));
    assert_eq!(val["ratio"], json!(12.7525));
    assert_eq!(val["exp"], json!(-0.0025));
    assert_eq!(val["flag_t"], json!(true));
    assert_eq!(val["flag_f"], json!(false));
    assert_eq!(
        val["multi_arr"],
        json!(["alpha", "beta", {"k": 1, "v": "x"}])
    );
    assert_eq!(
        val["inline"],
        json!({"a": 1, "b": "x", "nested": {"ok": true}})
    );
    assert_eq!(val["empty_inline"], json!({}));
    assert_eq!(val["section"]["key"], json!("val"));
    assert_eq!(val["section"]["dotted"]["inner"], json!(99));
    assert_eq!(val["section"]["sub"]["enabled"], json!(true));
    assert_eq!(val["parent"], json!({"y": 2, "child": {"x": 1}}));
    assert_eq!(
        val["items"],
        json!([
            {"name": "first", "tags": ["a", "b"], "meta": {"weight": 10}},
            {"name": "second", "sub": [{"id": 101}]},
        ])
    );
}

#[test]
fn toml_parser_multiline_strings_and_quoted_keys() {
    let doc = r#"
["a.b".'c d']
text = """
hello \
  world"""
lit = '''
line1
line2'''
"#;
    let val = toml_parser::parse(doc).expect("valid multiline");
    assert_eq!(val["a.b"]["c d"]["text"], json!("hello world"));
    assert_eq!(val["a.b"]["c d"]["lit"], json!("line1\nline2"));
}

#[test]
fn toml_parser_reports_line_col_on_duplicates_and_malformed_syntax() {
    let cases = [
        ("a = 1\na = 2\n", 2, 1, "duplicate key"),
        ("[sec]\na = 1\n[sec]\nb = 2\n", 3, 1, "duplicate table"),
        ("a.b = 1\n[a]\nc = 2\n", 2, 1, "dotted"),
        ("[a]\nb = 1\n[a.c]\nd = 2\n", 0, 0, ""), // valid case checked separately? wait: let's keep only invalid here!
    ];
    // Test valid case separately
    assert!(toml_parser::parse(cases[3].0).is_ok());

    let error_cases = [
        ("a = 1\na = 2\n", 2, 1, "duplicate key"),
        ("[sec]\na = 1\n[sec]\nb = 2\n", 3, 1, "duplicate table"),
        ("a.b = 1\n[a]\nc = 2\n", 2, 1, "dotted"),
        ("a = 1\n[a]\nb = 2\n", 2, 1, "value"),
        ("a = { x = 1 }\n[a.y]\nz = 2\n", 2, 1, "inline"),
        ("x = { a = 1, }\n", 1, 14, "trailing comma"),
        ("x = \"unterminated\n", 1, 18, "newline"),
        ("x = \"unterminated", 1, 5, "unterminated"),
        ("x = \"bad\\q\"\n", 1, 9, "escape"),
        ("x = 01\n", 1, 5, "leading zero"),
        ("[unclosed\n", 1, 10, "expected ']'"),
        ("missing_eq 123\n", 1, 12, "expected '='"),
    ];

    for (input, exp_line, exp_col, needle) in error_cases {
        let err = toml_parser::parse(input).expect_err(&format!("must fail: {input:?}"));
        assert_eq!(err.line, exp_line, "line mismatch for {input:?}: {err}");
        assert_eq!(err.col, exp_col, "col mismatch for {input:?}: {err}");
        assert!(
            err.to_string().contains(&format!("{exp_line}:{exp_col}:"))
                && err.to_string().to_lowercase().contains(needle),
            "unexpected error for {input:?}: {err}"
        );
    }
}

// =========================================================================
// 2. SpawnConfig validation, SecretRef, redaction, and env mapping tests
// =========================================================================

#[test]
fn spawn_config_rejects_unknown_fields_and_bad_schema_version() {
    let wrong_version = r#"
schema_version = 2

[instance]
data_dir = "/srv/spawn"
"#;
    let err = SpawnConfig::from_toml_str(wrong_version).expect_err("schema_version != 1");
    assert!(err.contains("schema_version"), "{err}");

    let unknown_top = r#"
schema_version = 1
surprise = true

[instance]
data_dir = "/srv/spawn"
"#;
    let err = SpawnConfig::from_toml_str(unknown_top).expect_err("unknown top field");
    assert!(err.contains("unknown field"), "{err}");

    let unknown_nested = r#"
schema_version = 1

[instance]
data_dir = "/srv/spawn"

[sandbox]
mode = "required"
extra_flag = 123
"#;
    let err = SpawnConfig::from_toml_str(unknown_nested).expect_err("unknown nested field");
    assert!(err.contains("unknown field"), "{err}");

    let unknown_mount = r#"
schema_version = 1

[instance]
data_dir = "/srv/spawn"

[[sandbox.writable_mounts]]
host = "/srv/shared"
mode = "rw"
extra = "nope"
"#;
    let err = SpawnConfig::from_toml_str(unknown_mount).expect_err("unknown mount field");
    assert!(err.contains("unknown field"), "{err}");
}

#[test]
fn spawn_config_rejects_plaintext_secrets_with_clear_error() {
    for snippet in [
        r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[database]
url = "postgres://spawn:plaintext@127.0.0.1:5432/spawn"
"#,
        r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[database]
host = "127.0.0.1"
name = "spawn"
user = "spawn"
password = "plaintext-password"
"#,
        r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[server]
auth = "token"
token = "plaintext-token"
"#,
    ] {
        let err = SpawnConfig::from_toml_str(snippet).expect_err("plaintext secret rejected");
        assert!(
            err.contains("from_env")
                && err.contains("from_file")
                && err.contains("never plaintext"),
            "missing helpful secret error: {err}"
        );
    }
}

#[test]
fn spawn_config_rejects_relative_and_bare_system_data_dirs() {
    for bad in [
        "relative/dir",
        ".",
        "spawn",
        "/",
        "/tmp",
        "/var",
        "/var/lib",
        "/var/tmp",
        "/home",
        "/root",
        "/etc",
        "/usr",
        "/opt",
        "/srv",
        "/srv/spawn/../other",
    ] {
        let toml = format!("schema_version = 1\n[instance]\ndata_dir = \"{bad}\"\n");
        let err = SpawnConfig::from_toml_str(&toml).expect_err(&format!("reject {bad}"));
        assert!(err.contains("data_dir"), "bad={bad}: {err}");
    }
}

#[test]
fn spawn_config_validates_terminal_shards_range() {
    for bad in [0usize, 65] {
        let toml = format!(
            "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn\"\n[terminal]\nshards = {bad}\n"
        );
        let err = SpawnConfig::from_toml_str(&toml).expect_err(&format!("reject shards={bad}"));
        assert!(err.contains("shards"), "bad={bad}: {err}");
    }
    let ok = SpawnConfig::from_toml_str(
        "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn\"\n[terminal]\nshards = 16\n",
    )
    .expect("accept valid shards");
    assert_eq!(ok.terminal.shards, Some(16));
}

fn build_full_config_fixture(dir: &Path) -> (SpawnConfig, PathBuf, PathBuf) {
    let pw_file = dir.join("pg.pw");
    fs::write(&pw_file, "S3cr3tPgPass!\r\n").expect("write pw file");
    fs::set_permissions(&pw_file, fs::Permissions::from_mode(0o600)).expect("chmod");

    let tok_file = dir.join("serve.tok");
    fs::write(&tok_file, "Tok3nValueXYZ\n").expect("write token file");

    let toolchain_bin = dir.join("bin");
    fs::create_dir_all(&toolchain_bin).expect("bin dir");
    let fake_herdr = toolchain_bin.join("herdr");
    fs::write(&fake_herdr, "#!/bin/sh\nexit 0\n").expect("write fake herdr");
    fs::set_permissions(&fake_herdr, fs::Permissions::from_mode(0o755)).expect("chmod herdr");

    let data_dir = dir.join("instance-data");
    let toml = format!(
        r#"
schema_version = 1

[instance]
name = "alpha"
data_dir = "{data_dir}"
default_repo = "/srv/repos/main.git"

[database]
host = "db.internal"
port = 5433
name = "spawn_prod"
user = "spawn_user"
password = {{ from_file = "{pw_file}" }}

[server]
listen = "127.0.0.1:9090"
auth = "local-open"
token = {{ from_file = "{tok_file}" }}
web_dir = "/srv/spawn/web"

[storage]
workspace_backend = "cow"
canonical_mount = "/workspace"
cas_dedup = true
max_checkpoint_file_mb = 250

[terminal]
backend = "herdr"
shards = 16
toolchain_paths = ["{toolchain_bin}"]

[network]
http_proxy = "http://127.0.0.1:3128"
https_proxy = "http://127.0.0.1:3129"
no_proxy = ["localhost", "127.0.0.1", ".internal"]

[sandbox]
mode = "required"
home_isolation = "whitelist"
auto_approve = "startup"

[[sandbox.writable_mounts]]
host = "/srv/cache"
mode = "rw"

[[sandbox.writable_mounts]]
host = "/srv/models"
mode = "ro"

[engines.claude]
enabled = true
config_dir = "~/.claude-spawn"

[engines.agy]
enabled = true
default_model = "gemini-2.5-pro"
auth_source = "~/.gemini/antigravity-cli"
isolate_workers = true
isolate_brain = true

[advanced]
loop_interval_ms = 250
screen_quiet_ms = 400
message_taken_secs = 90
"#,
        data_dir = data_dir.display(),
        pw_file = pw_file.display(),
        tok_file = tok_file.display(),
        toolchain_bin = toolchain_bin.display(),
    );

    let cfg = SpawnConfig::from_toml_str(&toml).expect("parse full config");
    (cfg, data_dir, toolchain_bin)
}

#[test]
fn spawn_config_resolves_core_storage_terminal_and_network_env() {
    let dir = TempDir::new("resolve-core");
    let (cfg, data_dir, toolchain_bin) = build_full_config_fixture(dir.path());
    let fake_env = env_map(&[("HOME", "/home/alice"), ("PATH", "/usr/bin:/bin")]);
    let resolved = env_as_map(
        resolve_env_entries_with(&cfg, &|k| fake_env.get(k).cloned()).expect("resolve env"),
    );

    assert_eq!(
        resolved["SPAWN_DATABASE_URL"],
        "postgres://spawn_user:S3cr3tPgPass%21@db.internal:5433/spawn_prod"
    );
    assert_eq!(
        resolved["SPAWN_WORKSPACE_ROOT"],
        data_dir.join("workspaces").display().to_string()
    );
    assert_eq!(resolved["SPAWN_GIT_REPO"], "/srv/repos/main.git");
    assert_eq!(resolved["SPAWN_SERVE_ADDRESS"], "127.0.0.1:9090");
    assert_eq!(resolved["SPAWN_SERVE_LOCAL_OPEN"], "1");
    assert_eq!(resolved["SPAWN_SERVE_TOKEN"], "Tok3nValueXYZ");
    assert_eq!(resolved["SPAWN_WEB_DIR"], "/srv/spawn/web");
    assert_eq!(resolved["SPAWN_WORKSPACE_BACKEND"], "cow");
    assert_eq!(resolved["SPAWN_CAS_DEDUP"], "1");
    assert_eq!(resolved["SPAWN_MAX_FILE_MB"], "250");
    assert_eq!(resolved["SPAWN_TERMINAL_BACKEND"], "herdr");
    assert_eq!(resolved["SPAWN_HERDR_SESSION"], "spawn-alpha");
    assert_eq!(resolved["SPAWN_HERDR_SHARDS"], "16");
    assert_eq!(
        resolved["SPAWN_HERDR_PROGRAM"],
        toolchain_bin.join("herdr").display().to_string()
    );
    assert_eq!(
        resolved["PATH"],
        format!("{}:/usr/bin:/bin", toolchain_bin.display())
    );
    assert_eq!(resolved["http_proxy"], "http://127.0.0.1:3128");
    assert_eq!(resolved["HTTP_PROXY"], "http://127.0.0.1:3128");
    assert_eq!(resolved["https_proxy"], "http://127.0.0.1:3129");
    assert_eq!(resolved["HTTPS_PROXY"], "http://127.0.0.1:3129");
    assert_eq!(resolved["no_proxy"], "localhost,127.0.0.1,.internal");
    assert_eq!(resolved["NO_PROXY"], "localhost,127.0.0.1,.internal");
}

#[test]
fn spawn_config_resolves_engines_sandbox_xdg_and_redacts_secrets() {
    let dir = TempDir::new("resolve-engines");
    let (cfg, data_dir, _) = build_full_config_fixture(dir.path());
    let fake_env = env_map(&[("HOME", "/home/alice"), ("PATH", "/usr/bin:/bin")]);
    let lookup = |k: &str| fake_env.get(k).cloned();
    let resolved = env_as_map(resolve_env_entries_with(&cfg, &lookup).expect("resolve env"));

    assert_eq!(resolved["SPAWN_ENGINES"], "claude,agy");
    assert_eq!(
        resolved["SPAWN_AGY_HOME"],
        "/home/alice/.gemini/antigravity-cli"
    );
    assert_eq!(
        resolved["SPAWN_AGY_DATA_DIR"],
        data_dir.join("engines/agy").display().to_string()
    );
    assert_eq!(resolved["SPAWN_AGY_MODEL"], "gemini-2.5-pro");
    assert_eq!(resolved["SPAWN_AGY_ISOLATE_WORKERS"], "1");
    assert_eq!(resolved["SPAWN_AGY_ISOLATE_BRAIN"], "1");
    assert_eq!(
        resolved["SPAWN_CLAUDE_CONFIG_DIR"],
        "/home/alice/.claude-spawn"
    );
    assert_eq!(resolved["SPAWN_ENGINE_VIEW"], "required");
    assert_eq!(
        resolved["SPAWN_ENGINE_VIEW_WRITABLE"],
        "/srv/cache,/srv/models:ro"
    );
    assert_eq!(resolved["SPAWN_SANDBOX_CANONICAL_WORKSPACE"], "/workspace");
    assert_eq!(resolved["SPAWN_SANDBOX_HOME_ISOLATION"], "whitelist");
    assert_eq!(resolved["SPAWN_AUTO_APPROVE"], "startup");
    assert_eq!(resolved["SPAWN_LOOP_INTERVAL_MS"], "250");
    assert_eq!(resolved["SPAWN_SCREEN_QUIET_MS"], "400");
    assert_eq!(resolved["SPAWN_MESSAGE_TAKEN_SECS"], "90");
    assert_eq!(
        resolved["TMPDIR"],
        data_dir.join("tmp").display().to_string()
    );
    assert_eq!(
        resolved["XDG_CONFIG_HOME"],
        data_dir.join("xdg-config").display().to_string()
    );
    assert_eq!(
        resolved["XDG_CACHE_HOME"],
        data_dir.join("xdg-cache").display().to_string()
    );

    let eff = effective_json_with(&cfg, &lookup).expect("effective json");
    let eff_text = eff.to_string();
    assert!(!eff_text.contains("S3cr3tPgPass"), "{eff_text}");
    assert!(!eff_text.contains("Tok3nValueXYZ"), "{eff_text}");
    assert_eq!(
        eff["env"]["SPAWN_DATABASE_URL"],
        json!("postgres://spawn_user:***@db.internal:5433/spawn_prod")
    );
    assert_eq!(eff["env"]["SPAWN_SERVE_TOKEN"], json!("***"));
}

#[test]
fn from_env_secret_and_empty_from_file_are_handled_cleanly() {
    let dir = TempDir::new("empty-secret");
    let empty_file = dir.path().join("empty.secret");
    fs::write(&empty_file, "\r\n").expect("write empty secret");

    let toml_env = r#"
schema_version = 1

[instance]
data_dir = "/srv/spawn-instance"

[database]
url = { from_env = "MY_DB_URL" }
"#;
    let cfg = SpawnConfig::from_toml_str(toml_env).expect("parse from_env");
    let present = env_map(&[(
        "MY_DB_URL",
        "postgres://u:TopSecretFromEnv@localhost:5432/db",
    )]);
    let resolved =
        env_as_map(resolve_env_entries_with(&cfg, &|k| present.get(k).cloned()).expect("resolve"));
    assert_eq!(
        resolved["SPAWN_DATABASE_URL"],
        "postgres://u:TopSecretFromEnv@localhost:5432/db"
    );

    let eff = effective_json_with(&cfg, &|k| present.get(k).cloned()).expect("eff");
    assert!(!eff.to_string().contains("TopSecretFromEnv"), "{eff}");
    assert_eq!(
        eff["env"]["SPAWN_DATABASE_URL"],
        json!("postgres://u:***@localhost:5432/db")
    );

    // Unset env var fails cleanly
    let missing_err = resolve_env_entries_with(&cfg, &|_| None).expect_err("missing env");
    assert!(missing_err.contains("MY_DB_URL"), "{missing_err}");

    // Empty secret file fails cleanly
    let toml_empty = format!(
        "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn-instance\"\n[server]\nauth = \"token\"\ntoken = {{ from_file = \"{}\" }}\n",
        empty_file.display()
    );
    let cfg_empty = SpawnConfig::from_toml_str(&toml_empty).expect("parse");
    let empty_err = resolve_env_entries_with(&cfg_empty, &|_| None).expect_err("empty file");
    assert!(empty_err.contains("empty"), "{empty_err}");
}

// =========================================================================
// 3. End-to-end binary tests (CARGO_BIN_EXE_spawn)
// =========================================================================

struct CliFixture {
    dir: TempDir,
    data_dir: PathBuf,
    cfg_path: PathBuf,
    db_secret: &'static str,
    token_secret: &'static str,
}

impl CliFixture {
    fn new(name: &str) -> Self {
        let dir = TempDir::new(name);
        let data_dir = dir.path().join("spawn-data-uncreated");
        let secret_file = dir.path().join("db.secret");
        let db_secret = "UltraSecretDbPass_98765";
        let token_secret = "UltraSecretServeToken_54321";
        fs::write(&secret_file, format!("{db_secret}\r\n")).expect("write secret file");
        fs::set_permissions(&secret_file, fs::Permissions::from_mode(0o600)).expect("chmod");

        let cfg_path = dir.path().join("spawn.toml");
        let toml = format!(
            r#"
schema_version = 1

[instance]
name = "e2e"
data_dir = "{data_dir}"

[database]
host = "127.0.0.1"
port = 1
name = "spawn_e2e"
user = "spawn"
password = {{ from_file = "{secret_file}" }}

[server]
listen = "127.0.0.1:8888"
auth = "token"
token = {{ from_env = "E2E_SERVE_TOKEN" }}

[engines.agy]
enabled = true
default_model = "gemini-2.5-flash"
"#,
            data_dir = data_dir.display(),
            secret_file = secret_file.display(),
        );
        fs::write(&cfg_path, toml).expect("write spawn.toml");
        Self {
            dir,
            data_dir,
            cfg_path,
            db_secret,
            token_secret,
        }
    }

    fn cfg_str(&self) -> &str {
        self.cfg_path.to_str().expect("utf-8")
    }
}

#[test]
fn cli_config_check_and_effective_redact_secrets() {
    let fx = CliFixture::new("e2e-check");

    let check_out = spawn_bin(
        &["--config", fx.cfg_str(), "config", "check", "--json"],
        &[("E2E_SERVE_TOKEN", fx.token_secret)],
    );
    let check_stdout = String::from_utf8_lossy(&check_out.stdout);
    let check_stderr = String::from_utf8_lossy(&check_out.stderr);
    assert_eq!(
        check_out.status.code(),
        Some(0),
        "stdout={check_stdout}\nstderr={check_stderr}"
    );
    let check_json: Value = serde_json::from_str(&check_stdout).expect("check JSON");
    assert_eq!(check_json["ok"], json!(true), "{check_json}");
    assert_eq!(check_json["errors"], json!([]), "{check_json}");
    assert!(
        !fx.data_dir.exists(),
        "config check must stay read-only and not create data_dir"
    );

    let eff_out = spawn_bin(
        &["--config", fx.cfg_str(), "config", "effective", "--json"],
        &[("E2E_SERVE_TOKEN", fx.token_secret)],
    );
    let eff_stdout = String::from_utf8_lossy(&eff_out.stdout);
    let eff_stderr = String::from_utf8_lossy(&eff_out.stderr);
    assert_eq!(
        eff_out.status.code(),
        Some(0),
        "stdout={eff_stdout}\nstderr={eff_stderr}"
    );
    assert!(!eff_stdout.contains(fx.db_secret) && !eff_stderr.contains(fx.db_secret));
    assert!(!eff_stdout.contains(fx.token_secret) && !eff_stderr.contains(fx.token_secret));
    let eff_json: Value = serde_json::from_str(&eff_stdout).expect("effective JSON");
    assert_eq!(
        eff_json["env"]["SPAWN_DATABASE_URL"],
        json!("postgres://spawn:***@127.0.0.1:1/spawn_e2e")
    );
    assert_eq!(eff_json["env"]["SPAWN_SERVE_TOKEN"], json!("***"));
    assert_eq!(
        eff_json["env"]["SPAWN_SANDBOX_CANONICAL_WORKSPACE"],
        json!("/workspace")
    );
    assert_eq!(
        eff_json["env"]["SPAWN_SANDBOX_HOME_ISOLATION"],
        json!("whitelist")
    );
    assert_eq!(
        eff_json["env"]["SPAWN_AGY_DATA_DIR"],
        json!(fx.data_dir.join("engines/agy").display().to_string())
    );
    assert_eq!(eff_json["env"]["SPAWN_AGY_ISOLATE_BRAIN"], json!("1"));
}

#[test]
fn cli_doctor_init_and_global_config_exec() {
    let fx = CliFixture::new("e2e-doctor");

    let doc_out = spawn_bin(
        &["--config", fx.cfg_str(), "doctor", "--json"],
        &[("E2E_SERVE_TOKEN", fx.token_secret)],
    );
    let doc_stdout = String::from_utf8_lossy(&doc_out.stdout);
    let doc_stderr = String::from_utf8_lossy(&doc_out.stderr);
    assert_eq!(
        doc_out.status.code(),
        Some(0),
        "stdout={doc_stdout}\nstderr={doc_stderr}"
    );
    let doc_json: Value = serde_json::from_str(&doc_stdout).expect("doctor JSON");
    assert!(
        doc_json["kernel"]["release"].as_str().is_some(),
        "{doc_json}"
    );
    assert!(doc_json["tools"]["git"].is_object(), "{doc_json}");
    assert!(doc_json["tools"]["herdr"].is_object(), "{doc_json}");
    assert!(doc_json["tools"]["bwrap"].is_object(), "{doc_json}");
    assert!(doc_json["tools"]["zstd"].is_object(), "{doc_json}");
    assert!(doc_json["filesystems"]["root"].is_object(), "{doc_json}");
    assert_eq!(
        doc_json["storage_probes"]["probed"],
        json!(true),
        "{doc_json}"
    );

    let init_data_dir = fx.dir.path().join("init-data");
    let init_toml = fx.dir.path().join("generated.toml");
    let init_out = spawn_bin(
        &[
            "config",
            "init",
            "--data-dir",
            init_data_dir.to_str().expect("utf-8"),
            "--name",
            "prod1",
            "--output",
            init_toml.to_str().expect("utf-8"),
        ],
        &[],
    );
    assert_eq!(init_out.status.code(), Some(0));
    let check_init = spawn_bin(
        &[
            "config",
            "check",
            "--config",
            init_toml.to_str().expect("utf-8"),
            "--json",
        ],
        &[],
    );
    assert_eq!(check_init.status.code(), Some(0));
    let init_rendered = fs::read_to_string(&init_toml).expect("read init toml");
    assert!(
        init_rendered.contains("listen = \"127.0.0.1:8787\""),
        "{init_rendered}"
    );
    assert!(
        init_rendered.contains("auto_approve = \"startup\""),
        "{init_rendered}"
    );

    let fleet_out = spawn_bin(
        &["--config", fx.cfg_str(), "fleet"],
        &[("E2E_SERVE_TOKEN", fx.token_secret)],
    );
    let fleet_stderr = String::from_utf8_lossy(&fleet_out.stderr);
    assert_eq!(fleet_out.status.code(), Some(1), "{fleet_stderr}");
    assert!(
        !fleet_stderr.contains("SPAWN_DATABASE_URL is not set"),
        "expected --config to inject SPAWN_DATABASE_URL before fleet ran: {fleet_stderr}"
    );

    let bad_cfg = fx.dir.path().join("bad.toml");
    fs::write(
        &bad_cfg,
        "schema_version = 1\n[instance]\ndata_dir = \"/tmp\"\n",
    )
    .expect("bad");
    let bad_out = spawn_bin(
        &[
            "config",
            "check",
            "--config",
            bad_cfg.to_str().expect("utf-8"),
            "--json",
        ],
        &[],
    );
    assert_eq!(bad_out.status.code(), Some(1));
    let bad_json: Value =
        serde_json::from_str(&String::from_utf8_lossy(&bad_out.stdout)).expect("bad check JSON");
    assert_eq!(bad_json["ok"], json!(false));
    assert!(!bad_json["errors"].as_array().unwrap().is_empty());

    let help_out = spawn_bin(
        &["--config", bad_cfg.to_str().expect("utf-8"), "--help"],
        &[],
    );
    assert_eq!(help_out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help_out.stdout).starts_with("usage: spawn <init-db|"));
}

#[test]
fn spawn_config_sandbox_defaults_startup_and_rejects_safe() {
    let dir = TempDir::new("sandbox-startup");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");

    let default_toml = format!(
        "schema_version = 1\n[instance]\nname = \"s1\"\ndata_dir = \"{}\"\n",
        data_dir.display()
    );
    let cfg = SpawnConfig::from_toml_str(&default_toml).expect("default config");
    assert_eq!(cfg.sandbox.auto_approve.as_deref(), Some("startup"));
    let resolved =
        env_as_map(resolve_env_entries_with(&cfg, &|_| None).expect("resolve default env"));
    assert_eq!(resolved["SPAWN_AUTO_APPROVE"], "startup");

    for valid in ["off", "startup", "all"] {
        let doc = format!("{default_toml}\n[sandbox]\nauto_approve = \"{valid}\"\n");
        let parsed = SpawnConfig::from_toml_str(&doc).expect(valid);
        assert_eq!(parsed.sandbox.auto_approve.as_deref(), Some(valid));
    }

    let safe_doc = format!("{default_toml}\n[sandbox]\nauto_approve = \"safe\"\n");
    let err = SpawnConfig::from_toml_str(&safe_doc).expect_err("safe must be rejected");
    assert!(err.contains("[sandbox].auto_approve"), "{err}");
    assert!(err.contains("startup"), "{err}");
}

#[test]
fn spawn_config_rejects_proxy_userinfo_and_redacts_effective_json() {
    let dir = TempDir::new("proxy-redact");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");

    for (field, url, secret) in [
        (
            "http_proxy",
            "http://alice:Pr0xyPass!@proxy.local:3128",
            "Pr0xyPass!",
        ),
        (
            "https_proxy",
            "http://TokOnlySecret@proxy.local:3129",
            "TokOnlySecret",
        ),
    ] {
        let doc = format!(
            "schema_version = 1\n[instance]\nname = \"p\"\ndata_dir = \"{}\"\n[network]\n{field} = \"{url}\"\n",
            data_dir.display()
        );
        let err = SpawnConfig::from_toml_str(&doc).expect_err("embedded proxy creds rejected");
        assert!(err.contains(field), "{err}");
        assert!(!err.contains(secret), "error must not echo secret: {err}");
    }

    let base_doc = format!(
        "schema_version = 1\n[instance]\nname = \"p\"\ndata_dir = \"{}\"\n",
        data_dir.display()
    );
    let mut cfg = SpawnConfig::from_toml_str(&base_doc).expect("base config");
    cfg.network.http_proxy = Some("http://alice:Pr0xyPass!@proxy.local:3128".to_string());
    cfg.network.https_proxy = Some("http://TokOnlySecret@proxy.local:3129".to_string());

    let eff_err = effective_json_with(&cfg, &|_| None)
        .expect_err("effective_json_with refuses embedded userinfo");
    assert!(
        !eff_err.contains("Pr0xyPass!") && !eff_err.contains("TokOnlySecret"),
        "{eff_err}"
    );

    let masked_cfg = config_file::redacted_config_for_display(&cfg);
    assert_eq!(
        masked_cfg.network.http_proxy.as_deref(),
        Some("http://alice:***@proxy.local:3128")
    );
    assert_eq!(
        masked_cfg.network.https_proxy.as_deref(),
        Some("http://***@proxy.local:3129")
    );

    cfg.network.http_proxy = Some("http://proxy.local:3128?password=QueryPass99".to_string());
    cfg.network.https_proxy = Some("http://proxy.local:3129?token=QueryTok88".to_string());
    let eff = effective_json_with(&cfg, &|_| None).expect("effective json");
    let eff_text = eff.to_string();
    assert!(!eff_text.contains("QueryPass99"), "{eff_text}");
    assert!(!eff_text.contains("QueryTok88"), "{eff_text}");
    assert_eq!(
        eff["config"]["network"]["http_proxy"],
        json!("http://proxy.local:3128?password=***")
    );
    assert_eq!(
        eff["config"]["network"]["https_proxy"],
        json!("http://proxy.local:3129?token=***")
    );
    assert_eq!(
        eff["env"]["http_proxy"],
        json!("http://proxy.local:3128?password=***")
    );
    assert_eq!(
        eff["env"]["https_proxy"],
        json!("http://proxy.local:3129?token=***")
    );
}

#[test]
fn toml_parser_rejects_excessive_nesting_depth() {
    let ok_arrays = format!("a = {}1{}", "[".repeat(16), "]".repeat(16));
    assert!(toml_parser::parse(&ok_arrays).is_ok());

    let deep_arrays = format!("a = {}1{}", "[".repeat(70), "]".repeat(70));
    let err_arr = toml_parser::parse(&deep_arrays).expect_err("deep array rejected");
    assert!(err_arr.message.contains("depth"), "{err_arr}");

    let deep_tables = format!("t = {}{}{}", "{ a = ".repeat(70), "1", " }".repeat(70));
    let err_tbl = toml_parser::parse(&deep_tables).expect_err("deep inline table rejected");
    assert!(err_tbl.message.contains("depth"), "{err_tbl}");
}

#[test]
fn sandbox_container_host_inherit_and_claude_sandbox_resolve_env() {
    let dir = TempDir::new("inherit-claude-sbx");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");

    let doc = format!(
        r#"
schema_version = 1

[instance]
name = "ctr-host"
data_dir = "{}"

[sandbox]
mode = "container"
home_isolation = "whitelist"
inherit_host = "auto"
inherit_host_paths = ["/opt/tools", "~/.local/sdk"]

[engines.claude]
enabled = true
sandbox = true
"#,
        data_dir.display()
    );
    let cfg = SpawnConfig::from_toml_str(&doc).expect("valid config");
    let entries: BTreeMap<String, String> = resolve_env_entries_with(&cfg, &|name| {
        (name == "HOME").then(|| "/home/tester".to_string())
    })
    .expect("resolve env")
    .into_iter()
    .collect();

    assert_eq!(
        entries.get("SPAWN_ENGINE_VIEW").map(String::as_str),
        Some("container")
    );
    assert_eq!(
        entries
            .get("SPAWN_SANDBOX_INHERIT_HOST")
            .map(String::as_str),
        Some("auto")
    );
    assert_eq!(
        entries
            .get("SPAWN_SANDBOX_INHERIT_HOST_PATHS")
            .map(String::as_str),
        Some("/opt/tools,/home/tester/.local/sdk")
    );
    assert_eq!(
        entries.get("SPAWN_CLAUDE_SANDBOX").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        entries.get("SPAWN_CLAUDE_DATA_DIR").map(String::as_str),
        Some(data_dir.join("engines/claude").to_string_lossy().as_ref())
    );

    let bad_inherit = format!(
        "schema_version = 1\n[instance]\ndata_dir = \"{}\"\n[sandbox]\ninherit_host = \"unsafe\"\n",
        data_dir.display()
    );
    assert!(SpawnConfig::from_toml_str(&bad_inherit).is_err());

    let bad_root = format!(
        "schema_version = 1\n[instance]\ndata_dir = \"{}\"\n[sandbox]\ninherit_host_paths = [\"/\"]\n",
        data_dir.display()
    );
    assert!(SpawnConfig::from_toml_str(&bad_root).is_err());
}

#[test]
fn cli_config_init_from_host_docker_plan_and_doctor_emit_valid_outputs() {
    let dir = TempDir::new("cli-host-docker");
    let fake_home = dir.path().join("home");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(fake_home.join(".rustup/toolchains/stable")).expect("rustup");
    fs::create_dir_all(fake_home.join(".cargo/bin")).expect("cargo bin");
    fs::write(
        fake_home.join(".cargo/config.toml"),
        "[net]\noffline = true\n",
    )
    .expect("cargo config");
    fs::write(
        fake_home.join(".gitconfig"),
        "[user]\n\tname = Ada\n\temail = ada@example.com\n",
    )
    .expect("gitconfig");
    fs::create_dir_all(fake_home.join(".ssh")).expect("ssh");
    fs::write(fake_home.join(".ssh/id_ed25519"), "SECRET_SSH_KEY").expect("ssh key");
    fs::create_dir_all(&data_dir).expect("data dir");

    let cfg_out = dir.path().join("spawn.from-host.toml");
    let init_out = Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args([
            "config",
            "init",
            "--from-host",
            "--data-dir",
            &data_dir.display().to_string(),
            "--name",
            "host-test",
            "--output",
            &cfg_out.display().to_string(),
        ])
        .env("HOME", &fake_home)
        .output()
        .expect("run config init --from-host");
    assert!(
        init_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let generated = fs::read_to_string(&cfg_out).expect("read generated toml");
    let parsed_cfg = SpawnConfig::from_toml_str(&generated).expect("generated toml validates");
    assert_eq!(parsed_cfg.sandbox.inherit_host.as_deref(), Some("auto"));

    let plan_out = Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args([
            "config",
            "docker-plan",
            "--config",
            &cfg_out.display().to_string(),
            "--json",
        ])
        .env("HOME", &fake_home)
        .output()
        .expect("run config docker-plan --json");
    assert!(
        plan_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&plan_out.stderr)
    );
    let plan_json: Value = serde_json::from_slice(&plan_out.stdout).expect("docker-plan JSON");
    assert_eq!(plan_json["ok"], json!(true));
    assert_eq!(plan_json["env"]["SPAWN_ENGINE_VIEW"], json!("container"));
    let ro = plan_json["readonly_mounts"].as_array().expect("ro mounts");
    assert!(
        ro.iter()
            .any(|m| m["container"] == "/home/spawn/.gitconfig"),
        "safe .gitconfig mounted: {ro:?}"
    );
    assert!(
        !ro.iter()
            .any(|m| m["container"].as_str().is_some_and(|s| s.contains(".ssh"))),
        ".ssh must never appear in docker-plan mounts: {ro:?}"
    );

    let doc_out = Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args([
            "doctor",
            "--json",
            "--root",
            &data_dir.display().to_string(),
        ])
        .env("HOME", &fake_home)
        .output()
        .expect("run doctor --json");
    assert!(
        doc_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&doc_out.stderr)
    );
    let doc_json: Value = serde_json::from_slice(&doc_out.stdout).expect("doctor JSON");
    assert!(doc_json.get("container").is_some());
    assert!(doc_json.get("bwrap_capability").is_some());
    assert!(doc_json.get("host_inherit").is_some());
    assert!(doc_json.get("archive_mask_tool").is_some());
    assert!(doc_json.get("model_endpoints").is_some());
    assert_eq!(doc_json["probe_root_origin"], json!("flag"));
    assert_eq!(doc_json["config_path_origin"], json!(null));
}

#[test]
fn cli_doctor_reports_config_and_probe_root_origins() {
    let dir = TempDir::new("doctor-origins");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let cfg_path = dir.path().join("spawn.toml");
    fs::write(
        &cfg_path,
        format!(
            "schema_version = 1\n[instance]\ndata_dir = \"{}\"\n",
            data_dir.display()
        ),
    )
    .expect("write cfg");

    let doc_cfg = spawn_bin(
        &[
            "doctor",
            "--config",
            &cfg_path.display().to_string(),
            "--json",
        ],
        &[],
    );
    assert!(
        doc_cfg.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&doc_cfg.stderr)
    );
    let doc_cfg_json: Value = serde_json::from_slice(&doc_cfg.stdout).expect("doctor cfg JSON");
    assert_eq!(doc_cfg_json["config_path_origin"], json!("flag"));
    assert_eq!(doc_cfg_json["probe_root_origin"], json!("file"));

    let doc_env = spawn_bin(
        &["doctor", "--json"],
        &[("SPAWN_CONFIG", &cfg_path.display().to_string())],
    );
    assert!(
        doc_env.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&doc_env.stderr)
    );
    let doc_env_json: Value = serde_json::from_slice(&doc_env.stdout).expect("doctor env JSON");
    assert_eq!(doc_env_json["config_path_origin"], json!("env"));
    assert_eq!(doc_env_json["probe_root_origin"], json!("file"));
}

#[test]
fn cli_doctor_reports_model_endpoints_unconfigured() {
    let dir = TempDir::new("doctor-model-unconfigured");
    let root_str = dir.path().display().to_string();

    let out = spawn_bin(&["doctor", "--json", "--root", &root_str], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );

    let report: Value = serde_json::from_str(&stdout).expect("unconfigured doctor JSON");
    assert_eq!(
        report["model_endpoints"]["model_relay"]["status"],
        json!("not_configured")
    );
    assert_eq!(
        report["model_endpoints"]["judge"]["status"],
        json!("not_configured")
    );
    assert!(report["model_endpoints"]["model_relay"]["latency_ms"].is_null());
    assert!(report["model_endpoints"]["judge"]["latency_ms"].is_null());
}

#[test]
fn cli_doctor_reports_model_endpoints_ok_with_live_loopback_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    let dir = TempDir::new("doctor-model-ok");
    let root_str = dir.path().display().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback endpoint");
    let ok_port = listener.local_addr().expect("local addr").port();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept doctor probe");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = stream.flush();
        }
    });

    let endpoint_url = format!("http://127.0.0.1:{ok_port}");
    let out = spawn_bin(
        &["doctor", "--json", "--root", &root_str],
        &[
            ("SPAWN_MODEL_RELAY", &endpoint_url),
            ("SPAWN_JUDGE_URL", &endpoint_url),
            ("SPAWN_DEFAULT_MODEL", "claude-opus-5-5"),
            ("SPAWN_CHEAP_MODEL", "deepseek-v4-pro"),
            ("SPAWN_SMALL_MODEL", "deepseek-flash"),
        ],
    );
    server.join().expect("join mock server");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );

    let report: Value = serde_json::from_str(&stdout).expect("ok doctor JSON");
    assert_eq!(
        report["model_endpoints"]["model_relay"]["status"],
        json!("ok")
    );
    assert_eq!(
        report["model_endpoints"]["model_relay"]["http_status"],
        json!(200)
    );
    assert_eq!(
        report["model_endpoints"]["model_relay"]["target"],
        json!(format!("127.0.0.1:{ok_port}"))
    );
    assert!(
        report["model_endpoints"]["model_relay"]["latency_ms"]
            .as_u64()
            .is_some()
    );
    assert_eq!(report["model_endpoints"]["judge"]["status"], json!("ok"));
    assert_eq!(
        report["model_endpoints"]["judge"]["http_status"],
        json!(200)
    );
    assert!(
        report["model_endpoints"]["judge"]["latency_ms"]
            .as_u64()
            .is_some()
    );
    assert_eq!(
        report["model_endpoints"]["model_slots"]["default"],
        json!("claude-opus-5-5")
    );
    assert_eq!(
        report["model_endpoints"]["model_slots"]["cheap"],
        json!("deepseek-v4-pro")
    );
    assert_eq!(
        report["model_endpoints"]["model_slots"]["small"],
        json!("deepseek-flash")
    );
}

#[test]
fn cli_doctor_reports_model_endpoints_unreachable_without_credential_leak() {
    use std::net::TcpListener;

    let dir = TempDir::new("doctor-model-unreachable");
    let root_str = dir.path().display().to_string();
    let closed_listener = TcpListener::bind("127.0.0.1:0").expect("bind closed port");
    let closed_port = closed_listener.local_addr().expect("closed addr").port();
    drop(closed_listener);

    let secret_relay = format!(
        "http://relayuser:RelaySecretPass99@127.0.0.1:{closed_port}/v1?api_key=RelayQueryTok88"
    );
    let secret_judge = format!(
        "http://judgeuser:JudgeSecretPass77@127.0.0.1:{closed_port}/v1/systemone?token=JudgeQueryTok66"
    );
    let env = [
        ("SPAWN_MODEL_RELAY", secret_relay.as_str()),
        ("SPAWN_JUDGE_URL", secret_judge.as_str()),
    ];

    let down_json_out = spawn_bin(&["doctor", "--json", "--root", &root_str], &env);
    let down_stdout = String::from_utf8_lossy(&down_json_out.stdout);
    let down_stderr = String::from_utf8_lossy(&down_json_out.stderr);
    assert_eq!(
        down_json_out.status.code(),
        Some(0),
        "stdout={down_stdout}\nstderr={down_stderr}"
    );
    let down_json: Value = serde_json::from_str(&down_stdout).expect("down doctor JSON");

    for slot in ["model_relay", "judge"] {
        assert_eq!(
            down_json["model_endpoints"][slot]["status"],
            json!("unreachable"),
            "{down_json}"
        );
        assert_eq!(
            down_json["model_endpoints"][slot]["target"],
            json!(format!("127.0.0.1:{closed_port}"))
        );
        assert!(
            down_json["model_endpoints"][slot]["remediation"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "{down_json}"
        );
    }
    let warnings = down_json["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .is_some_and(|s| s.contains("model_relay") && s.contains("unreachable"))),
        "{warnings:?}"
    );

    let down_human_out = spawn_bin(&["doctor", "--root", &root_str], &env);
    assert_eq!(down_human_out.status.code(), Some(0));
    let human_stdout = String::from_utf8_lossy(&down_human_out.stdout);
    let human_stderr = String::from_utf8_lossy(&down_human_out.stderr);
    assert!(
        human_stdout.contains("model_endpoints:")
            && human_stdout.contains("unreachable")
            && human_stdout.contains("remediation:"),
        "{human_stdout}"
    );

    let all_outputs = format!("{down_stdout}\n{down_stderr}\n{human_stdout}\n{human_stderr}");
    for secret in [
        "RelaySecretPass99",
        "RelayQueryTok88",
        "relayuser",
        "JudgeSecretPass77",
        "JudgeQueryTok66",
        "judgeuser",
    ] {
        assert!(
            !all_outputs.contains(secret),
            "credential {secret:?} leaked:\n{all_outputs}"
        );
    }
}

#[test]
fn config_toml_rejects_removed_tmux_terminal_backend() {
    let dir = std::env::temp_dir().join(format!("spawn-config-toml-tmux-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let cfg = dir.join("spawn.toml");
    std::fs::write(
        &cfg,
        "schema_version = 1\n\n[instance]\ndata_dir = \"/srv/spawn-instance\"\n\n[terminal]\nbackend = \"tmux\"\n",
    )
    .expect("write config");
    let out = Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(["config", "check", "--config", &cfg.display().to_string()])
        .output()
        .expect("run config check");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.status.success(),
        "config check must reject backend = \"tmux\""
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tmux"), "{stderr}");
    assert!(
        stderr.contains("removed") || stderr.contains("no longer supported"),
        "{stderr}"
    );
}

#[test]
fn spawn_config_accepts_d6_backends_and_projects_storage_d6_config() {
    for backend in ["d6", "d6-shadow"] {
        let toml = format!(
            r#"
schema_version = 1

[instance]
name = "d6-test"
data_dir = "/srv/spawn-d6"

[storage]
workspace_backend = "{backend}"

[storage.d6]
cas_dir = "/var/lib/spawn/d6-cas"
tmpfs_root = "/dev/shm/spawn-d6-l1"
tmpfs_budget_mb = 4096
strict_fail_closed = true
"#
        );
        let cfg = SpawnConfig::from_toml_str(&toml)
            .unwrap_or_else(|e| panic!("accept backend {backend}: {e}"));
        let d6 = cfg.storage.d6.as_ref().expect("storage.d6 populated");
        assert_eq!(
            d6.cas_dir.as_deref(),
            Some(Path::new("/var/lib/spawn/d6-cas"))
        );
        assert_eq!(
            d6.tmpfs_root.as_deref(),
            Some(Path::new("/dev/shm/spawn-d6-l1"))
        );
        assert_eq!(d6.tmpfs_budget_mb, Some(4096));
        assert_eq!(d6.strict_fail_closed, Some(true));

        let env = env_as_map(resolve_env_entries_with(&cfg, &|_| None).expect("resolve env"));
        assert_eq!(env["SPAWN_WORKSPACE_BACKEND"], backend);
        assert_eq!(env["SPAWN_D6_CAS_DIR"], "/var/lib/spawn/d6-cas");
        assert_eq!(env["SPAWN_D6_TMPFS_ROOT"], "/dev/shm/spawn-d6-l1");
        assert_eq!(env["SPAWN_D6_TMPFS_BUDGET_MB"], "4096");
        assert_eq!(env["SPAWN_D6_STRICT_FAIL_CLOSED"], "1");

        let eff = effective_json_with(&cfg, &|_| None).expect("effective json");
        assert_eq!(
            eff["config"]["storage"]["workspace_backend"],
            json!(backend)
        );
        assert_eq!(
            eff["config"]["storage"]["d6"]["cas_dir"],
            json!("/var/lib/spawn/d6-cas")
        );
        assert_eq!(
            eff["config"]["storage"]["d6"]["tmpfs_root"],
            json!("/dev/shm/spawn-d6-l1")
        );
        assert_eq!(
            eff["config"]["storage"]["d6"]["tmpfs_budget_mb"],
            json!(4096)
        );
        assert_eq!(
            eff["config"]["storage"]["d6"]["strict_fail_closed"],
            json!(true)
        );
        assert_eq!(eff["env"]["SPAWN_WORKSPACE_BACKEND"], json!(backend));
        assert_eq!(
            eff["env"]["SPAWN_D6_CAS_DIR"],
            json!("/var/lib/spawn/d6-cas")
        );
        assert_eq!(
            eff["env"]["SPAWN_D6_TMPFS_ROOT"],
            json!("/dev/shm/spawn-d6-l1")
        );
        assert_eq!(eff["env"]["SPAWN_D6_TMPFS_BUDGET_MB"], json!("4096"));
        assert_eq!(eff["env"]["SPAWN_D6_STRICT_FAIL_CLOSED"], json!("1"));
        assert_eq!(eff["origins"]["storage.d6"], json!("file"));
    }

    let strict_false = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn-d6"
[storage]
workspace_backend = "d6-shadow"
[storage.d6]
strict_fail_closed = false
"#;
    let cfg_false = SpawnConfig::from_toml_str(strict_false).expect("strict false parses");
    let env_false = env_as_map(resolve_env_entries_with(&cfg_false, &|_| None).expect("env false"));
    assert_eq!(env_false["SPAWN_D6_STRICT_FAIL_CLOSED"], "0");
    assert!(!env_false.contains_key("SPAWN_D6_CAS_DIR"));
}

#[test]
fn spawn_config_rejects_invalid_workspace_backend_and_storage_d6_fail_closed() {
    let bad_backend = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[storage]
workspace_backend = "zfs"
"#;
    let err = SpawnConfig::from_toml_str(bad_backend).expect_err("invalid workspace_backend");
    assert!(err.contains("d6") && err.contains("d6-shadow"), "{err}");

    for bad_cas in ["relative/cas", "/", "/tmp", "/var/lib", "/srv/d6/../bad"] {
        let toml = format!(
            "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn\"\n[storage.d6]\ncas_dir = \"{bad_cas}\"\n"
        );
        let err =
            SpawnConfig::from_toml_str(&toml).expect_err(&format!("reject cas_dir={bad_cas}"));
        assert!(err.contains("[storage.d6].cas_dir"), "{err}");
    }

    for bad_tmpfs in ["relative/tmpfs", "/", "/dev", "/tmp", "/dev/shm/../escape"] {
        let toml = format!(
            "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn\"\n[storage.d6]\ntmpfs_root = \"{bad_tmpfs}\"\n"
        );
        let err =
            SpawnConfig::from_toml_str(&toml).expect_err(&format!("reject tmpfs_root={bad_tmpfs}"));
        assert!(err.contains("[storage.d6].tmpfs_root"), "{err}");
    }

    let zero_budget = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[storage.d6]
tmpfs_budget_mb = 0
"#;
    let budget_err = SpawnConfig::from_toml_str(zero_budget).expect_err("reject zero budget");
    assert!(
        budget_err.contains("[storage.d6].tmpfs_budget_mb"),
        "{budget_err}"
    );

    let unknown_d6 = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[storage.d6]
unexpected_key = true
"#;
    let unknown_err = SpawnConfig::from_toml_str(unknown_d6).expect_err("reject unknown d6 field");
    assert!(unknown_err.contains("unknown field"), "{unknown_err}");
}

#[test]
fn config_origin_and_secret_string_enforce_redaction_and_non_serialization() {
    trait FallbackNoDisplay {
        fn implements_display(&self) -> bool {
            false
        }
    }
    trait FallbackNoSerialize {
        fn implements_serialize(&self) -> bool {
            false
        }
    }
    trait FallbackNoPartialEq {
        fn implements_partial_eq(&self) -> bool {
            false
        }
    }
    struct DisplayProbe<T>(std::marker::PhantomData<T>);
    struct SerProbe<T>(std::marker::PhantomData<T>);
    struct EqProbe<T>(std::marker::PhantomData<T>);

    impl<T> FallbackNoDisplay for DisplayProbe<T> {}
    impl<T: std::fmt::Display> DisplayProbe<T> {
        fn implements_display(&self) -> bool {
            let _ = &self.0;
            true
        }
    }

    impl<T> FallbackNoSerialize for SerProbe<T> {}
    impl<T: serde::Serialize> SerProbe<T> {
        fn implements_serialize(&self) -> bool {
            let _ = &self.0;
            true
        }
    }

    impl<T> FallbackNoPartialEq for EqProbe<T> {}
    impl<T: PartialEq> EqProbe<T> {
        fn implements_partial_eq(&self) -> bool {
            let _ = &self.0;
            true
        }
    }

    assert_eq!(ConfigOrigin::Default.as_str(), "default");
    assert_eq!(ConfigOrigin::File.as_str(), "file");
    assert_eq!(ConfigOrigin::Env.as_str(), "env");
    assert_eq!(ConfigOrigin::Credential.as_str(), "credential");
    assert_eq!(ConfigOrigin::Flag.as_str(), "flag");

    let env_ref = SecretRef::FromEnv {
        from_env: "MY_ENV_SECRET".to_string(),
    };
    let file_ref = SecretRef::FromFile {
        from_file: PathBuf::from("/etc/spawn/secret"),
    };
    assert_eq!(env_ref.origin(), ConfigOrigin::Env);
    assert_eq!(file_ref.origin(), ConfigOrigin::Credential);

    let secret = env_ref
        .resolve_secret_with(&|k| (k == "MY_ENV_SECRET").then(|| "SuperSensitive123".to_string()))
        .expect("resolve secret");
    assert_eq!(format!("{secret:?}"), "[REDACTED]");
    assert_eq!(format!("{secret:#?}"), "[REDACTED]");
    assert!(!format!("{secret:?}").contains("SuperSensitive123"));
    assert_eq!(secret.expose_secret(), "SuperSensitive123");

    let str_disp = DisplayProbe::<String>(std::marker::PhantomData);
    let sec_disp = DisplayProbe::<SecretString>(std::marker::PhantomData);
    assert!(str_disp.implements_display());
    assert!(!sec_disp.implements_display());

    let str_ser = SerProbe::<String>(std::marker::PhantomData);
    let sec_ser = SerProbe::<SecretString>(std::marker::PhantomData);
    assert!(str_ser.implements_serialize());
    assert!(!sec_ser.implements_serialize());

    let str_eq = EqProbe::<String>(std::marker::PhantomData);
    let sec_eq = EqProbe::<SecretString>(std::marker::PhantomData);
    assert!(str_eq.implements_partial_eq());
    assert!(!sec_eq.implements_partial_eq());
}

#[test]
fn effective_json_origins_distinguish_file_defaults_and_secret_subrefs() {
    // Passfile-authenticated [database] (host/user/name only) must report "file",
    // while omitted [storage].workspace_backend must report "default" and project "auto".
    let passfile_toml = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[database]
host = "127.0.0.1"
port = 5432
name = "spawn"
user = "spawn"
"#;
    let passfile_cfg = SpawnConfig::from_toml_str(passfile_toml).expect("passfile cfg parses");
    assert_eq!(passfile_cfg.storage.workspace_backend, None);
    let passfile_eff = effective_json_with(&passfile_cfg, &|_| None).expect("passfile eff");
    assert_eq!(passfile_eff["origins"]["database"], json!("file"));
    assert!(passfile_eff["origins"].get("database.url").is_none());
    assert!(passfile_eff["origins"].get("database.password").is_none());
    assert_eq!(
        passfile_eff["origins"]["storage.workspace_backend"],
        json!("default")
    );
    assert_eq!(
        passfile_eff["env"]["SPAWN_WORKSPACE_BACKEND"],
        json!("auto")
    );

    // Omitted [database] -> "default"; explicit workspace_backend = "auto" even inside empty [storage]
    let dir = TempDir::new("origins-probes");
    let pw_file = dir.path().join("db.pw");
    fs::write(&pw_file, "S3cr3tPass_991\n").expect("write pw");
    fs::set_permissions(&pw_file, fs::Permissions::from_mode(0o600)).expect("chmod");

    let bare_toml = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[storage]
cas_dedup = false
"#;
    let bare_cfg = SpawnConfig::from_toml_str(bare_toml).expect("bare cfg parses");
    assert_eq!(bare_cfg.storage.workspace_backend, None);
    let bare_eff = effective_json_with(&bare_cfg, &|_| None).expect("bare eff");
    assert_eq!(bare_eff["origins"]["database"], json!("default"));
    assert_eq!(
        bare_eff["origins"]["storage.workspace_backend"],
        json!("default")
    );
    assert_eq!(bare_eff["env"]["SPAWN_WORKSPACE_BACKEND"], json!("auto"));

    // Secret sub-refs under [database] report database = "file" + database.{url,password} = env/credential
    let subref_pw_toml = format!(
        r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[database]
name = "spawn"
user = "spawn"
password = {{ from_file = "{}" }}
[storage]
workspace_backend = "auto"
"#,
        pw_file.display()
    );
    let subref_pw_cfg = SpawnConfig::from_toml_str(&subref_pw_toml).expect("subref pw parses");
    assert_eq!(
        subref_pw_cfg.storage.workspace_backend,
        Some(WorkspaceBackend::Auto)
    );
    let subref_pw_eff = effective_json_with(&subref_pw_cfg, &|_| None).expect("subref pw eff");
    assert_eq!(subref_pw_eff["origins"]["database"], json!("file"));
    assert_eq!(
        subref_pw_eff["origins"]["database.password"],
        json!("credential")
    );
    assert!(subref_pw_eff["origins"].get("database.url").is_none());
    assert_eq!(
        subref_pw_eff["origins"]["storage.workspace_backend"],
        json!("file")
    );

    let subref_url_toml = r#"
schema_version = 1
[instance]
data_dir = "/srv/spawn"
[database]
url = { from_env = "PROBE_DB_URL" }
[storage]
workspace_backend = "d6"
"#;
    let subref_url_cfg = SpawnConfig::from_toml_str(subref_url_toml).expect("subref url parses");
    assert_eq!(
        subref_url_cfg.storage.workspace_backend,
        Some(WorkspaceBackend::Named(spawn_host::WorkspaceBackend::D6))
    );
    let url_env = env_map(&[("PROBE_DB_URL", "postgres://u:pw@127.0.0.1:5432/db")]);
    let subref_url_eff =
        effective_json_with(&subref_url_cfg, &|k| url_env.get(k).cloned()).expect("subref url eff");
    assert_eq!(subref_url_eff["origins"]["database"], json!("file"));
    assert_eq!(subref_url_eff["origins"]["database.url"], json!("env"));
    assert!(subref_url_eff["origins"].get("database.password").is_none());
    assert_eq!(
        subref_url_eff["origins"]["storage.workspace_backend"],
        json!("file")
    );
}

#[test]
fn workspace_backend_host_roundtrip_and_auto() {
    let concrete = [
        (spawn_host::WorkspaceBackend::Git, "git"),
        (spawn_host::WorkspaceBackend::Cow, "cow"),
        (spawn_host::WorkspaceBackend::Overlay, "overlay"),
        (spawn_host::WorkspaceBackend::D6Shadow, "d6-shadow"),
        (spawn_host::WorkspaceBackend::D6, "d6"),
    ];
    for (host, name) in concrete {
        let converted = WorkspaceBackend::Named(host);
        assert_eq!(converted.to_host(), Some(host));
        assert_eq!(converted.name(), name);
        assert_eq!(WorkspaceBackend::parse(name), Some(converted));
    }
    assert_eq!(WorkspaceBackend::Auto.to_host(), None);
    assert_eq!(WorkspaceBackend::Auto.name(), "auto");
    assert_eq!(
        WorkspaceBackend::parse("auto"),
        Some(WorkspaceBackend::Auto)
    );
    assert_eq!(WorkspaceBackend::parse("unknown"), None);
}
