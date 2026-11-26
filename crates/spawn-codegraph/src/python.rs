//! What one Python file (`.py` or `.pyi`) says: its static `import`,
//! `from ... import ...`, `TYPE_CHECKING`-guarded imports, constant-literal
//! `importlib.import_module(...)` calls, module-level exports / `__all__`,
//! and dynamic `import_module(expr)` diagnostics, read with an iterative
//! hand-written lexer (zero external Python parser crates; offline-safe).
//!
//! Nothing here resolves module names or reads the filesystem:
//! [`crate::resolve`] does that against the in-scope inventory.

use std::collections::BTreeSet;

use spawn_core::feature_deps::{EdgeKind, Language, Scope};

/// Maximum bracket nesting depth accepted by the Python lexer.
pub const MAX_BRACKET_DEPTH: usize = 256;

/// Maximum nested f-string depth accepted by the Python lexer.
pub const MAX_FSTRING_DEPTH: usize = 32;

/// Extensions read as Python sources or stub files.
pub const SOURCE_EXTENSIONS: [&str; 2] = ["py", "pyi"];

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rfind('.')
        .filter(|at| *at > 0)
        .map(|at| &name[at + 1..])
}

/// Whether `path` is a Python source or stub file (`.py`, `.pyi`).
pub fn is_python_source(path: &str) -> bool {
    extension(path).is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// Whether `path` is a Python type stub (`.pyi`).
pub fn is_stub_file(path: &str) -> bool {
    extension(path) == Some("pyi")
}

/// Whether `path` is a package `__init__.py` or `__init__.pyi`.
pub fn is_package_init(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "__init__.py" || name == "__init__.pyi"
}

/// Whether `path` is a Python test file (`test/`, `tests/`, `__tests__/`,
/// `conftest.py`, `test_*.py`, `test-*.py`, `*_test.py`, `*-test.py`).
pub fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    if parts
        .iter()
        .any(|dir| matches!(*dir, "test" | "tests" | "__tests__"))
    {
        return true;
    }
    let Some(stem) = name
        .strip_suffix(".py")
        .or_else(|| name.strip_suffix(".pyi"))
    else {
        return false;
    };
    stem == "conftest"
        || stem.starts_with("test_")
        || stem.starts_with("test-")
        || stem.ends_with("_test")
        || stem.ends_with("-test")
}

/// The scope of every edge from `path`.
pub fn scope_of(path: &str) -> Scope {
    if is_test_path(path) {
        Scope::Test
    } else {
        Scope::Prod
    }
}

/// The language for a Python file.
pub const fn language() -> Language {
    Language::Python
}

/// One imported name inside a `from ... import ...` statement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PyImported {
    /// `orig` or `orig as alias` (`explicit_alias` records whether `as` was
    /// written: PEP 484 treats explicit redundant `from .x import Y as Y` in
    /// `__init__.py` as an explicit re-export).
    Named {
        orig: String,
        alias: String,
        explicit_alias: bool,
    },
    /// `from pkg import *`.
    Star,
}

/// One Python import or constant `importlib.import_module(...)` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyImport {
    /// Relative level (`0` for absolute; `1` for `.`, `2` for `..`, ...).
    pub level: usize,
    /// Dotted module path without leading dots (`""` for `from . import x`).
    pub module: String,
    /// Bound names for `from ... import ...`; empty for `import ...` and
    /// `importlib.import_module(...)`.
    pub names: Vec<PyImported>,
    /// Edge kind before `__init__.py` barrel re-export promotion.
    pub kind: EdgeKind,
    /// 1-based line number of the statement or call.
    pub line: u32,
    /// Whether the import sits inside an `if TYPE_CHECKING:` block.
    pub type_checking: bool,
    /// Whether the import sits outside `def` / `class` scopes.
    pub top_level: bool,
    /// Literal `package="..."` argument passed to a relative
    /// `importlib.import_module(".x", package="pkg")` call, if any.
    pub package_arg: Option<String>,
}

/// Static facts extracted from one `.py` or `.pyi` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PyFacts {
    pub imports: Vec<PyImport>,
    /// Top-level symbols defined, assigned, or imported in the module.
    pub exports: BTreeSet<String>,
    /// String literal entries statically listed in top-level `__all__`.
    pub explicit_all: BTreeSet<String>,
    /// 1-based lines of non-literal or unanchored `import_module(...)` calls.
    pub dynamic_unresolved: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LineStart { indent: usize, line: u32 },
    Newline,
    Ident(String),
    Str(String),
    FString,
    Dot,
    ColonEq,
    Punct(u8),
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    tok: Tok,
    line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FStringMode {
    Text,
    Expr {
        bracket_depth: usize,
        in_format_spec: bool,
    },
    FormatSpec,
}

