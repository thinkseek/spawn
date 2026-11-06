//! Every definition of the scorecard (docs/design.md §15) on small
//! histories, without a database: the facts are written as the store
//! projects them.

use serde_json::{Value, json};

use super::history::{Command, Facts, History, Kind, Record, stall_of};
use super::measure::{
    Better, Counts, MIN_N, Number, Scope, Window, counts, dimensions, nearest_rank, ratio_milli,
    trend, union_ms,
};
use super::time::{DAY_MS, HOUR_MS, day_of, days, format_rfc3339, parse_rfc3339, parse_span};
use super::transcripts::{Message, read_archive};
use super::{Failure, When, card, text, window};
use crate::deliver::delivery_of;

/// 2026-10-02T00:00:00Z.
fn t0() -> i64 {
    parse_rfc3339("2026-10-02T00:00:00Z").expect("a time")
}

/// `hours` after [`t0`], in ms (minutes as fractions are written as ms).
fn at(hours: i64) -> i64 {
    t0() + hours * HOUR_MS
}

/// A history told record by record.
#[derive(Default)]
struct Story {
    control: Vec<Record>,
    commands: Vec<Command>,
    messages: Vec<Message>,
}

impl Story {
    fn rec(&mut self, at: i64, record: Value) -> &mut Self {
        let mut object = record.as_object().expect("a record").clone();
        object.insert("seq".into(), json!(self.control.len() + 1));
        object.insert("at".into(), json!(at));
        self.control
            .push(serde_json::from_value(Value::Object(object)).expect("a projected record"));
        self
    }

    fn mission(&mut self, id: i64, gate: bool) -> &mut Self {
        self.rec(at(-100), json!({"type": "mission.added", "id": id}));
        if gate {
            self.rec(
                at(-100),
                json!({"type": "mission.configured", "id": id, "gate": "/g", "accept": "gate"}),
            );
        }
        self
    }

    fn assign(&mut self, when: i64, id: i64, mission: i64, objective: Option<i64>, by: &str) -> &mut Self {
        self.rec(
            when,
            json!({"type": "task.assigned", "id": id, "mission_id": mission,
                   "agent_id": format!("agent_{id}"), "session_id": format!("session_{id}"),
                   "objective": objective, "brief": format!("brief {}", objective.unwrap_or(id)),
                   "created_by": by}),
        )
    }

    fn task(&mut self, id: i64, record: &str, when: i64) -> &mut Self {
        self.rec(when, json!({"type": record, "id": id}))
    }

    fn gated(&mut self, when: i64, id: i64, verdict: &str, stage_ms: Option<i64>) -> &mut Self {
        self.rec(
            when,
            json!({"type": "task.gated", "id": id, "verdict": verdict, "stage_ms": stage_ms}),
        )
    }

    fn reviewed(&mut self, when: i64, id: i64, accepted: bool) -> &mut Self {
        self.rec(when, json!({"type": "task.reviewed", "id": id, "accepted": accepted}))
    }

    fn cancelled(&mut self, when: i64, id: i64, by: Option<&str>) -> &mut Self {
        self.rec(when, json!({"type": "task.cancelled", "id": id, "by": by}))
    }

    /// One finished command, one second long.
    #[allow(clippy::too_many_arguments)]
    fn command(
        &mut self,
        principal: &str,
        key: &str,
        session: &str,
        action: &str,
        created: i64,
        succeeded: Option<bool>,
        error: Option<&str>,
        stalls: &[&str],
    ) -> &mut Self {
        let id = i64::try_from(self.commands.len()).expect("few") + 1;
        self.commands.push(Command {
            id,
            principal: principal.to_string(),
            key: key.to_string(),
            session_id: session.to_string(),
            action: Some(action.to_string()),
            created_at_ms: created,
            finished_at_ms: succeeded.map(|_| created + 1000),
            succeeded,
            error: error.map(str::to_string),
            stalls: stalls.iter().map(|tag| (*tag).to_string()).collect(),
        });
        self
    }

    fn message(&mut self, agent: &str, when: i64, model: &str, tokens: i64) -> &mut Self {
        let id = format!("msg_{}", self.messages.len());
        self.messages.push(Message {
            agent: agent.to_string(),
            id,
            at_ms: when,
            model: model.to_string(),
            tokens,
        });
        self
    }

    fn history(&self) -> History {
        let facts = Facts {
            control: self.control.clone(),
            commands: self.commands.clone(),
        };
        History::from_record(&facts, &self.messages)
    }
}

fn window_hours(since: i64, until: i64) -> Window {
    Window {
        since: at(since),
        until: at(until),
    }
}

fn dimension(counts: &Counts, previous: Option<&Counts>, name: &str) -> Value {
    dimensions(counts, previous)[name].clone()
}

/// Three landings of mission 1, one task each, `[h, h + 2]`.
fn three_landings(story: &mut Story, first: i64) {
    for (index, start) in [first, first + 3, first + 6].into_iter().enumerate() {
        let id = 100 + i64::try_from(index).expect("small") + first;
        story
            .assign(at(start), id, 1, None, "spawn_supervisor")
            .task(id, "task.delivered", at(start))
            .task(id, "task.landed", at(start + 2));
    }
}

// --- times ---------------------------------------------------------------------

