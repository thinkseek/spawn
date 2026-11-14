//! The notice fold (objective 151): a task's or mission's notice is
//! superseded by its subject's next one.
//!
//! The alert step records a `task_state` notice for each record that moves a
//! task (key `task_state:<task>:<event>`) and a `mission_state` notice for
//! each that moves a mission (`mission_state:<mission>:<event>`). They are
//! news, never cleared, so a window holds every earlier state of a task. At
//! read time, each such notice that a later one of the same kind and subject
//! follows (later by its event, compared as a number) gets `superseded_by`,
//! the key of the next one. Every row stays, in its order; nothing is
//! recorded, and the store's rows are unchanged.

use serde_json::Value;

/// The kinds folded: a task's and a mission's state notices.
const FOLDED: [&str; 2] = ["task_state", "mission_state"];

/// A folded notice's kind, subject id and event, from its row; `None` for
/// any other row (open, cleared, another kind, a key that does not parse).
fn folded(row: &Value) -> Option<(&str, &str, i64)> {
    if row.get("state").and_then(Value::as_str) != Some("notice") {
        return None;
    }
    let kind = row.get("kind").and_then(Value::as_str)?;
    if !FOLDED.contains(&kind) {
        return None;
    }
    let key = row.get("key").and_then(Value::as_str)?;
    let mut parts = key.split(':');
    let (Some(key_kind), Some(subject), Some(event), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if key_kind != kind || subject.is_empty() {
        return None;
    }
    let event = event.parse::<i64>().ok()?;
    Some((kind, subject, event))
}

/// Adds `superseded_by` to every notice of `rows` (an array of alert rows,
/// as `crate::alerts::window` answers) that a later notice of its subject
/// follows: the key of the next one by event. Anything else is left as it
/// is.
pub(super) fn fold(rows: &mut Value) {
    let Some(array) = rows.as_array_mut() else {
        return;
    };
    // (kind, subject, event, index), sorted, so each notice's next one of
    // its subject is the entry after it.
    let mut notices: Vec<(String, String, i64, usize)> = array
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            folded(row)
                .map(|(kind, subject, event)| (kind.to_string(), subject.to_string(), event, index))
        })
        .collect();
    notices.sort();
    for pair in notices.windows(2) {
        let (earlier, next) = (&pair[0], &pair[1]);
        if earlier.0 == next.0 && earlier.1 == next.1 && earlier.2 < next.2 {
            let key = array[next.3].get("key").cloned().unwrap_or(Value::Null);
            if let Some(row) = array[earlier.3].as_object_mut() {
                row.insert("superseded_by".to_string(), key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fold;
    use serde_json::{Value, json};

    fn row(key: &str, kind: &str, state: &str) -> Value {
        json!({"key": key, "kind": kind, "subject": "s", "state": state})
    }

    fn superseded(rows: &Value) -> Vec<(String, Option<String>)> {
        rows.as_array()
            .expect("an array")
            .iter()
            .map(|row| {
                (
                    row["key"].as_str().expect("a key").to_string(),
                    row.get("superseded_by")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                )
            })
            .collect()
    }

    #[test]
    fn a_notice_is_superseded_by_its_subjects_next_one() {
        let mut rows = json!([
            row("task_state:7:101", "task_state", "notice"),
            row("memory_low:1", "memory_low", "open"),
            row("task_state:7:12", "task_state", "notice"),
            row("mission_state:4:20", "mission_state", "notice"),
            row("task_state:8:15", "task_state", "notice"),
            row("task_state:7:9", "task_state", "notice"),
            row("task_state:7:30", "task_state", "cleared"),
            row("mission_state:4:21", "mission_state", "notice"),
            row("agent_stuck:7:5", "agent_stuck", "notice"),
            row("task_state:7:x", "task_state", "notice"),
            row("task_state:7", "task_state", "notice"),
            row("mission_state:7:200", "mission_state", "notice"),
        ]);
        let before: Vec<String> = superseded(&rows).into_iter().map(|(key, _)| key).collect();
        fold(&mut rows);
        let some = |key: &str| Some(key.to_string());
        assert_eq!(
            superseded(&rows),
            [
                // 12 before 101: the event is a number, not text.
                ("task_state:7:101".to_string(), None),
                ("memory_low:1".to_string(), None),
                ("task_state:7:12".to_string(), some("task_state:7:101")),
                ("mission_state:4:20".to_string(), some("mission_state:4:21")),
                ("task_state:8:15".to_string(), None),
                ("task_state:7:9".to_string(), some("task_state:7:12")),
                // A cleared task_state row is never folded, nor does it fold.
                ("task_state:7:30".to_string(), None),
                ("mission_state:4:21".to_string(), None),
                // Another kind, and keys that do not parse.
                ("agent_stuck:7:5".to_string(), None),
                ("task_state:7:x".to_string(), None),
                ("task_state:7".to_string(), None),
                // Mission 7 is not task 7.
                ("mission_state:7:200".to_string(), None),
            ]
        );
        let after: Vec<String> = superseded(&rows).into_iter().map(|(key, _)| key).collect();
        assert_eq!(before, after, "every row stays, in its order");

        let mut empty = json!([]);
        fold(&mut empty);
        assert_eq!(empty, json!([]));
    }
}
