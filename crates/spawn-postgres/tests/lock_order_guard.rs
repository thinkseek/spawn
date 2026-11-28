//! Static CI guard for `sql/schema.sql`: enforces the 4-tier advisory lock
//! hierarchy (`L1 -> L0 -> L2 -> L3`), zero occurrences of the retired global
//! lock `1399957335`, mandatory unconditional `perform spawn_lock_migrate_shared();`
//! before DML or inner locks in non-trigger functions, forbidden dynamic DML /
//! advisory lock strings in `EXECUTE`, and unconditional `blocking L3 + commit_xid`
//! protection on every `spawn_events` write across **all** declared functions.
//!
//! Scope boundary: monotonic lock-order verification (`L1 -> L0 -> L2 -> L3`)
//! inspects sequential lock acquisitions intra-procedurally within each
//! individual function or `DO` block (it does not trace cross-function call
//! graphs). Trigger functions (`RETURNS TRIGGER`) are exempt from self-acquiring
//! `L0` (`spawn_lock_migrate_shared()`) because they fire inside an outer DML
//! statement that already holds `L0`, **but trigger functions are still audited
//! for monotonic lock ordering, zero retired `1399957335`, dynamic `EXECUTE`,
//! and `spawn_events` write rules**.

use std::fmt;

const SCHEMA_SQL: &str = include_str!("../sql/schema.sql");

const RETIRED_GLOBAL_LOCK: &str = "1399957335";
const LOCK_L1_LANDING_HOLD: &str = "1399957337";
const LOCK_L0_MIGRATE: &str = "1399957400";
const LOCK_L2_CONTROL: &str = "1399957402";
const LOCK_L3_SESSION: &str = "1399957401";
const LOCK_COST_FOLD: &str = "1399957341";
const LOCK_D6_REFS: &str = "1399957360";

/// Explicitly audited write functions exempt from calling
/// `spawn_lock_migrate_shared()`. Must remain empty unless an exemption has a
/// verified proof; any name listed here that no longer exists or already calls
/// `spawn_lock_migrate_shared()` fails the audit.
const EXEMPT_DML_FUNCTIONS: &[&str] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdvisoryLockTier {
    L1LandingHold,
    L0SchemaMigrate,
    L2Control,
    L2CostFold,
    L2D6Ref,
    L3Session,
}

impl AdvisoryLockTier {
    const fn rank(self) -> u8 {
        match self {
            Self::L1LandingHold => 1,
            Self::L0SchemaMigrate => 2,
            Self::L2Control | Self::L2CostFold | Self::L2D6Ref => 3,
            Self::L3Session => 4,
        }
    }

    const fn is_inner_under_l0(self) -> bool {
        matches!(
            self,
            Self::L2Control | Self::L2CostFold | Self::L2D6Ref | Self::L3Session
        )
    }

    fn from_lock_key(key: &str) -> Option<Self> {
        match key {
            LOCK_L1_LANDING_HOLD => Some(Self::L1LandingHold),
            LOCK_L0_MIGRATE => Some(Self::L0SchemaMigrate),
            LOCK_L2_CONTROL => Some(Self::L2Control),
            LOCK_COST_FOLD => Some(Self::L2CostFold),
            LOCK_D6_REFS => Some(Self::L2D6Ref),
            LOCK_L3_SESSION => Some(Self::L3Session),
            _ => None,
        }
    }

    fn compatible_next(self, next: Self) -> bool {
        if next.rank() < self.rank() {
            return false;
        }
        match self {
            Self::L1LandingHold | Self::L0SchemaMigrate => true,
            Self::L2Control => matches!(next, Self::L2Control | Self::L3Session),
            Self::L2CostFold => next == Self::L2CostFold,
            Self::L2D6Ref => next == Self::L2D6Ref,
            Self::L3Session => next == Self::L3Session,
        }
    }
}

impl fmt::Display for AdvisoryLockTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::L1LandingHold => write!(f, "L1({LOCK_L1_LANDING_HOLD})"),
            Self::L0SchemaMigrate => write!(f, "L0({LOCK_L0_MIGRATE})"),
            Self::L2Control => write!(f, "L2({LOCK_L2_CONTROL})"),
            Self::L2CostFold => write!(f, "CostLeaf({LOCK_COST_FOLD})"),
            Self::L2D6Ref => write!(f, "D6RefLeaf({LOCK_D6_REFS})"),
            Self::L3Session => write!(f, "L3({LOCK_L3_SESSION})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmlKind {
    Insert,
    Update,
    Delete,
    Truncate,
    Merge,
}

impl fmt::Display for DmlKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Insert => f.write_str("INSERT"),
            Self::Update => f.write_str("UPDATE"),
            Self::Delete => f.write_str("DELETE"),
            Self::Truncate => f.write_str("TRUNCATE"),
            Self::Merge => f.write_str("MERGE"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Violation {
    RetiredGlobalLock {
        line: usize,
        snippet: String,
    },
    MissingExclusiveBootstrapLock,
    MissingMigrateSharedLock {
        function: String,
        line: usize,
        reason: String,
    },
    ConditionalMigrateSharedLock {
        function: String,
        line: usize,
    },
    MigrateSharedLockAfterDml {
        function: String,
        l0_line: usize,
        dml_line: usize,
        dml: String,
    },
    MigrateSharedLockAfterInnerLock {
        function: String,
        l0_line: usize,
        lock: AdvisoryLockTier,
        lock_line: usize,
    },
    InvalidLockOrder {
        scope: String,
        prev: AdvisoryLockTier,
        prev_line: usize,
        next: AdvisoryLockTier,
        next_line: usize,
    },
    UnknownAdvisoryLock {
        scope: String,
        line: usize,
        call: String,
    },
    UnbalancedControlFlow {
        scope: String,
        line: usize,
        detail: String,
    },
    ForbiddenDynamicSql {
        scope: String,
        line: usize,
        detail: String,
    },
    SpawnEventsInsertWithoutL3Lock {
        function: String,
        insert_line: usize,
    },
    SpawnEventsInsertWithoutCommitXid {
        function: String,
        insert_line: usize,
        detail: String,
    },
    UnusedExemption {
        function: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetiredGlobalLock { line, snippet } => {
                write!(
                    f,
                    "line {line}: retired lock {RETIRED_GLOBAL_LOCK}: {snippet}"
                )
            }
            Self::MissingExclusiveBootstrapLock => {
                write!(
                    f,
                    "missing L0 exclusive lock ({LOCK_L0_MIGRATE}) before first CREATE TABLE"
                )
            }
            Self::MissingMigrateSharedLock {
                function,
                line,
                reason,
            } => write!(
                f,
                "{function} (L{line}): missing spawn_lock_migrate_shared() ({reason})"
            ),
            Self::ConditionalMigrateSharedLock { function, line } => {
                write!(
                    f,
                    "{function} (L{line}): spawn_lock_migrate_shared() called only inside conditional scope"
                )
            }
            Self::MigrateSharedLockAfterDml {
                function,
                l0_line,
                dml_line,
                dml,
            } => write!(
                f,
                "{function}: spawn_lock_migrate_shared() (L{l0_line}) appears after {dml} (L{dml_line})"
            ),
            Self::MigrateSharedLockAfterInnerLock {
                function,
                l0_line,
                lock,
                lock_line,
            } => write!(
                f,
                "{function}: spawn_lock_migrate_shared() (L{l0_line}) appears after {lock} (L{lock_line})"
            ),
            Self::InvalidLockOrder {
                scope,
                prev,
                prev_line,
                next,
                next_line,
            } => write!(
                f,
                "{scope}: invalid lock order {prev} (L{prev_line}) -> {next} (L{next_line})"
            ),
            Self::UnknownAdvisoryLock { scope, line, call } => {
                write!(
                    f,
                    "{scope} (L{line}): unrecognized advisory lock call `{call}`"
                )
            }
            Self::UnbalancedControlFlow {
                scope,
                line,
                detail,
            } => write!(f, "{scope} (L{line}): unbalanced control flow ({detail})"),
            Self::ForbiddenDynamicSql {
                scope,
                line,
                detail,
            } => write!(f, "{scope} (L{line}): forbidden dynamic EXECUTE ({detail})"),
            Self::SpawnEventsInsertWithoutL3Lock {
                function,
                insert_line,
            } => write!(
                f,
                "{function} (L{insert_line}): write to spawn_events lacks unconditional blocking L3 ({LOCK_L3_SESSION})"
            ),
            Self::SpawnEventsInsertWithoutCommitXid {
                function,
                insert_line,
                detail,
            } => write!(
                f,
                "{function} (L{insert_line}): INSERT INTO spawn_events bad commit_xid: {detail}"
            ),
            Self::UnusedExemption { function } => {
                write!(
                    f,
                    "exempt function `{function}` is missing or no longer exempt"
                )
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AuditSummary {
    total_functions: usize,
    trigger_functions: usize,
    dml_functions: usize,
    do_blocks: usize,
    spawn_events_writers: Vec<String>,
    retired_lock_hits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Word(String),
    Number(String),
    SingleQuoted(String),
    DollarQuoted {
        body: String,
        body_start_line: usize,
    },
    LParen,
    RParen,
    Comma,
    Semicolon,
    Dot,
    Other(char),
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    line: usize,
    offset: usize,
}

impl Token {
    fn is_word(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Word(word) if word == expected)
    }

    fn as_word(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Word(word) => Some(word.as_str()),
            _ => None,
        }
    }
}

fn lex_sql(input: &str, start_line: usize) -> Vec<Token> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = start_line;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            if b == b'\n' {
                line += 1;
            }
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            (i, line) = skip_block_comment(bytes, i + 2, line);
        } else if (matches!(b, b'e' | b'E') && bytes.get(i + 1) == Some(&b'\'')) || b == b'\'' {
            let offset = i;
            let start = line;
            let escape_backslash = b != b'\'';
            let quote_start = if escape_backslash { i + 2 } else { i + 1 };
            let text;
            (text, i, line) = skip_single_quoted(bytes, quote_start, line, escape_backslash);
            tokens.push(Token {
                kind: TokenKind::SingleQuoted(text),
                line: start,
                offset,
            });
        } else if b == b'"' {
            let offset = i;
            let start = line;
            let ident;
            (ident, i, line) = read_quoted_ident(input, i + 1, line);
            tokens.push(Token {
                kind: TokenKind::Word(ident),
                line: start,
                offset,
            });
        } else if b == b'$'
            && let Some((tag, content_start)) = parse_dollar_tag(input, i)
            && let Some(end_rel) = input[content_start..].find(tag)
        {
            let body = input[content_start..content_start + end_rel].to_string();
            let body_lines = body.bytes().filter(|&ch| ch == b'\n').count();
            tokens.push(Token {
                kind: TokenKind::DollarQuoted {
                    body,
                    body_start_line: line,
                },
                line,
                offset: i,
            });
            line += body_lines;
            i = content_start + end_rel + tag.len();
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let offset = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Word(input[offset..i].to_ascii_lowercase()),
                line,
                offset,
            });
        } else if b.is_ascii_digit() {
            let offset = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
            let num: String = input[offset..i].chars().filter(|&ch| ch != '_').collect();
            tokens.push(Token {
                kind: TokenKind::Number(num),
                line,
                offset,
            });
        } else {
            tokens.push(Token {
                kind: match b {
                    b'(' => TokenKind::LParen,
                    b')' => TokenKind::RParen,
                    b',' => TokenKind::Comma,
                    b';' => TokenKind::Semicolon,
                    b'.' => TokenKind::Dot,
                    _ => TokenKind::Other(b as char),
                },
                line,
                offset: i,
            });
            i += 1;
        }
    }

    tokens
}

