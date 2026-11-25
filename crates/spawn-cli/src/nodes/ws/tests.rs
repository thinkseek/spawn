//! The WebSocket on this host's loopback: the upgrade (its accept key, its
//! bearer, a refusal), text and binary both ways, fragments assembled, a
//! ping answered, a close answered, masks enforced, frames bounded, a
//! frame cut across reads kept whole; and wss:// through rustls against a
//! test CA.

use super::{Message, WsStream, accept_of, tls_config_with};
use crate::nodes::coord::CoordUrl;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

/// A throwaway CA, and a `localhost` / 127.0.0.1 certificate it signed,
/// made with openssl for these tests only (100 years): they secure
/// nothing else.
pub(crate) const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBmDCCAT2gAwIBAgIUPHdwmiYreAupSxZcvYNVEbfGKO8wCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNU3Bhd24gVGVzdCBDQTAgFw0yNjExMjQxNjEzNThaGA8yMTI2
MTAzMTE2MTM1OFowGDEWMBQGA1UEAwwNU3Bhd24gVGVzdCBDQTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABHTZyMWfzzd68pAVPFqgcGsNcVeparVLxG3LhfQZxe+2
UOOem6m9LdAbbelSyPU5dCprzP63h7dFI91is2IAl9ujYzBhMB0GA1UdDgQWBBR1
ddA+4pXV7BmOkH0/DAnG8KZ0dTAfBgNVHSMEGDAWgBR1ddA+4pXV7BmOkH0/DAnG
8KZ0dTAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAKBggqhkjOPQQD
AgNJADBGAiEA6iznOrqQ90bx/F7LbbXc7Uee7IzReqAviiYP1HAAah4CIQCsgePz
iA4GzIQPoGcLN3sPSd5kw1riyR0xedFY2uWj0Q==
-----END CERTIFICATE-----
";
const TEST_LEAF: &str = "-----BEGIN CERTIFICATE-----
MIIBwzCCAWmgAwIBAgIUfyN/dTZrL5lqOyIzTWyZVn1+BUIwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNU3Bhd24gVGVzdCBDQTAgFw0yNjExMjQxNjEzNThaGA8yMTI2
MTAzMTE2MTM1OFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAEyMBVg5mWxN8S6gwvGXurumZDMlCvmMzJL00dgqFQfsD+UO/R
ltGrLSnImkhBaQwOOozgSZj9BvIgZ+vqQ8S9y6OBkjCBjzAaBgNVHREEEzARggls
b2NhbGhvc3SHBH8AAAEwDAYDVR0TAQH/BAIwADATBgNVHSUEDDAKBggrBgEFBQcD
ATAOBgNVHQ8BAf8EBAMCB4AwHQYDVR0OBBYEFN2yTuv8nFkK/IRd132pyLlrQAt4
MB8GA1UdIwQYMBaAFHV10D7ildXsGY6QfT8MCcbwpnR1MAoGCCqGSM49BAMCA0gA
MEUCIA7k64gpR4t5lWwJlrEdV3biI09zs48J4Z1TjI4EOKjLAiEA5o2BMo6urhJT
S1TnVDw2XSPL1QGhYp/i7k5uo4OVH0I=
-----END CERTIFICATE-----
";
/// The test certificate's key (P-256, PKCS#8): a throwaway, as above.
const TEST_LEAF_KEY: &str = concat!(
    "-----BEGIN ",
    "PRIVATE KEY-----\n",
    "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgaU7qymnqwsQ9gfgn\n",
    "Ne1K/TxlNGlU1E53W0l5jI0LGkyhRANCAATIwFWDmZbE3xLqDC8Ze6u6ZkMyUK+Y\n",
    "zMkvTR2CoVB+wP5Q79GW0astKciaSEFpDA46jOBJmP0G8iBn6+pDxL3L\n",
    "-----END ",
    "PRIVATE KEY-----\n"
);

fn url(text: &str) -> CoordUrl {
    CoordUrl::parse(text).expect("a URL")
}

/// A server on loopback that accepts one upgrade (with `bearer`) and hands
/// the stream to `then`.
fn upgrade_once<F>(
    bearer: Option<&'static str>,
    then: F,
) -> (u16, std::thread::JoinHandle<Result<(), String>>)
where
    F: FnOnce(WsStream, super::Accepted) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(|error| error.to_string())?;
        let (ws, accepted) = WsStream::accept(stream, bearer, T)?;
        then(ws, accepted);
        Ok(())
    });
    (port, handle)
}

