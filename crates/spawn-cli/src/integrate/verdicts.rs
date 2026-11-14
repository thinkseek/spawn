//! How a gate's outcome becomes a record's fields: the host-fault,
//! base-disagreement, cannot-judge and main-red fields, room and budget
//! shortfalls, and a rejection's reason and evidence. Moved here from
//! integrate.rs unchanged (S2 of the split). integrate.rs re-exports
//! what it and its test modules use, so every name resolves inside
//! `integrate` as it did; nothing outside `integrate` used these, and
//! `crate::integrate::<item>` no longer reaches them.

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    BASE_DISAGREES_ALERT_AT, HOST_FAULT_ESCALATE_AT, Mission, NOTE_LOG_CHARS, ROOM_ESCALATE_AFTER,
    Task, cut, git, host, short_commit, spoken,
};
use crate::gate::{self, GateFailure, Shortfall};

/// The record of a gate whose rejection was the host's (`fault`), the
/// `faults`-th host fault in a row on the mission, with its `backoff`: the
/// reason naming the evidence, the evidence itself, the gate's output, the
/// count and `escalate` (the second in a row only). Its `due_ms` is the
/// record's own time plus the backoff, written by the store.
pub(crate) fn host_fault_fields(
    fault: &host::HostFault,
    faults: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let count = fault.failures.len();
    let evidence = if fault.crash.is_empty() {
        format!(
            "every failure check-tests.sh printed ({count}) is a connection error to the test \
             database"
        )
    } else {
        format!(
            "the test database's log shows a crash during the gate ({}), and every failure \
             check-tests.sh printed ({count}) is a database error",
            fault.crash[0]
        )
    };
    let mut fields = json!({
        "reason": format!(
            "host fault: {evidence}; not a verdict on the work, judged again in {}",
            spoken(backoff)
        ),
        "log": log,
        "evidence": fault.evidence(),
        "failures": fault.failures_value(),
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    });
    if !fault.crash.is_empty() {
        fields["crash"] = json!(fault.crash);
    }
    fields
}

/// The record of a gate whose base disagreed with itself (objective 033):
/// the scenario the gate named, both of its exits, the gate's setup seconds
/// when it printed its timing, the `faults`-th host fault in a row on the
/// mission (020's run, which this evidence shares) and the
/// `disagreements`-th base disagreement in a row, with its `backoff`.
///
/// A base that disagrees with itself never escalates (`escalate` false, for
/// every one of the run): the record that makes the run
/// [`BASE_DISAGREES_ALERT_AT`] carries `alert` true, and every other one
/// false. That record is the alert until objective 030 exists, and the
/// dispatcher's digest takes it to the supervisor once
/// ([`crate::dispatch::digest::base_disagrees_alerts`]).
pub(crate) fn base_disagrees_fields(
    scenario: &str,
    exits: [u8; 2],
    setup_s: Option<u64>,
    faults: u32,
    disagreements: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let setup = setup_s.map_or_else(String::new, |seconds| {
        format!(", after a setup of {seconds} s")
    });
    let mut fields = json!({
        "reason": format!(
            "host fault: the base disagrees with itself: {scenario} exited {}, then {} on the base\
             {setup}; not a verdict, judged again later",
            exits[0], exits[1]
        ),
        "log": log,
        "evidence": BASE_DISAGREES,
        "scenario": scenario,
        "exits": exits,
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": false,
        "base_disagrees": disagreements,
        "alert": disagreements == BASE_DISAGREES_ALERT_AT,
    });
    // The setup's seconds only when the gate printed its timing line: the
    // field is left out, not null.
    if let Some(seconds) = setup_s {
        fields["setup_s"] = json!(seconds);
    }
    fields
}

/// The evidence a `host_fault` record names when the base disagreed with
/// itself (its own scenario failing and then passing, before any candidate
/// code ran): objective 020's verdict with this objective's evidence.
pub(crate) const BASE_DISAGREES: &str = "base_disagrees";