fn skip_block_comment(bytes: &[u8], mut i: usize, mut line: usize) -> (usize, usize) {
    let mut depth = 1_usize;
    while i < bytes.len() && depth > 0 {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
        } else {
            if bytes[i] == b'\n' {
                line += 1;
            }
            i += 1;
        }
    }
    (i, line)
}

fn skip_single_quoted(
    bytes: &[u8],
    mut i: usize,
    mut line: usize,
    escape_backslash: bool,
) -> (String, usize, usize) {
    let mut text = Vec::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            text.push(b);
            line += 1;
            i += 1;
        } else if escape_backslash && b == b'\\' {
            if let Some(&next) = bytes.get(i + 1) {
                if next == b'\n' {
                    line += 1;
                }
                text.push(next);
            }
            i = (i + 2).min(bytes.len());
        } else if b == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                text.push(b'\'');
                i += 2;
            } else {
                i += 1;
                break;
            }
        } else {
            text.push(b);
            i += 1;
        }
    }
    (String::from_utf8_lossy(&text).into_owned(), i, line)
}

fn read_quoted_ident(input: &str, mut i: usize, mut line: usize) -> (String, usize, usize) {
    let bytes = input.as_bytes();
    let mut ident = String::new();
    while i < bytes.len() {
        let ch = input[i..].chars().next().map_or('\0', |c| c);
        i += ch.len_utf8();
        if ch == '\n' {
            line += 1;
        }
        if ch == '"' {
            if bytes.get(i) == Some(&b'"') {
                ident.push('"');
                i += 1;
            } else {
                break;
            }
        } else {
            ident.push(ch.to_ascii_lowercase());
        }
    }
    (ident, i, line)
}

