//! A strict reader for the subset of TOML that `Cargo.toml` files use, a
//! JSON reader for `package.json` and (with comments stripped)
//! `tsconfig.json` / `jsconfig.json`, and the manifest facts the resolver
//! needs.
//!
//! No `toml` crate: the gate builds offline and `toml` is not in Spawn's
//! `Cargo.lock`. The subset is tables, arrays of tables, dotted and quoted
//! keys, basic and literal strings (single and multi-line), arrays, inline
//! tables, booleans, and bare scalars (numbers, dates) kept as raw text.
//! Anything else is an error with its line: the caller turns it into a
//! visible `manifest_unsupported` diagnostic, never a guess.

use std::collections::BTreeMap;
use std::sync::Arc;

/// One TOML value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Str(String),
    Bool(bool),
    /// A number, date or other bare scalar, as written.
    Raw(String),
    Array(Vec<Value>),
    Table(Table),
}

/// A TOML table.
pub(crate) type Table = BTreeMap<String, Value>;

/// Why a manifest was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TomlError {
    /// 1-based line; 0 for a manifest-level problem.
    pub line: u32,
    pub message: String,
}

impl std::fmt::Display for TomlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

/// Parse `text` as the supported TOML subset.
pub(crate) fn parse_toml(text: &str) -> Result<Table, TomlError> {
    let mut parser = Parser {
        chars: text.chars().collect(),
        at: 0,
        line: 1,
    };
    parser.document()
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    line: u32,
}

impl Parser {
    fn error<T>(&self, message: impl Into<String>) -> Result<T, TomlError> {
        Err(TomlError {
            line: self.line,
            message: message.into(),
        })
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn starts_with(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(offset, c)| self.peek_at(offset) == Some(c))
    }

    /// Spaces and tabs.
    fn skip_blank(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
    }

    /// Blanks, comments and newlines (inside arrays).
    fn skip_space(&mut self) {
        loop {
            self.skip_blank();
            self.skip_comment();
            match self.peek() {
                Some('\n') => {
                    self.bump();
                }
                Some('\r') if self.peek_at(1) == Some('\n') => {
                    self.bump();
                    self.bump();
                }
                _ => return,
            }
        }
    }

    /// The rest of a line after a statement: blanks, a comment, a newline.
    fn end_of_line(&mut self) -> Result<(), TomlError> {
        self.skip_blank();
        self.skip_comment();
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some('\r') if self.peek_at(1) == Some('\n') => {
                self.bump();
                self.bump();
                Ok(())
            }
            Some(other) => self.error(format!("unexpected {other:?} after a value")),
        }
    }

    fn document(&mut self) -> Result<Table, TomlError> {
        let mut root = Table::new();
        let mut current: Vec<String> = Vec::new();
        loop {
            self.skip_space();
            let Some(c) = self.peek() else {
                return Ok(root);
            };
            // Errors found after the statement's newline still name its line.
            let line = self.line;
            let at_line = |mut error: TomlError| {
                error.line = line;
                error
            };
            if c == '[' {
                self.bump();
                let array = self.peek() == Some('[');
                if array {
                    self.bump();
                }
                self.skip_blank();
                let keys = self.key_path()?;
                self.skip_blank();
                let close = if array { "]]" } else { "]" };
                if !self.starts_with(close) {
                    return self.error(format!("expected {close} to close a table header"));
                }
                for _ in 0..close.len() {
                    self.bump();
                }
                self.end_of_line()?;
                if array {
                    self.push_array_table(&mut root, &keys).map_err(at_line)?;
                } else {
                    self.open_table(&mut root, &keys).map_err(at_line)?;
                }
                current = keys;
            } else {
                let keys = self.key_path()?;
                self.skip_blank();
                // Peek before consuming: a key ending its line is reported
                // on that line, not the next.
                if self.peek() != Some('=') {
                    return self.error("expected = after a key");
                }
                self.bump();
                self.skip_blank();
                let value = self.value()?;
                self.end_of_line()?;
                let table = self.table_at(&mut root, &current).map_err(at_line)?;
                self.insert(table, &keys, value).map_err(at_line)?;
            }
        }
    }

    /// `a.b."c d"`.
    fn key_path(&mut self) -> Result<Vec<String>, TomlError> {
        let mut keys = vec![self.key()?];
        loop {
            self.skip_blank();
            if self.peek() != Some('.') {
                return Ok(keys);
            }
            self.bump();
            self.skip_blank();
            keys.push(self.key()?);
        }
    }

    fn key(&mut self) -> Result<String, TomlError> {
        match self.peek() {
            Some('"') if !self.starts_with("\"\"\"") => self.basic_string(),
            Some('\'') if !self.starts_with("'''") => self.literal_string(),
            _ => {
                let mut key = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        key.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                if key.is_empty() {
                    return self.error("expected a key");
                }
                Ok(key)
            }
        }
    }

    fn value(&mut self) -> Result<Value, TomlError> {
        match self.peek() {
            Some('"') if self.starts_with("\"\"\"") => self.multiline_basic().map(Value::Str),
            Some('"') => self.basic_string().map(Value::Str),
            Some('\'') if self.starts_with("'''") => self.multiline_literal().map(Value::Str),
            Some('\'') => self.literal_string().map(Value::Str),
            Some('[') => self.array(),
            Some('{') => self.inline_table(),
            Some(_) => {
                let mut raw = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.' | ':') {
                        raw.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                match raw.as_str() {
                    "" => self.error("expected a value"),
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => Ok(Value::Raw(raw)),
                }
            }
            None => self.error("expected a value, found the end of the file"),
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), TomlError> {
        let Some(c) = self.bump() else {
            return self.error("unterminated escape");
        };
        match c {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' | 'U' => {
                let width = if c == 'u' { 4 } else { 8 };
                let mut hex = String::new();
                for _ in 0..width {
                    match self.bump() {
                        Some(digit) if digit.is_ascii_hexdigit() => hex.push(digit),
                        _ => return self.error("bad unicode escape"),
                    }
                }
                let Some(decoded) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                else {
                    return self.error("bad unicode escape");
                };
                out.push(decoded);
            }
            other => return self.error(format!("unsupported escape \\{other}")),
        }
        Ok(())
    }

    fn basic_string(&mut self) -> Result<String, TomlError> {
        self.bump();
        let mut out = String::new();
        loop {
            if matches!(self.peek(), None | Some('\n')) {
                return self.error("unterminated string");
            }
            match self.bump() {
                None => return self.error("unterminated string"),
                Some('"') => return Ok(out),
                Some('\\') => self.escape(&mut out)?,
                Some(c) => out.push(c),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, TomlError> {
        self.bump();
        let mut out = String::new();
        loop {
            if matches!(self.peek(), None | Some('\n')) {
                return self.error("unterminated string");
            }
            match self.bump() {
                None => return self.error("unterminated string"),
                Some('\'') => return Ok(out),
                Some(c) => out.push(c),
            }
        }
    }

    fn multiline_basic(&mut self) -> Result<String, TomlError> {
        for _ in 0..3 {
            self.bump();
        }
        if self.peek() == Some('\n') {
            self.bump();
        }
        let mut out = String::new();
        loop {
            if self.starts_with("\"\"\"") {
                for _ in 0..3 {
                    self.bump();
                }
                return Ok(out);
            }
            match self.bump() {
                None => return self.error("unterminated multi-line string"),
                Some('\\') if matches!(self.peek(), Some('\n' | ' ' | '\t' | '\r')) => {
                    // A line-ending backslash trims up to the next text.
                    while matches!(self.peek(), Some('\n' | ' ' | '\t' | '\r')) {
                        self.bump();
                    }
                }
                Some('\\') => self.escape(&mut out)?,
                Some(c) => out.push(c),
            }
        }
    }

    fn multiline_literal(&mut self) -> Result<String, TomlError> {
        for _ in 0..3 {
            self.bump();
        }
        if self.peek() == Some('\n') {
            self.bump();
        }
        let mut out = String::new();
        loop {
            if self.starts_with("'''") {
                for _ in 0..3 {
                    self.bump();
                }
                return Ok(out);
            }
            match self.bump() {
                None => return self.error("unterminated multi-line string"),
                Some(c) => out.push(c),
            }
        }
    }

    fn array(&mut self) -> Result<Value, TomlError> {
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.peek() == Some(']') {
                self.bump();
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip_space();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                _ => return self.error("expected , or ] in an array"),
            }
        }
    }

    fn inline_table(&mut self) -> Result<Value, TomlError> {
        self.bump();
        let mut table = Table::new();
        self.skip_space();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Value::Table(table));
        }
        loop {
            self.skip_space();
            let keys = self.key_path()?;
            self.skip_blank();
            if self.peek() != Some('=') {
                return self.error("expected = in an inline table");
            }
            self.bump();
            self.skip_blank();
            let value = self.value()?;
            self.insert(&mut table, &keys, value)?;
            self.skip_space();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(Value::Table(table));
                }
                _ => return self.error("expected , or } in an inline table"),
            }
        }
    }

    /// The table a `[a.b]` header names, created when missing. An array
    /// of tables on the way selects its last element.
    fn table_at<'t>(
        &self,
        root: &'t mut Table,
        keys: &[String],
    ) -> Result<&'t mut Table, TomlError> {
        let mut table = root;
        for key in keys {
            let entry = table
                .entry(key.clone())
                .or_insert_with(|| Value::Table(Table::new()));
            table = match entry {
                Value::Table(next) => next,
                Value::Array(items) => match items.last_mut() {
                    Some(Value::Table(next)) => next,
                    _ => return self.error(format!("{key} is not a table")),
                },
                _ => return self.error(format!("{key} is not a table")),
            };
        }
        Ok(table)
    }

    fn open_table(&self, root: &mut Table, keys: &[String]) -> Result<(), TomlError> {
        self.table_at(root, keys).map(|_| ())
    }

    fn push_array_table(&self, root: &mut Table, keys: &[String]) -> Result<(), TomlError> {
        let Some((last, parents)) = keys.split_last() else {
            return self.error("empty table header");
        };
        let parent = self.table_at(root, parents)?;
        match parent
            .entry(last.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(items) => {
                items.push(Value::Table(Table::new()));
                Ok(())
            }
            _ => self.error(format!("{last} is not an array of tables")),
        }
    }

    fn insert(&self, table: &mut Table, keys: &[String], value: Value) -> Result<(), TomlError> {
        let Some((last, parents)) = keys.split_last() else {
            return self.error("empty key");
        };
        let target = self.table_at(table, parents)?;
        if target.contains_key(last) {
            return self.error(format!("duplicate key {last}"));
        }
        target.insert(last.clone(), value);
        Ok(())
    }
}

