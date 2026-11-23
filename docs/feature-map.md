# Feature map

The feature map answers one question per capability: **how far has this
actually got?** It separates four kinds of evidence — planned, implemented,
verified and live — and never treats one as another. A merged pull request, an
accepted task and a review are none of them "live".

It is a read model over records Spawn already keeps. Nothing on the page is
hand-maintained: the only durable artifact is an optional mapping of
capability identities to provenance sources, and it holds no status.

A separate hand-maintained capability audit with fixed identifiers FM-01
through FM-19, code navigation and defect evidence lives in
[feature-map-static.md](./feature-map-static.md). It covers a different need
and is validated by `scripts/check-feature-map.py`; this derived map has no
checked-in status for that script to inspect.

## 1. Where it lives

| Object | Where | Purpose |
| --- | --- | --- |
| API | `crates/spawn-cli/src/serve/feature_map.rs`, `GET /api/features` | Derives the map from the records below |
| UI | `web/src/components/features/FeatureMapView.tsx`, `#/features` | Four columns, evidence and gaps |
| Read model | `web/src/features.ts`, `web/test/features.test.ts` | Normalization and words only; never derives a state |
| Store reads | `crates/spawn-postgres/src/features.rs` | `deploy_facts`, `live_deploy_fact`, `mission_blockers` |
| Mission setting | `objectives.features` (`spawn mission configure --features <file>`) | The absolute path of a mapping file |
| Mapping file | any JSON file the owner keeps in the project tree | Capability ids, titles and provenance sources |

The map reuses the reads the RSI page and `spawn verify` already use: the
mission's backlog and tasks (`objective_missions`), the landings and
verifications (`verification_facts`, `objective.verified`), and adds only the
deploy records and the mission blockers. It is computed when the page reads;
there is no stored state, no materialized view and no second tracker.

## 2. What each state means

A capability's state is the furthest stage **every objective it names**
supports. With one objective that is the furthest stage Spawn has a record
for; with several, the least advanced objective sets the state, so a live
sibling never lifts an objective that never landed. The four stages are
nested as a ladder (live > verified > implemented > planned), but the four
booleans in the answer are independent: a capability can be live and never
verified, and the page says so.

| State | Evidence required | Not enough |
| --- | --- | --- |
| **planned** | A capability exists in the mapping, or a backlog objective exists, and nothing landed | a task assigned, delivered, reported, accepted or reviewed; a merged PR or issue |
| **implemented** | For **every** named objective: a `task.landed` record, or an `objective.solved` (its scenario passes at the base) | one named objective without a landing (the capability stays planned); an accepted task; a branch; a report |
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
  them.** Every named objective must have landed before the capability is
  implemented; a never-landed objective holds it at planned even beside a
  live sibling. If one landed objective is unverified (or failed), a live
  sibling does not make the capability verified. The gaps name the objective
  that is behind.

## 3. The mapping (identity only)

A mission names its mapping with an absolute file path:

```bash
spawn mission configure --id 4 --features /path/to/features.json
```

`spawn mission add` takes the same `--features` beside `--backlog`,
`--resident` and `--gate`; without `--gate` it refuses them (objectives could
never land), rather than accepting the flags and dropping them. The path is
checked when it is configured: it must be a file and parse as a map.
`--no-features` clears it. The setting is a normal mission setting: it is
recorded in the control stream as `mission.configured`.

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

### 3.2 The curated registry as the mapping

