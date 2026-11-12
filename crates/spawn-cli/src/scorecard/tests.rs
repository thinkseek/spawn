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
use super::transcripts::{Message, Parsed, Reach, read_archive, read_archive_parsed};
use super::{Failure, When, card, reach_tokens, text, window};
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
        let Value::Object(mut object) = record else {
            panic!("a record is an object: {record}");
        };
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

    fn assign(
        &mut self,
        when: i64,
        id: i64,
        mission: i64,
        objective: Option<i64>,
        by: &str,
    ) -> &mut Self {
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
        self.rec(
            when,
            json!({"type": "task.reviewed", "id": id, "accepted": accepted}),
        )
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
            intent: None,
            intent_at_ms: None,
        });
        self
    }

    /// A command that succeeded.
    fn ok(&mut self, principal: &str, key: &str, session: &str, created: i64) -> &mut Self {
        self.command(
            principal,
            key,
            session,
            "input",
            created,
            Some(true),
            None,
            &[],
        )
    }

    /// A dispatcher's command that failed with `error`.
    fn failed(&mut self, key: &str, session: &str, created: i64, error: &str) -> &mut Self {
        self.command(
            "spawn-dispatch",
            key,
            session,
            "input",
            created,
            Some(false),
            Some(error),
            &[],
        )
    }

    /// A digest to the supervisor carrying `tags`.
    fn digest(&mut self, principal: &str, created: i64, tags: &[&str]) -> &mut Self {
        let key = format!("digest-{created}");
        self.command(
            principal,
            &key,
            "session_supervisor",
            "input",
            created,
            Some(true),
            None,
            tags,
        )
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
            owner_runs_from_ms: None,
            live_from_ms: None,
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
    assert_eq!(
        parse_rfc3339("2026-10-02T00:00:00Z"),
        Some(1_790_899_200_000)
    );
    assert_eq!(
        parse_rfc3339("2026-10-02T02:00:00.123+02:00"),
        Some(1_790_899_200_123)
    );
    assert_eq!(
        parse_rfc3339("2026-10-02t00:00:00.5z"),
        Some(1_790_899_200_500)
    );
    assert_eq!(parse_rfc3339("2026-10-01T19:00:00-05:00"), Some(t0()));
    assert_eq!(parse_rfc3339("2026-10-02"), Some(t0()));
    assert!(parse_rfc3339("2024-02-29T00:00:00Z").is_some());
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
    assert_eq!(
        days(at(1), at(2)),
        vec![("2026-10-02".to_string(), at(1), at(2))]
    );
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
            window(
                Some(When::Ago(36 * HOUR_MS)),
                Some(When::At(at(48))),
                asked_at
            ),
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
    assert_eq!(
        ratio_milli(1, 2000),
        Some(1),
        "0.0005 rounds away from zero"
    );
    assert_eq!(ratio_milli(-1, 2000), Some(-1));
    assert_eq!(
        ratio_milli(999, 2_000_000),
        Some(0),
        "0.0004995 rounds down"
    );
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
    let intervals = [
        (at(-30), Some(at(2))),
        (at(40), None),
        (at(-5), Some(at(-1))),
    ];
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
    let throughput = dimension(
        &counts(&history, Scope::Fleet, window_hours(0, 24)),
        None,
        "throughput",
    );
    assert_eq!(throughput["landed"], json!(3));
    assert_eq!(throughput["open_hours"], json!(20.0));
    assert_eq!(throughput["value"], json!(0.15));
    assert_eq!(throughput["better"], json!("higher"));
}

#[test]
fn over_budget_closes_a_task_and_no_later_record_lands_it() {
    let mut story = Story::default();
    story.mission(1, true);
    // Open 0-4, closed over budget; its landing after that changes nothing,
    // and it is no person's cancellation (the next try is no intervention).
    story
        .assign(at(0), 7, 1, Some(3), "spawn_supervisor")
        .task(7, "task.over_budget", at(4))
        .task(7, "task.landed", at(6))
        .assign(at(8), 8, 1, Some(3), "spawn_supervisor");
    let history = story.history();
    let counts = counts(&history, Scope::Fleet, window_hours(0, 10));
    assert_eq!(counts.landed, 0);
    assert_eq!(counts.open_ms, 6 * HOUR_MS, "0-4 and 8-10");
    assert_eq!(
        dimension(&counts, None, "autonomy")["interventions"],
        json!(0)
    );
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
    let lead = dimension(
        &counts(&history, Scope::Mission(1), window_hours(0, 24)),
        None,
        "lead_time",
    );
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
    for (first, objective, by) in [
        (10, 7, None),
        (12, 8, Some("spawn-dispatch")),
        (14, 9, Some("spawn_supervisor")),
    ] {
        story
            .assign(at(2), first, 1, Some(objective), "spawn-integrate")
            .cancelled(at(3), first, by)
            .assign(at(4), first + 1, 1, Some(objective), "spawn-integrate");
    }
    // Without an objective, the same brief; another brief is new work.
    story
        .assign(at(5), 16, 2, None, "spawn_supervisor")
        .cancelled(at(6), 16, Some("human"));
    story.rec(
        at(7),
        json!({"type": "task.assigned", "id": 17, "mission_id": 2, "agent_id": "a",
               "session_id": "s", "brief": "brief 16", "created_by": "spawn_supervisor"}),
    );
    story
        .assign(at(5), 18, 2, None, "spawn_supervisor")
        .cancelled(at(6), 18, Some("human"))
        .assign(at(7), 19, 2, None, "spawn_supervisor");
    story.rec(at(8), json!({"type": "mission.resumed", "id": 1}));
    // Commands: a person's and the CLI's count; Spawn's loops' do not.
    story
        .ok("human", "board-1", "session_1", at(9))
        .ok("spawn", "cli-1", "elsewhere", at(9))
        .ok("spawn-dispatch", "deliver-1-0", "session_1", at(9))
        .ok("spawn_supervisor", "s-1", "session_1", at(9));
    let history = story.history();
    let autonomy = dimension(
        &counts(&history, Scope::Fleet, window_hours(0, 24)),
        None,
        "autonomy",
    );
    assert_eq!(
        autonomy["by_kind"],
        json!({"command": 2, "resume": 1, "owner_task": 1, "cancel_reassign": 2})
    );
    assert_eq!(autonomy["interventions"], json!(6));
    let mission_1 = dimension(
        &counts(&history, Scope::Mission(1), window_hours(0, 24)),
        None,
        "autonomy",
    );
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
    assert_eq!(
        cancel_reassign,
        vec![at(4), at(7)],
        "counted at the reassignment"
    );
}

