import {
  ArrowLeft,
  ArrowRight,
  ArrowUpRight,
  Check,
  Circle,
  ExternalLink,
  FileCode,
  GitBranch,
  Link2,
  OctagonAlert,
  UserRoundX,
  X,
} from "lucide-react";

import { clockTime, relativeTime } from "../../format";
import type {
  Feature,
  FeatureDeploy,
  FeatureLanding,
  FeatureMission,
  FeatureSource,
  FeatureTask,
} from "../../features";
import {
  STATE_INFO,
  STATES,
  blockerText,
  deployNote,
  gapText,
  isExternal,
  landingLabel,
  newestVerification,
  objectiveName,
  ownerText,
  sourceLabel,
  verificationNote,
} from "../../features";
import type {
  FeatureMindMapTree,
  FileIndex,
  MindMapTreeNode,
  RelationIndex,
} from "../../featuresGraph";
import type { FeatureTruthResponse } from "../../featureCoverage";
import { featureTruthSummaryFor, purityCodeLabel } from "../../featureCoverage";
import { RELATION_META, getSharedCoOwners } from "../../featuresGraph";
import { FOCUS_RING, cx } from "../ui";

export interface FeatureInspectorProps {
  mission: FeatureMission;
  node: MindMapTreeNode;
  tree: FeatureMindMapTree;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  truth?: FeatureTruthResponse | null;
  onSelectNode: (nodeId: string) => void;
  onOpenSession: (sessionId: string) => void;
  onClose: () => void;
}

/** Detail side-panel for the selected Mind Map node (feature, branch cluster,
 * or mission root). Surfaces all 4 independent state booleans, typed outgoing &
 * incoming links, many-to-many shared files, owner session link, evidence,
 * provenance sources, gaps, and blockers. */
export function FeatureInspector({
  mission,
  node,
  tree,
  fileIndex,
  relationIndex,
  truth = null,
  onSelectNode,
  onOpenSession,
  onClose,
}: FeatureInspectorProps) {
  const truthSummary = node.feature ? featureTruthSummaryFor(truth, node.feature.id) : null;
  return (
    <aside
      aria-label={`Inspector: ${node.title}`}
      className="flex h-full w-full flex-col border-l border-surface-border bg-surface lg:w-[390px] xl:w-[420px] lg:shrink-0"
    >
      <header className="flex shrink-0 items-start gap-2 border-b border-surface-border px-4 py-3">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-1.5">
            {node.feature ? (
              <>
                <span
                  className={cx(
                    "inline-flex items-center gap-1 rounded-chip px-1.5 py-px text-micro font-medium ring-1 ring-inset",
                    STATE_INFO[node.feature.state].badge,
                  )}
                >
                  <span className={cx("size-1.5 rounded-full", STATE_INFO[node.feature.state].dot)} />
                  {STATE_INFO[node.feature.state].label}
                </span>
                <span className="rounded-chip bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground">
                  {node.feature.kind}
                </span>
                {node.feature.complete && (
                  <span className="rounded-chip bg-ok-tint px-1.5 py-px text-micro font-medium text-success">
                    complete
                  </span>
                )}
                {node.feature.live && !node.feature.verified && (
                  <span className="rounded-chip bg-wait-tint px-1.5 py-px text-micro text-warning">
                    unverified live
                  </span>
                )}
                {truthSummary && (
                  <span
                    className={cx(
                      "rounded-chip px-1.5 py-px text-micro font-medium",
                      truthSummary.status === "error"
                        ? "bg-fail-tint text-destructive"
                        : truthSummary.status === "warn"
                          ? "bg-wait-tint text-warning"
                          : "bg-ok-tint text-success",
                    )}
                  >
                    {truthSummary.status === "error"
                      ? `truth/purity: ${truthSummary.truthErrors + truthSummary.purityErrors} err`
                      : truthSummary.status === "warn"
                        ? `truth: ${truthSummary.truthWarnings + truthSummary.purityWarnings} warn`
                        : "truth clean"}
                  </span>
                )}
              </>
            ) : (
              <span className="rounded-chip bg-surface-selected px-1.5 py-px text-micro font-medium text-muted-foreground">
                {node.nodeKind === "mission" ? `Mission #${mission.id}` : "Branch group"}
              </span>
            )}
          </div>
          <h2 className="mt-1.5 text-body font-semibold text-foreground">{node.title}</h2>
          <p className="mt-0.5 truncate font-mono text-micro text-faint-foreground">{node.id}</p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close inspector"
          className={cx(
            "grid size-7 shrink-0 place-items-center rounded-control text-muted-foreground hover:bg-surface-hover hover:text-foreground",
            FOCUS_RING,
          )}
        >
          <X size={15} />
        </button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3.5">
        {node.feature ? (
          <FeatureDetailBody
            feature={node.feature}
            mission={mission}
            tree={tree}
            fileIndex={fileIndex}
            relationIndex={relationIndex}
            truthSummary={truthSummary}
            onSelectNode={onSelectNode}
            onOpenSession={onOpenSession}
          />
        ) : (
          <GroupDetailBody
            node={node}
            mission={mission}
            tree={tree}
            fileIndex={fileIndex}
            relationIndex={relationIndex}
            onSelectNode={onSelectNode}
          />
        )}
      </div>
    </aside>
  );
}

