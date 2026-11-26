//! Internal, source-agnostic repository & workload store (`spawn source`):
//! manages `<root>/<name>.git`, `<root>/<name>.meta.json`, and
//! `<root>/<name>.scenarios/` inside `SPAWN_SOURCES_ROOT` (or
//! `$HOME/.local/share/spawn-sources`) without coupling Spawn to GitHub or any
//! external forge. External URLs (`https://`, `ssh://`) and local paths are
//! purely optional `init` / `sync` / `export` edges.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Flags;
use crate::sources::{MissionSource, controller_git};

/// Usage text for `spawn source` (read by `main.rs` help tests).
pub(crate) const USAGE: &str = "usage: spawn source init <name> [--kind git|task|dataset|scratch] [--from <dir|url>] [--branch <branch>] [--verify <cmd>] [--description <text>] [--sources-root <dir>]\n       \
                                spawn source list [--json] [--sources-root <dir>]\n       \
                                spawn source show <name> [--json] [--sources-root <dir>]\n       \
                                spawn source sync <name> [--from <dir|url>] [--branch <branch>] [--sources-root <dir>]\n       \
                                spawn source export <name> --to <dir|url> [--branch <branch>] [--sources-root <dir>]";

const VALID_KINDS: [&str; 4] = ["git", "task", "dataset", "scratch"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SourceMeta {
    pub(crate) v: u32,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) description: String,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) upstream: Option<String>,
    pub(crate) upstream_branch: Option<String>,
    pub(crate) repo_path: String,
    pub(crate) scenarios_path: String,
    pub(crate) verify_command: Option<String>,
}

/// Dispatch `spawn source <verb> ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some((verb, tail)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    match verb.as_str() {
        "init" => {
            let (name, flags) = parse_named_flags(tail)?;
            let out = init(&name, &flags)?;
            print_json(&out);
        }
        "list" => {
            let flags = Flags::parse_with_switches(tail, &["json"])?;
            let root = resolve_sources_root(&flags)?;
            let rows = list(&root)?;
            if flags.switch("json") {
                print_json(&json!(rows));
            } else {
                for row in &rows {
                    println!(
                        "{}\t{}\t{}\t{}",
                        row["name"].as_str().unwrap_or(""),
                        row["kind"].as_str().unwrap_or(""),
                        row["head"].as_str().unwrap_or("-"),
                        row["repo_path"].as_str().unwrap_or(""),
                    );
                }
            }
        }
        "show" => {
            let (name, flags) = parse_named_flags(tail)?;
            let root = resolve_sources_root(&flags)?;
            let out = show(&root, &name)?;
            if flags.switch("json") {
                print_json(&out);
            } else {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    out["name"].as_str().unwrap_or(""),
                    out["kind"].as_str().unwrap_or(""),
                    out["head"].as_str().unwrap_or("-"),
                    out["repo_path"].as_str().unwrap_or(""),
                    out["scenarios_path"].as_str().unwrap_or(""),
                );
            }
        }
        "sync" => {
            let (name, flags) = parse_named_flags(tail)?;
            let out = sync(&name, &flags)?;
            print_json(&out);
        }
        "export" => {
            let (name, flags) = parse_named_flags(tail)?;
            let out = export(&name, &flags)?;
            print_json(&out);
        }
        other => {
            return Err(format!("unknown `spawn source` verb `{other}`\n{USAGE}"));
        }
    }
    Ok(0)
}

fn parse_named_flags(tail: &[String]) -> Result<(String, Flags), String> {
    let (positional, flag_args) = match tail.first() {
        Some(first) if !first.starts_with("--") => (Some(first.clone()), &tail[1..]),
        _ => (None, tail),
    };
    let flags = Flags::parse_with_switches(flag_args, &["json"])?;
    let name = match (positional, flags.get("name")) {
        (Some(pos), Some(flag)) if pos != flag => {
            return Err(format!("conflicting source names `{pos}` and `--name {flag}`"));
        }
        (Some(pos), _) => pos,
        (None, Some(flag)) => flag,
        (None, None) => return Err(format!("source <name> is required\n{USAGE}")),
    };
    validate_slug(&name)?;
    Ok((name, flags))
}

