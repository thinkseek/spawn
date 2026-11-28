//! Wave-1 static env boundary guard (`spawn-core`, `spawn-cas`, `spawn-postgres`).
//!
//! Enforces two invariants across `crates/spawn-core/src`, `crates/spawn-cas/src`,
//! and `crates/spawn-postgres/src`:
//!
//! 1. **Zero production `std::env::var*` access**: Using a Rust lexer, fail-closed
//!    `#[cfg(test)]` module stripper, recursive `use`-tree analyzer, and production
//!    path scanner (with exact line numbers), rejects every production `std::env::var`,
//!    `std::env::var_os`, `std::env::vars`, `std::env::vars_os`, short `env::var*`,
//!    renamed `use std::env as ...`, glob `use std::env::*`, nested or multi-line
//!    `use` trees (`use std::{env::{var, var_os}, fmt};`), `env::$...`, and `$m::var*`
//!    macro path accesses. Only `crates/spawn-postgres/src/testing.rs`, `#![cfg(test)]`,
//!    and `#[cfg(test)] mod <ident> { ... }` (plus `#[cfg(all(..., test, ...))] mod ...`)
//!    are skipped; all other `#[cfg(test)]` targets are audited as production code.
//! 2. **Zero production `spawn-postgres/testing` activation**: Queries Cargo's real
//!    dependency graph (`cargo tree --offline --workspace -e normal,build`) and fails if
//!    `spawn-postgres` ever resolves with the `"testing"` feature on any non-dev edge.
//!
//! Known single-file lexical / pre-expansion static limits:
//! - `crates/spawn-postgres/src/testing.rs` is test-support gated behind the `"testing"`
//!   feature (enforced by invariant 2 above).
//! - Cross-file `pub use std::env as ...;` alias re-exports (note: `use std::env as <alias>;`
//!   declarations inside any core crate are already rejected directly in the declaring file).
//! - Module-level or split-metavariable macro token pasting (`std::$m::var` or `$m::$f`)
//!   and proc-macro synthesis that require full `rustc` macro expansion.
//!
//! Wave-2 closes remaining `env::var` call sites in `spawn-host`/`spawn-cli` and enables
//! workspace-wide `clippy.toml` `disallowed-methods` to cover all post-expansion paths.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const TARGET_CRATE_SRCS: &[&str] = &[
    "crates/spawn-core/src",
    "crates/spawn-cas/src",
    "crates/spawn-postgres/src",
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

fn is_excluded_test_support(repo_root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    rel == Path::new("crates/spawn-postgres/src/testing.rs")
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

fn cfg_test_mod_end(tokens: &[Token], mut i: usize) -> Option<usize> {
    while tokens.get(i).is_some_and(|t| t.kind == TokenKind::Pound)
        && tokens
            .get(i + 1)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
        && let Some(end_bracket) = matching_close(tokens, i + 1)
    {
        i = end_bracket + 1;
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
            && let Some(end_bracket) = matching_close(tokens, i + 1)
            && is_cfg_test_attr(&tokens[i + 2..end_bracket])
            && let Some(mod_end) = cfg_test_mod_end(tokens, end_bracket + 1)
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
            // Flag renamed `std::env` imports directly in these core crates so
            // cross-module alias re-exports (`pub(crate) use std::env as zz;`) cannot slip by.
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
                    audit_use_subtree(item, &cur_path, env_aliases, violated_lines);
                }
            }
            return;
        }
        if tok.is_kw("as") {
            let alias = slice.get(i + 1).and_then(Token::ident_name);
            let err_line = slice.get(i + 1).map_or(leaf_line, |t| t.line);
            check_leaf_use(&cur_path, alias, err_line, env_aliases, violated_lines);
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
        check_leaf_use(&cur_path, None, leaf_line, env_aliases, violated_lines);
    }
}

fn collect_use_slices(tokens: &[Token]) -> Vec<&[Token]> {
    let mut uses = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].is_kw("use") {
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
            uses.push(&tokens[i + 1..slice_end]);
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
        for u in &uses {
            audit_use_subtree(u, &[], &mut env_aliases, violated_lines);
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

fn scan_source(rel_or_abs_path: &Path, src: &str) -> Vec<Violation> {
    let tokens = tokenize(src);
    let prod_tokens = strip_cfg_test_tokens(&tokens);
    let mut violated_lines = BTreeSet::new();
    let env_aliases = audit_use_trees(&prod_tokens, &mut violated_lines);
    audit_path_accesses(&prod_tokens, &env_aliases, &mut violated_lines);

    let orig_lines: Vec<&str> = src.lines().collect();
    violated_lines
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

// ---------------------------------------------------------------------------
// Cargo Dependency Graph Guard (`cargo tree --offline --workspace -e normal,build`)
// ---------------------------------------------------------------------------

fn parse_cargo_tree_testing_violations(stdout: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for raw_line in stdout.lines() {
        let trimmed = raw_line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        if trimmed.split_whitespace().next() != Some("spawn-postgres") {
            continue;
        }
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
    violations
}

fn check_cargo_tree_no_prod_testing_feature(manifest_path: &Path) -> Vec<String> {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--offline",
            "--workspace",
            "-e",
            "normal,build",
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
    parse_cargo_tree_testing_violations(&stdout)
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
    assert_eq!(parse_cargo_tree_testing_violations(synthetic).len(), 2);

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
}
