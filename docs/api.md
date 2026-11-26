# The HTTP API of `spawn serve`

Every route `spawn serve` answers under `/api/`, on one page. The router is
`handle()` and `reader()` in
[`crates/spawn-cli/src/serve.rs`](../crates/spawn-cli/src/serve.rs), which
dispatch on one table, `ROUTES`; what each answer holds is documented on its
handler there. Words are as in the [glossary](./glossary.md).

`main` serves 31 GET route patterns and 9 POST routes. (Before issue 98's
canonical names it was 28 GET patterns: a session's commands and
judgements were one pattern, `/api/fleet/<session>/<view>`, and are two
routes now.) A `<name>` in a path is one non-empty segment: `<id>` a
session id (under `/api/sessions/`) or a mission id (under
`/api/missions/`), `<n>` a task number, `<m>/<n>` a mission id and an
objective number. Any other path, under `/api/` or not, is looked up as a
file of the built UI (`--web`, `SPAWN_WEB_DIR`, default `web/dist`), so an
unknown API path is a 404. The PWA shell (issue 91 P1b) is answered before
any file: `/manifest.webmanifest` (`application/manifest+json`), `/sw.js`
(`application/javascript; charset=utf-8`) and `/icon.svg`
(`image/svg+xml`), each `no-cache`, embedded in the binary
([`serve/pwa.rs`](../crates/spawn-cli/src/serve/pwa.rs)); no path is ever
answered with `index.html`, so a missing file (`/missing.webmanifest`,
`/assets/sw.js`) is a 404. The service worker never answers or caches
`/api/*`, a request with an `Authorization` header or a URL with a
token. A service worker (so installing the PWA) and Web Locks need a
secure context, `https://` or `http://localhost` (`127.0.0.1`): on a
plain-http address such as `http://192.168.1.5:8787` the page registers
no worker, and each tab reads its own `/api/stream`, closed while the tab
is hidden, instead of one stream shared among tabs. The route tables below and the alias table are
checked against `ROUTES` and `ALIASES` by the unit test
`serve::tests::routes_match_docs_api_md`: a route added, renamed or removed
in one and not the other fails `cargo test`.

## Reads

GET or HEAD only (`/api/stream`: GET only): any other method on a read
answers 405. Every read runs as the read-only `spawn_reader` login. Each answer carries a weak `ETag`, and
`If-None-Match` gets 304 while nothing it shows changed; a board read with
`Cache-Control: max-age=1` may share the answer computed for another page up
to a second ago ([`serve/answers.rs`](../crates/spawn-cli/src/serve/answers.rs)).

