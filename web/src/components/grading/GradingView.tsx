import { Check, Clock, Cpu, Layers, Lock, OctagonAlert, RefreshCw, Terminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { apiHeaders, usePoll } from "../../api";
import {
  EPISODE_OUTCOMES,
  availableMissionIds,
  episodeSyncPoints,
  fetchOptionalEpisodes,
  filterEpisodes,
  outcomeTone,
  type EpisodeOutcome,
  type EpisodesResponse,
  type TaskEpisode,
} from "../../episodes.ts";
import { fetchGrading } from "../../gradingApi";
import type {
  GradingCatalog,
  GradingPreset,
  TopDownTierGroup,
} from "../../grading.ts";
import { gradeTone, groupByTopDownTier, presetStateTone } from "../../grading.ts";
import { Fact, SectionLabel } from "../fleet/bits";
import { Badge } from "../ui/badge";
import { Card, CardHeader, EmptyState, LiveStatus, PageHeader, cx } from "../ui";

const POLL_MS = 10_000;

/**
 * Read-only Top-Down 3-tier Agent Grading view backed by `GET /api/grading`.
 * Shows `T3 top-decision` (`claude-fable-5-2`), `T2 mainline`
 * (`gemini-pro-orchestrator-neon`), opt-in `T1 mechanical-offload`, and
 * every preset's routing metadata, lock state, and re-probe status.
 */
export function GradingView() {
  const grading = usePoll<GradingCatalog>("grading", fetchGrading, POLL_MS);
  const data = grading.data;
  const groups = data ? groupByTopDownTier(data.presets) : [];
  const routableCount =
    data?.presets.filter((preset) => preset.routing_grade !== null).length ?? 0;

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Agent grading
          </h1>
        }
      >
        <LiveStatus
          error={grading.error}
          label={
            data
              ? `${data.presets.length} preset${data.presets.length === 1 ? "" : "s"} · ${routableCount} routable`
              : "Connecting"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {grading.error
              ? `Could not reach the grading API: ${grading.error}`
              : "Loading grading catalog…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1400px] flex-col gap-4 p-3 sm:p-4">
            <TopDownOverview groups={groups} profiles={data.profiles} />

            {data.presets.length === 0 ? (
              <Card>
                <EmptyState
                  icon={<Layers size={20} />}
                  title="Grading catalog is empty"
                  hint={
                    <span>
                      Seed the top-down stack with{" "}
                      <code className="font-mono select-all">spawn grade init-topdown</code> (add{" "}
                      <code className="font-mono select-all">--with-l1</code> for mechanical offload).
                    </span>
                  }
                />
              </Card>
            ) : (
              <div className="flex flex-col gap-3">
                {groups.map((group) => (
                  <TierSection key={group.id} group={group} />
                ))}
              </div>
            )}

            <EpisodesTimelineSection />
          </div>
        </div>
      )}
    </main>
  );
}

function TopDownOverview({
  groups,
  profiles,
}: {
  groups: TopDownTierGroup[];
  profiles: string[];
}) {
  const tiers = groups.filter((group) => group.id !== "ungraded");
  return (
    <div className="space-y-2.5">
      <div className="grid gap-3 md:grid-cols-3">
        {tiers.map((tier) => {
          const active = tier.presets.filter((preset) => preset.state === "active");
          const lead = active[0] ?? tier.presets[0] ?? null;
          return (
            <Card key={tier.id} className="p-3.5">
              <div className="flex items-center justify-between gap-2">
                <div className="flex items-center gap-1.5">
                  <Badge tone={gradeTone(tier.grade)}>{tier.grade ?? "—"}</Badge>
                  {tier.tierRole && (
                    <span className="font-mono text-micro text-muted-foreground">
                      {tier.tierRole}
                    </span>
                  )}
                </div>
                <span className="tnum text-micro text-faint-foreground">
                  {active.length}/{tier.presets.length} active
                </span>
              </div>
              <div className="mt-2 text-label font-semibold text-foreground">
                {tier.label}
              </div>
              <p className="mt-0.5 text-caption text-muted-foreground">
                {tier.subtitle}
              </p>
              <div className="mt-2.5 border-t border-surface-border pt-2 text-caption">
                {lead ? (
                  <div className="flex flex-wrap items-center gap-1.5">
                    <Cpu size={12} className="shrink-0 text-faint-foreground" />
                    <span className="font-mono text-foreground">{lead.engine_label}</span>
                    <span className="text-faint-foreground">/</span>
                    <span className="font-mono font-medium text-foreground">
                      {lead.model_label}
                    </span>
                    {lead.locked_grade && (
                      <Lock
                        size={11}
                        className="ml-auto shrink-0 text-muted-foreground"
                        aria-label={`Locked at ${lead.locked_grade}`}
                      />
                    )}
                  </div>
                ) : (
                  <span className="text-faint-foreground">
                    {tier.id === "t1"
                      ? "Not enabled (T1 offload is opt-in via --with-l1; L1 work falls back upward to T2)"
                      : "No presets configured for this tier"}
                  </span>
                )}
              </div>
            </Card>
          );
        })}
      </div>

      {profiles.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 px-1 text-caption text-muted-foreground">
          <SectionLabel>Execution profiles</SectionLabel>
          {profiles.map((profile) => (
            <Badge key={profile} tone="neutral">
              <span className="font-mono">{profile}</span>
            </Badge>
          ))}
        </div>
      )}
    </div>
  );
}

