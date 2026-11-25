//! What one TypeScript or JavaScript file says: its static imports,
//! re-exports, `import(...)` and `require(...)` calls, and the names it
//! exports, read with a hand-written lexer (no JS parser crate: the gate
//! builds offline).
//!
//! The lexer knows line and block comments, single and double quoted
//! strings, template literals with nested `${ ... }` substitutions (whose
//! code is read as code), and regular expression literals, told from a
//! division by the previous significant token. It is iterative: a template
//! nested past [`MAX_TEMPLATE_DEPTH`] makes the file `unparsed`, never a
//! deep stack. Statements are then recognised from the token stream.
//!
//! Nothing here resolves a specifier: [`crate::resolve`] does that against
//! the in-scope inventory.

use std::collections::{BTreeMap, BTreeSet};

use spawn_core::feature_deps::{EdgeKind, Language, Scope};

/// Template literals nest at most this deep (`${ `${ ... }` }`); a deeper
/// file is unparsed.
pub const MAX_TEMPLATE_DEPTH: usize = 32;

/// The extensions read as TypeScript or JavaScript (`.d.ts` ends in `.ts`).
pub const SOURCE_EXTENSIONS: [&str; 8] = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Component files no extractor reads: listed `unsupported`, never guessed.
pub const UNSUPPORTED_EXTENSIONS: [&str; 2] = ["vue", "svelte"];

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rfind('.')
        .filter(|at| *at > 0)
        .map(|at| &name[at + 1..])
}

/// A TypeScript or JavaScript source (`.ts`, `.tsx`, `.mts`, `.cts`,
/// `.js`, `.jsx`, `.mjs`, `.cjs`, `.d.ts`).
pub fn is_ecma_source(path: &str) -> bool {
    extension(path).is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A `.vue` or `.svelte` component.
pub fn is_unsupported_component(path: &str) -> bool {
    extension(path).is_some_and(|ext| UNSUPPORTED_EXTENSIONS.contains(&ext))
}

/// The language `path` is read as.
pub fn language_of(path: &str) -> Language {
    match extension(path) {
        Some("ts" | "tsx" | "mts" | "cts") => Language::Typescript,
        _ => Language::Javascript,
    }
}

/// A test file: `*.test.*`, `*.spec.*`, or under a `test/`, `tests/` or
/// `__tests__/` directory.
pub fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    parts
        .iter()
        .any(|dir| matches!(*dir, "test" | "tests" | "__tests__"))
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// The scope of every edge from `path`.
pub fn scope_of(path: &str) -> Scope {
    if is_test_path(path) {
        Scope::Test
    } else {
        Scope::Prod
    }
}

/// `specifier` without its `?query` or `#hash` suffix (`./a.svg?raw`).
pub fn strip_query(specifier: &str) -> &str {
    let cut = specifier
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '?' || *c == '#')
        .map_or(specifier.len(), |(at, _)| at);
    &specifier[..cut]
}

/// A file a module specifier resolves to as code: a TypeScript or
/// JavaScript source, or a `.vue` / `.svelte` component. Any other file an
/// import reaches (`.css`, `.svg`, `.json`, ...) is an asset.
pub fn is_code_file(path: &str) -> bool {
    is_ecma_source(path) || is_unsupported_component(path)
}

/// A declaration file (`.d.ts`, `.d.mts`, `.d.cts`): every `import(...)`
/// in it is a type.
pub fn is_declaration_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// What one import binds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Imported {
    /// `{ name }` or `{ name as local }`: the exported name.
    Named(String),
    /// `local` (the default export).
    Default,
    /// `* as local`.
    Namespace,
}

/// One reference to another module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRef {
    pub specifier: String,
    /// The statement's kind; the resolver makes it [`EdgeKind::Asset`]
    /// when the specifier reaches a non-code file.
    pub kind: EdgeKind,
    /// 1-based line of the `import` / `export` / `require` keyword.
    pub line: u32,
    /// What the statement binds; empty for a side-effect import, a
    /// re-export, `import(...)` and `require(...)` (all stop at the file
    /// the specifier names).
    pub imported: Vec<Imported>,
}

/// One `export ... from` a barrel makes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reexport {
    /// `export { orig as name } from 'specifier'` (also an imported name
    /// exported again: `import { A } from './a'; export { A };`).
    Named {
        name: String,
        orig: String,
        specifier: String,
    },
    /// `export * from 'specifier'`.
    Star { specifier: String },
}

