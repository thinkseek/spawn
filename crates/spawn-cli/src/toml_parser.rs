//! Zero-dependency, pure safe-Rust TOML v1.0 subset parser returning
//! [`serde_json::Value`] with `line:col` error diagnostics.
//!
//! Supports standard tables (`[section]`, `[section.sub]`), arrays of tables
//! (`[[array_of_tables]]`), inline tables (`{ a = 1, b = "x" }`), single-line
//! and multiline arrays (`[a, b]`), basic (`"..."`) and multiline basic
//! (`"""..."""`) strings with escapes, literal (`'...'`) and multiline literal
//! (`'''...'''`) strings, integers (decimal with underscores/signs, hex, octal,
//! binary), floats, booleans, full-line and trailing `#` comments, dotted keys,
//! and duplicate key/table conflict detection.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Number, Value};

/// A syntax or structure error located at a 1-based `line` and `col`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Maximum nesting depth permitted for arrays and inline tables.
const MAX_DEPTH: usize = 64;

/// Parse a TOML document into a [`serde_json::Value::Object`].
pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser::new(input);
    parser.parse_document()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableOrigin {
    Root,
    Header,
    Implicit,
    Dotted,
    Inline,
    ArrayElement,
}

#[derive(Debug)]
enum TomlNode {
    Table {
        origin: TableOrigin,
        touched_by_dotted: bool,
        entries: BTreeMap<String, TomlNode>,
    },
    ArrayOfTables(Vec<TomlNode>),
    Value(Value),
}

impl TomlNode {
    fn new_table(origin: TableOrigin) -> Self {
        Self::Table {
            origin,
            touched_by_dotted: false,
            entries: BTreeMap::new(),
        }
    }

    fn into_json(self) -> Value {
        match self {
            Self::Table { entries, .. } => {
                let mut map = Map::new();
                for (key, node) in entries {
                    map.insert(key, node.into_json());
                }
                Value::Object(map)
            }
            Self::ArrayOfTables(items) => {
                Value::Array(items.into_iter().map(Self::into_json).collect())
            }
            Self::Value(value) => value,
        }
    }
}

