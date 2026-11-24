//! A WebSocket (RFC 6455), synchronous, for the coordinators (issue #87):
//! the client a node's `spawn node connect` (and the primary's probe of a
//! coord node) opens to `ws://` or `wss://` (TLS by rustls on ring,
//! verified against the Mozilla roots of webpki-roots and, when set, the
//! PEM certificates of `SPAWN_COORD_CA_FILE`), and the server end of
//! `spawn node coord --ws`. No async runtime, no thread of its own: one
//! owner reads and writes it, each read bounded by its timeout.
//!
//! - The client upgrade carries `Authorization: Bearer <token>`
//!   (`SPAWN_COORD_TOKEN`) on `wss://` only, never in the clear on
//!   `ws://`; the server says whether it matched
//!   ([`Accepted::bearer`]), and refuses one that does not (401).
//! - Client frames are masked with 4 random bytes, server frames never;
//!   each side refuses the other's frames masked the wrong way.
//! - Text, binary and continuation frames are assembled into one message
//!   of at most [`MAX_FRAME`] bytes; a ping is answered with its pong, a
//!   close with its close.
//! - The TLS handshake and the upgrade are bounded by the connect's
//!   timeout, every read and write of them by the time left.
//! - The tcp connect is super::transport's ([`connect_happy`]: IPv6 and
//!   IPv4 interleaved, each address bounded).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use sha1::Digest as _;

use super::coord::{CoordUrl, Scheme};
use super::crypto::random;
use super::transport::connect_happy;

/// The largest message (and frame) taken or sent.
pub(crate) const MAX_FRAME: usize = 512 * 1024;
/// The largest HTTP head of an upgrade.
const MAX_HEAD: usize = 16 * 1024;
/// RFC 6455's GUID, appended to the key whose SHA-1 the server answers.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The tcp stream under a WebSocket: plain, or in TLS.
pub(crate) enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => stream.get_ref(),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

/// A message received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    Text(String),
    Binary(Vec<u8>),
    /// The peer closed (its close answered).
    Close,
}

/// The rustls client configuration: the Mozilla roots, and those of
/// `SPAWN_COORD_CA_FILE` (PEM) when set.
pub(crate) fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let extra = match crate::config::env_value("SPAWN_COORD_CA_FILE") {
        Some(file) => {
            std::fs::read(&file).map_err(|error| format!("SPAWN_COORD_CA_FILE: {error}"))?
        }
        None => Vec::new(),
    };
    tls_config_with(&extra)
}

/// [`tls_config`] with `pem`'s certificates as roots too.
pub(crate) fn tls_config_with(pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls_pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !pem.is_empty() {
        for cert in rustls_pki_types::CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|_| "a CA file that is not PEM certificates".to_string())?;
            roots
                .add(cert)
                .map_err(|error| format!("a CA certificate refused: {error}"))?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| error.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// What the server end of an upgrade learnt of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Accepted {
    /// The request's path (and query).
    pub(crate) path: String,
    /// Whether it carried the bearer token asked for (false without one,
    /// or when none was asked).
    pub(crate) bearer: bool,
}

/// One end of a WebSocket.
pub(crate) struct WsStream {
    conn: Conn,
    /// Read and not yet parsed.
    buffer: Vec<u8>,
    /// Whether this end is the client (it masks what it sends).
    client: bool,
    closed: bool,
    /// A fragmented message being assembled: its opcode and bytes.
    partial: Option<(u8, Vec<u8>)>,
}

/// The header named `name` (any case) in an HTTP head's lines.
fn header<'a>(lines: &[&'a str], name: &str) -> Option<&'a str> {
    lines.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The `Sec-WebSocket-Accept` of `key`.
fn accept_of(key: &str) -> String {
    let mut sha = sha1::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sha.finalize())
}