#[test]
fn rfc3339_times_and_spans_read_as_epoch_milliseconds() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2026-10-02T00:00:00Z"), Some(1_790_899_200_000));
    assert_eq!(
        parse_rfc3339("2026-10-02T02:00:00.123+02:00"),
        Some(1_790_899_200_123)
    );
    assert_eq!(parse_rfc3339("2026-10-02t00:00:00.5z"), Some(1_790_899_200_500));
    assert_eq!(parse_rfc3339("2026-10-01T19:00:00-05:00"), Some(t0()));
    assert_eq!(parse_rfc3339("2026-10-02"), Some(t0()));
    assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z").is_some(), true);
    for bad in [
        "2026-02-29T00:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-10-02T24:00:00Z",
        "2026-10-02T00:00:00",
        "2026-10-02T00:00Z",
        "36h",
        "",
        "2026-10-02T00:00:00.Z",
    ] {
        assert_eq!(parse_rfc3339(bad), None, "{bad}");
    }
    assert_eq!(parse_span("36h"), Some(36 * HOUR_MS));
    assert_eq!(parse_span("7d"), Some(7 * DAY_MS));
    assert_eq!(parse_span("90m"), Some(90 * 60_000));
    assert_eq!(parse_span("2w"), Some(14 * DAY_MS));
    for bad in ["0h", "h", "-3h", "+3h", "3y", "3.5h", ""] {
        assert_eq!(parse_span(bad), None, "{bad}");
    }
    assert_eq!(format_rfc3339(t0()), "2026-10-02T00:00:00Z");
    assert_eq!(format_rfc3339(t0() + 1), "2026-10-02T00:00:00.001Z");
    assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(day_of(t0() - 1), "2026-10-01");
}

#[test]
fn days_are_the_utc_dates_the_window_covers_clipped_to_it() {
    assert_eq!(
        days(at(0), at(48)),
        vec![
            ("2026-10-02".to_string(), at(0), at(24)),
            ("2026-10-03".to_string(), at(24), at(48)),
        ]
    );
    assert_eq!(
        days(at(6), at(30)),
        vec![
            ("2026-10-02".to_string(), at(6), at(24)),
            ("2026-10-03".to_string(), at(24), at(30)),
        ]
    );
    assert_eq!(days(at(1), at(2)), vec![("2026-10-02".to_string(), at(1), at(2))]);
}

#[test]
fn the_window_defaults_to_seven_days_and_a_given_until_fixes_it() {
    let now = at(100);
    assert_eq!(
        window(None, None, now),
        Ok(Window {
            since: now - 7 * DAY_MS,
            until: now
        })
    );
    // A span for --since counts back from the window's end, so with --until
    // given the answer does not depend on when it was asked.
    for asked_at in [at(100), at(5000)] {
        assert_eq!(
            window(Some(When::Ago(36 * HOUR_MS)), Some(When::At(at(48))), asked_at),
            Ok(window_hours(12, 48))
        );
    }
    assert_eq!(
        window(None, Some(When::Ago(DAY_MS)), now),
        Ok(Window {
            since: now - 8 * DAY_MS,
            until: now - DAY_MS
        })
    );
    assert!(matches!(
        window(Some(When::At(at(5))), Some(When::At(at(5))), now),
        Err(Failure::Request(_))
    ));
    assert!(matches!(
        window(Some(When::Ago(401 * DAY_MS)), None, now),
        Err(Failure::Request(_))
    ));
}

// --- arithmetic ------------------------------------------------------------------

#[test]
fn ratios_round_to_three_decimals_half_away_from_zero() {
    assert_eq!(ratio_milli(6, 35), Some(171));
    assert_eq!(ratio_milli(7, 6), Some(1167));
    assert_eq!(ratio_milli(1, 3), Some(333));
    assert_eq!(ratio_milli(2, 3), Some(667));
    assert_eq!(ratio_milli(1, 2000), Some(1), "0.0005 rounds away from zero");
    assert_eq!(ratio_milli(-1, 2000), Some(-1));
    assert_eq!(ratio_milli(999, 2_000_000), Some(0), "0.0004995 rounds down");
    assert_eq!(ratio_milli(16020, 6), Some(2_670_000));
    assert_eq!(ratio_milli(5, 0), None);
    assert_eq!(Number::Milli(171).json(), json!(0.171));
    assert_eq!(Number::Milli(2_670_000).json(), json!(2670.0));
    assert_eq!(Number::Whole(600_000).json(), json!(600_000));
}

#[test]
fn percentiles_are_by_nearest_rank() {
    let six = [2, 4, 5, 6, 7, 8];
    assert_eq!(nearest_rank(&six, 50), Some(5), "rank ceil(3) = 3");
    assert_eq!(nearest_rank(&six, 90), Some(8), "rank ceil(5.4) = 6");
    let four = [4, 6, 7, 8];
    assert_eq!(nearest_rank(&four, 50), Some(6));
    assert_eq!(nearest_rank(&four, 90), Some(8));
    let five = [3, 6, 6, 9, 12];
    assert_eq!(nearest_rank(&five, 50), Some(6), "rank ceil(2.5) = 3");
    let ten: Vec<i64> = (1..=10).collect();
    assert_eq!(nearest_rank(&ten, 90), Some(9), "rank 9 exactly");
    assert_eq!(nearest_rank(&ten, 50), Some(5));
    assert_eq!(nearest_rank(&[42], 90), Some(42));
    assert_eq!(nearest_rank(&[], 50), None);
}