/// The facts of one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaFacts {
    pub refs: Vec<ModuleRef>,
    /// Names the file declares and exports itself (`default` included).
    pub exports: BTreeSet<String>,
    pub reexports: Vec<Reexport>,
    /// Lines of `import(expr)` calls whose argument is not a literal.
    pub dynamic_unresolved: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    /// A template literal; its text when it has no substitution.
    Template(Option<String>),
    Punct(u8),
    /// A number or regular expression literal.
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    tok: Tok,
    line: u32,
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' || byte >= 0x80
}

const fn is_ident_byte(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

/// Keywords after which a `/` opens a regular expression.
const REGEX_AFTER_KEYWORDS: [&str; 15] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    "extends",
];

struct Lexer<'a> {
    bytes: &'a [u8],
    at: usize,
    line: u32,
    tokens: Vec<Token>,
    /// Open `${` substitutions, each with its count of open `{`.
    templates: Vec<usize>,
    /// The line of the last failed regular expression scan, and how many
    /// failed on it: past [`MAX_REGEX_RETRIES`] a `/` on that line is a
    /// division, so a line of `(/[` repeated stays linear.
    regex_failures: (u32, usize),
}

/// Failed regular expression scans allowed on one line.
pub const MAX_REGEX_RETRIES: usize = 16;

/// Tokens looked at after `import('x',` for the call's `)`.
const CALL_LOOKAHEAD: usize = 64;

