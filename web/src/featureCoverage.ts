// Pure read model for Feature Map file coverage & one-click unassociated-file
// ("野文件") scan (t559u Slice B / t565u). No fetch, no DOM — testable under
// `node --test`.

export type FeatureKind =
  | "domain"
  | "capability"
  | "initiative"
  | "slice"
  | "research-initiative"
  | "subsystem";

export type RelationKind =
  | "depends_on"
  | "blocks"
  | "implements"
  | "extends"
  | "relates_to"
  | "supersedes"
  | "verifies"
  | "documented_by"
  | "contains";

export interface ExclusionRule {
  id: string;
  pattern: string;
  category: string;
  reason: string;
}

export interface BoundaryPolicy {
  project_name: string;
  description: string;
  included_categories: string[];
  exclusions: ExclusionRule[];
}

export interface FeatureRelation {
  kind: RelationKind;
  target: string;
  note: string | null;
}

export interface FeatureProvenance {
  objectives: string[];
  tasks: string[];
  agents: string[];
  commits: string[];
  tests: string[];
  deploys: string[];
  artifacts: string[];
  discussions: string[];
}

export interface CoverageFeatureSummary {
  id: string;
  title: string;
  kind: FeatureKind;
  parent: string | null;
  summary: string;
  status_note: string | null;
  file_count: number;
  relations: FeatureRelation[];
  provenance: FeatureProvenance;
}

export interface ScanCounts {
  total_seen: number;
  in_scope: number;
  covered: number;
  unassociated_wild: number;
  stale_references: number;
  explicit_exclusions: number;
  multi_feature_files: number;
  orphan_features: number;
  invalid_relations: number;
}

export interface WildFileFinding {
  path: string;
  tracked: boolean;
  size_bytes: number;
  reason: string;
  evidence: string[];
  suggested_features: string[];
}

export interface StaleFileReference {
  path: string;
  referencing_features: string[];
  rename_candidates: string[];
}

export interface ExcludedFileEntry {
  path: string;
  rule_id: string;
  category: string;
  reason: string;
}

export interface BidirectionalIndex {
  feature_to_files: Record<string, string[]>;
  file_to_features: Record<string, string[]>;
}

export interface ConsistencyReport {
  orphan_features: string[];
  dangling_relations: string[];
  self_links: string[];
  cycle_errors: string[];
  duplicate_feature_ids: string[];
  invalid_paths: string[];
}

export interface ScanReport {
  schema_version: number;
  scanned_at_ms: number;
  initiative_id: string;
  boundary: BoundaryPolicy;
  counts: ScanCounts;
  unassociated_files: WildFileFinding[];
  stale_references: StaleFileReference[];
  excluded_files: ExcludedFileEntry[];
  bidirectional_index: BidirectionalIndex;
  consistency: ConsistencyReport;
}

export interface AssociationUpdate {
  path: string;
  added_to_features: string[];
  already_associated_features: string[];
  all_associated_features: string[];
}

export interface FeatureCoverageResponse {
  root: string;
  registry_path: string;
  /** Whether the server can save associations into the registry. `false` on
   * a deployed controller tree, which is read-only. */
  registry_writable: boolean;
  /** Why the registry is read-only, when `registry_writable` is `false`. */
  read_only_reason: string | null;
  report: ScanReport;
  features: CoverageFeatureSummary[];
  update: AssociationUpdate | null;
}

type JsonObject = Record<string, unknown>;

