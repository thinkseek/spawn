//! `spawn features graph` as a black box (issue #100): the compiled
//! binary's `--json` output is byte-identical across runs on an unchanged
//! tree, and an unknown `--feature` fails with rc 1 naming it. No store is
//! opened, so the environment is empty but for `PATH`.

#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A temporary project written from a string table, removed when dropped.
struct Project(PathBuf);

impl Project {
    fn new(name: &str, table: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "spawn-features-graph-{name}-{}-{}",
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

const REGISTRY: &str = r#"{
  "schema_version": 1,
  "initiative_id": "fx",
  "boundary": {
    "project_name": "fx",
    "description": "fixture",
    "included_categories": [],
    "exclusions": []
  },
  "features": [
    { "id": "dom-fx", "title": "fx", "kind": "domain", "summary": "" },
    { "id": "cap-app", "title": "app", "kind": "capability", "parent": "dom-fx", "summary": "",
      "files": ["app/Cargo.toml", "app/src/main.rs"] },
    { "id": "cap-core", "title": "core", "kind": "capability", "parent": "dom-fx", "summary": "",
      "files": ["core/Cargo.toml", "core/src/lib.rs", "core/src/util.rs"] }
  ]
}
"#;

const PROJECT: &[(&str, &str)] = &[
    ("Cargo.toml", "[workspace]\nmembers = [\"core\", \"app\"]\n"),
    (
        "core/Cargo.toml",
        "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\n",
    ),
    ("core/src/lib.rs", "pub mod util;\npub use util::clamp;\n"),
    (
        "core/src/util.rs",
        "pub fn clamp(x: i32) -> i32 {\n    x\n}\n",
    ),
    (
        "app/Cargo.toml",
        "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nfx-core = { path = \"../core\" }\n",
    ),
    (
        "app/src/main.rs",
        "use fx_core::clamp;\n\nfn main() {\n    println!(\"{}\", fx_core::util::clamp(clamp(1)));\n}\n",
    ),
    ("registry.json", REGISTRY),
];

fn graph(root: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(["features", "graph", "--root"])
        .arg(root)
        .arg("--registry")
        .arg(root.join("registry.json"))
        .args(extra)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("run spawn")
}

#[test]
fn graph_json_is_byte_identical_across_two_runs() {
    let project = Project::new("bytes", PROJECT);
    let first = graph(&project.0, &["--json"]);
    let second = graph(&project.0, &["--json"]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert_eq!(first.status.code(), Some(0), "{stderr}");
    assert_eq!(second.status.code(), Some(0));
    assert!(!first.stdout.is_empty());
    assert_eq!(first.stdout, second.stdout, "two runs print the same bytes");
    let payload: serde_json::Value = serde_json::from_slice(&first.stdout).expect("json");
    assert_eq!(payload["truth"]["counts"]["by_code"]["under_declared"], 1);
}

#[test]
fn an_unknown_feature_fails_naming_it() {
    let project = Project::new("unknown", PROJECT);
    let output = graph(&project.0, &["--feature", "no-such-feature"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.starts_with("spawn: --feature \"no-such-feature\" names no feature"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
}

/// Rust and TypeScript / JavaScript side by side: a web package whose
/// files and manifests the walker hands the extractor next to the crates.
const POLYGLOT: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"fx-poly\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "pub mod util;\n"),
    ("src/util.rs", "pub fn f() {}\n"),
    (
        "web/package.json",
        "{ \"name\": \"fx-web\", \"type\": \"module\", \"main\": \"./src/main.ts\" }\n",
    ),
    (
        "web/tsconfig.json",
        "{\n  // aliases\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"src/*\"] }, },\n}\n",
    ),
    (
        "web/src/main.ts",
        "import { view } from '@/view.js';\nimport './main.css';\nimport React from 'react';\nconst lazy = () => import(name);\nexport const main = view;\n",
    ),
    ("web/src/view.tsx", "export function view() {}\n"),
    ("web/src/main.css", "body {}\n"),
    (
        "web/test/view.test.ts",
        "import { view } from '../src/view';\n",
    ),
    ("registry.json", POLYGLOT_REGISTRY),
];

const POLYGLOT_REGISTRY: &str = r#"{
  "schema_version": 1,
  "initiative_id": "fx",
  "boundary": {
    "project_name": "fx",
    "description": "fixture",
    "included_categories": [],
    "exclusions": []
  },
  "features": [
    { "id": "dom-fx", "title": "fx", "kind": "domain", "summary": "",
      "files": ["Cargo.toml", "src/lib.rs", "src/util.rs", "registry.json"] },
    { "id": "cap-web", "title": "web", "kind": "capability", "parent": "dom-fx", "summary": "",
      "files": ["web/package.json", "web/tsconfig.json", "web/src/main.ts", "web/src/view.tsx",
                "web/src/main.css", "web/test/view.test.ts"] }
  ]
}
"#;

#[test]
fn graph_json_includes_js_ts_alongside_rust_byte_identically() {
    let project = Project::new("polyglot", POLYGLOT);
    let first = graph(&project.0, &["--level", "file", "--json"]);
    let second = graph(&project.0, &["--level", "file", "--json"]);
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert_eq!(first.status.code(), Some(0), "{stderr}");
    assert_eq!(first.stdout, second.stdout, "two runs print the same bytes");
    let payload: serde_json::Value = serde_json::from_slice(&first.stdout).expect("json");
    let summary = &payload["graph"];
    assert_eq!(summary["languages"]["rust"]["files"], 2);
    assert_eq!(summary["languages"]["typescript"]["files"], 3);
    assert_eq!(summary["languages"]["typescript"]["edges"], 3);
    assert_eq!(summary["by_kind"]["asset"], 1);
    assert_eq!(summary["external_imports"], 1, "react");
    assert_eq!(summary["diagnostic_counts"]["dynamic_unresolved"], 1);
    let units: Vec<&str> = summary["units"]
        .as_array()
        .expect("units")
        .iter()
        .filter_map(|unit| unit["id"].as_str())
        .collect();
    assert_eq!(units, vec!["cargo:fx-poly", "npm:fx-web"]);
    let edges: Vec<(String, String, String)> = payload["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| {
            (
                format!("{}:{}", edge["from"].as_str().unwrap_or(""), edge["line"]),
                edge["to"].as_str().unwrap_or("").to_string(),
                edge["kind"].as_str().unwrap_or("").to_string(),
            )
        })
        .filter(|(from, _, _)| from.starts_with("web/"))
        .collect();
    let edge =
        |from: &str, to: &str, kind: &str| (from.to_string(), to.to_string(), kind.to_string());
    assert_eq!(
        edges,
        vec![
            edge("web/src/main.ts:2", "web/src/main.css", "asset"),
            edge("web/src/main.ts:1", "web/src/view.tsx", "import"),
            edge("web/test/view.test.ts:1", "web/src/view.tsx", "import"),
        ]
    );
}
