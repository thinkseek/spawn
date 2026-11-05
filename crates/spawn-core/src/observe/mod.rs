//! Observer: turns stored facts into semantic state, in two tiers.
//!
//! 1. [`rules`] decide every state the facts settle. No model is called.
//! 2. Only a case the rules mark [`Ambiguity`] goes to the configured
//!    [`StateJudge`] (Jev in production). Its answer is narrowed to the states
//!    the facts still allow, so a model cannot contradict a fact.
//!
//! The Observer never acts. It appends an `observation.recorded` event when
//! the judgement changes, with who decided, why the rules could not, and the
//! evidence the model saw. A judge failure keeps the rules' guess, marks it
//! `JudgeUnavailable` and asks for attention; it never reports "dead". With
//! no judge configured an ambiguous case goes the same way, its judge named
//! [`NO_JUDGE`].
//!
//! `judge` holds the judge contract, `snapshot` the per-session facts,
//! `classify` reads facts against answers (the rules, and narrowing a model
//! answer), and `observer` is the loop, with what the model is shown.
//!
//! [`Ambiguity`]: crate::observation::Ambiguity

use std::time::Duration;

mod classify;
mod judge;
mod observer;
mod snapshot;

pub use classify::{RulesVerdict, attention_reason, narrow, rules};
pub use judge::{JudgeError, JudgeInput, Judgement, StateJudge, TurnFact};
pub use observer::{NO_JUDGE, ObservationContext, Observer};

/// A live execution with no open tool and no fact for this long is ambiguous.
pub const QUIET_AFTER: Duration = Duration::from_mins(2);

/// A live session is classified again this often when no fact arrives, so a
/// silent hang is noticed.
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);

/// A tool open this long with no progress of the engine (no transcript
/// fact; a busy screen read again is none) is ambiguous: it may hang.
pub const LONG_TOOL: Duration = Duration::from_mins(10);

/// A busy screen reading older than this no longer proves work: the screen is
/// read only when a command runs, so a hang would keep it forever.
pub const SCREEN_STALE: Duration = QUIET_AFTER;

/// First pause before asking an unavailable judge again; doubles per failure.
pub const RETRY_BASE: Duration = Duration::from_secs(30);

/// Longest pause between retries of an unavailable judge.
pub const RETRY_MAX: Duration = Duration::from_mins(10);

/// Without a distribution, below this confidence a model judgement asks for
/// attention.
pub const CONFIDENT: f64 = 0.6;

/// At or above this probability `needs_human` or `stuck` asks for attention.
pub const LIKELY: f64 = 0.5;
