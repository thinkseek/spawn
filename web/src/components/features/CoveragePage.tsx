import { useCallback, useEffect, useState } from "react";

import {
  coverageProjects,
  type CoverageProject,
  type FeatureCoverageResponse,
} from "../../featureCoverage";
import {
  associateWildFile,
  fetchFeatureCoverage,
  runWildFileScan,
} from "../../featureCoverageApi";
import { fetchFeatureMap } from "../../featuresApi";
import { relativeTime } from "../../format";
import { LiveStatus, PageHeader } from "../ui";
import { WildFileScanPanel } from "./WildFileScanPanel";

/** Feature file coverage & one-click unassociated-file ("野文件") scan view
 * (`#/coverage`): the server's default project, or any mission with a
 * configured `--features` registry (`?mission=<id>`). */
export function CoveragePage() {
  // null: the server's default project (SPAWN_FEATURE_ROOT).
  const [mission, setMission] = useState<number | null>(null);
  const [projects, setProjects] = useState<CoverageProject[]>([]);
  const [data, setData] = useState<FeatureCoverageResponse | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);

  // The project list is a convenience: without it the default project still reads.
  useEffect(() => {
    const controller = new AbortController();
    void fetchFeatureMap(controller.signal)
      .then((map) => setProjects(coverageProjects(map.missions)))
      .catch(() => setProjects([]));
    return () => controller.abort();
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    setData(null);
    setLoadError(null);
    setActionError(null);
    setActionNotice(null);
    void fetchFeatureCoverage(controller.signal, mission)
      .then((next) => {
        setData(next);
        setLoadError(null);
      })
      .catch((cause) => {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setLoadError(cause instanceof Error ? cause.message : "Failed to load coverage");
      });
    return () => controller.abort();
  }, [mission]);

  const handleScan = useCallback(async () => {
    setScanning(true);
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await runWildFileScan(undefined, mission);
      setData(next);
      setLoadError(null);
      setActionNotice(
        `Scan complete: ${next.report.counts.in_scope} in-scope, ${next.report.counts.covered} covered, ${next.report.counts.unassociated_wild} wild, ${next.report.counts.stale_references} stale.`,
      );
    } catch (cause) {
      setActionError(cause instanceof Error ? cause.message : "Scan failed");
    } finally {
      setScanning(false);
    }
  }, [mission]);

  const handleAssociate = useCallback(async (path: string, featureIds: string[]) => {
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await associateWildFile(path, featureIds, undefined, mission);
      setData(next);
      setActionNotice(
        `Associated ${path} with ${featureIds.join(", ")} (remaining wild files: ${next.report.counts.unassociated_wild}).`,
      );
    } catch (cause) {
      setActionError(cause instanceof Error ? cause.message : "Association failed");
    }
  }, [mission]);

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
              setMission(next === "" ? null : Number(next));
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
          <WildFileScanPanel
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
