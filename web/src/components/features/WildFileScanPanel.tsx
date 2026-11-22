import {
  AlertTriangle,
  CheckCircle2,
  EyeOff,
  FileQuestion,
  FileWarning,
  GitBranch,
  Link2,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { useMemo, useState } from "react";

import {
  consistencyIssueCount,
  formatBytes,
  initiativesAndSlices,
  isResearchInitiative,
  isScanClean,
  kindLabel,
  type CoverageFeatureSummary,
  type FeatureCoverageResponse,
  type WildFileFinding,
} from "../../featureCoverage";
import { Ago, Card, CardHeader, FOCUS_RING, cx } from "../ui";

interface WildFileScanPanelProps {
  data: FeatureCoverageResponse;
  scanning: boolean;
  actionError: string | null;
  actionNotice: string | null;
  onScan: () => void;
  onAssociate: (path: string, featureIds: string[]) => Promise<void>;
}

/** One-click unassociated-file ("野文件") scan panel with strictly separated
 * coverage metrics, multi-feature association picker, stale-reference viewer,
 * explicit exclusions disclosure, and initiative/graph consistency overview. */
export function WildFileScanPanel({
  data,
  scanning,
  actionError,
  actionNotice,
  onScan,
  onAssociate,
}: WildFileScanPanelProps) {
  const { report, features } = data;
  const counts = report.counts;
  const clean = isScanClean(report);
  const consistencyIssues = consistencyIssueCount(report.consistency);
  const trackedInitiatives = useMemo(() => initiativesAndSlices(features), [features]);
  const [showExclusionRules, setShowExclusionRules] = useState(false);

  return (
    <div className="mx-auto flex max-w-[1600px] flex-col gap-4 px-4 py-4 sm:px-5">
      {/* Top action & disclosed boundary bar */}
      <Card className="px-4 py-3">
        <div className="flex flex-wrap items-start gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="text-body font-semibold text-foreground">
                One-click unassociated-file scan (野文件扫描)
              </h2>
              <span
                className={cx(
                  "inline-flex items-center gap-1 rounded-chip px-2 py-0.5 text-micro font-medium",
                  clean ? "bg-ok-tint text-success" : "bg-wait-tint text-warning",
                )}
              >
                {clean ? <ShieldCheck size={12} /> : <AlertTriangle size={12} />}
                {clean ? "Zero drift" : "Attention needed"}
              </span>
            </div>
            <p className="mt-1 text-caption text-muted-foreground">
              {report.boundary.description}
            </p>
            <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-micro text-faint-foreground">
              <span>
                Last scanned:{" "}
                <Ago ms={report.scanned_at_ms} className="text-muted-foreground" />
              </span>
              <span className="font-mono" title={data.registry_path}>
                registry: {data.registry_path}
              </span>
              <span>
                {report.boundary.exclusions.length} disclosed exclusion rules
              </span>
            </div>
          </div>

          <button
            type="button"
            onClick={onScan}
            disabled={scanning}
            className={cx(
              "inline-flex h-8 shrink-0 items-center gap-1.5 rounded-control bg-foreground px-3 text-caption font-medium text-background transition-opacity hover:opacity-90 disabled:opacity-50",
              FOCUS_RING,
            )}
          >
            <RefreshCw size={13} className={cx(scanning && "animate-spin")} />
            {scanning ? "Scanning…" : "Scan unassociated files (野文件)"}
          </button>
        </div>

        <div className="mt-3 flex items-center gap-2 rounded-control border border-surface-border bg-page-canvas px-3 py-1.5 text-micro text-muted-foreground">
          <ShieldCheck size={13} className="shrink-0 text-success" />
          <span>
            Safety guarantee: scanning is read-only and never auto-deletes files. Excluded
            paths are disclosed separately and are never counted as covered.
          </span>
        </div>

        {actionError && (
          <div className="mt-2.5 flex items-start gap-2 rounded-control border border-destructive/30 bg-fail-tint px-3 py-2 text-caption text-destructive">
            <ShieldAlert size={14} className="mt-0.5 shrink-0" />
            <span>{actionError}</span>
          </div>
        )}
        {actionNotice && (
          <div className="mt-2.5 flex items-start gap-2 rounded-control border border-success/30 bg-ok-tint px-3 py-2 text-caption text-success">
            <CheckCircle2 size={14} className="mt-0.5 shrink-0" />
            <span>{actionNotice}</span>
          </div>
        )}
      </Card>

      {/* Strictly separated metrics strip */}
      <div className="grid grid-cols-2 gap-2.5 sm:grid-cols-3 lg:grid-cols-6">
        <MetricTile
          label="In-scope files"
          value={counts.in_scope}
          hint="Project files inside boundary"
        />
        <MetricTile
          label="Covered files"
          value={counts.covered}
          tone={counts.covered === counts.in_scope ? "success" : "neutral"}
          hint="Associated with >= 1 feature"
        />
        <MetricTile
          label="Unassociated (wild)"
          value={counts.unassociated_wild}
          tone={counts.unassociated_wild > 0 ? "warning" : "success"}
          hint="Existing files with 0 links"
        />
        <MetricTile
          label="Stale / missing refs"
          value={counts.stale_references}
          tone={counts.stale_references > 0 ? "danger" : "success"}
          hint="Feature links to absent files"
        />
        <MetricTile
          label="Multi-feature files"
          value={counts.multi_feature_files}
          hint="Many-to-many shared files"
        />
        <MetricTile
          label="Explicit exclusions"
          value={counts.explicit_exclusions}
          tone="muted"
          hint="Not counted as covered"
        />
      </div>

      {/* 1. Unassociated ("wild" / 野文件) files */}
      <Card>
        <CardHeader
          icon={<FileQuestion size={14} />}
          title="Unassociated (wild) files — 野文件"
          meta={
            <span className="tnum">
              {report.unassociated_files.length} finding
              {report.unassociated_files.length === 1 ? "" : "s"}
            </span>
          }
        />
        {report.unassociated_files.length === 0 ? (
          <div className="flex items-center gap-2 px-4 py-4 text-caption text-success">
            <CheckCircle2 size={15} />
            <span>
              All {counts.in_scope} in-scope project files are associated with at least one
              genuine feature node.
            </span>
          </div>
        ) : (
          <div className="divide-y divide-surface-border">
            {report.unassociated_files.map((finding) => (
              <WildFileRow
                key={finding.path}
                finding={finding}
                features={features}
                onAssociate={onAssociate}
              />
            ))}
          </div>
        )}
      </Card>

      {/* 2. Stale / missing references (strictly separated from wild files) */}
      <Card>
        <CardHeader
          icon={<FileWarning size={14} />}
          title="Stale / missing file references"
          meta={
            <span className="tnum">
              {report.stale_references.length} stale reference
              {report.stale_references.length === 1 ? "" : "s"}
            </span>
          }
        />
        {report.stale_references.length === 0 ? (
          <p className="px-4 py-3 text-caption text-muted-foreground">
            Every feature-map file link resolves to an existing in-scope file on disk.
          </p>
        ) : (
          <ul className="divide-y divide-surface-border">
            {report.stale_references.map((stale) => (
              <li key={stale.path} className="px-4 py-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-label font-medium text-destructive">
                    {stale.path}
                  </span>
                  <span className="rounded-chip bg-fail-tint px-1.5 py-px text-micro text-destructive">
                    missing from inventory
                  </span>
                </div>
                <p className="mt-1 text-micro text-muted-foreground">
                  Referenced by:{" "}
                  {stale.referencing_features.map((fid) => (
                    <span
                      key={fid}
                      className="mr-1 inline-block rounded bg-muted px-1.5 py-px font-mono text-foreground"
                    >
                      {fid}
                    </span>
                  ))}
                </p>
                {stale.rename_candidates.length > 0 && (
                  <p className="mt-1 text-micro text-faint-foreground">
                    Possible rename candidates:{" "}
                    {stale.rename_candidates.map((cand) => (
                      <span
                        key={cand}
                        className="mr-1 inline-block rounded bg-surface-selected px-1.5 py-px font-mono text-muted-foreground"
                      >
                        {cand}
                      </span>
                    ))}
                  </p>
                )}
              </li>
            ))}
          </ul>
        )}
      </Card>

      {/* 3. Explicit exclusions (never counted as covered) */}
      <Card>
        <CardHeader
          icon={<EyeOff size={14} />}
          title="Explicit exclusions (not counted as covered)"
          meta={
            <div className="flex items-center gap-2">
              <span className="tnum">
                {report.excluded_files.length} matched · {report.boundary.exclusions.length}{" "}
                rules
              </span>
              <button
                type="button"
                onClick={() => setShowExclusionRules((open) => !open)}
                className={cx(
                  "rounded-control border border-surface-border px-2 py-0.5 text-micro text-muted-foreground hover:text-foreground",
                  FOCUS_RING,
                )}
              >
                {showExclusionRules ? "Hide rules" : "Show policy rules"}
              </button>
            </div>
          }
        />
        <div className="space-y-3 px-4 py-3">
          {report.excluded_files.length === 0 ? (
            <p className="text-caption text-muted-foreground">
              No excluded paths were encountered in this scan.
            </p>
          ) : (
            <ul className="space-y-1.5">
              {report.excluded_files.map((item) => (
                <li
                  key={`${item.rule_id}:${item.path}`}
                  className="flex flex-wrap items-baseline gap-2 text-micro"
                >
                  <span className="font-mono font-medium text-foreground">{item.path}</span>
                  <span className="rounded-chip bg-muted px-1.5 py-px text-muted-foreground">
                    {item.category}
                  </span>
                  <span className="font-mono text-faint-foreground">{item.rule_id}</span>
                  <span className="text-muted-foreground">— {item.reason}</span>
                </li>
              ))}
            </ul>
          )}

          {showExclusionRules && (
            <div className="rounded-control border border-surface-border bg-page-canvas p-3">
              <p className="mb-2 text-micro font-medium uppercase tracking-wider text-faint-foreground">
                Disclosed boundary exclusion rules
              </p>
              <ul className="grid grid-cols-1 gap-1.5 md:grid-cols-2">
                {report.boundary.exclusions.map((rule) => (
                  <li key={rule.id} className="text-micro text-muted-foreground">
                    <span className="font-mono text-foreground">{rule.pattern}</span>{" "}
                    <span className="rounded bg-muted px-1 py-px text-faint-foreground">
                      {rule.category}
                    </span>{" "}
                    — {rule.reason}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      </Card>

      {/* 4. Initiatives & Graph Consistency */}
      <Card>
        <CardHeader
          icon={<GitBranch size={14} />}
          title="Initiatives & graph consistency"
          meta={
            <span
              className={cx(
                "tnum rounded-chip px-2 py-px text-micro font-medium",
                consistencyIssues === 0
                  ? "bg-ok-tint text-success"
                  : "bg-fail-tint text-destructive",
              )}
            >
              {consistencyIssues === 0
                ? "Graph consistent"
                : `${consistencyIssues} consistency issue${consistencyIssues === 1 ? "" : "s"}`}
            </span>
          }
        />
        <div className="space-y-4 px-4 py-3">
          {trackedInitiatives.length > 0 && (
            <div className="grid grid-cols-1 gap-2.5 md:grid-cols-2 xl:grid-cols-3">
              {trackedInitiatives.map((node) => {
                const research = isResearchInitiative(node);
                return (
                  <div
                    key={node.id}
                    className="rounded-control border border-surface-border bg-page-canvas p-3"
                  >
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="text-label font-semibold text-foreground">
                        {node.title}
                      </span>
                      <span
                        className={cx(
                          "rounded-chip px-1.5 py-px text-micro font-medium",
                          research
                            ? "bg-wait-tint text-warning"
                            : "bg-surface-selected text-muted-foreground",
                        )}
                      >
                        {research
                          ? "Staged research/design — not production"
                          : kindLabel(node.kind)}
                      </span>
                    </div>
                    <p className="mt-0.5 font-mono text-micro text-faint-foreground">
                      {node.id} · {node.file_count} file{node.file_count === 1 ? "" : "s"}
                    </p>
                    <p className="mt-1 text-caption text-muted-foreground">{node.summary}</p>
                    {node.status_note && (
                      <p className="mt-1 text-micro text-faint-foreground">{node.status_note}</p>
                    )}
                    {node.relations.length > 0 && (
                      <div className="mt-2 flex flex-wrap gap-1">
                        {node.relations.map((rel) => (
                          <span
                            key={`${rel.kind}:${rel.target}`}
                            className="rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground"
                          >
                            {rel.kind} → {rel.target}
                          </span>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          )}

          <div className="grid grid-cols-2 gap-2 text-micro sm:grid-cols-3 lg:grid-cols-6">
            <ConsistencyBadge
              label="Orphan features"
              count={report.consistency.orphan_features.length}
            />
            <ConsistencyBadge
              label="Dangling relations"
              count={report.consistency.dangling_relations.length}
            />
            <ConsistencyBadge label="Self-links" count={report.consistency.self_links.length} />
            <ConsistencyBadge
              label="Cycle errors"
              count={report.consistency.cycle_errors.length}
            />
            <ConsistencyBadge
              label="Duplicate IDs"
              count={report.consistency.duplicate_feature_ids.length}
            />
            <ConsistencyBadge
              label="Invalid paths"
              count={report.consistency.invalid_paths.length}
            />
          </div>

          {consistencyIssues > 0 && (
            <ul className="space-y-1 rounded-control border border-destructive/30 bg-fail-tint p-3 font-mono text-micro text-destructive">
              {[
                ...report.consistency.orphan_features.map((x) => `orphan: ${x}`),
                ...report.consistency.dangling_relations.map((x) => `dangling: ${x}`),
                ...report.consistency.self_links.map((x) => `self-link: ${x}`),
                ...report.consistency.cycle_errors.map((x) => `cycle: ${x}`),
                ...report.consistency.duplicate_feature_ids.map((x) => `duplicate: ${x}`),
                ...report.consistency.invalid_paths.map((x) => `invalid-path: ${x}`),
              ].map((line) => (
                <li key={line}>{line}</li>
              ))}
            </ul>
          )}
        </div>
      </Card>
    </div>
  );
}

function MetricTile({
  label,
  value,
  hint,
  tone = "neutral",
}: {
  label: string;
  value: number;
  hint: string;
  tone?: "neutral" | "success" | "warning" | "danger" | "muted";
}) {
  const valueStyle =
    tone === "success"
      ? "text-success"
      : tone === "warning"
        ? "text-warning"
        : tone === "danger"
          ? "text-destructive"
          : tone === "muted"
            ? "text-muted-foreground"
            : "text-foreground";
  return (
    <Card className="px-3.5 py-2.5">
      <div className="text-micro text-faint-foreground">{label}</div>
      <div className={cx("tnum mt-0.5 text-xl font-semibold", valueStyle)}>{value}</div>
      <div className="mt-0.5 text-micro text-muted-foreground">{hint}</div>
    </Card>
  );
}

function ConsistencyBadge({ label, count }: { label: string; count: number }) {
  return (
    <div className="flex items-center justify-between rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5">
      <span className="text-muted-foreground">{label}</span>
      <span
        className={cx(
          "tnum font-semibold",
          count === 0 ? "text-success" : "text-destructive",
        )}
      >
        {count}
      </span>
    </div>
  );
}

function WildFileRow({
  finding,
  features,
  onAssociate,
}: {
  finding: WildFileFinding;
  features: CoverageFeatureSummary[];
  onAssociate: (path: string, featureIds: string[]) => Promise<void>;
}) {
  const [selected, setSelected] = useState<string[]>(() =>
    finding.suggested_features.slice(0, 1),
  );
  const [filter, setFilter] = useState("");
  const [saving, setSaving] = useState(false);

  const toggle = (fid: string) => {
    setSelected((prev) =>
      prev.includes(fid) ? prev.filter((item) => item !== fid) : [...prev, fid],
    );
  };

  const filteredFeatures = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return features;
    return features.filter(
      (f) =>
        f.id.toLowerCase().includes(q) ||
        f.title.toLowerCase().includes(q) ||
        f.summary.toLowerCase().includes(q),
    );
  }, [features, filter]);

  const submit = async () => {
    if (selected.length === 0 || saving) return;
    setSaving(true);
    try {
      await onAssociate(finding.path, selected);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-2.5 px-4 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-label font-semibold text-foreground">
          {finding.path}
        </span>
        <span className="rounded-chip bg-wait-tint px-1.5 py-px text-micro text-warning">
          {finding.tracked ? "git-tracked" : "untracked"}
        </span>
        <span className="tnum text-micro text-faint-foreground">
          {formatBytes(finding.size_bytes)}
        </span>
      </div>
      <p className="text-caption text-muted-foreground">{finding.reason}</p>
      <div className="flex flex-wrap gap-1.5">
        {finding.evidence.map((ev) => (
          <span
            key={ev}
            className="rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground"
          >
            {ev}
          </span>
        ))}
      </div>

      {finding.suggested_features.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 text-micro">
          <span className="text-faint-foreground">Suggested features:</span>
          {finding.suggested_features.map((fid) => {
            const active = selected.includes(fid);
            return (
              <button
                key={fid}
                type="button"
                onClick={() => toggle(fid)}
                className={cx(
                  "rounded-chip px-2 py-0.5 font-mono transition-colors",
                  FOCUS_RING,
                  active
                    ? "bg-surface-selected font-medium text-foreground ring-1 ring-foreground/25"
                    : "bg-muted text-muted-foreground hover:text-foreground",
                )}
              >
                {active ? "✓ " : "+ "}
                {fid}
              </button>
            );
          })}
        </div>
      )}

      <div className="flex flex-col gap-2 rounded-control border border-surface-border bg-page-canvas p-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="search"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="Filter features to associate (supports many-to-many)…"
            aria-label={`Filter features for ${finding.path}`}
            className={cx(
              "h-7 min-w-[220px] flex-1 rounded-control border border-surface-border bg-surface px-2.5 text-caption text-foreground placeholder:text-faint-foreground",
              FOCUS_RING,
            )}
          />
          <button
            type="button"
            disabled={selected.length === 0 || saving}
            onClick={() => void submit()}
            className={cx(
              "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-control bg-foreground px-2.5 text-caption font-medium text-background transition-opacity hover:opacity-90 disabled:opacity-40",
              FOCUS_RING,
            )}
          >
            <Link2 size={12} />
            {saving
              ? "Linking…"
              : `Link to selected features (${selected.length})`}
          </button>
        </div>

        <div className="flex max-h-28 flex-wrap gap-1 overflow-y-auto pt-0.5">
          {filteredFeatures.map((feature) => {
            const checked = selected.includes(feature.id);
            return (
              <button
                key={feature.id}
                type="button"
                onClick={() => toggle(feature.id)}
                title={`${feature.title} — ${feature.summary}`}
                className={cx(
                  "inline-flex items-center gap-1 rounded-chip px-2 py-0.5 text-micro transition-colors",
                  FOCUS_RING,
                  checked
                    ? "bg-ok-tint font-medium text-success ring-1 ring-success/30"
                    : "bg-surface text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                )}
              >
                <span>{checked ? "✓" : "+"}</span>
                <span className="font-mono">{feature.id}</span>
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}
