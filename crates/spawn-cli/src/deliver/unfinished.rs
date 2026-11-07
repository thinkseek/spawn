//! Unfinished reports: a `.spawn/report.md` that is still a draft is not
//! taken as its task's report ([`super::collect_reports`]).
//!
//! A worker that writes its report as a template early, and whose turn
//! stops while its validation still runs, leaves a file the turn's end would
//! otherwise make the task's report. Before a report is taken, its text is
//! read for the marks of a draft ([`markers`]):
//!
//! - a template placeholder: `PENDING_` followed by a capital, as a word of
//!   its own (`PENDING_RESULTS`, not `ORDER_PENDING_PAYMENT`);
//! - a line that is only `(pending)`, `TBD` or `TODO` (in any case, in
//!   parentheses or brackets, as a list item, or as the value of a labelled
//!   line such as `**Results.** (pending)` or `Open issues: TBD`), which a
//!   section whose whole body is the placeholder has too;
//! - an Outcome that says the report is a draft or the work in progress:
//!   the `## Outcome` heading itself (`## Outcome (draft)`), the first line
//!   of its body, or a line labelled `Outcome:` or `Status:`, that begins
//!   with `draft`, `in progress`, `work in progress` or `WIP`, or carries
//!   `(draft)` or `(in progress)`.
//!
//! Code is quoted, not written: lines inside fenced blocks and text inside
//! backticks are not read, so a report may name a marker (`` `TODO` ``) or
//! show a file that holds one. A word in a sentence is never a marker: "the
//! pending payout" or "the draft contract is written" are taken.

use serde_json::{Value, json};

/// What makes a report's line a mark of a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A template placeholder (`PENDING_RESULTS`).
    Placeholder(String),
    /// A line, or a labelled line's value, that is only a placeholder word
    /// (`(pending)`, `TBD`, `TODO`), as written.
    Pending(String),
    /// The Outcome says the report is a draft or the work in progress.
    Draft,
}

/// One mark of a draft in a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marker {
    /// The line it is on, from 1.
    pub(crate) line: usize,
    pub(crate) kind: Kind,
    /// The line, on one line and cut short.
    pub(crate) text: String,
}

/// The most of a line a marker quotes.
const QUOTED: usize = 120;

impl Marker {
    /// What the worker is told about it: the line, and what is unfinished.
    pub(crate) fn describe(&self) -> String {
        let line = self.line;
        match &self.kind {
            Kind::Placeholder(token) => {
                format!("line {line}: the placeholder {token} («{}»)", self.text)
            }
            Kind::Pending(word) => format!("line {line}: «{}» is only {word}", self.text),
            Kind::Draft => format!(
                "line {line}: the Outcome says it is a draft or in progress («{}»)",
                self.text
            ),
        }
    }

    /// The marker in its record (`task.report_unfinished`'s `markers`).
    pub(crate) fn record(&self) -> Value {
        let (kind, marker) = match &self.kind {
            Kind::Placeholder(token) => ("placeholder", token.as_str()),
            Kind::Pending(word) => ("pending", word.as_str()),
            Kind::Draft => ("draft", "draft"),
        };
        json!({"line": self.line, "kind": kind, "marker": marker, "text": self.text})
    }
}

