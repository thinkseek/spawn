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
    /// The named execution profile it stands for (phase 2, any
    /// [`ExecutionProfile`] name: `opus-max`, `local-qwen`, ...), run on
    /// `model_profile`. None (left out of every record and answer): the
    /// preset's profile is its `model_profile`'s name, as in phase 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<ExecutionProfile>,
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

    /// The name of the profile it runs as: its [`ExecutionProfile`], else
    /// its `model_profile`'s wire name (`default` or `cheap`).
    pub fn profile_name(&self) -> &str {
        self.execution_profile
            .as_ref()
            .map_or_else(|| self.model_profile.as_str(), ExecutionProfile::as_str)
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
    /// The preset's named [`ExecutionProfile`], when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<ExecutionProfile>,
    /// Whether the anti-starvation re-probe chose the preset
    /// ([`route_with_reprobe`]) over the minimum-sufficient one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reprobe: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
fn is_false(value: &bool) -> bool {
    !*value
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
        execution_profile: preset.execution_profile.clone(),
        reprobe: false,
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

/// The longest [`ExecutionProfile`] name.
pub const EXECUTION_PROFILE_MAX: usize = 64;

/// A named execution profile a graded preset stands for (phase 2): any
/// name of [`is_preset_id`]'s shape, so a catalog tells apart as many
/// runtimes as it names, not two. It travels as its bare name, like a
/// [`ModelProfile`]; the two phase-1 names (`default`, `cheap`) are the
/// [`ModelProfile`]s themselves and are never stored as one (a preset
/// named on them keeps `execution_profile` null, exactly as in phase 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionProfile(String);

impl ExecutionProfile {
    /// The profile named `text`, when it is a name: `[a-z0-9][a-z0-9_-]{0,63}`.
    pub fn parse(text: &str) -> Option<Self> {
        is_preset_id(text).then(|| Self(text.to_string()))
    }

    /// Its name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The [`ModelProfile`] of the same name, for `default` and `cheap`.
    pub fn builtin(&self) -> Option<ModelProfile> {
        ModelProfile::parse(&self.0)
    }
}

impl TryFrom<String> for ExecutionProfile {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or_else(|| {
            format!("an execution profile is [a-z0-9][a-z0-9_-]{{0,63}}, not {text}")
        })
    }
}

impl From<ExecutionProfile> for String {
    fn from(profile: ExecutionProfile) -> Self {
        profile.0
    }
}

impl From<ModelProfile> for ExecutionProfile {
    fn from(profile: ModelProfile) -> Self {
        Self(profile.as_str().to_string())
    }
}

impl fmt::Display for ExecutionProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The profile a preset registers as, from `spawn grade register`'s
/// `--model-profile` (the runtime switch, `default` or `cheap`) and
/// `--execution-profile` (any name): `(model_profile, execution_profile)`.
/// A builtin name given as the execution profile is that model profile
/// (and must agree with `--model-profile` when both are given); any other
/// name runs on `--model-profile`, `default` unless given.
pub fn registered_profile(
    model_profile: Option<&str>,
    execution_profile: Option<&str>,
) -> Result<(ModelProfile, Option<ExecutionProfile>), String> {
    let model = model_profile
        .map(|text| {
            ModelProfile::parse(text)
                .ok_or_else(|| format!("--model-profile is default or cheap, not {text}"))
        })
        .transpose()?;
    let Some(text) = execution_profile else {
        return Ok((model.unwrap_or_default(), None));
    };
    let named = ExecutionProfile::parse(text).ok_or_else(|| {
        format!("--execution-profile is a name [a-z0-9][a-z0-9_-]{{0,63}}, not {text}")
    })?;
    match (named.builtin(), model) {
        (Some(builtin), Some(model)) if builtin != model => Err(format!(
            "--execution-profile {text} is the {text} model profile, not --model-profile {model}",
            model = model.as_str()
        )),
        (Some(builtin), _) => Ok((builtin, None)),
        (None, model) => Ok((model.unwrap_or_default(), Some(named))),
    }
}

/// The version of [`classify_difficulty`]'s rules, carried on every
/// classification so a recorded one says which rules decided it.
pub const CLASSIFIER_RULES: &str = "brief-v2";

/// A classification's score at or above which a task is `l3`.
pub const CLASSIFY_L3_AT: i32 = 4;

/// A classification's score at or below which a task is `l1` (only when
/// no risk marker matched, see [`classify_difficulty`]).
pub const CLASSIFY_L1_AT: i32 = -2;

/// The longest brief [`classify_difficulty`] may call `l1`.
pub const CLASSIFY_L1_MAX_CHARS: usize = 400;

/// One rule of [`classify_difficulty`] that matched: its name, the points
/// it gave and what it matched. The evidence is the rule's own vocabulary
/// (a keyword of its list, a count): never free text of the brief, so a
/// recorded classification carries nothing the brief's masking would hide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationSignal {
    pub rule: String,
    pub points: i32,
    pub evidence: Vec<String>,
}