/// Read an HTTP head (up to its blank line) by `deadline`: the head, and
/// what came after it.
fn read_head(conn: &mut Conn, deadline: Instant) -> Result<(String, Vec<u8>), String> {
    let mut read = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(end) = read.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = read.split_off(end + 4);
            let head = String::from_utf8(read).map_err(|_| "an upgrade that is not text")?;
            return Ok((head, rest));
        }
        if read.len() > MAX_HEAD {
            return Err("an upgrade over 16 KiB".to_string());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no upgrade in time".to_string());
        }
        conn.tcp()
            .set_read_timeout(Some(left))
            .map_err(|error| error.to_string())?;
        match conn.read(&mut chunk) {
            Ok(0) => return Err("the connection closed during the upgrade".to_string()),
            Ok(count) => read.extend_from_slice(&chunk[..count]),
            Err(error) if timed_out(&error) => return Err("no upgrade in time".to_string()),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Bound `tcp`'s reads and writes by the time left before `deadline`.
fn bound(tcp: &TcpStream, deadline: Instant) -> Result<(), String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("no upgrade in time".to_string());
    }
    tcp.set_read_timeout(Some(left))
        .and_then(|()| tcp.set_write_timeout(Some(left)))
        .map_err(|error| error.to_string())
}

fn timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