/// Every mark of a draft in `report`, in line order (see the module's
/// documentation); none for a finished report.
pub(crate) fn markers(report: &str) -> Vec<Marker> {
    let mut found = Vec::new();
    let mut fence: Option<String> = None;
    // Inside an Outcome section whose first body line is not read yet.
    let mut outcome_first_line = false;
    for (index, raw) in report.lines().enumerate() {
        let line = index + 1;
        let mut push = |kind: Kind| {
            found.push(Marker {
                line,
                kind,
                text: super::one_line(raw, QUOTED),
            });
        };
        if let Some(open) = &fence {
            if closes_fence(raw, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opens_fence(raw) {
            fence = Some(open);
            continue;
        }
        let text = without_code_spans(raw);
        for token in placeholders(&text) {
            push(Kind::Placeholder(token));
        }
        if let Some(title) = heading(&text) {
            let title = plain(title);
            outcome_first_line = false;
            if let Some(rest) = after_word(&title.to_lowercase(), "outcome") {
                outcome_first_line = true;
                if says_draft(rest) {
                    push(Kind::Draft);
                }
            }
            continue;
        }
        let body = plain(&text);
        if body.is_empty() {
            continue;
        }
        let labelled = labelled(body);
        let first_of_outcome = std::mem::take(&mut outcome_first_line);
        let labelled_outcome = labelled.is_some_and(|(label, value)| {
            matches!(label.to_lowercase().as_str(), "outcome" | "status") && says_draft(value)
        });
        if (first_of_outcome && says_draft(body)) || labelled_outcome {
            push(Kind::Draft);
        }
        if let Some(word) = only_placeholder(body)
            .or_else(|| labelled.and_then(|(_, value)| only_placeholder(value)))
        {
            push(Kind::Pending(word.to_string()));
        }
    }
    found
}

/// The fence a line opens (three or more backticks or tildes, after up to
/// three spaces), as written.
fn opens_fence(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let mark = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = trimmed.len() - trimmed.trim_start_matches(mark).len();
    (run >= 3).then(|| mark.to_string().repeat(run))
}

/// Whether `line` closes the fence `open`: at least as many of its marks,
/// and nothing else.
fn closes_fence(line: &str, open: &str) -> bool {
    let trimmed = line.trim();
    let mark = open.chars().next().unwrap_or('`');
    trimmed.len() >= open.len() && trimmed.chars().all(|c| c == mark)
}

/// `line` with every backtick code span blanked out; an unmatched backtick
/// stays as written.
fn without_code_spans(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        let from = &rest[start..];
        let run = from.len() - from.trim_start_matches('`').len();
        let (ticks, body) = from.split_at(run);
        if let Some(end) = body.find(ticks) {
            out.push(' ');
            rest = &body[end + run..];
        } else {
            out.push_str(ticks);
            rest = body;
        }
    }
    out.push_str(rest);
    out
}

/// The template placeholders on a line: `PENDING_` and a capital, not
/// inside a longer word, up to the end of its capitals, digits and
/// underscores. Each once.
fn placeholders(line: &str) -> Vec<String> {
    const PREFIX: &str = "PENDING_";
    let mut tokens: Vec<String> = Vec::new();
    for (at, _) in line.match_indices(PREFIX) {
        let before = line[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let tail = &line[at + PREFIX.len()..];
        if !tail.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let length = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let token = format!("{PREFIX}{}", &tail[..length]);
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

/// The title of an ATX heading (`## Outcome`), without its closing hashes.
fn heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    let rest = &trimmed[hashes..];
    if !(1..=6).contains(&hashes) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim_end())
}

/// A line's words without what only frames them: a quote's `>`, a list
/// item's bullet or number, a checkbox, and emphasis around it.
fn plain(line: &str) -> &str {
    let mut text = line.trim();
    loop {
        let before = text;
        text = text.trim_start_matches('>').trim_start();
        for bullet in ["- ", "* ", "+ ", "[ ] ", "[x] ", "[X] "] {
            text = text.strip_prefix(bullet).unwrap_or(text).trim_start();
        }
        let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            let rest = &text[digits..];
            if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
                text = after.trim_start();
            }
        }
        if text == before {
            break;
        }
    }
    text.trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace())
}

