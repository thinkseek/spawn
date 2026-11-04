//! Mission-layer news turned into digest items for the supervisor.

use serde_json::Value;

use super::News;
use crate::dispatch::{Escalation, Why};

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn whole(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

/// `text` on one line, cut to `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    }
}

/// Digest items from the mission layer.
pub(crate) fn news_items(news: &News) -> Vec<Escalation> {
    let mut items = Vec::new();
    for mission in &news.missions {
        let id = whole(mission, "id");
        items.push(Escalation {
            session_id: "missions".to_string(),
            id: format!("mission-{id}"),
            why: Why::Mission,
            line: format!(
                "#{id} from {}: {}",
                field(mission, "origin"),
                one_line(field(mission, "text"), 300)
            ),
        });
    }
    for report in &news.reports {
        let id = whole(report, "id");
        items.push(Escalation {
            session_id: field(report, "session_id").to_string(),
            id: format!("report-{id}"),
            why: Why::Report,
            line: format!(
                "task #{id} ({}, mission #{}): {}",
                field(report, "kind"),
                whole(report, "mission_id"),
                one_line(field(report, "report"), 400)
            ),
        });
    }
    for resident in &news.approved {
        let name = field(resident, "name");
        items.push(Escalation {
            session_id: format!("session_resident_{name}"),
            id: format!("approved-{name}"),
            why: Why::Approved,
            line: format!("a human approved resident `{name}`; assign it tasks with --kind resident --worker {name}"),
        });
    }
    for message in &news.human {
        items.push(Escalation {
            session_id: field(message, "session_id").to_string(),
            id: format!("human-{}", whole(message, "command_id")),
            why: Why::Human,
            line: format!("a human told it: {}", one_line(field(message, "text"), 200)),
        });
    }
    items
}
