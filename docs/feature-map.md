# Feature map

The feature map answers one question per capability: **how far has this
actually got?** It separates four kinds of evidence — planned, implemented,
verified and live — and never treats one as another. A merged pull request, an
accepted task and a review are none of them "live".

It is a read model over records Spawn already keeps. Nothing on the page is
hand-maintained: the only durable artifact is an optional mapping of
capability identities to provenance sources, and it holds no status.

## 1. Where it lives

| Object | Where | Purpose |
| --- | --- | --- |
| API | `crates/spawn-cli/src/serve/feature_map.rs`, `GET /api/features` | Derives the map from the records below |
| UI | `web/src/components/features/FeatureMapView.tsx`, `#/features` | Four columns, evidence and gaps |
| Read model | `web/src/features.ts`, `web/tests/features.test.ts` | Normalization and words only; never derives a state |
| Store reads | `crates/spawn-postgres/src/features.rs` | `deploy_facts`, `mission_blockers` |
| Mission setting | `objectives.features` (`spawn mission configure --features <file>`) | The absolute path of a mapping file |
| Mapping file | any JSON file the owner keeps in the project tree | Capability ids, titles and provenance sources |

The map reuses the reads the RSI page and `spawn verify` already use: the
mission's backlog and tasks (`objective_missions`), the landings and
verifications (`verification_facts`, `objective.verified`), and adds only the
deploy records and the mission blockers. It is computed when the page reads;
there is no stored state, no materialized view and no second tracker.

## 2. What each state means

A capability's state is the furthest stage for which Spawn has a record. The
four stages are nested as a ladder (live > verified > implemented > planned),
but the four booleans in the answer are independent: a capability can be live
and never verified, and the page says so.

| State | Evidence required | Not enough |
| --- | --- | --- |
| **planned** | A capability exists in the mapping, or a backlog objective exists, and nothing landed | a task assigned, delivered, reported, accepted or reviewed; a merged PR or issue |
| **implemented** | A `task.landed` record for a named objective, or an `objective.solved` (its scenario passes at the base) | an accepted task; a branch; a report |
| **verified** | For **every landed** objective: the newest `objective.verified` after that objective's newest landing is `pass` | a verification recorded before the landing; a verification of another objective |
| **live** | The mission's source is shipped by Spawn's own deploy (a source ending `/missions/spawn.git`) **and** the newest successful `deploy.finished` was recorded after the capability's newest landing | a landing; a failed deploy; a deploy recorded before the landing; a mission no deploy ships |

Two consequences worth stating:

- **A mission no deploy ships is never called live.** Supply PA and Flake
  Doctor land on their own sources; Spawn has no record that the running
  product has the change. The capability stops at verified (or implemented)
  and carries the gap `live_not_observable`. The scorecard's report-to-live
  metric counts such a landing as live at landing (objective 159); that is a
  latency convention for a metric, not evidence that a running system has the
  capability, so the feature map does not borrow it.
- **A capability with several objectives takes the lowest state among
  them.** If one named objective is unverified, a live sibling does not make
  the capability verified. The gaps name the objective that is behind.

## 3. The mapping (identity only)

A mission names its mapping with an absolute file path:

```bash
spawn mission configure --id 4 --features /path/to/features.json
```

`spawn mission add` takes the same `--features` beside `--backlog` and
`--resident`. The path is checked when it is configured: it must be a file and
parse as a map. `--no-features` clears it. The setting is a normal mission
setting: it is recorded in the control stream as `mission.configured`.

The file holds identities and links, never a status:

```json
{"features": [
  {"id": "feature-map", "title": "Feature map",
   "summary": "planned, implemented, verified and live capabilities",
   "sources": [
     {"kind": "objective", "objective": 488},
     {"kind": "issue", "ref": "63", "title": "a feature map",
      "url": "https://github.com/thinkseek/spawn/issues/63"},
     {"kind": "pr", "ref": "69",
      "url": "https://github.com/thinkseek/spawn/pull/69"},
     {"kind": "request", "ref": "owner-2026-11-21", "title": "make it first-class"}
   ]}
]}
```

### 3.1 Sources, native and external

A source has a `kind`, a stable reference and optional `url`/`title`. The
reference is canonical per kind, so the same record named twice is one source:

| `kind` | Fields | Canonical reference | Linked to |
| --- | --- | --- | --- |
| `objective` | `objective` (from 1), optional `mission` | `objective:4/488` | the backlog row; **this decides the state** |
| `verification` | `objective`, optional `mission` | `verification:4/488` | the `objective.verified` records |
| `mission` | `mission` | `mission:7` | the mission (an owner request) |
| `task` | `task` | `task:544` | the task, its agent and session |
| `deploy` | `event` | `deploy:1234` | the `deploy.finished` record |
| anything else | `ref`, optional `url`, `title` | `<kind>:<ref>` | an external link only |

