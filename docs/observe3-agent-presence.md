# spawn-agent-presence v1: the fleet's agents for observe3

`scripts/observe3/agent_presence.py` writes one sanitized document: which Spawn fleet agents are present and what Spawn's own records say they are doing. It feeds the read-only observe3 viewer. The viewer gets no control path.

## Source and output
- **Reads**, only from the local page service at 127.0.0.1:8787. The host is fixed, no redirect is followed, and each read has a 5 s socket timeout:
  - `GET /api/fleet` on every run, capped at 1 MiB.
  - Only for a live agent with an open turn (at most 8 per run): `GET /api/session/<session>?limit=1` for the session's last position, then `?after=<last-64>&limit=64` for its newest events. Each answer is capped at 256 KiB. A session id is put in the path only if it matches `[A-Za-z0-9_.:-]{1,128}` and is not dots only, and it is percent-encoded.
  - No database, environment file or credential is read.
- **Writes**, in `~/.local/state/spawn-agent-presence/`, a 0700 folder owned by the user, all files 0600:
  - `presence.json`: the document, at most 16384 bytes. It is written atomically (temp file, then rename), at most once every 10 s; the timer runs every 15 s.
  - `ids.json`: the exporter's own map from a digest of each agent id to its opaque number, plus the identity epoch. Raw ids are never stored.
  - `issued.json`: the high-water mark, the epoch and the next number ever issued in it. It is kept apart from the map and written before it. The write order is mark, then map, then document, each file synced and the folder synced after each rename. A test injects a crash at each write and checks that no id is ever reused (rev-e R5 F1, n1).
  - `lock`: an empty lock file.
  - **Only `presence.json` may ever leave the host.** The map, the mark and the lock never do.