/// Validate a plain internal source slug (`[a-z0-9][a-z0-9._-]{0,63}`).
pub(crate) fn validate_slug(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    if !valid {
        return Err(format!(
            "source name {name:?} must match [a-z0-9][a-z0-9._-]{{0,63}} without `..`"
        ));
    }
    Ok(())
}

/// Determine the internal sources root directory from `--sources-root`,
/// `SPAWN_SOURCES_ROOT`, or `$HOME/.local/share/spawn-sources`.
pub(crate) fn resolve_sources_root(flags: &Flags) -> Result<PathBuf, String> {
    if let Some(raw) = flags
        .get("sources-root")
        .or_else(|| crate::env_value("SPAWN_SOURCES_ROOT"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return std::path::absolute(raw).map_err(|e| format!("--sources-root: {e}"));
    }
    let home = crate::env_value("HOME").unwrap_or_else(|| "/root".to_string());
    Ok(PathBuf::from(home).join(".local/share/spawn-sources"))
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn meta_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.meta.json"))
}

fn repo_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.git"))
}

fn scenarios_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.scenarios"))
}

#[derive(Debug, Clone)]
enum IngestOrigin {
    LocalDir(PathBuf),
    RemoteGit(String),
}

fn classify_origin(raw: &str) -> Result<IngestOrigin, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("source origin cannot be blank".to_string());
    }
    if trimmed.contains("://") || is_scp_style_remote(trimmed) {
        let validated = MissionSource::parse(trimmed)?;
        return Ok(IngestOrigin::RemoteGit(validated.as_str().to_string()));
    }
    let abs = std::path::absolute(trimmed).map_err(|e| format!("resolve path {trimmed:?}: {e}"))?;
    if !abs.exists() {
        return Err(format!("local source path {} does not exist", abs.display()));
    }
    Ok(IngestOrigin::LocalDir(abs))
}

fn is_scp_style_remote(text: &str) -> bool {
    !text.starts_with('/')
        && !text.starts_with("./")
        && !text.starts_with("../")
        && text.contains('@')
        && text.contains(':')
}

/// Initialize an internal bare repository `<root>/<name>.git`, metadata JSON
/// `<root>/<name>.meta.json`, and scenario directory `<root>/<name>.scenarios/`.
pub(crate) fn init(name: &str, flags: &Flags) -> Result<Value, String> {
    validate_slug(name)?;
    let root = resolve_sources_root(flags)?;
    fs::create_dir_all(&root)
        .map_err(|e| format!("create sources root {}: {e}", root.display()))?;

    let bare = repo_path(&root, name);
    let meta_file = meta_path(&root, name);
    let scenarios = scenarios_path(&root, name);
    if bare.exists() || meta_file.exists() {
        return Err(format!(
            "source {name:?} already exists under {}",
            root.display()
        ));
    }

    let kind = flags.get("kind").unwrap_or_else(|| "git".to_string());
    if !VALID_KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "--kind must be one of {}, not {kind:?}",
            VALID_KINDS.join(", ")
        ));
    }
    let description = flags
        .get("description")
        .unwrap_or_else(|| format!("Internal Spawn {kind} source `{name}`"));
    let verify_cmd = flags.get("verify");
    let branch = flags.get("branch");
    let from = flags.get("from");

    let mut upstream_recorded: Option<String> = None;
    let init_result = (|| -> Result<String, String> {
        run_git(&["init", "--bare", &bare.display().to_string()])?;
        run_git(&[
            "--git-dir",
            &bare.display().to_string(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ])?;
        if let Some(origin_raw) = from.as_deref() {
            match classify_origin(origin_raw)? {
                IngestOrigin::LocalDir(local) => {
                    upstream_recorded = Some(local.display().to_string());
                    ingest_local_dir(&bare, &local, branch.as_deref(), name)?;
                }
                IngestOrigin::RemoteGit(url) => {
                    upstream_recorded = Some(url.clone());
                    fetch_remote_into_main(&bare, &url, branch.as_deref())?;
                }
            }
        } else {
            seed_internal_repo(&bare, name, &kind, &description)?;
        }
        write_scenarios(&scenarios, verify_cmd.as_deref())?;
        let head = read_main_head(&bare)?;
        let ts = now_ms();
        let meta = SourceMeta {
            v: 1,
            name: name.to_string(),
            kind: kind.clone(),
            description: description.clone(),
            created_at_ms: ts,
            updated_at_ms: ts,
            upstream: upstream_recorded.clone(),
            upstream_branch: branch.clone(),
            repo_path: bare.display().to_string(),
            scenarios_path: scenarios.display().to_string(),
            verify_command: verify_cmd.clone(),
        };
        write_meta(&meta_file, &meta)?;
        Ok(head)
    })();

    let head = match init_result {
        Ok(head) => head,
        Err(error) => {
            let _ = fs::remove_dir_all(&bare);
            let _ = fs::remove_dir_all(&scenarios);
            let _ = fs::remove_file(&meta_file);
            return Err(error);
        }
    };

    Ok(json!({
        "name": name,
        "kind": kind,
        "repo": bare.display().to_string(),
        "scenarios": scenarios.display().to_string(),
        "head": head,
        "upstream": upstream_recorded,
    }))
}