function TierSection({ group }: { group: TopDownTierGroup }) {
  return (
    <Card>
      <CardHeader
        icon={<Layers size={14} />}
        title={
          <span className="flex items-center gap-2">
            <span>{group.label}</span>
            {group.tierRole && (
              <Badge tone={gradeTone(group.grade)}>{group.tierRole}</Badge>
            )}
          </span>
        }
        meta={
          <span className="tnum">
            {group.presets.length} preset{group.presets.length === 1 ? "" : "s"}
          </span>
        }
      />

      {group.presets.length === 0 ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">
          {group.id === "t1" ? (
            <>
              Opt-in mechanical offload is off. Seed with{" "}
              <code className="font-mono select-all">spawn grade init-topdown --with-l1</code> to
              enable <code className="font-mono">T1</code>; meanwhile deterministic{" "}
              <code className="font-mono">l1</code> tasks fall back upward to{" "}
              <code className="font-mono">T2 mainline</code>.
            </>
          ) : (
            "No presets currently in this tier."
          )}
        </p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {group.presets.map((preset) => (
            <PresetRow key={preset.preset_id} preset={preset} />
          ))}
        </ul>
      )}
    </Card>
  );
}

function PresetRow({ preset }: { preset: GradingPreset }) {
  return (
    <li className="flex flex-col gap-2 px-4 py-3 lg:flex-row lg:items-center lg:gap-4">
      <div className="min-w-0 flex-1 space-y-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-label font-semibold text-foreground">
            {preset.preset_id}
          </span>
          <Badge tone={presetStateTone(preset.state)}>{preset.state}</Badge>
          <Badge tone={gradeTone(preset.routing_grade)}>
            route {preset.routing_grade ?? "none"}
          </Badge>
          {preset.tier_role && <Badge tone="neutral">{preset.tier_role}</Badge>}
          {preset.locked_grade && (
            <Badge
              tone="info"
              icon={<Lock size={11} />}
              title={`Grade locked at ${preset.locked_grade}; calibration leaves this preset alone`}
            >
              locked {preset.locked_grade}
            </Badge>
          )}
          {preset.reprobe_due && (
            <Badge
              tone="warning"
              icon={<RefreshCw size={11} />}
              title="Due an anti-starvation re-probe when SPAWN_GRADING_REPROBE_SECS is enabled"
            >
              re-probe due
            </Badge>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-caption text-muted-foreground">
          <span className="inline-flex items-center gap-1 font-mono text-foreground/90">
            <Terminal size={11} className="text-faint-foreground" />
            {preset.engine_label}/{preset.model_label}
          </span>
          <span>
            driver{" "}
            <code className="font-mono text-foreground/85">{preset.driver}</code>
            {preset.label_only && (
              <span className="ml-1 text-faint-foreground">(label-only)</span>
            )}
          </span>
        </div>
      </div>

      <div className="grid shrink-0 grid-cols-3 gap-4 sm:grid-cols-5">
        <Fact label="Profile" mono>
          <span className={cx(preset.execution_profile && "text-foreground")}>
            {preset.profile}
          </span>
        </Fact>
        <Fact label="Model" mono>
          {preset.model_profile}
        </Fact>
        <Fact label="Cost rank" mono>
          {preset.cost_rank}
        </Fact>
        <Fact label="Calibrated" mono>
          {preset.calibrated_grade ?? "—"}
        </Fact>
        <Fact label="Onboarded" mono>
          {preset.onboarded_grade ?? "—"}
        </Fact>
      </div>
    </li>
  );
}

function useOptionalEpisodesState() {
  const [data, setData] = useState<EpisodesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const etagRef = useRef<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let activeController: AbortController | null = null;

    const load = async () => {
      activeController?.abort();
      const controller = new AbortController();
      activeController = controller;
      const result = await fetchOptionalEpisodes({
        signal: controller.signal,
        headers: apiHeaders(),
        etag: etagRef.current,
      });
      if (cancelled || controller.signal.aborted) return;
      if (result.ok) {
        etagRef.current = result.etag;
        setError(null);
        if (!result.notModified) {
          setData(result.data);
        }
      } else {
        setError(result.error);
      }
    };

    void load();
    const timer = setInterval(() => {
      void load();
    }, POLL_MS);

    return () => {
      cancelled = true;
      activeController?.abort();
      clearInterval(timer);
    };
  }, []);

  return { data, error };
}