fn parse_dollar_tag(input: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let next = *bytes.get(start + 1)?;
    if next == b'$' {
        return Some((&input[start..start + 2], start + 2));
    }
    // Positional parameters like $1 are not dollar-quoted strings.
    if !(next.is_ascii_alphabetic() || next == b'_') {
        return None;
    }
    let mut end = start + 2;
    while end < bytes.len() {
        let b = bytes[end];
        if b == b'$' {
            return Some((&input[start..=end], end + 1));
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        end += 1;
    }
    None
}

#[derive(Debug)]
struct ParsedFunction {
    name: String,
    line: usize,
    is_trigger: bool,
    body_tokens: Vec<Token>,
}

#[derive(Debug)]
struct ParsedDoBlock {
    line: usize,
    offset: usize,
    body_tokens: Vec<Token>,
}

#[derive(Debug, Default)]
struct ExtractedSchema {
    functions: Vec<ParsedFunction>,
    do_blocks: Vec<ParsedDoBlock>,
    first_create_table_offset: Option<usize>,
    spawn_events_has_xid_default: bool,
    saw_alter_default: bool,
}

fn split_statements(tokens: &[Token]) -> Vec<&[Token]> {
    tokens
        .split(|tok| matches!(tok.kind, TokenKind::Semicolon))
        .filter(|stmt| !stmt.is_empty())
        .collect()
}

fn extract_schema(sql: &str) -> ExtractedSchema {
    let tokens = lex_sql(sql, 1);
    let mut extracted = ExtractedSchema::default();
    process_statement_stream(&tokens, true, &mut extracted);
    extracted
}

fn process_statement_stream(tokens: &[Token], top_level: bool, extracted: &mut ExtractedSchema) {
    for stmt in split_statements(tokens) {
        if let Some(func) = try_parse_function(stmt) {
            extracted.functions.push(func);
            continue;
        }
        if top_level && let Some(do_block) = try_parse_do_block(stmt) {
            inspect_table_defaults_in_tokens(&do_block.body_tokens, extracted);
            collect_nested_dollar_functions(&do_block.body_tokens, extracted);
            extracted.do_blocks.push(do_block);
            continue;
        }
        inspect_table_default_stmt(stmt, extracted);
        if top_level
            && extracted.first_create_table_offset.is_none()
            && stmt.len() >= 2
            && stmt[0].is_word("create")
            && (stmt[1].is_word("table")
                || (stmt.get(2).is_some_and(|t| t.is_word("table")) && stmt[1].is_word("unlogged")))
        {
            extracted.first_create_table_offset = Some(stmt[0].offset);
        }
    }
}

fn collect_nested_dollar_functions(tokens: &[Token], extracted: &mut ExtractedSchema) {
    for tok in tokens {
        if let TokenKind::DollarQuoted {
            body,
            body_start_line,
        } = &tok.kind
        {
            let nested = lex_sql(body, *body_start_line);
            process_statement_stream(&nested, false, extracted);
        }
    }
}

fn inspect_table_defaults_in_tokens(tokens: &[Token], extracted: &mut ExtractedSchema) {
    for stmt in split_statements(tokens) {
        inspect_table_default_stmt(stmt, extracted);
    }
}

fn skip_schema_prefix(stmt: &[Token], mut idx: usize) -> usize {
    if stmt
        .get(idx + 1)
        .is_some_and(|t| matches!(t.kind, TokenKind::Dot))
    {
        idx += 2;
    }
    idx
}

fn inspect_table_default_stmt(stmt: &[Token], extracted: &mut ExtractedSchema) {
    if stmt.len() < 4 || !stmt[0].is_word("create") && !stmt[0].is_word("alter") {
        return;
    }
    if stmt[0].is_word("create") && stmt[1].is_word("table") {
        let mut idx = 2;
        if stmt
            .get(idx..idx + 3)
            .is_some_and(|s| s[0].is_word("if") && s[1].is_word("not") && s[2].is_word("exists"))
        {
            idx += 3;
        }
        idx = skip_schema_prefix(stmt, idx);
        if stmt.get(idx).is_some_and(|t| t.is_word("spawn_events"))
            && let Some(lparen) = stmt[idx..]
                .iter()
                .position(|t| matches!(t.kind, TokenKind::LParen))
        {
            let body = &stmt[idx + lparen..];
            if has_commit_xid_current_xact_default(body) {
                extracted.spawn_events_has_xid_default = true;
            }
        }
        return;
    }
    if stmt[0].is_word("alter") && stmt[1].is_word("table") {
        let mut idx = 2;
        if stmt
            .get(idx..idx + 2)
            .is_some_and(|s| s[0].is_word("if") && s[1].is_word("exists"))
        {
            idx += 2;
        }
        idx = skip_schema_prefix(stmt, idx);
        if !stmt.get(idx).is_some_and(|t| t.is_word("spawn_events")) {
            return;
        }
        let rest = &stmt[idx + 1..];
        for i in 0..rest.len() {
            if !rest[i].is_word("commit_xid") {
                continue;
            }
            if let Some(pos) = find_set_default_window(rest, i) {
                extracted.saw_alter_default = true;
                extracted.spawn_events_has_xid_default = is_pg_current_xact_id_call(&rest[pos..]);
            }
        }
    }
}

fn find_set_default_window(tokens: &[Token], start: usize) -> Option<usize> {
    // Handles both `ALTER [COLUMN] commit_xid SET DEFAULT ...` and
    // `ADD COLUMN [IF NOT EXISTS] commit_xid ... DEFAULT ...`.
    let is_alter =
        start > 0 && (tokens[start - 1].is_word("alter") || tokens[start - 1].is_word("column"));
    if !is_alter {
        return None;
    }
    if tokens.get(start + 1).is_some_and(|t| t.is_word("set"))
        && tokens.get(start + 2).is_some_and(|t| t.is_word("default"))
    {
        return Some(start + 3);
    }
    None
}

fn has_commit_xid_current_xact_default(body: &[Token]) -> bool {
    let mut depth = 0_usize;
    for i in 0..body.len() {
        match body[i].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ if depth == 1 && body[i].is_word("commit_xid") => {
                let mut j = i + 1;
                let mut inner_depth = depth;
                while j < body.len() {
                    match body[j].kind {
                        TokenKind::LParen => inner_depth += 1,
                        TokenKind::RParen => {
                            if inner_depth == 1 {
                                break;
                            }
                            inner_depth -= 1;
                        }
                        TokenKind::Comma if inner_depth == 1 => break,
                        _ if body[j].is_word("default") => {
                            return is_pg_current_xact_id_call(&body[j + 1..]);
                        }
                        _ => {}
                    }
                    j += 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_pg_current_xact_id_call(tokens: &[Token]) -> bool {
    tokens.len() >= 3
        && tokens[0].is_word("pg_current_xact_id")
        && matches!(tokens[1].kind, TokenKind::LParen)
        && matches!(tokens[2].kind, TokenKind::RParen)
}

fn try_parse_function(stmt: &[Token]) -> Option<ParsedFunction> {
    let mut idx = 0;
    if !stmt.get(idx)?.is_word("create") {
        return None;
    }
    idx += 1;
    if stmt.get(idx).is_some_and(|t| t.is_word("or"))
        && stmt.get(idx + 1).is_some_and(|t| t.is_word("replace"))
    {
        idx += 2;
    }
    if !stmt.get(idx)?.is_word("function") {
        return None;
    }
    idx += 1;
    idx = skip_schema_prefix(stmt, idx);
    let name = stmt.get(idx)?.as_word()?.to_string();
    let line = stmt[0].line;

    let lparen = idx
        + stmt[idx..]
            .iter()
            .position(|t| matches!(t.kind, TokenKind::LParen))?;
    let rparen = find_matching_paren(stmt, lparen)?;
    let as_pos = rparen
        + 1
        + stmt[rparen + 1..].windows(2).position(|pair| {
            pair[0].is_word("as") && matches!(pair[1].kind, TokenKind::DollarQuoted { .. })
        })?;
    let header_tail = &stmt[rparen + 1..as_pos];
    let is_trigger = header_tail
        .windows(2)
        .any(|pair| pair[0].is_word("returns") && pair[1].is_word("trigger"));

    let (body, body_start_line) = match &stmt.get(as_pos + 1)?.kind {
        TokenKind::DollarQuoted {
            body,
            body_start_line,
        } => (body.as_str(), *body_start_line),
        _ => return None,
    };

    Some(ParsedFunction {
        name,
        line,
        is_trigger,
        body_tokens: lex_sql(body, body_start_line),
    })
}

fn try_parse_do_block(stmt: &[Token]) -> Option<ParsedDoBlock> {
    if !stmt.first()?.is_word("do") {
        return None;
    }
    let mut idx = 1;
    if stmt.get(idx).is_some_and(|t| t.is_word("language")) {
        idx += 2;
    }
    let tok = stmt.get(idx)?;
    let (body, body_start_line) = match &tok.kind {
        TokenKind::DollarQuoted {
            body,
            body_start_line,
        } => (body.as_str(), *body_start_line),
        _ => return None,
    };
    Some(ParsedDoBlock {
        line: stmt[0].line,
        offset: stmt[0].offset,
        body_tokens: lex_sql(body, body_start_line),
    })
}

fn find_matching_paren(tokens: &[Token], open_idx: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, tok) in tokens[open_idx..].iter().enumerate() {
        match tok.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_idx + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct LockAcquisition {
    token_idx: usize,
    line: usize,
    tier: AdvisoryLockTier,
    cond_depth: usize,
    blocking: bool,
}

#[derive(Debug, Clone)]
struct DmlSite {
    token_idx: usize,
    line: usize,
    kind: DmlKind,
    table: String,
}

#[derive(Debug, Default)]
struct BodyScan {
    locks: Vec<LockAcquisition>,
    dmls: Vec<DmlSite>,
    unconditional_l3_ready_at: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeFrame {
    Block {
        in_exception: bool,
    },
    If,
    TryLockL3Fallback {
        xact_scoped: bool,
        key_tokens: Vec<TokenKind>,
        fallback_acquired: bool,
        saw_elsif_or_else: bool,
    },
    CaseStmt,
    CaseExpr,
    Loop,
}

fn cond_depth(stack: &[ScopeFrame]) -> usize {
    stack
        .iter()
        .filter(|frame| {
            !matches!(
                frame,
                ScopeFrame::Block {
                    in_exception: false
                }
            )
        })
        .count()
}

fn skip_label(tokens: &[Token], i: usize) -> Option<usize> {
    if matches!(tokens.get(i).map(|t| &t.kind), Some(TokenKind::Other('<')))
        && matches!(
            tokens.get(i + 1).map(|t| &t.kind),
            Some(TokenKind::Other('<'))
        )
        && tokens.get(i + 2).and_then(Token::as_word).is_some()
        && matches!(
            tokens.get(i + 3).map(|t| &t.kind),
            Some(TokenKind::Other('>'))
        )
        && matches!(
            tokens.get(i + 4).map(|t| &t.kind),
            Some(TokenKind::Other('>'))
        )
    {
        return Some(i + 5);
    }
    None
}

fn find_matching_then(tokens: &[Token], start: usize) -> Option<usize> {
    let mut paren_depth = 0_usize;
    let mut case_depth = 0_usize;
    for (offset, tok) in tokens[start..].iter().enumerate() {
        match tok.kind {
            TokenKind::LParen => paren_depth += 1,
            TokenKind::RParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::Semicolon if paren_depth == 0 && case_depth == 0 => return None,
            _ if tok.is_word("case") => case_depth += 1,
            _ if tok.is_word("end") => case_depth = case_depth.saturating_sub(1),
            _ if paren_depth == 0 && case_depth == 0 && tok.is_word("then") => {
                return Some(start + offset);
            }
            _ => {}
        }
    }
    None
}

fn try_match_l3_trylock_condition(tokens: &[Token], if_idx: usize) -> Option<ScopeFrame> {
    let then_idx = find_matching_then(tokens, if_idx + 1)?;
    let cond = &tokens[if_idx + 1..then_idx];
    if !cond.first()?.is_word("not") {
        return None;
    }
    let fn_name = cond.get(1)?.as_word()?;
    let xact_scoped = match fn_name {
        "pg_try_advisory_xact_lock" => true,
        "pg_try_advisory_lock" => false,
        _ => return None,
    };
    if !matches!(cond.get(2).map(|t| &t.kind), Some(TokenKind::LParen)) {
        return None;
    }
    let close = find_matching_paren(cond, 2)?;
    if close + 1 != cond.len() {
        return None;
    }
    let args = split_top_level_commas(&cond[3..close]);
    if args.len() != 2 {
        return None;
    }
    let key_is_l3 = args[0].len() == 1
        && matches!(&args[0][0].kind, TokenKind::Number(n) if n == LOCK_L3_SESSION);
    if !key_is_l3 || args[1].is_empty() {
        return None;
    }
    Some(ScopeFrame::TryLockL3Fallback {
        xact_scoped,
        key_tokens: args[1].iter().map(|t| t.kind.clone()).collect(),
        fallback_acquired: false,
        saw_elsif_or_else: false,
    })
}

fn check_dynamic_execute(scope: &str, tokens: &[Token], i: usize, violations: &mut Vec<Violation>) {
    let exec_line = tokens[i].line;
    let mut saw_literal = false;
    for tok in &tokens[i + 1..] {
        if matches!(tok.kind, TokenKind::Semicolon) {
            break;
        }
        let body = match &tok.kind {
            TokenKind::SingleQuoted(s) => s.as_str(),
            TokenKind::DollarQuoted { body, .. } => body.as_str(),
            _ => continue,
        };
        saw_literal = true;
        for sub in lex_sql(body, tok.line) {
            if let Some(word) = sub.as_word()
                && (matches!(
                    word,
                    "insert" | "update" | "delete" | "truncate" | "merge" | "spawn_events"
                ) || word.starts_with("pg_advisory"))
            {
                violations.push(Violation::ForbiddenDynamicSql {
                    scope: scope.to_string(),
                    line: sub.line,
                    detail: format!("dynamic EXECUTE contains forbidden keyword `{word}`"),
                });
            }
        }
    }
    if !saw_literal {
        violations.push(Violation::ForbiddenDynamicSql {
            scope: scope.to_string(),
            line: exec_line,
            detail: "opaque dynamic EXECUTE without literal SQL".to_string(),
        });
    }
}

fn handle_end_keyword(
    scope: &str,
    tokens: &[Token],
    i: usize,
    stack: &mut Vec<ScopeFrame>,
    scan: &mut BodyScan,
    violations: &mut Vec<Violation>,
) -> usize {
    let line = tokens[i].line;
    let next = tokens.get(i + 1);
    if next.is_some_and(|t| t.is_word("if")) {
        match stack.pop() {
            Some(ScopeFrame::If) => {}
            Some(ScopeFrame::TryLockL3Fallback {
                fallback_acquired,
                saw_elsif_or_else,
                ..
            }) => {
                if fallback_acquired && !saw_elsif_or_else {
                    scan.unconditional_l3_ready_at.push(i + 1);
                }
            }
            other => violations.push(Violation::UnbalancedControlFlow {
                scope: scope.to_string(),
                line,
                detail: format!("END IF without matching IF (top={other:?})"),
            }),
        }
        return i + 2;
    }
    if next.is_some_and(|t| t.is_word("case")) {
        match stack.pop() {
            Some(ScopeFrame::CaseStmt) => {}
            other => violations.push(Violation::UnbalancedControlFlow {
                scope: scope.to_string(),
                line,
                detail: format!("END CASE without matching CASE (top={other:?})"),
            }),
        }
        return i + 2;
    }
    if next.is_some_and(|t| t.is_word("loop")) {
        match stack.pop() {
            Some(ScopeFrame::Loop) => {}
            other => violations.push(Violation::UnbalancedControlFlow {
                scope: scope.to_string(),
                line,
                detail: format!("END LOOP without matching LOOP (top={other:?})"),
            }),
        }
        return i + 2;
    }
    match stack.pop() {
        Some(ScopeFrame::CaseExpr | ScopeFrame::Block { .. }) => {}
        other => violations.push(Violation::UnbalancedControlFlow {
            scope: scope.to_string(),
            line,
            detail: format!("END without matching BEGIN or CASE expression (top={other:?})"),
        }),
    }
    i + 1
}

fn handle_control_word(
    scope: &str,
    tokens: &[Token],
    i: usize,
    at_stmt_start: &mut bool,
    stack: &mut Vec<ScopeFrame>,
    violations: &mut Vec<Violation>,
) {
    let Some(word) = tokens[i].as_word() else {
        *at_stmt_start = false;
        return;
    };
    let prev = i.checked_sub(1).and_then(|idx| tokens.get(idx));
    let stmt_start = *at_stmt_start;
    match word {
        "declare" if stmt_start => {
            *at_stmt_start = true;
        }
        "begin" if stmt_start => {
            stack.push(ScopeFrame::Block {
                in_exception: false,
            });
            *at_stmt_start = true;
        }
        "exception" if stmt_start && !prev.is_some_and(|t| t.is_word("raise")) => {
            if let Some(ScopeFrame::Block { in_exception }) = stack
                .iter_mut()
                .rev()
                .find(|frame| matches!(frame, ScopeFrame::Block { .. }))
            {
                *in_exception = true;
            } else {
                violations.push(Violation::UnbalancedControlFlow {
                    scope: scope.to_string(),
                    line: tokens[i].line,
                    detail: "EXCEPTION without enclosing BEGIN block".to_string(),
                });
            }
            *at_stmt_start = false;
        }
        "if" if stmt_start => {
            let frame = if cond_depth(stack) == 0 {
                try_match_l3_trylock_condition(tokens, i).unwrap_or(ScopeFrame::If)
            } else {
                ScopeFrame::If
            };
            stack.push(frame);
            *at_stmt_start = false;
        }
        "elsif" | "elseif" if stmt_start => {
            if let Some(ScopeFrame::TryLockL3Fallback {
                saw_elsif_or_else, ..
            }) = stack.last_mut()
            {
                *saw_elsif_or_else = true;
            }
            *at_stmt_start = false;
        }
        "case" => {
            stack.push(if stmt_start {
                ScopeFrame::CaseStmt
            } else {
                ScopeFrame::CaseExpr
            });
            *at_stmt_start = false;
        }
        "loop" => {
            stack.push(ScopeFrame::Loop);
            *at_stmt_start = true;
        }
        "then" => {
            *at_stmt_start = matches!(
                stack.last(),
                Some(
                    ScopeFrame::If
                        | ScopeFrame::TryLockL3Fallback { .. }
                        | ScopeFrame::CaseStmt
                        | ScopeFrame::Block { in_exception: true }
                )
            );
        }
        "else" => {
            if let Some(ScopeFrame::TryLockL3Fallback {
                saw_elsif_or_else, ..
            }) = stack.last_mut()
            {
                *saw_elsif_or_else = true;
            }
            *at_stmt_start = matches!(
                stack.last(),
                Some(ScopeFrame::If | ScopeFrame::TryLockL3Fallback { .. } | ScopeFrame::CaseStmt)
            );
        }
        _ => *at_stmt_start = false,
    }
}

fn scan_body_events(scope: &str, tokens: &[Token], violations: &mut Vec<Violation>) -> BodyScan {
    let mut scan = BodyScan::default();
    let mut stack = Vec::new();
    let mut at_stmt_start = true;
    let mut i = 0;

    while i < tokens.len() {
        if let Some(next_i) = skip_label(tokens, i) {
            i = next_i;
            continue;
        }
        if matches!(tokens[i].kind, TokenKind::Semicolon) {
            at_stmt_start = true;
            i += 1;
            continue;
        }
        let after_dot = i
            .checked_sub(1)
            .and_then(|idx| tokens.get(idx))
            .is_some_and(|t| matches!(t.kind, TokenKind::Dot));
        if !after_dot && tokens[i].is_word("end") {
            i = handle_end_keyword(scope, tokens, i, &mut stack, &mut scan, violations);
            at_stmt_start = false;
            continue;
        }
        if !after_dot && at_stmt_start && tokens[i].is_word("execute") {
            check_dynamic_execute(scope, tokens, i, violations);
            at_stmt_start = false;
            i += 1;
            continue;
        }
        if let Some((acq, call_name, second_arg, next_i)) =
            try_parse_lock_call(scope, tokens, i, cond_depth(&stack), violations)
        {
            if cond_depth(&stack) == 1 {
                record_trylock_fallback_hit(&mut stack, acq, call_name, &second_arg);
            }
            scan.locks.push(acq);
            at_stmt_start = false;
            i = next_i;
            continue;
        }
        if let Some(dml) = try_parse_dml(tokens, i) {
            scan.dmls.push(dml);
        }
        if after_dot {
            at_stmt_start = false;
        } else {
            handle_control_word(scope, tokens, i, &mut at_stmt_start, &mut stack, violations);
        }
        i += 1;
    }

    if !stack.is_empty() {
        violations.push(Violation::UnbalancedControlFlow {
            scope: scope.to_string(),
            line: tokens.last().map_or(1, |t| t.line),
            detail: format!("unclosed control-flow scopes at end of body: {stack:?}"),
        });
    }
    scan
}

fn record_trylock_fallback_hit(
    stack: &mut [ScopeFrame],
    acq: LockAcquisition,
    call_name: &str,
    second_arg: &[TokenKind],
) {
    if acq.tier != AdvisoryLockTier::L3Session || !acq.blocking {
        return;
    }
    if let Some(ScopeFrame::TryLockL3Fallback {
        xact_scoped,
        key_tokens,
        fallback_acquired,
        saw_elsif_or_else: false,
    }) = stack.last_mut()
    {
        let expected_call = if *xact_scoped {
            "pg_advisory_xact_lock"
        } else {
            "pg_advisory_lock"
        };
        if call_name == expected_call && second_arg == key_tokens.as_slice() {
            *fallback_acquired = true;
        }
    }
}

fn try_parse_lock_call<'a>(
    scope: &str,
    tokens: &'a [Token],
    i: usize,
    cond_depth: usize,
    violations: &mut Vec<Violation>,
) -> Option<(LockAcquisition, &'a str, Vec<TokenKind>, usize)> {
    let name = tokens[i].as_word()?;
    if !matches!(tokens.get(i + 1).map(|t| &t.kind), Some(TokenKind::LParen)) {
        return None;
    }
    let close = find_matching_paren(tokens, i + 1)?;
    let line = tokens[i].line;

    if name == "spawn_lock_migrate_shared" && close == i + 2 {
        return Some((
            LockAcquisition {
                token_idx: i,
                line,
                tier: AdvisoryLockTier::L0SchemaMigrate,
                cond_depth,
                blocking: true,
            },
            name,
            Vec::new(),
            close + 1,
        ));
    }

    let is_unlock = matches!(name, "pg_advisory_unlock" | "pg_advisory_unlock_shared");
    let is_lock = matches!(
        name,
        "pg_advisory_xact_lock"
            | "pg_advisory_xact_lock_shared"
            | "pg_try_advisory_xact_lock"
            | "pg_try_advisory_xact_lock_shared"
            | "pg_advisory_lock"
            | "pg_advisory_lock_shared"
            | "pg_try_advisory_lock"
            | "pg_try_advisory_lock_shared"
    );
    if !is_lock && !is_unlock {
        return None;
    }

    let args = &tokens[i + 2..close];
    let mut split_args = args.split(|tok| matches!(tok.kind, TokenKind::Comma));
    let first_arg = split_args.next().unwrap_or(&[]);
    let second_arg_kinds: Vec<TokenKind> = split_args
        .next()
        .unwrap_or(&[])
        .iter()
        .map(|tok| tok.kind.clone())
        .collect();
    let numeric_key = first_arg.iter().find_map(|tok| match &tok.kind {
        TokenKind::Number(num) => Some(num.as_str()),
        _ => None,
    });

    match numeric_key {
        Some(RETIRED_GLOBAL_LOCK) => {
            violations.push(Violation::RetiredGlobalLock {
                line,
                snippet: format!("{name}({RETIRED_GLOBAL_LOCK}) in {scope}"),
            });
            None
        }
        Some(key) if is_unlock => {
            if AdvisoryLockTier::from_lock_key(key).is_none() {
                violations.push(Violation::UnknownAdvisoryLock {
                    scope: scope.to_string(),
                    line,
                    call: format!("{name}({key})"),
                });
            }
            None
        }
        Some(key) => {
            if let Some(tier) = AdvisoryLockTier::from_lock_key(key) {
                Some((
                    LockAcquisition {
                        token_idx: i,
                        line,
                        tier,
                        cond_depth,
                        blocking: !name.starts_with("pg_try_"),
                    },
                    name,
                    second_arg_kinds,
                    close + 1,
                ))
            } else {
                violations.push(Violation::UnknownAdvisoryLock {
                    scope: scope.to_string(),
                    line,
                    call: format!("{name}({key})"),
                });
                None
            }
        }
        None => {
            violations.push(Violation::UnknownAdvisoryLock {
                scope: scope.to_string(),
                line,
                call: name.to_string(),
            });
            None
        }
    }
}

fn try_parse_dml(tokens: &[Token], i: usize) -> Option<DmlSite> {
    let prev = i.checked_sub(1).and_then(|idx| tokens.get(idx));
    let prev2 = i.checked_sub(2).and_then(|idx| tokens.get(idx));
    let line = tokens[i].line;

    if tokens[i].is_word("insert") && tokens.get(i + 1).is_some_and(|t| t.is_word("into")) {
        let table_idx = skip_schema_prefix(tokens, i + 2);
        let table = tokens.get(table_idx)?.as_word()?.to_string();
        return Some(DmlSite {
            token_idx: i,
            line,
            kind: DmlKind::Insert,
            table,
        });
    }

    if tokens[i].is_word("delete") && tokens.get(i + 1).is_some_and(|t| t.is_word("from")) {
        let mut table_idx = i + 2;
        if tokens.get(table_idx).is_some_and(|t| t.is_word("only")) {
            table_idx += 1;
        }
        table_idx = skip_schema_prefix(tokens, table_idx);
        let table = tokens.get(table_idx)?.as_word()?.to_string();
        return Some(DmlSite {
            token_idx: i,
            line,
            kind: DmlKind::Delete,
            table,
        });
    }

    if tokens[i].is_word("truncate") {
        let is_trigger_event =
            prev.is_some_and(|t| t.is_word("before") || t.is_word("after") || t.is_word("or"));
        if is_trigger_event {
            return None;
        }
        let mut table_idx = i + 1;
        if tokens.get(table_idx).is_some_and(|t| t.is_word("table")) {
            table_idx += 1;
        }
        if tokens.get(table_idx).is_some_and(|t| t.is_word("only")) {
            table_idx += 1;
        }
        table_idx = skip_schema_prefix(tokens, table_idx);
        let table = tokens.get(table_idx)?.as_word()?.to_string();
        return Some(DmlSite {
            token_idx: i,
            line,
            kind: DmlKind::Truncate,
            table,
        });
    }

    if tokens[i].is_word("merge") && tokens.get(i + 1).is_some_and(|t| t.is_word("into")) {
        let mut table_idx = i + 2;
        if tokens.get(table_idx).is_some_and(|t| t.is_word("only")) {
            table_idx += 1;
        }
        table_idx = skip_schema_prefix(tokens, table_idx);
        let table = tokens.get(table_idx)?.as_word()?.to_string();
        return Some(DmlSite {
            token_idx: i,
            line,
            kind: DmlKind::Merge,
            table,
        });
    }

    if tokens[i].is_word("update") {
        let is_conflict_update = prev.is_some_and(|t| t.is_word("do"))
            || tokens.get(i + 1).is_some_and(|t| t.is_word("set"));
        let is_row_lock = prev.is_some_and(|t| t.is_word("for"))
            || (prev2.is_some_and(|t| t.is_word("no")) && prev.is_some_and(|t| t.is_word("key")));
        let is_trigger_event = prev.is_some_and(|t| {
            t.is_word("before") || t.is_word("after") || t.is_word("of") || t.is_word("or")
        });
        if is_conflict_update || is_row_lock || is_trigger_event {
            return None;
        }
        let mut table_idx = i + 1;
        if tokens.get(table_idx).is_some_and(|t| t.is_word("only")) {
            table_idx += 1;
        }
        table_idx = skip_schema_prefix(tokens, table_idx);
        let table = tokens.get(table_idx)?.as_word()?;
        if matches!(
            table,
            "set" | "then" | "end" | "when" | "else" | "from" | "where" | "on"
        ) {
            return None;
        }
        return Some(DmlSite {
            token_idx: i,
            line,
            kind: DmlKind::Update,
            table: table.to_string(),
        });
    }

    None
}

fn check_monotonic_locks(scope: &str, locks: &[LockAcquisition], violations: &mut Vec<Violation>) {
    for pair in locks.windows(2) {
        let prev = pair[0];
        let next = pair[1];
        if !prev.tier.compatible_next(next.tier) {
            violations.push(Violation::InvalidLockOrder {
                scope: scope.to_string(),
                prev: prev.tier,
                prev_line: prev.line,
                next: next.tier,
                next_line: next.line,
            });
        }
    }
}

fn audit_function(
    func: &ParsedFunction,
    spawn_events_has_xid_default: bool,
    summary: &mut AuditSummary,
    violations: &mut Vec<Violation>,
) {
    let scan = scan_body_events(&func.name, &func.body_tokens, violations);
    check_monotonic_locks(&func.name, &scan.locks, violations);

    if !scan.dmls.is_empty() {
        summary.dml_functions += 1;
    }

    let is_exempt = EXEMPT_DML_FUNCTIONS.contains(&func.name.as_str());
    if !func.is_trigger && !is_exempt {
        check_function_l0_rules(func, &scan, violations);
    }

    for dml in &scan.dmls {
        if dml.table == "spawn_events" && matches!(dml.kind, DmlKind::Insert | DmlKind::Merge) {
            summary.spawn_events_writers.push(func.name.clone());
            check_spawn_events_write(func, dml, &scan, spawn_events_has_xid_default, violations);
        }
    }
}

fn check_function_l0_rules(
    func: &ParsedFunction,
    scan: &BodyScan,
    violations: &mut Vec<Violation>,
) {
    let first_inner = scan
        .locks
        .iter()
        .find(|lock| lock.tier.is_inner_under_l0())
        .copied();
    let first_dml = scan.dmls.first();
    if first_dml.is_none() && first_inner.is_none() {
        return;
    }

    let first_unconditional_l0 = scan
        .locks
        .iter()
        .find(|lock| lock.tier == AdvisoryLockTier::L0SchemaMigrate && lock.cond_depth == 0)
        .copied();

    let Some(l0) = first_unconditional_l0 else {
        if let Some(cond_l0) = scan
            .locks
            .iter()
            .find(|lock| lock.tier == AdvisoryLockTier::L0SchemaMigrate)
        {
            violations.push(Violation::ConditionalMigrateSharedLock {
                function: func.name.clone(),
                line: cond_l0.line,
            });
        } else {
            let reason = match (first_dml, first_inner) {
                (Some(dml), _) => format!("performs {} {}", dml.kind, dml.table),
                (None, Some(lock)) => format!("acquires {}", lock.tier),
                (None, None) => unreachable!(),
            };
            violations.push(Violation::MissingMigrateSharedLock {
                function: func.name.clone(),
                line: func.line,
                reason,
            });
        }
        return;
    };

    if let Some(dml) = first_dml
        && l0.token_idx > dml.token_idx
    {
        violations.push(Violation::MigrateSharedLockAfterDml {
            function: func.name.clone(),
            l0_line: l0.line,
            dml_line: dml.line,
            dml: format!("{} {}", dml.kind, dml.table),
        });
    }

    if let Some(inner) = first_inner
        && l0.token_idx > inner.token_idx
    {
        violations.push(Violation::MigrateSharedLockAfterInnerLock {
            function: func.name.clone(),
            l0_line: l0.line,
            lock: inner.tier,
            lock_line: inner.line,
        });
    }
}

fn check_spawn_events_write(
    func: &ParsedFunction,
    dml: &DmlSite,
    scan: &BodyScan,
    spawn_events_has_xid_default: bool,
    violations: &mut Vec<Violation>,
) {
    let has_unconditional_l3 = scan.locks.iter().any(|lock| {
        lock.tier == AdvisoryLockTier::L3Session
            && lock.blocking
            && lock.cond_depth == 0
            && lock.token_idx < dml.token_idx
    }) || scan
        .unconditional_l3_ready_at
        .iter()
        .any(|&ready_idx| ready_idx < dml.token_idx);
    if !has_unconditional_l3 {
        violations.push(Violation::SpawnEventsInsertWithoutL3Lock {
            function: func.name.clone(),
            insert_line: dml.line,
        });
    }

    if dml.kind == DmlKind::Merge {
        violations.push(Violation::SpawnEventsInsertWithoutCommitXid {
            function: func.name.clone(),
            insert_line: dml.line,
            detail: "MERGE INTO spawn_events is forbidden".to_string(),
        });
        return;
    }

    let tokens = &func.body_tokens;
    let table_idx = skip_schema_prefix(tokens, dml.token_idx + 2);
    let mut cursor = table_idx + 1;
    if tokens.get(cursor).is_some_and(|t| t.is_word("as")) {
        cursor += 2;
    }

    let Some(stmt_end_rel) = tokens[cursor..]
        .iter()
        .position(|t| matches!(t.kind, TokenKind::Semicolon))
    else {
        violations.push(Violation::SpawnEventsInsertWithoutCommitXid {
            function: func.name.clone(),
            insert_line: dml.line,
            detail: "unterminated INSERT statement".to_string(),
        });
        return;
    };

    if let Err(detail) = verify_spawn_events_commit_xid(
        &tokens[cursor..cursor + stmt_end_rel],
        spawn_events_has_xid_default,
    ) {
        violations.push(Violation::SpawnEventsInsertWithoutCommitXid {
            function: func.name.clone(),
            insert_line: dml.line,
            detail,
        });
    }
}

fn verify_spawn_events_commit_xid(
    insert_stmt: &[Token],
    spawn_events_has_xid_default: bool,
) -> Result<(), String> {
    let first_tok = insert_stmt
        .first()
        .ok_or_else(|| "empty INSERT target clause".to_string())?;
    if !matches!(first_tok.kind, TokenKind::LParen) {
        return Err("positional INSERT without explicit column list".to_string());
    }
    let cols_close =
        find_matching_paren(insert_stmt, 0).ok_or_else(|| "malformed column list".to_string())?;

    let cols = split_top_level_commas(&insert_stmt[1..cols_close]);
    let Some(xid_col) = cols
        .iter()
        .position(|col| col.len() == 1 && col[0].is_word("commit_xid"))
    else {
        return if spawn_events_has_xid_default {
            Ok(())
        } else {
            Err("commit_xid omitted without pg_current_xact_id() table default".to_string())
        };
    };

    let after_cols = &insert_stmt[cols_close + 1..];
    if after_cols.first().is_some_and(|t| t.is_word("values")) {
        let mut tuple_pos = 1;
        let mut checked = 0_usize;
        while tuple_pos < after_cols.len()
            && matches!(after_cols[tuple_pos].kind, TokenKind::LParen)
        {
            let vals_close = find_matching_paren(after_cols, tuple_pos)
                .ok_or_else(|| "malformed VALUES tuple".to_string())?;
            let values = split_top_level_commas(&after_cols[tuple_pos + 1..vals_close]);
            let Some(expr) = values.get(xid_col) else {
                return Err("explicit commit_xid value missing from VALUES tuple".to_string());
            };
            if !is_pg_current_xact_id_call(expr) {
                return Err("explicit commit_xid value must be pg_current_xact_id()".to_string());
            }
            checked += 1;
            tuple_pos = vals_close + 1;
            if after_cols
                .get(tuple_pos)
                .is_some_and(|t| matches!(t.kind, TokenKind::Comma))
            {
                tuple_pos += 1;
            } else {
                break;
            }
        }
        return if checked > 0 {
            Ok(())
        } else {
            Err("explicit commit_xid value must be pg_current_xact_id()".to_string())
        };
    }

    if after_cols.first().is_some_and(|t| t.is_word("select")) {
        let select_end = after_cols[1..]
            .iter()
            .position(|t| t.is_word("from") || t.is_word("on") || t.is_word("returning"))
            .map_or(after_cols.len(), |pos| pos + 1);
        let projections = split_top_level_commas(&after_cols[1..select_end]);
        if projections
            .get(xid_col)
            .is_some_and(|expr| is_pg_current_xact_id_call(expr))
        {
            return Ok(());
        }
    }

    Err("explicit commit_xid must evaluate to pg_current_xact_id()".to_string())
}

fn split_top_level_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0_usize;
    for (idx, tok) in tokens.iter().enumerate() {
        match tok.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            TokenKind::Comma if depth == 0 => {
                parts.push(&tokens[start..idx]);
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

fn audit_schema_sql(sql: &str) -> Result<AuditSummary, Vec<Violation>> {
    let mut violations = Vec::new();
    let mut retired_lock_hits = 0_usize;
    for (idx, raw_line) in sql.lines().enumerate() {
        if raw_line.contains(RETIRED_GLOBAL_LOCK) {
            retired_lock_hits += 1;
            violations.push(Violation::RetiredGlobalLock {
                line: idx + 1,
                snippet: raw_line.trim().to_string(),
            });
        }
    }

    let extracted = extract_schema(sql);
    let all_functions: Vec<&ParsedFunction> = extracted.functions.iter().collect();

    let mut summary = AuditSummary {
        total_functions: all_functions.len(),
        trigger_functions: all_functions.iter().filter(|f| f.is_trigger).count(),
        do_blocks: extracted.do_blocks.len(),
        retired_lock_hits,
        ..AuditSummary::default()
    };

    check_bootstrap_l0(&extracted, &mut violations);

    for do_block in &extracted.do_blocks {
        let scope = format!("DO@L{}", do_block.line);
        let scan = scan_body_events(&scope, &do_block.body_tokens, &mut violations);
        check_monotonic_locks(&scope, &scan.locks, &mut violations);
    }

    for func in &all_functions {
        audit_function(
            func,
            extracted.spawn_events_has_xid_default,
            &mut summary,
            &mut violations,
        );
    }

    for exempt in EXEMPT_DML_FUNCTIONS {
        if !all_functions.iter().any(|f| f.name == *exempt) {
            violations.push(Violation::UnusedExemption {
                function: (*exempt).to_string(),
            });
        }
    }

    if violations.is_empty() {
        Ok(summary)
    } else {
        Err(violations)
    }
}

fn check_bootstrap_l0(extracted: &ExtractedSchema, violations: &mut Vec<Violation>) {
    let Some(first_table_off) = extracted.first_create_table_offset else {
        return;
    };
    let mut sink = Vec::new();
    let has_bootstrap_l0 = extracted
        .do_blocks
        .iter()
        .filter(|block| block.offset < first_table_off)
        .any(|block| {
            let scope = format!("DO@L{}", block.line);
            let scan = scan_body_events(&scope, &block.body_tokens, &mut sink);
            scan.locks.iter().any(|lock| {
                lock.tier == AdvisoryLockTier::L0SchemaMigrate
                    && lock.blocking
                    && lock.cond_depth == 0
            })
        });
    if !has_bootstrap_l0 {
        violations.push(Violation::MissingExclusiveBootstrapLock);
    }
}

fn wrap_with_valid_bootstrap(fn_sql: &str, events_default: bool) -> String {
    let default_clause = if events_default {
        "default pg_current_xact_id()"
    } else {
        ""
    };
    format!(
        "do $$ begin perform pg_advisory_xact_lock(1399957400, 1); end $$;\n\
         create table if not exists spawn_events (id bigint, commit_xid xid8 not null {default_clause});\n\
         {fn_sql}"
    )
}

#[test]
fn schema_sql_satisfies_four_tier_lock_order_and_xid8_watermark_invariants() {
    let summary = match audit_schema_sql(SCHEMA_SQL) {
        Ok(summary) => summary,
        Err(violations) => {
            let report = violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ");
            panic!(
                "schema.sql violated {} lock-order / xid8 invariant(s):\n  {report}",
                violations.len()
            );
        }
    };

    assert!(
        summary.total_functions >= 120,
        "expected >= 120 spawn_* functions in schema.sql, parsed {}",
        summary.total_functions
    );
    assert!(
        summary.trigger_functions >= 5,
        "expected >= 5 trigger functions, parsed {}",
        summary.trigger_functions
    );
    assert!(
        summary.dml_functions >= 35,
        "expected >= 35 DML functions in schema.sql, parsed {}",
        summary.dml_functions
    );
    assert_eq!(summary.retired_lock_hits, 0);
    assert_eq!(summary.spawn_events_writers, vec!["spawn_append_trusted"]);
}

#[test]
fn mutation_rejects_missing_l0_for_insert_update_and_delete() {
    for (stmt, expected_reason) in [
        (
            "insert into mission_tasks (id) values (1);",
            "performs INSERT mission_tasks",
        ),
        (
            "update mission_tasks set state = 'done' where id = $1;",
            "performs UPDATE mission_tasks",
        ),
        (
            "delete from mission_tasks where id = $1;",
            "performs DELETE mission_tasks",
        ),
    ] {
        let sql = wrap_with_valid_bootstrap(
            &format!(
                "create or replace function spawn_bad_writer() returns void language plpgsql as $$\n\
                 begin\n\
                     {stmt}\n\
                 end $$;"
            ),
            true,
        );
        let errors = audit_schema_sql(&sql).expect_err("missing L0 before DML must fail");
        assert!(
            errors.iter().any(|err| matches!(
                err,
                Violation::MissingMigrateSharedLock { function, reason, .. }
                if function == "spawn_bad_writer" && reason == expected_reason
            )),
            "expected MissingMigrateSharedLock({expected_reason}), got {errors:?}"
        );
    }
}

#[test]
fn mutation_rejects_missing_l0_before_l2_and_l3_locks() {
    for lock_call in [
        "perform pg_advisory_xact_lock(1399957402, 0);",
        "perform pg_advisory_xact_lock(1399957401, hashtext('s'));",
    ] {
        let sql = wrap_with_valid_bootstrap(
            &format!(
                "create or replace function spawn_bad_lock_only() returns void language plpgsql as $$\n\
                 begin\n\
                     {lock_call}\n\
                 end $$;"
            ),
            true,
        );
        let errors = audit_schema_sql(&sql).expect_err("L2/L3 without L0 must fail");
        assert!(
            errors.iter().any(|err| matches!(
                err,
                Violation::MissingMigrateSharedLock { function, .. }
                if function == "spawn_bad_lock_only"
            )),
            "expected MissingMigrateSharedLock for `{lock_call}`, got {errors:?}"
        );
    }
}

#[test]
fn mutation_rejects_l0_after_dml_and_l0_after_inner_lock() {
    let dml_first = wrap_with_valid_bootstrap(
        "create or replace function spawn_l0_after_dml() returns void language plpgsql as $$\n\
         begin\n\
             update mission_tasks set state = 'done';\n\
             perform spawn_lock_migrate_shared();\n\
         end $$;",
        true,
    );
    let err_dml = audit_schema_sql(&dml_first).expect_err("L0 after DML must fail");
    assert!(
        err_dml.iter().any(|err| matches!(
            err,
            Violation::MigrateSharedLockAfterDml { function, dml, .. }
            if function == "spawn_l0_after_dml" && dml == "UPDATE mission_tasks"
        )),
        "expected MigrateSharedLockAfterDml, got {err_dml:?}"
    );

    let l2_first = wrap_with_valid_bootstrap(
        "create or replace function spawn_l0_after_l2() returns void language plpgsql as $$\n\
         begin\n\
             perform pg_advisory_xact_lock(1399957402, 0);\n\
             perform spawn_lock_migrate_shared();\n\
         end $$;",
        true,
    );
    let err_l2 = audit_schema_sql(&l2_first).expect_err("L0 after L2 must fail");
    assert!(
        err_l2.iter().any(|err| matches!(
            err,
            Violation::MigrateSharedLockAfterInnerLock {
                function,
                lock: AdvisoryLockTier::L2Control,
                ..
            } if function == "spawn_l0_after_l2"
        )),
        "expected MigrateSharedLockAfterInnerLock, got {err_l2:?}"
    );
}

#[test]
fn mutation_rejects_l3_before_l2_and_l1_after_l0_inversions() {
    let l3_before_l2 = wrap_with_valid_bootstrap(
        "create or replace function spawn_l3_before_l2() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957401, hashtext('s'));\n\
             perform pg_advisory_xact_lock(1399957402, 0);\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&l3_before_l2).expect_err("L3 -> L2 inversion must fail");
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::InvalidLockOrder {
                scope,
                prev: AdvisoryLockTier::L3Session,
                next: AdvisoryLockTier::L2Control,
                ..
            } if scope == "spawn_l3_before_l2"
        )),
        "expected L3 -> L2 InvalidLockOrder, got {errs:?}"
    );

    let l1_after_l0 = wrap_with_valid_bootstrap(
        "create or replace function spawn_l1_after_l0() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957337);\n\
         end $$;",
        true,
    );
    let errs_l1 = audit_schema_sql(&l1_after_l0).expect_err("L0 -> L1 inversion must fail");
    assert!(
        errs_l1.iter().any(|err| matches!(
            err,
            Violation::InvalidLockOrder {
                scope,
                prev: AdvisoryLockTier::L0SchemaMigrate,
                next: AdvisoryLockTier::L1LandingHold,
                ..
            } if scope == "spawn_l1_after_l0"
        )),
        "expected L0 -> L1 InvalidLockOrder, got {errs_l1:?}"
    );
}

#[test]
fn mutation_rejects_retired_global_lock_1399957335() {
    let retired = wrap_with_valid_bootstrap(
        "create or replace function spawn_uses_retired() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957335);\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&retired).expect_err("retired lock 1399957335 must fail");
    assert!(
        errs.iter()
            .any(|err| matches!(err, Violation::RetiredGlobalLock { .. })),
        "expected RetiredGlobalLock, got {errs:?}"
    );
}

#[test]
fn mutation_rejects_spawn_events_insert_without_blocking_l3_lock() {
    let no_l3 = wrap_with_valid_bootstrap(
        "create or replace function spawn_unprotected_append() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&no_l3).expect_err("spawn_events insert without L3 must fail");
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutL3Lock { function, .. }
            if function == "spawn_unprotected_append"
        )),
        "expected SpawnEventsInsertWithoutL3Lock, got {errs:?}"
    );

    let try_only_l3 = wrap_with_valid_bootstrap(
        "create or replace function spawn_try_only_append() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_try_advisory_xact_lock(1399957401, hashtext('s'));\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let try_errs = audit_schema_sql(&try_only_l3)
        .expect_err("try-only L3 before spawn_events insert must fail");
    assert!(
        try_errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutL3Lock { function, .. }
            if function == "spawn_try_only_append"
        )),
        "expected SpawnEventsInsertWithoutL3Lock for try-only L3, got {try_errs:?}"
    );
}

#[test]
fn mutation_rejects_spawn_events_insert_without_valid_commit_xid() {
    let forged_xid = wrap_with_valid_bootstrap(
        "create or replace function spawn_bad_xid_writer() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957401, hashtext('s'));\n\
             insert into spawn_events (id, commit_xid) values (1, '3'::xid8);\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&forged_xid).expect_err("forged commit_xid must fail");
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutCommitXid { function, .. }
            if function == "spawn_bad_xid_writer"
        )),
        "expected SpawnEventsInsertWithoutCommitXid for forged xid, got {errs:?}"
    );

    let omitted_no_default = wrap_with_valid_bootstrap(
        "create or replace function spawn_omitted_xid_writer() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957401, hashtext('s'));\n\
             insert into spawn_events (id) values (1);\n\
         end $$;",
        false,
    );
    let omitted_errs = audit_schema_sql(&omitted_no_default)
        .expect_err("omitted commit_xid without default must fail");
    assert!(
        omitted_errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutCommitXid { function, .. }
            if function == "spawn_omitted_xid_writer"
        )),
        "expected SpawnEventsInsertWithoutCommitXid for missing default, got {omitted_errs:?}"
    );

    let omitted_with_default = wrap_with_valid_bootstrap(
        "create or replace function spawn_default_xid_writer() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957401, hashtext('s'));\n\
             insert into spawn_events (id) values (1);\n\
         end $$;",
        true,
    );
    audit_schema_sql(&omitted_with_default)
        .expect("omitted commit_xid allowed when table defines default pg_current_xact_id()");
}

