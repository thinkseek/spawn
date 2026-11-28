//! Wave-1 static env boundary guard (`spawn-core`, `spawn-cas`, `spawn-postgres`).
//!
//! Asserts that zero non-test files under `crates/spawn-core/src`,
//! `crates/spawn-cas/src`, and `crates/spawn-postgres/src` (excluding
//! `testing.rs` and `#[cfg(test)]` regions) invoke `std::env::var`,
//! `std::env::var_os`, `std::env::vars`, `std::env::vars_os`, or short
//! `env::var*` forms. Wave-2 clears `spawn-host`/`spawn-cli` before enabling
//! workspace-wide `clippy.toml` `disallowed-methods`.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    path: PathBuf,
    line: usize,
    snippet: String,
}

const TARGET_CRATE_SRCS: &[&str] = &[
    "crates/spawn-core/src",
    "crates/spawn-cas/src",
    "crates/spawn-postgres/src",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate sits under <root>/crates/spawn-postgres")
        .to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    let mut paths: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn is_excluded_test_support(repo_root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    rel == Path::new("crates/spawn-postgres/src/testing.rs")
}

/// Mask comments, strings, and `#[cfg(test)]` items with spaces while preserving newlines
/// so reported line numbers stay exact.
fn mask_non_production_rust(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out[i] = b'\n';
        }
    }
    let mut i = 0usize;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    let mut brace_depth: usize = 0;
    let mut pending_cfg_test = false;
    let mut cfg_test_brace_end: Option<usize> = None;

    while i < bytes.len() {
        if let Some(target_depth) = cfg_test_brace_end {
            if let Some(next_i) = skip_non_code_or_count_brace(bytes, i, &mut brace_depth) {
                if brace_depth <= target_depth && bytes[i] == b'}' {
                    cfg_test_brace_end = None;
                }
                i = next_i;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(next_i) = try_skip_comment_or_string(bytes, i) {
            i = next_i;
            continue;
        }

        if paren_depth == 0
            && bracket_depth == 0
            && let Some((end_attr, is_cfg_test)) = parse_attribute_at(bytes, i)
        {
            if is_cfg_test {
                pending_cfg_test = true;
            }
            i = end_attr;
            continue;
        }

        match bytes[i] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = (paren_depth - 1).max(0),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = (bracket_depth - 1).max(0),
            b'{' => {
                if pending_cfg_test {
                    cfg_test_brace_end = Some(brace_depth);
                    pending_cfg_test = false;
                    brace_depth += 1;
                    i += 1;
                    continue;
                }
                brace_depth += 1;
            }
            b';' if pending_cfg_test => {
                pending_cfg_test = false;
                i += 1;
                continue;
            }
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }

        if !pending_cfg_test {
            out[i] = bytes[i];
        }
        i += 1;
    }

    String::from_utf8(out).expect("masked ASCII/UTF-8 boundaries preserved")
}

fn skip_non_code_or_count_brace(bytes: &[u8], i: usize, brace_depth: &mut usize) -> Option<usize> {
    if let Some(next_i) = try_skip_comment_or_string(bytes, i) {
        return Some(next_i);
    }
    match bytes[i] {
        b'{' => {
            *brace_depth += 1;
            Some(i + 1)
        }
        b'}' => {
            *brace_depth = brace_depth.saturating_sub(1);
            Some(i + 1)
        }
        _ => None,
    }
}

fn parse_attribute_at(bytes: &[u8], start: usize) -> Option<(usize, bool)> {
    if bytes.get(start) != Some(&b'#') || bytes.get(start + 1) != Some(&b'[') {
        return None;
    }
    let mut depth = 1usize;
    let mut i = start + 2;
    while i < bytes.len() && depth > 0 {
        if let Some(next_i) = try_skip_comment_or_string(bytes, i) {
            i = next_i;
            continue;
        }
        match bytes[i] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let compact: String = std::str::from_utf8(&bytes[start..i])
        .unwrap_or("")
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let is_cfg_test = compact == "#[cfg(test)]";
    Some((i, is_cfg_test))
}

fn try_skip_comment_or_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes[start..].starts_with(b"//") {
        let mut i = start + 2;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        return Some(i);
    }
    if bytes[start..].starts_with(b"/*") {
        let mut depth = 1usize;
        let mut i = start + 2;
        while i < bytes.len() && depth > 0 {
            if bytes[i..].starts_with(b"/*") {
                depth += 1;
                i += 2;
            } else if bytes[i..].starts_with(b"*/") {
                depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        return Some(i);
    }
    if let Some(end) = try_skip_raw_string(bytes, start) {
        return Some(end);
    }
    if bytes[start] == b'"' || (bytes[start] == b'b' && bytes.get(start + 1) == Some(&b'"')) {
        let mut i = start + if bytes[start] == b'b' { 2 } else { 1 };
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i = (i + 2).min(bytes.len());
            } else if bytes[i] == b'"' {
                return Some(i + 1);
            } else {
                i += 1;
            }
        }
        return Some(bytes.len());
    }
    try_skip_char_literal(bytes, start)
}