/// One build target a manifest declares (`[lib]`, `[[bin]]`, ...).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TargetDecl {
    pub name: Option<String>,
    pub path: Option<String>,
}

/// Which table a dependency was declared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DepKind {
    Normal,
    Dev,
    Build,
}

/// One dependency: the name the code uses and the package it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DepDecl {
    /// The dependency key (`serde`, `core_alias`).
    pub alias: String,
    /// The package (`package = "..."`, else the key).
    pub package: String,
    /// `workspace = true`: the package comes from `[workspace.dependencies]`.
    pub from_workspace: bool,
    pub kind: DepKind,
}

/// What the resolver needs from one `Cargo.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CargoManifest {
    /// `[package].name`; `None` for a virtual workspace manifest.
    pub package: Option<String>,
    pub lib: Option<TargetDecl>,
    pub bins: Vec<TargetDecl>,
    pub tests: Vec<TargetDecl>,
    pub examples: Vec<TargetDecl>,
    pub benches: Vec<TargetDecl>,
    /// `[package].build`: `Some(None)` when `build = false`.
    // Three-state field: omitted (`None`), disabled via `build = false` (`Some(None)`), or path.
    #[allow(clippy::option_option)]
    pub build: Option<Option<String>>,
    pub dependencies: Vec<DepDecl>,
    /// `[workspace.dependencies]`.
    pub workspace_dependencies: Vec<DepDecl>,
}

fn manifest_error<T>(message: impl Into<String>) -> Result<T, TomlError> {
    Err(TomlError {
        line: 0,
        message: message.into(),
    })
}

fn optional_string(table: &Table, key: &str, context: &str) -> Result<Option<String>, TomlError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Str(text)) => Ok(Some(text.clone())),
        Some(_) => manifest_error(format!("{context}.{key} is not a string")),
    }
}

fn target(value: &Value, context: &str) -> Result<TargetDecl, TomlError> {
    let Value::Table(table) = value else {
        return manifest_error(format!("{context} is not a table"));
    };
    Ok(TargetDecl {
        name: optional_string(table, "name", context)?,
        path: optional_string(table, "path", context)?,
    })
}

fn targets(root: &Table, key: &str) -> Result<Vec<TargetDecl>, TomlError> {
    match root.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|item| target(item, key)).collect(),
        Some(_) => manifest_error(format!("{key} is not an array of tables")),
    }
}

fn dependencies(table: &Table, kind: DepKind, out: &mut Vec<DepDecl>) -> Result<(), TomlError> {
    for (alias, spec) in table {
        let (package, from_workspace) = match spec {
            Value::Str(_) => (alias.clone(), false),
            Value::Table(fields) => (
                optional_string(fields, "package", alias)?.unwrap_or_else(|| alias.clone()),
                fields.get("workspace") == Some(&Value::Bool(true)),
            ),
            _ => {
                return manifest_error(format!(
                    "dependency {alias} is neither a version nor a table"
                ));
            }
        };
        out.push(DepDecl {
            alias: alias.clone(),
            package,
            from_workspace,
            kind,
        });
    }
    Ok(())
}

fn dependency_tables(table: &Table, out: &mut Vec<DepDecl>) -> Result<(), TomlError> {
    for (key, kind) in [
        ("dependencies", DepKind::Normal),
        ("dev-dependencies", DepKind::Dev),
        ("dev_dependencies", DepKind::Dev),
        ("build-dependencies", DepKind::Build),
        ("build_dependencies", DepKind::Build),
    ] {
        match table.get(key) {
            None => {}
            Some(Value::Table(deps)) => dependencies(deps, kind, out)?,
            Some(_) => return manifest_error(format!("{key} is not a table")),
        }
    }
    Ok(())
}

/// Read a `Cargo.toml`.
pub(crate) fn read_cargo(text: &str) -> Result<CargoManifest, TomlError> {
    let root = parse_toml(text)?;
    let mut manifest = CargoManifest::default();
    match root.get("package") {
        None => {}
        Some(Value::Table(package)) => {
            match package.get("name") {
                Some(Value::Str(name)) if !name.is_empty() => {
                    manifest.package = Some(name.clone());
                }
                _ => return manifest_error("[package] has no name string"),
            }
            manifest.build = match package.get("build") {
                None => None,
                Some(Value::Str(path)) => Some(Some(path.clone())),
                Some(Value::Bool(false)) => Some(None),
                Some(Value::Bool(true)) => Some(Some("build.rs".to_string())),
                Some(_) => return manifest_error("package.build is neither a path nor a boolean"),
            };
        }
        Some(_) => return manifest_error("package is not a table"),
    }
    manifest.lib = root.get("lib").map(|lib| target(lib, "lib")).transpose()?;
    manifest.bins = targets(&root, "bin")?;
    manifest.tests = targets(&root, "test")?;
    manifest.examples = targets(&root, "example")?;
    manifest.benches = targets(&root, "bench")?;
    dependency_tables(&root, &mut manifest.dependencies)?;
    match root.get("target") {
        None => {}
        Some(Value::Table(platforms)) => {
            for spec in platforms.values() {
                let Value::Table(spec) = spec else {
                    return manifest_error("a target.<cfg> entry is not a table");
                };
                dependency_tables(spec, &mut manifest.dependencies)?;
            }
        }
        Some(_) => return manifest_error("target is not a table"),
    }
    if let Some(Value::Table(workspace)) = root.get("workspace")
        && let Some(Value::Table(deps)) = workspace.get("dependencies")
    {
        dependencies(deps, DepKind::Normal, &mut manifest.workspace_dependencies)?;
    }
    Ok(manifest)
}

// ---------------------------------------------------------------------
// JavaScript / TypeScript manifests: `package.json`, `tsconfig.json`,
// `jsconfig.json`. A small JSON reader (no `serde_json` in this crate's
// dependencies: none is added), and a comment-stripping pre-pass for the
// JSON-with-comments that `tsconfig.json` allows.