/// Identifies a table currently open for key-value assignments.
#[derive(Debug, Clone)]
enum TableStep {
    Key(String),
    LastElement(String),
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    line: usize,
    col: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            line: 1,
            col: 1,
            depth: 0,
        }
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            line: self.line,
            col: self.col,
            message: message.into(),
        }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.remaining().starts_with(prefix)
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn skip_horizontal_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.advance();
        }
    }

    fn skip_comment(&mut self) -> Result<(), ParseError> {
        if self.peek() == Some('#') {
            self.advance();
            while let Some(ch) = self.peek() {
                if ch == '\n' || ch == '\r' {
                    break;
                }
                if is_disallowed_control(ch) {
                    return Err(self.err("control character is not allowed in comment"));
                }
                self.advance();
            }
        }
        Ok(())
    }

    fn consume_newline(&mut self) -> Result<bool, ParseError> {
        match self.peek() {
            Some('\n') => {
                self.advance();
                Ok(true)
            }
            Some('\r') => {
                let line = self.line;
                let col = self.col;
                if self.starts_with("\r\n") {
                    self.advance();
                    self.advance();
                    Ok(true)
                } else {
                    Err(err_at(
                        line,
                        col,
                        "bare carriage return is not valid in TOML",
                    ))
                }
            }
            _ => Ok(false),
        }
    }

    fn skip_ws_newlines_and_comments(&mut self) -> Result<(), ParseError> {
        loop {
            self.skip_horizontal_ws();
            self.skip_comment()?;
            if !self.consume_newline()? {
                break;
            }
        }
        Ok(())
    }

    fn expect_line_end(&mut self) -> Result<(), ParseError> {
        self.skip_horizontal_ws();
        self.skip_comment()?;
        if self.peek().is_none() {
            return Ok(());
        }
        if self.consume_newline()? {
            return Ok(());
        }
        let found = self.peek().map_or_else(
            || "end of input".to_string(),
            |ch| format!("unexpected character '{ch}'"),
        );
        Err(self.err(format!("expected newline or comment, found {found}")))
    }

    fn parse_document(&mut self) -> Result<Value, ParseError> {
        let mut root = TomlNode::new_table(TableOrigin::Root);
        let mut current_path: Vec<TableStep> = Vec::new();

        loop {
            self.skip_ws_newlines_and_comments()?;
            let Some(ch) = self.peek() else {
                break;
            };
            if ch == '[' {
                current_path = self.parse_table_header(&mut root)?;
            } else {
                let key_line = self.line;
                let key_col = self.col;
                let key_path = self.parse_dotted_key()?;
                self.skip_horizontal_ws();
                if self.peek() != Some('=') {
                    return Err(self.err("expected '=' after key"));
                }
                self.advance();
                self.skip_horizontal_ws();
                let value = self.parse_value()?;
                self.expect_line_end()?;
                let target = resolve_table_mut(&mut root, &current_path)
                    .map_err(|msg| err_at(key_line, key_col, msg))?;
                insert_dotted_key(target, &key_path, value, key_line, key_col)?;
            }
        }

        Ok(root.into_json())
    }

    fn parse_table_header(&mut self, root: &mut TomlNode) -> Result<Vec<TableStep>, ParseError> {
        let header_line = self.line;
        let header_col = self.col;
        self.advance(); // consume first '['
        let is_array = if self.peek() == Some('[') {
            self.advance();
            true
        } else {
            false
        };
        self.skip_horizontal_ws();
        let path = self.parse_dotted_key()?;
        self.skip_horizontal_ws();
        if is_array {
            if !self.starts_with("]]") {
                return Err(self.err("expected ']]' to close array-of-tables header"));
            }
            self.advance();
            self.advance();
        } else {
            if self.peek() != Some(']') {
                return Err(self.err("expected ']' to close table header"));
            }
            self.advance();
        }
        self.expect_line_end()?;

        if is_array {
            open_array_of_tables(root, &path, header_line, header_col)
        } else {
            open_table_header(root, &path, header_line, header_col)
        }
    }

    fn parse_dotted_key(&mut self) -> Result<Vec<String>, ParseError> {
        let mut segments = Vec::new();
        loop {
            self.skip_horizontal_ws();
            segments.push(self.parse_simple_key()?);
            self.skip_horizontal_ws();
            if self.peek() == Some('.') {
                self.advance();
            } else {
                break;
            }
        }
        Ok(segments)
    }

    fn parse_simple_key(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some('"') => {
                if self.starts_with("\"\"\"") {
                    return Err(self.err("multiline string cannot be used as a key"));
                }
                self.parse_basic_string()
            }
            Some('\'') => {
                if self.starts_with("'''") {
                    return Err(self.err("multiline literal string cannot be used as a key"));
                }
                self.parse_literal_string()
            }
            Some(ch) if is_bare_key_char(ch) => {
                let mut key = String::new();
                while let Some(next) = self.peek() {
                    if is_bare_key_char(next) {
                        key.push(next);
                        self.advance();
                    } else {
                        break;
                    }
                }
                Ok(key)
            }
            Some(other) => Err(self.err(format!("expected key, found '{other}'"))),
            None => Err(self.err("expected key, found end of input")),
        }
    }

    fn parse_nested<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.err(format!("maximum nesting depth ({MAX_DEPTH}) exceeded")));
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn parse_value(&mut self) -> Result<TomlNode, ParseError> {
        match self.peek() {
            Some('"') => {
                let s = if self.starts_with("\"\"\"") {
                    self.parse_multiline_basic_string()?
                } else {
                    self.parse_basic_string()?
                };
                Ok(TomlNode::Value(Value::String(s)))
            }
            Some('\'') => {
                let s = if self.starts_with("'''") {
                    self.parse_multiline_literal_string()?
                } else {
                    self.parse_literal_string()?
                };
                Ok(TomlNode::Value(Value::String(s)))
            }
            Some('[') => self.parse_nested(Self::parse_array).map(TomlNode::Value),
            Some('{') => self.parse_nested(Self::parse_inline_table),
            Some('t' | 'f') => self.parse_boolean().map(TomlNode::Value),
            Some('+' | '-' | '0'..='9') => self.parse_number().map(TomlNode::Value),
            Some(other) => Err(self.err(format!("unexpected token '{other}' while parsing value"))),
            None => Err(self.err("expected value, found end of input")),
        }
    }

    fn parse_basic_string(&mut self) -> Result<String, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance(); // opening '"'
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(err_at(start_line, start_col, "unterminated basic string"));
                }
                Some('"') => {
                    self.advance();
                    return Ok(out);
                }
                Some('\n' | '\r') => {
                    return Err(self.err("newline is not allowed in single-line basic string"));
                }
                Some('\\') => {
                    out.push(self.parse_escape()?);
                }
                Some(ch) if is_disallowed_control(ch) => {
                    return Err(self.err("control character is not allowed in string"));
                }
                Some(ch) => {
                    self.advance();
                    out.push(ch);
                }
            }
        }
    }

    fn parse_multiline_basic_string(&mut self) -> Result<String, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance();
        self.advance();
        self.advance();
        let _ = self.consume_newline()?;
        let mut out = String::new();
        loop {
            if self.starts_with("\"\"\"") {
                self.advance();
                self.advance();
                self.advance();
                // Up to 2 additional quotes immediately before closing delimiter.
                let mut extra = 0;
                while self.peek() == Some('"') && extra < 2 {
                    self.advance();
                    out.push('"');
                    extra += 1;
                }
                if self.peek() == Some('"') {
                    return Err(self.err("too many consecutive quotes in multiline basic string"));
                }
                return Ok(out);
            }
            match self.peek() {
                None => {
                    return Err(err_at(
                        start_line,
                        start_col,
                        "unterminated multiline basic string",
                    ));
                }
                Some('\r') => {
                    self.consume_newline()?;
                    out.push('\n');
                }
                Some('\\') => {
                    if self.is_line_ending_backslash() {
                        self.advance(); // '\'
                        self.skip_horizontal_ws();
                        self.consume_newline()?;
                        loop {
                            self.skip_horizontal_ws();
                            if !self.consume_newline()? {
                                break;
                            }
                        }
                    } else {
                        out.push(self.parse_escape()?);
                    }
                }
                Some(ch) if ch != '\n' && is_disallowed_control(ch) => {
                    return Err(self.err("control character is not allowed in multiline string"));
                }
                Some(ch) => {
                    self.advance();
                    out.push(ch);
                }
            }
        }
    }

    fn is_line_ending_backslash(&self) -> bool {
        let rest = &self.remaining()[1..];
        for ch in rest.chars() {
            match ch {
                ' ' | '\t' => {}
                '\n' | '\r' => return true,
                _ => return false,
            }
        }
        false
    }

    fn parse_escape(&mut self) -> Result<char, ParseError> {
        let esc_line = self.line;
        let esc_col = self.col;
        self.advance(); // consume '\'
        let Some(code) = self.advance() else {
            return Err(err_at(esc_line, esc_col, "unterminated escape sequence"));
        };
        match code {
            'b' => Ok('\u{0008}'),
            't' => Ok('\t'),
            'n' => Ok('\n'),
            'f' => Ok('\u{000C}'),
            'r' => Ok('\r'),
            '"' => Ok('"'),
            '\\' => Ok('\\'),
            'u' => self.parse_unicode_escape(4, esc_line, esc_col),
            'U' => self.parse_unicode_escape(8, esc_line, esc_col),
            other => Err(err_at(
                esc_line,
                esc_col,
                format!("invalid escape sequence '\\{other}'"),
            )),
        }
    }

    fn parse_unicode_escape(
        &mut self,
        digits: usize,
        esc_line: usize,
        esc_col: usize,
    ) -> Result<char, ParseError> {
        let mut hex = String::with_capacity(digits);
        for _ in 0..digits {
            let Some(ch) = self.peek() else {
                return Err(err_at(esc_line, esc_col, "incomplete unicode escape"));
            };
            if !ch.is_ascii_hexdigit() {
                return Err(err_at(
                    esc_line,
                    esc_col,
                    format!("invalid hex digit '{ch}' in unicode escape"),
                ));
            }
            hex.push(ch);
            self.advance();
        }
        let value = u32::from_str_radix(&hex, 16)
            .map_err(|_| err_at(esc_line, esc_col, "invalid unicode scalar"))?;
        char::from_u32(value).ok_or_else(|| {
            err_at(
                esc_line,
                esc_col,
                format!("unicode codepoint U+{hex} is not a valid scalar value"),
            )
        })
    }

    fn parse_literal_string(&mut self) -> Result<String, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance(); // opening '\''
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(err_at(start_line, start_col, "unterminated literal string"));
                }
                Some('\'') => {
                    self.advance();
                    return Ok(out);
                }
                Some('\n' | '\r') => {
                    return Err(self.err("newline is not allowed in single-line literal string"));
                }
                Some(ch) if is_disallowed_control(ch) => {
                    return Err(self.err("control character is not allowed in literal string"));
                }
                Some(ch) => {
                    self.advance();
                    out.push(ch);
                }
            }
        }
    }

    fn parse_multiline_literal_string(&mut self) -> Result<String, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance();
        self.advance();
        self.advance();
        let _ = self.consume_newline()?;
        let mut out = String::new();
        loop {
            if self.starts_with("'''") {
                self.advance();
                self.advance();
                self.advance();
                let mut extra = 0;
                while self.peek() == Some('\'') && extra < 2 {
                    self.advance();
                    out.push('\'');
                    extra += 1;
                }
                if self.peek() == Some('\'') {
                    return Err(self.err("too many consecutive quotes in multiline literal string"));
                }
                return Ok(out);
            }
            match self.peek() {
                None => {
                    return Err(err_at(
                        start_line,
                        start_col,
                        "unterminated multiline literal string",
                    ));
                }
                Some('\r') => {
                    self.consume_newline()?;
                    out.push('\n');
                }
                Some(ch) if ch != '\n' && is_disallowed_control(ch) => {
                    return Err(
                        self.err("control character is not allowed in multiline literal string")
                    );
                }
                Some(ch) => {
                    self.advance();
                    out.push(ch);
                }
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws_newlines_and_comments()?;
            if self.peek() == Some(']') {
                self.advance();
                return Ok(Value::Array(items));
            }
            if self.peek().is_none() {
                return Err(err_at(start_line, start_col, "unterminated array"));
            }
            let value = self.parse_value()?.into_json();
            items.push(value);
            self.skip_ws_newlines_and_comments()?;
            match self.peek() {
                Some(',') => {
                    self.advance();
                }
                Some(']') => {
                    self.advance();
                    return Ok(Value::Array(items));
                }
                Some(other) => {
                    return Err(self.err(format!("expected ',' or ']' in array, found '{other}'")));
                }
                None => {
                    return Err(err_at(start_line, start_col, "unterminated array"));
                }
            }
        }
    }

    fn parse_inline_table(&mut self) -> Result<TomlNode, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        self.advance(); // '{'
        let mut table = TomlNode::new_table(TableOrigin::Inline);
        self.skip_horizontal_ws();
        if self.peek() == Some('}') {
            self.advance();
            return Ok(table);
        }
        loop {
            self.skip_horizontal_ws();
            if matches!(self.peek(), Some('\n' | '\r')) {
                return Err(self.err("newline is not allowed inside inline table in TOML 1.0"));
            }
            let key_line = self.line;
            let key_col = self.col;
            let key_path = self.parse_dotted_key()?;
            self.skip_horizontal_ws();
            if self.peek() != Some('=') {
                return Err(self.err("expected '=' after key in inline table"));
            }
            self.advance();
            self.skip_horizontal_ws();
            let value = self.parse_value()?;
            insert_dotted_key(&mut table, &key_path, value, key_line, key_col)?;
            self.skip_horizontal_ws();
            match self.peek() {
                Some(',') => {
                    self.advance();
                    self.skip_horizontal_ws();
                    if self.peek() == Some('}') {
                        return Err(
                            self.err("trailing comma is not allowed in inline table in TOML 1.0")
                        );
                    }
                }
                Some('}') => {
                    self.advance();
                    return Ok(table);
                }
                Some('\n' | '\r') => {
                    return Err(self.err("newline is not allowed inside inline table in TOML 1.0"));
                }
                Some(other) => {
                    return Err(self.err(format!(
                        "expected ',' or '}}' in inline table, found '{other}'"
                    )));
                }
                None => {
                    return Err(err_at(start_line, start_col, "unterminated inline table"));
                }
            }
        }
    }

    fn parse_boolean(&mut self) -> Result<Value, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        if self.starts_with("true") {
            let after = self.remaining().chars().nth(4);
            if after.is_none_or(is_value_delimiter) {
                for _ in 0..4 {
                    self.advance();
                }
                return Ok(Value::Bool(true));
            }
        } else if self.starts_with("false") {
            let after = self.remaining().chars().nth(5);
            if after.is_none_or(is_value_delimiter) {
                for _ in 0..5 {
                    self.advance();
                }
                return Ok(Value::Bool(false));
            }
        }
        Err(err_at(
            start_line,
            start_col,
            "invalid boolean or bare value",
        ))
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start_line = self.line;
        let start_col = self.col;
        let mut raw = String::new();
        while let Some(ch) = self.peek() {
            if is_value_delimiter(ch) {
                break;
            }
            raw.push(ch);
            self.advance();
        }
        parse_number_token(&raw).map_err(|msg| err_at(start_line, start_col, msg))
    }
}