- **Off switch:** `touch ~/.config/spawn-agent-presence/disabled`, or `systemctl --user disable --now spawn-agent-presence.timer`.
- **No port, no process, no write to anything it reads.** It shares nothing with the passive observer or the KPI viz exporter.
- **Run result words** (printed to the journal when not `ok`): `unavailable`, `stale`, `source_too_large` (the fleet answer passed 1 MiB: about 930 agents at today's size; the read counts as failed, so the document is `stale` with all agents `unknown` while the last good read is at most 300 s old, then `unavailable`), `busy`, `too_soon`, `disabled`, `disk_low_no_write`, `out_dir_refused`, `no_out_dir`, `invalid_projection`, `write_error`, `disk_full`.

## Document
```json
{"version":"1","generated_utc":"2026-11-16T07:11:16Z","source_last_ok_utc":"2026-11-16T07:11:16Z",
 "source_status":"ok","stale_after_s":60,"id_epoch":"9f2c4e1a7b3d5e60",
 "provenance":{"adapter":"spawn-agent-presence","version":"1"},
 "agents":[{"id":"a1","role":"supervisor","state":"idle","evidence":"observed"}]}
```
| Field | Rule |
|---|---|
| `version` | `"1"` |
| `generated_utc` | the start of this sample, taken after the fleet read and before the session reads (UTC, `Z`, whole seconds). It is conservative: the file is written a moment later |
| `source_last_ok_utc` | the last successful read of the source, or `null` if there has never been one. It is carried, never refreshed, when a read fails |
| `source_status` | `ok`: read now. `stale`: the read failed, and the last good read is at most 300 s old; agents are listed, all `unknown`. `unavailable`: no usable read; `agents` is empty |
| `stale_after_s` | 60. A consumer treats the whole document as stale once `now - generated_utc` exceeds this |
| `id_epoch` | 16 hex characters, random, or `null` when `agents` is empty. **Identity is the pair (`id_epoch`, `id`).** A new epoch starts when the id map is missing or damaged; when the high-water mark (`issued.json`) is missing, damaged, names another epoch or is ahead of the map; or when the map is behind the exporter's own last document. So a map restored from an older copy is caught even after the higher ids have left the document (cc-astra A90, rev-e r3n1). The first run of this revision on a host that ran R4 starts one new epoch, because R4 wrote no mark (rev-e R5 n2). **Recovery rule:** never restore `ids.json` or `issued.json` from a backup. Delete them instead, and the exporter starts a new epoch. If both were rolled back together to the same older copy, nothing would show it, so the rule is what protects identity in that case, not detection A consumer must treat a new epoch as a new set of agents |
| `provenance` | fixed `{adapter: "spawn-agent-presence", version: "1"}` |
| `agents[]` | at most 64: live agents, then agents whose execution ended within 24 h (newest first) |
| `agents[].id` | `a<n>`: opaque, never reused within an epoch. It is not derived reversibly from any Spawn id |
| `agents[].role` | `supervisor`, `worker`, `rsi`, or `other` |
| `agents[].state` | `idle`, `working`, `offline`, `unknown` (rules below) |
| `agents[].evidence` | always `observed`: the state comes from recorded events |
| per-agent times | none. The exporter uses recorded times to decide the state, but writes none of them: to the second, they would show when people typed to an agent, and observe3 v1 has no field for them (rev-e n1) |

## State rules (Spawn's records only)
| State | When |
|---|---|
| `offline` | the agent's execution has ended (`live: false` in `/api/fleet`) |
| `working` | `live: true`; the execution's latest turn record is a start (`message.received`, not followed by `turn.stopped`/`turn.failed`; `/api/fleet`'s `turn_open`); and among the session's newest 64 events, one written by **the agent's own engine** (event `source` equal to the agent's `engine`) **in its current execution** is at most 900 s old. If a tool call is open, up to 3600 s |
| `idle` | `live: true` and no open turn |
| `unknown` | an open turn whose engine has been quiet longer than those limits; no own-engine event among the newest 64; more than 8 open turns in one run; a missing or malformed field; or the source failed |

Records written by the observer, the bridge or the gateway never make an agent `working`. They can be written while the agent itself is silent. `/api/fleet`'s `last_event_at_ms` counts them, so it is not used (cc-astra A89).

Never used: the judge model's state (WAITING / WORKING / BLOCKED ...), which is inferred; the existence of a tmux session or process; screen text; task text.

**Known limit: `idle` means "the last recorded state", not real-time liveness.** If Spawn stops following an agent's transcript (incident I-006, 2026-11-15), an agent whose last record was the end of a turn still shows `idle`. A fresh read proves that the page service answered, not that every follower is current. Objective 205 makes such a failure visible to Spawn; mapping it to `unknown` is a later change. **The viewer must show this caveat too**, not only this document.

## Never in the document
Agent, session and execution ids; per-agent times; labels; task text; mission names; screen text; tool names and summaries; event names and details; judge states and reasons; engine names; paths; prompts; transcripts; argv; environment; credentials; addresses. The tests plant a marker value in every source field, the session timelines included, and check that it never reaches either file.

## What publishing it discloses (cc-astra's public-data review, R2)
This is pseudonymous activity telemetry, not anonymous data. Stable ids with the supervisor and rsi roles, sampled every 15 s, let a public observer rebuild each agent's transitions, the number of agents, their uptime and outages, and activity schedules to about the sampling interval. A single supervisor can be correlated with the owner's own activity, although "working" does not prove that a person typed. No direct personal identifier is in the document. Publishing at this granularity is the owner's decision. Aggregating, delaying or coarsening before publication are the alternatives.

## Mapping to observe3 canonical envelope v1 (for the receive adapter)
The viewer-side adapter (not Spawn code) reads the delivered file and builds one complete snapshot per sample. It drops the whole document if it fails these rules.
| This document | observe3 v1 |
|---|---|
| the source | entity `source`, type `Source`, `state.sourceKind: "live"`, `evidence: "observed"`. `state.freshness`: `live` when `source_status` is `ok` and the file is within `stale_after_s`; `stale` when `source_status` is `stale` or the file is older; `unavailable` when `source_status` is `unavailable` |
| provenance | envelope `provenance: {adapter: "spawn-agent-presence", version: "1"}` |
| `id_epoch` change | a relay epoch reset (`reset = True`): a new snapshot, and no identity carried over |
| each agent | entity id `agent.<id_epoch>.<id>` (for example `agent.9f2c4e1a7b3d5e60.a3`), type `Agent`, `kind: <role>`, `label: "<role> <id>"`, `evidence: "observed"` |
| `working` | `state: {activity: "running", freshness: "live"}` |
| `idle` | `state: {activity: "idle", freshness: "live"}`. The inspector says "last recorded state" |
| `offline` | `state: {health: "down", freshness: "live"}`, no `activity`. A dedicated presence word would read better; v1 has none, so this is the adapter author's call |
| `unknown` | `state: {freshness: "unavailable"}`, no `activity` |
| file older than `stale_after_s` | every agent `freshness: "stale"`, no `activity` |
| relations, flows, causes | none. This source records no actions and no causality |

Freshness is per document (`generated_utc`, `source_last_ok_utc`, `source_status`). A per-agent age, if wanted later, is a new contract version and a new public-data review.

## Delivery off the host
Not part of this script, and not approved. The proposed transport is a separate owner decision: only `presence.json` is pushed to the viewer host with a restricted, push-only key, and a receiver there checks the size and these rules before an atomic write.
