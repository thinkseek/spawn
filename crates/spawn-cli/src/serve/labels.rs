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
//! - `mission_name`: the head of the mission's text (`RSI: improve ...` is
//!   `RSI`, `Owner work on Supply PA: ...` is `Supply PA owner work`), or its
//!   first sentence, with the project of its source in front when the head
//!   does not name it (`Spawn RSI`); at most [`MISSION_MAX`] characters.
//! - `task_title`: an objective task's backlog title (the brief's first line,
//!   `Objective <n>: <title>`, as `objectives::brief` writes it; a title that
//!   is only `Task <n>` gives way to the prompt's first sentence); any other
//!   task's brief's first sentence, without the owner-work boilerplate in
//!   front, parentheses and markdown; at most [`TITLE_MAX`] characters, cut
//!   on a word.
//! - `label`: one line of at most [`LABEL_MAX`] characters, parts joined by
//!   ` · `: a task's is `<mission> · task <id>` (`objective <n>` for an
//!   objective task) `· <title> · <state>`; an agent's is its task's, with
//!   `resident <name>` before the state for a resident on its current task,
//!   `resident <name> · idle · last: ...` for one without, and
//!   `supervisor · ...` for the supervisor.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::supervise::SUPERVISOR_SESSION;

/// Most characters of a label.
pub(super) const LABEL_MAX: usize = 90;

/// Most characters of a task title.
pub(super) const TITLE_MAX: usize = 80;

/// Most characters of a mission name.
pub(super) const MISSION_MAX: usize = 32;

/// Fewest characters a title keeps inside a label, however long the rest.
const TITLE_MIN: usize = 16;

/// Characters of its title a label keeps before it shortens the mission
/// name.
const TITLE_ROOM: usize = 40;

/// Fewest characters a mission name keeps inside a label that shortens it
/// for its title.
const MISSION_MIN: usize = 12;

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

/// What a label is made of: missions, tasks and residents, from a board
/// (`/api/missions`' shape, or [`spawn_postgres::PostgresStore::label_facts`]).
#[derive(Debug, Clone, Default)]
pub(super) struct Context {
    missions: Vec<Mission>,
    tasks: Vec<Task>,
    residents: Vec<Resident>,
}

/// Objective titles by `(mission, objective)`, from a loaded backlog: what
/// a title falls back on when a brief does not carry one.
pub(super) type Titles = BTreeMap<(i64, u64), String>;

