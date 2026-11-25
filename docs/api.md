# The HTTP API of `spawn serve`

Every route `spawn serve` answers under `/api/`, on one page. The router is
`handle()` and `reader()` in
[`crates/spawn-cli/src/serve.rs`](../crates/spawn-cli/src/serve.rs); what
each answer holds is documented on its handler there. Words are as in the
[glossary](./glossary.md).

`main` serves 24 GET route patterns and 7 POST routes. Any other path,
under `/api/` or not, is looked up as a file of the built UI (`--web`,
`SPAWN_WEB_DIR`, default `web/dist`), so an unknown API path is a 404.

## Reads

GET or HEAD only: any other method on a read answers 405. Every read runs as
the read-only `spawn_reader` login. Each answer carries a weak `ETag`, and
`If-None-Match` gets 304 while nothing it shows changed; a board read with
`Cache-Control: max-age=1` may share the answer computed for another page up
to a second ago ([`serve/answers.rs`](../crates/spawn-cli/src/serve/answers.rs)).

| Method | Path | Answers | CLI twin | Read by |
| --- | --- | --- | --- | --- |
| GET | `/api/overview` | Every session's state and judgement, without the judgement's evidence | – | web (`api.ts`) |
| GET | `/api/fleet` | The fleet board: every session with its judgement, activity and label, plus the host's health, the decision ladder and gate-room waits | `spawn fleet`: the sessions alone, without the health | web (Fleet, Home) |
| GET | `/api/fleet/<session>/judgements`, `/api/fleet/<session>/commands` | One session's judgements, or its commands (each with `waiting` while the Bridge holds it for the turn); `?limit=` (50, at most 500) | `spawn tail --session <id>` (overlapping, not compared field by field) | web (agent detail) |
| GET | `/api/attention` | The Escalations queue, computed by the dispatcher's own code; the supervisor's question and "For the owner:" lines | – | web (Fleet, Home, sidebar) |
| GET | `/api/missions` | The mission board: missions, each task with its stages and last gate verdict, the owner's holds; texts as first lines | `spawn mission list` (the same board; the route adds labels) | web (Missions) |
| GET | `/api/missions/task/<id>` | One task's brief, report and review note in full | – | web (Missions) |
| GET | `/api/events` | The whole log in log order, a page at a time: `?after=<position>` or `?since_ms=<ms>`, `?limit=` (200, at most 1000), and `next` | – | web (Show) |
| GET | `/api/session/<id>` | One session: state, executions (with `restart_of`), its events after `?after=`, `reassigned_to` and `reassigned_from` | `spawn tail --session <id>` (overlapping) | web (agent detail) |
| GET | `/api/pane/<id>` | The tail of the session's live terminal, read from tmux | – | web (agent detail) |
| GET | `/api/rsi` | Every open objective mission: its backlog's objectives, tasks, gate verdicts, metrics and resident | `spawn mission objectives` (the backlog alone) | web (RSI) |
| GET | `/api/rsi/task/<id>` | One objective task in full: brief, report, latest gate log, every gate verdict with its log | `spawn task stages --id N` (its stages alone) | web (RSI) |
| GET | `/api/rsi/objective/<mission>/<id>` | One objective's whole prompt | – | web (RSI) |
| GET | `/api/rsi/pane` | The live pane of an objective mission's resident; `?mission=` (the first open objective mission by default) | – | web (RSI) |
| GET | `/api/scorecard` | Every dimension of the fleet at once; `?mission=&since=&until=` | `spawn scorecard --json` with the same flags: the same JSON | web (Scorecard) |
| GET | `/api/scorecard/fleet` | How the fleet decides over a window; `?window=24h\|7d\|30d\|90d&until=<RFC 3339>` | – | web (Scorecard) |
| GET | `/api/interventions` | The north star (human interventions per agent-hour) and the interventions behind it; `?window=24h\|7d&until=` | – | web (Home, Fleet) |
| GET | `/api/alerts` | The alerts raised, notices folded; `?since=<span>` (the configured window when empty) | `spawn alerts --once --json` with the same flag: the same array | web (Home, sidebar) |
| GET | `/api/cost` | What the model calls cost per model, mission, task, agent and execution, and per landing; `?window=24h\|7d` | – | web (Cost) |
| GET | `/api/core-lines` | The core lines the owner steers by, each figure with its value, range and state | – | web (Home) |
| GET | `/api/verifications` | What is landed and verified or not | `spawn verify list` (the same, ages aside) | no web page; nightly scenario 133 |
| GET | `/api/nodes` | Every node joined and not removed, without `ssh`, `endpoint`, `note` or `pubkey` | `spawn node list --json` (which also prints those four) | web (Fleet) |
| GET | `/api/features` | The feature map of every open objective mission (a mission's `--features` file), or of `?mission=`; `?limit=` (10, at most 50) | – | web (Features) |
| GET | `/api/features/coverage` | The feature registry's file coverage (`docs/feature-map/registry.json`): wild files, stale references, features; `?mission=<id>` for that mission's project | `spawn features scan --json` | web (Coverage) |
| GET | `/api/features/coverage/projects` | Every project the Coverage page can read, over all open objective missions | – | web (Coverage) |

## Writes

POST only (405 otherwise). The session and mission writes run as the
`spawn_human` login, whose only rights are the `spawn_human_*` functions:
each records as principal `human` and nothing else. Their guards, in order:
the read guard below; POST; without the token, an `Origin` naming this
server as the `Host` does, and with no token set at all, a client socket the
owner's uid holds; `Content-Type: application/json` (415); a body of at most
16 KiB (413); then the route's own fields (400). On the input and the
resume, `{"probe": true}` passes every guard and writes nothing, so the page
knows before it offers the box.

| Method | Path | Does | CLI twin | Read by |
| --- | --- | --- | --- | --- |
| POST | `/api/session/<id>/input` | A person's input to a session, `{"text", "key"}`, typed when the agent's turn ends; 202 with the command | `spawn submit --principal human --action '{"type":"input",...}'`: the same command | web (agent detail, the supervisor's box) |
| POST | `/api/session/<id>/key` | A named key (escape, up, down, left, right, enter, y, n), `{"name", "key"}`, sent at once | `spawn submit --principal human --action '{"type":"key",...}'` | web (terminal panel) |
| POST | `/api/session/<id>/restart` | Restarts the session's delivered task on a fresh execution, `{"reason"}` | `spawn task restart --id <task>` (by task, not by session) | web (agent detail) |
| POST | `/api/session/<id>/reassign` | Moves the session's delivered task to a fresh worker, `{"reason"}` | `spawn task reassign --id <task>` (by task) | web (agent detail) |
| POST | `/api/mission/<id>/resume` | Resumes the mission from the named halt, `{"halt", "key"}` | `spawn mission resume --id N` (which also ends a pause) | web (Home, Needs you) |
| POST | `/api/features/scan` | A fresh read-only scan of the feature registry's coverage; changes no file | `spawn features scan` | web (Coverage) |
| POST | `/api/features/associate` | Associates one file with features and saves the registry | `spawn features associate --path <file> --feature <id>` | web (Coverage) |

The two feature writes do not touch the database and do not use the
`spawn_human` login: `associate` changes the feature registry file
(`docs/feature-map/registry.json` of the project read), and both need the
token or this page's own `Origin`; `?mission=<id>` names the project.

## Aliases

None on `main`: every path above is the only spelling of its route. When a
route gains a second spelling, both are listed here, the old one with the
canonical path it answers as.

## Access

`serve` binds `127.0.0.1:8787` (`--address`, `SPAWN_SERVE_ADDRESS`); on any
other address it refuses to start without `SPAWN_SERVE_TOKEN`. With a token
set, every API request carries it (`Authorization: Bearer <token>` or
`?token=<token>`), except from a loopback client running as the owner's uid
when `SPAWN_SERVE_LOCAL_OPEN=1`, and from a client inside
`SPAWN_SERVE_OPEN_NETWORKS` that names the server by an IP address inside
it. A read served without the token also needs a `Host` naming this server
and no cross-origin `Origin`. A refused request is 401 with a bearer
challenge when a token is set, 403 otherwise. The built UI's static files
are served without the token. The full rules, and the reasons for each, are
the opening comment of
[`crates/spawn-cli/src/serve.rs`](../crates/spawn-cli/src/serve.rs).