/// List all internal sources under `root`.
pub(crate) fn list(root: &Path) -> Result<Vec<Value>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    let entries =
        fs::read_dir(root).map_err(|e| format!("read {}: {e}", root.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if let Some(name) = file.strip_suffix(".meta.json") {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| show(root, &name))
        .collect()
}

/// Show detailed metadata and live `main` head of internal source `name`.
pub(crate) fn show(root: &Path, name: &str) -> Result<Value, String> {
    validate_slug(name)?;
    let meta = read_meta(&meta_path(root, name))?;
    let bare = PathBuf::from(&meta.repo_path);
    let head = read_main_head(&bare).ok();
    let mut scenario_files = Vec::new();
    if let Ok(entries) = fs::read_dir(&meta.scenarios_path) {
        for entry in entries.filter_map(Result::ok) {
            if let Some(fname) = entry.file_name().to_str() {
                scenario_files.push(fname.to_string());
            }
        }
    }
    scenario_files.sort();
    Ok(json!({
        "v": meta.v,
        "name": meta.name,
        "kind": meta.kind,
        "description": meta.description,
        "repo_path": meta.repo_path,
        "scenarios_path": meta.scenarios_path,
        "scenario_files": scenario_files,
        "head": head,
        "upstream": meta.upstream,
        "upstream_branch": meta.upstream_branch,
        "verify_command": meta.verify_command,
        "created_at_ms": meta.created_at_ms,
        "updated_at_ms": meta.updated_at_ms,
    }))
}

/// Optionally synchronize an internal source from its configured upstream or `--from`.
pub(crate) fn sync(name: &str, flags: &Flags) -> Result<Value, String> {
    validate_slug(name)?;
    let root = resolve_sources_root(flags)?;
    let meta_file = meta_path(&root, name);
    let mut meta = read_meta(&meta_file)?;
    let bare = PathBuf::from(&meta.repo_path);
    let origin_raw = flags
        .get("from")
        .or_else(|| meta.upstream.clone())
        .ok_or_else(|| format!("source {name:?} has no upstream and --from was not given"))?;
    let branch = flags.get("branch").or_else(|| meta.upstream_branch.clone());
    match classify_origin(&origin_raw)? {
        IngestOrigin::LocalDir(local) => {
            ingest_local_dir(&bare, &local, branch.as_deref(), name)?;
        }
        IngestOrigin::RemoteGit(url) => {
            fetch_remote_into_main(&bare, &url, branch.as_deref())?;
        }
    }
    meta.updated_at_ms = now_ms();
    write_meta(&meta_file, &meta)?;
    let head = read_main_head(&bare)?;
    Ok(json!({
        "name": name,
        "head": head,
        "synced_from": origin_raw,
    }))
}

/// Optionally export an internal source's `main` branch to a local directory or
/// any Git destination URL (`--to`).
pub(crate) fn export(name: &str, flags: &Flags) -> Result<Value, String> {
    validate_slug(name)?;
    let root = resolve_sources_root(flags)?;
    let meta = read_meta(&meta_path(&root, name))?;
    let bare = PathBuf::from(&meta.repo_path);
    let head = read_main_head(&bare)?;
    let to_raw = flags.required("to")?;
    let target_branch = flags.get("branch").unwrap_or_else(|| "main".to_string());

    if to_raw.contains("://") || is_scp_style_remote(&to_raw) {
        let validated = MissionSource::parse(&to_raw)?;
        let refspec = format!("refs/heads/main:refs/heads/{target_branch}");
        run_git(&[
            "--git-dir",
            &bare.display().to_string(),
            "push",
            validated.as_str(),
            &refspec,
        ])?;
        return Ok(json!({
            "name": name,
            "exported_to": validated.as_str(),
            "branch": target_branch,
            "head": head,
        }));
    }

    let dest = std::path::absolute(&to_raw).map_err(|e| format!("--to: {e}"))?;
    if dest.join("objects").is_dir() && dest.join("HEAD").is_file() {
        let refspec = format!("refs/heads/main:refs/heads/{target_branch}");
        run_git(&[
            "--git-dir",
            &bare.display().to_string(),
            "push",
            &dest.display().to_string(),
            &refspec,
        ])?;
    } else {
        fs::create_dir_all(&dest)
            .map_err(|e| format!("create export dir {}: {e}", dest.display()))?;
        export_archive_to_dir(&bare, &head, &dest)?;
    }
    Ok(json!({
        "name": name,
        "exported_to": dest.display().to_string(),
        "branch": target_branch,
        "head": head,
    }))
}

fn export_archive_to_dir(bare: &Path, commit: &str, dest: &Path) -> Result<(), String> {
    let tar_bytes = controller_git()
        .args([
            "--git-dir",
            &bare.display().to_string(),
            "archive",
            "--format=tar",
            commit,
        ])
        .output()
        .map_err(|e| format!("git archive: {e}"))?;
    if !tar_bytes.status.success() {
        return Err(format!(
            "git archive failed: {}",
            String::from_utf8_lossy(&tar_bytes.stderr).trim()
        ));
    }
    let mut child = std::process::Command::new("tar")
        .args(["-x", "-C", &dest.display().to_string()])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn tar: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        stdin
            .write_all(&tar_bytes.stdout)
            .map_err(|e| format!("write tar archive: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wait tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar extract failed with {status}"));
    }
    Ok(())
}

