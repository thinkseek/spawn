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

fn is_test_only_src_file(src_root: &Path, path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "tests.rs" || name.ends_with("_tests.rs") {
        return true;
    }
    path.strip_prefix(src_root)
        .ok()
        .is_some_and(|rel| rel.components().any(|c| c.as_os_str() == "tests"))
}

fn rel_path_unix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("path inside repo root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_ident_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
}

fn skip_comment(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    if i + 1 >= n || bytes[i] != b'/' {
        return None;
    }
    if bytes[i + 1] == b'/' {
        let mut j = i + 2;
        while j < n && bytes[j] != b'\n' {
            j += 1;
        }
        return Some(j);
    }
    if bytes[i + 1] == b'*' {
        let mut j = i + 2;
        let mut depth = 1usize;
        while j + 1 < n && depth > 0 {
            if bytes[j] == b'/' && bytes[j + 1] == b'*' {
                depth += 1;
                j += 2;
            } else if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        return Some(j.min(n));
    }
    None
}

fn skip_raw_string_literal(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    if !matches!(bytes[i], b'r' | b'b' | b'c') || !is_ident_boundary_before(bytes, i) {
        return None;
    }
    let mut p = if matches!(bytes[i], b'b' | b'c') {
        i + 1
    } else {
        i
    };
    if bytes.get(p) != Some(&b'r') {
        return None;
    }
    p += 1;
    let hashes_start = p;
    while p < n && bytes[p] == b'#' {
        p += 1;
    }
    if bytes.get(p) != Some(&b'"') {
        return None;
    }
    let hashes = p - hashes_start;
    let mut j = p + 1;
    while j < n {
        if bytes[j] == b'"'
            && j + 1 + hashes <= n
            && bytes[j + 1..j + 1 + hashes].iter().all(|&b| b == b'#')
        {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(n)
}

fn skip_quoted_string(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    let mut j = if bytes[i] == b'"' {
        i + 1
    } else if matches!(bytes[i], b'b' | b'c')
        && bytes.get(i + 1) == Some(&b'"')
        && is_ident_boundary_before(bytes, i)
    {
        i + 2
    } else {
        return None;
    };
    while j < n {
        if bytes[j] == b'\\' {
            j = (j + 2).min(n);
        } else if bytes[j] == b'"' {
            return Some(j + 1);
        } else {
            j += 1;
        }
    }
    Some(n)
}

fn skip_char_literal(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    let c = if bytes[i] == b'\'' {
        i + 1
    } else if bytes[i] == b'b'
        && bytes.get(i + 1) == Some(&b'\'')
        && is_ident_boundary_before(bytes, i)
    {
        i + 2
    } else {
        return None;
    };
    if c < n && bytes[c] == b'\\' {
        let mut j = c + 2;
        while j < n && bytes[j] != b'\'' && bytes[j] != b'\n' {
            j += 1;
        }
        if bytes.get(j) == Some(&b'\'') {
            return Some(j + 1);
        }
    } else if c + 1 < n && bytes[c + 1] == b'\'' && bytes[c] != b'\'' && bytes[c] != b'\n' {
        return Some(c + 2);
    }
    None
}

fn skip_comment_or_literal(bytes: &[u8], i: usize) -> Option<usize> {
    skip_comment(bytes, i)
        .or_else(|| skip_raw_string_literal(bytes, i))
        .or_else(|| skip_quoted_string(bytes, i))
        .or_else(|| skip_char_literal(bytes, i))
}

fn match_cfg_test_attr(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'#') {
        return None;
    }
    let n = bytes.len();
    let mut p = start + 1;
    while p < n && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    if bytes.get(p) != Some(&b'[') {
        return None;
    }
    let content_start = p + 1;
    let mut q = content_start;
    let mut depth = 1usize;
    while q < n && depth > 0 {
        if let Some(next) = skip_comment_or_literal(bytes, q) {
            q = next;
            continue;
        }
        match bytes[q] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        q += 1;
    }
    if depth != 0 {
        return None;
    }
    let compact: String = bytes[content_start..q - 1]
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .map(char::from)
        .collect();
    (compact == "cfg(test)").then_some(q)
}

fn skip_any_attr(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'#') {
        return None;
    }
    let n = bytes.len();
    let mut p = start + 1;
    while p < n && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    if bytes.get(p) != Some(&b'[') {
        return None;
    }
    p += 1;
    let mut depth = 1usize;
    while p < n && depth > 0 {
        if let Some(next) = skip_comment_or_literal(bytes, p) {
            p = next;
            continue;
        }
        match bytes[p] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        p += 1;
    }
    (depth == 0).then_some(p)
}

fn strip_cfg_test_blocks(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < n {
        if let Some(next) = skip_comment_or_literal(bytes, i) {
            out.push_str(&src[i..next]);
            i = next;
            continue;
        }
        if let Some(mut j) = match_cfg_test_attr(bytes, i) {
            loop {
                while j < n && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if let Some(next) = skip_comment_or_literal(bytes, j) {
                    j = next;
                } else if let Some(next) = skip_any_attr(bytes, j) {
                    j = next;
                } else {
                    break;
                }
            }
            let mut brace_depth = 0usize;
            while j < n {
                if let Some(next) = skip_comment_or_literal(bytes, j) {
                    j = next;
                    continue;
                }
                match bytes[j] {
                    b'{' => {
                        brace_depth += 1;
                        j += 1;
                    }
                    b'}' => {
                        j += 1;
                        if brace_depth > 0 {
                            brace_depth -= 1;
                            if brace_depth == 0 {
                                break;
                            }
                        }
                    }
                    b';' if brace_depth == 0 => {
                        j += 1;
                        break;
                    }
                    _ => j += 1,
                }
            }
            out.push('\n');
            i = j;
            continue;
        }
        let ch_len = src[i..].chars().next().expect("valid utf8 char").len_utf8();
        out.push_str(&src[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn contains_unsafe_token(src: &str) -> bool {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    while i < n {
        if let Some(next) = skip_comment_or_literal(bytes, i) {
            i = next;
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let mut j = i + 1;
            while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if &bytes[i..j] == b"unsafe" {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

fn collect_prod_std_net_files(root: &Path, krate: &str) -> BTreeSet<String> {
    let src_root = root.join("crates").join(krate).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_root, &mut files);
    files.sort();
    let mut matched = BTreeSet::new();
    for path in files {
        if is_test_only_src_file(&src_root, &path) {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read rs file");
        let prod = strip_cfg_test_blocks(&text);
        if prod.contains("std::net") {
            matched.insert(rel_path_unix(root, &path));
        }
    }
    matched
}

/// Ratchet guard for Issue #113 Phase 2d: ensures pure crates stay at 0
/// `std::net` references and no new file in the workspace introduces
/// `std::net` outside the shrinking Phase 2d removal allowlist.
#[test]
fn production_std_net_ratchet_matches_phase2d_allowlist() {
    let root = repo_root();

    for pure in [
        "spawn-cas",
        "spawn-host",
        "spawn-codegraph",
        "spawn-kdf",
        "spawn-hc",
        "ghostty-vt",
        "spawn-node-proto",
    ] {
        let found = collect_prod_std_net_files(&root, pure);
        assert!(
            found.is_empty(),
            "pure crate {pure} must not reference std::net in production code: {found:?}"
        );
    }

    let core_found = collect_prod_std_net_files(&root, "spawn-core");
    let core_allowed: BTreeSet<String> = ["crates/spawn-core/src/bridge.rs"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert!(
        core_found.is_subset(&core_allowed),
        "spawn-core introduced disallowed production std::net files (allowed subset {core_allowed:?}): {core_found:?}"
    );

    let exact_allowlist: [(&str, &[&str]); 4] = [
        (
            "spawn-cli",
            &[
                "crates/spawn-cli/src/nodes/commands.rs",
                "crates/spawn-cli/src/nodes/coord.rs",
                "crates/spawn-cli/src/nodes/relay.rs",
                "crates/spawn-cli/src/nodes/transport.rs",
                "crates/spawn-cli/src/nodes/ws.rs",
                "crates/spawn-cli/src/serve.rs",
            ],
        ),
        ("spawn-jev", &["crates/spawn-jev/src/lib.rs"]),
        (
            "spawn-node-gateway",
            &[
                "crates/spawn-node-gateway/src/config.rs",
                "crates/spawn-node-gateway/src/gateway.rs",
                "crates/spawn-node-gateway/src/http.rs",
                "crates/spawn-node-gateway/src/limits.rs",
                "crates/spawn-node-gateway/src/main.rs",
            ],
        ),
        ("spawn-postgres", &["crates/spawn-postgres/src/host.rs"]),
    ];

    for (krate, expected) in exact_allowlist {
        let want: BTreeSet<String> = expected.iter().map(|p| (*p).to_string()).collect();
        let got = collect_prod_std_net_files(&root, krate);
        assert_eq!(
            got, want,
            "production std::net ratchet mismatch for {krate}; Phase 2d deletions must update dependency_guard.rs explicitly and additions are forbidden"
        );
    }
}

/// Issue #98 boundary guard: pure/leaf library crates explicitly declare
/// `#![forbid(unsafe_code)]` and contain zero `unsafe` tokens in `src/`.
#[test]
fn pure_crates_forbid_unsafe_code() {
    let root = repo_root();
    for krate in ["spawn-cas", "spawn-kdf", "spawn-codegraph", "spawn-jev"] {
        let src_root = root.join("crates").join(krate).join("src");
        let lib_rs = fs::read_to_string(src_root.join("lib.rs")).expect("read crate lib.rs");
        assert!(
            lib_rs
                .lines()
                .any(|line| line.trim() == "#![forbid(unsafe_code)]"),
            "{krate}/src/lib.rs must declare #![forbid(unsafe_code)]"
        );
        let mut files = Vec::new();
        collect_rs_files(&src_root, &mut files);
        assert!(!files.is_empty(), "expected files under {krate}/src");
        for path in files {
            let text = fs::read_to_string(&path).expect("read src file");
            assert!(
                !contains_unsafe_token(&text),
                "{} must contain zero unsafe blocks/items",
                rel_path_unix(&root, &path)
            );
        }
    }
}
