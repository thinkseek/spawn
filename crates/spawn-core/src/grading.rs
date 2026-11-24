//! Agent grading (docs/agent-grading.md): which graded preset a task runs
//! on, chosen by how hard the task is and how well each preset has proven
//! itself, and how that proof moves.
//!
//! The words are chosen so none of them already means something else in
//! Spawn (docs/agent-grading.md, "Naming"):
//!
//! - a [`GradedPreset`] is one entry of the grading catalog
//!   (`grading_presets`), named by its `preset_id`. It is not a
//!   [`ModelProfile`]: it *maps to* one (`model_profile`), the switch the
//!   dispatcher already knows (`default` or `cheap`). In phase 1 that is
//!   all of it that reaches a worker: `engine_label` and `model_label` are
//!   labels for people.
//! - a [`SkillGrade`] is what a preset has proven it can do (`l1` to `l3`);
//!   a [`TaskDifficulty`] is what a task needs (`l1` to `l3`). Both travel
//!   as `l1`..`l3` on every record and every JSON answer, and are stored as
//!   their rank, 1 to 3.
//!
//! Every rule here is pure and integer-only: the store reads the facts
//! (the catalog, a task's quality rejections, a preset's recent outcomes
//! from `mission_tasks`) and records what these functions decide.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::ModelProfile;

/// The rolling window of a preset's outcomes calibration reads: its
/// latest closed tasks at one difficulty, newest first.
pub const CALIBRATION_WINDOW: usize = 10;

/// Calibration demotes only once the window holds this many outcomes.
pub const DEMOTE_MIN_TOTAL: u32 = 5;

/// ...and when fewer than this percent of them passed first time.
pub const DEMOTE_BELOW_PERCENT: u32 = 60;

/// Calibration restores only once the window holds this many outcomes.
pub const RESTORE_MIN_TOTAL: u32 = 8;

/// ...and when at least this percent of them passed first time. Between
/// the two bounds ([60 %, 90 %)) a preset holds its grade: the band keeps
/// a grade from flapping on one outcome.
pub const RESTORE_AT_PERCENT: u32 = 90;

/// The longest `preset_id`.
pub const PRESET_ID_MAX: usize = 64;

/// What a preset has proven it can do: `l1` (simple, well-specified
/// work) to `l3` (the hardest). Ordered: `L1 < L2 < L3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillGrade {
    L1 = 1,
    L2 = 2,
    L3 = 3,
}

impl SkillGrade {
    /// Every grade, lowest first.
    pub const ALL: [Self; 3] = [Self::L1, Self::L2, Self::L3];

    /// Wire name: `l1`, `l2` or `l3`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::L3 => "l3",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|grade| grade.as_str() == text)
    }

    /// The stored rank, 1 to 3.
    pub fn rank(self) -> i16 {
        self as i16
    }

    /// The grade of a stored rank.
    pub fn from_rank(rank: i64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|grade| i64::from(grade.rank()) == rank)
    }

    /// One grade lower; none below `l1`.
    pub fn demote(self) -> Option<Self> {
        match self {
            Self::L1 => None,
            Self::L2 => Some(Self::L1),
            Self::L3 => Some(Self::L2),
        }
    }

    /// One grade higher; none above `l3`.
    pub fn promote(self) -> Option<Self> {
        match self {
            Self::L1 => Some(Self::L2),
            Self::L2 => Some(Self::L3),
            Self::L3 => None,
        }
    }

    /// The difficulty of the same rank: the tasks calibration reads for a
    /// preset at this grade.
    pub fn difficulty(self) -> TaskDifficulty {
        match self {
            Self::L1 => TaskDifficulty::L1,
            Self::L2 => TaskDifficulty::L2,
            Self::L3 => TaskDifficulty::L3,
        }
    }
}

impl fmt::Display for SkillGrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a task needs: `l1` to `l3`, `l2` unless the assigner says.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum TaskDifficulty {
    L1 = 1,
    #[default]
    L2 = 2,
    L3 = 3,
}

impl TaskDifficulty {
    /// Every difficulty, easiest first.
    pub const ALL: [Self; 3] = [Self::L1, Self::L2, Self::L3];

    /// Wire name: `l1`, `l2` or `l3`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::L3 => "l3",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|difficulty| difficulty.as_str() == text)
    }

    /// The stored rank (`mission_tasks.difficulty`), 1 to 3.
    pub fn rank(self) -> i16 {
        self as i16
    }

    /// The difficulty of a stored rank.
    pub fn from_rank(rank: i64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|difficulty| i64::from(difficulty.rank()) == rank)
    }

    /// The least grade a preset needs to be given the task.
    pub fn required_grade(self) -> SkillGrade {
        match self {
            Self::L1 => SkillGrade::L1,
            Self::L2 => SkillGrade::L2,
            Self::L3 => SkillGrade::L3,
        }
    }

    /// The difficulty a task's rework is routed at once it has been sent
    /// back for its quality `quality_rejections` times (a review's
    /// `revise`, a gate's `rejected`; never a merge conflict or an
    /// incompatible head): rework that keeps failing on quality goes to a
    /// stronger grade. `l1` steps up to `l2` at the first and to `l3` at
    /// the third; `l2` to `l3` at the second; `l3` stays.
    #[must_use]
    pub fn for_rework(self, quality_rejections: u32) -> Self {
        match self {
            Self::L1 if quality_rejections >= 3 => Self::L3,
            Self::L1 if quality_rejections >= 1 => Self::L2,
            Self::L2 if quality_rejections >= 2 => Self::L3,
            other => other,
        }
    }
}

