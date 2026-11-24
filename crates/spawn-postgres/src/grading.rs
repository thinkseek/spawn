//! Agent grading's store (docs/agent-grading.md): the grading catalog
//! (`grading_presets`, the fold of the control session's
//! `grading.recorded` events), the facts routing and calibration decide
//! on, and the records they make.
//!
//! The rules themselves are `spawn_core::grading`'s, pure; this module
//! reads what they need and records what they decide, through the checked
//! SQL functions (`spawn_grading_record`, `spawn_task_assign`,
//! `spawn_task_reroute`), which validate against the tables under the
//! append lock.

use serde::Serialize;
use serde_json::Value;
use spawn_core::event::ModelProfile;
use spawn_core::grading::{
    CALIBRATION_WINDOW, GradedPreset, PresetState, SkillGrade, TaskDifficulty, TaskOutcomeSample,
};
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// Every graded preset, by id.
const CATALOG: &str = "select preset_id, engine_label, model_label, model_profile, cost_rank,
                              state, onboarded_grade, calibrated_grade, locked_grade,
                              window_start_ms, created_at_ms, updated_at_ms, last_event
                         from grading_presets order by preset_id";

/// A preset's calibration window: the tasks first assigned to preset `$1`
/// (`first_preset_id`, which a rework reroute never moves: a task counts
/// for the preset it was assigned to, never the stronger one that took its
/// rework) at difficulty `$2`, assigned since `$3` (its window start),
/// closed, newest first, at most [`CALIBRATION_WINDOW`] (`$4`), through
/// `mission_tasks_first_preset`.
///
/// Closed: landed or accepted; or cancelled after at least one quality
/// rejection, unless a re-issue or reassign carried its work (and its
/// rejections) on to a successor, which counts instead. A cancel with no
/// quality rejection (a re-issue, a reassign, over budget, aborted, its
/// mission closed, its resident retired, a person's clean cancel) says
/// nothing of the preset and is left out. First pass: landed or accepted
/// with no quality rejection; a gate's `conflict` or `incompatible` is no
/// quality rejection.
const OUTCOMES: &str = r"
select k.id, (k.state in ('landed', 'accepted') and k.quality_rejections = 0) as first_pass
  from mission_tasks k
 where k.first_preset_id = $1
   and k.difficulty = $2
   and k.created_at_ms >= $3
   and (k.state in ('landed', 'accepted')
        or (k.state = 'cancelled' and k.quality_rejections > 0
            and not exists (select 1 from mission_tasks s
                             where s.reissue_of = k.id or s.reassign_of = k.id)))
 order by k.updated_at_ms desc, k.id desc
 limit $4";

/// What a rework reroute reads of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutedTask {
    pub task: i64,
    pub state: String,
    pub revisions: i32,
    /// Sends-back for quality (a review's `revise`, a gate's `rejected`).
    pub quality_rejections: i32,
    pub difficulty: TaskDifficulty,
    pub preset_id: Option<String>,
    pub first_preset_id: Option<String>,
    pub model: String,
}

fn grade(rank: Option<i16>) -> Result<Option<SkillGrade>, StoreError> {
    rank.map(|rank| {
        SkillGrade::from_rank(i64::from(rank))
            .ok_or_else(|| StoreError::Backend(format!("a stored grade of {rank}")))
    })
    .transpose()
}

fn preset(row: &postgres::Row) -> Result<GradedPreset, StoreError> {
    let model: String = row.get("model_profile");
    let state: String = row.get("state");
    let cost_rank: i32 = row.get("cost_rank");
    Ok(GradedPreset {
        preset_id: row.get("preset_id"),
        engine_label: row.get("engine_label"),
        model_label: row.get("model_label"),
        model_profile: ModelProfile::parse(&model)
            .ok_or_else(|| StoreError::Backend(format!("a stored model profile {model}")))?,
        cost_rank: u32::try_from(cost_rank)
            .map_err(|_| StoreError::Backend(format!("a stored cost rank {cost_rank}")))?,
        state: PresetState::parse(&state)
            .ok_or_else(|| StoreError::Backend(format!("a stored preset state {state}")))?,
        onboarded_grade: grade(row.get("onboarded_grade"))?,
        calibrated_grade: grade(row.get("calibrated_grade"))?,
        locked_grade: grade(row.get("locked_grade"))?,
        window_start_ms: row.get("window_start_ms"),
        created_at_ms: row.get("created_at_ms"),
        updated_at_ms: row.get("updated_at_ms"),
        last_event: row.get("last_event"),
    })
}

