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
    report: normalizeScanReport(raw.report),
    features: arr(raw.features).map(normalizeFeatureSummary),
    update: normalizeUpdate(raw.update),
  };
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