#[test]
fn open_hours_are_the_union_of_the_intervals_clipped_to_the_window() {
    let window = window_hours(0, 48);
    // Overlapping, nested and touching intervals count once.
    let intervals = [
        (at(1), Some(at(7))),
        (at(2), Some(at(10))),
        (at(3), Some(at(4))),
        (at(10), Some(at(12))),
        (at(20), Some(at(24))),
        (at(24), Some(at(27))),
    ];
    assert_eq!(union_ms(&intervals, window), 11 * HOUR_MS + 7 * HOUR_MS);
    // Clipped at both ends; an open interval runs to the window's end.
    let intervals = [(at(-30), Some(at(2))), (at(40), None), (at(-5), Some(at(-1)))];
    assert_eq!(union_ms(&intervals, window), 2 * HOUR_MS + 8 * HOUR_MS);
    assert_eq!(union_ms(&[], window), 0);
    assert_eq!(union_ms(&[(at(50), None)], window), 0);
}

#[test]
fn a_trend_compares_the_rounded_values_by_the_direction() {
    let m = |value| Some(Number::Milli(value));
    assert_eq!(trend(m(171), m(250), Better::Higher), Some("worse"));
    assert_eq!(trend(m(250), m(171), Better::Higher), Some("better"));
    assert_eq!(trend(m(171), m(250), Better::Lower), Some("better"));
    assert_eq!(trend(m(500), m(500), Better::Lower), Some("same"));
    assert_eq!(trend(None, m(500), Better::Lower), None);
    assert_eq!(trend(m(500), None, Better::Lower), None);
    let w = |value| Some(Number::Whole(value));
    assert_eq!(trend(w(600_000), w(900_000), Better::Lower), Some("better"));
}

// --- the dimensions --------------------------------------------------------------

#[test]
fn below_the_minimum_n_a_dimension_has_no_value_and_says_so() {
    let mut story = Story::default();
    story.mission(1, false);
    for id in [1, 2] {
        story
            .assign(at(id), id, 1, None, "spawn_supervisor")
            .task(id, "task.delivered", at(id))
            .gated(at(id + 1), id, "rejected", Some(60_000))
            .reviewed(at(id + 2), id, true);
    }
    story.message("agent_1", at(1) + 1, "m", 500);
    let history = story.history();
    let two = counts(&history, Scope::Fleet, window_hours(0, 24));
    assert_eq!(two.landed, 2);
    let card = dimensions(&two, None);
    for name in [
        "throughput",
        "lead_time",
        "first_pass",
        "rework",
        "autonomy",
        "reliability",
        "cost",
    ] {
        assert_eq!(card[name]["value"], Value::Null, "{name}");
        assert_eq!(card[name]["enough"], json!(false), "{name}");
        assert_eq!(card[name]["n"], json!(2), "{name}");
        assert_eq!(card[name]["previous"], Value::Null, "{name}");
        assert_eq!(card[name]["trend"], Value::Null, "{name}");
    }
    // Per-landing parts are null; counts are not, and every part is there.
    assert_eq!(card["lead_time"]["p90"], Value::Null);
    assert_eq!(card["cost"]["gate_ms_per_landed"], Value::Null);
    assert_eq!(card["cost"]["by_model"]["m"]["per_landed"], Value::Null);
    assert_eq!(card["cost"]["by_model"]["m"]["tokens"], json!(500));
    assert_eq!(card["cost"]["gate_ms"], json!(120_000));
    assert_eq!(card["rework"]["send_backs"], json!(2));
    assert_eq!(card["throughput"]["landed"], json!(2));
    assert_eq!(card["gate_time"]["n"], json!(2));
    assert_eq!(card["gate_time"]["value"], Value::Null);
    assert_eq!(MIN_N, 3);

    // A third landing is enough.
    story
        .assign(at(5), 3, 1, None, "spawn_supervisor")
        .gated(at(6), 3, "rejected", Some(60_000))
        .reviewed(at(7), 3, true);
    let three = counts(&story.history(), Scope::Fleet, window_hours(0, 24));
    let card = dimensions(&three, None);
    assert_eq!(card["throughput"]["enough"], json!(true));
    assert_eq!(card["rework"]["value"], json!(1.0));
    assert_eq!(card["gate_time"]["value"], json!(60_000));
}

#[test]
fn throughput_is_landings_per_open_hour() {
    let mut story = Story::default();
    story.mission(1, true);
    three_landings(&mut story, 0);
    // Open 0-2, 3-5, 6-8 and an open task from 10: 6 + 14 = 20 hours.
    story.assign(at(10), 9, 1, None, "spawn_supervisor");
    let history = story.history();
    let throughput = dimension(&counts(&history, Scope::Fleet, window_hours(0, 24)), None, "throughput");
    assert_eq!(throughput["landed"], json!(3));
    assert_eq!(throughput["open_hours"], json!(20.0));
    assert_eq!(throughput["value"], json!(0.15));
    assert_eq!(throughput["better"], json!("higher"));
}