impl Lexer<'_> {
    fn push(&mut self, tok: Tok, line: u32) {
        self.tokens.push(Token { tok, line });
    }

    fn bump_line(&mut self, byte: u8) {
        if byte == b'\n' {
            self.line += 1;
        }
    }

    /// Whether a `/` here starts a regular expression.
    fn regex_allowed(&self) -> bool {
        match self.tokens.last().map(|token| &token.tok) {
            None => true,
            Some(Tok::Ident(word)) => REGEX_AFTER_KEYWORDS.contains(&word.as_str()),
            Some(Tok::Punct(punct)) => !matches!(punct, b')' | b']'),
            Some(Tok::Str(_) | Tok::Template(_) | Tok::Value) => false,
        }
    }

    /// Skip a quoted string from after its opening quote; it ends at the
    /// quote or, unterminated, at the end of the line.
    fn string(&mut self, quote: u8) -> String {
        let mut value = Vec::new();
        while self.at < self.bytes.len() {
            let byte = self.bytes[self.at];
            if byte == quote {
                self.at += 1;
                break;
            }
            if byte == b'\n' {
                break;
            }
            if byte == b'\\' {
                self.at += 1;
                if let Some(&next) = self.bytes.get(self.at) {
                    self.bump_line(next);
                    value.push(next);
                    self.at += 1;
                }
                continue;
            }
            value.push(byte);
            self.at += 1;
        }
        String::from_utf8_lossy(&value).into_owned()
    }

    /// Read template text from after a backtick or a substitution's `}`:
    /// up to the closing backtick (`Ok(Some(text))`), a `${` (`Ok(None)`,
    /// the substitution then opened), or the end of the file (`Err`).
    fn template_text(&mut self) -> Result<Option<String>, String> {
        let opened = self.line;
        let mut value = Vec::new();
        while self.at < self.bytes.len() {
            let byte = self.bytes[self.at];
            match byte {
                b'`' => {
                    self.at += 1;
                    return Ok(Some(String::from_utf8_lossy(&value).into_owned()));
                }
                b'\\' => {
                    self.at += 1;
                    if let Some(&next) = self.bytes.get(self.at) {
                        self.bump_line(next);
                        value.push(next);
                        self.at += 1;
                    }
                }
                b'$' if self.bytes.get(self.at + 1) == Some(&b'{') => {
                    self.at += 2;
                    return Ok(None);
                }
                _ => {
                    self.bump_line(byte);
                    value.push(byte);
                    self.at += 1;
                }
            }
        }
        Err(format!(
            "a template literal open at line {opened} never closes: the rest of the file would be lost"
        ))
    }

    /// Continue a template after a backtick or a substitution's `}`.
    fn template(&mut self, opening: bool, line: u32) -> Result<(), String> {
        if let Some(text) = self.template_text()? {
            if opening {
                self.push(Tok::Template(Some(text)), line);
            }
            return Ok(());
        }
        if opening {
            self.push(Tok::Template(None), line);
        }
        if self.templates.len() >= MAX_TEMPLATE_DEPTH {
            return Err(format!(
                "template literals nest past {MAX_TEMPLATE_DEPTH} levels at line {}",
                self.line
            ));
        }
        self.templates.push(0);
        Ok(())
    }

    fn regex_retry_left(&self) -> bool {
        self.regex_failures.0 != self.line || self.regex_failures.1 < MAX_REGEX_RETRIES
    }

    /// Skip a regular expression literal from after its `/`; `false` (and
    /// nothing consumed) when none ends on this line.
    fn regex(&mut self) -> bool {
        let mut at = self.at;
        let mut class = false;
        while let Some(&byte) = self.bytes.get(at) {
            match byte {
                b'\n' | b'\r' => return false,
                b'\\' => at += 1,
                b'[' => class = true,
                b']' => class = false,
                b'/' if !class => {
                    at += 1;
                    while self.bytes.get(at).is_some_and(|b| is_ident_byte(*b)) {
                        at += 1;
                    }
                    self.at = at;
                    return true;
                }
                _ => {}
            }
            at += 1;
        }
        false
    }

    fn run(mut self) -> Result<Vec<Token>, String> {
        if self.bytes.starts_with(b"#!") {
            while self.at < self.bytes.len() && self.bytes[self.at] != b'\n' {
                self.at += 1;
            }
        }
        while self.at < self.bytes.len() {
            let byte = self.bytes[self.at];
            let line = self.line;
            let next = self.bytes.get(self.at + 1).copied();
            if byte == b'/' && next == Some(b'/') {
                while self.at < self.bytes.len() && self.bytes[self.at] != b'\n' {
                    self.at += 1;
                }
            } else if byte == b'/' && next == Some(b'*') {
                self.at += 2;
                while self.at < self.bytes.len()
                    && !(self.bytes[self.at] == b'*' && self.bytes.get(self.at + 1) == Some(&b'/'))
                {
                    self.bump_line(self.bytes[self.at]);
                    self.at += 1;
                }
                if self.at >= self.bytes.len() {
                    return Err(format!(
                        "a block comment opened at line {line} never closes: the rest of the file would be lost"
                    ));
                }
                self.at += 2;
            } else if byte.is_ascii_whitespace() {
                self.bump_line(byte);
                self.at += 1;
            } else if byte == b'\'' || byte == b'"' {
                self.at += 1;
                let value = self.string(byte);
                self.push(Tok::Str(value), line);
            } else if byte == b'`' {
                self.at += 1;
                self.template(true, line)?;
            } else if byte == b'/' && self.regex_allowed() && self.regex_retry_left() {
                self.at += 1;
                if self.regex() {
                    self.push(Tok::Value, line);
                } else {
                    self.regex_failures = if self.regex_failures.0 == line {
                        (line, self.regex_failures.1 + 1)
                    } else {
                        (line, 1)
                    };
                    self.push(Tok::Punct(b'/'), line);
                }
            } else if is_ident_start(byte) {
                let start = self.at;
                while self.at < self.bytes.len() && is_ident_byte(self.bytes[self.at]) {
                    self.at += 1;
                }
                let word = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
                self.push(Tok::Ident(word), line);
            } else if byte.is_ascii_digit()
                || (byte == b'.' && next.is_some_and(|b| b.is_ascii_digit()))
            {
                while self.at < self.bytes.len()
                    && (is_ident_byte(self.bytes[self.at]) || self.bytes[self.at] == b'.')
                {
                    self.at += 1;
                }
                self.push(Tok::Value, line);
            } else {
                self.at += 1;
                match byte {
                    b'{' => {
                        if let Some(open) = self.templates.last_mut() {
                            *open += 1;
                        }
                    }
                    b'}' => match self.templates.last_mut() {
                        Some(0) => {
                            self.templates.pop();
                            self.template(false, line)?;
                            continue;
                        }
                        Some(open) => *open -= 1,
                        None => {}
                    },
                    _ => {}
                }
                self.push(Tok::Punct(byte), line);
            }
        }
        if !self.templates.is_empty() {
            return Err(
                "a template substitution `${` never closes: the rest of the file would be lost"
                    .to_string(),
            );
        }
        Ok(self.tokens)
    }
}

