import { Cpu, Layers, Lock, RefreshCw, Terminal } from "lucide-react";

import { usePoll } from "../../api";
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