/// Deepest nesting a JSON manifest may have; a deeper one is refused.
pub(crate) const MAX_JSON_DEPTH: usize = 128;

/// One JSON value. Objects keep their keys in written order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    /// A number, as written.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of `key` in an object (the last one written wins).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Obj(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl JsonParser<'_> {
    fn error<T>(&self, message: &str) -> Result<T, String> {
        let line = self.bytes[..self.at.min(self.bytes.len())]
            .split(|b| *b == b'\n')
            .count();
        Err(format!("line {line}: {message}"))
    }

    fn skip_space(&mut self) {
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_JSON_DEPTH {
            return self.error(&format!("nested past {MAX_JSON_DEPTH} levels"));
        }
        self.skip_space();
        match self.bytes.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut entries = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(Json::Obj(entries));
                }
                loop {
                    self.skip_space();
                    if self.bytes.get(self.at) != Some(&b'"') {
                        return self.error("expected a string key");
                    }
                    let key = self.string()?;
                    self.skip_space();
                    if self.bytes.get(self.at) != Some(&b':') {
                        return self.error("expected `:`");
                    }
                    self.at += 1;
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Json::Obj(entries));
                        }
                        _ => return self.error("expected `,` or `}`"),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Json::Arr(items));
                        }
                        _ => return self.error("expected `,` or `]`"),
                    }
                }
            }
            Some(b'"') => self.string().map(Json::Str),
            Some(_) => {
                let start = self.at;
                while self.at < self.bytes.len()
                    && (self.bytes[self.at].is_ascii_alphanumeric()
                        || matches!(self.bytes[self.at], b'-' | b'+' | b'.'))
                {
                    self.at += 1;
                }
                let word = std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or_default();
                match word {
                    "null" => Ok(Json::Null),
                    "true" => Ok(Json::Bool(true)),
                    "false" => Ok(Json::Bool(false)),
                    _ if is_json_number(word) => Ok(Json::Num(word.to_string())),
                    _ => {
                        self.at = start;
                        self.error("unexpected value")
                    }
                }
            }
            None => self.error("unexpected end"),
        }
    }

    /// Exactly four hex digits after `\u`.
    fn hex4(&mut self) -> Result<u32, String> {
        let Some(hex) = self
            .bytes
            .get(self.at..self.at + 4)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
        else {
            return self.error("a \\u escape needs four hex digits");
        };
        let code = hex.iter().fold(0u32, |code, digit| {
            code * 16 + char::from(*digit).to_digit(16).unwrap_or(0)
        });
        self.at += 4;
        Ok(code)
    }

    /// The character of a `\u` escape after its `u`: a UTF-16 surrogate
    /// pair (`\uD83D\uDE00`) is one character; a lone surrogate is U+FFFD.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let high = self.hex4()?;
        if (0xD800..0xDC00).contains(&high)
            && self.bytes.get(self.at..self.at + 2) == Some(b"\\u".as_slice())
        {
            let back = self.at;
            self.at += 2;
            let low = self.hex4()?;
            if (0xDC00..0xE000).contains(&low) {
                let code = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                return Ok(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            self.at = back;
        }
        Ok(char::from_u32(high).unwrap_or('\u{fffd}'))
    }

    /// A string from its opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&byte) = self.bytes.get(self.at) else {
                return self.error("unterminated string");
            };
            self.at += 1;
            match byte {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => {
                    let Some(&escape) = self.bytes.get(self.at) else {
                        return self.error("unterminated string");
                    };
                    self.at += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => out.push(escape),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => {
                            let c = self.unicode_escape()?;
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return self.error("invalid escape in string"),
                    }
                }
                byte if byte < 0x20 => return self.error("control character in string"),
                other => out.push(other),
            }
        }
    }
}

/// The JSON number grammar: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
fn is_json_number(word: &str) -> bool {
    let bytes = word.strip_prefix('-').unwrap_or(word).as_bytes();
    let digits = |at: usize| {
        bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let int = digits(0);
    if int == 0 || (int > 1 && bytes[0] == b'0') {
        return false;
    }
    let mut at = int;
    if bytes.get(at) == Some(&b'.') {
        let frac = digits(at + 1);
        if frac == 0 {
            return false;
        }
        at += 1 + frac;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exp = digits(at);
        if exp == 0 {
            return false;
        }
        at += exp;
    }
    at == bytes.len()
}

/// Parse strict JSON (RFC 8259: what `package.json` must be).
pub(crate) fn parse_json(text: &str) -> Result<Json, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = JsonParser {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = parser.value(0)?;
    parser.skip_space();
    if parser.at < parser.bytes.len() {
        return parser.error("text after the value");
    }
    Ok(value)
}

/// JSON with comments and trailing commas made JSON: comments become
/// spaces (newlines kept, so lines still count), and a `,` before `}` or
/// `]` is dropped. Strings are left alone.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'"' {
            out.push(byte);
            at += 1;
            while at < bytes.len() {
                let inner = bytes[at];
                out.push(inner);
                at += 1;
                if inner == b'\\' {
                    if let Some(&next) = bytes.get(at) {
                        out.push(next);
                        at += 1;
                    }
                } else if inner == b'"' || inner == b'\n' {
                    break;
                }
            }
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                out.push(b' ');
                at += 1;
            }
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            out.extend_from_slice(b"  ");
            at += 2;
            while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                out.push(if bytes[at] == b'\n' { b'\n' } else { b' ' });
                at += 1;
            }
            if at < bytes.len() {
                out.extend_from_slice(b"  ");
                at += 2;
            }
        } else if byte == b'}' || byte == b']' {
            // Drop a trailing comma before the closer.
            let mut back = out.len();
            while back > 0 && out[back - 1].is_ascii_whitespace() {
                back -= 1;
            }
            if back > 0 && out[back - 1] == b',' {
                out[back - 1] = b' ';
            }
            out.push(byte);
            at += 1;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the resolver needs from one `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PackageJson {
    pub name: Option<String>,
    /// `exports`, as written.
    pub exports: Option<Json>,
    /// `imports` (`#name` specifiers), as written.
    pub imports: Option<Json>,
    pub main: Option<String>,
    pub module: Option<String>,
    /// `types`, else `typings`.
    pub types: Option<String>,
    /// `bin` targets (a string, or each value of an object).
    pub bin: Vec<String>,
    /// Every package named in `dependencies`, `devDependencies`,
    /// `peerDependencies` and `optionalDependencies`.
    pub dependencies: Vec<String>,
}

fn json_string(value: &Json, key: &str) -> Option<String> {
    value.get(key).and_then(Json::as_str).map(str::to_string)
}