/// The record of a host fault the gate reported itself
/// ([`GateFailure::HostFault`], exit [`gate::HOST_FAULT_EXIT`] with its
/// fact), the `faults`-th host fault in a row on the mission, with its
/// `backoff`: `evidence` `gate` and the gate's `fact` (`scenario`, `on`:
/// `base` or `candidate and base`, `runs`), and the rest as
/// [`host_fault_fields`] writes it. Neither `gate_aborts` nor
/// `gate_rejections` moves.
pub(crate) fn gate_host_fault_fields(
    failure: &GateFailure,
    faults: u32,
    backoff: Duration,
) -> Value {
    if let GateFailure::ValidatorDefect {
        scenario,
        runs,
        reason,
        defects,
        log,
    } = failure
    {
        return json!({
            "reason": format!(
                "validator defect: {} fails on the candidate and on the base, run again now \
                 ({runs}): the scenario's, not the work's and not the host's; never a \
                 rejection; fix it or put it on the owner's `quarantined` list; not a verdict \
                 on the work, judged again in {}",
                defects.join(", "),
                spoken(backoff)
            ),
            "log": log,
            "evidence": gate::VALIDATOR_DEFECT,
            "validator_defects": defects,
            "fact": {"scenario": scenario, "on": "candidate and base", "runs": runs,
                     "reason": reason},
            "host_faults": faults,
            "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            "escalate": faults == HOST_FAULT_ESCALATE_AT,
        });
    }
    let GateFailure::HostFault {
        scenario,
        on,
        runs,
        reason,
        tree,
        log,
    } = failure
    else {
        return aborted(failure);
    };
    let place = if on == "base" {
        format!("{scenario} exited {runs} on the base, before any candidate code ran")
    } else if on == "sandbox" {
        format!("the gate's sandbox never started {scenario} ({reason})")
    } else if on == "test server" {
        format!("the gate's own test server never started ({reason})")
    } else if on == "runner" {
        format!("the gate was abandoned: {reason}")
    } else {
        format!("{scenario} failed on the candidate and on the base ({runs})")
    };
    // A sandbox that never started has its own evidence, so a record says
    // which fault it was without reading its text.
    let evidence = if on == "sandbox" {
        gate::SANDBOX_NEVER_STARTED
    } else if on == "test server" {
        gate::TEST_SERVER_NEVER_STARTED
    } else if on == "runner" {
        gate::ABANDONED
    } else {
        "gate"
    };
    json!({
        "reason": format!(
            "host fault: {place}; not a verdict on the work, judged again in {}",
            spoken(backoff)
        ),
        "log": log,
        "evidence": evidence,
        "fact": if let Some(tree) = tree {
            json!({"scenario": scenario, "on": on, "runs": runs, "reason": reason,
                   "tree": tree.as_str()})
        } else {
            json!({"scenario": scenario, "on": on, "runs": runs, "reason": reason})
        },
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    })
}

/// The record of a gate that cannot judge `scenario` here, the gate's own
/// `cannot_judge` fact (exit [`gate::HOST_FAULT_EXIT`], the key that tells
/// it from the gate's `host_fault` fact): the `faults`-th host fault in a row
/// on the mission (020's run, which every evidence shares), with its
/// `backoff`, never that the work failed.
///
/// Its cause is the scenario's own, quoted from the gate's log
/// ([`cannot_judge_cause`]) into `cause` and the reason: 029 says it needs a
/// systemd user manager, Supply PA's 073 that it needs a PostgreSQL server,
/// and a record that named one cause for all of them sent a supervisor to
/// build a task on a cause nobody had seen (task 208). With no such line in
/// the log, the record says the gate gave no cause, and names none.
pub(crate) fn cannot_judge_fields(
    scenario: &str,
    faults: u32,
    backoff: Duration,
    log: &str,
) -> Value {
    let cause = cannot_judge_cause(scenario, log);
    let said = cause.as_deref().map_or_else(
        || "the gate's log gives no cause".to_string(),
        |cause| format!("it says: {cause}"),
    );
    json!({
        "reason": format!(
            "host fault: this host cannot judge {scenario} ({said}); not a verdict on the work, \
             judged again in {}",
            spoken(backoff)
        ),
        "cause": cause,
        "log": log,
        "evidence": CANNOT_JUDGE,
        "scenario": scenario,
        "host_faults": faults,
        "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
        "escalate": faults == HOST_FAULT_ESCALATE_AT,
    })
}

/// What `scenario` said when it could not be judged, from the gate's `log`:
/// the last line of its own block (from a `| == ` line naming it to the next
/// `| == ` line) that says it cannot judge ("073: cannot judge: …", 029's
/// "029: cannot be judged here: …"), on one line, without the log's
/// markers (the gate's `| `, the integrator's `! ` before the gate's stderr).
/// Outside any block (a line on the gate's stderr, after its blocks), the
/// last line that begins with the scenario's own name and says the same.
/// None when the log has neither: the scenario gave no cause.
pub(crate) fn cannot_judge_cause(scenario: &str, log: &str) -> Option<String> {
    let says = |text: &str| text.contains("cannot judge") || text.contains("cannot be judged");
    let stem = scenario.split('.').next().unwrap_or(scenario);
    let mut in_block = false;
    let mut in_own_block = None;
    let mut by_name = None;
    for line in log.lines() {
        if let Some(header) = line.strip_prefix("| == ") {
            in_block = header.split([' ', ':']).any(|word| word == scenario);
            continue;
        }
        // The gate's `| ` before a scenario's output, the integrator's `! `
        // before a line of the gate's stderr: markers of the log, not the
        // scenario's words.
        let text = line
            .strip_prefix('|')
            .or_else(|| line.strip_prefix('!'))
            .unwrap_or(line)
            .trim();
        // The gate's own stage lines ("stage gate cannot judge 120ms base:
        // 073.sh", an uncached base run that exited 6) name the outcome,
        // not a cause: never quoted as the scenario's words.
        if !says(text) || text.starts_with("stage ") {
            continue;
        }
        if in_block {
            in_own_block = Some(text);
        }
        if text.starts_with(&format!("{stem}:")) {
            by_name = Some(text);
        }
    }
    in_own_block
        .or(by_name)
        .map(|text| crate::deliver::one_line(text, 400))
}

/// The evidence a `host_fault` record names when the host cannot judge the
/// objective's scenario at all (029's systemd user scopes): a fact about the
/// host, which judges that objective outside the gate.
pub(crate) const CANNOT_JUDGE: &str = "cannot_judge";

