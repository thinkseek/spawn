import { useCallback, useEffect, useState } from "react";

import type { FeatureCoverageResponse } from "../../featureCoverage";
import {
  associateWildFile,
  fetchFeatureCoverage,
  runWildFileScan,
} from "../../featureCoverageApi";
import { relativeTime } from "../../format";
import { LiveStatus, PageHeader } from "../ui";
import { WildFileScanPanel } from "./WildFileScanPanel";

/** Feature file coverage & one-click unassociated-file ("野文件") scan view (`#/coverage`). */
export function CoveragePage() {
  const [data, setData] = useState<FeatureCoverageResponse | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    void fetchFeatureCoverage(controller.signal)
      .then((next) => {
        setData(next);
        setLoadError(null);
      })
      .catch((cause) => {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setLoadError(cause instanceof Error ? cause.message : "Failed to load coverage");
      });
    return () => controller.abort();
  }, []);

  const handleScan = useCallback(async () => {
    setScanning(true);
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await runWildFileScan();
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
  }, []);

  const handleAssociate = useCallback(async (path: string, featureIds: string[]) => {
    setActionError(null);
    setActionNotice(null);
    try {
      const next = await associateWildFile(path, featureIds);
      setData(next);
      setActionNotice(
        `Associated ${path} with ${featureIds.join(", ")} (remaining wild files: ${next.report.counts.unassociated_wild}).`,
      );
    } catch (cause) {
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