function FeatureDetailBody({
  feature,
  mission,
  tree,
  fileIndex,
  relationIndex,
  truthSummary,
  onSelectNode,
  onOpenSession,
}: {
  feature: Feature;
  mission: FeatureMission;
  tree: FeatureMindMapTree;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  truthSummary: ReturnType<typeof featureTruthSummaryFor>;
  onSelectNode: (nodeId: string) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const outgoing = relationIndex.outgoing[feature.id] ?? [];
  const incoming = relationIndex.incoming[feature.id] ?? [];
  const newest = newestTask(feature);
  const parentNode = feature.parent ? tree.nodes[feature.parent] : null;
  const childIds = tree.nodes[feature.id]?.children ?? [];

  return (
    <div className="flex flex-col gap-4">
      {feature.summary && (
        <p className="text-label text-muted-foreground">{feature.summary}</p>
      )}

      {feature.blocked && (
        <div className="flex items-start gap-2 rounded-control border border-destructive/30 bg-fail-tint px-3 py-2 text-caption text-destructive">
          <OctagonAlert size={14} className="mt-0.5 shrink-0" />
          <span className="min-w-0">{blockerText(feature.blocked)}</span>
        </div>
      )}

      {/* 4 independent state booleans */}
      <InspectorSection title="State evidence (4 independent flags)">
        <div className="grid grid-cols-2 gap-1.5">
          {(
            [
              ["planned", "Planned", feature.planned],
              ["implemented", "Implemented", feature.implemented],
              ["verified", "Verified", feature.verified],
              ["live", "Live", feature.live],
            ] as const
          ).map(([key, label, value]) => (
            <div
              key={key}
              className={cx(
                "flex items-center justify-between rounded-control border px-2.5 py-1.5 text-caption",
                value
                  ? "border-surface-border bg-surface-selected text-foreground"
                  : "border-surface-border/70 bg-page-canvas text-faint-foreground",
              )}
            >
              <span className="inline-flex items-center gap-1.5">
                <span
                  className={cx(
                    "size-1.5 rounded-full",
                    value ? STATE_INFO[key].dot : "bg-surface-border",
                  )}
                />
                {label}
              </span>
              <span className="inline-flex items-center gap-0.5 font-mono text-micro">
                {value ? <Check size={11} className="text-success" /> : <Circle size={9} />}
                {value ? "yes" : "no"}
              </span>
            </div>
          ))}
        </div>
      </InspectorSection>

      {/* Hierarchy & Tags */}
      {(parentNode || childIds.length > 0 || feature.tags.length > 0) && (
        <InspectorSection title="Hierarchy & tags" icon={<GitBranch size={12} />}>
          <div className="flex flex-col gap-2">
            {parentNode && (
              <div className="flex items-center gap-2 text-caption">
                <span className="text-faint-foreground">Parent</span>
                <button
                  type="button"
                  onClick={() => onSelectNode(parentNode.id)}
                  className={cx(
                    "inline-flex min-w-0 items-center gap-1 rounded-chip bg-surface-selected px-2 py-0.5 text-caption text-info hover:underline",
                    FOCUS_RING,
                  )}
                >
                  <span className="truncate">{parentNode.title}</span>
                  <ArrowUpRight size={11} className="shrink-0" />
                </button>
              </div>
            )}
            {childIds.length > 0 && (
              <div className="flex flex-wrap items-center gap-1">
                <span className="mr-1 text-caption text-faint-foreground">
                  Children ({childIds.length})
                </span>
                {childIds.slice(0, 8).map((childId) => {
                  const child = tree.nodes[childId];
                  if (!child) return null;
                  return (
                    <button
                      key={childId}
                      type="button"
                      onClick={() => onSelectNode(childId)}
                      className={cx(
                        "inline-flex max-w-full items-center gap-1 rounded-chip bg-muted px-2 py-0.5 text-micro text-foreground hover:bg-surface-hover",
                        FOCUS_RING,
                      )}
                    >
                      {child.feature && (
                        <span
                          className={cx("size-1.5 rounded-full", STATE_INFO[child.feature.state].dot)}
                        />
                      )}
                      <span className="truncate">{child.title}</span>
                    </button>
                  );
                })}
                {childIds.length > 8 && (
                  <span className="text-micro text-faint-foreground">
                    +{childIds.length - 8} more
                  </span>
                )}
              </div>
            )}
            {feature.tags.length > 0 && (
              <div className="flex flex-wrap gap-1">
                {feature.tags.map((tag) => (
                  <span
                    key={tag}
                    className="rounded-chip bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground"
                  >
                    #{tag}
                  </span>
                ))}
              </div>
            )}
          </div>
        </InspectorSection>
      )}

      {/* Typed Feature Links */}
      <InspectorSection
        title={`Typed feature links (${outgoing.length + incoming.length})`}
        icon={<Link2 size={12} />}
      >
        {outgoing.length === 0 && incoming.length === 0 ? (
          <p className="text-caption text-faint-foreground">No typed links declared.</p>
        ) : (
          <div className="flex flex-col gap-2">
            {outgoing.length > 0 && (
              <div className="flex flex-col gap-1">
                <p className="text-micro font-medium text-faint-foreground">Outgoing</p>
                {outgoing.map((rel) => {
                  const meta = RELATION_META[rel.kind];
                  const targetFeature = rel.targetFeature;
                  return (
                    <button
                      key={`out:${rel.kind}:${rel.target}`}
                      type="button"
                      onClick={() => onSelectNode(rel.target)}
                      className={cx(
                        "flex w-full items-center gap-2 rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5 text-left transition-colors hover:bg-surface-hover",
                        FOCUS_RING,
                      )}
                    >
                      <ArrowRight size={11} className="shrink-0 text-faint-foreground" />
                      <span
                        className={cx(
                          "shrink-0 rounded-chip px-1.5 py-px text-micro font-medium",
                          meta.badgeClass,
                        )}
                      >
                        {meta.label}
                      </span>
                      <span className="min-w-0 flex-1 truncate text-caption text-foreground">
                        {targetFeature?.title ?? rel.target}
                      </span>
                      {targetFeature && (
                        <span
                          title={STATE_INFO[targetFeature.state].label}
                          className={cx(
                            "size-2 shrink-0 rounded-full",
                            STATE_INFO[targetFeature.state].dot,
                          )}
                        />
                      )}
                      {rel.label && (
                        <span className="hidden max-w-[110px] truncate text-micro text-faint-foreground sm:inline">
                          {rel.label}
                        </span>
                      )}
                    </button>
                  );
                })}
              </div>
            )}

            {incoming.length > 0 && (
              <div className="flex flex-col gap-1">
                <p className="text-micro font-medium text-faint-foreground">Incoming</p>
                {incoming.map((rel) => {
                  const meta = RELATION_META[rel.kind];
                  const sourceFeature = rel.sourceFeature;
                  return (
                    <button
                      key={`in:${rel.kind}:${rel.source}`}
                      type="button"
                      onClick={() => onSelectNode(rel.source)}
                      className={cx(
                        "flex w-full items-center gap-2 rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5 text-left transition-colors hover:bg-surface-hover",
                        FOCUS_RING,
                      )}
                    >
                      <ArrowLeft size={11} className="shrink-0 text-faint-foreground" />
                      <span
                        className={cx(
                          "shrink-0 rounded-chip px-1.5 py-px text-micro font-medium",
                          meta.badgeClass,
                        )}
                      >
                        {meta.inverseLabel}
                      </span>
                      <span className="min-w-0 flex-1 truncate text-caption text-foreground">
                        {sourceFeature?.title ?? rel.source}
                      </span>
                      {sourceFeature && (
                        <span
                          title={STATE_INFO[sourceFeature.state].label}
                          className={cx(
                            "size-2 shrink-0 rounded-full",
                            STATE_INFO[sourceFeature.state].dot,
                          )}
                        />
                      )}
                    </button>
                  );
                })}
              </div>
            )}
          </div>
        )}
      </InspectorSection>

      {/* Linked Files (with multi-feature shared file co-owners) */}
      <InspectorSection
        title={`Linked files (${feature.files.length})`}
        icon={<FileCode size={12} />}
      >
        {feature.files.length === 0 ? (
          <p className="text-caption text-faint-foreground">No repo files mapped.</p>
        ) : (
          <ul className="flex flex-col gap-1.5">
            {feature.files.map((filePath) => {
              const coOwners = getSharedCoOwners(fileIndex, filePath, feature.id);
              const isShared = coOwners.length > 0;
              return (
                <li
                  key={filePath}
                  className={cx(
                    "rounded-control border px-2.5 py-1.5",
                    isShared
                      ? "border-info/30 bg-info-tint/35"
                      : "border-surface-border bg-page-canvas",
                  )}
                >
                  <div className="flex items-center gap-1.5">
                    <span
                      className="min-w-0 flex-1 truncate font-mono text-micro text-foreground"
                      title={filePath}
                    >
                      {filePath}
                    </span>
                    {isShared && (
                      <span className="shrink-0 rounded-chip bg-info-tint px-1.5 py-px text-micro font-medium text-info">
                        shared ({coOwners.length + 1})
                      </span>
                    )}
                  </div>
                  {isShared && (
                    <div className="mt-1.5 flex flex-wrap items-center gap-1">
                      <span className="text-micro text-faint-foreground">Also in:</span>
                      {coOwners.map((coOwner) => (
                        <button
                          key={coOwner.id}
                          type="button"
                          onClick={() => onSelectNode(coOwner.id)}
                          className={cx(
                            "inline-flex max-w-full items-center gap-1 rounded-chip bg-surface px-1.5 py-px text-micro text-info shadow-hairline hover:bg-surface-hover hover:underline",
                            FOCUS_RING,
                          )}
                        >
                          <span
                            className={cx(
                              "size-1.5 shrink-0 rounded-full",
                              STATE_INFO[coOwner.state].dot,
                            )}
                          />
                          <span className="truncate">{coOwner.title}</span>
                        </button>
                      ))}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </InspectorSection>

      {/* Objectives */}
      <InspectorSection title={`Objectives (${feature.objective_rows.length})`}>
        {feature.objective_rows.length === 0 ? (
          <p className="text-caption text-faint-foreground">No backlog objectives linked.</p>
        ) : (
          <ul className="flex flex-col gap-1">
            {feature.objective_rows.map((objective) => (
              <li
                key={objective.id ?? objective.title ?? "obj"}
                className="flex min-w-0 items-baseline gap-2 text-caption"
              >
                <span className="shrink-0 font-mono text-micro text-faint-foreground">
                  {objectiveName(mission.id, objective.id)}
                </span>
                <span className="min-w-0 flex-1 text-foreground">
                  {objective.title ?? "Untitled objective"}
                </span>
                {objective.status && (
                  <span className="shrink-0 rounded-chip bg-muted px-1.5 py-px text-micro text-muted-foreground">
                    {objective.status}
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </InspectorSection>

      {/* Owner & Session */}
      <InspectorSection title="Ownership">
        <div className="flex flex-col gap-1.5">
          {feature.unowned && (
            <span className="inline-flex w-fit items-center gap-1 rounded-chip bg-muted px-2 py-0.5 text-micro text-muted-foreground">
              <UserRoundX size={11} /> unowned (no task in flight)
            </span>
          )}
          <OwnerRow feature={feature} onOpenSession={onOpenSession} />
        </div>
      </InspectorSection>

      {/* Delivery & Verification Evidence */}
      <InspectorSection title={`Evidence (${feature.evidence.tasks_total} tasks)`}>
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5 text-caption">
          <dt className="text-faint-foreground">Landed</dt>
          <dd className="min-w-0 text-foreground">{landingText(feature)}</dd>
          <dt className="text-faint-foreground">Verified</dt>
          <dd className="min-w-0 text-foreground">
            {verificationNote(feature)}
            {newestVerification(feature)?.at_ms ? (
              <span className="text-faint-foreground">
                {" "}
                · {relativeTime(newestVerification(feature)?.at_ms)}
              </span>
            ) : null}
          </dd>
          <dt className="text-faint-foreground">Deployed</dt>
          <dd
            className="min-w-0 text-foreground"
            title={deployTitle(feature.evidence.deploy)}
          >
            {deployNote(feature)}
          </dd>
          {newest && (
            <>
              <dt className="text-faint-foreground">Review</dt>
              <dd className="min-w-0 text-foreground">{reviewText(newest)}</dd>
            </>
          )}
        </dl>
      </InspectorSection>

      {/* Provenance Sources */}
      {feature.sources.length > 0 && (
        <InspectorSection title={`Provenance sources (${feature.sources.length})`}>
          <div className="flex flex-wrap gap-1.5">
            {feature.sources.map((source) => (
              <SourcePill key={source.ref} source={source} />
            ))}
          </div>
        </InspectorSection>
      )}

      {/* Gaps */}
      {feature.gaps.length > 0 && (
        <InspectorSection title={`Gaps (${feature.gaps.length})`}>
          <div className="flex flex-wrap gap-1.5">
            {feature.gaps.map((gap) => (
              <span
                key={gap.kind + gap.objectives.join(",")}
                title={gap.detail ?? undefined}
                className="rounded-chip bg-wait-tint px-2 py-0.5 text-micro text-warning"
              >
                {gapText(gap, mission.id)}
              </span>
            ))}
          </div>
        </InspectorSection>
      )}

      {/* Read-only Dependency Truth & Purity */}
      {truthSummary &&
        (truthSummary.truthFindings.length > 0 || truthSummary.purityFindings.length > 0) && (
          <InspectorSection
            title={`Dependency truth & purity (${truthSummary.truthFindings.length + truthSummary.purityFindings.length})`}
          >
            <ul className="flex flex-col gap-1.5 text-micro">
              {truthSummary.truthFindings.map((finding, idx) => (
                <li
                  key={`${finding.code}:${idx}`}
                  className="rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5"
                >
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span
                      className={cx(
                        "rounded-chip px-1.5 py-px font-medium",
                        finding.severity === "error"
                          ? "bg-fail-tint text-destructive"
                          : finding.severity === "warn"
                            ? "bg-wait-tint text-warning"
                            : "bg-info-tint text-info",
                      )}
                    >
                      {finding.severity}
                    </span>
                    <span className="font-mono text-foreground">{finding.code}</span>
                    {finding.waived && <span className="text-faint-foreground">(waived)</span>}
                  </div>
                  {finding.fix && <p className="mt-1 text-muted-foreground">{finding.fix}</p>}
                  {finding.evidence.slice(0, 2).map((edge, edgeIdx) => (
                    <p key={edgeIdx} className="mt-0.5 truncate font-mono text-faint-foreground">
                      {edge.from}:{edge.line} → {edge.to}
                    </p>
                  ))}
                </li>
              ))}
              {truthSummary.purityFindings.map((finding, idx) => (
                <li
                  key={`${finding.code}:purity:${idx}`}
                  className="rounded-control border border-destructive/30 bg-fail-tint/40 px-2.5 py-1.5"
                >
                  <div className="flex items-center gap-1.5">
                    <span className="rounded-chip bg-fail-tint px-1.5 py-px font-medium text-destructive">
                      {finding.severity}
                    </span>
                    <span className="font-mono text-foreground">
                      {purityCodeLabel(finding.code)}
                    </span>
                  </div>
                  <p className="mt-1 text-muted-foreground">{finding.detail}</p>
                </li>
              ))}
            </ul>
          </InspectorSection>
        )}
    </div>
  );
}

function GroupDetailBody({
  node,
  mission,
  tree,
  fileIndex,
  relationIndex,
  onSelectNode,
}: {
  node: MindMapTreeNode;
  mission: FeatureMission;
  tree: FeatureMindMapTree;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  onSelectNode: (nodeId: string) => void;
}) {
  const sharedEntries = Object.entries(fileIndex.sharedFiles);
  return (
    <div className="flex flex-col gap-4">
      <InspectorSection title="Subtree state breakdown">
        <div className="grid grid-cols-2 gap-1.5">
          {STATES.map((state) => (
            <div
              key={state}
              className="flex items-center justify-between rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5 text-caption"
            >
              <span className="inline-flex items-center gap-1.5 text-muted-foreground">
                <span className={cx("size-2 rounded-full", STATE_INFO[state].dot)} />
                {STATE_INFO[state].label}
              </span>
              <span className="tnum font-medium text-foreground">{node.stateCounts[state]}</span>
            </div>
          ))}
        </div>
      </InspectorSection>

      {node.nodeKind === "mission" && (
        <InspectorSection title="Graph summary">
          <dl className="grid grid-cols-[1fr_auto] gap-y-1 text-caption">
            <dt className="text-muted-foreground">Capabilities</dt>
            <dd className="tnum font-medium text-foreground">{mission.features.length}</dd>
            <dt className="text-muted-foreground">Typed feature links</dt>
            <dd className="tnum font-medium text-foreground">{relationIndex.total}</dd>
            <dt className="text-muted-foreground">Indexed files</dt>
            <dd className="tnum font-medium text-foreground">{fileIndex.allFiles.length}</dd>
            <dt className="text-muted-foreground">Shared multi-feature files</dt>
            <dd className="tnum font-medium text-foreground">{sharedEntries.length}</dd>
          </dl>
        </InspectorSection>
      )}

      {node.nodeKind === "mission" && sharedEntries.length > 0 && (
        <InspectorSection title={`Shared files (${sharedEntries.length})`} icon={<FileCode size={12} />}>
          <ul className="flex flex-col gap-1.5">
            {sharedEntries.slice(0, 12).map(([filePath, owners]) => (
              <li
                key={filePath}
                className="rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5"
              >
                <p className="truncate font-mono text-micro text-foreground" title={filePath}>
                  {filePath}
                </p>
                <div className="mt-1 flex flex-wrap gap-1">
                  {owners.map((owner) => (
                    <button
                      key={owner.id}
                      type="button"
                      onClick={() => onSelectNode(owner.id)}
                      className={cx(
                        "inline-flex items-center gap-1 rounded-chip bg-surface px-1.5 py-px text-micro text-info shadow-hairline hover:bg-surface-hover",
                        FOCUS_RING,
                      )}
                    >
                      <span className={cx("size-1.5 rounded-full", STATE_INFO[owner.state].dot)} />
                      {owner.title}
                    </button>
                  ))}
                </div>
              </li>
            ))}
          </ul>
        </InspectorSection>
      )}

      <InspectorSection title={`Direct branches (${node.children.length})`}>
        <div className="flex flex-col gap-1">
          {node.children.map((childId) => {
            const child = tree.nodes[childId];
            if (!child) return null;
            return (
              <button
                key={childId}
                type="button"
                onClick={() => onSelectNode(childId)}
                className={cx(
                  "flex items-center justify-between gap-2 rounded-control border border-surface-border bg-page-canvas px-2.5 py-1.5 text-left text-caption hover:bg-surface-hover",
                  FOCUS_RING,
                )}
              >
                <span className="flex min-w-0 items-center gap-1.5">
                  {child.feature && (
                    <span
                      className={cx("size-2 shrink-0 rounded-full", STATE_INFO[child.feature.state].dot)}
                    />
                  )}
                  <span className="truncate text-foreground">{child.title}</span>
                </span>
                <span className="tnum shrink-0 text-micro text-muted-foreground">
                  {child.nodeKind === "feature"
                    ? STATE_INFO[child.feature!.state].label
                    : `${child.totalFeatures}`}
                </span>
              </button>
            );
          })}
        </div>
      </InspectorSection>
    </div>
  );
}

function InspectorSection({
  title,
  icon,
  children,
}: {
  title: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col gap-1.5">
      <h3 className="flex items-center gap-1.5 text-micro font-medium uppercase tracking-[0.06em] text-faint-foreground">
        {icon}
        <span>{title}</span>
      </h3>
      {children}
    </section>
  );
}

function OwnerRow({
  feature,
  onOpenSession,
}: {
  feature: Feature;
  onOpenSession: (sessionId: string) => void;
}) {
  const owner = feature.owner;
  const last = feature.last_owner;
  const session = owner?.session_id ?? last?.session_id ?? null;
  const label = ownerText(owner) ?? (last ? `last: ${ownerText(last) ?? ""}` : null);
  if (label === null) {
    return <p className="text-caption text-faint-foreground">No task assigned yet.</p>;
  }
  return (
    <div className="flex flex-wrap items-center gap-2 text-caption text-muted-foreground">
      <span className="min-w-0 truncate text-foreground">{label}</span>
      {session && (
        <button
          type="button"
          onClick={() => onOpenSession(session)}
          className={cx(
            "inline-flex shrink-0 items-center gap-0.5 rounded-chip bg-info-tint px-2 py-0.5 text-micro font-medium text-info hover:underline",
            FOCUS_RING,
          )}
        >
          Open session <ArrowUpRight size={11} />
        </button>
      )}
    </div>
  );
}

function SourcePill({ source }: { source: FeatureSource }) {
  const label = sourceLabel(source);
  const title = source.title ?? source.ref;
  if (isExternal(source) && source.url) {
    return (
      <a
        href={source.url}
        target="_blank"
        rel="noreferrer"
        title={title}
        className="inline-flex max-w-full items-center gap-1 truncate rounded-chip bg-surface-selected px-2 py-0.5 text-micro text-info hover:underline"
      >
        {label} <ExternalLink size={10} className="shrink-0" />
      </a>
    );
  }
  return (
    <span
      title={title}
      className={cx(
        "max-w-full truncate rounded-chip px-2 py-0.5 text-micro",
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

function newestLanding(feature: Feature): FeatureLanding | null {
  let newest: FeatureLanding | null = null;
  for (const landing of feature.evidence.landings) {
    if (newest === null || (landing.at_ms ?? 0) > (newest.at_ms ?? 0)) {
      newest = landing;
    }
  }
  return newest;
}

function landingText(feature: Feature): string {
  const landing = newestLanding(feature);
  if (landing === null) return "not landed";
  return landingLabel(landing, landing.at_ms ? relativeTime(landing.at_ms) : "");
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