fn lex(text: &str) -> Result<Vec<Token>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    Lexer {
        bytes: text.as_bytes(),
        at: 0,
        line: 1,
        tokens: Vec::new(),
        templates: Vec::new(),
        regex_failures: (0, 0),
    }
    .run()
}

/// What an `import` statement's clause binds, its local names, and whether
/// every binding is type-only.
struct Clause {
    imported: Vec<(Imported, String)>,
    type_only: bool,
}

struct Reader {
    tokens: Vec<Token>,
    /// A declaration file: every `import(...)` is a type.
    declaration: bool,
    facts: EcmaFacts,
    /// Local name to (specifier, exported name) of each named import.
    bindings: BTreeMap<String, (String, String)>,
    /// `export { local as name }` without `from`: (name, local).
    local_exports: Vec<(String, String)>,
}

impl Reader {
    fn ident(&self, at: usize) -> Option<&str> {
        match self.tokens.get(at).map(|token| &token.tok) {
            Some(Tok::Ident(word)) => Some(word),
            _ => None,
        }
    }

    fn is_ident(&self, at: usize, word: &str) -> bool {
        self.ident(at) == Some(word)
    }

    fn is_punct(&self, at: usize, punct: u8) -> bool {
        matches!(self.tokens.get(at).map(|token| &token.tok), Some(Tok::Punct(p)) if *p == punct)
    }

    fn string(&self, at: usize) -> Option<&str> {
        match self.tokens.get(at).map(|token| &token.tok) {
            Some(Tok::Str(value) | Tok::Template(Some(value))) => Some(value),
            _ => None,
        }
    }

    fn line(&self, at: usize) -> u32 {
        self.tokens.get(at).map_or(0, |token| token.line)
    }

    /// A keyword at `at` that is not a member (`x.import`): a `.` before
    /// it is a member access unless it ends a spread (`...require('x')`).
    fn keyword(&self, at: usize, word: &str) -> bool {
        let member = at > 0
            && self.is_punct(at - 1, b'.')
            && !(at >= 3 && self.is_punct(at - 2, b'.') && self.is_punct(at - 3, b'.'));
        self.is_ident(at, word) && !member
    }

    fn add_ref(&mut self, specifier: &str, kind: EdgeKind, line: u32, imported: Vec<Imported>) {
        if specifier.is_empty() {
            return;
        }
        self.facts.refs.push(ModuleRef {
            specifier: specifier.to_string(),
            kind,
            line,
            imported,
        });
    }

    fn run(mut self) -> EcmaFacts {
        let mut at = 0;
        while at < self.tokens.len() {
            at = if self.keyword(at, "import") {
                self.import(at)
            } else if self.keyword(at, "export") {
                self.export(at)
            } else if self.keyword(at, "require")
                && self.is_punct(at + 1, b'(')
                && self.string(at + 2).is_some()
                && (self.is_punct(at + 3, b')') || self.is_punct(at + 3, b','))
            {
                let specifier = self.string(at + 2).unwrap_or_default().to_string();
                self.add_ref(&specifier, EdgeKind::Require, self.line(at), Vec::new());
                at + 4
            } else {
                at + 1
            };
        }
        for (name, local) in std::mem::take(&mut self.local_exports) {
            if let Some((specifier, orig)) = self.bindings.get(&local) {
                self.facts.reexports.push(Reexport::Named {
                    name,
                    orig: orig.clone(),
                    specifier: specifier.clone(),
                });
            } else {
                self.facts.exports.insert(name);
            }
        }
        self.facts
    }

