//! Credentials out of the free text the log keeps.
//!
//! Engine errors, screen lines and command failures are stored as written,
//! and an engine prints whatever it was given: a token in a failed `curl`, a
//! password in a database URL. [`redact`] replaces the obvious shapes with
//! [`REDACTED`] before such text reaches the log; [`redact_event`] applies
//! it to every free-text field of an event. The stores call it on append,
//! so no writer can forget it.
//!
//! What counts as obvious:
//!
//! - API keys by their prefix: `sk-…`, `ghp_…` (and GitHub's other `gh?_`
//!   tokens), `github_pat_…`, `AKIA…` (AWS access key ids);
//! - `Bearer <token>`;
//! - a credential header's value: `Authorization:` and
//!   `Proxy-Authorization:` whatever the scheme (`Basic …`, `token …`, a
//!   raw value; the scheme word stays), `Cookie:`, and any header whose name
//!   ends in `-token`, `-key`, `-secret` or `-password` (`x-api-key`,
//!   `PRIVATE-TOKEN`, `X-Auth-Token`);
//! - the password of a URL's userinfo, `scheme://user:pass@` (the user
//!   stays); a password may hold `@` or `/`: the userinfo ends at the last
//!   `@` before the query;
//! - the value of an assignment whose name ends in `TOKEN`, `SECRET`,
//!   `KEY`, `AUTH`, `CREDENTIALS` or `COOKIE`, or holds `PASS` (so
//!   `PASSWORD`, `PASSWD`) or `PWD`, any case (`PGPASSWORD=…`, `DB_PASS=…`, `api_key=…`,
//!   `--token=…`, `?token=…&`);
//! - a JSON string under such a key (`"token": "…"`, `"password": "…"`);
//! - a credential flag's value: `-u`/`--user user:pass` (the user stays),
//!   `--password v`, `sshpass -p v`, and `-pv` on a `mysql`/`mariadb`
//!   command line.

use serde_json::Value;

use crate::event::EventKind;

/// What a credential is replaced with.
pub const REDACTED: &str = "[redacted]";

/// `text` with every credential of an obvious shape replaced by
/// [`REDACTED`]. Text without one comes back unchanged.
pub fn redact(text: &str) -> String {
    let text = prefixed_keys(text);
    let text = bearer(&text);
    let text = headers(&text);
    let text = userinfo(&text);
    let text = json_keys(&text);
    let text = flags(&text);
    assignments(&text)
}

/// Redact the free-text fields of `kind`: engine and command errors,
/// screen evidence, a tool call's summary of its input, and the text an
/// observation quotes. Identifiers,
/// paths and control records (which carry mission sources and prompts the
/// folds read back) are left as they are.
pub fn redact_event(kind: &mut EventKind) {
    let clean = |text: &mut String| *text = redact(text);
    match kind {
        EventKind::ToolFailed { error, .. }
        | EventKind::TurnFailed { error, .. }
        | EventKind::ExecutionEndStalled { error, .. }
        | EventKind::CommandFinished {
            error: Some(error), ..
        } => clean(error),
        EventKind::ScreenObserved { evidence, .. } => clean(evidence),
        EventKind::ToolStarted {
            summary: Some(summary),
            summary_whole,
            ..
        } => {
            // A summary the redactor changed is no longer the call's whole
            // input: two calls with different secrets would read the same.
            let before = summary.clone();
            clean(summary);
            if *summary != before {
                *summary_whole = false;
            }
        }
        EventKind::ObservationRecorded { observation } => {
            if let Some(text) = observation.human_question.as_mut() {
                clean(text);
            }
            if let Some(text) = observation.attention_reason.as_mut() {
                clean(text);
            }
            if let Some(evidence) = observation.evidence.as_mut() {
                redact_strings(evidence);
            }
        }
        _ => {}
    }
}

fn redact_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact(text),
        Value::Array(items) => items.iter_mut().for_each(redact_strings),
        Value::Object(fields) => fields.values_mut().for_each(redact_strings),
        _ => {}
    }
}

