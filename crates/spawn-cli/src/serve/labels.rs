//! Semantic labels: what the dashboard (and an agent reading its API) shows
//! first for an agent, a session, a task or an attention item, so that one
//! line says what it is doing. The raw ids stay beside them: they are the
//! keys of the CLI and of SQL.
//!
//! Everything comes from records that already exist, read at the request:
//! the mission's text, the task's brief and state, the objective's title as
//! the brief carries it, the resident roster. Nothing is stored and no id is
//! renamed (docs/view-architecture.md §12).
//!
//! - `mission_name`: a short head of the mission's text (four words at
//!   most before its first colon: `RSI: ...` is `Spawn RSI`, `Owner work on
//!   Supply PA: ...` is `Supply PA · owner work`), the project of its source
//!   in front when the head does not name it; otherwise the project and the
//!   mission's id (`Spawn mission 3`): never a clip of a sentence.
//! - `task_title`: an objective task's backlog title (the brief's first line,
//!   `Objective <n>: <title>`, as `objectives::brief` writes it; a title that
//!   is only `Task <n>` gives way to the prompt's first sentence); any other
//!   task's `Goal:` line when its brief has one (a brief may open with a
//!   setup step), else its first line, as its first sentence, without the
//!   owner-work boilerplate in front, parentheses and markdown; at most
//!   [`TITLE_MAX`] characters, cut on a word.
//! - `label`: a complete phrase, parts joined by ` · `, that leaves out what
//!   its row shows beside it (the raw id, the state): a task's is
//!   `<mission> · <title>`, with `objective <n>` after it for an objective
//!   task; a worker's is its latest task's; a resident's is its task's while
//!   it works one (assigned, delivered, revise), else `resident <name> ·
//!   idle · ...` with its last task, or what its latest input says (a task
//!   cancelled); the supervisor's says what its latest input asks of it (a
//!   dispatcher digest: `supervisor · reviewing the report of task 162 ·
//!   Implement extras`; an owner note: `supervisor · answering an owner
//!   note`), `idle · last: ...` once its turn has ended.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::supervise::SUPERVISOR_SESSION;

/// Most characters of a label: a bound against a runaway text, never met by
/// the parts themselves (a mission name, a whole title, a reference), so a
/// label is never cut mid-phrase. Two lines of a row; the supervisor's
/// card, which spans the page, may take a third.
pub(super) const LABEL_MAX: usize = 200;

/// Most characters of a task title.
pub(super) const TITLE_MAX: usize = 120;

/// Most words of a mission text's head that name the mission.
const HEAD_WORDS: usize = 4;

/// What joins the parts of a label.
const JOIN: &str = " · ";

/// One mission, as labels need it.
#[derive(Debug, Clone)]
struct Mission {
    id: i64,
    name: String,
    /// It has a gate: accepted work still waits for it.
    gated: bool,
    /// The supervisor reviews its reports.
    review: bool,
}

/// One task, as labels need it.
#[derive(Debug, Clone)]
struct Task {
    id: i64,
    mission: i64,
    session_id: Option<String>,
    state: String,
    objective: Option<u64>,
    title: String,
}

/// One resident.
#[derive(Debug, Clone)]
struct Resident {
    name: String,
    session_id: String,
}

/// A session's latest input: who sent it (`spawn-dispatch`, `human`,
/// `spawn-liveness`, ...) and its text's start.
#[derive(Debug, Clone)]
struct Input {
    principal: String,
    text: String,
}

/// What a label is made of: missions, tasks and residents, from a board
/// (`/api/missions`' shape, or [`spawn_postgres::PostgresStore::label_facts`]).
#[derive(Debug, Clone, Default)]
pub(super) struct Context {
    missions: Vec<Mission>,
    tasks: Vec<Task>,
    residents: Vec<Resident>,
    /// The latest input of the supervisor and of each resident.
    inputs: BTreeMap<String, Input>,
}

/// Objective titles by `(mission, objective)`, from a loaded backlog: what
/// a title falls back on when a brief does not carry one.
pub(super) type Titles = BTreeMap<(i64, u64), String>;

impl Context {
    /// From `{missions: [{id, text, sources, gate, accept, tasks: [...]}],
    /// residents: [{name, session_id}], inputs?: [{session_id, principal,
    /// text}]}`.
    pub(super) fn from_board(board: &Value, titles: &Titles) -> Context {
        let mut context = Context::default();
        for mission in rows(&board["missions"]) {
            let Some(id) = mission["id"].as_i64() else {
                continue;
            };
            context.missions.push(Mission {
                id,
                name: mission_name(
                    id,
                    mission["text"].as_str().unwrap_or_default(),
                    &sources(mission),
                ),
                gated: !mission["gate"].is_null(),
                review: mission["accept"] == "review",
            });
            for task in rows(&mission["tasks"]) {
                let Some(task_id) = task["id"].as_i64() else {
                    continue;
                };
                let objective = task["objective"].as_u64();
                let fallback = objective
                    .and_then(|n| titles.get(&(id, n)))
                    .map(String::as_str);
                context.tasks.push(Task {
                    id: task_id,
                    mission: id,
                    session_id: task["session_id"].as_str().map(str::to_string),
                    state: task["state"].as_str().unwrap_or("unknown").to_string(),
                    objective,
                    title: task_title(
                        task["brief"].as_str().unwrap_or_default(),
                        objective,
                        fallback,
                    ),
                });
            }
        }
        for resident in rows(&board["residents"]) {
            if let (Some(name), Some(session)) =
                (resident["name"].as_str(), resident["session_id"].as_str())
            {
                context.residents.push(Resident {
                    name: name.to_string(),
                    session_id: session.to_string(),
                });
            }
        }
        for input in rows(&board["inputs"]) {
            if let (Some(session), Some(principal)) =
                (input["session_id"].as_str(), input["principal"].as_str())
            {
                context.inputs.insert(
                    session.to_string(),
                    Input {
                        principal: principal.to_string(),
                        text: input["text"].as_str().unwrap_or_default().to_string(),
                    },
                );
            }
        }
        context
    }

    fn mission(&self, id: i64) -> Option<&Mission> {
        self.missions.iter().find(|mission| mission.id == id)
    }

    /// `{label, task_title, mission_name}` of task `id`.
    fn task_fields(&self, id: i64) -> Option<Value> {
        let task = self.tasks.iter().find(|task| task.id == id)?;
        let mission = self.mission(task.mission)?;
        Some(json!({
            "label": task_label(mission, task),
            "task_title": task.title,
            "mission_name": mission.name,
        }))
    }