    /// `import(...)`, `import 'x'`, `import ... from 'x'`,
    /// `import a = require('x')`; returns where to read on.
    fn import(&mut self, at: usize) -> usize {
        let line = self.line(at);
        if self.is_punct(at + 1, b'(') {
            if let Some(specifier) = self.string(at + 2).map(str::to_string)
                && (self.is_punct(at + 3, b')') || self.is_punct(at + 3, b','))
            {
                // A type: in a declaration file, after `typeof`, or with a
                // member other than a promise's (`import('./x').T`).
                let after_call = self.close_of(at + 3);
                let promise_member = matches!(
                    after_call.and_then(|after| self.ident(after + 1)),
                    Some("then" | "catch" | "finally")
                );
                let member = after_call.is_some_and(|after| self.is_punct(after, b'.'));
                let kind = if self.declaration
                    || (at > 0 && self.is_ident(at - 1, "typeof"))
                    || (member && !promise_member)
                {
                    EdgeKind::ImportType
                } else {
                    EdgeKind::DynamicImport
                };
                self.add_ref(&specifier, kind, line, Vec::new());
                return at + 4;
            }
            self.facts.dynamic_unresolved.push(line);
            return at + 2;
        }
        if self.is_punct(at + 1, b'.') {
            return at + 2;
        }
        if let Some(specifier) = self.string(at + 1).map(str::to_string) {
            self.add_ref(&specifier, EdgeKind::Import, line, Vec::new());
            return at + 2;
        }
        let mut cursor = at + 1;
        let mut type_only = false;
        if self.is_ident(cursor, "type")
            && !self.is_ident(cursor + 1, "from")
            && !self.is_punct(cursor + 1, b',')
            && !self.is_punct(cursor + 1, b'=')
        {
            type_only = true;
            cursor += 1;
        }
        // `import a = require('x')` / `import type a = require('x')`.
        if self.ident(cursor).is_some()
            && self.is_punct(cursor + 1, b'=')
            && self.is_ident(cursor + 2, "require")
            && self.is_punct(cursor + 3, b'(')
        {
            if let Some(specifier) = self.string(cursor + 4).map(str::to_string) {
                self.add_ref(&specifier, EdgeKind::Require, line, Vec::new());
            }
            return cursor + 5;
        }
        let Some((clause, next)) = self.clause(cursor) else {
            return at + 1;
        };
        if !self.is_ident(next, "from") {
            return next;
        }
        let Some(specifier) = self.string(next + 1).map(str::to_string) else {
            return next + 1;
        };
        let kind = if type_only || clause.type_only {
            EdgeKind::ImportType
        } else {
            EdgeKind::Import
        };
        for (imported, local) in &clause.imported {
            if let Imported::Named(name) = imported {
                self.bindings
                    .insert(local.clone(), (specifier.clone(), name.clone()));
            }
        }
        let imported = clause.imported.into_iter().map(|(i, _)| i).collect();
        self.add_ref(&specifier, kind, line, imported);
        next + 2
    }