/// The evidence of a `host_fault` whose gate found a regression
/// load-sensitive on the candidate (gate.sh's "Like for like before a
/// rejection": failed at load, passed alone as the base does): no verdict on
/// the work, judged again after the backoff, counted toward no cap or halt.
/// Its re-judging is bounded (sync/design-load-sensitive-bound.md): the
/// second one for the same pushed commit is [`LOAD_SENSITIVE_PARKED`].
pub(crate) const LOAD_SENSITIVE: &str = "load_sensitive";

/// The evidence of the bound's record: a task's pushed commit found
/// load-sensitive twice, by whatever scenarios, is parked. It is not judged
/// again until a new commit is pushed or a person releases it (`spawn task
/// release`); the mission's other tasks are judged meanwhile, and the
/// escalation names the scenarios and their root-fix owners.
pub(crate) const LOAD_SENSITIVE_PARKED: &str = "load_sensitive_parked";

/// The record of a task's pushed commit `pushed` parked by the
/// load-sensitive bound: the `scenarios` its two judgings found (the record's `sensitive`)
/// load-sensitive (every one, in the order found), each with its root-fix
/// owner from `owners` (None when the owners list names none), and the
/// gate's `log`. No backoff: nothing judges it again until it is released.
pub(crate) fn load_sensitive_parked_fields(
    pushed: &str,
    scenarios: &[String],
    owners: &[(String, Option<String>)],
    log: &str,
) -> Value {
    let named = scenarios.join(", ");
    let fixers = owners
        .iter()
        .map(|(scenario, owner)| match owner {
            Some(owner) => format!("{scenario}: {owner}"),
            None => format!("{scenario}: no owner named (scenario-owners)"),
        })
        .collect::<Vec<_>>()
        .join("; ");
    json!({
        "reason": format!(
            "host fault: the pushed commit {} was load-sensitive twice ({named}): parked, not \
             judged again until a new commit or a person's release (spawn task release); a root \
             fix is wanted ({fixers}); not a verdict on the work",
            pushed.get(..12).unwrap_or(pushed)
        ),
        "evidence": LOAD_SENSITIVE_PARKED,
        "scenario": scenarios.first().map_or("", String::as_str),
        "sensitive": scenarios,
        "owners": owners
            .iter()
            .map(|(scenario, owner)| json!({"scenario": scenario, "owner": owner}))
            .collect::<Vec<_>>(),
        "pushed": pushed,
        "parked": true,
        "escalate": true,
        "backoff_ms": 0,
        "log": log,
    })
}

/// The evidence of a task's `host_fault` whose gate found main of its source
/// red on a check ([`main_red_fields`]): no fault of the work, no run of host
/// faults, no count toward the cap or a halt.
pub(crate) const MAIN_RED: &str = "main_red";

/// The record of a gate that had no room to judge `task`, at `now_ms`: the
/// reason with the check, its measure and its threshold, the facts
/// themselves, and `escalate` when the run of them on this task began more
/// than [`ROOM_ESCALATE_AFTER`] ago and none escalated yet. Another task's
/// run gives it no age.
pub(crate) fn no_room(
    mission: &Mission,
    task: i64,
    short: &Shortfall,
    log: &str,
    now_ms: i64,
) -> Value {
    short_of_room(mission, task, short, log, now_ms)
}

/// The record of a judging of `task` that found the host short of room at
/// `now_ms`, by the gate's own floor or by the integrator's checks before
/// it ([`gate::Room`]): `check`, `measured` and `threshold`; for disk also
/// `free_kb`, `min_gb` and `tmpdir`, for the database its `server` (never
/// the URL, so never its password); a reason that names the check, the
/// measure and the threshold; `log`; and `escalate` as [`no_room`] says.
pub(crate) fn short_of_room(
    mission: &Mission,
    task: i64,
    short: &Shortfall,
    log: &str,
    now_ms: i64,
) -> Value {
    let after = i64::try_from(ROOM_ESCALATE_AFTER.as_millis()).unwrap_or(i64::MAX);
    let escalate = mission.gate_room_task == Some(task)
        && !mission.gate_room_escalated
        && mission
            .gate_room_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) > after);
    let reason = match short {
        Shortfall::Disk {
            free_kb,
            min_gb,
            tmpdir,
        } => {
            #[allow(clippy::cast_precision_loss)]
            let free_gib = *free_kb as f64 / 1_048_576.0;
            format!(
                "no room to judge now: {free_gib:.1} GiB free in {tmpdir} (TMPDIR), under \
                 SPAWN_GATE_MIN_FREE_GB={min_gb}; not a verdict, judged again later"
            )
        }
        Shortfall::Memory {
            available_mb,
            min_mb,
        } => format!(
            "no room to judge now: the host's memory has {available_mb} MiB available \
             (MemAvailable), under SPAWN_GATE_MIN_FREE_MEMORY_MB={min_mb}; not a verdict, judged \
             again later"
        ),
        Shortfall::Database {
            server,
            probe,
            error,
        } => format!(
            "no room to judge now: the test database ({server}) failed the probe \"{probe}\": \
             {error}; not a verdict, judged again later"
        ),
        Shortfall::Slot { waited_ms, budget } => format!(
            "no room to judge now: a scenario run waited {} for a host scenario slot \
             (SPAWN_GATE_HOST_JOBS={budget} in all, shared by every gate on this host); \
             not a verdict, judged again later",
            gate::spoken_ms(*waited_ms)
        ),
    };
    let mut record = json!({
        "reason": reason,
        "check": short.check(),
        "measured": short.measured(),
        "threshold": short.threshold(),
        "log": log,
        "escalate": escalate,
    });
    let extra = match short {
        Shortfall::Disk {
            free_kb,
            min_gb,
            tmpdir,
        } => json!({"free_kb": free_kb, "min_gb": min_gb, "tmpdir": tmpdir}),
        Shortfall::Memory { .. } | Shortfall::Slot { .. } => json!({}),
        Shortfall::Database { server, .. } => json!({ "server": server }),
    };
    if let (Some(target), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        target.extend(extra.clone());
    }
    record
}

