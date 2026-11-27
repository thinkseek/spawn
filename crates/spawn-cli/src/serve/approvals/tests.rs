//! The approval detector and the act's choice of keys, each without a
//! pane: screens written as the engines draw their prompts (Claude Code's
//! numbered permission dialog, Codex CLI's selection, OpenCode's buttons,
//! a shell's `(y/N)`, an `Enter to confirm` dialog) and the items made of
//! them.

use super::{
    ATTENTION_NEEDS_HUMAN, Act, ActRequest, Answer, Choice, ChoiceAction, KeysAct, Pane, Prompt,
    Send, action_of, detect, excerpt, fnv1a, items, what_to_send,
};
use serde_json::{Value, json};
use spawn_core::command::NamedKey;
use spawn_core::id::SessionId;

const CLAUDE: &str = "\
● Bash(rm -rf target && cargo build)
╭──────────────────────────────────────────────────────────╮
│ Bash command                                             │
│                                                          │
│   rm -rf target && cargo build                           │
│                                                          │
│ Do you want to proceed?                                  │
│ ❯ 1. Yes                                                 │
│   2. Yes, and don't ask again for rm commands in /work   │
│   3. No, and tell Claude what to do differently (esc)    │
╰──────────────────────────────────────────────────────────╯
";

const CODEX: &str = "\
Allow command?
  $ git push origin main

▌ 1. Yes, allow once
  2. Yes, always allow this command for this session
  3. No, provide feedback

Press Enter to confirm or Esc to cancel
";

const OPENCODE: &str = "\
△ Permission required
  edit src/main.rs
  Allow once   Allow always   Reject
";

fn keys(prompt: &Prompt, action: ChoiceAction) -> Vec<NamedKey> {
    prompt
        .choices
        .iter()
        .find(|choice| choice.action == action)
        .map(|choice| choice.keys.clone())
        .unwrap_or_default()
}

#[test]
fn claude_codes_numbered_permission_dialog_is_three_digit_answers() {
    let prompt = detect(CLAUDE).expect("a prompt");
    assert_eq!(prompt.kind, "permission");
    assert_eq!(prompt.summary.as_deref(), Some("Do you want to proceed?"));
    let ids: Vec<&str> = prompt
        .choices
        .iter()
        .map(|choice| choice.id.as_str())
        .collect();
    assert_eq!(ids, ["choice1", "choice2", "choice3"]);
    assert_eq!(keys(&prompt, ChoiceAction::Allow), [NamedKey::Digit1]);
    assert_eq!(keys(&prompt, ChoiceAction::AllowAlways), [NamedKey::Digit2]);
    assert_eq!(keys(&prompt, ChoiceAction::Deny), [NamedKey::Digit3]);
    let danger: Vec<bool> = prompt.choices.iter().map(|choice| choice.danger).collect();
    assert_eq!(danger, [false, true, false], "always is the risky one");
}

#[test]
fn codex_clis_selection_is_numbered_before_its_enter_hint() {
    let prompt = detect(CODEX).expect("a prompt");
    assert_eq!(prompt.kind, "permission");
    assert_eq!(prompt.summary.as_deref(), Some("Allow command?"));
    assert_eq!(keys(&prompt, ChoiceAction::Allow), [NamedKey::Digit1]);
    assert_eq!(keys(&prompt, ChoiceAction::AllowAlways), [NamedKey::Digit2]);
    assert_eq!(keys(&prompt, ChoiceAction::Deny), [NamedKey::Digit3]);
}

#[test]
fn opencodes_buttons_are_enter_right_enter_and_escape() {
    let prompt = detect(OPENCODE).expect("a prompt");
    assert_eq!(prompt.kind, "permission");
    assert_eq!(keys(&prompt, ChoiceAction::Allow), [NamedKey::Enter]);
    assert_eq!(
        keys(&prompt, ChoiceAction::AllowAlways),
        [NamedKey::Right, NamedKey::Enter]
    );
    assert_eq!(keys(&prompt, ChoiceAction::Deny), [NamedKey::Escape]);
    assert_eq!(prompt.choices[2].label, "Reject");
}

#[test]
fn a_yes_no_question_and_a_confirm_are_found_at_the_bottom_only() {
    let shell = "Removing 3 files.\nContinue? [y/N] ";
    let prompt = detect(shell).expect("a yes/no");
    assert_eq!(prompt.kind, "yes_no");
    assert_eq!(
        keys(&prompt, ChoiceAction::Allow),
        [NamedKey::Y, NamedKey::Enter]
    );
    assert_eq!(
        keys(&prompt, ChoiceAction::Deny),
        [NamedKey::N, NamedKey::Enter]
    );
    for mark in ["(y/n)", "[Y/n]", "(yes/no)", "( y / N )"] {
        assert!(detect(&format!("Go on? {mark}")).is_some(), "{mark}");
    }
    let confirm = "Select a model\n  Sonnet\n  Opus\nEnter to confirm · Esc to cancel";
    let prompt = detect(confirm).expect("a confirm");
    assert_eq!(prompt.kind, "confirm");
    assert_eq!(keys(&prompt, ChoiceAction::Allow), [NamedKey::Enter]);
    assert_eq!(keys(&prompt, ChoiceAction::Deny), [NamedKey::Escape]);
    // A question answered long ago, scrolled up past the tail's bottom, is none.
    let old = format!("Continue? [y/N] y\n{}", "working...\n".repeat(10));
    assert_eq!(detect(&old), None);
    let far = format!("{CLAUDE}{}", "output line\n".repeat(40));
    assert_eq!(detect(&far), None, "out of the tail");
}

