import {
  ArrowUpRight,
  ExternalLink,
  OctagonAlert,
  UserRoundX,
} from "lucide-react";
import { useState } from "react";

import { usePoll } from "../../api";
import { clockTime, relativeTime } from "../../format";
import { freshAt } from "../../validated";
import type {
  Feature,
  FeatureDeploy,
  FeatureLanding,
  FeatureMission,
  FeatureSource,
  FeatureTask,
} from "../../features";
import {
  blockerText,
  deployNote,
  gapText,
  groupByState,
  isExternal,
  newestVerification,
  objectiveName,
  ownerText,
  sourceLabel,
  verificationNote,
} from "../../features";
import { fetchFeatureMap } from "../../featuresApi";
import { Card, FOCUS_RING, LiveStatus, PageHeader, chip, cx } from "../ui";

interface FeatureMapViewProps {
  /** Open a task's session timeline: the evidence behind a capability. */
  onOpenSession: (sessionId: string) => void;
}

/** The Feature map: a mission's capabilities, planned, implemented,
 * verified and live, each with the records that say so. The states are the
 * server's, derived from Spawn's log; this page only lays them out. */
export function FeatureMapView({ onOpenSession }: FeatureMapViewProps) {
  const poll = usePoll("features", fetchFeatureMap, 4_000, { checked: true });
  const [chosen, setChosen] = useState<number | null>(null);
  const data = poll.data;
  const mission =
    data?.missions.find((entry) => entry.id === chosen) ?? data?.missions[0] ?? null;

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Feature map{mission ? ` · mission #${mission.id}` : ""}
          </h1>
        }
      >
        {data && data.missions.length > 1 && (
          <div className="flex gap-1.5">
            {data.missions.map((entry) => (
              <button
                key={entry.id}
                type="button"
                onClick={() => setChosen(entry.id)}
                aria-current={entry.id === mission?.id ? "true" : undefined}
                className={cx("tnum", chip(entry.id === mission?.id), FOCUS_RING)}
              >
                #{entry.id}
              </button>
            ))}
          </div>
        )}
        <LiveStatus
          error={poll.error}
          label={
            data
              ? `Updated ${relativeTime(freshAt(data.generated_at_ms, poll.checkedAt))}`
              : "Connecting"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center text-label text-muted-foreground">
          {poll.error ? `The feature map could not be read: ${poll.error}` : "Reading the map…"}
        </div>
      ) : !mission ? (
        <div className="grid flex-1 place-items-center px-6 text-center text-label text-muted-foreground">
          No open objective mission has a capability map.
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1600px] flex-col gap-3 px-4 py-4 sm:px-5">
            {mission.blocker && <MissionBlocker mission={mission} />}
            {mission.mapping.error && <MappingError mission={mission} />}
            <MissionHead mission={mission} />
            <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
              {groupByState(mission.features).map((column) => (
                <section
                  key={column.state}
                  aria-label={column.info.label}
                  className="flex min-w-0 flex-col gap-2"
                >
                  <header className="rounded-card bg-surface px-3 py-2 shadow-card">
                    <div className="flex items-center gap-2">
                      <span className={cx("size-2 rounded-full", column.info.dot)} />
                      <h2 className="text-label font-semibold text-foreground">
                        {column.info.label}
                      </h2>
                      <span className="tnum ml-auto text-caption text-muted-foreground">
                        {column.features.length}
                      </span>
                    </div>
                    <p className="mt-0.5 text-micro text-faint-foreground">{column.info.blurb}</p>
                  </header>
                  {column.features.length === 0 ? (
                    <p className="rounded-card border border-dashed border-surface-border px-3 py-4 text-caption text-faint-foreground">
                      Nothing in this state.
                    </p>
                  ) : (
                    column.features.map((feature) => (
                      <FeatureCard
                        key={feature.id}
                        feature={feature}
                        mission={mission}
                        onOpenSession={onOpenSession}
                      />
                    ))
                  )}
                </section>
              ))}
            </div>
            {mission.unmapped.length > 0 && (
              <p className="rounded-card border border-surface-border bg-surface px-3 py-2 text-caption text-muted-foreground">
                <span className="font-medium text-foreground">
                  {mission.unmapped.length} objective
                  {mission.unmapped.length === 1 ? "" : "s"} not in the mapping:
                </span>{" "}
                {mission.unmapped
                  .map((objective) => objectiveName(mission.id, objective.id))
                  .join(", ")}
              </p>
            )}
            <p className="pb-2 text-micro text-faint-foreground">
              Planned is not implemented (a task, a merge or a review is not a landing).
              Implemented is not verified. Verified is not live: live is a successful deploy
              recorded after the newest landing, and a mission no deploy ships is never called
              live here.
            </p>
          </div>
        </div>
      )}
    </main>
  );
}

function MissionBlocker({ mission }: { mission: FeatureMission }) {
  const blocker = mission.blocker;
  if (!blocker) return null;
  return (
    <div className="flex items-start gap-3 rounded-card border border-destructive/30 bg-fail-tint px-4 py-2.5">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-destructive" />
      <div className="min-w-0 text-label text-foreground">{blockerText(blocker)}</div>
    </div>
  );
}