function obj(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function arr(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function str(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function optStr(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function num(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function strList(value: unknown): string[] {
  return arr(value).filter((entry): entry is string => typeof entry === "string");
}

function strRecord(value: unknown): Record<string, string[]> {
  const raw = obj(value);
  const out: Record<string, string[]> = {};
  for (const [key, val] of Object.entries(raw)) {
    out[key] = strList(val);
  }
  return out;
}

const FEATURE_KINDS = new Set<FeatureKind>([
  "domain",
  "capability",
  "initiative",
  "slice",
  "research-initiative",
  "subsystem",
]);

function featureKind(value: unknown): FeatureKind {
  if (typeof value === "string" && FEATURE_KINDS.has(value as FeatureKind)) {
    return value as FeatureKind;
  }
  return "capability";
}

const RELATION_KINDS = new Set<RelationKind>([
  "depends_on",
  "blocks",
  "implements",
  "extends",
  "relates_to",
  "supersedes",
  "verifies",
  "documented_by",
  "contains",
]);

function relationKind(value: unknown): RelationKind {
  if (typeof value === "string" && RELATION_KINDS.has(value as RelationKind)) {
    return value as RelationKind;
  }
  return "relates_to";
}

function normalizeBoundary(value: unknown): BoundaryPolicy {
  const raw = obj(value);
  return {
    project_name: str(raw.project_name, "Spawn"),
    description: str(raw.description),
    included_categories: strList(raw.included_categories),
    exclusions: arr(raw.exclusions).map((entry) => {
      const rule = obj(entry);
      return {
        id: str(rule.id),
        pattern: str(rule.pattern),
        category: str(rule.category, "excluded"),
        reason: str(rule.reason),
      };
    }),
  };
}

function normalizeCounts(value: unknown): ScanCounts {
  const raw = obj(value);
  return {
    total_seen: num(raw.total_seen),
    in_scope: num(raw.in_scope),
    covered: num(raw.covered),
    unassociated_wild: num(raw.unassociated_wild),
    stale_references: num(raw.stale_references),
    explicit_exclusions: num(raw.explicit_exclusions),
    multi_feature_files: num(raw.multi_feature_files),
    orphan_features: num(raw.orphan_features),
    invalid_relations: num(raw.invalid_relations),
  };
}

function normalizeConsistency(value: unknown): ConsistencyReport {
  const raw = obj(value);
  return {
    orphan_features: strList(raw.orphan_features),
    dangling_relations: strList(raw.dangling_relations),
    self_links: strList(raw.self_links),
    cycle_errors: strList(raw.cycle_errors),
    duplicate_feature_ids: strList(raw.duplicate_feature_ids),
    invalid_paths: strList(raw.invalid_paths),
  };
}

export function normalizeScanReport(value: unknown): ScanReport {
  const raw = obj(value);
  const bi = obj(raw.bidirectional_index);
  return {
    schema_version: num(raw.schema_version, 1),
    scanned_at_ms: num(raw.scanned_at_ms),
    initiative_id: str(raw.initiative_id, "t559u"),
    boundary: normalizeBoundary(raw.boundary),
    counts: normalizeCounts(raw.counts),
    unassociated_files: arr(raw.unassociated_files).map((entry) => {
      const item = obj(entry);
      return {
        path: str(item.path),
        tracked: item.tracked === true,
        size_bytes: num(item.size_bytes),
        reason: str(item.reason),
        evidence: strList(item.evidence),
        suggested_features: strList(item.suggested_features),
      };
    }),
    stale_references: arr(raw.stale_references).map((entry) => {
      const item = obj(entry);
      return {
        path: str(item.path),
        referencing_features: strList(item.referencing_features),
        rename_candidates: strList(item.rename_candidates),
      };
    }),
    excluded_files: arr(raw.excluded_files).map((entry) => {
      const item = obj(entry);
      return {
        path: str(item.path),
        rule_id: str(item.rule_id),
        category: str(item.category),
        reason: str(item.reason),
      };
    }),
    bidirectional_index: {
      feature_to_files: strRecord(bi.feature_to_files),
      file_to_features: strRecord(bi.file_to_features),
    },
    consistency: normalizeConsistency(raw.consistency),
  };
}

function normalizeFeatureSummary(value: unknown): CoverageFeatureSummary {
  const raw = obj(value);
  const prov = obj(raw.provenance);
  return {
    id: str(raw.id),
    title: str(raw.title),
    kind: featureKind(raw.kind),
    parent: optStr(raw.parent),
    summary: str(raw.summary),
    status_note: optStr(raw.status_note),
    file_count: num(raw.file_count),
    relations: arr(raw.relations).map((entry) => {
      const rel = obj(entry);
      return {
        kind: relationKind(rel.kind),
        target: str(rel.target),
        note: optStr(rel.note),
      };
    }),
    provenance: {
      objectives: strList(prov.objectives),
      tasks: strList(prov.tasks),
      agents: strList(prov.agents),
      commits: strList(prov.commits),
      tests: strList(prov.tests),
      deploys: strList(prov.deploys),
      artifacts: strList(prov.artifacts),
      discussions: strList(prov.discussions),
    },
  };
}

function normalizeUpdate(value: unknown): AssociationUpdate | null {
  if (value === null || typeof value !== "object") return null;
  const raw = obj(value);
  const path = str(raw.path);
  if (!path) return null;
  return {
    path,
    added_to_features: strList(raw.added_to_features),
    already_associated_features: strList(raw.already_associated_features),
    all_associated_features: strList(raw.all_associated_features),
  };
}

export function normalizeFeatureCoverage(value: unknown): FeatureCoverageResponse {
  const raw = obj(value);
  return {
    root: str(raw.root),
    registry_path: str(raw.registry_path),
    // Only an explicit `false` means read-only: a server that predates the
    // field still accepts associations.
    registry_writable: raw.registry_writable !== false,
    read_only_reason: raw.registry_writable === false ? optStr(raw.read_only_reason) : null,
    report: normalizeScanReport(raw.report),
    features: arr(raw.features).map(normalizeFeatureSummary),
    update: normalizeUpdate(raw.update),
  };
}

function isPlainObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Strict validator for HTTP coverage responses. Rejects non-object payloads
 * or missing required report/count/collection fields so a malformed or non-JSON
 * 200 response can NEVER render as a false "Zero drift" clean bill. */
export function parseFeatureCoverageResponse(value: unknown): FeatureCoverageResponse {
  if (!isPlainObject(value)) {
    throw new Error("Invalid coverage response: expected a JSON object");
  }
  if (!isPlainObject(value.report)) {
    throw new Error("Invalid coverage response: missing report object");
  }
  if (!Array.isArray(value.features)) {
    throw new Error("Invalid coverage response: missing features array");
  }
  const report = value.report;
  if (!isPlainObject(report.counts)) {
    throw new Error("Invalid coverage response: missing report.counts");
  }
  for (const key of [
    "total_seen",
    "in_scope",
    "covered",
    "unassociated_wild",
    "stale_references",
    "explicit_exclusions",
  ]) {
    const metric = report.counts[key];
    if (typeof metric !== "number" || !Number.isFinite(metric)) {
      throw new Error(`Invalid coverage response: missing numeric report.counts.${key}`);
    }
  }
  if (
    !isPlainObject(report.boundary) ||
    !isPlainObject(report.consistency) ||
    !isPlainObject(report.bidirectional_index) ||
    !Array.isArray(report.unassociated_files) ||
    !Array.isArray(report.stale_references) ||
    !Array.isArray(report.excluded_files)
  ) {
    throw new Error("Invalid coverage response: incomplete report payload");
  }
  return normalizeFeatureCoverage(value);
}

export interface HttpJsonResponseLike {
  ok: boolean;
  status: number;
  json(): Promise<unknown>;
}

/** Parse and validate an HTTP response from `/api/features/{scan,associate}`.
 * Never swallows JSON parse failures on HTTP 200 responses. */
export async function parseCoverageHttpResponse(
  response: HttpJsonResponseLike,
  makeError: (status: number, detail: string) => Error,
): Promise<FeatureCoverageResponse> {
  if (!response.ok) {
    let detail = "";
    try {
      const payload = await response.json();
      if (isRegistryReadOnlyRefusal(response.status, payload)) {
        detail =
          str(payload.error) || str(payload.read_only_reason) || DEFAULT_READ_ONLY_REASON;
      } else if (isPlainObject(payload) && typeof payload.error === "string") {
        detail = payload.error;
      }
    } catch {
      // Non-JSON error body: HTTP status carries the failure.
    }
    throw makeError(response.status, detail);
  }
  let payload: unknown;
  try {
    payload = await response.json();
  } catch {
    throw makeError(
      response.status,
      "server returned a non-JSON body where a coverage scan JSON payload was expected",
    );
  }
  return parseFeatureCoverageResponse(payload);
}

/** Initial feature selection for a wild-file row: ALWAYS empty so suggested
 * heuristics are never pre-checked or silently submitted without explicit
 * user selection. */
export function initialWildFileSelection(_finding?: WildFileFinding): string[] {
  return [];
}

/** Toggle one feature ID in a wild-file row's explicit user selection. */
export function toggleWildFileSelection(
  selected: readonly string[],
  featureId: string,
): string[] {
  return selected.includes(featureId)
    ? selected.filter((item) => item !== featureId)
    : [...selected, featureId];
}

/** Linking requires at least one explicitly selected feature, no in-flight
 * save, and a registry the server can write (never on a read-only deployed
 * controller). */
export function canSubmitWildFileAssociation(
  selected: readonly string[],
  saving = false,
  registryWritable = true,
): boolean {
  return registryWritable && selected.length > 0 && !saving;
}

/** Error code of the structured `409` answer from `POST
 * /api/features/associate` when the registry is read-only. */
export const REGISTRY_READ_ONLY_CODE = "registry_read_only";

const DEFAULT_READ_ONLY_REASON =
  "The feature registry on this server is read-only, so associations cannot be saved here.";

/** Banner text for a read-only registry (a deployed controller tree), or
 * `null` when associations can be saved. */
export function registryReadOnlyNotice(
  data: Pick<FeatureCoverageResponse, "registry_writable" | "read_only_reason">,
): string | null {
  if (data.registry_writable) return null;
  const reason = data.read_only_reason?.trim();
  return reason ? reason : DEFAULT_READ_ONLY_REASON;
}

/** Whether an HTTP answer is the associate route's read-only refusal. */
export function isRegistryReadOnlyRefusal(
  status: number,
  payload: unknown,
): payload is Record<string, unknown> {
  return (
    status === 409 && isPlainObject(payload) && payload.code === REGISTRY_READ_ONLY_CODE
  );
}

export function consistencyIssueCount(consistency: ConsistencyReport): number {
  return (
    consistency.orphan_features.length +
    consistency.dangling_relations.length +
    consistency.self_links.length +
    consistency.cycle_errors.length +
    consistency.duplicate_feature_ids.length +
    consistency.invalid_paths.length
  );
}

export function isScanClean(report: ScanReport): boolean {
  return (
    report.unassociated_files.length === 0 &&
    report.stale_references.length === 0 &&
    consistencyIssueCount(report.consistency) === 0
  );
}

/** Verify the strict coverage accounting identities hold on a scan report:
 * - covered + unassociated_wild === in_scope
 * - in_scope + explicit_exclusions === total_seen
 * - array lengths match counters
 * - no excluded path appears in covered or wild collections. */
export function accountingIdentitiesHold(report: ScanReport): boolean {
  const c = report.counts;
  if (c.covered + c.unassociated_wild !== c.in_scope) return false;
  if (c.in_scope + c.explicit_exclusions !== c.total_seen) return false;
  if (report.unassociated_files.length !== c.unassociated_wild) return false;
  if (report.stale_references.length !== c.stale_references) return false;
  if (report.excluded_files.length !== c.explicit_exclusions) return false;
  if (Object.keys(report.bidirectional_index.file_to_features).length !== c.covered) {
    return false;
  }
  const wildSet = new Set(report.unassociated_files.map((item) => item.path));
  for (const excluded of report.excluded_files) {
    if (wildSet.has(excluded.path)) return false;
    if (excluded.path in report.bidirectional_index.file_to_features) return false;
  }
  return true;
}

export function fileAssociatedFeatures(report: ScanReport, path: string): string[] {
  return report.bidirectional_index.file_to_features[path] ?? [];
}

export function featureAssociatedFiles(report: ScanReport, featureId: string): string[] {
  return report.bidirectional_index.feature_to_files[featureId] ?? [];
}

export function isResearchInitiative(feature: CoverageFeatureSummary): boolean {
  return feature.kind === "research-initiative";
}

export function initiativesAndSlices(
  features: CoverageFeatureSummary[],
): CoverageFeatureSummary[] {
  return features.filter(
    (feature) =>
      feature.kind === "initiative" ||
      feature.kind === "slice" ||
      feature.kind === "research-initiative" ||
      feature.id.startsWith("t559u") ||
      feature.id.startsWith("t565u") ||
      feature.id.startsWith("t567u"),
  );
}

export function kindLabel(kind: FeatureKind): string {
  switch (kind) {
    case "domain":
      return "Domain";
    case "capability":
      return "Capability";
    case "initiative":
      return "Initiative";
    case "slice":
      return "Slice";
    case "research-initiative":
      return "Research / design";
    case "subsystem":
      return "Subsystem";
  }
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

/** A coverage route for one mission's project (`?mission=<id>`), or the
 * server's default project when `mission` is null. */
export function coverageUrl(path: string, mission: number | null): string {
  return mission === null ? path : `${path}?mission=${encodeURIComponent(String(mission))}`;
}

/** One project the coverage page can read: a mission with a `--features`
 * registry file configured. */
export interface CoverageProject {
  mission: number;
  label: string;
  registry: string;
}

/** The projects of `/api/features`' missions whose map is a readable
 * coverage registry file (`mapping.kind` "file" with `mapping.registry`; a
 * legacy identity-only map is left out), in the order given. */
export function coverageProjects(
  missions: ReadonlyArray<{
    id: number;
    text: string;
    mapping: { kind: string; path: string | null; registry: boolean };
  }>,
): CoverageProject[] {
  return missions
    .filter(
      (mission) =>
        mission.mapping.kind === "file" &&
        mission.mapping.registry === true &&
        typeof mission.mapping.path === "string" &&
        mission.mapping.path !== "",
    )
    .map((mission) => {
      const title = mission.text.split("\n")[0]?.trim() ?? "";
      const short = title.length > 48 ? `${title.slice(0, 47)}…` : title;
      return {
        mission: mission.id,
        label: short ? `Mission #${mission.id} — ${short}` : `Mission #${mission.id}`,
        registry: mission.mapping.path as string,
      };
    });
}

/** What the coverage page shows: one mission's project (null: the server's
 * default) in one selection `epoch`. Every project switch starts a new
 * epoch, so A -> B -> A is not the A it left. */
export interface CoverageScope {
  mission: number | null;
  epoch: number;
}

export const INITIAL_COVERAGE_SCOPE: CoverageScope = { mission: null, epoch: 0 };

/** The scope after selecting `mission`: always a new epoch. */
export function nextCoverageScope(current: CoverageScope, mission: number | null): CoverageScope {
  return { mission, epoch: current.epoch + 1 };
}

/** Whether a load, scan or association started in `started` may still land:
 * only while the page is in that same scope. A late completion from an
 * earlier selection is discarded. */
export function isCurrentCoverageScope(started: CoverageScope, current: CoverageScope): boolean {
  return started.epoch === current.epoch && started.mission === current.mission;
}