function EpisodesTimelineSection() {
  const { data, error } = useOptionalEpisodesState();
  const [missionFilter, setMissionFilter] = useState<string>("all");
  const [outcomeFilter, setOutcomeFilter] = useState<EpisodeOutcome | "all">("all");
  const [reworkOnly, setReworkOnly] = useState(false);

  const episodes = data?.episodes ?? [];
  const missionIds = availableMissionIds(episodes);
  const parsedMission = missionFilter === "all" ? null : Number(missionFilter);
  const filtered = filterEpisodes(episodes, {
    missionId: parsedMission !== null && Number.isFinite(parsedMission) ? parsedMission : null,
    outcome: outcomeFilter,
    reworkOnly,
  })
    .slice()
    .sort((a, b) => b.assigned_seq - a.assigned_seq);

  return (
    <Card>
      <CardHeader
        icon={<Clock size={14} />}
        title={
          <span className="flex flex-wrap items-center gap-2">
            <span>Control-stream Episode Replay &amp; Sync Points</span>
            {data && (
              <Badge tone={data.parity_ok ? "success" : "danger"}>
                {data.parity_ok ? "SQL parity OK" : `parity drift (${data.drift_count})`}
              </Badge>
            )}
            {data && data.warnings > 0 && (
              <Badge tone="warning" icon={<OctagonAlert size={11} />}>
                {data.warnings} compat skip{data.warnings === 1 ? "" : "s"}
              </Badge>
            )}
          </span>
        }
        meta={
          data ? (
            <span className="tnum">
              {filtered.length}/{data.summary.total} episode
              {data.summary.total === 1 ? "" : "s"} · {data.summary.review_sync_points} review ·{" "}
              {data.summary.gate_verdicts} gate
            </span>
          ) : (
            <span>Read-only replay</span>
          )
        }
      />

      {error && !data && (
        <p className="border-b border-surface-border px-4 py-2.5 text-caption text-faint-foreground">
          Episode replay feed unavailable ({error}); grading catalog remains unaffected.
        </p>
      )}

      {data && (
        <div className="space-y-3 p-4">
          <div className="flex flex-wrap items-center justify-between gap-2 border-b border-surface-border pb-3">
            <div className="flex flex-wrap items-center gap-2 text-caption">
              <Badge tone="success">landed {data.summary.landed}</Badge>
              <Badge tone="success">accepted {data.summary.accepted}</Badge>
              <Badge tone="info">in flight {data.summary.in_flight}</Badge>
              <Badge tone="warning">rework {data.summary.total_rework}</Badge>
              <Badge tone="neutral">reroutes {data.summary.total_reroutes}</Badge>
            </div>

            <div className="flex flex-wrap items-center gap-2 text-caption">
              <label className="flex items-center gap-1.5 text-muted-foreground">
                <span>Mission</span>
                <select
                  aria-label="Filter episodes by mission"
                  value={missionFilter}
                  onChange={(e) => setMissionFilter(e.target.value)}
                  className="rounded border border-surface-border bg-surface px-2 py-1 text-caption text-foreground"
                >
                  <option value="all">All</option>
                  {missionIds.map((mid) => (
                    <option key={mid} value={String(mid)}>
                      #{mid}
                    </option>
                  ))}
                </select>
              </label>

              <label className="flex items-center gap-1.5 text-muted-foreground">
                <span>Outcome</span>
                <select
                  aria-label="Filter episodes by outcome"
                  value={outcomeFilter}
                  onChange={(e) => setOutcomeFilter(e.target.value as EpisodeOutcome | "all")}
                  className="rounded border border-surface-border bg-surface px-2 py-1 text-caption text-foreground"
                >
                  <option value="all">All</option>
                  {EPISODE_OUTCOMES.map((out) => (
                    <option key={out} value={out}>
                      {out}
                    </option>
                  ))}
                </select>
              </label>

              <label className="inline-flex cursor-pointer items-center gap-1.5 rounded border border-surface-border bg-surface px-2 py-1 text-caption text-foreground">
                <input
                  type="checkbox"
                  checked={reworkOnly}
                  onChange={(e) => setReworkOnly(e.target.checked)}
                />
                <span>rework &gt; 0</span>
              </label>
            </div>
          </div>

          {filtered.length === 0 ? (
            <p className="py-2 text-caption text-faint-foreground">
              No replayed episodes match the selected filter.
            </p>
          ) : (
            <ul className="divide-y divide-surface-border">
              {filtered.map((ep) => (
                <EpisodeRow key={ep.task_id} episode={ep} />
              ))}
            </ul>
          )}
        </div>
      )}
    </Card>
  );
}