#[test]
fn lead_time_runs_from_the_objectives_first_try() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, true);
    // Objective 5: task 1 cancelled by a person, re-issued as task 2.
    story
        .assign(at(0), 1, 1, Some(5), "spawn-integrate")
        .cancelled(at(2), 1, None)
        .assign(at(2), 2, 1, Some(5), "spawn-integrate")
        .task(2, "task.landed", at(5));
    // Same objective, another mission: not a try of mission 1's.
    story
        .assign(at(-10), 3, 2, Some(5), "spawn-integrate")
        .cancelled(at(-9), 3, None);
    // Cancelled after task 4's assignment: not an earlier try.
    story
        .assign(at(6), 4, 1, Some(6), "spawn-integrate")
        .assign(at(7), 5, 1, Some(6), "spawn-integrate")
        .cancelled(at(8), 5, None)
        .task(4, "task.landed", at(9));
    // Objective 5 again, after its landing: a new round, from its own
    // assignment, not from task 1's.
    story
        .assign(at(10), 6, 1, Some(5), "spawn-integrate")
        .task(6, "task.landed", at(12));
    // A task without an objective: from its own assignment.
    story
        .assign(at(13), 7, 1, None, "spawn_supervisor")
        .task(7, "task.landed", at(20));
    let history = story.history();
    let leads: Vec<(i64, i64)> = history
        .landings
        .iter()
        .map(|landing| (landing.task, landing.lead_ms / HOUR_MS))
        .collect();
    assert_eq!(leads, vec![(2, 5), (4, 3), (6, 2), (7, 7)]);
    let lead = dimension(&counts(&history, Scope::Mission(1), window_hours(0, 24)), None, "lead_time");
    // 2 3 5 7: median rank 2, p90 rank 4.
    assert_eq!(lead["value"], json!(3 * HOUR_MS));
    assert_eq!(lead["p90"], json!(7 * HOUR_MS));
    assert_eq!(lead["better"], json!("lower"));
}

#[test]
fn first_pass_and_rework_count_send_backs_by_verdict() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false);
    story
        .assign(at(0), 1, 1, None, "spawn-integrate")
        .gated(at(1), 1, "rejected", None)
        .gated(at(2), 1, "conflict", None)
        .gated(at(3), 1, "accepted", None)
        .task(1, "task.landed", at(3));
    story
        .assign(at(0), 2, 1, None, "spawn-integrate")
        .gated(at(4), 2, "incompatible", None)
        .gated(at(5), 2, "accepted", None)
        .task(2, "task.landed", at(5));
    story
        .assign(at(0), 3, 1, None, "spawn-integrate")
        .gated(at(6), 3, "aborted", None)
        .gated(at(6), 3, "stale", None)
        .gated(at(7), 3, "accepted", None)
        .task(3, "task.landed", at(7));
    story
        .assign(at(0), 4, 2, None, "spawn_supervisor")
        .reviewed(at(8), 4, false)
        .reviewed(at(9), 4, true);
    // A send-back after the window counts nowhere in it.
    story
        .assign(at(0), 5, 2, None, "spawn_supervisor")
        .reviewed(at(30), 5, false);
    let fleet = counts(&story.history(), Scope::Fleet, window_hours(0, 24));
    let first = dimension(&fleet, None, "first_pass");
    assert_eq!(first["first_pass"], json!(1), "only task 3 never went back");
    assert_eq!(first["value"], json!(0.25));
    let rework = dimension(&fleet, None, "rework");
    assert_eq!(rework["send_backs"], json!(4));
    assert_eq!(
        rework["by_verdict"],
        json!({"rejected": 1, "conflict": 1, "incompatible": 1, "reviewed": 1})
    );
    assert_eq!(rework["value"], json!(1.0));
}

#[test]
fn a_review_lands_only_on_a_mission_without_a_gate() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false);
    story
        .assign(at(0), 1, 1, None, "spawn_supervisor")
        .reviewed(at(1), 1, true)
        .assign(at(0), 2, 2, None, "spawn_supervisor")
        .reviewed(at(2), 2, true)
        .reviewed(at(3), 2, true);
    let history = story.history();
    let landed: Vec<(i64, i64)> = history
        .landings
        .iter()
        .map(|landing| (landing.task, landing.at))
        .collect();
    assert_eq!(landed, vec![(2, at(2))], "once, at the first acceptance");
}

#[test]
fn autonomy_counts_the_owners_interventions() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false);
    // Owner tasks: a person's, on a mission a review accepts.
    story
        .assign(at(1), 1, 2, None, "human")
        .assign(at(1), 2, 2, None, "spawn_supervisor")
        .assign(at(1), 3, 1, None, "human");
    // Cancel and reassign: by nobody named or a person; not by a loop.
    story
        .assign(at(2), 10, 1, Some(7), "spawn-integrate")
        .cancelled(at(3), 10, None)
        .assign(at(4), 11, 1, Some(7), "spawn-integrate")
        .assign(at(2), 12, 1, Some(8), "spawn-integrate")
        .cancelled(at(3), 12, Some("spawn-dispatch"))
        .assign(at(4), 13, 1, Some(8), "spawn-integrate")
        .assign(at(2), 14, 1, Some(9), "spawn-integrate")
        .cancelled(at(3), 14, Some("spawn_supervisor"))
        .assign(at(4), 15, 1, Some(9), "spawn-integrate");
    // Without an objective, the same brief; another brief is new work.
    story
        .assign(at(5), 16, 2, None, "spawn_supervisor")
        .cancelled(at(6), 16, Some("human"));
    story.rec(
        at(7),
        json!({"type": "task.assigned", "id": 17, "mission_id": 2, "agent_id": "a", "session_id": "s",
               "brief": "brief 16", "created_by": "spawn_supervisor"}),
    );
    story
        .assign(at(5), 18, 2, None, "spawn_supervisor")
        .cancelled(at(6), 18, Some("human"))
        .assign(at(7), 19, 2, None, "spawn_supervisor");
    story.rec(at(8), json!({"type": "mission.resumed", "id": 1}));
    // Commands: a person's and the CLI's count; Spawn's loops' do not.
    story
        .command("human", "board-1", "session_1", "input", at(9), Some(true), None, &[])
        .command("spawn", "cli-1", "elsewhere", "stop", at(9), Some(true), None, &[])
        .command("spawn-dispatch", "deliver-1-0", "session_1", "start", at(9), Some(true), None, &[])
        .command("spawn_supervisor", "s-1", "session_1", "input", at(9), Some(true), None, &[]);
    let history = story.history();
    let autonomy = dimension(&counts(&history, Scope::Fleet, window_hours(0, 24)), None, "autonomy");
    assert_eq!(
        autonomy["by_kind"],
        json!({"command": 2, "resume": 1, "owner_task": 1, "cancel_reassign": 2})
    );
    assert_eq!(autonomy["interventions"], json!(6));
    let mission_1 = dimension(&counts(&history, Scope::Mission(1), window_hours(0, 24)), None, "autonomy");
    // The board's input went to task 1's session (mission 2); the CLI's to
    // no task's: the fleet's only.
    assert_eq!(
        mission_1["by_kind"],
        json!({"command": 0, "resume": 1, "owner_task": 0, "cancel_reassign": 1})
    );
    let cancel_reassign: Vec<i64> = history
        .marks
        .iter()
        .filter(|mark| mark.kind == Kind::CancelReassign)
        .map(|mark| mark.at)
        .collect();
    assert_eq!(cancel_reassign, vec![at(4), at(7)], "counted at the reassignment");
}

