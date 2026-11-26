//! Strict HTTP/1.1 framing, limits, and header validation for `spawn-ipc-v1`
//! (`docs/spawn-ipc-v1.md`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

pub(super) const MAX_IPC_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

type ParsedHeaders = (BTreeMap<String, String>, bool, Option<usize>);

struct HeadError {
    status: u16,
    code: &'static str,
    message: String,
    close: bool,
}

impl HeadError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: message.into(),
            close: true,
        }
    }

    fn into_outcome(self) -> ReadOutcome {
        ReadOutcome::Error {
            status: self.status,
            code: self.code,
            message: self.message,
            close: self.close,
        }
    }
}

pub(super) struct IpcRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: String,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) idempotency_key: Option<String>,
    pub(super) body: Vec<u8>,
    pub(super) close: bool,
}

impl IpcRequest {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

pub(super) struct IpcResponse {
    pub(super) status: u16,
    pub(super) content_type: &'static str,
    pub(super) body: Vec<u8>,
    pub(super) headers: Vec<(&'static str, String)>,
    pub(super) close: bool,
}

impl IpcResponse {
    pub(super) fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: value.to_string().into_bytes(),
            headers: Vec::new(),
            close: false,
        }
    }

    pub(super) fn text(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
            headers: Vec::new(),
            close: false,
        }
    }

    pub(super) fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: Vec::new(),
            headers: Vec::new(),
            close: false,
        }
    }

    pub(super) fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub(super) fn with_close(mut self, close: bool) -> Self {
        self.close = close;
        self
    }
}

pub(super) enum ReadOutcome {
    Request(IpcRequest),
    Eof,
    Error {
        status: u16,
        code: &'static str,
        message: String,
        close: bool,
    },
}

fn bad_request(message: impl Into<String>, close: bool) -> ReadOutcome {
    ReadOutcome::Error {
        status: 400,
        code: "bad_request",
        message: message.into(),
        close,
    }
}

fn length_required(message: impl Into<String>, close: bool) -> ReadOutcome {
    ReadOutcome::Error {
        status: 411,
        code: "length_required",
        message: message.into(),
        close,
    }
}

pub(super) fn write_response(
    stream: &mut UnixStream,
    response: &IpcResponse,
) -> std::io::Result<()> {
    let reason = reason_phrase(response.status);
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n",
        response.status,
        response.body.len()
    );
    if !response.body.is_empty() || response.status != 304 {
        let _ = write!(head, "Content-Type: {}\r\n", response.content_type);
    }
    for (name, value) in &response.headers {
        let safe = value.replace(['\r', '\n'], "");
        let _ = write!(head, "{name}: {safe}\r\n");
    }
    head.push_str(if response.close {
        "Connection: close\r\n\r\n"
    } else {
        "Connection: keep-alive\r\n\r\n"
    });
    stream.write_all(head.as_bytes())?;
    if !response.body.is_empty() {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}

pub(super) fn write_json_error(
    stream: &mut UnixStream,
    status: u16,
    code: &str,
    message: &str,
    close: bool,
) -> std::io::Result<()> {
    let response =
        IpcResponse::json(status, &json!({ "error": message, "code": code })).with_close(close);
    write_response(stream, &response)
}

pub(super) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn read_crlf_line(
    reader: &mut BufReader<UnixStream>,
    max_len: usize,
) -> std::io::Result<Option<String>> {
    let mut raw = Vec::new();
    let n = reader.read_until(b'\n', &mut raw)?;
    if n == 0 {
        return Ok(None);
    }
    if raw.len() > max_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "line exceeds limit",
        ));
    }
    let Some(stripped) = raw.strip_suffix(b"\r\n") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing CRLF line ending",
        ));
    };
    if stripped.contains(&b'\r') || stripped.contains(&b'\n') || stripped.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control byte in HTTP head",
        ));
    }
    let text = std::str::from_utf8(stripped)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 HTTP head"))?;
    Ok(Some(text.to_string()))
}

pub(super) fn validate_idempotency_key(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(format!(
            "Idempotency-Key must be 1..={MAX_IDEMPOTENCY_KEY_LEN} bytes"
        ));
    }
    if value.bytes().any(|b| b <= 0x1F || b == 0x7F) {
        return Err("Idempotency-Key contains control characters".to_string());
    }
    Ok(value.to_string())
}

