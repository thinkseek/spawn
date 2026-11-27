# The HTTP API of `spawn serve`

Every route `spawn serve` answers under `/api/`, on one page. The router is
`handle()` and `reader()` in
[`crates/spawn-cli/src/serve.rs`](../crates/spawn-cli/src/serve.rs), which
dispatch on one table, `ROUTES`; what each answer holds is documented on its
handler there. Words are as in the [glossary](./glossary.md).

`main` serves 33 GET route patterns and 9 POST routes. (Before issue 98's
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
| GET | `/api/missions` | The mission board: missions, each task with its stages, last gate verdict, native grading/preset fields (`difficulty`, `routed_difficulty`, `preset_id`, `first_preset_id`, `tier_role`, `quality_rejections`, `engine_label`, `model_label`, `execution_profile`, `rework_step_up`), and the owner's holds; texts as first lines | `spawn mission list [--limit N] [--json \| --table]` (same board; route adds labels); `spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]` (episode replay & parity) | web (Missions) |
| GET | `/api/events` | The whole log in log order, a page at a time: `?after=<position>` or `?since_ms=<ms>` (alias `?since=<ms>`), `?limit=` (200, at most 1000), and `next` | – | web (Show) |
| GET | `/api/sessions/<id>` | One session: state, executions (with `restart_of`), its events after `?after=`, `reassigned_to` and `reassigned_from` | `spawn tail --session <id>` (overlapping) | web (agent detail) |
| GET | `/api/sessions/<id>/commands` | One session's commands, each with `waiting` while the Bridge holds it for the turn; `?limit=` (50, at most 500) | `spawn tail --session <id>` (overlapping, not compared field by field) | web (agent detail) |
| GET | `/api/sessions/<id>/judgements` | One session's judgements, newest first; `?limit=` (50, at most 500) | `spawn tail --session <id>` (overlapping, not compared field by field) | web (agent detail) |
| GET | `/api/sessions/<id>/pane` | The tail of the session's live terminal, read from its terminal host; as it is only when the session's newest start is positively Claude Code's, else without its account lines (a failed lookup masks too) | – | web (agent detail) |
| GET | `/api/tasks/<n>` | One task's brief, report and review note in full | `spawn task commits <id>`, `spawn task diff <id>`, `spawn task land <id>` (bare-repo review and merge counterparts) | web (Missions) |
| GET | `/api/tasks/<n>/gate` | One objective task in full: brief, report, latest gate log, every gate verdict with its log | `spawn task stages --id N` (its stages alone) | web (RSI) |
| GET | `/api/objectives` | Every open objective mission: its backlog's objectives, tasks, gate verdicts, metrics and resident | `spawn mission objectives` (the backlog alone) | web (RSI) |
| GET | `/api/objectives/<m>/<n>` | One objective's whole prompt | – | web (RSI) |
| GET | `/api/objectives/pane` | The live pane of an objective mission's resident; `?mission=` (the first open objective mission by default) | – | web (RSI) |
| GET | `/api/scorecard` | Every dimension of the fleet at once; `?mission=&since=&until=` | `spawn scorecard --json` with the same flags: the same JSON | web (Scorecard) |
| GET | `/api/scorecard/fleet` | How the fleet decides over a window; `?window=24h\|7d\|30d\|90d&until=<RFC 3339>` | – | web (Scorecard) |
| GET | `/api/interventions` | The north star (human interventions per agent-hour) and the interventions behind it; `?window=24h\|7d&until=` | – | web (Home, Fleet) |
| GET | `/api/alerts` | The alerts raised, notices folded (including `source_main_advanced` and `source_main_synced` repo-main tracking notices); `?since=<span>` (alias `?window=<span>`) (the configured window when empty) | `spawn alerts --once --json` with the same flag: the same array | web (Home, sidebar) |
| GET | `/api/cost` | What the model calls cost per model, mission, task, agent and execution, and per landing; `?window=24h\|7d` | – | web (Cost) |
| GET | `/api/core-lines` | The core lines the owner steers by, each figure with its value, range and state | – | web (Home) |
| GET | `/api/verifications` | What is landed and verified or not | `spawn verify list` (the same, ages aside) | no web page; nightly scenario 133 |
| GET | `/api/nodes` | Every node joined and not removed, without `ssh`, `endpoint`, `note` or `pubkey` | `spawn node list --json` (which also prints those four) | web (Fleet, with each node's power and soft drain) |
| GET | `/api/grading` | The grading catalog: every preset with every `spawn grade list --json` field plus `routing_grade`, `profile` (its execution profile, else its model profile), `driver` and `label_only` (the engine its worker runs: its `engine_label` when that names a registered driver, else `claude` with `label_only: true`), `tier_role` and `reprobe_due`; `profiles`, every profile name, sorted; and `calibration_window` (PV5 28-day readiness: `samples_collected`, `window_days_elapsed`, `min_window_days`, `min_samples`, `ready_to_calibrate`, `window_start_ms`, `tier_samples`, `per_preset`) | `spawn grade status --json` (exact JSON) and `spawn grade list --json` (presets array alone) | web (Grading, `#/grading`) |
| GET | `/api/capabilities` | What this host can run: `probed_at_ms`; `engines`, each registered engine with `program`, `enabled` (`SPAWN_ENGINES`), `installed` and `version` (`<program> --version`; both null for an engine `SPAWN_ENGINES` does not name, whose program is never run), `version_tested` (agy: its `major.minor` is one Spawn was checked against), `login` (its login files, each `{path, present}` from `stat`, never opened), `known_models` (agy, enabled: the labels `agy models` lists), `model` (`SPAWN_AGY_MODEL`) and `unavailable` (why it cannot start here; never set for Claude Code); `terminal`, single Herdr backend (`backend: "herdr"`, embedded `herdr-core` via sibling `spawn-hc` by default with `engine: "herdr-core"`, `program`, `session`, `version`, `expected_generation`, `witness: "host"`, optional `shards`, `server`/`server_error`; or legacy `herdr 0.9.5` under `SPAWN_HERDR_MODE=legacy` with `expected_protocol`, `pinned_manifests`, `server`/`server_error`; or `{backend: null, error}` when refused; `tmux` has been removed). The probe is bounded (its output too), runs no shell and is cached for an hour ([`serve/capabilities.rs`](../crates/spawn-cli/src/serve/capabilities.rs)) | – | no web page yet |
| GET | `/api/features` | The feature map of every open objective mission (a mission's `--features` file), or of `?mission=`; `?limit=` (10, at most 50) | – | web (Features) |
| GET | `/api/features/coverage` | The feature registry's file coverage (`docs/feature-map/registry.json`): wild files, stale references, features; `?mission=<id>` for that mission's project | `spawn features scan --json` | web (Coverage) |
| GET | `/api/features/coverage/projects` | Every project the Coverage page can read, over all open objective missions | – | web (Coverage) |
| GET | `/api/stream` | The log as Server-Sent Events, live: `event: ready`, then one frame per event (its `/api/events` row), a comment probe `:` every 500 ms of quiet (a page gone, a reset included, frees its slot within a few probes) and `: keepalive` in its place every 15 s; catch-up of at most 500 events after `Last-Event-ID` or `?after=<id>`, else `event: resync`; on 64 slots of its own, at most 16 per client address that is not loopback (503 when all are taken, or that address's are), no `ETag` ([`serve/stream.rs`](../crates/spawn-cli/src/serve/stream.rs)) | – | web (`stream.ts`: live invalidation of Home's boards, one stream shared among tabs) |
| GET | `/api/approvals` | The prompts a person must answer, read from the live panes (numbered choices, permission buttons, yes/no, confirm), sessions whose judgement asks for a person, and halted missions; each item's `id` is its `approval_id` ([`serve/approvals.rs`](../crates/spawn-cli/src/serve/approvals.rs)); a session's item also carries `auto: {state, answered, breaker}`, what the run-time resolver did and may do there ("Automatic answers" below) | – | web (Home, `approvals.ts`: the approvals queue) |
| GET | `/api/context/digest` | The fleet in one small answer: states, nodes with power and the low-battery soft drain, open missions with their last gate failure, pending approvals, the supervisor's notes for the owner, and a Markdown `digest_text` under 4000 bytes ([`serve/digest.rs`](../crates/spawn-cli/src/serve/digest.rs)) | – | web (Home, `digest.ts`: the digest line and drawer) |
| GET | `/api/prov/who` | Who made a file as the last recorded first-parent commit of `main` that touched it left it: that `land` or `import` Change, its entry, the `edit` revision the content came from (`origin`) and the path's history, oldest first; `?path=` (required), `?source=` (the domain `git:<source>`; every domain when empty); a `line` is 400 (a line's blame needs the repository) ([`serve/prov.rs`](../crates/spawn-cli/src/serve/prov.rs), [provenance](./provenance.md)) | `spawn prov who <path>[:<line>] --source <source>` (file-level from git's `main`, and a line's blame) | no web page yet |
| GET | `/api/prov/change/<id>` | Every revision of a Change (`<id>` its `change_id`), every Change of a task (digits) or the Change recorded for a commit (40 hex characters, read through the commit map both ways), each with its entries and edges; `{changes: []}` when none is recorded | `spawn prov show <id>`: the same JSON | no web page yet |
| GET | `/api/prov/blast` | The blast radius of a Change (`?target=` its `change_id`), of a task (digits) or of the Change recorded for a commit (40 hex characters): the Changes it names, every later Change with an entry on one of their paths (each with those paths), the landings that include them (`landed_by`), every Change that depends on them by an edge or an entry's `origin` (`radius`, nearest first, with one shortest path) and the part reached by `used` edges alone (`used`); from the database alone, on the reader login; 400 without a valid `target` ([`serve/prov.rs`](../crates/spawn-cli/src/serve/prov.rs), [provenance](./provenance.md)) | `spawn prov blast <target>`: the same JSON | no web page yet |

### Out-of-table read-only endpoints

These four contract-locked read-only endpoints are dispatched directly before `ROUTES` (so `ROUTES` stays 33 GET + 9 POST patterns; all four execute on the `reads` pool as `spawn_reader`, return `405` with `Allow: GET` on non-`GET`/`HEAD`, and keep weak `ETag` / `304` validation):

- **`GET|HEAD /api/episodes[?mission=<id>]`**: Replays `session_control` into typed `TaskEpisode`s and reports parity against SQL projections: `{mission, episodes, summary, parity_ok, warnings, drift_count}` (weak `ETag` / `304`, no wall clock in body). CLI twin: `spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]` (`--verify-parity` exits `0` on match, `1` on drift). Read by web (`#/grading` episode replay panel).
- **`GET|HEAD /api/features/truth[?project=<name>|?mission=<id>]`**: Evaluates real code-graph dependency and purity truth against `docs/feature-map/registry.json` (bounded LRU cache, weak `ETag`): `{root, registry_path, git_head, today, projection, counts, enforced_violations, findings, purity}`. CLI twin: `spawn features check --deps report|enforce --purity report|enforce`. Read by web (`#/features`).
- **`GET|HEAD /api/storage/tree[?project=<name>&limit=<N>]`**: Reads the D6 CAS snapshot DAG (`spawn_snapshots`), live refs (`spawn_refs`, closed `kind`: `main | task | worker | wip | tag`), and bounded append-only transition ledger (`spawn_ref_log`, closed `action`: `created | merged | fast-forwarded | rolled-back | retired | moved`) on the `spawn_reader` login: `{projects, active_project, summary, nodes, refs, history}` (`limit` default `120`, max `500`; `400` on invalid `limit`/`project`; closed `summary.backend`: `d6 | cow | overlay | git`; closed node `state`: `live | collected`; merge status derived client/consumer-side from `parent_ids.len >= 2`; weak `ETag` / `304`, no wall clock in body). Gracefully returns `200` with empty `nodes`/`refs`/`history` and `summary.backend` + `summary.mode_note` when no D6 data is recorded, in `git` mode, or when `SPAWN_DATABASE_URL` is unset. CLI twin: `spawn storage cutover --status --json`. Read by web (`#/storage`).
- **`GET|HEAD /api/storage/snapshot/<id>`**: Reads one D6 snapshot by `sha256:<64 hex>` snapshot ID or `40 hex` projected `git_oid`: `{snapshot, refs, ancestors, children, history, recovery_commands}` (BFS ancestors up to depth `32`, scoped `spawn_ref_log` transitions, and POSIX shell-escaped read-only / branch / rollback CLI preview commands; `400` on malformed id, `404` when unknown or when `SPAWN_DATABASE_URL` is unset, weak `ETag` / `304`). Read by web (`#/storage` Time-Machine drawer).

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
| POST | `/api/sessions/<id>/input` | A person's input to a session, `{"text", "key"}`, typed when the agent's turn ends; 202 with the command | `spawn say <agent> <message> [...]` or `spawn submit --principal human --action '{"type":"input",...}'`: the same command | web (agent detail, the supervisor's box) |
| POST | `/api/sessions/<id>/key` | A named key (escape, up, down, left, right, enter, y, n, digit1 to digit9; a bare `1` to `9` is its digit), `{"name", "key"}`, sent at once | `spawn submit --principal human --action '{"type":"key",...}'` | web (terminal panel) |
| POST | `/api/sessions/<id>/restart` | Restarts the session's delivered task on a fresh execution, `{"reason", "key"}` (with `Idempotency-Key` header; 202 replay for same key/reason, 409 conflict) | `spawn task restart --id <task>` (by task, not by session) | web (agent detail) |
| POST | `/api/sessions/<id>/reassign` | Moves the session's delivered task to a fresh worker, `{"reason", "key"}` (with `Idempotency-Key` header; 202 replay for same key/reason, 409 conflict) | `spawn task reassign --id <task>` (by task) | web (agent detail) |
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

## Operator task review and merge CLI (PR #172)

Three operator-facing subcommands under `spawn task` in
[`crates/spawn-cli/src/missions.rs`](../crates/spawn-cli/src/missions.rs) inspect
and land worker-authored commits preserved in the mission's bare repository,
while keeping all existing `spawn task` verbs (`assign`, `accept`, `reject`,
`cancel`, `release`, `restart`, `reassign`, `steal`, `stages`) backwards
compatible and never printing credentials:

- **`spawn task commits <id> [--json]`:** resolves the task's preserved ref in
  precedence order — `refs/spawn/tasks/<id>/head` (`spawn_host::git::task_head_ref`),
  `refs/spawn/workers/<agent>/head` (`spawn_host::git::worker_head_ref`), then
  `refs/heads/spawn/<agent>` — and lists `refs/heads/main..<resolved_ref>`,
  tagging each entry as either `author` or synthetic `checkpoint` (`spawn
  checkpoint`). Prints a compact table by default; with `--json` outputs a
  `schema_version: 1` payload containing `task_id`, `mission_id`, `agent_id`,
  `resolved_ref`, `base_ref`, `base_sha`, `head_sha`, `commit_count`,
  `author_commit_count`, `checkpoint_commit_count`, and `commits`. Returns an
  actionable error when the task, mission repository, `refs/heads/main`, or
  preserved ref is missing.
- **`spawn task diff <id> [--stat]`:** resolves the task's preserved ref via the
  same lookup order and runs `git diff main...<resolved_ref>` (or `--stat`)
  inside the mission bare repository through `controller_git()`
  (`spawn_host::git_command`).
- **`spawn task land <id> [--ff-only | --squash] [--dry-run] [--json]`:**
  safely lands clean task work onto `refs/heads/main` in the mission's local
  bare repository:
  - Defaults to `--ff-only`; passing `--ff-only` and `--squash` together is
    refused.
  - `--dry-run` validates ref existence, fast-forward ancestry (`--ff-only`) or
    conflict-free mergeability (`--squash`), and prints the `schema_version: 1`
    report (`task_id`, `mission_id`, `resolved_ref`, `base_ref`, `base_sha`,
    `head_sha`, `new_main_sha`, `mode`, `dry_run`, `landed`, `can_land`, and
    commit counts) without mutating `refs/heads/main`.
  - Non-dry-run `--ff-only` verifies `refs/heads/main` is an ancestor of
    `<resolved_ref>` and atomically advances `refs/heads/main` via
    `git update-ref refs/heads/main <head_sha> <base_sha>`.
  - Non-dry-run `--squash` stages the merge against `refs/heads/main` in an
    isolated temporary worktree backed by `objects/info/alternates`, refuses
    conflicts without mutating the bare repo, builds a single-parent squash commit
    whose message omits synthetic `spawn checkpoint` subjects (carrying
    `Spawn-Task` and `Spawn-Head` trailers), fetches it, and atomically updates
    `refs/heads/main`.

## External `refs/heads/main` advancement detection (PR #171)

Local directory mission sources are polled for out-of-band pushes to
`refs/heads/main` by `digest::sync_external_main` (invoked in
[`dispatch::step`](../crates/spawn-cli/src/dispatch.rs) and
[`Integrator::pass_on`](../crates/spawn-cli/src/integrate.rs)), and surfaced via
supervisor digests and `/api/alerts` notices (`notice: true`):

- **Poll cadence (`SPAWN_EXTERNAL_MAIN_POLL_SECS`):** positive whole seconds
  `1..=3600`, default `15` (unset, invalid, or out-of-range values fall back to
  `15` s). Bounds `digest::external_main_poll_ms()` wakeups in `dispatch::step`
  when local sources are watched and no database event moved the log watermark.
- **Notice kinds recorded in `spawn_alerts`:**
  - `source_main_synced`: durable non-escalating baseline / checkpoint recorded
    on initial observation and after `spawn integrate` landings or Step 0 owner
    pushes.
  - `source_main_advanced`: durable notice recorded when `refs/heads/main` moves
    outside Spawn's own landing path (with idempotency key
    `source_main_advanced:{source_hash:016x}:{anchor_event}:{range_hash:016x}`),
    carrying `from`, `to`, `update_kind` (`fast_forward`, `non_fast_forward`, or
    `rewound`), `old_reachable`, `ahead`, `behind`, up to 5 commit `subjects` (120
    chars each), and affected `missions`.
- **Own-landing exclusions & digest delivery:** ignores `task.landed`,
  accepted gated `source.moved`, Step 0 `source.moved` requests, pending
  `owner_requests`, and in-flight `accepted`/`landing` candidate heads. Active
  `source_main_advanced` notices feed `Why::MainAdvanced` in supervisor digests,
  deduplicated per notice via `missions main-advanced-<id>` tags.

## Fleet storage breakdown & shared build caches (`GET /api/fleet` `health.storage`, PR #180 / PR #184)

`GET /api/fleet` includes `health.storage` computed incrementally inside the
600 s `READ_EVERY_MS` cache window ([`crates/spawn-cli/src/storage.rs`](../crates/spawn-cli/src/storage.rs),
[`crates/spawn-cli/src/serve/storage.rs`](../crates/spawn-cli/src/serve/storage.rs); see also
[storage.md](./storage.md)):

- **`workspaces` (`{path, bytes}` or `null`):** Regular-file apparent bytes
  under `SPAWN_WORKSPACE_ROOT`, excluding `.transcripts`,
  `.transcripts-progress`, the opt-in shared build cache directory `.caches`,
  `.spawn-workspace-backend`, and `test-schemas-seen.json` (plus `.partial`).
- **`workspace_breakdown` (or `null` when no workspace root is configured):**
  Read-only, symlink-safe per-workspace classification across all non-`.`
  directories under the workspace root, bounded globally by
  `WORKSPACE_BREAKDOWN_MAX_ENTRIES = 200_000` directory entries:
  - Shape:
    `{workspace_count, total_bytes, repo_bytes, git_bytes, build_cache_bytes, files, truncated, workspaces}`
    where each entry in `workspaces` has
    `{name, total_bytes, repo_bytes, git_bytes, build_cache_bytes, files, truncated}`.
  - Classification rules: `.git` directories and regular files (such as
    worktree `.git` pointer files) map to `git_bytes`; directories named
    `target`, `.zig-cache`, `zig-out`, `node_modules`, `.venv`, `__pycache__`,
    `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.cache`, `dist`, or `build`,
    or containing a regular `CACHEDIR.TAG` file matching the standard
    `Signature: 8a477f597d28d172789f06886806bc55` header (opened
    `O_NOFOLLOW | O_NONBLOCK`, never following symlinks or FIFOs), map to
    `build_cache_bytes`; remaining regular files map to `repo_bytes`.
  - Ordering and cap: `workspaces` is sorted by `total_bytes` descending (ties
    broken ascending by `name`) and truncated to top `32`
    (`WORKSPACE_BREAKDOWN_TOP_LIMIT`), while top-level totals still cover all
    `workspace_count` workspaces (setting `truncated: true` if the global entry
    bound was hit).
- **`shared_build_cache` (or `null`):** Reports `{path, bytes, files, enabled}`
  when shared host build caches are configured (`enabled: true`), or when a
  real non-symlink `<workspace_root>/.caches` directory remains on disk
  (`enabled: false`). Apparent regular-file sizes are summed without following
  symlinks.

## Operator CLI, preflight diagnostics & terminal backend

- **`spawn status [--json]`** (PR #167): Single-command overview of active fleet sessions, open tasks, and uncleared alerts. Prints a human-readable table by default, or `{schema_version: 1, generated_at_ms, summary: {sessions_total, sessions_live, sessions_attention, open_tasks, open_alerts}, sessions, tasks, alerts}` with `--json`.
- **`spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--principal <name>] [--json]`** (PR #167): Shorthand for `{"type":"input","text":"<message>"}` to `<agent>` (default `--principal`: `human`). Omitting `--session` auto-resolves the agent's single live session (refuses if 0 or >1 live sessions exist); omitting `--key` auto-generates `say-<ms>-<pid>-<seq>-<hex>`. With `--json`, prints `{schema_version: 1, command_id, agent_id, session_id, idempotency_key, principal, action: "input"}`.
- **`spawn submit` smart defaults** (PR #167): `spawn submit --agent <agent id> [--session <session id>] [--key <idempotency key>] --action <command action JSON> [--principal <name>]` makes `--session` and `--key` optional (auto-resolving a single live session and auto-generating a unique key while keeping default principal `spawn` and full compatibility with explicit callers).
- **`spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]`**: Replays `session_control` into typed `TaskEpisode`s and optionally verifies 100% parity against `mission_tasks` SQL projections (`--verify-parity` exits `0` on match, `1` on drift).
- **`spawn doctor [--config <path>] [--root <dir>] [--json]` & startup model preflight** (PR #165): `doctor` reports `model_endpoints` (`preflight_mode`, `model_relay` and `judge` health probes with `status`, `target`, `latency_ms`, `detail`, `remediation`, and `model_slots: {default, cheap, small}`). Startup preflight honors `SPAWN_MODEL_PREFLIGHT=warn|strict|off` (default `warn`; `strict` aborts startup when configured endpoints fail preflight; `off` skips probes) and `SPAWN_CHEAP_FALLBACK` (enabled by default in `warn` mode: unreachable `SPAWN_MODEL_RELAY` is cleared at startup so sessions fall back to direct/default; set `SPAWN_CHEAP_FALLBACK=off` — or `0`/`false`/`no`/`keep-relay` — to retain the relay).
- **Opt-in shared host build caches & `spawn doctor` diagnostics** (PR #180): Default-off opt-in cache sharing via `SPAWN_SHARED_BUILD_CACHES` (`1|true|yes|on` enables `<workspace_root>/.caches`; `0|false|no|off` or unset disables; invalid values fail closed) and `SPAWN_SHARED_BUILD_CACHE_DIR=<abs-dir>` (explicit validated absolute path rejecting relative paths, `..`, control characters, bare system dirs, sensitive subtrees such as `/run/keep*` and `/var/run/keep*`, `$HOME` directly, and symlinks resolving into forbidden paths). Materializes eight zero-secret toolchain variables (`ZIG_GLOBAL_CACHE_DIR`, `CCACHE_DIR`, `UV_CACHE_DIR`, `SCCACHE_DIR`, `PIP_CACHE_DIR`, `NPM_CONFIG_CACHE`, `GOMODCACHE`, `GOCACHE`) while preserving non-empty caller overrides, binds a deduplicated writable `ExtraMount` into sandboxed engines (`BwrapView` / `ContainerView`), and adds the active root to [`host.storage_bytes`](../crates/spawn-cli/src/storage_bytes.rs) `build_cache` canonical extent accounting. [`spawn doctor --json`](../crates/spawn-cli/src/config_file.rs) performs a strictly read-only check (never creating directories and redacting secrets in validation errors) and reports `shared_build_caches: {enabled, status ("disabled"|"ok"|"not_writable"|"not_a_directory"|"invalid"), path, exists, writable, managed_env_vars, error}`.
- **Single Herdr terminal backend** (PR #150): `SPAWN_TERMINAL_BACKEND` supports `herdr` only (default when unset/empty; `SPAWN_HERDR_MODE=core` uses embedded `herdr-core` via sibling `spawn-hc`, `SPAWN_HERDR_MODE=legacy` uses external `herdr 0.9.5`, `SPAWN_HERDR_SHARDS=1..=64`). The `tmux` backend has been removed: both `SPAWN_TERMINAL_BACKEND=tmux` and `[terminal].backend = "tmux"` in `spawn.toml` are refused fail-closed.