#[test]
fn reliability_counts_failures_of_the_machinery() {
    let mut story = Story::default();
    story.mission(1, true);
    story
        .assign(at(1), 6, 1, Some(13), "spawn-integrate")
        .assign(at(1), 8, 1, Some(14), "spawn-integrate")
        .assign(at(1), 9, 1, Some(15), "spawn-integrate")
        .gated(at(3), 8, "aborted", None)
        .gated(at(4), 8, "no_room", None)
        .gated(at(5), 8, "host_fault", None)
        .gated(at(6), 8, "rejected", None);
    let host = "start terminal: new-session failed";
    // Task 6: four failed attempts, nothing typed: spent, at the last finish.
    for (index, key) in ["deliver-6-0", "deliver-6-0-retry-1", "deliver-6-0-retry-2", "deliver-6-0-retry-3"]
        .into_iter()
        .enumerate()
    {
        let minutes = i64::try_from(index).expect("few") * 600_000;
        story.command("spawn-dispatch", key, "session_6", "start", at(2) + minutes, Some(false), Some(host), &[]);
    }
    // Task 8: a refusal for a pane not ready, then delivered: not spent.
    story
        .command("spawn-dispatch", "deliver-8-0", "session_8", "input", at(2), Some(false),
                 Some("pane state Busy does not accept input"), &[])
        .command("spawn-dispatch", "deliver-8-0-retry-1", "session_8", "input", at(2) + 5000, Some(true), None, &[]);
    // A stall tag sent twice counts once, at the first digest; another
    // principal's tags count nothing.
    let tag = "session_9 stall-9-activity-1";
    story
        .command("spawn-dispatch", "digest-a", "session_supervisor", "input", at(7), Some(true), None, &[tag])
        .command("spawn-dispatch", "digest-b", "session_supervisor", "input", at(8), Some(true), None,
                 &[tag, "session_9 stall-9-judgement-4"])
        .command("human", "board", "session_supervisor", "input", at(9), Some(true), None,
                 &["session_9 stall-9-delivery-5"]);
    let history = story.history();
    let reliability = dimension(&counts(&history, Scope::Mission(1), window_hours(0, 24)), None, "reliability");
    assert_eq!(
        reliability["by_kind"],
        json!({"failed_command": 5, "spent_delivery": 1, "stall": 2, "gate_infrastructure": 3})
    );
    assert_eq!(reliability["events"], json!(11));
    let marks = |kind: Kind| -> Vec<i64> {
        history
            .marks
            .iter()
            .filter(|mark| mark.kind == kind)
            .map(|mark| mark.at)
            .collect()
    };
    assert_eq!(marks(Kind::SpentDelivery), vec![at(2) + 1_800_000 + 1000]);
    assert_eq!(marks(Kind::Stall), vec![at(7), at(8)]);
}

#[test]
fn a_delivery_is_spent_only_as_attempts_judges_it() {
    let spent = |attempts: &[(Option<bool>, Option<&str>)]| -> bool {
        let mut story = Story::default();
        story.mission(1, true).assign(at(0), 4, 1, None, "human");
        for (index, (succeeded, error)) in attempts.iter().enumerate() {
            let key = if index == 0 {
                "deliver-4-0".to_string()
            } else {
                format!("deliver-4-0-retry-{index}")
            };
            let created = at(1) + i64::try_from(index).expect("few") * 60_000;
            story.command("spawn-dispatch", &key, "session_4", "input", created, *succeeded, *error, &[]);
        }
        story
            .history()
            .marks
            .iter()
            .any(|mark| mark.kind == Kind::SpentDelivery)
    };
    let failed = (Some(false), Some("host error"));
    let refused = (Some(false), Some("pane state Busy does not accept input"));
    let typed = (
        Some(false),
        Some("the text may have been typed and the log cannot tell; it is not typed again"),
    );
    assert!(spent(&[failed; 4]), "four failures: retries spent");
    assert!(!spent(&[failed; 3]), "three failures: a retry is due");
    assert!(!spent(&[failed, failed, failed, typed]), "may have typed: not spent");
    assert!(!spent(&[failed, failed, failed, (None, None)]), "an attempt still open");
    assert!(!spent(&[failed, failed, failed, (Some(true), None)]), "delivered");
    // Refusals for a pane not ready are free, up to ten.
    let mut free = vec![refused; 10];
    free.extend([failed; 3]);
    assert!(!spent(&free));
    let mut past = vec![refused; 11];
    past.extend([failed; 3]);
    assert!(spent(&past), "the eleventh refusal counts as a failure");
}

