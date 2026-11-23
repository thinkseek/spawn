import { useCallback, useEffect, useRef, useState } from "react";

import {
  INITIAL_COVERAGE_SCOPE,
  isCurrentCoverageScope,
  nextCoverageScope,
  type CoverageProject,
  type CoverageScope,
  type FeatureCoverageResponse,
} from "../../featureCoverage";
import {
  associateWildFile,
  fetchCoverageProjects,
  fetchFeatureCoverage,
  runWildFileScan,
} from "../../featureCoverageApi";
import { relativeTime } from "../../format";
import { LiveStatus, PageHeader } from "../ui";
import { WildFileScanPanel } from "./WildFileScanPanel";

/** Feature file coverage & one-click unassociated-file ("野文件") scan view
 * (`#/coverage`): the server's default project, or any mission with a
 * configured `--features` coverage registry (`?mission=<id>`). Every result
 * is scoped to the selection it was asked for: one that completes after the
 * project changed is discarded. */
export function CoveragePage() {
  // mission null: the server's default project (SPAWN_FEATURE_ROOT).
  const [scope, setScope] = useState<CoverageScope>(INITIAL_COVERAGE_SCOPE);
  const scopeRef = useRef<CoverageScope>(INITIAL_COVERAGE_SCOPE);
  const [projects, setProjects] = useState<CoverageProject[]>([]);
  const [shown, setShown] = useState<{ scope: CoverageScope; data: FeatureCoverageResponse } | null>(
    null,
  );
  const [loadError, setLoadError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);
  const mission = scope.mission;
  // Only the current scope's report is ever drawn (or associated against).
  const data = shown && isCurrentCoverageScope(shown.scope, scope) ? shown.data : null;

  // The project list is a convenience: without it the default project still reads.
  useEffect(() => {
    const controller = new AbortController();
    void fetchCoverageProjects(controller.signal)
      .then(setProjects)
      .catch(() => setProjects([]));
    return () => controller.abort();
  }, []);

  const selectMission = useCallback((next: number | null) => {
    const nextScope = nextCoverageScope(scopeRef.current, next);
    scopeRef.current = nextScope;
    setScope(nextScope);
    setShown(null);
    setScanning(false);
    setLoadError(null);
    setActionError(null);
    setActionNotice(null);
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    const started = scope;
    void fetchFeatureCoverage(controller.signal, started.mission)
      .then((next) => {
        if (!isCurrentCoverageScope(started, scopeRef.current)) return;
        setShown({ scope: started, data: next });
        setLoadError(null);
      })
      .catch((cause) => {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        if (!isCurrentCoverageScope(started, scopeRef.current)) return;
        setLoadError(cause instanceof Error ? cause.message : "Failed to load coverage");
      });
    return () => controller.abort();
  }, [scope]);

  const handleScan = useCallback(async () => {
    const started = scopeRef.current;
    setScanning(true);
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await runWildFileScan(undefined, started.mission);
      if (!isCurrentCoverageScope(started, scopeRef.current)) return;
      setShown({ scope: started, data: next });
      setLoadError(null);
      setActionNotice(
        `Scan complete: ${next.report.counts.in_scope} in-scope, ${next.report.counts.covered} covered, ${next.report.counts.unassociated_wild} wild, ${next.report.counts.stale_references} stale.`,
      );
    } catch (cause) {
      if (!isCurrentCoverageScope(started, scopeRef.current)) return;
      setActionError(cause instanceof Error ? cause.message : "Scan failed");
    } finally {
      if (isCurrentCoverageScope(started, scopeRef.current)) setScanning(false);
    }
  }, []);

  const handleAssociate = useCallback(async (path: string, featureIds: string[]) => {
    const started = scopeRef.current;
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await associateWildFile(path, featureIds, undefined, started.mission);
      if (!isCurrentCoverageScope(started, scopeRef.current)) return;
      setShown({ scope: started, data: next });
      setActionNotice(
        `Associated ${path} with ${featureIds.join(", ")} (remaining wild files: ${next.report.counts.unassociated_wild}).`,
      );
    } catch (cause) {
      if (!isCurrentCoverageScope(started, scopeRef.current)) return;
      setActionError(cause instanceof Error ? cause.message : "Association failed");
    }
  }, []);

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            File coverage · Wild-file scan (野文件)
          </h1>
        }
      >
        {projects.length > 0 ? (
          <select
            aria-label="Project"
            data-testid="coverage-project-select"
            value={mission === null ? "" : String(mission)}
            onChange={(event) => {
              const next = event.target.value;
              selectMission(next === "" ? null : Number(next));
            }}
            className="max-w-xs truncate rounded-md border border-surface-border bg-surface px-2 text-caption font-medium text-foreground transition-colors hover:border-border-strong focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring"
          >
            <option value="">Default project (this server)</option>
            {projects.map((project) => (
              <option key={project.mission} value={String(project.mission)} title={project.registry}>
                {project.label}
              </option>
            ))}
          </select>
        ) : null}
        <LiveStatus
          error={loadError}
          label={
            data
              ? `Scanned ${relativeTime(data.report.scanned_at_ms)}`
              : "Loading"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center px-6 text-label text-muted-foreground">
          {loadError
            ? `File coverage could not be loaded: ${loadError}`
            : "Loading feature-map file coverage…"}
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          {/* A new scope remounts the panel: no selection from another project survives. */}
          <WildFileScanPanel
            key={scope.epoch}
            data={data}
            scanning={scanning}
            actionError={actionError}
            actionNotice={actionNotice}
            onScan={() => void handleScan()}
            onAssociate={handleAssociate}
          />
        </div>
      )}
    </main>
  );
}