    /// The token after the `)` matching the `(` before `at`, looking at
    /// most [`CALL_LOOKAHEAD`] tokens ahead (so a file of `import('x',`
    /// stays linear); `None` past that.
    fn close_of(&self, start: usize) -> Option<usize> {
        let mut depth = 1usize;
        for at in start..(start + CALL_LOOKAHEAD).min(self.tokens.len()) {
            if self.is_punct(at, b'(') {
                depth += 1;
            } else if self.is_punct(at, b')') {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
        }
        None
    }

    /// An import clause from `at`: `D`, `* as ns`, `{ a, b as c, type d }`
    /// and their combinations; `None` when it is not one.
    fn clause(&self, mut at: usize) -> Option<(Clause, usize)> {
        let mut imported = Vec::new();
        let mut named = 0usize;
        let mut named_types = 0usize;
        loop {
            if self.is_punct(at, b'*') && self.is_ident(at + 1, "as") {
                let local = self.ident(at + 2)?.to_string();
                imported.push((Imported::Namespace, local));
                at += 3;
            } else if self.is_punct(at, b'{') {
                at += 1;
                while !self.is_punct(at, b'}') {
                    let mut entry_type = false;
                    if self.is_ident(at, "type")
                        && (self.ident(at + 1).is_some() || self.string(at + 1).is_some())
                        && !self.is_ident(at + 1, "as")
                    {
                        entry_type = true;
                        at += 1;
                    }
                    let name = self.ident(at).or_else(|| self.string(at))?.to_string();
                    at += 1;
                    let local = if self.is_ident(at, "as") {
                        at += 2;
                        self.ident(at - 1)?.to_string()
                    } else {
                        name.clone()
                    };
                    named += 1;
                    if entry_type {
                        named_types += 1;
                    }
                    let entry = if name == "default" {
                        Imported::Default
                    } else {
                        Imported::Named(name)
                    };
                    imported.push((entry, local));
                    if self.is_punct(at, b',') {
                        at += 1;
                    } else if !self.is_punct(at, b'}') {
                        return None;
                    }
                }
                at += 1;
            } else if let Some(local) = self.ident(at) {
                if local == "from" && !imported.is_empty() {
                    break;
                }
                imported.push((Imported::Default, local.to_string()));
                at += 1;
            } else {
                return None;
            }
            if self.is_punct(at, b',') {
                at += 1;
            } else {
                break;
            }
        }
        let type_only = named > 0 && named == named_types && imported.len() == named;
        Some((
            Clause {
                imported,
                type_only,
            },
            at,
        ))
    }

    /// Every `export` form; returns where to read on.
    fn export(&mut self, at: usize) -> usize {
        let line = self.line(at);
        let mut cursor = at + 1;
        let mut type_only = false;
        if self.is_ident(cursor, "type")
            && (self.is_punct(cursor + 1, b'{') || self.is_punct(cursor + 1, b'*'))
        {
            type_only = true;
            cursor += 1;
        }
        let kind = if type_only {
            EdgeKind::ImportType
        } else {
            EdgeKind::Reexport
        };
        if self.is_punct(cursor, b'*') {
            let mut next = cursor + 1;
            let mut namespace = None;
            if self.is_ident(next, "as") {
                namespace = self
                    .ident(next + 1)
                    .or_else(|| self.string(next + 1))
                    .map(str::to_string);
                next += 2;
            }
            if self.is_ident(next, "from")
                && let Some(specifier) = self.string(next + 1).map(str::to_string)
            {
                match namespace {
                    Some(name) => {
                        self.facts.exports.insert(name);
                    }
                    None => self.facts.reexports.push(Reexport::Star {
                        specifier: specifier.clone(),
                    }),
                }
                self.add_ref(&specifier, kind, line, Vec::new());
                return next + 2;
            }
            return next;
        }
        if self.is_punct(cursor, b'{') {
            return self.export_braces(cursor, kind, line);
        }
        if self.is_ident(cursor, "import") {
            // `export import a = require('x')`.
            return cursor;
        }
        if self.is_ident(cursor, "default") {
            self.facts.exports.insert("default".to_string());
            return cursor + 1;
        }
        while (matches!(self.ident(cursor), Some("declare" | "async" | "abstract"))
            && self.ident(cursor + 1).is_some())
            || (self.is_ident(cursor, "const") && self.is_ident(cursor + 1, "enum"))
        {
            cursor += 1;
        }
        if matches!(
            self.ident(cursor),
            Some(
                "function"
                    | "class"
                    | "interface"
                    | "type"
                    | "enum"
                    | "namespace"
                    | "module"
                    | "let"
                    | "var"
                    | "const"
            )
        ) {
            cursor += 1;
            if self.is_punct(cursor, b'*') {
                cursor += 1;
            }
            if let Some(name) = self.ident(cursor).map(str::to_string) {
                self.facts.exports.insert(name);
                return cursor + 1;
            }
        }
        cursor
    }

    /// `export { a, b as c } [from 'x']` from its `{`; returns where to
    /// read on.
    fn export_braces(&mut self, cursor: usize, kind: EdgeKind, line: u32) -> usize {
        let mut entries = Vec::new();
        let mut next = cursor + 1;
        while next < self.tokens.len() && !self.is_punct(next, b'}') {
            if self.is_ident(next, "type")
                && (self.ident(next + 1).is_some() || self.string(next + 1).is_some())
                && !self.is_ident(next + 1, "as")
            {
                next += 1;
            }
            let Some(orig) = self
                .ident(next)
                .or_else(|| self.string(next))
                .map(str::to_string)
            else {
                return next;
            };
            next += 1;
            let name = if self.is_ident(next, "as") {
                next += 2;
                self.ident(next - 1)
                    .or_else(|| self.string(next - 1))
                    .map_or_else(|| orig.clone(), str::to_string)
            } else {
                orig.clone()
            };
            entries.push((name, orig));
            if self.is_punct(next, b',') {
                next += 1;
            } else if !self.is_punct(next, b'}') {
                return next;
            }
        }
        next += 1;
        if self.is_ident(next, "from")
            && let Some(specifier) = self.string(next + 1).map(str::to_string)
        {
            for (name, orig) in entries {
                self.facts.reexports.push(Reexport::Named {
                    name,
                    orig,
                    specifier: specifier.clone(),
                });
            }
            self.add_ref(&specifier, kind, line, Vec::new());
            return next + 2;
        }
        self.local_exports.extend(entries);
        next
    }
}

/// Read one file. `Err` (an `unparsed` diagnostic) when the lexer meets a
/// shape past its bounds, or a comment or template literal that never
/// closes (which would hide the rest of the file).
pub fn parse(text: &str) -> Result<EcmaFacts, String> {
    parse_at("", text)
}

/// [`parse`] for the file at `path`: in a declaration file every
/// `import(...)` is a type.
pub fn parse_at(path: &str, text: &str) -> Result<EcmaFacts, String> {
    let tokens = lex(text)?;
    Ok(Reader {
        tokens,
        declaration: is_declaration_file(path),
        facts: EcmaFacts::default(),
        bindings: BTreeMap::new(),
        local_exports: Vec::new(),
    }
    .run())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(text: &str) -> Vec<(String, EdgeKind, u32)> {
        parse(text)
            .expect("parsed")
            .refs
            .into_iter()
            .map(|r| (r.specifier, r.kind, r.line))
            .collect()
    }

    #[test]
    fn extensions_languages_and_test_paths() {
        assert!(is_ecma_source("a/b.d.ts"));
        assert!(is_ecma_source("a/b.cjs"));
        assert!(!is_ecma_source("a/b.json"));
        assert!(is_unsupported_component("a/App.vue"));
        assert_eq!(language_of("a.mts"), Language::Typescript);
        assert_eq!(language_of("a.jsx"), Language::Javascript);
        assert!(is_test_path("web/test/a.ts"));
        assert!(is_test_path("src/a.spec.tsx"));
        assert!(is_test_path("src/__tests__/a.js"));
        assert!(!is_test_path("src/testing.ts"));
        assert!(is_code_file("a/App.vue") && is_code_file("a/b.d.ts"));
        assert!(!is_code_file("a/b.css") && !is_code_file("a/data.json"));
        assert!(is_declaration_file("a/b.d.ts") && is_declaration_file("b.d.cts"));
        assert!(!is_declaration_file("a/b.ts") && !is_declaration_file("a/d.ts"));
        assert_eq!(strip_query("./a.svg?raw"), "./a.svg");
        assert_eq!(strip_query("#internal?x"), "#internal");
    }

    #[test]
    fn every_statement_form_is_read() {
        let text = "import a, { b as c, type D } from './a';\nimport type { T } from './t';\nimport './side';\nexport * from './star';\nexport type { U } from './u';\nexport { e } from './e';\nconst x = await import('./lazy');\nconst y = require(\"./req\");\nimport z = require('./tsreq');\ntype Q = import('./typed').Q;\nimport('./w', { with: {} });\n";
        assert_eq!(
            refs(text),
            vec![
                ("./a".to_string(), EdgeKind::Import, 1),
                ("./t".to_string(), EdgeKind::ImportType, 2),
                ("./side".to_string(), EdgeKind::Import, 3),
                ("./star".to_string(), EdgeKind::Reexport, 4),
                ("./u".to_string(), EdgeKind::ImportType, 5),
                ("./e".to_string(), EdgeKind::Reexport, 6),
                ("./lazy".to_string(), EdgeKind::DynamicImport, 7),
                ("./req".to_string(), EdgeKind::Require, 8),
                ("./tsreq".to_string(), EdgeKind::Require, 9),
                ("./typed".to_string(), EdgeKind::ImportType, 10),
                ("./w".to_string(), EdgeKind::DynamicImport, 11),
            ]
        );
    }

    #[test]
    fn exports_and_barrels_are_collected() {
        let facts = parse(
            "import { A } from './a';\nexport { A };\nexport const B = 1;\nexport default function f() {}\nexport async function g() {}\nexport { C as D } from './c';\nexport * from './e';\nexport * as ns from './n';\nexport interface I {}\n",
        )
        .expect("parsed");
        assert_eq!(
            facts.exports.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["B", "I", "default", "g", "ns"]
        );
        assert!(facts.reexports.contains(&Reexport::Named {
            name: "A".into(),
            orig: "A".into(),
            specifier: "./a".into()
        }));
        assert!(facts.reexports.contains(&Reexport::Named {
            name: "D".into(),
            orig: "C".into(),
            specifier: "./c".into()
        }));
        assert!(facts.reexports.contains(&Reexport::Star {
            specifier: "./e".into()
        }));
    }

    #[test]
    fn template_substitutions_are_code_and_their_text_is_not() {
        let text = "const s = `import x from './no' ${ await import('./yes') } ${ `${ {a: 1}.a }` } done`;\nimport b from './after';\n";
        assert_eq!(
            refs(text),
            vec![
                ("./yes".to_string(), EdgeKind::DynamicImport, 1),
                ("./after".to_string(), EdgeKind::Import, 2),
            ]
        );
    }

    #[test]
    fn members_and_keys_named_like_keywords_are_not_statements() {
        assert!(
            refs("x.import('./a'); const o = { import: 1, require: 2 }; y.require('./b');\n")
                .is_empty()
        );
    }

    #[test]
    fn a_promise_member_keeps_import_dynamic_and_any_other_member_is_a_type() {
        let text = "const a = import('./a').then((m) => m.a);\nimport('./b').catch(() => null);\nimport('./c')\n  .finally(done);\ntype D = import('./d').D;\ntype E = typeof import('./e');\nlet f: import('./f').F<import('./g').G>;\nconst h = (await import('./h')).h;\n";
        assert_eq!(
            refs(text),
            vec![
                ("./a".to_string(), EdgeKind::DynamicImport, 1),
                ("./b".to_string(), EdgeKind::DynamicImport, 2),
                ("./c".to_string(), EdgeKind::DynamicImport, 3),
                ("./d".to_string(), EdgeKind::ImportType, 5),
                ("./e".to_string(), EdgeKind::ImportType, 6),
                ("./f".to_string(), EdgeKind::ImportType, 7),
                ("./g".to_string(), EdgeKind::ImportType, 7),
                ("./h".to_string(), EdgeKind::DynamicImport, 8),
            ]
        );
        // In a declaration file every import() is a type, `.then` or not.
        let declared = parse_at(
            "types/x.d.ts",
            "declare const a: import('./a').then;\nexport type B = import('./b');\n",
        )
        .expect("parsed");
        assert!(
            declared.refs.iter().all(|r| r.kind == EdgeKind::ImportType),
            "{declared:?}"
        );
        assert_eq!(declared.refs.len(), 2);
    }

    #[test]
    fn a_spread_require_or_import_is_a_reference_but_a_member_is_not() {
        assert_eq!(
            refs(
                "const o = { ...require('./a') };\nconst l = [...require('./b')];\nconst m = { ...(await import('./c')) };\nx.require('./no');\nx?.require('./no');\n"
            ),
            vec![
                ("./a".to_string(), EdgeKind::Require, 1),
                ("./b".to_string(), EdgeKind::Require, 2),
                ("./c".to_string(), EdgeKind::DynamicImport, 3),
            ]
        );
    }

    #[test]
    fn a_comment_or_template_that_never_closes_makes_the_file_unparsed() {
        for text in [
            "import a from './a';\n/* never closed\nimport b from './b';\n",
            "import a from './a';\nconst t = `never closed\nimport b from './b';\n",
            "import a from './a';\nconst t = `${ never closed\nimport b from './b';\n",
        ] {
            let error = parse(text).expect_err("unparsed");
            assert!(error.contains("never closes"), "{error}");
        }
        assert!(parse("const t = `ok ${ `inner` } ok`; /* closed */\n").is_ok());
    }

    /// Generous bounds: each shape is quadratic (or worse) without its
    /// bound, minutes at these sizes; linear, it takes milliseconds.
    #[test]
    fn regex_retries_and_call_lookahead_keep_hostile_lines_linear() {
        let started = std::time::Instant::now();
        let regexes = format!("{}\nimport a from './a';\n", "(/[".repeat(300_000));
        assert_eq!(
            refs(&regexes),
            vec![("./a".to_string(), EdgeKind::Import, 2)]
        );
        let calls = format!("{}\n", "import('x',".repeat(200_000));
        let facts = parse(&calls).expect("parsed");
        assert_eq!(facts.refs.len(), 200_000);
        assert!(facts.refs.iter().all(|r| r.kind == EdgeKind::DynamicImport));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        // A regex on a line after failed scans on earlier lines still reads.
        assert_eq!(
            refs("a = b / c / d / e;\nconst r = /'/;\nimport x from './x';\n"),
            vec![("./x".to_string(), EdgeKind::Import, 3)]
        );
    }
}