function MappingError({ mission }: { mission: FeatureMission }) {
  return (
    <div className="flex items-start gap-3 rounded-card border border-warning/30 bg-wait-tint px-4 py-2.5">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 text-label text-foreground">
        The mapping {mission.mapping.path} could not be read, so each backlog objective is shown
        instead: <span className="font-mono text-caption">{mission.mapping.error}</span>
      </div>
    </div>
  );
}

function MissionHead({ mission }: { mission: FeatureMission }) {
  const counts = mission.counts;
  return (
    <Card className="px-4 py-3">
      <div className="flex flex-wrap items-start gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1">
          <p className="text-label text-foreground">{mission.text}</p>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-caption text-muted-foreground">
            <span>{mission.state}</span>
            {mission.gate && (
              <span className="truncate font-mono text-micro" title={mission.gate}>
                gate {mission.gate.split("/").slice(-2).join("/")}
              </span>
            )}
            <span>accepts by {mission.accept}</span>
            {mission.resident?.name && <span>resident {mission.resident.name}</span>}
            <span>
              {mission.shipped_by_deploy
                ? "Spawn's deploy ships this source"
                : "no deploy ships this source"}
            </span>
            {mission.mapping.kind === "file" && (
              <span className="truncate font-mono text-micro" title={mission.mapping.path ?? ""}>
                mapping {mission.mapping.path}
              </span>
            )}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Count label="capabilities" value={counts.features} />
          <Count label="unowned" value={counts.unowned} tone="warning" />
          <Count label="blocked" value={counts.blocked} tone="danger" />
          <Count label="complete" value={counts.complete} tone="success" />
        </div>
      </div>
      {mission.source && (
        <p className="mt-2 text-micro text-faint-foreground">
          Provenance: {sourceLabel(mission.source)}
          {mission.source.title ? ` — ${mission.source.title}` : ""}
        </p>
      )}
    </Card>
  );
}

function Count({
  label,
  value,
  tone = "muted",
}: {
  label: string;
  value: number;
  tone?: "muted" | "warning" | "danger" | "success";
}) {
  const style =
    value === 0
      ? "bg-muted text-muted-foreground"
      : tone === "danger"
        ? "bg-fail-tint text-destructive"
        : tone === "warning"
          ? "bg-wait-tint text-warning"
          : tone === "success"
            ? "bg-ok-tint text-success"
            : "bg-surface-selected text-foreground";
  return (
    <span className={cx("tnum rounded-chip px-1.5 py-px text-caption", style)} title={label}>
      {value} {label}
    </span>
  );
}

function FeatureCard({
  feature,
  mission,
  onOpenSession,
}: {
  feature: Feature;
  mission: FeatureMission;
  onOpenSession: (sessionId: string) => void;
}) {
  const newest = newestTask(feature);
  return (
    <Card className="flex min-w-0 flex-col gap-2 px-3 py-2.5">
      <div className="flex min-w-0 items-start gap-2">
        <h3 className="min-w-0 flex-1 text-label font-semibold text-foreground">
          {feature.title}
        </h3>
        {feature.complete && (
          <span className="shrink-0 rounded-chip bg-ok-tint px-1.5 py-px text-micro font-medium text-success">
            complete
          </span>
        )}
        {feature.unowned && (
          <span
            title="No task in flight and not live: nobody is working on it"
            className="inline-flex shrink-0 items-center gap-1 rounded-chip bg-muted px-1.5 py-px text-micro text-muted-foreground"
          >
            <UserRoundX size={10} /> unowned
          </span>
        )}
      </div>
      <p className="truncate font-mono text-micro text-faint-foreground">{feature.id}</p>
      {feature.summary && (
        <p className="text-caption text-muted-foreground">{feature.summary}</p>
      )}

      {feature.objective_rows.length > 0 && (
        <ul className="space-y-0.5">
          {feature.objective_rows.map((objective) => (
            <li key={objective.id} className="flex min-w-0 items-baseline gap-1.5 text-micro">
              <span className="shrink-0 font-mono text-faint-foreground">
                {objectiveName(mission.id, objective.id)}
              </span>
              <span className="min-w-0 truncate text-muted-foreground" title={objective.title ?? ""}>
                {objective.title}
              </span>
            </li>
          ))}
        </ul>
      )}

      {feature.sources.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {feature.sources.map((source) => (
            <SourceChip key={source.ref} source={source} />
          ))}
        </div>
      )}

      {feature.blocked && (
        <p className="rounded-control bg-fail-tint px-2 py-1 text-micro text-destructive">
          {blockerText(feature.blocked)}
        </p>
      )}

      <OwnerLine
        feature={feature}
        onOpenSession={onOpenSession}
      />

      <dl className="grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 text-micro">
        <dt className="text-faint-foreground">landed</dt>
        <dd className="min-w-0 truncate text-foreground">{landingText(feature)}</dd>
        <dt className="text-faint-foreground">verified</dt>
        <dd className="min-w-0 truncate text-foreground">
          {verificationNote(feature)}
          {newestVerification(feature)?.at_ms ? (
            <span className="text-faint-foreground">
              {" "}
              · {relativeTime(newestVerification(feature)?.at_ms)}
            </span>
          ) : null}
        </dd>
        <dt className="text-faint-foreground">deployed</dt>
        <dd className="min-w-0 truncate text-foreground" title={deployTitle(feature.evidence.deploy ?? feature.evidence.last_deploy)}>
          {deployNote(feature)}
        </dd>
        {newest && (
          <>
            <dt className="text-faint-foreground">review</dt>
            <dd className="min-w-0 truncate text-foreground">{reviewText(newest)}</dd>
          </>
        )}
      </dl>

      {feature.gaps.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {feature.gaps.map((gap) => (
            <span
              key={gap.kind + gap.objectives.join(",")}
              title={gap.detail ?? undefined}
              className="rounded-chip bg-wait-tint px-1.5 py-px text-micro text-warning"
            >
              {gapText(gap, mission.id)}
            </span>
          ))}
        </div>
      )}
    </Card>
  );
}