/// Whether a delivered task ran past its mission's budget
/// (`{"minutes": n, "tool_calls": n}`, either optional). `active_ms` is the
/// time it could work ([`active_ms`]), never the wall clock.
pub(crate) fn over_budget(budget: &Value, active_ms: i64, tool_calls: i64) -> Option<String> {
    if let Some(minutes) = budget.get("minutes").and_then(Value::as_i64)
        && active_ms > minutes.saturating_mul(60_000)
    {
        return Some(format!(
            "the task worked past its {minutes}-minute budget ({} minutes active)",
            active_ms / 60_000
        ));
    }
    if let Some(limit) = budget.get("tool_calls").and_then(Value::as_i64)
        && tool_calls > limit
    {
        return Some(format!("the task made more than {limit} tool calls"));
    }
    None
}

fn tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Whether a gate `host_fault` record's fields say the fault is not the
/// candidate's doing: the base's alone (the gate's fact with `on` `base`: a
/// scenario's harness error before any candidate code ran), or a check that
/// failed on the base as well as on the candidate (`on` `candidate and
/// base`: main fails it without the candidate). Those never count toward a
/// halt, so the cap neither counts nor caps them ([`HOST_FAULT_CAP`]).
pub(super) fn base_only_fault(fields: &Value) -> bool {
    // Objective 033's evidence is a base that disagreed with itself, before
    // any candidate code; the gate's own fact names `on` `base`; the
    // `cannot_judge` a scenario says of a host that cannot judge it at all
    // (029) is no fault of the work either. A check failing on the base too
    // is main's failure, whatever the candidate does: a red main (Supply PA's
    // check-tests, 2026-11-09 00:48-03:42) would otherwise cap every task's
    // next gate as an abort and feed the abort halt. No one of them counts
    // toward the cap nor is ever capped.
    // A red main (evidence `main_red`, `on` `main`) is main's, not the
    // candidate's, likewise, and so is the gate's own test server that did
    // not start (`on` `test server`, evidence `test_server_never_started`),
    // whichever start it was: no candidate code makes a server start.
    matches!(
        fields.get("evidence").and_then(Value::as_str),
        Some(BASE_DISAGREES | CANNOT_JUDGE | MAIN_RED | LOAD_SENSITIVE | LOAD_SENSITIVE_PARKED)
    ) || matches!(
        fields
            .get("fact")
            .and_then(|fact| fact.get("on"))
            .and_then(Value::as_str),
        Some("base" | "candidate and base" | "main" | "test server")
    )
        // A sandbox that never started in the base's tree or the gate's own
        // (the socket scan): before any candidate code ran, as on the base.
        || fields.get("fact").is_some_and(|fact| {
            fact.get("on").and_then(Value::as_str) == Some("sandbox")
                && fact.get("tree").and_then(Value::as_str) != Some("candidate")
        })
}

/// The `aborted` record of a host fault past the cap: `faults` host faults
/// already at this revision, `cap` allowed; the fault's own record kept under
/// `host_fault` (its log at the top, as an abort's).
pub(super) fn capped_host_fault(
    task: &Task,
    main: &str,
    faults: i64,
    cap: u32,
    mut fault: Value,
) -> Value {
    let log = fault
        .as_object_mut()
        .and_then(|fields| fields.remove("log"))
        .unwrap_or(Value::Null);
    // The fault as it was said, without its promise to judge again.
    let said = fault["reason"]
        .as_str()
        .unwrap_or_default()
        .split("; not a verdict on the work")
        .next()
        .unwrap_or_default()
        .to_string();
    json!({
        "reason": format!(
            "a host fault again: task {} has had {faults} at report revision {} on main {}, the \
             most SPAWN_GATE_HOST_FAULT_CAP allows is {cap}, so this one counts as an abort \
             ({said})",
            task.id,
            task.revisions,
            short_commit(main)
        ),
        "log": log,
        "host_fault": fault,
    })
}

/// The note that sends work back before it is judged, when a merge of main
/// waits for its markers (`pending`) or the branch carries marker blocks in
/// the files it changes (`marked`).
pub(super) fn markers_note(pending: bool, marked: &[String]) -> Option<String> {
    if !pending && marked.is_empty() {
        return None;
    }
    let files = if marked.is_empty() {
        "the files the merge listed".to_string()
    } else {
        marked.join(", ")
    };
    Some(format!(
        "Your work still carries conflict markers from merging the source's main ({files}). \
         Resolve them first: edit the marked regions (<<<<<<< / ======= / >>>>>>>), keeping \
         both sides' intent; do not rewrite the files from memory. Then report again."
    ))
}