impl PostgresStore {
    /// The whole grading catalog, by preset id.
    pub fn grading_catalog(&self) -> Result<Vec<GradedPreset>, StoreError> {
        self.query(CATALOG, &[])?.iter().map(preset).collect()
    }

    /// Record one grading change (`spawn_grading_record`, owner only): a
    /// `preset.*` record, checked against the catalog and appended as one
    /// `grading.recorded` event of the control session, folded into
    /// `grading_presets` in the same transaction. Its free text is masked
    /// first. Returns the event id.
    pub fn grading_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_grading_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// Rebuild `grading_presets` from its log (`spawn_grading_refold`).
    pub fn grading_refold(&self) -> Result<(), StoreError> {
        self.execute("select spawn_grading_refold()", &[])?;
        Ok(())
    }

    /// `preset`'s calibration window ([`OUTCOMES`]): its closed tasks at
    /// `difficulty` assigned at or after `since_ms`, newest first.
    pub fn grading_outcomes(
        &self,
        preset: &str,
        difficulty: TaskDifficulty,
        since_ms: i64,
    ) -> Result<Vec<TaskOutcomeSample>, StoreError> {
        let window = i64::try_from(CALIBRATION_WINDOW).unwrap_or(i64::MAX);
        Ok(self
            .query(OUTCOMES, &[&preset, &difficulty.rank(), &since_ms, &window])?
            .iter()
            .map(|row| TaskOutcomeSample {
                task_id: row.get(0),
                first_pass: row.get(1),
            })
            .collect())
    }

    /// What a rework reroute reads of `task`, or none for no such task.
    pub fn routed_task(&self, task: i64) -> Result<Option<RoutedTask>, StoreError> {
        self.query_opt(
            "select state, revisions, quality_rejections, difficulty, preset_id,
                    first_preset_id, model
               from mission_tasks where id = $1",
            &[&task],
        )?
        .map(|row| {
            let rank: i16 = row.get("difficulty");
            Ok(RoutedTask {
                task,
                state: row.get("state"),
                revisions: row.get("revisions"),
                quality_rejections: row.get("quality_rejections"),
                difficulty: TaskDifficulty::from_rank(i64::from(rank))
                    .ok_or_else(|| StoreError::Backend(format!("a stored difficulty {rank}")))?,
                preset_id: row.get("preset_id"),
                first_preset_id: row.get("first_preset_id"),
                model: row.get("model"),
            })
        })
        .transpose()
    }

    /// As [`Self::task_assign_with`], at `difficulty` and on the graded
    /// preset `preset` (routed by the caller; `model` its model profile),
    /// or on none.
    #[allow(clippy::too_many_arguments)]
    pub fn task_assign_routed(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
        model: &str,
        again: bool,
        pin: bool,
        difficulty: TaskDifficulty,
        preset: Option<&str>,
    ) -> Result<i64, StoreError> {
        let row = self.query_one(
            "select spawn_task_assign($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &mission,
                &kind,
                &worker,
                &crate::missions::masked(brief),
                &model,
                &again,
                &pin,
                &i32::from(difficulty.rank()),
                &preset,
            ],
        )?;
        Ok(row.get(0))
    }

    /// Send `task` back with `note` (as [`Self::task_review`]) and, in the
    /// same statement, reroute its next revision to `preset`
    /// (`spawn_task_reroute`): routed for `routed_revision`, the revision
    /// the task is at once sent back, at `routed_difficulty`. Either both
    /// are recorded or neither. Returns the `task.rerouted` record.
    pub fn task_reject_rerouted(
        &self,
        task: i64,
        note: &str,
        revision: Option<i32>,
        preset: &str,
        routed_revision: i32,
        routed_difficulty: TaskDifficulty,
    ) -> Result<Value, StoreError> {
        self.query_json(
            "with reviewed as (select spawn_task_review($1, false, $2, $3) as done)
             select spawn_task_reroute($1, $4, $5, $6) from reviewed",
            &[
                &task,
                &crate::missions::masked(note),
                &revision,
                &preset,
                &routed_revision,
                &i32::from(routed_difficulty.rank()),
            ],
        )
    }
}
