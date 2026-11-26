import {
  AlertTriangle,
  CheckCircle2,
  GitBranch,
  Lock,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { useMemo, useState } from "react";

import {
  filterPurityFindings,
  filterTruthFindings,
  isEnforcedTruthFinding,
  purityCodeLabel,
  truthCodeLabel,
  type FeatureTruthResponse,
  type PurityFinding,
  type TruthFinding,
  type TruthSeverity,
} from "../../featureCoverage";
import { Card, CardHeader, FOCUS_RING, cx } from "../ui";

interface DependencyTruthPanelProps {
  data: FeatureTruthResponse | null;
  loading: boolean;
  error: string | null;
  onRetry?: () => void;
}

const TRUTH_CODES = [
  "under_declared",
  "implied_only",
  "test_only_undeclared",
  "ghost",
  "inverted",
  "unverifiable",
  "static_cycle",
  "basis_without_note",
  "kernel_leak",
  "entrypoint_imported",
  "waiver_unused",
] as const;

const SEVERITIES: Array<"all" | TruthSeverity> = ["all", "error", "warn", "info"];

/** Read-only Dependency Truth & Semantic Purity panel rendered in `#/coverage`. */
export function DependencyTruthPanel({
  data,
  loading,
  error,
  onRetry,
}: DependencyTruthPanelProps) {
  const [severity, setSeverity] = useState<"all" | TruthSeverity>("all");
  const [code, setCode] = useState<string>("all");
  const [query, setQuery] = useState<string>("");
  const [includeWaived, setIncludeWaived] = useState<boolean>(true);

  const filteredFindings = useMemo(
    () =>
      data
        ? filterTruthFindings(data.findings, { code, severity, includeWaived, query })
        : [],
    [data, code, severity, includeWaived, query],
  );
  const filteredPurity = useMemo(
    () =>
      data
        ? filterPurityFindings(data.purity.findings, { severity, query })
        : [],
    [data, severity, query],
  );

  if (!data) {
    return (
      <Card className="px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2 text-caption text-muted-foreground">
            <GitBranch size={14} className="shrink-0 text-faint-foreground" />
            <span>
              {loading
                ? "Loading dependency truth & semantic purity…"
                : error
                  ? `Dependency truth & semantic purity unavailable: ${error}`
                  : "Dependency truth & semantic purity have not loaded yet."}
            </span>
          </div>
          {onRetry && !loading && (
            <button
              type="button"
              onClick={onRetry}
              className={cx(
                "inline-flex h-7 items-center gap-1.5 rounded-control border border-surface-border bg-page-canvas px-2.5 text-micro font-medium text-muted-foreground hover:text-foreground",
                FOCUS_RING,
              )}
            >
              <RefreshCw size={11} />
              Retry read
            </button>
          )}
        </div>
      </Card>
    );
  }

  const { counts, purity, projection, graph } = data;
  const truthClean = counts.enforced_violations === 0 && counts.warn === 0;
  const shortHead = data.git_head ? data.git_head.slice(0, 9) : "no git HEAD";

  return (
    <div className="flex flex-col gap-4" data-testid="dependency-truth-panel">
      {/* Header & verdict summary */}
      <Card className="px-4 py-3">
        <div className="flex flex-wrap items-start gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="text-body font-semibold text-foreground">
                Dependency truth &amp; semantic purity
              </h2>
              <span className="inline-flex items-center gap-1 rounded-chip bg-muted px-2 py-0.5 text-micro font-medium text-muted-foreground">
                <Lock size={11} />
                Read-only
              </span>
              <span
                className={cx(
                  "inline-flex items-center gap-1 rounded-chip px-2 py-0.5 text-micro font-medium",
                  counts.enforced_violations > 0
                    ? "bg-fail-tint text-destructive"
                    : truthClean
                      ? "bg-ok-tint text-success"
                      : "bg-wait-tint text-warning",
                )}
              >
                {counts.enforced_violations > 0 ? (
                  <ShieldAlert size={12} />
                ) : truthClean ? (
                  <ShieldCheck size={12} />
                ) : (
                  <AlertTriangle size={12} />
                )}
                {counts.enforced_violations > 0
                  ? `${counts.enforced_violations} enforced truth violation${counts.enforced_violations === 1 ? "" : "s"}`
                  : counts.warn > 0
                    ? `${counts.warn} truth warning${counts.warn === 1 ? "" : "s"}`
                    : "Truth clean"}
              </span>
              <span
                data-testid="purity-status-badge"
                className={cx(
                  "inline-flex items-center gap-1 rounded-chip px-2 py-0.5 text-micro font-medium",
                  purity.clean
                    ? "bg-ok-tint text-success"
                    : "bg-fail-tint text-destructive",
                )}
              >
                {purity.clean ? <CheckCircle2 size={12} /> : <ShieldAlert size={12} />}
                {purity.clean
                  ? "Purity clean"
                  : `Purity: ${purity.counts.error} err / ${purity.counts.warn} warn`}
              </span>
            </div>
            <p className="mt-1 text-caption text-muted-foreground">
              Static code graph edges ({graph.files} source files across {graph.units} package unit
              {graph.units === 1 ? "" : "s"}, {projection.file_edges} file edges:{" "}
              {projection.declared} declared, {projection.implied} implied,{" "}
              {projection.undeclared} undeclared) verified against declared feature relations.
            </p>
            <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-micro text-faint-foreground">
              <span>project: {data.project}</span>
              <span className="font-mono">head: {shortHead}</span>
              <span>as of: {data.today}</span>
              <span className="font-mono" title={data.registry_path}>
                registry: {data.registry_path}
              </span>
            </div>
          </div>
        </div>
      </Card>

      {/* Truth & Purity metrics strip */}
      <div className="grid grid-cols-2 gap-2.5 sm:grid-cols-4">
        <TruthMetricTile
          label="Under-declared"
          value={counts.under_declared}
          tone={counts.under_declared > 0 ? "danger" : "success"}
          hint="Prod edges missing depends_on"
        />
        <TruthMetricTile
          label="Implied-only"
          value={counts.implied_only}
          tone={counts.implied_only > 0 ? "warning" : "success"}
          hint="Transitive chain without direct edge"
        />
        <TruthMetricTile
          label="Test-only undeclared"
          value={counts.test_only_undeclared}
          tone={counts.test_only_undeclared > 0 ? "info" : "neutral"}
          hint="Test/build edges undeclared"
        />
        <TruthMetricTile
          label="Ghost"
          value={counts.ghost}
          tone={counts.ghost > 0 ? "danger" : "success"}
          hint="Declared depends_on with 0 edges"
        />
        <TruthMetricTile
          label="Static cycles"
          value={counts.static_cycle}
          tone={counts.static_cycle > 0 ? "warning" : "success"}
          hint="Mutual/cyclic feature dependencies"
        />
        <TruthMetricTile
          label="Unverifiable"
          value={counts.unverifiable}
          tone={counts.unverifiable > 0 ? "info" : "neutral"}
          hint="Side lacks analyzed source files"
        />
        <TruthMetricTile
          label="Enforced violations"
          value={counts.enforced_violations}
          tone={counts.enforced_violations > 0 ? "danger" : "success"}
          hint="Blocking rules under --deps enforce"
        />
        <TruthMetricTile
          label="Semantic purity"
          value={purity.clean ? "Clean" : `${purity.counts.error}E / ${purity.counts.warn}W`}
          tone={purity.clean ? "success" : "danger"}
          hint="Registry structural & scope hygiene"
        />
      </div>

      {/* Findings list with code/severity/feature filters */}
      <Card>
        <CardHeader
          icon={<GitBranch size={14} />}
          title="Dependency truth findings"
          meta={
            <span className="tnum">
              {filteredFindings.length} of {data.findings.length} shown
            </span>
          }
        />
        <div className="flex flex-wrap items-center gap-2 border-b border-surface-border px-4 py-2.5">
          <div className="flex items-center gap-1" role="group" aria-label="Filter by severity">
            {SEVERITIES.map((item) => (
              <button
                key={item}
                type="button"
                onClick={() => setSeverity(item)}
                aria-pressed={severity === item}
                className={cx(
                  "rounded-chip px-2 py-0.5 text-micro font-medium transition-colors",
                  FOCUS_RING,
                  severity === item
                    ? "bg-foreground text-background"
                    : "bg-muted text-muted-foreground hover:text-foreground",
                )}
              >
                {item === "all" ? "All severities" : item}
              </button>
            ))}
          </div>

          <select
            aria-label="Filter by finding code"
            value={code}
            onChange={(event) => setCode(event.target.value)}
            className={cx(
              "h-7 rounded-control border border-surface-border bg-surface px-2 text-micro text-foreground",
              FOCUS_RING,
            )}
          >
            <option value="all">All codes</option>
            {TRUTH_CODES.map((known) => (
              <option key={known} value={known}>
                {truthCodeLabel(known)} ({counts.by_code[known] ?? 0})
              </option>
            ))}
          </select>

          <input
            type="search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Filter by feature ID, code, or evidence path…"
            aria-label="Search dependency truth and purity findings"
            className={cx(
              "h-7 min-w-[220px] flex-1 rounded-control border border-surface-border bg-surface px-2.5 text-caption text-foreground placeholder:text-faint-foreground",
              FOCUS_RING,
            )}
          />

          {counts.waived > 0 && (
            <label className="inline-flex cursor-pointer items-center gap-1.5 text-micro text-muted-foreground">
              <input
                type="checkbox"
                checked={includeWaived}
                onChange={(event) => setIncludeWaived(event.target.checked)}
                className="accent-foreground"
              />
              Show waived ({counts.waived})
            </label>
          )}
        </div>

        {filteredFindings.length === 0 ? (
          <div className="flex items-center gap-2 px-4 py-4 text-caption text-muted-foreground">
            <CheckCircle2 size={15} className="text-success" />
            <span>
              {data.findings.length === 0
                ? "No dependency-truth findings reported for this project."
                : "No findings match the current severity, code, or search filter."}
            </span>
          </div>
        ) : (
          <ul className="divide-y divide-surface-border">
            {filteredFindings.map((finding, index) => (
              <TruthFindingRow
                key={`${finding.code}:${finding.from_any_of.join(",")}:${finding.to_any_of.join(",")}:${finding.features.join(",")}:${index}`}
                finding={finding}
              />
            ))}
          </ul>
        )}
      </Card>

      {/* Semantic purity panel */}
      <Card>
        <CardHeader
          icon={purity.clean ? <ShieldCheck size={14} /> : <ShieldAlert size={14} />}
          title="Semantic purity checks"
          meta={
            <span
              className={cx(
                "rounded-chip px-2 py-px text-micro font-medium",
                purity.clean
                  ? "bg-ok-tint text-success"
                  : "bg-fail-tint text-destructive",
              )}
            >
              {purity.clean
                ? "Clean"
                : `${purity.counts.error} error / ${purity.counts.warn} warn`}
            </span>
          }
        />
        {data.purity.findings.length === 0 ? (
          <div className="flex items-center gap-2 px-4 py-3 text-caption text-success">
            <CheckCircle2 size={14} />
            <span>
              Clean: zero domain-direct files, glob catch-alls, catch-all buckets, or unrelated
              directory spans.
            </span>
          </div>
        ) : filteredPurity.length === 0 ? (
          <p className="px-4 py-3 text-caption text-muted-foreground">
            No semantic purity findings match the active severity/search filter.
          </p>
        ) : (
          <ul className="divide-y divide-surface-border">
            {filteredPurity.map((finding, index) => (
              <PurityFindingRow
                key={`${finding.code}:${finding.feature_id}:${index}`}
                finding={finding}
              />
            ))}
          </ul>
        )}
      </Card>
    </div>
  );
}

function TruthMetricTile({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: number | string;
  hint: string;
  tone: "neutral" | "success" | "warning" | "danger" | "info";
}) {
  const toneClass =
    tone === "success"
      ? "text-success"
      : tone === "warning"
        ? "text-warning"
        : tone === "danger"
          ? "text-destructive"
          : tone === "info"
            ? "text-info"
            : "text-foreground";
  return (
    <Card className="px-3.5 py-2.5">
      <div className="text-micro text-faint-foreground">{label}</div>
      <div className={cx("tnum mt-0.5 text-xl font-semibold", toneClass)}>{value}</div>
      <div className="mt-0.5 text-micro text-muted-foreground">{hint}</div>
    </Card>
  );
}

function severityPillClass(severity: TruthSeverity): string {
  switch (severity) {
    case "error":
      return "bg-fail-tint text-destructive";
    case "warn":
      return "bg-wait-tint text-warning";
    case "info":
      return "bg-info-tint text-info";
  }
}

function TruthFindingRow({ finding }: { finding: TruthFinding }) {
  const enforced = isEnforcedTruthFinding(finding);
  const subject =
    finding.from_any_of.length > 0 || finding.to_any_of.length > 0
      ? `${finding.from_any_of.join(" | ")} → ${finding.to_any_of.join(" | ")}`
      : finding.features.join(" ↔ ");

  return (
    <li className="space-y-1.5 px-4 py-3">
      <div className="flex flex-wrap items-center gap-1.5">
        <span
          className={cx(
            "rounded-chip px-1.5 py-px text-micro font-medium",
            severityPillClass(finding.severity),
          )}
        >
          {finding.severity}
        </span>
        <span className="rounded-chip bg-muted px-1.5 py-px font-mono text-micro text-foreground">
          {finding.code}
        </span>
        {enforced && (
          <span className="rounded-chip bg-fail-tint px-1.5 py-px text-micro font-medium text-destructive">
            enforced
          </span>
        )}
        {finding.waived && (
          <span className="rounded-chip bg-surface-selected px-1.5 py-px text-micro text-muted-foreground">
            waived
          </span>
        )}
        {subject && (
          <span className="min-w-0 font-mono text-caption font-medium text-foreground">
            {subject}
          </span>
        )}
      </div>

      {finding.fix && <p className="text-caption text-muted-foreground">{finding.fix}</p>}

      {finding.evidence.length > 0 && (
        <ul className="space-y-1 rounded-control border border-surface-border bg-page-canvas px-3 py-2 font-mono text-micro">
          {finding.evidence.map((edge, idx) => (
            <li
              key={`${edge.from}:${edge.line}:${edge.to}:${idx}`}
              className="flex flex-wrap items-baseline gap-1.5 text-muted-foreground"
            >
              <span className="text-foreground">
                {edge.from}:{edge.line}
              </span>
              <span>→</span>
              <span className="text-foreground">{edge.to}</span>
              <span className="rounded bg-muted px-1 py-px text-faint-foreground">
                {edge.kind} · {edge.scope}
              </span>
              {edge.via && <span className="text-faint-foreground">(via {edge.via})</span>}
            </li>
          ))}
          {finding.evidence_total > finding.evidence.length && (
            <li className="text-faint-foreground">
              +{finding.evidence_total - finding.evidence.length} more evidence edge
              {finding.evidence_total - finding.evidence.length === 1 ? "" : "s"}
            </li>
          )}
        </ul>
      )}

      {finding.suppressed && finding.suppressed.pairs.length > 0 && (
        <p className="text-micro text-faint-foreground">
          Suppressed within cycle: {finding.suppressed.total} edge group
          {finding.suppressed.total === 1 ? "" : "s"} ({finding.suppressed.file_edges} prod file
          edges).
        </p>
      )}
    </li>
  );
}

function PurityFindingRow({ finding }: { finding: PurityFinding }) {
  return (
    <li className="space-y-1.5 px-4 py-3">
      <div className="flex flex-wrap items-center gap-1.5">
        <span
          className={cx(
            "rounded-chip px-1.5 py-px text-micro font-medium",
            severityPillClass(finding.severity),
          )}
        >
          {finding.severity}
        </span>
        <span className="rounded-chip bg-muted px-1.5 py-px font-mono text-micro text-foreground">
          {purityCodeLabel(finding.code)}
        </span>
        <span className="font-mono text-caption font-medium text-foreground">
          {finding.feature_id}
        </span>
      </div>
      <p className="text-caption text-muted-foreground">{finding.detail}</p>
      {finding.sample_paths.length > 0 && (
        <div className="flex flex-wrap gap-1 font-mono text-micro">
          {finding.sample_paths.map((sample) => (
            <span
              key={sample}
              className="rounded bg-page-canvas px-1.5 py-0.5 text-muted-foreground ring-1 ring-surface-border"
            >
              {sample}
            </span>
          ))}
        </div>
      )}
      {finding.fix && <p className="text-micro text-faint-foreground">Fix: {finding.fix}</p>}
    </li>
  );
}
