import { Bot, Hourglass, Wrench } from "lucide-react";

import type { FleetAgent } from "../../fleet";
import { FLEET_STATES, jobsText, onOwnJob, percent, stateLabelUpper } from "../../fleet";
import { absoluteTitle } from "../../format";
import type { AgentGroup, AgentGroupId, FleetContext, TaskRef } from "../../ordering";
import { AGENT_GROUP_LABEL, heldTask, lastTask } from "../../ordering";
import { useStoredChoice } from "../ViewToggle";
import { Ago, Card, CardHeader, EmptyState, FOCUS_RING_INSET, GroupHeader, LabeledId, cx, sinceText } from "../ui";
import { FleetStateDot, JudgeBadge } from "./bits";

/** One line for "what is it doing right now": its open tool; else, when the
 * rules judged it waiting at its prompt on a background job of its own, that
 * job and its age (`job`); else its latest event. */
export function doingNow(agent: FleetAgent): {
  label: string;
  tool: boolean;
  job: boolean;
  at: number | null;
} {
  const activity = agent.activity;
  if (activity.current_tool) {
    return { label: activity.current_tool, tool: true, job: false, at: activity.last_event_at_ms };
  }
  const jobs = onOwnJob(agent) ? jobsText(activity.jobs) : null;
  if (jobs) {
    return { label: `background job: ${jobs}`, tool: false, job: true, at: activity.last_event_at_ms };
  }
  return {
    label: activity.last_event_name ?? "—",
    tool: false,
    job: false,
    at: activity.last_event_at_ms,
  };
}

/** A live worker on a task this long without an event is quiet enough to
 * look at: the dispatcher's activity stall (SPAWN_STALL_ACTIVITY_SECS). One
 * waiting at its prompt on its own background job is working, not quiet:
 * the dispatcher does not stall it either. */
export const QUIET_MS = 15 * 60_000;

export function isQuiet(agent: FleetAgent, now = Date.now()): boolean {
  if (onOwnJob(agent)) return false;
  const at = agent.activity.last_event_at_ms;
  return at !== null && now - at > QUIET_MS;
}

/** The icon of "doing now": a tool, or a background job. */
export function DoingIcon({ now, size = 12 }: { now: { tool: boolean; job: boolean }; size?: number }) {
  if (now.tool) return <Wrench size={size} className="shrink-0 text-success" />;
  if (now.job) return <Hourglass size={size} className="shrink-0 text-success" />;
  return null;
}

const COLUMNS =
  "lg:grid lg:grid-cols-[minmax(0,26rem)_minmax(0,1.8fr)_minmax(0,1fr)_6rem_5rem_5.5rem] lg:items-center lg:gap-4";

/** The fleet below the always-on agents: on a task, idle, ended (folded,
 * remembered). The groups and their order come from ordering.ts, as the
 * sidebar's do. */
