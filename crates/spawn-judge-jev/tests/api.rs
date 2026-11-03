use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use spawn_judge_jev::{JevConfig, JevJudge, state_from_wire_key, state_wire_key};
use spawn_observer::{SessionSnapshot, StateJudge};
use spawn_types::clock::Timestamp;
use spawn_types::observation::SemanticState;

fn start_fake_server(response_body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer);
            let body = response_body.as_bytes();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();
        }
    });
    format!("http://127.0.0.1:{}", address.port())
}

#[test]
fn wire_key_round_trips_every_state() {
    for state in [
        SemanticState::Working,
        SemanticState::Waiting,
        SemanticState::Blocked,
        SemanticState::Completed,
        SemanticState::Unknown,
    ] {
        assert_eq!(state_from_wire_key(state_wire_key(state)), Some(state));
    }
    assert!(state_from_wire_key("NOT_A_STATE").is_none());
}

#[test]
fn judge_maps_systemone_choice_answer() {
    let body = r#"{
        "model": "startlux-9b",
        "answers": {
            "semantic_state": {
                "type": "choice",
                "choice": "BLOCKED",
                "confidence": 0.91,
                "probabilities": {"BLOCKED": 0.91}
            }
        }
    }"#;
    let base_url = start_fake_server(body);
    let mut config = JevConfig::new(base_url);
    config.timeout = Duration::from_secs(5);
    let judge = JevJudge::new(config);

    let judgement = judge
        .judge(&SessionSnapshot::default(), Timestamp::from_unix_millis(0))
        .unwrap();
    assert_eq!(judgement.state, SemanticState::Blocked);
    assert!((judgement.confidence.value() - 0.91).abs() < f64::EPSILON);
    assert_eq!(judge.name(), "jev");
}

#[test]
fn judge_rejects_missing_answer() {
    let body = r#"{"model":"startlux-9b","answers":{}}"#;
    let base_url = start_fake_server(body);
    let judge = JevJudge::new(JevConfig::new(base_url));
    let result = judge.judge(&SessionSnapshot::default(), Timestamp::from_unix_millis(0));
    assert!(result.is_err());
}

#[test]
fn choice_question_serializes_criteria_as_ordered_object() {
    use spawn_judge_jev::{ChoiceCriteria, SystemOneQuestion};

    let question = SystemOneQuestion::Choice {
        instructions: "Classify the current agent session state.".to_string(),
        criteria: ChoiceCriteria(vec![
            ("WORKING".to_string(), "active".to_string()),
            ("WAITING".to_string(), "idle".to_string()),
            ("BLOCKED".to_string(), "needs permission".to_string()),
        ]),
    };
    // Assert on the wire bytes: serde_json::Value would re-sort keys, but the
    // HTTP client serializes to a string where option order must be preserved.
    let text = serde_json::to_string(&question).unwrap();
    assert!(text.contains("\"type\":\"choice\""));
    assert!(
        !text.contains("[["),
        "criteria must not be an array of pairs"
    );
    let ordered = "\"criteria\":{\"WORKING\":\"active\",\"WAITING\":\"idle\",\
                   \"BLOCKED\":\"needs permission\"}";
    assert!(text.contains(ordered), "unexpected wire body: {text}");

    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let criteria = value["criteria"]
        .as_object()
        .expect("criteria must be a JSON object");
    assert_eq!(criteria["WORKING"], "active");
}
