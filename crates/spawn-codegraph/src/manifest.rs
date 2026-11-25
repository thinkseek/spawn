//! A strict reader for the subset of TOML that `Cargo.toml` files use, and
//! the manifest facts the resolver needs.
//!
//! No `toml` crate: the gate builds offline and `toml` is not in Spawn's
//! `Cargo.lock`. The subset is tables, arrays of tables, dotted and quoted
//! keys, basic and literal strings (single and multi-line), arrays, inline
//! tables, booleans, and bare scalars (numbers, dates) kept as raw text.
//! Anything else is an error with its line: the caller turns it into a
//! visible `manifest_unsupported` diagnostic, never a guess.

use std::collections::BTreeMap;

/// One TOML value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Bool(bool),
    /// A number, date or other bare scalar, as written.
    Raw(String),
    Array(Vec<Value>),
    Table(Table),
}

/// A TOML table.
pub type Table = BTreeMap<String, Value>;

/// Why a manifest was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlError {
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
pub fn parse_toml(text: &str) -> Result<Table, TomlError> {
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
pub struct TargetDecl {
    pub name: Option<String>,
    pub path: Option<String>,
}

/// Which table a dependency was declared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

/// One dependency: the name the code uses and the package it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepDecl {
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
pub struct CargoManifest {
    /// `[package].name`; `None` for a virtual workspace manifest.
    pub package: Option<String>,
    pub lib: Option<TargetDecl>,
    pub bins: Vec<TargetDecl>,
    pub tests: Vec<TargetDecl>,
    pub examples: Vec<TargetDecl>,
    pub benches: Vec<TargetDecl>,
    /// `[package].build`: `Some(None)` when `build = false`.
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
pub fn read_cargo(text: &str) -> Result<CargoManifest, TomlError> {
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
}