#[test]
fn mutation_rejects_conditional_l0_and_unknown_advisory_locks() {
    let conditional_l0 = wrap_with_valid_bootstrap(
        "create or replace function spawn_cond_l0(p_flag boolean) returns void language plpgsql as $$\n\
         begin\n\
             if p_flag then\n\
                 perform spawn_lock_migrate_shared();\n\
             end if;\n\
             delete from mission_tasks where id = 1;\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&conditional_l0).expect_err("conditional L0 must fail");
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::ConditionalMigrateSharedLock { function, .. }
            if function == "spawn_cond_l0"
        )),
        "expected ConditionalMigrateSharedLock, got {errs:?}"
    );

    let unknown_lock = wrap_with_valid_bootstrap(
        "create or replace function spawn_unknown_lock() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(999999999);\n\
         end $$;",
        true,
    );
    let unknown_errs = audit_schema_sql(&unknown_lock).expect_err("unknown lock must fail");
    assert!(
        unknown_errs.iter().any(|err| matches!(
            err,
            Violation::UnknownAdvisoryLock { scope, .. }
            if scope == "spawn_unknown_lock"
        )),
        "expected UnknownAdvisoryLock, got {unknown_errs:?}"
    );
}

#[test]
fn mutation_accepts_valid_hierarchy_and_ignores_comments_strings_for_update_and_do_update() {
    let valid_sql = wrap_with_valid_bootstrap(
        "create or replace function spawn_valid_full(p_id bigint, p_s text) returns void language plpgsql as $$\n\
         declare\n\
             v_note text := 'insert into spawn_events values (1); update mission_tasks set state = ''x'';';\n\
         begin\n\
             -- insert into mission_tasks values (1); pg_advisory_xact_lock(999999999);\n\
             if exists (select 1 from mission_tasks where case when id = $1 then true else false end) then\n\
                 raise notice 'checked %', v_note;\n\
             end if;\n\
             perform pg_advisory_lock(1399957337);\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957402, 0);\n\
             perform pg_advisory_xact_lock(1399957401, hashtext(p_s));\n\
             select id from mission_tasks where id = $1 for update;\n\
             insert into mission_tasks (id) values ($1) on conflict (id) do update set id = excluded.id;\n\
             perform pg_advisory_unlock(1399957337);\n\
         end $$;",
        true,
    );
    let summary = audit_schema_sql(&valid_sql).expect("valid L1->L0->L2->L3 SQL should pass");
    assert_eq!(summary.total_functions, 1);
    assert_eq!(summary.dml_functions, 1);
}

