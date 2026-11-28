//! A small synchronous HTTP/1.1 server over `std::net`, just what the
//! gateway's five routes need: the request target exactly as sent (never
//! decoded), every header with its repeats, a body read only when the
//! handler asks for it and never past its `Content-Length`, and responses
//! that always carry a `Content-Length` (a file body is streamed, never
//! buffered).
//!
//! Not `tiny_http`: when a request with a large declared `Content-Length` is
//! answered unread, its reader's drop allocates the whole remainder at once
//! (an attacker's 2^40 bytes abort the process), and a malformed
//! `Content-Length` is silently taken for no body. Here a request the
//! handler did not read to its end closes the connection, and the
//! `Content-Length` is the handler's to judge.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The request line and headers together: at most this many bytes.
pub const MAX_HEAD: u64 = 16 * 1024;
/// At most this many header lines.
pub const MAX_HEADERS: usize = 64;
/// One read that waits longer than this (a stalled body) fails.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// A peer that does not read its answer for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_mins(1);
/// What every answer carries unless it says otherwise: no cache and no
/// proxy keeps or rewrites a gateway answer (signed bodies would no longer
/// match their signature, and refusals and grants are for one request).
pub const DEFAULT_CACHE_CONTROL: &str = "no-store, no-transform";
/// Requests served on one kept-alive connection before it is closed.
const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
/// Connections waiting for a worker, per worker; past the channel bound
/// (`max(MIN_BACKLOG, workers * BACKLOG_PER_WORKER)`), a new connection is
/// closed at once.
const BACKLOG_PER_WORKER: usize = 256;
/// Minimum socket queue capacity so a 1C/1G gateway (`workers = 6`) absorbs
/// simultaneous bursts (1,000+ connections) cleanly without dropping sockets.
const MIN_BACKLOG: usize = 1024;

/// How long a connection may take, so that slow or idle peers cannot hold
/// the workers: every wait is bounded by a deadline, not only per read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Between requests (and before the first): a kept-alive connection
    /// idle this long is closed.
    pub idle: Duration,
    /// From a request's first byte to the end of its head.
    pub head: Duration,
    /// A body's deadline: this, plus its length at `body_min_rate`.
    pub body_base: Duration,
    /// The slowest upload taken, in bytes per second.
    pub body_min_rate: u64,
    /// After a refusal that left the body unread: at most this many body
    /// bytes are read and discarded (after the answer is written), so the
    /// client reads the answer instead of a reset...
    pub drain_bytes: u64,
    /// ...for at most this long.
    pub drain_time: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(65),
            head: Duration::from_secs(10),
            body_base: Duration::from_secs(10),
            body_min_rate: 256 * 1024,
            drain_bytes: 4 * 1024 * 1024,
            drain_time: Duration::from_secs(2),
        }
    }
}

impl Timeouts {
    /// The time a body of `len` bytes may take.
    pub fn body(&self, len: u64) -> Duration {
        self.body_base + Duration::from_secs(len / self.body_min_rate.max(1))
    }
}

/// The socket's read side under a deadline: each read waits at most until
/// the deadline (and at most [`READ_TIMEOUT`]); past it, reads fail with
/// `TimedOut`.
pub struct TimedStream {
    stream: TcpStream,
    deadline: Cell<Option<Instant>>,
}

impl TimedStream {
    fn set_deadline(&self, after: Duration) {
        self.deadline.set(Some(Instant::now() + after));
    }
}

impl Read for TimedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = match self.deadline.get() {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline passed"));
                }
                left.min(READ_TIMEOUT)
            }
            None => READ_TIMEOUT,
        };
        self.stream.set_read_timeout(Some(wait))?;
        self.stream.read(buf)
    }
}

/// One request: the head parsed, the body still on the socket.
pub struct Request<'a> {
    /// HTTP request method (`GET`, `POST`, etc.).
    pub method: String,
    /// The request target, byte for byte.
    pub target: String,
    /// Every header line, in order, repeats kept.
    pub headers: Vec<(String, String)>,
    /// The TCP peer.
    pub peer: IpAddr,
    body: io::Take<&'a mut BufReader<TimedStream>>,
    writer: &'a mut TcpStream,
    continue_pending: bool,
}

