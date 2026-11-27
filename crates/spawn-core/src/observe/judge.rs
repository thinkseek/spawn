//! The judge contract: what a judge is shown and what it answers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::{EndCause, SessionSeq};
use crate::observation::{Ambiguity, Confidence, SemanticState};
use crate::screen::ScreenState;

/// Judge failure.
#[derive(Debug, Error)]
pub enum JudgeError {
    /// The judge could not produce a judgement.
    #[error("judge unavailable: {0}")]
    Unavailable(String),
}

/// Facts one judgement uses. The shape is the judge's input contract.
///
/// It is stored as the evidence of every model judgement, and every field
/// defaults, so a row written before a field existed still reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeInput {
    /// Owning agent.
    pub agent_id: String,
    /// Judged session.
    pub session_id: String,
    /// Engine of the latest execution, as its record names it.
    pub engine: Option<String>,
    /// Where the latest execution is in its life, written as its four wire
    /// fields ([`ExecutionPhase`]).
    #[serde(flatten)]
    pub phase: ExecutionPhase,
    /// Number of tool calls still open.
    pub open_tools: usize,
    /// Number of background jobs the running execution started that have
    /// not ended (`job.started` without its `job.ended`): work the engine
    /// wakes the agent for when it reports. Left out when none.
    #[serde(skip_serializing_if = "is_zero")]
    pub open_jobs: usize,
    /// Milliseconds since the oldest of those jobs started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_job_ms: Option<u64>,
    /// Tool that started last.
    pub last_tool: Option<String>,
    /// Whether a permission dialog is open.
    pub permission_pending: bool,
    /// Latest state read from the engine pane.
    pub screen: Option<ScreenState>,
    /// State of the latest turn.
    pub turn: Option<TurnFact>,
    /// Latest engine error, truncated.
    pub last_error: Option<String>,
    /// Evidence sequence of the snapshot.
    pub evidence_seq: SessionSeq,
    /// Why the rules could not decide; set only for the model judge.
    pub ambiguity: Option<Ambiguity>,
    /// Milliseconds since the session's latest fact.
    pub quiet_ms: Option<u64>,
    /// Milliseconds since the engine last moved: its latest transcript fact
    /// (a tool, a message, a turn), or the execution's start. Screen
    /// readings and commands are not progress: a spinner redrawn over a
    /// hung tool keeps refreshing the screen, never this.
    pub progress_ms: Option<u64>,
    /// The agent's task, as it was typed into the agent.
    pub task: Option<String>,
    /// Tail of the agent's terminal, as a person would see it.
    pub screen_tail: Option<String>,
    /// The model's latest reply: its own text since the last prompt, from the
    /// transcript. Where a question for a person is read (`HUMAN:`); the
    /// question itself is kept on the observation, so this stays out of the
    /// stored evidence and of what the judge is shown.
    #[serde(skip)]
    pub reply: Option<String>,
    /// The session is meant never to end (the supervisor): it waits for
    /// instructions rather than completing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub perpetual: bool,
}

/// Where the session's latest execution is in its life, as the log tells:
/// `execution.started` makes it [`Self::Running`], `execution.end_stalled`
/// of the running execution [`Self::EndStalled`], and `execution.ended`
/// [`Self::Ended`]; the next start makes it running again.
///
/// On the wire it keeps the four fields judge evidence has always carried
/// (`execution_active`, `stalled_end` left out when none, `last_end`,
/// `exit_code`), so stored evidence, the model's request and stored
/// fingerprints read the same. A combination no phase writes is rejected;
/// one that leaves a field out reads as that field's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionPhase {
    /// No execution has started in the facts folded so far.
    #[default]
    NeverRan,
    /// An execution is running.
    Running,
    /// The running execution's end keeps failing (its final checkpoint, or
    /// the kill of its runner): its workspace stays mounted until the end
    /// gets through or someone discards it.
    EndStalled(EndCause),
    /// The latest execution ended.
    Ended {
        /// How it ended.
        cause: EndCause,
        /// Its exit status, when its process reported one.
        exit_code: Option<i32>,
    },
}

impl ExecutionPhase {
    /// Whether the execution still holds its terminal and workspace:
    /// running, or with its end stalled.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::EndStalled(_))
    }

    /// The four wire fields of this phase.
    pub(super) fn wire(self) -> PhaseWire {
        let (execution_active, stalled_end, last_end, exit_code) = match self {
            Self::NeverRan => (false, None, None, None),
            Self::Running => (true, None, None, None),
            Self::EndStalled(cause) => (true, Some(cause), None, None),
            Self::Ended { cause, exit_code } => (false, None, Some(cause), exit_code),
        };
        PhaseWire {
            execution_active,
            stalled_end,
            last_end,
            exit_code,
        }
    }
}