    /// `{label, task_title, mission_name}` of an agent's session.
    /// `state` is its fleet state (`WORKING`, ...) when known.
    pub(super) fn agent_fields(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
        state: Option<&str>,
    ) -> Value {
        if session_id == SUPERVISOR_SESSION {
            return json!({
                "label": self.supervisor_label(state),
                "task_title": null,
                "mission_name": null,
            });
        }
        let mine = self.tasks_of(session_id);
        if let Some(resident) = self
            .residents
            .iter()
            .find(|resident| resident.session_id == session_id)
        {
            // A resident works a task while it is assigned, delivered or
            // sent back; once reported it waits on others.
            let current = mine
                .iter()
                .find(|task| matches!(task.state.as_str(), "assigned" | "delivered" | "revise"));
            if let Some(task) = current {
                return self.with_task(task);
            }
            let idle = format!("resident {}{JOIN}idle", resident.name);
            let cancelled = self
                .inputs
                .get(session_id)
                .and_then(|input| cancelled_task(&input.text));
            let label = match (cancelled, mine.first()) {
                (Some(id), _) => {
                    let what = self
                        .task_by_id(id)
                        .map(|task| format!(" · {}", task.title))
                        .unwrap_or_default();
                    format!("{idle}{JOIN}task {id} cancelled{what}")
                }
                (None, Some(last)) => format!("{idle}{JOIN}last: {}", self.task_phrase(last)),
                (None, None) => idle,
            };
            let last = mine.first();
            return json!({
                "label": clip(&label, LABEL_MAX),
                "task_title": last.map(|task| task.title.clone()),
                "mission_name": last.and_then(|task| self.mission(task.mission)).map(|m| m.name.clone()),
            });
        }
        if let Some(task) = mine.first() {
            return self.with_task(task);
        }
        let mut label = humanized(agent_id.unwrap_or(session_id));
        if let Some(state) = state {
            label = format!("{label}{JOIN}{}", state.to_lowercase());
        }
        json!({"label": clip(&label, LABEL_MAX), "task_title": null, "mission_name": null})
    }

    /// A session's tasks, newest first.
    fn tasks_of(&self, session_id: &str) -> Vec<&Task> {
        let mut mine: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| task.session_id.as_deref() == Some(session_id))
            .collect();
        mine.sort_by_key(|task| std::cmp::Reverse(task.id));
        mine
    }

    fn task_by_id(&self, id: i64) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    fn gated(&self, task: &Task) -> bool {
        self.mission(task.mission).is_some_and(|m| m.gated)
    }

    /// A task as a label names it where its row shows the agent's state,
    /// not the task's (the fleet, attention, a resident, the supervisor's
    /// reference): its mission and title, and its state unless it is being
    /// worked (`· cancelled`, `· landed`, `· reported`, `· gating`), so a
    /// finished task never reads as current work.
    fn task_phrase(&self, task: &Task) -> String {
        let phrase = match self.mission(task.mission) {
            Some(mission) => task_label(mission, task),
            None => task_words(task),
        };
        self.with_state(task, phrase)
    }

    /// `phrase · <state>`, unless the task is being worked.
    fn with_state(&self, task: &Task, phrase: String) -> String {
        match state_phrase(&task.state, self.gated(task)) {
            "working" => phrase,
            state => format!("{phrase}{JOIN}{state}"),
        }
    }

    fn with_task(&self, task: &Task) -> Value {
        let mission = self.mission(task.mission);
        json!({
            "label": clip(&self.task_phrase(task), LABEL_MAX),
            "task_title": task.title,
            "mission_name": mission.map(|m| m.name.clone()),
        })
    }

    /// What the supervisor is doing, from its latest input (module
    /// comment); `idle · last: ...` when its fleet state says its turn has
    /// ended. Without an input: the reports waiting in its review missions,
    /// else its state.
    fn supervisor_label(&self, state: Option<&str>) -> String {
        let busy = state.is_none_or(|state| state.eq_ignore_ascii_case("WORKING"));
        let doing = self
            .inputs
            .get(SUPERVISOR_SESSION)
            .map(|input| self.input_phrase(input));
        let label = match doing {
            Some(doing) if busy => format!("supervisor{JOIN}{doing}"),
            Some(doing) => format!("supervisor{JOIN}idle{JOIN}last: {doing}"),
            None => self.reviews_waiting().map_or_else(
                || {
                    format!(
                        "supervisor{JOIN}{}",
                        state.map_or_else(|| "idle".to_string(), str::to_lowercase)
                    )
                },
                |reviews| format!("supervisor{JOIN}{reviews}"),
            ),
        };
        clip(&label, LABEL_MAX)
    }

    /// `reviewing 2 reports (missions 7, 8)`, while reported tasks wait in
    /// review missions.
    fn reviews_waiting(&self) -> Option<String> {
        let waiting: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| {
                task.state == "reported" && self.mission(task.mission).is_some_and(|m| m.review)
            })
            .collect();
        if waiting.is_empty() {
            return None;
        }
        let mut missions: Vec<i64> = waiting.iter().map(|task| task.mission).collect();
        missions.sort_unstable();
        missions.dedup();
        let names: Vec<String> = missions.iter().map(i64::to_string).collect();
        let noun = if waiting.len() == 1 {
            "report"
        } else {
            "reports"
        };
        let which = if names.len() == 1 {
            "mission"
        } else {
            "missions"
        };
        Some(format!(
            "reviewing {} {noun} ({which} {})",
            waiting.len(),
            names.join(", ")
        ))
    }

    /// What an input asks of the supervisor, in a few words.
    fn input_phrase(&self, input: &Input) -> String {
        match input.principal.as_str() {
            "spawn-dispatch" => self.digest_phrase(&input.text),
            "human" => "answering an owner note".to_string(),
            "spawn-liveness" => "restarted, catching up on the fleet".to_string(),
            other => format!("working on {} input", humanized(other)),
        }
    }

    /// A dispatcher digest (`Spawn dispatcher: 2 updates for you. ... -
    /// session_x — completed, review it: ...`): its first update in words,
    /// and how many more.
    fn digest_phrase(&self, text: &str) -> String {
        let Some((session, kind)) = first_update(text) else {
            return "reading a dispatcher digest".to_string();
        };
        let reference = match self.tasks_of(&session).first() {
            // The task's id names it; its title says what it is, whole (no
            // mission: the id is enough to find it, and the label stays
            // within its bound).
            Some(task) => {
                self.with_state(task, format!("task {}{JOIN}{}", task.id, task_words(task)))
            }
            None if session == "host" || session == "missions" => session.clone(),
            None => humanized(&session),
        };
        // One line, its heading: up to the first ':' outside parentheses,
        // and a repeat's count (`attention (2 of them, the latest)`).
        let (kind, repeats) = repeated(heading(kind.lines().next().unwrap_or("")));
        let kind = kind.to_lowercase();
        let counted = match repeats {
            Some(count) => format!("{kind} ({count})"),
            None => kind.clone(),
        };
        let doing = if kind.starts_with("completed") || kind.starts_with("report") {
            // A report waits for review: the task's own "reported" would
            // say it twice.
            let reference = reference.strip_suffix(" · reported").unwrap_or(&reference);
            format!("reviewing the report of {reference}")
        } else if kind.starts_with("idle") {
            format!("next step for {reference}")
        } else if kind.starts_with("human message") {
            format!("following up a human's message to {reference}")
        } else if kind.starts_with("attention") || kind.starts_with("stalled") {
            format!("checking {reference}{JOIN}{counted}")
        } else {
            format!("handling {reference}: {counted}")
        };
        match updates(text) {
            Some(count) if count > 1 => format!("{doing} (+{} more)", count - 1),
            _ => doing,
        }
    }
}