#[derive(Debug, Clone)]
struct FStringFrame {
    quote: u8,
    triple: bool,
    raw: bool,
    line: u32,
    mode: FStringMode,
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

const fn is_ident_byte(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

/// Parse Python string prefix (`r`, `u`, `b`, `f`, `rf`, `fr`, `rb`, `br`,
/// case-insensitive). Returns `(is_raw, is_fstring)`.
fn string_prefix_kind(prefix: &[u8]) -> Option<(bool, bool)> {
    match prefix.len() {
        1 => match prefix[0].to_ascii_lowercase() {
            b'r' => Some((true, false)),
            b'f' => Some((false, true)),
            b'u' | b'b' => Some((false, false)),
            _ => None,
        },
        2 => {
            let a = prefix[0].to_ascii_lowercase();
            let b = prefix[1].to_ascii_lowercase();
            match (a, b) {
                (b'r', b'f') | (b'f', b'r') => Some((true, true)),
                (b'r', b'b') | (b'b', b'r') => Some((true, false)),
                _ => None,
            }
        }
        _ => None,
    }
}

struct Lexer<'a> {
    bytes: &'a [u8],
    at: usize,
    line: u32,
    tokens: Vec<Token>,
    bracket_depth: usize,
    at_logical_start: bool,
    line_indent: usize,
    line_start_num: u32,
    fstrings: Vec<FStringFrame>,
}

impl<'a> Lexer<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            line: 1,
            tokens: Vec::new(),
            bracket_depth: 0,
            at_logical_start: true,
            line_indent: 0,
            line_start_num: 1,
            fstrings: Vec::new(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.at + offset).copied()
    }

    fn starts_triple(&self, quote: u8) -> bool {
        self.peek() == Some(quote)
            && self.peek_at(1) == Some(quote)
            && self.peek_at(2) == Some(quote)
    }

    fn emit_line_start_if_needed(&mut self) {
        if self.at_logical_start && self.bracket_depth == 0 && self.fstrings.is_empty() {
            self.tokens.push(Token {
                tok: Tok::LineStart {
                    indent: self.line_indent,
                    line: self.line_start_num,
                },
                line: self.line_start_num,
            });
            self.at_logical_start = false;
        }
    }

    fn push(&mut self, tok: Tok, line: u32) {
        self.emit_line_start_if_needed();
        self.tokens.push(Token { tok, line });
    }

    fn consume_newline(&mut self) -> bool {
        match self.peek() {
            Some(b'\n') => {
                self.at += 1;
                self.line += 1;
                true
            }
            Some(b'\r') => {
                self.at += 1;
                if self.peek() == Some(b'\n') {
                    self.at += 1;
                }
                self.line += 1;
                true
            }
            _ => false,
        }
    }

    fn skip_comment_line(&mut self) {
        while let Some(byte) = self.peek() {
            if byte == b'\n' || byte == b'\r' {
                break;
            }
            self.at += 1;
        }
    }

    fn scan_unicode_hex(&mut self, count: usize, out: &mut Vec<u8>) {
        if self.at + count > self.bytes.len() {
            out.extend_from_slice(&self.bytes[self.at..]);
            self.at = self.bytes.len();
            return;
        }
        let hex_slice = &self.bytes[self.at..self.at + count];
        if let Ok(hex_str) = std::str::from_utf8(hex_slice)
            && let Ok(code) = u32::from_str_radix(hex_str, 16)
            && let Some(ch) = char::from_u32(code)
        {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            self.at += count;
            return;
        }
        out.extend_from_slice(hex_slice);
        self.at += count;
    }

    /// Read a quoted string (single- or triple-quoted, raw or escaped) starting
    /// immediately after its opening quote delimiter.
    fn scan_string(
        &mut self,
        quote: u8,
        triple: bool,
        raw: bool,
        opened: u32,
    ) -> Result<String, String> {
        let mut out = Vec::new();
        while self.at < self.bytes.len() {
            if triple && self.starts_triple(quote) {
                self.at += 3;
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            let byte = self.bytes[self.at];
            if !triple && byte == quote {
                self.at += 1;
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            if !triple && (byte == b'\n' || byte == b'\r') {
                // Single-quoted unclosed line: stop at newline rather than swallowing file.
                self.consume_newline();
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            if byte == b'\\' {
                self.at += 1;
                if self.consume_newline() {
                    if raw {
                        out.push(b'\\');
                        out.push(b'\n');
                    }
                    continue;
                }
                let Some(next) = self.peek() else {
                    break;
                };
                self.at += 1;
                if raw {
                    out.push(b'\\');
                    out.push(next);
                } else {
                    match next {
                        b'\\' | b'\'' | b'"' => out.push(next),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'a' => out.push(0x07),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'v' => out.push(0x0b),
                        b'x' => self.scan_unicode_hex(2, &mut out),
                        b'u' => self.scan_unicode_hex(4, &mut out),
                        b'U' => self.scan_unicode_hex(8, &mut out),
                        other => {
                            out.push(b'\\');
                            out.push(other);
                        }
                    }
                }
                continue;
            }
            if byte == b'\n' || byte == b'\r' {
                self.consume_newline();
                out.push(b'\n');
                continue;
            }
            out.push(byte);
            self.at += 1;
        }
        if triple {
            Err(format!(
                "triple-quoted string opened at line {opened} never closes"
            ))
        } else {
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
    }

    /// Consume `\N{NAME}` in non-raw f-string literal portions so braces inside
    /// named Unicode escapes do not open an interpolation expression.
    fn skip_fstring_escape(&mut self, raw: bool) {
        self.at += 1; // consume '\'
        if self.consume_newline() {
            return;
        }
        let Some(next) = self.peek() else {
            return;
        };
        self.at += 1;
        if !raw && next == b'N' && self.peek() == Some(b'{') {
            self.at += 1;
            while let Some(b) = self.peek() {
                if b == b'}' {
                    self.at += 1;
                    break;
                }
                if b == b'\n' || b == b'\r' {
                    break;
                }
                self.at += 1;
            }
        }
    }

    fn advance_fstring_text(&mut self) -> Result<(), String> {
        let Some(frame) = self.fstrings.last().cloned() else {
            return Ok(());
        };
        while self.at < self.bytes.len() {
            if frame.triple && self.starts_triple(frame.quote) {
                self.at += 3;
                let popped = self.fstrings.pop().expect("frame");
                if self.fstrings.is_empty() {
                    self.push(Tok::FString, popped.line);
                }
                return Ok(());
            }
            let byte = self.bytes[self.at];
            if !frame.triple && byte == frame.quote {
                self.at += 1;
                let popped = self.fstrings.pop().expect("frame");
                if self.fstrings.is_empty() {
                    self.push(Tok::FString, popped.line);
                }
                return Ok(());
            }
            if !frame.triple && (byte == b'\n' || byte == b'\r') {
                self.consume_newline();
                let popped = self.fstrings.pop().expect("frame");
                if self.fstrings.is_empty() {
                    self.push(Tok::FString, popped.line);
                }
                return Ok(());
            }
            if byte == b'\\' {
                self.skip_fstring_escape(frame.raw);
                continue;
            }
            if byte == b'{' {
                if self.peek_at(1) == Some(b'{') {
                    self.at += 2;
                    continue;
                }
                self.at += 1;
                if let Some(top) = self.fstrings.last_mut() {
                    top.mode = FStringMode::Expr {
                        bracket_depth: 0,
                        in_format_spec: false,
                    };
                }
                return Ok(());
            }
            if byte == b'}' && self.peek_at(1) == Some(b'}') {
                self.at += 2;
                continue;
            }
            if byte == b'\n' || byte == b'\r' {
                self.consume_newline();
                continue;
            }
            self.at += 1;
        }
        Err(format!(
            "f-string opened at line {} never closes",
            frame.line
        ))
    }

    fn advance_fstring_format_spec(&mut self) -> Result<(), String> {
        if self.fstrings.last().map(|f| f.mode) != Some(FStringMode::FormatSpec) {
            return Ok(());
        }
        let frame_line = self.fstrings.last().map_or(self.line, |f| f.line);
        let triple = self.fstrings.last().is_some_and(|f| f.triple);
        while self.at < self.bytes.len() {
            let byte = self.bytes[self.at];
            if !triple && (byte == b'\n' || byte == b'\r') {
                return Err(format!(
                    "unclosed f-string format spec on line {frame_line}"
                ));
            }
            if byte == b'\\' {
                self.at += 1;
                if !self.consume_newline() && self.at < self.bytes.len() {
                    self.at += 1;
                }
                continue;
            }
            match byte {
                b'{' => {
                    self.at += 1;
                    if let Some(top) = self.fstrings.last_mut() {
                        top.mode = FStringMode::Expr {
                            bracket_depth: 0,
                            in_format_spec: true,
                        };
                    }
                    return Ok(());
                }
                b'}' => {
                    self.at += 1;
                    if let Some(top) = self.fstrings.last_mut() {
                        top.mode = FStringMode::Text;
                    }
                    return self.advance_fstring_text();
                }
                b'\n' | b'\r' => {
                    self.consume_newline();
                }
                _ => {
                    self.at += 1;
                }
            }
        }
        Err(format!("f-string opened at line {frame_line} never closes"))
    }

    fn open_string(
        &mut self,
        quote: u8,
        raw: bool,
        fstring: bool,
        line: u32,
    ) -> Result<(), String> {
        let triple = self.starts_triple(quote);
        self.at += if triple { 3 } else { 1 };
        if !fstring {
            let text = self.scan_string(quote, triple, raw, line)?;
            self.push(Tok::Str(text), line);
            return Ok(());
        }
        // Check nesting limit before pushing an active f-string frame; emit LineStart
        // before entering f-string so tokens inside `{expr}` belong to the current line.
        self.emit_line_start_if_needed();
        if self.fstrings.len() >= MAX_FSTRING_DEPTH {
            return Err(format!(
                "f-strings nest past {MAX_FSTRING_DEPTH} levels at line {line}"
            ));
        }
        self.fstrings.push(FStringFrame {
            quote,
            triple,
            raw,
            line,
            mode: FStringMode::Text,
        });
        self.advance_fstring_text()
    }

    fn skip_logical_line_leading(&mut self) -> bool {
        if !(self.at_logical_start && self.bracket_depth == 0 && self.fstrings.is_empty()) {
            return false;
        }
        match self.bytes[self.at] {
            b' ' => {
                self.line_indent += 1;
                self.at += 1;
                true
            }
            b'\t' => {
                self.line_indent = (self.line_indent / 8 + 1) * 8;
                self.at += 1;
                true
            }
            b'#' => {
                self.skip_comment_line();
                self.consume_newline();
                self.line_indent = 0;
                self.line_start_num = self.line;
                true
            }
            b'\n' | b'\r' => {
                self.consume_newline();
                self.line_indent = 0;
                self.line_start_num = self.line;
                true
            }
            b'\\' if matches!(self.peek_at(1), Some(b'\n' | b'\r')) => {
                self.at += 1;
                self.consume_newline();
                true
            }
            _ => false,
        }
    }

    fn advance_ident_or_prefixed_string(&mut self, line: u32) -> Result<(), String> {
        let start = self.at;
        while self.at < self.bytes.len() && is_ident_byte(self.bytes[self.at]) {
            self.at += 1;
        }
        let word = &self.bytes[start..self.at];
        if let Some(q @ (b'\'' | b'"')) = self.peek()
            && let Some((raw, fstring)) = string_prefix_kind(word)
        {
            return self.open_string(q, raw, fstring, line);
        }
        let ident = String::from_utf8_lossy(word).into_owned();
        self.push(Tok::Ident(ident), line);
        Ok(())
    }

    fn advance_number(&mut self, line: u32) {
        self.at += 1;
        while let Some(b) = self.peek() {
            if is_ident_byte(b) || b == b'.' {
                self.at += 1;
            } else if matches!(b, b'e' | b'E') && matches!(self.peek_at(1), Some(b'+' | b'-')) {
                self.at += 2;
            } else {
                break;
            }
        }
        self.push(Tok::Value, line);
    }

    fn advance_fstring_expr_punct(&mut self, byte: u8, line: u32) -> Result<bool, String> {
        let next_is_eq = self.peek_at(1) == Some(b'=');
        let Some(FStringMode::Expr {
            bracket_depth: expr_brackets,
            in_format_spec,
        }) = self.fstrings.last_mut().map(|f| &mut f.mode)
        else {
            return Ok(false);
        };
        let in_fmt = *in_format_spec;
        match byte {
            b'(' | b'[' | b'{' => {
                *expr_brackets += 1;
                self.at += 1;
                if self.bracket_depth >= MAX_BRACKET_DEPTH {
                    return Err(format!(
                        "brackets nest past {MAX_BRACKET_DEPTH} levels at line {line}"
                    ));
                }
                self.bracket_depth += 1;
                self.push(Tok::Punct(byte), line);
                Ok(true)
            }
            b')' | b']' => {
                *expr_brackets = expr_brackets.saturating_sub(1);
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                self.at += 1;
                self.push(Tok::Punct(byte), line);
                Ok(true)
            }
            b'}' if *expr_brackets > 0 => {
                *expr_brackets -= 1;
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                self.at += 1;
                self.push(Tok::Punct(byte), line);
                Ok(true)
            }
            b'}' => {
                self.at += 1;
                let next_mode = if in_fmt {
                    FStringMode::FormatSpec
                } else {
                    FStringMode::Text
                };
                if let Some(top) = self.fstrings.last_mut() {
                    top.mode = next_mode;
                }
                if in_fmt {
                    self.advance_fstring_format_spec()?;
                } else {
                    self.advance_fstring_text()?;
                }
                Ok(true)
            }
            b':' if !in_fmt && *expr_brackets == 0 && !next_is_eq => {
                self.at += 1;
                if let Some(top) = self.fstrings.last_mut() {
                    top.mode = FStringMode::FormatSpec;
                }
                self.advance_fstring_format_spec()?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn handle_newline(&mut self, line: u32) {
        self.consume_newline();
        if self.bracket_depth == 0 && self.fstrings.is_empty() {
            if !self.at_logical_start {
                self.tokens.push(Token {
                    tok: Tok::Newline,
                    line,
                });
            }
            self.at_logical_start = true;
            self.line_indent = 0;
            self.line_start_num = self.line;
        }
    }

    fn run(mut self) -> Result<Vec<Token>, String> {
        while self.at < self.bytes.len() {
            if let Some(top) = self.fstrings.last() {
                match top.mode {
                    FStringMode::Text => {
                        self.advance_fstring_text()?;
                        continue;
                    }
                    FStringMode::FormatSpec => {
                        self.advance_fstring_format_spec()?;
                        continue;
                    }
                    FStringMode::Expr { .. } => {}
                }
            }

            let byte = self.bytes[self.at];
            let line = self.line;

            if self.skip_logical_line_leading() {
                continue;
            }
            if byte == b'\\' && matches!(self.peek_at(1), Some(b'\n' | b'\r')) {
                self.at += 1;
                self.consume_newline();
                continue;
            }
            if byte == b'#' {
                self.skip_comment_line();
                continue;
            }
            if matches!(byte, b' ' | b'\t' | b'\x0c') {
                self.at += 1;
                continue;
            }
            if byte == b'\n' || byte == b'\r' {
                self.handle_newline(line);
                continue;
            }
            if byte == b'\'' || byte == b'"' {
                self.open_string(byte, false, false, line)?;
                continue;
            }
            if is_ident_start(byte) {
                self.advance_ident_or_prefixed_string(line)?;
                continue;
            }
            if byte.is_ascii_digit()
                || (byte == b'.' && self.peek_at(1).is_some_and(|b| b.is_ascii_digit()))
            {
                self.advance_number(line);
                continue;
            }
            if byte == b'.' {
                self.at += 1;
                self.push(Tok::Dot, line);
                continue;
            }
            if self.advance_fstring_expr_punct(byte, line)? {
                continue;
            }
            if byte == b':' && self.peek_at(1) == Some(b'=') {
                self.at += 2;
                self.push(Tok::ColonEq, line);
                continue;
            }
            if matches!(byte, b'(' | b'[' | b'{') {
                if self.bracket_depth >= MAX_BRACKET_DEPTH {
                    return Err(format!(
                        "brackets nest past {MAX_BRACKET_DEPTH} levels at line {line}"
                    ));
                }
                self.bracket_depth += 1;
            } else if matches!(byte, b')' | b']' | b'}') {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
            }

            self.at += 1;
            self.push(Tok::Punct(byte), line);
        }

        if let Some(frame) = self.fstrings.last() {
            return Err(format!(
                "f-string opened at line {} never closes",
                frame.line
            ));
        }

        if !self.at_logical_start
            || !matches!(
                self.tokens.last().map(|t| &t.tok),
                Some(Tok::Newline) | None
            )
        {
            self.tokens.push(Token {
                tok: Tok::Newline,
                line: self.line,
            });
        }
        Ok(self.tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    TypeChecking,
    DefOrClass,
    Other,
}

struct BlockFrame {
    opener_indent: usize,
    kind: BlockKind,
}

struct Parser {
    importlib_aliases: BTreeSet<String>,
    import_module_fns: BTreeSet<String>,
    blocks: Vec<BlockFrame>,
    facts: PyFacts,
}

impl Parser {
    fn new() -> Self {
        Self {
            importlib_aliases: BTreeSet::from(["importlib".to_string()]),
            import_module_fns: BTreeSet::new(),
            blocks: Vec::new(),
            facts: PyFacts::default(),
        }
    }

    fn in_type_checking(&self) -> bool {
        self.blocks
            .iter()
            .any(|b| b.kind == BlockKind::TypeChecking)
    }

    fn in_def_or_class(&self) -> bool {
        self.blocks.iter().any(|b| b.kind == BlockKind::DefOrClass)
    }

    fn run(mut self, tokens: &[Token]) -> PyFacts {
        let mut at = 0;
        while at < tokens.len() {
            let Tok::LineStart { indent, .. } = tokens[at].tok else {
                at += 1;
                continue;
            };
            at += 1;
            let end = tokens[at..]
                .iter()
                .position(|t| matches!(t.tok, Tok::Newline))
                .map_or(tokens.len(), |offset| at + offset);
            let line_tokens = &tokens[at..end];
            at = end + 1;
            if line_tokens.is_empty() {
                continue;
            }
            while self
                .blocks
                .last()
                .is_some_and(|b| b.opener_indent >= indent)
            {
                self.blocks.pop();
            }
            self.logical_line(indent, line_tokens);
        }
        self.facts
    }

    fn logical_line(&mut self, indent: usize, line_tokens: &[Token]) {
        // Strip leading `@decorator` lines (treated separately as expression/header lines).
        if matches!(line_tokens.first().map(|t| &t.tok), Some(Tok::Punct(b'@'))) {
            self.scan_importlib_calls(
                line_tokens,
                self.in_type_checking(),
                !self.in_def_or_class(),
            );
            return;
        }

        // Check for compound header ending in `:` or single-line suite after `:`.
        if let Some(colon_idx) = find_compound_colon(line_tokens) {
            let header = &line_tokens[..colon_idx];
            let suite = &line_tokens[colon_idx + 1..];
            let kind = classify_header(header);
            self.scan_importlib_calls(header, self.in_type_checking(), !self.in_def_or_class());
            if matches!(kind, BlockKind::DefOrClass)
                && !self.in_def_or_class()
                && let Some(name) = declared_def_or_class_name(header)
            {
                self.facts.exports.insert(name);
            }
            if suite.is_empty() {
                self.blocks.push(BlockFrame {
                    opener_indent: indent,
                    kind,
                });
            } else {
                let tc = self.in_type_checking() || kind == BlockKind::TypeChecking;
                let top = !self.in_def_or_class() && kind != BlockKind::DefOrClass;
                self.simple_statements(suite, tc, top);
            }
            return;
        }

        let tc = self.in_type_checking();
        let top = !self.in_def_or_class();
        self.simple_statements(line_tokens, tc, top);
    }

    fn simple_statements(&mut self, tokens: &[Token], type_checking: bool, top_level: bool) {
        let mut start = 0;
        let mut depth = 0usize;
        for idx in 0..=tokens.len() {
            let at_sep = if idx == tokens.len() {
                true
            } else {
                match tokens[idx].tok {
                    Tok::Punct(b'(' | b'[' | b'{') => {
                        depth += 1;
                        false
                    }
                    Tok::Punct(b')' | b']' | b'}') => {
                        depth = depth.saturating_sub(1);
                        false
                    }
                    Tok::Punct(b';') if depth == 0 => true,
                    _ => false,
                }
            };
            if at_sep {
                if start < idx {
                    self.simple_statement(&tokens[start..idx], type_checking, top_level);
                }
                start = idx + 1;
            }
        }
    }

    fn simple_statement(&mut self, tokens: &[Token], type_checking: bool, top_level: bool) {
        if tokens.is_empty() {
            return;
        }
        let line = tokens[0].line;
        let kind_override = type_checking.then_some(EdgeKind::ImportType);

        // `import a, b.c as d`
        if matches!(&tokens[0].tok, Tok::Ident(word) if word == "import")
            && parse_import_stmt(
                &tokens[1..],
                line,
                type_checking,
                top_level,
                &mut self.importlib_aliases,
                &mut self.facts,
                kind_override,
            )
        {
            return;
        }

        // `from [. ...]pkg.mod import (...)`
        if matches!(&tokens[0].tok, Tok::Ident(word) if word == "from")
            && parse_from_stmt(
                &tokens[1..],
                line,
                type_checking,
                top_level,
                &mut self.import_module_fns,
                &mut self.facts,
                kind_override,
            )
        {
            return;
        }

        // Top-level assignments and `__all__`.
        if top_level {
            self.record_top_level_assignment(tokens);
        }

        self.scan_importlib_calls(tokens, type_checking, top_level);
    }

    fn record_top_level_assignment(&mut self, tokens: &[Token]) {
        let Some(Tok::Ident(name)) = tokens.first().map(|t| &t.tok) else {
            return;
        };
        if matches!(
            name.as_str(),
            "return" | "yield" | "raise" | "del" | "assert" | "global" | "nonlocal" | "pass"
        ) {
            return;
        }
        // Scan for top-level assignment `=` or annotated `name: T = ...` or `__all__ += ...`.
        let mut depth = 0usize;
        let mut eq_idx = None;
        let mut is_aug_all = false;
        let mut valid_lhs = true;
        for (idx, token) in tokens.iter().enumerate().skip(1) {
            match &token.tok {
                Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
                Tok::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                Tok::Punct(b'=') if depth == 0 => {
                    // Reject `==`, `!=`, `<=`, `>=`.
                    let prev_punct = tokens.get(idx - 1).and_then(|t| match t.tok {
                        Tok::Punct(p) => Some(p),
                        _ => None,
                    });
                    if matches!(prev_punct, Some(b'=' | b'!' | b'<' | b'>')) {
                        valid_lhs = false;
                        break;
                    }
                    is_aug_all = name == "__all__" && prev_punct == Some(b'+');
                    if idx == 1
                        || (idx > 1 && tokens.get(1).is_some_and(|t| t.tok == Tok::Punct(b':')))
                        || is_aug_all
                    {
                        eq_idx = Some(idx);
                    }
                    break;
                }
                Tok::ColonEq if depth == 0 => {
                    eq_idx = Some(idx);
                    break;
                }
                Tok::Punct(b'.') if idx == 1 && depth == 0 => {
                    // Attribute mutation `x.y = ...` does not define a module symbol.
                    valid_lhs = false;
                    break;
                }
                _ => {}
            }
        }
        if !valid_lhs {
            return;
        }
        let Some(eq_at) = eq_idx else {
            // Bare top-level annotation `x: int` without `=`.
            if tokens.get(1).is_some_and(|t| t.tok == Tok::Punct(b':')) {
                self.facts.exports.insert(name.clone());
            }
            return;
        };
        self.facts.exports.insert(name.clone());
        if name == "__all__"
            && (eq_at == 1
                || is_aug_all
                || tokens.get(1).is_some_and(|t| t.tok == Tok::Punct(b':')))
        {
            let rhs = &tokens[eq_at + 1..];
            collect_static_all(rhs, &mut self.facts.explicit_all);
        }
    }

    fn scan_importlib_calls(&mut self, tokens: &[Token], type_checking: bool, top_level: bool) {
        let mut idx = 0;
        while idx < tokens.len() {
            let call_open = if let (
                Some(Tok::Ident(obj)),
                Some(Tok::Dot),
                Some(Tok::Ident(attr)),
                Some(Tok::Punct(b'(')),
            ) = (
                tokens.get(idx).map(|t| &t.tok),
                tokens.get(idx + 1).map(|t| &t.tok),
                tokens.get(idx + 2).map(|t| &t.tok),
                tokens.get(idx + 3).map(|t| &t.tok),
            ) && attr == "import_module"
                && self.importlib_aliases.contains(obj)
            {
                let not_attr = idx == 0 || tokens[idx - 1].tok != Tok::Dot;
                not_attr.then_some((idx + 3, tokens[idx].line))
            } else if let (Some(Tok::Ident(fn_name)), Some(Tok::Punct(b'('))) = (
                tokens.get(idx).map(|t| &t.tok),
                tokens.get(idx + 1).map(|t| &t.tok),
            ) && self.import_module_fns.contains(fn_name)
                && (idx == 0 || tokens[idx - 1].tok != Tok::Dot)
            {
                Some((idx + 1, tokens[idx].line))
            } else {
                None
            };

            let Some((paren_open, line)) = call_open else {
                idx += 1;
                continue;
            };
            let Some(paren_close) = find_matching_paren(tokens, paren_open) else {
                self.facts.dynamic_unresolved.push(line);
                return;
            };
            let args = split_top_level_commas(&tokens[paren_open + 1..paren_close]);
            self.handle_importlib_args(&args, line, type_checking, top_level);
            // Continue scanning nested expressions inside or after the call.
            idx = paren_open + 1;
        }
    }

    fn handle_importlib_args(
        &mut self,
        args: &[&[Token]],
        line: u32,
        type_checking: bool,
        top_level: bool,
    ) {
        let mut pos_name: Option<&[Token]> = None;
        let mut pos_pkg: Option<&[Token]> = None;
        let mut kw_name: Option<&[Token]> = None;
        let mut kw_pkg: Option<&[Token]> = None;
        let mut pos_idx = 0usize;

        for arg in args {
            if let Some(Tok::Punct(b'*')) = arg.first().map(|t| &t.tok) {
                if pos_idx == 0 && kw_name.is_none() {
                    self.facts.dynamic_unresolved.push(line);
                    return;
                }
                pos_idx += 1;
                continue;
            }
            if let (Some(Tok::Ident(k)), Some(Tok::Punct(b'='))) =
                (arg.first().map(|t| &t.tok), arg.get(1).map(|t| &t.tok))
            {
                let val = &arg[2..];
                if k == "name" {
                    kw_name = Some(val);
                } else if k == "package" {
                    kw_pkg = Some(val);
                }
            } else {
                if pos_idx == 0 {
                    pos_name = Some(arg);
                } else if pos_idx == 1 {
                    pos_pkg = Some(arg);
                }
                pos_idx += 1;
            }
        }

        let Some(name_expr) = kw_name.or(pos_name) else {
            self.facts.dynamic_unresolved.push(line);
            return;
        };
        let Some(spec) = folded_str_literal(name_expr) else {
            self.facts.dynamic_unresolved.push(line);
            return;
        };
        if spec.is_empty() {
            self.facts.dynamic_unresolved.push(line);
            return;
        }

        let dots = spec.bytes().take_while(|b| *b == b'.').count();
        let rest = &spec[dots..];
        let package_arg = if dots > 0 {
            let Some(pkg_expr) = kw_pkg.or(pos_pkg) else {
                self.facts.dynamic_unresolved.push(line);
                return;
            };
            if let Some(pkg) = folded_str_literal(pkg_expr) {
                if pkg.is_empty() {
                    self.facts.dynamic_unresolved.push(line);
                    return;
                }
                Some(pkg)
            } else if matches!(
                pkg_expr,
                [Token { tok: Tok::Ident(w), .. }] if w == "__package__" || w == "__name__"
            ) {
                None
            } else {
                self.facts.dynamic_unresolved.push(line);
                return;
            }
        } else {
            None
        };

        self.facts.imports.push(PyImport {
            level: dots,
            module: rest.to_string(),
            names: Vec::new(),
            kind: if type_checking {
                EdgeKind::ImportType
            } else {
                EdgeKind::DynamicImport
            },
            line,
            type_checking,
            top_level,
            package_arg,
        });
    }
}

/// Parse a `from <dots><module> import ...` statement slice (after `from`).
fn parse_from_stmt(
    tokens: &[Token],
    line: u32,
    type_checking: bool,
    top_level: bool,
    import_module_fns: &mut BTreeSet<String>,
    facts: &mut PyFacts,
    kind_override: Option<EdgeKind>,
) -> bool {
    let mut at = 0usize;
    let mut level = 0usize;
    while at < tokens.len() && tokens[at].tok == Tok::Dot {
        level += 1;
        at += 1;
    }

    let mut module_parts: Vec<&str> = Vec::new();
    while at < tokens.len() {
        match &tokens[at].tok {
            Tok::Ident(word) if word == "import" && (level > 0 || !module_parts.is_empty()) => {
                break;
            }
            Tok::Ident(seg) => {
                module_parts.push(seg.as_str());
                at += 1;
                if at < tokens.len() && tokens[at].tok == Tok::Dot {
                    at += 1;
                } else {
                    break;
                }
            }
            _ => return false,
        }
    }

    if at >= tokens.len() || !matches!(&tokens[at].tok, Tok::Ident(w) if w == "import") {
        return false;
    }
    at += 1;
    let mut list = &tokens[at..];
    if list.first().is_some_and(|t| t.tok == Tok::Punct(b'('))
        && list.last().is_some_and(|t| t.tok == Tok::Punct(b')'))
    {
        list = &list[1..list.len() - 1];
    }

    let mut names = Vec::new();
    if list.len() == 1 && list[0].tok == Tok::Punct(b'*') {
        names.push(PyImported::Star);
    } else {
        let mut cursor = 0usize;
        while cursor < list.len() {
            let Some(Tok::Ident(orig)) = list.get(cursor).map(|t| &t.tok) else {
                if list.get(cursor).is_some_and(|t| t.tok == Tok::Punct(b',')) {
                    cursor += 1;
                    continue;
                }
                return false;
            };
            cursor += 1;
            let (alias, explicit_alias) = if matches!(
                list.get(cursor).map(|t| &t.tok),
                Some(Tok::Ident(kw)) if kw == "as"
            ) {
                let Some(Tok::Ident(as_name)) = list.get(cursor + 1).map(|t| &t.tok) else {
                    return false;
                };
                cursor += 2;
                (as_name.clone(), true)
            } else {
                (orig.clone(), false)
            };
            names.push(PyImported::Named {
                orig: orig.clone(),
                alias,
                explicit_alias,
            });
            match list.get(cursor).map(|t| &t.tok) {
                Some(Tok::Punct(b',')) => cursor += 1,
                None => break,
                _ => return false,
            }
        }
    }

    if names.is_empty() {
        return false;
    }

    let module = module_parts.join(".");
    if level == 0 && module == "importlib" {
        for item in &names {
            if let PyImported::Named { orig, alias, .. } = item
                && orig == "import_module"
            {
                import_module_fns.insert(alias.clone());
            }
        }
    }

    facts.imports.push(PyImport {
        level,
        module,
        names,
        kind: kind_override.unwrap_or(EdgeKind::PyFrom),
        line,
        type_checking,
        top_level,
        package_arg: None,
    });
    true
}

/// Parse an `import a, b.c as d` statement slice (after `import`).
fn parse_import_stmt(
    tokens: &[Token],
    line: u32,
    type_checking: bool,
    top_level: bool,
    importlib_aliases: &mut BTreeSet<String>,
    facts: &mut PyFacts,
    kind_override: Option<EdgeKind>,
) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let mut at = 0usize;
    let mut parsed_any = false;
    while at < tokens.len() {
        let mut parts: Vec<&str> = Vec::new();
        while at < tokens.len() {
            let Some(Tok::Ident(seg)) = tokens.get(at).map(|t| &t.tok) else {
                return false;
            };
            if seg == "as" && parts.is_empty() {
                return false;
            }
            parts.push(seg.as_str());
            at += 1;
            if at < tokens.len() && tokens[at].tok == Tok::Dot {
                at += 1;
            } else {
                break;
            }
        }
        if parts.is_empty() {
            return false;
        }
        let module = parts.join(".");
        let _bound_name = if matches!(tokens.get(at).map(|t| &t.tok), Some(Tok::Ident(kw)) if kw == "as")
        {
            let Some(Tok::Ident(alias)) = tokens.get(at + 1).map(|t| &t.tok) else {
                return false;
            };
            at += 2;
            if module == "importlib" {
                importlib_aliases.insert(alias.clone());
            }
            alias.clone()
        } else {
            parts[0].to_string()
        };
        facts.imports.push(PyImport {
            level: 0,
            module,
            names: Vec::new(),
            kind: kind_override.unwrap_or(EdgeKind::PyImport),
            line,
            type_checking,
            top_level,
            package_arg: None,
        });
        parsed_any = true;
        match tokens.get(at).map(|t| &t.tok) {
            Some(Tok::Punct(b',')) => at += 1,
            None => break,
            _ => return false,
        }
    }
    parsed_any
}

fn find_compound_colon(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    let first = match tokens.first().map(|t| &t.tok) {
        Some(Tok::Ident(w)) => w.as_str(),
        _ => return None,
    };
    let is_compound = matches!(
        first,
        "if" | "elif"
            | "else"
            | "for"
            | "async"
            | "while"
            | "with"
            | "try"
            | "except"
            | "finally"
            | "def"
            | "class"
            | "match"
            | "case"
    );
    if !is_compound {
        return None;
    }
    for (idx, token) in tokens.iter().enumerate() {
        match token.tok {
            Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
            Tok::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            Tok::Punct(b':') if depth == 0 => {
                if idx == 1 && !matches!(first, "else" | "try" | "finally" | "except") {
                    return None;
                }
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

fn strip_outer_parens(mut slice: &[Token]) -> &[Token] {
    while slice.len() >= 2
        && slice[0].tok == Tok::Punct(b'(')
        && find_matching_paren(slice, 0) == Some(slice.len() - 1)
    {
        slice = &slice[1..slice.len() - 1];
    }
    slice
}

fn is_type_checking_expr(expr: &[Token]) -> bool {
    let expr = strip_outer_parens(expr);
    if expr.is_empty() {
        return false;
    }
    // `if TYPE_CHECKING:` / `if (TYPE_CHECKING):` / `if typing.TYPE_CHECKING:` / `if TYPE_CHECKING and ...`
    for raw_clause in split_on_ident_and(expr) {
        let clause = strip_outer_parens(raw_clause);
        match clause {
            [
                Token {
                    tok: Tok::Ident(w), ..
                },
            ] if w == "TYPE_CHECKING" => return true,
            [
                Token {
                    tok: Tok::Ident(_), ..
                },
                Token { tok: Tok::Dot, .. },
                Token {
                    tok: Tok::Ident(w), ..
                },
            ] if w == "TYPE_CHECKING" => return true,
            _ => {}
        }
    }
    false
}

fn split_on_ident_and(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (idx, token) in tokens.iter().enumerate() {
        match &token.tok {
            Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
            Tok::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            Tok::Ident(w) if depth == 0 && w == "and" => {
                out.push(&tokens[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    out.push(&tokens[start..]);
    out
}

fn classify_header(header: &[Token]) -> BlockKind {
    let first = match header.first().map(|t| &t.tok) {
        Some(Tok::Ident(w)) => w.as_str(),
        _ => return BlockKind::Other,
    };
    match first {
        "if" | "elif" if is_type_checking_expr(&header[1..]) => BlockKind::TypeChecking,
        "def" | "class" => BlockKind::DefOrClass,
        "async" if matches!(header.get(1).map(|t| &t.tok), Some(Tok::Ident(w)) if w == "def") => {
            BlockKind::DefOrClass
        }
        _ => BlockKind::Other,
    }
}

fn declared_def_or_class_name(header: &[Token]) -> Option<String> {
    match (
        header.first().map(|t| &t.tok),
        header.get(1).map(|t| &t.tok),
        header.get(2).map(|t| &t.tok),
    ) {
        (Some(Tok::Ident(kw)), Some(Tok::Ident(name)), _) if kw == "def" || kw == "class" => {
            Some(name.clone())
        }
        (Some(Tok::Ident(a)), Some(Tok::Ident(d)), Some(Tok::Ident(name)))
            if a == "async" && d == "def" =>
        {
            Some(name.clone())
        }
        _ => None,
    }
}

fn collect_static_all(rhs: &[Token], explicit_all: &mut BTreeSet<String>) {
    let mut slice = rhs;
    if slice.len() >= 2
        && matches!(slice[0].tok, Tok::Punct(b'[' | b'('))
        && matches!(slice[slice.len() - 1].tok, Tok::Punct(b']' | b')'))
    {
        slice = &slice[1..slice.len() - 1];
    } else {
        return;
    }
    for item in split_top_level_commas(slice) {
        if let Some(name) = folded_str_literal(item)
            && !name.is_empty()
        {
            explicit_all.insert(name);
        }
    }
}

fn folded_str_literal(tokens: &[Token]) -> Option<String> {
    let slice = strip_outer_parens(tokens);
    if slice.is_empty() {
        return None;
    }
    let mut out = String::new();
    for token in slice {
        match &token.tok {
            Tok::Str(s) => out.push_str(s),
            _ => return None,
        }
    }
    Some(out)
}

fn find_matching_paren(tokens: &[Token], open_at: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, token) in tokens.iter().enumerate().skip(open_at) {
        match token.tok {
            Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
            Tok::Punct(b')' | b']' | b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (token.tok == Tok::Punct(b')')).then_some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, token) in tokens.iter().enumerate() {
        match token.tok {
            Tok::Punct(b'(' | b'[' | b'{') => depth += 1,
            Tok::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            Tok::Punct(b',') if depth == 0 => {
                if start < idx {
                    out.push(&tokens[start..idx]);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        out.push(&tokens[start..]);
    }
    out
}

/// Extract static module imports, exports, and dynamic unresolved lines from a
/// Python source or `.pyi` stub.
pub fn parse(text: &str) -> Result<PyFacts, String> {
    if text.len() as u64 > crate::MAX_SOURCE_BYTES {
        return Err(format!(
            "source exceeds MAX_SOURCE_BYTES ({} bytes)",
            crate::MAX_SOURCE_BYTES
        ));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let tokens = Lexer::new(text.as_bytes()).run()?;
    Ok(Parser::new().run(&tokens))
}