fn err_at(line: usize, col: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        col,
        message: message.into(),
    }
}

fn is_bare_key_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn is_disallowed_control(ch: char) -> bool {
    let code = ch as u32;
    (code < 0x20 && ch != '\t') || code == 0x7F
}

fn is_value_delimiter(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '#' | ',' | ']' | '}')
}

fn parse_number_token(raw: &str) -> Result<Value, String> {
    if raw.is_empty() {
        return Err("empty number".to_string());
    }
    if let Some(hex) = raw.strip_prefix("0x") {
        return parse_radix_int(hex, 16, "hexadecimal");
    }
    if let Some(oct) = raw.strip_prefix("0o") {
        return parse_radix_int(oct, 8, "octal");
    }
    if let Some(bin) = raw.strip_prefix("0b") {
        return parse_radix_int(bin, 2, "binary");
    }
    if raw.starts_with("+0x")
        || raw.starts_with("-0x")
        || raw.starts_with("+0o")
        || raw.starts_with("-0o")
        || raw.starts_with("+0b")
        || raw.starts_with("-0b")
    {
        return Err(format!("prefixed integers cannot have a sign: {raw}"));
    }

    let is_float = raw.contains('.') || raw.contains('e') || raw.contains('E');
    if is_float {
        parse_float_token(raw)
    } else {
        parse_decimal_int_token(raw)
    }
}