export function FleetBoard({
  groups,
  total,
  context,
  usualEngine,
  selectedId,
  onSelect,
}: {
  groups: AgentGroup<FleetAgent>[];
  total: FleetAgent[];
  context: FleetContext;
  usualEngine: string | null;
  selectedId: string | null;
  onSelect: (sessionId: string) => void;
}) {
  const [ended, setEnded] = useStoredChoice("spawn.fleet.ended", ["open", "closed"] as const, "closed");
  const shown = groups.filter((group) => group.id !== "always_on");
  const live = total.filter((agent) => agent.live).length;
  const byState = FLEET_STATES.map((state) => [
    state,
    total.filter((agent) => agent.live && agent.state === state).length,
  ] as const).filter(([, count]) => count > 0);

  return (
    <Card className="flex shrink-0 flex-col overflow-hidden">
      <CardHeader
        icon={<Bot size={14} />}
        title="Fleet"
        meta={
          <span className="flex items-center gap-3">
            <span className="hidden items-center gap-2.5 sm:flex" title="Live agents by state">
              {byState.map(([state, count]) => (
                <span key={state} className="inline-flex items-center gap-1">
                  <FleetStateDot state={state} size="sm" />
                  <span className="tnum text-foreground">{count}</span>
                  {stateLabelUpper(state).toLowerCase()}
                </span>
              ))}
            </span>
            <span className="tnum">
              {live} live · {total.length} agents
            </span>
          </span>
        }
      />
      {total.length === 0 ? (
        <EmptyState
          title="No agents yet"
          hint="Agents appear here once the supervisor or a worker starts."
        />
      ) : (
        <>
          <div
            aria-hidden="true"
            className={cx(
              "hidden border-b border-surface-border px-4 py-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground",
              COLUMNS,
            )}
          >
            <span>Agent</span>
            <span>Task</span>
            <span>Doing now</span>
            <span className="text-right">Last event</span>
            <span className="text-right">In state</span>
            <span>Judged by</span>
          </div>
          {shown.map((group) => {
            const folds = group.id === "ended";
            const open = !folds || ended === "open";
            return (
              <section
                key={group.id}
                aria-label={AGENT_GROUP_LABEL[group.id]}
                className="border-b border-surface-border last:border-b-0"
              >
                <GroupHeader
                  className="bg-page-canvas/40 px-4 py-1.5"
                  label={AGENT_GROUP_LABEL[group.id]}
                  count={group.agents.length}
                  expanded={folds ? open : undefined}
                  onToggle={folds ? () => setEnded(open ? "closed" : "open") : undefined}
                  trailing={<GroupHint id={group.id} />}
                />
                {group.id === "idle" && !context.known && (
                  <p className="border-t border-surface-border px-4 py-2 text-caption text-warning">
                    Tasks unknown: /api/missions has not answered, so no worker shows as on a task.
                  </p>
                )}
                {/* Folded, the group still shows the agent open in the drawer. */}
                {(open || group.agents.some((agent) => agent.session_id === selectedId)) &&
                  (group.agents.length === 0 ? (
                    group.id === "on_task" &&
                    context.known && (
                      <p className="border-t border-surface-border px-4 py-2 text-caption text-faint-foreground">
                        No worker holds an open task.
                      </p>
                    )
                  ) : (
                    <ul className="divide-y divide-surface-border border-t border-surface-border">
                      {group.agents
                        .filter((agent) => open || agent.session_id === selectedId)
                        .map((agent) => (
                        <li key={agent.session_id}>
                          <AgentRow
                            agent={agent}
                            group={group.id}
                            held={heldTask(agent, context)}
                            last={lastTask(agent, context)}
                            usualEngine={usualEngine}
                            selected={agent.session_id === selectedId}
                            onSelect={onSelect}
                          />
                        </li>
                      ))}
                    </ul>
                  ))}
              </section>
            );
          })}
        </>
      )}
    </Card>
  );
}

function GroupHint({ id }: { id: AgentGroupId }) {
  const text =
    id === "on_task"
      ? "escalated first, then the quietest"
      : id === "idle"
        ? "live, no open task"
        : id === "ended"
          ? "newest end first"
          : null;
  return text ? <span className="text-micro text-faint-foreground">{text}</span> : null;
}

/** The task cell: the held task (mission, id, title, state), the last task
 * of an ended worker, else the fleet's own free text (the agent's last
 * input), else a muted "No task". */
export function TaskCell({
  task,
  ended = false,
  input = null,
}: {
  task: TaskRef | null;
  ended?: boolean;
  /** The fleet's `task` text: what the agent was last told, when no
   * mission task matches it. */
  input?: string | null;
}) {
  if (!task) {
    const line = input?.split("\n").map((part) => part.trim()).find((part) => part !== "");
    if (line) {
      return (
        <span className="flex min-w-0 flex-col text-caption" title={input ?? undefined}>
          <span className="min-w-0 truncate text-muted-foreground">{line}</span>
          <span className="text-micro text-faint-foreground">last input · no mission task</span>
        </span>
      );
    }
    return <span className="text-caption text-faint-foreground">No task</span>;
  }
  return (
    <span className="flex min-w-0 flex-col">
      <span className="flex min-w-0 items-baseline gap-1.5 text-caption">
        <span className="tnum shrink-0 font-mono text-micro text-faint-foreground">
          T{task.task_id ?? "?"}
        </span>
        <span className={cx("min-w-0 truncate", ended ? "text-muted-foreground" : "text-foreground")}>
          {task.title ?? "Untitled task"}
        </span>
      </span>
      <span className="flex min-w-0 items-center gap-1.5 text-micro text-muted-foreground">
        <span className="min-w-0 truncate">
          #{task.mission_id ?? "?"} {task.mission_name}
        </span>
        <span aria-hidden="true" className="text-faint-foreground">·</span>
        <span className={cx("shrink-0", task.label === "sent back" && "text-warning")}>
          {ended ? `last: ${task.label}` : task.label}
        </span>
      </span>
    </span>
  );
}