/// Read a `package.json`.
pub(crate) fn read_package_json(text: &str) -> Result<PackageJson, String> {
    let value = parse_json(text)?;
    if !matches!(value, Json::Obj(_)) {
        return Err("not a JSON object".to_string());
    }
    let bin = match value.get("bin") {
        Some(Json::Str(path)) => vec![path.clone()],
        Some(Json::Obj(entries)) => entries
            .iter()
            .filter_map(|(_, path)| path.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let mut dependencies = Vec::new();
    for table in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(Json::Obj(entries)) = value.get(table) {
            dependencies.extend(entries.iter().map(|(name, _)| name.clone()));
        }
    }
    dependencies.sort();
    dependencies.dedup();
    Ok(PackageJson {
        name: json_string(&value, "name").filter(|name| !name.is_empty()),
        exports: value.get("exports").cloned(),
        imports: value.get("imports").cloned(),
        main: json_string(&value, "main"),
        module: json_string(&value, "module"),
        types: json_string(&value, "types").or_else(|| json_string(&value, "typings")),
        bin,
        dependencies,
    })
}

/// One `compilerOptions.paths` entry: pattern and its ordered target list.
pub(crate) type TsPathEntry = (String, Vec<String>);

/// Indexed `compilerOptions.paths` mappings preserving written order.
///
/// Shared via [`Arc`] across `extends` chains so deep directory hierarchies do
/// not copy inherited alias tables while keeping specifier lookup sublinear
/// for exact patterns.
#[derive(Debug, Clone, Default)]
pub(crate) struct TsPathMap {
    entries: Vec<TsPathEntry>,
    /// First written entry index for each exact pattern.
    exact: BTreeMap<String, usize>,
    /// Single-`*` wildcard rules `(prefix_len, entry_idx)` in written order.
    wildcards: Vec<(usize, usize)>,
}

impl PartialEq for TsPathMap {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for TsPathMap {}

impl TsPathMap {
    /// Build an indexed path map from entries in written order.
    pub(crate) fn from_entries(entries: Vec<TsPathEntry>) -> Self {
        let mut exact = BTreeMap::new();
        let mut wildcards = Vec::new();
        for (idx, (pattern, _)) in entries.iter().enumerate() {
            exact.entry(pattern.clone()).or_insert(idx);
            if let Some((prefix, suffix)) = pattern.split_once('*')
                && !suffix.contains('*')
            {
                wildcards.push((prefix.len(), idx));
            }
        }
        Self {
            entries,
            exact,
            wildcards,
        }
    }

    /// Returns `true` when the map contains zero path entries.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Underlying `(pattern, targets)` slice in written order.
    #[cfg(test)]
    pub(crate) fn as_entries(&self) -> &[TsPathEntry] {
        &self.entries
    }

    /// Match a bare specifier against `compilerOptions.paths`.
    ///
    /// Preserves TypeScript / original resolver precedence: exact pattern
    /// matches have rank `usize::MAX` (first occurrence wins), otherwise the
    /// matching wildcard with the longest prefix wins (first written wins ties).
    pub(crate) fn match_specifier<'a>(
        &'a self,
        spec: &'a str,
    ) -> Option<(usize, &'a [String], &'a str)> {
        if let Some(&idx) = self.exact.get(spec) {
            return Some((usize::MAX, &self.entries[idx].1, ""));
        }
        let mut best: Option<(usize, &'a [String], &'a str)> = None;
        for &(rank, idx) in &self.wildcards {
            let (pattern, targets) = &self.entries[idx];
            let Some((prefix, suffix)) = pattern.split_once('*') else {
                continue;
            };
            if spec.len() < prefix.len() + suffix.len() {
                continue;
            }
            let Some(star) = spec
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix))
            else {
                continue;
            };
            if best.is_none_or(|(best_rank, _, _)| rank > best_rank) {
                best = Some((rank, targets, star));
            }
        }
        best
    }
}

/// What the resolver needs from one `tsconfig.json` / `jsconfig.json`,
/// before its `extends` chain is merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TsConfig {
    /// `extends` (a string or, since TypeScript 5, an array), as written.
    pub extends: Vec<String>,
    pub base_url: Option<String>,
    /// `compilerOptions.paths`: indexed pattern-to-targets map in written
    /// order; `None` when the file does not set it. Shared via [`Arc`] so deep
    /// `extends` chains do not clone inherited path maps per level.
    pub paths: Option<Arc<TsPathMap>>,
}

/// Read a `tsconfig.json` or `jsconfig.json` (comments and trailing commas
/// allowed).
pub(crate) fn read_tsconfig(text: &str) -> Result<TsConfig, String> {
    let value = parse_json(&strip_jsonc(text))?;
    if !matches!(value, Json::Obj(_)) {
        return Err("not a JSON object".to_string());
    }
    let extends = match value.get("extends") {
        Some(Json::Str(path)) => vec![path.clone()],
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let options = value.get("compilerOptions");
    let base_url = options.and_then(|options| json_string(options, "baseUrl"));
    let paths = match options.and_then(|options| options.get("paths")) {
        Some(Json::Obj(entries)) => {
            let collected: Vec<TsPathEntry> = entries
                .iter()
                .map(|(pattern, targets)| {
                    let targets = match targets {
                        Json::Arr(items) => items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect(),
                        Json::Str(target) => vec![target.clone()],
                        _ => Vec::new(),
                    };
                    (pattern.clone(), targets)
                })
                .collect();
            Some(Arc::new(TsPathMap::from_entries(collected)))
        }
        _ => None,
    };
    Ok(TsConfig {
        extends,
        base_url,
        paths,
    })
}

// ---------------------------------------------------------------------
// Python manifests: `pyproject.toml`, read with the built-in strict TOML
// subset parser above (`parse_toml`).

/// What the resolver needs from one `pyproject.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PyProjectManifest {
    /// PEP 503 normalized package name (`[project].name`, else `[tool.poetry].name`).
    pub name: Option<String>,
    /// Candidate source roots relative to the manifest directory (e.g. `"src"`).
    pub source_roots: Vec<String>,
    /// Explicit package-name to directory overrides (`pkg` -> `"custom/pkg"`).
    pub package_dirs: Vec<(String, String)>,
    /// Entry-point module paths named by `[project.scripts]`,
    /// `[project.gui-scripts]`, and `[tool.poetry.scripts]`.
    pub script_modules: Vec<String>,
    /// PEP 503 normalized dependency distribution names.
    pub dependencies: Vec<String>,
}

/// Normalize a Python distribution name per PEP 503: lowercase ASCII and
/// collapse runs of `[-_.]` to a single `-`.
pub(crate) fn normalize_py_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !out.is_empty() && !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

fn pep508_dep_name(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(trimmed.len());
    let name = normalize_py_name(&trimmed[..end]);
    (!name.is_empty()).then_some(name)
}

fn push_unique_str(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn push_script_targets(table: &Table, out: &mut Vec<String>) {
    for val in table.values() {
        let target = match val {
            Value::Str(s) => Some(s.as_str()),
            Value::Table(inner) => inner.get("callable").and_then(|v| match v {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            }),
            _ => None,
        };
        if let Some(target) = target {
            let module = target.split(':').next().unwrap_or(target).trim();
            if !module.is_empty() {
                push_unique_str(out, module.to_string());
            }
        }
    }
}

fn push_pep508_array(items: &[Value], out: &mut Vec<String>) {
    for item in items {
        if let Value::Str(spec) = item
            && let Some(name) = pep508_dep_name(spec)
        {
            push_unique_str(out, name);
        }
    }
}

fn clean_rel_dir(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed == "." {
        String::new()
    } else {
        trimmed.to_string()
    }
}

fn read_pyproject_project(root: &Table, manifest: &mut PyProjectManifest) {
    if let Some(Value::Table(project)) = root.get("project") {
        if let Some(Value::Str(name)) = project.get("name") {
            let norm = normalize_py_name(name);
            if !norm.is_empty() {
                manifest.name = Some(norm);
            }
        }
        if let Some(Value::Array(deps)) = project.get("dependencies") {
            push_pep508_array(deps, &mut manifest.dependencies);
        }
        if let Some(Value::Table(opt)) = project.get("optional-dependencies") {
            for val in opt.values() {
                if let Value::Array(deps) = val {
                    push_pep508_array(deps, &mut manifest.dependencies);
                }
            }
        }
        for key in ["scripts", "gui-scripts"] {
            if let Some(Value::Table(scripts)) = project.get(key) {
                push_script_targets(scripts, &mut manifest.script_modules);
            }
        }
    }

    if let Some(Value::Table(groups)) = root.get("dependency-groups") {
        for val in groups.values() {
            if let Value::Array(items) = val {
                push_pep508_array(items, &mut manifest.dependencies);
            }
        }
    }
}

fn read_pyproject_setuptools(tool: &Table, manifest: &mut PyProjectManifest) {
    let Some(Value::Table(setuptools)) = tool.get("setuptools") else {
        return;
    };
    if let Some(Value::Table(pkg_dir)) = setuptools.get("package-dir") {
        for (pkg, dir_val) in pkg_dir {
            let Value::Str(dir_str) = dir_val else {
                continue;
            };
            let cleaned = clean_rel_dir(dir_str);
            if pkg.is_empty() || pkg == "." {
                if !cleaned.is_empty() {
                    push_unique_str(&mut manifest.source_roots, cleaned);
                }
            } else if !cleaned.is_empty() {
                manifest.package_dirs.push((pkg.clone(), cleaned));
            }
        }
    }
    if let Some(Value::Table(packages)) = setuptools.get("packages")
        && let Some(Value::Table(find)) = packages.get("find")
    {
        match find.get("where") {
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::Str(dir_str) = item {
                        let cleaned = clean_rel_dir(dir_str);
                        if !cleaned.is_empty() {
                            push_unique_str(&mut manifest.source_roots, cleaned);
                        }
                    }
                }
            }
            Some(Value::Str(dir_str)) => {
                let cleaned = clean_rel_dir(dir_str);
                if !cleaned.is_empty() {
                    push_unique_str(&mut manifest.source_roots, cleaned);
                }
            }
            _ => {}
        }
    }
}