/// The session and kind of a digest's first update: `- session_x — <kind>`.
fn first_update(text: &str) -> Option<(String, String)> {
    let at = text.find("- session_").map(|at| at + 2).or_else(|| {
        ["- host — ", "- missions — "]
            .iter()
            .filter_map(|mark| text.find(mark).map(|at| at + 2))
            .min()
    })?;
    let rest = &text[at..];
    let (session, kind) = rest.split_once(" — ")?;
    let session = session.trim();
    if session.is_empty() || session.contains(char::is_whitespace) {
        return None;
    }
    Some((session.to_string(), kind.trim().to_string()))
}

/// A digest update's heading: its text up to the first `:` outside
/// parentheses (`host faults at the gate (the host's, two runs): ...` keeps
/// its parenthesis whole).
fn heading(kind: &str) -> &str {
    let mut depth = 0usize;
    for (at, c) in kind.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return kind[..at].trim(),
            _ => {}
        }
    }
    kind.trim()
}

/// A heading without a repeat's ` (<n> of them, the latest)`, and `n`.
fn repeated(heading: &str) -> (&str, Option<u64>) {
    if let Some(open) = heading.rfind(" (")
        && let Some(inner) = heading[open + 2..].strip_suffix(" of them, the latest)")
        && let Ok(count) = inner.parse()
    {
        return (heading[..open].trim_end(), Some(count));
    }
    (heading, None)
}

/// The count of a digest's `Spawn dispatcher: <n> update(s) for you.`
fn updates(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("Spawn dispatcher: ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// The task of a `[Task #149 was cancelled ...` notice.
fn cancelled_task(text: &str) -> Option<i64> {
    let rest = text.strip_prefix("[Task #")?;
    let (id, after) = rest.split_once(' ')?;
    after
        .starts_with("was cancelled")
        .then(|| id.parse().ok())?
}

/// A task without its mission: its title, or `task <id>`.
fn task_words(task: &Task) -> String {
    if task.title.is_empty() {
        format!("task {}", task.id)
    } else {
        task.title.clone()
    }
}

fn rows(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn sources(mission: &Value) -> Vec<String> {
    rows(&mission["sources"])
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// A task state as a label says it: `working` for delivered, `revising`
/// for revise, `gating` for accepted work a gate still has to take.
fn state_phrase(state: &str, gated: bool) -> &str {
    match state {
        "delivered" => "working",
        "revise" => "revising",
        "accepted" if gated => "gating",
        other => other,
    }
}

/// `<mission> · <title>`, `· objective <n>` after it for an objective
/// task: the whole title, never cut. A mission's task card shows the task's
/// id and state beside it; elsewhere [`Context::task_phrase`] adds the state.
fn task_label(mission: &Mission, task: &Task) -> String {
    let title = without_place(&task.title, &mission.name);
    let mut parts = vec![mission.name.clone()];
    parts.push(if title.is_empty() {
        format!("task {}", task.id)
    } else {
        title.to_string()
    });
    if let Some(objective) = task.objective {
        parts.push(format!("objective {objective}"));
    }
    parts.join(JOIN)
}

/// `title` without a closing ` in <place>` the mission's name already says
/// (`Implement waitlists in Supply PA` in a Supply PA mission).
fn without_place<'a>(title: &'a str, mission: &str) -> &'a str {
    match title.rfind(" in ") {
        Some(at) if at > 0 && mentions(mission, &title[at + 4..]) => &title[..at],
        _ => title,
    }
}

/// `text` at most `max` characters, as a complete phrase: a sentence longer
/// than that ends at its last clause boundary (`, `, `; `, ` — `) inside
/// the bound, so a title is not cut mid-phrase; only a sentence with no
/// such boundary in its first third onward is cut on a word ([`clip`]).
fn whole_clause(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let inside: String = text.chars().take(max).collect();
    let boundary = [", ", "; ", " — "]
        .iter()
        .filter_map(|mark| inside.rfind(mark))
        .max()
        .filter(|at| *at >= inside.len() / 3);
    match boundary {
        Some(at) => inside[..at].trim_end().to_string(),
        None => clip(text, max),
    }
}

/// `text` cut on a word boundary to at most `max` characters, `…` marking
/// the cut.
pub(super) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep: String = text.chars().take(max.saturating_sub(1)).collect();
    let cut = match keep.rfind(' ') {
        Some(at) if at >= keep.len() / 2 => &keep[..at],
        _ => keep.as_str(),
    };
    format!("{}…", cut.trim_end_matches([' ', ',', ';', ':', '·']))
}

/// An id read as words: `agent_smoke_transcript` is `smoke transcript`.
fn humanized(id: &str) -> String {
    let bare = id
        .strip_prefix("agent_")
        .or_else(|| id.strip_prefix("session_"))
        .unwrap_or(id);
    bare.replace('_', " ")
}

/// The project a source names: `/…/missions/supply-pa.git` is `Supply PA`
/// (a word of two letters or fewer in capitals, others capitalised).
fn project(source: &str) -> Option<String> {
    let base = source.trim_end_matches('/').rsplit('/').next()?;
    let base = base.strip_suffix(".git").unwrap_or(base);
    if base.is_empty() {
        return None;
    }
    let words: Vec<String> = base
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            if word.chars().count() <= 2 {
                word.to_uppercase()
            } else {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            }
        })
        .collect();
    Some(words.join(" "))
}

/// A mission's name, from its id, text and sources (module comment).
pub(super) fn mission_name(id: i64, text: &str, sources: &[String]) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let first = unmarked(first);
    // A short head before a colon names the mission (`RSI: ...`).
    let head = first.split_once(':').and_then(|(head, _)| {
        let head = without_parentheses(head);
        let head = head.trim();
        let subject = strip_prefix_ci(head, "Owner work on ").unwrap_or(head);
        (!head.is_empty() && subject.split_whitespace().count() <= HEAD_WORDS)
            .then(|| head.to_string())
    });
    let project = sources.first().and_then(|source| project(source));
    let Some(head) = head else {
        return match project {
            Some(project) => format!("{project} mission {id}"),
            None => format!("mission {id}"),
        };
    };
    let name = match strip_prefix_ci(&head, "Owner work on ") {
        Some(subject) => format!("{subject}{JOIN}owner work"),
        None => head,
    };
    match project {
        Some(project) if !mentions(&name, &project) => format!("{project} {name}"),
        _ => name,
    }
}