function AgentRow({
  agent,
  group,
  held,
  last,
  usualEngine,
  selected,
  onSelect,
}: {
  agent: FleetAgent;
  group: AgentGroupId;
  held: TaskRef | null;
  last: TaskRef | null;
  usualEngine: string | null;
  selected: boolean;
  onSelect: (sessionId: string) => void;
}) {
  const now = doingNow(agent);
  const judgement = agent.judgement;
  // Quiet matters for a worker that should be moving: one on a task.
  const quiet = group === "on_task" && agent.live && isQuiet(agent);
  const lost = group === "on_task" && !agent.live;
  const engine = agent.engine && agent.engine !== usualEngine ? agent.engine : null;
  return (
    <button
      type="button"
      onClick={() => onSelect(agent.session_id)}
      aria-current={selected ? "true" : undefined}
      className={cx(
        "relative flex w-full flex-wrap items-center gap-x-3 gap-y-1.5 px-4 py-2 text-left transition-colors",
        COLUMNS,
        FOCUS_RING_INSET,
        selected ? "bg-surface-selected" : "hover:bg-surface-hover",
      )}
    >
      {agent.attention && (
        <span
          aria-hidden="true"
          className="absolute inset-y-0 left-0 w-0.5 bg-warning"
        />
      )}

      {/* Agent */}
      <div className="flex w-full min-w-0 items-center gap-2 lg:w-auto">
        <FleetStateDot state={agent.state} />
        <div className="min-w-0">
          <LabeledId
            label={agent.label}
            id={agent.agent_id || agent.session_id}
            className="text-label font-medium text-foreground"
          />
          <div className="mt-0.5 flex min-w-0 items-center gap-1.5 whitespace-nowrap text-micro text-muted-foreground">
            <span>{stateLabelUpper(agent.state)}</span>
            {agent.attention && (
              <span className="font-medium text-warning">· escalated</span>
            )}
            {lost && (
              <span className="font-medium text-warning" title="Its engine ended while it holds an open task">
                · engine ended
              </span>
            )}
            {engine && <span className="text-faint-foreground">· {engine}</span>}
          </div>
        </div>
      </div>

      {/* Task */}
      <div className="w-full min-w-0 pl-4 lg:w-auto lg:pl-0">
        <TaskCell task={held ?? (group === "ended" ? last : null)} ended={!held} input={agent.task} />
      </div>

      {/* Doing now */}
      <div className="flex w-full min-w-0 items-center gap-1.5 pl-4 text-caption lg:w-auto lg:pl-0">
        <DoingIcon now={now} />
        <span
          className={cx(
            "min-w-0 truncate font-mono",
            now.tool || now.job ? "text-foreground" : "text-muted-foreground",
          )}
          title={now.job ? "Its turn ended while a job it started still runs; the engine wakes it when the job reports" : undefined}
        >
          {now.label}
        </span>
      </div>

      {/* Last event */}
      <div
        className={cx(
          "pl-4 text-caption lg:pl-0 lg:text-right",
          quiet ? "font-medium text-warning" : "text-muted-foreground",
        )}
      >
        <span className="lg:hidden">last event </span>
        <Ago ms={now.at} />
        {quiet && <span className="sr-only"> (quiet)</span>}
      </div>

      {/* In state */}
      <div className="tnum pl-4 text-caption text-muted-foreground lg:pl-0 lg:text-right">
        <span className="lg:hidden">in state </span>
        <span title={absoluteTitle(agent.state_since_ms)}>{sinceText(agent.state_since_ms)}</span>
      </div>

      {/* Judged by */}
      <div
        className="flex items-center gap-1.5 pl-4 lg:pl-0"
        title={`Confidence of the latest judgement: ${percent(judgement?.confidence)}`}
      >
        <JudgeBadge judge={judgement?.judge} compact />
        <span className="tnum text-caption text-muted-foreground">
          {percent(judgement?.confidence)}
        </span>
      </div>
    </button>
  );
}