impl Request<'_> {
    /// The values of header `name` (case-insensitive), in order.
    pub fn header_values<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'s str> + 's {
        self.headers
            .iter()
            .filter(move |(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// How many times header `name` was sent.
    pub fn header_count(&self, name: &str) -> usize {
        self.header_values(name).count()
    }

    /// The single value of header `name`; `None` when absent or repeated.
    pub fn header(&self, name: &str) -> Option<&str> {
        let mut found = None;
        for (field, value) in &self.headers {
            if field.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return None;
                }
                found = Some(value.as_str());
            }
        }
        found
    }

    /// The body, at most its framed `Content-Length` (nothing when the
    /// request is not framed by exactly one canonical `Content-Length`).
    /// The first call answers an `Expect: 100-continue`.
    pub fn body(&mut self) -> &mut impl BufRead {
        if self.continue_pending {
            self.continue_pending = false;
            // A failure shows when the body is read.
            let _ = self.writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        &mut self.body
    }

    /// Body bytes framed but not read yet.
    pub fn body_remaining(&self) -> u64 {
        self.body.limit()
    }
}

/// A response body.
pub enum Body {
    /// In-memory byte buffer payload.
    Bytes(Vec<u8>),
    /// `len` bytes of `file` from `offset`, streamed.
    File {
        /// Open file handle to stream from.
        file: File,
        /// Byte offset inside `file` to start streaming from.
        offset: u64,
        /// Exact byte length to stream.
        len: u64,
    },
}

/// One response; `Content-Length` is always written from the body.
pub struct Response {
    /// HTTP status code (e.g., `200`, `400`).
    pub status: u16,
    /// Additional response headers `(name, value)` beyond `Content-Length`.
    pub headers: Vec<(String, String)>,
    /// Response payload bytes or streamed file body.
    pub body: Body,
}

impl Response {
    /// Builds a response with an in-memory byte payload and empty header list.
    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Body::Bytes(body),
        }
    }

    /// Appends a header `(name, value)` to the response builder.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// The first value of header `name`.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn content_length(&self) -> u64 {
        match &self.body {
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::File { len, .. } => *len,
        }
    }
}

/// The reason phrase of a status.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// What a worker does with requests. Each worker thread owns one (its own
/// database connection, say).
pub trait Handler: Send {
    /// The answer to a parsed request.
    fn handle(&mut self, request: &mut Request<'_>) -> Response;
    /// The answer to bytes that are not an HTTP/1.x request head (the
    /// connection is closed after it).
    fn malformed(&mut self, peer: IpAddr) -> Response;
}

/// Serve `listener` with `workers` threads, each with the handler `make`
/// gives it; returns the bound address. The threads run until the process
/// exits.
pub fn spawn<H, F>(
    listener: TcpListener,
    workers: usize,
    timeouts: Timeouts,
    make: F,
) -> io::Result<SocketAddr>
where
    H: Handler + 'static,
    F: Fn() -> H,
{
    let workers = workers.max(1);
    let capacity = workers.saturating_mul(BACKLOG_PER_WORKER).max(MIN_BACKLOG);
    spawn_with_backlog(listener, workers, capacity, timeouts, make)
}

fn spawn_with_backlog<H, F>(
    listener: TcpListener,
    workers: usize,
    capacity: usize,
    timeouts: Timeouts,
    make: F,
) -> io::Result<SocketAddr>
where
    H: Handler + 'static,
    F: Fn() -> H,
{
    let addr = listener.local_addr()?;
    let (sender, receiver) = sync_channel::<TcpStream>(capacity);
    let receiver = Arc::new(Mutex::new(receiver));
    for index in 0..workers {
        let receiver = Arc::clone(&receiver);
        let handler = make();
        std::thread::Builder::new()
            .name(format!("gateway-worker-{index}"))
            .spawn(move || worker(&receiver, handler, timeouts))?;
    }
    std::thread::Builder::new()
        .name("gateway-accept".into())
        .spawn(move || accept(&listener, &sender))?;
    Ok(addr)
}

fn write_overload_503(mut stream: TcpStream) {
    const BODY: &[u8] = b"{\"error\":\"service unavailable\"}";
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        BODY.len()
    );
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    let _ = stream.set_nodelay(true);
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(BODY);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn accept(listener: &TcpListener, sender: &SyncSender<TcpStream>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match sender.try_send(stream) {
                Ok(()) => {}
                Err(TrySendError::Full(stream)) => write_overload_503(stream),
                Err(TrySendError::Disconnected(_)) => return,
            },
            Err(error) => {
                eprintln!("spawn-node-gateway: accept: {error}");
                // EMFILE and its kin: do not spin.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn worker<H: Handler>(receiver: &Mutex<Receiver<TcpStream>>, mut handler: H, timeouts: Timeouts) {
    loop {
        let stream = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            receiver.recv()
        };
        let Ok(stream) = stream else { return };
        if let Err(error) = connection(stream, &mut handler, timeouts)
            && !matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::TimedOut
            )
        {
            eprintln!("spawn-node-gateway: connection: {error}");
        }
    }
}