#[test]
fn tokens_count_once_per_message_across_lines_and_copies() {
    let root = std::env::temp_dir().join(format!(
        "spawn-scorecard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos())
    ));
    let agent = root.join("agent_resident_r1");
    std::fs::create_dir_all(&agent).expect("archive");
    let line = |time: &str, id: &str, model: &str, usage: [u64; 4]| {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\
             \"content\":[{{\"type\":\"text\",\"text\":\"x\"}}],\"usage\":{{\"input_tokens\":{},\
             \"cache_creation_input_tokens\":{},\"cache_read_input_tokens\":{},\"output_tokens\":{}}}}}}}\n",
            usage[0], usage[1], usage[2], usage[3]
        )
    };
    let native = "0a0a0a0a-0000-4000-8000-000000000001";
    let copy0 = [
        line("2026-10-02T02:00:01.000Z", "msg_a", "opus", [100, 1000, 5000, 400]),
        line("2026-10-02T02:00:00.000Z", "msg_a", "opus", [100, 1000, 5000, 400]),
        "{\"type\":\"user\",\"timestamp\":\"2026-10-02T02:00:00.000Z\",\"message\":{\"role\":\"user\",\"usage\":{}}}\n".to_string(),
        "{\"type\":\"assistant\",\"timestamp\":\"2026-10-02T02:00:02.000Z\",\"message\":{\"id\":\"msg_s\",\"model\":\"<synthetic>\",\"content\":[]}}\n".to_string(),
        line("2026-10-03T07:00:00.000Z", "msg_e", "sonnet", [10, 0, 2000, 90]),
        "not json \"usage\"\n".to_string(),
    ]
    .concat();
    std::fs::write(agent.join(format!("{native}.jsonl")), copy0).expect("copy 0");
    let copy1 = [
        line("2026-10-03T07:00:00.000Z", "msg_e", "sonnet", [10, 0, 2000, 90]),
        line("2026-10-03T17:00:00.000Z", "msg_f", "opus", [50, 0, 1000, 50]),
    ]
    .concat();
    std::fs::write(agent.join(format!("{native}.1.jsonl")), copy1).expect("copy 1");
    std::fs::write(agent.join("notes.txt"), line("2026-10-03T17:00:00.000Z", "msg_x", "opus", [1, 1, 1, 1]))
        .expect("not a copy file");
    let other = root.join("agent_supervisor");
    std::fs::create_dir_all(&other).expect("archive");
    // The same message id under another agent is another message.
    std::fs::write(
        other.join("0a0a0a0a-0000-4000-8000-000000000002.jsonl"),
        line("2026-10-02T22:00:00.000Z", "msg_a", "opus", [10, 0, 0, 90]),
    )
    .expect("supervisor");
    let messages = read_archive(&root, t0()).expect("read the archive");
    let seen: Vec<(&str, &str, i64, &str, i64)> = messages
        .iter()
        .map(|message| {
            (
                message.agent.as_str(),
                message.id.as_str(),
                message.at_ms,
                message.model.as_str(),
                message.tokens,
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("agent_resident_r1", "msg_a", at(2), "opus", 6500),
            ("agent_supervisor", "msg_a", at(22), "opus", 100),
            ("agent_resident_r1", "msg_e", at(31), "sonnet", 2100),
            ("agent_resident_r1", "msg_f", at(41), "opus", 1100),
        ]
    );
    assert_eq!(read_archive(&root.join("missing"), t0()), Ok(Vec::new()));
    std::fs::remove_dir_all(&root).expect("clean up");
}

#[test]
fn a_missions_tokens_are_its_tasks_agents_between_delivery_and_close() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false);
    story
        .assign(at(1), 4, 1, None, "spawn-integrate")
        .task(4, "task.delivered", at(1))
        .task(4, "task.landed", at(7))
        .assign(at(2), 5, 2, None, "human")
        .assign(at(30), 9, 1, None, "spawn-integrate")
        .task(9, "task.delivered", at(30));
    story
        .message("agent_4", at(0), "opus", 1) // before the delivery
        .message("agent_4", at(2), "opus", 6500)
        .message("agent_4", at(7), "opus", 10) // at the close: after it
        .message("agent_5", at(3), "haiku", 20) // never delivered
        .message("agent_9", at(41), "sonnet", 1100) // open: to the window's end
        .message("agent_supervisor", at(22), "opus", 100); // the fleet's only
    let history = story.history();
    let window = window_hours(0, 48);
    let fleet = dimension(&counts(&history, Scope::Fleet, window), None, "cost");
    assert_eq!(fleet["tokens"], json!(7731));
    assert_eq!(
        fleet["by_model"],
        json!({"haiku": {"tokens": 20, "per_landed": null}, "opus": {"tokens": 6611, "per_landed": null},
               "sonnet": {"tokens": 1100, "per_landed": null}})
    );
    let mission_1 = dimension(&counts(&history, Scope::Mission(1), window), None, "cost");
    assert_eq!(mission_1["tokens"], json!(7600));
    let mission_2 = dimension(&counts(&history, Scope::Mission(2), window), None, "cost");
    assert_eq!(mission_2["tokens"], json!(0));
}