#[test]
fn ordinary_output_and_lone_numbers_are_no_prompt() {
    for text in [
        "",
        "Compiling spawn-cli v0.1.0\n   Finished dev profile",
        "Steps:\n1. Build the crate",
        "2. second\n3. third",
        "1. one\n3. three",
        "✻ Thinking… (esc to interrupt)",
    ] {
        assert_eq!(detect(text), None, "{text:?}");
    }
}

/// Review M1: a numbered list an agent wrote in its reply, left on an idle
/// pane, is no prompt (no cursor mark, no frame, no key footer); the same
/// choices drawn as a dialog are.
#[test]
fn a_prose_list_on_an_idle_pane_is_no_permission_prompt() {
    let reply = "\
● Here is the plan. Do you want me to proceed?

  1. Yes, run the migration
  2. No, keep the old schema
  3. Always back up first

> ";
    assert_eq!(detect(reply), None, "a reply's list at an idle prompt");
    let ending = "Plan:\n1. Yes, build\n2. No tests\n3. Always lint";
    assert_eq!(
        detect(ending),
        None,
        "a list ending the pane, unframed, no footer"
    );
    // A cursor mark, or a frame, or a key footer makes it a dialog.
    let marked = "Pick one\n❯ 1. red\n  2. green";
    assert_eq!(detect(marked).expect("marked").kind, "choice");
    let framed = "╭────╮\n│ Go? │\n│ 1. Yes │\n│ 2. No │\n╰────╯";
    assert_eq!(detect(framed).expect("framed").kind, "permission");
    let footer = "Go?\n1. Yes\n2. No\nEnter to confirm · Esc to cancel";
    assert_eq!(detect(footer).expect("footer").kind, "permission");
    // Framed but with output under it: no longer at the bottom.
    let scrolled = format!("{framed}\nline\nline\nline\nline");
    assert_eq!(detect(&scrolled), None);
}

/// Review L6: an answer's action is read by whole words, never a prefix.
#[test]
fn an_answers_action_is_read_by_whole_words() {
    for (label, action, wire) in [
        ("Node 18", ChoiceAction::Keys, "keys"),
        ("Normal mode", ChoiceAction::Keys, "keys"),
        ("Okta login", ChoiceAction::Keys, "keys"),
        ("Runtime only", ChoiceAction::Keys, "keys"),
        ("Continue without saving", ChoiceAction::Keys, "keys"),
        ("Nothing", ChoiceAction::Keys, "keys"),
        ("Yesterday's build", ChoiceAction::Keys, "keys"),
        ("Yes", ChoiceAction::Allow, "allow"),
        ("Yes, proceed", ChoiceAction::Allow, "allow"),
        ("OK", ChoiceAction::Allow, "allow"),
        ("Allow once", ChoiceAction::Allow, "allow"),
        ("No", ChoiceAction::Deny, "deny"),
        (
            "No, and tell Claude what to do differently",
            ChoiceAction::Deny,
            "deny",
        ),
        ("Deny", ChoiceAction::Deny, "deny"),
        ("Cancel", ChoiceAction::Deny, "deny"),
        (
            "Yes, and don't ask again for this command",
            ChoiceAction::AllowAlways,
            "allow_always",
        ),
        (
            "Yes, always allow",
            ChoiceAction::AllowAlways,
            "allow_always",
        ),
        (
            "Allow all edits during this session",
            ChoiceAction::AllowAlways,
            "allow_always",
        ),
        ("Hallways", ChoiceAction::Keys, "keys"),
    ] {
        assert_eq!(action_of(label), action, "{label:?}");
        assert_eq!(action_of(label).as_str(), wire, "{label:?}");
    }
}