/// What [`classify_difficulty`] decided, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifficultyClassification {
    pub difficulty: TaskDifficulty,
    pub score: i32,
    /// [`CLASSIFIER_RULES`].
    pub rules: String,
    /// Every rule that matched, in the rule-set's order.
    pub signals: Vec<ClassificationSignal>,
}

impl DifficultyClassification {
    /// Whether any rule matched: a brief no rule says anything of is the
    /// default `l2`, exactly as an unflagged phase-1 assignment.
    pub fn has_signal(&self) -> bool {
        !self.signals.is_empty()
    }

    /// One line for a person: `l3 (score 5: security +4 [auth]; ...)`.
    pub fn rationale(&self) -> String {
        if self.signals.is_empty() {
            return format!("{} (no signal: the default)", self.difficulty);
        }
        let parts: Vec<String> = self
            .signals
            .iter()
            .map(|signal| {
                format!(
                    "{} {:+} [{}]",
                    signal.rule,
                    signal.points,
                    signal.evidence.join(", ")
                )
            })
            .collect();
        format!(
            "{} (score {}: {})",
            self.difficulty,
            self.score,
            parts.join("; ")
        )
    }
}

/// Security markers: a token starting with one of these.
const SECURITY_STEMS: &[&str] = &[
    "security",
    "vulnerab",
    "exploit",
    "cve-",
    "authenticat",
    "authoriz",
    "credential",
    "password",
    "privilege",
    "injection",
    "csrf",
    "xss",
    "sandbox",
    "secret",
];

/// Migration markers: a token starting with one of these, or a phrase.
const MIGRATION_STEMS: &[&str] = &["migrat", "schema", "backfill", "refold"];
const MIGRATION_PHRASES: &[&str] = &["alter table", "data loss", "breaking change"];

/// Concurrency markers.
const CONCURRENCY_STEMS: &[&str] = &["deadlock", "concurren", "transaction", "atomic"];
const CONCURRENCY_PHRASES: &[&str] = &["race condition", "advisory lock"];

/// Cross-cutting markers.
const CROSS_CUTTING_STEMS: &[&str] = &["refactor", "redesign", "architect", "rewrite"];
const CROSS_CUTTING_PHRASES: &[&str] = &["across crates", "every crate", "backwards compat"];

/// Multi-step markers.
const MULTI_STEP_STEMS: &[&str] = &["multi-step", "end-to-end", "phase", "pipeline"];

/// Low-risk markers (`brief-v2`): mechanical, repetitive or tagging work
/// only, where the result is decided by the request and not by judgement.
/// Semantic writing (`wording`, `readme`, `docstring`, "docs only") is not
/// one, and neither is `one-line`, which says nothing about determinism.
/// A generic verb or noun (`comment`, `rename`, `bump`) also names real
/// engineering work ("add comment support", "rename it and update every
/// caller", "bump tokio and fix whatever breaks"), so none of those is one.
const LOW_RISK_STEMS: &[&str] = &[
    "typo",
    "spelling",
    "formatting",
    "whitespace",
    "rustfmt",
    "tag",
    "label",
    "mechanical",
    "repetitive",
    "bulk",
    "regenerat",
];
const LOW_RISK_PHRASES: &[&str] = &["cargo fmt", "registry entry"];

/// Words that widen a change past a small one, authoring words (writing
/// that needs judgement) and words naming code to build around a tag or a
/// label: any of them cancels the low-risk rule.
const BROADENING_STEMS: &[&str] = &[
    "every",
    "whatever",
    "caller",
    "implement",
    "support",
    "feature",
    "write",
    "draft",
    "document",
    "explain",
    "describ",
    "summar",
    "clarif",
    "reword",
    "rephras",
    "comment",
    "filter",
    "render",
    "pars",
    "endpoint",
    "api",
    "display",
];

/// File extensions a path token ends with.
const PATH_EXTENSIONS: &[&str] = &[
    ".rs", ".sql", ".ts", ".tsx", ".md", ".sh", ".toml", ".json", ".py", ".yml", ".yaml",
];

/// The keywords of `stems` (prefixes of a word) and `phrases` (anywhere
/// in the text) that `words` / `text` hold, each once, in list order.
fn matched(words: &[&str], text: &str, stems: &[&str], phrases: &[&str]) -> Vec<String> {
    stems
        .iter()
        .filter(|stem| words.iter().any(|word| word.starts_with(*stem)))
        .chain(phrases.iter().filter(|phrase| text.contains(*phrase)))
        .map(|keyword| (*keyword).to_string())
        .collect()
}