/// What a candidate the gate rejected comes to, once its head was judged
/// alone when that can say something ([`rejection`]).
#[derive(Debug)]
pub(super) enum Rejection {
    /// Record this verdict with these fields.
    Record(&'static str, Value),
    /// The head's own run found the host short of room: `no_room`, as any,
    /// with `head` (the fields naming the head and the rejected merge)
    /// added.
    HeadNoRoom {
        short: Shortfall,
        log: String,
        head: Value,
    },
    /// The head's own run found the base disagreeing with itself:
    /// `host_fault` with that evidence, as any, with `head` (the fields
    /// naming the head and the rejected merge) added.
    HeadBaseDisagrees {
        scenario: String,
        setup_s: Option<u64>,
        log: String,
        head: Value,
    },
    /// The head's own run found a host fault (a scenario's harness error on
    /// the base, a check failing on both trees): `host_fault`, as any, with
    /// `head` added.
    HeadHostFault { failure: GateFailure, head: Value },
    /// The head's own run cannot judge the objective here: `host_fault` with
    /// that evidence, as any, with `head` added.
    HeadCannotJudge {
        scenario: String,
        log: String,
        /// The regressions the head's run found load-sensitive, if any.
        load_sensitive: Vec<String>,
        head: Value,
    },
}

/// The record of a judging of the head alone ([`rejection`]) that gave no
/// verdict (no room, a base that disagreed with itself, a host fault, a
/// scenario the host cannot judge): `extra` with the head's fields
/// (`candidate`, `head`, `head_base`) and a reason that says what it covers.
/// Its log and fact are the head's run on `head_base`, the base the work was
/// built on, and never the merge's on `main`: Supply PA's task 207
/// (2026-11-09) recorded the head's run on a red head_base as "check-tests
/// failed on the candidate and on the base" under main's commit, and read as
/// main failing.
pub(super) fn on_the_head(mut extra: Value, head: &Value, main: &str, merged: &str) -> Value {
    let short = |commit: &str| commit.get(..12).unwrap_or(commit).to_string();
    let head_base = head["head_base"]
        .as_str()
        .map_or_else(|| "its base".to_string(), short);
    let said = extra["reason"].as_str().unwrap_or_default().to_string();
    extra["reason"] = json!(format!(
        "the merge with main {} was rejected ({merged}); judging the work alone against the \
         base it was built on, {head_base}, gave no verdict: {said}",
        short(main)
    ));
    if let (Some(target), Some(head)) = (extra.as_object_mut(), head.as_object()) {
        target.extend(head.clone());
    }
    extra
}

/// The merge's rejection, standing: the head alone, judged against
/// `head_base` (the base it was built on), failed `scenario` there on the head
/// and on that base too (`runs`), so that base cannot judge it and the
/// gate's verdict on the merge with main is the one there is. `rejected`, with
/// the merge's reason, log and note, `head_verdict` `unjudged` and a
/// `head_reason` that says why; the note ends by saying it.
fn unjudged_head(
    fields: Value,
    named: &Value,
    head_base: &str,
    scenario: &str,
    runs: &str,
) -> Rejection {
    let short = head_base.get(..12).unwrap_or(head_base);
    let why = format!(
        "the work alone could not be judged against the base it was built on, {short}: \
         {scenario} fails there too ({runs})"
    );
    let mut record = fields;
    if let (Some(target), Some(named)) = (record.as_object_mut(), named.as_object()) {
        target.extend(named.clone());
    }
    let note = record["note"].as_str().unwrap_or_default().to_string();
    record["note"] = json!(format!(
        "{note}\n\nThe work alone could not be judged against the base it was built on \
         ({short}): {scenario} fails there too, so the gate's rejection of the merge with main \
         stands. Fix what fails on the merge."
    ));
    record["head_verdict"] = json!("unjudged");
    record["head_reason"] = json!(why);
    Rejection::Record("rejected", record)
}

/// The tests a rejection names at most; the rest are counted.
const NAMED_FAILURES: usize = 5;

/// The reason a rejection's record gives: the gate's own `reason`, except
/// for check-tests.sh's regression whose output `log` names failing tests
/// ([`host::check_tests_failures`]): then "check-tests.sh failed: <test>"
/// for each of the first [`NAMED_FAILURES`], in the output's order, joined
/// by "; ", then "; and <n> more" for the rest, then the gate's reason in
/// brackets.
fn rejection_reason(reason: &str, log: &str) -> String {
    if !reason.trim().starts_with(host::CHECK_TESTS_REGRESSION) {
        return reason.to_string();
    }
    let failures = host::check_tests_failures(log);
    if failures.is_empty() {
        return reason.to_string();
    }
    let mut named: Vec<String> = failures
        .iter()
        .take(NAMED_FAILURES)
        .map(|failure| format!("check-tests.sh failed: {}", failure.test))
        .collect();
    if failures.len() > NAMED_FAILURES {
        named.push(format!("and {} more", failures.len() - NAMED_FAILURES));
    }
    format!("{} ({})", named.join("; "), reason.trim())
}

/// The record of a gate rejection: the reason (naming check-tests.sh's
/// failing tests, [`rejection_reason`], or another scenario's first failed
/// check, [`checks_reason`]), the gate's output, the note that quotes the
/// reason and the output's tail, and `failed` {scenario, first, count} when
/// the deciding run names a check ([`host::rejected_checks`]).
pub(super) fn rejected_fields(reason: &str, log: &str) -> Value {
    let checks = host::rejected_checks(log);
    let reason = match &checks {
        Some(checks)
            if checks.scenario != host::CHECK_TESTS
                && !reason.trim().starts_with(host::CHECK_TESTS_REGRESSION) =>
        {
            checks_reason(reason, checks)
        }
        _ => rejection_reason(reason, log),
    };
    let mut record = json!({"log": log, "reason": reason, "note": format!(
        "The gate rejected this work: {reason}.\n\nGate output (tail):\n{}",
        tail(log, NOTE_LOG_CHARS))});
    if let Some(checks) = checks {
        record["failed"] = json!({
            "scenario": checks.scenario,
            "first": checks.first,
            "count": checks.count,
        });
    }
    record
}

/// The reason of a rejection whose deciding run (of a scenario other than
/// check-tests.sh) names checks: "<scenario> failed: <first>", then "; and
/// <n> more" for the rest, then the gate's reason in brackets.
fn checks_reason(reason: &str, checks: &host::Checks) -> String {
    let more = match checks.count {
        0 | 1 => String::new(),
        count => format!("; and {} more", count - 1),
    };
    format!(
        "{} failed: {}{more} ({})",
        checks.scenario,
        checks.first,
        reason.trim()
    )
}

/// The main the pushed `head` last took in, `git merge-base <head> <main>`,
/// when the landing candidate is a real merge: `None` when `head` contains
/// `main` (the candidate's tree is the head's, so a rejection of it is a
/// rejection of the head) or the two share no history.
fn head_base(mirror_path: &str, main: &str, head: &str) -> Option<String> {
    let contains = git(&[
        "--git-dir",
        mirror_path,
        "merge-base",
        "--is-ancestor",
        main,
        head,
    ])
    .is_ok();
    if contains {
        return None;
    }
    git(&["--git-dir", mirror_path, "merge-base", head, main])
        .ok()
        .filter(|base| !base.is_empty())
}

/// Main's first-parent commits in `head_base..main`, oldest first:
/// `(full hash, subject)`. The commits a head judged on `head_base` must
/// adapt to.
fn main_commits(
    mirror_path: &str,
    head_base: &str,
    main: &str,
) -> Result<Vec<(String, String)>, String> {
    let listing = git(&[
        "--git-dir",
        mirror_path,
        "log",
        "--first-parent",
        "--reverse",
        "--format=%H %s",
        &format!("{head_base}..{main}"),
    ])?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let (hash, subject) = line.split_once(' ').unwrap_or((line, ""));
            (!hash.is_empty()).then(|| (hash.to_string(), subject.to_string()))
        })
        .collect())
}

