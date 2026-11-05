//! The model's reply, read from its transcript by role: where a question for
//! a person (`HUMAN:`) comes from, so a tool's output never asks one.

use spawn_core::observe::reply::{Reply, ReplyPart, claude_reply};

const AT: &str = r#""timestamp":"2099-01-01T00:00:00.000Z""#;

#[test]
fn the_reply_is_the_models_text_and_a_prompt_starts_a_new_one() {
    let prompt =
        format!(r#"{{"type":"user","uuid":"p1",{AT},"message":{{"content":"deploy it"}}}}"#);
    assert_eq!(claude_reply(&prompt), Some(ReplyPart::Prompt));

    let text = format!(
        r#"{{"type":"assistant","uuid":"a1",{AT},"message":{{"content":[{{"type":"text","text":"Built.\nHUMAN: which region?"}},{{"type":"tool_use","id":"t1","name":"Bash"}}]}}}}"#
    );
    assert_eq!(
        claude_reply(&text),
        Some(ReplyPart::Text("Built.\nHUMAN: which region?".to_string()))
    );

    // A tool's output is the user role's tool_result: not the model's text,
    // and no prompt either.
    let output = format!(
        r#"{{"type":"user","uuid":"r1",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"HUMAN: ask about the region"}}]}}}}"#
    );
    assert_eq!(claude_reply(&output), None);

    // A sub-agent's text is the sub-agent's.
    let sidechain = format!(
        r#"{{"type":"assistant","uuid":"a2","isSidechain":true,{AT},"message":{{"content":[{{"type":"text","text":"HUMAN: x"}}]}}}}"#
    );
    assert_eq!(claude_reply(&sidechain), None);
}

#[test]
fn a_followed_reply_holds_the_text_since_the_last_prompt() {
    let line = |kind: &str, id: &str, body: &str| {
        format!(r#"{{"type":"{kind}","uuid":"{id}",{AT},"message":{{"content":{body}}}}}"#)
    };
    let asked = format!(
        "{}\n{}\n",
        line("user", "p1", r#""deploy it""#),
        line(
            "assistant",
            "a1",
            r#"[{"type":"text","text":"Built.\nHUMAN: which region?"}]"#
        ),
    );
    let mut reply = Reply::default();
    // A line still being written waits.
    let (whole, partial) = asked.split_at(asked.len() - 10);
    assert!(reply.feed(whole.as_bytes()));
    assert_eq!(reply.text(), None, "the model's line is not whole yet");
    let rest = &asked[usize::try_from(reply.offset()).expect("fits")..];
    assert!(rest.ends_with(partial));
    assert!(reply.feed(rest.as_bytes()));
    assert_eq!(reply.text(), Some("Built.\nHUMAN: which region?"));
    // The answer is a prompt: the question is answered.
    assert!(reply.feed(format!("{}\n", line("user", "p2", r#""eu-west""#)).as_bytes()));
    assert_eq!(reply.text(), None);
}