/// A labelled line's label and value: `**Label.** value`, `**Label:**
/// value`, `**Label**: value`, or `Label: value` with a short label of
/// words (letters, digits, spaces, `-`, `/`).
fn labelled(line: &str) -> Option<(&str, &str)> {
    let punctuation = |c: char| c == ':' || c == '.' || c.is_whitespace();
    for strong in ["**", "__"] {
        if let Some(inner) = line.strip_prefix(strong)
            && let Some(end) = inner.find(strong)
        {
            let label = inner[..end].trim_end_matches(punctuation);
            let value = inner[end + strong.len()..].trim_start_matches(punctuation);
            return (!label.is_empty()).then_some((label, value));
        }
    }
    let (label, value) = line.split_once(':')?;
    let words = label.chars().all(|c| {
        c.is_alphanumeric() || c == ' ' || c == '-' || c == '/' || c == '*' || c == '_'
    });
    let label = label.trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace());
    (words && !label.is_empty() && label.chars().count() <= 40)
        .then_some((label, value.trim_start_matches(punctuation)))
}

/// The placeholder word `text` is only, as written: `(pending)`, `TBD` or
/// `TODO`, in any case, bare or in parentheses or brackets (`pending` only
/// in them: a bare one is a state, "Payout 3: pending"), with emphasis and
/// a closing `.`, `:` or `!` around it.
fn only_placeholder(text: &str) -> Option<&str> {
    let word = text
        .trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace())
        .trim_end_matches(['.', ':', '!'])
        .trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace());
    let inner = word
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .or_else(|| word.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')))
        .map(str::trim);
    let bare = word.to_lowercase();
    let matched = match inner.map(str::to_lowercase) {
        Some(inner) => matches!(inner.as_str(), "pending" | "tbd" | "todo"),
        None => matches!(bare.as_str(), "tbd" | "todo"),
    };
    matched.then_some(word)
}

/// The rest of `text` (lowercase) after `word` at its start as a whole
/// word.
fn after_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(word)?;
    (!rest.starts_with(|c: char| c.is_alphanumeric())).then_some(rest)
}

/// Whether an Outcome's words say the report is a draft or the work in
/// progress: they begin with `draft`, `in progress`, `work in progress`,
/// `still in progress` or `WIP` (after punctuation), or carry one of them
/// in parentheses or brackets.
fn says_draft(text: &str) -> bool {
    let lower = text.to_lowercase();
    let start = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
    let begins = ["draft", "wip"]
        .iter()
        .any(|word| after_word(start, word).is_some())
        || ["in progress", "in-progress", "work in progress", "still in progress"]
            .iter()
            .any(|words| start.starts_with(words));
    let tagged = ["draft", "wip", "in progress", "in-progress", "work in progress"]
        .iter()
        .any(|words| lower.contains(&format!("({words})")) || lower.contains(&format!("[{words}]")));
    begins || tagged
}

#[cfg(test)]
mod tests {
    use super::{Kind, markers};

    fn kinds(report: &str) -> Vec<(usize, Kind)> {
        markers(report)
            .into_iter()
            .map(|marker| (marker.line, marker.kind))
            .collect()
    }

    /// Task 124's report (2026-11-07): written as a template, collected while
    /// its validation still ran.
    const TEMPLATE: &str = "## Outcome\n\ndone: the slice 10 owner contract is written.\n\n\
## What changed\n\nOne change set.\n\n**Results.** PENDING_RESULTS\n\n## Open issues\n\n\
PENDING_ISSUES\n";

    #[test]
    fn template_placeholders_are_markers_with_their_lines() {
        assert_eq!(
            kinds(TEMPLATE),
            [
                (9, Kind::Placeholder("PENDING_RESULTS".to_string())),
                (13, Kind::Placeholder("PENDING_ISSUES".to_string())),
            ]
        );
        let found = markers(TEMPLATE);
        assert_eq!(found[0].text, "**Results.** PENDING_RESULTS");
        assert!(
            found[0]
                .describe()
                .starts_with("line 9: the placeholder PENDING_RESULTS"),
            "{}",
            found[0].describe()
        );
    }

