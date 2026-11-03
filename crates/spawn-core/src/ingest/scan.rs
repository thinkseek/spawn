//! Fast path for the common payload shape.
//!
//! Engine payloads are almost always flat objects of plain strings and
//! unsigned integers. This scanner reads that subset straight into the same
//! borrowed structs serde would build, without the derive and visitor layers.
//!
//! It is conservative: on anything outside the subset it returns `None` and
//! the caller parses with serde_json, so errors and edge cases behave exactly
//! as before. Outside the subset are: escapes in keys and known fields,
//! control characters in strings, `null` or another type in a known field, signed or fractional
//! numbers, leading zeros, `u64` overflow, a duplicated known field, an
//! `error` value (kept as a [`serde_json::Value`]), a missing required field,
//! any top-level value other than an object, and nesting deeper than
//! [`MAX_DEPTH`]. Whatever the scanner accepts is valid JSON that serde_json
//! maps to the same struct.

use std::borrow::Cow;

use super::{ClaudeHook, CodexItem, CodexLine, CodexUsage, Str};

/// Nesting allowed inside an ignored value; far below serde_json's 128.
const MAX_DEPTH: u32 = 32;

struct Scan<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    /// Skip JSON whitespace and return the next byte.
    fn peek(&mut self) -> Option<u8> {
        while let Some(&byte) = self.bytes.get(self.pos) {
            if !matches!(byte, b' ' | b'\n' | b'\t' | b'\r') {
                return Some(byte);
            }
            self.pos += 1;
        }
        None
    }

    fn eat(&mut self, expected: u8) -> Option<()> {
        (self.peek()? == expected).then(|| self.pos += 1)
    }

    /// Only trailing whitespace may follow the value.
    fn end(&mut self) -> Option<()> {
        self.peek().is_none().then_some(())
    }

    /// A string without escapes or control characters, borrowed.
    fn string(&mut self) -> Option<&'a str> {
        self.eat(b'"')?;
        let start = self.pos;
        loop {
            match *self.bytes.get(self.pos)? {
                b'"' => break,
                b'\\' | 0..=0x1f => return None,
                _ => self.pos += 1,
            }
        }
        let text = self.text.get(start..self.pos)?;
        self.pos += 1;
        Some(text)
    }

    /// Skip an ignored string value. Escapes are checked the way serde_json
    /// checks them when it ignores a string (`ignore_str`): a known escape
    /// letter or `\u` and four hex digits, with no surrogate check.
    fn skip_string(&mut self) -> Option<()> {
        self.eat(b'"')?;
        loop {
            match *self.bytes.get(self.pos)? {
                b'"' => {
                    self.pos += 1;
                    return Some(());
                }
                0..=0x1f => return None,
                b'\\' => {
                    self.pos += 1;
                    match *self.bytes.get(self.pos)? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            let hex = self.bytes.get(self.pos + 1..self.pos + 5)?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return None;
                            }
                            self.pos += 5;
                        }
                        _ => return None,
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    fn str(&mut self) -> Option<Str<'a>> {
        self.string().map(|text| Str(Cow::Borrowed(text)))
    }

    /// Digits of a non-negative integer without leading zeros.
    fn digits(&mut self) -> Option<&'a [u8]> {
        self.peek()?;
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        let digits = &self.bytes[start..self.pos];
        match digits {
            [] | [b'0', _, ..] => None,
            _ => Some(digits),
        }
    }

    fn uint(&mut self) -> Option<u64> {
        self.digits()?.iter().try_fold(0u64, |value, digit| {
            value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
        })
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        let end = self.pos + word.len();
        (self.bytes.get(self.pos..end)? == word).then(|| self.pos = end)
    }

    /// Validate and skip any value of the subset.
    fn skip(&mut self, depth: u32) -> Option<()> {
        match self.peek()? {
            b'"' => self.skip_string(),
            b'{' => self.object(depth, |scan, _| scan.skip(depth - 1)),
            b'[' => self.array(depth),
            b'0'..=b'9' => self.digits().map(drop),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => None,
        }
    }

    fn array(&mut self, depth: u32) -> Option<()> {
        if depth == 0 {
            return None;
        }
        self.eat(b'[')?;
        if self.eat(b']').is_some() {
            return Some(());
        }
        loop {
            self.skip(depth - 1)?;
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// An object; `field` consumes the value of each key.
    fn object(
        &mut self,
        depth: u32,
        mut field: impl FnMut(&mut Self, &'a str) -> Option<()>,
    ) -> Option<()> {
        if depth == 0 {
            return None;
        }
        self.eat(b'{')?;
        if self.eat(b'}').is_some() {
            return Some(());
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            field(self, key)?;
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }
}

/// Store a known field; a duplicate is left to serde, which rejects it.
fn set<T>(slot: &mut Option<T>, value: T) -> Option<()> {
    slot.is_none().then(|| *slot = Some(value))
}

pub(super) fn claude(payload: &str) -> Option<ClaudeHook<'_>> {
    let mut scan = Scan::new(payload);
    let mut hook_event_name = None;
    let mut session_id = None;
    let mut transcript_path = None;
    let mut tool_name = None;
    let mut tool_use_id = None;
    let mut duration_ms = None;
    let mut notification_type = None;
    let mut reason = None;
    scan.object(MAX_DEPTH, |scan, key| match key {
        "hook_event_name" => set(&mut hook_event_name, scan.str()?),
        "session_id" => set(&mut session_id, scan.str()?),
        "transcript_path" => set(&mut transcript_path, scan.str()?),
        "tool_name" => set(&mut tool_name, scan.str()?),
        "tool_use_id" => set(&mut tool_use_id, scan.str()?),
        "duration_ms" => set(&mut duration_ms, scan.uint()?),
        "notification_type" => set(&mut notification_type, scan.str()?),
        "reason" => set(&mut reason, scan.str()?),
        "error" => None,
        _ => scan.skip(MAX_DEPTH - 1),
    })?;
    scan.end()?;
    Some(ClaudeHook {
        hook_event_name: hook_event_name?,
        session_id,
        transcript_path,
        tool_name,
        tool_use_id,
        duration_ms,
        error: None,
        notification_type,
        reason,
    })
}

fn codex_usage(scan: &mut Scan<'_>) -> Option<CodexUsage> {
    let mut input_tokens = None;
    let mut output_tokens = None;
    scan.object(MAX_DEPTH - 1, |scan, key| match key {
        "input_tokens" => set(&mut input_tokens, scan.uint()?),
        "output_tokens" => set(&mut output_tokens, scan.uint()?),
        _ => scan.skip(MAX_DEPTH - 2),
    })?;
    Some(CodexUsage {
        input_tokens,
        output_tokens,
    })
}

fn codex_item<'a>(scan: &mut Scan<'a>) -> Option<CodexItem<'a>> {
    let mut id = None;
    let mut item_type = None;
    let mut status = None;
    scan.object(MAX_DEPTH - 1, |scan, key| match key {
        "id" => set(&mut id, scan.str()?),
        "type" => set(&mut item_type, scan.str()?),
        "status" => set(&mut status, scan.str()?),
        "error" => None,
        _ => scan.skip(MAX_DEPTH - 2),
    })?;
    Some(CodexItem {
        id: id?,
        item_type: item_type?,
        status,
        error: None,
    })
}