fn parse_radix_int(digits: &str, radix: u32, kind: &str) -> Result<Value, String> {
    let clean = strip_and_validate_underscores(digits, |ch| ch.is_digit(radix))
        .ok_or_else(|| format!("invalid {kind} integer literal '{digits}'"))?;
    let val = i64::from_str_radix(&clean, radix)
        .map_err(|_| format!("{kind} integer out of i64 range: '{digits}'"))?;
    Ok(Value::Number(Number::from(val)))
}

fn parse_decimal_int_token(raw: &str) -> Result<Value, String> {
    let (sign, unsigned) = if let Some(rest) = raw.strip_prefix('+') {
        ("", rest)
    } else if let Some(rest) = raw.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", raw)
    };
    if unsigned.starts_with('0') && unsigned.len() > 1 {
        return Err(format!("leading zeros are not allowed in integer '{raw}'"));
    }
    let clean = strip_and_validate_underscores(unsigned, |ch| ch.is_ascii_digit())
        .ok_or_else(|| format!("invalid integer literal '{raw}'"))?;
    let signed = format!("{sign}{clean}");
    let val: i64 = signed
        .parse()
        .map_err(|_| format!("integer out of i64 range: '{raw}'"))?;
    Ok(Value::Number(Number::from(val)))
}

fn parse_float_token(raw: &str) -> Result<Value, String> {
    let (sign, unsigned) = if let Some(rest) = raw.strip_prefix('+') {
        ("", rest)
    } else if let Some(rest) = raw.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", raw)
    };
    let (mantissa, exp_part) = match unsigned.find(['e', 'E']) {
        Some(idx) => (&unsigned[..idx], Some(&unsigned[idx + 1..])),
        None => (unsigned, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_p, frac_p)) => (int_p, Some(frac_p)),
        None => (mantissa, None),
    };
    if int_part.starts_with('0') && int_part.len() > 1 {
        return Err(format!("leading zeros are not allowed in float '{raw}'"));
    }
    let clean_int = strip_and_validate_underscores(int_part, |ch| ch.is_ascii_digit())
        .ok_or_else(|| format!("invalid float integer part in '{raw}'"))?;
    let mut clean = format!("{sign}{clean_int}");
    if let Some(frac) = frac_part {
        let clean_frac = strip_and_validate_underscores(frac, |ch| ch.is_ascii_digit())
            .ok_or_else(|| format!("invalid float fractional part in '{raw}'"))?;
        clean.push('.');
        clean.push_str(&clean_frac);
    }
    if let Some(exp) = exp_part {
        let (exp_sign, exp_digits) = if let Some(rest) = exp.strip_prefix('+') {
            ("+", rest)
        } else if let Some(rest) = exp.strip_prefix('-') {
            ("-", rest)
        } else {
            ("", exp)
        };
        let clean_exp = strip_and_validate_underscores(exp_digits, |ch| ch.is_ascii_digit())
            .ok_or_else(|| format!("invalid float exponent in '{raw}'"))?;
        clean.push('e');
        clean.push_str(exp_sign);
        clean.push_str(&clean_exp);
    }
    let val: f64 = clean
        .parse()
        .map_err(|_| format!("invalid float literal '{raw}'"))?;
    if !val.is_finite() {
        return Err(format!("non-finite float '{raw}' is not supported"));
    }
    Number::from_f64(val)
        .map(Value::Number)
        .ok_or_else(|| format!("float '{raw}' cannot be represented in JSON"))
}