#[test]
fn the_accept_key_is_rfc_6455s() {
    // RFC 6455 section 1.3's example.
    assert_eq!(
        accept_of("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
fn a_client_and_a_server_exchange_text_binary_fragments_pings_and_a_close() {
    let (port, server) = upgrade_once(Some("coord-token"), |mut ws, accepted| {
        assert_eq!(accepted.path, "/mesh?zone=eu");
        assert!(!accepted.bearer, "no bearer crosses ws:// in the clear");
        // Echo what comes, until a close.
        loop {
            match ws.recv(T) {
                Ok(Some(Message::Text(text))) => {
                    ws.send_text(&format!("echo {text}")).expect("sent");
                }
                Ok(Some(Message::Binary(bytes))) => ws.send_binary(&bytes).expect("sent"),
                Ok(Some(Message::Close)) | Err(_) => break,
                Ok(None) => {}
            }
        }
    });
    let mut client = WsStream::connect(
        &url(&format!("ws://127.0.0.1:{port}/mesh?zone=eu")),
        Some("coord-token"),
        T,
        None,
    )
    .expect("connected");
    client.send_text("hello").expect("sent");
    assert_eq!(client.recv(T), Ok(Some(Message::Text("echo hello".into()))));
    // Every length class: 7 bits, 16, 64.
    for len in [0, 125, 126, 65_535, 65_536, 300_000] {
        let bytes: Vec<u8> = (0..len)
            .map(|i| u8::try_from(i % 251).expect("a byte"))
            .collect();
        client.send_binary(&bytes).expect("sent");
        assert_eq!(client.recv(T), Ok(Some(Message::Binary(bytes))), "{len}");
    }
    // A fragmented text (masked by hand, as a client must) and a ping in
    // between: the ping is answered, the fragments assembled.
    let frame = |fin: bool, opcode: u8, payload: &[u8]| {
        let mask = [1_u8, 2, 3, 4];
        let mut frame = vec![
            if fin { 0x80 } else { 0 } | opcode,
            0x80 | u8::try_from(payload.len()).expect("short"),
        ];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    };
    client.raw(&frame(false, 0x1, b"frag")).expect("sent");
    client
        .raw(&frame(true, 0x9, b"are you there"))
        .expect("a ping");
    client.raw(&frame(true, 0x0, b"mented")).expect("sent");
    assert_eq!(
        client.recv(T),
        Ok(Some(Message::Text("echo fragmented".into())))
    );
    // Nothing more: none within the wait.
    assert_eq!(client.recv(Duration::from_millis(100)), Ok(None));
    client.close();
    server.join().expect("the server").expect("upgraded");
}

#[test]
fn an_upgrade_with_another_bearer_or_none_of_a_websocket_is_refused() {
    // A bearer (sent by hand: a wss:// client's, past its TLS) that is
    // not the server's: 401; the server's: upgraded, and said so.
    let upgrade = |port: u16, bearer: &str| {
        let mut raw = TcpStream::connect(("127.0.0.1", port)).expect("connected");
        raw.set_read_timeout(Some(T)).expect("a timeout");
        let request = format!(
            "GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             Authorization: Bearer {bearer}\r\n\r\n"
        );
        raw.write_all(request.as_bytes()).expect("sent");
        let mut answer = vec![0_u8; 12];
        raw.read_exact(&mut answer).expect("an answer");
        String::from_utf8(answer).expect("text")
    };
    let (port, server) = upgrade_once(Some("right"), |_, _| {});
    assert_eq!(upgrade(port, "wrong"), "HTTP/1.1 401");
    assert!(server.join().expect("the server").is_err());
    let (port, server) = upgrade_once(Some("right"), |_, accepted| assert!(accepted.bearer));
    assert_eq!(upgrade(port, "right"), "HTTP/1.1 101");
    server.join().expect("the server").expect("upgraded");
    // Without a bearer: upgraded, and said so (the coordinator then wants
    // the hello's own auth).
    let (port, server) = upgrade_once(Some("right"), |_, accepted| assert!(!accepted.bearer));
    WsStream::connect(&url(&format!("ws://127.0.0.1:{port}/")), None, T, None).expect("upgraded");
    server.join().expect("the server").expect("upgraded");
    // A plain HTTP request is no upgrade.
    let (port, server) = upgrade_once(None, |_, _| {});
    let mut plain = TcpStream::connect(("127.0.0.1", port)).expect("connected");
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("sent");
    let mut answer = String::new();
    let _ = plain.read_to_string(&mut answer);
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    assert!(server.join().expect("the server").is_err());
}

#[test]
fn masks_are_enforced_and_frames_bounded() {
    // A server refuses an unmasked frame from its client.
    let (port, server) = upgrade_once(None, |mut ws, _| {
        let error = ws.recv(T).expect_err("unmasked");
        assert!(error.contains("unmasked"), "{error}");
    });
    let mut client = WsStream::connect(&url(&format!("ws://127.0.0.1:{port}")), None, T, None)
        .expect("connected");
    client.raw(&[0x81, 0x02, b'h', b'i']).expect("sent");
    server.join().expect("the server").expect("upgraded");
    // A frame announcing more than 512 KiB is refused before it is read.
    let (port, server) = upgrade_once(None, |mut ws, _| {
        let error = ws.recv(T).expect_err("too long");
        assert!(error.contains("over"), "{error}");
    });
    let mut client = WsStream::connect(&url(&format!("ws://127.0.0.1:{port}")), None, T, None)
        .expect("connected");
    let mut huge = vec![0x82, 0x80 | 127];
    huge.extend_from_slice(&(u64::try_from(super::MAX_FRAME).expect("len") + 1).to_be_bytes());
    huge.extend_from_slice(&[0, 0, 0, 0]);
    client.raw(&huge).expect("sent");
    server.join().expect("the server").expect("upgraded");
    assert!(client.send_binary(&vec![0; super::MAX_FRAME + 1]).is_err());
}

#[test]
fn a_frame_cut_across_reads_waits_whole_and_a_close_is_answered() {
    let (port, server) = upgrade_once(None, |mut ws, _| {
        // Nothing whole within the first wait: none, and the half kept.
        assert_eq!(ws.recv(Duration::from_millis(300)), Ok(None));
        assert_eq!(ws.recv(T), Ok(Some(Message::Text("whole".into()))));
        assert_eq!(ws.recv(T), Ok(Some(Message::Close)));
        assert!(ws.closed());
    });
    let mut client = WsStream::connect(&url(&format!("ws://127.0.0.1:{port}")), None, T, None)
        .expect("connected");
    let mask = [9_u8, 8, 7, 6];
    let mut frame = vec![0x81, 0x80 | 5];
    frame.extend_from_slice(&mask);
    frame.extend(b"whole".iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    client.raw(&frame[..4]).expect("half");
    std::thread::sleep(Duration::from_millis(500));
    client.raw(&frame[4..]).expect("the rest");
    client.close();
    // The server's close comes back.
    assert_eq!(client.recv(T).ok().flatten(), Some(Message::Close));
    server.join().expect("the server").expect("upgraded");
}

#[test]
fn wss_goes_through_tls_verified_against_its_ca_and_refuses_another() {
    use rustls_pki_types::pem::PemObject as _;
    let cert =
        rustls_pki_types::CertificateDer::from_pem_slice(TEST_LEAF.as_bytes()).expect("a cert");
    let key =
        rustls_pki_types::PrivateKeyDer::from_pem_slice(TEST_LEAF_KEY.as_bytes()).expect("a key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("a server config");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    let tls_server = {
        let config = Arc::clone(&config);
        std::thread::spawn(move || {
            for round in 0..2 {
                let (stream, _) = listener.accept().expect("a connection");
                stream.set_read_timeout(Some(T)).expect("a timeout");
                let connection = rustls::ServerConnection::new(Arc::clone(&config)).expect("tls");
                let mut tls = rustls::StreamOwned::new(connection, stream);
                // The second client trusts another CA: its handshake fails.
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if tls.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                if round == 1 {
                    assert!(head.is_empty(), "the other client never upgraded");
                    return;
                }
                let head = String::from_utf8(head).expect("text");
                let key = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                    .expect("a key");
                let response = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                     Sec-WebSocket-Accept: {}\r\n\r\n",
                    accept_of(key.trim())
                );
                tls.write_all(response.as_bytes()).expect("upgraded");
                // One masked text frame in, echoed unmasked.
                let mut header = [0_u8; 6];
                tls.read_exact(&mut header).expect("a frame");
                let len = usize::from(header[1] & 0x7F);
                let mut payload = vec![0_u8; len];
                tls.read_exact(&mut payload).expect("its payload");
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= header[2 + i % 4];
                }
                let mut frame = vec![0x81, u8::try_from(len).expect("short")];
                frame.extend_from_slice(&payload);
                tls.write_all(&frame).expect("echoed");
                tls.flush().expect("flushed");
            }
        })
    };
    let trusted = tls_config_with(TEST_CA.as_bytes()).expect("a client config");
    let mut client = WsStream::connect(
        &url(&format!("wss://localhost:{port}/mesh")),
        None,
        T,
        Some(trusted),
    )
    .expect("a wss connection");
    client.send_text("over tls").expect("sent");
    assert_eq!(client.recv(T), Ok(Some(Message::Text("over tls".into()))));
    drop(client);
    // The Mozilla roots alone do not trust the test CA.
    let error = WsStream::connect(
        &url(&format!("wss://localhost:{port}/mesh")),
        None,
        T,
        Some(tls_config_with(b"").expect("a client config")),
    )
    .err()
    .expect("an untrusted certificate");
    assert!(!error.is_empty());
    tls_server.join().expect("the tls server");
    assert!(
        tls_config_with(b"-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----\n").is_err()
    );
}

/// Review M3: a server that accepts and then says nothing (no TLS
/// handshake, no upgrade) costs a connect its timeout, not forever.
#[test]
fn a_silent_server_fails_a_wss_or_ws_connect_within_its_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    let held = std::thread::spawn(move || {
        // Accept both connections and hold them, silent.
        let first = listener.accept().expect("a connection");
        let second = listener.accept().expect("a connection");
        std::thread::sleep(Duration::from_secs(3));
        drop((first, second));
    });
    for scheme in ["wss", "ws"] {
        let begun = std::time::Instant::now();
        let outcome = WsStream::connect(
            &url(&format!("{scheme}://localhost:{port}/mesh")),
            None,
            Duration::from_millis(500),
            Some(tls_config_with(TEST_CA.as_bytes()).expect("a config")),
        );
        assert!(outcome.is_err(), "{scheme}: connected to a silent server");
        assert!(
            begun.elapsed() < Duration::from_secs(2),
            "{scheme}: {:?}",
            begun.elapsed()
        );
    }
    held.join().expect("the silent server");
}

/// Review M5: on ws:// the coordinator's token never crosses the network
/// in the clear (no Authorization header); on wss:// it does, inside TLS.
#[test]
fn a_ws_upgrade_never_carries_the_bearer_in_the_clear() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    let captured = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("a connection");
        stream.set_read_timeout(Some(T)).expect("a timeout");
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        String::from_utf8(head).expect("text")
    });
    let _ = WsStream::connect(
        &url(&format!("ws://127.0.0.1:{port}/")),
        Some("coord-secret-token"),
        Duration::from_millis(800),
        None,
    );
    let head = captured.join().expect("captured");
    assert!(head.starts_with("GET / HTTP/1.1"), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("authorization"),
        "{head}"
    );
    assert!(!head.contains("coord-secret-token"), "{head}");
}