function OwnerLine({
  feature,
  onOpenSession,
}: {
  feature: Feature;
  onOpenSession: (sessionId: string) => void;
}) {
  const owner = feature.owner;
  const last = feature.last_owner;
  const session = owner?.session_id ?? last?.session_id ?? null;
  const theText = ownerText(owner) ?? (last ? `last: ${ownerText(last) ?? ""}` : null);
  if (theText === null) {
    return <p className="text-micro text-faint-foreground">no task yet</p>;
  }
  return (
    <p className="flex min-w-0 items-center gap-1 text-micro text-muted-foreground">
      <span className="truncate">{theText}</span>
      {session && (
        <button
          type="button"
          onClick={() => onOpenSession(session)}
          className={cx(
            "inline-flex shrink-0 items-center gap-0.5 rounded-chip px-1 text-micro text-info hover:underline",
            FOCUS_RING,
          )}
        >
          session <ArrowUpRight size={10} />
        </button>
      )}
    </p>
  );
}

function SourceChip({ source }: { source: FeatureSource }) {
  const label = sourceLabel(source);
  const title = source.title ?? source.ref;
  if (isExternal(source) && source.url) {
    return (
      <a
        href={source.url}
        target="_blank"
        rel="noreferrer"
        title={title}
        className="inline-flex max-w-full items-center gap-0.5 truncate rounded-chip bg-surface-selected px-1.5 py-px text-micro text-info hover:underline"
      >
        {label} <ExternalLink size={9} className="shrink-0" />
      </a>
    );
  }
  return (
    <span
      title={title}
      className={cx(
        "max-w-full truncate rounded-chip px-1.5 py-px text-micro",
        source.linked
          ? "bg-surface-selected text-muted-foreground"
          : "bg-muted text-faint-foreground",
      )}
    >
      {label}
      {source.native && !source.linked ? " (missing)" : ""}
    </span>
  );
}

function newestTask(feature: Feature): FeatureTask | null {
  let newest: FeatureTask | null = null;
  for (const task of feature.evidence.tasks) {
    if (newest === null || (task.updated_at_ms ?? 0) > (newest.updated_at_ms ?? 0)) {
      newest = task;
    }
  }
  return newest;
}

function landingText(feature: Feature): string {
  const landing = newestLanding(feature);
  if (landing === null) return "not landed";
  const task = landing.task === null ? "a task" : `task #${landing.task}`;
  const commit = landing.commit ? ` · ${landing.commit.slice(0, 10)}` : "";
  const when = landing.at_ms ? ` · ${relativeTime(landing.at_ms)}` : "";
  const solved = landing.kind === "solved_on_base" ? "solved on the base" : "landed";
  return `${solved} ${task}${commit}${when}`;
}

function newestLanding(feature: Feature): FeatureLanding | null {
  let newest: FeatureLanding | null = null;
  for (const landing of feature.evidence.landings) {
    if (newest === null || (landing.at_ms ?? 0) > (newest.at_ms ?? 0)) {
      newest = landing;
    }
  }
  return newest;
}

function reviewText(task: FeatureTask): string {
  const gate = task.review.last_gate;
  if (task.review.sent_back) return "sent back";
  if (task.review.accepted) {
    return gate ? `gate ${gate.verdict}` : `accepted at revision ${task.revisions}`;
  }
  if (gate) return `${gate.verdict}${gate.reason ? `: ${gate.reason}` : ""}`;
  return task.state;
}

function deployTitle(deploy: FeatureDeploy | null): string | undefined {
  if (!deploy) return undefined;
  return [
    deploy.head ? `head ${deploy.head}` : null,
    deploy.by ? `by ${deploy.by}` : null,
    deploy.at_ms ? clockTime(deploy.at_ms) : null,
    deploy.reason || null,
  ]
    .filter(Boolean)
    .join("\n");
}
