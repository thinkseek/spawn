//! Black-box integration tests for `spawn features check --deps/--purity`,
//! `spawn features generate`, and `spawn features sync` (Issue #100 PR 5).
//! No store is opened (`env_clear()` + `PATH`).

#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project(PathBuf);

impl Project {
    fn new(name: &str, table: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "spawn-features-sync-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        for (path, text) in table {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(file, text).expect("write fixture file");
        }
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn_features(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .arg("features")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("run spawn features")
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

const POLYGLOT_REPO: &[(&str, &str)] = &[
    ("README.md", "# Polyglot App\n"),
    ("Cargo.toml", "[workspace]\nmembers = [\"core\", \"app\"]\n"),
    (
        "core/Cargo.toml",
        "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\n",
    ),
    (
        "core/src/lib.rs",
        "pub fn ping() -> &'static str {\n    \"pong\"\n}\n",
    ),
    (
        "app/Cargo.toml",
        "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nfx-core = { path = \"../core\" }\n",
    ),
    (
        "app/src/main.rs",
        "use fx_core::ping;\nfn main() { let _ = ping(); }\n",
    ),
    (
        "web/package.json",
        "{ \"name\": \"@fx/web\", \"type\": \"module\", \"main\": \"./src/index.ts\" }\n",
    ),
    (
        "web/tsconfig.json",
        "{ \"compilerOptions\": { \"target\": \"ES2022\" } }\n",
    ),
    (
        "web/src/index.ts",
        "import { format } from './util';\nexport const out = format('ok');\n",
    ),
    (
        "web/src/util.ts",
        "export const format = (s: string) => s;\n",
    ),
    ("py/pyproject.toml", "[project]\nname = \"py-billing\"\n"),
    ("py/billing/__init__.py", "from .calc import total\n"),
    ("py/billing/calc.py", "def total(x):\n    return x\n"),
    (
        "py/billing/api.py",
        "from .calc import total\ndef run():\n    return total(1)\n",
    ),
];

#[test]
fn generate_builds_pure_acyclic_polyglot_registry_and_enforces_cleanly() {
    let project = Project::new("generate-polyglot", POLYGLOT_REPO);
    let root_s = project.0.to_string_lossy().into_owned();
    let reg_path = project.0.join("docs/feature-map/registry.json");

    // 1. Default dry-run prints valid JSON without writing to disk.
    let dry = spawn_features(&["generate", "--root", &root_s]);
    assert_eq!(
        dry.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(!reg_path.exists(), "dry-run must not write registry.json");
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout).expect("dry json");
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["purity"]["counts"]["error"], 0);

    // 2. --apply writes registry atomically; re-running without --force refuses overwrite.
    let apply = spawn_features(&["generate", "--root", &root_s, "--apply", "--json"]);
    assert_eq!(
        apply.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(reg_path.exists(), "--apply must write registry.json");
    let again = spawn_features(&["generate", "--root", &root_s, "--apply"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));
    let forced = spawn_features(&["generate", "--root", &root_s, "--apply", "--force"]);
    assert_eq!(forced.status.code(), Some(0));

    // 3. Generated registry immediately passes full check (--deps enforce + --purity enforce).
    let chk = spawn_features(&[
        "check", "--root", &root_s, "--deps", "enforce", "--purity", "enforce", "--json",
    ]);
    let chk_err = String::from_utf8_lossy(&chk.stderr);
    let chk_json: serde_json::Value = serde_json::from_slice(&chk.stdout).expect("check json");
    assert_eq!(chk.status.code(), Some(0), "{chk_err}\n{chk_json:#}");
    assert_eq!(chk_json["counts"]["unassociated_wild"], 0);
    assert_eq!(chk_json["counts"]["stale_references"], 0);
    assert_eq!(chk_json["counts"]["orphan_features"], 0);
    assert_eq!(chk_json["deps_enforced_violations"], serde_json::json!([]));
    assert_eq!(chk_json["purity"]["counts"]["error"], 0);
}

fn sync_fixture_registry() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "initiative_id": "custom",
        "boundary": {
            "project_name": "custom",
            "description": "fixture",
            "included_categories": ["tracked-source", "docs"],
            "exclusions": []
        },
        "features": [
            {
                "id": "domain-platform",
                "title": "Platform Domain",
                "kind": "domain",
                "parent": null,
                "summary": "Custom domain summary",
                "files": [],
                "relations": [],
                "provenance": {}
            },
            {
                "id": "cap-custom-worker",
                "title": "Hand-Curated Async Worker",
                "kind": "capability",
                "parent": "domain-platform",
                "summary": "Preserved manual worker description.",
                "files": [
                    "py/pyproject.toml",
                    "py/worker/__init__.py",
                    "py/worker/deleted_old.py",
                    "py/worker/run.py"
                ],
                "relations": [],
                "provenance": {
                    "tests": ["py/worker/deleted_test.py", "pytest -q"],
                    "commits": ["abc1234"]
                }
            },
            {
                "id": "cap-custom-docs",
                "title": "Hand-Curated Docs",
                "kind": "capability",
                "parent": "domain-platform",
                "summary": "Preserved docs description.",
                "files": ["docs/feature-map/registry.json", "docs/guide.md"],
                "relations": [],
                "provenance": {}
            }
        ]
    })
}

