#![cfg(unix)]
//! Tests for the local `out.sock` UNIX domain socket transport in `spawn-jev`,
//! plus cross-process verification against `spawn-node front --serve outbound`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spawn_core::observation::SemanticState;
use spawn_core::observe::{ExecutionPhase, JudgeError, JudgeInput, StateJudge};
use spawn_jev::{JevConfig, JevJudge, OUT_SOCK_ENV, parse_reply, validate_out_sock};

const THREE_ANSWERS: &str = r#"{"model":"jev-1.14.0","answers":{"semantic_state":{"type":"choice","choice":"BLOCKED","confidence":0.77,"probabilities":{"WORKING":0.0,"WAITING":0.18,"BLOCKED":0.82,"COMPLETED":0.0,"UNKNOWN":0.0}},"needs_human":{"type":"noul","noul":0.96},"stuck":{"type":"noul","noul":0.22}},"usage":{"input_tokens":550,"output_tokens":98}}"#;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("spj-{prefix}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sample_input() -> JudgeInput {
    JudgeInput {
        agent_id: "agent_one".to_string(),
        session_id: "session_one".to_string(),
        engine: Some(spawn_core::event::ENGINE.to_string()),
        phase: ExecutionPhase::Running,
        open_tools: 1,
        open_jobs: 0,
        oldest_job_ms: None,
        last_tool: Some("Bash".to_string()),
        permission_pending: false,
        screen: Some(spawn_core::screen::ScreenState::Busy),
        turn: None,
        last_error: None,
        evidence_seq: spawn_core::event::SessionSeq(1),
        ambiguity: None,
        quiet_ms: None,
        progress_ms: None,
        task: None,
        screen_tail: None,
        reply: None,
        perpetual: false,
    }
}

fn read_http_request<R: Read>(stream: &mut R) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).expect("read request");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head_end = end + 4;
            let head_text = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
            let content_length = head_text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buffer.len() >= head_end + content_length {
                break;
            }
        }
    }
    String::from_utf8(buffer).expect("HTTP request must be UTF-8")
}

fn stub_uds(
    sock: &Path,
    status: &str,
    body: &'static str,
    chunked: bool,
) -> thread::JoinHandle<String> {
    let listener = UnixListener::bind(sock).expect("bind unix listener");
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    let status = status.to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept unix connection");
        let request = read_http_request(&mut stream);
        let response = if chunked {
            let (head, tail) = body.split_at(body.len() / 2);
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n\
                 {:x}\r\n{head}\r\n{:x}\r\n{tail}\r\n0\r\n\r\n",
                head.len(),
                tail.len()
            )
        } else {
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        stream.write_all(response.as_bytes()).expect("write reply");
        stream.flush().ok();
        request
    })
}

#[test]
fn out_sock_refuses_missing_path() {
    let dir = TempDir::new("miss");
    let sock = dir.join("missing.sock");
    let err = validate_out_sock(&sock).expect_err("missing socket must fail");
    assert!(err.contains("stat out.sock"), "{err}");

    let err = JevJudge::from_out_sock(&sock)
        .judge(&sample_input())
        .expect_err("missing socket dial must fail");
    assert!(matches!(err, JudgeError::Unavailable(_)));
}

#[test]
fn out_sock_refuses_regular_file() {
    let dir = TempDir::new("reg");
    let file = dir.join("not-a-sock");
    std::fs::write(&file, b"not a socket").expect("write regular file");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    let err = validate_out_sock(&file).expect_err("regular file must be rejected");
    assert!(err.contains("not a UNIX domain socket"), "{err}");
}