fn strip_and_validate_underscores(s: &str, is_digit: impl Fn(char) -> bool) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_was_digit = false;
    let mut prev_was_underscore = false;
    for ch in s.chars() {
        if ch == '_' {
            if !prev_was_digit || prev_was_underscore {
                return None;
            }
            prev_was_underscore = true;
            prev_was_digit = false;
        } else if is_digit(ch) {
            out.push(ch);
            prev_was_digit = true;
            prev_was_underscore = false;
        } else {
            return None;
        }
    }
    if prev_was_underscore || out.is_empty() {
        return None;
    }
    Some(out)
}

fn resolve_table_mut<'a>(
    mut node: &'a mut TomlNode,
    steps: &[TableStep],
) -> Result<&'a mut TomlNode, String> {
    for step in steps {
        match step {
            TableStep::Key(key) => {
                let TomlNode::Table { entries, .. } = node else {
                    return Err(format!("'{key}' parent is not a table"));
                };
                node = entries
                    .get_mut(key)
                    .ok_or_else(|| format!("missing table '{key}'"))?;
            }
            TableStep::LastElement(key) => {
                let TomlNode::Table { entries, .. } = node else {
                    return Err(format!("'{key}' parent is not a table"));
                };
                let arr = entries
                    .get_mut(key)
                    .ok_or_else(|| format!("missing array of tables '{key}'"))?;
                let TomlNode::ArrayOfTables(items) = arr else {
                    return Err(format!("'{key}' is not an array of tables"));
                };
                node = items
                    .last_mut()
                    .ok_or_else(|| format!("array of tables '{key}' is empty"))?;
            }
        }
    }
    Ok(node)
}