/// Bytes that continue a token or an identifier: a match must not start
/// inside one.
fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Every cut below is at an ASCII byte, so the pieces stay UTF-8.
fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn starts_with_ignoring_case(bytes: &[u8], at: usize, word: &str) -> bool {
    bytes
        .get(at..at + word.len())
        .is_some_and(|slice| slice.eq_ignore_ascii_case(word.as_bytes()))
}

/// API keys by prefix, each with the least number of token bytes after the
/// prefix that makes it a key and not a word (`sk-learn` stays).
fn prefixed_keys(text: &str) -> String {
    const PREFIXES: [(&str, usize); 7] = [
        ("github_pat_", 20),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghu_", 20),
        ("ghs_", 20),
        ("ghr_", 20),
        ("sk-", 16),
    ];
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let boundary = at == 0 || !word_byte(bytes[at - 1]);
        if boundary {
            let length = PREFIXES.iter().find_map(|(prefix, least)| {
                let rest = bytes.get(at..)?.strip_prefix(prefix.as_bytes())?;
                let body = rest.iter().take_while(|byte| word_byte(**byte)).count();
                (body >= *least).then_some(prefix.len() + body)
            });
            // An AWS access key id: AKIA and 16 upper-case letters or digits.
            let aws = bytes.get(at..at + 20).is_some_and(|key| {
                key.starts_with(b"AKIA")
                    && key[4..]
                        .iter()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                    && bytes.get(at + 20).is_none_or(|next| !word_byte(*next))
            });
            if let Some(length) = length.or(aws.then_some(20)) {
                out.extend_from_slice(REDACTED.as_bytes());
                at += length;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

/// `Bearer <token>`: the word stays, the token goes.
fn bearer(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let boundary = at == 0 || !word_byte(bytes[at - 1]);
        if boundary && starts_with_ignoring_case(bytes, at, "bearer") {
            let spaces = bytes[at + 6..]
                .iter()
                .take_while(|byte| **byte == b' ' || **byte == b'\t')
                .count();
            let start = at + 6 + spaces;
            let token = bytes[start..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=_".contains(byte))
                .count();
            if spaces > 0 && token >= 8 {
                out.extend_from_slice(&bytes[at..start]);
                out.extend_from_slice(REDACTED.as_bytes());
                at = start + token;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

/// The end of a value that runs to whitespace, a quote or a separator.
fn value_end(bytes: &[u8], start: usize, stops: &[u8]) -> usize {
    start
        + bytes[start..]
            .iter()
            .take_while(|byte| !byte.is_ascii_whitespace() && !stops.contains(byte))
            .count()
}

/// Is `name` (a header's, any case) one whose value is a credential?
fn credential_header(name: &[u8]) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_slice(),
        b"authorization" | b"proxy-authorization" | b"cookie"
    ) || [&b"-token"[..], b"-key", b"-secret", b"-password"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// A credential header, `Name: value` (or `=`), quoted or not: the name
/// stays, and so does an authorization's scheme word (`Basic`, `token`);
/// the value goes. A `Bearer` token is already gone ([`bearer`]).
fn headers(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let boundary = at == 0 || !word_byte(bytes[at - 1]);
        let name_len = bytes[at..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'-')
            .count();
        if boundary && name_len > 0 && credential_header(&bytes[at..at + name_len]) {
            let mut start = at + name_len;
            let skip = |start: &mut usize, set: &[u8]| {
                while bytes.get(*start).is_some_and(|byte| set.contains(byte)) {
                    *start += 1;
                }
            };
            skip(&mut start, b" \t\"'");
            // A header's colon is followed by a space or a quote; a colon
            // glued to its value is a URL's userinfo (`token:pass@`), which
            // [`userinfo`] reads.
            let separator = bytes.get(start).copied();
            let spaced = bytes
                .get(start + 1)
                .is_some_and(|byte| b" \t\"'".contains(byte));
            if separator == Some(b'=') || (separator == Some(b':') && spaced) {
                start += 1;
                skip(&mut start, b" \t\"'");
                // `Basic <credentials>`: the scheme stays. A scheme is a
                // word of letters followed by a space and more value.
                let word = bytes[start..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphabetic())
                    .count();
                if word > 0
                    && bytes.get(start + word) == Some(&b' ')
                    && bytes
                        .get(start + word + 1)
                        .is_some_and(|byte| !byte.is_ascii_whitespace() && !b"\"'".contains(byte))
                {
                    start += word + 1;
                }
                let end = value_end(bytes, start, b"\"',;}");
                if end > start && &bytes[start..end] != REDACTED.as_bytes() {
                    out.extend_from_slice(&bytes[at..start]);
                    out.extend_from_slice(REDACTED.as_bytes());
                    at = end;
                    continue;
                }
            }
        }
        // Past the whole name: a header name never starts inside one.
        let step = name_len.max(1);
        out.extend_from_slice(&bytes[at..at + step]);
        at += step;
    }
    text_of(out)
}

/// `scheme://user:password@host`: the password goes, the user stays. A
/// password may hold `@` or `/`, so the userinfo runs to the last `@`
/// before the query or fragment; a `host:port/path@x` (digits up to the
/// first `/`) is a port, not a password.
fn userinfo(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"://") {
            let start = at + 3;
            let end = start
                + bytes[start..]
                    .iter()
                    .take_while(|byte| !byte.is_ascii_whitespace() && !b"?#\"'<>`".contains(byte))
                    .count();
            let token = &bytes[start..end];
            if let Some(last_at) = token.iter().rposition(|byte| *byte == b'@')
                && let Some(colon) = token[..last_at].iter().position(|byte| *byte == b':')
                && !token[..colon].contains(&b'/')
                && colon + 1 < last_at
                && !is_port(&token[colon + 1..last_at])
                && &token[colon + 1..last_at] != REDACTED.as_bytes()
            {
                out.extend_from_slice(&bytes[at..=start + colon]);
                out.extend_from_slice(REDACTED.as_bytes());
                out.push(b'@');
                at = start + last_at + 1;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

/// `8080/…`: digits up to the first `/` (or the end) read as a port.
fn is_port(after_colon: &[u8]) -> bool {
    let digits = after_colon
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0 && after_colon.get(digits).is_none_or(|byte| *byte == b'/')
}

/// Does an assignment's or a key's `name` say its value is a credential?
fn secret_name(name: &[u8]) -> bool {
    // `PASSWORD` and `PASSWD` hold `PASS` (below).
    const ENDINGS: [&str; 6] = ["TOKEN", "SECRET", "KEY", "AUTH", "CREDENTIALS", "COOKIE"];
    const WITHIN: [&str; 2] = ["PASS", "PWD"];
    let upper = name.to_ascii_uppercase();
    ENDINGS.iter().any(|word| upper.ends_with(word.as_bytes()))
        || WITHIN.iter().any(|word| {
            upper
                .windows(word.len())
                .any(|window| window == word.as_bytes())
        })
}

/// `"token": "value"` in JSON: a string under a credential's key goes.
fn json_keys(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'"' {
            let name_len = bytes[at + 1..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(byte))
                .count();
            let close = at + 1 + name_len;
            if name_len > 0 && bytes.get(close) == Some(&b'"') && secret_name(&bytes[at + 1..close])
            {
                let mut start = close + 1;
                while bytes
                    .get(start)
                    .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                {
                    start += 1;
                }
                if bytes.get(start) == Some(&b':') {
                    start += 1;
                    while bytes
                        .get(start)
                        .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
                    {
                        start += 1;
                    }
                    if bytes.get(start) == Some(&b'"') {
                        let value = start + 1;
                        let mut end = value;
                        while end < bytes.len() && bytes[end] != b'"' {
                            end += usize::from(bytes[end] == b'\\') + 1;
                        }
                        let end = end.min(bytes.len());
                        if end > value && &bytes[value..end] != REDACTED.as_bytes() {
                            out.extend_from_slice(&bytes[at..value]);
                            out.extend_from_slice(REDACTED.as_bytes());
                            at = end;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

/// The token starting at `start`: up to whitespace or a closing quote.
fn token_end(bytes: &[u8], start: usize) -> usize {
    value_end(bytes, start, b"\"';&|")
}

/// Credential flags on a command line: `-u`/`--user user:pass` (the user
/// stays), `--password v`, `sshpass -p v`, and `-pv` on a line that runs
/// `mysql` or `mariadb` (there `-p` glued to a value is the password).
fn flags(text: &str) -> String {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    let mut line_start = 0;
    while at < bytes.len() {
        if bytes[at] == b'\n' {
            line_start = at + 1;
        }
        let boundary = at == 0 || bytes[at - 1].is_ascii_whitespace();
        if boundary && bytes[at] == b'-' {
            let flag_len = bytes[at..]
                .iter()
                .take_while(|byte| !byte.is_ascii_whitespace() && **byte != b'=')
                .count();
            let flag = &bytes[at..at + flag_len];
            let line = &lower[line_start..at];
            let next = |from: usize| -> Option<(usize, usize)> {
                let gap = bytes[from..]
                    .iter()
                    .take_while(|byte| **byte == b' ' || **byte == b'=')
                    .count();
                let start = from + gap;
                let start =
                    start + usize::from(bytes.get(start).is_some_and(|byte| b"\"'".contains(byte)));
                let end = token_end(bytes, start);
                (gap > 0 && end > start).then_some((start, end))
            };
            // The span of the value to replace, if this flag carries one.
            let secret: Option<(usize, usize)> = match flag {
                b"-u" | b"--user" => next(at + flag_len).and_then(|(start, end)| {
                    let colon = bytes[start..end].iter().position(|byte| *byte == b':')?;
                    (start + colon + 1 < end).then_some((start + colon + 1, end))
                }),
                b"--password" | b"--passwd" => next(at + flag_len),
                b"-p" if line.trim_end().ends_with("sshpass") => next(at + flag_len),
                _ if flag.len() > 2
                    && flag.starts_with(b"-p")
                    && !flag.starts_with(b"--")
                    && (line.contains("mysql") || line.contains("mariadb")) =>
                {
                    Some((at + 2, at + flag_len))
                }
                _ => None,
            };
            if let Some((start, end)) = secret
                && &bytes[start..end] != REDACTED.as_bytes()
            {
                out.extend_from_slice(&bytes[at..start]);
                out.extend_from_slice(REDACTED.as_bytes());
                at = end;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

/// `NAME=value` where the name ends in a secret's word: the value goes.
fn assignments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'=' {
            let name_start = bytes[..at]
                .iter()
                .rposition(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
                .map_or(0, |before| before + 1);
            let name = &bytes[name_start..at];
            if !name.is_empty() && secret_name(name) {
                let quote = bytes
                    .get(at + 1)
                    .copied()
                    .filter(|byte| *byte == b'"' || *byte == b'\'');
                let start = at + 1 + usize::from(quote.is_some());
                let end = match quote {
                    Some(quote) => {
                        start
                            + bytes[start..]
                                .iter()
                                .take_while(|byte| **byte != quote)
                                .count()
                    }
                    None => value_end(bytes, start, b"\"',;&"),
                };
                if end > start && &bytes[start..end] != REDACTED.as_bytes() {
                    out.extend_from_slice(&bytes[at..start]);
                    out.extend_from_slice(REDACTED.as_bytes());
                    at = end;
                    continue;
                }
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    text_of(out)
}

#[cfg(test)]
mod tests {
    use super::{REDACTED, redact, redact_event};
    use crate::event::EventKind;

    #[test]
    fn api_keys_go_by_their_prefix() {
        assert_eq!(
            redact("auth failed for sk-ant-api03-AbCdEf0123456789xyz, retry"),
            format!("auth failed for {REDACTED}, retry")
        );
        assert_eq!(
            redact("token ghp_0123456789abcdefABCDEF0123 rejected"),
            format!("token {REDACTED} rejected")
        );
        assert_eq!(
            redact("github_pat_11ABCDEFG0123456789_abcdefghijklmnop"),
            REDACTED
        );
        assert_eq!(
            redact("key AKIAIOSFODNN7EXAMPLE used"),
            format!("key {REDACTED} used")
        );
        // Words that merely start the same way stay.
        assert_eq!(redact("pip install sk-learn"), "pip install sk-learn");
        assert_eq!(redact("risk-assessment"), "risk-assessment");
        assert_eq!(redact("AKIAX is short"), "AKIAX is short");
    }

    #[test]
    fn bearer_tokens_and_api_key_headers_lose_their_values() {
        assert_eq!(
            redact("curl -H 'Authorization: Bearer eyJhbGciOi.J9x-y_z' https://api"),
            format!("curl -H 'Authorization: Bearer {REDACTED}' https://api")
        );
        assert_eq!(
            redact("x-api-key: abcdef0123456789"),
            format!("x-api-key: {REDACTED}")
        );
        assert_eq!(
            redact(r#"{"X-Api-Key": "abc123def"}"#),
            format!(r#"{{"X-Api-Key": "{REDACTED}"}}"#)
        );
        // Prose about bearers keeps its words.
        assert_eq!(redact("the bearer of bad news"), "the bearer of bad news");
    }

    #[test]
    fn a_urls_password_goes_and_its_user_stays() {
        assert_eq!(
            redact("connect postgres://spawn:hunter2@127.0.0.1:5432/spawn failed"),
            format!("connect postgres://spawn:{REDACTED}@127.0.0.1:5432/spawn failed")
        );
        assert_eq!(
            redact("https://x-access-token:abc@github.com/o/r.git"),
            format!("https://x-access-token:{REDACTED}@github.com/o/r.git")
        );
        // No password, or no userinfo: nothing to redact.
        assert_eq!(
            redact("git@github.com:o/r.git and https://user@host/x"),
            "git@github.com:o/r.git and https://user@host/x"
        );
        assert_eq!(
            redact("http://127.0.0.1:8791/v1"),
            "http://127.0.0.1:8791/v1"
        );
    }

    #[test]
    fn secret_assignments_lose_their_values() {
        assert_eq!(
            redact("PGPASSWORD=s3cret psql"),
            format!("PGPASSWORD={REDACTED} psql")
        );
        assert_eq!(
            redact("export GITHUB_TOKEN=\"abc def\"; next"),
            format!("export GITHUB_TOKEN=\"{REDACTED}\"; next")
        );
        assert_eq!(
            redact("--api_key=xyz --client-secret=q"),
            format!("--api_key={REDACTED} --client-secret={REDACTED}")
        );
        assert_eq!(
            redact("GET /cb?token=abc&page=2"),
            format!("GET /cb?token={REDACTED}&page=2")
        );
        assert_eq!(
            redact("AWS_SECRET_ACCESS_KEY=wJal"),
            format!("AWS_SECRET_ACCESS_KEY={REDACTED}")
        );
        // Other assignments, and empty values, stay.
        assert_eq!(redact("PATH=/bin a==b KEY= x"), "PATH=/bin a==b KEY= x");
    }

    #[test]
    fn text_without_credentials_comes_back_unchanged() {
        for text in [
            "",
            "error: exit 1",
            "命令失败：没有这个文件",
            "❯ ready · 249 tok/s",
            "see https://docs.rs/serde and a@b.c",
        ] {
            assert_eq!(redact(text), text);
        }
        // Redacting twice changes nothing more.
        let once = redact("PASSWORD=x Bearer abcdefghijk sk-0123456789abcdefgh");
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn an_event_loses_the_credentials_in_its_free_text() {
        let mut failed = EventKind::ToolFailed {
            tool_use_id: "toolu_sk-0123456789abcdefgh".to_string(),
            error: "curl: Bearer abcdefghijkl denied".to_string(),
        };
        redact_event(&mut failed);
        assert_eq!(
            failed,
            EventKind::ToolFailed {
                tool_use_id: "toolu_sk-0123456789abcdefgh".to_string(),
                error: format!("curl: Bearer {REDACTED} denied"),
            },
            "the error is redacted, the identifier is not"
        );
        let mut finished = EventKind::CommandFinished {
            command_id: crate::id::CommandId(1),
            succeeded: false,
            error: Some("push https://u:p4ss@host/r.git refused".to_string()),
        };
        redact_event(&mut finished);
        assert!(matches!(
            finished,
            EventKind::CommandFinished { error: Some(error), .. } if !error.contains("p4ss")
        ));
    }

    /// The shapes rev-e's probe found reaching a stored tool summary
    /// (review-ui-ia-tools.md), each with its own assertion so a pass that
    /// loses one fails here.
    #[test]
    fn credential_headers_lose_their_values_whatever_the_scheme() {
        let r = REDACTED;
        assert_eq!(
            redact("curl -H 'Authorization: Basic YWxpY2U6aHVudGVyMg==' https://x"),
            format!("curl -H 'Authorization: Basic {r}' https://x")
        );
        assert_eq!(
            redact("curl -H \"Authorization: s3cr3tR4wT0k3n\" https://x"),
            format!("curl -H \"Authorization: {r}\" https://x")
        );
        assert_eq!(
            redact("curl -H 'Authorization: token 0123456789abcdef0123' https://x"),
            format!("curl -H 'Authorization: token {r}' https://x")
        );
        assert_eq!(
            redact("Proxy-Authorization: Basic dTpw"),
            format!("Proxy-Authorization: Basic {r}")
        );
        assert_eq!(
            redact("curl -H 'PRIVATE-TOKEN: glpat-AbCdEf0123456789' https://x"),
            format!("curl -H 'PRIVATE-TOKEN: {r}' https://x")
        );
        assert_eq!(
            redact("curl -H 'X-Auth-Token: abcd1234efgh5678' https://x"),
            format!("curl -H 'X-Auth-Token: {r}' https://x")
        );
        assert_eq!(redact("Cookie: sid=abc123"), format!("Cookie: {r}"));
        assert_eq!(
            redact("X-Client-Secret: q1w2e3"),
            format!("X-Client-Secret: {r}")
        );
        assert_eq!(
            redact("X-Db-Password: q1w2e3"),
            format!("X-Db-Password: {r}")
        );
        // A header name in prose, a colon glued to a URL's userinfo, and
        // other headers stay.
        assert_eq!(
            redact("Authorization failed: check it"),
            "Authorization failed: check it"
        );
        assert_eq!(
            redact("Content-Type: application/json"),
            "Content-Type: application/json"
        );
        assert_eq!(redact("author: Ann"), "author: Ann");
    }

    #[test]
    fn credential_flags_lose_their_values() {
        let r = REDACTED;
        assert_eq!(
            redact("curl -u alice:hunter2pass https://api.example/v1"),
            format!("curl -u alice:{r} https://api.example/v1")
        );
        assert_eq!(
            redact("curl --user alice:hunter2pass https://x"),
            format!("curl --user alice:{r} https://x")
        );
        assert_eq!(
            redact("mysql --password hunter2pass -u root"),
            format!("mysql --password {r} -u root")
        );
        assert_eq!(
            redact("mysql -phunter2pass -u root"),
            format!("mysql -p{r} -u root")
        );
        assert_eq!(
            redact("mariadb -uroot -phunter2pass db"),
            format!("mariadb -uroot -p{r} db")
        );
        assert_eq!(
            redact("sshpass -p hunter2pass ssh host"),
            format!("sshpass -p {r} ssh host")
        );
        // The same flags meaning something else stay: a user without a
        // password, a port, a path, a parent directory.
        for text in [
            "curl -u alice https://x",
            "ssh -p 2222 host",
            "mkdir -p crates/x",
            "cargo test -p spawn-cli",
            "git log -p -3",
            "ps -u ckl",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn pass_and_pwd_names_and_json_keys_lose_their_values() {
        let r = REDACTED;
        assert_eq!(
            redact("DB_PASS=hunter2pass ./run"),
            format!("DB_PASS={r} ./run")
        );
        assert_eq!(
            redact("PGPWD=hunter2pass ./run"),
            format!("PGPWD={r} ./run")
        );
        assert_eq!(
            redact("SMTP_PASSPHRASE=x ./run"),
            format!("SMTP_PASSPHRASE={r} ./run")
        );
        assert_eq!(redact("GH_AUTH=abc ./run"), format!("GH_AUTH={r} ./run"));
        assert_eq!(
            redact("GCP_CREDENTIALS=abc ./run"),
            format!("GCP_CREDENTIALS={r} ./run")
        );
        assert_eq!(
            redact("SESSION_COOKIE=abc ./run"),
            format!("SESSION_COOKIE={r} ./run")
        );
        assert_eq!(
            redact("curl -d '{\"token\": \"abcdef123456\"}' https://x"),
            format!("curl -d '{{\"token\": \"{r}\"}}' https://x")
        );
        assert_eq!(
            redact(r#"{"password":"p\"w","user":"u"}"#),
            format!(r#"{{"password":"{r}","user":"u"}}"#)
        );
        assert_eq!(
            redact(r#"{"api_key": "k1", "client_secret": "s1"}"#),
            format!(r#"{{"api_key": "{r}", "client_secret": "{r}"}}"#)
        );
        // Other keys and names stay.
        assert_eq!(
            redact(r#"{"tokens": 3, "user": "u", "path": "/x"}"#),
            r#"{"tokens": 3, "user": "u", "path": "/x"}"#
        );
        assert_eq!(redact("RUST_LOG=debug ./run"), "RUST_LOG=debug ./run");
    }

    #[test]
    fn a_urls_password_holding_an_at_or_a_slash_goes_whole() {
        let r = REDACTED;
        assert_eq!(
            redact("psql postgres://spawn:hun@ter2pass@db/spawn"),
            format!("psql postgres://spawn:{r}@db/spawn")
        );
        assert_eq!(
            redact("psql postgres://spawn:hun/ter2pass@db/spawn"),
            format!("psql postgres://spawn:{r}@db/spawn")
        );
        assert_eq!(
            redact("DATABASE_URL=postgres://spawn:hunter2pass@db/spawn ./run"),
            format!("DATABASE_URL=postgres://spawn:{r}@db/spawn ./run")
        );
        // A port, then an `@` in the path, is not a password.
        assert_eq!(
            redact("https://example.com:8080/a@b/c"),
            "https://example.com:8080/a@b/c"
        );
        assert_eq!(
            redact("https://example.com/a:b@c"),
            "https://example.com/a:b@c"
        );
    }

    #[test]
    fn a_redacted_summary_is_no_longer_whole() {
        let started = |summary: &str| EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "a".to_string(),
            summary: Some(summary.to_string()),
            summary_whole: true,
        };
        let mut secret = started("PGPASSWORD=s3cret psql");
        redact_event(&mut secret);
        assert_eq!(
            secret,
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: "a".to_string(),
                summary: Some(format!("PGPASSWORD={REDACTED} psql")),
                summary_whole: false,
            }
        );
        let mut plain = started("git status");
        redact_event(&mut plain);
        assert_eq!(
            plain,
            started("git status"),
            "nothing to redact: still whole"
        );
    }
}