/// Objective 049: a person's command sent again after a refusal continues
/// its intent (as the store names it): one intervention, at the intent's
/// time, and the commands beyond its first are `retries`.
#[test]
fn autonomy_counts_a_persons_intent_once_and_its_retries_apart() {
    let mut story = Story::default();
    story.mission(1, true);
    story
        .command("human", "board-1", "session_1", "input", at(9), Some(false), None, &[])
        .command("human", "board-2", "session_1", "input", at(10), Some(false), None, &[])
        .ok("human", "board-3", "session_1", at(11))
        .ok("spawn", "cli-1", "session_1", at(12))
        // An intent begun before the window: its retry in it counts nowhere.
        .command("human", "board-0", "session_1", "input", at(1), Some(false), None, &[])
        .ok("human", "board-0b", "session_1", at(4));
    // As the store names them: commands 1-3 one intent (1's), 5-6 another.
    for command in &mut story.commands {
        let (intent, at_ms) = match command.id {
            1..=3 => (1, at(9)),
            5 | 6 => (5, at(1)),
            id => (id, command.created_at_ms),
        };
        command.intent = Some(intent);
        command.intent_at_ms = Some(at_ms);
    }
    let history = story.history();
    let autonomy = dimension(
        &counts(&history, Scope::Fleet, window_hours(3, 24)),
        None,
        "autonomy",
    );
    assert_eq!(
        autonomy["by_kind"]["command"],
        json!(2),
        "two intents in the window"
    );
    assert_eq!(autonomy["interventions"], json!(2));
    assert_eq!(autonomy["retries"], json!(2), "board-2 and board-3");
    let wider = dimension(
        &counts(&history, Scope::Fleet, window_hours(0, 24)),
        None,
        "autonomy",
    );
    assert_eq!(wider["by_kind"]["command"], json!(3));
    assert_eq!(wider["retries"], json!(3));
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
    // Task 6: four failed attempts, nothing typed: spent, at the last finish.
    let host = "start terminal: new-session failed";
    story
        .failed("deliver-6-0", "session_6", at(2), host)
        .failed("deliver-6-0-retry-1", "session_6", at(2) + 600_000, host)
        .failed("deliver-6-0-retry-2", "session_6", at(2) + 1_200_000, host)
        .failed("deliver-6-0-retry-3", "session_6", at(2) + 1_800_000, host);
    // Task 8: a refusal for a pane not ready, then delivered: not spent.
    story
        .failed(
            "deliver-8-0",
            "session_8",
            at(2),
            "pane state Busy does not accept input",
        )
        .ok(
            "spawn-dispatch",
            "deliver-8-0-retry-1",
            "session_8",
            at(2) + 5000,
        );
    // A stall tag sent twice counts once, at the first digest; another
    // principal's tags count nothing.
    let tag = "session_9 stall-9-activity-1";
    story
        .digest("spawn-dispatch", at(7), &[tag])
        .digest(
            "spawn-dispatch",
            at(8),
            &[tag, "session_9 stall-9-judgement-4"],
        )
        .digest("human", at(9), &["session_9 stall-9-delivery-5"]);
    let history = story.history();
    let reliability = dimension(
        &counts(&history, Scope::Mission(1), window_hours(0, 24)),
        None,
        "reliability",
    );
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

/// GitHub #16 item 3: a failure is a fact of the moment the command
/// finished, so a window that ended before it does not hold it, and the
/// window it finished in does.
#[test]
fn a_failed_command_counts_in_the_window_it_finished_in() {
    let mut story = Story::default();
    story.mission(1, true);
    story.assign(at(1), 6, 1, Some(13), "spawn-integrate");
    // Created half a second before hour 24, finished half a second after.
    story.failed("dispatch-resume-1", "session_6", at(24) - 500, "lost");
    let history = story.history();
    let failed = |since: i64, until: i64| {
        dimension(
            &counts(&history, Scope::Mission(1), window_hours(since, until)),
            None,
            "reliability",
        )["by_kind"]["failed_command"]
            .clone()
    };
    assert_eq!(failed(0, 24), json!(0), "not yet failed in this window");
    assert_eq!(failed(24, 48), json!(1), "failed in this one");
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
            story.command(
                "spawn-dispatch",
                &key,
                "session_4",
                "input",
                created,
                *succeeded,
                *error,
                &[],
            );
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
    assert!(
        !spent(&[failed, failed, failed, typed]),
        "may have typed: not spent"
    );
    assert!(
        !spent(&[failed, failed, failed, (None, None)]),
        "an attempt still open"
    );
    assert!(
        !spent(&[failed, failed, failed, (Some(true), None)]),
        "delivered"
    );
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
        line(
            "2026-10-03T07:00:00.000Z",
            "msg_e",
            "sonnet",
            [10, 0, 2000, 90],
        ),
        line(
            "2026-10-03T17:00:00.000Z",
            "msg_f",
            "opus",
            [50, 0, 1000, 50],
        ),
    ]
    .concat();
    std::fs::write(agent.join(format!("{native}.1.jsonl")), copy1).expect("copy 1");
    std::fs::write(
        agent.join("notes.txt"),
        line("2026-10-03T17:00:00.000Z", "msg_x", "opus", [1, 1, 1, 1]),
    )
    .expect("not a copy file");
    let other = root.join("agent_supervisor");
    std::fs::create_dir_all(&other).expect("archive");
    // The same message id under another agent is another message.
    std::fs::write(
        other.join("0a0a0a0a-0000-4000-8000-000000000002.jsonl"),
        line("2026-10-02T22:00:00.000Z", "msg_a", "opus", [10, 0, 0, 90]),
    )
    .expect("supervisor");
    let (messages, _) = read_archive(&root, t0()).expect("read the archive");
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
    assert_eq!(
        read_archive(&root.join("missing"), t0()).map(|(messages, _)| messages),
        Ok(Vec::new())
    );
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
    assert_eq!(
        mission("deliver-3-0-retry-2", "session_r", at(1)),
        Some(3),
        "the key's task"
    );
    assert_eq!(mission("board-1", "session_r", at(1)), Some(1));
    assert_eq!(
        mission("board-2", "session_r", at(6)),
        Some(1),
        "the oldest open task"
    );
    assert_eq!(
        mission("board-3", "session_r", at(10)),
        Some(2),
        "task 1 closed at 10"
    );
    assert_eq!(
        mission("board-4", "session_r", at(25)),
        None,
        "no task open: the fleet's"
    );
    assert_eq!(mission("board-5", "session_supervisor", at(1)), None);
    assert_eq!(
        mission("deliver-99-0", "session_r", at(31)),
        Some(3),
        "an unknown task: the session's"
    );
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

/// Objective 067: the gate time the window's runs spent, and what of it
/// decided nothing (aborted, no_room, host_fault, stale), for the fleet and
/// each mission; a record with no gate time, or a run outside the window,
/// adds nothing.
#[test]
fn gate_time_says_how_much_of_it_decided_nothing() {
    let mut story = Story::default();
    story.mission(1, true).mission(2, true);
    for task in 1..=8 {
        story.assign(
            at(0),
            task,
            if task <= 6 { 1 } else { 2 },
            None,
            "spawn-integrate",
        );
    }
    story
        .gated(at(1), 1, "accepted", Some(600_000))
        .gated(at(1), 2, "rejected", Some(900_000))
        .gated(at(2), 3, "conflict", Some(300_000))
        .gated(at(2), 4, "aborted", Some(1_200_000))
        .gated(at(3), 5, "host_fault", Some(480_000))
        // Stale as the integrator writes it when main moved under the gate.
        .rec(
            at(3),
            json!({"type": "task.gated", "id": 6, "verdict": "stale", "found": "accepted",
                   "main_now": "m", "revision": 0, "gate_ms": 420_000}),
        )
        // A stale with no gate run behind it (discarded): nothing.
        .rec(
            at(4),
            json!({"type": "task.gated", "id": 6, "verdict": "stale", "found": "discarded"}),
        )
        .gated(at(4), 7, "no_room", Some(60_000))
        .gated(at(5), 8, "incompatible", Some(240_000))
        // Outside the window.
        .gated(at(30), 7, "aborted", Some(9_000_000))
        .gated(at(-2), 8, "host_fault", Some(9_000_000));
    let history = story.history();
    let parts = |scope: Scope| {
        let gate = dimension(
            &counts(&history, scope, window_hours(0, 24)),
            None,
            "gate_time",
        );
        json!({"runs": gate["runs"], "total_ms": gate["total_ms"],
               "undecided_ms": gate["undecided_ms"], "undecided_runs": gate["undecided_runs"]})
    };
    assert_eq!(
        parts(Scope::Fleet),
        json!({"runs": 8, "total_ms": 4_200_000, "undecided_ms": 2_160_000, "undecided_runs": 4})
    );
    assert_eq!(
        parts(Scope::Mission(1)),
        json!({"runs": 6, "total_ms": 3_900_000, "undecided_ms": 2_100_000, "undecided_runs": 3}),
        "65 min, 35 of them aborted, host_fault and stale"
    );
    assert_eq!(
        parts(Scope::Mission(2)),
        json!({"runs": 2, "total_ms": 300_000, "undecided_ms": 60_000, "undecided_runs": 1})
    );
    // No run: zeros, not missing.
    assert_eq!(
        parts(Scope::Mission(3)),
        json!({"runs": 0, "total_ms": 0, "undecided_ms": 0, "undecided_runs": 0})
    );
}

/// Objective 081: a landing's wait from its task's last report before it
/// landed to the first deploy that succeeded at or after it, for the fleet
/// and each mission; a failed deploy ships nothing, a re-report restarts
/// the wait, a landing with no deploy since is not live, and one whose task
/// never reported counts in neither.
#[test]
fn report_to_live_runs_from_the_last_report_to_the_deploy_that_ships_it() {
    let minutes = |m: i64| at(0) + m * 60_000;
    let deployed = |story: &mut Story, m: i64, ok: bool| {
        story.rec(
            minutes(m),
            json!({"type": "deploy.finished", "ok": ok, "head": "h"}),
        );
    };
    let mut story = Story::default();
    story.mission(1, true).mission(2, false);
    // The live record began long before (objective 095), with a deploy that
    // failed: every landing here counts, and it ships nothing.
    deployed(&mut story, -6000, false);
    for task in 1..=6 {
        story.assign(
            minutes(60),
            task,
            if task == 6 { 2 } else { 1 },
            None,
            "spawn-integrate",
        );
    }
    // T1: reported 120, landed 150, deploy at 180: 60 min.
    story
        .task(1, "task.reported", minutes(120))
        .task(1, "task.landed", minutes(150));
    deployed(&mut story, 180, true);
    // T2: reported 210, sent back, reported again 240, landed 276; a failed
    // deploy at 300; the next ok at 360: 120 min from its last report.
    story
        .task(2, "task.reported", minutes(210))
        .gated(minutes(222), 2, "rejected", None)
        .task(2, "task.reported", minutes(240))
        .task(2, "task.landed", minutes(276));
    deployed(&mut story, 300, false);
    deployed(&mut story, 360, true);
    // T3 and T4 shipped by one deploy at 468 (48 and 36 min); T6, a review
    // mission's accepted report, by the same (78 min).
    story
        .task(3, "task.reported", minutes(420))
        .task(4, "task.reported", minutes(432))
        .task(6, "task.reported", minutes(390))
        .reviewed(minutes(400), 6, true)
        .task(3, "task.landed", minutes(450))
        .task(4, "task.landed", minutes(468));
    // A deploy that finished as T4 landed ships it (at or after).
    deployed(&mut story, 468, true);
    // T5 landed with no report recorded: in neither figure.
    story.task(5, "task.landed", minutes(500));
    // T7: reported, landed, no deploy since: not live.
    story.assign(minutes(60), 7, 1, None, "spawn-integrate");
    story
        .task(7, "task.reported", minutes(600))
        .task(7, "task.landed", minutes(630));
    let history = story.history();
    let rtl = |scope: Scope| {
        let d = dimension(
            &counts(&history, scope, window_hours(0, 24)),
            None,
            "report_to_live",
        );
        json!({"value": d["value"], "p90": d["p90"], "n": d["n"], "not_live": d["not_live"],
               "enough": d["enough"], "better": d["better"]})
    };
    // Fleet live waits: 60, 120, 48 (T3: 420 -> 468), 36 (T4: 432 -> 468),
    // 78 (T6: 390 -> 468); sorted 36, 48, 60, 78, 120.
    assert_eq!(
        rtl(Scope::Fleet),
        json!({"value": 3_600_000, "p90": 7_200_000, "n": 5, "not_live": 1,
               "enough": true, "better": "lower"})
    );
    // Mission 1: 36, 48, 60, 120.
    assert_eq!(
        rtl(Scope::Mission(1)),
        json!({"value": 2_880_000, "p90": 7_200_000, "n": 4, "not_live": 1,
               "enough": true, "better": "lower"})
    );
    // Mission 2: one live landing, below the minimum: no value.
    assert_eq!(
        rtl(Scope::Mission(2)),
        json!({"value": null, "p90": null, "n": 1, "not_live": 0,
               "enough": false, "better": "lower"})
    );
}

/// Objective 095: report to live counts only the landings the live record
/// can see. The record begins at the first `deploy.finished` of any outcome
/// (a failed one too); a landing before it is neither live nor not live,
/// while its other dimensions are unchanged. `measured_from_ms` is the later
/// of the window's start and the record's, `covered` whether the record
/// starts at or before the window; with no deploy record nothing counts.
#[test]
fn report_to_live_counts_only_landings_from_the_live_records_start() {
    let minutes = |m: i64| at(0) + m * 60_000;
    let story_with = |first_ok: bool| {
        let mut story = Story::default();
        story.mission(1, true);
        for task in 1..=5 {
            story.assign(minutes(0), task, 1, None, "spawn-integrate");
        }
        // T1 lands before the record begins (at 120).
        story
            .task(1, "task.reported", minutes(60))
            .task(1, "task.landed", minutes(100));
        story.rec(
            minutes(120),
            json!({"type": "deploy.finished", "ok": first_ok, "head": "h"}),
        );
        // T2, T3 and T4 land after it; the deploy at 300 ships them.
        story
            .task(2, "task.reported", minutes(130))
            .task(2, "task.landed", minutes(140))
            .task(3, "task.reported", minutes(200))
            .task(3, "task.landed", minutes(210))
            .task(4, "task.reported", minutes(240))
            .task(4, "task.landed", minutes(270));
        story.rec(
            minutes(300),
            json!({"type": "deploy.finished", "ok": true, "head": "h"}),
        );
        // T5: landed after the last deploy, not live.
        story
            .task(5, "task.reported", minutes(320))
            .task(5, "task.landed", minutes(330));
        story
    };
    let rtl = |history: &History, window: Window| {
        let counts = counts(history, Scope::Fleet, window);
        let d = dimension(&counts, None, "report_to_live");
        json!({"value": d["value"], "n": d["n"], "not_live": d["not_live"],
               "measured_from_ms": d["measured_from_ms"], "covered": d["covered"],
               "lead_n": dimension(&counts, None, "lead_time")["n"]})
    };
    // A window from before the record: T1 is left out of report to live
    // (and in lead time). Waits 170 (T2), 100 (T3), 60 (T4); median 100.
    // The first record is a failed deploy: it begins the record all the
    // same, and T1 is still not shipped by it.
    for first_ok in [false, true] {
        let history = story_with(first_ok).history();
        assert_eq!(history.live_from, Some(minutes(120)));
        assert_eq!(
            rtl(&history, window_hours(0, 24)),
            json!({"value": 6_000_000, "n": 3, "not_live": 1,
                   "measured_from_ms": minutes(120), "covered": false, "lead_n": 5}),
            "first deploy ok: {first_ok}"
        );
    }
    let history = story_with(false).history();
    // A window that starts at the record's start: covered.
    assert_eq!(
        rtl(
            &history,
            Window {
                since: minutes(120),
                until: at(24)
            }
        )["covered"],
        json!(true)
    );
    // A window starting after the record: covered, measured from its own start.
    assert_eq!(
        rtl(
            &history,
            Window {
                since: minutes(135),
                until: at(24)
            }
        ),
        json!({"value": 6_000_000, "n": 3, "not_live": 1,
               "measured_from_ms": minutes(135), "covered": true, "lead_n": 4})
    );
    // The record's start read from the facts over the whole log stands even
    // when the folded records begin later.
    let mut story = story_with(false);
    let facts = Facts {
        control: story.control.clone(),
        commands: Vec::new(),
        owner_runs_from_ms: None,
        live_from_ms: Some(minutes(-60)),
    };
    let read = History::from_record(&facts, &[]);
    assert_eq!(read.live_from, Some(minutes(-60)));
    assert_eq!(rtl(&read, window_hours(0, 24))["n"], json!(4));
    assert_eq!(rtl(&read, window_hours(0, 24))["covered"], json!(true));
    // No deploy record at all: no landing counts in either part.
    let mut bare = Story::default();
    bare.mission(1, true);
    bare.assign(minutes(0), 1, 1, None, "spawn-integrate")
        .task(1, "task.reported", minutes(60))
        .task(1, "task.landed", minutes(100));
    story = bare;
    let history = story.history();
    assert_eq!(history.live_from, None);
    assert_eq!(
        rtl(&history, window_hours(0, 24)),
        json!({"value": null, "n": 0, "not_live": 0,
               "measured_from_ms": null, "covered": false, "lead_n": 1})
    );
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
    assert_eq!(
        stall_of("stall-12-delivery-40"),
        Some(("stall-12-delivery-40", 12))
    );
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
    assert_eq!(
        fleet["dimensions"]["lead_time"]["value"],
        json!(2 * HOUR_MS)
    );
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
    assert_eq!(
        card,
        super::card(&history, window_hours(0, 48), None).expect("again")
    );
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
    assert!(
        shown.contains("2026-10-02T00:00:00Z to 2026-10-04T00:00:00Z"),
        "{shown}"
    );
    assert!(shown.contains("mission 3"), "{shown}");
    assert!(
        shown.contains("not enough data (n 0, minimum 3)"),
        "{shown}"
    );
    assert!(
        shown.contains("throughput   0.5 landings per open hour"),
        "{shown}"
    );
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
    let lead = fleet
        .lines()
        .find(|line| line.contains("lead"))
        .expect("a lead time line");
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
        .command(
            "human",
            "board-9",
            "session_72",
            "input",
            at(-250),
            Some(false),
            None,
            &[],
        );
    let whole = story.history();
    let mut clipped = story.history();
    clipped.clip(at(-48), at(48));
    assert!(clipped.tasks.len() < whole.tasks.len());
    assert_eq!(
        card(&clipped, window_hours(0, 48), None),
        card(&whole, window_hours(0, 48), None)
    );
}

/// Objective 47 (b): the card says whether its window is still settling,
/// and the text form says so on its header line.
#[test]
fn a_settling_window_says_so_in_its_json_and_on_its_header_line() {
    let history = small_card_story().history();
    let mut settled = card(&history, window_hours(0, 48), None).expect("a card");
    super::settle(&mut settled, false);
    assert_eq!(settled["settling"], serde_json::Value::Bool(false));
    let header = |card: &serde_json::Value| text(card).lines().next().map(str::to_string);
    assert!(
        !header(&settled).unwrap_or_default().contains("settling"),
        "{:?}",
        header(&settled)
    );
    let mut open = card(&history, window_hours(0, 48), None).expect("a card");
    super::settle(&mut open, true);
    assert_eq!(open["settling"], serde_json::Value::Bool(true));
    assert!(
        header(&open).unwrap_or_default().contains("settling"),
        "{:?}",
        header(&open)
    );
    // The figures are the same: settling says only that they may grow.
    let mut figures = open.clone();
    figures["settling"] = serde_json::Value::Bool(false);
    assert_eq!(figures, settled);
}

/// The archive says how far back its tokens reach: from its oldest message
/// when every file was read, past the time asked from when it holds a file
/// last written more than a day before it, and from nothing when it holds
/// no message.
#[test]
fn the_archive_says_how_far_back_its_tokens_reach() {
    let root = std::env::temp_dir().join(format!(
        "spawn-scorecard-reach-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos())
    ));
    let agent = root.join("agent_w1");
    std::fs::create_dir_all(&agent).expect("archive");
    let line = |time: &str, id: &str| {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\
             \"model\":\"opus\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n"
        )
    };
    assert_eq!(
        read_archive(&root.join("missing"), t0()),
        Ok((Vec::new(), Reach::From(None)))
    );
    assert_eq!(
        read_archive(&root, t0()).map(|(_, reach)| reach),
        Ok(Reach::From(None)),
        "an archive with no message"
    );
    std::fs::write(
        agent.join("0a0a0a0a-0000-4000-8000-000000000001.jsonl"),
        [
            line("2026-10-02T05:00:00.000Z", "msg_b"),
            line("2026-10-02T03:00:00.000Z", "msg_a"),
        ]
        .concat(),
    )
    .expect("a transcript");
    assert_eq!(
        read_archive(&root, t0()).map(|(_, reach)| reach),
        Ok(Reach::From(Some(at(3)))),
        "every file read: the oldest message"
    );
    // A file last written two days before the time asked from holds lines
    // from before it: the archive reaches back past it, and that file is
    // not read.
    let old = agent.join("0a0a0a0a-0000-4000-8000-000000000002.jsonl");
    std::fs::write(&old, line("2026-09-29T00:00:00.000Z", "msg_old")).expect("an old one");
    let written = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(
            u64::try_from(t0() - 2 * DAY_MS).expect("after the epoch"),
        );
    std::fs::File::options()
        .write(true)
        .open(&old)
        .and_then(|file| file.set_modified(written))
        .expect("age the file");
    let (messages, reach) = read_archive(&root, t0()).expect("read the archive");
    assert_eq!(reach, Reach::Before);
    assert_eq!(
        messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["msg_a", "msg_b"]
    );
    // Asked from before it, the old file is read and is the oldest.
    assert_eq!(
        read_archive(&root, t0() - 4 * DAY_MS).map(|(_, reach)| reach),
        Ok(Reach::From(Some(t0() - 3 * DAY_MS)))
    );
    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A window that starts before the archive's oldest model call says so in
/// its JSON and on its header line, and so does one whose previous window
/// alone starts before it; the figures are the same either way.
#[test]
fn a_window_before_the_archive_says_so_in_its_json_and_on_its_header_line() {
    let history = small_card_story().history();
    let window = window_hours(24, 48);
    let header = |card: &Value| text(card).lines().next().unwrap_or_default().to_string();
    let read = |reach: Reach| {
        let mut card = card(&history, window, None).expect("a card");
        reach_tokens(&mut card, reach, window.since, window.since - DAY_MS);
        card
    };
    let whole = read(Reach::Before);
    assert_eq!(whole["tokens_short"], Value::Null);
    assert_eq!(whole["tokens_from_ms"], Value::Null);
    assert!(!header(&whole).contains("tokens"), "{}", header(&whole));
    // The archive's oldest call at or before the previous window's start.
    for oldest in [at(-1), at(0)] {
        let card = read(Reach::From(Some(oldest)));
        assert_eq!(card["tokens_short"], Value::Null, "{oldest}");
        assert_eq!(card, whole);
    }
    let previous = read(Reach::From(Some(at(5))));
    assert_eq!(previous["tokens_short"], json!("previous"));
    assert_eq!(previous["tokens_from_ms"], json!(at(5)));
    assert!(
        header(&previous).contains(&format!(
            "tokens: the transcript archive begins at {}, after the previous window's start",
            format_rfc3339(at(5))
        )),
        "{}",
        header(&previous)
    );
    // The window's own start, exactly, is still covered.
    assert_eq!(
        read(Reach::From(Some(at(24))))["tokens_short"],
        json!("previous")
    );
    let short = read(Reach::From(Some(at(30))));
    assert_eq!(short["tokens_short"], json!("window"));
    assert_eq!(short["tokens_from_ms"], json!(at(30)));
    assert!(
        header(&short).contains(&format!(
            "tokens: the transcript archive begins at {}, after this window's start: \
             tokens before it are not counted",
            format_rfc3339(at(30))
        )),
        "{}",
        header(&short)
    );
    let none = read(Reach::From(None));
    assert_eq!(none["tokens_short"], json!("window"));
    assert_eq!(none["tokens_from_ms"], Value::Null);
    assert!(
        header(&none).contains("the transcript archive holds no model call"),
        "{}",
        header(&none)
    );
    // The figures are the same: the fields say only where the tokens begin.
    for card in [previous, short, none] {
        let mut figures = card.clone();
        figures["tokens_short"] = Value::Null;
        figures["tokens_from_ms"] = Value::Null;
        assert_eq!(figures, whole);
    }
}

/// Two gate-accepting missions on `source`, and a task's gating on each:
/// 10 min on mission 1, 6.7 min on mission 2.
fn record_two_missions_with_a_task_gating(
    record: &mut dyn FnMut(&str, Value) -> i64,
    source: &str,
) {
    for mission in [1, 2] {
        record(
            "spawn_control_record",
            json!({"type": "mission.added", "id": mission, "text": "m", "origin": "human",
                   "sources": [source]}),
        );
        record(
            "spawn_control_record",
            json!({"type": "mission.configured", "id": mission, "accept": "gate", "gate": "/g"}),
        );
    }
    for (task, mission, ms) in [(1, 1, 600_000), (2, 2, 400_000)] {
        record(
            "spawn_control_record",
            json!({"type": "task.assigned", "id": task, "mission_id": mission, "objective": task,
                   "kind": "ephemeral", "created_by": "spawn-integrate",
                   "agent_id": format!("agent_w_{task}"), "session_id": format!("session_w_{task}"),
                   "brief": "b"}),
        );
        record(
            "spawn_control_record",
            json!({"type": "task.reported", "id": task, "commit": "c"}),
        );
        record(
            "spawn_control_record",
            json!({"type": "task.gated", "id": task, "verdict": "rejected", "revision": 0,
                   "base": "b", "gate_ms": ms}),
        );
    }
}

/// Objective 061, through the real store: the owner's judgings that name
/// their gate's wall clock (`gate_ms`) count in gate time beside the tasks'
/// gatings, for the fleet and for the mission whose gate judged them
/// (`mission`), never for the missions the landing moved; one from before
/// (no `gate_ms`) counts nothing.
#[test]
fn gate_time_counts_the_owners_judgings_for_the_fleet_and_their_mission() {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let schema = spawn_postgres::testing::TestSchema::new("gate_owner");
    let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
        .expect("connect and migrate");
    let root =
        std::env::temp_dir().join(format!("spawn-gate-owner-{}-{nanos:x}", std::process::id()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let mut client =
            postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
        let mut record = |function: &str, value: Value| -> i64 {
            client
                .query_one(&format!("select {function}($1)"), &[&value])
                .expect("a record")
                .get(0)
        };
        let source = "/srv/source.git";
        record_two_missions_with_a_task_gating(&mut record, source);
        let base = "a".repeat(40);
        let candidate = "b".repeat(40);
        let mut request = |commit: &str| {
            record(
                "spawn_control_record",
                json!({"type": "source.push_requested", "source": source, "commit": commit,
                       "ref": format!("refs/spawn-owner/{commit}"), "by": "human", "gated": true}),
            )
        };
        let accepted = request(&candidate);
        let refused = request(&base);
        let before = request(&candidate);
        // Judged by mission 1's gate: accepted (5 min), refused (8.3 min);
        // and one recorded before the gate's wall clock was (no `gate_ms`).
        record(
            "spawn_integrator_record",
            json!({"type": "source.moved", "source": source, "from": base, "to": candidate,
                   "request": accepted, "by": "human", "verdict": "accepted", "mission": 1,
                   "gate_ms": 300_000}),
        );
        record(
            "spawn_integrator_record",
            json!({"type": "source.push_refused", "request": refused, "reason": "rejected",
                   "verdict": "rejected", "mission": 1, "gate_ms": 500_000}),
        );
        record(
            "spawn_integrator_record",
            json!({"type": "source.moved", "source": source, "from": candidate, "to": base,
                   "request": before, "by": "human", "verdict": "accepted"}),
        );
        let now = store.database_now_ms().expect("now");
        let card = super::measure(
            &store,
            Some(&root),
            &super::Query {
                mission: None,
                since: Some(format_rfc3339(now - HOUR_MS)),
                until: Some(format_rfc3339(now + 60_000)),
            },
        )
        .expect("the scorecard");
        let gate = |scope: &str, mission: Value| {
            let found = card["scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|entry| entry["scope"] == scope && entry["mission"] == mission)
                .unwrap_or_else(|| panic!("no {scope} {mission} scope: {card}"));
            let d = &found["dimensions"]["gate_time"];
            json!({"value": d["value"], "n": d["n"], "runs": d["runs"],
                   "task_runs": d["task_runs"], "owner_runs": d["owner_runs"]})
        };
        assert_eq!(
            gate("fleet", Value::Null),
            json!({"value": 400_000, "n": 4, "runs": 4, "task_runs": 2, "owner_runs": 2}),
            "the median of 5, 6.7, 8.3 and 10 min: nearest rank"
        );
        assert_eq!(
            gate("mission", json!(1)),
            json!({"value": 500_000, "n": 3, "runs": 3, "task_runs": 1, "owner_runs": 2})
        );
        assert_eq!(
            gate("mission", json!(2)),
            json!({"value": null, "n": 1, "runs": 1, "task_runs": 1, "owner_runs": 0}),
            "the landings moved mission 2 too; its gate judged none of them"
        );
    }));
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// An owner's gated landing request for `commit` on `source`, as `spawn
/// mission push` records it: its id.
fn owner_request(record: &mut dyn FnMut(&str, Value) -> i64, source: &str, commit: &str) -> i64 {
    record(
        "spawn_control_record",
        json!({"type": "source.push_requested", "source": source, "commit": commit,
               "ref": format!("refs/spawn-owner/{commit}"), "by": "human", "gated": true}),
    )
}

/// When the latest record of `kind` was recorded, read as the owner.
fn recorded_at(owner: &str, kind: &str) -> i64 {
    postgres::Client::connect(owner, postgres::NoTls)
        .expect("the owner connects")
        .query_one(
            "select max(recorded_at_ms) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = $1",
            &[&kind],
        )
        .expect("the record's time")
        .get(0)
}

/// Objective 076, through the real store: `owner_runs_from_ms` is the time
/// of the earliest owner judging that names its gate's wall clock (a
/// landing recorded without one before it is not), whatever the window;
/// null while none does.
#[test]
fn the_card_says_from_when_the_owners_judgings_are_counted() {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let schema = spawn_postgres::testing::TestSchema::new("owner_from");
    let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
        .expect("connect and migrate");
    let root =
        std::env::temp_dir().join(format!("spawn-owner-from-{}-{nanos:x}", std::process::id()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let mut client =
            postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
        let mut record = |function: &str, value: Value| -> i64 {
            client
                .query_one(&format!("select {function}($1)"), &[&value])
                .expect("a record")
                .get(0)
        };
        let read = |since: i64, until: Option<i64>| {
            super::measure(
                &store,
                Some(&root),
                &super::Query {
                    mission: None,
                    since: Some(format_rfc3339(since)),
                    until: until.map(format_rfc3339),
                },
            )
            .expect("the scorecard")["owner_runs_from_ms"]
                .clone()
        };
        let source = "/srv/source.git";
        let (base, candidate) = ("a".repeat(40), "b".repeat(40));
        record_two_missions_with_a_task_gating(&mut record, source);
        let before = owner_request(&mut record, source, &candidate);
        // Recorded before the gate's wall clock was: not counted.
        record(
            "spawn_integrator_record",
            json!({"type": "source.moved", "source": source, "from": base, "to": candidate,
                   "request": before, "by": "human", "verdict": "accepted"}),
        );
        let now = store.database_now_ms().expect("now");
        assert_eq!(
            read(now - HOUR_MS, None),
            Value::Null,
            "none names a gate time"
        );
        let refused = owner_request(&mut record, source, &"c".repeat(40));
        record(
            "spawn_integrator_record",
            json!({"type": "source.push_refused", "request": refused, "reason": "rejected",
                   "verdict": "rejected", "mission": 1, "gate_ms": 403_000}),
        );
        let at = recorded_at(&owner, "source.push_refused");
        let landed = owner_request(&mut record, source, &"d".repeat(40));
        record(
            "spawn_integrator_record",
            json!({"type": "source.moved", "source": source, "from": base, "to": "d".repeat(40),
                   "request": landed, "by": "human", "verdict": "accepted", "mission": 1,
                   "gate_ms": 300_000}),
        );
        let now = store.database_now_ms().expect("now");
        assert_eq!(
            read(now - HOUR_MS, None),
            json!(at),
            "the refusal: the first counted"
        );
        assert_eq!(
            read(now + 1, Some(now + HOUR_MS)),
            json!(at),
            "a later window"
        );
        assert_eq!(read(at, None), json!(at), "a window from it");
        assert_eq!(
            read(now - 2 * HOUR_MS, Some(now - HOUR_MS)),
            json!(at),
            "a window that ends before it"
        );
    }));
    let _ = std::fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// The archive as a first reading in this process reads it: copied to a
/// directory of its own (another archive, so nothing of it is kept), read,
/// removed.
fn read_fresh(root: &std::path::Path, copy: &str) -> Vec<Message> {
    let fresh = root.with_extension(copy);
    for agent in std::fs::read_dir(root).expect("the archive") {
        let agent = agent.expect("an agent").path();
        let to = fresh.join(agent.file_name().expect("a name"));
        std::fs::create_dir_all(&to).expect("a copy");
        for file in std::fs::read_dir(&agent).expect("an agent's files") {
            let file = file.expect("a file").path();
            std::fs::copy(&file, to.join(file.file_name().expect("a name"))).expect("copy");
        }
    }
    let (messages, _, parsed) = read_archive_parsed(&fresh, t0()).expect("read the copy");
    assert!(
        parsed.whole_files > 0,
        "a first reading reads from the start"
    );
    std::fs::remove_dir_all(&fresh).expect("clean up");
    messages
}

/// A scratch archive with one agent's transcript file in it: the root, the
/// file, and a transcript line of a message (at `time`, under `model`, with
/// `tokens` input tokens and one output token).
fn kept_archive(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "spawn-scorecard-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos())
    ));
    let agent = root.join("agent_w1");
    std::fs::create_dir_all(&agent).expect("archive");
    let file = agent.join("0a0a0a0a-0000-4000-8000-000000000001.jsonl");
    (root, file)
}