#[test]
fn out_sock_refuses_symlink_to_valid_socket() {
    let dir = TempDir::new("sym");
    let real_sock = dir.join("real.sock");
    let _listener = UnixListener::bind(&real_sock).expect("bind real unix socket");
    std::fs::set_permissions(&real_sock, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600");
    validate_out_sock(&real_sock).expect("real 0600 socket is valid");

    let link = dir.join("link.sock");
    symlink(&real_sock, &link).expect("create symlink");
    let err = validate_out_sock(&link).expect_err("symlink must be refused");
    assert!(err.contains("symlink"), "{err}");

    let err = JevJudge::from_out_sock(&link)
        .judge(&sample_input())
        .expect_err("dial via symlink must fail");
    let JudgeError::Unavailable(msg) = err;
    assert!(msg.contains("symlink"), "{msg}");
}

#[test]
fn out_sock_refuses_group_and_world_permissions() {
    let dir = TempDir::new("perm");
    let sock = dir.join("out.sock");
    let _listener = UnixListener::bind(&sock).expect("bind unix socket");

    for bad_mode in [0o640, 0o644, 0o660, 0o604, 0o666] {
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(bad_mode))
            .expect("chmod bad mode");
        let err = validate_out_sock(&sock).expect_err("group/world permission must fail");
        assert!(
            err.contains("insecure permissions"),
            "{bad_mode:04o}: {err}"
        );
    }

    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    validate_out_sock(&sock).expect("0600 socket must pass");
}

#[test]
fn out_sock_env_lookup_reads_spawn_out_sock() {
    assert!(JevConfig::from_out_sock_lookup(|_| None).is_none());

    let mut vars = HashMap::new();
    vars.insert(OUT_SOCK_ENV.to_string(), "   ".to_string());
    assert!(JevConfig::from_out_sock_lookup(|k| vars.get(k).cloned()).is_none());

    vars.insert(
        OUT_SOCK_ENV.to_string(),
        " /run/spawn/out.sock ".to_string(),
    );
    let cfg = JevConfig::from_out_sock_lookup(|k| vars.get(k).cloned())
        .expect("non-empty SPAWN_OUT_SOCK constructs config");
    assert_eq!(
        cfg.out_sock.as_deref(),
        Some(Path::new("/run/spawn/out.sock"))
    );
}

#[test]
fn out_sock_posts_http11_with_explicit_content_length_and_parses_judgement() {
    let dir = TempDir::new("post");
    let sock = dir.join("out.sock");
    let handle = stub_uds(&sock, "200 OK", THREE_ANSWERS, false);

    let judge = JevJudge::from_out_sock(&sock);
    let judgement = judge.judge(&sample_input()).expect("out.sock judge passes");
    let expected = parse_reply(THREE_ANSWERS).expect("expected judgement parses");
    assert_eq!(judgement.state, expected.state);
    assert_eq!(judgement.state, SemanticState::Blocked);
    assert_eq!(judgement.confidence, expected.confidence);
    assert_eq!(judgement.probabilities, expected.probabilities);
    assert_eq!(judgement.needs_human, expected.needs_human);
    assert_eq!(judgement.stuck, expected.stuck);

    let raw_req = handle.join().expect("stub joins");
    let (head, body) = raw_req
        .split_once("\r\n\r\n")
        .expect("HTTP request has header separator");
    assert!(
        head.starts_with("POST /out/v1/jev/call HTTP/1.1\r\n"),
        "{head}"
    );
    let lower_head = head.to_ascii_lowercase();
    assert!(
        lower_head.contains("host: spawn-out\r\n"),
        "expected Host: spawn-out in {head}"
    );
    assert!(
        lower_head.contains("content-type: application/json\r\n"),
        "expected JSON content-type in {head}"
    );
    assert!(
        lower_head.contains(&format!("content-length: {}\r\n", body.len())),
        "expected exact Content-Length {} in {head}",
        body.len()
    );
    assert!(
        !lower_head.contains("transfer-encoding:"),
        "Transfer-Encoding must never be sent over out.sock: {head}"
    );
    assert!(
        lower_head.contains("connection: close"),
        "expected Connection: close in {head}"
    );
}

