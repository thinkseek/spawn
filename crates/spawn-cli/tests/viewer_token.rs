//! Subprocess CLI contracts for `spawn serve viewer-token`: validates deterministic
//! `sv1` token emission, `--ttl` upper-bound rejection, credential-file support,
//! help text, and zero leakage of `SPAWN_SERVE_TOKEN`.

use std::process::{Command, Output};

const TEST_MASTER_TOKEN: &str = "cli-master-secret-do-not-leak-9f8e7d";

fn run_spawn(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .expect("execute spawn binary")
}

fn assert_clean_viewer_stderr(stderr: &str) {
    assert!(
        !stderr.contains(TEST_MASTER_TOKEN),
        "master secret leaked into stderr"
    );
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        let benign_non_linux = cfg!(not(target_os = "linux"))
            && line.starts_with("spawn: not dumpable is a Linux setting;");
        assert!(
            benign_non_linux,
            "unexpected stderr line: {line:?} (full stderr: {stderr:?})"
        );
    }
}

#[test]
fn viewer_token_cli_emits_only_sv1_token_and_never_echoes_master_secret() {
    let default_out = run_spawn(
        &["serve", "viewer-token"],
        &[("SPAWN_SERVE_TOKEN", TEST_MASTER_TOKEN)],
    );
    let stdout = String::from_utf8_lossy(&default_out.stdout);
    let stderr = String::from_utf8_lossy(&default_out.stderr);
    assert_eq!(default_out.status.code(), Some(0), "{stderr}");
    assert_clean_viewer_stderr(&stderr);
    let trimmed = stdout.trim();
    let parts: Vec<&str> = trimmed.split('.').collect();
    assert_eq!(parts.len(), 3, "unexpected token shape: {trimmed:?}");
    assert_eq!(parts[0], "sv1");
    assert!(
        parts[1].parse::<u64>().is_ok_and(|exp| exp > 0),
        "bad expiry: {trimmed:?}"
    );
    assert!(!parts[2].is_empty());
    assert!(
        !stdout.contains(TEST_MASTER_TOKEN) && !stderr.contains(TEST_MASTER_TOKEN),
        "master secret leaked into CLI output"
    );

    let custom_out = run_spawn(
        &["serve", "viewer-token", "--ttl", "30m"],
        &[("SPAWN_SERVE_TOKEN", TEST_MASTER_TOKEN)],
    );
    let custom_stdout = String::from_utf8_lossy(&custom_out.stdout);
    let custom_stderr = String::from_utf8_lossy(&custom_out.stderr);
    assert_eq!(custom_out.status.code(), Some(0), "{custom_stderr}");
    assert_clean_viewer_stderr(&custom_stderr);
    assert!(custom_stdout.trim().starts_with("sv1."));
    assert!(!custom_stdout.contains(TEST_MASTER_TOKEN));
}

#[test]
fn viewer_token_cli_reads_keep_credential_file_without_echoing_secret() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("spawn-viewer-cred-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create cred dir");
    let cred_path = dir.join("spawn-serve.token");
    std::fs::write(&cred_path, format!("{TEST_MASTER_TOKEN}\n")).expect("write cred");
    std::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600");

    let cred_str = dir.to_str().expect("utf8 path");
    let out = run_spawn(
        &["serve", "viewer-token", "--ttl", "24h"],
        &[("CREDENTIALS_DIRECTORY", cred_str)],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_clean_viewer_stderr(&stderr);
    assert!(stdout.trim().starts_with("sv1."));
    assert!(!stdout.contains(TEST_MASTER_TOKEN));
    assert!(!stderr.contains(TEST_MASTER_TOKEN));

    std::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(0o644))
        .expect("chmod 0644");
    let bad_mode = run_spawn(
        &["serve", "viewer-token"],
        &[("CREDENTIALS_DIRECTORY", cred_str)],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(bad_mode.status.code(), Some(78));
    assert!(bad_mode.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&bad_mode.stderr).contains(TEST_MASTER_TOKEN));
}

#[test]
fn viewer_token_cli_rejects_bad_ttls_missing_secret_and_unknown_flags() {
    for args in [
        &["serve", "viewer-token", "--ttl", "25h"][..],
        &["serve", "viewer-token", "--ttl", "24h1s"],
        &["serve", "viewer-token", "--ttl", "0s"],
        &["serve", "viewer-token", "--ttl", "bogus"],
        &["serve", "viewer-token", "--unknown", "1"],
    ] {
        let out = run_spawn(args, &[("SPAWN_SERVE_TOKEN", TEST_MASTER_TOKEN)]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0), "{args:?} must fail");
        assert!(stdout.is_empty(), "{args:?}: unexpected stdout {stdout}");
        assert!(
            !stderr.contains(TEST_MASTER_TOKEN) && !stdout.contains(TEST_MASTER_TOKEN),
            "master secret leaked on error for {args:?}"
        );
    }

    let missing = run_spawn(&["serve", "viewer-token"], &[]);
    assert_ne!(missing.status.code(), Some(0));
    assert!(missing.stdout.is_empty());

    let bad_master = run_spawn(
        &["serve", "viewer-token"],
        &[("SPAWN_SERVE_TOKEN", "bad secret with space")],
    );
    assert_ne!(bad_master.status.code(), Some(0));
    assert!(bad_master.stdout.is_empty());
    let bad_stderr = String::from_utf8_lossy(&bad_master.stderr);
    assert!(!bad_stderr.contains("bad secret with space"));
}

#[test]
fn serve_help_documents_viewer_token_and_opens_no_store() {
    for args in [
        &["serve", "--help"][..],
        &["serve", "viewer-token", "--help"],
    ] {
        let out = run_spawn(args, &[]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
        assert!(
            stdout.contains("spawn serve viewer-token [--ttl <duration>]"),
            "{stdout}"
        );
        assert_clean_viewer_stderr(&stderr);
    }
}