impl Context {
    /// From `{missions: [{id, text, sources, gate, accept, tasks: [...]}],
    /// residents: [{name, session_id}]}`.
    pub(super) fn from_board(board: &Value, titles: &Titles) -> Context {
        let mut context = Context::default();
        for mission in rows(&board["missions"]) {
            let Some(id) = mission["id"].as_i64() else {
                continue;
            };
            context.missions.push(Mission {
                id,
                name: mission_name(
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
            "label": task_label(mission, task, None),
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
        // Newest first: a session that took more than one task is on its
        // latest.
        let mut mine: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| task.session_id.as_deref() == Some(session_id))
            .collect();
        mine.sort_by_key(|task| std::cmp::Reverse(task.id));
        if let Some(resident) = self
            .residents
            .iter()
            .find(|resident| resident.session_id == session_id)
        {
            let current = mine.iter().find(|task| self.open(task));
            return match (current, mine.first()) {
                (Some(task), _) => self.with_task(task, Some(&resident.name)),
                (None, Some(last)) => {
                    let mission = self.mission(last.mission);
                    let head = vec![format!("resident {}", resident.name), "idle".to_string()];
                    let last_ref = format!(
                        "last: {}{}",
                        mission.map(|m| format!("{} ", m.name)).unwrap_or_default(),
                        task_ref(last)
                    );
                    json!({
                        "label": fit(&[head, vec![last_ref]].concat(), &last.title, &[]),
                        "task_title": last.title,
                        "mission_name": mission.map(|m| m.name.clone()),
                    })
                }
                (None, None) => json!({
                    "label": format!("resident {}{JOIN}idle", resident.name),
                    "task_title": null,
                    "mission_name": null,
                }),
            };
        }
        if let Some(task) = mine.first() {
            return self.with_task(task, None);
        }
        let mut label = humanized(agent_id.unwrap_or(session_id));
        if let Some(state) = state {
            label = format!("{label}{JOIN}{}", state.to_lowercase());
        }
        json!({"label": clip(&label, LABEL_MAX), "task_title": null, "mission_name": null})
    }

    fn with_task(&self, task: &Task, resident: Option<&str>) -> Value {
        match self.mission(task.mission) {
            Some(mission) => json!({
                "label": task_label(mission, task, resident),
                "task_title": task.title,
                "mission_name": mission.name,
            }),
            None => json!({"label": clip(&task.title, LABEL_MAX), "task_title": task.title,
                           "mission_name": null}),
        }
    }

    /// Whether a task is still being worked: not landed or cancelled, and
    /// not accepted unless its mission's gate still has to take it.
    fn open(&self, task: &Task) -> bool {
        match task.state.as_str() {
            "landed" | "cancelled" => false,
            "accepted" => self.mission(task.mission).is_some_and(|m| m.gated),
            _ => true,
        }
    }

    /// `supervisor · reviewing <n> reports (missions 7, 8)` while reports
    /// wait in its review missions, else `supervisor · <state>`.
    fn supervisor_label(&self, state: Option<&str>) -> String {
        let waiting: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|task| {
                task.state == "reported" && self.mission(task.mission).is_some_and(|m| m.review)
            })
            .collect();
        if waiting.is_empty() {
            let state = state.map_or_else(|| "idle".to_string(), str::to_lowercase);
            return format!("supervisor{JOIN}{state}");
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
        clip(
            &format!(
                "supervisor{JOIN}reviewing {} {noun} ({which} {})",
                waiting.len(),
                names.join(", ")
            ),
            LABEL_MAX,
        )
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

/// `task <id>`, or `objective <n>` for an objective task.
fn task_ref(task: &Task) -> String {
    match task.objective {
        Some(objective) => format!("objective {objective}"),
        None => format!("task {}", task.id),
    }
}

/// A task state as a label says it: `working` for delivered, `revising`
/// for revise, `awaiting gate` for accepted work a gate still has to take.
fn state_phrase(state: &str, gated: bool) -> &str {
    match state {
        "delivered" => "working",
        "revise" => "revising",
        "accepted" if gated => "awaiting gate",
        other => other,
    }
}

/// `<mission> · task <id> · <title> [· resident <name>] · <state>`.
fn task_label(mission: &Mission, task: &Task, resident: Option<&str>) -> String {
    let mut after = Vec::new();
    if let Some(name) = resident {
        after.push(format!("resident {name}"));
    }
    after.push(state_phrase(&task.state, mission.gated).to_string());
    let title = without_place(&task.title, &mission.name);
    // The title says most: a long mission name gives way to it first.
    let len = |part: &String| part.chars().count();
    let rest = len(&task_ref(task))
        + after.iter().map(len).sum::<usize>()
        + JOIN.chars().count() * (after.len() + 2);
    let named = mission.name.chars().count();
    let room = LABEL_MAX.saturating_sub(rest + named);
    let wanted = title.chars().count().min(TITLE_ROOM);
    let name = if room >= wanted {
        mission.name.clone()
    } else {
        clip(
            &mission.name,
            named.saturating_sub(wanted - room).max(MISSION_MIN),
        )
    };
    fit(&[name, task_ref(task)], title, &after)
}

/// `title` without a closing ` in <place>` the mission's name already says
/// (`Implement waitlists in Supply PA` in a Supply PA mission).
fn without_place<'a>(title: &'a str, mission: &str) -> &'a str {
    match title.rfind(" in ") {
        Some(at) if at > 0 && mentions(mission, &title[at + 4..]) => &title[..at],
        _ => title,
    }
}

/// `before · title · after`, the title cut on a word so that the whole is
/// at most [`LABEL_MAX`] characters (it keeps [`TITLE_MIN`] at least; the
/// whole is then clipped). An empty title is left out.
fn fit(before: &[String], title: &str, after: &[String]) -> String {
    let fixed: usize = before
        .iter()
        .chain(after)
        .map(|part| part.chars().count())
        .sum::<usize>()
        + JOIN.chars().count() * (before.len() + after.len());
    let room = LABEL_MAX.saturating_sub(fixed).max(TITLE_MIN);
    let mut parts: Vec<String> = before.to_vec();
    if !title.is_empty() {
        parts.push(clip(title, room));
    }
    parts.extend(after.iter().cloned());
    clip(&parts.join(JOIN), LABEL_MAX)
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

/// A mission's name, from its text and sources (module comment).
pub(super) fn mission_name(text: &str, sources: &[String]) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let first = unmarked(first);
    // A short head before a colon names the mission (`RSI: ...`).
    let head = first
        .split_once(':')
        .map(|(head, _)| head.trim())
        .and_then(|head| {
            let head = without_parentheses(head);
            let head = head.trim();
            (!head.is_empty() && head.split_whitespace().count() <= 6).then(|| head.to_string())
        });
    let from_head = head.is_some();
    let name = match head {
        Some(head) => match strip_prefix_ci(&head, "Owner work on ") {
            Some(subject) => format!("{subject} owner work"),
            None => head,
        },
        None => sentence(&without_parentheses(&first)),
    };
    let project = sources.first().and_then(|source| project(source));
    let name = match project {
        Some(project) if !mentions(&name, &project) => {
            if name.is_empty() {
                project
            } else if from_head {
                format!("{project} {name}")
            } else {
                format!("{project}: {name}")
            }
        }
        _ => name,
    };
    clip(&name, MISSION_MAX)
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
            return clip(&capitalized(&sentence(&unmarked(title))), TITLE_MAX);
        }
    }
    for line in rest {
        let title = cleaned(line);
        if !title.is_empty() {
            return clip(&title, TITLE_MAX);
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
    fn a_mission_is_named_by_its_texts_head_with_its_project() {
        let cases = [
            (
                "Owner work on Supply PA: changes the owner reviews before merging (protected files included: gate/, ...). Each",
                SUPPLY,
                "Supply PA owner work",
            ),
            (
                "Owner work on Spawn: changes the owner reviews before merging (protected files included: the gate, rsi/, scripts/, deploy/).",
                SPAWN,
                "Spawn owner work",
            ),
            (
                "Supply PA: give every provider (merchants, drivers, tour guides, producers, independent professionals) an AI business agent",
                SUPPLY,
                "Supply PA",
            ),
            (
                "RSI: improve Spawn by pursuing the controller's backlog objectives (rsi/backlog).",
                SPAWN,
                "Spawn RSI",
            ),
            (
                "Spawn follow-ups from mission 1 (behaviour-preserving unless stated):\n1. Task states as a type",
                SPAWN,
                "Spawn follow-ups from mission 1",
            ),
            (
                "Move the RSI page onto the RSI objective mission, and retire the old RSI controller's last reader (docs/fleet-architecture.md §10.4 and §10.6).\n\nRSI is now mission 4",
                SPAWN,
                "Spawn: Move the RSI page onto…",
            ),
            (
                "Recode Spawn for clarity and structure, without changing behaviour or the core.",
                SPAWN,
                "Recode Spawn for clarity and…",
            ),
            ("", SPAWN, "Spawn"),
        ];
        for (text, source, name) in cases {
            assert_eq!(mission_name(text, &sources(source)), name, "{text}");
            assert!(name.chars().count() <= MISSION_MAX);
        }
    }

    /// Brief shapes of the live tasks (first lines, read 2026-11-07).
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
                "Make the gate fast by running scenarios in parallel, each gate on its own…",
            ),
            (
                "Owner work on Spawn's web UI (web/): make it easier to read, ordered and grouped the way an observer needs, and lighter.",
                "Make it easier to read, ordered and grouped the way an observer needs, and…",
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
                "Review branch spawn/agent_worker_gate_throughput_45 of…",
            ),
            (
                "Review only, change nothing. The owner merged six branches into ~/projects/Spawn main at 0fb9e3f (fetch it read-only: git fetch",
                "Review only: The owner merged six branches into ~/projects/Spawn main at 0fb9e3f",
            ),
            // Plain sentences.
            (
                "A gate that cannot run for lack of disk room must not count toward a mission's halt.\n\nToday scripts/rsi/gate.sh exits 3",
                "A gate that cannot run for lack of disk room must not count toward a mission's…",
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
    fn a_label_gives_its_title_room_before_a_long_mission_name() {
        let long = Mission {
            id: 1,
            name: "Recode Spawn for clarity and…".to_string(),
            gated: false,
            review: true,
        };
        let task = Task {
            id: 7,
            mission: 1,
            session_id: None,
            state: "cancelled".to_string(),
            objective: None,
            title: "Fix two configuration inconsistencies in spawn-cli".to_string(),
        };
        let label = task_label(&long, &task, None);
        assert_eq!(
            label,
            "Recode Spawn for clarity… · task 7 · Fix two configuration inconsistencies… · cancelled"
        );
        assert!(label.chars().count() <= LABEL_MAX);
        // A place the mission already names is not said twice.
        let supply = Mission {
            id: 6,
            name: "Supply PA".to_string(),
            gated: true,
            review: false,
        };
        let objective = Task {
            id: 152,
            mission: 6,
            session_id: None,
            state: "accepted".to_string(),
            objective: Some(58),
            title: "Implement languages in Supply PA".to_string(),
        };
        assert_eq!(
            task_label(&supply, &objective, None),
            "Supply PA · objective 58 · Implement languages · awaiting gate"
        );
        assert_eq!(
            without_place("Move the RSI page in Spawn", "Supply PA"),
            "Move the RSI page in Spawn"
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
                     "brief": "Objective 51: Task 51\n\nImplement provider teams in Supply PA: docs/protocol.md"}
                 ]}
            ],
            "residents": [
                {"name": "rsi", "agent_id": "agent_resident_rsi", "session_id": "session_resident_rsi"},
                {"name": "supply", "agent_id": "agent_resident_supply", "session_id": "session_resident_supply"},
                {"name": "spare", "agent_id": "agent_resident_spare", "session_id": "session_resident_spare"}
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
    fn agents_are_labelled_by_their_task_or_their_role() {
        let context = Context::from_board(&board(), &Titles::new());
        assert_eq!(
            label_of(
                &context,
                "session_worker_slice12_156",
                None,
                Some("COMPLETED")
            ),
            "Supply PA owner work · task 156 · Write the slice 12 owner contract the same… · cancelled"
        );
        assert_eq!(
            label_of(&context, "session_resident_rsi", None, Some("WORKING")),
            "Spawn RSI · objective 58 · Budget overrun keeps the work… · resident rsi · awaiting gate"
        );
        assert_eq!(
            label_of(&context, "session_resident_supply", None, None),
            "resident supply · idle · last: Supply PA objective 51 · Implement provider teams in…"
        );
        assert_eq!(
            label_of(&context, "session_resident_spare", None, None),
            "resident spare · idle"
        );
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("WORKING")),
            "supervisor · reviewing 2 reports (missions 7, 8)"
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
        let fields = context.agent_fields("session_worker_draft_report_141", None, None);
        assert_eq!(fields["mission_name"], "Spawn owner work");
        assert_eq!(
            fields["task_title"],
            "A report that is still a draft is not taken as the task's report"
        );
        // Every label is one line within the bound.
        for session in [
            "session_worker_slice12_156",
            "session_worker_flaky_157",
            "session_worker_draft_report_141",
            "session_resident_rsi",
            "session_resident_supply",
            SUPERVISOR_SESSION,
        ] {
            let label = label_of(&context, session, None, None);
            assert!(label.chars().count() <= LABEL_MAX, "{label}");
            assert!(!label.contains('\n'), "{label}");
        }
    }

    #[test]
    fn the_supervisor_without_reports_says_its_state() {
        let mut quiet = board();
        quiet["missions"][0]["tasks"][1]["state"] = json!("accepted");
        quiet["missions"][1]["tasks"][0]["state"] = json!("accepted");
        let context = Context::from_board(&quiet, &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, Some("WORKING")),
            "supervisor · working"
        );
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, None),
            "supervisor · idle"
        );
        quiet["missions"][1]["tasks"][0]["state"] = json!("reported");
        let context = Context::from_board(&quiet, &Titles::new());
        assert_eq!(
            label_of(&context, SUPERVISOR_SESSION, None, None),
            "supervisor · reviewing 1 report (mission 7)"
        );
    }

    #[test]
    fn the_board_its_rsi_missions_the_fleet_and_attention_carry_labels() {
        let mut labelled = board();
        label_board(&mut labelled);
        let mission = &labelled["missions"][0];
        assert_eq!(mission["mission_name"], "Supply PA owner work");
        let task = &mission["tasks"][1];
        assert_eq!(task["mission_name"], "Supply PA owner work");
        assert_eq!(task["task_title"], "Make 048's restart test deterministic");
        assert_eq!(
            task["label"],
            "Supply PA owner work · task 157 · Make 048's restart test deterministic · reported"
        );
        assert_eq!(
            labelled["residents"][1]["label"],
            "resident supply · idle · last: Supply PA objective 51 · Implement provider teams in…"
        );

        // /api/rsi: a mission row carries its resident; a brief without a
        // title takes the backlog's.
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
            "Spawn RSI · objective 58 · Budget overrun keeps work · working"
        );
        assert_eq!(
            rsi[0]["resident"]["label"],
            "Spawn RSI · objective 58 · Budget overrun keeps work · resident rsi · working"
        );

        let context = Context::from_board(&board(), &Titles::new());
        let mut agents = json!([
            {"session_id": "session_worker_flaky_157", "agent_id": "agent_worker_flaky_157", "state": "WORKING"},
            {"session_id": "session_cli", "agent_id": "agent_cli", "state": "IDLE"}
        ]);
        label_fleet(&mut agents, &context);
        assert_eq!(
            agents[0]["label"],
            "Supply PA owner work · task 157 · Make 048's restart test deterministic · reported"
        );
        assert_eq!(
            agents[0]["task_title"],
            "Make 048's restart test deterministic"
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
            "Spawn owner work · task 141 · A report that is still a draft is not taken as… · reported"
        );
        assert_eq!(answer["pending"][1]["label"], "host");
        assert_eq!(answer["told"][0]["label"], "missions");
        assert_eq!(
            answer["on_own_jobs"][0]["mission_name"],
            "Supply PA owner work"
        );
        assert_eq!(
            answer["supervisor"]["label"],
            "supervisor · reviewing 2 reports (missions 7, 8)"
        );
    }
}