fn read_pyproject_poetry(tool: &Table, manifest: &mut PyProjectManifest) {
    let Some(Value::Table(poetry)) = tool.get("poetry") else {
        return;
    };
    if manifest.name.is_none()
        && let Some(Value::Str(name)) = poetry.get("name")
    {
        let norm = normalize_py_name(name);
        if !norm.is_empty() {
            manifest.name = Some(norm);
        }
    }
    if let Some(Value::Array(packages)) = poetry.get("packages") {
        for pkg in packages {
            let Value::Table(entry) = pkg else {
                continue;
            };
            let from_dir = entry
                .get("from")
                .and_then(|v| match v {
                    Value::Str(s) => Some(clean_rel_dir(s)),
                    _ => None,
                })
                .filter(|s| !s.is_empty());
            if let Some(ref from_dir) = from_dir {
                push_unique_str(&mut manifest.source_roots, from_dir.clone());
            }
            if let Some(Value::Str(inc)) = entry.get("include")
                && !inc.contains('*')
                && !inc.contains('?')
            {
                let cleaned_inc = clean_rel_dir(inc);
                if !cleaned_inc.is_empty() {
                    let pkg_name = cleaned_inc.replace('/', ".");
                    let rel_dir = match from_dir {
                        Some(from_dir) => format!("{from_dir}/{cleaned_inc}"),
                        None => cleaned_inc,
                    };
                    manifest.package_dirs.push((pkg_name, rel_dir));
                }
            }
        }
    }
    for key in ["dependencies", "dev-dependencies"] {
        if let Some(Value::Table(deps)) = poetry.get(key) {
            for dep_key in deps.keys() {
                let norm = normalize_py_name(dep_key);
                if !norm.is_empty() && norm != "python" {
                    push_unique_str(&mut manifest.dependencies, norm);
                }
            }
        }
    }
    if let Some(Value::Table(groups)) = poetry.get("group") {
        for group in groups.values() {
            if let Value::Table(g_table) = group
                && let Some(Value::Table(deps)) = g_table.get("dependencies")
            {
                for dep_key in deps.keys() {
                    let norm = normalize_py_name(dep_key);
                    if !norm.is_empty() && norm != "python" {
                        push_unique_str(&mut manifest.dependencies, norm);
                    }
                }
            }
        }
    }
    if let Some(Value::Table(scripts)) = poetry.get("scripts") {
        push_script_targets(scripts, &mut manifest.script_modules);
    }
}

fn read_hatch_sources(table: &Table, manifest: &mut PyProjectManifest) {
    match table.get("sources") {
        Some(Value::Array(items)) => {
            for item in items {
                if let Value::Str(s) = item {
                    let cleaned = clean_rel_dir(s);
                    if !cleaned.is_empty() {
                        push_unique_str(&mut manifest.source_roots, cleaned);
                    }
                }
            }
        }
        Some(Value::Table(map)) => {
            for (src_dir, dst_pkg) in map {
                let cleaned_src = clean_rel_dir(src_dir);
                let Value::Str(dst_str) = dst_pkg else {
                    continue;
                };
                let cleaned_dst = clean_rel_dir(dst_str);
                if cleaned_dst.is_empty() && !cleaned_src.is_empty() {
                    push_unique_str(&mut manifest.source_roots, cleaned_src);
                } else if !cleaned_dst.is_empty() && !cleaned_src.is_empty() {
                    manifest
                        .package_dirs
                        .push((cleaned_dst.replace('/', "."), cleaned_src));
                }
            }
        }
        _ => {}
    }
}

fn read_pyproject_hatch(tool: &Table, manifest: &mut PyProjectManifest) {
    let Some(Value::Table(hatch)) = tool.get("hatch") else {
        return;
    };
    let Some(Value::Table(build)) = hatch.get("build") else {
        return;
    };
    read_hatch_sources(build, manifest);
    if let Some(Value::Table(targets)) = build.get("targets")
        && let Some(Value::Table(wheel)) = targets.get("wheel")
    {
        read_hatch_sources(wheel, manifest);
        if let Some(Value::Array(packages)) = wheel.get("packages") {
            for item in packages {
                if let Value::Str(p) = item {
                    let cleaned = clean_rel_dir(p);
                    if !cleaned.is_empty()
                        && let Some((parent, pkg)) = cleaned.rsplit_once('/')
                    {
                        if !parent.is_empty() {
                            push_unique_str(&mut manifest.source_roots, parent.to_string());
                        }
                        manifest.package_dirs.push((pkg.to_string(), cleaned));
                    }
                }
            }
        }
    }
}

/// Read a `pyproject.toml`.
pub(crate) fn read_pyproject(text: &str) -> Result<PyProjectManifest, TomlError> {
    let root = parse_toml(text)?;
    let mut manifest = PyProjectManifest::default();
    read_pyproject_project(&root, &mut manifest);
    if let Some(Value::Table(tool)) = root.get("tool") {
        read_pyproject_setuptools(tool, &mut manifest);
        read_pyproject_poetry(tool, &mut manifest);
        read_pyproject_hatch(tool, &mut manifest);
    }
    manifest.dependencies.sort();
    manifest.dependencies.dedup();
    Ok(manifest)
}

/// One local path `replace` directive from a `go.mod` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoModReplace {
    /// Replaced module import path on the left-hand side of `=>`.
    pub old_path: String,
    /// Relative filesystem directory path on the right-hand side of `=>`.
    pub new_local_path: String,
}

/// Parsed module metadata, `require` targets, and local path `replace` rules
/// from a `go.mod` manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GoModManifest {
    /// Canonical module import path declared by `module <path>`.
    pub module_path: Option<String>,
    /// Sorted, deduplicated required module paths from `require` directives.
    pub requires: Vec<String>,
    /// Deterministic local path replacements (`old_path => ./local`).
    pub local_replaces: Vec<GoModReplace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GoModTok {
    Word(String, u32),
    LParen(u32),
    RParen(u32),
    Newline,
}