    /// Task 122's report (2026-11-07): an Outcome in progress, Results
    /// pending.
    #[test]
    fn a_draft_outcome_and_pending_lines_are_markers() {
        let report = "## Outcome\nIn progress (draft)\n\n## What changed\n- crates/x.rs\n\n\
### Results\n(pending)\n\n## Open issues\nTBD\n";
        assert_eq!(
            kinds(report),
            [
                (2, Kind::Draft),
                (8, Kind::Pending("(pending)".to_string())),
                (11, Kind::Pending("TBD".to_string())),
            ]
        );
    }

    #[test]
    fn every_form_of_a_placeholder_line_is_a_marker() {
        for line in [
            "(pending)",
            "(Pending)",
            "[pending]",
            "- (pending)",
            "> TODO",
            "* TBD.",
            "1. todo",
            "**Results.** (pending)",
            "**Results:** TBD",
            "**Results**: (pending)",
            "Open issues: TODO",
            "_(pending)_",
        ] {
            let report = format!("## Outcome\ndone\n## Results\n{line}\n");
            assert!(
                matches!(kinds(&report).as_slice(), [(4, Kind::Pending(_))]),
                "{line:?}: {:?}",
                kinds(&report)
            );
        }
    }

    #[test]
    fn every_form_of_a_draft_outcome_is_a_marker() {
        for report in [
            "## Outcome\nDraft: validation still running\n",
            "## Outcome\n\n**In progress**\n",
            "## Outcome\nwork in progress, tests running\n",
            "## Outcome\nWIP\n",
            "## Outcome\ndone (draft)\n",
            "## Outcome (draft)\ndone\n",
            "## Outcome: in progress\n",
            "Outcome: in progress\n",
            "**Status:** draft\n",
        ] {
            assert!(
                kinds(report).contains(&(1, Kind::Draft))
                    || kinds(report).iter().any(|(_, kind)| *kind == Kind::Draft),
                "{report:?}: {:?}",
                kinds(report)
            );
        }
    }

    #[test]
    fn a_finished_report_has_no_marker() {
        let report = "## Outcome\ndone: drafts are no longer taken as reports.\n\n\
## What changed\n- crates/spawn-cli/src/deliver.rs: the check (`PENDING_` placeholders, \
`(pending)`, `TBD`, `TODO`).\n- The draft contract is written; the work in progress \
before it was dropped.\n\n```\n// TODO: kept in the file, quoted here\nPENDING_X\nTBD\n```\n\n\
## Open issues\nnone\n";
        assert_eq!(kinds(report), []);
    }

    #[test]
    fn the_word_pending_in_a_sentence_is_not_a_marker() {
        let report = "## Outcome\ndone\n## What changed\nThe pending payout is now settled \
once; a payout still pending after a day is retried.\n- Payout 3: pending\n\
- ORDER_PENDING_PAYMENT and Pending_review are names in the code.\n## Open issues\n\
None, but pending review by the owner.\n";
        assert_eq!(kinds(report), []);
    }

    #[test]
    fn an_outcome_that_mentions_a_draft_in_a_sentence_is_not_a_marker() {
        for report in [
            "## Outcome\ndone: the draft contract is written.\n",
            "## Outcome\ndone; the in-progress flag is gone.\n",
            "## Outcome\nDrafted and verified: done.\n",
            "## Outcome\ndone\n\nDraft reports were refused twice.\n",
        ] {
            assert_eq!(kinds(report), [], "{report:?}");
        }
    }

    #[test]
    fn a_marker_in_a_code_span_or_block_is_quoted_not_written() {
        let report = "## Outcome\ndone\n## What changed\nThe check reads `PENDING_RESULTS` \
and `` `TODO` ``.\n~~~\n(pending)\n~~~\n";
        assert_eq!(kinds(report), []);
    }

    #[test]
    fn a_marker_record_names_its_line_kind_and_text() {
        let found = markers("## Outcome\nIn progress\n");
        assert_eq!(
            found[0].record(),
            serde_json::json!({"line": 2, "kind": "draft", "marker": "draft", "text": "In progress"})
        );
    }
}