impl fmt::Display for TaskDifficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a graded preset stands (`grading_presets.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresetState {
    /// Registered and not graded yet (or its only grade, a locked one,
    /// was unlocked): never routed.
    Unexamined,
    /// Onboarded and at its onboarded grade (or enabled on a locked one).
    Active,
    /// Routed, at a grade below its onboarded one (calibration demoted it).
    Demoted,
    /// Never routed: failed onboarding's `l1`, calibration's `l1`, or a
    /// person disabled it.
    Disabled,
}

impl PresetState {
    /// Every state, in the order of the SQL check.
    pub const ALL: [Self; 4] = [
        Self::Unexamined,
        Self::Active,
        Self::Demoted,
        Self::Disabled,
    ];

    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unexamined => "unexamined",
            Self::Active => "active",
            Self::Demoted => "demoted",
            Self::Disabled => "disabled",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }

    /// Whether a task may be routed to a preset in this state.
    pub fn is_routable(self) -> bool {
        matches!(self, Self::Active | Self::Demoted)
    }
}

impl fmt::Display for PresetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether `text` is a `preset_id`: `[a-z0-9][a-z0-9_-]{0,63}`.
pub fn is_preset_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= PRESET_ID_MAX
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// One entry of the grading catalog: a `grading_presets` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GradedPreset {
    /// Its name, [`is_preset_id`].
    pub preset_id: String,
    /// The engine it stands for (`claude`, ...): a label for people;
    /// nothing runs on it in phase 1.
    pub engine_label: String,
    /// The model it stands for, as the engine names it: a label for
    /// people; nothing runs on it in phase 1.
    pub model_label: String,
    /// The [`ModelProfile`] the dispatcher starts its worker on: in phase 1
    /// the only part of a preset that reaches a worker.
    pub model_profile: ModelProfile,
    /// Lower is cheaper; ties broken by `preset_id`.
    pub cost_rank: u32,
    pub state: PresetState,
    /// The grade its onboarding exams proved; none before onboarding, or
    /// when it failed `l1`.
    pub onboarded_grade: Option<SkillGrade>,
    /// The grade calibration keeps it at: its onboarded grade, or lower.
    pub calibrated_grade: Option<SkillGrade>,
    /// A person's grade for it: routing reads it before the calibrated
    /// one, and calibration leaves the preset alone while it is set.
    pub locked_grade: Option<SkillGrade>,
    /// When its calibration window starts: the last change of its
    /// calibrated or locked grade, or of its state to a routed one.
    /// Calibration reads only the tasks assigned since.
    pub window_start_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The `grading.recorded` event that last changed it.
    pub last_event: i64,
}

impl GradedPreset {
    /// The grade routing reads: none unless the state is routed, else the
    /// locked grade if set, else the calibrated one.
    pub fn routing_grade(&self) -> Option<SkillGrade> {
        if self.state.is_routable() {
            self.locked_grade.or(self.calibrated_grade)
        } else {
            None
        }
    }

    /// Whether routing may give it a task: a routed state and a grade.
    pub fn is_routable(&self) -> bool {
        self.routing_grade().is_some()
    }
}

/// Whether some routable preset of `catalog` is graded for `difficulty`:
/// what an unflagged `spawn task assign` needs before it routes through
/// the catalog at the default `l2` (docs/agent-grading.md, "Assigning").
pub fn catalog_covers(catalog: &[GradedPreset], difficulty: TaskDifficulty) -> bool {
    let required = difficulty.required_grade();
    catalog
        .iter()
        .filter_map(GradedPreset::routing_grade)
        .any(|grade| grade >= required)
}

/// Where a task goes ([`route_preset`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub preset_id: String,
    /// The preset's [`ModelProfile`], which the task's `model` takes.
    pub model_profile: ModelProfile,
    /// The task's own difficulty, as assigned.
    pub difficulty: TaskDifficulty,
    /// The difficulty routed at: stepped up for its quality rejections.
    pub routed_difficulty: TaskDifficulty,
    pub required_grade: SkillGrade,
    /// The chosen preset's routing grade.
    pub preset_grade: SkillGrade,
    pub cost_rank: u32,
    /// Whether the assigner named the preset.
    pub explicit: bool,
}

impl RoutingDecision {
    /// Whether quality rejections stepped the task up to a harder
    /// difficulty.
    pub fn stepped_up(&self) -> bool {
        self.routed_difficulty != self.difficulty
    }
}

/// Why no preset was chosen. A named preset that cannot take the task is
/// an error, and so is a catalog with nothing strong enough; only an
/// unflagged `spawn task assign` falls back, and says so.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RoutingError {
    #[error("preset {preset_id} is not in the grading catalog")]
    UnknownPreset { preset_id: String },
    #[error("preset {preset_id} is {state}: only an active or demoted preset is routed")]
    NotRoutable {
        preset_id: String,
        state: PresetState,
    },
    #[error("preset {preset_id} has no grade: onboard it or lock its grade first")]
    Ungraded { preset_id: String },
    #[error(
        "preset {preset_id} is graded {grade}, below the {required} a task routed at {routed_difficulty} needs"
    )]
    BelowRequired {
        preset_id: String,
        grade: SkillGrade,
        required: SkillGrade,
        routed_difficulty: TaskDifficulty,
    },
    #[error(
        "no routable preset is graded {required} or above (a task routed at {routed_difficulty})"
    )]
    NoSufficientPreset {
        required: SkillGrade,
        routed_difficulty: TaskDifficulty,
    },
}

/// The preset a task of `difficulty`, sent back for its quality
/// `quality_rejections` times, runs on. Named (`explicit_preset`): that
/// preset, if it is in the catalog, routable and graded at least what the
/// routed difficulty requires, else an error. Otherwise the
/// minimum-sufficient preset: of the routable presets graded at least
/// that, the least by (grade, cost rank, preset id).
pub fn route_preset(
    difficulty: TaskDifficulty,
    quality_rejections: u32,
    explicit_preset: Option<&str>,
    catalog: &[GradedPreset],
) -> Result<RoutingDecision, RoutingError> {
    let routed_difficulty = difficulty.for_rework(quality_rejections);
    let required = routed_difficulty.required_grade();
    let decide = |preset: &GradedPreset, grade: SkillGrade, explicit: bool| RoutingDecision {
        preset_id: preset.preset_id.clone(),
        model_profile: preset.model_profile,
        difficulty,
        routed_difficulty,
        required_grade: required,
        preset_grade: grade,
        cost_rank: preset.cost_rank,
        explicit,
    };
    if let Some(name) = explicit_preset {
        let preset = catalog
            .iter()
            .find(|preset| preset.preset_id == name)
            .ok_or_else(|| RoutingError::UnknownPreset {
                preset_id: name.to_string(),
            })?;
        if !preset.state.is_routable() {
            return Err(RoutingError::NotRoutable {
                preset_id: name.to_string(),
                state: preset.state,
            });
        }
        let grade = preset
            .routing_grade()
            .ok_or_else(|| RoutingError::Ungraded {
                preset_id: name.to_string(),
            })?;
        if grade < required {
            return Err(RoutingError::BelowRequired {
                preset_id: name.to_string(),
                grade,
                required,
                routed_difficulty,
            });
        }
        return Ok(decide(preset, grade, true));
    }
    catalog
        .iter()
        .filter_map(|preset| Some((preset, preset.routing_grade()?)))
        .filter(|(_, grade)| *grade >= required)
        .min_by(|(a, a_grade), (b, b_grade)| {
            a_grade
                .cmp(b_grade)
                .then(a.cost_rank.cmp(&b.cost_rank))
                .then_with(|| a.preset_id.cmp(&b.preset_id))
        })
        .map(|(preset, grade)| decide(preset, grade, false))
        .ok_or(RoutingError::NoSufficientPreset {
            required,
            routed_difficulty,
        })
}

/// One onboarding exam case's result (evals/onboarding/).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExamCaseResult {
    pub case_id: String,
    /// The grade the case examines (its directory's `l<n>-` prefix).
    pub grade: SkillGrade,
    pub passed: bool,
}

/// What onboarding decides ([`evaluate_onboarding`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingVerdict {
    /// The highest grade every case at or below which passed; none when
    /// `l1` did not.
    pub onboarded_grade: Option<SkillGrade>,
    /// `active` with a grade, `disabled` without.
    pub state: PresetState,
    pub passed: u32,
    pub total: u32,
    /// The cases that failed, in the order given.
    pub failed_cases: Vec<String>,
}

/// The grade a preset's exam results prove: the highest grade G such that
/// every grade up to G has at least one case and every case at or below G
/// passed. A grade with no case, or with one failure, stops the climb
/// there. Without `l1`, the preset is disabled.
pub fn evaluate_onboarding(results: &[ExamCaseResult]) -> OnboardingVerdict {
    let mut onboarded_grade = None;
    for grade in SkillGrade::ALL {
        let mut at_grade = results.iter().filter(|result| result.grade == grade);
        let mut any = false;
        let all_passed = at_grade.all(|result| {
            any = true;
            result.passed
        });
        if !(any && all_passed) {
            break;
        }
        onboarded_grade = Some(grade);
    }
    let passed = results.iter().filter(|result| result.passed).count();
    OnboardingVerdict {
        onboarded_grade,
        state: if onboarded_grade.is_some() {
            PresetState::Active
        } else {
            PresetState::Disabled
        },
        passed: u32::try_from(passed).unwrap_or(u32::MAX),
        total: u32::try_from(results.len()).unwrap_or(u32::MAX),
        failed_cases: results
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.case_id.clone())
            .collect(),
    }
}

/// One closed task of a preset, as calibration reads it: whether it
/// passed first time (landed or accepted, never sent back for quality).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutcomeSample {
    pub task_id: i64,
    pub first_pass: bool,
}

/// What calibration decides for one preset ([`calibrate_preset`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "lowercase")]
pub enum CalibrationDecision {
    /// Nothing changes.
    Hold { reason: String },
    /// One grade lower.
    Demote {
        from_grade: SkillGrade,
        to_grade: SkillGrade,
        first_pass: u32,
        total: u32,
    },
    /// One grade back up toward its onboarded grade.
    Restore {
        from_grade: SkillGrade,
        to_grade: SkillGrade,
        first_pass: u32,
        total: u32,
    },
    /// Failing at `l1`: no longer routed.
    Disable {
        from_grade: SkillGrade,
        first_pass: u32,
        total: u32,
    },
}

impl CalibrationDecision {
    /// The preset's calibrated grade and state after the decision; none
    /// for a hold.
    pub fn outcome(&self, preset: &GradedPreset) -> Option<(SkillGrade, PresetState)> {
        match self {
            Self::Hold { .. } => None,
            Self::Demote { to_grade, .. } => Some((*to_grade, PresetState::Demoted)),
            Self::Restore { to_grade, .. } => Some((
                *to_grade,
                if preset.onboarded_grade == Some(*to_grade) {
                    PresetState::Active
                } else {
                    PresetState::Demoted
                },
            )),
            Self::Disable { from_grade, .. } => Some((*from_grade, PresetState::Disabled)),
        }
    }
}

/// Move a preset's calibrated grade on its recent outcomes (newest first;
/// only the first [`CALIBRATION_WINDOW`] count). Integer arithmetic only:
///
/// - a preset with a locked grade, an unrouted one, or one with no
///   calibrated grade holds;
/// - with at least [`DEMOTE_MIN_TOTAL`] outcomes of which fewer than
///   [`DEMOTE_BELOW_PERCENT`] % passed first time, it is demoted one grade
///   (disabled at `l1`);
/// - below its onboarded grade, with at least [`RESTORE_MIN_TOTAL`]
///   outcomes of which at least [`RESTORE_AT_PERCENT`] % passed first time,
///   it is restored one grade;
/// - otherwise it holds.
pub fn calibrate_preset(
    preset: &GradedPreset,
    recent_outcomes: &[TaskOutcomeSample],
) -> CalibrationDecision {
    let hold = |reason: &str| CalibrationDecision::Hold {
        reason: reason.to_string(),
    };
    if preset.locked_grade.is_some() {
        return hold("grade locked");
    }
    if !preset.state.is_routable() {
        return hold(preset.state.as_str());
    }
    let Some(calibrated) = preset.calibrated_grade else {
        return hold("ungraded");
    };
    let window = &recent_outcomes[..recent_outcomes.len().min(CALIBRATION_WINDOW)];
    // The window is at most CALIBRATION_WINDOW long: both counts fit.
    let total = u32::try_from(window.len()).unwrap_or(u32::MAX);
    let first_pass =
        u32::try_from(window.iter().filter(|sample| sample.first_pass).count()).unwrap_or(u32::MAX);
    if total >= DEMOTE_MIN_TOTAL && first_pass * 100 < total * DEMOTE_BELOW_PERCENT {
        return match calibrated.demote() {
            Some(to_grade) => CalibrationDecision::Demote {
                from_grade: calibrated,
                to_grade,
                first_pass,
                total,
            },
            None => CalibrationDecision::Disable {
                from_grade: calibrated,
                first_pass,
                total,
            },
        };
    }
    if let Some(onboarded) = preset.onboarded_grade
        && calibrated < onboarded
        && total >= RESTORE_MIN_TOTAL
        && first_pass * 100 >= total * RESTORE_AT_PERCENT
        && let Some(to_grade) = calibrated.promote()
    {
        return CalibrationDecision::Restore {
            from_grade: calibrated,
            to_grade,
            first_pass,
            total,
        };
    }
    if total < DEMOTE_MIN_TOTAL {
        return hold("too few outcomes");
    }
    hold("within band")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str, grade: SkillGrade, cost_rank: u32) -> GradedPreset {
        GradedPreset {
            preset_id: id.to_string(),
            engine_label: "claude".to_string(),
            model_label: format!("label-{id}"),
            model_profile: if cost_rank == 0 {
                ModelProfile::Cheap
            } else {
                ModelProfile::Default
            },
            cost_rank,
            state: PresetState::Active,
            onboarded_grade: Some(grade),
            calibrated_grade: Some(grade),
            locked_grade: None,
            window_start_ms: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_event: 0,
        }
    }

    fn samples(first_pass: usize, total: usize) -> Vec<TaskOutcomeSample> {
        (0..total)
            .map(|i| TaskOutcomeSample {
                task_id: i64::try_from(i).unwrap(),
                first_pass: i < first_pass,
            })
            .collect()
    }

    fn case(id: &str, grade: SkillGrade, passed: bool) -> ExamCaseResult {
        ExamCaseResult {
            case_id: id.to_string(),
            grade,
            passed,
        }
    }

    #[test]
    fn grades_and_difficulties_have_wire_names_ranks_and_order() {
        for (grade, name, rank) in [
            (SkillGrade::L1, "l1", 1),
            (SkillGrade::L2, "l2", 2),
            (SkillGrade::L3, "l3", 3),
        ] {
            assert_eq!(grade.as_str(), name);
            assert_eq!(SkillGrade::parse(name), Some(grade));
            assert_eq!(grade.rank(), rank);
            assert_eq!(SkillGrade::from_rank(i64::from(rank)), Some(grade));
            assert_eq!(grade.difficulty().rank(), rank);
            assert_eq!(
                serde_json::to_value(grade).unwrap(),
                serde_json::json!(name)
            );
        }
        for (difficulty, name, rank) in [
            (TaskDifficulty::L1, "l1", 1),
            (TaskDifficulty::L2, "l2", 2),
            (TaskDifficulty::L3, "l3", 3),
        ] {
            assert_eq!(difficulty.as_str(), name);
            assert_eq!(TaskDifficulty::parse(name), Some(difficulty));
            assert_eq!(difficulty.rank(), rank);
            assert_eq!(TaskDifficulty::from_rank(i64::from(rank)), Some(difficulty));
            assert_eq!(
                serde_json::to_value(difficulty).unwrap(),
                serde_json::json!(name)
            );
        }
        assert_eq!(SkillGrade::parse("L1"), None);
        assert_eq!(SkillGrade::parse("4"), None);
        assert_eq!(SkillGrade::from_rank(0), None);
        assert_eq!(TaskDifficulty::from_rank(4), None);
        assert!(SkillGrade::L1 < SkillGrade::L2 && SkillGrade::L2 < SkillGrade::L3);
        assert_eq!(TaskDifficulty::default(), TaskDifficulty::L2);
        assert_eq!(SkillGrade::L1.demote(), None);
        assert_eq!(SkillGrade::L2.demote(), Some(SkillGrade::L1));
        assert_eq!(SkillGrade::L3.demote(), Some(SkillGrade::L2));
        assert_eq!(SkillGrade::L1.promote(), Some(SkillGrade::L2));
        assert_eq!(SkillGrade::L2.promote(), Some(SkillGrade::L3));
        assert_eq!(SkillGrade::L3.promote(), None);
        for difficulty in TaskDifficulty::ALL {
            assert_eq!(difficulty.required_grade().rank(), difficulty.rank());
        }
    }

    #[test]
    fn rework_steps_up_at_its_quality_rejection_thresholds() {
        use TaskDifficulty::{L1, L2, L3};
        for (from, rejections, to) in [
            (L1, 0, L1),
            (L1, 1, L2),
            (L1, 2, L2),
            (L1, 3, L3),
            (L1, 9, L3),
            (L2, 0, L2),
            (L2, 1, L2),
            (L2, 2, L3),
            (L2, 7, L3),
            (L3, 0, L3),
            (L3, 5, L3),
        ] {
            assert_eq!(from.for_rework(rejections), to, "{from} after {rejections}");
        }
    }

    #[test]
    fn states_route_only_when_active_or_demoted() {
        for state in PresetState::ALL {
            assert_eq!(PresetState::parse(state.as_str()), Some(state));
        }
        assert!(PresetState::Active.is_routable());
        assert!(PresetState::Demoted.is_routable());
        assert!(!PresetState::Unexamined.is_routable());
        assert!(!PresetState::Disabled.is_routable());
        assert_eq!(PresetState::parse("probation"), None);
    }

    #[test]
    fn preset_ids_are_lowercase_names_of_at_most_64() {
        for good in ["a", "0", "claude-opus_l3", "x".repeat(64).as_str()] {
            assert!(is_preset_id(good), "{good}");
        }
        for bad in [
            "",
            "-a",
            "_a",
            "A",
            "a b",
            "a.b",
            "é",
            "x".repeat(65).as_str(),
        ] {
            assert!(!is_preset_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_routing_grade_prefers_the_lock_and_needs_a_routed_state() {
        let mut p = preset("p", SkillGrade::L2, 1);
        assert_eq!(p.routing_grade(), Some(SkillGrade::L2));
        p.locked_grade = Some(SkillGrade::L3);
        assert_eq!(p.routing_grade(), Some(SkillGrade::L3));
        p.calibrated_grade = None;
        assert_eq!(p.routing_grade(), Some(SkillGrade::L3));
        p.state = PresetState::Unexamined;
        assert_eq!(p.routing_grade(), None);
        assert!(!p.is_routable());
        p.state = PresetState::Active;
        p.locked_grade = None;
        assert_eq!(p.routing_grade(), None);
        assert!(!p.is_routable());
    }

    #[test]
    fn the_catalog_covers_a_difficulty_only_with_a_routable_preset_graded_for_it() {
        let l1_only = vec![preset("small", SkillGrade::L1, 0)];
        assert!(catalog_covers(&l1_only, TaskDifficulty::L1));
        assert!(!catalog_covers(&l1_only, TaskDifficulty::L2));
        assert!(!catalog_covers(&[], TaskDifficulty::L1));
        let mut off = preset("off", SkillGrade::L3, 1);
        off.state = PresetState::Disabled;
        let mut demoted = preset("demoted", SkillGrade::L2, 1);
        demoted.state = PresetState::Demoted;
        demoted.calibrated_grade = Some(SkillGrade::L1);
        let catalog = vec![preset("small", SkillGrade::L1, 0), off, demoted];
        assert!(!catalog_covers(&catalog, TaskDifficulty::L2));
        let mut locked = preset("locked", SkillGrade::L1, 2);
        locked.locked_grade = Some(SkillGrade::L2);
        assert!(catalog_covers(&[locked], TaskDifficulty::L2));
    }

    #[test]
    fn routing_picks_the_minimum_sufficient_grade_then_cost_then_id() {
        let catalog = vec![
            preset("strong", SkillGrade::L3, 0),
            preset("mid-b", SkillGrade::L2, 5),
            preset("mid-a", SkillGrade::L2, 5),
            preset("mid-dear", SkillGrade::L2, 9),
            preset("small", SkillGrade::L1, 3),
        ];
        let pick = |difficulty, rejections| {
            route_preset(difficulty, rejections, None, &catalog)
                .unwrap()
                .preset_id
        };
        // The lowest sufficient grade wins over a cheaper stronger one.
        assert_eq!(pick(TaskDifficulty::L1, 0), "small");
        // Equal grade and cost: the preset id breaks the tie.
        assert_eq!(pick(TaskDifficulty::L2, 0), "mid-a");
        assert_eq!(pick(TaskDifficulty::L3, 0), "strong");
        // Stepped up: l1 after one quality rejection needs l2.
        let decision = route_preset(TaskDifficulty::L1, 1, None, &catalog).unwrap();
        assert_eq!(decision.preset_id, "mid-a");
        assert_eq!(decision.routed_difficulty, TaskDifficulty::L2);
        assert_eq!(decision.difficulty, TaskDifficulty::L1);
        assert_eq!(decision.required_grade, SkillGrade::L2);
        assert_eq!(decision.preset_grade, SkillGrade::L2);
        assert!(decision.stepped_up());
        assert!(!decision.explicit);
        assert_eq!(pick(TaskDifficulty::L1, 3), "strong");
        assert_eq!(pick(TaskDifficulty::L2, 2), "strong");
    }

    #[test]
    fn a_cheaper_rank_wins_within_one_grade() {
        let catalog = vec![
            preset("zz-cheap", SkillGrade::L2, 1),
            preset("aa-dear", SkillGrade::L2, 2),
        ];
        let decision = route_preset(TaskDifficulty::L2, 0, None, &catalog).unwrap();
        assert_eq!(decision.preset_id, "zz-cheap");
        assert_eq!(decision.cost_rank, 1);
        assert_eq!(decision.model_profile, ModelProfile::Default);
    }

    #[test]
    fn routing_skips_unroutable_presets_and_reads_the_lock() {
        let mut unexamined = preset("a-unexamined", SkillGrade::L3, 0);
        unexamined.state = PresetState::Unexamined;
        let mut disabled = preset("b-disabled", SkillGrade::L3, 0);
        disabled.state = PresetState::Disabled;
        let mut demoted = preset("c-demoted", SkillGrade::L3, 0);
        demoted.state = PresetState::Demoted;
        demoted.calibrated_grade = Some(SkillGrade::L2);
        let mut locked = preset("d-locked", SkillGrade::L1, 0);
        locked.locked_grade = Some(SkillGrade::L3);
        let catalog = vec![unexamined, disabled, demoted, locked];
        assert_eq!(
            route_preset(TaskDifficulty::L2, 0, None, &catalog)
                .unwrap()
                .preset_id,
            "c-demoted"
        );
        assert_eq!(
            route_preset(TaskDifficulty::L3, 0, None, &catalog)
                .unwrap()
                .preset_id,
            "d-locked"
        );
    }

    #[test]
    fn routing_says_why_nothing_takes_the_task() {
        let mut off = preset("off", SkillGrade::L3, 0);
        off.state = PresetState::Disabled;
        let mut ungraded = preset("ungraded", SkillGrade::L3, 0);
        ungraded.calibrated_grade = None;
        let catalog = vec![preset("small", SkillGrade::L1, 0), off, ungraded];
        assert_eq!(
            route_preset(TaskDifficulty::L2, 0, None, &catalog),
            Err(RoutingError::NoSufficientPreset {
                required: SkillGrade::L2,
                routed_difficulty: TaskDifficulty::L2
            })
        );
        assert_eq!(
            route_preset(TaskDifficulty::L1, 0, Some("nobody"), &catalog),
            Err(RoutingError::UnknownPreset {
                preset_id: "nobody".to_string()
            })
        );
        assert_eq!(
            route_preset(TaskDifficulty::L1, 0, Some("off"), &catalog),
            Err(RoutingError::NotRoutable {
                preset_id: "off".to_string(),
                state: PresetState::Disabled
            })
        );
        assert_eq!(
            route_preset(TaskDifficulty::L1, 0, Some("ungraded"), &catalog),
            Err(RoutingError::Ungraded {
                preset_id: "ungraded".to_string()
            })
        );
        assert_eq!(
            route_preset(TaskDifficulty::L1, 1, Some("small"), &catalog),
            Err(RoutingError::BelowRequired {
                preset_id: "small".to_string(),
                grade: SkillGrade::L1,
                required: SkillGrade::L2,
                routed_difficulty: TaskDifficulty::L2
            })
        );
        assert!(route_preset(TaskDifficulty::L1, 0, None, &[]).is_err());
    }

    #[test]
    fn a_named_preset_is_taken_even_when_another_is_cheaper() {
        let catalog = vec![
            preset("cheap", SkillGrade::L3, 0),
            preset("named", SkillGrade::L3, 9),
        ];
        let decision = route_preset(TaskDifficulty::L2, 0, Some("named"), &catalog).unwrap();
        assert_eq!(decision.preset_id, "named");
        assert_eq!(decision.preset_grade, SkillGrade::L3);
        assert!(decision.explicit);
    }

    #[test]
    fn onboarding_climbs_contiguous_passed_grades() {
        use SkillGrade::{L1, L2, L3};
        let all = [
            case("a", L1, true),
            case("b", L2, true),
            case("c", L3, true),
        ];
        let verdict = evaluate_onboarding(&all);
        assert_eq!(verdict.onboarded_grade, Some(L3));
        assert_eq!(verdict.state, PresetState::Active);
        assert_eq!((verdict.passed, verdict.total), (3, 3));
        assert!(verdict.failed_cases.is_empty());

        let l3_failed = [
            case("a", L1, true),
            case("b", L2, true),
            case("c", L3, false),
        ];
        let verdict = evaluate_onboarding(&l3_failed);
        assert_eq!(verdict.onboarded_grade, Some(L2));
        assert_eq!(verdict.failed_cases, vec!["c".to_string()]);

        // A failed l2 stops the climb even when l3 passed.
        let gap = [
            case("a", L1, true),
            case("b", L2, false),
            case("c", L3, true),
        ];
        assert_eq!(evaluate_onboarding(&gap).onboarded_grade, Some(L1));

        // A grade with no case stops it too.
        let missing = [case("a", L1, true), case("c", L3, true)];
        assert_eq!(evaluate_onboarding(&missing).onboarded_grade, Some(L1));

        // One failure among a grade's cases fails the grade.
        let mixed = [
            case("a", L1, true),
            case("b1", L2, true),
            case("b2", L2, false),
        ];
        assert_eq!(evaluate_onboarding(&mixed).onboarded_grade, Some(L1));

        for none in [
            &[case("a", L1, false), case("b", L2, true)][..],
            &[case("b", L2, true), case("c", L3, true)][..],
            &[][..],
        ] {
            let verdict = evaluate_onboarding(none);
            assert_eq!(verdict.onboarded_grade, None, "{none:?}");
            assert_eq!(verdict.state, PresetState::Disabled);
        }
    }

    #[test]
    fn calibration_leaves_locked_unrouted_and_ungraded_presets_alone() {
        let failing = samples(0, 10);
        let mut locked = preset("p", SkillGrade::L2, 0);
        locked.locked_grade = Some(SkillGrade::L2);
        assert_eq!(
            calibrate_preset(&locked, &failing),
            CalibrationDecision::Hold {
                reason: "grade locked".to_string()
            }
        );
        for state in [PresetState::Unexamined, PresetState::Disabled] {
            let mut p = preset("p", SkillGrade::L2, 0);
            p.state = state;
            assert_eq!(
                calibrate_preset(&p, &failing),
                CalibrationDecision::Hold {
                    reason: state.as_str().to_string()
                }
            );
        }
        let mut ungraded = preset("p", SkillGrade::L2, 0);
        ungraded.calibrated_grade = None;
        assert!(matches!(
            calibrate_preset(&ungraded, &failing),
            CalibrationDecision::Hold { .. }
        ));
    }

    #[test]
    fn calibration_demotes_below_sixty_percent_from_five_outcomes() {
        let p = preset("p", SkillGrade::L3, 0);
        // 4 outcomes: too few, whatever they say.
        assert_eq!(
            calibrate_preset(&p, &samples(0, 4)),
            CalibrationDecision::Hold {
                reason: "too few outcomes".to_string()
            }
        );
        // 2 of 5 = 40 %: demoted.
        assert_eq!(
            calibrate_preset(&p, &samples(2, 5)),
            CalibrationDecision::Demote {
                from_grade: SkillGrade::L3,
                to_grade: SkillGrade::L2,
                first_pass: 2,
                total: 5
            }
        );
        // 3 of 5 = exactly 60 %: held (the bound is strict).
        assert!(matches!(
            calibrate_preset(&p, &samples(3, 5)),
            CalibrationDecision::Hold { .. }
        ));
        // 5 of 9 = 55.6 %: demoted.
        assert!(matches!(
            calibrate_preset(&p, &samples(5, 9)),
            CalibrationDecision::Demote { .. }
        ));
        // 6 of 10 = 60 %: held.
        assert!(matches!(
            calibrate_preset(&p, &samples(6, 10)),
            CalibrationDecision::Hold { .. }
        ));
    }

    #[test]
    fn calibration_disables_a_failing_l1() {
        let p = preset("p", SkillGrade::L1, 0);
        let decision = calibrate_preset(&p, &samples(1, 6));
        assert_eq!(
            decision,
            CalibrationDecision::Disable {
                from_grade: SkillGrade::L1,
                first_pass: 1,
                total: 6
            }
        );
        assert_eq!(
            decision.outcome(&p),
            Some((SkillGrade::L1, PresetState::Disabled))
        );
    }

    #[test]
    fn calibration_reads_only_the_newest_ten() {
        let p = preset("p", SkillGrade::L2, 0);
        // Newest ten all passed; older failures beyond the window ignored.
        let mut window = samples(10, 10);
        window.extend(samples(0, 20));
        assert!(matches!(
            calibrate_preset(&p, &window),
            CalibrationDecision::Hold { .. }
        ));
        // Newest ten all failed; older passes beyond the window ignored.
        let mut window = samples(0, 10);
        window.extend(samples(20, 20));
        assert_eq!(
            calibrate_preset(&p, &window),
            CalibrationDecision::Demote {
                from_grade: SkillGrade::L2,
                to_grade: SkillGrade::L1,
                first_pass: 0,
                total: 10
            }
        );
    }

    #[test]
    fn calibration_restores_one_step_at_ninety_percent_from_eight() {
        let mut p = preset("p", SkillGrade::L3, 0);
        p.calibrated_grade = Some(SkillGrade::L1);
        p.state = PresetState::Demoted;
        // 7 of 7: too few to restore.
        assert!(matches!(
            calibrate_preset(&p, &samples(7, 7)),
            CalibrationDecision::Hold { .. }
        ));
        // 8 of 8: one step up, still below its onboarded grade.
        let decision = calibrate_preset(&p, &samples(8, 8));
        assert_eq!(
            decision,
            CalibrationDecision::Restore {
                from_grade: SkillGrade::L1,
                to_grade: SkillGrade::L2,
                first_pass: 8,
                total: 8
            }
        );
        assert_eq!(
            decision.outcome(&p),
            Some((SkillGrade::L2, PresetState::Demoted))
        );
        // 9 of 10 = 90 %: restored; 8 of 9 = 88.9 %: held.
        assert!(matches!(
            calibrate_preset(&p, &samples(9, 10)),
            CalibrationDecision::Restore { .. }
        ));
        assert!(matches!(
            calibrate_preset(&p, &samples(8, 9)),
            CalibrationDecision::Hold { .. }
        ));
        // Back at its onboarded grade it is active again.
        p.calibrated_grade = Some(SkillGrade::L2);
        let decision = calibrate_preset(&p, &samples(10, 10));
        assert_eq!(
            decision.outcome(&p),
            Some((SkillGrade::L3, PresetState::Active))
        );
        // Never above the onboarded grade.
        p.calibrated_grade = Some(SkillGrade::L3);
        p.state = PresetState::Active;
        assert!(matches!(
            calibrate_preset(&p, &samples(10, 10)),
            CalibrationDecision::Hold { .. }
        ));
    }

    #[test]
    fn the_hysteresis_band_holds_between_sixty_and_ninety_percent() {
        let mut demoted = preset("p", SkillGrade::L3, 0);
        demoted.calibrated_grade = Some(SkillGrade::L2);
        demoted.state = PresetState::Demoted;
        let active = preset("q", SkillGrade::L2, 0);
        for passed in 6..=8 {
            for p in [&demoted, &active] {
                assert_eq!(
                    calibrate_preset(p, &samples(passed, 10)),
                    CalibrationDecision::Hold {
                        reason: "within band".to_string()
                    },
                    "{passed}/10 for {}",
                    p.preset_id
                );
            }
        }
        assert!(matches!(
            calibrate_preset(&active, &samples(5, 10)),
            CalibrationDecision::Demote { .. }
        ));
        assert!(matches!(
            calibrate_preset(&demoted, &samples(9, 10)),
            CalibrationDecision::Restore { .. }
        ));
    }

    #[test]
    fn decisions_and_presets_serialize_with_wire_names() {
        let decision = CalibrationDecision::Demote {
            from_grade: SkillGrade::L3,
            to_grade: SkillGrade::L2,
            first_pass: 1,
            total: 5,
        };
        assert_eq!(
            serde_json::to_value(&decision).unwrap(),
            serde_json::json!({"decision": "demote", "from_grade": "l3", "to_grade": "l2",
                               "first_pass": 1, "total": 5})
        );
        let p = preset("p", SkillGrade::L2, 0);
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value["model_profile"], serde_json::json!("cheap"));
        assert_eq!(value["calibrated_grade"], serde_json::json!("l2"));
        assert_eq!(value["state"], serde_json::json!("active"));
        let result = serde_json::json!({"case_id": "l1-a", "grade": "l1", "passed": true});
        assert_eq!(
            serde_json::from_value::<ExamCaseResult>(result).unwrap(),
            case("l1-a", SkillGrade::L1, true)
        );
    }
}
