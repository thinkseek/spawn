// Read model for `/api/grading`: normalizes the grading catalog and groups
// presets by the top-down 3-tier model stack (`T3 top-decision`, `T2 mainline`,
// `T1 mechanical-offload`), degrading gracefully when fields are absent.
// Pure module with no DOM or fetch imports so `node --test` runs it directly.

export type SkillGrade = "l1" | "l2" | "l3";
export type PresetState = "unexamined" | "active" | "demoted" | "disabled" | "unknown";
export type TierRole = "top-decision" | "mainline" | "mechanical-offload";
export type TopDownTierId = "t3" | "t2" | "t1" | "ungraded";

export interface GradingPreset {
  preset_id: string;
  engine_label: string;
  model_label: string;
  model_profile: string;
  execution_profile: string | null;
  profile: string;
  driver: string;
  label_only: boolean;
  cost_rank: number;
  state: PresetState;
  raw_state: string | null;
  onboarded_grade: SkillGrade | null;
  calibrated_grade: SkillGrade | null;
  locked_grade: SkillGrade | null;
  routing_grade: SkillGrade | null;
  tier_role: TierRole | null;
  reprobe_due: boolean;
  window_start_ms: number | null;
}

export interface GradingCatalog {
  presets: GradingPreset[];
  profiles: string[];
}

export interface TopDownTierGroup {
  id: TopDownTierId;
  label: string;
  grade: SkillGrade | null;
  tierRole: TierRole | null;
  subtitle: string;
  presets: GradingPreset[];
}

export interface TaskGradingInput {
  difficulty?: SkillGrade | null;
  preset_id?: string | null;
  first_preset_id?: string | null;
  tier_role?: TierRole | null;
  quality_rejections?: number | null;
  execution_profile?: string | null;
}

export interface TaskGradingSummary {
  difficulty: SkillGrade | null;
  tierRole: TierRole | null;
  presetId: string | null;
  firstPresetId: string | null;
  rerouted: boolean;
  qualityRejections: number;
  executionProfile: string | null;
}

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

export function normalizeSkillGrade(value: unknown): SkillGrade | null {
  if (typeof value !== "string") return null;
  const key = value.toLowerCase();
  return key === "l1" || key === "l2" || key === "l3" ? key : null;
}

export function normalizeTierRole(value: unknown): TierRole | null {
  if (typeof value !== "string") return null;
  const key = value.toLowerCase();
  return key === "top-decision" || key === "mainline" || key === "mechanical-offload" ? key : null;
}

export function tierRoleForGrade(grade: SkillGrade | null): TierRole | null {
  switch (grade) {
    case "l3":
      return "top-decision";
    case "l2":
      return "mainline";
    case "l1":
      return "mechanical-offload";
    default:
      return null;
  }
}

function normalizeState(value: unknown): PresetState {
  if (typeof value !== "string") return "unknown";
  const key = value.toLowerCase();
  return key === "unexamined" || key === "active" || key === "demoted" || key === "disabled"
    ? key
    : "unknown";
}

function normalizePreset(value: unknown): GradingPreset | null {
  if (!isObject(value)) return null;
  const preset_id = optionalText(value.preset_id);
  if (preset_id === null) return null;
  const state = normalizeState(value.state);
  const onboarded_grade = normalizeSkillGrade(value.onboarded_grade);
  const calibrated_grade = normalizeSkillGrade(value.calibrated_grade);
  const locked_grade = normalizeSkillGrade(value.locked_grade);
  const routable = state === "active" || state === "demoted";
  const derivedRoutingGrade = routable ? (locked_grade ?? calibrated_grade) : null;
  const routing_grade =
    "routing_grade" in value ? normalizeSkillGrade(value.routing_grade) : derivedRoutingGrade;
  const engine_label = optionalText(value.engine_label) ?? "claude";
  const model_label = optionalText(value.model_label) ?? preset_id;
  const model_profile = optionalText(value.model_profile) ?? "default";
  const execution_profile = optionalText(value.execution_profile);
  const profile = optionalText(value.profile) ?? execution_profile ?? model_profile;
  const tier_role = normalizeTierRole(value.tier_role) ?? tierRoleForGrade(routing_grade);
  const cost_rank = Math.max(0, Math.trunc(optionalNumber(value.cost_rank) ?? 0));
  return {
    preset_id,
    engine_label,
    model_label,
    model_profile,
    execution_profile,
    profile,
    driver: optionalText(value.driver) ?? engine_label,
    label_only: value.label_only === true,
    cost_rank,
    state,
    raw_state: optionalText(value.state),
    onboarded_grade,
    calibrated_grade,
    locked_grade,
    routing_grade,
    tier_role,
    reprobe_due: value.reprobe_due === true,
    window_start_ms: optionalNumber(value.window_start_ms),
  };
}