/// [`ExecutionPhase`] as judge evidence writes it. Every field defaults, so
/// evidence written before a field existed still reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct PhaseWire {
    /// Whether an execution is running.
    pub(super) execution_active: bool,
    /// How the running execution was ending when its end stalled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stalled_end: Option<EndCause>,
    /// How the latest execution ended.
    pub(super) last_end: Option<EndCause>,
    /// Exit status of the latest execution, when its process reported one.
    pub(super) exit_code: Option<i32>,
}

impl PhaseWire {
    /// The phase these fields write, or `None` for a combination no phase
    /// writes: a stalled end of no running execution, an end while one runs,
    /// an exit status with no end.
    pub(super) fn phase(self) -> Option<ExecutionPhase> {
        match (
            self.execution_active,
            self.stalled_end,
            self.last_end,
            self.exit_code,
        ) {
            (false, None, None, None) => Some(ExecutionPhase::NeverRan),
            (true, None, None, None) => Some(ExecutionPhase::Running),
            (true, Some(cause), None, None) => Some(ExecutionPhase::EndStalled(cause)),
            (false, None, Some(cause), exit_code) => {
                Some(ExecutionPhase::Ended { cause, exit_code })
            }
            _ => None,
        }
    }
}

impl Serialize for ExecutionPhase {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.wire().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExecutionPhase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = PhaseWire::deserialize(deserializer)?;
        wire.phase().ok_or_else(|| {
            serde::de::Error::custom(format!(
                "no execution phase writes execution_active={}, stalled_end={:?}, \
                 last_end={:?}, exit_code={:?}",
                wire.execution_active, wire.stalled_end, wire.last_end, wire.exit_code
            ))
        })
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// How the latest turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFact {
    /// The model finished a turn.
    Stopped,
    /// The turn failed.
    Failed,
}

/// One judgement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Judgement {
    /// Judged state.
    pub state: SemanticState,
    /// Judge confidence.
    pub confidence: Confidence,
    /// Distribution over [`SemanticState::ALL`], in that order, when the
    /// judge gives one.
    pub probabilities: Option<[f64; 5]>,
    /// Probability that a human must answer or decide.
    pub needs_human: Option<f64>,
    /// Probability that the agent loops or makes no progress.
    pub stuck: Option<f64>,
}

impl Judgement {
    /// A judgement with a state and a confidence only.
    pub fn of(state: SemanticState, confidence: Confidence) -> Self {
        Self {
            state,
            confidence,
            probabilities: None,
            needs_human: None,
            stuck: None,
        }
    }
}

/// Component that classifies a snapshot.
pub trait StateJudge: Send + Sync {
    /// Judge name stored in the observation.
    fn name(&self) -> &'static str;

    /// Classify one snapshot.
    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError>;
}

#[cfg(test)]
mod tests {
    use super::{ExecutionPhase, JudgeInput, PhaseWire};
    use crate::event::{EndCause, SessionSeq};

    const CAUSES: [EndCause; 3] = [EndCause::Stopped, EndCause::Exited, EndCause::Lost];

    /// Every phase, with each end cause and exit status kind.
    fn phases() -> Vec<ExecutionPhase> {
        let mut all = vec![ExecutionPhase::NeverRan, ExecutionPhase::Running];
        for cause in CAUSES {
            all.push(ExecutionPhase::EndStalled(cause));
            for exit_code in [None, Some(0), Some(1), Some(-9)] {
                all.push(ExecutionPhase::Ended { cause, exit_code });
            }
        }
        all
    }

    fn input(phase: ExecutionPhase) -> JudgeInput {
        JudgeInput {
            agent_id: "agent_a".into(),
            session_id: "session_a".into(),
            engine: Some("claude".into()),
            phase,
            open_tools: 1,
            evidence_seq: SessionSeq(7),
            ..JudgeInput::default()
        }
    }