/// The head of one request, or why there is none.
enum Head {
    Request {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        http10: bool,
    },
    /// The peer closed between requests.
    Closed,
    Malformed,
}

fn connection<H: Handler>(
    stream: TcpStream,
    handler: &mut H,
    timeouts: Timeouts,
) -> io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let peer = stream.peer_addr()?.ip();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(TimedStream {
        stream,
        deadline: Cell::new(None),
    });
    for _ in 0..MAX_REQUESTS_PER_CONNECTION {
        // Idle until a request's first byte, then the whole head within
        // its deadline.
        reader.get_ref().set_deadline(timeouts.idle);
        match reader.fill_buf() {
            Ok([]) => return Ok(()),
            Ok(_) => {}
            Err(error) if is_idle_end(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
        reader.get_ref().set_deadline(timeouts.head);
        let (method, target, headers, http10) = match read_head(&mut reader)? {
            Head::Request {
                method,
                target,
                headers,
                http10,
            } => (method, target, headers, http10),
            Head::Closed => return Ok(()),
            Head::Malformed => {
                let response = handler.malformed(peer);
                return write_response(&mut writer, response, true);
            }
        };
        let framed = framed_length(&headers);
        let expect_continue = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("Expect") && value.eq_ignore_ascii_case("100-continue")
        });
        let wants_close = http10
            || headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("Connection")
                    && value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("close"))
            });
        reader
            .get_ref()
            .set_deadline(timeouts.body(framed.unwrap_or(0)));
        let mut request = Request {
            method,
            target,
            headers,
            peer,
            body: (&mut reader).take(framed.unwrap_or(0)),
            writer: &mut writer,
            continue_pending: expect_continue,
        };
        let response = handler.handle(&mut request);
        // A body not read to its end (or a request not framed by one
        // Content-Length) leaves the stream where the next request cannot
        // be found: the connection ends with this answer.
        let unread = framed.is_none() || request.body_remaining() > 0;
        // A client told to wait for `100 Continue` and never told sends no
        // body: nothing to drain.
        let body_coming = unread && !request.continue_pending;
        let close = wants_close || unread;
        drop(request);
        write_response(&mut writer, response, close)?;
        if close {
            if body_coming {
                drain(&mut reader, timeouts);
            }
            return Ok(());
        }
    }
    Ok(())
}

/// Reads and discards what the client still sends of a refused body, up to
/// [`Timeouts::drain_bytes`] and [`Timeouts::drain_time`], after the answer
/// is written and the write side shut: a client still writing its upload
/// (Go's `net/http`) then reads the signed refusal instead of `broken
/// pipe`, and a peer cannot keep the worker past the bound.
fn drain(reader: &mut BufReader<TimedStream>, timeouts: Timeouts) {
    reader.get_ref().set_deadline(timeouts.drain_time);
    let _ = io::copy(&mut reader.take(timeouts.drain_bytes), &mut io::sink());
}