#[test]
fn a_command_belongs_to_the_task_its_key_names_else_the_task_open_on_its_session() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false).mission(3, false);
    // One resident session with tasks of three missions.
    for (id, mission, assigned, closed) in [(1, 1, 0, 10), (2, 2, 5, 20), (3, 3, 30, 40)] {
        story.rec(
            at(assigned),
            json!({"type": "task.assigned", "id": id, "mission_id": mission, "agent_id": "agent_r",
                   "session_id": "session_r", "brief": "b", "created_by": "spawn_supervisor"}),
        );
        story.cancelled(at(closed), id, Some("spawn_supervisor"));
    }
    let history = story.history();
    let by_session = {
        let mut map = std::collections::HashMap::new();
        for task in history.tasks.values() {
            map.entry(task.session.as_str())
                .or_insert_with(Vec::new)
                .push(task);
        }
        map
    };
    let mission = |key: &str, session: &str, created: i64| {
        let command = Command {
            key: key.to_string(),
            session_id: session.to_string(),
            created_at_ms: created,
            ..Command::default()
        };
        history.command_mission(&command, &by_session)
    };
    assert_eq!(mission("deliver-3-0-retry-2", "session_r", at(1)), Some(3), "the key's task");
    assert_eq!(mission("board-1", "session_r", at(1)), Some(1));
    assert_eq!(mission("board-2", "session_r", at(6)), Some(1), "the oldest open task");
    assert_eq!(mission("board-3", "session_r", at(10)), Some(2), "task 1 closed at 10");
    assert_eq!(mission("board-4", "session_r", at(25)), None, "no task open: the fleet's");
    assert_eq!(mission("board-5", "session_supervisor", at(1)), None);
    assert_eq!(mission("deliver-99-0", "session_r", at(31)), Some(3), "an unknown task: the session's");
}

#[test]
fn gate_time_is_the_median_wall_clock_of_the_runs_that_name_one() {
    let mut story = Story::default();
    story.mission(1, true);
    story.assign(at(0), 1, 1, None, "spawn-integrate");
    story.rec(at(1), json!({"type": "task.gated", "id": 1, "verdict": "rejected", "gate_ms": 300_000, "stage_ms": 1}));
    story
        .gated(at(2), 1, "conflict", None)
        .gated(at(3), 1, "accepted", Some(900_000))
        .gated(at(4), 1, "aborted", Some(600_000))
        .gated(at(30), 1, "accepted", Some(1));
    let fleet = counts(&story.history(), Scope::Fleet, window_hours(0, 24));
    assert_eq!(fleet.gate_runs, vec![300_000, 600_000, 900_000]);
    let gate = dimension(&fleet, None, "gate_time");
    assert_eq!(gate["value"], json!(600_000));
    assert_eq!(gate["runs"], json!(3));
    assert_eq!(gate["n"], json!(3));
    assert_eq!(dimension(&fleet, None, "cost")["gate_ms"], json!(1_800_000));
}

#[test]
fn keys_and_tags_name_their_task() {
    assert_eq!(delivery_of("deliver-6-0"), Some((6, "deliver-6-0")));
    assert_eq!(delivery_of("deliver-6-2-retry-3"), Some((6, "deliver-6-2")));
    assert_eq!(delivery_of("deliver-6"), None);
    assert_eq!(delivery_of("deliver-x-0"), None);
    assert_eq!(delivery_of("deliver-6-0-retry-"), None);
    assert_eq!(delivery_of("cancel-notice-6-1"), None);
    assert_eq!(
        stall_of("session_resident_r1 stall-9-activity-1"),
        Some(("stall-9-activity-1", 9))
    );
    assert_eq!(stall_of("stall-12-delivery-40"), Some(("stall-12-delivery-40", 12)));
    assert_eq!(stall_of("missions stall-mission-1-halted-5"), None);
    assert_eq!(stall_of("session_x report-12-0"), None);
}

// --- the answer ------------------------------------------------------------------

/// Mission 1 lands three tasks in the window and two before it; mission 2
/// has nothing; mission 3 has a task open in the window.
fn small_card_story() -> Story {
    let mut story = Story::default();
    story.mission(1, true).mission(2, false).mission(3, false);
    three_landings(&mut story, 1);
    story
        .assign(at(-20), 50, 1, None, "spawn-integrate")
        .task(50, "task.landed", at(-18))
        .assign(at(-12), 51, 1, None, "spawn-integrate")
        .task(51, "task.landed", at(-8));
    story.assign(at(20), 60, 3, None, "spawn_supervisor");
    story
}