fn kept_line(time: &str, id: &str, model: &str, tokens: u64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"message\":{{\"id\":\"{id}\",\
         \"model\":\"{model}\",\"usage\":{{\"input_tokens\":{tokens},\"output_tokens\":1}}}}}}\n"
    )
}

fn append_to(file: &std::path::Path, text: &str) {
    use std::io::Write;
    std::fs::File::options()
        .append(true)
        .open(file)
        .and_then(|mut handle| handle.write_all(text.as_bytes()))
        .expect("append");
}

fn length_of(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).expect("a file").len()
}

/// A file read again parses only what was added to it, and gives what a
/// first reading gives (a line being written included). Counted (files read
/// from the start, bytes parsed), not timed.
#[test]
fn a_file_read_again_parses_only_what_was_added_to_it() {
    let (root, file) = kept_archive("kept-grown");
    let line = kept_line;
    let append = |text: &str| append_to(&file, text);
    let length = length_of;
    let first = [
        line("2026-10-02T03:00:00.000Z", "msg_a", "opus", 10),
        line("2026-10-02T04:00:00.000Z", "msg_b", "opus", 20),
    ]
    .concat();
    std::fs::write(&file, &first).expect("a transcript");
    let read = || read_archive_parsed(&root, t0()).expect("read the archive");

    let (messages, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 1,
            bytes: length(&file)
        }
    );
    assert_eq!(messages, read_fresh(&root, "fresh-1"));
    assert_eq!(messages.len(), 2);

    // Unchanged: nothing parsed, the same messages.
    let (again, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed::default(),
        "an unchanged file parses nothing"
    );
    assert_eq!(again, messages);

    // Grown, with a line of a message it holds (earlier, more tokens): only
    // the added lines are parsed, and they count as a first reading counts
    // them.
    let added = [
        line("2026-10-02T02:00:00.000Z", "msg_b", "sonnet", 50),
        line("2026-10-02T05:00:00.000Z", "msg_c", "opus", 30),
    ]
    .concat();
    append(&added);
    let (grown, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 0,
            bytes: added.len() as u64
        }
    );
    assert_eq!(grown, read_fresh(&root, "fresh-2"));
    assert_eq!(grown.len(), 3);
    assert!(
        grown
            .iter()
            .any(|m| m.id == "msg_b" && m.at_ms == at(2) && m.model == "sonnet" && m.tokens == 51),
        "{grown:?}"
    );

    // A line being written (no newline yet) counts as a first reading
    // counts it, and is parsed again once it is whole.
    let next = line("2026-10-02T06:00:00.000Z", "msg_d", "opus", 40);
    let (part, rest) = next.split_at(next.len() - 1);
    append(part);
    let (partial, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 0,
            bytes: part.len() as u64
        }
    );
    assert_eq!(partial, read_fresh(&root, "fresh-3"));
    append(rest);
    let (whole, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 0,
            bytes: next.len() as u64
        }
    );
    assert_eq!(whole, read_fresh(&root, "fresh-4"));
    assert_eq!(whole.len(), 4);

    std::fs::remove_dir_all(&root).expect("clean up");
}