/// Review M2: a choice past 3 is its own digit key, wherever the cursor
/// stands (here on choice 3): no arrows counted from an assumed first row.
#[test]
fn a_choice_past_three_is_its_digit_wherever_the_cursor_stands() {
    let menu = "Which model?\n  1. Haiku\n  2. Sonnet\n❯ 3. Opus\n  4. Fable\n  5. Custom";
    let prompt = detect(menu).expect("a choice");
    assert_eq!(prompt.cursor.as_deref(), Some("choice3"));
    let key_of = |id: &str| {
        prompt
            .choices
            .iter()
            .find(|choice| choice.id == id)
            .map(|choice| choice.keys.clone())
            .expect(id)
    };
    assert_eq!(key_of("choice4"), [NamedKey::Digit4]);
    assert_eq!(key_of("choice5"), [NamedKey::Digit5]);
    assert_eq!(key_of("choice1"), [NamedKey::Digit1]);
    let nine = format!(
        "Pick\n{}",
        (1..=9)
            .map(|n| format!("{}{n}. option {n}", if n == 7 { "❯ " } else { "  " }))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let prompt = detect(&nine).expect("nine choices");
    assert_eq!(prompt.choices.len(), 9);
    assert_eq!(prompt.choices[8].keys, [NamedKey::Digit9]);
}

/// Review L7: an item's id is a stable hash of the prompt (its question and
/// answers), not of the pane: a spinner, a timer, output above it, or its
/// cursor moving leave it; another prompt changes it.
#[test]
fn a_prompts_id_is_stable_across_its_panes_other_lines() {
    let id = |text: &str| detect(text).expect("a prompt").id("session_worker_a");
    let base = id(CLAUDE);
    let spinner = format!("✻ Working… (12s · esc to interrupt)\n{CLAUDE}");
    assert_eq!(id(&spinner), base);
    let later = format!("✻ Working… (47s · esc to interrupt)\nmore output\n{CLAUDE}");
    assert_eq!(id(&later), base);
    let moved = CLAUDE
        .replace("❯ 1. Yes", "  1. Yes")
        .replace("  2. Yes,", "❯ 2. Yes,");
    assert_eq!(id(&moved), base, "the cursor moving is the same prompt");
    let other = CLAUDE.replace("rm commands", "git commands");
    assert_ne!(id(&other), base, "another answer is another prompt");
    assert!(base.starts_with("session_worker_a:") && base.len() == "session_worker_a:".len() + 16);
    // FNV-1a's published test vectors: the same in every build.
    assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
}

#[test]
fn an_excerpt_is_the_last_lines_masked_and_bounded() {
    let text = format!(
        "{}export TOKEN=sk-ant-api03-{}\nDo you want to proceed?\n\n\n",
        "line\n".repeat(30),
        "x".repeat(40)
    );
    let cut = excerpt(&text);
    assert_eq!(cut.lines().count(), 12);
    assert!(cut.ends_with("Do you want to proceed?"), "{cut}");
    assert!(!cut.contains("sk-ant-api03-xxxx"), "masked: {cut}");
    let wide = "y".repeat(5000);
    assert!(excerpt(&wide).len() <= 1500);
}

fn pane(session: &str, text: &str, judgement: Value) -> Pane {
    Pane {
        session_id: session.to_string(),
        agent_id: session.replace("session_", "agent_"),
        backend: Some("herdr"),
        text: text.to_string(),
        judgement,
    }
}

#[test]
fn the_items_are_the_prompts_the_asking_judgements_and_the_halted_missions() {
    let asking = json!({"state": "WAITING", "needs_human": ATTENTION_NEEDS_HUMAN,
                        "attention": "question", "reason": "asks which branch to use"});
    let calm = json!({"state": "WORKING", "needs_human": 0.1});
    let board = json!({"missions": [
        {"id": 7, "halt": {"event": 991, "reason": "3 gate rejections in a row", "at_ms": 5}},
        {"id": 8, "halt": null},
    ]});
    let listed = items(
        &[
            pane("session_worker_a", CLAUDE, calm.clone()),
            pane("session_worker_b", "idle\n> ", asking),
            pane("session_worker_c", "building...", calm),
        ],
        &board,
        1_000,
    );
    let kinds: Vec<&str> = listed
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    assert_eq!(kinds, ["permission", "attention", "mission_halted"]);
    let first = &listed[0];
    assert_eq!(first["session_id"], "session_worker_a");
    assert_eq!(first["agent_id"], "agent_worker_a");
    assert_eq!(first["backend"], "herdr");
    assert_eq!(first["detected_at_ms"], 1_000);
    assert_eq!(first["options"][1]["keys"], json!(["digit2"]));
    assert_eq!(first["options"][1]["danger"], true);
    assert_eq!(first["options"][0]["text"], Value::Null);
    assert!(
        first["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("session_worker_a:"))
    );
    // The same screen is the same item id; another is another.
    let again = items(
        &[pane("session_worker_a", CLAUDE, Value::Null)],
        &json!({}),
        2_000,
    );
    assert_eq!(again[0]["id"], first["id"]);
    assert_eq!(listed[1]["label"], "asks which branch to use");
    assert_eq!(listed[1]["options"], json!([]));
    let halted = &listed[2];
    assert_eq!(halted["resume"]["endpoint"], "/api/missions/7/resume");
    assert_eq!(halted["resume"]["halt"], 991);
    // Its option has the documented fields, and only them.
    let mut fields: Vec<&str> = halted["options"][0]
        .as_object()
        .expect("an option")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["action", "danger", "id", "keys", "label", "text"]);
    assert_eq!(halted["detected_at_ms"], 5);
}

fn shown(prompt: Option<Prompt>) -> impl Fn(&SessionId) -> Result<Option<Prompt>, String> {
    move |_| Ok(prompt.clone())
}

fn answer_id(approval_id: &str, choice_id: Option<&str>) -> Answer {
    Answer {
        approval_id: approval_id.to_string(),
        choice_id: choice_id.map(str::to_string),
    }
}

fn keys_for_choice(
    approval_id: &str,
    choice_id: &str,
    expected_keys: Option<Vec<NamedKey>>,
) -> Act {
    Act::Keys(KeysAct::ForChoice {
        approval_id: approval_id.to_string(),
        choice_id: choice_id.to_string(),
        expected_keys,
    })
}

/// A dialog whose three answers are all "Yes" (all `allow`), drawn as
/// Claude Code draws its numbered dialog.
const THREE_YES: &str = "\
╭──────────────────────────────────────────────────────────╮
│ Bash command                                             │
│                                                          │
│   cargo publish                                          │
│                                                          │
│ Do you want to proceed?                                  │
│ ❯ 1. Yes                                                 │
│   2. Yes                                                 │
│   3. Yes                                                 │
╰──────────────────────────────────────────────────────────╯
";

/// Review B2: the option a button stood for is the one pressed. Three
/// "Yes" answers share `allow`: option 3's click presses 3, never 1; an
/// `allow` that names no option is ambiguous (409, nothing pressed); an
/// option of another action, or none of that id, is 409 too.
#[test]
fn the_option_clicked_is_pressed_and_a_shared_action_needs_its_choice() {
    let session = SessionId::new("session_worker_a").expect("session");
    let prompt = detect(THREE_YES).expect("a prompt");
    let actions: Vec<ChoiceAction> = prompt.choices.iter().map(|choice| choice.action).collect();
    assert_eq!(
        actions,
        [
            ChoiceAction::Allow,
            ChoiceAction::Allow,
            ChoiceAction::Allow
        ]
    );
    let id = prompt.id(session.as_str());
    let now = shown(Some(prompt));
    assert_eq!(
        what_to_send(Act::Allow(answer_id(&id, Some("choice3"))), &session, &now),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
    assert_eq!(
        what_to_send(Act::Allow(answer_id(&id, Some("choice2"))), &session, &now),
        Ok(Send::Keys(vec![NamedKey::Digit2]))
    );
    let ambiguous =
        what_to_send(Act::Allow(answer_id(&id, None)), &session, &now).expect_err("ambiguous");
    assert_eq!(ambiguous.0, 409, "{ambiguous:?}");
    assert!(ambiguous.1.contains("choice_id"), "{ambiguous:?}");
    for (act, why) in [
        (Act::Deny(answer_id(&id, Some("choice3"))), "another action"),
        (
            Act::Allow(answer_id(&id, Some("choice9"))),
            "no such option",
        ),
    ] {
        let refused = what_to_send(act, &session, &now).expect_err(why);
        assert_eq!(refused.0, 409, "{why}: {refused:?}");
    }
    let claude = shown(detect(CLAUDE));
    let claude_id = detect(CLAUDE).expect("a prompt").id(session.as_str());
    assert_eq!(
        what_to_send(
            Act::Deny(answer_id(&claude_id, Some("choice3"))),
            &session,
            &claude
        ),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
    assert_eq!(
        what_to_send(
            Act::Allow(answer_id(&claude_id, Some("choice3"))),
            &session,
            &claude
        )
        .expect_err("choice3 is a deny")
        .0,
        409
    );
}

/// Review B2: `keys` with the approval they answer are pressed only while
/// that prompt shows; with its `choice_id` they are that option's keys.
/// Bare keys read no pane, as before.
#[test]
fn keys_for_an_approval_are_pressed_only_while_it_shows() {
    let session = SessionId::new("session_worker_a").expect("session");
    let prompt = detect(THREE_YES).expect("a prompt");
    let id = prompt.id(session.as_str());
    let now = shown(Some(prompt));
    let stale = shown(detect(CLAUDE));
    let nothing = shown(None);
    for (act, pane, why) in [
        (
            Act::Keys(KeysAct::ForApproval {
                approval_id: id.clone(),
                keys: vec![NamedKey::Digit3],
            }),
            &stale,
            "stale approval",
        ),
        (
            keys_for_choice(&id, "choice3", None),
            &stale,
            "stale with its choice",
        ),
        (
            Act::Keys(KeysAct::ForApproval {
                approval_id: id.clone(),
                keys: vec![NamedKey::Digit3],
            }),
            &nothing,
            "no prompt",
        ),
        (
            keys_for_choice(&id, "choice7", None),
            &now,
            "no such option",
        ),
        (
            keys_for_choice(&id, "choice3", Some(vec![NamedKey::Digit1])),
            &now,
            "another option's keys",
        ),
    ] {
        let refused = what_to_send(act, &session, pane).expect_err(why);
        assert_eq!(refused.0, 409, "{why}: {refused:?}");
    }
    assert_eq!(
        what_to_send(keys_for_choice(&id, "choice3", None), &session, &now),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
    assert_eq!(
        what_to_send(
            keys_for_choice(&id, "choice3", Some(vec![NamedKey::Digit3])),
            &session,
            &now
        ),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
    assert_eq!(
        what_to_send(
            Act::Keys(KeysAct::ForApproval {
                approval_id: id,
                keys: vec![NamedKey::Enter],
            }),
            &session,
            &now
        ),
        Ok(Send::Keys(vec![NamedKey::Enter]))
    );
    let refused = ActRequest::parse(br#"{"action":"keys","choice_id":"choice3"}"#)
        .expect_err("choice without approval");
    assert_eq!(refused.0, 400, "{refused:?}");
    assert_eq!(
        what_to_send(
            Act::Keys(KeysAct::Bare {
                keys: vec![NamedKey::Digit3],
            }),
            &session,
            &nothing
        ),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
}

#[test]
fn an_answer_presses_the_keys_of_the_prompt_showing_now() {
    let session = SessionId::new("session_worker_a").expect("session");
    let claude = shown(detect(CLAUDE));
    let id = detect(CLAUDE).expect("a prompt").id(session.as_str());
    let send = |act: Act, prompt: &dyn Fn(&SessionId) -> Result<Option<Prompt>, String>| {
        what_to_send(act, &session, prompt)
    };
    assert_eq!(
        send(Act::Allow(answer_id(&id, None)), &claude),
        Ok(Send::Keys(vec![NamedKey::Digit1]))
    );
    assert_eq!(
        send(Act::AllowAlways(answer_id(&id, None)), &claude),
        Ok(Send::Keys(vec![NamedKey::Digit2]))
    );
    assert_eq!(
        send(Act::Deny(answer_id(&id, None)), &claude),
        Ok(Send::Keys(vec![NamedKey::Digit3]))
    );
    let refused = ActRequest::parse(br#"{"action":"allow"}"#).expect_err("no id");
    assert_eq!(refused.0, 400);
    assert!(refused.1.contains("approval_id"), "{refused:?}");
    let nothing = shown(None);
    let refused = send(Act::Allow(answer_id(&id, None)), &nothing).expect_err("no prompt");
    assert_eq!(refused.0, 409);
    assert!(refused.1.contains("no approval prompt"), "{refused:?}");
    let only_yes = shown(Some(Prompt {
        kind: "confirm",
        label: "x".to_string(),
        summary: None,
        choices: vec![Choice {
            id: "confirm".to_string(),
            label: "Confirm".to_string(),
            keys: vec![NamedKey::Enter],
            danger: false,
            action: ChoiceAction::Allow,
        }],
        cursor: None,
        body: Vec::new(),
    }));
    let confirm_id = only_yes(&session)
        .expect("read")
        .expect("shown")
        .id(session.as_str());
    assert_eq!(
        send(Act::AllowAlways(answer_id(&confirm_id, None)), &only_yes)
            .expect_err("none")
            .0,
        409
    );
    assert_eq!(
        send(
            Act::Keys(KeysAct::Bare {
                keys: vec![NamedKey::Digit5, NamedKey::Enter],
            }),
            &nothing
        ),
        Ok(Send::Keys(vec![NamedKey::Digit5, NamedKey::Enter]))
    );
    assert_eq!(
        send(
            Act::Text {
                text: "use the main branch".to_string(),
            },
            &nothing
        ),
        Ok(Send::Text("use the main branch".to_string()))
    );
    for (text, why) in [("   ", "a blank text"), ("a\u{1b}[2J", "an escape")] {
        let refused = send(
            Act::Text {
                text: text.to_string(),
            },
            &claude,
        )
        .expect_err(why);
        assert_eq!(refused.0, 400, "{why}: {refused:?}");
    }
}

/// Review H2: the page showed one prompt, and by the click another shows
/// (the first answered, a new one asked): the click presses nothing, 409,
/// whatever the new prompt's answers are.
#[test]
fn a_stale_click_on_a_replaced_prompt_presses_nothing() {
    let session = SessionId::new("session_worker_a").expect("session");
    let shown_then = detect(CLAUDE).expect("a prompt").id(session.as_str());
    let replaced = CLAUDE
        .replace(
            "rm -rf target && cargo build",
            "git push --force origin main",
        )
        .replace("rm commands", "git commands");
    let now = shown(detect(&replaced));
    for act in [
        Act::Allow(answer_id(&shown_then, None)),
        Act::AllowAlways(answer_id(&shown_then, None)),
        Act::Deny(answer_id(&shown_then, None)),
    ] {
        let label = act.action_name();
        let refused = what_to_send(act, &session, &now).expect_err("stale");
        assert_eq!(refused.0, 409, "{label}: {refused:?}");
        assert!(refused.1.contains("not approval"), "{refused:?}");
    }
    let fresh = detect(&replaced).expect("a prompt").id(session.as_str());
    assert_eq!(
        what_to_send(Act::Allow(answer_id(&fresh, None)), &session, &now),
        Ok(Send::Keys(vec![NamedKey::Digit1]))
    );
}

/// Claude Code's newer dialog: no box, a rule on top, the command between
/// its heading and its question.
fn claude_ruled(command: &str) -> String {
    format!(
        "\
● I'll clean the build first.
────────────────────────────────────────────────────────────
 Bash command

   {command}
   Remove the build output

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for rm commands in /work
   3. No, and tell Claude what to do differently (esc)

 Esc to cancel · Tab to amend
"
    )
}

/// Claude Code's boxed dialog over `command`.
fn claude_boxed(command: &str) -> String {
    format!(
        "\
╭──────────────────────────────────────────────────────────╮
│ Bash command                                             │
│                                                          │
│   {command:<54} │
│                                                          │
│ Do you want to proceed?                                  │
│ ❯ 1. Yes                                                 │
│   2. Yes, and don't ask again for this command           │
│   3. No, and tell Claude what to do differently (esc)    │
╰──────────────────────────────────────────────────────────╯
"
    )
}

/// Codex CLI's approval over `command`: the question, then the command.
fn codex(command: &str) -> String {
    format!(
        "\
• Ran cargo test
  └ test result: ok. 12 passed

Would you like to run the following command?

  $ {command}

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for this command (a)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
"
    )
}

/// OpenCode's permission over `what`, its bar on the left.
fn opencode(what: &str) -> String {
    format!(
        "\
┃  △ Permission required
┃  {what}
┃
┃  Allow once   Allow always   Reject
"
    )
}

/// Each engine's dialog, twice: the same question and answers over two
/// commands.
fn command_pairs() -> Vec<(&'static str, String, String)> {
    vec![
        (
            "claude ruled",
            claude_ruled("rm -rf target"),
            claude_ruled("rm -rf /home/ckl/projects"),
        ),
        (
            "claude ruled build/push",
            claude_ruled("cargo build"),
            claude_ruled("git push --force"),
        ),
        (
            "claude boxed",
            claude_boxed("rm -rf target"),
            claude_boxed("rm -rf /home/ckl/projects"),
        ),
        ("codex", codex("cargo build"), codex("git push --force")),
        (
            "codex rm",
            codex("rm -rf target"),
            codex("rm -rf /home/ckl/projects"),
        ),
        // Review rev 2: case is the whole difference here.
        (
            "claude ruled case",
            claude_ruled("git branch -D main"),
            claude_ruled("git branch -d main"),
        ),
        (
            "claude boxed case",
            claude_boxed("git branch -D main"),
            claude_boxed("git branch -d main"),
        ),
        (
            "codex case",
            codex("git branch -D main"),
            codex("git branch -d main"),
        ),
        (
            "opencode case",
            opencode("bash git branch -D main"),
            opencode("bash git branch -d main"),
        ),
        (
            "codex short",
            CODEX.to_string(),
            CODEX.replace("git push origin main", "git push --force origin main"),
        ),
        (
            "opencode",
            opencode("bash rm -rf target"),
            opencode("bash rm -rf /home/ckl/projects"),
        ),
        (
            "opencode short",
            OPENCODE.to_string(),
            OPENCODE.replace("edit src/main.rs", "bash git push --force"),
        ),
    ]
}

/// Review H2: the same question with the same answers over another command
/// is another prompt: another id, and a click the page made for the first
/// presses nothing on the second (409), for every engine's dialog.
#[test]
fn the_same_question_over_another_command_is_another_prompt() {
    let session = SessionId::new("session_worker_a").expect("session");
    let pairs = command_pairs();
    for (engine, seen, now) in &pairs {
        let (then, later) = (
            detect(seen).unwrap_or_else(|| panic!("{engine}: a prompt")),
            detect(now).unwrap_or_else(|| panic!("{engine}: a prompt")),
        );
        assert_eq!(then.summary, later.summary, "{engine}: the same question");
        let labels = |prompt: &Prompt| -> Vec<String> {
            prompt
                .choices
                .iter()
                .map(|choice| choice.label.clone())
                .collect()
        };
        assert_eq!(labels(&then), labels(&later), "{engine}: the same answers");
        let shown_then = then.id(session.as_str());
        assert_ne!(shown_then, later.id(session.as_str()), "{engine}");
        let showing = shown(Some(later));
        for act in [
            Act::Allow(answer_id(&shown_then, None)),
            Act::AllowAlways(answer_id(&shown_then, None)),
            Act::Deny(answer_id(&shown_then, None)),
        ] {
            let label = act.action_name();
            let refused = what_to_send(act, &session, &showing).expect_err("stale");
            assert_eq!(refused.0, 409, "{engine} {label}: {refused:?}");
        }
    }
    // The command is in the id; what is above the dialog is not.
    let id = |text: &str| detect(text).expect("a prompt").id("session_worker_a");
    let ruled = claude_ruled("rm -rf target");
    assert_eq!(
        id(&format!("older output\n✻ Baked for 2m 3s\n{ruled}")),
        id(&ruled),
        "output above the rule"
    );
    let asked = codex("cargo build");
    assert_eq!(
        id(&format!("✻ Working… (9s · esc to interrupt)\n{asked}")),
        id(&format!("✻ Working… (31s · esc to interrupt)\n{asked}")),
        "a timer above"
    );
    assert_eq!(
        detect(&claude_boxed("rm -rf target")).expect("boxed").body,
        ["Bash command", "rm -rf target", "Do you want to proceed?"]
    );
}

/// Review M1: five screens that are no prompt (PA1b to PA1f): a dialog is
/// the last thing on its pane, `●` and `>` begin lines agents write, and a
/// working spinner's `esc to interrupt` is no dialog's key footer.
#[test]
fn lists_agents_write_are_no_prompt_wherever_they_stand() {
    let screens = [
        // PA1b: a prose list above a working spinner.
        "Plan:\n1. Read the code\n2. Fix the bug\n3. Run the tests\n\n✻ Working… (12s · esc to interrupt)",
        // PA1c: a working pane: the list, its spinner and the input box's rules.
        "1. Read the code\n2. Fix the bug\n✻ Working… (3s · esc to interrupt)\n────────────\n> \n────────────\n  ⏵⏵ accept edits on (shift+tab to cycle)",
        // PA1c, the list right above the idle input box's rules.
        "Next steps:\n1. Allow the migration\n2. Deny the old path\n────────────\n> \n────────────",
        // PA1d: an idle reply beginning `● 1. ...`.
        "● 1. Yes, build the crate\n  2. No tests yet\n  3. Always lint first\n\n> ",
        // PA1e: an echoed message `> 1. ...` with work below it.
        "> 1. fix the bug\n  2. add a test\n\n● I'll start with the bug.\n  Read(src/main.rs)\n✻ Working… (esc to interrupt)",
        // PA1f: an idle Markdown quote `> 1. ...`.
        "● The spec says:\n  > 1. Allow once\n  > 2. Deny\n\n> ",
    ];
    for screen in screens {
        assert_eq!(detect(screen), None, "{screen}");
    }
    // A strong cursor mark away from the bottom is no dialog either.
    let scrolled = "Pick one\n❯ 1. red\n  2. green\nline\nline\nline\nline";
    assert_eq!(detect(scrolled), None);
    // `>` as a cursor still marks a dialog drawn as one (a frame, a footer).
    let boxed = "╭────╮\n│ Go? │\n│ > 1. Yes │\n│   2. No │\n╰────╯";
    assert_eq!(
        detect(boxed).expect("boxed").cursor.as_deref(),
        Some("choice1")
    );
    let footed = "Go?\n> 1. Yes\n  2. No\nEnter to confirm · Esc to cancel";
    assert!(detect(footed).is_some());
}

/// Review rev 2 (H2, case): `git branch -D main` (force) and `git branch
/// -d main` are two prompts: a click the page made for the safe one presses
/// nothing (no `digit1`) on the forced one.
#[test]
fn a_commands_case_is_part_of_its_prompt() {
    let session = SessionId::new("session_worker_a").expect("session");
    let safe = detect(&claude_ruled("git branch -d main")).expect("a prompt");
    let forced = detect(&claude_ruled("git branch -D main")).expect("a prompt");
    let shown_then = safe.id(session.as_str());
    assert_ne!(shown_then, forced.id(session.as_str()));
    let now = shown(Some(forced));
    let refused =
        what_to_send(Act::Allow(answer_id(&shown_then, None)), &session, &now).expect_err("stale");
    assert_eq!(refused.0, 409, "{refused:?}");
}

#[test]
fn wire_contract_valid_payloads_parse_and_resolve() {
    let session = SessionId::new("session_worker_a").expect("session");
    let claude = detect(CLAUDE).expect("a prompt");
    let id = claude.id(session.as_str());
    let pane = shown(Some(claude));
    assert_eq!(
        ActRequest::parse(br#"{"probe":true}"#),
        Ok(ActRequest::Probe)
    );
    assert_eq!(
        ActRequest::parse(
            br#"{"probe":true,"action":null,"approval_id":null,"choice_id":null,"keys":null,"text":null,"key":null}"#
        ),
        Ok(ActRequest::Probe)
    );
    let cases = [
        (
            format!(r#"{{"action":"allow","approval_id":"{id}","key":"k1"}}"#),
            Some("k1"),
            "allow",
            Send::Keys(vec![NamedKey::Digit1]),
        ),
        (
            format!(r#"{{"action":"allow","approval_id":"{id}","choice_id":null,"key":null}}"#),
            None,
            "allow",
            Send::Keys(vec![NamedKey::Digit1]),
        ),
        (
            format!(r#"{{"action":"allow","approval_id":"{id}","choice_id":"choice1"}}"#),
            None,
            "allow",
            Send::Keys(vec![NamedKey::Digit1]),
        ),
        (
            format!(r#"{{"action":"allow_always","approval_id":"{id}","key":"k2"}}"#),
            Some("k2"),
            "allow_always",
            Send::Keys(vec![NamedKey::Digit2]),
        ),
        (
            format!(r#"{{"action":"deny","approval_id":"{id}","key":"k3"}}"#),
            Some("k3"),
            "deny",
            Send::Keys(vec![NamedKey::Digit3]),
        ),
        (
            r#"{"probe":false,"action":"keys","keys":["5","enter"],"key":"k4"}"#.to_string(),
            Some("k4"),
            "keys",
            Send::Keys(vec![NamedKey::Digit5, NamedKey::Enter]),
        ),
        (
            format!(r#"{{"action":"keys","approval_id":"{id}","keys":["enter"],"key":"k5"}}"#),
            Some("k5"),
            "keys",
            Send::Keys(vec![NamedKey::Enter]),
        ),
        (
            format!(r#"{{"action":"keys","approval_id":"{id}","choice_id":"choice3"}}"#),
            None,
            "keys",
            Send::Keys(vec![NamedKey::Digit3]),
        ),
        (
            format!(
                r#"{{"action":"keys","approval_id":"{id}","choice_id":"choice3","keys":["digit3"]}}"#
            ),
            None,
            "keys",
            Send::Keys(vec![NamedKey::Digit3]),
        ),
        (
            r#"{"action":"text","text":"use main","key":"k6"}"#.to_string(),
            Some("k6"),
            "text",
            Send::Text("use main".to_string()),
        ),
    ];
    for (raw, expected_key, expected_action, expected_send) in cases {
        let parsed = ActRequest::parse(raw.as_bytes()).expect(&raw);
        let ActRequest::Perform { key, act } = parsed else {
            panic!("expected Perform for {raw}");
        };
        assert_eq!(key.as_deref(), expected_key, "{raw}");
        assert_eq!(act.action_name(), expected_action, "{raw}");
        assert_eq!(
            what_to_send(act, &session, &pane),
            Ok(expected_send),
            "{raw}"
        );
    }
}

#[test]
fn wire_contract_rejects_cross_variant_and_unknown_fields_with_400() {
    for (raw, why) in [
        (
            r#"{"action":"allow","approval_id":"a:1","keys":["y"]}"#,
            "allow + keys",
        ),
        (
            r#"{"action":"allow","approval_id":"a:1","keys":null}"#,
            "allow + null keys",
        ),
        (
            r#"{"action":"allow","approval_id":"a:1","text":"hi"}"#,
            "allow + text",
        ),
        (
            r#"{"action":"allow_always","approval_id":"a:1","keys":["y"]}"#,
            "allow_always + keys",
        ),
        (
            r#"{"action":"allow_always","approval_id":"a:1","text":"hi"}"#,
            "allow_always + text",
        ),
        (
            r#"{"action":"deny","approval_id":"a:1","keys":["n"]}"#,
            "deny + keys",
        ),
        (
            r#"{"action":"deny","approval_id":"a:1","text":"no"}"#,
            "deny + text",
        ),
        (
            r#"{"action":"keys","keys":["y"],"text":"hi"}"#,
            "keys + text",
        ),
        (
            r#"{"action":"keys","approval_id":"a:1","choice_id":"c1","text":"hi"}"#,
            "keys choice + text",
        ),
        (
            r#"{"action":"text","text":"hi","keys":["y"]}"#,
            "text + keys",
        ),
        (
            r#"{"action":"text","text":"hi","choice_id":"c1"}"#,
            "text + choice_id",
        ),
        (
            r#"{"action":"text","text":"hi","approval_id":"a:1"}"#,
            "text + approval_id",
        ),
        (
            r#"{"action":"allow","approval_id":"a:1","extra":1}"#,
            "allow + unknown field",
        ),
        (
            r#"{"action":"keys","keys":["y"],"extra":1}"#,
            "keys + unknown field",
        ),
        (
            r#"{"action":"text","text":"hi","extra":1}"#,
            "text + unknown field",
        ),
        (r#"{"probe":true,"extra":1}"#, "probe + unknown field"),
        (
            r#"{"probe":true,"extra":null}"#,
            "probe + null unknown field",
        ),
        (
            r#"{"action":"approve","approval_id":"a:1"}"#,
            "unknown action",
        ),
        (r#"{"action":123,"approval_id":"a:1"}"#, "non-string action"),
    ] {
        let err = ActRequest::parse(raw.as_bytes()).expect_err(why);
        assert_eq!(err.0, 400, "{why}: {err:?}");
    }
}

#[test]
fn wire_contract_rejects_missing_duplicate_probe_and_bad_values_with_400() {
    for (raw, why) in [
        (r"{}", "empty object"),
        (r#"{"probe":false}"#, "probe false without action"),
        (r#"{"action":null,"keys":["y"]}"#, "null action"),
        (r#"{"action":"allow"}"#, "allow missing approval_id"),
        (
            r#"{"action":"allow","approval_id":null}"#,
            "allow null approval_id",
        ),
        (
            r#"{"action":"allow_always"}"#,
            "allow_always missing approval_id",
        ),
        (r#"{"action":"deny"}"#, "deny missing approval_id"),
        (r#"{"action":"keys"}"#, "keys with nothing"),
        (
            r#"{"action":"keys","approval_id":"a:1"}"#,
            "keys with only approval_id",
        ),
        (
            r#"{"action":"keys","choice_id":"c3"}"#,
            "keys choice without approval_id",
        ),
        (
            r#"{"action":"keys","choice_id":"c3","keys":["digit3"]}"#,
            "keys choice+keys without approval_id",
        ),
        (r#"{"action":"keys","keys":[]}"#, "empty keys"),
        (
            r#"{"action":"keys","keys":["y","y","y","y","y","y","y","y","y"]}"#,
            "9 keys > max",
        ),
        (r#"{"action":"keys","keys":["f1"]}"#, "unknown key name"),
        (
            r#"{"action":"keys","approval_id":"a:1","choice_id":"c3","keys":[]}"#,
            "empty expected keys",
        ),
        (
            r#"{"action":"keys","approval_id":"a:1","choice_id":"c3","keys":["f1"]}"#,
            "unknown expected key",
        ),
        (r#"{"action":"text"}"#, "text missing text"),
        (r#"{"action":"text","text":null}"#, "text null"),
        (r#"{"action":"keys","action":"text"}"#, "duplicate action"),
        (r#"{"probe":true,"probe":false}"#, "duplicate probe"),
        (
            r#"{"action":"keys","keys":["y"],"key":"a","key":"b"}"#,
            "duplicate key",
        ),
        (
            r#"{"action":"allow","approval_id":"a:1","approval_id":"a:2"}"#,
            "duplicate approval_id",
        ),
        (r"[]", "json array"),
        (r"null", "json null"),
        (r"{bad", "malformed json"),
        (r#"{"probe":"yes"}"#, "wrong type for probe"),
        (
            r#"{"probe":null,"action":"keys","keys":["y"]}"#,
            "null probe",
        ),
        (
            r#"{"action":"keys","keys":["y"],"key":123}"#,
            "wrong type for key",
        ),
        (r#"{"probe":true,"action":"allow"}"#, "probe + action"),
        (
            r#"{"probe":true,"approval_id":"a:1"}"#,
            "probe + approval_id",
        ),
        (r#"{"probe":true,"choice_id":"c1"}"#, "probe + choice_id"),
        (r#"{"probe":true,"keys":["y"]}"#, "probe + keys"),
        (r#"{"probe":true,"text":"hi"}"#, "probe + text"),
        (r#"{"probe":true,"key":"k"}"#, "probe + key"),
    ] {
        let err = ActRequest::parse(raw.as_bytes()).expect_err(why);
        assert_eq!(err.0, 400, "{why}: {err:?}");
        if why == "allow missing approval_id" {
            assert!(err.1.contains("approval_id"), "{err:?}");
        }
    }
}

#[test]
fn wire_contract_conflicts_return_409() {
    let session = SessionId::new("session_worker_a").expect("session");
    let three_yes = detect(THREE_YES).expect("prompt");
    let id = three_yes.id(session.as_str());
    let pane = shown(Some(three_yes));
    let nothing = shown(None);
    for (raw, use_pane, why) in [
        (
            format!(r#"{{"action":"allow","approval_id":"{id}"}}"#),
            true,
            "ambiguous allow",
        ),
        (
            format!(r#"{{"action":"deny","approval_id":"{id}","choice_id":"choice3"}}"#),
            true,
            "wrong action for choice",
        ),
        (
            format!(r#"{{"action":"allow","approval_id":"{id}","choice_id":"choice9"}}"#),
            true,
            "missing choice",
        ),
        (
            r#"{"action":"allow","approval_id":"session_worker_a:0000000000000000"}"#.to_string(),
            true,
            "stale allow",
        ),
        (
            format!(
                r#"{{"action":"keys","approval_id":"{id}","choice_id":"choice3","keys":["digit1"]}}"#
            ),
            true,
            "mismatched expected keys",
        ),
        (
            format!(r#"{{"action":"keys","approval_id":"{id}","keys":["digit1"]}}"#),
            false,
            "no prompt showing",
        ),
    ] {
        let ActRequest::Perform { act, .. } = ActRequest::parse(raw.as_bytes()).expect(&raw) else {
            panic!("expected Perform");
        };
        let target = if use_pane { &pane } else { &nothing };
        let err = what_to_send(act, &session, target).expect_err(why);
        assert_eq!(err.0, 409, "{why}: {err:?}");
    }
}
