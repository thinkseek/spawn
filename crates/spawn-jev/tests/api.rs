//! Public API tests for spawn-jev.
//!
//! The judge talks plain HTTP/1.1 over `TcpStream`, so the tests run a local
//! stub server: no network, no extra dependency.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use serde_json::Value;
use spawn_core::observation::{Confidence, SemanticState};
use spawn_core::observe::{JudgeError, JudgeInput, StateJudge};
use spawn_jev::{JevConfig, JevJudge, build_request, endpoint, parse_reply};

/// The answer body observed from the running service.
const ANSWER: &str = r#"{"answers":{"semantic_state":{"type":"choice","choice":"WORKING","confidence":0.8284565105428603,"probabilities":{"WORKING":0.8284565105428603,"WAITING":0.16},"none_probability":0.009251}},"usage":{"input_tokens":155,"output_tokens":0},"model":"StartLux-Decision-v1.1-9B-Instant"}"#;

fn input() -> JudgeInput {
    JudgeInput {
        agent_id: "agent_one".to_string(),
        session_id: "session_one".to_string(),
        engine: Some(spawn_core::event::Engine::Claude),
        execution_active: true,
        stalled_end: None,
        last_end: None,
        exit_code: None,
        open_tools: 1,
        last_tool: Some("Bash".to_string()),
        permission_pending: false,
        screen: Some(spawn_core::screen::ScreenState::Busy),
        turn: None,
        last_error: None,
        evidence_seq: spawn_core::event::SessionSeq(1),
        ambiguity: None,
        quiet_ms: None,
        task: None,
        screen_tail: None,
        perpetual: false,
    }
}

/// A live three-question answer, captured from jev-1.14.0 on 2026-11-04.
const THREE_ANSWERS: &str = r#"{"model":"jev-1.14.0","answers":{"semantic_state":{"type":"choice","choice":"BLOCKED","confidence":0.77,"probabilities":{"WORKING":0.0,"WAITING":0.18,"BLOCKED":0.82,"COMPLETED":0.0,"UNKNOWN":0.0}},"needs_human":{"type":"noul","noul":0.96},"stuck":{"type":"noul","noul":0.22}},"usage":{"input_tokens":550,"output_tokens":98}}"#;

#[test]
fn reply_carries_the_distribution_and_both_yes_no_answers() {
    let judgement = parse_reply(THREE_ANSWERS).expect("three answers parse");
    assert_eq!(judgement.state, SemanticState::Blocked);
    let distribution = judgement.probabilities.expect("a distribution");
    assert!(
        (distribution[2] - 0.82).abs() < 1e-9,
        "BLOCKED is the third state"
    );
    assert_eq!(judgement.needs_human, Some(0.96));
    assert_eq!(judgement.stuck, Some(0.22));
}

#[test]
fn request_asks_the_state_and_two_yes_no_questions() {
    let body = build_request(&input());
    let value: Value = serde_json::from_str(&body).expect("request is JSON");
    assert_eq!(value["questions"]["needs_human"]["type"], "noul");
    assert_eq!(value["questions"]["stuck"]["type"], "noul");
}

/// One-shot stub: accepts a single request, replies, returns base URL and the
/// raw request text.
fn stub(status: &str, body: &'static str, chunked: bool) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let address = listener.local_addr().expect("local address");
    let status = status.to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one connection");
        let request = read_request(&mut stream);
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
    });
    (format!("http://{address}"), handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).expect("read the request");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head_end = end + 4;
            let length = content_length(&buffer[..head_end]);
            if buffer.len() >= head_end + length {
                break;
            }
        }
    }
    String::from_utf8(buffer).expect("request is UTF-8")
}

fn content_length(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("content-length:") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

#[test]
fn request_names_one_choice_question_in_declaration_order() {
    let body = build_request(&input());
    let value: Value = serde_json::from_str(&body).expect("request is JSON");
    assert_eq!(value["state"]["agent_id"], "agent_one");
    assert_eq!(
        value["model"], "jev-latest",
        "the hosted service requires a model"
    );
    let question = &value["questions"]["semantic_state"];
    assert_eq!(question["type"], "choice");
    assert!(question["criteria"].is_object(), "criteria is an object");
    // The service renders one option per criteria entry, so the raw order
    // matters; a JSON map would hide it.
    let mut previous = 0;
    for state in SemanticState::ALL {
        let marker = format!("\"{}\":", state.key());
        let position = body.find(&marker).expect("criteria holds every state");
        assert!(
            position >= previous,
            "criteria follow the declaration order"
        );
        previous = position;
    }
}

#[test]
fn reply_parses_the_live_answer() {
    let judgement = parse_reply(ANSWER).expect("live answer parses");
    assert_eq!(judgement.state, SemanticState::Working);
    assert!(
        (judgement.confidence.value() - 0.828_456_5).abs() < 1e-6,
        "confidence is carried through"
    );
}

#[test]
fn reply_falls_back_to_the_largest_probability() {
    let body = r#"{"answers":{"semantic_state":{"probabilities":{"WORKING":0.2,"BLOCKED":0.7}}}}"#;
    let judgement = parse_reply(body).expect("answer without a choice parses");
    assert_eq!(judgement.state, SemanticState::Blocked);
    assert_eq!(judgement.confidence, Confidence::ZERO);
}

#[test]
fn reply_rejects_a_missing_answer() {
    let error = parse_reply(r#"{"answers":{}}"#).expect_err("an empty answer is an error");
    assert!(matches!(error, JudgeError::Unavailable(_)));
}

#[test]
fn judge_posts_to_the_endpoint_and_reads_the_answer() {
    let (base, handle) = stub("200 OK", ANSWER, false);
    let judge = JevJudge::new(JevConfig::new(&base));
    let judgement = judge.judge(&input()).expect("judge succeeds");
    assert_eq!(judgement.state, SemanticState::Working);
    let request = handle.join().expect("stub finishes");
    assert!(
        request.starts_with("POST /v1/systemone HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("semantic_state"), "{request}");
}

#[test]
fn judge_reads_a_chunked_answer() {
    let (base, handle) = stub("200 OK", ANSWER, true);
    let judge = JevJudge::new(JevConfig::new(&base));
    let judgement = judge.judge(&input()).expect("chunked answer parses");
    assert_eq!(judgement.state, SemanticState::Working);
    handle.join().expect("stub finishes");
}

#[test]
fn judge_reports_the_status_and_body_of_a_failure() {
    let (base, handle) = stub("503 Service Unavailable", r#"{"error":"loading"}"#, false);
    let judge = JevJudge::new(JevConfig::new(&base));
    let error = judge.judge(&input()).expect_err("a 503 is an error");
    let JudgeError::Unavailable(message) = error;
    assert!(message.contains("503"), "{message}");
    assert!(message.contains("loading"), "{message}");
    handle.join().expect("stub finishes");
}

#[test]
fn judge_reports_a_dead_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let address = listener.local_addr().expect("local address");
    drop(listener);
    let judge = JevJudge::new(JevConfig::new(&format!("http://{address}")));
    let error = judge
        .judge(&input())
        .expect_err("a closed port is an error");
    assert!(matches!(error, JudgeError::Unavailable(_)));
}

#[test]
fn endpoint_trims_the_trailing_slash() {
    assert_eq!(endpoint("http://host:1/"), "http://host:1/v1/systemone");
}