#[test]
fn mutation_rejects_subdomain_leaf_mixing_and_missing_bootstrap_l0() {
    let mixed_leaf = wrap_with_valid_bootstrap(
        "create or replace function spawn_mixed_leaf() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957341);\n\
             perform pg_advisory_xact_lock(1399957401, hashtext('s'));\n\
         end $$;",
        true,
    );
    let mix_errs = audit_schema_sql(&mixed_leaf).expect_err("CostLeaf -> L3 must fail");
    assert!(
        mix_errs.iter().any(|err| matches!(
            err,
            Violation::InvalidLockOrder {
                scope,
                prev: AdvisoryLockTier::L2CostFold,
                next: AdvisoryLockTier::L3Session,
                ..
            } if scope == "spawn_mixed_leaf"
        )),
        "expected CostLeaf -> L3 InvalidLockOrder, got {mix_errs:?}"
    );

    let no_bootstrap = "create table if not exists spawn_events (id bigint, commit_xid xid8 not null default pg_current_xact_id());\n\
         create or replace function spawn_ok_fn() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             delete from mission_tasks where id = 1;\n\
         end $$;";
    let boot_errs = audit_schema_sql(no_bootstrap).expect_err("missing bootstrap L0 must fail");
    assert!(
        boot_errs
            .iter()
            .any(|err| matches!(err, Violation::MissingExclusiveBootstrapLock)),
        "expected MissingExclusiveBootstrapLock, got {boot_errs:?}"
    );
}

