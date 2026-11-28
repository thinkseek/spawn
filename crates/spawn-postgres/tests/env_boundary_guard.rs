//! Wave-1 static env boundary guard (`spawn-core`, `spawn-cas`, `spawn-postgres`).
//!
//! Enforces two invariants across `crates/spawn-core/src`, `crates/spawn-cas/src`,
//! and `crates/spawn-postgres/src`:
//!
//! 1. **Zero production `std::env::var*` access**: Using a Rust lexer, nested `#[cfg(test)]`
//!    token stripper, recursive `use`-tree analyzer, and production path scanner (with
//!    line-preserving spans), rejects every non-test `std::env::{var, var_os, vars, vars_os}`,
//!    short `env::var*`, module alias (`use std::env as e; e::var*`), glob (`use std::env::*;`),
//!    nested/wrapped `use` tree (`use std::{env::{var, var_os}, fmt};`), and `env::$...`
//!    macro path access.
//! 2. **Zero production `spawn-postgres/testing` activation**: Scans workspace and crate
//!    `Cargo.toml` manifests to guarantee `spawn-postgres`'s `"testing"` feature is enabled
//!    only in `[dev-dependencies]` / `[target.*.dev-dependencies]`, never in production
//!    dependency or feature tables.
//!
//! Static boundary: `crates/spawn-postgres/src/testing.rs` is explicitly scoped as test-support
//! (guarded by the `Cargo.toml` `"testing"` feature check). Without full `rustc` macro
//! expansion, cross-macro token synthesis that splits *both* the module path and item name
//! across separate metavariables (`$m::$f`) or proc-macro generation cannot be proved statically
//! and will be closed workspace-wide in Wave-2 via `clippy.toml` `disallowed-methods`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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

    fn kw(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn is_kw(&self, expected: &str) -> bool {
        self.kw() == Some(expected)
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
        let mut i = after + 1;
        while i < bytes.len() && bytes[i] != b'\'' && bytes[i] != b'\n' {
            i = (i + if bytes[i] == b'\\' { 2 } else { 1 }).min(bytes.len());
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
    if bytes[start..].starts_with(b"r#") {
        let body = start + 2;
        let first = src[body..].chars().next()?;
        if first.is_alphabetic() || first == '_' {
            let mut end = body + first.len_utf8();
            for ch in src[end..].chars() {
                if ch.is_alphanumeric() || ch == '_' {
                    end += ch.len_utf8();
                } else {
                    break;
                }
            }
            return Some((TokenKind::RawIdent(src[body..end].to_string()), end));
        }
    }
    let first = src[start..].chars().next()?;
    if first.is_alphabetic() || first == '_' {
        let mut end = start + first.len_utf8();
        for ch in src[end..].chars() {
            if ch.is_alphanumeric() || ch == '_' {
                end += ch.len_utf8();
            } else {
                break;
            }
        }
        return Some((TokenKind::Ident(src[start..end].to_string()), end));
    }
    None
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
// Layer B: Exact #[cfg(test)] Stripping
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
        if tokens[0].is_kw("not") {
            return false;
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

fn skip_visibility(tokens: &[Token], mut i: usize) -> usize {
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
    i
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CfgTargetKind {
    SemiItem,
    BlockOrSemiItem,
    MacroRules,
    BlockControl,
    NonItem,
}

fn classify_cfg_target(tokens: &[Token], start: usize) -> CfgTargetKind {
    let after_vis = skip_visibility(tokens, start);
    let mut k = after_vis;
    let first_is_const = tokens.get(k).is_some_and(|t| t.is_kw("const"));
    let starts_unsafe_or_async = tokens
        .get(k)
        .is_some_and(|t| t.is_kw("unsafe") || t.is_kw("async"));

    while let Some(tok) = tokens.get(k) {
        if matches!(tok.kw(), Some("default" | "unsafe" | "async" | "const")) {
            k += 1;
        } else if tok.is_kw("extern") {
            k += 1;
            if tokens.get(k).is_some_and(|n| n.kind == TokenKind::Literal) {
                k += 1;
            }
        } else {
            break;
        }
    }

    match tokens.get(k).and_then(Token::kw) {
        Some("fn" | "mod" | "impl" | "trait" | "struct" | "enum" | "union") => {
            CfgTargetKind::BlockOrSemiItem
        }
        Some("use" | "type" | "static") => CfgTargetKind::SemiItem,
        Some("macro_rules") if tokens.get(k + 1).is_some_and(|t| t.kind == TokenKind::Bang) => {
            CfgTargetKind::MacroRules
        }
        _ if first_is_const => CfgTargetKind::SemiItem,
        _ if tokens
            .get(k)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Brace))
            && (after_vis == start || starts_unsafe_or_async) =>
        {
            CfgTargetKind::BlockControl
        }
        _ if after_vis == start
            && matches!(
                tokens.get(start).and_then(Token::kw),
                Some("if" | "match" | "for" | "while" | "loop")
            ) =>
        {
            CfgTargetKind::BlockControl
        }
        _ => CfgTargetKind::NonItem,
    }
}

fn skip_until_semi_or_enclosing_close(tokens: &[Token], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            TokenKind::Semi if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

fn skip_block_or_semi_item(tokens: &[Token], mut i: usize) -> usize {
    let mut stack: Vec<Delim> = Vec::new();
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(d) => stack.push(d),
            TokenKind::Close(d) => {
                if stack.is_empty() {
                    return i;
                }
                let popped = stack.pop();
                if popped == Some(d) && stack.is_empty() && d == Delim::Brace {
                    return i + 1;
                }
            }
            TokenKind::Semi if stack.is_empty() => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

fn skip_macro_rules_item(tokens: &[Token], mut i: usize) -> usize {
    while i < tokens.len() {
        if let TokenKind::Open(d) = tokens[i].kind
            && let Some(close) = matching_close(tokens, i)
        {
            let mut end = close + 1;
            if d != Delim::Brace && tokens.get(end).is_some_and(|t| t.kind == TokenKind::Semi) {
                end += 1;
            }
            return end;
        }
        i += 1;
    }
    i
}

fn skip_single_block_control(tokens: &[Token], mut i: usize) -> usize {
    let mut stack: Vec<Delim> = Vec::new();
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(d) => stack.push(d),
            TokenKind::Close(d) => {
                if stack.is_empty() {
                    return i;
                }
                let popped = stack.pop();
                if popped == Some(d) && stack.is_empty() && d == Delim::Brace {
                    return i + 1;
                }
            }
            TokenKind::Semi | TokenKind::Comma if stack.is_empty() => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

fn skip_block_control(tokens: &[Token], start: usize) -> usize {
    let is_if = tokens.get(start).is_some_and(|t| t.is_kw("if"));
    let mut end = skip_single_block_control(tokens, start);
    if is_if {
        while tokens.get(end).is_some_and(|t| t.is_kw("else")) {
            end = skip_single_block_control(tokens, end + 1);
        }
    }
    end
}

fn skip_non_item(tokens: &[Token], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            TokenKind::Comma | TokenKind::Semi if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

fn skip_cfg_test_target(tokens: &[Token], mut i: usize) -> usize {
    while tokens.get(i).is_some_and(|t| t.kind == TokenKind::Pound)
        && tokens
            .get(i + 1)
            .is_some_and(|t| t.kind == TokenKind::Open(Delim::Bracket))
        && let Some(end_bracket) = matching_close(tokens, i + 1)
    {
        i = end_bracket + 1;
    }
    match classify_cfg_target(tokens, i) {
        CfgTargetKind::SemiItem => skip_until_semi_or_enclosing_close(tokens, i),
        CfgTargetKind::BlockOrSemiItem => skip_block_or_semi_item(tokens, i),
        CfgTargetKind::MacroRules => skip_macro_rules_item(tokens, i),
        CfgTargetKind::BlockControl => skip_block_control(tokens, i),
        CfgTargetKind::NonItem => skip_non_item(tokens, i),
    }
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
        {
            i = skip_cfg_test_target(tokens, end_bracket + 1);
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
            let end = skip_until_semi_or_enclosing_close(tokens, i + 1);
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
// Cargo.toml `spawn-postgres` `"testing"` feature guard
// ---------------------------------------------------------------------------

fn strip_toml_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut in_basic = false;
    let mut in_literal = false;
    while i < bytes.len() {
        if in_basic {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                in_basic = false;
            }
        } else if in_literal {
            if bytes[i] == b'\'' {
                in_literal = false;
            }
        } else {
            match bytes[i] {
                b'#' => return &line[..i],
                b'"' => in_basic = true,
                b'\'' => in_literal = true,
                _ => {}
            }
        }
        i += 1;
    }
    line
}

fn extract_toml_statements(src: &str) -> Vec<(String, usize, String)> {
    let mut stmts = Vec::new();
    let mut section = String::new();
    let mut cur = String::new();
    let mut start_line = 1usize;
    let mut depth = 0usize;

    for (idx, raw_line) in src.lines().enumerate() {
        let line_no = idx + 1;
        let clean = strip_toml_line_comment(raw_line).trim();
        if clean.is_empty() {
            continue;
        }
        if depth == 0 && clean.starts_with('[') && clean.ends_with(']') && !clean.contains('=') {
            section = clean[1..clean.len() - 1].trim().to_string();
            continue;
        }
        if cur.is_empty() {
            start_line = line_no;
        } else {
            cur.push(' ');
        }
        cur.push_str(clean);
        depth = update_toml_depth(&cur);
        if depth == 0 {
            stmts.push((section.clone(), start_line, std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        stmts.push((section, start_line, cur));
    }
    stmts
}

fn update_toml_depth(stmt: &str) -> usize {
    let bytes = stmt.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    let mut in_basic = false;
    let mut in_literal = false;
    while i < bytes.len() {
        if in_basic {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                in_basic = false;
            }
        } else if in_literal {
            if bytes[i] == b'\'' {
                in_literal = false;
            }
        } else {
            match bytes[i] {
                b'"' => in_basic = true,
                b'\'' => in_literal = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        i += 1;
    }
    depth
}

fn unquote_toml_string(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        Some(&trimmed[1..trimmed.len() - 1])
    } else {
        None
    }
}

fn split_toml_quoted_items(array_text: &str) -> Vec<String> {
    let trimmed = array_text.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    let mut in_basic = false;
    let mut in_literal = false;
    while i < bytes.len() {
        if in_basic {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                in_basic = false;
            }
        } else if in_literal {
            if bytes[i] == b'\'' {
                in_literal = false;
            }
        } else {
            match bytes[i] {
                b'"' => in_basic = true,
                b'\'' => in_literal = true,
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    if let Some(val) = unquote_toml_string(&inner[start..i]) {
                        items.push(val.to_string());
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if let Some(val) = unquote_toml_string(&inner[start..]) {
        items.push(val.to_string());
    }
    items
}

fn section_is_non_dev_dep_table(section: &str) -> bool {
    let s = section.trim();
    if matches!(
        s,
        "dependencies" | "workspace.dependencies" | "build-dependencies"
    ) {
        return true;
    }
    s.starts_with("target.") && (s.ends_with(".dependencies") || s.ends_with(".build-dependencies"))
}

fn section_is_non_dev_spawn_pg_subtable(section: &str) -> bool {
    section
        .trim()
        .strip_suffix(".spawn-postgres")
        .is_some_and(section_is_non_dev_dep_table)
}

fn inline_features_list(inline_table: &str) -> Option<&str> {
    let idx = inline_table.find("features")?;
    let after = inline_table[idx + "features".len()..].trim_start();
    let eq = after.strip_prefix('=')?.trim_start();
    if !eq.starts_with('[') {
        return None;
    }
    let bytes = eq.as_bytes();
    let mut depth = 0usize;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut i = 0usize;
    while i < bytes.len() {
        if in_basic {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                in_basic = false;
            }
        } else if in_literal {
            if bytes[i] == b'\'' {
                in_literal = false;
            }
        } else {
            match bytes[i] {
                b'"' => in_basic = true,
                b'\'' => in_literal = true,
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(&eq[..=i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn scan_cargo_toml_for_spawn_postgres_testing(path: &Path, src: &str) -> Vec<Violation> {
    let mut violations = Vec::new();
    for (section, line, stmt) in extract_toml_statements(src) {
        let Some((lhs_raw, rhs_raw)) = stmt.split_once('=') else {
            continue;
        };
        let lhs = lhs_raw.trim();
        let rhs = rhs_raw.trim();

        let bad_dep = if section_is_non_dev_dep_table(&section) && lhs == "spawn-postgres" {
            inline_features_list(rhs)
                .is_some_and(|arr| split_toml_quoted_items(arr).iter().any(|f| f == "testing"))
        } else if section_is_non_dev_spawn_pg_subtable(&section) && lhs == "features" {
            split_toml_quoted_items(rhs).iter().any(|f| f == "testing")
        } else if section == "features" && lhs != "testing" {
            split_toml_quoted_items(rhs)
                .iter()
                .any(|f| f == "spawn-postgres/testing" || f == "spawn-postgres?/testing")
        } else {
            false
        };

        if bad_dep {
            violations.push(Violation {
                path: path.to_path_buf(),
                line,
                snippet: stmt,
            });
        }
    }
    violations
}

fn collect_workspace_cargo_tomls(root: &Path) -> Vec<PathBuf> {
    let mut manifests = vec![root.join("Cargo.toml")];
    let crates_dir = root.join("crates");
    let entries =
        fs::read_dir(&crates_dir).unwrap_or_else(|e| panic!("read {}: {e}", crates_dir.display()));
    let mut dirs: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    dirs.sort();
    for dir in dirs {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.is_file() {
            manifests.push(cargo_toml);
        }
    }
    manifests
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
    let root = workspace_root();
    let manifests = collect_workspace_cargo_tomls(&root);
    assert!(!manifests.is_empty(), "expected workspace Cargo.toml files");

    let mut violations = Vec::new();
    for manifest in manifests {
        let rel = manifest.strip_prefix(&root).unwrap_or(&manifest);
        let text = fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        violations.extend(scan_cargo_toml_for_spawn_postgres_testing(rel, &text));
    }

    assert!(
        violations.is_empty(),
        "forbidden production activation of spawn-postgres/testing:\n{}",
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

#[test]
fn env_boundary_guard_catches_all_cfg_field_and_import_bypass_mutations() {
    let store_like = r##"
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

pub struct NoTrailingComma {
    #[cfg(test)]
    hook: Option<usize>
}

pub fn fn_after_cfg_field() -> Option<String> {
    std::env::var("AFTER_CFG_FIELD").ok()
}

pub fn with_stmt_cfg_and_raw_chars<'a>(ch: char) -> Option<String> {
    let _c = (',', '}', '\'', '❯', r#"std::env::var("RAW_IGNORED")"#);
    let _keep: &'a str = self::after_lifetime_still_scanned();
    #[cfg(test)]
    if !ch.is_whitespace() {
        let _ = std::env::var("TEST_ONLY_IF_BLOCK");
    }
    std::env::var("AFTER_TEST_IF").ok()
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
"##;
    let found_cfg = scan_source(Path::new("store_like.rs"), store_like);
    let cfg_lines: Vec<usize> = found_cfg.iter().map(|v| v.line).collect();
    assert_eq!(cfg_lines, vec![10, 21, 31, 36, 41], "got {found_cfg:?}");

    let import_and_macro_bypasses = r#"
use std::env as e;
use e as e2;
use std::env::*;
use std::{env::{var as a, var_os as b}, fmt};
use std::env::{
    Args,
    current_dir,
    var as v,
};

macro_rules! grab {
    ($f:ident) => {
        std::env::$f("MACRO_BYPASS")
    };
}

pub fn use_bypasses() {
    let _ = e::var("ALIAS_1");
    let _ = e2::var_os("ALIAS_2");
    let _ = var("GLOB_CALL");
}
"#;
    let found_imp = scan_source(Path::new("bypasses.rs"), import_and_macro_bypasses);
    let imp_lines: Vec<usize> = found_imp.iter().map(|v| v.line).collect();
    assert_eq!(imp_lines, vec![4, 5, 9, 14, 19, 20], "got {found_imp:?}");
}

#[test]
fn cargo_toml_guard_catches_production_testing_feature_and_allows_dev_dependencies() {
    let manifest = r#"
[workspace.dependencies]
spawn-postgres = { path = "crates/spawn-postgres", features = ["testing"] }

[dependencies]
# spawn-postgres = { workspace = true, features = ["testing"] }
spawn-postgres = { workspace = true, features = ["testing"] }

[dependencies.spawn-postgres]
workspace = true
features = [
    "testing",
]

[target.'cfg(unix)'.dependencies]
spawn-postgres = { workspace = true, features = ["testing"] }

[features]
default = ["spawn-postgres/testing"]
testing = ["spawn-postgres?/testing"]

[dev-dependencies]
spawn-postgres = { workspace = true, features = ["testing"] }

[target.'cfg(unix)'.dev-dependencies]
spawn-postgres = { workspace = true, features = ["testing"] }
"#;
    let found = scan_cargo_toml_for_spawn_postgres_testing(Path::new("Cargo.toml"), manifest);
    let lines: Vec<usize> = found.iter().map(|v| v.line).collect();
    assert_eq!(lines, vec![3, 7, 11, 16, 19], "got {found:?}");
}