fn lex_go_mod_quoted(chars: &[char], idx: &mut usize, line: u32) -> Result<String, TomlError> {
    *idx += 1;
    let mut out = String::new();
    while let Some(&ch) = chars.get(*idx) {
        *idx += 1;
        match ch {
            '"' => return Ok(out),
            '\n' => {
                return Err(TomlError {
                    line,
                    message: "unterminated string in go.mod".to_string(),
                });
            }
            '\\' => {
                let Some(&esc) = chars.get(*idx) else {
                    return Err(TomlError {
                        line,
                        message: "unterminated escape in go.mod string".to_string(),
                    });
                };
                *idx += 1;
                match esc {
                    '"' | '\\' => out.push(esc),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            other => out.push(other),
        }
    }
    Err(TomlError {
        line,
        message: "unterminated string in go.mod".to_string(),
    })
}

fn lex_go_mod_raw(chars: &[char], idx: &mut usize, line: &mut u32) -> Result<String, TomlError> {
    let start_line = *line;
    *idx += 1;
    let mut out = String::new();
    while let Some(&ch) = chars.get(*idx) {
        *idx += 1;
        match ch {
            '`' => return Ok(out),
            '\r' => {}
            '\n' => {
                *line += 1;
                out.push('\n');
            }
            other => out.push(other),
        }
    }
    Err(TomlError {
        line: start_line,
        message: "unterminated raw string in go.mod".to_string(),
    })
}

fn lex_go_mod_tokens(text: &str) -> Result<Vec<GoModTok>, TomlError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut idx = 0usize;
    let mut line = 1u32;
    while let Some(&ch) = chars.get(idx) {
        match ch {
            ' ' | '\t' | '\r' => idx += 1,
            '\n' => {
                tokens.push(GoModTok::Newline);
                idx += 1;
                line += 1;
            }
            '/' if chars.get(idx + 1) == Some(&'/') => {
                while chars.get(idx).is_some_and(|c| *c != '\n') {
                    idx += 1;
                }
            }
            '(' => {
                tokens.push(GoModTok::LParen(line));
                idx += 1;
            }
            ')' => {
                tokens.push(GoModTok::RParen(line));
                idx += 1;
            }
            '"' => {
                let s = lex_go_mod_quoted(&chars, &mut idx, line)?;
                tokens.push(GoModTok::Word(s, line));
            }
            '`' => {
                let tok_line = line;
                let s = lex_go_mod_raw(&chars, &mut idx, &mut line)?;
                tokens.push(GoModTok::Word(s, tok_line));
            }
            '=' if chars.get(idx + 1) == Some(&'>') => {
                tokens.push(GoModTok::Word("=>".to_string(), line));
                idx += 2;
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.get(idx) {
                    if matches!(c, ' ' | '\t' | '\r' | '\n' | '(' | ')' | '"' | '`')
                        || (c == '/' && chars.get(idx + 1) == Some(&'/'))
                        || (c == '=' && chars.get(idx + 1) == Some(&'>'))
                    {
                        break;
                    }
                    word.push(c);
                    idx += 1;
                }
                tokens.push(GoModTok::Word(word, line));
            }
        }
    }
    Ok(tokens)
}

fn collect_go_mod_block(
    tokens: &[GoModTok],
    idx: &mut usize,
    open_line: u32,
) -> Result<Vec<(u32, Vec<String>)>, TomlError> {
    let mut entries = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_line = open_line;
    while let Some(tok) = tokens.get(*idx) {
        *idx += 1;
        match tok {
            GoModTok::Word(word, ln) => {
                if current.is_empty() {
                    current_line = *ln;
                }
                current.push(word.clone());
            }
            GoModTok::Newline => {
                if !current.is_empty() {
                    entries.push((current_line, std::mem::take(&mut current)));
                }
            }
            GoModTok::RParen(_) => {
                if !current.is_empty() {
                    entries.push((current_line, current));
                }
                return Ok(entries);
            }
            GoModTok::LParen(ln) => {
                return Err(TomlError {
                    line: *ln,
                    message: "unexpected '(' inside go.mod block".to_string(),
                });
            }
        }
    }
    Err(TomlError {
        line: open_line,
        message: "unterminated '(' block in go.mod".to_string(),
    })
}

fn is_local_go_replace_target(target: &str) -> bool {
    !target.is_empty() && !target.starts_with('/') && !target.contains(':')
}

fn apply_go_mod_replace(
    line: u32,
    args: &[String],
    replaces: &mut BTreeMap<String, String>,
) -> Result<(), TomlError> {
    let Some(arrow) = args.iter().position(|tok| tok == "=>") else {
        return Err(TomlError {
            line,
            message: "go.mod replace directive missing '=>'".to_string(),
        });
    };
    let lhs = &args[..arrow];
    let rhs = &args[arrow + 1..];
    if !matches!(lhs.len(), 1 | 2) || !matches!(rhs.len(), 1 | 2) {
        return Err(TomlError {
            line,
            message: "invalid go.mod replace syntax".to_string(),
        });
    }
    let old_path = &lhs[0];
    let new_target = &rhs[0];
    if old_path.is_empty() || new_target.is_empty() {
        return Err(TomlError {
            line,
            message: "empty path in go.mod replace".to_string(),
        });
    }
    // Local filesystem directory replacements never take a version token.
    if rhs.len() == 1 && is_local_go_replace_target(new_target) {
        replaces.insert(old_path.clone(), new_target.clone());
    }
    Ok(())
}

fn apply_go_mod_directive(
    verb: &str,
    entries: Vec<(u32, Vec<String>)>,
    manifest: &mut GoModManifest,
    replaces: &mut BTreeMap<String, String>,
) -> Result<(), TomlError> {
    match verb {
        "module" => {
            for (line, args) in entries {
                if args.len() != 1 || args[0].is_empty() {
                    return Err(TomlError {
                        line,
                        message: "go.mod module directive requires exactly one module path"
                            .to_string(),
                    });
                }
                if manifest.module_path.is_some() {
                    return Err(TomlError {
                        line,
                        message: "duplicate module directive in go.mod".to_string(),
                    });
                }
                manifest.module_path = Some(args[0].clone());
            }
        }
        "require" => {
            for (line, args) in entries {
                if !matches!(args.len(), 1 | 2) || args[0].is_empty() {
                    return Err(TomlError {
                        line,
                        message: "invalid go.mod require entry".to_string(),
                    });
                }
                manifest.requires.push(args[0].clone());
            }
        }
        "replace" => {
            for (line, args) in entries {
                apply_go_mod_replace(line, &args, replaces)?;
            }
        }
        "go" | "toolchain" | "exclude" | "retract" | "tool" | "ignore" | "godebug" => {}
        _ => {
            let line = entries.first().map_or(0, |(ln, _)| *ln);
            return Err(TomlError {
                line,
                message: format!("unsupported go.mod directive {verb:?}"),
            });
        }
    }
    Ok(())
}

fn collect_go_mod_inline(
    tokens: &[GoModTok],
    idx: &mut usize,
    start_line: u32,
) -> Result<Vec<(u32, Vec<String>)>, TomlError> {
    let mut args = Vec::new();
    while let Some(next) = tokens.get(*idx) {
        match next {
            GoModTok::Newline => {
                *idx += 1;
                break;
            }
            GoModTok::Word(w, _) => {
                args.push(w.clone());
                *idx += 1;
            }
            GoModTok::LParen(ln) | GoModTok::RParen(ln) => {
                return Err(TomlError {
                    line: *ln,
                    message: "unexpected parenthesis in go.mod directive".to_string(),
                });
            }
        }
    }
    if args.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![(start_line, args)])
    }
}