pub(super) fn codex(line: &str) -> Option<CodexLine<'_>> {
    let mut scan = Scan::new(line);
    let mut line_type = None;
    let mut thread_id = None;
    let mut usage = None;
    let mut message = None;
    let mut item = None;
    scan.object(MAX_DEPTH, |scan, key| match key {
        "type" => set(&mut line_type, scan.str()?),
        "thread_id" => set(&mut thread_id, scan.str()?),
        "usage" => set(&mut usage, codex_usage(scan)?),
        "message" => set(&mut message, scan.str()?),
        "item" => set(&mut item, codex_item(scan)?),
        "error" => None,
        _ => scan.skip(MAX_DEPTH - 1),
    })?;
    scan.end()?;
    Some(CodexLine {
        line_type: line_type?,
        thread_id,
        usage,
        error: None,
        message,
        item,
    })
}

#[cfg(test)]
mod tests {
    use super::{claude, codex};
    use crate::ingest::{ClaudeHook, CodexLine};

    const SEEDS: &[&str] = &[
        r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_use_id":"toolu_01","tool_input":{"command":"ls -la"}}"#,
        r#"{"hook_event_name":"PostToolUse","session_id":"s1","tool_use_id":"toolu_01","duration_ms":42,"tool_response":{"stdout":"a\nb\nc","list":[1,true,false,null,{}]}}"#,
        r#" { "hook_event_name" : "SessionStart" , "session_id":"é∑漢🙂", "transcript_path":"/t", "reason":"exit", "notification_type":"idle" } "#,
        r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","status":"completed","command":"ls","aggregated_output":"a\nb"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":340,"cached":0}}"#,
        r#"{"type":"thread.started","thread_id":"th_1","message":"m","item":{"id":"i","type":"t","status":"failed"}}"#,
    ];

    /// Edge cases the scanner must either reject or read exactly like serde.
    const EDGES: &[&str] = &[
        "",
        " ",
        "{",
        "}",
        "[]",
        "null",
        "{}",
        r#"{"hook_event_name":"A"}x"#,
        r#"{"hook_event_name":"A",}"#,
        r#"{"hook_event_name":"A","hook_event_name":"B"}"#,
        r#"{"hook_event_name":"A","x":1,"x":2}"#,
        r#"{"hook_event_name":null}"#,
        r#"{"hook_event_name":"A","session_id":null}"#,
        r#"{"hook_event_name":"A","duration_ms":-1}"#,
        r#"{"hook_event_name":"A","duration_ms":01}"#,
        r#"{"hook_event_name":"A","duration_ms":0}"#,
        r#"{"hook_event_name":"A","duration_ms":1.5}"#,
        r#"{"hook_event_name":"A","duration_ms":1e3}"#,
        r#"{"hook_event_name":"A","duration_ms":18446744073709551615}"#,
        r#"{"hook_event_name":"A","duration_ms":18446744073709551616}"#,
        r#"{"hook_event_name":"A","duration_ms":"5"}"#,
        r#"{"hook_event_name":"A","x":nul}"#,
        r#"{"hook_event_name":"A","x":truex}"#,
        r#"{"hook_event_name":"A","x":[1,]}"#,
        r#"{"hook_event_name":"A","x":[1 2]}"#,
        r#"{"hook_event_name":"A","x":01}"#,
        r#"{"hook_event_name":"A","x":-0.5e+3}"#,
        r#"{"hook_event_name":"AB"}"#,
        r#"{"hook_event_name":"A"}"#,
        r#"{"hook_event_name":"A","error":"e"}"#,
        "{\"hook_event_name\":\"A\tB\"}",
        "{\"hook_event_name\":\"A\"}\n\r\t ",
        "\u{a0}{\"hook_event_name\":\"A\"}",
        r#"["A"]"#,
        r#"{"type":"item.started","item":null}"#,
        r#"{"type":"item.started","item":{}}"#,
        r#"{"type":"item.started","item":{"id":"i"}}"#,
        r#"{"type":"item.started","item":["i","t"]}"#,
        r#"{"type":"item.started","item":{"id":"i","type":"t","error":{"message":"m"}}}"#,
        r#"{"type":"turn.completed","usage":null}"#,
        r#"{"type":"turn.completed","usage":[1,2]}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"input_tokens":2}}"#,
        r#"{"type":"turn.completed","usage":{}}"#,
        r#"{"type":"x","usage":{},"usage":{}}"#,
    ];

    /// Whatever the scanner accepts, serde reads to the same struct.
    /// Returns whether the scanner accepted, so tests can check coverage.
    fn same_claude(payload: &str) -> bool {
        let Some(fast) = claude(payload) else {
            return false;
        };
        let slow: ClaudeHook = serde_json::from_str(payload)
            .unwrap_or_else(|error| panic!("scanner accepted {payload:?}: {error}"));
        assert_eq!(format!("{fast:?}"), format!("{slow:?}"), "{payload:?}");
        true
    }

    fn same_codex(line: &str) -> bool {
        let Some(fast) = codex(line) else {
            return false;
        };
        let slow: CodexLine = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("scanner accepted {line:?}: {error}"));
        assert_eq!(format!("{fast:?}"), format!("{slow:?}"), "{line:?}");
        true
    }

    #[test]
    fn scanner_reads_common_payloads() {
        for payload in &SEEDS[..3] {
            assert!(same_claude(payload), "{payload}");
        }
        for line in &SEEDS[3..] {
            assert!(same_codex(line), "{line}");
        }
    }

    #[test]
    fn scanner_matches_serde_on_edge_cases() {
        for payload in SEEDS.iter().chain(EDGES) {
            same_claude(payload);
            same_codex(payload);
        }
        // Escapes are fine in ignored values, not in the fields that are read.
        assert!(same_claude(
            r#"{"hook_event_name":"A","x":["\n\"\/\u00e9\ud800"]}"#
        ));
        assert!(!same_claude(r#"{"hook_event_name":"A","x":"\q"}"#));
        assert!(!same_claude(r#"{"hook_event_name":"A","x":"\u12"}"#));
        assert!(!same_claude(r#"{"hook_event_name":"A\n"}"#));
        let deep = format!(
            r#"{{"hook_event_name":"A","x":{}{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        assert!(claude(&deep).is_none());
        same_claude(&deep);
    }

    #[test]
    fn scanner_matches_serde_on_mutations() {
        const PIECES: &[&str] = &[
            "\"",
            "\\",
            ",",
            ":",
            "{",
            "}",
            "[",
            "]",
            " ",
            "\n",
            "0",
            "1",
            "-",
            ".",
            "e",
            "null",
            "true",
            "x",
            "é",
            "\u{1}",
            "\"error\":1",
            "\"id\":\"q\"",
            "\"usage\":{}",
            "\"item\":{\"id\":\"i\",\"type\":\"t\"}",
            "\"duration_ms\":7",
            "\\n",
            "\\u00e9",
            "\\ud800",
            "\\uD83D\\uDE00",
            "\\u12",
            "\\uZZZZ",
            "\\x",
            "\"tool_input\":{\"k\\n\":\"v\\\"\"}",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |bound: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(state >> 33).unwrap() % bound
        };
        let mut accepted = 0;
        for _ in 0..20_000 {
            let mut text = SEEDS[next(SEEDS.len())].to_string();
            for _ in 0..=next(3) {
                let mut at = next(text.len() + 1);
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                match next(3) {
                    0 => text.insert_str(at, PIECES[next(PIECES.len())]),
                    1 if at < text.len() => {
                        text.remove(at);
                    }
                    _ => {
                        let piece = PIECES[next(PIECES.len())];
                        let end = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
                        text.replace_range(at..end, piece);
                    }
                }
            }
            accepted += usize::from(same_claude(&text)) + usize::from(same_codex(&text));
        }
        // The comparison is only meaningful if the scanner takes many of them.
        assert!(
            accepted > 2_000,
            "scanner accepted only {accepted} mutations"
        );
    }
}