The project's own feature registry, `docs/feature-map/registry.json`
(`"schema_version": 1`, the file `spawn-core`'s feature coverage reads), is
also a mapping, so it can be configured directly:

```bash
spawn mission configure --id 4 --features "$PWD/docs/feature-map/registry.json"
```

The registry's top level is `schema_version`, `initiative_id`, `boundary` and
`features`; a node takes `id`, `title`, `summary`, `kind`, `parent`, `tags`,
`files`, `relations`, `provenance` and `status_note` (anything else is refused,
as in a plain map; `sources` is the plain map's field, not the registry's).
`parent`, `kind`, `files` and `relations` draw the Mind Map's hierarchy. Each
`provenance` list becomes sources:

| `provenance` list | Entry | Source |
| --- | --- | --- |
| `objectives` | `"4/001"` (this mission) | native `objective:4/001`: **decides the state** |
| `objectives` | anything else (`"t559u"`, `"5/003"`) | `external-objective:<entry>` link |
| `tasks` | a whole number | native `task:<n>` |
| `tasks` | a name (`"slice-a"`) | `external-task:<entry>` link |
| `deploys` | a whole number | native `deploy:<event>` |
| `deploys` | a name | `external-deploy:<entry>` link |
| `agents`, `commits`, `tests`, `artifacts`, `discussions` | any | `agent:`, `commit:`, `test:`, `artifact:`, `discussion:` links |

Unlike a plain map, a registry may name one objective on a parent and on its
children: the hierarchy shows the same work at two depths, it does not claim
it twice. Within one node an entry named twice is one source. A plain map
(no `schema_version`) keeps its rules: one objective, one capability.

### 3.3 What the page adds

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
    "resident": {...} | null, "backlog": "/...",
    "backlog_error": "read /...: ..." | null, "source": {...},
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
negation) are separate evidence. `state` is the furthest one every named
objective supports; it does not rewrite the others. `deploy` is the newest
deploy of any outcome (read within a bounded window for display);
`live_anchor` is the newest successful deploy over the whole log, so a run of
failures never hides the one that shipped the source. `backlog_error` names a
backlog that could not be read — a directory that was moved or deleted is an
error, not an empty backlog: the mission has no backlog objectives then, a
readable mapping may still supply its capabilities, and the page says why
rather than rendering four empty columns.

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

A warning banner names a mapping file or a backlog that could not be read: the
page never shows empty columns without saying why. A backlog banner says the
mission's objectives are unknown and, when a readable mapping still supplies
the cards, that they come from the mapping; it does not claim the map is
empty. The `deployed` row is the evidence's anchor and only that: a Planned
card says nothing has landed, and an unshipped mission says live is not
observable, instead of printing the newest Spawn deploy of another source.

The view only lays out the server's answer. It does not recompute a state, so
the API and the page can never disagree about what is implemented, verified or
live.

## 7. Tests

| What | Where |
| --- | --- |
| The four states never collapse; an accepted task is not implemented; a pass is not live; a live capability may be unverified; a deploy before the landing is not live | `crates/spawn-cli/src/serve/feature_map.rs` tests |
| A capability is only as far as its least advanced objective (a never-landed one holds it at planned); a pre-landing pass verifies nothing; the newest verification decides | same |
| The live anchor is the newest success in the whole log, through the route's own store read (one success, then sixty failures); a backlog that cannot be read is named in the answer | same |
| A mission no deploy ships is not live (the gap says so) and its evidence has no deploy; only `/missions/spawn.git` counts as shipped; a capability with no objective is planned | same |
| The boundaries are strict: a deploy or a verification at exactly the landing's millisecond is not after it; the later event decides within one millisecond | same |
| The same objective named twice in one capability is one source, not a conflict | same |
| A backlog directory that is gone is an error, not an empty backlog (`load_backlog`, `with_objectives`) | `crates/spawn-cli/src/objectives.rs`, `.../serve.rs` tests |
| `mission add --features` accepts a mapping that names its own (not yet known) mission; refuses objectives without `--gate`; stores `--features` through a real store | `crates/spawn-cli/src/missions.rs` tests |
| `/api/features` is a route and its clock stays out of the ETag | `crates/spawn-cli/src/serve.rs`, `.../serve/answers.rs` tests |
| Blocked and unowned come from the records; a revision is in flight, a cancellation is blocked | same |
| The mapping is checked (unknown fields, duplicate ids/objectives, external URLs), and native provenance is added and deduplicated | same |
| Deploys newest first and bounded; the live anchor found outside the newest 50; halt/pause/red main blockers; `objectives.features` is an absolute path that clears | `crates/spawn-postgres/tests/features.rs` |
| Normalization, four columns, live-without-verified, an Implemented card never says "verified by", a Planned card says another objective has not landed, the deploy row shows only the evidence's anchor, the backlog banner's wording, gaps, source labels and a backlog error | `web/test/features.test.ts` |
| The web build serves the view | `rsi/scenarios/check-web.sh` (as for every view) |