/// The rustls server configuration of the test certificate (TLS 1.3).
pub(crate) fn test_server_config() -> Arc<rustls::ServerConfig> {
    use rustls_pki_types::pem::PemObject as _;
    let cert =
        rustls_pki_types::CertificateDer::from_pem_slice(TEST_LEAF.as_bytes()).expect("a cert");
    let key =
        rustls_pki_types::PrivateKeyDer::from_pem_slice(TEST_LEAF_KEY.as_bytes()).expect("a key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("a server config");
    Arc::new(config)
}

/// The server end of a test `wss://` connection.
pub(crate) type TlsServer = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

/// TLS accepted on `stream` (the test certificate) and its WebSocket
/// upgrade answered.
pub(crate) fn accept_tls_upgrade(stream: TcpStream) -> TlsServer {
    stream.set_read_timeout(Some(T)).expect("a timeout");
    let connection = rustls::ServerConnection::new(test_server_config()).expect("tls");
    let mut tls = rustls::StreamOwned::new(connection, stream);
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(tls.read(&mut byte).expect("the upgrade"), 1, "an upgrade");
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).expect("text");
    let key = head
        .lines()
        .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
        .expect("a key");
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_of(key.trim())
    );
    tls.write_all(response.as_bytes()).expect("upgraded");
    tls.flush().expect("flushed");
    tls
}