export function normalizeGrading(raw: unknown): GradingCatalog {
  const obj = isObject(raw) ? raw : {};
  const rawPresets = Array.isArray(obj.presets) ? obj.presets : [];
  const presets = rawPresets
    .map(normalizePreset)
    .filter((preset): preset is GradingPreset => preset !== null);
  const rawProfiles = Array.isArray(obj.profiles)
    ? obj.profiles.filter((entry): entry is string => typeof entry === "string" && entry !== "")
    : [];
  const profiles =
    rawProfiles.length > 0
      ? [...new Set(rawProfiles)].sort()
      : [...new Set(presets.map((preset) => preset.profile))].sort();
  return { presets, profiles };
}

const STATE_ORDER: Record<PresetState, number> = {
  active: 0,
  demoted: 1,
  disabled: 2,
  unexamined: 3,
  unknown: 4,
};

function sortPresets(presets: GradingPreset[]): GradingPreset[] {
  return [...presets].sort(
    (left, right) =>
      STATE_ORDER[left.state] - STATE_ORDER[right.state] ||
      left.cost_rank - right.cost_rank ||
      left.preset_id.localeCompare(right.preset_id),
  );
}

export function groupByTopDownTier(presets: ReadonlyArray<GradingPreset>): TopDownTierGroup[] {
  const buckets: Record<TopDownTierId, GradingPreset[]> = {
    t3: [],
    t2: [],
    t1: [],
    ungraded: [],
  };

  for (const preset of presets) {
    const grade =
      preset.routing_grade ??
      preset.locked_grade ??
      preset.calibrated_grade ??
      preset.onboarded_grade;
    if (grade === "l3") buckets.t3.push(preset);
    else if (grade === "l2") buckets.t2.push(preset);
    else if (grade === "l1") buckets.t1.push(preset);
    else buckets.ungraded.push(preset);
  }

  const groups: TopDownTierGroup[] = [
    {
      id: "t3",
      label: "T3 · Top Decision",
      grade: "l3",
      tierRole: "top-decision",
      subtitle:
        "Major architecture, security, credentials, sandbox, schema migrations, and L2 rework escalation fallback (claude-fable-5-2).",
      presets: sortPresets(buckets.t3),
    },
    {
      id: "t2",
      label: "T2 · Mainline",
      grade: "l2",
      tierRole: "mainline",
      subtitle:
        "Supervisor and default strong execution mainline; also absorbs L1 upward fallback when T1 is absent (gemini-pro-orchestrator-neon).",
      presets: sortPresets(buckets.t2),
    },
    {
      id: "t1",
      label: "T1 · Mechanical Offload",
      grade: "l1",
      tierRole: "mechanical-offload",
      subtitle:
        "Opt-in strictly deterministic low-risk mechanical work only (deepseek-v4 / deepseek-v4.1-flash / kimi); escalates to T2 on first rejection.",
      presets: sortPresets(buckets.t1),
    },
  ];

  if (buckets.ungraded.length > 0) {
    groups.push({
      id: "ungraded",
      label: "Unexamined / Ungraded",
      grade: null,
      tierRole: null,
      subtitle: "Registered presets without an onboarded or locked skill grade yet.",
      presets: sortPresets(buckets.ungraded),
    });
  }

  return groups;
}

export type BadgeTone = "neutral" | "info" | "success" | "warning" | "danger" | "purple";

export function gradeTone(grade: SkillGrade | null): BadgeTone {
  switch (grade) {
    case "l3":
      return "purple";
    case "l2":
      return "info";
    default:
      return "neutral";
  }
}

export function presetStateTone(state: PresetState): BadgeTone {
  switch (state) {
    case "active":
      return "success";
    case "demoted":
      return "warning";
    case "disabled":
      return "danger";
    default:
      return "neutral";
  }
}

export function taskGradingSummary(task: TaskGradingInput): TaskGradingSummary | null {
  const difficulty = normalizeSkillGrade(task.difficulty);
  const presetId = optionalText(task.preset_id);
  const firstPresetId = optionalText(task.first_preset_id);
  const tierRole = normalizeTierRole(task.tier_role) ?? tierRoleForGrade(difficulty);
  const qualityRejections = Math.max(0, Math.trunc(optionalNumber(task.quality_rejections) ?? 0));
  const executionProfile = optionalText(task.execution_profile);
  if (
    difficulty === null &&
    presetId === null &&
    firstPresetId === null &&
    tierRole === null &&
    executionProfile === null &&
    qualityRejections === 0
  ) {
    return null;
  }
  const rerouted = firstPresetId !== null && presetId !== null && firstPresetId !== presetId;
  return {
    difficulty,
    tierRole,
    presetId,
    firstPresetId,
    rerouted,
    qualityRejections,
    executionProfile,
  };
}