#[test]
fn the_card_holds_the_fleet_then_each_active_mission_with_previous_and_days() {
    let history = small_card_story().history();
    let card = card(&history, window_hours(0, 48), None).expect("a card");
    assert_eq!(card["since_ms"], json!(at(0)));
    assert_eq!(card["until_ms"], json!(at(48)));
    assert_eq!(card["previous_since_ms"], json!(at(-48)));
    assert_eq!(card["min_n"], json!(3));
    let scopes: Vec<(Value, Value)> = card["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|scope| (scope["scope"].clone(), scope["mission"].clone()))
        .collect();
    assert_eq!(
        scopes,
        vec![
            (json!("fleet"), Value::Null),
            (json!("mission"), json!(1)),
            (json!("mission"), json!(3)),
        ],
        "mission 2 has nothing in either window"
    );
    let fleet = &card["scopes"][0];
    // Previous: two landings, not enough; so no trend.
    assert_eq!(fleet["dimensions"]["throughput"]["previous"], Value::Null);
    assert_eq!(fleet["dimensions"]["throughput"]["trend"], Value::Null);
    assert_eq!(fleet["dimensions"]["lead_time"]["value"], json!(2 * HOUR_MS));
    let days: Vec<&str> = fleet["days"]
        .as_array()
        .expect("days")
        .iter()
        .map(|day| day["day"].as_str().expect("a day"))
        .collect();
    assert_eq!(days, vec!["2026-10-02", "2026-10-03"]);
    let first_day = &fleet["days"][0]["dimensions"];
    assert_eq!(first_day["throughput"]["landed"], json!(3));
    assert_eq!(first_day["throughput"]["previous"], Value::Null);
    assert_eq!(first_day["throughput"]["trend"], Value::Null);
    // Every dimension has every field.
    for name in [
        "throughput",
        "lead_time",
        "first_pass",
        "rework",
        "autonomy",
        "reliability",
        "cost",
        "gate_time",
    ] {
        for field in ["value", "n", "enough", "better", "previous", "trend"] {
            assert!(
                fleet["dimensions"][name].get(field).is_some(),
                "{name} has {field}"
            );
        }
    }
    // The same record gives the same answer.
    assert_eq!(card, super::card(&history, window_hours(0, 48), None).expect("again"));
}

#[test]
fn the_previous_window_gives_previous_and_trend() {
    let mut story = small_card_story();
    story
        .assign(at(-6), 52, 1, None, "spawn-integrate")
        .task(52, "task.landed", at(-5));
    let history = story.history();
    let card = card(&history, window_hours(0, 48), Some(1)).expect("a card");
    let scopes = card["scopes"].as_array().expect("scopes");
    assert_eq!(scopes.len(), 1, "the mission alone");
    let lead = &scopes[0]["dimensions"]["lead_time"];
    // Previous leads 2, 4, 1 hours: median 2; now 2, 2, 2: same.
    assert_eq!(lead["previous"], json!(2 * HOUR_MS));
    assert_eq!(lead["trend"], json!("same"));
    let throughput = &scopes[0]["dimensions"]["throughput"];
    // Previous: 3 landings over 2 + 4 + 1 open hours = 0.429; now 3 over 6.
    assert_eq!(throughput["previous"], json!(0.429));
    assert_eq!(throughput["value"], json!(0.5));
    assert_eq!(throughput["trend"], json!("better"));
}

#[test]
fn a_mission_no_record_names_is_refused() {
    let history = small_card_story().history();
    assert!(matches!(
        card(&history, window_hours(0, 48), Some(7)),
        Err(Failure::Request(_))
    ));
    let alone = card(&history, window_hours(0, 48), Some(2)).expect("mission 2 alone");
    assert_eq!(alone["scopes"][0]["mission"], json!(2));
}

#[test]
fn the_text_form_says_not_enough_data_with_n_and_the_minimum() {
    let history = small_card_story().history();
    let card = card(&history, window_hours(0, 48), None).expect("a card");
    let shown = text(&card);
    assert!(shown.contains("2026-10-02T00:00:00Z to 2026-10-04T00:00:00Z"), "{shown}");
    assert!(shown.contains("mission 3"), "{shown}");
    assert!(
        shown.contains("not enough data (n 0, minimum 3)"),
        "{shown}"
    );
    assert!(shown.contains("throughput   0.5 landings per open hour"), "{shown}");
    assert!(shown.contains("higher is better"), "{shown}");
    assert!(shown.contains("lower is better"), "{shown}");
    // Below the minimum, no lead time is shown as a number.
    let mission_3 = shown.split("mission 3").nth(1).expect("mission 3's part");
    let lead = mission_3
        .lines()
        .find(|line| line.contains("lead"))
        .expect("a lead time line");
    assert!(lead.contains("not enough data"), "{lead}");
    assert!(!shows_a_duration(lead), "{lead}");
    let fleet = shown.split("mission 1").next().expect("the fleet's part");
    let lead = fleet.lines().find(|line| line.contains("lead")).expect("a lead time line");
    assert!(shows_a_duration(lead), "{lead}");
}

/// Whether `line` holds a number followed by a unit of time (`5h`, `3 ms`,
/// `2.5 min`).
fn shows_a_duration(line: &str) -> bool {
    let bytes = line.as_bytes();
    (0..bytes.len()).any(|end| {
        if !bytes[end].is_ascii_digit() || bytes.get(end + 1).is_some_and(u8::is_ascii_digit) {
            return false;
        }
        let rest = line[end + 1..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        rest.starts_with('h') || rest.starts_with("ms") || rest.starts_with("min")
    })
}

#[test]
fn clipping_the_history_to_both_windows_changes_no_number() {
    let mut story = small_card_story();
    story
        .assign(at(-200), 70, 1, Some(3), "spawn-integrate")
        .cancelled(at(-199), 70, None)
        .assign(at(-150), 71, 1, Some(3), "spawn-integrate")
        .task(71, "task.landed", at(-149))
        .assign(at(30), 72, 1, Some(3), "spawn-integrate")
        .task(72, "task.landed", at(31))
        .message("agent_72", at(30), "opus", 7)
        .message("agent_supervisor", at(-300), "opus", 9)
        .command("human", "board-9", "session_72", "input", at(-250), Some(false), None, &[]);
    let whole = story.history();
    let mut clipped = story.history();
    clipped.clip(at(-48), at(48));
    assert!(clipped.tasks.len() < whole.tasks.len());
    assert_eq!(
        card(&clipped, window_hours(0, 48), None),
        card(&whole, window_hours(0, 48), None)
    );
}