/// A file shorter than read, with other first bytes, or of the same length
/// at another time is read from its start; a file gone is no longer kept.
#[test]
fn a_file_not_the_one_read_is_read_from_its_start() {
    let (root, file) = kept_archive("kept-replaced");
    let line = kept_line;
    let length = length_of;
    let read = || read_archive_parsed(&root, t0()).expect("read the archive");
    std::fs::write(
        &file,
        [
            line("2026-10-02T03:00:00.000Z", "msg_a", "opus", 10),
            line("2026-10-02T04:00:00.000Z", "msg_b", "opus", 20),
        ]
        .concat(),
    )
    .expect("a transcript");
    let (messages, _, parsed) = read();
    assert_eq!((messages.len(), parsed.whole_files), (2, 1));

    // Shorter than read (truncated, rotated): from its start.
    std::fs::write(&file, line("2026-10-02T07:00:00.000Z", "msg_e", "opus", 1)).expect("truncate");
    let (short, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 1,
            bytes: length(&file)
        }
    );
    assert_eq!(short, read_fresh(&root, "fresh-5"));
    assert_eq!(short.len(), 1);

    // Written in its place, longer, with other first bytes: from its start.
    // Its last line lies past the first bytes kept.
    let mut replaced: String = (0..40)
        .map(|n| {
            line(
                "2026-10-02T08:00:00.000Z",
                &format!("msg_f{n:02}"),
                "opus",
                1,
            )
        })
        .collect();
    replaced.push_str(&line("2026-10-02T09:00:00.000Z", "msg_g", "opus", 1));
    assert!(replaced.len() > 4096 + 200);
    std::fs::write(&file, &replaced).expect("replace");
    let (other, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 1,
            bytes: length(&file)
        }
    );
    assert_eq!(other, read_fresh(&root, "fresh-6"));
    assert!(other.iter().all(|m| m.id != "msg_e"), "{other:?}");

    // The same length and first bytes at another time (a line past them
    // rewritten): from its start.
    let same_length = replaced.replace("msg_g", "msg_h");
    assert_eq!(same_length.len(), replaced.len());
    std::fs::write(&file, &same_length).expect("rewrite");
    std::fs::File::options()
        .write(true)
        .open(&file)
        .and_then(|handle| {
            handle.set_modified(std::time::SystemTime::now() + std::time::Duration::from_hours(1))
        })
        .expect("another time");
    let (rewritten, _, parsed) = read();
    assert_eq!(
        parsed,
        Parsed {
            whole_files: 1,
            bytes: length(&file)
        }
    );
    assert_eq!(rewritten, read_fresh(&root, "fresh-7"));
    assert!(rewritten.iter().any(|m| m.id == "msg_h"), "{rewritten:?}");

    // A file gone is no longer read nor kept.
    std::fs::remove_file(&file).expect("remove");
    let (none, _, parsed) = read();
    assert_eq!((none.len(), parsed), (0, Parsed::default()));
    std::fs::remove_dir_all(&root).expect("clean up");
}