#[test]
fn mutation_rejects_if_exists_if_not_exists_conditional_l0_and_underflow() {
    let cases = [
        (
            "if_exists_l0",
            "create or replace function spawn_b1_if_exists() returns void language plpgsql as $$\n\
             begin\n\
                 if exists (select 1 from mission_tasks) then\n\
                     perform spawn_lock_migrate_shared();\n\
                 end if;\n\
                 delete from mission_tasks where id = 1;\n\
             end $$;",
        ),
        (
            "if_not_exists_l0",
            "create or replace function spawn_b1_if_not_exists() returns void language plpgsql as $$\n\
             begin\n\
                 if not exists (select 1 from mission_tasks) then\n\
                     perform spawn_lock_migrate_shared();\n\
                 end if;\n\
                 delete from mission_tasks where id = 1;\n\
             end $$;",
        ),
        (
            "nested_if_exists_underflow_evasion",
            "create or replace function spawn_b1_nested(p_flag boolean) returns void language plpgsql as $$\n\
             begin\n\
                 if p_flag then\n\
                     if exists (select 1 from mission_tasks) then\n\
                         null;\n\
                     end if;\n\
                     perform spawn_lock_migrate_shared();\n\
                 end if;\n\
                 delete from mission_tasks where id = 1;\n\
             end $$;",
        ),
    ];

    for (name, fn_sql) in cases {
        let sql = wrap_with_valid_bootstrap(fn_sql, true);
        let errs = audit_schema_sql(&sql).expect_err(&format!("{name} must fail"));
        assert!(
            errs.iter()
                .any(|err| matches!(err, Violation::ConditionalMigrateSharedLock { .. })),
            "{name}: expected ConditionalMigrateSharedLock, got {errs:?}"
        );
    }

    let underflow = wrap_with_valid_bootstrap(
        "create or replace function spawn_b1_underflow() returns void language plpgsql as $$\n\
         begin\n\
             end if;\n\
             perform spawn_lock_migrate_shared();\n\
             delete from mission_tasks where id = 1;\n\
         end $$;",
        true,
    );
    let underflow_errs =
        audit_schema_sql(&underflow).expect_err("control-flow underflow must fail");
    assert!(
        underflow_errs
            .iter()
            .any(|err| matches!(err, Violation::UnbalancedControlFlow { .. })),
        "expected UnbalancedControlFlow, got {underflow_errs:?}"
    );
}