    /// The JSON the four loose fields wrote before the phase was one value
    /// (captured from 69d618e5): the phase writes it byte for byte, in the
    /// same place, and reads it back.
    #[test]
    fn the_phase_writes_the_evidence_the_loose_fields_wrote() {
        let tail = r#""open_tools":1,"last_tool":null,"permission_pending":false,"screen":null,"turn":null,"last_error":null,"evidence_seq":7,"ambiguity":null,"quiet_ms":null,"progress_ms":null,"task":null,"screen_tail":null}"#;
        let head = r#"{"agent_id":"agent_a","session_id":"session_a","engine":"claude","#;
        let cases = [
            (
                ExecutionPhase::NeverRan,
                r#""execution_active":false,"last_end":null,"exit_code":null,"#,
            ),
            (
                ExecutionPhase::Running,
                r#""execution_active":true,"last_end":null,"exit_code":null,"#,
            ),
            (
                ExecutionPhase::EndStalled(EndCause::Stopped),
                r#""execution_active":true,"stalled_end":"stopped","last_end":null,"exit_code":null,"#,
            ),
            (
                ExecutionPhase::Ended {
                    cause: EndCause::Exited,
                    exit_code: Some(1),
                },
                r#""execution_active":false,"last_end":"exited","exit_code":1,"#,
            ),
            (
                ExecutionPhase::Ended {
                    cause: EndCause::Lost,
                    exit_code: None,
                },
                r#""execution_active":false,"last_end":"lost","exit_code":null,"#,
            ),
        ];
        for (phase, fields) in cases {
            let golden = format!("{head}{fields}{tail}");
            let written = serde_json::to_string(&input(phase)).expect("serializes");
            assert_eq!(written, golden, "{phase:?}");
            let read: JudgeInput = serde_json::from_str(&golden).expect("reads");
            assert_eq!(read, input(phase), "{phase:?}");
        }
    }

    /// Every phase survives a round trip through its wire fields and
    /// through JSON.
    #[test]
    fn every_phase_round_trips() {
        for phase in phases() {
            assert_eq!(phase.wire().phase(), Some(phase), "{phase:?}");
            let json = serde_json::to_value(input(phase)).expect("serializes");
            let read: JudgeInput = serde_json::from_value(json).expect("reads");
            assert_eq!(read.phase, phase);
        }
    }

    /// Of every combination of the four fields, exactly the ones a phase
    /// writes read; the rest are rejected, as wire fields and as evidence.
    #[test]
    fn a_combination_no_phase_writes_is_rejected() {
        let written: Vec<PhaseWire> = phases().into_iter().map(ExecutionPhase::wire).collect();
        let mut legal = 0;
        for execution_active in [false, true] {
            for stalled_end in [None, Some(EndCause::Stopped), Some(EndCause::Lost)] {
                for last_end in [None, Some(EndCause::Stopped), Some(EndCause::Exited)] {
                    for exit_code in [None, Some(0), Some(1)] {
                        let wire = PhaseWire {
                            execution_active,
                            stalled_end,
                            last_end,
                            exit_code,
                        };
                        let json = serde_json::to_value(wire).expect("serializes");
                        let read = serde_json::from_value::<ExecutionPhase>(json.clone());
                        let evidence = serde_json::from_value::<JudgeInput>(json);
                        if written.contains(&wire) {
                            legal += 1;
                            assert_eq!(wire.phase().map(ExecutionPhase::wire), Some(wire));
                            assert_eq!(read.expect("legal reads"), wire.phase().expect("legal"));
                            assert!(evidence.is_ok(), "{wire:?}");
                        } else {
                            assert_eq!(wire.phase(), None, "{wire:?}");
                            let error = read.expect_err("illegal is rejected").to_string();
                            assert!(error.contains("no execution phase writes"), "{error}");
                            assert!(evidence.is_err(), "{wire:?}");
                        }
                    }
                }
            }
        }
        // NeverRan, Running, two stalled causes, and two end causes with
        // three exit statuses each.
        assert_eq!(legal, 1 + 1 + 2 + 2 * 3);
    }

    /// Evidence written before a field existed reads as that field's
    /// default.
    #[test]
    fn evidence_missing_phase_fields_reads_their_defaults() {
        let read = |json: &str| {
            serde_json::from_str::<JudgeInput>(json)
                .expect("reads")
                .phase
        };
        assert_eq!(read("{}"), ExecutionPhase::NeverRan);
        assert_eq!(
            read(r#"{"execution_active":true}"#),
            ExecutionPhase::Running
        );
        assert_eq!(
            read(r#"{"execution_active":true,"last_end":null}"#),
            ExecutionPhase::Running
        );
        assert_eq!(
            read(r#"{"execution_active":false,"last_end":"stopped"}"#),
            ExecutionPhase::Ended {
                cause: EndCause::Stopped,
                exit_code: None
            }
        );
        assert_eq!(
            read(r#"{"execution_active":true,"stalled_end":"lost"}"#),
            ExecutionPhase::EndStalled(EndCause::Lost)
        );
    }

    #[test]
    fn only_a_running_or_stalled_execution_is_live() {
        for phase in phases() {
            let live = matches!(
                phase,
                ExecutionPhase::Running | ExecutionPhase::EndStalled(_)
            );
            assert_eq!(phase.is_live(), live, "{phase:?}");
            assert_eq!(phase.wire().execution_active, live, "{phase:?}");
        }
    }
}