#[test]
fn sync_preserves_manual_semantics_and_assigns_wild_by_affinity_then_cohesion() {
    let project = Project::new(
        "sync-preserve",
        &[
            ("py/pyproject.toml", "[project]\nname = \"worker-svc\"\n"),
            ("py/worker/__init__.py", ""),
            ("py/worker/run.py", "def execute():\n    return 42\n"),
            ("docs/guide.md", "# Guide\n"),
        ],
    );
    let root_s = project.0.to_string_lossy().into_owned();
    let reg_path = project.0.join("docs/feature-map/registry.json");
    write_file(
        &project.0,
        "docs/feature-map/registry.json",
        &serde_json::to_string_pretty(&sync_fixture_registry()).expect("json"),
    );

    write_file(
        &project.0,
        "hooks/trigger.py",
        "from worker.run import execute\nexecute()\n",
    );
    write_file(&project.0, "py/worker/README.txt", "worker notes\n");

    let before_disk = std::fs::read_to_string(&reg_path).expect("read");
    let dry = spawn_features(&["sync", "--root", &root_s, "--json"]);
    assert_eq!(dry.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&reg_path).expect("read after dry"),
        before_disk
    );
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout).expect("dry sync json");
    assert_eq!(dry_json["changed"], true);
    assert_eq!(dry_json["applied"], false);

    let apply = spawn_features(&["sync", "--root", &root_s, "--apply", "--json"]);
    assert_eq!(apply.status.code(), Some(0));
    let sync_json: serde_json::Value = serde_json::from_slice(&apply.stdout).expect("sync json");
    assert_eq!(sync_json["applied"], true);
    assert_eq!(sync_json["counts_after"]["unassociated_wild"], 0);
    assert_eq!(sync_json["counts_after"]["stale_references"], 0);
    assert_eq!(sync_json["purity"]["counts"]["error"], 0);

    let assignments = sync_json["wild_assigned"].as_array().expect("assigned");
    let hook_assign = assignments
        .iter()
        .find(|a| a["path"] == "hooks/trigger.py")
        .expect("hooks/trigger.py");
    assert_eq!(hook_assign["feature_id"], "cap-custom-worker");
    assert_eq!(hook_assign["strategy"], "code_graph_affinity");

    let note_assign = assignments
        .iter()
        .find(|a| a["path"] == "py/worker/README.txt")
        .expect("py/worker/README.txt");
    assert_eq!(note_assign["feature_id"], "cap-custom-worker");
    assert_eq!(note_assign["strategy"], "directory_cohesion");

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&reg_path).expect("saved")).expect("json");
    let worker = saved["features"]
        .as_array()
        .expect("features")
        .iter()
        .find(|f| f["id"] == "cap-custom-worker")
        .expect("cap-custom-worker");
    assert_eq!(worker["title"], "Hand-Curated Async Worker");
    assert_eq!(worker["summary"], "Preserved manual worker description.");
    assert_eq!(
        worker["provenance"]["tests"],
        serde_json::json!(["pytest -q"])
    );
    assert_eq!(
        worker["provenance"]["commits"],
        serde_json::json!(["abc1234"])
    );
}

fn deps_policy_fixture() -> (Project, serde_json::Value) {
    let project = Project::new(
        "deps-policy",
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"core\", \"app\"]\n"),
            (
                "core/Cargo.toml",
                "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\n",
            ),
            ("core/src/lib.rs", "pub fn ok() -> i32 { 1 }\n"),
            (
                "app/Cargo.toml",
                "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nfx-core = { path = \"../core\" }\n",
            ),
            (
                "app/src/main.rs",
                "use fx_core::ok;\nfn main() { let _ = ok(); }\n",
            ),
        ],
    );
    let reg = serde_json::json!({
        "schema_version": 1,
        "initiative_id": "fx",
        "boundary": { "project_name": "fx", "description": "fixture", "included_categories": [], "exclusions": [] },
        "features": [
            { "id": "dom-fx", "title": "FX", "kind": "domain", "summary": "", "files": [], "relations": [] },
            { "id": "cap-app", "title": "App", "kind": "capability", "parent": "dom-fx", "summary": "", "files": ["Cargo.toml", "app/Cargo.toml", "app/src/main.rs", "registry.json"], "relations": [] },
            { "id": "cap-core", "title": "Core", "kind": "capability", "parent": "dom-fx", "summary": "", "files": ["core/Cargo.toml", "core/src/lib.rs"], "relations": [] }
        ]
    });
    write_file(
        &project.0,
        "registry.json",
        &serde_json::to_string_pretty(&reg).expect("json"),
    );
    (project, reg)
}