/// Main's commits a note lists at most; the record lists them all.
pub(super) const NOTE_COMMITS: usize = 40;

/// Main's commits the note's first line names at most.
const HEADLINE_COMMITS: usize = 5;

/// Characters of the gate output's failure lines the note's first line
/// quotes at most.
const HEADLINE_FAILURE_CHARS: usize = 600;

/// The lines of a gate's output that say what failed, for the first line of
/// a note (a reader that shows one line shows the failure): those that name
/// an error or a failure, the verdict line left out, up to three, joined;
/// without any, the last line before the verdict. Only quoted: no verdict is
/// decided by it.
fn failure_lines(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('{'))
        .collect();
    let failing: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("fail") || lower.contains("panicked")
        })
        .take(3)
        .collect();
    let quoted = if failing.is_empty() {
        lines.last().copied().unwrap_or_default().to_string()
    } else {
        failing.join(" / ")
    };
    cut(&quoted, HEADLINE_FAILURE_CHARS).to_string()
}

/// The note an `incompatible` verdict sends back, from the record alone.
/// Its first line says it all for a reader that shows one line: the work
/// passes on its own base and fails merged with main, main's commits since
/// that base by their 12-digit prefix, the rejection's reason and the gate
/// output's failure lines. Then main's commits with their subjects, what to
/// do (main is merged into the workspace with this revision), and the
/// merge's gate output (its tail, as `rejected` quotes it).
pub(super) fn incompatible_note(
    reason: &str,
    log: &str,
    head_base: &str,
    main: &str,
    commits: &[(String, String)],
) -> String {
    let short = |hash: &str| hash.chars().take(12).collect::<String>();
    let mut named: Vec<String> = commits
        .iter()
        .take(HEADLINE_COMMITS)
        .map(|(hash, _)| short(hash))
        .collect();
    if commits.len() > HEADLINE_COMMITS {
        named.push(format!("{} more", commits.len() - HEADLINE_COMMITS));
    }
    let failure = failure_lines(log);
    let mut note = format!(
        "Main moved under this work: it passes the gate on its own base {} but not merged with \
         main {}, which took in {} since: {reason}{}",
        short(head_base),
        short(main),
        if named.is_empty() {
            "nothing".to_string()
        } else {
            named.join(", ")
        },
        if failure.is_empty() {
            String::new()
        } else {
            format!(": {failure}")
        },
    );
    note.push_str(
        "\n\nThe merge was clean as text; what main's commits changed breaks it. Main's \
         commits since your work's base, oldest first (adapt the work to them):\n",
    );
    for (hash, subject) in commits.iter().take(NOTE_COMMITS) {
        let _ = writeln!(note, "- {} {subject}", short(hash));
    }
    if commits.len() > NOTE_COMMITS {
        let _ = writeln!(
            note,
            "- and {} more (`git log --first-parent {}..{}`)",
            commits.len() - NOTE_COMMITS,
            short(head_base),
            short(main)
        );
    }
    let _ = write!(
        note,
        "\nMain is merged into your workspace with this revision. Adapt the work to those \
         commits (the gate output below names what fails), check it, then report again. \
         This is not counted as a rejection of your work.\n\nGate output on the merge \
         (tail):\n{}",
        tail(log, NOTE_LOG_CHARS)
    );
    note
}