#[test]
fn mutation_rejects_case_loop_while_and_exception_conditional_l0() {
    let cases = [
        (
            "case_stmt_l0",
            "create or replace function spawn_b2_case(p_x integer) returns void language plpgsql as $$\n\
             begin\n\
                 case p_x\n\
                 when 1 then\n\
                     perform spawn_lock_migrate_shared();\n\
                 else\n\
                     null;\n\
                 end case;\n\
                 delete from mission_tasks where id = 1;\n\
             end $$;",
        ),
        (
            "while_loop_l0",
            "create or replace function spawn_b2_while(p_flag boolean) returns void language plpgsql as $$\n\
             begin\n\
                 while p_flag loop\n\
                     perform spawn_lock_migrate_shared();\n\
                     exit;\n\
                 end loop;\n\
                 update mission_tasks set state = 'done';\n\
             end $$;",
        ),
        (
            "for_loop_l0",
            "create or replace function spawn_b2_for() returns void language plpgsql as $$\n\
             declare v_i integer;\n\
             begin\n\
                 for v_i in 1..1 loop\n\
                     perform spawn_lock_migrate_shared();\n\
                 end loop;\n\
                 insert into mission_tasks (id) values (1);\n\
             end $$;",
        ),
        (
            "exception_handler_l0",
            "create or replace function spawn_b2_exception() returns void language plpgsql as $$\n\
             begin\n\
                 select 1;\n\
             exception when others then\n\
                 perform spawn_lock_migrate_shared();\n\
                 delete from mission_tasks where id = 1;\n\
             end $$;",
        ),
    ];

    for (name, fn_sql) in cases {
        let sql = wrap_with_valid_bootstrap(fn_sql, true);
        let errs = audit_schema_sql(&sql).expect_err(&format!("{name} must fail"));
        assert!(
            errs.iter()
                .any(|err| matches!(err, Violation::ConditionalMigrateSharedLock { .. })),
            "{name}: expected ConditionalMigrateSharedLock, got {errs:?}"
        );
    }
}

