//! Workspace-wide static env boundary guard across all 12 workspace crates.
//!
//! Enforces five invariants across every member crate listed in root `Cargo.toml`:
//!
//! 1. **Strict production `std::env::var*` convergence**: Using a Rust lexer, fail-closed
//!    `#[cfg(test)]` module stripper, recursive `use`-tree analyzer, and production
//!    path scanner (with exact line numbers), rejects every production `std::env::var`,
//!    `std::env::var_os`, `std::env::vars`, `std::env::vars_os`, short `env::var*`,
//!    renamed `use std::env as ...`, unaliased `pub ... use std::env;` / `use std::env::{self};`,
//!    glob `use std::env::*`, nested or multi-line `use` trees, `env::$...`, and `$m::var*`
//!    macro path accesses outside `ENV_BOUNDARY_ALLOWLIST`
//!    (`crates/spawn-cli/src/config.rs`, `crates/spawn-node-gateway/src/main.rs`, and
//!    `crates/spawn-postgres/src/testing.rs`). Only `#![cfg(test)]`, `#[cfg(test)] mod <ident> { ... }`
//!    (regardless of outer attribute order on that module), and `#[cfg(all(..., test, ...))] mod ...`
//!    are stripped; all other `#[cfg(test)]` targets are audited as production code.
//! 2. **Zero unallowlisted clippy/env lint suppressions**: Rejects any
//!    production `allow(...)` or `expect(...)` mentioning `disallowed_methods`,
//!    `clippy::all`, `clippy::style`, bare `clippy`, or `warnings` outside
//!    `ENV_BOUNDARY_ALLOWLIST`, verifies root `clippy.toml` forbids all four
//!    `std::env::{var, var_os, vars, vars_os}` methods, and forbids any nested
//!    or dot Clippy config outside the single root `clippy.toml`.
//! 3. **Production `--force-warn clippy::disallowed_methods` audit**: Runs real `cargo clippy`
//!    with `--offline --locked --force-warn clippy::disallowed_methods` and
//!    `CLIPPY_CONF_DIR` pinned to the workspace root across all workspace
//!    production targets (`--lib --bins`) so even macro-generated `#[allow/expect(clippy::disallowed_methods)]`
//!    suppressions or crate-local configs cannot silence disallowed `std::env` calls
//!    outside `ENV_BOUNDARY_ALLOWLIST`.
//! 4. **Workspace member parity**: Asserts `WORKSPACE_CRATES` matches `[workspace] members`
//!    in root `Cargo.toml` so newly added workspace crates cannot evade auditing.
//! 5. **Zero production `spawn-postgres/testing` activation**: Queries Cargo's real
//!    dependency graph (`cargo tree --offline --locked --workspace -e normal,build --target all`)
//!    with non-vacuous `seen_spawn_postgres > 0` verification and fails if `spawn-postgres`
//!    ever resolves with `"testing"` on any normal or build edge.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

const WORKSPACE_CRATES: &[&str] = &[
    "crates/spawn-core",
    "crates/spawn-cas",
    "crates/spawn-postgres",
    "crates/spawn-host",
    "crates/spawn-jev",
    "crates/spawn-codegraph",
    "crates/spawn-kdf",
    "crates/spawn-node-proto",
    "crates/spawn-hc",
    "crates/ghostty-vt",
    "crates/spawn-cli",
    "crates/spawn-node-gateway",
];

const ENV_BOUNDARY_ALLOWLIST: &[&str] = &[
    "crates/spawn-cli/src/config.rs",
    "crates/spawn-node-gateway/src/main.rs",
    "crates/spawn-postgres/src/testing.rs",
];