/// The verdict on `candidate`, the landing merge of `main` and the pushed
/// `head`, which the gate rejected (`merged`). When the merge is real (the
/// head does not contain main), the head alone is judged by the same gate
/// (`run_gate(base, candidate)`) against [`head_base`]: it passes, and the
/// work is `incompatible` (sent back with main's commits, not counted); it
/// fails, and the work is `rejected`, naming the head's verdict; its run
/// gives no verdict, and the pass is `aborted` (or `no_room`), naming the
/// head. The decision rests on the two verdicts alone, never on the text
/// of a reason.
pub(super) fn rejection(
    mirror_path: &str,
    main: &str,
    candidate: &str,
    head: &str,
    merged: &gate::Verdict,
    run_gate: &dyn Fn(&str, &str) -> Result<gate::Verdict, GateFailure>,
) -> Result<Rejection, String> {
    let fields = rejected_fields(&merged.reason, &merged.output);
    let Some(head_base) = head_base(mirror_path, main, head) else {
        return Ok(Rejection::Record("rejected", fields));
    };
    let named = json!({"candidate": candidate, "head": head, "head_base": head_base});
    let with = |mut value: Value, extra: &Value| {
        if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        value
    };
    Ok(match run_gate(&head_base, head) {
        Ok(alone) if alone.accepted => {
            let commits = main_commits(mirror_path, &head_base, main)?;
            let note =
                incompatible_note(&merged.reason, &merged.output, &head_base, main, &commits);
            let hashes: Vec<&str> = commits.iter().map(|(hash, _)| hash.as_str()).collect();
            Rejection::Record(
                "incompatible",
                with(
                    named,
                    &json!({"head_verdict": "accepted", "main_commits": hashes,
                            "reason": merged.reason, "log": merged.output, "note": note}),
                ),
            )
        }
        Ok(alone) => Rejection::Record(
            "rejected",
            with(
                with(fields, &named),
                &json!({"head_verdict": "rejected", "head_reason": alone.reason}),
            ),
        ),
        Err(GateFailure::NoRoom { short, log }) => Rejection::HeadNoRoom {
            short,
            log,
            head: named,
        },
        Err(GateFailure::BaseDisagrees {
            scenario,
            setup_s,
            log,
        }) => Rejection::HeadBaseDisagrees {
            scenario,
            setup_s,
            log,
            head: named,
        },
        // The head alone fails a check on its own base too (`candidate and
        // base`, or red there): the base it was built on cannot judge it, so
        // the merge's rejection stands, as the gate gave it on main, with a
        // word that the head could not be judged alone. Never a host fault:
        // retried, it would find that base failing every time (Supply PA's
        // task 207, 2026-11-09, on the red main fcf5e1e).
        Err(GateFailure::HostFault {
            scenario, on, runs, ..
        }) if on == "candidate and base" => {
            unjudged_head(fields, &named, &head_base, &scenario, &runs)
        }
        Err(GateFailure::MainRed { scenario, runs, .. }) => {
            unjudged_head(fields, &named, &head_base, &scenario, &runs)
        }
        Err(failure @ GateFailure::HostFault { .. }) => Rejection::HeadHostFault {
            failure,
            head: named,
        },
        Err(GateFailure::CannotJudge {
            scenario,
            log,
            load_sensitive,
        }) => Rejection::HeadCannotJudge {
            scenario,
            log,
            load_sensitive,
            head: named,
        },
        Err(failure) => {
            let mut record = aborted(&failure);
            if let Some(reason) = record["reason"].as_str() {
                record["reason"] = json!(format!(
                    "judging the head {head} alone against {head_base}, after the gate \
                     rejected the merge with main ({}): {reason}",
                    merged.reason
                ));
            }
            Rejection::Record("aborted", with(record, &named))
        }
    })
}