#[test]
fn mutation_rejects_conditional_l3_before_spawn_events_insert_and_accepts_trylock_fallback() {
    let conditional_l3 = wrap_with_valid_bootstrap(
        "create or replace function spawn_b3_cond_l3(p_flag boolean) returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             if p_flag then\n\
                 perform pg_advisory_xact_lock(1399957401, 1);\n\
             end if;\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let cond_errs = audit_schema_sql(&conditional_l3).expect_err("conditional L3 must fail");
    assert!(
        cond_errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutL3Lock { function, .. }
            if function == "spawn_b3_cond_l3"
        )),
        "expected SpawnEventsInsertWithoutL3Lock, got {cond_errs:?}"
    );

    let extra_clause = wrap_with_valid_bootstrap(
        "create or replace function spawn_b3_extra_cond(p_session text, p_ok boolean) returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             if not pg_try_advisory_xact_lock(1399957401, hashtext(p_session)) and p_ok then\n\
                 perform pg_advisory_xact_lock(1399957401, hashtext(p_session));\n\
             end if;\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let extra_errs =
        audit_schema_sql(&extra_clause).expect_err("compound try-lock guard must fail");
    assert!(
        extra_errs
            .iter()
            .any(|err| matches!(err, Violation::SpawnEventsInsertWithoutL3Lock { .. })),
        "expected SpawnEventsInsertWithoutL3Lock for compound guard, got {extra_errs:?}"
    );

    let key_mismatch = wrap_with_valid_bootstrap(
        "create or replace function spawn_b3_mismatch(p_session text) returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             if not pg_try_advisory_xact_lock(1399957401, hashtext(p_session)) then\n\
                 perform pg_advisory_xact_lock(1399957401, 42);\n\
             end if;\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let mismatch_errs = audit_schema_sql(&key_mismatch).expect_err("mismatched L3 keys must fail");
    assert!(
        mismatch_errs
            .iter()
            .any(|err| matches!(err, Violation::SpawnEventsInsertWithoutL3Lock { .. })),
        "expected SpawnEventsInsertWithoutL3Lock for key mismatch, got {mismatch_errs:?}"
    );

    let valid_fallback = wrap_with_valid_bootstrap(
        "create or replace function spawn_b3_valid_fallback(p_session text, p_dup boolean) returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             if not pg_try_advisory_xact_lock(1399957401, hashtext(p_session)) then\n\
                 if p_dup then\n\
                     return;\n\
                 end if;\n\
                 perform pg_advisory_xact_lock(1399957401, hashtext(p_session));\n\
             end if;\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let ok = audit_schema_sql(&valid_fallback).expect("exact L3 try-lock fallback should pass");
    assert_eq!(ok.spawn_events_writers, vec!["spawn_b3_valid_fallback"]);
}

#[test]
fn mutation_audits_non_spawn_prefixed_functions_and_tracks_writers() {
    let bypass_attempt = wrap_with_valid_bootstrap(
        "create or replace function helper_writer() returns void language plpgsql as $$\n\
         begin\n\
             insert into spawn_events (id, commit_xid) values (1, '3'::xid8);\n\
         end $$;",
        true,
    );
    let errs = audit_schema_sql(&bypass_attempt).expect_err("non-spawn_ writer must be audited");
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::MissingMigrateSharedLock { function, .. } if function == "helper_writer"
        )),
        "expected MissingMigrateSharedLock for helper_writer, got {errs:?}"
    );
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutL3Lock { function, .. } if function == "helper_writer"
        )),
        "expected SpawnEventsInsertWithoutL3Lock for helper_writer, got {errs:?}"
    );
    assert!(
        errs.iter().any(|err| matches!(
            err,
            Violation::SpawnEventsInsertWithoutCommitXid { function, .. } if function == "helper_writer"
        )),
        "expected SpawnEventsInsertWithoutCommitXid for helper_writer, got {errs:?}"
    );

    let tracked_ok = wrap_with_valid_bootstrap(
        "create or replace function helper_writer_ok() returns void language plpgsql as $$\n\
         begin\n\
             perform spawn_lock_migrate_shared();\n\
             perform pg_advisory_xact_lock(1399957401, 7);\n\
             insert into spawn_events (id, commit_xid) values (1, pg_current_xact_id());\n\
         end $$;",
        true,
    );
    let ok = audit_schema_sql(&tracked_ok).expect("compliant non-spawn function should pass");
    assert_eq!(ok.spawn_events_writers, vec!["helper_writer_ok"]);
}

#[test]
fn mutation_rejects_dynamic_execute_dml_or_locks_and_truncate_or_merge_without_l0() {
    let dynamic_cases = [
        (
            "exec_delete",
            "create or replace function spawn_exec_del() returns void language plpgsql as $$\n\
             begin\n\
                 execute 'delete from mission_tasks where id = 1';\n\
             end $$;",
        ),
        (
            "exec_lock",
            "create or replace function spawn_exec_lock() returns void language plpgsql as $$\n\
             begin\n\
                 perform spawn_lock_migrate_shared();\n\
                 execute 'select pg_advisory_xact_lock(999999999)';\n\
             end $$;",
        ),
        (
            "exec_spawn_events",
            "create or replace function spawn_exec_events() returns void language plpgsql as $$\n\
             begin\n\
                 perform spawn_lock_migrate_shared();\n\
                 execute 'insert into spawn_events (id) values (1)';\n\
             end $$;",
        ),
        (
            "exec_opaque_var",
            "create or replace function spawn_exec_var(p_sql text) returns void language plpgsql as $$\n\
             begin\n\
                 perform spawn_lock_migrate_shared();\n\
                 execute p_sql;\n\
             end $$;",
        ),
    ];

    for (name, fn_sql) in dynamic_cases {
        let sql = wrap_with_valid_bootstrap(fn_sql, true);
        let errs = audit_schema_sql(&sql).expect_err(&format!("{name} must fail"));
        assert!(
            errs.iter()
                .any(|err| matches!(err, Violation::ForbiddenDynamicSql { .. })),
            "{name}: expected ForbiddenDynamicSql, got {errs:?}"
        );
    }

    let truncate_no_l0 = wrap_with_valid_bootstrap(
        "create or replace function spawn_trunc_no_l0() returns void language plpgsql as $$\n\
         begin\n\
             truncate table mission_tasks;\n\
         end $$;",
        true,
    );
    let trunc_errs = audit_schema_sql(&truncate_no_l0).expect_err("TRUNCATE without L0 must fail");
    assert!(
        trunc_errs.iter().any(|err| matches!(
            err,
            Violation::MissingMigrateSharedLock { function, .. } if function == "spawn_trunc_no_l0"
        )),
        "expected MissingMigrateSharedLock for TRUNCATE, got {trunc_errs:?}"
    );

    let merge_no_l0 = wrap_with_valid_bootstrap(
        "create or replace function spawn_merge_no_l0() returns void language plpgsql as $$\n\
         begin\n\
             merge into mission_tasks t using mission_tasks s on t.id = s.id when not matched then insert (id) values (s.id);\n\
         end $$;",
        true,
    );
    let merge_errs = audit_schema_sql(&merge_no_l0).expect_err("MERGE without L0 must fail");
    assert!(
        merge_errs.iter().any(|err| matches!(
            err,
            Violation::MissingMigrateSharedLock { function, .. } if function == "spawn_merge_no_l0"
        )),
        "expected MissingMigrateSharedLock for MERGE, got {merge_errs:?}"
    );
}

#[test]
fn mutation_enforces_lock_order_and_retired_lock_on_trigger_functions() {
    let valid_trigger = wrap_with_valid_bootstrap(
        "create or replace function spawn_trig_ok() returns trigger language plpgsql as $$\n\
         begin\n\
             perform pg_advisory_xact_lock(1399957402, 0);\n\
             perform pg_advisory_xact_lock(1399957401, 1);\n\
             insert into mission_tasks (id) values (1);\n\
             return new;\n\
         end $$;",
        true,
    );
    let ok = audit_schema_sql(&valid_trigger).expect("valid trigger exempt from L0 should pass");
    assert_eq!(ok.trigger_functions, 1);

    let bad_order_trigger = wrap_with_valid_bootstrap(
        "create or replace function spawn_trig_bad_order() returns trigger language plpgsql as $$\n\
         begin\n\
             perform pg_advisory_xact_lock(1399957401, 1);\n\
             perform pg_advisory_xact_lock(1399957402, 0);\n\
             return new;\n\
         end $$;",
        true,
    );
    let order_errs =
        audit_schema_sql(&bad_order_trigger).expect_err("trigger L3->L2 inversion must fail");
    assert!(
        order_errs.iter().any(|err| matches!(
            err,
            Violation::InvalidLockOrder {
                scope,
                prev: AdvisoryLockTier::L3Session,
                next: AdvisoryLockTier::L2Control,
                ..
            } if scope == "spawn_trig_bad_order"
        )),
        "expected InvalidLockOrder on trigger, got {order_errs:?}"
    );

    let retired_trigger = wrap_with_valid_bootstrap(
        "create or replace function spawn_trig_retired() returns trigger language plpgsql as $$\n\
         begin\n\
             perform pg_advisory_xact_lock(1399957335);\n\
             return new;\n\
         end $$;",
        true,
    );
    let ret_errs = audit_schema_sql(&retired_trigger).expect_err("trigger retired lock must fail");
    assert!(
        ret_errs
            .iter()
            .any(|err| matches!(err, Violation::RetiredGlobalLock { .. })),
        "expected RetiredGlobalLock on trigger, got {ret_errs:?}"
    );
}
