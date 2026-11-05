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
//! - an `x-api-key` header's value;
//! - the password of a URL's userinfo, `scheme://user:pass@` (the user
//!   stays);
//! - the value of an assignment whose name ends in `PASSWORD`, `PASSWD`,
//!   `TOKEN`, `SECRET` or `KEY`, any case (`PGPASSWORD=…`, `api_key=…`,
//!   `--token=…`, `?token=…&`).

use serde_json::Value;

use crate::event::EventKind;

/// What a credential is replaced with.
pub const REDACTED: &str = "[redacted]";

/// `text` with every credential of an obvious shape replaced by
/// [`REDACTED`]. Text without one comes back unchanged.
pub fn redact(text: &str) -> String {
    let text = prefixed_keys(text);
    let text = bearer(&text);
    let text = api_key_header(&text);
    let text = userinfo(&text);
    assignments(&text)
}

/// Redact the free-text fields of `kind`: engine and command errors,
/// screen evidence, and the text an observation quotes. Identifiers,
/// paths and control records (which carry mission sources and prompts the
/// folds read back) are left as they are.
pub fn redact_event(kind: &mut EventKind) {
    let clean = |text: &mut String| *text = redact(text);
    match kind {
        EventKind::ToolFailed { error, .. }
        | EventKind::TurnFailed { error }
        | EventKind::ExecutionEndStalled { error, .. }
        | EventKind::CommandFinished {
            error: Some(error), ..
        } => clean(error),
        EventKind::ScreenObserved { evidence, .. } => clean(evidence),
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

/// `x-api-key: <value>` (or `=`), quoted or not: the name stays.
fn api_key_header(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let boundary = at == 0 || !word_byte(bytes[at - 1]);
        if boundary && starts_with_ignoring_case(bytes, at, "x-api-key") {
            let mut start = at + 9;
            let skip = |start: &mut usize, set: &[u8]| {
                while bytes.get(*start).is_some_and(|byte| set.contains(byte)) {
                    *start += 1;
                }
            };
            skip(&mut start, b" \t\"'");
            if bytes
                .get(start)
                .is_some_and(|byte| *byte == b':' || *byte == b'=')
            {
                start += 1;
                skip(&mut start, b" \t\"'");
                let end = value_end(bytes, start, b"\"',;}");
                if end > start {
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

/// `scheme://user:password@host`: the password goes, the user stays.
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
                    .take_while(|byte| !byte.is_ascii_whitespace() && !b"@/?#\"'<>".contains(byte))
                    .count();
            if bytes.get(end) == Some(&b'@')
                && let Some(colon) = bytes[start..end].iter().position(|byte| *byte == b':')
                && start + colon + 1 < end
            {
                out.extend_from_slice(&bytes[at..=start + colon]);
                out.extend_from_slice(REDACTED.as_bytes());
                out.push(b'@');
                at = end + 1;
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
    const NAMES: [&str; 5] = ["PASSWORD", "PASSWD", "TOKEN", "SECRET", "KEY"];
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
            let secret = NAMES.iter().any(|word| {
                name.len() >= word.len()
                    && name[name.len() - word.len()..].eq_ignore_ascii_case(word.as_bytes())
            });
            if secret {
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
}