fn run_git(args: &[&str]) -> Result<String, String> {
    let out = controller_git()
        .args(args)
        .output()
        .map_err(|e| format!("run git {}: {e}", args.first().copied().unwrap_or("")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_main_head(bare: &Path) -> Result<String, String> {
    run_git(&[
        "--git-dir",
        &bare.display().to_string(),
        "rev-parse",
        "refs/heads/main",
    ])
}

fn is_git_source(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    run_git(&[
        "--git-dir",
        &dir.display().to_string(),
        "rev-parse",
        "--is-bare-repository",
    ])
    .is_ok_and(|out| out.trim() == "true")
}

fn ingest_local_dir(
    bare: &Path,
    local: &Path,
    branch: Option<&str>,
    name: &str,
) -> Result<(), String> {
    if is_git_source(local) {
        return fetch_remote_into_main(bare, &local.display().to_string(), branch);
    }
    let tmp = TempWorkDir::new(&format!("spawn-source-ingest-{name}"))?;
    copy_dir_excluding_git(local, tmp.path())?;
    commit_dir_to_bare(tmp.path(), bare, &format!("Initialize internal source {name}"))
}

fn fetch_remote_into_main(bare: &Path, url: &str, branch: Option<&str>) -> Result<(), String> {
    let bare_str = bare.display().to_string();
    let refspec = match branch {
        Some(branch) => format!("+refs/heads/{branch}:refs/heads/main"),
        None => "+HEAD:refs/heads/main".to_string(),
    };
    run_git(&["--git-dir", &bare_str, "fetch", "--depth=1", url, &refspec])
        .or_else(|_| run_git(&["--git-dir", &bare_str, "fetch", url, &refspec]))?;
    run_git(&[
        "--git-dir",
        &bare_str,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ])?;
    Ok(())
}

fn seed_internal_repo(
    bare: &Path,
    name: &str,
    kind: &str,
    description: &str,
) -> Result<(), String> {
    let tmp = TempWorkDir::new(&format!("spawn-source-seed-{name}"))?;
    let readme = format!("# {name}\n\nKind: {kind}\n\n{description}\n");
    fs::write(tmp.path().join("README.md"), readme)
        .map_err(|e| format!("write README.md: {e}"))?;
    let manifest = json!({
        "v": 1,
        "name": name,
        "kind": kind,
        "description": description,
    });
    fs::write(
        tmp.path().join("SOURCE.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n",
    )
    .map_err(|e| format!("write SOURCE.json: {e}"))?;
    commit_dir_to_bare(tmp.path(), bare, &format!("Initialize internal source {name}"))
}

fn commit_dir_to_bare(work: &Path, bare: &Path, message: &str) -> Result<(), String> {
    let work_str = work.display().to_string();
    let bare_str = bare.display().to_string();
    run_git(&["-C", &work_str, "init"])?;
    run_git(&["-C", &work_str, "symbolic-ref", "HEAD", "refs/heads/main"])?;
    run_git(&["-C", &work_str, "add", "-A"])?;
    let status = run_git(&["-C", &work_str, "status", "--porcelain"])?;
    if status.is_empty() {
        fs::write(work.join(".gitkeep"), "").map_err(|e| format!("write .gitkeep: {e}"))?;
        run_git(&["-C", &work_str, "add", ".gitkeep"])?;
    }
    run_git(&[
        "-C",
        &work_str,
        "-c",
        "user.name=Spawn Internal Store",
        "-c",
        "user.email=spawn@local",
        "commit",
        "-m",
        message,
    ])?;
    run_git(&[
        "-C",
        &work_str,
        "push",
        "-f",
        &bare_str,
        "HEAD:refs/heads/main",
    ])?;
    Ok(())
}

fn copy_dir_excluding_git(src: &Path, dst: &Path) -> Result<(), String> {
    let entries = fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let meta = entry
            .metadata()
            .map_err(|e| format!("stat {}: {e}", from.display()))?;
        if meta.is_dir() {
            fs::create_dir_all(&to).map_err(|e| format!("mkdir {}: {e}", to.display()))?;
            copy_dir_excluding_git(&from, &to)?;
        } else if meta.is_file() {
            fs::copy(&from, &to)
                .map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn write_scenarios(scenarios: &Path, verify_cmd: Option<&str>) -> Result<(), String> {
    fs::create_dir_all(scenarios)
        .map_err(|e| format!("create scenarios {}: {e}", scenarios.display()))?;
    fs::write(scenarios.join("no-test-database"), "")
        .map_err(|e| format!("write no-test-database: {e}"))?;

    let script = if let Some(cmd) = verify_cmd {
        format!("#!/usr/bin/env bash\nset -euo pipefail\n{cmd}\n")
    } else {
        r#"#!/usr/bin/env bash
set -euo pipefail
if [ -n "${SPAWN_GATE_BASE:-}" ] && [ -n "${SPAWN_GATE_CANDIDATE:-}" ] && [ "${SPAWN_GATE_BASE}" = "${SPAWN_GATE_CANDIDATE}" ]; then
    echo "candidate commit equals base commit" >&2
    exit 1
fi
if [ -f "Cargo.toml" ]; then
    cargo test --offline || cargo test
elif [ -f "package.json" ]; then
    npm test
elif ls test_*.py *_test.py >/dev/null 2>&1 || [ -d "tests" ]; then
    python3 -m unittest discover -s . -v
else
    # Generic internal task/dataset/scratch verification: require non-empty diff.
    git --git-dir="${SPAWN_GATE_REPO:?}" diff --quiet "${SPAWN_GATE_BASE:?}" "${SPAWN_GATE_CANDIDATE:?}" && exit 1 || true
fi
"#
        .to_string()
    };
    let verify_path = scenarios.join("verify.sh");
    fs::write(&verify_path, script).map_err(|e| format!("write {}: {e}", verify_path.display()))?;
    let mut perms = fs::metadata(&verify_path)
        .map_err(|e| format!("stat {}: {e}", verify_path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&verify_path, perms)
        .map_err(|e| format!("chmod {}: {e}", verify_path.display()))?;
    Ok(())
}

fn write_meta(path: &Path, meta: &SourceMeta) -> Result<(), String> {
    let body = serde_json::to_string_pretty(meta).map_err(|e| format!("encode metadata: {e}"))?;
    fs::write(path, format!("{body}\n")).map_err(|e| format!("write {}: {e}", path.display()))
}

fn read_meta(path: &Path) -> Result<SourceMeta, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

struct TempWorkDir(PathBuf);

impl TempWorkDir {
    fn new(prefix: &str) -> Result<Self, String> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| format!("create temp dir {}: {e}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{export, init, list, show, sync, validate_slug};
    use crate::Flags;
    use serde_json::json;

    #[test]
    fn validates_slug_names() {
        for good in ["a", "flake-doctor", "internal.audit_01"] {
            assert!(validate_slug(good).is_ok(), "{good}");
        }
        for bad in ["", "../etc", ".hidden", "-bad", "HasUpper", "a/b", "a..b"] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn internal_non_git_task_and_local_dir_lifecycle_works_without_github() {
        let scratch = spawn_host::Scratch::new("spawn-managed-sources");
        let sources_root = scratch.join("sources");
        let root_arg = sources_root.display().to_string();

        // 1. Pure internal non-GitHub task workload (no --from).
        let flags = Flags::parse(&[
            "--kind".to_string(),
            "task".to_string(),
            "--description".to_string(),
            "Internal benchmark ledger".to_string(),
            "--sources-root".to_string(),
            root_arg.clone(),
        ])
        .expect("flags");
        let created = init("internal-ledger", &flags).expect("init task");
        assert_eq!(created["name"], json!("internal-ledger"));
        assert_eq!(created["kind"], json!("task"));
        assert_eq!(created["upstream"], json!(null));
        assert!(sources_root.join("internal-ledger.git/HEAD").is_file());
        assert!(sources_root.join("internal-ledger.scenarios/verify.sh").is_file());
        assert!(sources_root
            .join("internal-ledger.scenarios/no-test-database")
            .is_file());

        // 2. Local plain non-git directory ingest + list + show + sync + export.
        let local_proj = scratch.join("local-proj");
        std::fs::create_dir_all(&local_proj).expect("local proj");
        std::fs::write(local_proj.join("hello.txt"), "v1\n").expect("write v1");
        let ingest_flags = Flags::parse(&[
            "--kind".to_string(),
            "git".to_string(),
            "--from".to_string(),
            local_proj.display().to_string(),
            "--verify".to_string(),
            "grep -q v2 hello.txt || grep -q v1 hello.txt".to_string(),
            "--sources-root".to_string(),
            root_arg.clone(),
        ])
        .expect("ingest flags");
        let ingested = init("local-proj", &ingest_flags).expect("init local dir");
        assert_eq!(ingested["kind"], json!("git"));

        let listed = list(&sources_root).expect("list");
        assert_eq!(listed.len(), 2);
        let details = show(&sources_root, "local-proj").expect("show");
        assert_eq!(details["upstream"], json!(local_proj.display().to_string()));

        // Modify local dir and sync into internal store.
        std::fs::write(local_proj.join("hello.txt"), "v2\n").expect("write v2");
        let sync_flags = Flags::parse(&["--sources-root".to_string(), root_arg.clone()]).expect("sync");
        let synced = sync("local-proj", &sync_flags).expect("sync");
        assert_ne!(synced["head"], ingested["head"]);

        // Export to fresh directory (zero git remote needed).
        let export_dir = scratch.join("exported");
        let export_flags = Flags::parse(&[
            "--to".to_string(),
            export_dir.display().to_string(),
            "--sources-root".to_string(),
            root_arg,
        ])
        .expect("export");
        export("local-proj", &export_flags).expect("export");
        assert_eq!(
            std::fs::read_to_string(export_dir.join("hello.txt")).expect("read exported"),
            "v2\n"
        );
    }
}