/// The body length of a request framed by exactly one `Content-Length` of
/// ASCII digits (no sign, no space) and no `Transfer-Encoding`; a request
/// with neither frames an empty body.
fn framed_length(headers: &[(String, String)]) -> Option<u64> {
    let mut lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("Content-Length"));
    let chunked = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("Transfer-Encoding"));
    match (lengths.next(), lengths.next(), chunked) {
        (None, None, false) => Some(0),
        (Some((_, value)), None, false)
            if !value.is_empty()
                && value.len() <= 19
                && value.bytes().all(|b| b.is_ascii_digit()) =>
        {
            value.parse().ok()
        }
        _ => None,
    }
}

fn read_head(reader: &mut BufReader<TimedStream>) -> io::Result<Head> {
    let mut budget = reader.by_ref().take(MAX_HEAD);
    let mut line = Vec::new();
    // Tolerate empty lines before a request line (RFC 9112 §2.2).
    loop {
        line.clear();
        let read = match budget.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(error) if is_idle_end(&error) => return Ok(Head::Closed),
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(if budget.limit() == 0 {
                Head::Malformed
            } else {
                Head::Closed
            });
        }
        if line != b"\r\n" && line != b"\n" {
            break;
        }
    }
    let Some(request_line) = strip_eol(&line) else {
        return Ok(Head::Malformed);
    };
    let Some((method, target, http10)) = parse_request_line(request_line) else {
        return Ok(Head::Malformed);
    };
    let mut headers = Vec::new();
    loop {
        line.clear();
        budget.read_until(b'\n', &mut line)?;
        let Some(text) = strip_eol(&line) else {
            return Ok(Head::Malformed);
        };
        if text.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Ok(Head::Malformed);
        }
        let Some(header) = parse_header(text) else {
            return Ok(Head::Malformed);
        };
        headers.push(header);
    }
    Ok(Head::Request {
        method,
        target,
        headers,
        http10,
    })
}

/// The peer went quiet or away before sending a new request.
fn is_idle_end(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
    )
}

/// A line without its `\r\n` (or bare `\n`); `None` when it has no end
/// (the head's budget ran out) or holds a byte that is not visible ASCII,
/// space or tab.
fn strip_eol(line: &[u8]) -> Option<&str> {
    let line = line.strip_suffix(b"\n")?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line
        .iter()
        .all(|&b| b == b'\t' || (b' '..=b'~').contains(&b))
    {
        std::str::from_utf8(line).ok()
    } else {
        None
    }
}

fn parse_request_line(line: &str) -> Option<(String, String, bool)> {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return None,
    };
    if method.is_empty() || !method.bytes().all(is_token_byte) || target.is_empty() {
        return None;
    }
    Some((method.to_string(), target.to_string(), http10))
}

fn parse_header(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once(':')?;
    // No obs-fold, no space before the colon.
    if name.is_empty() || !name.bytes().all(is_token_byte) {
        return None;
    }
    Some((
        name.to_string(),
        value.trim_matches(|c| c == ' ' || c == '\t').to_string(),
    ))
}