/// Whether `text` names `project`, case and `-` aside.
fn mentions(text: &str, project: &str) -> bool {
    let fold = |text: &str| text.to_lowercase().replace('-', " ");
    fold(text).contains(&fold(project))
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// A task's title (module comment). `fallback` is the backlog's title of
/// its objective, for a brief that does not carry one.
pub(super) fn task_title(brief: &str, objective: Option<u64>, fallback: Option<&str>) -> String {
    let lines: Vec<&str> = brief
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut rest = lines.as_slice();
    if let Some(n) = objective {
        let marker = format!("Objective {n}:");
        let carried = rest
            .first()
            .and_then(|line| line.trim().strip_prefix(&marker))
            .map(str::trim);
        if carried.is_some() {
            rest = &rest[1..];
        }
        let title = carried
            .filter(|title| !generic(title))
            .or(fallback.filter(|title| !generic(title)));
        if let Some(title) = title {
            return whole_clause(&capitalized(&sentence(&unmarked(title))), TITLE_MAX);
        }
    }
    // A brief may open with a setup step ("First check you have ..."): its
    // `Goal:` line, when it has one, says what the task is.
    let goal = rest
        .iter()
        .take(8)
        .find(|line| strip_prefix_ci(unmarked(line).trim(), "Goal:").is_some());
    if let Some(line) = goal {
        let title = cleaned(line);
        if !title.is_empty() {
            return whole_clause(&title, TITLE_MAX);
        }
    }
    for line in rest {
        let title = cleaned(line);
        if !title.is_empty() {
            return whole_clause(&title, TITLE_MAX);
        }
    }
    String::new()
}

/// A title that names nothing: empty, or only `Task <n>` / `Objective <n>`
/// (`load_backlog`'s stand-in for a missing title).
fn generic(title: &str) -> bool {
    let title = title.trim();
    if title.is_empty() {
        return true;
    }
    ["Task ", "Objective "].iter().any(|word| {
        title
            .strip_prefix(word)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// One brief line as a title: no markdown, no parentheses, no owner-work
/// boilerplate in front, its first sentence, capitalised.
fn cleaned(line: &str) -> String {
    let mut text = without_parentheses(&unmarked(line)).trim().to_string();
    // `Owner work on Spawn: ...`, `Owner-protected work on Supply PA. ...`,
    // `Owner work on Spawn, top priority: ...`: the subject is the mission's.
    for prefix in ["Owner work on ", "Owner-protected work on ", "Owner work: "] {
        if let Some(after) = strip_prefix_ci(&text, prefix) {
            let end = [": ", ". "]
                .iter()
                .filter_map(|mark| after.find(mark))
                .min();
            text = match end {
                Some(at) => after[at + 2..].trim().to_string(),
                None => after.trim_end_matches(['.', ':']).trim().to_string(),
            };
            break;
        }
    }
    if let Some(after) = strip_prefix_ci(&text, "Goal: ") {
        text = after.trim().to_string();
    }
    if let Some(after) = strip_prefix_ci(&text, "Review only, change nothing.") {
        let after = after.trim();
        if !(after.starts_with("Review") || after.starts_with("Re-review")) {
            return format!("Review only: {}", sentence(after));
        }
        text = after.to_string();
    }
    capitalized(&sentence(&text))
}

/// The first sentence of `text`: up to the first `. `, `: `, `; ` or ` — `
/// (not after `e.g` or `i.e`), without a trailing `e.g`, comma or stop.
fn sentence(text: &str) -> String {
    let mut end = text.len();
    for mark in [". ", ": ", "; ", " — "] {
        let mut from = 0;
        while let Some(at) = text[from..].find(mark).map(|at| at + from) {
            let before = &text[..at];
            if mark == ". " && (before.ends_with("e.g") || before.ends_with("i.e")) {
                from = at + mark.len();
                continue;
            }
            end = end.min(at);
            break;
        }
    }
    let mut out = text[..end].trim();
    loop {
        let trimmed = out
            .trim_end_matches(['.', ',', ':', ';'])
            .trim_end()
            .trim_end_matches(" e.g")
            .trim_end_matches(" i.e")
            .trim_end();
        if trimmed == out {
            break;
        }
        out = trimmed;
    }
    out.to_string()
}

/// `text` with its first letter in capitals.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `text` without parenthesised groups (nested ones too; an unclosed one
/// runs to the end), spaces folded.
fn without_parentheses(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let folded = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // A group removed before a stop leaves `word .`: the stop goes back to
    // its word. A stop that starts a word (`.gitignore`) stays where it is.
    let chars: Vec<char> = folded.chars().collect();
    let mut joined = String::with_capacity(folded.len());
    for (at, &c) in chars.iter().enumerate() {
        let ends_word = chars.get(at + 1).is_none_or(|next| next.is_whitespace());
        if matches!(c, '.' | ',' | ':' | ';') && ends_word && joined.ends_with(' ') {
            joined.pop();
        }
        joined.push(c);
    }
    joined
}

/// A line without its markdown: heading, quote and list marks in front,
/// emphasis and code marks, links reduced to their text.
fn unmarked(line: &str) -> String {
    let mut text = line.trim();
    loop {
        let before = text;
        text = text.trim_start_matches('#').trim_start();
        text = text.strip_prefix("> ").unwrap_or(text);
        text = text
            .strip_prefix("- ")
            .or_else(|| text.strip_prefix("* "))
            .unwrap_or(text);
        if let Some((number, rest)) = text.split_once(". ")
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
        {
            text = rest;
        }
        if text == before {
            break;
        }
    }
    let text = text.replace("**", "").replace("__", "").replace('`', "");
    // [text](url) → text
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|at| at + open) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|at| at + close) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `label`, `task_title` and `mission_name` set on `object`.
fn set(object: &mut Value, fields: &Value) {
    if let (Some(object), Some(fields)) = (object.as_object_mut(), fields.as_object()) {
        for (key, value) in fields {
            object.insert(key.clone(), value.clone());
        }
    }
}

/// `/api/missions`' board labelled: each mission its `mission_name`, each
/// task its `label`, `task_title` and `mission_name`, each resident its
/// `label`. Read before the board is slimmed (it reads the briefs).
pub(super) fn label_board(board: &mut Value) {
    let context = Context::from_board(board, &Titles::new());
    label_missions(board.get_mut("missions"), &context);
    if let Some(residents) = board.get_mut("residents").and_then(Value::as_array_mut) {
        for resident in residents {
            let fields = resident_fields(&context, resident);
            set(resident, &fields);
        }
    }
}

fn resident_fields(context: &Context, resident: &Value) -> Value {
    let session = resident["session_id"].as_str().unwrap_or_default();
    context.agent_fields(session, resident["agent_id"].as_str(), None)
}

fn label_missions(missions: Option<&mut Value>, context: &Context) {
    let Some(missions) = missions.and_then(Value::as_array_mut) else {
        return;
    };
    for mission in missions {
        let name = mission["id"]
            .as_i64()
            .and_then(|id| context.mission(id))
            .map(|m| m.name.clone());
        if let Some(fields) = mission.as_object_mut() {
            fields.insert("mission_name".to_string(), json!(name));
        }
        let Some(tasks) = mission.get_mut("tasks").and_then(Value::as_array_mut) else {
            continue;
        };
        for task in tasks {
            if let Some(fields) = task["id"].as_i64().and_then(|id| context.task_fields(id)) {
                set(task, &fields);
            }
        }
    }
}

/// `/api/rsi`'s missions labelled, as [`label_board`] does, each resident
/// too; an objective title a brief lacks comes from the loaded backlog (the
/// mission's `objectives` rows).
pub(super) fn label_rsi(missions: &mut [Value]) {
    let mut titles = Titles::new();
    let mut residents = Vec::new();
    for mission in missions.iter() {
        let Some(id) = mission["id"].as_i64() else {
            continue;
        };
        for objective in rows(&mission["objectives"]) {
            if let (Some(n), Some(title)) = (objective["id"].as_u64(), objective["title"].as_str())
            {
                titles.insert((id, n), title.to_string());
            }
        }
        if mission["resident"].is_object() {
            residents.push(mission["resident"].clone());
        }
    }
    let board = json!({"missions": missions, "residents": residents});
    let context = Context::from_board(&board, &titles);
    let mut list = Value::Array(missions.to_vec());
    label_missions(Some(&mut list), &context);
    if let Value::Array(labelled) = list {
        for (mission, mut done) in missions.iter_mut().zip(labelled) {
            if done["resident"].is_object() {
                let fields = resident_fields(&context, &done["resident"]);
                set(&mut done["resident"], &fields);
            }
            *mission = done;
        }
    }
}

/// `/api/fleet`'s agents labelled.
pub(super) fn label_fleet(agents: &mut Value, context: &Context) {
    for agent in agents.as_array_mut().into_iter().flatten() {
        let session = agent["session_id"].as_str().unwrap_or_default().to_string();
        let fields = context.agent_fields(
            &session,
            agent["agent_id"].as_str(),
            agent["state"].as_str(),
        );
        set(agent, &fields);
    }
}

/// `/api/attention` labelled: each item of `pending` and `told`, each of
/// `on_own_jobs`, and the supervisor. An item about the host or the
/// missions as a whole is labelled `host` or `missions`.
pub(super) fn label_attention(answer: &mut Value, context: &Context) {
    for list in ["pending", "told", "on_own_jobs"] {
        for item in answer
            .get_mut(list)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let session = item["session_id"].as_str().unwrap_or_default().to_string();
            let fields = match (session.as_str(), item["agent_id"].as_str()) {
                ("host" | "missions", None) => {
                    json!({"label": session, "task_title": null, "mission_name": null})
                }
                (_, agent) => context.agent_fields(&session, agent, None),
            };
            set(item, &fields);
        }
    }
    if answer["supervisor"].is_object() {
        let state = answer
            .pointer("/supervisor/judgement/state")
            .and_then(Value::as_str)
            .map(str::to_string);
        let fields = context.agent_fields(SUPERVISOR_SESSION, None, state.as_deref());
        set(&mut answer["supervisor"], &fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPAWN: &str = "/home/u/.local/share/spawn-rsi/missions/spawn.git";
    const SUPPLY: &str = "/home/u/.local/share/spawn-rsi/missions/supply-pa.git";

    fn sources(source: &str) -> Vec<String> {
        vec![source.to_string()]
    }

    /// Mission texts as the live missions 1-8 begin (read 2026-11-07).
    #[test]
    fn a_mission_is_named_by_a_short_head_or_its_project_and_id() {
        let cases = [
            (
                8,
                "Owner work on Supply PA: changes the owner reviews before merging (protected files included: gate/, ...). Each",
                SUPPLY,
                "Supply PA · owner work",
            ),
            (
                7,
                "Owner work on Spawn: changes the owner reviews before merging (protected files included: the gate, rsi/, scripts/, deploy/).",
                SPAWN,
                "Spawn · owner work",
            ),
            (
                6,
                "Supply PA: give every provider (merchants, drivers, tour guides, producers, independent professionals) an AI business agent",
                SUPPLY,
                "Supply PA",
            ),
            (
                4,
                "RSI: improve Spawn by pursuing the controller's backlog objectives (rsi/backlog).",
                SPAWN,
                "Spawn RSI",
            ),
            (
                3,
                "Spawn follow-ups from mission 1 (behaviour-preserving unless stated):\n1. Task states as a type",
                SPAWN,
                "Spawn mission 3",
            ),
            (
                5,
                "Move the RSI page onto the RSI objective mission, and retire the old RSI controller's last reader (docs/fleet-architecture.md §10.4 and §10.6).\n\nRSI is now mission 4",
                SPAWN,
                "Spawn mission 5",
            ),
            (
                1,
                "Recode Spawn for clarity and structure, without changing behaviour or the core.",
                SPAWN,
                "Spawn mission 1",
            ),
            (9, "", SPAWN, "Spawn mission 9"),
        ];
        for (id, text, source, name) in cases {
            assert_eq!(mission_name(id, text, &sources(source)), name, "{text}");
        }
        assert_eq!(mission_name(2, "Add a view.", &[]), "mission 2");
    }

    #[test]
    fn a_brief_that_opens_with_a_setup_step_is_titled_by_its_goal() {
        let brief = "First check you have the refactored tree: crates/spawn-cli/src/commands/ must exist. If they do not, stop.\n\nGoal: fix two configuration inconsistencies in spawn-cli. This task deliberately changes behaviour.\n\nFacts: (a) ...";
        assert_eq!(
            task_title(brief, None, None),
            "Fix two configuration inconsistencies in spawn-cli"
        );
    }

    #[test]
    fn a_title_is_the_briefs_first_sentence_without_boilerplate() {
        let cases = [
            // Owner work, the subject the mission's.
            (
                "Owner work on Spawn: a report that is still a draft is not taken as the task's report.\n\nWhat happened",
                "A report that is still a draft is not taken as the task's report",
            ),
            (
                "Owner work on Supply PA: write the slice 12 owner contract the same way slice 11 was written. Read the slice 11 contract on main",
                "Write the slice 12 owner contract the same way slice 11 was written",
            ),
            (
                "Owner work on Supply PA: make 048's restart test deterministic.",
                "Make 048's restart test deterministic",
            ),
            // Parenthesised permissions, then a sentence or a colon.
            (
                "Owner work on Spawn (owner-protected: you may edit scripts/rsi/gate.sh, scripts/rsi/test-gate.sh, scripts/disk.sh and rsi/scenarios). Implement objective 022",
                "Implement objective 022",
            ),
            (
                "Owner work on Spawn (you may edit rsi/scenarios/*.sh and scripts/blackbox/; this is owner work on protected files): the shared test Postgres keeps",
                "The shared test Postgres keeps",
            ),
            (
                "Owner work on Spawn (protected files: you may edit rsi/backlog/028.json-033.json and rsi/scenarios/028.sh-033.sh only). Goal: record the owner's ruling",
                "Record the owner's ruling",
            ),
            (
                "Owner-protected work on Supply PA (you may edit vitest.config.ts and test helpers). The gate rejected mission 6's task 38 (objective 015) on",
                "The gate rejected mission 6's task 38 on",
            ),
            (
                "Owner work on Spawn, top priority: make the gate fast by running scenarios in parallel, each gate on its own disposable PostgreSQL server (G",
                "Make the gate fast by running scenarios in parallel, each gate on its own disposable PostgreSQL server",
            ),
            (
                "Owner work on Spawn's web UI (web/): make it easier to read, ordered and grouped the way an observer needs, and lighter.",
                "Make it easier to read, ordered and grouped the way an observer needs, and lighter",
            ),
            // Goal: ..., e.g.
            (
                "Goal: split crates/spawn-cli/src/main.rs (~1000 lines mixing every command, runtime setup and the loop) into modules by responsibility, e.g.",
                "Split crates/spawn-cli/src/main.rs into modules by responsibility",
            ),
            (
                "Goal: replace bare task-state strings with one Rust enum. Behaviour-preserving.",
                "Replace bare task-state strings with one Rust enum",
            ),
            // Reviews.
            (
                "Review only, change nothing. Review branch spawn/agent_worker_gate_throughput_45 of ~/.local/share/spawn-rsi/missions/spawn.git (fetch read-",
                "Review branch spawn/agent_worker_gate_throughput_45 of ~/.local/share/spawn-rsi/missions/spawn.git",
            ),
            (
                "Review only, change nothing. The owner merged six branches into ~/projects/Spawn main at 0fb9e3f (fetch it read-only: git fetch",
                "Review only: The owner merged six branches into ~/projects/Spawn main at 0fb9e3f",
            ),
            // Plain sentences.
            (
                "A gate that cannot run for lack of disk room must not count toward a mission's halt.\n\nToday scripts/rsi/gate.sh exits 3",
                "A gate that cannot run for lack of disk room must not count toward a mission's halt",
            ),
            (
                "Small owner-protected fix in Supply PA's .gitignore. Its rule 'node_modules/' (trailing slash) matches",
                "Small owner-protected fix in Supply PA's .gitignore",
            ),
            (
                "Gate throughput is the fleet's bottleneck (owner-protected work; you may edit scripts/rsi/gate.sh, crates/spawn-cli/src/integrate.rs and doc",
                "Gate throughput is the fleet's bottleneck",
            ),
            // Markdown.
            (
                "## **Fix** the `flaky` [runner test](https://x/y)\n",
                "Fix the flaky runner test",
            ),
            ("", ""),
        ];
        for (brief, title) in cases {
            let got = task_title(brief, None, None);
            assert_eq!(got, title, "{brief}");
            assert!(got.chars().count() <= TITLE_MAX, "{got}");
        }
    }
    #[test]
    fn an_objective_tasks_title_is_its_backlog_title() {
        // RSI: the brief carries the backlog's title.
        assert_eq!(
            task_title(
                "Objective 9: Keep up to N objective tasks in flight per mission, each on its own agent\n\nRead `assign_next`",
                Some(9),
                None
            ),
            "Keep up to N objective tasks in flight per mission, each on its own agent"
        );
        // A long title: its first clause, cut on a word.
        let long = "Objective 33: A base that disagrees with itself is a host fault, not an abort: recorded with the scenario and both exits\n\nRead";
        assert_eq!(
            task_title(long, Some(33), None),
            "A base that disagrees with itself is a host fault, not an abort"
        );
        // Supply PA: the backlog has no title (`Task 47` stands in), so the
        // prompt's first sentence names it.
        assert_eq!(
            task_title(
                "Objective 47: Task 47\n\nImplement provider verification in Supply PA: docs/protocol.md, sections \"Provider verification\"",
                Some(47),
                None
            ),
            "Implement provider verification in Supply PA"
        );
        // A brief without the line: the loaded backlog's title.
        assert_eq!(
            task_title("Do the thing.", Some(5), Some("Count the gulls")),
            "Count the gulls"
        );
        // Neither: the brief.
        assert_eq!(
            task_title("Do the thing.", Some(5), Some("Task 5")),
            "Do the thing"
        );
    }
    #[test]
    fn a_clip_cuts_on_a_word() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one two three four", 12), "one two…");
        assert_eq!(clip("abcdefghijklmnop", 8), "abcdefg…");
        assert_eq!(clip("one two, three", 10), "one two…");
    }
    fn board() -> Value {
        json!({
            "missions": [
                {"id": 8, "text": "Owner work on Supply PA: changes the owner reviews.",
                 "sources": [SUPPLY], "gate": null, "accept": "review",
                 "tasks": [
                    {"id": 156, "session_id": "session_worker_slice12_156",
                     "agent_id": "agent_worker_slice12_156", "state": "cancelled", "objective": null,
                     "brief": "Owner work on Supply PA: write the slice 12 owner contract the same way slice 11 was written."},
                    {"id": 157, "session_id": "session_worker_flaky_157",
                     "agent_id": "agent_worker_flaky_157", "state": "reported", "objective": null,
                     "brief": "Owner work on Supply PA: make 048's restart test deterministic."}
                 ]},
                {"id": 7, "text": "Owner work on Spawn: changes the owner reviews.",
                 "sources": [SPAWN], "gate": null, "accept": "review",
                 "tasks": [
                    {"id": 141, "session_id": "session_worker_draft_report_141",
                     "agent_id": "agent_worker_draft_report_141", "state": "reported", "objective": null,
                     "brief": "Owner work on Spawn: a report that is still a draft is not taken as the task's report."}
                 ]},
                {"id": 4, "text": "RSI: improve Spawn by pursuing the controller's backlog objectives.",
                 "sources": [SPAWN], "gate": "/x/rsi/scenarios", "accept": "gate",
                 "tasks": [
                    {"id": 150, "session_id": "session_resident_rsi", "agent_id": "agent_resident_rsi",
                     "state": "landed", "objective": 57,
                     "brief": "Objective 57: Budget overrun keeps work\n\nRead it."},
                    {"id": 160, "session_id": "session_resident_rsi", "agent_id": "agent_resident_rsi",
                     "state": "accepted", "objective": 58,
                     "brief": "Objective 58: Budget overrun keeps the work it did\n\nRead it."}
                 ]},
                {"id": 6, "text": "Supply PA: give every provider an AI business agent.",
                 "sources": [SUPPLY], "gate": "/x/gate", "accept": "gate",
                 "tasks": [
                    {"id": 139, "session_id": "session_resident_supply",
                     "agent_id": "agent_resident_supply", "state": "landed", "objective": 51,
                     "brief": "Objective 51: Task 51\n\nImplement provider teams in Supply PA: docs/protocol.md"},
                    {"id": 149, "session_id": "session_resident_supply",
                     "agent_id": "agent_resident_supply", "state": "cancelled", "objective": 53,
                     "brief": "Objective 53: Task 53\n\nImplement packages and memberships in Supply PA: docs"},
                    {"id": 162, "session_id": "session_worker_objective_162",
                     "agent_id": "agent_worker_objective_162", "state": "delivered", "objective": 62,
                     "brief": "Objective 62: Task 62\n\nImplement extras in Supply PA: docs/protocol.md, sections \"Extras\""}
                 ]}
            ],
            "residents": [
                {"name": "rsi", "agent_id": "agent_resident_rsi", "session_id": "session_resident_rsi"},
                {"name": "supply", "agent_id": "agent_resident_supply", "session_id": "session_resident_supply"},
                {"name": "spare", "agent_id": "agent_resident_spare", "session_id": "session_resident_spare"}
            ],
            "inputs": [
                {"session_id": "session_supervisor", "principal": "spawn-dispatch",
                 "text": "Spawn dispatcher: 2 updates for you. Check a worker with `spawn tail --session <session>` before acting. - session_worker_objective_162 — completed, review it: finished its task (COMPLETED). - host — disk space"},
                {"session_id": "session_resident_supply", "principal": "spawn-dispatch",
                 "text": "[Task #149 was cancelled — this is not a new task] Task #149 of mission #6 was cancelled by the integrator."}
            ]
        })
    }

    fn label_of(
        context: &Context,
        session: &str,
        agent: Option<&str>,
        state: Option<&str>,
    ) -> String {
        context.agent_fields(session, agent, state)["label"]
            .as_str()
            .expect("a label")
            .to_string()
    }

    #[test]
    fn a_label_carries_the_whole_title_and_leaves_out_the_id() {
        let context = Context::from_board(&board(), &Titles::new());
        assert_eq!(
            label_of(
                &context,
                "session_worker_slice12_156",
                None,
                Some("COMPLETED")
            ),
            "Supply PA · owner work · Write the slice 12 owner contract the same way slice 11 was written · cancelled"
        );
        assert_eq!(
            label_of(
                &context,
                "session_worker_objective_162",
                None,
                Some("WORKING")
            ),
            "Supply PA · Implement extras · objective 62"
        );
        let fields = context.agent_fields("session_worker_draft_report_141", None, None);
        assert_eq!(fields["mission_name"], "Spawn · owner work");
        assert_eq!(
            fields["label"],
            "Spawn · owner work · A report that is still a draft is not taken as the task's report · reported"
        );
        assert_eq!(
            label_of(
                &context,
                "session_smoke_transcript",
                Some("agent_smoke_transcript"),
                Some("COMPLETED")
            ),
            "smoke transcript · completed"
        );
        // Long live titles stay whole.
        let mut long = board();
        long["missions"][2]["tasks"][1]["state"] = json!("delivered");
        long["missions"][2]["tasks"][1]["objective"] = json!(19);
        long["missions"][2]["tasks"][1]["brief"] = json!(
            "Objective 19: A delivery whose retries are spent without typing is re-issued, not left for a human\n\nRead"
        );
        let context = Context::from_board(&long, &Titles::new());
        let label = label_of(&context, "session_resident_rsi", None, Some("WORKING"));
        assert_eq!(
            label,
            "Spawn RSI · A delivery whose retries are spent without typing is re-issued, not left for a human · objective 19"
        );
        assert!(!label.contains('…'));
    }

    #[test]
    fn a_resident_between_tasks_says_idle_and_why() {
        let context = Context::from_board(&board(), &Titles::new());
        // Its latest task is accepted, waiting on the gate: idle, its last.
        assert_eq!(
            label_of(&context, "session_resident_rsi", None, Some("COMPLETED")),
            "resident rsi · idle · last: Spawn RSI · Budget overrun keeps the work it did · objective 58 · gating"
        );
        // Its latest input is a cancellation.
        assert_eq!(
            label_of(&context, "session_resident_supply", None, None),
            "resident supply · idle · task 149 cancelled · Implement packages and memberships in Supply PA"
        );
        assert_eq!(
            label_of(&context, "session_resident_spare", None, None),
            "resident spare · idle"
        );
    }

    #[test]
    fn the_supervisor_says_what_its_latest_input_asks_of_it() {
        let context = Context::from_board(&board(), &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("WORKING")),
            "supervisor · reviewing the report of task 162 · Implement extras in Supply PA (+1 more)"
        );
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("COMPLETED")),
            "supervisor · idle · last: reviewing the report of task 162 · Implement extras in Supply PA (+1 more)"
        );
        let mut noted = board();
        noted["inputs"][0] = json!({"session_id": "session_supervisor", "principal": "human",
                                    "text": "Owner note (2026-11-07 23:07 SGT), act on it now: ..."});
        let context = Context::from_board(&noted, &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("WORKING")),
            "supervisor · answering an owner note"
        );
        noted["inputs"][0] = json!({"session_id": "session_supervisor", "principal": "spawn-dispatch",
                                    "text": "Spawn dispatcher: 1 update for you. - session_worker_flaky_157 — idle, give it the next step or review it: s"});
        let context = Context::from_board(&noted, &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, None),
            "supervisor · next step for task 157 · Make 048's restart test deterministic · reported"
        );
        // No input: the reports waiting in its review missions.
        let mut quiet = board();
        quiet["inputs"] = json!([]);
        let context = Context::from_board(&quiet, &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("WORKING")),
            "supervisor · reviewing 2 reports (missions 7, 8)"
        );
    }

    #[test]
    fn the_board_its_rsi_missions_the_fleet_and_attention_carry_labels() {
        let mut labelled = board();
        label_board(&mut labelled);
        let mission = &labelled["missions"][0];
        assert_eq!(mission["mission_name"], "Supply PA · owner work");
        let task = &mission["tasks"][1];
        assert_eq!(task["task_title"], "Make 048's restart test deterministic");
        assert_eq!(
            task["label"],
            "Supply PA · owner work · Make 048's restart test deterministic"
        );

        let mut rsi = vec![json!({
            "id": 4, "text": "RSI: improve Spawn.", "sources": [SPAWN], "gate": "/g", "accept": "gate",
            "resident": {"name": "rsi", "agent_id": "agent_resident_rsi",
                         "session_id": "session_resident_rsi", "state": "active"},
            "objectives": [{"id": 58, "title": "Budget overrun keeps work"}],
            "tasks": [{"id": 160, "session_id": "session_resident_rsi", "state": "delivered",
                       "objective": 58, "brief": "Work on it."}]
        })];
        label_rsi(&mut rsi);
        assert_eq!(rsi[0]["mission_name"], "Spawn RSI");
        assert_eq!(
            rsi[0]["tasks"][0]["task_title"],
            "Budget overrun keeps work"
        );
        assert_eq!(
            rsi[0]["tasks"][0]["label"],
            "Spawn RSI · Budget overrun keeps work · objective 58"
        );
        assert_eq!(
            rsi[0]["resident"]["label"],
            "Spawn RSI · Budget overrun keeps work · objective 58"
        );

        let context = Context::from_board(&board(), &Titles::new());
        let mut agents = json!([
            {"session_id": "session_worker_flaky_157", "agent_id": "agent_worker_flaky_157", "state": "WORKING"},
            {"session_id": "session_cli", "agent_id": "agent_cli", "state": "IDLE"}
        ]);
        label_fleet(&mut agents, &context);
        assert_eq!(
            agents[0]["label"],
            "Supply PA · owner work · Make 048's restart test deterministic · reported"
        );
        assert_eq!(agents[1]["label"], "cli · idle");
        assert_eq!(agents[1]["mission_name"], json!(null));

        let mut answer = json!({
            "pending": [
                {"session_id": "session_worker_draft_report_141", "agent_id": "agent_worker_draft_report_141"},
                {"session_id": "host", "agent_id": null}
            ],
            "told": [{"session_id": "missions", "agent_id": null}],
            "on_own_jobs": [{"session_id": "session_worker_flaky_157", "agent_id": "agent_worker_flaky_157"}],
            "supervisor": {"session_id": "session_supervisor", "judgement": {"state": "WORKING"}}
        });
        label_attention(&mut answer, &context);
        assert_eq!(
            answer["pending"][0]["label"],
            "Spawn · owner work · A report that is still a draft is not taken as the task's report · reported"
        );
        assert_eq!(answer["pending"][1]["label"], "host");
        assert_eq!(answer["told"][0]["label"], "missions");
        assert_eq!(
            answer["on_own_jobs"][0]["mission_name"],
            "Supply PA · owner work"
        );
        assert!(answer["supervisor"]["label"].as_str().is_some_and(|label| {
            label.starts_with("supervisor · reviewing the report of task 162")
        }));
    }

    /// Live first sentences longer than the bound (tasks 92 and 109).
    #[test]
    fn a_long_title_ends_at_a_clause_not_mid_phrase() {
        let briefs = [
            (
                "Owner work on Spawn: implement objective 022 plus one gate saving, as owner work rather than a mission 4 objective, because both change the gate itself and the owner reviews such changes.",
                "Implement objective 022 plus one gate saving, as owner work rather than a mission 4 objective",
            ),
            (
                "Owner work on Spawn: the owner can type to an agent from the web page, and the page stops presenting the human as something the fleet waits on, everywhere it says so.",
                "The owner can type to an agent from the web page",
            ),
        ];
        for (brief, title) in briefs {
            let got = task_title(brief, None, None);
            assert_eq!(got, title);
            assert!(!got.contains('…'));
        }
        assert_eq!(whole_clause("short", 10), "short");
        assert_eq!(whole_clause("nocommas in this long text", 12), "nocommas…");
    }

    /// rev-b: objective_113 (cancelled) and objective_160 (working) on one
    /// objective read the same once the state was dropped.
    #[test]
    fn a_cancelled_and_a_working_task_of_one_objective_read_differently() {
        let board = json!({
            "missions": [{"id": 4, "text": "RSI: improve Spawn.", "sources": [SPAWN],
                          "gate": "/g", "accept": "gate",
                          "tasks": [
                {"id": 113, "session_id": "session_worker_objective_113", "state": "cancelled",
                 "objective": 23, "brief": "Objective 23: An idle fleet costs the database one watermark read per pass\n\nRead"},
                {"id": 160, "session_id": "session_worker_objective_160", "state": "delivered",
                 "objective": 23, "brief": "Objective 23: An idle fleet costs the database one watermark read per pass\n\nRead"},
                {"id": 115, "session_id": "session_worker_objective_115", "state": "landed",
                 "objective": 44, "brief": "Objective 44: Keep it\n\nRead"},
                {"id": 116, "session_id": "session_worker_objective_116", "state": "accepted",
                 "objective": 45, "brief": "Objective 45: Gate it\n\nRead"}
            ]}],
            "residents": []
        });
        let context = Context::from_board(&board, &Titles::new());
        let cancelled = label_of(
            &context,
            "session_worker_objective_113",
            None,
            Some("COMPLETED"),
        );
        let working = label_of(
            &context,
            "session_worker_objective_160",
            None,
            Some("WORKING"),
        );
        assert_ne!(cancelled, working);
        assert_eq!(
            cancelled,
            "Spawn RSI · An idle fleet costs the database one watermark read per pass · objective 23 · cancelled"
        );
        assert_eq!(
            working,
            "Spawn RSI · An idle fleet costs the database one watermark read per pass · objective 23"
        );
        assert_eq!(
            label_of(&context, "session_worker_objective_115", None, None),
            "Spawn RSI · Keep it · objective 44 · landed"
        );
        assert_eq!(
            label_of(&context, "session_worker_objective_116", None, None),
            "Spawn RSI · Gate it · objective 45 · gating"
        );
        // A mission's task card shows the state itself: its label stays without.
        let mut cards = board.clone();
        label_board(&mut cards);
        assert_eq!(
            cards["missions"][0]["tasks"][0]["label"],
            "Spawn RSI · An idle fleet costs the database one watermark read per pass · objective 23"
        );
    }

    #[test]
    fn a_digest_heading_is_one_line_and_keeps_its_parenthesis() {
        assert_eq!(
            heading("host faults at the gate (the host's, two runs): retried"),
            "host faults at the gate (the host's, two runs)"
        );
        assert_eq!(heading("attention: WAITING by jev"), "attention");
        assert_eq!(heading("no colon here"), "no colon here");
        let context =
            Context::from_board(&json!({"missions": [], "residents": []}), &Titles::new());
        let phrase = context.digest_phrase(
            "Spawn dispatcher: 1 update for you. - host — host faults at the gate (the host's, two runs)\nmore lines\nand more",
        );
        assert_eq!(
            phrase,
            "handling host: host faults at the gate (the host's, two runs)"
        );
        assert!(!phrase.contains('\n'));
    }

    #[test]
    fn a_digests_first_update_and_a_cancellation_are_read() {
        assert_eq!(
            first_update(
                "Spawn dispatcher: 1 update for you. Check ... - session_a — completed, review it: x"
            ),
            Some((
                "session_a".to_string(),
                "completed, review it: x".to_string()
            ))
        );
        assert_eq!(
            first_update("Spawn dispatcher: 1 update for you. - host — disk space on this host"),
            Some(("host".to_string(), "disk space on this host".to_string()))
        );
        assert_eq!(first_update("Owner note: hello"), None);
        assert_eq!(updates("Spawn dispatcher: 4 updates for you."), Some(4));
        // A report digest reads as a review, like a completed one, never
        // as a raw heading (rev-b: "· reported: report, review it (name…").
        let context = Context::from_board(&board(), &Titles::new());
        let report = context.digest_phrase(
            "Spawn dispatcher: 1 update for you. - session_worker_flaky_157 — report, review it (name its revision): rev 0",
        );
        assert_eq!(
            report,
            "reviewing the report of task 157 · Make 048's restart test deterministic"
        );
        assert!(!report.contains("report, review it"));
        // A repeated item: its count, not the digest's wording.
        assert_eq!(
            context.digest_phrase(
                "Spawn dispatcher: 1 update for you. - session_worker_flaky_157 — attention (2 of them, the latest): WAITING"
            ),
            "checking task 157 · Make 048's restart test deterministic · reported · attention (2)"
        );
        assert_eq!(
            repeated("attention (2 of them, the latest)"),
            ("attention", Some(2))
        );
        assert_eq!(
            repeated("host faults (the host's, two runs)"),
            ("host faults (the host's, two runs)", None)
        );
        assert_eq!(
            cancelled_task("[Task #149 was cancelled — this is not a new task]"),
            Some(149)
        );
        assert_eq!(cancelled_task("New task #158 for mission #6"), None);
    }
}