const FORBIDDEN_ENV_SYMS: &[&str] = &["var", "var_os", "vars", "vars_os"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    path: PathBuf,
    line: usize,
    snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delim {
    Brace,
    Paren,
    Bracket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    RawIdent(String),
    ColonColon,
    Semi,
    Comma,
    Star,
    Pound,
    Bang,
    Dollar,
    Open(Delim),
    Close(Delim),
    Literal,
    Lifetime,
    Other(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    line: usize,
}

impl Token {
    fn ident_name(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(s) | TokenKind::RawIdent(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn is_kw(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Ident(s) if s == expected)
    }
}

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

fn is_allowlisted_env_boundary(repo_root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    ENV_BOUNDARY_ALLOWLIST
        .iter()
        .any(|allowed| rel == Path::new(allowed))
}

// ---------------------------------------------------------------------------
// Layer A: Rust Lexer
// ---------------------------------------------------------------------------

fn skip_block_comment(bytes: &[u8], mut i: usize, line: &mut usize) -> usize {
    let mut depth = 1usize;
    while i < bytes.len() && depth > 0 {
        if bytes[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if bytes[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
        } else {
            if bytes[i] == b'\n' {
                *line += 1;
            }
            i += 1;
        }
    }
    i
}

fn try_lex_raw_string(bytes: &[u8], start: usize, line: &mut usize) -> Option<usize> {
    let idx = if bytes[start..].starts_with(b"br") || bytes[start..].starts_with(b"cr") {
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
        if bytes[i] == b'\n' {
            *line += 1;
        } else if bytes[i] == b'"'
            && i + 1 + hashes <= bytes.len()
            && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
        {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }
    Some(bytes.len())
}

fn try_lex_quoted_string(bytes: &[u8], start: usize, line: &mut usize) -> Option<usize> {
    let idx = if bytes[start] == b'"' {
        start + 1
    } else if matches!(bytes.get(start), Some(b'b' | b'c')) && bytes.get(start + 1) == Some(&b'"') {
        start + 2
    } else {
        return None;
    };
    let mut i = idx;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    *line += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'"' => return Some(i + 1),
            b'\n' => {
                *line += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Some(bytes.len())
}

fn try_lex_char_or_lifetime(src: &str, bytes: &[u8], start: usize) -> Option<(TokenKind, usize)> {
    let (is_byte, q) = if bytes[start] == b'b' && bytes.get(start + 1) == Some(&b'\'') {
        (true, start + 1)
    } else if bytes[start] == b'\'' {
        (false, start)
    } else {
        return None;
    };
    let after = q + 1;
    if after >= bytes.len() {
        return None;
    }
    if bytes[after] == b'\\' {
        if after + 1 >= bytes.len() {
            return None;
        }
        // Step over both `\` and the escaped character (including `\\` and `\'`).
        let mut i = after + 2;
        while i < bytes.len() && bytes[i] != b'\'' && bytes[i] != b'\n' {
            i += 1;
        }
        return (bytes.get(i) == Some(&b'\'')).then_some((TokenKind::Literal, i + 1));
    }
    if let Some(ch) = src[after..].chars().next()
        && ch != '\''
        && ch != '\n'
    {
        let next = after + ch.len_utf8();
        if bytes.get(next) == Some(&b'\'') {
            return Some((TokenKind::Literal, next + 1));
        }
    }
    if !is_byte {
        let ch = src[after..].chars().next()?;
        if ch.is_alphabetic() || ch == '_' {
            let mut end = after + ch.len_utf8();
            for c in src[end..].chars() {
                if c.is_alphanumeric() || c == '_' {
                    end += c.len_utf8();
                } else {
                    break;
                }
            }
            return Some((TokenKind::Lifetime, end));
        }
    }
    None
}

fn try_lex_ident(src: &str, bytes: &[u8], start: usize) -> Option<(TokenKind, usize)> {
    let (is_raw, ident_start) = if bytes[start..].starts_with(b"r#") {
        (true, start + 2)
    } else {
        (false, start)
    };
    let first = src[ident_start..].chars().next()?;
    if !first.is_alphabetic() && first != '_' {
        return None;
    }
    let mut end = ident_start + first.len_utf8();
    for ch in src[end..].chars() {
        if ch.is_alphanumeric() || ch == '_' {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    let text = src[ident_start..end].to_string();
    let kind = if is_raw {
        TokenKind::RawIdent(text)
    } else {
        TokenKind::Ident(text)
    };
    Some((kind, end))
}

fn lex_punct(bytes: &[u8], i: usize) -> (TokenKind, usize) {
    match bytes[i] {
        b':' if bytes.get(i + 1) == Some(&b':') => (TokenKind::ColonColon, i + 2),
        b';' => (TokenKind::Semi, i + 1),
        b',' => (TokenKind::Comma, i + 1),
        b'*' => (TokenKind::Star, i + 1),
        b'#' => (TokenKind::Pound, i + 1),
        b'!' => (TokenKind::Bang, i + 1),
        b'$' => (TokenKind::Dollar, i + 1),
        b'{' => (TokenKind::Open(Delim::Brace), i + 1),
        b'}' => (TokenKind::Close(Delim::Brace), i + 1),
        b'(' => (TokenKind::Open(Delim::Paren), i + 1),
        b')' => (TokenKind::Close(Delim::Paren), i + 1),
        b'[' => (TokenKind::Open(Delim::Bracket), i + 1),
        b']' => (TokenKind::Close(Delim::Bracket), i + 1),
        other => (TokenKind::Other(other as char), i + 1),
    }
}

fn tokenize(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line += 1;
            i += 1;
        } else if b.is_ascii_whitespace() {
            i += 1;
        } else if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            i = skip_block_comment(bytes, i + 2, &mut line);
        } else if let Some(end) = try_lex_raw_string(bytes, i, &mut line)
            .or_else(|| try_lex_quoted_string(bytes, i, &mut line))
        {
            out.push(Token {
                kind: TokenKind::Literal,
                line,
            });
            i = end;
        } else if let Some((kind, end)) =
            try_lex_char_or_lifetime(src, bytes, i).or_else(|| try_lex_ident(src, bytes, i))
        {
            out.push(Token { kind, line });
            i = end;
        } else if !b.is_ascii() {
            let ch = src[i..].chars().next().expect("non-empty UTF-8 char");
            out.push(Token {
                kind: TokenKind::Other(ch),
                line,
            });
            i += ch.len_utf8();
        } else {
            let (kind, end) = lex_punct(bytes, i);
            out.push(Token { kind, line });
            i = end;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Layer B: Fail-Closed #[cfg(test)] Stripping (#![cfg(test)] & #[cfg(test)] mod { ... })
// ---------------------------------------------------------------------------

fn matching_close(tokens: &[Token], open_idx: usize) -> Option<usize> {
    let TokenKind::Open(open) = tokens.get(open_idx)?.kind else {
        return None;
    };
    let mut stack = vec![open];
    let mut i = open_idx + 1;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(d) => stack.push(d),
            TokenKind::Close(d) => {
                if stack.pop() != Some(d) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, tok) in tokens.iter().enumerate() {
        match tok.kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => depth = depth.saturating_sub(1),
            TokenKind::Comma if depth == 0 => {
                if start < idx {
                    parts.push(&tokens[start..idx]);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    parts
}

fn is_cfg_test_pred(tokens: &[Token]) -> bool {
    if tokens.len() == 1 {
        return tokens[0].is_kw("test");
    }
    if tokens.len() >= 3
        && tokens[1].kind == TokenKind::Open(Delim::Paren)
        && matching_close(tokens, 1) == Some(tokens.len() - 1)
    {
        let args = split_top_level_commas(&tokens[2..tokens.len() - 1]);
        if tokens[0].is_kw("all") {
            return args.iter().any(|arg| is_cfg_test_pred(arg));
        }
        if tokens[0].is_kw("any") {
            return !args.is_empty() && args.iter().all(|arg| is_cfg_test_pred(arg));
        }
    }
    false
}

fn is_cfg_test_attr(inner: &[Token]) -> bool {
    inner.len() >= 3
        && inner[0].is_kw("cfg")
        && inner[1].kind == TokenKind::Open(Delim::Paren)
        && matching_close(inner, 1) == Some(inner.len() - 1)
        && is_cfg_test_pred(&inner[2..inner.len() - 1])
}

fn cfg_test_mod_group_end(tokens: &[Token], mut i: usize) -> Option<usize> {
    let mut has_cfg_test = false;
    while tokens.get(i).is_some_and(|t| t.kind == TokenKind::Pound)
        && tokens
            .get(i + 1)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
        && let Some(end_bracket) = matching_close(tokens, i + 1)
    {
        if is_cfg_test_attr(&tokens[i + 2..end_bracket]) {
            has_cfg_test = true;
        }
        i = end_bracket + 1;
    }
    if !has_cfg_test {
        return None;
    }
    if tokens.get(i).is_some_and(|t| t.is_kw("pub")) {
        i += 1;
        if tokens
            .get(i)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Paren))
            && let Some(close) = matching_close(tokens, i)
        {
            i = close + 1;
        }
    }
    if !tokens.get(i).is_some_and(|t| t.is_kw("mod")) {
        return None;
    }
    let name_ok = tokens.get(i + 1).and_then(Token::ident_name).is_some();
    let brace_open = i + 2;
    if !name_ok
        || !tokens
            .get(brace_open)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Brace))
    {
        return None;
    }
    matching_close(tokens, brace_open).map(|close| close + 1)
}

fn strip_cfg_test_tokens(tokens: &[Token]) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    let mut brace_depth = 0usize;
    let mut inner_skip_target: Option<usize> = None;

    while i < tokens.len() {
        if let Some(target_depth) = inner_skip_target {
            match tokens[i].kind {
                TokenKind::Open(Delim::Brace) => brace_depth += 1,
                TokenKind::Close(Delim::Brace) => {
                    if brace_depth <= target_depth {
                        inner_skip_target = None;
                        continue;
                    }
                    brace_depth -= 1;
                }
                _ => {}
            }
            i += 1;
            continue;
        }

        if tokens[i].kind == TokenKind::Pound
            && tokens.get(i + 1).is_some_and(|t| t.kind == TokenKind::Bang)
            && tokens
                .get(i + 2)
                .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
            && let Some(end_bracket) = matching_close(tokens, i + 2)
            && is_cfg_test_attr(&tokens[i + 3..end_bracket])
        {
            inner_skip_target = Some(brace_depth);
            i = end_bracket + 1;
            continue;
        }

        if tokens[i].kind == TokenKind::Pound
            && tokens
                .get(i + 1)
                .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
            && let Some(mod_end) = cfg_test_mod_group_end(tokens, i)
        {
            i = mod_end;
            continue;
        }

        match tokens[i].kind {
            TokenKind::Open(Delim::Brace) => brace_depth += 1,
            TokenKind::Close(Delim::Brace) => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Layer C: Two-Pass Production Token Audit
// ---------------------------------------------------------------------------

fn is_env_module_path(path: &[String], env_aliases: &BTreeSet<String>) -> bool {
    path.ends_with(&["std".to_string(), "env".to_string()])
        || path == ["env"]
        || (path.len() == 1 && env_aliases.contains(&path[0]))
        || (path.len() == 2 && path[0] == "self" && env_aliases.contains(&path[1]))
}

fn check_leaf_use(
    path: &[String],
    alias: Option<&str>,
    is_pub: bool,
    line: usize,
    env_aliases: &mut BTreeSet<String>,
    violated_lines: &mut BTreeSet<usize>,
) {
    let eff: Vec<String> = if path.last().is_some_and(|s| s == "self") {
        path[..path.len() - 1].to_vec()
    } else {
        path.to_vec()
    };
    if eff.is_empty() {
        return;
    }
    if is_env_module_path(&eff, env_aliases) {
        if let Some(name) = alias {
            env_aliases.insert(name.to_string());
            // Flag renamed `std::env` imports in non-allowlisted production files so
            // cross-module alias re-exports (`pub(crate) use std::env as zz;`) cannot slip by.
            violated_lines.insert(line);
        } else if is_pub {
            // Also flag unaliased public re-exports (`pub use std::env;`, `pub(crate) use std::{env};`)
            // so another module cannot bypass explicit `std::env` paths via `use ...::env`.
            violated_lines.insert(line);
        }
        return;
    }
    let parent = &eff[..eff.len() - 1];
    let sym = eff[eff.len() - 1].as_str();
    if is_env_module_path(parent, env_aliases) && FORBIDDEN_ENV_SYMS.contains(&sym) {
        violated_lines.insert(line);
    }
}

fn audit_use_subtree(
    slice: &[Token],
    prefix: &[String],
    is_pub: bool,
    env_aliases: &mut BTreeSet<String>,
    violated_lines: &mut BTreeSet<usize>,
) {
    let mut i = usize::from(
        slice
            .first()
            .is_some_and(|t| t.kind == TokenKind::ColonColon),
    );
    let mut cur_path = prefix.to_vec();
    let mut leaf_line = slice.first().map_or(1, |t| t.line);

    while i < slice.len() {
        let tok = &slice[i];
        if tok.kind == TokenKind::Star {
            if is_env_module_path(&cur_path, env_aliases) {
                violated_lines.insert(tok.line);
            }
            return;
        }
        if tok.kind == TokenKind::Open(Delim::Brace) {
            if let Some(close) = matching_close(slice, i) {
                for item in split_top_level_commas(&slice[i + 1..close]) {
                    audit_use_subtree(item, &cur_path, is_pub, env_aliases, violated_lines);
                }
            }
            return;
        }
        if tok.is_kw("as") {
            let alias = slice.get(i + 1).and_then(Token::ident_name);
            let err_line = slice.get(i + 1).map_or(leaf_line, |t| t.line);
            check_leaf_use(
                &cur_path,
                alias,
                is_pub,
                err_line,
                env_aliases,
                violated_lines,
            );
            return;
        }
        if let Some(seg) = tok.ident_name() {
            cur_path.push(seg.to_string());
            leaf_line = tok.line;
            i += 1;
            if slice
                .get(i)
                .is_some_and(|t| t.kind == TokenKind::ColonColon)
            {
                i += 1;
            }
            continue;
        }
        i += 1;
    }

    if cur_path.len() > prefix.len() {
        check_leaf_use(
            &cur_path,
            None,
            is_pub,
            leaf_line,
            env_aliases,
            violated_lines,
        );
    }
}

fn is_pub_before(tokens: &[Token], use_idx: usize) -> bool {
    if use_idx == 0 {
        return false;
    }
    let prev = &tokens[use_idx - 1];
    if prev.is_kw("pub") {
        return true;
    }
    if prev.kind == TokenKind::Close(Delim::Paren) {
        let mut depth = 1usize;
        let mut j = use_idx - 1;
        while j > 0 {
            j -= 1;
            match tokens[j].kind {
                TokenKind::Close(Delim::Paren) => depth += 1,
                TokenKind::Open(Delim::Paren) => {
                    depth -= 1;
                    if depth == 0 {
                        return j > 0 && tokens[j - 1].is_kw("pub");
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn collect_use_slices(tokens: &[Token]) -> Vec<(bool, &[Token])> {
    let mut uses = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].is_kw("use") {
            let is_pub = is_pub_before(tokens, i);
            let mut end = i + 1;
            let mut depth = 0usize;
            while end < tokens.len() {
                match tokens[end].kind {
                    TokenKind::Open(_) => depth += 1,
                    TokenKind::Close(_) => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    TokenKind::Semi if depth == 0 => {
                        end += 1;
                        break;
                    }
                    _ => {}
                }
                end += 1;
            }
            let slice_end = if end > i + 1 && tokens[end - 1].kind == TokenKind::Semi {
                end - 1
            } else {
                end
            };
            uses.push((is_pub, &tokens[i + 1..slice_end]));
            i = end;
        } else {
            i += 1;
        }
    }
    uses
}

fn audit_use_trees(tokens: &[Token], violated_lines: &mut BTreeSet<usize>) -> BTreeSet<String> {
    let uses = collect_use_slices(tokens);
    let mut env_aliases = BTreeSet::new();
    loop {
        let before = env_aliases.len();
        for &(is_pub, u) in &uses {
            audit_use_subtree(u, &[], is_pub, &mut env_aliases, violated_lines);
        }
        if env_aliases.len() == before {
            break;
        }
    }
    env_aliases
}

fn audit_path_accesses(
    tokens: &[Token],
    env_aliases: &BTreeSet<String>,
    violated_lines: &mut BTreeSet<usize>,
) {
    for i in 0..tokens.len() {
        if tokens[i].kind == TokenKind::Dollar
            && tokens.get(i + 1).and_then(Token::ident_name).is_some()
            && tokens
                .get(i + 2)
                .is_some_and(|t| t.kind == TokenKind::ColonColon)
            && tokens
                .get(i + 3)
                .and_then(Token::ident_name)
                .is_some_and(|sym| FORBIDDEN_ENV_SYMS.contains(&sym))
        {
            violated_lines.insert(tokens[i].line);
            continue;
        }

        let Some(mod_name) = tokens[i].ident_name() else {
            continue;
        };
        if mod_name != "env" && !env_aliases.contains(mod_name) {
            continue;
        }
        if !tokens
            .get(i + 1)
            .is_some_and(|t| t.kind == TokenKind::ColonColon)
        {
            continue;
        }
        let Some(next) = tokens.get(i + 2) else {
            continue;
        };
        if next.kind == TokenKind::Dollar
            || next
                .ident_name()
                .is_some_and(|sym| FORBIDDEN_ENV_SYMS.contains(&sym))
        {
            violated_lines.insert(tokens[i].line);
        }
    }
}

fn violations_from_lines(
    rel_or_abs_path: &Path,
    src: &str,
    lines: BTreeSet<usize>,
) -> Vec<Violation> {
    let orig_lines: Vec<&str> = src.lines().collect();
    lines
        .into_iter()
        .map(|line| Violation {
            path: rel_or_abs_path.to_path_buf(),
            line,
            snippet: orig_lines
                .get(line.saturating_sub(1))
                .map_or_else(String::new, |s| s.trim().to_string()),
        })
        .collect()
}

fn scan_source(rel_or_abs_path: &Path, src: &str) -> Vec<Violation> {
    let tokens = tokenize(src);
    let prod_tokens = strip_cfg_test_tokens(&tokens);
    let mut violated_lines = BTreeSet::new();
    let env_aliases = audit_use_trees(&prod_tokens, &mut violated_lines);
    audit_path_accesses(&prod_tokens, &env_aliases, &mut violated_lines);
    violations_from_lines(rel_or_abs_path, src, violated_lines)
}

fn scan_whoami_source(rel_or_abs_path: &Path, src: &str) -> Vec<Violation> {
    let tokens = tokenize(src);
    let prod_tokens = strip_cfg_test_tokens(&tokens);
    let violated_lines: BTreeSet<usize> = prod_tokens
        .iter()
        .filter(|tok| tok.ident_name() == Some("whoami"))
        .map(|tok| tok.line)
        .collect();
    violations_from_lines(rel_or_abs_path, src, violated_lines)
}

fn is_forbidden_lint_args(args: &[Token]) -> bool {
    let mut i = 0usize;
    while i < args.len() {
        match args[i].ident_name() {
            Some("disallowed_methods" | "warnings") => return true,
            Some("clippy") => {
                let is_group_or_bare = match args.get(i + 1).map(|t| &t.kind) {
                    Some(TokenKind::ColonColon) => matches!(
                        args.get(i + 2).and_then(Token::ident_name),
                        Some("all" | "style")
                    ),
                    _ => true,
                };
                if is_group_or_bare {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

fn attribute_contains_forbidden_lint(inside: &[Token]) -> bool {
    let mut i = 0usize;
    while i < inside.len() {
        if matches!(inside[i].ident_name(), Some("allow" | "expect"))
            && inside
                .get(i + 1)
                .is_some_and(|t| t.kind == TokenKind::Open(Delim::Paren))
            && let Some(paren_close) = matching_close(inside, i + 1)
        {
            if is_forbidden_lint_args(&inside[i + 2..paren_close]) {
                return true;
            }
            i = paren_close + 1;
            continue;
        }
        i += 1;
    }
    false
}

fn scan_disallowed_methods_allows(rel_or_abs_path: &Path, src: &str) -> Vec<Violation> {
    let tokens = tokenize(src);
    let prod_tokens = strip_cfg_test_tokens(&tokens);
    let mut violated_lines = BTreeSet::new();
    let mut i = 0usize;
    while i < prod_tokens.len() {
        if prod_tokens[i].kind != TokenKind::Pound {
            i += 1;
            continue;
        }
        let bracket_idx = if prod_tokens
            .get(i + 1)
            .is_some_and(|t| t.kind == TokenKind::Bang)
        {
            i + 2
        } else {
            i + 1
        };
        if !prod_tokens
            .get(bracket_idx)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
        {
            i += 1;
            continue;
        }
        let Some(close_idx) = matching_close(&prod_tokens, bracket_idx) else {
            i += 1;
            continue;
        };
        let inside = &prod_tokens[bracket_idx + 1..close_idx];
        if attribute_contains_forbidden_lint(inside) {
            violated_lines.insert(prod_tokens[i].line);
        }
        i = close_idx + 1;
    }
    violations_from_lines(rel_or_abs_path, src, violated_lines)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClippyDisallowedAudit {
    allowlisted_diagnostics: usize,
    violations: Vec<String>,
}

fn normalize_diagnostic_path(manifest_dir: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    let rel_owned = if path.is_absolute() {
        if let Ok(stripped) = path.strip_prefix(manifest_dir) {
            stripped.to_path_buf()
        } else if let (Ok(canon_root), Ok(canon_path)) =
            (fs::canonicalize(manifest_dir), fs::canonicalize(path))
            && let Ok(stripped) = canon_path.strip_prefix(&canon_root)
        {
            stripped.to_path_buf()
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    let normalized = rel_owned.to_string_lossy().replace('\\', "/");
    normalized
        .strip_prefix("./")
        .unwrap_or(&normalized)
        .to_string()
}

fn collect_span_files(span: &Value, manifest_dir: &Path, out: &mut BTreeSet<(String, u64)>) {
    if let Some(file_name) = span.get("file_name").and_then(Value::as_str)
        && !file_name.is_empty()
        && !file_name.starts_with('<')
    {
        let line = span.get("line_start").and_then(Value::as_u64).unwrap_or(0);
        out.insert((normalize_diagnostic_path(manifest_dir, file_name), line));
    }
    if let Some(expansion) = span.get("expansion") {
        if let Some(call_site) = expansion.get("span") {
            collect_span_files(call_site, manifest_dir, out);
        }
        if let Some(def_site) = expansion.get("def_site_span") {
            collect_span_files(def_site, manifest_dir, out);
        }
    }
}

fn parse_clippy_disallowed_methods_json(
    manifest_dir: &Path,
    stdout: &str,
    allowlist: &[&str],
) -> ClippyDisallowedAudit {
    let mut allowlisted_diagnostics = 0usize;
    let mut violations = Vec::new();
    for line in stdout.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = obj.get("message") else {
            continue;
        };
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str);
        if code != Some("clippy::disallowed_methods") {
            continue;
        }

        let spans = msg
            .get("spans")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let primary_spans: Vec<&Value> = spans
            .iter()
            .filter(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
            .collect();
        let selected: Vec<&Value> = if primary_spans.is_empty() {
            spans.iter().collect()
        } else {
            primary_spans
        };

        let mut locs = BTreeSet::new();
        for span in selected {
            collect_span_files(span, manifest_dir, &mut locs);
        }

        if !locs.is_empty()
            && locs
                .iter()
                .all(|(path, _)| allowlist.contains(&path.as_str()))
        {
            allowlisted_diagnostics += 1;
        } else {
            let summary = msg
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("clippy::disallowed_methods");
            let location = if locs.is_empty() {
                "<unknown>".to_string()
            } else {
                locs.iter()
                    .map(|(path, ln)| format!("{path}:{ln}"))
                    .collect::<Vec<_>>()
                    .join(" <- ")
            };
            violations.push(format!("{location}: {summary}"));
        }
    }
    ClippyDisallowedAudit {
        allowlisted_diagnostics,
        violations,
    }
}

fn run_clippy_force_warn_disallowed_methods(
    manifest_path: &Path,
    target_dir: Option<&Path>,
    allowlist: &[&str],
) -> ClippyDisallowedAudit {
    let manifest_dir = manifest_path
        .parent()
        .expect("Cargo.toml has parent directory");
    assert!(
        manifest_dir.join("clippy.toml").is_file(),
        "expected workspace root clippy.toml under {}",
        manifest_dir.display()
    );
    let mut cmd = Command::new(env!("CARGO"));
    // Pin CLIPPY_CONF_DIR to the workspace root containing `clippy.toml` so
    // crate-local configs or inherited env vars cannot shadow `disallowed-methods`.
    cmd.env("CLIPPY_CONF_DIR", manifest_dir);
    cmd.args([
        "clippy",
        "--offline",
        "--locked",
        "--workspace",
        "--lib",
        "--bins",
        "--message-format=json",
        "--manifest-path",
    ])
    .arg(manifest_path);
    if let Some(dir) = target_dir {
        cmd.arg("--target-dir").arg(dir);
    }
    cmd.args(["--", "--force-warn", "clippy::disallowed_methods"]);

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("run cargo clippy for {}: {e}", manifest_path.display()));
    assert!(
        output.status.success(),
        "cargo clippy --force-warn failed for {} (status {}):\nstdout:\n{}\nstderr:\n{}",
        manifest_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_clippy_disallowed_methods_json(manifest_dir, &stdout, allowlist)
}

fn strip_toml_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_basic = false;
    let mut in_literal = false;
    while let Some(ch) = chars.next() {
        if in_basic {
            out.push(ch);
            if ch == '\\' {
                if let Some(esc) = chars.next() {
                    out.push(esc);
                }
            } else if ch == '"' {
                in_basic = false;
            }
            continue;
        }
        if in_literal {
            out.push(ch);
            if ch == '\'' {
                in_literal = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_basic = true;
                out.push(ch);
            }
            '\'' => {
                in_literal = true;
                out.push(ch);
            }
            '#' => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn disallowed_methods_array_slice(stripped: &str) -> Option<&str> {
    let key_idx = stripped.find("disallowed-methods")?;
    let rest = &stripped[key_idx + "disallowed-methods".len()..];
    let eq_idx = rest.find('=')?;
    let after_eq = &rest[eq_idx + 1..];
    let open_rel = after_eq.find('[')?;
    let body_start = key_idx + "disallowed-methods".len() + eq_idx + 1 + open_rel;
    let mut depth = 0usize;
    for (offset, ch) in stripped[body_start..].char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&stripped[body_start..=body_start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn missing_disallowed_env_methods(clippy_toml: &str) -> Vec<&'static str> {
    const REQUIRED: [&str; 4] = [
        "std::env::var",
        "std::env::var_os",
        "std::env::vars",
        "std::env::vars_os",
    ];
    let stripped = strip_toml_comments(clippy_toml);
    let Some(section) = disallowed_methods_array_slice(&stripped) else {
        return REQUIRED.to_vec();
    };
    REQUIRED
        .into_iter()
        .filter(|sym| {
            !section.contains(&format!("\"{sym}\"")) && !section.contains(&format!("'{sym}'"))
        })
        .collect()
}

fn parse_workspace_members(cargo_toml: &str) -> Vec<String> {
    let stripped = strip_toml_comments(cargo_toml);
    let ws_idx = stripped.find("[workspace]").expect("[workspace] section");
    let after_ws = &stripped[ws_idx..];
    let members_idx = after_ws.find("members").expect("members key");
    let after_members = &after_ws[members_idx..];
    let open_idx = after_members.find('[').expect("members '['");
    let close_idx = after_members[open_idx..].find(']').expect("members ']'") + open_idx;
    let list_body = &after_members[open_idx + 1..close_idx];
    let mut members = Vec::new();
    let mut rest = list_body;
    while let Some(q1) = rest.find('"') {
        let after = &rest[q1 + 1..];
        let Some(q2) = after.find('"') else {
            break;
        };
        members.push(after[..q2].to_string());
        rest = &after[q2 + 1..];
    }
    members
}

fn collect_unexpected_clippy_configs(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    let mut paths: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let meta =
            fs::symlink_metadata(&path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
        if meta.is_dir() {
            if dir == root && matches!(name, "target" | ".git") {
                continue;
            }
            collect_unexpected_clippy_configs(root, &path, out);
            continue;
        }
        if matches!(name, "clippy.toml" | ".clippy.toml") {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if rel != Path::new("clippy.toml") {
                out.push(rel);
            }
        }
    }
}

fn find_unexpected_clippy_configs(root: &Path) -> Vec<PathBuf> {
    let mut unexpected = Vec::new();
    collect_unexpected_clippy_configs(root, root, &mut unexpected);
    unexpected.sort();
    unexpected
}

// ---------------------------------------------------------------------------
// Cargo Dependency Graph Guard (`cargo tree --offline --locked --target all`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeAudit {
    seen_spawn_postgres: usize,
    violations: Vec<String>,
}

fn parse_cargo_tree_testing_violations(stdout: &str) -> TreeAudit {
    let mut seen_spawn_postgres = 0usize;
    let mut violations = Vec::new();
    for raw_line in stdout.lines() {
        let trimmed = raw_line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        if trimmed.split_whitespace().next() != Some("spawn-postgres") {
            continue;
        }
        seen_spawn_postgres += 1;
        let without_dedup = trimmed
            .trim_end()
            .strip_suffix("(*)")
            .unwrap_or(trimmed)
            .trim_end();
        if let Some(body) = without_dedup.strip_suffix(']')
            && let Some(open) = body.rfind('[')
            && body[open + 1..]
                .split(',')
                .any(|feat| feat.trim() == "testing")
        {
            violations.push(trimmed.to_string());
        }
    }
    TreeAudit {
        seen_spawn_postgres,
        violations,
    }
}

fn check_cargo_tree_no_prod_testing_feature(manifest_path: &Path) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--offline",
            "--locked",
            "--workspace",
            "-e",
            "normal,build",
            "--target",
            "all",
            "-f",
            "{p} [{f}]",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .output()
        .unwrap_or_else(|e| panic!("run cargo tree for {}: {e}", manifest_path.display()));
    assert!(
        output.status.success(),
        "cargo tree failed fail-closed for {} (status {}):\nstdout:\n{}\nstderr:\n{}",
        manifest_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let audit = parse_cargo_tree_testing_violations(&stdout);
    assert!(
        audit.seen_spawn_postgres > 0,
        "non-vacuous cargo tree check failed for {}: zero spawn-postgres rows observed in output:\n{stdout}",
        manifest_path.display(),
    );
    audit.violations
}

fn collect_workspace_prod_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for crate_dir in WORKSPACE_CRATES {
        collect_rs_files(&root.join(crate_dir).join("src"), &mut files);
    }
    assert!(!files.is_empty(), "expected workspace crate source files");
    files
}

#[test]
fn workspace_production_code_contains_zero_unallowlisted_env_var_or_suppress_allows() {
    let root = workspace_root();
    let files = collect_workspace_prod_files(&root);

    let mut env_violations = Vec::new();
    let mut allow_violations = Vec::new();
    for file in files {
        if is_allowlisted_env_boundary(&root, &file) {
            continue;
        }
        let rel = file.strip_prefix(&root).unwrap_or(&file);
        let text =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        env_violations.extend(scan_source(rel, &text));
        allow_violations.extend(scan_disallowed_methods_allows(rel, &text));
    }

    assert!(
        env_violations.is_empty(),
        "forbidden production env::var* call(s) found outside ENV_BOUNDARY_ALLOWLIST:\n{}",
        env_violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.path.display(), v.line, v.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        allow_violations.is_empty(),
        "forbidden production allow/expect(clippy::disallowed_methods) found outside ENV_BOUNDARY_ALLOWLIST:\n{}",
        allow_violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.path.display(), v.line, v.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn spawn_postgres_production_has_no_implicit_os_user_fallback() {
    let root = workspace_root();
    let mut files = Vec::new();
    collect_rs_files(&root.join("crates/spawn-postgres/src"), &mut files);
    let mut violations = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&root).unwrap_or(&file);
        let text =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        violations.extend(scan_whoami_source(rel, &text));
    }
    assert!(
        violations.is_empty(),
        "forbidden production whoami reference(s) found:\n{}",
        violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.path.display(), v.line, v.snippet))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let probe = scan_whoami_source(
        Path::new("synthetic.rs"),
        "pub fn bad() { let _ = whoami::username().ok(); }\n#[cfg(test)] mod tests { fn ok() { let _ = whoami::username(); } }\n",
    );
    assert_eq!(
        probe.len(),
        1,
        "negative probe must catch production whoami: {probe:?}"
    );
    assert_eq!(probe[0].line, 1);
}

#[test]
fn workspace_crates_list_matches_root_cargo_toml_and_clippy_toml_forbids_all_env_methods() {
    let root = workspace_root();
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let mut actual = parse_workspace_members(&cargo_toml);
    actual.sort();
    let mut expected: Vec<String> = WORKSPACE_CRATES.iter().map(|s| (*s).to_string()).collect();
    expected.sort();
    assert_eq!(actual, expected, "WORKSPACE_CRATES drifted from Cargo.toml");

    let clippy_toml = fs::read_to_string(root.join("clippy.toml")).expect("read root clippy.toml");
    let missing = missing_disallowed_env_methods(&clippy_toml);
    assert!(
        missing.is_empty(),
        "root clippy.toml is missing required std::env entries: {missing:?}"
    );

    let unexpected_clippy_configs = find_unexpected_clippy_configs(&root);
    assert!(
        unexpected_clippy_configs.is_empty(),
        "unexpected nested or dot clippy config(s) outside root clippy.toml: {unexpected_clippy_configs:?}"
    );
}

#[test]
fn no_production_crate_enables_spawn_postgres_testing_feature() {
    let manifest = workspace_root().join("Cargo.toml");
    let violations = check_cargo_tree_no_prod_testing_feature(&manifest);
    assert!(
        violations.is_empty(),
        "forbidden production activation of spawn-postgres/testing:\n{}",
        violations.join("\n")
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

#[test]
fn env_boundary_guard_catches_all_cfg_field_and_import_bypass_mutations() {
    let cfg_and_char_samples = r##"
pub struct Store<'a> {
    root: &'a str,
    #[cfg(test)]
    pub(crate) after_pinned: Option<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Store<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = std::env::var("STORE_IMPL_BYPASS");
        f.debug_struct("Store").field("root", &self.root).finish()
    }
}

#[cfg(test)]
thread_local! {
    static TL: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn fn_after_thread_local() -> Option<String> {
    std::env::var("AFTER_THREAD_LOCAL").ok()
}

pub fn match_block_and_backslash_char(ch: char, b: u8) -> Option<char> {
    match ch {
        #[cfg(test)]
        't' => {
            let _ = 1;
            None
        }
        'a' => {
            let _v = std::env::var("NEXT_MATCH_ARM");
            None
        }
        '\\' => std::env::var("BACKSLASH_CHAR").ok().map(|_| 'x'),
        _ if b == b'\\' => std::env::var("BYTE_BACKSLASH_CHAR").ok().map(|_| 'y'),
        _ => None,
    }
}

pub fn brace_macro_and_raw_strings<'a>() -> Option<String> {
    #[cfg(test)]
    eprintln! { "x" }
    let _c = (',', '}', '\'', '❯', r#"std::env::var("RAW_IGNORED")"#);
    let _keep: &'a str = self::after_lifetime();
    let v = std::env::var("AFTER_BRACE_MACRO").ok();
    v
}

#[cfg(not(test))]
pub fn cfg_not_test_is_prod() -> Option<String> {
    std::env::var("NOT_TEST_PROD").ok()
}

#[cfg(any(test, unix))]
pub fn cfg_any_unix_is_prod() -> Option<String> {
    std::env::var("ANY_UNIX_PROD").ok()
}

#[cfg(all(test, feature = "testing"))]
mod gated_tests {
    fn allowed() {
        let _ = std::env::var("ALL_TEST_ALLOWED");
    }
}

pub mod inner_cfg_mod {
    #![cfg(test)]
    pub fn inner_allowed() {
        let _ = std::env::var("INNER_CFG_ALLOWED");
    }
}

pub fn after_inner_cfg_mod() -> Option<String> {
    std::env::var("AFTER_INNER_MOD").ok()
}
"##;
    let found_cfg = scan_source(Path::new("cfg_and_chars.rs"), cfg_and_char_samples);
    let cfg_lines: Vec<usize> = found_cfg.iter().map(|v| v.line).collect();
    assert_eq!(
        cfg_lines,
        vec![10, 21, 32, 35, 36, 46, 52, 57, 75],
        "got {found_cfg:?}"
    );
}

#[test]
fn env_boundary_guard_catches_alias_glob_nested_wrapped_and_macro_bypasses() {
    let import_and_macro_bypasses = r#"
pub(crate) use std::env as zz;
use zz as e2;
use std::env::*;
use std::{env::{var as a, var_os as b}, fmt};
use std::env::{
    Args,
    current_dir,
    var as v,
};

macro_rules! grab_fn {
    ($f:ident) => {
        std::env::$f("MACRO_FN_BYPASS")
    };
}

macro_rules! grab_mod {
    ($m:ident) => {
        std::$m::var("MACRO_MOD_BYPASS")
    };
}

pub fn use_bypasses() {
    let _ = zz::var("ALIAS_1");
    let _ = e2::var_os("ALIAS_2");
    let _ = var("GLOB_CALL");
}
"#;
    let found_imp = scan_source(Path::new("bypasses.rs"), import_and_macro_bypasses);
    let imp_lines: Vec<usize> = found_imp.iter().map(|v| v.line).collect();
    assert_eq!(
        imp_lines,
        vec![2, 3, 4, 5, 9, 14, 20, 25, 26],
        "got {found_imp:?}"
    );
}

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "spawn-env-guard-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp workspace");
        Self(dir)
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, content).expect("write");
    }

    fn root_manifest(&self) -> PathBuf {
        self.0.join("Cargo.toml")
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_temp_cargo_tree_case(
    prefix: &str,
    pg_features: &str,
    consumer_manifest_tail: &str,
) -> Vec<String> {
    let ws = TempWorkspace::new(prefix);
    ws.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"spawn-postgres\", \"consumer\"]\n\n[workspace.dependencies]\nspawn-postgres = { path = \"spawn-postgres\" }\n",
    );
    ws.write(
        "spawn-postgres/Cargo.toml",
        &format!(
            "[package]\nname = \"spawn-postgres\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\n{pg_features}"
        ),
    );
    ws.write("spawn-postgres/src/lib.rs", "");
    ws.write(
        "consumer/Cargo.toml",
        &format!(
            "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{consumer_manifest_tail}"
        ),
    );
    ws.write("consumer/src/lib.rs", "");
    let lock = Command::new(env!("CARGO"))
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(ws.root_manifest())
        .output()
        .expect("generate lockfile for temp workspace");
    assert!(
        lock.status.success(),
        "cargo generate-lockfile --offline failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&lock.stdout),
        String::from_utf8_lossy(&lock.stderr),
    );
    check_cargo_tree_no_prod_testing_feature(&ws.root_manifest())
}

#[test]
fn cargo_tree_guard_catches_all_production_testing_feature_forms_and_allows_dev_deps() {
    let synthetic = "\
consumer v0.1.0 (/tmp/consumer) []
├── spawn-postgres v0.1.0 (/tmp/crates[1]/spawn-postgres) []
├── spawn-postgres v0.1.0 (/tmp/crates[1]/spawn-postgres) [default,testing] (*)
└── spawn-postgres v0.1.0 (/tmp/crates/spawn-postgres) [testing]
";
    let parsed_synthetic = parse_cargo_tree_testing_violations(synthetic);
    assert_eq!(parsed_synthetic.seen_spawn_postgres, 3);
    assert_eq!(parsed_synthetic.violations.len(), 2);

    let unrelated = "consumer v0.1.0 (/tmp/consumer) []\n";
    let parsed_unrelated = parse_cargo_tree_testing_violations(unrelated);
    assert_eq!(parsed_unrelated.seen_spawn_postgres, 0);
    assert!(parsed_unrelated.violations.is_empty());

    let pg_clean = "default = []\ntesting = []\n";
    let pg_default_bad = "default = [\"testing\"]\ntesting = []\n";

    // 1. Clean: `[dev-dependencies]` enables `testing`, normal deps do not.
    assert!(
        run_temp_cargo_tree_case(
            "dev-ok",
            pg_clean,
            "[dependencies]\nspawn-postgres.workspace = true\n[dev-dependencies]\nspawn-postgres = { path = \"../spawn-postgres\", features = [\"testing\"] }\n",
        )
        .is_empty()
    );
    // 2. Dotted key `spawn-postgres.features = ["testing"]`.
    assert!(
        !run_temp_cargo_tree_case(
            "dotted",
            pg_clean,
            "[dependencies]\nspawn-postgres.workspace = true\nspawn-postgres.features = [\"testing\"]\n",
        )
        .is_empty()
    );
    // 3. `default = ["testing"]` inside `spawn-postgres/Cargo.toml`.
    assert!(
        !run_temp_cargo_tree_case(
            "default-self",
            pg_default_bad,
            "[dependencies]\nspawn-postgres.workspace = true\n",
        )
        .is_empty()
    );
    // 4. `default = ["testing"]` + `testing = ["spawn-postgres/testing"]` in consumer.
    assert!(
        !run_temp_cargo_tree_case(
            "forwarded",
            pg_clean,
            "[features]\ndefault = [\"testing\"]\ntesting = [\"spawn-postgres/testing\"]\n\n[dependencies]\nspawn-postgres.workspace = true\n",
        )
        .is_empty()
    );
    // 5. Renamed dependency `zz-pg = { package = "spawn-postgres", ..., features = ["testing"] }`.
    assert!(
        !run_temp_cargo_tree_case(
            "renamed",
            pg_clean,
            "[dependencies]\nzz-pg = { package = \"spawn-postgres\", path = \"../spawn-postgres\", features = [\"testing\"] }\n",
        )
        .is_empty()
    );
    // 6. Target-specific dependency caught by `--target all`.
    assert!(
        !run_temp_cargo_tree_case(
            "target-cfg-windows",
            pg_clean,
            "[target.'cfg(windows)'.dependencies]\nspawn-postgres = { path = \"../spawn-postgres\", features = [\"testing\"] }\n",
        )
        .is_empty()
    );
}

#[test]
fn env_boundary_guard_catches_unaliased_pub_reexports_and_allows_private_use() {
    let sample = r"
pub use std::env;
pub(crate) use std::env;
pub(super) use std::{env, fmt};
pub(in crate::a) use std::env::{self};

use std::env;
pub fn ok() {
    let _ = env::current_dir();
}
";
    let found = scan_source(Path::new("reexports.rs"), sample);
    let lines: Vec<usize> = found.iter().map(|v| v.line).collect();
    assert_eq!(lines, vec![2, 3, 4, 5], "got {found:?}");
}

#[test]
fn env_boundary_guard_verifies_clippy_toml_and_prod_disallowed_allows() {
    let incomplete = "disallowed-methods = [\n  { path = \"std::env::var\" },\n  # { path = \"std::env::var_os\" },\n]\n";
    assert_eq!(
        missing_disallowed_env_methods(incomplete),
        vec!["std::env::var_os", "std::env::vars", "std::env::vars_os"]
    );

    let prod_allows = r#"
#[allow(clippy::disallowed_methods)]
fn a() {}

fn b() {
    #![allow(clippy::disallowed_methods)]
}

#[expect(clippy::disallowed_methods)]
fn c() {}

#[cfg_attr(unix, allow(clippy::disallowed_methods))]
fn d() {}

#[allow(clippy::style)]
fn e_style() {
    macro_rules! grab {
        ($m:ident, $f:ident) => {
            std::$m::$f("BYPASS")
        };
    }
    let _ = grab!(env, var);
}

#[allow(clippy::all)]
fn f_all() {}

#[expect(clippy::style)]
fn g_expect_style() {}

#[allow(warnings)]
fn h_warnings() {}

fn i_inner_all() {
    #![allow(clippy::all)]
}

#[cfg_attr(unix, allow(clippy::style))]
fn j_cfg_style() {}

#[allow(clippy)]
fn k_bare_clippy() {}

#[cfg_attr(all(unix), allow(clippy::too_many_lines))]
fn ok_unrelated_clippy_and_cfg_all() {}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    #![allow(clippy::all)]
    #[test]
    fn ok() {}
}
"#;
    let found = scan_disallowed_methods_allows(Path::new("allows.rs"), prod_allows);
    let lines: Vec<usize> = found.iter().map(|v| v.line).collect();
    assert_eq!(
        lines,
        vec![2, 6, 9, 12, 15, 25, 28, 31, 35, 38, 41],
        "got {found:?}"
    );

    let test_file = "\n#![cfg(test)]\n#![allow(clippy::disallowed_methods)]\n#[test]\nfn ok() {}\n";
    assert!(scan_disallowed_methods_allows(Path::new("tests.rs"), test_file).is_empty());
}

#[test]
fn env_boundary_guard_strips_cfg_test_mod_outer_attrs_regardless_of_order() {
    let sample = r#"
#[allow(clippy::all)]
#[cfg(test)]
mod lint_first {
    fn test_only() {
        let _ = std::env::var("TEST_ONLY_LINT_FIRST");
    }
}

#[expect(clippy::disallowed_methods)]
#[cfg(all(test, unix))]
pub(crate) mod expect_first {
    fn test_only() {
        let _ = std::env::var_os("TEST_ONLY_EXPECT_FIRST");
    }
}

#[cfg(test)]
#[allow(clippy::all)]
mod cfg_first {
    fn test_only() {
        let _ = std::env::var("TEST_ONLY_CFG_FIRST");
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods)]
mod cfg_then_expect {
    fn test_only() {
        let _ = std::env::vars();
    }
}

#[allow(clippy::all)]
#[cfg(not(test))]
mod prod_not_test {
    fn bad() {
        let _ = std::env::var("PROD_NOT_TEST");
    }
}

#[allow(clippy::all)]
fn prod_before_test_mod() {}

#[cfg(test)]
mod trailing_test_mod {}
"#;
    let found_allows = scan_disallowed_methods_allows(Path::new("attr_order.rs"), sample);
    let allow_lines: Vec<usize> = found_allows.iter().map(|v| v.line).collect();
    assert_eq!(allow_lines, vec![34, 42], "got allows: {found_allows:?}");

    let found_env = scan_source(Path::new("attr_order.rs"), sample);
    let env_lines: Vec<usize> = found_env.iter().map(|v| v.line).collect();
    assert_eq!(env_lines, vec![38], "got env: {found_env:?}");
}

#[test]
fn workspace_production_clippy_force_warn_finds_zero_unallowlisted_disallowed_methods() {
    let root = workspace_root();
    let audit = run_clippy_force_warn_disallowed_methods(
        &root.join("Cargo.toml"),
        None,
        ENV_BOUNDARY_ALLOWLIST,
    );
    assert!(
        audit.allowlisted_diagnostics > 0,
        "non-vacuous clippy --force-warn check expected allowlisted env boundary diagnostics, got {audit:?}"
    );
    assert!(
        audit.violations.is_empty(),
        "forbidden production clippy::disallowed_methods found outside ENV_BOUNDARY_ALLOWLIST (even via macro-generated suppressions):\n{}",
        audit.violations.join("\n")
    );
}

fn setup_clippy_macro_workspace(sneaky_src: &str) -> TempWorkspace {
    let ws = TempWorkspace::new("clippy-macro-bypass");
    ws.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"boundary\", \"sneaky\"]\n",
    );
    ws.write(
        "clippy.toml",
        "disallowed-methods = [\n  { path = \"std::env::var\" },\n  { path = \"std::env::var_os\" },\n  { path = \"std::env::vars\" },\n  { path = \"std::env::vars_os\" },\n]\n",
    );
    ws.write(
        "boundary/Cargo.toml",
        "[package]\nname = \"boundary\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    ws.write(
        "boundary/src/lib.rs",
        "#[allow(clippy::disallowed_methods)]\npub fn allowed_read(key: &str) -> Option<String> {\n    std::env::var(key).ok()\n}\n",
    );
    ws.write(
        "sneaky/Cargo.toml",
        "[package]\nname = \"sneaky\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    ws.write("sneaky/src/lib.rs", sneaky_src);

    let lock = Command::new(env!("CARGO"))
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(ws.root_manifest())
        .output()
        .expect("generate lockfile for clippy macro workspace");
    assert!(
        lock.status.success(),
        "cargo generate-lockfile --offline failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&lock.stdout),
        String::from_utf8_lossy(&lock.stderr),
    );
    ws
}

#[test]
fn clippy_force_warn_catches_macro_generated_suppression_and_env_read() {
    let sneaky_src = r#"
macro_rules! expect_bypass {
    ($kind:ident, $group:ident, $lint:ident, $m:ident, $f:ident) => {{
        #[$kind($group::$lint)]
        let v = std::$m::$f("MACRO_EXPECT_BYPASS");
        v.ok()
    }};
}

macro_rules! allow_bypass {
    ($m:ident, $f:ident) => {{
        #[allow(clippy::disallowed_methods)]
        let v = std::$m::$f("MACRO_ALLOW_BYPASS");
        v
    }};
}

pub fn via_expect() -> Option<String> {
    expect_bypass!(expect, clippy, disallowed_methods, env, var)
}

pub fn via_allow() -> Option<std::ffi::OsString> {
    allow_bypass!(env, var_os)
}
"#;
    let ws = setup_clippy_macro_workspace(sneaky_src);
    let target_dir = ws.0.join("target");
    // Baseline: standard `-D warnings` lets macro-generated `#[expect/allow]` suppress the read.
    let mut baseline_cmd = Command::new(env!("CARGO"));
    baseline_cmd
        .env("CLIPPY_CONF_DIR", &ws.0)
        .args([
            "clippy",
            "--offline",
            "--locked",
            "--workspace",
            "--lib",
            "--target-dir",
        ])
        .arg(&target_dir)
        .arg("--manifest-path")
        .arg(ws.root_manifest())
        .args(["--", "-D", "warnings"]);
    let baseline = baseline_cmd
        .output()
        .expect("run baseline clippy -D warnings");
    assert!(
        baseline.status.success(),
        "expected baseline -D warnings to be bypassed by macro #[expect]/#[allow], got:\n{}",
        String::from_utf8_lossy(&baseline.stderr)
    );

    let audit = run_clippy_force_warn_disallowed_methods(
        &ws.root_manifest(),
        Some(&target_dir),
        &["boundary/src/lib.rs"],
    );
    assert!(
        audit.allowlisted_diagnostics >= 1,
        "expected allowlisted boundary diagnostic, got {audit:?}"
    );
    assert_eq!(
        audit.violations.len(),
        2,
        "expected macro-generated #[expect] and #[allow] env reads to be caught: {audit:?}"
    );
    assert!(
        audit
            .violations
            .iter()
            .all(|v| v.contains("sneaky/src/lib.rs:")),
        "expected all violations to blame sneaky/src/lib.rs: {audit:?}"
    );
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.contains("std::env::var`")),
        "missing std::env::var violation: {audit:?}"
    );
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.contains("std::env::var_os")),
        "missing std::env::var_os violation: {audit:?}"
    );
}

#[test]
fn clippy_force_warn_ignores_crate_local_clippy_toml_on_cold_and_cached_runs() {
    let sneaky_src = r#"
macro_rules! allow_local_override {
    ($m:ident, $f:ident) => {{
        #[allow(clippy::disallowed_methods)]
        let v = std::$m::$f("LOCAL_CLIPPY_BYPASS");
        v.ok()
    }};
}

pub fn via_local_clippy() -> Option<String> {
    allow_local_override!(env, var)
}
"#;
    let ws = setup_clippy_macro_workspace(sneaky_src);
    ws.write("sneaky/clippy.toml", "too-many-arguments-threshold = 8\n");
    assert_eq!(
        find_unexpected_clippy_configs(&ws.0),
        vec![PathBuf::from("sneaky/clippy.toml")]
    );

    let target_dir = ws.0.join("target");
    let cold = run_clippy_force_warn_disallowed_methods(
        &ws.root_manifest(),
        Some(&target_dir),
        &["boundary/src/lib.rs"],
    );
    assert!(
        cold.allowlisted_diagnostics >= 1,
        "expected allowlisted boundary diagnostic on cold run, got {cold:?}"
    );
    assert_eq!(
        cold.violations.len(),
        1,
        "expected crate-local clippy.toml override to still fail cold force-warn audit: {cold:?}"
    );
    assert!(
        cold.violations[0].contains("sneaky/src/lib.rs:")
            && cold.violations[0].contains("std::env::var"),
        "unexpected cold violation details: {cold:?}"
    );

    let cached = run_clippy_force_warn_disallowed_methods(
        &ws.root_manifest(),
        Some(&target_dir),
        &["boundary/src/lib.rs"],
    );
    assert_eq!(
        cached, cold,
        "expected cached rerun to catch the same violation stably under crate-local clippy.toml"
    );
}

#[test]
fn env_boundary_guard_rejects_nested_and_dot_clippy_configs_only() {
    let ws = TempWorkspace::new("nested-clippy-cfg");
    ws.write(
        "clippy.toml",
        "disallowed-methods = [\n  { path = \"std::env::var\" },\n]\n",
    );
    ws.write("crates/spawn-kdf/src/lib.rs", "");
    ws.write("target/clippy.toml", "too-many-arguments-threshold = 8\n");
    ws.write(".git/.clippy.toml", "too-many-arguments-threshold = 8\n");
    assert!(
        find_unexpected_clippy_configs(&ws.0).is_empty(),
        "expected root clippy.toml plus target/ and .git/ to pass"
    );

    ws.write(
        "crates/spawn-kdf/clippy.toml",
        "too-many-arguments-threshold = 8\n",
    );
    ws.write(
        "crates/spawn-kdf/src/target/clippy.toml",
        "too-many-arguments-threshold = 8\n",
    );
    ws.write(
        "crates/spawn-kdf/src/target/.clippy.toml",
        "too-many-arguments-threshold = 8\n",
    );
    ws.write(
        "crates/spawn-kdf/src/.git/clippy.toml",
        "too-many-arguments-threshold = 8\n",
    );
    ws.write(
        "crates/spawn-core/.clippy.toml",
        "too-many-arguments-threshold = 8\n",
    );
    ws.write(".clippy.toml", "too-many-arguments-threshold = 8\n");
    assert_eq!(
        find_unexpected_clippy_configs(&ws.0),
        vec![
            PathBuf::from(".clippy.toml"),
            PathBuf::from("crates/spawn-core/.clippy.toml"),
            PathBuf::from("crates/spawn-kdf/clippy.toml"),
            PathBuf::from("crates/spawn-kdf/src/.git/clippy.toml"),
            PathBuf::from("crates/spawn-kdf/src/target/.clippy.toml"),
            PathBuf::from("crates/spawn-kdf/src/target/clippy.toml"),
        ]
    );
}
