# Glossary

The nouns Spawn's surfaces use (the CLI, the HTTP API in
[api.md](./api.md), the web pages and the docs), each with the one meaning
it has here. The anchor is the event record: its type names (`mission.*`,
`task.*`, `objective.*`, `resident.*`, `node.*`, `alert.*`, `source.*`,
`execution.*`, `command.*`, `message.*`, `session.linked`,
`deploy.finished`, `backup.*`) are the vocabulary every surface already
shares, and the nouns below are taken from them.

Where a surface spells the same thing another way today, the entry says so.
Those spellings still work; this page names what they mean.

| Noun | Meaning | Other spellings and near neighbours today |
| --- | --- | --- |
| **Mission** | A piece of work the owner hands to Spawn, with an optional source, gate and backlog. | `spawn mission list` prints the task board, not a list of missions. Seven `mission` verbs (`push`, `pushes`, `moved`, `landing`, `deployed`, `hold`, `release`) act on a source, not a mission. The Coverage page and `/api/features/coverage/projects` call an open objective mission a "project". |
| **Objective** | One numbered item of a mission's backlog, written `<mission>/<objective>` (`4/210`). | HTTP reads them at `/api/objectives` (legacy `/api/rsi`, [api.md](./api.md#aliases)); the web page is still called RSI. The CLI writes one three ways: `spawn verify 4/210`, `spawn objective retire --mission 4 --objective 210`, `spawn scratch claim --objective 4/210`. |
| **Task** | One assignment of work to a worker, with its stages. | Read over HTTP at `/api/tasks/<n>` (its texts) and `/api/tasks/<n>/gate` (its gate logs; legacy `/api/missions/task/<n>` and `/api/rsi/task/<n>`). The CLI has no read of one task's texts. |
| **Source** | The git repository a mission lands on. | A noun of the record (`source.held`, `source.moved`, `source.push_requested`) with no noun of its own on the CLI or in HTTP; its verbs are under `spawn mission`. `WorkspaceStore::project()` returns it under the name project. |
| **Agent** | A durable identity with its own branch and workspace (`AgentId`). | One agent has one session today, so the surfaces swap the two words: the route is `/api/sessions/<id>` (legacy `/api/session/<id>`), the web component `AgentDetail`, the docs say "agent page", the URL fragment is `#/sessions`. |
| **Session** | An agent's conversation with its engine (`SessionId`). | Read under one prefix: `/api/sessions/<id>` and its `/commands`, `/judgements` and `/pane` (legacy `/api/session/`, `/api/fleet/<id>/` and `/api/pane/`). `spawn fleet` and `/api/fleet` list sessions under the word "fleet"; `/api/fleet` also carries the host's health, `spawn fleet` does not. |
| **Execution** | One run of the engine in a terminal. | – |
| **Worker** | An agent that does tasks: a resident, or an ephemeral one. | Also used for the Bridge process that claims queue commands (`SPAWN_WORKER`, `lease_worker`, `CommandQueue::claim(worker)`), for a node's role (`spawn node connect --role worker\|peer`), and for a thread in the nodes code. Those are not workers in this sense. |
| **Resident** | A worker kept for a lasting concern, with a charter, approved by a human. | The web groups residents with the supervisor as "always-on agents". |
| **Supervisor** | The one always-on session that routes work. | – |
| **Node** | A machine Spawn may use beside the primary. | Not a host: `spawn-host` and `TerminalHost` say host for the local operating-system adapters, and a gate's `host_fault` verdict is a fault of the machine the gate ran on. The README says "this host" for the primary. |
| **Alert** | Something Spawn tells about itself (`alert.fired`, `alert.cleared`). | Not an **escalation**: the dispatcher's item for the supervisor or the owner (the route `/api/attention`, the page "Escalations", Home's "Needs you"). Not an **intervention**: a human input, counted for the north star (`/api/interventions`). Not the judge's **attention flag**, which is not an escalation either. The CLI reads alerts with `spawn alerts` and writes with `spawn alert`. |
| **Command** | One row of the queue, with an action: `start`, `input`, `resume`, `stop` or `key`. | `spawn ack <command id>` answers what its own docs call a message, and the record says both (`command.finished`, `message.taken`). "Command" is also the CLI's word for a subcommand (`unknown command <word>`). |
| **Judgement**, **judging**, **verdict**, **grade** | A judgement is the observer's reading of a session. A judging is one gate run. A verdict is a judging's result. A grade is a graded preset's skill level (`l1`, `l2`, `l3`). | `spawn alert judge` records the owner's verdict on an alert (right, false, stale, duplicate), a different kind of verdict. `judge` alone is a retired command and stays unknown. |
| **Secret** | A value that must not reach a transcript. | A **credential** is what Keep holds and delivers (`/run/keep`); a **secret value** is what `spawn mask` replaces with `[SECRET:NAME]`; tokens such as `SPAWN_SERVE_TOKEN` are credentials. `spawn secret` is a store of secrets by name that no running part of Spawn uses. |
| **Feature** | A capability of a product. | Two files are both called the feature map. The **feature map** is a mission's `--features` file: capabilities linked to objectives, issues and PRs, with a status derived from them (`/api/features`, the Features page, `spawn mission configure --features`). The **feature registry** is `docs/feature-map/registry.json`: capabilities (`cap-*`) linked to files (`/api/features/coverage`, the Coverage page, `spawn features`). |
| **Workspace** | A worker's working tree (`WorkspaceId`, `SPAWN_WORKSPACE`). | `--root` is the workspace root for `scorecard`, `backup`, `store`, `integrate`, `tick` and `mission show`, and the repository root for `spawn features`. A scratch directory (`spawn scratch`) is a second kind of working folder with its own noun. |
| **Store** | The PostgreSQL record (`PostgresStore`, `open_store`, `StoreError`). | `spawn store` is the off-host copy of the transcript archive and the native sessions, filed in the code under backup (`backup/store.rs`). Storage is also one of the core lines (`/api/core-lines`). |