#[test]
fn out_sock_handles_chunked_and_http_error_responses() {
    let dir = TempDir::new("err");
    let sock_chunked = dir.join("chunked.sock");
    let handle = stub_uds(&sock_chunked, "200 OK", THREE_ANSWERS, true);
    let judgement = JevJudge::from_out_sock(&sock_chunked)
        .judge(&sample_input())
        .expect("chunked reply parses");
    assert_eq!(judgement.state, SemanticState::Blocked);
    handle.join().expect("chunked stub joins");

    let sock_err = dir.join("err.sock");
    let handle = stub_uds(
        &sock_err,
        "502 Bad Gateway",
        r#"{"error":"relay: upstream down"}"#,
        false,
    );
    let err = JevJudge::from_out_sock(&sock_err)
        .judge(&sample_input())
        .expect_err("502 must fail");
    let JudgeError::Unavailable(msg) = err;
    assert!(msg.contains("502"), "{msg}");
    assert!(msg.contains("upstream down"), "{msg}");
    handle.join().expect("err stub joins");
}

fn spawn_node_clean_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    for (k, _) in std::env::vars_os() {
        let upper = k.to_string_lossy().to_ascii_uppercase();
        if upper.contains("DATABASE_URL") {
            cmd.env_remove(&k);
        }
    }
    for proxy in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(proxy);
    }
    cmd.env_remove("TYPESAFE_API_KEY");
    cmd
}

fn start_mock_typesafe(expected_key: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock TypeSafe server");
    let addr = listener.local_addr().expect("mock TypeSafe local addr");
    let base_url = format!("http://{addr}");
    let handle = thread::spawn(move || {
        let mut saw_bearer_auth = false;
        for step in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept mock request");
            let req = read_http_request(&mut stream);
            assert!(
                req.starts_with("POST /v1/systemone HTTP/1.1"),
                "step {step} unexpected request: {req}"
            );
            if step > 0 {
                let expected_auth = format!("Authorization: Bearer {expected_key}");
                assert!(
                    req.lines().any(|l| l.eq_ignore_ascii_case(&expected_auth)),
                    "missing outbound Bearer header on step {step}"
                );
                saw_bearer_auth = true;
            }
            let (status, body) = if step < 2 {
                ("200 OK".to_string(), THREE_ANSWERS.to_string())
            } else {
                (
                    "502 Bad Gateway".to_string(),
                    format!("upstream rejected Bearer {expected_key} and token {expected_key}"),
                )
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).expect("write mock reply");
            stream.flush().ok();
        }
        assert!(saw_bearer_auth);
    });
    (base_url, handle)
}