fn descend_header_parent<'a>(
    mut current: &'a mut TomlNode,
    parents: &[String],
    full_path: &str,
    bracketed: &str,
    line: usize,
    col: usize,
) -> Result<(&'a mut TomlNode, Vec<TableStep>), ParseError> {
    let mut steps = Vec::with_capacity(parents.len() + 1);
    for seg in parents {
        let TomlNode::Table { entries, .. } = current else {
            return Err(err_at(
                line,
                col,
                format!("cannot open {bracketed} inside non-table at '{seg}'"),
            ));
        };
        if !entries.contains_key(seg) {
            entries.insert(seg.clone(), TomlNode::new_table(TableOrigin::Implicit));
        }
        match entries.get_mut(seg) {
            Some(next @ TomlNode::Table { .. }) => {
                if let TomlNode::Table {
                    origin: TableOrigin::Inline | TableOrigin::Dotted,
                    ..
                } = next
                {
                    return Err(err_at(
                        line,
                        col,
                        format!(
                            "cannot define '{bracketed}' ({full_path}) inside inline or dotted-key table '{seg}'"
                        ),
                    ));
                }
                steps.push(TableStep::Key(seg.clone()));
                current = next;
            }
            Some(TomlNode::ArrayOfTables(items)) => {
                let last = items.last_mut().ok_or_else(|| {
                    err_at(line, col, format!("array of tables '{seg}' is empty"))
                })?;
                steps.push(TableStep::LastElement(seg.clone()));
                current = last;
            }
            Some(TomlNode::Value(_)) => {
                return Err(err_at(
                    line,
                    col,
                    format!("'{seg}' is already defined as a value, not a table"),
                ));
            }
            None => unreachable!(),
        }
    }
    Ok((current, steps))
}