/// One masked client frame read whole: its payload, unmasked.
pub(crate) fn read_client_frame(tls: &mut TlsServer) -> Vec<u8> {
    let mut head = [0_u8; 2];
    tls.read_exact(&mut head).expect("a frame");
    let len = match head[1] & 0x7F {
        126 => {
            let mut bytes = [0_u8; 2];
            tls.read_exact(&mut bytes).expect("its length");
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0_u8; 8];
            tls.read_exact(&mut bytes).expect("its length");
            usize::try_from(u64::from_be_bytes(bytes)).expect("a length")
        }
        len => usize::from(len),
    };
    let mut mask = [0_u8; 4];
    tls.read_exact(&mut mask).expect("its mask");
    let mut payload = vec![0_u8; len];
    tls.read_exact(&mut payload).expect("its payload");
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    payload
}

/// A server frame of `opcode` (unmasked) carrying `payload`.
pub(crate) fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        len @ 0..=125 => frame.push(u8::try_from(len).expect("short")),
        len @ 126..=0xFFFF => {
            frame.push(126);
            frame.extend_from_slice(&u16::try_from(len).expect("a length").to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&u64::try_from(len).expect("a length").to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    frame
}

/// `bytes`, then a TLS close_notify, in one flight: both encrypted before
/// either is written, then written back to back.
pub(crate) fn send_then_close_notify(tls: &mut TlsServer, bytes: &[u8]) {
    tls.conn.writer().write_all(bytes).expect("buffered");
    tls.conn.send_close_notify();
    while tls.conn.wants_write() {
        tls.conn.write_tls(&mut tls.sock).expect("written");
    }
    tls.sock.flush().expect("flushed");
}

/// Review of #569 (F1): the pump's way over `wss://` (read_ready, then
/// next_buffered: the plaintext rustls opened taken with no IO) delivers
/// a flight of frames, one of them across many TLS records; and a last
/// message that comes in the same flight as the TLS close_notify is still
/// delivered, the end said only after it.
#[test]
fn a_wss_pump_takes_buffered_frames_and_the_last_one_before_close_notify() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    let big = vec![b'z'; 100 * 1024];
    let (flight_sent, wait_flight) = std::sync::mpsc::channel();
    let server = {
        let big = big.clone();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a connection");
            let mut tls = accept_tls_upgrade(stream);
            // A flight of three: small, 100 KiB (many records), small.
            let mut flight = server_frame(0x1, b"one");
            flight.extend(server_frame(0x2, &big));
            flight.extend(server_frame(0x1, b"three"));
            tls.write_all(&flight).expect("a flight");
            tls.flush().expect("flushed");
            // Asked once more: the answer and the close in one flight.
            assert_eq!(read_client_frame(&mut tls), b"last?");
            send_then_close_notify(&mut tls, &server_frame(0x1, b"the last answer"));
            flight_sent.send(()).expect("said");
            std::thread::sleep(Duration::from_millis(300));
        })
    };
    let mut client = WsStream::connect(
        &url(&format!("wss://localhost:{port}/mesh")),
        None,
        T,
        Some(tls_config_with(TEST_CA.as_bytes()).expect("a client config")),
    )
    .expect("a wss connection");
    let deadline = std::time::Instant::now() + T;
    let mut got = Vec::new();
    while got.len() < 3 {
        assert!(std::time::Instant::now() < deadline, "only {got:?}");
        while let Some(message) = client.next_buffered().expect("a message") {
            got.push(message);
        }
        if got.len() < 3 {
            client.read_ready().expect("a read");
        }
    }
    assert_eq!(
        got,
        vec![
            Message::Text("one".into()),
            Message::Binary(big),
            Message::Text("three".into())
        ]
    );
    client.send_text("last?").expect("asked");
    wait_flight.recv_timeout(T).expect("the flight");
    // The whole flight (the answer, then close_notify) is on the socket
    // now: one read takes both.
    std::thread::sleep(Duration::from_millis(100));
    client
        .read_ready()
        .expect("the end is not said by the read");
    assert_eq!(
        client.next_buffered(),
        Ok(Some(Message::Text("the last answer".into())))
    );
    assert!(client.next_buffered().is_err(), "the end, said after it");
    assert!(client.closed());
    server.join().expect("the server");
}
