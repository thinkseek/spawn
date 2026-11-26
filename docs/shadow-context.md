# Shadow observer: context contract (phase 1)

What the passive shadow observer (`scripts/shadow/shadow_observe.py`) watches, how it reads it, what a reading does and does not show, and what it cannot see. It is the source of truth for the observer's coverage. Facts marked **read** were read on the host on 2026-11-15; **inferred** and **unknown** are said as such. The observer has no action path; this file gives it none.

## Components

| Component | Owner | Depends on | Read path (interface) | A reading shows | Health is | Freshness | Blind spots |
|---|---|---|---|---|---|---|---|
| `spawn-loop.service` (Bridge, Observer, dispatcher, supervisor and resident liveness, per `deploy/systemd/README.md`) | systemd user manager | record, relays | `systemctl --user show -p ActiveState,SubState,NRestarts,ActiveEnterTimestampMonotonic` | the process exists; restart count | observed: process up or down. Not: that it dispatches | one sample (30 s) | a hung loop; what its own liveness checks decide (not read) |
| `spawn-serve.service` (page and API, port 8787) | same | record | same, plus `GET /` on 127.0.0.1:8787, status only | process up; the read path answers | observed for the read path only | 30 s | write path; correctness of what the page shows |
| `spawn-integrate.service` (gates and landings) | same | mission store, record | same | process up | observed: process only | 30 s | a gate that hangs; a wrong verdict |
| `spawn-jev-relay.service`, `spawn-model-relay.service` | same | providers, credentials | same | process up | observed: process only | 30 s | provider state, quota, auth: **unknown** in this phase (no free read path reviewed) |
| Mission store main (`missions/spawn.git`) | integrator | disk | `git log -1 --format=%ct refs/heads/main` | the committer time of the commit now at main's tip | observed: "main moved". **Not** evidence that a gate checked that commit: the observer reads no gate verdict and links no gate log to a commit | 30 s | which objective; whether a gate ran on it; whether it was right; a commit made earlier and landed later shows an older age than the landing |
| Gate logs (`ws/gate-logs`) | integrator | disk | newest entry's mtime | a gate wrote something | activity only | 30 s | verdicts (content is not read) |
| Queue (`/var/tmp/spawn/sync/queue.md`) | lead and reviewers | disk | mtime, size | someone posted | activity only | 30 s | who, what, whether answered |
| Loop log (`loop.log`) | loop | disk | mtime, size | the loop wrote | activity only | 30 s | content |
| tmux sessions `supervisor`, `cc-astra` | the lead; no unit | tmux server, provider | `tmux list-panes -s -t <session>`: dead flag, last activity time | a pane exists and when it last changed | activity only | 30 s | pane text is never read: refusals, prompts waiting, quota notices are invisible |
| Keep test delivery (`/run/keep-test/VERSION`) | Keep, on the Mac | owner login on the Mac | existence, mtime, size | the test folder was delivered | observed | 30 s | whether its content is current; production delivery does not exist yet |
| Host | — | Mac, OrbStack | boot id, monotonic clock, loadavg, MemAvailable, statvfs | resources; reboot; a gap in sampling | observed | 30 s | the Mac itself; power; the VM's disk durability |

> Note: The `tmux list-panes` row above reflects `scripts/shadow/shadow_observe.py`'s current phase-1 read-only probe of the human lead's external operator sessions (`supervisor`, `cc-astra`); Spawn's own agent runtime runs exclusively on Herdr (`herdr-core` / `spawn-hc`), not `tmux`.

## Not observed in this phase (explicit unknowns)
- **The record (Postgres).** No credential-safe, read-only read path has been reviewed for the observer, so it does not connect. Task state, claims, leases, attempts, alerts, pause and halt events: **unknown** to the observer. No claim about schema or fencing is made from names.
- **Desired state** (owner pause, stop, tombstone): the observer cannot read it yet. It honours only its own exclusion file.
- **Provider availability, quota and auth.** No paid probe; none free is known.
- **The Mac**: launch agents, OrbStack, Keep's store, the owner's login. A Mac-side observer is the next unit of this phase.
- **Fleet workers and residents individually.**
- **Backups and drills** (their timers and results).
- Nothing outside the lists above is ever enumerated: the observer walks no process list and no session list.

## Reading the samples
- `process: up` means a process exists. It never means healthy.
- `movement.label` is **inferred** from two ages with candidate thresholds: `main_moved_recent` (main's tip commit is under 2 h old), `activity_only` (not that, and something wrote within 15 min), `quiet`, `unknown`. None of these says progress, validation or usefulness. A main change with no gate, an unrelated gate run, or old evidence cannot become "validated" here: no sample holds such a word. `activity_only` is a baseline pattern of normal work, not a suspicion rate, until a trigger rule and independent labels exist.
- `discontinuity: true` (first sample of a boot, or a gap over 90 s) means earlier ages must not be carried across it.
- A probe result of `timeout`, `error`, `malformed`, `skipped_deadline` or `excluded` leaves that component `unknown`.

## Off, exclusions, removal
- Off: `touch ~/.config/spawn-shadow/disabled`. The observer then exits before any probe and writes nothing. Or disable the timer.
- Exclusions: `~/.config/spawn-shadow/exclude`, one component name per line. An excluded component is not probed. An unreadable file excludes everything.
- Turning the observer off, or removing it, changes no other unit, clears no exclusion, and leaves every pause, stop and tombstone as it was.
- Its only files: `~/.local/state/spawn-shadow/` (state, lock, `samples-YYYYMMDD.jsonl`). The folder must be a real folder of the user's own; a symbolic link is refused and nothing is written.
- At the 50 MB cap it rotates: its own oldest sample files are removed first, one day at a time. If only today's file is left and it is at the cap, that file is kept and nothing more is written until the day changes (the run ends with `cap_reached_no_write`; the gap in the samples is the record of it).
- Below 1 GB free disk it writes nothing.