/// The record of a gate run without a verdict to believe. A gate that never
/// returns, cannot run, or dies (killed, or exiting non-zero, whatever it
/// printed) says nothing about the work: an abort, never an acceptance.
pub(super) fn aborted(failure: &GateFailure) -> Value {
    match failure {
        GateFailure::Timeout => json!({"reason": "the gate exceeded its wall clock"}),
        GateFailure::Reaped => json!({"reason": "the integrator that started the gate reaped it"}),
        GateFailure::Failed(error) => json!({"reason": format!("the gate failed: {error}")}),
        GateFailure::Aborted { reason, log } => {
            json!({"reason": format!("the gate failed: {reason}"), "log": log})
        }
        // Recorded as `no_room` by the caller; counted here only if a
        // caller forgets that.
        GateFailure::NoRoom { short, log } => {
            json!({"reason": format!("the gate had no room: {}", short.line()), "log": log})
        }
        // Recorded as `host_fault` with evidence `base_disagrees` by the
        // caller; counted here only if a caller forgets that.
        GateFailure::BaseDisagrees { scenario, log, .. } => json!({
            "reason": format!(
                "host fault: the base disagrees with itself: {scenario} exited 1, then 0 on the \
                 base; not a verdict, judged again later"
            ),
            "evidence": BASE_DISAGREES,
            "scenario": scenario,
            "log": log,
        }),
        // Recorded as `host_fault` by the caller, likewise.
        GateFailure::HostFault {
            scenario, on, log, ..
        } => {
            json!({"reason": format!("the gate found {scenario} failing on the {on}"), "log": log})
        }
        // Recorded as `host_fault` with evidence `validator_defect` by the
        // caller; counted here only if a caller forgets that.
        GateFailure::ValidatorDefect { defects, log, .. } => json!({
            "reason": format!(
                "validator defect: {} fails on the base too; not a verdict on the work",
                defects.join(", ")
            ),
            "evidence": gate::VALIDATOR_DEFECT,
            "validator_defects": defects,
            "log": log,
        }),
        // Recorded as `host_fault` with evidence `main_red` (or as the
        // host's) by the caller; counted here only if a caller forgets that.
        GateFailure::MainRed { scenario, log, .. } => json!({
            "reason": format!("the gate found main red on {scenario}"),
            "log": log,
        }),
        // Recorded as `host_fault` with evidence `cannot_judge` by the
        // caller; counted here only if a caller forgets that.
        GateFailure::CannotJudge { scenario, log, .. } => json!({
            "reason": format!(
                "host fault: this host cannot judge {scenario}; the objective is judged outside \
                 the gate, by the owner; not a verdict on the work"
            ),
            "evidence": CANNOT_JUDGE,
            "scenario": scenario,
            "log": log,
        }),
    }
}

/// The gate's red-main fact taken as the host's: no failure was the work's,
/// or the test server's log shows a crash in the gate's window (`crash`), or
/// it was about another base than main. A check failing on the candidate and
/// on the base, as the gate says it without the red rule.
pub(super) fn red_as_host_fault(
    scenario: String,
    runs: String,
    crash: &[String],
    log: String,
) -> GateFailure {
    let why = crash.first().map_or_else(
        || {
            "not every shared failure is the work's (a connection or database error is the host's)"
                .to_string()
        },
        |line| format!("the test database's log shows a crash during the gate ({line})"),
    );
    // The base check (SPAWN_GATE_BASE_CHECK) runs before the candidate: no
    // candidate ran, so the fault is the base's alone.
    let (failed, on) = if before_the_candidate(&runs) {
        ("twice on the base, before the candidate", "base")
    } else {
        (
            "twice on the candidate and twice on the base",
            "candidate and base",
        )
    };
    GateFailure::HostFault {
        reason: format!(
            "{scenario} failed {failed} with the same tests, but {why}: a host or harness \
             fault, not a verdict on the candidate"
        ),
        scenario,
        on: on.to_string(),
        runs,
        tree: None,
        log,
    }
}

/// Whether a red's shared failures are main's own: at least one is the
/// work's (an assertion, a panic), and none is a connection or database
/// error, which a broken test server gives main and candidate alike
/// (design-main-red.md; §10.3).
pub(super) fn red_believed(classes: &[host::Class]) -> bool {
    classes.contains(&host::Class::Work)
        && !classes
            .iter()
            .any(|class| matches!(class, host::Class::Connection | host::Class::Database))
}

/// Whether the gate's `runs` are its base check's, run before the
/// candidate (gate.sh's "…, then … on the base, before the candidate").
pub(super) fn before_the_candidate(runs: &str) -> bool {
    runs.ends_with("on the base, before the candidate")
}

/// The task's record of a gate that found main red on `scenario`
/// (`host_fault`, evidence [`MAIN_RED`]): `fact` with `on` `main`, the
/// source, its main and the shared failures; no backoff of its own
/// (`backoff_ms` 0: the source's red holds the gates instead) and no
/// escalation (the owner is told once, of the source's `source.main_red`).
/// The mission's run of host faults is untouched (the store's fold).
pub(crate) fn main_red_fields(
    mission: &Mission,
    source: &str,
    main: &str,
    scenario: &str,
    runs: &str,
    failures: &[Value],
    log: &str,
) -> Value {
    json!({
        "reason": format!(
            "main of {source} ({}) is red on {scenario}: it failed twice on the base ({runs}) \
             with the same failing tests; not a verdict on the work, judged again when main \
             moves",
            main.get(..12).unwrap_or(main)
        ),
        "log": log,
        "evidence": MAIN_RED,
        "fact": {"scenario": scenario, "on": "main", "runs": runs, "source": source,
                 "main": main, "failures": failures},
        "host_faults": mission.gate_host_faults,
        "backoff_ms": 0,
        "escalate": false,
    })
}