/// RFC 9110 `tchar`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn write_response(stream: &mut TcpStream, response: Response, close: bool) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
        response.status,
        reason(response.status),
        response.content_length()
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if response.header_value("Cache-Control").is_none() {
        head.push_str("Cache-Control: ");
        head.push_str(DEFAULT_CACHE_CONTROL);
        head.push_str("\r\n");
    }
    if close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut out = io::BufWriter::with_capacity(64 * 1024, &mut *stream);
    out.write_all(head.as_bytes())?;
    match response.body {
        Body::Bytes(bytes) => out.write_all(&bytes)?,
        Body::File {
            mut file,
            offset,
            len,
        } => {
            file.seek(SeekFrom::Start(offset))?;
            let copied = io::copy(&mut file.take(len), &mut out)?;
            if copied != len {
                // The file shrank under us: the framing is broken, so is
                // the connection.
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "artifact shorter than its length",
                ));
            }
        }
    }
    out.flush()?;
    drop(out);
    if close {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn only_one_plain_content_length_frames_a_body() {
        assert_eq!(framed_length(&headers(&[])), Some(0));
        assert_eq!(
            framed_length(&headers(&[("content-length", "12")])),
            Some(12)
        );
        for bad in [
            headers(&[("Content-Length", "+1")]),
            headers(&[("Content-Length", "1 2")]),
            headers(&[("Content-Length", "")]),
            headers(&[("Content-Length", "99999999999999999999")]),
            headers(&[("Content-Length", "1"), ("Content-Length", "1")]),
            headers(&[("Transfer-Encoding", "chunked")]),
            headers(&[("Content-Length", "1"), ("Transfer-Encoding", "chunked")]),
        ] {
            assert_eq!(framed_length(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn heads_are_strict() {
        assert!(parse_request_line("GET /node/v1/spec HTTP/1.1").is_some());
        assert!(parse_request_line("GET  /node/v1/spec HTTP/1.1").is_none());
        assert!(parse_request_line("GET /node/v1/spec HTTP/2.0").is_none());
        assert!(parse_request_line("G(T /x HTTP/1.1").is_none());
        assert_eq!(
            parse_header("Spawn-Node-Seq:  7 "),
            Some(("Spawn-Node-Seq".into(), "7".into()))
        );
        assert!(parse_header(" folded").is_none());
        assert!(parse_header("Bad Name: x").is_none());
        assert_eq!(strip_eol(b"abc\r\n"), Some("abc"));
        assert_eq!(strip_eol(b"abc"), None);
        assert_eq!(strip_eol(b"a\x00c\r\n"), None);
    }

    #[test]
    fn default_idle_timeout_is_65_seconds() {
        assert_eq!(Timeouts::default().idle, Duration::from_secs(65));
    }

    #[test]
    fn backlog_overflow_returns_503_without_stalling_server() {
        use std::sync::mpsc::sync_channel;

        struct GateHandler {
            entered: SyncSender<()>,
            release: Receiver<()>,
        }

        impl Handler for GateHandler {
            fn handle(&mut self, request: &mut Request<'_>) -> Response {
                if request.target == "/block" {
                    let _ = self.entered.send(());
                    let _ = self.release.recv();
                }
                Response {
                    status: 200,
                    headers: Vec::new(),
                    body: Body::Bytes(b"ok".to_vec()),
                }
            }
            fn malformed(&mut self, _peer: IpAddr) -> Response {
                Response {
                    status: 400,
                    headers: Vec::new(),
                    body: Body::Bytes(Vec::new()),
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let (entered_tx, entered_rx) = sync_channel::<()>(4);
        let (release_tx, release_rx) = sync_channel::<()>(4);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let addr = spawn_with_backlog(listener, 1, 1, Timeouts::default(), move || GateHandler {
            entered: entered_tx.clone(),
            release: {
                let (tx, rx) = sync_channel::<()>(1);
                let shared = Arc::clone(&release_rx);
                std::thread::spawn(move || {
                    let _ = shared.lock().unwrap().recv();
                    let _ = tx.send(());
                });
                rx
            },
        })
        .expect("spawn server");

        let send = |path: &str| {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            write!(
                s,
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            s.flush().unwrap();
            s
        };

        let mut req1 = send("/block");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker entered");
        let mut req2 = send("/queued");
        std::thread::sleep(Duration::from_millis(30));

        // Backlog (capacity=1) is now full while the sole worker is blocked.
        // Subsequent connections must receive an explicit 503 immediately.
        for _ in 0..3 {
            let mut s = TcpStream::connect(addr).expect("connect overflow");
            s.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let _ = write!(
                s,
                "GET /overflow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            );
            let mut raw = String::new();
            s.read_to_string(&mut raw).expect("read 503");
            assert!(
                raw.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                "expected explicit 503, got {raw:?}"
            );
            assert!(raw.contains("\"service unavailable\""), "{raw:?}");
        }

        release_tx.send(()).unwrap();
        let mut b1 = String::new();
        let mut b2 = String::new();
        req1.read_to_string(&mut b1).unwrap();
        req2.read_to_string(&mut b2).unwrap();
        assert!(b1.starts_with("HTTP/1.1 200 OK"), "{b1:?}");
        assert!(b2.starts_with("HTTP/1.1 200 OK"), "{b2:?}");
    }
}
