//! What a worker is told when a task reaches it.

use super::{REPORT, TaskRow};

/// How every task asks to be reported on.
fn report_contract() -> String {
    format!(
        "When the task is finished, or you cannot go on, write `{REPORT}` \
at the root of your workspace with three sections: `## Outcome` (done, blocked or failed, in one \
line), `## What changed` (files, commits, results) and `## Open issues`. Then stop and wait. Do not \
start other work. If a person must decide something, say so in the report."
    )
}

/// The full prompt that starts a task in a new or resumed session.
pub(crate) fn task_prompt(task: &TaskRow) -> String {
    let role = match (&task.resident, &task.charter) {
        (Some(name), Some(charter)) => format!(
            "You are `{name}`, a long-lived resident worker of Spawn. Your charter:\n{charter}\n\n\
Tasks inside your charter arrive one at a time; keep what you learn about your domain."
        ),
        _ => "You are a Spawn worker started for one task. Work only on it.".to_string(),
    };
    format!(
        "{role}\n\nMission #{}: {}\n\nTask #{}:\n{}\n\n{}",
        task.mission_id,
        task.mission_text,
        task.id,
        task.brief,
        report_contract()
    )
}

/// What a live worker gets typed for a task: a new task, or a revision.
pub(super) fn typed(task: &TaskRow) -> String {
    if task.is_revision() {
        format!(
            "Task #{} was sent back by the supervisor: {}\nFix it, then rewrite `{REPORT}`.",
            task.id,
            task.review_note.as_deref().unwrap_or("see the review")
        )
    } else {
        format!(
            "New task #{} for mission #{}: {}\n\nTask:\n{}\n\n{}",
            task.id,
            task.mission_id,
            task.mission_text,
            task.brief,
            report_contract()
        )
    }
}

/// The prompt that resumes a session for a task; a revision also carries
/// the reviewer's note.
pub(super) fn resume_prompt(task: &TaskRow) -> String {
    let prompt = task_prompt(task);
    if task.is_revision() {
        format!("{prompt}\n\n{}", typed(task))
    } else {
        prompt
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_report_contract_names_the_report_file() {
        assert_eq!(
            super::report_contract(),
            "When the task is finished, or you cannot go on, write `.spawn/report.md` \
at the root of your workspace with three sections: `## Outcome` (done, blocked or failed, in one \
line), `## What changed` (files, commits, results) and `## Open issues`. Then stop and wait. Do not \
start other work. If a person must decide something, say so in the report."
        );
    }
}