fn spawn_outbound_ready(bin: &Path, sock: &Path, base_url: &str, cred_dir: &Path) -> ChildGuard {
    let allow_target = format!("{base_url}/v1/systemone");
    let mut cmd = spawn_node_clean_cmd(bin);
    cmd.args([
        "front",
        "--serve",
        "outbound",
        "--out-sock",
        sock.to_str().expect("utf-8 sock"),
        "--jev-base-url",
        base_url,
        "--jev-allowlist",
        &allow_target,
    ])
    .env("CREDENTIALS_DIRECTORY", cred_dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut guard = ChildGuard(cmd.spawn().expect("spawn spawn-node outbound"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = guard.0.try_wait().expect("try_wait") {
            let mut stderr = String::new();
            if let Some(mut s) = guard.0.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            panic!("spawn-node exited early with {status}: {stderr}");
        }
        if UnixStream::connect(sock).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    validate_out_sock(sock).expect("outbound socket ready and mode 0600");
    guard
}

#[test]
fn out_sock_cross_process_matches_direct_tcp_and_redacts_secret() {
    let Some(bin_os) = std::env::var_os("SPAWN_NODE_BIN") else {
        eprintln!("SPAWN_NODE_BIN not set; skipping live spawn-node out.sock cross-process test");
        return;
    };
    let bin = PathBuf::from(bin_os);
    assert!(
        bin.is_file(),
        "SPAWN_NODE_BIN {} is not a file",
        bin.display()
    );

    let fixture_token: &'static str = "test-dummy-jev-fixture-redaction-999";
    let (base_url, mock_handle) = start_mock_typesafe(fixture_token);

    let dir = TempDir::new("xproc");
    let cred_dir = dir.join("creds");
    std::fs::create_dir_all(&cred_dir).expect("create cred dir");
    let key_file = cred_dir.join("typesafe.api-key");
    std::fs::write(&key_file, format!("{fixture_token}\n")).expect("write test key file");
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).expect("chmod key");

    let sock = dir.join("out.sock");
    let guard = spawn_outbound_ready(&bin, &sock, &base_url, &cred_dir);

    let direct = JevJudge::new(JevConfig::new(&base_url))
        .judge(&sample_input())
        .expect("direct TCP judge succeeds");
    let via_sock = JevJudge::from_out_sock(&sock)
        .judge(&sample_input())
        .expect("out.sock judge succeeds");

    assert_eq!(via_sock.state, direct.state);
    assert_eq!(via_sock.confidence, direct.confidence);
    assert_eq!(via_sock.probabilities, direct.probabilities);
    assert_eq!(via_sock.needs_human, direct.needs_human);
    assert_eq!(via_sock.stuck, direct.stuck);

    let err = JevJudge::from_out_sock(&sock)
        .judge(&sample_input())
        .expect_err("502 response must error");
    let JudgeError::Unavailable(msg) = err;
    assert!(msg.contains("502"), "{msg}");
    assert!(
        !msg.contains(fixture_token),
        "secret leaked into error message: {msg}"
    );
    assert!(
        msg.contains("[key]") || msg.contains("[redacted]"),
        "expected redaction marker in {msg}"
    );

    drop(guard);
    mock_handle.join().expect("mock TypeSafe finishes");
}

#[test]
fn outbound_exits_78_on_missing_or_empty_systemd_credential() {
    let Some(bin_os) = std::env::var_os("SPAWN_NODE_BIN") else {
        eprintln!("SPAWN_NODE_BIN not set; skipping exit-78 credential test");
        return;
    };
    let bin = PathBuf::from(bin_os);
    let dir = TempDir::new("ex78");

    // 1. Missing $CREDENTIALS_DIRECTORY/typesafe.api-key -> exit 78.
    let empty_cred_dir = dir.join("missing-creds");
    std::fs::create_dir_all(&empty_cred_dir).expect("create missing cred dir");
    let out1 = spawn_node_clean_cmd(&bin)
        .args([
            "front",
            "--serve",
            "outbound",
            "--out-sock",
            dir.join("out1.sock").to_str().expect("utf-8"),
        ])
        .env("CREDENTIALS_DIRECTORY", &empty_cred_dir)
        .output()
        .expect("run spawn-node with missing cred");
    assert_eq!(
        out1.status.code(),
        Some(78),
        "expected exit 78 for missing credential, got {:?}: {}",
        out1.status,
        String::from_utf8_lossy(&out1.stderr)
    );

    // 2. Empty 0-byte $CREDENTIALS_DIRECTORY/typesafe.api-key -> exit 78.
    let zero_cred_dir = dir.join("empty-creds");
    std::fs::create_dir_all(&zero_cred_dir).expect("create zero cred dir");
    let key_file = zero_cred_dir.join("typesafe.api-key");
    std::fs::write(&key_file, b"").expect("write empty key");
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).expect("chmod key");
    let out2 = spawn_node_clean_cmd(&bin)
        .args([
            "front",
            "--serve",
            "outbound",
            "--out-sock",
            dir.join("out2.sock").to_str().expect("utf-8"),
        ])
        .env("CREDENTIALS_DIRECTORY", &zero_cred_dir)
        .output()
        .expect("run spawn-node with empty cred");
    assert_eq!(
        out2.status.code(),
        Some(78),
        "expected exit 78 for empty credential, got {:?}: {}",
        out2.status,
        String::from_utf8_lossy(&out2.stderr)
    );
}