| Method | Path | Answers | CLI twin | Read by |
| --- | --- | --- | --- | --- |
| GET | `/api/overview` | Every session's state and judgement, without the judgement's evidence | – | web (`api.ts`) |
| GET | `/api/fleet` | The fleet board: every session with its judgement, activity and label, plus the host's health, the decision ladder and gate-room waits | `spawn fleet`: the sessions alone, without the health | web (Fleet, Home) |
| GET | `/api/attention` | The Escalations queue, computed by the dispatcher's own code; the supervisor's question and "For the owner:" lines | – | web (Fleet, Home, sidebar) |
| GET | `/api/missions` | The mission board: missions, each task with its stages and last gate verdict, the owner's holds; texts as first lines | `spawn mission list` (the same board; the route adds labels) | web (Missions) |
| GET | `/api/events` | The whole log in log order, a page at a time: `?after=<position>` or `?since_ms=<ms>` (alias `?since=<ms>`), `?limit=` (200, at most 1000), and `next` | – | web (Show) |
| GET | `/api/sessions/<id>` | One session: state, executions (with `restart_of`), its events after `?after=`, `reassigned_to` and `reassigned_from` | `spawn tail --session <id>` (overlapping) | web (agent detail) |
| GET | `/api/sessions/<id>/commands` | One session's commands, each with `waiting` while the Bridge holds it for the turn; `?limit=` (50, at most 500) | `spawn tail --session <id>` (overlapping, not compared field by field) | web (agent detail) |
| GET | `/api/sessions/<id>/judgements` | One session's judgements, newest first; `?limit=` (50, at most 500) | `spawn tail --session <id>` (overlapping, not compared field by field) | web (agent detail) |
| GET | `/api/sessions/<id>/pane` | The tail of the session's live terminal, read from tmux | – | web (agent detail) |
| GET | `/api/tasks/<n>` | One task's brief, report and review note in full | – | web (Missions) |
| GET | `/api/tasks/<n>/gate` | One objective task in full: brief, report, latest gate log, every gate verdict with its log | `spawn task stages --id N` (its stages alone) | web (RSI) |
| GET | `/api/objectives` | Every open objective mission: its backlog's objectives, tasks, gate verdicts, metrics and resident | `spawn mission objectives` (the backlog alone) | web (RSI) |
| GET | `/api/objectives/<m>/<n>` | One objective's whole prompt | – | web (RSI) |
| GET | `/api/objectives/pane` | The live pane of an objective mission's resident; `?mission=` (the first open objective mission by default) | – | web (RSI) |
| GET | `/api/scorecard` | Every dimension of the fleet at once; `?mission=&since=&until=` | `spawn scorecard --json` with the same flags: the same JSON | web (Scorecard) |
| GET | `/api/scorecard/fleet` | How the fleet decides over a window; `?window=24h\|7d\|30d\|90d&until=<RFC 3339>` | – | web (Scorecard) |
| GET | `/api/interventions` | The north star (human interventions per agent-hour) and the interventions behind it; `?window=24h\|7d&until=` | – | web (Home, Fleet) |
| GET | `/api/alerts` | The alerts raised, notices folded; `?since=<span>` (alias `?window=<span>`) (the configured window when empty) | `spawn alerts --once --json` with the same flag: the same array | web (Home, sidebar) |
| GET | `/api/cost` | What the model calls cost per model, mission, task, agent and execution, and per landing; `?window=24h\|7d` | – | web (Cost) |
| GET | `/api/core-lines` | The core lines the owner steers by, each figure with its value, range and state | – | web (Home) |
| GET | `/api/verifications` | What is landed and verified or not | `spawn verify list` (the same, ages aside) | no web page; nightly scenario 133 |
| GET | `/api/nodes` | Every node joined and not removed, without `ssh`, `endpoint`, `note` or `pubkey` | `spawn node list --json` (which also prints those four) | web (Fleet, with each node's power and soft drain) |
| GET | `/api/grading` | The grading catalog: every preset with every `spawn grade list --json` field plus `routing_grade`, `profile` (its execution profile, else its model profile) and `reprobe_due`; `profiles`, every profile name, sorted | `spawn grade list --json` (the presets alone, without the three added fields) | no web page yet |
| GET | `/api/features` | The feature map of every open objective mission (a mission's `--features` file), or of `?mission=`; `?limit=` (10, at most 50) | – | web (Features) |
| GET | `/api/features/coverage` | The feature registry's file coverage (`docs/feature-map/registry.json`): wild files, stale references, features; `?mission=<id>` for that mission's project | `spawn features scan --json` | web (Coverage) |
| GET | `/api/features/coverage/projects` | Every project the Coverage page can read, over all open objective missions | – | web (Coverage) |
| GET | `/api/stream` | The log as Server-Sent Events, live: `event: ready`, then one frame per event (its `/api/events` row), a comment probe `:` every 500 ms of quiet (a page gone, a reset included, frees its slot within a few probes) and `: keepalive` in its place every 15 s; catch-up of at most 500 events after `Last-Event-ID` or `?after=<id>`, else `event: resync`; on 64 slots of its own, at most 16 per client address that is not loopback (503 when all are taken, or that address's are), no `ETag` ([`serve/stream.rs`](../crates/spawn-cli/src/serve/stream.rs)) | – | web (`stream.ts`: live invalidation of Home's boards, one stream shared among tabs) |
| GET | `/api/approvals` | The prompts a person must answer, read from the live panes (numbered choices, permission buttons, yes/no, confirm), sessions whose judgement asks for a person, and halted missions; each item's `id` is its `approval_id` ([`serve/approvals.rs`](../crates/spawn-cli/src/serve/approvals.rs)); a session's item also carries `auto: {state, answered, breaker}`, what the run-time resolver did and may do there ("Automatic answers" below) | – | web (Home, `approvals.ts`: the approvals queue) |
| GET | `/api/context/digest` | The fleet in one small answer: states, nodes with power and the low-battery soft drain, open missions with their last gate failure, pending approvals, the supervisor's notes for the owner, and a Markdown `digest_text` under 4000 bytes ([`serve/digest.rs`](../crates/spawn-cli/src/serve/digest.rs)) | – | web (Home, `digest.ts`: the digest line and drawer) |
| GET | `/api/prov/who` | Who made a file as the last recorded first-parent commit of `main` that touched it left it: that `land` or `import` Change, its entry, the `edit` revision the content came from (`origin`) and the path's history, oldest first; `?path=` (required), `?source=` (the domain `git:<source>`; every domain when empty); a `line` is 400 (a line's blame needs the repository) ([`serve/prov.rs`](../crates/spawn-cli/src/serve/prov.rs), [provenance](./provenance.md)) | `spawn prov who <path>[:<line>] --source <source>` (file-level from git's `main`, and a line's blame) | no web page yet |
| GET | `/api/prov/change/<id>` | Every revision of a Change (`<id>` its `change_id`), every Change of a task (digits) or the Change recorded for a commit (40 hex characters, read through the commit map both ways), each with its entries and edges; `{changes: []}` when none is recorded | `spawn prov show <id>`: the same JSON | no web page yet |

## Writes

POST only (405 otherwise). The session and mission writes run as the
`spawn_human` login, whose only rights are the `spawn_human_*` functions:
each records as principal `human` and nothing else. Their guards, in order:
the read guard below; POST; without the token, an `Origin` naming this
server as the `Host` does, and with no token set at all, a client socket the
owner's uid holds; `Content-Type: application/json` (415); a body of at most
16 KiB (413); then the route's own fields (400). On the input, the resume,
the master message and the act, `{"probe": true}` passes every guard and
writes nothing, so the page knows before it offers the box.

| Method | Path | Does | CLI twin | Read by |
| --- | --- | --- | --- | --- |
| POST | `/api/sessions/<id>/input` | A person's input to a session, `{"text", "key"}`, typed when the agent's turn ends; 202 with the command | `spawn submit --principal human --action '{"type":"input",...}'`: the same command | web (agent detail, the supervisor's box) |
| POST | `/api/sessions/<id>/key` | A named key (escape, up, down, left, right, enter, y, n, digit1 to digit9; a bare `1` to `9` is its digit), `{"name", "key"}`, sent at once | `spawn submit --principal human --action '{"type":"key",...}'` | web (terminal panel) |
| POST | `/api/sessions/<id>/restart` | Restarts the session's delivered task on a fresh execution, `{"reason"}` | `spawn task restart --id <task>` (by task, not by session) | web (agent detail) |
| POST | `/api/sessions/<id>/reassign` | Moves the session's delivered task to a fresh worker, `{"reason"}` | `spawn task reassign --id <task>` (by task) | web (agent detail) |
| POST | `/api/missions/<id>/resume` | Resumes the mission from the named halt, `{"halt", "key"}` | `spawn mission resume --id N` (which also ends a pause) | web (Home, Needs you) |
| POST | `/api/master/message` | A person's message to the supervisor, `{"text", "mode", "key"}` (`mode` auto, steer, queue or restart; auto by default), through `spawn_human_master_message`: a stopped or stopping supervisor is woken first, the message typed when its turn ends; 202 with the command ids; the same request again answers the first call's ids, another under the key is 409 | `spawn submit --principal human --session session_supervisor --action '{"type":"input",...}'` (no wake, no mode) | web (Home, `master.ts`: Master Control, with the offline outbox `outbox.ts`) |
| POST | `/api/approvals/<session_id>/act` | Answers one prompt of `/api/approvals`, `{"action", "approval_id", "choice_id", "keys", "text", "key"}`: `allow`, `allow_always` and `deny` need the item's `approval_id` (400 without) and press its keys only while the pane still shows that prompt (409 otherwise); `choice_id`, the option's `id`, presses that option, which must be of the action (409 otherwise), and without it an action several options share is 409 (ambiguous); `keys` presses named keys, with an `approval_id` only while that prompt shows (409 otherwise) and with a `choice_id` that option's keys; `text` types a text; 202 with the command ids | `spawn submit --principal human --action '{"type":"key",...}'`, one per key | web (Home, `approvals.ts`: the approvals queue's buttons) |
| POST | `/api/features/scan` | A fresh read-only scan of the feature registry's coverage; changes no file | `spawn features scan` | web (Coverage) |
| POST | `/api/features/associate` | Associates one file with features and saves the registry | `spawn features associate --path <file> --feature <id>` | web (Coverage) |

The two feature writes do not touch the database and do not use the
`spawn_human` login: `associate` changes the feature registry file
(`docs/feature-map/registry.json` of the project read), and both need the
token or this page's own `Origin`; `?mission=<id>` names the project.

## Aliases

Every route above has one canonical path. Issue 98 renamed 11 of them
to the nouns the UI and the CLI use (sessions, tasks, objectives); each old
spelling still answers, as an alias: 14 alias patterns (the table below;
the session writes are four rows of one rename). `handle()` rewrites a legacy path to
its canonical path before anything else (the read guard, the method, the
shared answer's key), so a legacy request gets exactly the canonical
answer, status and body, from the same cached answer, plus the header
`Link: <canonical path>; rel="successor-version"`. The header is left
out, the answer otherwise the same, when the path holds a byte that is not
graphic ASCII (a space, CR, LF or other control byte) or one of `<`, `>`
and `"`: a request line cannot write a header of its own. The server logs
the first request of each legacy spelling, once per process, as its two
patterns (legacy and canonical), never the request's own bytes. A legacy path
that fits no row below (an empty or extra segment) is an unknown path: 404.
The web UI reads only canonical paths.

| Legacy path | Canonical path |
| --- | --- |
| `/api/session/<id>` | `/api/sessions/<id>` |
| `/api/fleet/<id>/commands` | `/api/sessions/<id>/commands` |
| `/api/fleet/<id>/judgements` | `/api/sessions/<id>/judgements` |
| `/api/pane/<id>` | `/api/sessions/<id>/pane` |
| `/api/session/<id>/input` | `/api/sessions/<id>/input` |
| `/api/session/<id>/key` | `/api/sessions/<id>/key` |
| `/api/session/<id>/restart` | `/api/sessions/<id>/restart` |
| `/api/session/<id>/reassign` | `/api/sessions/<id>/reassign` |
| `/api/mission/<id>/resume` | `/api/missions/<id>/resume` |
| `/api/missions/task/<n>` | `/api/tasks/<n>` |
| `/api/rsi/task/<n>` | `/api/tasks/<n>/gate` |
| `/api/rsi` | `/api/objectives` |
| `/api/rsi/objective/<m>/<n>` | `/api/objectives/<m>/<n>` |
| `/api/rsi/pane` | `/api/objectives/pane` |

`/api/missions/task/resume` is the legacy task path, answered as
`/api/tasks/resume` (a malformed task number), as it always was, not a
resume of mission `task` (a mission id is a number).

Three query parameters have a second spelling. Giving both spellings at
once is 400.

| Parameter | Alias | On |
| --- | --- | --- |
| `since=<span>` | `window=<span>` | `/api/alerts` |
| `since_ms=<ms>` | `since=<ms>` | `/api/events` |
| `after=<id>` | `after_event_id=<id>` | `/api/stream` (the cursor `/api/events` and `/api/sessions/<id>` take) |

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

## Automatic answers (`spawn-resolve`)

A dialog an engine shows can be answered without a person by
[`crates/spawn-cli/src/resolve.rs`](../crates/spawn-cli/src/resolve.rs), a
step of `spawn loop` (Herdr PR 3, design section 6.3). Every answer is an
ordinary `key` command of the principal `spawn-resolve`, pressed by the
Bridge as a person's is. For each answer the record shows `command.accepted`
(principal `spawn-resolve`, idempotency key
`resolve:<execution>:<dialog id>:1:1`, the dialog's approvals id), then
`command.step pressing`, `pressed`, `command.finished`, then the next
`screen.observed`.

The Bridge records `screen.observed permission` on its own pass, normally
before the answer. The resolver does not wait for that record: it reads the
pane twice itself.

The Bridge's own answer to an engine's startup trust dialog takes the same
path, keyed `resolve:<execution>:trust:1:<k>`. Its keys are recorded
`pressing` and `pressed` while the start waits, finished once the start is
done, and never pressed twice.

- **Switch:** `SPAWN_AUTO_APPROVE = off | startup | all`, default `startup`.
  - `all`: the resolver reads panes and submits.
  - `startup`: only the Bridge's startup trust answer, as before.
  - `off`: no automatic answer at all, the startup trust dialog included. A
    start that shows it waits, until its readiness limit, for someone to
    answer it in the engine's terminal itself (a key sent through Spawn
    queues behind the start); if nobody does, the start fails naming the
    dialog and the switch.

  An unknown value is warned about and read as `startup`.
- **Table** (`spawn_core::driver::table_answer`): only answers someone has
  seen work, each one key. Today that is one row: agy's `trust_prompt` for
  the execution's own workspace, cursor on trust → Enter (probed).
  Everything else escalates, and nothing is pressed for it until a later
  change adds the captured screen as a test:
  - Claude Code's numbered permission, whose only screens are hand-written
    approvals fixtures;
  - Claude Code's trust dialog, which the Bridge answers at startup;
  - agy's permission and question prompts, Claude Code's question forms,
    Codex, OpenCode, and any dialog no row covers.
- **Hard blocks** (`spawn_core::driver::hard_block`): a login required or
  expired, a quota or rate limit, an update the engine needs, or exit-only
  choices. Each is escalated at once and never pressed. Their texts come from
  the engines' strings and docs; none is captured yet.
- **Guards:** every one must hold before a key is submitted.
  - Two pane reads in a row show the same dialog id.
  - On Herdr, for an engine other than Claude Code, the agent reads
    `blocked` by a named rule, with the blocker visible.
  - The driver's own reading of both panes shows the dialog: `Permission`,
    or, for `trust_prompt`, its startup trust dialog.
  - The row's own guard holds.
  - No command of the session is unfinished: a person's answer in flight
    holds the resolver off.
  - The breaker is closed.
- **The Bridge checks again before it presses** a key of `spawn-resolve`:
  - the driver still reads the dialog on the pane;
  - no other command of the session was recorded `pressed` or `typed` since
    this key was accepted.

  Otherwise the command ends `Failed` ("the dialog this answer was for is
  gone: nothing pressed"), with no `pressing` step.
- **Breaker:** folded from the log, so a restart keeps it. The counts come
  from the commands of `spawn-resolve`; the per-dialog count comes from
  `submit`'s idempotency.
  - A dialog gets at most one automatic answer per execution. If it still
    shows 5 s after its answer, it is escalated once and never pressed
    again.
  - An execution with 5 answers within 10 minutes gets no more.
  - The host gets no more after 20 answers across the fleet within a minute.

  Open means no more keys there and one line in the loop's log. The item
  stays in `/api/approvals` for a person.
- **`auto` on an approvals item:** `state` is the switch (`off`, `startup`,
  `all`). `answered` is the resolver's answers in the session within 10
  minutes. `breaker` is `open` or `closed`, or `off` unless the switch is
  `all`.