fn try_skip_raw_string(bytes: &[u8], start: usize) -> Option<usize> {
    let idx = if bytes[start] == b'b' && bytes.get(start + 1) == Some(&b'r') {
        start + 2
    } else if bytes[start] == b'r' {
        start + 1
    } else {
        return None;
    };
    let mut hashes = 0usize;
    while bytes.get(idx + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(idx + hashes) != Some(&b'"') {
        return None;
    }
    let mut i = idx + hashes + 1;
    while i < bytes.len() {
        if bytes[i] == b'"'
            && i + 1 + hashes <= bytes.len()
            && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
        {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }
    Some(bytes.len())
}

fn try_skip_char_literal(bytes: &[u8], start: usize) -> Option<usize> {
    let q = if bytes[start] == b'b' && bytes.get(start + 1) == Some(&b'\'') {
        start + 1
    } else if bytes[start] == b'\'' {
        start
    } else {
        return None;
    };
    let after = q + 1;
    if after >= bytes.len() {
        return None;
    }
    if bytes[after] == b'\\' {
        let mut i = after + 1;
        while i < bytes.len() && bytes[i] != b'\'' && bytes[i] != b'\n' {
            i += 1;
        }
        return (bytes.get(i) == Some(&b'\'')).then_some(i + 1);
    }
    if bytes.get(after + 1) == Some(&b'\'') {
        return Some(after + 2);
    }
    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn line_contains_ident(text: &str, ident: &str) -> bool {
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(ident) {
        let start = from + rel;
        let end = start + ident.len();
        let prev_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let next_ok = text[end..].chars().next().is_none_or(|c| !is_ident_char(c));
        if prev_ok && next_ok {
            return true;
        }
        from = end;
    }
    false
}

fn line_has_forbidden_env_var(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    for pat in ["env::var_os", "env::vars_os", "env::vars", "env::var"] {
        if line_contains_ident(trimmed, pat) {
            return true;
        }
    }
    if let Some((_, rest)) = trimmed.split_once("std::env::{") {
        let inner = rest.split('}').next().unwrap_or(rest);
        for sym in ["var_os", "vars_os", "vars", "var"] {
            if line_contains_ident(inner, sym) {
                return true;
            }
        }
    }
    false
}

fn scan_source(rel_or_abs_path: &Path, src: &str) -> Vec<Violation> {
    let masked = mask_non_production_rust(src);
    let mut violations = Vec::new();
    for (idx, (masked_line, orig_line)) in masked.lines().zip(src.lines()).enumerate() {
        if line_has_forbidden_env_var(masked_line) {
            violations.push(Violation {
                path: rel_or_abs_path.to_path_buf(),
                line: idx + 1,
                snippet: orig_line.trim().to_string(),
            });
        }
    }
    violations
}

#[test]
fn core_cas_and_postgres_production_code_contains_zero_env_var_calls() {
    let root = workspace_root();
    let mut files = Vec::new();
    for crate_src in TARGET_CRATE_SRCS {
        collect_rs_files(&root.join(crate_src), &mut files);
    }
    assert!(!files.is_empty(), "expected core/cas/postgres source files");

    let mut violations = Vec::new();
    for file in files {
        if is_excluded_test_support(&root, &file) {
            continue;
        }
        let rel = file.strip_prefix(&root).unwrap_or(&file);
        let text =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        violations.extend(scan_source(rel, &text));
    }

    assert!(
        violations.is_empty(),
        "forbidden production env::var* call(s) found in core crates:\n{}",
        violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.path.display(), v.line, v.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn env_boundary_guard_catches_production_calls_and_ignores_tests_and_comments() {
    let sample = r#"
//! Doc mentions std::env::var("IGNORED")
use std::env::var as leaked_import;
use std::env::{self, var_os as leaked_brace};
const MSG: &str = "std::env::var_os(\"IGNORED\")";

pub fn prod_bad() -> Option<String> {
    // std::env::var("COMMENTED")
    /* nested /* std::env::vars() */ block comment */
    let _x = env::var("PROD_ONE").ok();
    let _y = std::env::vars().collect::<Vec<_>>();
    std::env::var_os("PROD_TWO").map(|s| s.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn inside_test_mod_is_allowed() {
        let _ = std::env::var("SPAWN_TEST_DATABASE_URL");
        let _ = std::env::var_os("HOME");
    }
}

pub fn after_tests_still_checked() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect()
}
"#;
    let found = scan_source(Path::new("sample.rs"), sample);
    let lines: Vec<usize> = found.iter().map(|v| v.line).collect();
    assert_eq!(lines, vec![3, 4, 10, 11, 12, 25], "got {found:?}");
}