#[test]
fn check_deps_report_and_enforce_respect_policy_and_expired_waivers() {
    let (project, reg) = deps_policy_fixture();
    let root_s = project.0.to_string_lossy().into_owned();
    let reg_s = project
        .0
        .join("registry.json")
        .to_string_lossy()
        .into_owned();

    let rep = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--deps",
        "report",
        "--json",
    ]);
    assert_eq!(rep.status.code(), Some(0));
    let rep_json: serde_json::Value = serde_json::from_slice(&rep.stdout).expect("rep json");
    assert_eq!(
        rep_json["deps_truth"]["counts"]["by_code"]["under_declared"],
        1
    );

    let enf = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--deps",
        "enforce",
        "--json",
    ]);
    assert_eq!(enf.status.code(), Some(1));

    // Active waiver allows enforce to pass; expired waiver fails enforce.
    let active_policy = serde_json::json!({
        "waivers": [{
            "code": "under_declared",
            "from": "cap-app",
            "to": "cap-core",
            "reason": "migration in progress",
            "until": "2099-12-31"
        }]
    });
    let expired_policy = serde_json::json!({
        "waivers": [{
            "code": "under_declared",
            "from": "cap-app",
            "to": "cap-core",
            "reason": "expired waiver",
            "until": "2020-01-01"
        }]
    });
    write_file(&project.0, "active.json", &active_policy.to_string());
    write_file(&project.0, "expired.json", &expired_policy.to_string());
    // Cover policy files cleanly in cap-app before enforcing coverage + deps.
    let mut updated_reg = reg;
    updated_reg["features"][1]["files"]
        .as_array_mut()
        .expect("files")
        .extend([
            serde_json::json!("active.json"),
            serde_json::json!("expired.json"),
        ]);
    write_file(
        &project.0,
        "registry.json",
        &serde_json::to_string_pretty(&updated_reg).expect("json"),
    );

    let active_s = project.0.join("active.json").to_string_lossy().into_owned();
    let ok = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--deps",
        "enforce",
        "--policy",
        &active_s,
    ]);
    assert_eq!(ok.status.code(), Some(0));

    let exp_s = project
        .0
        .join("expired.json")
        .to_string_lossy()
        .into_owned();
    let bad = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--deps",
        "enforce",
        "--policy",
        &exp_s,
    ]);
    assert_eq!(bad.status.code(), Some(1));
}

#[test]
fn check_purity_reports_all_four_violations_and_enforces_exit_code() {
    let project = Project::new(
        "purity-gates",
        &[
            ("README.md", "# x\n"),
            ("alpha/a.rs", "pub fn a() {}\n"),
            ("beta/b.rs", "pub fn b() {}\n"),
            ("gamma/c.rs", "pub fn c() {}\n"),
            ("docs/guide.md", "# Guide\n"),
        ],
    );
    let reg = serde_json::json!({
        "schema_version": 1,
        "initiative_id": "fx",
        "boundary": { "project_name": "fx", "description": "", "included_categories": [], "exclusions": [] },
        "features": [
            { "id": "dom-fx", "title": "FX", "kind": "domain", "summary": "", "files": ["README.md"], "relations": [] },
            { "id": "cap-misc", "title": "Miscellaneous", "kind": "capability", "parent": "dom-fx", "summary": "", "files": ["alpha/a.rs", "beta/b.rs", "gamma/c.rs", "docs/guide.md", "registry.json"], "relations": [] }
        ]
    });
    write_file(&project.0, "registry.json", &reg.to_string());
    let root_s = project.0.to_string_lossy().into_owned();
    let reg_s = project
        .0
        .join("registry.json")
        .to_string_lossy()
        .into_owned();

    // Clean file coverage, but impure: --purity report exits 0 while --purity enforce exits 1.
    let rep = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--purity",
        "report",
        "--json",
    ]);
    assert_eq!(rep.status.code(), Some(0));
    let rep_json: serde_json::Value = serde_json::from_slice(&rep.stdout).expect("purity json");
    assert_eq!(
        rep_json["purity"]["counts"]["by_code"]["domain_direct_files"],
        1
    );
    assert_eq!(
        rep_json["purity"]["counts"]["by_code"]["catch_all_bucket"],
        1
    );
    assert_eq!(
        rep_json["purity"]["counts"]["by_code"]["unrelated_directory_span"],
        1
    );

    let enf = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &reg_s,
        "--purity",
        "enforce",
        "--json",
    ]);
    assert_eq!(enf.status.code(), Some(1));

    // Wildcard glob in files is caught by glob_catch_all too.
    let mut glob_reg = reg;
    glob_reg["features"][1]["files"] = serde_json::json!(["docs/*.md"]);
    write_file(&project.0, "glob.json", &glob_reg.to_string());
    let glob_s = project.0.join("glob.json").to_string_lossy().into_owned();
    let glob_output = spawn_features(&[
        "check",
        "--root",
        &root_s,
        "--registry",
        &glob_s,
        "--purity",
        "report",
        "--json",
    ]);
    let glob_json: serde_json::Value =
        serde_json::from_slice(&glob_output.stdout).expect("glob purity json");
    assert_eq!(
        glob_json["purity"]["counts"]["by_code"]["glob_catch_all"],
        1
    );
}
