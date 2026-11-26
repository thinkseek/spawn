//! Workspace dependency-closure guardrails.
//!
//! Ensures core, CAS, host, storage, KDF and codegraph crates never pull
//! TLS, key-management, base64 or HTTP server dependencies (`ring`, `rustls`,
//! `rustls-pki-types`, `webpki-roots`, `zeroize`, `base64`, `tiny_http`, or
//! `spawn-node-proto`), retains `sha1` in `Cargo.lock`, and keeps
//! `spawn-cli/src/integrate/` free of `spawn_node_proto` imports.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

const NO_CRYPTO_NO_BASE64_CRATES: [&str; 6] = [
    "spawn-core",
    "spawn-cas",
    "spawn-host",
    "spawn-kdf",
    "spawn-codegraph",
    "spawn-jev",
];

const BANNED_ALL: [&str; 8] = [
    "ring",
    "rustls",
    "rustls-pki-types",
    "webpki-roots",
    "zeroize",
    "base64",
    "tiny_http",
    "spawn-node-proto",
];

const BANNED_POSTGRES_TRANSITIVE: [&str; 7] = [
    "ring",
    "rustls",
    "rustls-pki-types",
    "webpki-roots",
    "zeroize",
    "tiny_http",
    "spawn-node-proto",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn parse_cargo_lock(lock: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut current_name: Option<String> = None;
    let mut in_deps = false;

    for line in lock.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            if let Some(name) = current_name.take() {
                graph.entry(name).or_default();
            }
            in_deps = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name = \"")
            && let Some(name) = rest.strip_suffix('"')
        {
            current_name = Some(name.to_string());
            graph.entry(name.to_string()).or_default();
            continue;
        }
        if trimmed == "dependencies = [" {
            in_deps = true;
            continue;
        }
        if in_deps {
            if trimmed == "]" {
                in_deps = false;
                continue;
            }
            if let Some(name) = current_name.as_ref()
                && let Some(quoted) = trimmed.strip_suffix(',').or(Some(trimmed))
                && let Some(inner) = quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                && let Some(dep_name) = inner.split_whitespace().next()
            {
                graph
                    .entry(name.clone())
                    .or_default()
                    .insert(dep_name.to_string());
            }
        }
    }
    if let Some(name) = current_name {
        graph.entry(name).or_default();
    }
    graph
}

fn transitive_deps(graph: &BTreeMap<String, BTreeSet<String>>, root: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root.to_string());
    while let Some(pkg) = queue.pop_front() {
        if let Some(deps) = graph.get(&pkg) {
            for dep in deps {
                if seen.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }
    seen
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read directory") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if entry.file_type().expect("file type").is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn direct_dependencies_section(toml: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let mut in_deps = false;
    for line in toml.lines() {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]";
            continue;
        }
        if in_deps && let Some((key, _)) = trimmed.split_once('=') {
            deps.insert(key.trim().to_string());
        }
    }
    deps
}

#[test]
fn restricted_crates_pull_no_tls_crypto_base64_or_node_proto() {
    let root = repo_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    let graph = parse_cargo_lock(&lock);
    assert!(
        graph.contains_key("sha1"),
        "sha1 must remain present in Cargo.lock"
    );

    for krate in NO_CRYPTO_NO_BASE64_CRATES {
        assert!(
            graph.contains_key(krate),
            "{krate} missing from workspace Cargo.lock"
        );
        let manifest =
            fs::read_to_string(root.join("crates").join(krate).join("Cargo.toml")).expect("toml");
        let direct = direct_dependencies_section(&manifest);
        assert!(
            !direct.contains("spawn-postgres") && !direct.contains("postgres"),
            "{krate} must not depend directly on postgres/spawn-postgres"
        );
        for banned in BANNED_ALL {
            assert!(
                !direct.contains(banned),
                "{krate} Cargo.toml directly depends on banned crate {banned}"
            );
        }

        let deps = transitive_deps(&graph, krate);
        for banned in BANNED_POSTGRES_TRANSITIVE {
            assert!(
                !deps.contains(banned),
                "restricted crate {krate} transitively depends on banned crate {banned}"
            );
        }
        if deps.contains("base64") {
            assert!(
                deps.contains("postgres-protocol"),
                "restricted crate {krate} pulls base64 outside postgres-protocol dev-dependencies"
            );
        }
    }

    assert!(
        graph.contains_key("spawn-postgres"),
        "spawn-postgres missing from workspace Cargo.lock"
    );
    let pg_deps = transitive_deps(&graph, "spawn-postgres");
    for banned in BANNED_POSTGRES_TRANSITIVE {
        assert!(
            !pg_deps.contains(banned),
            "spawn-postgres transitively depends on banned crate {banned}"
        );
    }
    assert!(
        !graph["spawn-postgres"].contains("base64"),
        "spawn-postgres must not directly depend on base64"
    );

    // Ensure no restricted crate's transitive closure introduces base64 except postgres-protocol.
    let base64_users: Vec<&str> = graph
        .iter()
        .filter(|(_, deps)| deps.contains("base64"))
        .map(|(name, _)| name.as_str())
        .collect();
    for user in &base64_users {
        for krate in NO_CRYPTO_NO_BASE64_CRATES {
            let mut without_pg = graph.clone();
            without_pg.remove("postgres-protocol");
            let clean_deps = transitive_deps(&without_pg, krate);
            assert!(
                !clean_deps.contains("base64"),
                "restricted crate {krate} pulls base64 outside postgres-protocol (via {user})"
            );
        }
    }
}

#[test]
fn integrate_module_has_zero_spawn_node_proto_references() {
    let integrate_dir = repo_root().join("crates/spawn-cli/src/integrate");
    let mut files = Vec::new();
    collect_rs_files(&integrate_dir, &mut files);
    assert!(!files.is_empty(), "expected integrate/*.rs files");
    for path in files {
        let text = fs::read_to_string(&path).expect("read rs file");
        assert!(
            !text.contains("spawn_node_proto"),
            "{} must not depend on spawn_node_proto",
            path.display()
        );
    }
}