fn open_table_header(
    root: &mut TomlNode,
    path: &[String],
    line: usize,
    col: usize,
) -> Result<Vec<TableStep>, ParseError> {
    let (parents, last_seg) = path
        .split_at_checked(path.len().saturating_sub(1))
        .expect("non-empty path");
    let seg = &last_seg[0];
    let full = path.join(".");
    let (parent, mut steps) =
        descend_header_parent(root, parents, &full, &format!("[{full}]"), line, col)?;

    let TomlNode::Table { entries, .. } = parent else {
        return Err(err_at(
            line,
            col,
            format!("cannot open table inside non-table at '{seg}'"),
        ));
    };

    match entries.get_mut(seg) {
        None => {
            entries.insert(seg.clone(), TomlNode::new_table(TableOrigin::Header));
        }
        Some(TomlNode::Table {
            origin: origin @ TableOrigin::Implicit,
            touched_by_dotted,
            ..
        }) => {
            if *touched_by_dotted {
                return Err(err_at(
                    line,
                    col,
                    format!("table '[{full}]' was already extended with dotted keys"),
                ));
            }
            *origin = TableOrigin::Header;
        }
        Some(TomlNode::Table {
            origin: TableOrigin::Header,
            ..
        }) => {
            return Err(err_at(line, col, format!("duplicate table '[{full}]'")));
        }
        Some(TomlNode::Table {
            origin: TableOrigin::Dotted,
            ..
        }) => {
            return Err(err_at(
                line,
                col,
                format!("table '[{full}]' was already defined by dotted keys"),
            ));
        }
        Some(TomlNode::Table {
            origin: TableOrigin::Inline,
            ..
        }) => {
            return Err(err_at(
                line,
                col,
                format!("cannot reopen inline table '[{full}]'"),
            ));
        }
        Some(
            TomlNode::Table {
                origin: TableOrigin::Root | TableOrigin::ArrayElement,
                ..
            }
            | TomlNode::ArrayOfTables(_),
        ) => {
            return Err(err_at(
                line,
                col,
                format!("'[{full}]' conflicts with an array of tables of the same name"),
            ));
        }
        Some(TomlNode::Value(_)) => {
            return Err(err_at(
                line,
                col,
                format!(
                    "duplicate key '{seg}': already defined as a value, cannot define table '[{full}]'"
                ),
            ));
        }
    }
    steps.push(TableStep::Key(seg.clone()));
    Ok(steps)
}

