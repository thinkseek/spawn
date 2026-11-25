//! Shared by the gateway's integration tests: the golden fixtures of
//! `spawn-node-proto`, a gateway served in-process on `127.0.0.1:0`, and a
//! raw HTTP/1.1 client (so every byte on the wire is the test's).

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use spawn_node_gateway::{Connector, Gateway, Settings, spawn};
use spawn_node_proto::response::{RefusalBody, RequestId, verify_response};
use spawn_node_proto::{Method, NodeSigner, PublicKey, Refusal, SigningKey};

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../spawn-node-proto/testdata/node-protocol-v1")
}

pub fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

pub fn vectors() -> Value {
    serde_json::from_slice(&read("vectors.json")).expect("vectors.json")
}

pub fn v(key: &str) -> String {
    vectors()[key].as_str().expect(key).to_string()
}

fn seed(key: &str) -> [u8; 32] {
    let text = v(key);
    let bytes = spawn_node_proto::codec::unhex(&text, 64).expect("a hex seed");
    bytes.try_into().expect("32 bytes")
}

pub fn node_key() -> SigningKey {
    SigningKey::from_seed(&seed("node_seed_hex")).unwrap()
}

pub fn primary_key() -> SigningKey {
    SigningKey::from_seed(&seed("primary_seed_hex")).unwrap()
}

pub fn boot_id() -> String {
    v("boot_id")
}

pub fn node_id() -> String {
    v("node_id")
}

pub fn signer() -> NodeSigner {
    NodeSigner::new(node_key(), &boot_id()).unwrap()
}

/// A key that is not the node's (forged signatures).
pub fn stranger() -> NodeSigner {
    NodeSigner::new(SigningKey::from_seed(&[0x77; 32]).unwrap(), &boot_id()).unwrap()
}

pub fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A test's scratch directory, emptied first.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spawn-node-gateway-it-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A gateway serving on a port of its own, with the fixtures' primary key.
pub struct Harness {
    pub addr: SocketAddr,
    pub gateway: Arc<Gateway>,
    /// The clock: 0 follows the system's, anything else is the time.
    pub clock: Arc<AtomicU64>,
    pub artifacts: PathBuf,
    pub results: PathBuf,
}

/// A connector for a gateway with no database at all.
pub fn no_database() -> Connector {
    Arc::new(|| Err("no database in this test".to_string()))
}

impl Harness {
    pub fn start(name: &str, connector: Connector) -> Self {
        Self::start_with_prefix(name, connector, "")
    }

    pub fn start_with_prefix(name: &str, connector: Connector, prefix: &str) -> Self {
        let root = scratch(name);
        let artifacts = root.join("artifacts");
        let results = root.join("results");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::create_dir_all(&results).unwrap();
        let clock = Arc::new(AtomicU64::new(0));
        let ticking = Arc::clone(&clock);
        let gateway = Arc::new(Gateway::new(Settings {
            primary: primary_key(),
            cells: vec!["c0".to_string()],
            public_prefix: prefix.to_string(),
            artifacts: artifacts.clone(),
            results: results.clone(),
            clock: Arc::new(move || match ticking.load(Ordering::SeqCst) {
                0 => now_ms(),
                fixed => fixed,
            }),
            connector,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = spawn(listener, &gateway, 4).unwrap();
        Self {
            addr,
            gateway,
            clock,
            artifacts,
            results,
        }
    }

    pub fn set_clock(&self, ms: u64) {
        self.clock.store(ms, Ordering::SeqCst);
    }

    /// Remember the node's key as a verified beat would.
    pub fn remember_node(&self) {
        self.gateway
            .keys()
            .insert(&node_id(), node_key().public_key(), &boot_id());
    }

    /// One raw request, its answer.
    pub fn raw(&self, bytes: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        // A refused upload may be closed before all of it is written.
        let _ = stream.write_all(bytes);
        Reply::read(&mut stream)
    }

    /// A request with these headers (and `Content-Length` when `body` is
    /// not `None`).
    pub fn send(&self, method: &str, target: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Reply {
        let mut head = format!("{method} {target} HTTP/1.1\r\nHost: gateway\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = body {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("Connection: close\r\n\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body.unwrap_or_default());
        self.raw(&bytes)
    }

    /// A request signed by `signer` over `signed_body` (the whole body, or
    /// a result's metadata line).
    #[allow(clippy::too_many_arguments)]
    pub fn signed(
        &self,
        signer: &NodeSigner,
        method: Method,
        target: &str,
        seq: u64,
        ts_ms: u64,
        signed_body: &[u8],
        body: Option<&[u8]>,
        extra: &[(String, String)],
    ) -> Reply {
        let envelope = signer.sign(method, target, seq, ts_ms, signed_body).unwrap();
        let mut headers: Vec<(String, String)> = envelope
            .to_headers()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        headers.extend_from_slice(extra);
        self.send(method.as_str(), target, &headers, body)
    }

    pub fn post(&self, signer: &NodeSigner, target: &str, seq: u64, body: &[u8]) -> Reply {
        self.signed(signer, Method::Post, target, seq, now_ms(), body, Some(body), &[])
    }

    pub fn get(&self, signer: &NodeSigner, target: &str, seq: u64, extra: &[(String, String)]) -> Reply {
        self.signed(signer, Method::Get, target, seq, now_ms(), b"", None, extra)
    }
}

/// An answer as received.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    fn read(stream: &mut TcpStream) -> Self {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("a status line");
        let status = line
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("status line {line:?}"));
        let mut headers = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let text = line.trim_end_matches(['\r', '\n']);
            if text.is_empty() {
                break;
            }
            let (name, value) = text.split_once(':').unwrap();
            headers.push((name.to_string(), value.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
            .map(|(_, value)| value.parse().unwrap())
            .expect("every answer has a Content-Length");
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        Self {
            status,
            headers,
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!("{} body {:?}", self.status, String::from_utf8_lossy(&self.body))
        })
    }

    /// Checks the response signature against the fixtures' primary for the
    /// request `(node_id, boot_id, seq)`; returns `server_time_ms`.
    pub fn verify(&self, node_id: &str, boot_id: &str, seq: u64) -> u64 {
        verify_response(
            &primary_key().public_key(),
            self.status,
            RequestId {
                node_id,
                boot_id,
                seq,
            },
            self.header("Spawn-Primary-Key").expect("Spawn-Primary-Key"),
            self.header("Spawn-Primary-Sig").expect("Spawn-Primary-Sig"),
            &self.body,
        )
        .unwrap_or_else(|error| panic!("{} answer does not verify: {error}", self.status))
    }

    /// The node's own request was refused with `refusal`, signed.
    pub fn assert_refused(&self, refusal: Refusal, seq: u64) {
        assert_eq!(
            (self.status, self.json()["error"].as_str().unwrap_or_default()),
            (refusal.status(), refusal.error()),
            "{:?}",
            String::from_utf8_lossy(&self.body)
        );
        let time = self.verify(&node_id(), &boot_id(), seq);
        let body: RefusalBody = serde_json::from_slice(&self.body).unwrap();
        assert_eq!(body.server_time_ms, time);
    }
}

/// The fixtures' node public key.
pub fn node_public() -> PublicKey {
    node_key().public_key()
}