An objective may belong to one capability in a mission: two capabilities
claiming it would count the same records twice, and that is refused. Two
sources with one reference in one capability are merged. External sources may
repeat (a shared issue, a person's request), and they never affect the state.

### 3.2 What the page adds

Whether or not the file names them, the map adds the native sources that
exist: the capability's objectives, each task a person assigned
(`created_by: human`), each decisive verification, and the deploy that made the
capability live. The mission itself is the owner's request at the top of its
section. This is why a fallback map still has provenance: every backlog
objective is its own capability with its `objective:<mission>/<NNN>` source.

Without a mapping, each backlog objective is a capability of its own. A
mapping that cannot be read falls back to the objectives and names the error
on the page; the map stays useful and the gap is visible.

## 4. Gaps, unowned and blocked

The map never shows a percentage. It names what is missing:

| Gap | Meaning |
| --- | --- |
| `not_landed` | the capability names an objective with no landing |
| `unverified` | landed, and no pass recorded after the newest landing |
| `verification_failed` | the newest verification after the landing is a fail |
| `not_deployed` | landed after the newest successful deploy |
| `no_deploy_record` | the mission is shipped by a deploy and none has been recorded |
| `live_not_observable` | no deploy ships this mission's source |
| `no_task` | planned, and no task has ever been assigned |
| `accepted_without_landing` | an accepted task of a mission with no gate: it never lands |
| `objective_missing` | the mapping names an objective the backlog does not hold |
| `no_objective` | the mapping names no objective at all |

**Unowned** means no task is in flight and the capability is not live: nobody
is working on it. The newest in-flight task is the owner; otherwise the page
shows who last held it. **Blocked** comes from records: a halted or paused
mission, a red main (`mission_source_reds`), the newest task cancelled with no
newer task in flight, or an accepted task on a mission with no gate. The
blocker names its record, its time and its task where there is one.

## 5. The API

`GET /api/features[?mission=<id>][&limit=<missions>]` (at most 50 missions,
default 10) answers:

```text
{
  "generated_at_ms": 1795...,
  "deploy": {"event": 17, "at_ms": ..., "ok": false, "head": "eee...", "by": "owner", "reason": "..."} | null,
  "live_anchor": {"event": 8, "at_ms": ..., "ok": true, "head": "bbb...", "by": "landing", "reason": ""} | null,
  "missions": [{
    "id": 4, "text": "...", "state": "active", "halted": null, "baseline": "...",
    "sources": ["/.../missions/spawn.git"], "gate": "/...", "accept": "gate",
    "resident": {...} | null, "backlog": "/...", "source": {...},
    "mapping": {"kind": "file" | "objectives", "path": "...", "error": null},
    "blocker": {"kind": "mission_paused", "reason": "...", "event": 7, "at_ms": ...} | null,
    "shipped_by_deploy": true,
    "counts": {"features": 4, "planned": 1, "implemented": 1, "verified": 1, "live": 1,
               "blocked": 0, "unowned": 3, "complete": 1},
    "features": [{
      "id": "objective-002", "title": "...", "summary": null,
      "state": "verified", "planned": false, "implemented": true,
      "verified": true, "live": false, "complete": false,
      "objectives": [2], "objective_rows": [{...}],
      "sources": [{"kind": "objective", "ref": "objective:4/002", "native": true, "linked": true,
                   "url": null, "title": "..."}],
      "owner": {...,"task": 2, "agent_id": "...", "session_id": "..."} | null,
      "last_owner": {...} | null, "unowned": true,
      "blocked": null,
      "gaps": [{"kind": "not_deployed", "objectives": [2], "detail": "..."}],
      "evidence": {
        "tasks_total": 1,
        "tasks": [{"id": 2, "state": "landed", "review": {...}, "gates": [...]}],
        "landings": [{"objective": 2, "task": 2, "kind": "landed", "at_ms": ...,
                      "event": ..., "commit": "...", "base": "..."}],
        "verifications": [{"objective": 2, "result": "pass", "by": "lead", "how": "...",
                           "at_ms": ..., "event": ...}],
        "deploy": null, "last_deploy": {...} | null, "newest_landing_at_ms": ...
      }
    }],
    "unmapped": [{"objective": 5, "title": "...", "status": "open"}]
  }]
}
```

The four booleans (`implemented`, `verified`, `live`, and `planned` as their
negation) are separate evidence. `state` is the furthest one; it does not
rewrite the others.

## 6. The page

`#/features` shows one mission at a time (a mission selector when more than
one), four columns, and a card per capability:

- title, objective rows, and provenance sources (native references, external
  links; an unlinked native source says "missing");
- owner or last owner, with a link to the task's session timeline;
- the newest landing (task, commit, time), the verification (result, who,
  time), the deploy (head, time, or the failure), and the newest task's
  review (accepted, sent back, or the gate's last verdict);
- gap chips and the blocker, if any;
- an "unowned" badge when no task is in flight and the capability is not live.

The view only lays out the server's answer. It does not recompute a state, so
the API and the page can never disagree about what is implemented, verified or
live.

## 7. Tests

| What | Where |
| --- | --- |
| The four states never collapse; an accepted task is not implemented; a pass is not live; a live capability may be unverified; a deploy before the landing is not live | `crates/spawn-cli/src/serve/feature_map.rs` tests |
| A mission no deploy ships is not live (the gap says so) | same |
| Blocked and unowned come from the records; a revision is in flight, a cancellation is blocked | same |
| The mapping is checked (unknown fields, duplicate ids/objectives, external URLs), and native provenance is added and deduplicated | same |
| Deploys newest first and bounded; halt/pause/red main blockers; `objectives.features` is an absolute path that clears | `crates/spawn-postgres/tests/features.rs` |
| Normalization, four columns, live-without-verified, gaps and source labels | `web/test/features.test.ts` |
| The web build serves the view | `rsi/scenarios/check-web.sh` (as for every view) |