pub(super) fn read_request(reader: &mut BufReader<UnixStream>) -> ReadOutcome {
    let first = match read_crlf_line(reader, MAX_REQUEST_LINE_BYTES) {
        Ok(Some(line)) => line,
        Ok(None) => return ReadOutcome::Eof,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            return ReadOutcome::Eof;
        }
        Err(err) => return bad_request(format!("malformed request line: {err}"), true),
    };
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return bad_request("malformed HTTP/1.1 request line", true);
    };
    if version != "HTTP/1.1" || target.is_empty() || !target.starts_with('/') {
        return bad_request(
            "only HTTP/1.1 requests with origin-form targets are accepted",
            true,
        );
    }

    let (raw_headers, has_te, content_length) = match read_headers(reader) {
        Ok(parsed) => parsed,
        Err(err) => return err.into_outcome(),
    };
    if has_te {
        return length_required(
            "Transfer-Encoding is forbidden on spawn-ipc-v1; send Content-Length",
            true,
        );
    }
    if !matches!(method, "GET" | "HEAD") && content_length.is_none() {
        return length_required("Content-Length is mandatory on non-GET IPC requests", true);
    }
    let body_len = content_length.unwrap_or(0);
    if body_len > MAX_IPC_BODY_BYTES {
        return ReadOutcome::Error {
            status: 413,
            code: "payload_too_large",
            message: format!("request body {body_len} exceeds 4 MiB cap ({MAX_IPC_BODY_BYTES})"),
            close: true,
        };
    }

    let mut body = vec![0_u8; body_len];
    if body_len > 0
        && let Err(err) = reader.read_exact(&mut body)
    {
        return bad_request(format!("incomplete request body: {err}"), true);
    }

    let idempotency_key = if matches!(method, "GET" | "HEAD") {
        None
    } else {
        let Some(raw_key) = raw_headers.get("idempotency-key") else {
            return bad_request(
                "Idempotency-Key header is mandatory on non-GET requests",
                false,
            );
        };
        let key = match validate_idempotency_key(raw_key) {
            Ok(key) => key,
            Err(why) => return bad_request(why, false),
        };
        if !body.is_empty()
            && body.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{')
            && let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&body)
            && let Some(Value::String(body_key)) = obj.get("key")
            && body_key != &key
        {
            return bad_request("Idempotency-Key header differs from JSON body key", false);
        }
        Some(key)
    };

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let close = raw_headers.get("connection").is_some_and(|v| {
        v.split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
    });

    ReadOutcome::Request(IpcRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers: raw_headers,
        idempotency_key,
        body,
        close,
    })
}

fn read_headers(reader: &mut BufReader<UnixStream>) -> Result<ParsedHeaders, HeadError> {
    let mut headers = BTreeMap::new();
    let mut has_te = false;
    let mut content_length = None;
    for _ in 0..MAX_HEADERS {
        let line = read_crlf_line(reader, MAX_HEADER_LINE_BYTES)
            .map_err(|err| HeadError::bad_request(format!("malformed header: {err}")))?
            .ok_or_else(|| HeadError::bad_request("unexpected EOF in headers"))?;
        if line.is_empty() {
            return Ok((headers, has_te, content_length));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::bad_request("header missing colon"));
        };
        let lower = name.trim_end().to_ascii_lowercase();
        if lower.is_empty() || lower.bytes().any(|b| b <= 0x20 || b >= 0x7f) {
            return Err(HeadError::bad_request("invalid header name"));
        }
        let val = value.trim().to_string();
        if lower == "transfer-encoding" {
            has_te = true;
        } else if lower == "content-length" {
            if content_length.is_some()
                || val.is_empty()
                || !val.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(HeadError::bad_request(
                    "invalid or duplicate Content-Length",
                ));
            }
            let parsed = val
                .parse::<usize>()
                .map_err(|_| HeadError::bad_request("Content-Length overflow"))?;
            content_length = Some(parsed);
        }
        headers
            .entry(lower)
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&val);
            })
            .or_insert(val);
    }
    Err(HeadError::bad_request("too many headers"))
}