/// Read a `go.mod` manifest (zero external crates).
pub(crate) fn read_go_mod(text: &str) -> Result<GoModManifest, TomlError> {
    let tokens = lex_go_mod_tokens(text)?;
    let mut manifest = GoModManifest::default();
    let mut replaces = BTreeMap::new();
    let mut idx = 0usize;

    while let Some(tok) = tokens.get(idx) {
        let (verb, start_line) = match tok {
            GoModTok::Newline => {
                idx += 1;
                continue;
            }
            GoModTok::Word(verb, ln) => {
                idx += 1;
                (verb.clone(), *ln)
            }
            GoModTok::LParen(ln) | GoModTok::RParen(ln) => {
                return Err(TomlError {
                    line: *ln,
                    message: "unexpected parenthesis in go.mod".to_string(),
                });
            }
        };

        let entries = match tokens.get(idx) {
            Some(GoModTok::LParen(open_line)) => {
                let open = *open_line;
                idx += 1;
                collect_go_mod_block(&tokens, &mut idx, open)?
            }
            _ => collect_go_mod_inline(&tokens, &mut idx, start_line)?,
        };

        if entries.is_empty() && verb == "module" {
            return Err(TomlError {
                line: start_line,
                message: "go.mod module directive requires a module path".to_string(),
            });
        }
        apply_go_mod_directive(&verb, entries, &mut manifest, &mut replaces)?;
    }

    if manifest.module_path.is_none() {
        return Err(TomlError {
            line: 0,
            message: "go.mod missing module directive".to_string(),
        });
    }
    manifest.requires.sort();
    manifest.requires.dedup();
    manifest.local_replaces = replaces
        .into_iter()
        .map(|(old_path, new_local_path)| GoModReplace {
            old_path,
            new_local_path,
        })
        .collect();
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tables_arrays_of_tables_dotted_keys_and_inline_tables() {
        let text = r#"
# a comment
[package]
name = "fx-app" # trailing comment
version.workspace = true
description = """
Two lines \
  joined."""
build = false

[[bin]]
name = "fx"
path = 'src/bin/fx.rs'

[[bin]]
name = "other"

[dependencies]
fx-core = { path = "../core" }
alias = { package = "fx-util", version = "1", features = [
    "a", # inline comment
    "b",
] }
serde.workspace = true
anyhow = "1"

[target.'cfg(unix)'.dev-dependencies]
fx-test = { path = "../test" }
"#;
        let table = parse_toml(text).expect("parse");
        let Value::Table(package) = &table["package"] else {
            panic!("package");
        };
        assert_eq!(
            package["description"],
            Value::Str("Two lines joined.".into())
        );
        let manifest = read_cargo(text).expect("manifest");
        assert_eq!(manifest.package.as_deref(), Some("fx-app"));
        assert_eq!(manifest.build, Some(None));
        assert_eq!(manifest.bins.len(), 2);
        assert_eq!(manifest.bins[0].path.as_deref(), Some("src/bin/fx.rs"));
        let deps: Vec<_> = manifest
            .dependencies
            .iter()
            .map(|d| {
                (
                    d.alias.as_str(),
                    d.package.as_str(),
                    d.from_workspace,
                    d.kind,
                )
            })
            .collect();
        assert_eq!(
            deps,
            vec![
                ("alias", "fx-util", false, DepKind::Normal),
                ("anyhow", "anyhow", false, DepKind::Normal),
                ("fx-core", "fx-core", false, DepKind::Normal),
                ("serde", "serde", true, DepKind::Normal),
                ("fx-test", "fx-test", false, DepKind::Dev),
            ]
        );
    }

    #[test]
    fn reads_a_virtual_workspace_manifest() {
        let manifest = read_cargo(
            "[workspace]\nresolver = \"3\"\nmembers = [\n  \"crates/a\",\n]\n\n[workspace.dependencies]\na = { path = \"crates/a\" }\nb-alias = { package = \"b\", path = \"crates/b\" }\n\n[workspace.lints.clippy]\nall = { level = \"warn\", priority = -1 }\n",
        )
        .expect("workspace");
        assert_eq!(manifest.package, None);
        let deps: Vec<_> = manifest
            .workspace_dependencies
            .iter()
            .map(|d| (d.alias.as_str(), d.package.as_str()))
            .collect();
        assert_eq!(deps, vec![("a", "a"), ("b-alias", "b")]);
    }

    #[test]
    fn what_the_subset_does_not_read_is_an_error_with_its_line() {
        for (text, line, needle) in [
            (
                "[package]\nname = \"a\"\nname = \"b\"\n",
                3,
                "duplicate key",
            ),
            ("[package\nname = \"a\"\n", 1, "close a table header"),
            ("[package]\nname = \"a\" junk\n", 2, "after a value"),
            ("[package]\nname = \"a\n", 2, "unterminated"),
            ("[package]\nname = \"\\q\"\n", 2, "unsupported escape"),
            ("x = [1 2]\n", 1, "in an array"),
            // A key that ends its line: reported on that line.
            (
                "[package]\nname = \"a\"\nversion\nedition = \"2024\"\n",
                3,
                "expected = after a key",
            ),
            (
                "[dependencies]\na = { path = \"x\", version\n}\n",
                2,
                "expected = in an inline table",
            ),
        ] {
            let error = parse_toml(text).expect_err(text);
            assert_eq!(error.line, line, "{text}: {error}");
            assert!(error.message.contains(needle), "{text}: {error}");
        }
        let error = read_cargo("[package]\nversion = \"1\"\n").expect_err("no name");
        assert!(error.message.contains("no name"), "{error}");
        let error =
            read_cargo("[package]\nname = \"a\"\n[dependencies]\nb = 3\n").expect_err("dep");
        assert!(error.message.contains("dependency b"), "{error}");
    }

    #[test]
    fn json_reads_every_value_and_refuses_what_is_not_json() {
        let value = parse_json(
            "{ \"a\": [1, -2.5e3, true, false, null], \"b\": { \"c\": \"x\\\"\\u00e9\\n\" }, \"a\": 7 }",
        )
        .expect("json");
        assert_eq!(
            value.get("a"),
            Some(&Json::Num("7".to_string())),
            "last key wins"
        );
        assert_eq!(
            value
                .get("b")
                .and_then(|b| b.get("c"))
                .and_then(Json::as_str),
            Some("x\"\u{e9}\n")
        );
        for bad in [
            "",
            "{",
            "{\"a\" 1}",
            "[1,]",
            "{\"a\": 1,}",
            "nul",
            "{} x",
            "\"open",
            "{'a': 1}",
        ] {
            assert!(parse_json(bad).is_err(), "{bad:?}");
        }
        let deep = format!(
            "{}{}",
            "[".repeat(MAX_JSON_DEPTH + 5),
            "]".repeat(MAX_JSON_DEPTH + 5)
        );
        assert!(parse_json(&deep).expect_err("deep").contains("nested past"));
        let error = parse_json("{\n\n  \"a\": ?\n}").expect_err("line");
        assert!(error.starts_with("line 3:"), "{error}");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_stripped_but_strings_kept() {
        let text = "{\n  // a comment with \"quotes\"\n  \"url\": \"http://x/*y*/\", /* block\n comment */\n  \"list\": [1, 2,],\n}\n";
        let stripped = strip_jsonc(text);
        assert_eq!(stripped.lines().count(), text.lines().count(), "lines kept");
        let value = parse_json(&stripped).expect("json");
        assert_eq!(
            value.get("url").and_then(Json::as_str),
            Some("http://x/*y*/")
        );
        assert_eq!(
            value.get("list"),
            Some(&Json::Arr(vec![
                Json::Num("1".into()),
                Json::Num("2".into())
            ]))
        );
    }

    #[test]
    fn package_json_and_tsconfig_facts() {
        let package = read_package_json(
            r##"{ "name": "@a/b", "main": "./m.js", "module": "./m.mjs", "typings": "./t.d.ts",
                 "bin": { "x": "./x.js", "y": "./y.js" }, "exports": { ".": "./e.js" },
                 "imports": { "#i": "./i.js" },
                 "dependencies": { "z": "1" }, "devDependencies": { "c": "1", "z": "2" } }"##,
        )
        .expect("package");
        assert_eq!(package.name.as_deref(), Some("@a/b"));
        assert_eq!(package.types.as_deref(), Some("./t.d.ts"));
        assert_eq!(package.bin, vec!["./x.js", "./y.js"]);
        assert_eq!(package.dependencies, vec!["c", "z"]);
        assert!(package.exports.is_some() && package.imports.is_some());
        assert!(read_package_json("[]").is_err());
        assert_eq!(
            read_package_json("{ \"bin\": \"./cli.js\", \"name\": \"\" }").expect("bin"),
            PackageJson {
                bin: vec!["./cli.js".to_string()],
                ..PackageJson::default()
            }
        );

        let config = read_tsconfig(
            "{ // c\n \"extends\": [\"./a\", \"pkg/tsconfig\"],\n \"compilerOptions\": { \"baseUrl\": \"src\", \"paths\": { \"@/*\": [\"x/*\", \"y/*\",], \"one\": \"z\" } } }",
        )
        .expect("tsconfig");
        assert_eq!(config.extends, vec!["./a", "pkg/tsconfig"]);
        assert_eq!(config.base_url.as_deref(), Some("src"));
        assert_eq!(
            config.paths.as_deref().map(TsPathMap::as_entries),
            Some(
                [
                    (
                        "@/*".to_string(),
                        vec!["x/*".to_string(), "y/*".to_string()]
                    ),
                    ("one".to_string(), vec!["z".to_string()]),
                ]
                .as_slice()
            )
        );
        assert_eq!(read_tsconfig("{}").expect("empty"), TsConfig::default());
    }

    fn linear_path_match<'a>(
        entries: &'a [TsPathEntry],
        spec: &'a str,
    ) -> Option<(usize, &'a [String], &'a str)> {
        let mut best: Option<(usize, &'a [String], &'a str)> = None;
        for (pattern, targets) in entries {
            let candidate = if pattern == spec {
                Some((usize::MAX, ""))
            } else {
                pattern
                    .split_once('*')
                    .filter(|(_, suffix)| !suffix.contains('*'))
                    .and_then(|(prefix, suffix)| {
                        spec.len()
                            .ge(&(prefix.len() + suffix.len()))
                            .then(|| {
                                spec.strip_prefix(prefix)
                                    .and_then(|rest| rest.strip_suffix(suffix))
                                    .map(|star| (prefix.len(), star))
                            })
                            .flatten()
                    })
            };
            if let Some((rank, star)) = candidate
                && best.is_none_or(|(best_rank, _, _)| rank > best_rank)
            {
                best = Some((rank, targets.as_slice(), star));
            }
        }
        best
    }

    #[test]
    fn tsconfig_paths_arc_shares_and_indexes_large_maps() {
        let mut raw_paths: Vec<String> = (0..256)
            .map(|idx| format!("\"alias-{idx}\": [\"src/exact/{idx}.ts\"]"))
            .collect();
        // Equal-prefix wildcard tie: first written rule (`./first/*`) must win.
        raw_paths.push("\"@tie/*\": [\"src/first/*\"]".to_string());
        raw_paths.push("\"@tie/*\": [\"src/second/*\"]".to_string());
        raw_paths.push("\"*\": [\"src/fallback/*\"]".to_string());
        raw_paths.push("\"dup\": [\"src/dup-first.ts\"]".to_string());
        raw_paths.push("\"dup\": [\"src/dup-second.ts\"]".to_string());
        raw_paths.push("\"@pkg/exact/*\": [\"src/wildcard/*\"]".to_string());

        let text = format!(
            "{{ \"compilerOptions\": {{ \"baseUrl\": \".\", \"paths\": {{ {} }} }} }}",
            raw_paths.join(", ")
        );
        let config = read_tsconfig(&text).expect("large tsconfig");
        let paths_arc = config.paths.clone().expect("paths");
        let cloned_arc = Arc::clone(&paths_arc);
        assert!(
            Arc::ptr_eq(&paths_arc, &cloned_arc),
            "cloning inherited TsConfig.paths must share the same Arc allocation"
        );

        for spec in [
            "alias-0",
            "alias-127",
            "alias-255",
            "dup",
            "@tie/mod",
            "@pkg/exact/*",
            "@pkg/exact/item",
            "catchall",
        ] {
            assert_eq!(
                paths_arc.match_specifier(spec),
                linear_path_match(paths_arc.as_entries(), spec),
                "specifier={spec}"
            );
        }
        assert_eq!(
            paths_arc.match_specifier("dup").map(|(r, t, _)| (r, t)),
            Some((usize::MAX, &["src/dup-first.ts".to_string()][..]))
        );
        assert_eq!(
            paths_arc
                .match_specifier("@tie/mod")
                .map(|(_, t, star)| (t, star)),
            Some((&["src/first/*".to_string()][..], "mod"))
        );
        assert_eq!(
            paths_arc
                .match_specifier("catchall")
                .map(|(r, _, s)| (r, s)),
            Some((0, "catchall"))
        );
    }

    #[test]
    fn pyproject_reads_pep621_setuptools_poetry_and_hatch() {
        let setuptools = read_pyproject(
            r#"
[project]
name = "My_App.Core"
dependencies = [
    "fx_domain[fast]>=1.0; python_version >= '3.10'",
    "requests>=2.31",
]

[project.optional-dependencies]
dev = ["pytest>=8.0"]

[project.scripts]
myapp = "my_app.cli:main"

[tool.setuptools.package-dir]
"" = "src"
extra = "vendor/extra"

[tool.setuptools.packages.find]
where = ["src", "lib"]
"#,
        )
        .expect("setuptools pyproject");
        assert_eq!(setuptools.name.as_deref(), Some("my-app-core"));
        assert_eq!(setuptools.source_roots, vec!["src", "lib"]);
        assert_eq!(
            setuptools.package_dirs,
            vec![("extra".to_string(), "vendor/extra".to_string())]
        );
        assert_eq!(setuptools.script_modules, vec!["my_app.cli"]);
        assert_eq!(
            setuptools.dependencies,
            vec!["fx-domain", "pytest", "requests"]
        );

        let poetry = read_pyproject(
            r#"
[tool.poetry]
name = "Poetry_Svc"
packages = [{ include = "svc", from = "python" }]

[tool.poetry.dependencies]
python = "^3.11"
shared_pkg = { path = "../shared" }

[tool.poetry.group.test.dependencies]
pytest = "^8"

[tool.poetry.scripts]
run = "svc.app:run"
"#,
        )
        .expect("poetry pyproject");
        assert_eq!(poetry.name.as_deref(), Some("poetry-svc"));
        assert_eq!(poetry.source_roots, vec!["python"]);
        assert_eq!(
            poetry.package_dirs,
            vec![("svc".to_string(), "python/svc".to_string())]
        );
        assert_eq!(poetry.script_modules, vec!["svc.app"]);
        assert_eq!(poetry.dependencies, vec!["pytest", "shared-pkg"]);

        let hatch = read_pyproject(
            r#"
[project]
name = "hatch-pkg"

[tool.hatch.build.targets.wheel]
sources = ["src"]
packages = ["src/hatch_pkg"]
"#,
        )
        .expect("hatch pyproject");
        assert_eq!(hatch.name.as_deref(), Some("hatch-pkg"));
        assert_eq!(hatch.source_roots, vec!["src"]);
        assert_eq!(
            hatch.package_dirs,
            vec![("hatch_pkg".to_string(), "src/hatch_pkg".to_string())]
        );
    }

    #[test]
    fn json_decodes_surrogate_pairs_and_refuses_invalid_escapes_and_numbers() {
        let value = parse_json(r#"["😀", "é\/", "\ud83d", "\ude00x", "\ud83dA"]"#).expect("json");
        let Json::Arr(items) = value else {
            panic!("an array");
        };
        let texts: Vec<&str> = items.iter().filter_map(Json::as_str).collect();
        assert_eq!(
            texts,
            vec!["\u{1f600}", "\u{e9}/", "\u{fffd}", "\u{fffd}x", "\u{fffd}A"]
        );
        for bad in [
            r#""\u+041""#,
            r#""\u-041""#,
            r#""\u12""#,
            r#""\uZZZZ""#,
            r#""\x41""#,
            r#""\a""#,
            r#""\'""#,
            "\"tab\there\"",
        ] {
            assert!(parse_json(bad).is_err(), "{bad}");
        }
        for good in ["0", "-0", "12", "1.5", "-1.5e10", "2E-3", "3e+2"] {
            assert_eq!(parse_json(good), Ok(Json::Num(good.to_string())), "{good}");
        }
        for bad in [
            "01", "-", "1.", ".5", "1e", "1e+", "+1", "1.2.3", "0x10", "1-2", "Infinity",
        ] {
            assert!(parse_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn go_mod_reads_module_requires_and_local_replaces() {
        let text = r#"
// Top-level comment
module github.com/acme/svc // inline comment

go 1.26
toolchain go1.26.0

require (
    github.com/acme/shared v0.1.0 // local replace target
    example.com/ext v1.2.3 // indirect
)

require github.com/acme/single v0.2.0

replace (
    github.com/acme/shared => ../shared
    github.com/acme/withver v0.1.0 => "./libs/withver"
    example.com/ext v1.2.3 => example.com/fork v1.2.4
    example.com/abs => /opt/vendor/abs
)

replace github.com/acme/single => ../single

exclude example.com/bad v0.0.1
retract [v0.0.1, v0.0.2]
"#;
        let parsed = read_go_mod(text).expect("read_go_mod");
        assert_eq!(parsed.module_path.as_deref(), Some("github.com/acme/svc"));
        assert_eq!(
            parsed.requires,
            vec![
                "example.com/ext",
                "github.com/acme/shared",
                "github.com/acme/single",
            ]
        );
        assert_eq!(
            parsed.local_replaces,
            vec![
                GoModReplace {
                    old_path: "github.com/acme/shared".to_string(),
                    new_local_path: "../shared".to_string(),
                },
                GoModReplace {
                    old_path: "github.com/acme/single".to_string(),
                    new_local_path: "../single".to_string(),
                },
                GoModReplace {
                    old_path: "github.com/acme/withver".to_string(),
                    new_local_path: "./libs/withver".to_string(),
                },
            ]
        );
    }

    #[test]
    fn go_mod_rejects_malformed_or_missing_module_directives() {
        assert!(read_go_mod("go 1.26\n").is_err());
        assert!(read_go_mod("module a\nmodule b\n").is_err());
        assert!(read_go_mod("module a\nrequire (\n  x v1.0.0\n").is_err());
        assert!(read_go_mod("module a\nreplace x ./bad\n").is_err());
        assert!(read_go_mod("module \"unterminated\n").is_err());
        assert!(read_go_mod("module a\nunknown_kw x\n").is_err());
    }
}