/// How many lines of `text` are enumerated steps: starting `- `, `* `, or
/// a number of at most two digits and `.` or `)`.
fn enumerated_steps(text: &str) -> usize {
    text.lines()
        .map(str::trim_start)
        .filter(|line| {
            line.starts_with("- ")
                || line.starts_with("* ")
                || line.split_once(['.', ')']).is_some_and(|(number, _)| {
                    !number.is_empty()
                        && number.len() <= 2
                        && number.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .count()
}

/// How many distinct file paths `tokens` name: a token with a known
/// extension, or with a `/` and a letter.
fn named_paths(tokens: &[&str]) -> usize {
    let mut paths: Vec<&str> = tokens
        .iter()
        .copied()
        .filter(|token| {
            PATH_EXTENSIONS.iter().any(|ext| token.ends_with(ext))
                || (token.contains('/') && token.bytes().any(|b| b.is_ascii_alphabetic()))
        })
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths.len()
}

/// How many distinct crates `tokens` name: the segment after `crates/`
/// in a path, or a `spawn-<name>` token, `spawn-` dropped either way.
fn named_crates(tokens: &[&str]) -> usize {
    let mut crates: Vec<&str> = tokens
        .iter()
        .filter_map(|token| {
            token
                .split('/')
                .collect::<Vec<_>>()
                .windows(2)
                .find(|pair| pair[0] == "crates")
                .map(|pair| pair[1])
                .or_else(|| {
                    token
                        .strip_prefix("spawn-")
                        .filter(|rest| !rest.is_empty() && !rest.contains(['/', '.']))
                        .map(|_| *token)
                })
        })
        .map(|name| name.strip_prefix("spawn-").unwrap_or(name))
        .filter(|name| !name.is_empty())
        .collect();
    crates.sort_unstable();
    crates.dedup();
    crates.len()
}

/// The difficulty a task's brief (and objective, if the caller appends
/// it) reads as: a deterministic, integer-only rule-set over its structure
/// and wording, every matched rule kept as evidence ([`CLASSIFIER_RULES`]).
///
/// Points, summed:
/// - `security` +4: a security marker (auth, credential, injection, ...);
/// - `migration` +4: a schema or data migration (migrat*, schema, alter
///   table, backfill, data loss, ...);
/// - `concurrency` +2: a race, deadlock, transaction, ...;
/// - `cross_cutting` +2: a refactor, redesign, rewrite, ...;
/// - `multi_step` +1 for three or more enumerated steps (lines starting
///   `1.`, `2)`, `-` or `*`), +1 more from six, +1 for a multi-step marker;
/// - `file_scope` +1 from three distinct file paths named, +2 from six;
/// - `crate_scope` +1 from two distinct crates named (`crates/<name>` or a
///   `spawn-<name>` token), +2 from three;
/// - `long_brief` +1 above 2000 characters;
/// - `low_risk` -2: a mechanical marker (typo, spelling, formatting,
///   whitespace, rustfmt, "cargo fmt", tag*, label*, mechanical,
///   repetitive, bulk, regenerat*, "registry entry"), only when no
///   `security`, `migration` or `concurrency` rule matched and no
///   broadening word is there: a widening one (every, whatever, caller,
///   implement, support, feature), an authoring one (write, draft,
///   document*, explain*, describ*, summar*, clarif*, reword, rephras*,
///   comment*) or one naming code to build (filter, render, pars*,
///   endpoint, api, display).
///
/// `l3` from [`CLASSIFY_L3_AT`]; `l1` at or below [`CLASSIFY_L1_AT`] when
/// no risk rule matched, at most one file is named and the brief is at
/// most [`CLASSIFY_L1_MAX_CHARS`] characters; `l2` otherwise, and for a
/// brief no rule matched.
pub fn classify_difficulty(brief: &str) -> DifficultyClassification {
    let text = brief.to_ascii_lowercase();
    let tokens: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')))
        .map(|token| token.trim_matches(|c: char| matches!(c, '.' | '-' | '/')))
        .filter(|token| !token.is_empty())
        .collect();
    // Keywords are read in every path segment too: `sql/schema.sql` names
    // the schema.
    let words: Vec<&str> = tokens
        .iter()
        .flat_map(|token| token.split('/'))
        .filter(|word| !word.is_empty())
        .collect();
    let mut signals = Vec::new();
    let mut add = |rule: &str, points: i32, evidence: Vec<String>| {
        if points != 0 && !evidence.is_empty() {
            signals.push(ClassificationSignal {
                rule: rule.to_string(),
                points,
                evidence,
            });
        }
    };
    let security = matched(&words, &text, SECURITY_STEMS, &[]);
    let migration = matched(&words, &text, MIGRATION_STEMS, MIGRATION_PHRASES);
    let concurrency = matched(&words, &text, CONCURRENCY_STEMS, CONCURRENCY_PHRASES);
    let risky = !(security.is_empty() && migration.is_empty() && concurrency.is_empty());
    add("security", 4, security);
    add("migration", 4, migration);
    add("concurrency", 2, concurrency);
    add(
        "cross_cutting",
        2,
        matched(&words, &text, CROSS_CUTTING_STEMS, CROSS_CUTTING_PHRASES),
    );
    let steps = enumerated_steps(&text);
    let mut step_evidence = Vec::new();
    let mut step_points = 0;
    if steps >= 3 {
        step_points += if steps >= 6 { 2 } else { 1 };
        step_evidence.push(format!("{steps} steps"));
    }
    let markers = matched(&words, &text, MULTI_STEP_STEMS, &[]);
    if !markers.is_empty() {
        step_points += 1;
        step_evidence.extend(markers);
    }
    add("multi_step", step_points, step_evidence);
    let files = named_paths(&tokens);
    add(
        "file_scope",
        match files {
            0..=2 => 0,
            3..=5 => 1,
            _ => 2,
        },
        vec![format!("{files} paths")],
    );
    let crate_count = named_crates(&tokens);
    add(
        "crate_scope",
        match crate_count {
            0 | 1 => 0,
            2 => 1,
            _ => 2,
        },
        vec![format!("{crate_count} crates")],
    );
    let chars = brief.chars().count();
    add(
        "long_brief",
        i32::from(chars > 2000),
        vec![format!("{chars} chars")],
    );
    if !risky && matched(&words, &text, BROADENING_STEMS, &[]).is_empty() {
        add(
            "low_risk",
            -2,
            matched(&words, &text, LOW_RISK_STEMS, LOW_RISK_PHRASES),
        );
    }
    let score: i32 = signals.iter().map(|signal| signal.points).sum();
    let difficulty = if score >= CLASSIFY_L3_AT {
        TaskDifficulty::L3
    } else if score <= CLASSIFY_L1_AT && !risky && files <= 1 && chars <= CLASSIFY_L1_MAX_CHARS {
        TaskDifficulty::L1
    } else {
        TaskDifficulty::L2
    };
    DifficultyClassification {
        difficulty,
        score,
        rules: CLASSIFIER_RULES.to_string(),
        signals,
    }
}

/// How long a routable preset may go without a task first assigned to it
/// at its calibrated grade before the anti-starvation re-probe gives it
/// one, by default: 6 hours.
pub const REPROBE_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

/// When a preset last had a task first assigned to it at one difficulty
/// (`mission_tasks.first_preset_id`, `difficulty`, `created_at_ms`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetActivity {
    pub preset_id: String,
    pub difficulty: TaskDifficulty,
    pub last_assigned_ms: i64,
}

/// Whether `preset` is due a re-probe at `now_ms`: routable, its grade not
/// locked (calibration leaves a locked preset alone, so probing it proves
/// nothing), and no task first assigned to it at its calibrated grade's
/// difficulty for `interval_ms`, counted from the later of that last
/// assignment and its window's start (a preset just onboarded, enabled or
/// regraded is never due at once). A demoted preset losing every task at
/// its new grade to a cheaper one, or one graded above every task it is
/// given, is what this finds.
pub fn reprobe_due(
    preset: &GradedPreset,
    activity: &[PresetActivity],
    now_ms: i64,
    interval_ms: i64,
) -> bool {
    if preset.locked_grade.is_some() || !preset.state.is_routable() {
        return false;
    }
    let Some(grade) = preset.calibrated_grade else {
        return false;
    };
    let last = activity
        .iter()
        .filter(|seen| seen.preset_id == preset.preset_id && seen.difficulty == grade.difficulty())
        .map(|seen| seen.last_assigned_ms)
        .max()
        .unwrap_or(i64::MIN)
        .max(preset.window_start_ms);
    now_ms.saturating_sub(last) >= interval_ms.max(1)
}

/// Every preset of `catalog` due a re-probe ([`reprobe_due`]), by id.
pub fn reprobe_plan(
    catalog: &[GradedPreset],
    activity: &[PresetActivity],
    now_ms: i64,
    interval_ms: i64,
) -> Vec<String> {
    catalog
        .iter()
        .filter(|preset| reprobe_due(preset, activity, now_ms, interval_ms))
        .map(|preset| preset.preset_id.clone())
        .collect()
}

/// [`route_preset`] for an unflagged assignment (never a named preset,
/// never a rework), with the anti-starvation re-probe: when some preset
/// graded exactly for `difficulty` (so the task lands in its calibration
/// window) is due a re-probe ([`reprobe_due`]), the task goes to it
/// instead of the minimum-sufficient one: the longest-starved first, then
/// by cost rank and id. Bounded: once given the task, a preset is not due
/// again for `interval_ms`, so each starved preset takes at most one task
/// per interval, and every other task routes as [`route_preset`] says.
pub fn route_with_reprobe(
    difficulty: TaskDifficulty,
    catalog: &[GradedPreset],
    activity: &[PresetActivity],
    now_ms: i64,
    interval_ms: i64,
) -> Result<RoutingDecision, RoutingError> {
    let base = route_preset(difficulty, 0, None, catalog)?;
    let required = difficulty.required_grade();
    let last = |preset: &GradedPreset| {
        activity
            .iter()
            .filter(|seen| seen.preset_id == preset.preset_id && seen.difficulty == difficulty)
            .map(|seen| seen.last_assigned_ms)
            .max()
            .unwrap_or(i64::MIN)
            .max(preset.window_start_ms)
    };
    let probe = catalog
        .iter()
        .filter(|preset| {
            preset.preset_id != base.preset_id
                && preset.calibrated_grade == Some(required)
                && reprobe_due(preset, activity, now_ms, interval_ms)
        })
        .min_by(|a, b| {
            last(a)
                .cmp(&last(b))
                .then(a.cost_rank.cmp(&b.cost_rank))
                .then_with(|| a.preset_id.cmp(&b.preset_id))
        });
    Ok(match probe {
        Some(preset) => RoutingDecision {
            preset_id: preset.preset_id.clone(),
            model_profile: preset.model_profile,
            preset_grade: required,
            cost_rank: preset.cost_rank,
            execution_profile: preset.execution_profile.clone(),
            reprobe: true,
            ..base
        },
        None => base,
    })
}

/// Whether a gate's verdict sends a task back for its quality: `rejected`
/// alone. A merge `conflict` or an `incompatible` head (fleet concurrency),
/// a `host_fault`, `no_room` or `aborted` (the host could not judge it,
/// or a timeout), `stale` (nothing was judged) and `accepted` never count,
/// and never step a task's rework up.
pub fn is_quality_rejection(verdict: &str) -> bool {
    verdict == "rejected"
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
            execution_profile: None,
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

    #[test]
    fn execution_profiles_are_names_and_the_builtins_are_model_profiles() {
        let named = ExecutionProfile::parse("opus-max").unwrap();
        assert_eq!(named.as_str(), "opus-max");
        assert_eq!(named.builtin(), None);
        assert_eq!(
            ExecutionProfile::parse("cheap").unwrap().builtin(),
            Some(ModelProfile::Cheap)
        );
        assert_eq!(
            ExecutionProfile::from(ModelProfile::Default).as_str(),
            "default"
        );
        for bad in ["", "Opus", "a b", "-x", &"x".repeat(65)] {
            assert_eq!(ExecutionProfile::parse(bad), None, "{bad}");
        }
        assert_eq!(serde_json::to_value(&named).unwrap(), json!("opus-max"));
        assert_eq!(
            serde_json::from_value::<ExecutionProfile>(json!("local-qwen")).unwrap(),
            ExecutionProfile::parse("local-qwen").unwrap()
        );
        assert!(serde_json::from_value::<ExecutionProfile>(json!("Not A Name")).is_err());
    }

    #[test]
    fn a_preset_without_an_execution_profile_serializes_as_in_phase_one() {
        let mut p = preset("p", SkillGrade::L2, 0);
        let value = serde_json::to_value(&p).unwrap();
        assert!(value.get("execution_profile").is_none(), "{value}");
        assert_eq!(p.profile_name(), "cheap");
        // A phase-1 record (no execution_profile) still reads.
        assert_eq!(serde_json::from_value::<GradedPreset>(value).unwrap(), p);
        p.execution_profile = ExecutionProfile::parse("deepseek-r2");
        assert_eq!(p.profile_name(), "deepseek-r2");
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(value["execution_profile"], json!("deepseek-r2"));
        assert_eq!(value["model_profile"], json!("cheap"));
    }

    #[test]
    fn routing_carries_any_number_of_named_profiles() {
        let mut catalog = Vec::new();
        for (id, profile, rank) in [
            ("a", "local-qwen", 0),
            ("b", "deepseek-r2", 1),
            ("c", "opus-max", 2),
            ("d", "gemini-pro", 3),
        ] {
            let mut p = preset(id, SkillGrade::L2, rank);
            p.execution_profile = ExecutionProfile::parse(profile);
            catalog.push(p);
        }
        for (named, profile) in [
            ("a", "local-qwen"),
            ("b", "deepseek-r2"),
            ("c", "opus-max"),
            ("d", "gemini-pro"),
        ] {
            let decision = route_preset(TaskDifficulty::L2, 0, Some(named), &catalog).unwrap();
            assert_eq!(
                decision
                    .execution_profile
                    .as_ref()
                    .map(ExecutionProfile::as_str),
                Some(profile)
            );
        }
        let cheapest = route_preset(TaskDifficulty::L2, 0, None, &catalog).unwrap();
        assert_eq!(cheapest.preset_id, "a");
        let value = serde_json::to_value(&cheapest).unwrap();
        assert_eq!(value["execution_profile"], json!("local-qwen"));
        assert!(value.get("reprobe").is_none(), "{value}");
        // Without one, a decision serializes as in phase 1.
        let plain = route_preset(
            TaskDifficulty::L2,
            0,
            None,
            &[preset("p", SkillGrade::L2, 1)],
        )
        .unwrap();
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("execution_profile").is_none(), "{value}");
    }

    #[test]
    fn registering_a_profile_keeps_the_builtins_strict_and_names_any_other() {
        assert_eq!(
            registered_profile(None, None),
            Ok((ModelProfile::Default, None))
        );
        assert_eq!(
            registered_profile(Some("cheap"), None),
            Ok((ModelProfile::Cheap, None))
        );
        assert!(registered_profile(Some("x"), None).is_err());
        assert_eq!(
            registered_profile(None, Some("opus-max")),
            Ok((ModelProfile::Default, ExecutionProfile::parse("opus-max")))
        );
        assert_eq!(
            registered_profile(Some("cheap"), Some("local-qwen")),
            Ok((ModelProfile::Cheap, ExecutionProfile::parse("local-qwen")))
        );
        // A builtin name is the model profile itself, never stored as a name.
        assert_eq!(
            registered_profile(None, Some("cheap")),
            Ok((ModelProfile::Cheap, None))
        );
        assert!(registered_profile(Some("default"), Some("cheap")).is_err());
        assert!(registered_profile(None, Some("Bad Name")).is_err());
    }

    #[test]
    fn a_brief_no_rule_reads_is_the_default_l2() {
        for brief in [
            "a",
            "fix it",
            "one",
            "Implement the thing the objective asks for.",
        ] {
            let classified = classify_difficulty(brief);
            assert_eq!(classified.difficulty, TaskDifficulty::L2, "{brief}");
            assert!(!classified.has_signal(), "{brief}: {classified:?}");
            assert_eq!(classified.score, 0);
            assert_eq!(classified.rules, CLASSIFIER_RULES);
        }
        assert_eq!(
            classify_difficulty("a").rationale(),
            "l2 (no signal: the default)"
        );
    }

    #[test]
    fn security_and_migration_markers_classify_l3() {
        let security = classify_difficulty("Harden authentication: stop the SQL injection");
        assert_eq!(security.difficulty, TaskDifficulty::L3);
        assert_eq!(security.signals[0].rule, "security");
        assert_eq!(
            security.signals[0].evidence,
            vec!["authenticat".to_string(), "injection".to_string()]
        );
        let migration = classify_difficulty("Add a column with ALTER TABLE and backfill it");
        assert_eq!(migration.difficulty, TaskDifficulty::L3);
        assert!(
            migration
                .rationale()
                .starts_with("l3 (score 4: migration +4")
        );
        // Two medium signals together reach l3 too.
        let both = classify_difficulty("Refactor the dispatcher to remove the race condition");
        assert_eq!(both.difficulty, TaskDifficulty::L3);
        assert_eq!(both.score, 4);
        // One alone does not.
        assert_eq!(
            classify_difficulty("Refactor the dispatcher loop").difficulty,
            TaskDifficulty::L2
        );
    }

    #[test]
    fn scope_and_steps_add_up() {
        let brief = "Change crates/spawn-core/src/a.rs, crates/spawn-cli/src/b.rs and \
                     crates/spawn-postgres/sql/schema.sql:\n1. one\n2. two\n3. three\n";
        let classified = classify_difficulty(brief);
        let rules: Vec<&str> = classified
            .signals
            .iter()
            .map(|signal| signal.rule.as_str())
            .collect();
        // schema.sql names a migration; three files, three crates, three steps.
        assert_eq!(
            rules,
            vec!["migration", "multi_step", "file_scope", "crate_scope"]
        );
        assert_eq!(classified.difficulty, TaskDifficulty::L3);
        let crates = classified
            .signals
            .iter()
            .find(|signal| signal.rule == "crate_scope")
            .unwrap();
        assert_eq!(
            (crates.points, crates.evidence[0].as_str()),
            (2, "3 crates")
        );
        // Steps alone stay l2.
        let steps = classify_difficulty("- a\n- b\n- c\n- d\n- e\n- f\n");
        assert_eq!(steps.score, 2);
        assert_eq!(steps.difficulty, TaskDifficulty::L2);
    }

    #[test]
    fn small_low_risk_work_classifies_l1_unless_something_risky_is_named() {
        let typo = classify_difficulty("Fix the typo in README.md");
        assert_eq!(typo.difficulty, TaskDifficulty::L1);
        assert_eq!(typo.signals[0].rule, "low_risk");
        assert_eq!(typo.score, -2);
        // A risk marker cancels the low-risk rule.
        let risky = classify_difficulty("Fix the typo in the password check");
        assert_eq!(risky.difficulty, TaskDifficulty::L3);
        assert!(risky.signals.iter().all(|signal| signal.rule != "low_risk"));
        // Too many files, or too long, is not l1.
        assert_eq!(
            classify_difficulty("Rename x in a.rs and b.rs").difficulty,
            TaskDifficulty::L2
        );
        let long = format!("Fix a typo. {}", "word ".repeat(100));
        assert_eq!(classify_difficulty(&long).difficulty, TaskDifficulty::L2);
    }

    #[test]
    fn generic_words_never_send_real_work_to_l1() {
        for brief in [
            "Add comment support to the web timeline",
            "Rename spawn_task_assign and update every caller",
            "Bump tokio and fix whatever breaks",
            "Fix the typo in every error message",
            "Implement README rendering",
        ] {
            let classified = classify_difficulty(brief);
            assert!(
                classified.difficulty >= TaskDifficulty::L2,
                "{brief}: {classified:?}"
            );
            assert!(
                classified
                    .signals
                    .iter()
                    .all(|signal| signal.rule != "low_risk"),
                "{brief}: {classified:?}"
            );
        }
        assert_eq!(
            classify_difficulty("Fix the spelling in the docstring").difficulty,
            TaskDifficulty::L1
        );
    }

    #[test]
    fn brief_v2_keeps_semantic_writing_out_of_l1() {
        for brief in [
            "Update the README wording for the install section",
            "Write the docstring for route_preset",
            "Improve the wording of the error message",
            "Docs only: explain the reprobe rule",
            "Make the one-line change in a.rs",
            "Fix the typo and reword the sentence in README.md",
            "Fix the typo in the comment",
            "Draft a summary of the formatting rules",
        ] {
            let classified = classify_difficulty(brief);
            assert_eq!(
                classified.difficulty,
                TaskDifficulty::L2,
                "{brief}: {classified:?}"
            );
            assert!(
                classified
                    .signals
                    .iter()
                    .all(|signal| signal.rule != "low_risk"),
                "{brief}: {classified:?}"
            );
        }
    }

    #[test]
    fn brief_v2_sends_mechanical_and_tagging_work_to_l1() {
        for (brief, evidence) in [
            ("Tag the 40 open tasks with their capability", "tag"),
            ("Add the registry entry for the new file", "registry entry"),
            ("Label the stale issues", "label"),
            ("Run cargo fmt on crates/spawn-core/src/grading.rs", "cargo fmt"),
            ("Strip the trailing whitespace", "whitespace"),
            ("Regenerate the fixtures", "regenerat"),
            ("A mechanical bulk rename of the constant", "mechanical"),
            ("Fix the typo in README.md", "typo"),
        ] {
            let classified = classify_difficulty(brief);
            assert_eq!(
                classified.difficulty,
                TaskDifficulty::L1,
                "{brief}: {classified:?}"
            );
            let low = classified
                .signals
                .iter()
                .find(|signal| signal.rule == "low_risk")
                .expect(brief);
            assert_eq!(low.points, -2);
            assert!(low.evidence.contains(&evidence.to_string()), "{low:?}");
        }
    }

    #[test]
    fn brief_v2_tags_and_labels_inside_real_work_stay_out_of_l1() {
        for brief in [
            "Implement label support",
            "Add tag filtering to the task list",
            "Render the labels on the web timeline",
            "Parse the tag from the commit message",
            "Add a label endpoint to the API",
            "Display each task's tags",
            "Add a bulk delete feature",
            "Store the label in the tasks table inside one transaction",
        ] {
            let classified = classify_difficulty(brief);
            assert!(
                classified.difficulty >= TaskDifficulty::L2,
                "{brief}: {classified:?}"
            );
            assert!(
                classified
                    .signals
                    .iter()
                    .all(|signal| signal.rule != "low_risk"),
                "{brief}: {classified:?}"
            );
        }
    }

    #[test]
    fn classification_is_deterministic_and_carries_no_brief_text() {
        let brief = "Rotate the credential in deploy/secret.env; tok=sk-abcdef0123456789";
        let first = classify_difficulty(brief);
        assert_eq!(first, classify_difficulty(brief));
        let value = serde_json::to_value(&first).unwrap();
        let text = value.to_string();
        assert!(!text.contains("sk-abcdef"), "{text}");
        assert!(!text.contains("deploy/secret.env"), "{text}");
        assert_eq!(value["difficulty"], json!("l3"));
        assert_eq!(value["rules"], json!(CLASSIFIER_RULES));
    }

    fn seen(id: &str, difficulty: TaskDifficulty, at: i64) -> PresetActivity {
        PresetActivity {
            preset_id: id.to_string(),
            difficulty,
            last_assigned_ms: at,
        }
    }

    #[test]
    fn a_preset_is_due_a_reprobe_only_after_an_interval_without_tasks() {
        let interval = 1000;
        let mut p = preset("p", SkillGrade::L2, 1);
        p.window_start_ms = 5_000;
        // Never assigned: counted from its window's start.
        assert!(!reprobe_due(&p, &[], 5_999, interval));
        assert!(reprobe_due(&p, &[], 6_000, interval));
        // Assigned at its grade's difficulty recently: not due.
        let recent = [seen("p", TaskDifficulty::L2, 5_500)];
        assert!(!reprobe_due(&p, &recent, 6_000, interval));
        assert!(reprobe_due(&p, &recent, 6_500, interval));
        // A task at another difficulty, or another preset's, does not count.
        let other = [
            seen("p", TaskDifficulty::L1, 5_900),
            seen("q", TaskDifficulty::L2, 5_900),
        ];
        assert!(reprobe_due(&p, &other, 6_000, interval));
        // Locked, unroutable or ungraded: never due.
        let mut locked = p.clone();
        locked.locked_grade = Some(SkillGrade::L2);
        assert!(!reprobe_due(&locked, &[], 1_000_000, interval));
        let mut off = p.clone();
        off.state = PresetState::Disabled;
        assert!(!reprobe_due(&off, &[], 1_000_000, interval));
        let mut ungraded = p.clone();
        ungraded.calibrated_grade = None;
        assert!(!reprobe_due(&ungraded, &[], 1_000_000, interval));
        assert_eq!(
            reprobe_plan(&[p, locked, off], &recent, 6_500, interval),
            vec!["p".to_string()]
        );
    }

    #[test]
    fn a_starved_demoted_preset_gets_one_probe_per_interval() {
        let interval = 1000;
        let cheap = preset("cheap-l1", SkillGrade::L1, 0);
        let mut demoted = preset("demoted", SkillGrade::L2, 5);
        demoted.calibrated_grade = Some(SkillGrade::L1);
        demoted.state = PresetState::Demoted;
        let strong = preset("strong", SkillGrade::L3, 9);
        let catalog = vec![cheap, demoted, strong];
        // Before the interval: the cheapest l1, as route_preset says.
        let early = route_with_reprobe(TaskDifficulty::L1, &catalog, &[], 999, interval).unwrap();
        assert_eq!(early.preset_id, "cheap-l1");
        assert!(!early.reprobe);
        // After it: the starved one, flagged.
        let probe = route_with_reprobe(TaskDifficulty::L1, &catalog, &[], 1_000, interval).unwrap();
        assert_eq!(probe.preset_id, "demoted");
        assert!(probe.reprobe);
        assert!(!probe.explicit);
        assert_eq!(probe.preset_grade, SkillGrade::L1);
        assert_eq!(probe.cost_rank, 5);
        assert_eq!(
            serde_json::to_value(&probe).unwrap()["reprobe"],
            json!(true)
        );
        // Given that task, it is not due again within the interval: bounded.
        let after = [seen("demoted", TaskDifficulty::L1, 1_000)];
        assert_eq!(
            route_with_reprobe(TaskDifficulty::L1, &catalog, &after, 1_500, interval)
                .unwrap()
                .preset_id,
            "cheap-l1"
        );
        // The same facts decide the same way: idempotent.
        assert_eq!(
            route_with_reprobe(TaskDifficulty::L1, &catalog, &after, 1_500, interval),
            route_with_reprobe(TaskDifficulty::L1, &catalog, &after, 1_500, interval)
        );
        // An l3 preset graded above an l2 task is never probed with it: the
        // task would not land in its window.
        assert_eq!(
            route_with_reprobe(TaskDifficulty::L2, &catalog, &[], 10_000, interval)
                .unwrap()
                .preset_id,
            "strong"
        );
    }

    #[test]
    fn the_longest_starved_preset_is_probed_first() {
        let interval = 100;
        let catalog = vec![
            preset("base", SkillGrade::L2, 0),
            preset("x", SkillGrade::L2, 3),
            preset("y", SkillGrade::L2, 4),
        ];
        let activity = [
            seen("x", TaskDifficulty::L2, 500),
            seen("y", TaskDifficulty::L2, 200),
        ];
        let decision =
            route_with_reprobe(TaskDifficulty::L2, &catalog, &activity, 1_000, interval).unwrap();
        assert_eq!(decision.preset_id, "y");
        // With nothing routable the refusal is route_preset's.
        assert!(route_with_reprobe(TaskDifficulty::L3, &catalog, &[], 1_000, interval).is_err());
    }

    #[test]
    fn only_a_gates_rejected_is_a_quality_rejection() {
        assert!(is_quality_rejection("rejected"));
        for verdict in [
            "accepted",
            "conflict",
            "incompatible",
            "host_fault",
            "no_room",
            "aborted",
            "stale",
            "",
        ] {
            assert!(!is_quality_rejection(verdict), "{verdict}");
        }
    }
}