function EpisodeRow({ episode }: { episode: TaskEpisode }) {
  const syncPoints = episodeSyncPoints(episode);
  const rerouted =
    episode.first_preset_id !== null &&
    episode.preset_id !== null &&
    episode.first_preset_id !== episode.preset_id;

  return (
    <li className="space-y-2 py-3 first:pt-0 last:pb-0">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-label font-semibold text-foreground">
          T{episode.task_id}
        </span>
        <span className="font-mono text-micro text-faint-foreground">
          M#{episode.mission_id}
        </span>
        <Badge tone={outcomeTone(episode.outcome)}>{episode.outcome}</Badge>
        <Badge tone="neutral">{episode.state}</Badge>
        <Badge tone={gradeTone(episode.difficulty)}>{episode.difficulty}</Badge>
        {episode.preset_id && (
          <span className="font-mono text-micro text-muted-foreground">
            {rerouted ? `${episode.first_preset_id} -> ${episode.preset_id}` : episode.preset_id}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2 font-mono text-micro text-faint-foreground">
          <span>rework {episode.rework}</span>
          {episode.quality_rejections > 0 && <span>rej {episode.quality_rejections}</span>}
          {episode.reroutes > 0 && <span>reroute {episode.reroutes}</span>}
        </span>
      </div>

      {episode.brief && (
        <p className="line-clamp-2 text-caption text-muted-foreground">{episode.brief}</p>
      )}

      {syncPoints.length > 0 && (
        <ul
          aria-label={`Sync points for task ${episode.task_id}`}
          className="space-y-1 border-l-2 border-surface-border pl-3"
        >
          {syncPoints.map((item) => {
            if (item.kind === "review") {
              const sp = item.sync;
              return (
                <li
                  key={`review-${sp.event_seq}`}
                  className="flex flex-wrap items-center gap-2 text-micro text-muted-foreground"
                >
                  <Badge
                    tone={sp.accepted ? "success" : "warning"}
                    icon={sp.accepted ? <Check size={10} /> : undefined}
                  >
                    review {sp.accepted ? "accepted" : "revise"}
                  </Badge>
                  <span className="font-mono text-faint-foreground">
                    rev {sp.revision} · seq {sp.event_seq}
                  </span>
                  {sp.note && <span className="text-foreground/85">{sp.note}</span>}
                </li>
              );
            }
            const gv = item.sync;
            const gateTone =
              gv.verdict === "accepted"
                ? "success"
                : gv.verdict === "rejected" || gv.verdict === "aborted"
                  ? "danger"
                  : "warning";
            return (
              <li
                key={`gate-${gv.event_seq}`}
                className="flex flex-wrap items-center gap-2 text-micro text-muted-foreground"
              >
                <Badge tone={gateTone}>gate {gv.verdict}</Badge>
                <span className="font-mono text-faint-foreground">
                  rev {gv.revision} · seq {gv.event_seq}
                  {gv.class ? ` · ${gv.class}` : ""}
                </span>
                {gv.note && <span className="text-foreground/85">{gv.note}</span>}
              </li>
            );
          })}
        </ul>
      )}
    </li>
  );
}