impl WsStream {
    /// Connect to `url` (`ws://` or `wss://`), upgrade with `bearer` as
    /// the `Authorization` when given, all by `timeout`; `tls`, the
    /// client configuration for `wss://` ([`tls_config`] when none).
    pub(crate) fn connect(
        url: &CoordUrl,
        bearer: Option<&str>,
        timeout: Duration,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let addresses: Vec<SocketAddr> = (url.bare_host(), url.port)
            .to_socket_addrs()
            .map_err(|error| format!("{url}: {error}"))?
            .collect();
        let mut tcp = connect_happy(url, &addresses, deadline)?;
        let _ = tcp.set_nodelay(true);
        bound(&tcp, deadline).map_err(|error| format!("{url}: {error}"))?;
        let mut conn = match url.scheme {
            Scheme::Ws => Conn::Plain(tcp),
            Scheme::Wss => {
                let config = match tls {
                    Some(config) => config,
                    None => tls_config()?,
                };
                let name = rustls_pki_types::ServerName::try_from(url.bare_host().to_string())
                    .map_err(|_| format!("{url}: no TLS name"))?;
                let mut client = rustls::ClientConnection::new(config, name)
                    .map_err(|error| format!("{url}: {error}"))?;
                // The handshake, driven here under the deadline (each read
                // and write bounded by the time left), never inside a
                // first write with no bound.
                while client.is_handshaking() {
                    bound(&tcp, deadline).map_err(|error| format!("{url}: {error}"))?;
                    client.complete_io(&mut tcp).map_err(|error| {
                        if timed_out(&error) {
                            format!("{url}: no TLS handshake in time")
                        } else {
                            format!("{url}: {error}")
                        }
                    })?;
                }
                Conn::Tls(Box::new(rustls::StreamOwned::new(client, tcp)))
            }
            Scheme::Tcp => return Err(format!("{url} is no WebSocket")),
        };
        let key = base64::engine::general_purpose::STANDARD.encode(random::<16>()?);
        let default_port = if url.scheme == Scheme::Wss { 443 } else { 80 };
        let host = if url.port == default_port {
            url.host.clone()
        } else {
            format!("{}:{}", url.host, url.port)
        };
        let mut request = format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: spawn-node\r\n",
            url.path
        );
        // The bearer only inside TLS: on ws:// it would cross the network in
        // the clear (the hello's coord_auth proves the token there).
        if let Some(bearer) = bearer.filter(|_| url.scheme == Scheme::Wss) {
            use std::fmt::Write as _;
            let _ = write!(request, "Authorization: Bearer {bearer}\r\n");
        }
        request.push_str("\r\n");
        bound(conn.tcp(), deadline).map_err(|error| format!("{url}: {error}"))?;
        conn.write_all(request.as_bytes())
            .and_then(|()| conn.flush())
            .map_err(|error| format!("{url}: {error}"))?;
        let (head, rest) =
            read_head(&mut conn, deadline).map_err(|error| format!("{url}: {error}"))?;
        let lines: Vec<&str> = head.split("\r\n").collect();
        let status = lines.first().copied().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            let said = status.chars().take(80).collect::<String>();
            return Err(format!("{url}: the upgrade was refused ({said})"));
        }
        if !header(&lines, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            || header(&lines, "sec-websocket-accept") != Some(accept_of(&key).as_str())
        {
            return Err(format!("{url}: not a WebSocket upgrade"));
        }
        Ok(Self {
            conn,
            buffer: rest,
            client: true,
            closed: false,
            partial: None,
        })
    }

    /// The server end of an upgrade on `stream`, read by `timeout`: one
    /// whose `Authorization` names another bearer than `bearer` is refused
    /// (401), as is anything that is no WebSocket upgrade (400).
    pub(crate) fn accept(
        stream: TcpStream,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<(Self, Accepted), String> {
        let deadline = Instant::now() + timeout;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| error.to_string())?;
        let _ = stream.set_nodelay(true);
        let mut conn = Conn::Plain(stream);
        let (head, rest) = read_head(&mut conn, deadline)?;
        let lines: Vec<&str> = head.split("\r\n").collect();
        let mut request = lines
            .first()
            .copied()
            .unwrap_or_default()
            .split_whitespace();
        let (method, path) = (request.next(), request.next());
        let key = header(&lines, "sec-websocket-key")
            .filter(|key| {
                base64::engine::general_purpose::STANDARD
                    .decode(key)
                    .is_ok_and(|bytes| bytes.len() == 16)
            })
            .map(str::to_string);
        let upgrade =
            header(&lines, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let connection = header(&lines, "connection").is_some_and(|v| {
            v.split(',')
                .any(|word| word.trim().eq_ignore_ascii_case("upgrade"))
        });
        let (Some("GET"), Some(path), Some(key), true, true, Some("13")) = (
            method,
            path,
            key,
            upgrade,
            connection,
            header(&lines, "sec-websocket-version"),
        ) else {
            let _ = conn.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return Err("not a WebSocket upgrade".to_string());
        };
        let given = header(&lines, "authorization").and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        });
        let matched = match (bearer, given) {
            (Some(wanted), Some(given)) => {
                if !super::auth::same(given.trim(), wanted) {
                    let _ = conn.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return Err("an upgrade with another bearer token".to_string());
                }
                true
            }
            _ => false,
        };
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            accept_of(&key)
        );
        conn.write_all(response.as_bytes())
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                conn,
                buffer: rest,
                client: false,
                closed: false,
                partial: None,
            },
            Accepted {
                path: path.to_string(),
                bearer: matched,
            },
        ))
    }

    /// The peer's address.
    pub(crate) fn peer(&self) -> Option<SocketAddr> {
        self.conn.tcp().peer_addr().ok()
    }

    /// Whether the peer closed, or this end did.
    #[cfg(test)]
    pub(crate) fn closed(&self) -> bool {
        self.closed
    }

    /// Send one frame of `opcode`.
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        if payload.len() > MAX_FRAME {
            return Err(format!("a message over {MAX_FRAME} bytes"));
        }
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_bit = if self.client { 0x80 } else { 0 };
        match payload.len() {
            len @ 0..=125 => frame.push(mask_bit | u8::try_from(len).unwrap_or(125)),
            len @ 126..=0xFFFF => {
                frame.push(mask_bit | 0x7E);
                frame.extend_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_be_bytes());
            }
            len => {
                frame.push(mask_bit | 0x7F);
                frame.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
            }
        }
        if self.client {
            let mask = random::<4>()?;
            frame.extend_from_slice(&mask);
            frame.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(i, byte)| byte ^ mask[i % 4]),
            );
        } else {
            frame.extend_from_slice(payload);
        }
        self.conn
            .write_all(&frame)
            .and_then(|()| self.conn.flush())
            .map_err(|error| format!("the WebSocket: {error}"))
    }

    /// Send a text message.
    pub(crate) fn send_text(&mut self, text: &str) -> Result<(), String> {
        if self.closed {
            return Err("the WebSocket is closed".to_string());
        }
        self.send_frame(0x1, text.as_bytes())
    }

    /// Send a binary message.
    #[cfg(test)]
    pub(crate) fn send_binary(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.closed {
            return Err("the WebSocket is closed".to_string());
        }
        self.send_frame(0x2, bytes)
    }

    /// Close (normal closure), once.
    pub(crate) fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.send_frame(0x8, &1000_u16.to_be_bytes());
        }
    }

    /// One whole frame of the buffer, when there is one: `(fin, opcode,
    /// payload)`.
    fn parse_frame(&mut self) -> Result<Option<(bool, u8, Vec<u8>)>, String> {
        let buffer = &self.buffer;
        if buffer.len() < 2 {
            return Ok(None);
        }
        let fin = buffer[0] & 0x80 != 0;
        if buffer[0] & 0x70 != 0 {
            return Err("a frame with reserved bits set".to_string());
        }
        let opcode = buffer[0] & 0x0F;
        let masked = buffer[1] & 0x80 != 0;
        if masked == self.client {
            return Err(if self.client {
                "a masked frame from the server".to_string()
            } else {
                "an unmasked frame from the client".to_string()
            });
        }
        let (len, mut at) = match buffer[1] & 0x7F {
            126 if buffer.len() >= 4 => (u64::from(u16::from_be_bytes([buffer[2], buffer[3]])), 4),
            127 if buffer.len() >= 10 => {
                let mut bytes = [0_u8; 8];
                bytes.copy_from_slice(&buffer[2..10]);
                (u64::from_be_bytes(bytes), 10)
            }
            126 | 127 => return Ok(None),
            len => (u64::from(len), 2),
        };
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > MAX_FRAME {
            return Err(format!("a frame over {MAX_FRAME} bytes"));
        }
        if opcode >= 0x8 && (len > 125 || !fin) {
            return Err("a control frame too long or fragmented".to_string());
        }
        let mask = if masked {
            if buffer.len() < at + 4 {
                return Ok(None);
            }
            let mask = [buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]];
            at += 4;
            Some(mask)
        } else {
            None
        };
        if buffer.len() < at + len {
            return Ok(None);
        }
        let mut payload: Vec<u8> = self.buffer.drain(..at + len).skip(at).collect();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok(Some((fin, opcode, payload)))
    }

    /// Read more into the buffer by `deadline`: false when nothing came in
    /// time.
    fn fill(&mut self, deadline: Instant) -> Result<bool, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        self.conn
            .tcp()
            .set_read_timeout(Some(left))
            .map_err(|error| error.to_string())?;
        let mut chunk = [0_u8; 16 * 1024];
        match self.conn.read(&mut chunk) {
            Ok(0) => {
                self.closed = true;
                Err("the WebSocket's connection closed".to_string())
            }
            Ok(count) => {
                self.buffer.extend_from_slice(&chunk[..count]);
                Ok(true)
            }
            Err(error) if timed_out(&error) => Ok(false),
            Err(error) => {
                self.closed = true;
                Err(format!("the WebSocket: {error}"))
            }
        }
    }

    /// The next message, waiting at most `timeout`: none when none came
    /// whole in time (what came is kept for the next call). A ping is
    /// answered, a pong dropped, a close answered and said.
    pub(crate) fn recv(&mut self, timeout: Duration) -> Result<Option<Message>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some((fin, opcode, payload)) = self.parse_frame()? else {
                if self.fill(deadline)? {
                    continue;
                }
                return Ok(None);
            };
            match opcode {
                0x9 => self.send_frame(0xA, &payload)?,
                0xA => {}
                0x8 => {
                    self.close();
                    return Ok(Some(Message::Close));
                }
                0x0 => {
                    let (first, mut bytes) =
                        self.partial.take().ok_or("a continuation of no message")?;
                    if bytes.len() + payload.len() > MAX_FRAME {
                        return Err(format!("a message over {MAX_FRAME} bytes"));
                    }
                    bytes.extend_from_slice(&payload);
                    if fin {
                        return Self::message(first, bytes).map(Some);
                    }
                    self.partial = Some((first, bytes));
                }
                0x1 | 0x2 => {
                    if self.partial.is_some() {
                        return Err("a new message inside a fragmented one".to_string());
                    }
                    if fin {
                        return Self::message(opcode, payload).map(Some);
                    }
                    self.partial = Some((opcode, payload));
                }
                _ => return Err(format!("an unknown opcode {opcode}")),
            }
        }
    }

    fn message(opcode: u8, bytes: Vec<u8>) -> Result<Message, String> {
        if opcode == 0x1 {
            String::from_utf8(bytes)
                .map(Message::Text)
                .map_err(|_| "a text message that is not UTF-8".to_string())
        } else {
            Ok(Message::Binary(bytes))
        }
    }

    /// Send a raw frame (tests: a peer that breaks the protocol).
    #[cfg(test)]
    pub(crate) fn raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.conn
            .write_all(bytes)
            .map_err(|error| error.to_string())
    }
}

impl Drop for WsStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests;