fn open_array_of_tables(
    root: &mut TomlNode,
    path: &[String],
    line: usize,
    col: usize,
) -> Result<Vec<TableStep>, ParseError> {
    let (parents, last_seg) = path
        .split_at_checked(path.len().saturating_sub(1))
        .expect("non-empty path");
    let seg = &last_seg[0];
    let full = path.join(".");
    let (parent, mut steps) =
        descend_header_parent(root, parents, &full, &format!("[[{full}]]"), line, col)?;

    let TomlNode::Table { entries, .. } = parent else {
        return Err(err_at(
            line,
            col,
            format!("cannot open array of tables inside non-table at '{seg}'"),
        ));
    };

    match entries.get_mut(seg) {
        None => {
            entries.insert(
                seg.clone(),
                TomlNode::ArrayOfTables(vec![TomlNode::new_table(TableOrigin::ArrayElement)]),
            );
        }
        Some(TomlNode::ArrayOfTables(items)) => {
            items.push(TomlNode::new_table(TableOrigin::ArrayElement));
        }
        Some(_) => {
            return Err(err_at(
                line,
                col,
                format!(
                    "cannot define '[[{full}]]': '{seg}' is already defined as a table or value"
                ),
            ));
        }
    }
    steps.push(TableStep::LastElement(seg.clone()));
    Ok(steps)
}

fn insert_dotted_key(
    mut table: &mut TomlNode,
    path: &[String],
    value: TomlNode,
    line: usize,
    col: usize,
) -> Result<(), ParseError> {
    let last_idx = path.len().saturating_sub(1);
    for (idx, seg) in path.iter().enumerate() {
        let is_last = idx == last_idx;
        let TomlNode::Table { entries, .. } = table else {
            return Err(ParseError {
                line,
                col,
                message: format!("'{seg}' parent is not a table"),
            });
        };

        if is_last {
            if entries.contains_key(seg) {
                return Err(ParseError {
                    line,
                    col,
                    message: format!("duplicate key '{}'", path.join(".")),
                });
            }
            entries.insert(seg.clone(), value);
            return Ok(());
        }

        if !entries.contains_key(seg) {
            entries.insert(seg.clone(), TomlNode::new_table(TableOrigin::Dotted));
        }
        match entries.get_mut(seg) {
            Some(next @ TomlNode::Table { .. }) => {
                if let TomlNode::Table {
                    origin,
                    touched_by_dotted,
                    ..
                } = next
                {
                    match origin {
                        TableOrigin::Header | TableOrigin::Inline | TableOrigin::ArrayElement => {
                            return Err(ParseError {
                                line,
                                col,
                                message: format!(
                                    "cannot add dotted key '{}' to already defined table '{seg}'",
                                    path.join(".")
                                ),
                            });
                        }
                        TableOrigin::Implicit => {
                            *touched_by_dotted = true;
                        }
                        TableOrigin::Dotted | TableOrigin::Root => {}
                    }
                }
                table = next;
            }
            Some(_) => {
                return Err(ParseError {
                    line,
                    col,
                    message: format!("'{seg}' is not a table"),
                });
            }
            None => unreachable!(),
        }
    }
    Ok(())
}
