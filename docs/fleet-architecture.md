# Spawn fleet architecture

## 0. Goal

**Scale agents with as little human intervention as possible.**

The north-star metric is human interventions per agent-hour, at a given
throughput (tasks completed, scenarios passed). Every part below exists to
push that number down, and to do it with the system itself as the improver.

It counts what a person meant, once (objective 049): a person's command
(principal `human` or `spawn`, the board's and the CLI's) continues the
intent of the latest earlier command of the same principal, session and
action (the whole JSON, as jsonb) when that one finished failed at or
before this one was created, at most 60 minutes before it; otherwise it
starts one. An intent counts at its first command's creation; its outcome
is its last command's (`taken`, `failed`, `pending`). The view
`spawn_human_intents` holds one row per intent, and every count of a
person's commands reads it: the scorecard's autonomy (`command`, with the
part `retries`, the commands beyond each intent's first), `/api/fleet`'s
`ladder_24h.human`, and the experience views (§4). `/api/fleet`'s
`health.human_per_agent_hour` reads `spawn_interventions` (every kind, at_ms
in the last 24 h) over `agent_hours_24h`: the Home card's figure,
`/api/interventions?window=24h`'s `per_agent_hour`. `GET /api/interventions?window=24h|7d[&until=<RFC 3339>]` (any other
window, or an `until` that is no time, 400) answers the figure over
`[until − window, until)`: `window`, `since_ms`, `until_ms`, `agent_hours`
(the executions' running time clipped to the window, to 3 decimals),
`total`, `by_kind` {`command`, `resume`, `owner_task`, `cancel_reassign`},
`retries`, `per_agent_hour` (null with no agent hours) and `items`, the
interventions newest first, each with `kind`, `at_ms`, `principal`, `agent`,
`session`, `line`, `commands` (an intent's ids in order), `attempts` and
`outcome`, each command's outcome as it stood before `until`. Home shows it
for 24 h and 7 d with the interventions behind each figure
(`web/src/interventions.ts`).

Four ideas carry the design:

1. **A decision ladder.** Every judgement and every action goes to the
   cheapest tier that can decide it with confidence:
   rules (free, exact) → Jev (System 1: fast, calibrated, cheap) →
   supervisor (System 2: a Claude session, slow, expensive) → human (scarcest).
   A tier that is not confident escalates; it never guesses. Tiers are
   strictly layered (see §2.1): a lower-cost tier never overrides a fact,
   and Jev is a filter, not an authority.
2. **Everything is experience.** Each decision is an event: tier, evidence,
   answer, confidence. Later facts label it: the agent kept working after
   "COMPLETED", a human had to step in after "fine", the playbook action
   unblocked the worker. The event log is the store; labels are derived, not
   hand-written.
3. **RSI consumes the experience.** The system improves its own code through
   gated missions (§10.4). The objectives that would move decisions down the
   ladder by replaying recorded episodes are not built (§9).
4. **Synchronization points.** Work runs in parallel and joins only at
   named points, where a record says exactly what it covers (§1.1).

## 1. Layers

```text
 ┌──────────────────────────────────────────────────────────────────────┐
 │ 5 Interface   web UI (fleet board, escalations, agent detail)        │
 │               HTTP API: reads, one write (a person's input) · CLI     │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 4 Control     Supervisor: one perpetual Claude Code session           │
 │                 reads fleet state, submits commands to workers        │
 │               Dispatcher: delivers tasks, nudges, digests escalations │
 │               Integrator: gate + host-side commit + CAS baseline      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 3 Observation Rules: decide every state the facts settle              │
 │               Jev: decides only what the rules leave ambiguous        │
 │               → observation events (state, who decided, why)          │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 2 Event log   PostgreSQL: spawn_events (truth), spawn_commands        │
 │               (queue), grants, views; the control stream (§10.1)      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ 1 Runtime     Bridge: start / input / resume / stop                   │
 │               Claude Code engines in Herdr + git workspaces           │
 │               transcripts, screen, exit files → events                │
 └──────────────────────────────────────────────────────────────────────┘
```

Commands flow down (4 → queue → 1). Facts flow up (1 → log → 3 → 4/5).
No layer calls another layer's process directly; they meet in the log.

| Layer | Parts |
| --- | --- |
| 1 Runtime | Bridge, Herdr (`spawn-hc` / legacy `herdr`), git workspaces and mission mirrors, transcripts and exit files; the nodes Spawn may use (§1.2) |
| 2 Event log | `spawn_events` (with the control stream), `spawn_commands`, grants, projections, views |
| 3 Observation | Observer: rules first, then the model judge (Jev) on ambiguity; without a Jev URL, the rules alone |
| 4 Control | dispatcher and liveness rule (in `spawn loop`), supervisor session, integrator (`spawn integrate`) |
| 5 Interface | `spawn serve` (API and web UI: reads, and one write, a person's input to a session), the `spawn` CLI |

### 1.1 Synchronization points

Work runs in parallel between synchronization points and joins only at
them. A synchronization point is where work changes hands: the points
are named in the table below, one row per hand-off. Between two points no one waits on another's
progress; all waiting happens at a point (a landing that waits for a hold to
end or for its turn, §10.3, waits at the Gate). At a point nothing moves on
until a record names exactly what it covers (the task, the report's
revision, the commit) and who decided. A loop that moves on from inferred
progress (a quiet pane, a status that looks done) instead of such a
record is a defect, wherever it is. A file becomes a record only through
the one who reads it: a worker's `.spawn/report.md` counts once the
dispatcher has stored it as `task.reported`.

Two terms used below. The *lead* is the session that decides for a
mission's owner: Spawn's supervisor, or the owner's own supervising
session. A *ccd* is a Claude Code session run interactively with
`claude --dangerously-skip-permissions` (never print mode).

| Point | Joins | Moves on when | Record | Decided by | Enforced today |
| --- | --- | --- | --- | --- | --- |
| Brief | asker → worker | the task names its goal, inputs and done criteria, and is delivered once its worker has reported its previous task (§3.2) | `task.assigned`, then `task.delivered` | supervisor, the integrator for objective tasks (§10.4), or the owner | the records, and a brief that is not empty; its content is not checked. The base is the agent's latest `artifact.committed` at delivery (`reason` `base_merged` when the delivery merged `main`), or `execution.started` `fork.commit` for a branch the start created; it is not in `task.assigned` |
| Review | worker → reviewer | a verdict names the task, the report's revision and the commit it read | review verdict | a ccd session that did not write the work | not in code: the lead runs it, outside the log (§9) |
| Accept | report → mission | in a review-mode mission, the accept or reject names the revision it judged | `task.reviewed` (§10.3) | supervisor, or the owner | the revision check when `--revision` is given, a note on every send-back, and a refusal in a gate-mode mission; citing a review verdict is not checked (§9) |
| Gate, fleet | branch → `main` | the gate's verdict names the candidate; the baseline moves only to that commit, by compare-and-swap | `task.gated`, `task.landed` (§10.2, §10.3) | integrator | yes |
| Gate, owner work | owner's checkout → candidate | the owner's gate names the candidate and the base it judged against (the missions' `main`) | the gate's verdict file | the owner's gate | only at Deploy: accepted owner work reaches the owner's checkout before any gate |
| Deploy | candidate → `main` and live | the verdict is `accepted`, its candidate is HEAD, its base is the missions' `main`, and the tree is clean | `source.moved` when `main` moves (`spawn mission moved`, §10.3) | `deploy/deploy.sh` | yes, by `deploy/deploy.sh` |
| Publish | owner's checkout → remote | the pushed commit passed its gate and a secret scan | the push | owner | by the owner's procedure, not by code |

**The review.** A report gets a review before it moves on: in a
review-mode mission before it is accepted, in a gate-mode mission before
its gate runs (the cheap check first, the gate after). The reviewer is a
ccd that did not write the work, and it starts from the brief, the diff and the report,
never from the author's conversation. It checks two things:

- *Completion quality.* Every done criterion in the brief is met and shown
  (a test, a command and its output), not only claimed. The diff touches
  only what the brief needs. Changed behaviour has tests. Nothing is left
  half done: no TODO, no skipped or weakened test, no disabled check. The
  report matches the diff.
- *Efficiency.* What the work cost (elapsed time, turns, deliveries, restarts,
  rejected attempts, model spend) against the size of the change. It also
  looks for loops (the same failing command again and again), wandering
  outside the brief, and work redone that already existed. An efficiency
  finding never rejects correct work: it goes into experience, and into the
  briefs and routing rules that caused it.

The verdict is `pass`, `fix` (concrete defects, each with `file:line` and how
it fails) or `reject`. It holds for the revision and commit it names and no
other: a report sent back and revised needs a new review. Several review
sessions may run at once. The lead works with them as peers, one subject per session. The
lead's own commits pass the same point: a session other than the lead
reviews them. Today the lead runs reviews for owner work (missions 7 and 8)
and for everything it merges, gates or deploys; for the fleet's other
reports the Review point is not built (§9).

### 1.2 Runtime layer: nodes

Spawn may use machines beside the one it runs on (multi-host phase P1,
owner decision 2026-11-23; design: sync/design-multi-host-nodes.md). The
*primary* keeps the control plane; the others are *nodes*, at layer 1 only.
In P1 Spawn knows which nodes it may use and whether they answer: a node
runs no job yet, and a drain changes no scheduling.

- `spawn node add <name> --ssh <[user@]host|ssh://[user@]host[:port]>
  [--note <text>]` (the owner's login) runs a read-only inventory over ssh
  once and records `node.joined` (arch, os, kernel, cores, the cgroup's
  `cpu.max` and `memory.max`, memory, free disk of `$HOME`, GPUs;
  `exit_country` null until egress is read). A joined name is refused
  before any ssh. `spawn node drain|undrain|remove <name>` record
  `node.drained`, `node.undrained`, `node.removed`; a removed name joins
  again from nothing.
- `spawn loop`'s heartbeat runs on a worker thread with a database
  connection of its own (statements bounded at 5 s, a database error
  paced from 15 s to 5 min), so a pass never waits on a node, its
  records or its alerts; with no node joined it reads the membership once
  a minute and does nothing else. Every 15 s (a monotonic clock) it runs a
  small probe (load, available
  memory, free disk) on each node, one at a time per node, killed with its
  whole process group at 15 s. It records `node.heartbeat` every ten
  minutes while the node answers, `node.lost` after `SPAWN_NODE_LOST_SECS`
  (default 60, 30 to 86400) without an answer (counted from the worker's
  start for a node not heard since; `last_seen_ms` is only an answer it
  knows), and `node.back` at the next answer. Each names its `join`, the
  current `node.joined` event: the writer refuses one of an earlier join,
  so a probe still running when a node was removed and joined again
  records nothing. At each read of the membership it fires `node_lost` for
  each node's latest `node.lost` that has no alert and clears it once a
  `node.back`, a removal or a new join followed, so a loss and its
  recovery between two reads are both told, in order, and a failed alert
  write is made at the next read.
- `spawn node list [--json]` and `GET /api/nodes` (the reader login; no
  ssh destination, endpoint, note or public key, no clock) give each joined node with `state`
  (`lost` before `drained` before `live`), `lost_since_ms` while lost and
  its latest heartbeat sample. The Fleet details show one row per node and
  say when the read failed.

#### macOS nodes, power and the low-battery soft drain (issue #91 P1a)

- The inventory and the heartbeat probe are POSIX `sh` dispatched on
  `uname -s`: on macOS (`Darwin`) they read `sw_vers -productName`,
  `sysctl -n hw.ncpu`, `sysctl -n hw.memsize`, `vm_stat` (free, inactive
  and speculative pages times its page size: the available memory),
  `sysctl -n vm.loadavg` (`{ 1.52 1.71 1.80 }`) and `pmset -g batt`; on
  Linux what they read before, and `/sys/class/power_supply/*` (no glob:
  `ls`, so a zsh login shell takes it). A `darwin/arm64` node joins and
  beats instead of failing on "the probe's output has no cores".
- `power` (`{on_ac, battery_pct, charging, remaining_mins}`) rides on
  `node.joined` and `node.heartbeat` when the node has a battery or an AC
  sensor, and is left out otherwise (a desktop, a VM, every record before
  it); `spawn_node_record` checks its shape. The heartbeat records a sample
  early when the power moves (AC plugged or unplugged, charging started or
  stopped, the charge moved by 5 points), so the soft drain below takes
  effect within a probe's interval. `spawn node list`, `GET /api/nodes` and
  the digest give each node's `power` (the latest heartbeat's, else its
  join's, else null).
- The soft drain (crate `nodes/power.rs`): a node on battery at or under
  `SPAWN_NODE_LOW_BATTERY_PCT` (default 20, `0` off) is
  `low_battery_drain` and `effective_drained`, and `schedulable` (the
  scheduling-eligibility helper: live and not soft-drained) is false. It is
  read, never recorded: `state` and the owner's `drain`/`undrain` stand,
  and a node plugged in again is eligible at once.

#### Transports and hops (issue #85)

A node is reached over ssh (as above), or over tcp or udp to its own
`spawn node serve`, directly or through up to 8 other joined nodes, so a
node behind a NAT, a firewall or another network still joins.

- `spawn node add <name> --ssh <dest> | --tcp <[tcp://]host:port> |
  --udp <[udp://]host:port> [--via <hop>[,<hop>...]]
  [--mode direct|relay|hole_punch]` (exactly one transport). `--via` names
  joined nodes, the hops from the primary in order. For ssh they are jump
  hosts, reached through a nested `ProxyCommand` rather than `-J` (whose
  jump hosts get none of the command line's `-o` options): each jump is an
  `ssh` with all nine hardening options of the destination's (`BatchMode`,
  `ConnectTimeout`, `StrictHostKeyChecking=yes`, `UpdateHostKeys=no`,
  `ControlMaster=no`, `ControlPath=none`, `ClearAllForwardings`,
  `ForwardAgent=no`, `ForwardX11=no`) and `-T -W [%h]:%p -- <jump>`, every
  word POSIX single-quoted and each nested command's `%` doubled; a jump
  is an ssh node whose destination passes the same check. For tcp and udp
  the hops are relays (tcp or udp nodes, mixed as each hop's scheme says);
  the writer refuses a hop of the other kind for either. `--mode` defaults to
  `relay` with hops and `direct` without; `direct` with hops is refused;
  `hole_punch` labels a path the node opened by speaking first (the probe
  goes the same way: the label is the record's, not a different protocol).
- `node.joined` gains `transport` (`ssh`, `tcp`, `udp`; absent means ssh,
  every earlier join), `endpoint` (null for ssh, whose destination stays in
  `ssh`; `[scheme://]host:port` for tcp and udp: a name, an IPv4 or a
  bracketed IPv6 address, port 1 to 65535, no `@`, no leading `-`, no
  space, at most 255 characters), `via` (at most 8 joined names, never the
  node itself, none twice) and `mode`. `spawn_node_record` checks them under
  the append lock, the ssh check and its words unchanged, and walks the
  hops' own `via` (current joins only, each name once) to refuse a loop.
  The readers default an old join to `ssh`, `[]`, `direct`; `node_members`
  (the heartbeat's) gives the ssh destination as `endpoint`, `NODES` does
  not, and `/api/nodes` withholds `ssh`, `endpoint` and `note` alike (and,
  since #87, `pubkey`): the web learns how a node is reached (`transport`,
  `mode`, its hops' names), never where.
- The wire: one JSON object a message, a line on tcp (one exchange a
  connection), a datagram on udp, each `{nonce, op, at_ms, auth?, ...}`
  answered `{nonce, ok, ...}`. Every nonce is 16 bytes of `/dev/urandom` and a
  counter; a reply naming another nonce is refused on tcp and dropped on
  udp (the wait goes on, sent again with backoff until the deadline), so a
  spoofed or stale reply is never taken. Ops: `inventory` and `beat` (the
  node runs its own copy of the fixed read-only scripts, `{output}`, at
  most 256 KiB, masked on arrival); `forward` `{ttl, hops, timeout_ms,
  inner}` (at most 8 hops within the ttl, each a tcp or udp endpoint, none
  twice, at most 60 s, the inner op a probe or `register` only; each relay
  passes on 400 ms less time and a nonce of its own, and checks that the
  reply it relays names the inner request); `register`. Frames are bounded
  (320 KiB on tcp, 65 000 bytes on udp).
- Authentication: with `SPAWN_NODE_TOKEN` set, `spawn node add`, `spawn
  node register`, the heartbeat and every relay sign each request with
  `auth`, HMAC-SHA256 keyed by the token over a domain-separated list of
  its `op`, `nonce`, `at_ms` and what it acts on (`register`: `name,
  transport, endpoint, via, mode, note, inventory`; `forward`: `hops, ttl,
  timeout_ms` and the inner request's `op` and `nonce`). The token itself
  never goes on the wire. A node with the token refuses any request
  without a valid `auth`, or more than 120 s from its clock; a `register`
  always needs one, so `spawn node serve --register-store` refuses to start
  without `SPAWN_NODE_TOKEN` (fail closed), and `spawn node register` does
  not send without it.
- Each nonce runs once. Before a request takes any slot, the node looks
  its nonce up (at most 1024 kept): one answered in the last 125 s is
  answered from that cache, one in flight is dropped (a udp resend; a tcp
  peer is told so), and the mark of a request that never finished (an
  error, a panic, no slot) is cleared, so its next send runs. A udp probe
  through relays to a dead hop costs that hop one resend loop, not one per
  resend of every hop before it, and a `register` sent again while the
  store records it is recorded once and answered.
- A relay sends on only an inner request it rebuilt from the allowed
  fields of a probe (`op, nonce, at_ms, auth`) or a `register` (those and
  `name, transport, endpoint, via, mode, note, inventory`), each of its
  type; any other op or key, in the inner request or the envelope, is
  refused. It resolves its next hop itself and connects only to the
  addresses it checked: never a loopback, unspecified, link-local,
  broadcast or multicast one, IPv4-mapped ones included, unless it runs
  with `--allow-loopback-forward`. Every failure to reach the next hop
  (refused, silent, not a node, another nonce) is said in the same words,
  "the next hop gave no valid answer", so a forward is no port scanner.
- A listener handles at most 64 requests at once, 16 of them from one
  address, and a tcp peer has 2 s to send its request line, so a peer
  holding idle connections starves neither the others nor the heartbeat.
- `spawn node serve --listen <tcp://bind|udp://bind>[,...] | --tcp <bind> |
  --udp <bind> [--register-store] [--allow-loopback-forward]` answers those
  ops; with `--register-store` (the owner's login, on the primary or a host
  with its database) it records the `node.joined` a signed `register` asks
  for, each field checked as the writer checks it, the note masked.
- `spawn node register --to <[tcp://|udp://]host:port> --name <name>
  --listen <tcp://host:port|udp://host:port> [--via <hop>[=<endpoint>],...]
  [--mode ...] [--note ...]` runs the inventory on the node itself and
  sends it up, signed (up to five tries within 30 s, one nonce, one auth):
  directly, or, when the
  hops name endpoints, through them nearest first. `--listen` is the
  endpoint the primary will probe (the node's `spawn node serve`), never
  0.0.0.0 or [::].
- The heartbeat resolves each node's route from the membership at every
  read (a hop removed makes its probes fail, so the node is lost), and
  `spawn node list` shows `TRANSPORT` (`ssh`, `tcp/direct`, `udp/relay`,
  `ssh/relay`, `tcp/hole_punch`) and `via a->b`; the Fleet row shows the
  same badge, in the same words, and `via a → b`.

With `SPAWN_NODE_TOKEN` set, the tcp and udp transports are encrypted too
(issue #87, below): a reader on the path, or anyone without the token,
sees who talks to whom and when, never a probe's output, a registration
or an op. The key is the fleet's one shared token (rotated with
`SPAWN_NODE_TOKEN_PREV`), so this is not zero-trust: a relay hop is a
fleet member holding the token and could open what it forwards. Only the
coordinator path below (`spawn-e2e-v1`, sealed to the target node's own
key) is a blind relay. A node without a token speaks the #85 wire in the
clear, so run that over a network already trusted (a VPN, a LAN), as §4.2
of the distributed research leaves the overlay to the owner.

#### The end-to-end mesh (issue #87)

A node anywhere (behind any NAT, firewall or carrier-grade NAT, no inbound
port open) joins through a *coordinator* it dials out to, and everything
it is asked and answers is sealed end to end: the coordinators (one, a
tree of them, or a Cloudflare Worker and Durable Object relay on `:443`)
forward ciphertext they cannot open. Two layers:

- **Layer 1, the seal** (`nodes/crypto.rs`). Each node has an Ed25519
  identity (PKCS#8 v2, `ring`): its `node_id` is SHA-256 of the 32-byte
  public key in lowercase hex, its `pubkey` that key in base64. It is read
  from `--key-file` or `SPAWN_NODE_KEY_FILE` (made with mode 0600, its
  missing directories 0700, when absent; never overwritten), else
  `SPAWN_NODE_KEY` (base64 PKCS#8, wiped after use), else made for the run.
  It signs a rotating X25519 prekey: `prekey_sig = Ed25519(
  "spawn-prekey-v1" || node_id || x25519_pub || epoch_be64)`; the prekey
  before the current one still opens what was sealed to it. A request
  (`spawn-e2e-v1`) checks the target's id against its key, its key against
  the one pinned at its join, and its prekey's signature; takes a fresh
  ephemeral X25519 key, and derives with HKDF-SHA256 (domain
  `spawn-e2e-v1`, the token mixed into the salt when there is one) two
  ChaCha20-Poly1305 keys, the request's and the reply's, bound to both ids,
  the epoch, the channel, the time and the direction (wiped when dropped,
  `zeroize`). The header `(domain, from, to, from_pub, eph, to_xpub,
  epoch, channel_id, at_ms, req_nonce)` is signed by the sender and is the
  AEAD's associated data; the whole request is the ciphertext `ct`. The
  responder refuses a revoked sender (`SPAWN_NODE_REVOKED`, node ids),
  an `at_ms` more than 120 s off, a bad signature or id, a replay (each
  `(from, req_nonce)` kept until its `at_ms` can no longer pass), a prekey
  it no longer holds, and a ciphertext that opens under neither the token
  nor `SPAWN_NODE_TOKEN_PREV`; its reply is sealed under the reply key.
  X25519 is computed in Rust (RFC 7748's ladder, checked against the RFC's
  vectors and `ring`'s own agreement): `ring` keeps no reusable X25519
  key, and a prekey answers many requests.
- **The tcp and udp wire's seal** (`spawn-wire-v1`, `op: "sealed"`). With
  a token, `spawn node add`, `register`, `probe` and the heartbeat seal
  each request under a key HKDF derives from the token, 32 random bytes of
  salt, the nonce and the time, and keep the #85 `auth` on the outside
  (checked before anything is opened); a forward's inner request is sealed
  by the sender for the last node and a relay's own forwards are sealed
  too. Each reply comes back `sealed_reply` with a fresh 32-byte
  `reply_salt` (its key, HKDF of the token bound to the request's nonce,
  time and salt) and a fresh 12-byte `reply_nonce`, both in its AEAD data,
  so two replies to one request (two nodes sharing the token, a resend
  answered again) never share a keystream. What this protects against: a
  passive reader of the path, and anyone without the token. What it does
  not: a fleet member on the route (a `--via` relay holds the same token
  and could open the inner request); the zero-trust blind relay is the
  coordinators' `spawn-e2e-v1` alone. A request signed in the clear (#85)
  is still answered as before.
- **Layer 2, the coordinators** (`nodes/ws.rs`, `nodes/coord.rs`). One
  protocol, `spawn-coord-v1`, one JSON object a WebSocket text frame
  (`ws://`, `wss://`) or a line (`tcp://`). The WebSocket is RFC 6455,
  synchronous, without an async runtime: client frames masked, server
  frames not (each side refuses the other way), fragments assembled,
  pings answered, every message at most 512 KiB; the TLS handshake and
  the upgrade are bounded by the connect's timeout, every read and write
  of them by the time left; `wss://` is TLS by
  `rustls` on `ring`, verified against the Mozilla roots (`webpki-roots`)
  and the PEM certificates of `SPAWN_COORD_CA_FILE` when set. Messages:
  `hello`/`welcome` (keys, prekey bundle, `name`, `role`, `zone`,
  `udp_reflexive`, `kind` node, client or coord; signed by the sender's
  key; with `SPAWN_COORD_TOKEN`, the hello's `coord_auth`, HMAC-SHA256 of
  it under the token, and on `wss://` also the upgrade's `Authorization:
  Bearer`, sent inside TLS only: on `ws://` and `tcp://` the token never
  crosses the network, `coord_auth` alone proves it), `lookup` /
  `peer_info` (public keys, zone, role, udp mapping only), `relay {from,
  to, envelope}` (`from` must be the session's own node id, the envelope
  forwarded untouched), `zone_summary`, `punch`, `ping`/`pong`, `error`.
- **The recursive tree.** `spawn node coord --ws <bind> | --tcp <bind>
  [--udp <bind>] [--zone <name>] [--parent <ws://|wss://|tcp://...>]
  [--key-file <path>] [--lease-secs <n>] [--allow-unauthenticated]` keeps
  an in-memory directory of its sessions (soft state: a node that
  reconnects takes its id back, but a client's hello never takes a
  connected node's or coordinator's id: it is refused, and a probe asks
  again with a key of its own; a silent session closes after 75 s; at
  most 4096 sessions, 256 queued messages each, 256 connections not past
  their hello (more are closed at once), 64 lookups asked of the parent
  at once). A summary is cut to what fits one 512 KiB frame. With `--parent` it dials
  its parent as a `coord` session and pushes `zone_summary {zone,
  peers_summary, lease_epoch, lease_ms}`, its whole subtree, O(K) in its
  peers, at a third of its lease and at once when a session comes or goes
  (on the same connection, before anything that session sends); the
  parent forgets a summary when its lease runs out or its session closes,
  and takes from a child only the ids of its summary. A lookup or relay
  for an id it does not hold goes down the child whose summary has it, or
  up to its parent; one nobody holds is answered `error: no such peer`.
  A ws or tcp bind beyond loopback needs `SPAWN_COORD_TOKEN` or
  `--allow-unauthenticated`. `--udp` answers `{"op":"stun","nonce"}` with
  the address the datagram came from.
- **A node behind it.** `spawn node connect --coord <url> --name <name>
  [--role worker|peer] [--zone <name>] [--key-file <path>] [--udp-stun
  <host:port>] [--register-to <primary>] [--register-store] [--note
  <text>]` keeps one outbound connection (made again with a backoff from
  250 ms to 60 s; its prekey rotated hourly, the hello said again), and
  answers each sealed `inventory` or `beat` with its own copy of the fixed
  scripts, at most 32 at once (more are refused as busy). Without
  `SPAWN_NODE_TOKEN` it refuses to start unless `--allow-unauthenticated`
  (any client of the coordinator could then seal a probe to it). `--register-to` looks the primary up by name or id and sends it
  a sealed `register` (needs `SPAWN_NODE_TOKEN`, mixed into the keys):
  `transport` coord, `endpoint` the coordinator's URL, its `node_id`,
  `pubkey`, `role`, `zone`. A node run with `--register-store` (the
  owner's login, `SPAWN_NODE_TOKEN`) records it, the identity checked
  against the envelope's own sender, every field as the writer checks it,
  a store's refusal said in the writer's words only (never SQL). With
  `--udp-stun`, the node learns its udp mapping and announces it; a probe
  then tries that mapping straight over udp for 700 ms (P4, mode
  `hole_punch`), falling back at once to the relay (P3). The scope of P4
  as built: it reaches a node that is public or behind a NAT that lets
  datagrams into a mapping from anyone (endpoint-independent filtering);
  the prober announces no mapping of its own, so it asks no punch and
  opens no address-restricted NAT. A node does answer a `punch` that
  names an asker's mapping (for peers that announce one): one fixed
  datagram `{"op":"punch_probe","nonce"}` (a checked nonce) from its
  mapped socket, never to a loopback, unspecified, link-local, broadcast
  or multicast address, at most one per address and 16 in all every 2 s,
  so a punch cannot make it a reflector.
- **The primary.** `spawn node add <name> --coord <url> --node-id <hex> |
  --peer <name> [--mode relay|hole_punch]` runs the preflight inventory
  through the coordinator and records `node.joined` with `transport`
  coord, the coordinator's URL as `endpoint`, and the `node_id`, `pubkey`,
  `role` and `zone` it verified; `spawn node probe <name> [--op
  inventory|beat] [--json]` probes any joined node once over its route
  (ssh, tcp, udp or coord) and records nothing; the heartbeat probes a
  coord node through its coordinator, sealed to the key its join pinned,
  so a coordinator that answers with another node's keys is refused.
- **The writer.** `spawn_node_record` takes `transport` coord with an
  `endpoint` `^(ws|wss|tcp)://host[:port][/path]$` (at most 255
  characters, no `@`, space or control character, port 1 to 65535), a
  mode of `relay` or `hole_punch`, hops (if any) tcp, udp or coord nodes;
  and on any join optional `node_id` (`^[0-9a-f]{64}$`), `pubkey`
  (base64), `role` (worker or peer) and `zone`. `NODES` projects
  `node_id`, `role` (worker by default) and `zone`; `node_members` adds
  `node_id`, `pubkey`, `role`, `zone` when a join holds them;
  `/api/nodes` withholds `ssh`, `endpoint`, `note` and `pubkey`.
- **Hardening found in the tracks' runs.** A request's answer is kept
  against a resend until its own `at_ms` plus the skew and 5 s (at most
  twice the skew and 10 s), so a request from a clock running fast never
  runs twice. A udp exchange gives up after two `ECONNREFUSED` in a row
  (about 250 ms) instead of its 30 s bound. A node's refusal said before
  it read the request's nonce (`no whole message in time`) is shown in its
  words, not as "answered another request". A tcp connect interleaves the
  host's IPv6 and IPv4 addresses (RFC 8305's order) and bounds each by
  half the time left, at most 2 s: measured, a closed tcp port refuses in
  about 1 ms, while a silent one (its packets dropped) held a connect for
  its whole bound, 3.0 s, before the next address was tried. `spawn node
  serve` beyond loopback needs `SPAWN_NODE_TOKEN` or
  `--allow-unauthenticated`.

The records are `node.recorded` events of the control session, outside the
control fold: a refold of this schema, or of one rolled back to before P1,
reads none of them. The owner-only writer `spawn_node_record` checks each
record's shape and the node's state under the append lock; every check and
every read is a seek of one partial index (`spawn_events_node`), never a
scan of a node's heartbeats nor of past joins. `init-db` builds that index
after the schema's transaction, `CREATE INDEX CONCURRENTLY`, so appends do
not wait for it (it waits for transactions already open, such as a running
`pg_dump`); a build cut short is dropped and made again at the next
`init-db`.

The trust boundary is the one panes have: a node writes to the log only
what an owner process records for it. The primary sends it nothing but
two fixed texts of read-only commands (over tcp and udp, only their
names); a node's own `register` is recorded only by an owner's `spawn
node serve --register-store`, signed with the fleet's token and checked
field by field as `spawn node add`'s join is. ssh runs in batch mode
against an already trusted host key, every jump host's ssh too,
without a shared connection, forwarding or terminal, with an environment
of `PATH`, `HOME`, `USER`, `LOGNAME` and `SSH_AUTH_SOCK` alone. The
destination is checked in the CLI and again in the writer, and every string
of a record is masked. Spawn changes no ssh configuration, key or file on a
node.

- **WebCrypto browser E2E (`spawn-e2e-v2`) & Phase 2 systemd split topology.**
  For browser clients without native `ring`, `web/src/e2e.ts` implements
  `spawn-e2e-v2` (and `pkcs8-ed25519-v2` key parsing) purely over standard
  WebCrypto (`crypto.subtle`) with X25519 + HKDF-SHA256 + AEAD and the 120 s
  clock-skew and replay guard (`docs/spawn-e2e-v2.md`). On the host, production
  control-plane units run split across `deploy/systemd/spawn-core.service`
  (`spawn serve --ipc --ipc-sock %t/spawn/core.sock`),
  `spawn-node-edge.service` (Go HTTP/SSE edge & externalized PWA assets from
  `SPAWN_WEB_DIR`), `spawn-node-gw.service` (Go signed node gateway), and
  `spawn-node-out.service` (Go outbound unary RPC relay on `%t/spawn/out.sock`),
  with zero `spawn.env` / `*DATABASE_URL*` credentials in any Go unit
  ([spawn-ipc-v1.md](./spawn-ipc-v1.md)).

## 2. Observation: rules first, Jev on ambiguity

The Observer folds facts per session (transcript events, executions, screen
readings). Each time a session's facts change, and every 30 s while an
execution runs, it classifies:

| Facts | Decided by | State |
| --- | --- | --- |
| permission dialog on screen or permission notification | rules | BLOCKED (permission) |
| a tool call is open, or the pane read busy less than 2 min ago | rules | WORKING |
| turn stopped at its prompt while a background job the execution started is open (§2.2) | rules | WORKING (`WAITING_FOR_BACKGROUND_JOB`) |
| execution ended by Stop | rules | COMPLETED |
| no execution yet | rules | WAITING |
| turn stopped, prompt idle, no background job open: finished, asking a question, or waiting? | **Jev** | WAITING / BLOCKED / COMPLETED |
| execution live, no open tool, no fact for 2 min (or a tool open for 10 min with no transcript progress, however busy the screen looks): thinking or hung? | **Jev** | WORKING / WAITING / BLOCKED |
| failed turn or an error on screen: progressing or stuck? | **Jev** | WORKING / WAITING / BLOCKED |
| process exited on its own (a non-zero exit code makes BLOCKED the rules' guess and always asks for attention) | **Jev** | COMPLETED / BLOCKED |
| process lost | **Jev** | UNKNOWN / BLOCKED / COMPLETED |

A failing tool call alone is not ambiguous: it only becomes the latest error
that Jev sees when another case asks it.

When rules decide, no model call is made. When they cannot, Jev gets the
evidence a human would use: the terminal tail, the task, and the recent
facts. It answers in one request:

- `semantic_state` (choice): distribution over the five states;
- `needs_human` (yes/no): probability that a human must answer or decide;
- `stuck` (yes/no): probability that the agent loops or makes no progress.

Without a Jev URL (`SPAWN_JUDGE_URL` or `--judge-url`) or a local outbound UDS
(`SPAWN_OUT_SOCK`, pointing to `spawn-node-out.service`'s `%t/spawn/out.sock`
and serving `POST /out/v1/jev/call` with `LoadCredential=typesafe.api-key`,
validated by `spawn-jev`'s `validate_out_sock` against symlinks, foreign UIDs,
and non-`0600` modes), the Observer judges
by the rules alone. A case they cannot settle goes the way of a judge that is
down (below): the rules' guess with its ambiguity, `reason =
JudgeUnavailable`, `judge: "none"`, and attention, so the dispatcher
escalates it rather than nudging. Observations in the log that name
`judge: "heuristic"`, a judge since removed, read and show like any other.

Every observation records who decided and why:

```json
{ "state": "BLOCKED", "confidence": 0.82,
  "judge": "jev", "ambiguity": "turn_stopped_idle",
  "rules_state": "WAITING",
  "probabilities": {"WORKING":0.0,"WAITING":0.18,"BLOCKED":0.82,"COMPLETED":0.0,"UNKNOWN":0.0},
  "needs_human": 0.96, "stuck": 0.22, "latency_ms": 748,
  "evidence": {"screen_tail": "...", "task": "...", "open_tools": 0, "turn": "stopped"} }
```

Every field after `judge` is optional in the serde payload, so an observation
without them still parses. `EventKind` has no variant for them.

If Jev is down, the rules' best guess is written with
`reason = JudgeUnavailable`, and the session is retried after 30 s, doubling
to at most 10 min. While retries are pending they replace the 30 s recheck.
It is never marked dead.

The Observer writes an observation at once when the judgement changes
(state, reason, attention, ambiguity). It also keeps a session's latest
judgement covering its latest fact: when a later fact (any record of the
session not the Observer's: a screen reading, a Bridge step, a checkpoint, a
command) leaves the verdict as it was, the session is judged again and, once
its facts have settled (no newer fact for 2 s), that verdict is written with
`based_on_seq` at the latest fact. It is a judgement of those facts: the
rules run again, and a Jev answer stands for them only when the evidence it
was shown is the same (its fingerprint, which counts the engine's moves and
what the facts say, not every record), with no second call. A session with no
new fact gets no new observation; a burst of facts with an unchanged verdict
(a busy turn) gets one, once it settles. A restarted Observer checks every
session once, and covers one that lags with a single observation. The
dispatcher acts only on a judgement that covers the latest fact, so this is
what lets it act on a worker that ended its turn before the screen settled at
its prompt.

Every Jev answer is written, even when the state did not change, together
with the evidence and a stable fingerprint of it. That makes the log the full
record of what the model saw and said, and lets a restarted Observer (or a
one-shot `spawn tick`) skip evidence it already judged. The Observer restores
its state only from rows whose source is `observer`, and the projection only
takes observations from that source.

Jev's distribution doubles as the attention signal. An observation asks for
attention when the probability on states that need someone (BLOCKED,
UNKNOWN), among the states the facts allow, reaches 0.5, or when
`needs_human` or `stuck` does, or when more than half of Jev's mass lies on
states the facts rule out (Jev and the facts disagree). Uncertainty between harmless states (WAITING
0.56 against COMPLETED 0.40 for an idle agent) pages no one: a false alarm
costs exactly the human attention the system exists to save.

### 2.1 Trust boundaries

Jev is a model. It is useful exactly where facts run out, and it is wrong
some of the time. The layering keeps its errors cheap:

1. **Facts > rules > Jev.** Jev is consulted only for cases the rules mark
   ambiguous, and its answer can never override a fact: if a permission
   dialog is on screen, the state is BLOCKED whatever Jev says. A Jev answer
   is stored as an inference with its confidence and evidence, never as a
   fact.
2. **Observation never acts.** The observation layer writes judgements and
   nothing else. Deciding what to do is the control layer's job, under its
   own deterministic policy.
3. **Jev alone can only trigger reversible, bounded actions.** In control, a
   Jev judgement may at most cause a nudge that a human could undo by doing
   nothing (typing `continue` into an agent that stopped mid-task), at most
   once per episode and a few times per agent-hour. Stopping, restarting,
   answering an agent's question, landing code, or anything with side
   effects outside the agent's workspace needs rules over facts, the
   supervisor, or a human.
4. **Trust is measured, not assumed.** Every Jev judgement is later compared
   with what happened (§4). Its per-question agreement and calibration are
   shown in the UI. Thresholds change only through a gated landing, never at
   run time.
5. **Jev is optional.** If Jev is slow, down, or disagrees with itself, the
   system runs on rules alone and escalates what they cannot decide. Losing
   Jev costs more human attention, never correctness.

### 2.2 Waiting on its own job

Workers often end a turn while a job they started runs in the background: a
test run or a gate started with Bash `run_in_background` (or a command the
engine moved there when its timeout ran out), a Monitor watching a log, or
an Agent launched in the background. Claude Code wakes the agent when the
job reports. Such a worker is working, not idle, and that is a fact the
rules settle from the transcript:

- **The records.** The Bridge reads them from the transcript it already
  follows (`ingest::claude_line`), as source `claude`. `job.started`
  (`job_id`, `kind` `bash`, `monitor` or `agent`; when is the record's time)
  comes from the tool result that started the job: its
  `toolUseResult.backgroundTaskId` (Bash), `taskId` with `timeoutMs`
  (Monitor), or `agentId` with `isAsync` and `status` `async_launched`
  (Agent; also a Skill forked to the background, `status` `forked`,
  `background`). A `SendMessage` whose result says it resumed a finished
  Agent (`resumedAgentId`) starts that Agent again under its id: each run is
  a start of its own, keyed by the job's id and the call that started it.
  `job.ended` (`job_id`, `how`: `completed`, `failed`, `killed`, `stopped`,
  `expired`) comes from the engine's `<task-notification>` with a
  `<status>`, or a Monitor's `[Monitor expired …]` event, as the engine
  gave it to the agent (mid-turn as a `queued_command` attachment, or as the
  prompt of a turn of its own; the copies are the same bytes, and the end is
  keyed by the job's id and a digest of the notification, so it is stored
  once and a later run's end is another record), or from a `TaskStop` whose
  result says it stopped the job. The engine's queue (`queue-operation`) is
  not read: it also carries sub-agents' notifications (removed and handed to
  the sub-agent, whose jobs are not the session's), and it can hold a quick
  job's end before the line with the result that started it, so a start
  read after it would open a job already ended. A Monitor's other events end
  nothing; an Agent's notification that
  says it `is waiting on its own background work` ends nothing (it paused
  with work of its own and notifies again); a message only queued for a
  running Agent starts nothing; a stop that failed ends nothing.
- **Within its execution.** A job is open only in the execution that started
  it, while its latest start has no later end: the execution's end (and so a
  resume, a new execution) closes its open jobs, whatever the transcript
  says later; an end for an id no start of the execution names closes
  nothing.
- **The rule.** A turn that stopped at its prompt (a `turn.stopped`, or a
  ready screen) while at least one job of the running execution is open is
  WORKING with reason `WAITING_FOR_BACKGROUND_JOB`, decided by the rules: no
  Jev call, no attention, so no attention or idle escalation and no nudge,
  and its task's `activity` step is not stalled. A failed turn with a job
  open is still the judge's (`engine_error`), which sees `open_jobs`. A turn
  that stops with no job open is judged exactly as before.
- **The bound.** A job open for `SPAWN_STALL_JOB_SECS` (default 7200 s; a
  Monitor expires by itself within an hour) is escalated once, heading
  `background job past its bound`, line "a background job has run for
  <duration>" with its kind, id, record and execution
  (`dispatch/jobs.rs`, tag `<session> job-<execution>-<job>-<start record>`), its age on the
  database's clock. Told once, it stays told while the job is open, and it
  is the only item about the worker meanwhile: what to do (stop the job,
  stop the worker, wait) is the supervisor's decision.
- **Causality** (docs/design.md §14). Decided from these records alone. The
  screen's "N shells still running" or "N monitors still running" footer is
  evidence in Jev's view, never a rule.

The attention view (`GET /api/attention`, `on_own_jobs`) lists the workers
held this way with their jobs and each job's age, and the board shows such a
worker's "doing now" as its background job and its age, and never as quiet.

## 3. Control: dispatcher, supervisor, human

Observation says what state an agent is in. Control decides what to do about
it, on the same ladder.

- **Dispatcher** (inside `spawn loop`) reacts to attention observations
  (BLOCKED, `needs_human`, COMPLETED, `stuck`, execution lost):
  a deterministic **policy** that maps (facts, observation) to an action.
  Jev never chooses the action; its judgement is one input, and only within
  the limits of §2.1:
  1. *Facts and rules* decide fixed cases: every end of an execution is
     classified and given its way back (the recovery, below); where the
     recovery has not recorded an interruption, a lost execution is still
     resumed once (the one-resume rule, the fallback).
  2. *A guarded nudge*: when the agent stopped without asking anything
     (Jev: WAITING, `needs_human` low, confidence high) and its task is not
     done, type `continue`, once per episode, rate-limited per agent.
  3. *Everything else escalates*: BLOCKED, `needs_human` high, `stuck` high,
     low confidence, Jev unavailable, or a nudge that did not help. The
     escalation goes to the supervisor when there is one, else to a human.

  The code is `crates/spawn-cli/src/dispatch.rs`, principal `spawn-dispatch`.
  It acts only on a judgement that covers the session's latest fact; a lost
  execution is resumed once (the one-resume rule); the nudge needs Jev's
  WAITING at an idle prompt with confidence ≥ 0.7, `needs_human` ≤ 0.2 and
  `stuck` < 0.5, once per
  episode (the latest non-dispatcher command) and three per agent-hour. Only
  a nudge that reached the worker counts: one refused before typing (a pane
  not ready) is sent again under `<key>-retry-<n>` on a judgement made after
  it, twice, then escalated as never typed. A worker idle at its prompt and
  not nudged, a nudge that did not help, a
  failed resume (the one-resume rule),
  attention, and a finished execution nobody stopped (its recorded end
  cause is not `stopped`) all escalate. A worker the rules judged waiting
  at its prompt on a background job of its own (§2.2) is not idle: it is
  not nudged nor escalated, and gets no new work unless its last task has
  closed (a revision, or a task after a closed one, is typed at its prompt
  as for any stopped turn); only its job past its bound is escalated, once.
  A Stop or Discard that failed,
  whoever sent it (a model switch, a closed task's worker, a retired
  resident, the integrator's budget abort, the supervisor, a person),
  leaves the worker running: the dispatcher reads the Stops of the live
  execution from the command queue, sends the action again under
  `stop-<execution>-retry-<n>` with a wait of a minute doubling, 3 times,
  without waiting for a new judgement (a failed Stop changed nothing), then
  escalates `stop failed` naming the task, and sends no more. Meanwhile
  nothing is typed into that worker and it is not nudged. Escalations
  coalesce into one digest typed into the supervisor
  while it is idle. The dispatcher keeps no state: idempotency keys and
  their commands' outcomes mark its nudges, resumes and stops, and the tags
  a delivered digest files on its command (`<session> <id>` per item, never
  in the text, which quotes workers' reports) mark what the supervisor was
  told. An approval or a human command (once finished, with its outcome: one
  that failed is reported as failed) is news until it was told, however
  long that takes.

  **When the dispatcher's pass runs.** The loop acts on records and on the
  times records name, never on the clock. Every pass reads the log's
  watermark (`max(spawn_events.id)`, one index-only read of its primary
  key); the dispatcher's step — the facts, the mission rows, the stall
  facts, the gate and hold records, the liveness of every resident and of
  the supervisor — runs when that watermark has moved since it last ran, or
  when one of its own timers is due, and on an idle fleet it runs not at all.
  Its timers are all computed from the records it read, never counted:
  a delivery's retry (`deliver.rs` `attempts()`: `NOT_READY_RETRY_MS`, 5 s,
  after a refusal that typed nothing, `retry_wait_ms()` — a minute doubling,
  at most 30 — after a failure), a stall's bound from the record it stands
  on (`dispatch/stall.rs`), a failed Stop's retry (`dispatch/stops.rs`), a
  background job's bound (`dispatch/jobs.rs`), a cancelled resident's notice
  and a report note refused before typing. A due time the step cannot date
  is due now, never later; a time already reached is the step's to act on
  now, not a wait. Between passes the loop waits on `LISTEN spawn_events`
  (the append's own notify) with a timeout of the earlier of the pass
  interval and the next due time, so a change ends the wait at once and a
  notification lost costs one interval at most: the notification is a hint,
  the watermark is the record (§14 of design.md). The steps run at most once
  a pass interval — the resolution the loop always had — so a burst of
  appends inside one interval is one pass's work, while the Bridge's claim
  of a submitted command is not held behind that interval. The Bridge's
  reading of each engine's screen and transcript is not on this schedule:
  neither is announced by an append, so it runs every pass (§5 of
  design.md).

  **The one-resume rule.** A lost worker execution is resumed once
  (`dispatch-resume-<execution>`); once that resume has failed, the session
  is escalated (`resume failed`) and never resumed again by rule, until one
  of its executions stays up. Failed is read from what the log records
  about the session since its last execution that stayed up (it neither
  died young, lost or exited non-zero within a minute of its start, nor
  ended lost before its engine became ready), never from the key of its
  latest execution: a resume that started an execution gave it a new id,
  and no resume is ever sent under that key. Whichever comes first trips
  it, about the dispatcher's own resume:

  - a resume whose execution ended lost before its engine became ready
    (the log has no Bridge `screen.observed` of it in state `ready`: its
    ready wait failed and the start was abandoned), whatever its command's
    outcome and however long its end took. The item carries the evidence,
    the execution's end (`its start failed: pane never became ready, last
    state <state>…`) or else the command's error, and is named by that
    execution (`resume-<execution>`): a later execution that fails the same
    way, a person's resume, is not told again;
  - a resume whose command succeeded and whose engine was lost again (or
    exited non-zero) within a minute;
  - a resume whose command failed before it started an execution.

  A resume whose engine became ready and stayed up did not fail: the
  session's next loss is resumed once again. (Task 132: after the host
  move of 2026-11-07, every resume of one session failed at its ready
  wait and ended lost, while the rule looked only at the young-death count
  (successful resumes) and the latest execution's key, so it was resumed
  every 20 s for ten minutes.) The facts are `failed_resume` and
  `resumed_young` in `dispatch_facts`; the rule is `resume_lost` in
  `dispatch/worker.rs`.

  **The recovery: every way an execution ends is classified.** An
  execution ends in many ways, and how it is brought back used to depend on
  who kept it and how it ended; nothing recorded why. Now one rule
  (`crate::recover`, in the loop before the dispatcher and the liveness
  rule) reads every ending from recorded facts, records it once, and takes
  its way back:

  | Cause | Told from | Path | The numbers |
  | --- | --- | --- | --- |
  | `exited` | the end's exit status 0 | `resume` | due at once |
  | `crashed` | a non-zero exit status, the signal the runner wrote beside it (9 for a SIGKILL), or a terminal gone while its server lives | `resume` | due at once; a resume's backoff doubles per failed one: 10, 20, 40, 80 … up to 640 s |
  | `memory_limit` | the engine's resource envelope killed it (objective 029) | `resume` | as a crash |
  | `host_restart` | the end found while the terminal server is gone, so every terminal in it died | `resume` | as a crash |
  | `disk_full` | the workspace's filesystem read under `SPAWN_DISK_FLOOR_GB` at the end | `wait` | probed every `SPAWN_RECOVER_PROBE_SECS` (10 s); revived within one probe of room |
  | `database_unavailable` | two or more passes of the loop that could not reach the database while the execution ran | `wait` | cleared at the first pass that gets through |
  | `model_auth` | an API error line with status 401 or 403, `authentication_failed`, or a text holding `/login` | `wait` | probed every 10 s; cleared when the credentials file changes, one retry every `SPAWN_RECOVER_AUTH_RETRY_SECS` (1800 s) while it does not |
  | `model_unavailable` | an API error line with status 529, 503, 500, 429 or none, or error `overloaded`, `rate_limit`, `server_error` | `wait` | retried after `SPAWN_RECOVER_MODEL_BACKOFF_SECS` (60 s) doubling per failed turn, capped at 4 x it, or at `retry_at_ms` when the text names when an account is available again. An engine that *ended* after it is resumed in the same native session after the backoff at most `SPAWN_RECOVER_MODEL_RETRIES` (3, from 1 to 20; another value stops the loop at its start) times in a row with no model answer between (a revived execution that made progress resets the count); its next such end is `model_unavailable` on `final` (evidence: `retries`), never revived, and told to the supervisor once. An engine at its prompt is retried without this bound |
  | `model_error` | the engine's own end after an API error line that is none of the above (a 400 that is not `Prompt is too long`, a 404, a 413), with no progress after it | `final` | never revived (the same request fails the same way); told to the supervisor once |
  | `context_exhausted` | an API error line whose text holds `Prompt is too long` | `handoff` | due at once: a Stop with its final checkpoint, then a new native session in the same workspace and branch |
  | `budget` | the integrator's Stop of a task over its budget (objective 026) | `handoff` | as a context's |
  | `stopped` | a Stop or Discard from a person, the supervisor, a cancelled task or a retirement | `final` | never revived |
  | `restarted` | the dispatcher's own Stop (`spawn-dispatch`) keyed `restart-<task>-<n>-stop` by the task's latest `task.restarted` (or a retry of it) | `final` | never revived by the recovery: the restart's own resume (`restart-<task>-<n>`) follows, once this end is recorded |
  | `reassigned` | the dispatcher's own Stop keyed `reassign-<task>-stop` of a task whose `task.cancelled` names `reassigned_as` (or a retry of it); evidence `to_session`, the new worker's | `final` | never revived; the worker's task went to a fresh one |

  The cause `restarted` or `reassigned` is read from the record, never from
  a key alone: a Stop from anyone but the dispatcher is `stopped`, whatever
  its key.

  When several hold the first in that order wins: a person's Stop before the
  budget, a memory kill before a host restart, a full disk before a database
  outage, a model login before a model outage before a context, a request
  the model refused before a crash, a crash before a clean exit.

  The dispatcher tells the supervisor of each final model record once
  (`model_unavailable` with its retries spent, `model_error`): one digest
  item of the worker's session, tagged `<session> model-<execution>`,
  naming the session, the execution, the cause and the status or the
  retries. It is news: told, it is never repeated. A Stop that is part of a revival (a handoff's) is no
  interruption of its own.

  Each interruption is one control record, written by the owner only
  (`spawn_host_record`): `{"type": "execution.interrupted", "agent_id",
  "session_id", "execution_id", "cause", "evidence", "path", "attempt",
  "due_ms"}`. `attempt` counts the interruptions of that execution, so its
  revival's key `recover-<execution>-<attempt>` names this one and no other;
  `due_ms` is when the revival is due (null on `final`). The record names
  the fact it rests on, so a restarted loop finds nothing to record twice
  and revives nothing twice. The revival is one command — `resume` for a
  resume path and a wait whose execution ended, `input` into a live engine,
  `start` for a handoff — submitted as `spawn-liveness`, no earlier than
  its `due_ms`. A wait clears on an observation, never on time: a model
  answer to a retried turn, a changed credentials file, the loop's first
  pass that reaches the database, or a free-space reading at the floor; its
  clear is one `execution.cleared` record
  (`{"execution_id", "session_id", "cause", "evidence", "waited_ms"}`) and
  then the revival. A wait never counts toward the handoff.

  A resume fails when its execution ends within 60 s of its start before it
  made progress (no tool call completed and no turn ended after it started);
  a revival that made progress succeeded, whatever ends it later. After
  three failed resumes in a row the next interruption hands off instead, its
  evidence naming `resumes_failed`. No path discards the workspace or the
  checkpointed work: an ending execution's final checkpoint is taken as
  before, a handoff stops first and starts on the same branch, a wait
  leaves the engine or its workspace as it is, and a final path keeps the
  branch.

  One rule for every agent: the supervisor, the residents, and the ephemeral
  workers that hold a delivered task (an idle worker with no open task is
  not revived). Where the recovery has recorded an interruption, the
  liveness rule (`supervise.rs`) starts nothing and the dispatcher's
  one-resume rule revives nothing: they keep their own jobs (the charter,
  the digest, the mission rows). The code is `crates/spawn-cli/src/recover.rs`;
  the facts `spawn_postgres::RECOVERY_FACTS`, which reads the end's status
  and signal, the transcript's API error lines by event id, the Stops sent
  into the execution, its progress and the interruptions and revivals
  already recorded.

  **The loop outlives its database.** A pass that failed only because the
  database could not be reached (`StoreError::Unreachable`) does not count
  toward `MAX_FAILURES`: the loop keeps passing on its backoff (at most 30
  intervals), builds the Bridge and the Observer again when the database
  answers, and the engines run on untouched. At the first pass that gets
  through, the recovery records the outage's interruptions
  (`database_unavailable`, with its window, its failed passes and one line
  of the error), clears them at once (that pass *is* the observation) and
  the Bridge checkpoints what the engines wrote meanwhile. Every other
  failure counts as before.

  **Deliveries that never reached their worker.** A delivery whose retries
  are spent on a host fault typed nothing (every attempt was refused before
  typing), so assigning the task again is safe and needs no judgement; a
  deploy is when such a fault may have gone. So (`deliver.rs`):
  - *Re-issued.* A task's first delivery (the task `assigned`, revision 0)
    whose retries are spent and none of whose attempts may have typed
    (`MAY_HAVE_TYPED`) is re-issued `SPAWN_REISSUE_AFTER_SECS` (300) after
    its last attempt finished, on the database's clock: in one transaction
    (`spawn_task_reissue`, owner only) the task is cancelled, its
    `task.cancelled` naming `by` `spawn-dispatch`, `reissued_as` the new
    task and `attempts` (every attempt, in order: `command`, `key`,
    `error`), and a new task is assigned, its `task.assigned` naming
    `reissue_of` and copying the old task's mission, kind, worker (a
    resident's; an ephemeral task gets a new worker with the same label),
    brief, model, objective, scenario and `created_by`. The new task is
    delivered by the usual rules (it queues behind tasks of its session
    assigned before it). The old one was never delivered, so its worker is
    not stopped, set aside or told; an objective keeps a task in flight
    throughout, so the integrator never assigns it twice. Such a delivery
    is neither escalated nor a stall.
  - *Escalated only when the re-issue is spent too.* A re-issue is never
    re-issued again: its spent delivery is escalated as `delivery failed`
    (tag `<session> deliver-<task>-<revision>`), the line naming the task,
    the task it re-issued (`task #<new> (a re-issue of task #<old>) never
    reached its worker ...`) and the attempts of both. A delivery that may
    have typed, and a revision's delivery, are escalated as before and never
    re-issued.
  - *Tried once more after a deploy.* `spawn mission moved` records
    `source.moved` for every deploy, also when it moves no mission's
    baseline. After the latest such record, every task whose current
    delivery is spent (re-issued or not, escalated or not, never one that
    may have typed) and was spent before the record gets exactly one more
    attempt, keyed `deliver-<task>-<revision>-moved-<that record's event
    id>`, at once (the delivery rules still say when the worker can take
    it). A success delivers the task (and a re-issue still due for it is
    not made); a failure leaves it as it was: no new escalation, no attempt
    again until the next `source.moved`. A re-issue due goes before the
    deploy's attempt, which its new task does not need.
  Each rests on recorded facts, never on time or absence alone (design.md
  §14): the re-issue on the recorded attempts and their outcomes, the extra
  attempt on a recorded `source.moved`; and each is itself recorded (the
  cancellation and the assignment name each other, the attempt is a command
  under its own key).

  **Stalls: no task waits silently.** A stall is the absence of an event,
  so the dispatcher also measures, for every open task, the step it waits
  on and since which record, from the log alone (`dispatch/stall.rs`, the
  query `stall_facts`), and escalates a step that has not moved within its
  bound, once per stall:

  | Step | The task | Measured from | Bound (default) |
  | --- | --- | --- | --- |
  | `delivery` | assigned or sent back, and its session's turn (a revision first, then the oldest new task; no earlier open task of the session holding it) | `task.assigned`, or the review or gate verdict that sent it back | `SPAWN_STALL_DELIVERY_SECS` (600) |
  | `activity` | delivered, waiting for its worker (a worker idle at its prompt with no report collected included, or with a report not taken as unfinished) | the later of `task.delivered` and the session's latest fact (a record of the session that is not the Observer's nor storage's) | `SPAWN_STALL_ACTIVITY_SECS` (900) |
  | `judgement` | reported, under review | `task.reported` | `SPAWN_STALL_REVIEW_SECS` (1800) |
  | `judgement` | in a gated mission, the task the integrator judges next (`Mission::ready`'s order) | the later of the record that made it ready and the mission's latest verdict that is not `stale` | `SPAWN_STALL_GATE_SECS` (3600) |
  | `landing` | landing, its source not held by the owner (§10.3, "Landing: the owner's hold") | the later of its `task.gated` accepted and the record that ended the latest hold on its source | `SPAWN_STALL_LANDING_SECS` (600) |
  | `stop` | an ephemeral task closed (accepted without a gate, landed, cancelled) whose worker still runs | the record that closed it | `SPAWN_STALL_STOP_SECS` (300) |
  | `halted` | a halted mission with open tasks | its `mission.halted` | `SPAWN_STALL_HALTED_SECS` (0) |

  A task queued behind another of its session, or behind the task its
  mission's gate judges next, is not escalated: the one ahead of it is. A
  landing on a source the owner holds waits behind the hold, not for the
  integrator: its `landing` step is not escalated while the hold lasts, and
  after a record ends it the step is measured from that record. The
  tasks of a halted mission that wait for the gate or a landing are the
  halt's, escalated once per halt about session `missions`
  (`stall-mission-<mission>-halted-<record>`), with the reason and the open
  tasks. A failed command (a delivery attempt, a model switch's or a
  retirement's Stop, a resume) is the evidence of the step it blocks, not a
  step of its own. A delivery whose retries are spent is re-issued or was
  escalated as `delivery failed`, which is its stall's escalation; one that
  may have typed the task is not retried, and its stall is escalated at its
  bound.
  A worker waiting on a background job of its own is active (§2.2): while
  its live execution has a job open (`job.started` without its `job.ended`)
  and its latest turn has ended, its `activity` step is not escalated, since
  the engine resumes the agent when the job reports. A job open past
  `SPAWN_STALL_JOB_SECS` (7200) is escalated once on its own, and that is the
  task's only item while the job runs. The screen's "N shells still running"
  is no rule.

  The item, heading `stalled`, is one line naming `task #<id>`, the step,
  how long it stood still, the evidence (the record it is measured from and
  its age, the latest judgement's state and whether it covers the latest
  fact, every failed attempt with its key and error, the latest stale
  verdict's reason) and what Spawn already tried and still will (each
  delivery attempt and its outcome, a retry due, or due and held and why;
  a nudge, a resume, the idle or report item already sent). Its tag is
  `<session> stall-<task>-<step>-<n>`, `n` naming the record the step is
  measured from (the control record's id; for `activity` the `session_seq`
  of the latest fact, or the delivery record's id when no fact came after
  it): `told` sends it once, and a step that moved and stalled anew has a
  new record and a new tag. A failed retry, a stale verdict, a nudge, an
  input waiting for its turn or a judgement moves no step. Every age is on the database's clock
  (`spawn_now_ms()` against the records' times); nothing is inferred from
  another loop's timing or absence; a restarted loop computes the same
  stalls and tags. The stall layer acts on nothing: Spawn's own recoveries
  (delivery retries, the resume of a lost ephemeral worker and the liveness
  rule for residents, the integrator judging a `stale` report again, the
  retire Stop) run on their own schedules, and every default bound is longer
  than their first attempt. Typing into a worker, delivering again after an
  attempt that may have typed, cancelling, reassigning, resuming a halted
  mission, stopping a resident or extending a budget each need a decision:
  the supervisor's.
- **Supervisor**: one perpetual Claude Code session (`agent_supervisor`).
  It plans missions into worker tasks and handles what the dispatcher
  escalates. Its tools are CLI commands over the log: `spawn fleet` (fleet
  state as JSON), `spawn tail <session>` (screen and recent events) and
  `spawn submit`. It never waits on a human (its charter,
  `supervise::charter`; the owner, 2026-11-06: "no human but can input").
  Whatever its grants allow it decides and does in the same turn (reviews,
  accept or send back, assigning, routing and cancelling tasks, nudging a
  stalled worker, a `continue` after a transient model API error, stopping a
  worker, proposing or retiring a resident), reporting each as a line
  "Decided (the owner may reverse): ...". What they do not allow (deploys,
  owner-protected files, money, deleting data, a `discard` included,
  changing grants or credentials: approving a resident, setting a
  mission's gate, resuming a halted mission) is one line "For the owner: ..." that it does
  not wait on; it moves on to other work. It never ends a turn on a question
  or a list of decisions for the owner. The dispatcher wakes it by typing a
  digest into its pane while it is idle; digests coalesce while it works.
  Idle is its turn stopped at its prompt, whatever Jev judged: a turn that
  ended on a question for the human (one of the old charter) is judged
  BLOCKED, and digests still reach it. A digest has one line per
  session and reason: repeated items of one kind about one worker (a human's
  notes that could not be delivered, retried) are counted on one line, the
  latest quoted, so they cannot fill the digest. Its memory is its native
  session (restored by resume) plus the log.

  **A changed charter reaches it by record.** The charter
  (`supervise::charter`) is the prompt of a new native session (the first,
  or a handoff), and that start records the charter's hash (the `Start`
  command's `charter`, FNV-1a of the text). A restarted `spawn loop`, a
  resume (after an engine exit, a crash or a reboot) and a compaction keep
  the native session, and the charter it holds. Which charter that is, is
  read from records only (`dispatch_facts`' `charter`,
  `dispatch/charter.rs`): the start of the execution that first named the
  current native session (the latest `session.native_started`'s id) gave the
  hash it recorded (none, for a start that recorded none: a person's, or
  one from before the record), and the newest charter input the dispatcher
  sent into the session since that start which reached it (an input tagged
  `charter <hash>` that succeeded, or failed after typing may have begun)
  replaced it. When that is not the hash of the charter the running build
  writes, the dispatcher types the deployed charter in at the supervisor's
  next idle prompt, through the digest's path and in that pass's digest's
  place (the digest waits for the next idle prompt, its items pending): one
  input that opens with a line saying the charter changed and that the text
  replaces, in full, the one given earlier, tagged `charter <hash>` and
  keyed `dispatch-charter-<hash>-over-<command>`, `<command>` being the
  record it replaces (the start, or the charter input before it), so a
  restarted loop submits the same key, and a charter reverted and deployed
  again is typed again over the one that replaced it. Typed, the input is
  the record of the delivery: nothing is sent again for that hash, whatever
  restarts, and a new native session started with the charter holds it from
  its start and gets nothing. An attempt refused before anything was typed
  (a pane not ready, an execution gone) told nothing: it goes again under
  `<key>-retry-<n>` at an idle prompt judged on a fact after it (its own
  `command.finished` is one), 3 attempts in all, then it is escalated (`new
  charter not typed`, a condition that holds while those records do: the
  owner types it, or a handoff gives it) and not sent again. One that may
  have typed is not typed again. No step reads the time or the pane's text.

  Residents need none of this. A resident's charter is not Spawn's code: it
  is the text the supervisor proposed and a person approved
  (`resident.proposed`), which no deploy changes and which cannot change
  once approved (a proposal for a resident that is not a proposal is
  refused). The code around it holds no rule that a running resident could
  miss: the role line (`deliver::resident_role`, given at a start) only
  names the resident and its charter, and the report contract
  (`deliver::report_contract`) is typed again with every new task. A rule
  added to the role line would need the same record.
- **Human**: the escalations queue in the UI ("Escalations", top of the
  Fleet page: what the dispatcher raised, told to the supervisor or not) is
  computed by the dispatcher's own code, not by the UI:
  `GET /api/attention` runs the plan of the dispatcher's pass for this moment
  (`dispatch::read_plan`, the function the pass itself calls: the same facts,
  mission rows, disk and gate-room records and stalls, then `decide_with`)
  and submits, marks and collects nothing. The queue shows, top to bottom:
  what the supervisor asks ("The supervisor asks"), when its turn ended at
  its prompt judged BLOCKED or WAITING with `needs_human` at least 0.5 (or
  with a `HUMAN:` line; the judgement of that turn, based on its end or
  later, never an earlier turn's, so an answered question does not come
  back when the answer's turn ends), in its own words from its transcript: its last
  `HUMAN:` line not answered since, or else its reply's last paragraph, in
  at most three lines, the whole reply on request; right under it, always
  there whether it asks or not, the box to type to the supervisor; then the
  escalations not yet told to the supervisor; then those its digests told
  it whose condition still holds (a worker's state, a stall, a path still
  low on disk, a gate still without room), with when they were told, folded
  to a one-line count while nothing is pending. News (a disk recovery, an
  approval, a human's message, host faults at the gate) is resolved by
  being told, in the plan itself (`Why::is_news`), so the digest and the
  queue agree. A managed worker between tasks is not in it, since the
  dispatcher does not escalate it. Each item carries its digest heading,
  its line, its agent and its age. This queue is the UI's only definition
  of what is escalated: the sidebar's count is its count, and an agent is
  marked escalated when it has an item in it (pending or told and
  unresolved) or is the supervisor asking a question
  (`web/src/attention.ts`, `needsHuman`). A judge's `attention` flag is its
  opinion, shown with the judgement in the agent detail.
  A "For the owner:" line is no question: a latest reply that has one
  asks nothing, whatever Jev judged, unless a `HUMAN:` question of an
  old-style turn is still open. Those lines are listed apart, below the
  queue ("For the owner", `supervisor.for_owner`: newest first, each once,
  at most 10, from the end of its transcript; a bare "For the owner:"
  heading gives each item of the list right under it, an item wrapped onto
  an indented line as one, to a blank line or other text): items to read,
  counted in no
  queue; an owner's message clears none of them, since nothing waits on
  them. Nothing waits on a person: a person reads the queue when they
  choose. Also apart, and counted in no queue: the workers working on a
  background job of their own ("On their own background jobs",
  `on_own_jobs`, §2.2), each with its jobs and their ages, so a person sees
  why a worker at its prompt is in no queue. The owner can type to the
  supervisor at any time (the box at the top of the Fleet page, its pane, or
  `spawn submit`), and to any agent (the box in its detail drawer; both
  boxes are `POST /api/sessions/<id>/input`, §5); the supervisor reads that
  as an instruction, which may reverse one of its decisions, not as the
  answer to a question. Each human action is a command with principal
  `human`; it is the event the whole system tries to make rarer.
- **Inputs to a busy session wait for its turn.** A person's or the
  supervisor's input (`spawn submit` or the web page's input box, any
  principal but `spawn-dispatch`, `spawn-integrate` and `spawn-liveness`)
  that reaches a pane not ready is
  not refused: the Bridge records it `waiting` (`command.step`, once) and
  types it on the next Ready reading, the session's waiting inputs oldest
  first, each on a Ready reading of its own, at most once. It holds no stop,
  discard or resume behind it; if the execution ends first it fails, as
  stopped or with the end's cause, and a resume does not revive it. Spawn's
  own loops keep their contract: their inputs are refused at once while the
  pane is not ready (and behind a waiting input) and they retry on their
  own, as the nudge and delivery retries above describe. The wait has no
  timeout: a stall shows on the session's task like any other, and the
  `waiting` record is no activity (it moves no stall step and does not make
  a judgement look stale).
- **Named keys never wait** (backlog P1). A key command
  (`{"type":"key","key":<name>}`, the name one of escape, up, down, left,
  right, enter, y, n) presses one key in the agent's pane: from the page's
  terminal panel once a click has armed it (P11), or `spawn submit`. It is its own
  action, never text, so it never waits for the turn: Esc exists to
  interrupt a busy turn and the others answer a dialog, the very states in
  which text waits. The Bridge presses it at once, in any pane state, so
  it may go ahead of an input that waits, and a key behind a waiting input
  is claimed at once like a stop. Handling is serial and an input's typing
  is one call, so a key lands before or after a typing, never between its
  paste and its Enter. At most once: `command.step` `pressing` before the
  key goes, `pressed` after; a key with `pressing` and no `pressed` is not
  sent again. A key's steps are not `typing`/`typed`: those mark the input
  typed last, which every later input waits behind until a turn answers
  it, and an Esc starts no turn. A key for a session that runs no
  execution fails with the reason ("runs no execution: a key needs a
  running engine"); the page's path refuses it before recording anything
  (409). The trace and the agent page show it as `You → key · Esc`
  (`Supervisor → key · ↓` for another sender), with "ahead of N waiting
  inputs" when it went ahead of some.
- **Authority** comes from grants and DB roles. The dispatcher and the
  supervisor submit as their own principals and only for worker sessions.
  Neither can write observations or runtime events.
- **The integrator** (`spawn integrate`) lands work: it judges ready tasks
  with the mission's gate and lands them host-side (§10.3). RSI is a mission
  like any other, not a separate controller. One integrator holds a
  database's lock for its life (objective 221). Each start that takes it
  records who it is, on the lock's connection before anything else
  (objective 234): one `integrator.started` {pid, ppid, parent_comm,
  ancestors [{pid, comm}] up to pid 1 (at most 16), cwd, cgroup (the unified
  cgroup's path: the unit or scope it runs in), gate, gate_from_env, root,
  once}, and the same on its stdout, so a stray is named from the store. A
  record the store refuses is said on stderr and the start goes on; a start
  the gate check or the lock refuses records nothing.

### 3.1 The supervisor session never ends

The supervisor is one Spawn session, `session_supervisor`, for the life of
the system. Its continuity is a design requirement, not a best effort:

| Threat | What keeps the session |
| --- | --- |
| `spawn loop` restarts or is replaced | The engine runs inside the detached Herdr daemon (`spawn-hc` / legacy `herdr`) outside the loop's lifetime (`KillMode=process`); the next loop recovers the execution from the log and keeps observing it |
| The engine process exits or crashes | The **recovery** (control layer, facts only, no model): the end is an `exited` or `crashed` interruption — a signal, a non-zero status or a terminal gone while its server lives — recorded once and `resume`d in the same native session at once, then after 10, 20, 40, 80 … s as resumes fail, capped at 640 s. A resume whose execution ends within a minute before any progress failed; three in a row hand off |
| The machine reboots | The Herdr daemon is gone, so every terminal in it died: the recovery records `host_restart` and resumes each execution by its key, within one pass of the loop's start |
| The model API is overloaded, rate limited or unreachable, or the login expired | No execution ends: Claude Code writes its API error line and waits at its prompt. The recovery records `model_unavailable` or `model_auth` as the interruption and `wait`s: a retry after 60 s doubling to 4 minutes (or at the time a 429's text names), a probe of the credentials file every 10 s and one retry every 30 minutes while it does not change. The wait clears on the model's answer or the changed file, and the same native session goes on. An engine that ends after the error is resumed after the backoff at most `SPAWN_RECOVER_MODEL_RETRIES` (3) times in a row, then the supervisor is told; one that ends after a request the model will never accept (`model_error`) is never revived, and the supervisor is told |
| The context window is spent | `Prompt is too long` in an API error line: `context_exhausted`, and the handoff path below |
| The engine's resource envelope kills it, or the disk fills | `memory_limit` (objective 029) resumes at once; `disk_full` waits until a reading of the workspace's filesystem is at or above `SPAWN_DISK_FLOOR_GB` |
| Spawn's database is unreachable | The loop does not exit: only passes that could not reach it are not failed ones, the engines run on, and at the first pass that gets through the recovery records `database_unavailable` for each execution that lived through it, clears it, and the Bridge checkpoints what the engines wrote meanwhile |
| A transcript Claude Code deleted | The resume puts it back from the transcript archive (`<root>/.transcripts`, copied on each pass it grew and at execution ends; docs/storage.md, "The transcript archive"); one gone with no copy fails the resume instead of starting blank under the old id |
| The native session cannot be resumed (transcript lost with no copy, resume fails three times, or there is none) | `start` a fresh native session inside the same Spawn session, on the same workspace and branch, with a handoff prompt rebuilt from the log: mission, open worker tasks, recent escalations and decisions, the interruption and the branch head |
| The context window fills | Claude Code compacts on its own; the handoff path above is the fallback when it cannot |
| A deploy changes its charter | Each new native session's start records the hash of the charter it gives; when the native session that runs holds another (after a loop restart, a resume or a compaction), the dispatcher types the new charter in at its next idle prompt, once, by record (§3, the supervisor) |
| The supervisor hangs or loops | Its own observations (rules, then Jev) flag `stuck`; that goes to a human, never to an automatic stop |

Rules for the session itself:

- Nothing automatic ever stops it. Only a human `stop` command does, and the
  liveness rule treats any other end as an outage to repair.
- Its durable memory is the log, not its context window. Anything it must
  not forget (mission, decisions, open escalations) is either a command it
  submitted or an event about it, so a handoff can always be rebuilt.
- Its uptime, restarts, resumes and handoffs are first-class facts in the UI.

### 3.2 Missions, tasks and workers

Spawn works for missions, not repositories. A repository is just one possible
input of a mission.

**Distribution is layered, and every routing decision is the supervisor's.**

| Layer | Who | Does | Never does |
| --- | --- | --- | --- |
| Intake | human (or a standing source) | puts a mission in the inbox: `spawn mission add`, typing to the supervisor, answering a worker | bypass the record: every human input is a stored command with principal `human` |
| Decision | supervisor | accepts a mission, splits it into tasks, routes each task to a resident or a fresh ephemeral worker, accepts or rejects reports, closes the mission | type into panes itself, but for a nudge or a `continue` after a transient model API error |
| Delivery | dispatcher (rules) | delivers assigned tasks, one at a time per worker (a task typed into a live worker is preceded by a merge of `main` into its idle workspace, and its text says what the merge did: [storage.md](storage.md), "Main into a workspace"; a task is delivered when the command that carries it succeeds; a failed one, which typed nothing, is retried under `<key>-retry-<n>` with a doubling wait (`SPAWN_DELIVERY_RETRY_SECS`, 60 s first), 3 times; then a first delivery is re-issued (cancelled and assigned again, once) and any other is escalated, and after each deploy every spent delivery is tried once more (§3, "Deliveries that never reached their worker"); a refusal because the pane was not ready yet (`does not accept input`, refused before typing) is no failed attempt and goes again after `SPAWN_DELIVERY_NOT_READY_MS` (5 s), counted only past 10 of them; one that may have typed the task is escalated at once and never re-issued; a delivery that never reached the worker is not its last delivery, so its retry is not held back by itself); resumes, nudges, collects reports, wakes the supervisor with digests | choose who does what |
| Execution | workers | do one task, end with a report | take work from anywhere but their assignment |

**Two kinds of worker.**

| | Resident | Ephemeral |
| --- | --- | --- |
| Exists for | a lasting concern tied to the architecture (a domain or role with a charter) | one task |
| Started | proposed by the supervisor with a charter (`spawn resident propose`); runs only after a human approves it (`spawn resident approve`), then kept | by the supervisor, on demand, per task |
| Context | accumulates knowledge of its domain; one task at a time; handoff from the log when it grows stale; its charter is given at every start and handoff | a self-contained brief and nothing else |
| Liveness | resumed after outages like the supervisor; a stop by the supervisor or a human is respected (killable) | resumed once at most; ends when its report is accepted |
| Workspace | its own, long-lived | its own, per task |

**Context cohesion: the rules that decide routing and protect every context.**

1. One session, one concern. A task goes to a resident only when it falls
   inside that resident's charter; anything else gets an ephemeral worker.
2. Serial inside a session. A resident starts its next task only after it has
   reported the last one; its queue waits in storage, not in its context. A
   task still waiting there may move, unstarted, to an idle resident of the
   same pool ("Work stealing" below); a started one never moves.
3. Briefs, not histories. A worker receives a self-contained brief (goal,
   inputs, done criteria, report format), never the supervisor's conversation.
4. Reports, not transcripts. A worker ends by writing `.spawn/report.md`
   (outcome, what changed, open issues); the dispatcher stores it with the
   task and digests it. The brief tells it to write the file once, at the
   end, and to draft anywhere else: a report that is still a draft (a
   template placeholder such as `PENDING_RESULTS`, a line that is only
   `(pending)`, `TBD` or `TODO`, an Outcome that says draft or in progress)
   is not taken. The task stays delivered, the worker is told once per
   content of the file which markers are on which lines, and the check is
   recorded (`task.report_unfinished`, §10.1). The supervisor reads reports
   and judgements, and opens a transcript (`tail`) only when it must. Its
   own context stays about the mission.
5. Isolated workspaces. Every worker has its own; combining results is a task
   the supervisor decides on, not a side effect.
6. Human input is context too. A human may type to a worker directly. The
   message is delivered and recorded, and the supervisor gets a one-line
   summary of it in its next digest.

**Commands.** `spawn mission add --text ... [--source <repo>]` (a human),
`spawn task assign --mission N --kind resident|ephemeral --worker <name>
--brief <text> [--model default|cheap] [--pin]` (the supervisor; checked like the
gateway; the model profile is §3.3; `--pin` keeps the task with its worker,
"Work stealing"), `spawn task
accept` /
`reject`, `spawn task steal` (the owner, "Work stealing"), `spawn resident
propose [--pool P]` / `approve` / `retire`, `spawn mission
close`. Every one is a checked SQL function that appends to the control
stream (§10.1).

**When a task closes.** A task closes by landing (or, without a gate,
acceptance), or by a cancel: `spawn task cancel`, the supervisor's, the
integrator's overrun over budget (`task.over_budget`, whose work the
objective's next task continues, §10.4), a gate rejection the closure rule
turns into a cancel, or a mission closed or a resident retired under it
(§10). A cancelled task's live work ends with it, at the source:

- *Stopped.* When a delivered task is cancelled and it was its worker's
  latest delivery, the dispatcher stops the worker at once, in the middle of
  a turn or idle (a Stop takes a final checkpoint, which keeps what it did).
  A resident is stopped once for the task, not once per execution: a
  cancel's stop (`cancel-stop-<task>-<execution>`) or a reassign's
  (`reassign-<task>-stop`) that did not fail counts, so a resident revived
  since is not stopped again (GitHub #76). A stop that failed stopped
  nothing and is pursued (`stop-<execution>-retry-<n>`). An ephemeral
  worker is stopped and nothing more; a retired resident is stopped and not
  told. The recovery takes a cancel's stop, the first or only its retry, as
  an interrupt, not an end: the resident is revived (resumed, or handed off
  to a new native session when there is none to resume or after three
  failed resumes) and told not to continue the cancelled task and to wait
  for the notice.
- *Set aside.* Before a resident gets anything else, the task's work leaves
  its branch's tree: a new commit on top of the branch head, whose tree is
  the checkpoint the task started from ([storage.md](storage.md), "Work set
  aside"). Nothing is rewound or lost, and the next task's candidate holds
  none of it, so nothing rides into another objective's landing. For a
  resident whose engine runs, this happens only while it is idle at its
  prompt, asked again on the facts read now, under the branch's lock, as
  main is merged before a typed delivery: one found busy keeps its
  workspace as it is, and its notice waits for a later pass.
- *Told.* The notice names the task, says it is cancelled, and says where
  its work is (the commit that holds it). A resident whose engine no longer
  runs is resumed with it. One that runs again since its stop gets it typed
  in at its prompt, only when its turn stopped idle on its latest fact,
  never into a busy pane. Its next task waits until a notice is recorded:
  any recorded `cancel-notice-<task>-<n>` command counts as told, whatever
  its outcome (FM-18), so a notice is not sent again once one is recorded.
  A notice's key is the next number after every one already sent, however
  long ago (a reused key would be taken as already sent, and nothing would
  go).
- *Never collected.* A cancelled task's report is not collected, recorded
  or judged; a report file it leaves is not the next task's (the delivery
  mark rule).

A task never delivered changes nothing for its worker, and a resident whose
cancelled task is not its latest delivery is left alone. The dispatcher
keeps no state for this either: `spawn_events`, `spawn_commands` and their
keys (`cancel-stop-<task>-<execution>`, `reassign-<task>-stop`,
`stop-<execution>-retry-<n>`, `cancel-notice-<task>-<n>`,
`set-aside:<task>`) say what was done.

**Restart and reassign.** A worker that stalls (busy, no fact for
`SPAWN_STALL_ACTIVITY_SECS`, escalated under "stalled") is given a fresh
execution, or its task a fresh worker, without losing its work, by the
owner or the supervisor (`spawn task restart --id N [--reason <text>]`,
`spawn task reassign --id N [--reason <text>]`, the store's
`spawn_task_restart` and `spawn_task_reassign`), or by a person from the
agent page (`POST /api/sessions/<id>/restart` and `/reassign`, `by` `human`,
through `spawn_human_task`); a stuck worker's `agent_stuck` alert names its
`spawn task restart --id N` ("Alerts"). Each takes a `delivered` task only, refuses any
other naming its state (nothing recorded or sent), and prints `{task,
session_id, agent_id}`: the task and the worker that now carry the work.

- *Restart.* `task.restarted` {task, session_id, execution_id (the running
  one, if any), by, reason, n (from 1, per task)}. The dispatcher stops that
  execution (a Stop keyed `restart-<task>-<n>-stop`: its final checkpoint
  is kept), the recovery records its end `restarted` `final`, and the
  dispatcher then resumes the same native session on the same workspace,
  once (`restart-<task>-<n>`), with a prompt naming the restart and its
  reason. A worker that no longer ran is resumed at once. `GET
  /api/sessions/<id>` names, on the execution the resume started, the one it
  replaced (`restart_of`).
- *Reassign.* In one transaction, as the re-issue: the old task's
  `task.cancelled` names `by`, `reason` and `reassigned_as`; the new task's
  `task.assigned` names `reassign_of` and copies its mission, brief, model,
  objective, scenario and `created_by`, kind `ephemeral`, with a fresh
  worker (`agent_worker_<label>_<new id>`). The old worker is stopped under
  `reassign-<task>-stop` (not the routine retire), its end recorded
  `reassigned` `final` with evidence `to_session`, never revived. The new
  task is delivered once the old worker has ended, its branch starting from
  the old worker's last checkpoint (not a set-aside), as an objective's
  fork does; a mission without a source names that commit as the Start's
  `from`. A Stop of the old worker that fails is pursued and escalated
  (`dispatch::stops`, its line naming the reassign); once escalated, the
  new task goes without waiting further, from the last recorded checkpoint.
  `GET /api/sessions/<id>` links the two: `reassigned_to` on the old
  session, `reassigned_from` on the new, each `{task, session_id}`.

**Work stealing (objective 239).** Each resident's pending queue is its
tasks in `assigned` that delivery does not give it in this pass: `Work::blocked`
holds its session (a task delivered, under review, accepted and waiting for
the gate, landing, or a cancel notice not yet told), or another of its tasks
has the turn (`next_up`: a revision first, then the oldest). The queue stays
where it is today, in `mission_tasks`, folded from the control stream; there
is no other queue. A task in it may move, still unstarted, to another
resident that has nothing to do, so the waiting worker does not finish it
serially while a peer sits idle. What moves and what never does:

- *Pools: who may run whose task.* Context cohesion (rule 1) routes a task
  to the resident whose charter holds it, so a task may move only between
  residents a human approved as interchangeable: a resident proposed with
  `spawn resident propose --name N --charter C --pool P` (`resident.proposed`
  names `pool`; `mission_residents.pool`) is in pool P once a human approves
  it. A resident without a pool neither takes nor gives a task. The pool is
  the whole capability rule: Spawn's residents run one engine with one set of
  grants, a task carries its mission, model profile and brief with it, and
  delivery merges the mission's main into the new worker's workspace as for
  any of its tasks.
- *Stealable.* A task may move when all hold, read under the control
  stream's append lock: it is `kind` resident, in `assigned`, with
  `revisions` 0 and no delivery command (no `spawn_commands` key
  `deliver-<task>-…`: nothing was ever typed for it, by any attempt); it is
  in its worker's queue as above, never the task delivery gives that worker
  in this pass; it is not pinned (`spawn task assign --pin` records `pinned`
  true in `task.assigned`; `mission_tasks.pinned`, default false: work that
  needs its worker's context, such as a follow-up of the worker's unlanded
  candidate); and its mission is open, not paused and not halted (a task
  waiting for anything but its worker is not taken).
- *Idle.* The resident that takes it is approved, not retired, in the same
  pool as the task's worker, and holds no open task of its own (none
  `assigned`, `delivered`, `reported`, `revise`, `landing`, or accepted and
  waiting for the gate) and no cancel notice not yet told.
- *Never stolen.* A task delivered, under review, sent back, accepted,
  landing or closed stays with its worker until it ends, is cancelled, or
  follows the restart and reassign rules above. A revision always goes back
  to the worker that wrote it.
- *Who moves it, and when.* The dispatcher, in each pass, before it plans
  the pass's deliveries: for each pool, its idle residents in name order each
  take one task, the pool's oldest stealable task (lowest id, across every
  other member's queue). A resident that took a task is no longer idle, so
  one pass moves at most one task to each, a busy worker keeps the rest of
  its queue, and no idle resident drains the pool. It is local rule work: no
  model call, no judge, no digest item, no read beyond the pass's own
  `Work`. `SPAWN_WORK_STEALING=0` switches the automatic moves off (the
  owner's manual move still works); unset, they are on.
- *One owner, recorded.* A move is the store's `spawn_task_steal(task,
  resident, by)`, the dispatcher's (`by` `spawn-dispatch`) or the owner's
  (`spawn task steal --id N --to <resident> [--reason <text>]`, `by` `human`).
  Under the append lock it checks every rule above against the folded tables
  and either appends `task.stolen` {id, from_session, from_agent, to_session,
  to_agent, pool, by, reason, queued_since_ms (the task's `created_at_ms`)},
  folded onto the task's `session_id` and `agent_id`, or refuses naming the
  first rule that failed, recording nothing. The task keeps its id, brief,
  mission, model, objective and scenario; nothing is cancelled or re-issued.
  Two takers of one task: the first moves it, and the second is refused (the
  task is no longer in a busy worker's queue). A delivery that started
  first wins the same way (its command exists), and `spawn_task_delivered`
  refuses a command into a session that is not the task's worker, so a
  delivery planned before a move is never recorded for the old worker. A
  restarted loop reads the owner from the folded record and moves nothing
  twice; a refold of the log gives the same owner.
- *Seen.* `spawn task stages --id N` shows the stage `stolen` {from, to, by}
  between `assigned` and `delivered`; `GET /api/missions` shows the task
  under its new worker.

**Storage.** Missions, tasks and the resident roster are the `mission_*`
tables, folded from the control stream (§10.1): a mission has its text,
origin, sources, state and landing settings; a task has its mission, brief,
kind, assignee, model profile, state and report.

**Mission sources.** A mission names its source: a git repository, as an
absolute path, an `https://` or `ssh://` URL, or ssh's `user@host:path`,
mirrored into `$SPAWN_WORKSPACE_ROOT/.sources/`, or none (an empty scratch
repository). Each worker's workspace is a branch of that mirror.
`SPAWN_GIT_REPO` is the repository of agents no mission owns. A source is
git run as the owner, so `spawn_mission_add` refuses any other shape (an
`ext::` command, a relative path, an option), the supervisor may name only a
source a human already gave a mission, and every git the controller runs
allows only file, https and ssh (`GIT_ALLOW_PROTOCOL`), in the C locale
(docs/storage.md, "Branching and merging").

**Limits.** None fixed. The supervisor decides fleet size; humans stop what
overreaches, and the UI shows the live workers.

### 3.3 Engine models

A Claude Code engine reaches its models through the **model relay**
(`scripts/model-relay.py`, unit `spawn-model-relay`, `127.0.0.1:8791`). The
relay routes each call by the `model` in its body: `deepseek-*` goes to
DeepSeek's Anthropic-compatible endpoint with `x-api-key:
$DEEPSEEK_API_KEY` in place of the engine's credentials; anything else goes
unchanged to the gateway of Claude Code's own settings, with the engine's own
auth header. The DeepSeek key lives only in the relay's process (from
`~/.config/spawn/model-relay.env`); the gateway token only in `~/.claude/settings.json`.
Spawn's environment, the engine's environment, the event log and the panes
hold neither.

Every task runs on one of two **profiles**:

| Profile | Main model (and sonnet, opus slots) | Small model (haiku slot: titles, summaries, background work) | For |
| --- | --- | --- | --- |
| `default` | Claude Code's own (`claude-opus-5-5` on the gateway) | `deepseek-flash` | everything not listed below |
| `cheap` | `deepseek-flash` (`SPAWN_CHEAP_MODEL`) | `deepseek-flash` (`SPAWN_SMALL_MODEL`) | simple, well-specified work: docs, mechanical edits, tests for stated behaviour |

Design, concurrency, security, debugging and anything a gate judges as hard
stay on `default`; the supervisor's charter says so.

How the profile travels:

- `spawn task assign --model cheap` records it in `task.assigned` and
  `mission_tasks.model` (default `default`).
- Delivery puts it on the `Start` or `Resume` it sends. A `Start` or
  `Resume` that names no profile (liveness revivals, the dispatcher's resume
  of a lost engine, a human's `spawn submit`) keeps the profile the session's
  last execution started with, which `execution.started` records: a resumed
  cheap worker stays cheap, and a revived resident keeps its models.
- A running engine keeps the models it started with. A task for a live
  worker on another profile stops it at the moment typing would be safe
  (`switch-model-<task>-<revision>`); the next pass resumes it, context
  kept, on the task's profile. A switch Stop that fails is retried and then
  escalated like every failed Stop (§3, the dispatcher); the task stays
  `assigned` and is never marked delivered by it.
- The Bridge passes the relay and the slots in the inline `--settings`
  `env` (`ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`,
  `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL`,
  `ANTHROPIC_SMALL_FAST_MODEL`). Claude Code merges inline settings over the
  user's files, `env` name by name (checked with 2.1.318), so the gateway
  token still comes from the user's own settings: no token is ever set
  inline.
- `cheap` pins every slot (issue #79): the top-level `model`, the main,
  sonnet, opus and subagent slots to `SPAWN_CHEAP_MODEL`, the haiku and
  small-fast slots to `SPAWN_SMALL_MODEL`, so no model the user's own
  settings name takes a slot back to the gateway. `default` pins only what
  the loop names, each optional: `SPAWN_DEFAULT_MODEL` (the top-level
  `model` and `ANTHROPIC_MODEL`, and the fallback of the three below),
  `SPAWN_SONNET_MODEL`, `SPAWN_OPUS_MODEL`, `SPAWN_SUBAGENT_MODEL`. With
  none set it is as before: Claude Code's own main model.
- `SPAWN_CLAUDE_CONFIG_DIR` (opt-in, an absolute path) runs the engines
  on a Claude Code directory of their own (the login wait and the
  `sessions` backup read it too): they get it as `CLAUDE_CONFIG_DIR`, its
  sensitive files are denied as `~/.claude`'s are, and the Bridge reads the
  transcripts from its `projects` (unless `SPAWN_CLAUDE_PROJECTS` names
  another). Its `settings.json` must then give the gateway's token, since
  the relay holds none. Unset, nothing changes.
- Each `execution.started` records what its engine was launched with, no
  secret in it (`launch`: the relay's scheme, host and port, the pinned
  main and small models, the Claude Code directory), left out for an engine
  on Claude Code's own settings in `~/.claude` (no relay, or `default`'s
  fallback when the relay is down). `spawn fleet` and `/api/fleet` show it
  for each session's latest execution.

**When the relay is down.** The Bridge connects to the relay before every
Claude start. Unreachable: a `default` engine starts on Claude Code's own
settings (the gateway, its own small model) and the tick reports a warning;
a `cheap` start fails, is retried, and the failed delivery reaches the
supervisor's digest once its retries are spent. An engine already running when the relay dies sees API errors until
systemd restarts it (`Restart=always`). Engines use the relay only where
`SPAWN_MODEL_RELAY` names it (`http://127.0.0.1:8791` in `spawn.env`):
unset or `off`, every engine runs on Claude Code's own settings, nothing
probes a relay, and a cheap start fails.

**Peer messages are refused (objective 206, issue #42).** Every engine the
Bridge starts (the supervisor, the residents, the workers; both profiles;
through the relay, without one, and on the fallback) gets
`"crossSessionInbound": "refuse"` in the same inline `--settings`
(`CROSS_SESSION_INBOUND` in `crates/spawn-core/src/bridge.rs`). All the
engines and every other Claude Code session of this user share one HOME and
one runtime directory, so each lists the others from `~/.claude/sessions`
and can `SendMessage` into one; the engines skip permission prompts, and
under Claude Code's default (mode parity) a message from another bypassing
session would be delivered with no one approving it. With `refuse` the
engine takes none, and the sender gets a "refused" receipt. The inline value
wins over the user's settings file; only managed settings override it.
Claude Code enforces it, not Spawn, and a Claude Code without the setting
ignores the key (read in 2.1.325's loader). Spawn's own input, typed into
the engine's pane, is no peer message and is unchanged. It reaches an engine
at its next start or resume. What it does not cover: an engine can still
send into sessions outside Spawn (the lead's, the reviewers', cc-astra's),
which deliver it under mode parity until each sets its own
`crossSessionInbound`; and the shared `~/.claude/sessions` records, through
which every session lists the others, stay as they are.

### 3.4 Alerts: what Spawn tells about itself

The fleet used to be watched by a monitor the supervisor ran by hand: a loop
in its pane, its state in its process. Nothing of it was recorded, a restart
re-reported what was still true and forgot what changed, and no view of Spawn
showed an alert. `spawn loop` now raises its own, from records and readings
(`crates/spawn-cli/src/alerts.rs`):

- **A record per transition.** Each alert is two control records at most:
  `alert.fired {key, kind, subject, evidence, text}` and `alert.cleared {key,
  kind, subject, evidence}`, written through the owner-only
  `spawn_alert_record` and folded into `spawn_alerts`
  ([storage.md](storage.md#alerts)). `key` names the occurrence; the store
  refuses a second `alert.fired` for it, an `alert.cleared` without its
  `alert.fired`, and a second `alert.cleared`. The owner's `spawn alert
  judge <key> right|false|stale|duplicate` records a third, `alert.judged
  {key, verdict}` (objective 092): every verdict but `right` clears an open
  alert, and the step, reading it at its next pass, treats the alert as
  cleared; `right` (objective 148, a real need) leaves it open for its
  condition to clear, and is what "Needs you is right" counts. The step reads the keys back
  on start, so a restart fires nothing twice, and two loops at once still
  record each transition once.
- **What fires.** Notices, for every mission: `task_state` (a control record
  after which the task's state differs, with `from` and `to`),
  `mission_state` (its `state`, `halted`, `gate_rejections`, `gate_aborts` or
  `gate_room_escalated` differs, with `before` and `after`; the step folds
  `gate_rejections` by the store's rule, the run's objective included and
  read back on start, so a cancel that ends the run (a yield, the owner's
  cancel) is one: "gate_rejections 3 -> 0 (by task.cancelled)"; objective
  053),
  `gate_base_disagrees` (objective 033's host fault with `alert`), and
  `command_failed` (a command that finished failed; an owner's note is not
  news). Conditions: `scenario_red` (a gate of the mission found main red
  on a scenario, its `task.gated` record's `main_red_scenarios`: solved, as
  the same scenario, on an ancestor of the base and unsolved on it, which
  scripts/rsi/gate.sh reads from the solved index beside its base cache; one
  per mission and scenario while red, cleared at the first later `accepted`
  gating of the mission that does not name it; its text names the task
  whose gate saw it and the base's first 8 hex, an observation, not a
  present fact, issue #71), `validator_defect` (a
  regression that fails on the base too, run again in the gate: from a
  task's `host_fault` with evidence `validator_defect`, one per mission and
  scenario, also cleared at the mission's next `accepted` or `rejected`
  gating; or from an owner's accepted landing whose `source.moved` names it
  in `validator_defects`, one per source and scenario (objective 056), its
  evidence naming `source`, `scenario`, `owner`, `request`, `event_id` and
  `missions`, the moved missions that are not closed (none open: no alert;
  one already open for the source and scenario: nothing new). Either kind
  clears from the record, its `cleared_evidence` naming `cause` and
  `event_id`: `passed`, a later `task.gated` of a mission it names, or
  `source.moved` of its source, lists the scenario in `passed` (run on the
  candidate and passed) or `reused` (taken from the base's cache as
  solved); `gone`, a later `task.gated` of a mission it names carries
  `scenario_names` without it (the missions on one source share a gate
  directory), or, at a reading, no gate directory of an open mission on its
  source (its own, else its missions') holds the scenario any more, every
  such directory read and none missing (`reading_ms` and `gates` instead of
  `event_id`; a mission with no gate never gates, so an old per-mission
  alert about a retired scenario clears without waiting for one); `closed`,
  every mission it names is closed (the last
  `mission.closed`; at a start, one read back whose missions all closed
  before clears then); `quarantined`, a gate names it so. A gating that
  holds the scenario and lists it in neither leaves an owner's open; Home's
  Needs you shows an owner's alerts from before objective 056, one per
  mission and no `source`, as one item per landing and scenario naming
  every mission), `task_undelivered` (assigned longer than
  `SPAWN_ALERT_UNDELIVERED_SECS`, default 300, while its agent holds no
  delivered or reported task), `mission_halted` (cleared at its
  `mission.resumed`), `node_lost` (`node <name>`, key
  `node_lost:<name>:<its node.lost event>`, raised and cleared by the
  nodes' heartbeat worker, §1.2, not by this step), `postgres_crash` (a `terminated by signal`, `No space
  left` or `could not resize` line in a database's log, cleared by the
  first `database system is ready to accept connections` line after it or by
  `SPAWN_ALERT_PG_QUIET_SECS` of silence, default 900), `memory_low`
  (MemAvailable under `SPAWN_ALERT_MEMORY_LOW_MB`, default 4096; clears at
  `SPAWN_ALERT_MEMORY_OK_MB`, default 8192), `tmpfs_high` (the used space of
  the filesystem holding `SPAWN_ALERT_TMPFS_PATH`, default `/tmp`, over
  `SPAWN_ALERT_TMPFS_HIGH_MB`, default 8000; clears under
  `SPAWN_ALERT_TMPFS_OK_MB`, default 6000; only a tmpfs is read, objective
  215: on a path whose filesystem is not a tmpfs, such as a `/tmp` on the
  root disk, nothing fires and an open one clears naming the filesystem
  type), `disk_low` (objective 204: the
  free space of the filesystem holding `SPAWN_WORKSPACE_ROOT` under
  `SPAWN_ALERT_DISK_LOW_GB` GiB, default 0, off; clears at
  `SPAWN_ALERT_DISK_OK_GB`, default 45; the live loop's unit sets 40 and 45),
  `database_size` (`database
  <name>`, objective 074: every `SPAWN_ALERT_DATABASE_SECS`, default 600, the
  live database, the gates' test database and the engines'
  (`SPAWN_TEST_DATABASE_URL`, `SPAWN_FLEET_TEST_DATABASE_URL`, each when set)
  at or above `SPAWN_ALERT_DATABASE_FLOOR_MB`, default 1024, and above
  `SPAWN_ALERT_DATABASE_RATIO`, default 10, times the live event log's
  bytes; evidence `{database, role, bytes, log_bytes, ratio, schemas}`, one
  open per database, cleared by the first reading no longer so), `source_away` (`mission #<id>`,
  at two readings in a row: a source of an open or active mission that is a
  path on this host is not a directory, an unmounted volume or a moved
  checkout, so its agents' branches are not published to it until it is
  back; one reading only notes it, so a path that comes and goes does not
  flap, and a restart starts the count again; evidence `away_since_ms`, the
  first of the two readings; one per mission
  and source while away, cleared at the first reading that finds it, or once
  the mission no longer has it or is no longer open or active; the
  supervisor's digest tells each and its end, `host source-away-<event>` and
  `host source-back-<event>`), `agent_stuck` (the session's
  latest judgement at or above the Observer's 0.5, whatever the dispatcher
  does with it, about an agent someone must act on (objective 057): none,
  whatever the score, for an ephemeral worker (`agent_worker_*`; a resident
  outlives its tasks and takes other work) whose task is past its part (`reported`,
  gated, `accepted`, `landing`, held behind a hold, `landed`: it waits for
  the gate, a reviewer, its landing or the owner), nor for the supervisor
  while it has nothing to dispatch (no open question to the owner, no
  pending or told escalation of the attention view other than a worker's
  own judgement while that worker holds its task, and no mission of its own
  waiting for its first task; tasks a worker holds, delivered or later, are
  the workers' work); a worker's alert names its `task` and its `command`,
  the one command that acts on it, `spawn task restart --id <task>` (a
  delivered task's; one sent back for revision names no command), which
  Needs you shows to copy; clears under 0.5, or when the session's current
  execution has ended, whatever the cause, with the Bridge's
  `execution.ended` as evidence, or at the task record that moved a
  worker's task past its part (its `task.reported`, `task.gated` and so on,
  that record's id its `event_id`), or, for the supervisor, at the first
  pass that finds it with nothing to dispatch (the latest record read its
  `event_id`); a session whose current execution has ended fires none; its
  evidence and text carry the observation's `ambiguity` and
  `progress_secs` when it records them; once the owner judges one
  `false`, `stale` or `duplicate`, the session fires none again until its
  stuck stretch ends: a latest judgement under 0.5, a rules judgement with
  no `stuck`, the end of its execution or another execution current, the
  hold rebuilt from the alert rows when the loop starts unless the
  session recorded one under 0.5 or a rules one with no `stuck` after the
  judgement the alert fired on (issue #80)),
  `judge_down` (the model judge's watcher recorded `host.judge` `down`, two
  calls in a row failed after its last answer, and no `up` after it,
  `SPAWN_ALERT_JUDGE_DOWN_SECS` after the first failed (default 900): no
  call answered since; one failure alone, or two more than 15 min apart,
  records nothing; clears at the
  `up`, the first answer, with `failed_calls`; it rests
  on that record, never on how few judgements a judge that is down leaves:
  an open `agent_stuck` also shows when its session was last scored),
  `admission_wait` (objective 029's record older than
  `SPAWN_ALERT_ADMISSION_SECS`, default 600, with no admission after it),
  `backup_stale` (the newest dump older than
  `SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS`, default 7200, or bundle older than
  `SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS`, default 93600; no record at all
  is no alert), `backup_budget` (objective 031's `over_alert`), `drill_failed`
  (a `backup.verified` with `ok` false), `integrator_down` (no backend
  holds the integrator's lock in `pg_locks` at every reading for
  `SPAWN_ALERT_INTEGRATOR_DOWN_SECS`, default 300, longer than a deploy's
  restart, while a gated mission that is open or active, not halted, and on
  a source no owner holds has a reported task; cleared at the first reading
  that finds the lock held; the lock is read only while such a task is in
  the step's fold or the alert is open), `controller_behind` (subject
  `mission #<id>`: the owner's latest landing on a gated mission's source
  (the latest `source.moved`'s `to`; the mission open or active, not
  halted, with a reported task) is newer than the controller its gate's
  scenarios come from (`controllers/<name>-<commit>`) at every reading for
  `SPAWN_ALERT_CONTROLLER_BEHIND_SECS`, default 1800; the integrator holds
  that mission's gates until the deploy switches the controller (P22), so a
  wait this long is a landing not deployed; a task's landing needs no
  deploy and counts for nothing; cleared at the first reading that finds
  the controller has the landing or the mission no longer waiting),
  `model_auth` (objective 028's
  `execution.interrupted` with cause `model_auth`) and `cost_budget`
  (subject `mission #<id>`, key `cost_budget:<mission>:<UTC day>`: at a
  reading, the mission's cost of the model calls timed in the current UTC
  day has reached its `budget.daily_usd` (§3.5); once per mission and day,
  whatever restarts and however much more it spends; clears when the day
  ends, or at a reading that finds the budget removed or raised above the
  day's spend), `mission_idle` (objective 058; subject `mission #<id>`,
  key `mission_idle:<mission>:<event>`: at a reading, a mission that is not
  closed, takes its work from an objective backlog (`objectives` on its
  record, `--backlog`/`--resident`) and has had no task in work (assigned,
  delivered, reported, sent back, gated, landing or held) since its last
  task ended (landed, accepted or cancelled) for
  `SPAWN_ALERT_MISSION_IDLE_SECS`, default 900; evidence `mission`,
  `since_ms` (when that task ended) and `event_id` (that record), its text
  saying for how long; never for owner work (no backlog), a closed mission
  or one with a task in work; cleared at the mission's next `task.assigned`
  (`cleared_evidence.event_id` that record) or its close; a restart reads
  the mission's last ending from the store and an open one back, firing
  nothing again; Home's Needs you shows it, and the Missions card says how
  long each idle backlog mission has had no work), `owner_request_waiting`
  (S23; subject the source, key `owner_request_waiting:<request>`: an
  owner's landing request, `source.push_requested`, that no `source.moved`
  or `source.push_refused` has named for `SPAWN_ALERT_OWNER_REQUEST_SECS`,
  default 3600; deploy-on-landing waits for such a request unless main
  holds its commit already, so this is every deploy waiting on it;
  evidence `request`, `commit`, `requested_ms` and `withdraw`, the
  `spawn mission push --withdraw <request>` that ends it; once per
  request; cleared at its outcome, which a restarted step reads from the
  store), `unverified_landings` (objective 135; subject `objectives`, key
  `unverified_landings:<newest old landing ms>:<reading ms>`: at a reading,
  objectives whose newest landing is on or after 2026-11-13 00:00 +08:00
  and whose newest `objective.verified` after it is not a pass, `spawn
  verify list`'s rule, landed `SPAWN_ALERT_UNVERIFIED_SECS` or more ago,
  default 86400; evidence `old`, `unverified`, `unverified_ms`,
  `newest_landed_ms`, `listed_since_ms` and `reading_ms`; one open at a
  time, cleared at the first reading with none old; a fail after a pass
  raises a new one at a later reading), and `message_not_taken`
  (below, "Messages"). Each fires once per
  occurrence and clears with the record or reading that cleared it; a
  threshold of `0` disables its check.
- **Only later evidence clears (objective 050).** An alert clears only on
  evidence that comes after what fired it, at the loop's first start and
  after any restart: a record-borne clear on a control record (or, for
  `agent_stuck`, an observation or an execution's end) later in the log
  than the record the alert fired on (its event id is greater); a
  reading-borne clear on a reading taken at or after the alert's own. Each
  alert's evidence names what it fired on (`event_id`, or `reading_ms` for
  a host reading, a log line or the integrator's lock), so the rule is
  checked against what the store holds, never against the step's memory,
  which a restart empties: a restarted step reads its open alerts back and
  then the last 7 days of records again, and a record older than an open
  alert's own (a resume before the halt, an older dump, a good drill before
  the failed one) clears nothing and is no error. The records a pass reads
  are recorded one at a time, so a later record in the same read (a halt
  and its resume written while the loop was down, or both in the history
  of a store the step meets for the first time) finds the alert the earlier
  one fired and clears it, naming itself. The judge's exception stands: an
  `up` whose `down` is older than the history clears its open alert (the
  `up` is the later record). The deploys' records are read apart from that
  history: a deploy restarts the loop and only then records its
  `deploy.finished` (deploy/deploy.sh), so the record is often at or below
  the new step's watermark, where no check feeds it. A restarted step
  therefore replays, one at a time, the `deploy.finished` records after the
  last one its `deploy_failed` alerts name (the newest failure one fired on
  or success one was cleared by; the owner's judgement, for one it cleared)
  up to its watermark, whatever their age: the first success after an open
  alert's failure clears it, a success older than the failure is not read,
  a failure recorded while the loop was down fires once, and one that fired
  nothing while an alert was open is no new occurrence at a restart. A
  store with no `deploy_failed` alert yet reads the last 7 days.
- **When.** A record-borne alert is recorded within one pass of its record; a
  host reading or a log line within `SPAWN_ALERT_CHECK_SECS` (default 30)
  and one pass; a bound within one pass of falling due, never before,
  measured on the database's clock from the record's `recorded_at_ms`. The
  step runs when the event watermark moved or one of its timers is due, and
  an idle pass reads the watermark once and nothing else (§14; objective
  023). Like the disk watch it never fails a pass: an error is said and
  tried at the next check.
- **The same alerts everywhere.** `spawn alerts --once [--json] [--since
  <span>]` prints every open alert and the window's transitions
  (`SPAWN_ALERT_WINDOW_SECS`, default a day); `spawn alerts --follow
  [--json]` prints each transition as it is recorded; `GET /api/alerts`
  answers the same array as `--once --json`, from the same store method;
  both carry the notice fold: each task's or mission's notice that its
  subject's next one follows says `superseded_by`, that one's key (every row
  stays; objectives 151 and 166). The Alerts view's notices list and
  `--once` in text show each subject's newest with "· N earlier". The
  dashboard shows the open alerts on every page and an Alerts view with
  their evidence and their clearing. The supervisor's charter names the
  command: its hand-run monitor retires, and the fleet's alerting is the
  record's.
- **Causality (§14).** An alert rests on a named record (its `event_id`) or
  on a recorded reading (its measure and threshold); a clearing names what
  cleared it; nothing rests on the step's memory, on absence, or on time
  without a record. Evidence never holds a secret: no database URL with its
  password, no environment, process names without their arguments.
- **Credential health & UDS isolation (`scripts/keep/credential-health.sh`).**
  Keep checks manifest health endpoints over `http(s)://` or local UDS
  (`unix://<socket>|<http-path>`, expanding `%t/` and `~/`), and enforces host-wide
  `go_unit_pg_isolation` (`subject: spawn-units`): asserting zero `spawn.env` or
  `*DATABASE_URL*` references across `spawn-node-{edge,gw,out}.service` and
  their systemd drop-ins.

#### Messages: each one tracked by its id (objective 045)

What Spawn types into an agent's engine for a command is a message: a
person's input, the dispatcher's task, a send-back or a digest, a
supervisor's nudge (an `input`, or a `start` or `resume` with a prompt). A
command that types nothing (a key, a stop, a start with no prompt) has none.
A message's id is its command's id, carried end to end:

- **The marker.** The text the Bridge types for a command ends with the
  command's marker, `[spawn message #<command id>]`, on a line of its own
  after the text (the workspace notices typed before the text are part of
  the same message; `crates/spawn-core/src/message.rs`, the Bridge's
  `type_once`). A one-line text carries it at the end of its line instead,
  after a space, whatever the pane reads: a program that reads keys raw (its
  tty has `-icanon`), unless it asked for a bracketed paste, takes each
  pasted line break as Enter, and one that reads lines submits each pasted
  line, so a marker on a line of its own would be a prompt of its own, and
  the engine could act on the text, or exit, before the Bridge's Enter
  (regression 003 under load; rev-e's analysis of 2026-11-11). The answer
  line (objective 046, below) follows the marker the same way: after a
  space on a one-line text, so the paste stays one line. Every typing carries it: an input, and a start's or a
  resume's prompt. The Bridge records `message.typed` (keyed
  `cmd_<id>:message`) once the marked text is staged and before its
  `typing` step: the record that says this command's text carried the
  marker. The order the Bridge types in, the wait between inputs, and what a
  refused or unknown typing does are as before.
- **Taken.** The engine's transcript keeps the line it was given. A user
  line of the transcript (not a tool result) that holds a marker is that
  message taken: one `message.taken {command_id}` per marker, keyed by the
  line's uuid and the id (`<uuid>:taken:<id>`), so a line read again after
  any restart records nothing, a line with two markers takes both, and an
  engine that wrote its line twice took it twice. The `message.received`
  the line is stays as it was. An input typed while the agent works is
  given to it at its next step, and Claude Code writes it only as an
  `attachment` line of type `queued_command` with `commandMode` `prompt`
  (objective 214, GitHub #66): that line takes its markers the same way
  (`<uuid>:taken:<id>`) and starts no turn (no `message.received`).
  Without it, every input to a busy worker raised `message_not_taken`
  and kept it open until the session ended. A `queued_command` of any
  other mode (a background job's notification) is the engine's own text:
  it ends its jobs and takes nothing, whatever it quotes. The marker remains
  the positive taken signal. Separately, a transcript line recorded after the
  typing advances `read_since_typed` and suppresses or clears
  `message_not_taken` (below, issue #66 Part 1), even when the line is the
  agent's own output; that movement alone does not mark the message taken or
  answered.
- **The lifecycle, from records only.** `GET /api/sessions/<id>/commands`
  gives each command a `message` (`spawn_command_message` in the schema),
  null for a command that types nothing: `{state, typed_at_ms, taken_at_ms,
  answered_at_ms, taken, not_taken}`. `typed_at_ms` is the Bridge's `typed`
  step; `taken_at_ms` the first `message.taken` naming it, `taken` how many
  name it; `answered_at_ms` the session's first `turn.stopped` or
  `turn.failed` after that first `message.taken`. `state` is `answered`,
  else `taken`, else `typed`, else `recorded`. Two messages with the same
  text are two ids, each read by its own: never by the text or the order.
  Times are the records' `recorded_at_ms`, on the database's clock. The
  agent page says each in words beside its command (`web/src/messages.ts`:
  "typed, not taken yet", "not taken (typed)", "taken in 2 s · answered in 4
  min · taken 2×").
- **Not taken.** `not_taken` is true while no `message.taken` names a message
  and it was recorded `SPAWN_MESSAGE_TAKEN_SECS` ago or more (default 120;
  whole seconds; a value that is not one is warned of once and the default
  used): typed and never taken (a pane that dropped it), or never typed (a
  pane that refused every paste). Which messages are tracked is read per
  command from the Bridge's own records, never by age or by when the schema
  was upgraded. A command whose text carried the marker has `message.typed`,
  and its bound runs from its creation. Nothing is tracked that was sent while
  its session's transcript had held no marker (no `message.taken` recorded
  at or before the command's creation): an engine, or a stand-in, that does
  not keep the prompt it is given says nothing by its silence; every message
  sent after the session's first taking is tracked. So a session's very
  first message (a worker's task) is read (taken, answered) but never said
  not taken. A command an older Spawn typed has
  its `typing` step and no `message.typed`: no `message.taken` can ever name
  it, and it is never not taken, whenever it was typed — deploy.sh stops
  only the integrator before `init-db`, so the old loop types on, without
  markers, after the upgrade until the deploy restarts it. A command never
  typed (no `typing` step) that finished failed is none either: the Bridge
  refused it before typing, its sender is told (`command_failed`), and a
  loop sends it again as a new command. Any other command never typed is
  told by when a loop that types markers first started: each loop's Bridge records `host.messages_marked` when it is
  built, and the store keeps the first alone. A never-typed command still
  open at that start or after it, or finished after it, is tracked, its
  bound running from the later of its creation and that start; one that
  finished before it was the older Spawn's to type, and is not.
- **The alert.** The alerts step raises `message_not_taken` (subject the
  session, key `message_not_taken:<command id>`, evidence the command, the
  agent, the session, whether it was typed and, typed, how many transcript
  lines the Bridge recorded since the typing, `read_since_typed`, at most
  100 counted) once per message when it becomes not taken while its session
  has not ended with it and, if typed, its transcript has not moved since the
  typing. Whenever a typed message is read, before or at its bound, a positive
  `read_since_typed` suppresses it immediately and removes it from `pending`
  (issue #66 Part 1). If that pre-due count is zero, it stays pending until its
  `due_ms` bound, when it is read again: transcript movement by then suppresses
  it, otherwise it fires. An open alert clears, in precedence
  order, when the message is taken, when a later message of its session is
  taken (evidence `later_taken`), when the session ends with it untaken (the
  execution it was typed into, or its own start's, ended; a never-typed one
  finished while no live execution ran), or when its transcript moved after
  the typing (evidence `read_since_typed`, the positive count). On start the
  step reads every message that can be one (each live session's commands from
  its execution's own start on, and every unfinished command) and its open
  alerts back from the store, so a restart fires none again and loses none;
  then, when the log moved, only the commands new since, the pending messages
  that fell due, and the open alerts whose session took a message, ended an
  execution or recorded a transcript event since (an idle pass reads next to
  nothing, objective 023). An open one reaches `/api/alerts` and Home's Needs
  you, which names the agent and the message. A typed one with no line read
  since (S17) is not said as not taken: its text and Home's title ("Spawn has
  read nothing of a transcript since a message was typed") say whether the
  agent took it is unknown, since either Spawn is not reading the transcript
  or the engine wrote nothing. Out of scope: what a reply says, retrying a
  message not taken, and Part 2's Bridge typing/idle wait changes.
- **Acted: the agent's own answer (objective 046).** Answered says the turn
  a message started ended, not that the agent did what it asked. The agent
  says which: `spawn ack <command id> done|declined [<why>]`, run in its
  own pane (a why is required for `declined`: the words after the outcome;
  optional for `done`). The marker is followed by one line that tells it
  so (after a space, on the same line, for a text of one line;
  `message.rs`'s `answer_line`: `spawn ack <id> done`, or `spawn ack
  <id> declined <why>`, and that a task's delivery is answered by its
  report). The pane has no database login, and the ack needs none: it is
  a file, written whole (hidden, then renamed) into the workspace the
  pane was launched in, `$SPAWN_WORKSPACE/.spawn/acks/` (the Bridge sets
  `SPAWN_WORKSPACE` beside `SPAWN_AGENT_ID`, `SPAWN_SESSION_ID` and
  `SPAWN_EXECUTION_ID`; never the shell's current directory, so an ack
  from a subdirectory lands there too; `.spawn/` is in no checkpoint).
  `spawn ack` exits 0 once the file is in place, and non-zero, saying why
  and writing nothing, for a missing or non-numeric id, an outcome other
  than done or declined, a decline with no why, or outside an agent's pane
  (no `SPAWN_WORKSPACE`, or one that is not the agent's workspace under the
  loop's root). On every dispatcher pass the loop reads each agent's acks
  from its workspace the safe way (`agent_files`: no symlink followed),
  oldest first (`crates/spawn-cli/src/ack.rs`), records each and removes
  it; an ack made while the loop is down waits there until it is back.
  Whose ack it is, is the workspace's: it counts only for a message into
  that agent's session, never by an id it carries, so setting
  `SPAWN_SESSION_ID` to another worker's session, or writing an ack file by
  hand, answers nothing of another agent's. An ack of another session's
  message, an unknown id, a command that types nothing, or a task's
  delivery records nothing, and the loop's log says so. Recorded:
  `message.acted {command_id, outcome, why}` in the message's session,
  keyed `<id>:acted`, so the first answer stands and a later one records
  nothing, whatever restarts (`crates/spawn-postgres/src/acks.rs`).
- **A report answers its delivery.** A task's report answers the message
  that delivered it (the command the task's latest `task.delivered` before
  the report names): an Outcome of done is `done`; blocked or failed is
  `declined`, the Outcome line its why. It is recorded from the
  `task.reported` record, once (the same key), on the next pass after the
  report is taken; a loop's first pass reads every report, so one taken
  just before a stop is answered once it is back. Only a tracked message
  (the Bridge typed its marker) is answered.
- **Read.** The commands API's `message` gains `acted` (`{outcome, why}`,
  null until answered; `why` null for a done with none) and `acted_at_ms`;
  `state` is `acted` once answered, whatever else. The agent page appends
  it to what it says of the message: " · done", " · declined: <why>" (a why
  over 80 characters cut to its first 79 and "…"). A message never
  answered is no alert. The lifecycle above, its alert, the report's rules
  and the dispatcher's order are unchanged; `message.acted` is no fact
  about the engine for the dispatcher's judgements, as the storage records
  are not.

### 3.5 Cost: what the model calls cost

The engine writes each model call's usage to its transcript; Spawn records
it per execution and prices it (objective 041;
`crates/spawn-cli/src/cost.rs`):

- **Recorded per execution.** The Bridge appends a `model.usage` event for
  each assistant line of an execution's transcript that carries
  `message.id`, `message.usage` and a `timestamp` (`ingest::claude_usage`),
  keyed by the message id and its four counts, so the lines Claude Code
  writes per content block append one event. The loop's cost step folds
  them into `spawn_cost_messages` (`spawn_cost_record`, owner only): a
  message counts once per id, with the largest usage of its lines, at its
  earliest line's time, under its model (input, output, cache creation as
  `cache_write`, cache read), for the execution whose transcript held it,
  its agent, the task the agent held then (its latest task assigned at or
  before the message's time; a message timed before any, the latest
  assigned at or before the execution started; none: no task, as the
  supervisor's) and that task's mission. A restarted loop counts no message
  twice and loses none: the rows are the record, and the fold reads the
  events again from ten minutes before the newest it folded.
- **Counted from the archive too** (objective 048,
  `crates/spawn-cli/src/cost/archive.rs`). An execution the Bridge read
  before it emitted usage events (every one that ended before 041), or
  whose events are gone, is in the transcript archive and in no cost. Each
  time `spawn loop` starts, a thread of its own, with its own connection and
  never holding up a pass, reads the archive alone
  (`<root>/.transcripts/<agent>/<native>.jsonl` and its copies
  `<native>.<n>.jsonl`; never the live transcripts, never the event log)
  and counts every assistant message the record lacks by the same rules
  (once per id across every line and copy of the agent, read as the Bridge
  reads a line), for the agent whose directory holds it, that agent's
  execution running at the message's time (the latest started at or before
  it; one timed before all of them, the agent's first), the task the agent
  held then by the rule above, and that task's mission, priced when
  recorded (`spawn_cost_archive_record`, owner only, in batches of 500). A
  message the record holds keeps what it holds; a message of an agent with
  no recorded execution is not counted, and the loop's log says how many it
  skipped. `spawn_cost_archive_files` marks each file counted with its
  length and modification time: an agent whose files are all as marked is
  not read again, one with any file changed is read whole. A file that
  cannot be read, or a line that is not JSON, is skipped; an agent whose
  count fails is logged and counted again at the next start.
- **Priced when recorded.** `SPAWN_MODEL_PRICES` is a JSON object from model
  name to `{"input", "output", "cache_write", "cache_read"}`, US dollars per
  million tokens, each a number not below zero; a message costs each count
  times its rate over a million, in exact decimals, and keeps that cost
  (and the rates it was priced at) when the prices change later. A model the
  setting does not price counts its tokens and no cost (`usd` null; its
  tokens are unpriced, never $0). Unset, a built-in table prices Anthropic's
  first-party list prices of 2026-11 (input, output, cache write, cache read
  per million): `claude-fable-5-2` 8, 40, 10, 0.20; `claude-opus-5-5` 4,
  20, 5, 0.20; `claude-sonnet-5-5` 2, 10, 2.50, 0.10; `claude-opus-4-8`,
  `-4-7`, `-4-6` 5, 25, 6.25, 0.50; `claude-sonnet-4-6`, `-4-5` 3, 15,
  3.75, 0.30; `claude-haiku-4-5` 1, 5, 1.25, 0.10. Set, the setting replaces
  the table whole. A value that is not such an object stops `spawn loop`
  and `spawn serve` at start, naming the setting.
- **Served and shown.** `GET /api/cost?window=24h|7d` (24h when absent; any
  other window 400) answers the messages timed in [now minus the window,
  now) on the database's clock: `{window, since_ms, until_ms, total, models,
  missions, tasks, agents, executions, counted_since_ms}` (the last: the
  time of the earliest message the record holds, null when none; the page
  says "Counted since <YYYY-MM-DD HH:MM> UTC" on a window that starts
  before it), each with its `usd` (the exact sum;
  0 for an entry of unpriced use only, null for an unpriced model), its
  `tokens` and its `unpriced_tokens`. The dashboard's Cost page (sidebar)
  shows the total and each mission, task and agent over 24 h and 7 d
  (`web/src/cost.ts`): to the cent, a cost that is not zero never as $0.00
  ("<$0.01"), use only on an unpriced model as "unpriced", and any unpriced
  use marked.
- **A daily budget.** `spawn mission configure --id <mission> --daily-usd
  <amount>` records `budget.daily_usd` (`--no-daily-usd` clears it); the
  alerts step raises `cost_budget` once the mission's spend of the UTC day
  reaches it (§3.4).

### 3.6 Engines beyond Claude Code: Herdr's second witness and agy (issue #91 P2)

The Bridge stays the single state machine of every execution. What an engine
does comes from its transcript, which the Bridge follows, and from its pane,
which the engine's driver reads (`AgentDriver::read_screen`). On the Herdr
terminal host (the only supported backend: unset or `SPAWN_TERMINAL_BACKEND=herdr`,
defaulting to embedded `spawn-hc` via `SPAWN_HERDR_MODE=core`, or `legacy` for external `herdr`; `tmux` has been removed and is explicitly refused) a second witness is available: Herdr
detects the agent a pane runs and reads its state with rules of its own (its
agent manifests; in `spawn-hc` mode, `herdr_witness` provides the host-side reading).
That reading is a **second witness** (`TerminalHost::agent`, an `AgentReading`). It never takes over. A test host
without one (the in-memory host) answers `None`, and nothing changes:
Claude Code without witness readings records the same events, byte for byte. A test pins the
whole sequence a2f1ab397 recorded
(`claude_on_a_host_without_agent_readings_records_what_it_did_before`).

For now the witness is asked only for engines other than Claude Code (agy).
Claude Code's screen-reader screens have not yet been captured under Herdr
against its claude manifest (design §9.2). Until they are, Claude Code on
Herdr goes by its own readings: no merge, no keyed screen, and no
`agent()` call (`claude_on_an_agent_host_stays_on_its_own_readings_for_now`).

**The merge** (`Bridge::follow_screen`, `merge_witness`). Normally a screen
state is recorded once two readings of the pane in a row agree. With Herdr's
reading (H) beside the driver's (S), one capture is enough where the two
agree:

| H | S | Recorded |
| --- | --- | --- |
| `working` | Busy, or nothing it knows | Busy |
| `blocked` by a known rule with a visible dialog (`visible_blocker`) | Permission, or nothing it knows | Permission |
| `idle` (Herdr's `done` is idle too) | Ready or Error: a positive reading | S's state |
| `idle` by Herdr's fallback rule | Unknown (a popup, an announcement) | nothing: never Ready |
| `unknown`, or any disagreement | anything | S's own readings, two in a row |
| no agent (`agent: None`) | anything | S's own readings, and the exit status is read next pass |
| another agent than the driver's | anything | nothing (never `screen.observed`) |

A screen recorded on H's word is keyed
`<execution>:hs:<server pid>.<server start ms>:<seq>` (Herdr's
`state_change_seq`), so the same reading is recorded once, by a restarted
Bridge too. A Busy screen recorded again after `screen_refresh` is the same
reading, so it goes unkeyed, and the log's unique keys never swallow it.

**The Herdr host's agent surface** (`crates/spawn-host/src/herdr.rs`):

- `agent()` calls `agent.get` on the terminal's pane. It calls
  `agent.explain` (3 to 7 KB an answer) only when the status changed or is
  `blocked`, never on every pass. A pane Herdr knows no agent in
  (`agent_not_found`) is a reading of no agent.
- A thread subscribes (`events.subscribe`) to `pane.agent_detected` and to
  `pane.agent_status_changed` for each followed pane. It queues their
  terminals for `agent_changes()`, which the Bridge drains once per pass, and
  reads those panes on that pass. When the connection drops, the thread
  connects again, asks `session.snapshot`, and counts every followed terminal
  as changed.
- For the first seconds after `agent.start` (about 3.4 s observed), Herdr
  reports `launch_pending: true` with status `unknown`, and its agent input
  calls answer `agent_not_ready`. While `agent.get` says so, staging text is
  refused before anything is typed, as `agent_not_ready`, and so is staging
  when `agent.get` itself fails with `agent_not_ready` or `agent_blocked`.
  Both are refusals the Bridge retries. In practice `agent.get` returns
  neither code: they come from Herdr's agent input calls, which PR 2 does not
  use. A `blocked` status is not refused by the host; the Bridge's own check
  guards it, since it types only into a pane its own reading says accepts
  input.
  Within 1 s of a detection or a release push, a reading still
  `launch_pending` says nothing. Typing itself stays chunked `pane.send_text`
  plus Enter. The `agent.prompt` rollout is PR 3.
- The server is started with `[update] version_check = false`,
  `manifest_check = false` (the manifests Spawn was checked against stay
  until the owner moves them; agy's is 2026.10.05.1) and
  `resume_agents_on_restore = false`. The host checks `ping`'s protocol
  (22, herdr 0.9.5) and refuses another, naming both.
- The first time a host meets a server (pid and start time), it closes every
  `spawn-…` workspace there that none of its records names. Those are what a
  restarted server restored: idle shells whose sessions went with the old
  server.

**agy** (`AgyDriver`, the Antigravity CLI). It is registered beside Claude
Code, and it runs only where the host enables it: `SPAWN_ENGINES` (a comma
list, `claude` when unset) must name it, `SPAWN_AGY_MODEL` must name its model,
and the terminal host must read agents. A start that fails any of these is
refused before launch, saying which (`engine agy needs the herdr terminal
backend`). `SPAWN_AGY_PROGRAM` (default `agy`) and `SPAWN_AGY_HOME` (default
`~/.gemini/antigravity-cli`) say where it lives, and its home is added to the
transcript archive's mask roots.

- *Launch.* The runner's launch script starts agy, never `agent.start`, with
  `--model <label> --dangerously-skip-permissions --settings
  '{"trustedWorkspaces":["<cwd>"],"toolPermission":"always-proceed"}'`. The
  model is always named: agy falls back silently to its costliest default
  model when the label is unknown or missing. A resume adds `--conversation
  <id>` and repeats every flag. Every start gets a fresh workspace and pane:
  an agy killed with SIGKILL leaves bracketed paste on, and a reused pane
  could no longer be typed into.
- *Session binding* (`SessionBinding::Reported`). agy names its own
  conversation. Its execution's transcript is not read until the binding is
  made: the session's last native session belongs to an earlier execution. The
  Bridge binds the session when all of these hold:
  - Herdr reports a session for the execution's own pane (`agent_session`,
    an id), or, when it reports none (inside the filesystem view, §3.9), a
    conversation directory below `<agy home>/brain` names it;
  - the reading's agent is the driver's (`agy`; no agent at all allows only
    the scan);
  - the session's full transcript exists below agy's home;
  - a `USER_INPUT` line of that transcript holds the marker of a command
    whose `typing` step this session recorded, and that command is this
    execution's start or later.

  Any client of the server can call `pane.report_agent_session`. The marker
  check means a forged report, whose transcript quotes another session's
  marker or one never typed, is never bound.

  Then it records `session.native_started` and reads the transcript from
  offset 0.
- *Transcript.* The Bridge reads
  `<home>/brain/<id>/.system_generated/logs/transcript_full.jsonl`, never
  the short `transcript.jsonl`, which cuts `content` at about 4 KB
  (`truncated_fields`). `agy_line` emits only existing event kinds, keyed
  `<conversation>:<step>:…`: `message.received` and `message.taken`,
  `model.usage`, `tool.started` and `tool.completed` (results pair with calls
  in order, since agy gives no call ids; a failed tool, `ERROR` with `error`,
  completes too), `job.started` and `job.ended`, and `turn.stopped`. Steps
  may be missing: an interrupt (Esc) writes no line.
- *Open work.* A background job opens with a `GENERIC` `RUNNING` step naming
  `task id: <conv>/task-<n>` (agy never updates that step). It closes with a
  later `SYSTEM_MESSAGE` from `sender=<conv>/task-<n>`. A subagent opens with
  a `GENERIC` step written `DONE` at once (`Created the following subagent:`
  with its `conversationId`). It closes with a `SYSTEM_MESSAGE` from its
  `sender=`. A `SYSTEM_MESSAGE` with no sender (a reminder) closes nothing.
  While any job or subagent is open, no `turn.stopped` is emitted, whatever
  Herdr says: Herdr was seen reporting `idle` or `done` mid-turn three times.
  The screen agrees. While background work runs, agy 1.5.14 draws a task
  panel (`●` entries) and a closing rule between the prompt box and its
  footer. The footer then counts the open work, as in `? for shortcuts
  auto · Gemini 3.8 Flash · low · 1 subagent(s) · 2 task(s) · /tasks`, and
  reads Busy. The footer is the pane's last line, below the box and any
  panel. A footer whose counts are 0 or missing reads Ready.
- *Screen* (`read_agy_screen`). Dialogs (a permission, `↑/↓ Navigate · tab
  Amend …`, or a question, `… esc Skip`) read Permission. The trust dialog
  reads Unknown, and the Bridge answers it (`is_agy_trust_dialog`, Enter). A
  running turn reads Busy (`esc to cancel` under the prompt box, or a braille
  spinner). The prompt box (`>` between rules, `? for shortcuts`) reads
  Ready, with any banner above it. agy's startup banner names the signed-in
  account and plan, so every line holding an e-mail address is dropped from
  evidence and from excerpts.

**Sandbox and deploy.** The gate's `bwrap` sandbox binds `spawn-hc` or legacy `herdr` from the host when present (`SPAWN_REQUIRE_HERDR=1`).
Running agy in the fleet needs, on the host:
- embedded `spawn-hc` (default `SPAWN_HERDR_MODE=core`, with host-side `herdr_witness`) or legacy `herdr` 0.9.5 (protocol 22, `SPAWN_HERDR_MODE=legacy`) with its antigravity-cli integration installed;
- agy logged in;
- `SPAWN_TERMINAL_BACKEND=herdr` (or unset; `tmux` is removed and explicitly refused), `SPAWN_ENGINES=claude,agy` and
  `SPAWN_AGY_MODEL` in the loop's environment.

The SQL gateway's own command checks are unchanged by this PR.

### 3.7 Zero-human answers and lifecycle hardening on Herdr (issue #91, PR 3)

**The resolver.** `spawn loop` runs `crates/spawn-cli/src/resolve.rs` each
pass. It answers a dialog an engine shows at run time from a fixed table
(`spawn_core::driver::table_answer`), as `key` commands of its own
principal, `spawn-resolve`. That principal is one of `LOOP_PRINCIPALS`; a
key never waits for a turn, so listing it changes no input rule. The
resolver uses the approvals page's detection and dialog id
(`serve/approvals.rs`, `detect`), so a person and the resolver see one
dialog under one id.

The table never holds an answer nobody has seen work. Today one row
answers, with one key: agy's `trust_prompt` for its own workspace, cursor on
trust, Enter (probed). Every other row escalates:

- Claude Code's numbered permission: its only screens are the approvals
  page's hand-written fixtures;
- Claude Code's trust dialog: the Bridge answers it at startup, and the
  resolver leaves it to a person later;
- agy's permission and question prompts, Claude Code's question forms,
  Codex, OpenCode, and any dialog no row covers.

So does a hard block (`spawn_core::driver::hard_block`): a login, a quota,
a rate limit, a needed update, or exit-only choices.

Before a key is submitted, every guard must hold:

- two equal pane reads in a row;
- for an engine other than Claude Code, Herdr reads it `blocked` by a named
  rule, with the blocker visible;
- the driver's own reading of both panes shows the dialog: `Permission`, or,
  for `trust_prompt`, its startup trust dialog (Herdr's rule alone is never
  enough);
- the row's own guard holds;
- no command of the session is unfinished;
- the breaker is closed.

The Bridge checks again when it presses a key of `spawn-resolve`. It reads
the pane first and presses only if the driver still reads the dialog and no
other command of the session was recorded `pressed` or `typed` since this
key was accepted. Otherwise the key ends `Failed` ("the dialog this answer
was for is gone: nothing pressed"), with no `pressing` step. So when a
person answers first, the resolver's key presses nothing.

The breaker is folded from the log, never kept in memory alone. Its counts
come from the commands of `spawn-resolve`, plus `submit`'s idempotency on
the key `resolve:<execution>:<dialog id>:1:1`.

- A dialog gets at most one automatic answer per execution. If it still
  shows 5 s after its answer, it is escalated once ("still shows after its
  answer") and never pressed again.
- An execution with 5 answers in 10 minutes gets no more.
- The host gets no more after 20 answers across the fleet in a minute.

**The switch.** `SPAWN_AUTO_APPROVE = off | startup | all`, default
`startup`, which changes nothing. Landing PR 3 is inert until the owner sets
`all`, and a test pins that the resolver reads and submits nothing
otherwise. `off` means no automatic answer at all, the startup trust dialog
included (`BridgeConfig::auto_trust`, set by `runtime.rs`). A start that
shows the dialog under `off` waits, until its readiness limit, for someone
to answer it in the engine's terminal itself (a key sent through Spawn
queues behind the start). If nobody does, the start fails naming the dialog
and the switch.

**The audit trail.** Each automatic answer is on the record with kinds that
exist: `command.accepted` (principal `spawn-resolve`), `command.step
pressing` and `pressed`, `command.finished`, then the next
`screen.observed`. The Bridge records `screen.observed permission` on its
own pass, normally before the answer. The resolver does not wait for that
record: it reads the pane twice itself. The Bridge's startup trust answer was
unrecorded before and now goes the same way:

1. It submits one key command per key, keyed
   `resolve:<execution>:trust:1:<k>`.
2. It records `pressing`, sends the keys together as before, and records
   `pressed`.
3. The commands queue behind the start. Once the start is done, the Bridge
   finishes them without pressing again.

A restarted Bridge that sees the dialog again finds the keys pressed and
presses nothing. A live probe found no protected scenario that draws Claude
Code's trust dialog, so no scenario's record changes.

**Stragglers.** After a terminal is closed and its runner's session has
exited, both hosts look for processes of this user whose working directory
is beneath the workspace. They read it from `/proc/<pid>/cwd`, an exact
link; they never match an argument pattern (`pgrep -f`, `pkill -f`). An
engine may have detached such a process: agy's `--bg-updater`, a `nohup`
server. It is ended by pid through a pidfd, SIGTERM then SIGKILL, before the
final checkpoint. One that outlives both is a terminate error naming it. A
directory too broad to be one workspace is never swept: fewer than three
parts, or the home, temporary or current directory, or any directory above
one of them.

**Typing on Herdr.** `submit_staged` hands the staged text to the pane's
agent with `agent.prompt`: one ordered, paste-aware write, then its
submission. It never passes `wait`, which returns on a false idle. It uses
`agent.prompt` only while the request line stays under 256 KiB, because
Herdr 0.9.5 resets a connection whose request line passes 1 MiB (probed).
Three cases still go through chunked `pane.send_text` and Enter, as before:
a longer text, an empty one, and a pane with no agent Herdr detects
(`agent_not_found`; nothing typed).

`agent_blocked` and `agent_not_ready` are refusals before typing: nothing
is typed, and the text stays staged. But the Bridge records a typing before
it submits, so a refusal at that point still reads as one that may have
typed. The check at staging is what keeps that rare.

**One Spawn deployment per Herdr session.** A host writes a terminal's
record before `workspace.create`, as a pending record that names only the
server. A sweep of restored workspaces (§3.6) that lists the new workspace in
between therefore keeps it. A start that stopped in between leaves the
pending record behind, and the terminal's terminate closes the workspace
labelled with its name.

The sweep closes every Spawn-labelled workspace that no record of this
host's names. So two Spawn deployments on the same Herdr session
(`SPAWN_HERDR_SESSION`) would close each other's workspaces: run one
deployment per session.

**Subscriptions and manifests.** The host's event subscriber asks for the
session's snapshot, and marks every followed terminal changed, only after a
real reconnect (a dropped connection). Subscribing again because the
followed panes changed does neither.

Besides the protocol, a server is checked against the agent detection
manifests Spawn was tested with (`server.agent_manifests`): agy
`2026.10.05.1` and claude `2026.10.11.1`, Herdr 0.9.5's bundled ones. A
server whose active version differs, or that has none, is refused before
anything starts, and the message names the pinned and the actual version.
The `[update]` pin holds only for a server this host started; this check
holds for any server.

**agy's transcript does not rotate (N10, probed).**
`brain/<id>/.system_generated/logs/transcript_full.jsonl` is one append-only
file with no size cap. It kept the same inode past 6 MB over seven turns
(agy 1.5.14 for turns 1 to 6, 1.5.17 for turn 7).

agy also writes `logs/chunks/.../NNNNNNNN.jsonl` from the first turn: a
redundant mirror cut at 100 KiB by bytes, not by lines. The reader ignores
it.

The file's last line may lack its trailing newline; the reader takes a line
only once it ends. It keeps following the one file, and the existing rule
for a shrunk or replaced file stays as the safety net.

**A wrapped draft.** A draft wider than agy's prompt box wraps onto the
lines under the prompt line, which moves the lower rule down.
`read_agy_screen` reads it as `Drafted`, for up to 8 wrapped lines, never as
`Unknown` or `Ready`.

### 3.8 Routing by preset, masking and capabilities (issue #91, PR 4)

**The engine comes from the task's preset** (design 7.1;
docs/agent-grading.md, "The engine a preset runs"). Delivery starts a
worker on its preset's driver, with the preset's `model_label` for agy
(`start {model_label}`, agy's `--model`). The lifecycle layer never reads a
grade: it receives an engine and a model in the start and runs them. An
ephemeral worker changes engine by a stop and then a start; a resident keeps
one engine. An engine the host cannot run, or a label `agy models` does not
list, makes no start: the task waits and `engine_unavailable` is escalated,
never a fall back to Claude Code. The Master Control wake of a supervisor
with no native session starts it on the engine, model and model label of its
newest start (`spawn_human_master_message`), Claude Code when there is none.

**Every raw pane is masked where it leaves the host** (decision C2, N4).
There are three such places: the judge's terminal tail
(`HostContext::screen_tail`, `crates/spawn-cli/src/context.rs`, which the
Observer stores as evidence and the attention view reads back), the agent
page's pane (`/api/sessions/<id>/pane`) and the approvals' capture
(`serve/approvals.rs`, which the resolver reads too). Each looks up the
engine of the session's newest `start` (`PostgresStore::latest_engine`) and
applies `spawn_core::driver::excerpt_for`: the pane stays byte for byte only
when that engine is positively `claude`. agy's pane, an unknown engine's,
and any pane whose lookup fails or finds no start lose every line that holds
an e-mail address (`drop_agy_account_lines`).

**The capability probe** (N5, `crates/spawn-cli/src/serve/capabilities.rs`).
`spawn loop` probes at its start and then hourly, on a thread of its own.
It runs `<program> --version` for Claude Code and for each other engine
`SPAWN_ENGINES` enables and, for an enabled agy, `agy models`, read by
`AgyDriver::known_models`. An engine `SPAWN_ENGINES` does not name runs no
process at all (decision C3): it is reported not enabled, its install and
version unknown. Each run is bounded, its output too: a detached process
(agy's background updater) that keeps the output open past the bound fails
the run, and agy is reported down. Each run takes no shell and has its
standard input closed. Login files are only `stat`ed. A resumed agy session
runs the model label its last execution was launched on (its
`execution.started` launch summary), the host's only when that recorded
none.
A failure takes down agy alone, never Claude Code. agy's
"is not recognized as a known model" line on a starting pane fails that
start at once (`AgentDriver::start_refusal`). An agy `major.minor` outside
`AGY_TESTED` raises the notice `agy_version_untested` and refuses nothing.
`GET /api/capabilities` shows the probe: engines, versions, login files
present, agy's known models, and the terminal backend (Herdr's version,
protocol and manifests).

### 3.9 The filesystem view of an engine without deny rules (decision C2, N6; H5)

Claude Code is kept off the host's credentials by the deny rules it is
handed at launch (`Guard::Settings`). agy has none (`Guard::Sandbox`), so
**the Bridge starts it only inside a filesystem view**:
`crates/spawn-host/src/engine_view.rs` (`BwrapView`) behind the port
`EngineView` (`crates/spawn-core/src/port.rs`), handed to the Bridge as
`BridgeConfig::engine_view` (`crates/spawn-cli/src/runtime.rs`). Claude
Code never runs in it. The launch script Herdr's pane runs ends in

```text
exec bwrap --ro-bind / / --dev /dev --proc /proc --die-with-parent --unshare-pid \
    <masks> --bind <ws> <ws> --bind <engine home> <engine home> \
    --setenv TMPDIR <TMPDIR> --chdir <ws> -- \
    spawn run -- agy <args>
```

`<TMPDIR>` is the engine's own, `<engine home>/.spawn-tmp` (0700): never
the loop's shared `TMPDIR` (`/var/tmp/spawn` on a fleet host, where other
host jobs keep scratch), which is masked whole. It is writable through
the engine home's bind and is not bound again (a second bind would resolve
its path anew when `bwrap` runs, after a writer outside could swap it). The
engine home is checked where it would resolve before it is made on disk,
so a refused one is never created; it is checked again once made. The runner stays the
engine's parent and writes the exit status into the workspace. Binding rules, from the probe under herdr 0.9.5 and bwrap 0.9.0:
**never `--new-session`** (`setsid` takes the engine out of the pane's
foreground process group, and Herdr's `agent.get` answers
`agent_not_found`); **always `--unshare-pid`** (no `kill(2)` of a host
process; Herdr still detects the engine); never `--unshare-net`,
`--unshare-all` nor `--as-pid-1`. **Masks first, writable binds last**, each
bind at its own exact path, so the Bridge reads the transcripts on the host
where the engine writes them.

The masks, resolved again at every launch. Each target is canonicalized
(`--tmpfs` over a symbolic link aborts `bwrap`) and masked by what it
resolves to:

- directories, each an empty `--tmpfs`: `/run/keep`, `/run/keep-test`, the
  whole `$XDG_RUNTIME_DIR` (else `/run/user/<uid>`), the whole `/tmp` (any
  sockets or temporary files in `/tmp`), the loop's shared `TMPDIR`, the Herdr session
  directory
  (`$XDG_CONFIG_HOME/herdr` and `~/.config/herdr`), `~/.claude`, the other
  engines' homes (`~/.gemini` but for agy's own home below it, re-bound
  writable into the mask's tmpfs; `~/.config/antigravity`, `~/.codex`,
  `~/.hermes`), `~/.ssh`, `~/.gnupg`, `~/.config/gh`, `~/.aws`,
  `~/.config/anthropic`, `~/.config/spawn`, `~/.config/cloudflare`,
  `~/.config/observe3-private` (each `~/.config/<x>` also below a
  customized `$XDG_CONFIG_HOME`);
- regular files, each read-only bound over by one private empty 0444 file
  (never `/dev/null`, whose bind answers `EACCES` instead of reading empty),
  kept in `$XDG_RUNTIME_DIR/spawn-engine-view-<uid>` (0700, checked to be
  this user's alone; made with `create_new` and renamed into place), which
  the runtime directory's mask hides: the engine can neither rewrite nor
  swap it between launches:
  `~/.local/share/spawn-rsi/spawn.env` and every other `*.env*` and `env.sh`
  in that directory, `~/.claude.json`, `~/.pgpass`, `~/.netrc`,
  `~/.git-credentials`;
- a target that is missing, or of another type (a socket, a fifo, a
  device), is skipped and said on the loop's standard error, one line per
  launch (one missing target would abort `bwrap`).

**Herdr's socket and `/tmp` are not kept.** A probe canary showed that a
client inside the view could connect to Herdr's Unix socket, open a pane
outside the view and read the masked files through it; the same holds for
any `herdr-core` / host sockets under `$XDG_RUNTIME_DIR` or `/tmp`. Both are masked, so the engine's own Herdr
calls are none: `herdr_witness` / Herdr detects it from outside. The network stays.

**No writable bind overlaps a mask or the private directory.** A workspace
that holds or lies in a masked path, or reaches the private directory, is
refused. So is an engine home that holds the user's home, is or holds a
masked path, or reaches the private directory: a misconfigured
`SPAWN_AGY_HOME` of `~/.gemini` (which would show `oauth_creds.json`),
`~/.claude`, `~/.ssh`, `/run/keep`, `$XDG_RUNTIME_DIR`, `/tmp` or
`~/.config/herdr` is refused, never silently unmasked. The one exception is
an engine home below another engine's masked home (`~/.gemini`,
`~/.config/antigravity`, `~/.codex`, `~/.hermes`): agy's default
`~/.gemini/antigravity-cli` is bound back into the empty tmpfs of
`~/.gemini`. **Known residual:** a target missing at a launch that
appears later is visible to the engine already running (`--ro-bind / /` is
live); the next launch masks it.

**Policy** (`SPAWN_ENGINE_VIEW`): `required`, the default (unset or empty
too), starts a `Guard::Sandbox` engine only inside the view. The view is
made at the loop's start, and only when such an engine is enabled
(`SPAWN_ENGINES`): `bwrap` found on `PATH` and shown to work (a view of the
same kind running `/bin/true`). A host where it cannot be made refuses every
start of agy, before anything is mounted, with an outcome beginning
`engine_unavailable:` that says why; a view that cannot be made for one
launch fails that start the same way and starts no terminal. Claude Code is
never affected. `off` is the owner's explicit opt-out: agy runs with no
view, as before H5, said once at the loop's start. Any other value is read
as `required`, said once. Tested against a real `bwrap` and a real Herdr in
`crates/spawn-host/tests/engine_view.rs` (no engine is run there: never
agy; the Herdr agent is a shell stub named `claude`).

**What agy sees and how it is bound inside the view** (O55, C3 step 3).
The mask of `~/.gemini` covers `~/.gemini/config/{config.json,mcp_config.json}`
too: agy runs on its defaults and the `--settings` it is launched with, never
the host's configuration or MCP servers. The capability probe (§3.8) runs a
`Guard::Sandbox` engine's `<program> --version` and `<program> models` inside
the same view, by the same `SPAWN_ENGINE_VIEW` policy, in a scratch
workspace of its own (below `/var/tmp`, else `$HOME/.cache`), removed after
the runs. `--unshare-pid --die-with-parent` also ends agy's `setsid`
background updater with `bwrap`. With no view under `required`, the engine
is reported unavailable and its program is never run on the host; under
`off` it is probed with no view. The view also masks agy's hook file
(`~/.gemini/config/hooks.json`) and Herdr's socket (`$HERDR_SOCKET_PATH`), and
neither is ever unmasked, so agy reports no session to Herdr: Herdr reads
`agent=agy, session=None`. `Bridge::bind_reported` therefore tries Herdr's
reported session first, when there is one, then falls back to scanning
`<agy home>/brain/*` (`AgentDriver::session_scan_dir`,
`TerminalHost::list_dirs`: plain directory names only, never symlinks,
newest first, at most 64). The fallback runs only after something was typed
into this execution, and never while Herdr says another agent holds the
pane. Whichever way a conversation is found, it binds only when
`brain/<id>/.system_generated/logs/transcript_full.jsonl` has a
`USER_INPUT` line holding the marker of a command typed into this session
at or after this execution's start: the anti-forgery check of §3.6 (N2) is
unchanged.

**Anti-collision (D1, O55 C3 step 3 re-test).** A marker `[spawn message #N]`
is only a store-local command id, and `~/.gemini/antigravity-cli/brain/` is
shared: another store's or an older run's conversation can quote the same
`#N`. Bound, it would count foreign usage and a `resume` would type into the
wrong conversation. Three more guards in `Bridge::bind_reported`, each
failing closed:

- **Fresh prompt.** The `USER_INPUT` line holding the marker must say it was
  written (`created_at`, RFC 3339 with `Z` or `±HH:MM`, read by a small
  stdlib parser) no earlier than the execution's `execution.started` record
  time less 5 s of clock skew (`Active::started_at`, the same after a
  restart). No `created_at`, or one that does not parse, binds nothing.
- **Pre-existing conversations.** A fresh start (not a resume) lists
  `brain/` before it launches agy (`Active::preexisting_natives`); the scan
  never binds one of those. A restarted Bridge has no such list and relies
  on the other two guards.
- **One binding per native session.** A conversation that any execution of
  any session in the store was already bound to (`session.native_started`)
  is never bound again, whether Herdr reports it or the scan finds it.

## 4. Experience

Nothing is stored outside the log. Every model judgement is an
`observation.recorded` event with the evidence the model saw, its answer and
the rules' guess (§2); what happened next (the agent's next tool call, the
next command and who sent it, the end of the execution) follows it in the
same log. Nothing exports these episodes or replays them through the rules
(§9).

Four views derive the experience from the log, one row per item, each with
`at_ms`, the time it counts at (crates/spawn-postgres/sql/schema.sql;
readable by `spawn_reader` and the supervisor):

| View | One row per | Columns |
| --- | --- | --- |
| `spawn_decisions` | judgement (at its time), command of Spawn's loops (at its creation) and person's intent (`spawn_human_intents`, at its time; §0) | `at_ms`, `tier` (`rules` when the rules judged, else `model`; `dispatcher` for `spawn-dispatch`, `supervisor` for `spawn_supervisor`, `human` for an intent of `human` or `spawn`, `liveness` for `spawn-liveness`, any other principal as its own tier), `session_id`, `agent_id`, `source` (the judge or the principal) |
| `spawn_interventions` | the scorecard's interventions (docs/design.md §15, autonomy (a)-(d)) | `at_ms`, `kind` (`command`: a person's intent, `command_id` its first command; `resume`, a `mission.resumed` or `mission.unpaused`; `pause`, a `mission.paused`; `owner_task`, `cancel_reassign`), `mission_id`, `task_id`, `command_id`, `cancelled_task_id` (a cancel and reassign's cancelled task) |
| `spawn_human_intents` | a person's intent (§0) | `at_ms`, `intent` (its first command's id), `principal`, `agent_id`, `session_id`, `commands` (in order), `attempts`, `outcome` (`taken`, `failed`, `pending`), then each command's `created_at_ms`, `finished_at_ms` and `succeeded` in order, and `action` |
| `spawn_outcomes` | task's closing record | `at_ms`, `outcome` (`landed`, `accepted`: a review that closes it on a mission without a gate, `cancelled`, `aborted`, `over_budget`), `task_id`, `mission_id`, `by` |
| `spawn_escalations` | distinct tag of the dispatcher's inputs into the supervisor (`Escalation::tag`; a charter's tag is not one), at the first command that carried it | `at_ms`, `tag`, `command_id` |

They fold the control stream as the scorecard does: helper views give each
task's open intervals (`spawn_task_spans`: each assignment to its first
closing record) and each mission's open span (`spawn_mission_spans`: its
`mission.added` to its `mission.closed`). `GET
/api/scorecard/fleet?window=24h|7d|30d|90d[&until=<RFC 3339>]` (any other
window: 400) counts `[until − window, until)` from them, `agent_judgements`
and `agent_executions`: the ladder (`ladder`, `ladder_extra`), `agent_hours`
(clipped to the window), `human_per_agent_hour`, `escalations`,
`interventions` (`total`, `by_kind`), `outcomes`, `landed_per_day` (landed
plus accepted over the window's days), `judgements` (as the last hour of
`/api/fleet` counts them) and `missions` (each open in the window with
`hours`, `idle_hours`, its span less the union of its tasks' open
intervals, `owner_work`, true when its last configuration before
`until` gives it no objective backlog (nor does a mission never configured, objective 077), and `idle`, more than half of it,
never for owner work: its work comes from the owner, objective 062), ratios and hours to 3
decimals as the scorecard rounds (crates/spawn-cli/src/scorecard/fleet.rs).
The Scorecard's "How the fleet decides" reads it over the page's window;
`/api/fleet`'s 24 h ladder and human decisions per agent-hour read
`spawn_decisions` too, so home and the scorecard agree.

## 5. Interface

| View | Answers |
| --- | --- |
| Escalations (top of the board) | What the supervisor asks, in at most three lines (its `HUMAN:` line or its reply's last paragraph; the whole reply on request), and under it the box to type to the supervisor, always there; the dispatcher's escalations not yet told to the supervisor, and those told and unresolved (folded to a count while nothing is pending), each with its heading, line, agent and age (`/api/attention`, computed by the dispatcher's code); below it, the supervisor's "For the owner:" lines with their age, to read, not counted |
| Always-on agents (below the queue) | The supervisor, then the residents, pinned live or not: liveness, state and for how long, last activity, uptime, the task each holds |
| Health strip | In this order: liveness (always-on agents up, shortest uptime, liveness restarts in 24 h), resources (free disk per place against its warning and floor; memory available now against a 4 GiB warning line, of the total; each gated mission's waits for gate room: the run's task, how many `no_room` outcomes since when, whether it was escalated; a value the server could not read shows as unread, never guessed), judgements, the decision ladder and human decisions per agent-hour |
| Human interventions (Home) | The north star (§0) for the last 24 h and 7 days: interventions per agent-hour, how many, the retries left out, and on request the list behind each figure (kind, agent, line, its commands and tries, outcome) |
| Fleet board | The rest of the fleet in groups: on a task (escalated first, by the escalations queue, then the quietest), idle, ended (folded, newest end first); each row its state, the mission and task it holds, what it does now, its last event; the sidebar's session list uses the same groups |
| Agent detail | A box to type to its session, with what was sent there and whether it was typed or waits for the agent's turn; live terminal; the Jev panel (distribution, needs_human, stuck, the judge's attention flag and its reason, evidence, rules-vs-Jev); the commands into it (an input waiting for the turn says so); state timeline; event timeline |
| Missions | Missions with open tasks first, then idle active ones, then closed ones (folded); each mission's tasks in flight (oldest first), landed or accepted, sent back, cancelled (folded), an objective's retries nested under its latest attempt; grading difficulty/preset/step-up badges in `MissionsView` and `TaskPanel`; residents waiting for a human's approval |
| Grading & Episodes (`#/grading`) | `GradingView` renders Top-Down 3-tier grading presets and calibration/upkeep state (`GET /api/grading`) alongside the "Control-stream Episode Replay & Sync Points" panel (`GET /api/episodes`, SQL parity status, difficulty/rework/reroute and review/gate sync points) |
| Features (`#/features`) | `FeatureMapView` renders planned/implemented/verified/live capability graph (`GET /api/features`) enriched with read-only code dependency truth and semantic purity findings (`GET /api/features/truth`) |
| RSI | Objective missions: the backlog's objectives, tasks, gate verdicts and logs, metrics, the resident's terminal |

Every route the API answers, with its method, what it answers, its CLI twin
and who reads it, is listed once in [docs/api.md](./api.md): 33 canonical GET route
patterns and 9 POST routes on `main`, all from one table, `ROUTES` in
`crates/spawn-cli/src/serve.rs`, which the router dispatches on and unit
tests (`routes_match_docs_api_md` and `crates/spawn-core/tests/spawn_ipc_v1.rs`) hold equal to docs/api.md. In addition, two contract-locked out-of-table read-only `GET|HEAD` routes (`/api/features/truth` and `/api/episodes`, also enforced by `spawn_ipc_v1.rs`) run on `reads_gate` (`reads` pool) as `spawn_reader` and reject non-`GET`/`HEAD` requests with `405 Allow: GET`. Fronted by `spawn-node-edge.service`, the PWA shell serves externalized static assets from `SPAWN_WEB_DIR` (`web/public/{manifest.webmanifest,sw.js,icon.svg}`). The paths name
the record's nouns (issue 98): a session's reads and writes are under
`/api/sessions/<id>`, a task's under `/api/tasks/<n>`, the objective
missions' under `/api/objectives`, a mission's resume under
`/api/missions/<id>/resume`. The 14 older spellings (14 alias patterns for
the 11 renamed routes: `/api/session/`,
`/api/fleet/<id>/commands`, `/api/pane/`, `/api/rsi`, ...) are aliases
(`ALIASES`): `handle()` rewrites one to its canonical path before the read
guard and the shared answer's key, answers exactly as the canonical path
does with `Link: <canonical>; rel="successor-version"` (left out when the
path holds a byte unsafe in a header), and logs the first of each spelling
once, as its two patterns only. The web UI reads only canonical paths. The reads run
as `spawn_reader`, with GET only (any other method on a read is 405); each
read's answer is documented on its handler in
`crates/spawn-cli/src/serve.rs` (the answer of `/api/attention` in
`crates/spawn-cli/src/serve/attention.rs`).

Of the nine POST routes, seven are a person's writes, as the `spawn_human`
login: an input to a session and a named key (below), a restart or a
reassign of a stalled worker (§3.2), the resume of a halted mission
(§10.3), a message to the supervisor and the answer to an approval prompt
(issue #91, below). The other two are the feature registry's scan and association,
which change no record (docs/feature-map.md). The input is a person's input
to a session, `POST
/api/sessions/<id>/input` with `{"text", "key"}` (the key, an idempotency
key, may come as an `Idempotency-Key` header). That is the command `spawn
submit --principal human --agent <the session's agent> --session <id> --key
<key> --action '{"type":"input","text":...}'` makes, with the same
idempotency rule (a retried POST types once), and like every person's input
it waits for the session's turn (§3). It answers 202 with the command, which
`/commands` then follows; `{"probe": true}` passes every guard and writes
nothing, so the page knows before it offers the box. It runs as the
`spawn_human` login, whose rights for a session are `spawn_human_input` and
`spawn_human_key`: an input or a named key, as principal `human`, to the
agent the session belongs to; no other action, no read, no event (its other
rights, `spawn_human_task`, `spawn_human_resume` and
`spawn_human_master_message`, are the restart or reassign, the resume and
the Master Control message to the supervisor). Its guards, in order: the read guard (the clients that may
read may write: the token, `SPAWN_SERVE_LOCAL_OPEN`'s owner client,
`SPAWN_SERVE_OPEN_NETWORKS`' clients naming the server by its address);
POST only (405); without the token, an `Origin` naming the server as the
`Host` does, so a page on another site cannot make the owner's browser type,
and, with no token set at all, a client socket the owner's uid holds;
`Content-Type: application/json` (415), which no form can send without a
preflight; a body of at most 16 KiB (413); a key and a text a person types:
not blank, at most 8 KiB, no control character but tab and line ends (400).
A key answers with its first command only for the request that made it (the
same session and text, or key name): sent for another session or with other
content it is 409, makes no command, and the page takes a new key
(`spawn_human_replay`).

The key is a person's named key, `POST /api/sessions/<id>/key` with
`{"name", "key"}` (§3, "Named keys never wait"): the command `spawn submit
--principal human ... --action '{"type":"key","key":<name>}'` makes, through
`spawn_human_key` (the `spawn_human` login's other right), with the input's
guards and idempotency rule. The name is one of the set (400 otherwise, the
refusal naming it); a session no agent has used is 404, and one that runs no
execution is 409 with the reason, recording nothing. The page sends a key
only while the terminal panel itself has focus and is armed, never from a
text box, with a modifier or on auto-repeat (`web/src/keys.ts`). Focus alone
arms nothing (backlog P11): a reader who tabs into the panel, or presses Esc
to close the drawer, sends no key. A plain primary click inside the text
that selected nothing, or Enter on the focused panel, arms it; a scrollbar
drag or selecting text to copy does not, so the Esc after a copy still closes
the drawer. Armed, it is outlined and says "Keys on · Keys go to <agent>".
Blur (a click outside included) or the keys going to another agent disarms
it, and while it is not armed Esc closes the drawer and the arrows scroll.

Issue #91 P1a adds four endpoints for the dashboard, with no agent
protocol (no ACP): what an engine shows is read from its pane in the
Herdr terminal host (`spawn-hc` or legacy `herdr`), and every answer is a command on the
record.

- `GET /api/stream`: the event log as Server-Sent Events, with the read
  guard of every API read. Streams hold their own 64 slots (`SSE_SLOTS`,
  503 when all are taken), never the requests', at most 16 of them from
  one client address that is not loopback (`STREAMS_PER_PEER`, 503 `too
  many streams from this address`; loopback counts only toward the 64), and share one listener
  thread holding `LISTEN spawn_events` as `spawn_reader`, which also polls
  every second while anyone streams. `Last-Event-ID` (the browser's
  reconnect), else `?after=<id>` as `/api/events` takes it
  (`?after_event_id=` is its alias; both at once, or a cursor that is no
  whole number at least 0, is 400), replays at most 500 events before the
  live ones, none twice; a cursor further behind, or past the log's end, gets `event:
  resync` with `{"reason", "after_event_id", "latest_event_id"}` (`behind`,
  `cursor_ahead`, `stream_reset`, `slow_consumer`) and goes on live. Frames
  are `event: ready`, then `id:`/`event:`/`data:` per event (the
  `/api/events` row), a comment probe `:` every 500 ms of quiet (`PROBE`:
  a page gone, a reset included, fails the next write, so its slot, writer
  and socket are freed within about a second, with no database read), and
  `: keepalive` in its place every 15 s. A slow page (its
  256-frame channel full) is told `resync` and closed; a page that stops
  reading ends after a 10 s write timeout. An idle listener starts, once
  every stream there is has caught up, from the lowest id one caught up to
  (each stream's own floor, gone with it), never from the log's end, so no
  first event after an idle spell is lost
  (`crates/spawn-cli/src/serve/stream.rs`).
- `POST /api/master/message` with `{"text", "mode", "key"}`: a person's
  message to the supervisor from Master Control, through
  `spawn_human_master_message` (the `spawn_human` login, SECURITY DEFINER),
  with the input's guards. `mode` is `auto` (default) or `queue` (the plain
  input), `steer` (Esc first while it runs) or `restart` (a stop first
  while it runs). A supervisor that runs no execution, or will stop because
  of an unfinished stop or discard, is woken first (`resume` when a native
  session is bound, else `start`; the dispatcher types the charter at its
  first idle prompt), and the wake is always newer than the stop ahead of
  it. The key is 1 to 240 bytes (`MAX_MASTER_KEY_BYTES`); the same request
  again answers the first call's command ids exactly and makes none;
  another request under the key, or a key whose derived `:stop`, `:steer`
  or `:wake` key was already used, is 409. `{"probe": true}` answers the
  modes and the key bound.
- `GET /api/approvals`: the prompts a person must answer, found on the live
  panes' last 30 lines by fixed text rules (numbered choices only as a
  dialog: the pane's last thing, with a cursor mark, a frame or a key
  footer, never a reply's `● 1.` or a quoted `> 1.` list or a spinner's
  `esc to interrupt`; permission buttons at the bottom; yes/no; confirm),
  plus `attention` items (a judgement asking for a person) and
  `mission_halted` items. Each item's `id` is `<session_id>:<FNV-1a of its
  kind, question, body and answers>`; the body is what the dialog asks to
  approve (in a frame, every line from its top; otherwise the lines from
  the question, and a few contiguous ones above it, to the answers), so the
  same question over another command is another id, while a spinner or
  timer above leaves it. The engines' key maps are checked against
  hand-written screens, not yet live panes of every engine. `POST /api/approvals/<session_id>/act` with
  `{"action", "approval_id", "keys", "text", "key"}` answers one through
  `spawn_human_key` / `spawn_human_input`: `allow`, `allow_always` and
  `deny` need `approval_id` (400 without) and press only when the pane,
  read again, still shows that prompt (409 otherwise: a stale click presses
  nothing); `keys` (1 to 8 named keys, `digit1` to `digit9` included) and
  `text` need none (`crates/spawn-cli/src/serve/approvals.rs`).
- `GET /api/context/digest`: the fleet in one small answer (states, nodes
  with power and the soft drain, open missions with their last gate
  failure, pending approvals, the supervisor's notes for the owner) and a
  Markdown `digest_text` under 4000 bytes, masked; a part that cannot be
  read is named in `warnings` (`crates/spawn-cli/src/serve/digest.rs`).

`/api/missions` and `/api/objectives` send what the lists show: a task's brief,
report and review note come as their first lines with their whole length
(`<field>_bytes`), and `/api/tasks/<n>` answers the whole texts
when a task is opened; an objective's prompt comes as its first lines and
`/api/objectives/<m>/<n>` answers it whole when the objective is
pointed at. Every answer carries a weak `ETag` and a read that
sends it back (`If-None-Match`) gets 304 while nothing it shows changed; a
read that says `Cache-Control: max-age=1`, as the page's polls do, may be
given the answer computed for another reader up to a second ago, so one
computation serves every open page (`crates/spawn-cli/src/serve/answers.rs`).
The page polls only while it is visible and reads again the moment it shows.
Humans talk to the supervisor through that box (top of the Fleet page), its
pane, or `spawn submit --session session_supervisor --action
'{"type":"input",...}'`.

## 6. Invariants

**Storage and compute are separate.** PostgreSQL is the only source of
truth (event log, command queue, grants, the tables folded from the control
stream). Every other process is compute: it holds nothing that cannot be
rebuilt from storage, and everything people read (SQL views, API, UI,
metrics, corpora) is derived from storage. A model judgement becomes storage
the moment it is written, together with the evidence it saw.

Known edges, each with its rule:

| Edge | Rule |
| --- | --- |
| The UI's live terminal reads Herdr | A compute peek, labelled as such; the stored copy is the screen tail in each model observation |
| The supervisor's native transcript lives on the host (`~/.claude`, copied to the host's transcript archive) | A cache; the handoff rebuilt from the log is the authoritative recovery path (§3.1) |
| The attention queue reads the supervisor's question and its "For the owner:" lines from its transcript | A compute peek at the transcript the log names (`session.native_started`), read by the Observer's own reply reader; where it cannot be read, the screen tail stored with the judgement stands in for the question, labelled as such, and no line for the owner is listed |
| Code lives in git | Artifact storage; every commit is referenced from an event |

- The event log is the only state; every process is restartable.
- `EventKind` stays closed; observation fields are additive.
- Facts > rules > Jev; observation never acts; Jev alone triggers only reversible, bounded actions.
- Worker panes hold no database login. The Bridge reads what an engine does from files it writes anyway and from its screen: the Claude transcript (the Bridge names the session with `--session-id`, so it knows the path before the engine starts), the pane read every tick, and the exit file the runner writes. The supervisor's pane gets `spawn_supervisor`, which reads the fleet and submits commands through the gateway but appends no events of its own. The Bridge, the Observer and the controllers run as the owner. `spawn serve` reads as `spawn_reader`, which can write nothing, and types a person's input as `spawn_human`, which can call `spawn_human_input` and nothing else (an input, principal `human`, to the session's own agent); it binds loopback, and on any other address needs `SPAWN_SERVE_TOKEN`. Roles belong to the whole cluster, so each database has its own set, named after it (`spawn_supervisor_<db>`, `spawn_reader_<db>`, `spawn_human_<db>`, SQL `spawn_login_role`); grants, commands and records still name the principals `spawn_supervisor`, `spawn_reader` and `human` (`spawn_principal`). Two installs on one cluster thus share no privileges and set none of each other's passwords. The cluster-wide `spawn_supervisor`, `spawn_reader` and `spawn_terminal` of earlier installs lose every privilege in the schema and act as nobody; a migration warns while one can still log in, for a human to drop it. A login's password is PBKDF2-HMAC-SHA256 of the owner's password (600,000 rounds), salted with a random salt kept in the login's role comment and its name (`spawn_postgres::role_url`), and set by every migration: nothing secret is stored, only a holder of the owner's password can form it, and a login that knows its own password and salt pays the rounds for every guess at the owner's. The owner's password is percent-decoded first, so every spelling of it agrees. An owner URL without a password derives nothing: `role_url` refuses, and such a migration turns the logins off (no login, no password, with a warning) until a migration that has the owner's password.
- No model key in Spawn: the DeepSeek key stays in the model relay's process (or `spawn-node-out.service`'s `LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key`, never in Spawn's own environment), the gateway token in Claude Code's own settings (§3.3).
- Audited core/data safety boundary: `crates/spawn-core/tests/dependency_guard.rs` (`pure_crates_forbid_unsafe_code`) permanently enforces `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`, and zero non-test `unsafe` tokens across `spawn-core`, `spawn-postgres`, `spawn-cas`, `spawn-kdf`, `spawn-codegraph`, and `spawn-jev`, alongside the Phase 2d `std::net` ratchet.
- Workspace backend isolation (D6): opt-in `SPAWN_WORKSPACE_BACKEND=d6-shadow` runs a dual-track oracle compatible with authoritative `git` roots, while opt-in `d6` runs pure-Rust `D6Workspace` (zero `git` subprocesses) and is strictly refused on `git`/`cow`/`overlay`/`d6-shadow` roots without explicit cutover (`SPAWN_D6_{CAS_DIR,TMPFS_ROOT,TMPFS_BUDGET_MB,STRICT_FAIL_CLOSED}`, see [d6-cas-v1.md](./d6-cas-v1.md)).
- Every landing goes through the black-box gate, a host-side commit, a compare-and-swap move of the baseline, and the halt rules (§10).

## 7. The closed loop

```text
 worker acts ──► observe (rules → Jev) ──► dispatch (rules → Jev nudge → supervisor → human)
      ▲                                              │
      └──────────── command ◄────────────────────────┘
```

The supervisor is kept alive by the liveness rule (resume, then handoff),
receives escalation digests and submits to workers. Workers are Claude Code
sessions started for the supervisor's tasks. Observation runs rules first and
Jev on ambiguity, with the full payload. The dispatcher's policy decides
fixed cases by rules, allows one guarded Jev-gated nudge (`continue`), and
escalates everything else to the supervisor, which may escalate to a human.
A human reads the escalations queue when they choose and types to any
session from the web page (`POST /api/sessions/<id>/input`) or with `spawn
submit --principal human`.
Every decision is in the log with its evidence and what followed it.

### 7.1 The end-to-end check

`scripts/e2e.sh` runs the whole loop with a scripted engine and a stub Jev,
on real PostgreSQL, git and embedded `herdr-core` (`spawn-hc`), in its own schema and Herdr session (about
20 s). The gate runs it as `rsi/scenarios/check-e2e.sh` on every landing. It
asserts:

1. The liveness rule starts the supervisor.
2. A rule decides WORKING while a tool runs; Jev decides an idle prompt, and
   the supervisor gets the digest.
3. The guarded nudge restarts a stalled worker, once.
4. A `HUMAN:` question reaches the attention queue even when Jev is
   confident it is fine, and a human command resolves it.
5. With Jev down, a stalled worker is escalated on rules alone, not nudged.
6. Missions: a resident runs only after a human approves the supervisor's
   proposal; its tasks are delivered one at a time, reported, sent back once
   and accepted; a mission without a source works in a scratch workspace; an
   ephemeral worker gets its mission's repository, reports, and is stopped
   once accepted; a new worker starts from the source as it is, not as it
   was first mirrored.
7. Landing: accepted work passes the gate and lands on its source, the
   second landing as a merge; work the gate rejects goes back to its worker
   with the gate's output; a push outside the gate halts the mission until a
   human resumes it; an objective becomes a resident's task, the gate alone
   lands it, and a solved objective is not assigned again.
8. A human message to a worker reaches the supervisor; a killed resident is
   resumed; one the supervisor stopped stays down.
9. Killing the supervisor's engine, then its `herdr-core` daemon, ends with the same
   `session_supervisor` resumed natively; killing the loop leaves the same
   supervisor execution running.

## 8. Known limit: one uid

Every engine runs as the owner's Unix user, and unsandboxed panes can reach the shared terminal host. The database boundary holds
(no pane can write the log), but the file boundary does not: an unsandboxed agent can
read another process's environment through `/proc` or the terminal host (including the
supervisor pane's login), write the files the Bridge reads for another
session (a transcript, an exit file), which the Bridge then reads as that
session's facts, or inject text into another pane via the terminal host socket. The Bridge
reads those files without following symlinks, refuses anything but a regular
file and caps each read, so a planted FIFO or an endless file cannot stall
or exhaust it. Holding no login stops a worker from forging facts through
the database; it does not isolate agents from each other. Isolation between
agents needs one uid (or one sandbox) per agent. The gate runs candidate
code in a bubblewrap sandbox under the same uid (see docs/rsi.md §4).

## 9. Not built

- **Outcome labels for episodes**: task episode replay and SQL projection parity (`spawn mission episodes [--mission <id>] [--strict] [--verify-parity] [--json]`, `GET /api/episodes`, and the episodes panel in `#/grading`) are built, reconstructing per-task rework and review/gate sync points from `control.recorded`. What remains unbuilt is labelling each Observer judgement with its downstream outcome or replaying the playbook against exported corpora.
- **RSI objectives over the corpus**, judged by black-box replay through the
  candidate's `spawn` binary on a corpus snapshot the candidate cannot edit:
  rules coverage (share of episodes decided by rules up, agreement with the
  labelled outcome kept), Jev filtering (escalations to the supervisor down,
  outcome accuracy kept), playbook reach (interventions in the corpus the
  playbook would have resolved, up), and human interventions per agent-hour
  on live runs. Nothing replays the playbook over recorded episodes.
- **UI write endpoints (now built in `§5`)**: human inputs (`POST /api/sessions/<id>/input`), named keys (`POST /api/sessions/<id>/key`), worker restart/reassign, mission resume, supervisor Master Control (`POST /api/master/message`), and approval actions (`POST /api/approvals/<session_id>/act`) all dispatch as `spawn_human` on the record; `spawn submit --principal human` remains the CLI equivalent.
- **More dispatcher rules:** stopping a COMPLETED one-shot task (it
  escalates). (`restart` and `reassign` are built: §3.2, "Restart and
  reassign".) Retrying after a
  transient API error is the recovery's (§3.1): bounded for an engine that
  ended, unbounded for one at its prompt; telling the supervisor after N
  live retries is not built.
- **Ladder health over long periods.** The UI's figures cover the last 24 h.
- **The backlog, scenarios and gate versioned with the source**
  (docs/storage.md).
- **The Review point in code** (§1.1). Review sessions are run by the lead
  and their verdicts are kept outside the log. No record type holds a
  verdict, `spawn task accept` does not require one on the report's
  revision, and a gate-mode mission's integrator gates a report without
  one.

## 10. One log, one landing path

RSI is a mission whose tasks the gate accepts; there is no separate RSI
controller. Missions, tasks, residents and landings are events in
`spawn_events`, and the `mission_*` tables are a projection of them.

### 10.1 The control stream

- **Where it lives:** one stream with session `session_control`, agent
  `agent_control` and source `control`. Every record is the event
  `control.recorded`, whose detail is `{type, ...}`. A new record type adds
  no `EventKind` variant.
- **Record types:** `mission.added`, `mission.configured`, `mission.closed`,
  `mission.halted`, `mission.resumed`, `mission.paused`, `mission.unpaused`,
  `source.moved`, `source.held`,
  `source.released`, `source.hold_expired`, `task.assigned`,
  `task.delivered`, `task.report_unfinished`, `task.reported`,
  `task.reviewed`, `task.cancelled`, `task.gated`, `task.held`, `task.landed`,
  `task.aborted`, `task.over_budget`, `resident.proposed`, `resident.approved`,
  `resident.retired`, `legacy.imported`, `host.disk`, `host.judge`,
  `source.push_requested`, `source.push_refused`, `source.push_no_verdict`
  (§10.3, "The owner's landings"), and the loop's alerts: `alert.fired` and `alert.cleared`
  (§3.4, folded into `spawn_alerts`).
  `mission.paused` {id, reason} is the owner's hold on a mission (`spawn
  mission pause`, a human only; objective 099), never of a closed or an
  already paused one; `mission.unpaused` {id, pause_event} is `spawn mission
  resume` of a paused mission, and clears only the pause (a halt it is in
  stays, for a second resume). While a mission is paused no objective is
  assigned to it (`spawn_objective_assign` refuses, naming the pause),
  nothing is delivered to its workers, its resident and the workers of its
  delivered tasks are stopped by the dispatcher's Stop (`pause-<mission>-
  <pause record>-<execution>`: a final checkpoint, recorded `stopped`, never
  revived) and its delivered tasks stay delivered; no `mission_idle` fires
  (a pause clears an open one) and the Scorecard counts its paused time as
  `paused_hours`, not idle. At the unpause each worker the pause stopped is
  resumed once in its own native session (`unpause-<mission>-<unpause
  record>-<session>`), on its workspace rebuilt from its branch.
  `task.report_unfinished` is a delivered task's report the dispatcher read
  and did not take because it is still a draft: it names the task's
  `revision`, the content's `digest`, the `markers` (each with its `line`,
  `kind`, `marker` and `text`) and the note that told the worker (`note`,
  its idempotency key, and `command_id`); one per revision and content,
  folded into no table (the task stays delivered).

  `legacy.imported` carries one row of the mission tables as it stood
  before the control stream existed: a database with mission rows and no
  records imports them once, on migration. A `task.cancelled` names `by`,
  who cancelled: `human` (the owner's login, `spawn task cancel` and a
  person's close or retirement), `spawn_supervisor`, or the loop that did
  (`spawn-integrate`, `spawn-dispatch`, `spawn-migrate` for a migration's
  own records); a record from before it names none and counts as a
  person's. The fold does not read it; the scorecard does
  ([design.md](design.md) §15).
- **Who writes it:** only the checked mission functions, through
  `spawn_control_record`. The integrator's records (`task.gated`,
  `task.landed`, `task.aborted`, `task.over_budget`, `mission.halted`, and
  the outcomes of the
  owner's landings: `source.moved` naming a `request`,
  `source.push_refused`, and an owner judging's `source.push_no_verdict`),
  `source.moved` (`spawn mission moved`), the
  owner's `source.push_requested` (`spawn mission push`; folded into no
  table, nor is `source.push_refused`), the loop's `host.disk` and the
  judge watcher's `host.judge` (through `spawn_host_record`; folded into no table,
  [storage.md](storage.md#disk-room)) and the loop's `alert.fired` and
  `alert.cleared` (through `spawn_alert_record`, §3.4) need the owner's
  login. The
  restricted logins write no events at all: `spawn_append` refuses them.
- **The tables are a projection.** `spawn_control_apply(record)` is the only
  writer of `mission_*`, and it runs in the same transaction as the append.
  Every control record variant (67 variants) and grading record variant is
  modeled as a statically typed ADT (`ControlRecord`, `ControlRecordKind`,
  `GradingRecord`, `ControlRecordError`) in `crates/spawn-core/src/control.rs`.
  `spawn_control_refold()` rebuilds the tables from the log, and a test
  checks that the rebuilt tables equal the live ones; `spawn mission episodes
  [--mission <id>] [--strict] [--verify-parity] [--json]` replays per-task
  `TaskEpisode` timelines and checks projection parity against live SQL tables.
- **It is not an agent.** It is left out of the session projection, and the
  Bridge and the Observer skip it.

### 10.2 One gate

`scripts/rsi/gate.sh <repo> <base> <candidate> <scenarios> [<scenario>]`
judges every landing, RSI or fleet. It exports both commits to temporary
directories and judges the candidate with the controller's scenarios, never
the tree's own:

- it refuses changes to any path in `<scenarios>/protected` (one prefix per
  line, `#` comments, a line `!<prefix>` excepting paths under it; the
  deployed list, `rsi/scenarios/protected`, protects `rsi/`, `scripts/`,
  `deploy/`, `web/` but `web/src/` and `web/index.html`, `rust-toolchain`,
  `rust-toolchain.toml` and `.cargo/` — everything that runs as the owner
  outside the sandbox or that judges the candidate, which no candidate may
  edit), any build-time code
  (`build.rs`, `.cargo/`, a `build` key in a Cargo.toml), any change to lint
  configuration, fewer runnable tests than the base, and the loss of a base
  test by name. The owner retires a test with the feature it removed by
  naming it in `<scenarios>/retired-tests` (one name per line, `#` comments,
  kept out of the base cache's key), which the gate reads on the host from
  the scenarios directory it was given: for a mission gate,
  `rsi/scenarios/retired-tests`, so a retirement there is a commit to the
  mission's scenarios directory — a landing and a deploy of its own, not a
  supervisor-side edit; for the owner's gate, a copy of the scenarios
  directory the owner keeps in the checkout — `rsi/.gate-owner`, kept out of
  the tree by `.gitignore` — with `retired-tests` beside that copy's own
  `protected`, which is where the list has actually been used;
- `<scenarios>/build` builds each tree;
- when a task has a scenario, that scenario must fail on the base (exit 1)
  and pass on the candidate;
- every other scenario must pass on the candidate, except ratchet scenarios,
  task-only scenarios (`# rsi: task-only`: run only as their own task's
  scenario, never on the base for another gate nor as a regression; 029,
  which no gate's sandbox can judge) and objective scenarios (`<digits>.sh`)
  The checks are scenarios, among them `check-clippy`,
  `check-tests`, `check-e2e`, `check-blackbox`, `check-web`,
  `check-features` (`spawn features check`, plus `--deps` / `--purity` backed
  by `spawn-codegraph` and `spawn-core::feature_deps`) and
  `check-model-relay` (the relay in front of stub upstreams: the route by
  model, the model mapping, a stream asked for again before its first
  event, the cut signal, the body cap). The halt rule on the scenario count (§10.3) protects them.
  check-tests runs first among the regressions (the most often decisive,
  so its failure stops the rest early). It also fails a run whose tests
  all pass but leave something behind (objectives 54 and 55, folded into
  it): any entry in the run's own fresh TMPDIR, any schema in the run's
  own fresh database on the test server, or a relation, function or type
  new in that database's public, each one `FAILED leftover-scratch:`,
  `leftover-schema:` or `leftover-public:` line, which the integrator reads
  as the candidate's failure, not the host's;
- a check that fails on the candidate is run again there, as an
  objective's "unsolved" needs two runs that agree: passing the second
  time, it is a flake (said in the log, and named in the verdict's and the
  `task.gated` record's `flakes`, so a check that keeps flaking on one
  candidate or mission shows) and the gate goes on; failing
  twice, it is run on the base (built for it when every base result came
  from the cache): passing there, it is a regression; failing there too, it
  is a host or harness fault, no verdict (exit 6, a `host_fault`, §10.3:
  backed off, never counted toward a halt). That base run follows
  candidate code on the shared test server, so it can only turn a
  rejection into a host fault, never into an acceptance. (2026-11-07: one e2e
  run that failed under load rejected task 127's 85-minute gate; the same
  tree passes it.)
- a numbered scenario the base solved that fails on the candidate is
  judged the same way before it is called a regression (gate.sh "Three
  classes of a failing regression"; the owner, 2026-11-10, after an owner
  gate rejected valid work on 028, which failed on main itself at that
  load): it runs again on the candidate, then on the base, now and never
  from the cache, once its candidate runs have ended, with nothing else of
  the gate beside it and on a test server no candidate run used (a second
  server of the gate's own, from its template). The base passing twice:
  two failures on the candidate (of two, or of three after a pass) are a
  candidate failure, rejected with the reason it always had; one failure
  of three is unstable on the candidate, `cannot_judge` (exit 6), never
  accepted. The base failing a run: a validator defect, never a
  rejection. A task's candidate then gets no verdict (exit 6, `host_fault`
  with the evidence `validator_defect` and the key `validator_defects`),
  because a base run that follows candidate code may spare a rejection and
  never accept; the owner's own candidate (`SPAWN_GATE_OWNER_CANDIDATE=1`,
  which spawn integrate does not pass yet) is judged on the rest and its
  verdict names the validators (`validator_defects`). On a shared test
  server a base that fails after the candidate ran is no evidence for
  either: a host fault. The integrator reads neither new key yet: a
  task's validator defect is recorded as the host fault it is, with the
  gate's reason.
- `<scenarios>/quarantined` (a list like `serial`; the owner's, the tree's
  copy under the protected paths, in no base cache key) names validators
  the owner knows are broken: not run as regressions, nor judged on the
  base for one, and named in every gate's log and verdict (`quarantined`).
  The task's own scenario is never skipped by it.
- the regression that last ended a gate of the same scenarios (a candidate
  failure, a validator defect, unstable) is judged first by the next gate,
  before the serial list and alone, until a run of it passes (gate.sh "A
  known failure first"; the record is beside the base cache,
  `gate-solved/<root commit>/.known-failure.<hash of the scenarios'
  names>`, hidden from the sandbox): a failure that is still there costs
  that scenario's own runs, not the gate.
- everything the candidate controls runs in a bubblewrap sandbox (docs/rsi.md
  §4). The verdict is one JSON line that names the base and candidate
  commits it judged; the integrator believes only the verdict file it owns.
- a regression's result may be the base's, not a run (gate.sh "Results
  reused from the base", GitHub issue #48): a scenario that declares what
  a run reads of the tree (`# reads: binary [<path> ...]`; 001, 007 and
  027 so far) is not run on the candidate when the candidate's `spawn` has
  the sha256 of the binary the base's `solved` was taken on (the hash is in
  the base cache entry, written by the run that earned the result), the
  paths it declares and the tree's own copy of the scenario are the same in
  both trees, and it is not on the `serial` list. The log says so per
  scenario with the reason, the verdict names them (`reused`), a reused
  result seeds no cache entry, and a scenario that declares nothing is
  always run. `SPAWN_GATE_REUSE=0` runs everything.
- objective results on the base (solved, unsolved after two runs that
  agree, or `cannot_judge` when the scenario says the host cannot judge it,
  one run) are cached outside the sandbox, keyed by everything that decides
  them: the base commit, the blobs of the scenario and of the gate, the
  tools the scenarios run (`rustc -vV`, `cargo -V`, `node --version`,
  `psql -V`, `git --version`, `python3 -V`, `jq --version`, in
  the sandbox inside the base tree) and the test server's
  `server_version_num`, the scenarios directory's build hook and helpers
  (and the `bind` list's text), a fingerprint of each directory the `bind`
  list names (the blobs of pnpm's `.modules.yaml` and `.pnpm/lock.yaml`
  when present, else a listing of the directory: path, type, size, mtime,
  link target), and the files the scenario names on its code lines
  (`scripts/...` of the controller, transitively; a scenario that reaches
  into `scripts/` through a variable or a `cd` keys all of it). A
  controller file no scenario names (the black-box runner) changes no key.
  Not keyed: an edit inside a bound directory that has pnpm's install
  record but was not installed, and the state candidate code leaves on the
  shared test server (a disposable server per gate is GitHub issue #6;
  until then one gate runs at a time). Only the gate
  writes an entry, after a completed base run; an entry it cannot read is
  a miss; candidate runs are never cached; the log lists which base
  results came from the cache, and the time spent on the base, the
  candidate and the setup. A hit touches its entry; spawn-maintenance
  removes entries no gate used for 14 days.
  Candidate code cannot see or write the cache, but any process of the
  same uid outside the sandbox can (the same-uid limit).
- **Exit status.** The gate exits 0 with a verdict. It exits 5 when it has
  no room to judge now: `TMPDIR` has less than `SPAWN_GATE_MIN_FREE_GB`
  free (`scripts/disk.sh` `gate_room`), or a scenario run waited past its
  bound (`SPAWN_GATE_SLOT_WAIT_SECS`, 30 minutes by default) for one of the
  host's scenario slots (§10.2 "Host scenario slots"). In the first case
  it first writes the fact `{"no_room":true,"free_kb","min_gb","tmpdir"}`
  to the verdict file, before any scratch is made or any candidate code
  runs; in the second, `{"no_room":true,"check":"host scenario slot",
  "waited_ms","budget"}`, and no scenario of that run starts. On a disk
  with no room at all, that write may fail, and the gate still exits 5; it
  also prints the fact, and the integrator reads the last JSON line of
  stdout before measuring, so a slot wait keeps its own check. The
  integrator reads `no_room` from exit 5 together with the fact (`gate.rs`).
  Without a readable fact, it measures the free space itself (`df`), and it
  records `no_room` with its own measurement only when that is under the
  floor. It does this in two cases: the gate exited 5 (it measures the
  gate's `TMPDIR`), or the integrator could not create or use its private
  directory for the gate (it measures that directory's parent). It exits 6
  with a host-fault fact (a scenario's harness error on the base, or a
  check failing on the candidate and the base): `host_fault`, §10.3. It
  also exits 6, the same status, with a scenario's own `cannot_judge` fact
  (a scenario that says this host cannot judge it at all: 029, whose
  resource envelopes need a systemd user manager the sandbox has none of).
  On the base that is the cached result `cannot_judge`, one run (the host's
  answer does not change), so 029 in every Spawn gate's regression set
  stops no landing; on the candidate a regression that exits 6 is skipped,
  never passed or failed, and the task scenario that exits 6 states the
  fact: `host_fault` with the evidence `cannot_judge`, never a rejection and
  never an abort (§10.3). The gate states the same fact itself, in its
  first seconds, when the sandbox lacks a prerequisite a scenario or check
  declares (a line `# needs: controller:<path>`, `tool:<name>` or an
  absolute path, read without running the file; `check-web` declares the
  controller's `web/node_modules/`): all of them are asked for in one
  sandbox after the static checks, before the base's build and any
  scenario, and a missing one is `cannot_judge` with the scenario and the
  `prerequisite` (gate.sh "Preflight"). A file the tree under test may lack
  is that tree's result, never a prerequisite. Exit 6 with neither fact is an abort, as is any
  other status: exit 3 (no bwrap, a missing bind directory, an unreadable
  `TMPDIR`, a floor that is not whole GiB, the gate's own tree under `/tmp`,
  `/run` or `/var/tmp` — itself or below it — or under the host's `TMPDIR`
  when that is not `/tmp`, all of which it refuses up front so that no base
  key is computed from scripts the sandbox empties), exit 4 (a run the gate
  lost:
  ended without a result),
  a timeout or a signal. The integrator measures the same floor, free
  memory and the test database itself before it starts the gate (§10.3,
  step 3), so a short host is found before a gate runs; the gate's own
  check and exit 5 stay, for a `TMPDIR` that fills in between.

`scripts/rsi/test-gate.sh` tests the gate itself.

The controller is the deployed commit's tree, never the owner's working
checkout: `~/.local/share/spawn-rsi/controllers/<name>`, a symlink to
`<name>-<commit>`, switched atomically by a deploy (`deploy/controller.sh`,
deploy/systemd/README.md "The controller"). The integrator's `SPAWN_GATE`,
each mission's scenarios and backlog, and the maintenance script are paths
under it, and the integrator resolves the symlink once per step, so a gate
judges with one commit's gate, scenarios and helpers. **An owner edit is
live only after its deploy**: a change to the gate, a scenario or the backlog
is judged like any other commit before anything runs it, and what runs is a
record of what was judged (§14 of design.md).

A commit whose root holds a `pnpm-lock.yaml` (Supply PA) also has a deps
tree: its `pnpm install` output, which git does not hold, at
`~/.local/share/spawn-rsi/deps/<name>/<sha256 of the lockfile>` (read-only;
`deploy/controller.sh deps` installs one, `update` does it when the
lockfile's is missing, and a switch keeps the previous controller's tree and
any tree an agent's workspace still links to, and any a process still uses,
removing the rest, and points
`<live>/deps/<name>/current` at the current one). It is
never the owner's checkout, whose `node_modules` every agent may write. The
gate binds the store read-only into its sandbox, and engines and gates read
it through `SUPPLY_PA_DEPS` in `spawn.env`, the literal path of `current`
(unset, they use that pointer as they do anyway; set it only once a
controller's tree and its pointer exist, since a path to no tree fails every
engine's and gate's build) (deploy/systemd/README.md "The controller").
Read-only is a boundary against accidents, not agents: the same uid can
hardlink a store file and write it; boundaries against a deliberate agent
are per-agent uids or a mount per engine.

### 10.3 Landing

- **Mission settings** (`spawn mission configure`, owner only): a gate (a
  scenarios directory in the controller, the deployed tree), how tasks are accepted,
  an optional budget, and objectives (§10.4). `configure` changes only the
  settings it is given and keeps every other as the mission has it
  (objective 027): `spawn_mission_reconfigure` reads the mission's row
  under the append lock, merges the change into it (the gate and accept
  replaced, the budget's and the objectives' fields one by one), checks the
  result as `spawn_mission_configure` checks any, and records it whole, so
  `mission.configured` always carries every setting and two configures
  never lose each other's change. A setting is cleared only by its own
  switch, which takes no value: `--no-gate`, `--no-budget` (both fields),
  `--no-minutes`, `--no-tool-calls`, `--no-objectives`, `--no-workers`
  (back to the default 1). A switch and a value for the same setting, or a
  flag configure does not know, are refused; a refused call changes
  nothing. It prints `{mission, before, after}`; a call that leaves the
  settings as they were (nothing given, or the values they already have)
  records nothing. Clearing the gate, or moving accept away from `gate`,
  is refused while a task of the mission is `landing`, naming it (GitHub
  issue #9: nothing would land it, and `spawn task cancel` refuses
  `landing`). (2026-11-08: before this, `configure --id 4 --tool-calls 600`
  replaced every setting of mission 4, dropping its gate, objectives and
  minutes, and setting accept to review, until the owner restored them.)
  `spawn mission add` takes the settings whole, for a mission that has none yet:
  - `review`: the supervisor accepts, then the gate runs;
  - `gate`: the gate alone decides. The supervisor is not told of its
    reports (`mission_news`) and `spawn_task_review` refuses them, because
    a review could send back the revision the gate is judging. Both stand
    until the Review point is built (§1.1, §9);
  - budget: `--minutes` and `--tool-calls`. A delivered task over budget
    keeps its work: its worker is stopped (the Stop is submitted before the
    record, so a failed record is retried with it; a Stop that fails is
    pursued and escalated by the dispatcher) and the overrun is its own
    record, `task.over_budget`, written once the work reached the source's
    `spawn/<agent>` so it names the branch, the commit and the commit's
    diff stat; it counts toward no halt, and the objective's next task
    continues that work (§10.4, "A task over its budget keeps its work").
    The record names the measure that made it (`measure: "active"`, with
    the tool calls and elapsed time it measured), so a reader sees which
    budget ended the task; it is a continuation, not a failed attempt.
    `task.aborted` keeps its other meaning (a landing gone stale after its
    mission was done); records from before this were overruns recorded as
    aborts, and count toward the abort halt as they were written.
    The minutes are active time, never the wall clock (`integrate::active_ms`):
    from the delivery, through each event of the task's session, to now.
    Readings are no events here: the Bridge re-reads a busy pane every
    30 s (`screen.observed`, and the observer's `observation.recorded`),
    also while Claude Code only retries a model API that is down. A gap
    counts at most `SPAWN_BUDGET_GAP_SECS` (300 by default), and only if it
    ends in the model's progress: a tool call or background job starting
    or ending, a turn stopping. A gap that ends in a failed turn, a prompt
    arriving (`message.received`: a delivery, a nudge, a job's
    notification), a command (a delivery, a nudge, a resume) or the
    engine's own events counts nothing: the worker waited. The gap to now
    counts at most the cap. So a silence of any length or cause (the fleet
    stopped, the host frozen or rebooted, no live execution, the model API
    down) adds at most one cap, and no outage can by itself put a task over
    budget (2026-11-07 23:05: a five-hour stop aborted tasks 113 and 159 by
    the wall clock). One bound is not the cap: a subagent's job open across
    a model API outage retries out of the log's sight and, once it fails,
    ends; its gap counts the fleet's alive time, so up to Claude Code's
    retry window for a subagent, not the outage's length. The safe side: a
    model that thinks or streams longer than the cap without a tool call
    counts the cap.
    A gap inside a tool call or background job that later
    ended (a build, a gate in the worker) counts the time the whole fleet
    was alive in it: the log's gaps between those two events, each capped
    the same way, since an outage either kills the tool's engine or writes
    nothing anywhere.

  A gate needs a source that is a directory on the integrator's host:
  agents' branches reach no other (storage.md, "Landing"), so a gate on a
  URL is refused, and a mission configured with one before that is halted
  by the integrator with the reason.
- **Verdicts name what they judged.** A task's report has a revision (its
  `revisions`, one more each time it is sent back). `task.reviewed` and
  `task.gated` name the revision they judged (`spawn task accept|reject
  --revision R`, which the supervisor's digest gives; without it, a human's
  review takes the report as it is now). The checked functions refuse a
  verdict on a revision the task has left, and the fold applies none: a
  gate run on revision 0 never lands, or sends back, revision 1.
- **The integrator:** `spawn integrate` (unit `spawn-integrate`). A pass
  first records the owner's holds past their bound as expired (below,
  "Landing: the owner's hold"), then applies the owner's pending landings
  (below, "The owner's landings"), then visits the missions. A mission
  whose source it cannot fetch this pass is skipped: the mirror's last copy
  of `main` is no reading of the source's. A mission whose source has
  another mission's task in `landing` waits for it (the push may have moved
  `main` before its `task.landed`), unless that task waits behind the
  owner's hold on the source (its `task.held` names the active hold: it
  pushed nothing). Every pass visits every mission (its
  halts, budget, landing and objective assignment never wait for another
  mission's gate); only a landing makes the rest of its source wait for the
  next pass, and a task waiting behind a hold is no landing. A free gate goes to the ready task that has waited longest,
  across missions, counted from its report or from the end of its
  mission's previous judging, whichever is later; the mission id decides
  nothing. For a mission's lowest ready task (one per mission at a time:
  §10.4, "One gate at a time, for now"), on a thread of its own, it:
  1. fetches the agent's pushed branch, the source's `spawn/<agent>`, into
     its mirror's `refs/spawn-integrate/<agent>` (it never commits a
     workspace and never moves `refs/heads/<agent>`). A push that lags the
     agent's latest checkpoint is gated `stale` after a short wait: no gate
     run, no rejection, the task stays ready and is judged again later
     ([storage.md](storage.md), "Landing");
  2. builds the candidate, the landing commit C: one merge commit whose
     first parent is `main` (the base) and second the pushed head, with the
     head's tree when `main` is in it and the merge by `main`'s merge rules
     when `main` has moved, never a fast-forward to a worker's commit. Its
     message says what landed: the objective's title, the objective and the
     task on the first line (for a task without an objective, the brief's
     first line and the task), then the report's `## What changed` (the
     whole report without one, bounded, a line that could read as a
     `Spawn-` trailer indented), then `Spawn-Task`, `Spawn-Objective` and
     `Spawn-Head` trailers. So `git log --first-parent main` is one line
     per landed task, `git bisect start --first-parent` visits only gated
     landings, `git revert -m 1 <C>` undoes one task, and every checkpoint
     stays reachable through the second parents ([storage.md](storage.md),
     "Landing"). A conflict first merges `main` into the agent's
     idle workspace, with conflict markers, then sends the task back with a
     note that names the marked files ([storage.md](storage.md), "Main into
     a workspace"). A branch with a pending merge of `main` or with conflict
     markers is sent back as `conflict` before it is judged.
  3. measures the host before the gate starts: **the host's room is
     checked before a gate, not discovered by its crash.** For this
     judging's task only (one that got a gate slot; a task that waits is
     neither checked nor recorded), in this order, stopping at the first
     that falls short:
     - disk: the free KiB (`df`) of the gate's `TMPDIR` (`/tmp` when unset)
       against `SPAWN_GATE_MIN_FREE_GB` (default 10, the floor
       `scripts/disk.sh` reads too: one floor, one meaning);
     - memory: `MemAvailable` of `/proc/meminfo` in MiB against
       `SPAWN_GATE_MIN_FREE_MEMORY_MB` (default 2048; the integrator's
       only, not passed to the gate);
     - database: when `SPAWN_TEST_DATABASE_URL` is set in the integrator's
       environment (whatever the owner's-cluster check then decides for the
       gate), a connection within 10 s, then, in one transaction that
       changes nothing, `SET LOCAL max_parallel_workers_per_gather = 2`,
       `parallel_setup_cost = 0`, `parallel_tuple_cost = 0`,
       `debug_parallel_query = on` (`force_parallel_mode` before
       PostgreSQL 16) and `SELECT count(*) FROM generate_series(1, 100000)`
       within 10 s: a Gather with a worker, so the dynamic shared memory the
       tests need (`/dev/shm` in a container) is exercised. Any error fails
       it. Not for a mission whose scenarios hold `no-test-database` (§10.4):
       its gate never gets the URL, so a test server that is down does not
       hold back its work.

     A threshold of 0 disables its check; one that does not parse is an
     abort, as a bad `SPAWN_GATE_MIN_FREE_GB` is to the gate. A check that
     falls short runs no gate: the task is gated `no_room` (step 5) naming
     the check, its measure and its threshold. On 2026-11-06 a test server
     with a 64 MB `/dev/shm` failed "could not resize shared memory segment"
     under the tests' parallel queries and the gate read it, 25 minutes in,
     as a regression of the candidate; the probe finds it in seconds, before
     any candidate runs. The decision rests on measures taken at the
     judging, recorded with their thresholds (design.md §14);
  4. runs the gate on C, with a wall clock (`SPAWN_GATE_WALL_CLOCK_MINUTES`, 120 by default). When the gate
     rejects C and C is a real merge (the pushed head does not contain
     `main`, so C's tree is not the head's), it then judges the head alone
     before recording anything: the same gate, scenarios and task scenario,
     base `head_base` = `git merge-base <head> <main>` (the `main` the head
     last took in), candidate the pushed head. The head passes: the verdict
     is `incompatible` (below). The head fails too: `rejected`. The head
     fails a check on `head_base` as well (the gate's `candidate and base`,
     or a red main there): that base cannot judge it, and the merge's
     rejection stands, `rejected` and counted, with the merge's reason, log
     and note, `head_verdict` `unjudged`, a `head_reason` naming the check
     and `head_base`, and a note saying the work alone could not be judged
     there; never a host fault, which retried would find that base failing
     each time. The head's
     run gives no verdict otherwise: `aborted` (or `no_room`, or a host fault),
     naming the head. Such a record's log and fact are the head's run on
     `head_base`, never the merge's on `main`: it carries `head` and
     `head_base`, its reason begins "the merge with main <main> was rejected
     (<reason>); judging the work alone against the base it was built on,
     <head_base>, gave no verdict:", and the supervisor's digest item says
     the work was judged alone on that base (Supply PA's task 207,
     2026-11-09, read as main failing check-tests while its head_base was
     the red main). When
     the head contains `main`, a rejection of C is a rejection and no second
     run happens. The decision rests on two recorded verdicts on named
     commits, never on the text of a reason or on timing. The second run
     happens only for a rejected real merge and doubles that pass's gate
     time;
  5. records `task.gated` (verdict, log, metric, C as `candidate`) before
     git moves. A check before the gate that fell short (step 3), or a gate
     that had no room (exit 5 with its fact, §10.2), is gated `no_room`. This
     is no verdict on the work. The task stays where it was, and the record
     names `check` (`disk`, `memory`, `database` or `host scenario slot`;
     the gate's own floor is `disk`, and a slot wait is the host's scenario
     budget), `measured` and `threshold` (free KiB and the floor in KiB, with
     `free_kb`, `min_gb` and `tmpdir`; available MiB and the floor in MiB;
     the error and the probe it failed, with `server` as
     host:port/database or the socket directory, never the URL's password),
     `log`, `escalate`, and a reason that names the check, the measure and
     the threshold, ending "not a verdict, judged again later". It moves neither
     `gate_aborts` nor `gate_rejections`, so a disk that another project
     filled halts no mission. The fold keeps the mission's current run of
     them, which belongs to one task (`gate_room_task`, `gate_room_since_ms`,
     `gate_room_last_ms`, `gate_room_waits`, `gate_room_escalated`). These
     end the run: a gate that judges, conflicts or aborts, the task leaving
     the gate (`task.cancelled`, `task.aborted`), or `mission.resumed`. A
     `stale` leaves it. A `no_room` on another task starts a new run, with
     no inherited age, backoff or escalation. The integrator tries that task
     again only after a backoff from the latest `no_room`: 5 minutes,
     doubled each time, at most 40. The first `no_room` more than 2 hours
     into a run carries `escalate`. The dispatcher's digest takes that
     record to the supervisor, naming the check, its measure and its
     threshold ("no room for the gate on this host"). It stays pending until a delivered digest
     carries its tag (`host gate-room-<event>`), however long that takes, so
     it is told once.

     **A host fault is not a verdict on the work either.** When the gate
     rejects C with `regression: check-tests.sh fails on the candidate`,
     the integrator reads the failures check-tests.sh printed, from the
     gate's recorded output, in the check-tests section (from `| ==
     regression: check-tests.sh must still pass` to the next `| == ` or `|
     stage gate ` line). In the owner's form (one `| FAILED <test>: <first
     message line>` per failing test, then the excerpt), the FAILED lines
     are the failures and the excerpt is not read; in today's form (the
     40-line excerpt alone) a failure is a `| thread '<test>' panicked at
     <location>:` line with its message (up to a `note:`, `---- `, `test
     result:`, `--` or blank line), or the `| Error: ...` line of a test
     that returned an error. A failure the excerpt cut off is not seen. A
     failure is the host's when its message is a connection error to the
     test database (`error connecting to server`, `Connection refused`,
     `connection closed`, `server closed the connection unexpectedly`,
     `terminating connection because of crash of another server process`,
     `the database system is in recovery mode` / `is starting up` / `is
     shutting down` / `is not yet accepting connections`, `could not resize
     shared memory segment`, `SqlState(E57P0`, `SqlState(E080`), or a
     database error of any kind (`db error`, `Error { kind: Db`) while the
     test database's log (`SPAWN_TEST_DATABASE_LOG`, optional, lines as
     PostgreSQL writes them with `log_line_prefix` `%m [%p] `, `UTC` or a
     numeric offset) holds a crash line (`was terminated by signal`,
     `terminating any other active server processes`, `all server
     processes terminated; reinitializing`) stamped within the gate's run as
     the integrator measured it, one second either side. Unset or
     unreadable, the log is no evidence (said once). At least one failure
     read and every one the host's: `host_fault`, not `rejected` (and the
     head is not judged alone). The task stays where it was (`reported`, or
     `accepted` under review), its revisions and note do not move, nothing
     goes back to the worker, and neither `gate_rejections` nor
     `gate_aborts` grows or resets. The record names `base`, `candidate`,
     `revision`, `scenarios`, `reason` (`host fault: ...`, saying which
     evidence), `log`, `evidence` (`failures` when every failure is a
     connection error, else `server_log`), `failures` (per failure its
     `test`, its message's first line, bounded, and `class`, `connection`
     or `database`), `crash` (the log lines, when used), `host_faults`,
     `backoff_ms`, `due_ms` and `escalate`; the store refuses one without
     its evidence and writes `due_ms` itself, the record's own
     `recorded_at_ms` plus `backoff_ms`. The fold keeps the mission's host
     faults in a row (`gate_host_faults`, with the latest one's task and
     `due_ms`): + 1 on `host_fault`, unchanged on `no_room` and `stale`, 0
     on every other verdict, on `task.landed` and on `mission.resumed`.
     After the k-th in a row the task waits
     `SPAWN_GATE_HOST_FAULT_BACKOFF_SECS` (300 by default) x 2^(k-1), at
     most an hour: no pass gates it before the recorded `due_ms` (the
     mission's other tasks wait behind it, as behind a `no_room`), and the
     next pass after it judges it again. The one that makes the run 2
     carries `escalate`: the dispatcher's digest takes it to the supervisor
     once, as `host gate-host-fault-<event>` ("host faults at the gate"),
     naming the task, the mission, the count, the evidence (the crash lines,
     or the first failure's test and message) and when the task is judged
     again. A real failure among them, or none read, keeps it `rejected`;
     any other rejection reason is judged as before. The decision rests on
     the failures and the crash lines named in the record, the wait on the
     recorded `due_ms`, never on the reason's text alone or on time without
     a record (design.md §14).

     **A base that disagrees with itself is the host's too, not an abort.**
     The gate judges every objective scenario on the base before any
     candidate code runs, and an unsolved result needs two runs that agree.
     A first run that exits 1 and a second that exits 0 says nothing about
     the candidate: no candidate code has run yet, and the base is code that
     already landed. The gate states it itself and exits 4
     (`BASE_DISAGREES_EXIT`) with its own pair of lines at the end of its
     stdout: `| <name> exited 1, then 0 on the base: a harness error, not a
     verdict; the gate aborts`, then `| stage gate aborted <ms>ms base:
     <name> again (unsolved needs two runs that agree)`, with the same
     `<name>` in both (a scenario's file name, no space, no `|`), the gate's
     own trailing lines after them when it printed them — its `| base runs:`
     phase line and its `| gate time:` line, whose `setup <n> s` is the
     record's `setup_s` — and nothing else after the pair. That pair, with
     exit 4, is `host_fault` with the evidence
     `base_disagrees`, not `aborted`: the task stays where it was, its
     revisions and note do not move, nothing goes back to the worker, and
     neither `gate_rejections` nor `gate_aborts` grows or resets. The record
     names `base`, `candidate`, `revision`, `scenarios`, `reason`,
     `log`, `evidence` `base_disagrees`, `scenario`, `exits` ([1, 0]),
     `setup_s` (the whole seconds of the gate's `setup <n> s`, left out when
     it printed no timing line), `host_faults`, `backoff_ms`, `due_ms`,
     `base_disagrees` and `alert`. A base disagreement never carries
     `escalate` (020's escalation at the second in a row stays for its own
     evidence): the one that makes the mission's `gate_base_disagrees` 3
     carries `alert` true, and every other one, the later ones of the same
     run too, carries false. Until objective 030 exists that record is the
     alert — named by its event id, no second record of it — and the
     dispatcher's digest takes it to the supervisor once, as `host
     gate-base-disagrees-<event>`, naming the task, the mission, the count in
     a row, the scenario, `exited 1, then 0`, the setup time when known and
     when the task is judged again; 030 raises its `gate_base_disagrees`
     notice from the same record. The fold (`gate_base_disagrees`, beside
     `gate_host_faults`) keeps the mission's run of them: + 1 on a
     `host_fault` with that evidence, unchanged on `no_room` and `stale`, 0
     on every other verdict (020's host faults included), on `task.landed`
     and on `mission.resumed`. Both evidences share 020's run and backoff
     (`gate_host_faults`, `due_ms`). Every other harness error on the base — a
     first run that exits 2, `1, then 2`, `1, then 3`, the pair with any
     other exit status (0, 1, 3, 5, a signal) — stays an abort and counts,
     and so does a `| | …` line (a nested gate's, prefixed again) anywhere in
     the log. The decision rests on the gate's recorded exit status and the
     pair of lines named in the record (`scenario`, `exits`), never on the
     reason's text, on time or on a scenario's name; the wait rests on the
     recorded `due_ms`.
     **A scenario the host cannot judge is the host's, not the work.** Some
     objectives judge the host itself, and the gate's sandbox can never
     supply what they need: 029 needs a systemd user manager that delegates
     the memory controller, and the sandbox has no user manager at all (the
     owner's ruling, 2026-11-06). Such a scenario exits 6 and says why, with
     its own fact (`{"cannot_judge":true,"scenario","runs"}`): the same exit
     as a host fault, the key that tells them apart (gate.rs
     `GateFailure::CannotJudge`). On the base the gate records the result
     `cannot_judge`, once, in the base cache beside solved and unsolved; on
     the candidate a regression that exits 6 is skipped and a task scenario
     that exits 6 makes the gate state the fact and exit 6. The integrator
     records `host_fault` with the evidence `cannot_judge`: never a
     rejection, never an abort, no count toward a halt (and no count toward
     `SPAWN_GATE_HOST_FAULT_CAP`, like a base-only fault), under 020's
     backoff and escalation at the second in a row, the supervisor's digest
     naming the objective as judged outside the gate by the owner (mission
     7). Which objectives the gate cannot judge is said in
     deploy/systemd/README.md ("Which objectives the gate cannot judge") and
     in each scenario's header.
     The gate reports host faults of its own too: exit 6 with its fact in
     the verdict file (`{"host_fault":true,"scenario","on","runs",
     "reason"}`, gate.rs `HOST_FAULT_EXIT`), for a scenario's harness error
     on the base that is not the disagreement above (its two runs did not
     end 1 and then 0: an exit neither 0 nor 1, or 1 and then something
     else; `on` `base`), and for a check that failed twice on the candidate
     and on the base (`on` `candidate and base`, §10.2). Each is recorded
     `host_fault` with `evidence` `gate` and the gate's `fact`, under the
     same backoff, escalation and counts: `gate_aborts` does not move. The
     fact counts only from the integrator's own verdict file, which the
     sandbox never sees, and only with exit 6. A base that disagrees with
     itself stays the disagreement above: the gate keeps its exit 4 and its
     pair for it (`base_disagrees`), so the two never describe one run; a
     base-only fault of either evidence never counts toward
     `SPAWN_GATE_HOST_FAULT_CAP` and is never capped (§10.3).
     A task's report revision has at most `SPAWN_GATE_HOST_FAULT_CAP` (3 by
     default) host faults judged after the candidate ran (the test
     database's) against the same main, counted from its `task.gated`
     records whose `base` is the main being judged now (a fault on an
     older main says nothing of this one: Supply PA's task 207 had its
     01:45 fault on fcf5e1e still counted at 08:32; a head judged alone is
     recorded under main and counts there). The trade-off: every move of
     main starts the count again, so on a busy source a candidate that
     breaks the test server is capped only while main holds still; the
     escalation at the second host fault in a row still tells the
     supervisor. Every `host_fault` record names its `base` (the live
     store's 10 of 10, 2026-11-09), so none is left uncounted. The next
     one at that revision on that main is recorded `aborted` instead, its reason saying so and
     the fault kept under `host_fault`, so it counts toward the abort halt.
     A host fault is no verdict on the work, but one that keeps recurring on
     the same work may be the work's doing (candidate code leaving the test
     server broken for what runs after it), and an uncounted one would hold
     the task for ever. A base-only fault (the gate's fact `on` `base`, or
     the base's own disagreement) comes before any candidate code and
     counts toward no halt, so it neither counts toward the cap nor is ever
     capped. Nor does a check that failed on the candidate and on the base
     (`on` `candidate and base`): main fails it without the candidate, and
     counting it made a red main (Supply PA's check-tests, 2026-11-09
     00:48-03:42) cap every task's next gate as an abort, feeding the abort
     halt for a fault in no candidate.
     **Main is red** when a check fails twice on the candidate and then
     twice on the base, both base runs exit 1, and their `FAILED <test>:`
     lines share at least one test (the rule "an unsolved result needs two
     runs that agree", in one gate). The gate states it with its own fact,
     exit 6 (`{"host_fault":true,"scenario","on":"main","runs","failures":
     [{"test","message"}]}`, gate.rs `GateFailure::MainRed`). A base whose
     second run passes, or whose two runs share no failing test, stays `on`
     `candidate and base`. The integrator believes it only when some shared
     failure is classed the work's (host.rs `classify`), none is a
     connection or database error (a broken test server fails main and
     candidate alike, so a shared {assertion, connection refused} is the
     host's), and the test server's log shows no crash in the gate's window;
     otherwise it is a `candidate and base` host fault, as before (`base`
     when the base check below found it, before any candidate ran). A red is
     one record per source,
     `source.main_red` (`main`, `scenario`, `failures` with their class),
     the first of its red carrying `alert`; the store keeps one row per
     source (`mission_source_reds`: one row per source, whatever the check;
     a later red, on the same main or a later one, moves `main` on and
     keeps `since_ms`, with no new alert, and a red on a second check
     replaces the first's `scenario`) and GATED gives each mission its
     source's `red`. The task's gate is recorded `host_fault` with evidence
     `main_red` and `fact.on` `main`: no backoff, no run of host faults, no
     escalation, never counted toward the cap or a halt.
     While the source's fetched main is the red one, its gates wait: a
     ready task stays `reported`, no `task.gated` is written, and the
     integrator says so once. Each `SPAWN_GATE_MAIN_RED_PROBE_MS` (an hour
     by default) after the last red or probe, one gate runs anyway, a probe
     (`source.main_probe`, written as its gate starts, so a ready task that
     gets no gate that pass spends none; the store refuses one that is not
     due, and the gate then does not start; `probe` on its `task.gated`), so
     a candidate that fixes main is not starved. While a source is red, every gate on it is given
     `SPAWN_GATE_BASE_CHECK=<check>`: the gate builds the base first and
     runs that check on it twice before the candidate. A pass is written to
     the verdict file at once and kept in every fact the gate writes after
     (`base_check`), so the integrator records `source.main_green` whatever
     the gate ends with (a verdict, a host fault on another check, no room,
     an abort); it deletes the row (with the red's `since_ms`), and the
     gate goes on judging the candidate as usual. A mission on the source
     whose scenarios have no such check cannot run it ("not run"): it waits
     like the others, until main moves or another mission's gate finds main
     green. Two failures that
     agree are the red again, before the candidate is built. A main that
     moved lifts the hold at once (the next gate runs, with the base check).
     The supervisor's digest tells a red once (its `alert`) and a green
     once.

     **A verdict on a main that moved is `stale`.** When the gate ends with
     anything but `accepted` (rejected, conflict, incompatible, a host
     fault, no room, an abort, a judging that panicked), the integrator
     reads the source's main once (`git ls-remote`). If it is no longer the
     base the gate judged, the task is gated `stale` with what the gate
     found (`found`), main then (`base`) and now (`main_now`) and the
     found verdict's reason, and judged again on the new main. Nothing is
     counted: no rejection, no abort, no host fault toward
     `SPAWN_GATE_HOST_FAULT_CAP` or its run, no room count, and no
     `source.main_red` for a main that is no longer main. A read that
     fails records the verdict as found, with the read's error as
     `main_read` (main was never checked): it fails toward the behaviour
     before the check, where judging again on an unknown main could loop.
     Stale has no bound of its own; it is bounded in practice, since one
     gate runs per source and only landings, owner pushes and moves the
     owner records move main. Supply PA's mission 6 (2026-11-09):
     a gate started on ab21a25, main moved to 513a34e at 09:00:35, and at
     09:20:54 its host fault, capped as the third abort in a row, halted
     the mission. An `accepted` keeps step 6's compare-and-swap as its
     guard;
  6. moves `main` on the source with a compare-and-swap push
     (`--force-with-lease` on the base); if `main` moved meanwhile the
     verdict is `stale` and the task is judged again, unless the candidate
     is already in `main`'s history (an earlier push of this landing whose
     record was lost), which is a landing. Under the landing lock, before
     the push, it reads the owner's hold on the source from the store: on a
     held source it pushes nothing (below);
  7. records `task.landed` (`from`, `to` = C).
- **Landing: the owner's hold.** An owner deploy of Spawn gates a candidate
  against the live `main` for 25-80 minutes, then `deploy/deploy.sh`
  refuses unless the verdict's base is still the live `main`: a task landing
  in between makes the verdict stale (2026-11-06 21:45: task 85 landed
  during the owner's gate). Stopping `spawn-integrate` for the whole gate
  prevented that and stopped everything else too: every mission's gates
  (killed with no record) and objective assignment, which runs only in the
  integrator. The hold stops landings on one source and nothing else:
  - `spawn mission hold --source <source> [--reason <text>]` (owner only;
    the supervisor's login is refused). `<source>` is a mission source as
    the missions name it (the first source of at least one mission, whatever
    its state); a source no mission names, or a remote one, is refused and
    nothing is recorded. Under the integrator's landing lock (so a landing
    whose push has begun records its outcome first) it reads the source's
    `main` M and records `source.held` {`source`, `main`: M, `by`: `human`,
    `bound_s`, `reason` (at most 500 characters, when given), `renews`};
    the store writes `expires_ms`, the record's own time + `bound_s` x 1000.
    `bound_s` is `SPAWN_SOURCE_HOLD_SECS` in the command's environment, a
    whole number from 1 to 86400, 10800 (3 h) by default: longer than the
    slowest owner gate plus a deploy, and than the gate's default wall
    clock. Any other value is refused, naming the variable. It prints
    `{hold, source, main, expires_ms, renews}`: gate the candidate against
    that `main`. A hold on a source already held renews it: a new
    `source.held` naming `renews` (the active hold), which ends with it (no
    release or expiry is recorded for it). A source has at most one active
    hold.
  - `spawn mission release --source <source>` (owner only) records
    `source.released` {`source`, `hold`, `by`: `human`} and prints
    `{source, released, waiting}`. With no active hold it records nothing
    and prints `released` null, so a deploy may release on every exit path.
  - The fold keeps the active holds in `mission_source_holds` (by source:
    `hold`, `main`, `by`, `reason`, `held_at_ms`, `expires_ms`), set by
    `source.held` and removed by the `source.released` or
    `source.hold_expired` naming the hold; `spawn_control_refold()`
    rebuilds it, and each task's `held_by`.
  - While a source is held, no task lands on it. `land()` reads the hold
    from the store under the landing lock, before its push, never from
    memory, and pushes nothing. A candidate already in `main`'s history
    landed before the hold and is recorded `task.landed`; any other records
    `task.held` {`id`, `hold`, `source`} once per task and hold (the store
    records nothing for a second, and accepts one only for a task in
    `landing` and the active hold on its mission's source), and the task
    stays `landing` (`held_by` the hold). So a hold never splits a push from
    its record. The source's gates still run and record their verdicts (a
    judging thread's own `land()` records its `task.held`). A task waiting
    behind the hold ends nothing early: its mission's step goes on to the
    baseline check and objective assignment, its source is not counted as
    moved, and the mission's other ready tasks wait behind it (one task per
    mission between its gate and its landing); the other missions on the
    source are judged as usual. Assignment, the gates and landings of other
    sources, halts, budgets and `spawn mission moved` go on, and the halt
    for a `main` moved with no record stays.
  - After a record ends the hold, a waiting task lands at the next pass by
    the rules above: when `main` moved meanwhile (a deploy that recorded
    `source.moved`) its landing is `stale` and it is judged again against
    the new `main`, never discarded and never counted.
  - The bound. At the start of each pass, before any mission's step, the
    integrator records `source.hold_expired` {`source`, `hold`,
    `expires_ms`, `waiting`: the tasks waiting behind it, oldest first} for
    every active hold whose `expires_ms` the database's clock
    (`spawn_now_ms()`) has reached; the store refuses it before then and for
    a hold that is not the active one, so a release or renewal recorded first
    wins. Only that record ends the hold: past `expires_ms` without it
    nothing lands, and a hold that expired while the integrator was down is
    recorded expired at its first pass, before anything lands. The
    dispatcher's digest tells the supervisor once ("a source hold expired",
    session `missions`, tag `missions source-hold-expired-<event>`): the
    source, the hold (its event, reason, when it was taken and its bound),
    when it expired, each waiting `task #<id>` with its mission, and that
    landings resumed, so an owner verdict judged against the hold's `main`
    may be stale. A released hold is told to nobody.
  - Visible: `spawn mission list` and `GET /api/missions` carry `holds`
    (every active hold with its `waiting` tasks) and each mission's `hold`
    (the active hold on its source, or null); `GET /api/attention` carries
    `holds`, only those with a task waiting, beside `pending` and `told`
    (the supervisor is told of an expiry, never of a hold); `spawn task
    stages` shows `held`. A task waiting behind a hold is no landing stall
    (§3, "Stalls").
  - The owner's use (`deploy/hold.sh`, `deploy/deploy.sh`): hold the Spawn
    source, gate the candidate against the hold's `main`, deploy; the
    deploy releases the hold once `main` moved forward and `source.moved`
    is recorded, or on any failure. A hold forgotten or left by a crash
    expires by itself. Nothing in it stops or restarts `spawn-integrate`.
    Every decision rests on records read from the store under the landing
    lock (design.md §14); the expiry's only use of time is the recorded
    `expires_ms` against the database's clock, and what ends the hold is
    its record, not the clock.
- **On rejection:** the task goes back to the worker (`revise`), with the
  gate log as the note. A `rejected` decided after a run of the head alone
  names `head`, `head_base` and `head_verdict` (`rejected`), so the record
  shows why it counted.
- **On incompatible** (the head passes alone and fails only merged with
  `main`: a semantic conflict, like task 36 adding fields to a struct that
  task 48's new test builds): the task goes back (`revise`, one more
  revision), as on `conflict`, and it is not a rejection of the work. Its
  record names `base` (`main`), `revision`, `scenarios`, `candidate` (C),
  `head` (C's second parent), `head_base`, `head_verdict` (`accepted`),
  `main_commits` (`main`'s first-parent commits in `head_base..base`,
  oldest first, full hashes: what the work must adapt to), `reason` and
  `log` (C's) and `note`. The note says the work passes on its own base but
  not merged with `main`, quotes C's gate output (its tail), lists each of
  `main_commits` by its 12-digit prefix and subject, and says `main` is
  merged into the workspace with this revision: the dispatcher's delivery
  of the revision merges `main` into the idle workspace, as every delivery
  does.
- **Closure is one rule** (`spawn_task_may_continue` in `schema.sql`). Once a
  mission is closed, or a task's resident retired, work goes on only where
  it asks nothing new of a worker:
  - a done mission with a gate keeps its delivered, reported, accepted and
    landing tasks: reports still come in, are judged or reviewed, and land,
    and a delivered task is still held to the budget;
  - a retired resident's accepted and landing tasks still land;
  - a dropped mission keeps nothing.

  Every other open task is cancelled by the close or the retirement, for a
  done mission as for a dropped one. The fold of `mission.closed` or
  `resident.retired` cancels it in the tables, and the writer
  (`spawn_mission_close`, `spawn_resident_retire`) then appends one
  `task.cancelled` record for each task it cancelled. So `spawn task stages`
  ends with `cancelled` for every cancelled task, and no reader has to
  infer the stage. The fold never appends: rebuilding the tables
  (`spawn_control_refold()`) gives the same tables and adds no record. A
  database from before this rule gets the missing record once, on
  migration. This covers each cancelled task whose last `task.%` record is
  none of `task.cancelled`, `task.aborted` and `task.over_budget`. The
  integrator's selection uses the same rule, and no transition moves a task
  into a state the rule drops: a send-back is refused, a gate rejection
  cancels the task, and deliveries and reports are not recorded. A worker
  still working on a task the rule cancels is stopped, and a resident gets
  the task's work set aside and is told before anything else (§3.2, "When a
  task closes").
- **Halt rules,** per mission and read from the log:
  - three gate rejections in a row: `rejected` counts; `accepted` resets
    the count; `incompatible`, `conflict`, `stale`, `aborted`, `no_room`
    and `host_fault` leave it (three `incompatible` in a row halt nothing).
    In an objective mission, a run whose rejections are all one
    objective's tasks (`gate_rejection_objective`) halts nothing: that
    objective yields its slot instead, as one out of attempts does, with
    `rejections` on its `task.cancelled`. Any cancel of a task of the
    run's objective ends the run, whoever cancels it (that yield, the other
    rule's, or the owner's), so a later rejection is not mixed with a run
    whose objective is gone. A
    run across two objectives, or with a task of none, halts;
  - three gate aborts in a row (`no_room` and `host_fault` are not aborts,
    and leave the count);
  - the scenario count below its floor (the most scenarios a gate run of
    the mission has counted since it was configured or last resumed);
  - the source's `main` not where the last landing left it, read only
    from a fetch that succeeded, while no other mission's landing on the
    source is unresolved, while no owner's landing on the source is
    pending (step 0 below applies them first; one still pending, its
    landing failed, may have moved `main` before its record), and while the
    owner does not hold the source. The hold is exactly what says "the
    owner is moving `main`": on 2026-11-08 the owner held mission 6's
    source, pushed `main` and recorded the move afterwards, and a pass
    between the push and the record halted the mission. A move that no
    record explains and no hold covers (a push behind Spawn's back) halts;
    the halt's record names the source and both commits (`cause`:
    `main_moved`), so `spawn mission moved` can tell it from any other.

  A halted mission lands nothing until a human runs `spawn mission resume`,
  or resumes it from Needs you (`POST /api/missions/<id>/resume` {halt, key},
  as the `spawn_human` login through `spawn_human_resume`: the same
  `mission.resumed`, with `by` `human` and the `halt` it resumes, refused
  with the reason unless the mission is halted on exactly that halt, once
  per idempotency key). `/api/missions` names the halt a mission is in
  (`halt`: its `mission.halted` event, reason, time, and `gone`); a halt on
  the scenario floor names the scenario files gone since the gating that
  set the floor (that `task.gated` carries `scenario_names`, the halt
  `gone`). The floor guards against a task's candidate deleting its judge,
  not against the owner changing the set: the integrator's landing of an
  owner's request (`source.moved` naming its `request`) starts the floor
  again for the missions it moves, as a resume does (0, no names), and
  their next gate sets it from the scenarios then. That gate counts the
  deployed scenarios, since no task judging starts while main is ahead of
  its controller. A move with no request (`spawn mission moved`) keeps the
  floor.
  No rule counts host faults, however many in a row, whatever their evidence:
  a halt needs a human, and a host fault is the host's, so it is judged again
  after its backoff; 020's evidence (the test database) is escalated at two
  in a row instead, and a base that disagreed with itself raises one alert at
  three in a row (its record's `alert`), never a halt.
- **A shared source has one baseline.** A landing moves the baseline of
  every mission on that source whose baseline was its `from` and that is not
  halted (the record names them), so RSI and fleet work on the same
  repository don't halt each other, and a move of `main` that no landing
  explains still halts the missions that saw `main` before it — unless
  that is exactly the halt the move is: a mission halted because `main`
  was neither its baseline nor explained, at this source, from this commit
  to this one (`cause`: `main_moved` in its `mission.halted`), is named in
  the move's `clear` list, and the fold unhalts it as it moves its
  baseline. The owner's record is the explanation its halt was waiting
  for; a halt for any other reason keeps its baseline. A deploy that moves `main` forward records
  `source.moved` (`spawn mission moved`, run by `deploy/deploy.sh`), and the
  missions on that source take the new `main` as their baseline instead of
  halting.
- **The owner's landings** (objective 21). The owner lands on a mission
  source with one command and never stops the integrator for it:
  `spawn mission push --source <source> --commit <commit>` (owner only;
  `<commit>` is resolved in the git repository of the current directory,
  `--repo <dir>` names another, or is already in the source). It puts the
  commit in the source at `refs/spawn-owner/<hash>`, touching no branch,
  records `source.push_requested` {`source`, `commit`, `ref`, `by`:
  `human`} and prints `{request, source, commit}` (`request`: that event's
  id); `--wait <seconds>` also waits for the outcome and prints it, exit 0
  applied, 1 refused, 3 still pending. The source must be a directory on
  this host with a mission (a remote one is refused, as a gate on it is).
  The running integrator applies it as **step 0** of its next pass (after
  the holds past their bound are recorded), before the steps of the
  missions on that source: every pending request (no outcome names it yet),
  oldest first, under the landing lock (`spawn_landing_hold`'s, so no task
  landing's push and its record are ever split by it). The owner's hold
  (above) stops task landings, not the owner's own: a request on a held
  source is applied too, as `spawn mission moved` is recorded (an owner
  gating a candidate against the held `main` does not push meanwhile). It reads the source's `main` M from the source itself
  (never its mirror, which a judging may be using); when M is in the
  commit's history it moves `main` from M to the commit, otherwise to a
  merge commit (first parent M, second the commit, by `main`'s merge rules
  and the landing identity, its message naming the owner's landing, the
  request and the commit's subject), with a compare-and-swap
  (`update-ref <branch> <new> <M>` in the source); in the same hold it
  records `source.moved` {`source`, `from`: M, `to`, `missions`, `request`,
  `by`: `human`}: `missions` are the missions on the source at M and not
  halted, as `spawn_source_moved` names them, and the fold moves their
  baselines; the missions on the source with no baseline yet (new, or
  resumed) are named in `without_baseline` and take `to` as their first,
  so a gate of theirs judging against M when it landed does not leave them
  at M. A conflict, a commit the source does not have, or a
  compare-and-swap that fails twice records `source.push_refused`
  {`request`, `reason`}: nothing moves and nothing halts. A request whose
  commit `main` already holds was applied by an integrator that stopped
  before its record: it is recorded then, from the baseline its move began
  at. While a task landing on the source went out without its record, the
  request waits for it. It never moves `main` under a gate of its source
  (design-owner-push-between-gates.md, D1): while a judging of that
  source is in flight the request waits (said once), and the pass that
  reaps that judging applies it before any new gate starts, so the
  judging's verdict is on the `main` it ran against, never `stale` because
  of the owner (on 2026-11-09/10 seven Supply PA gates went stale under the
  owner's pushes). A judging of another source keeps running and lands by
  the usual rules. With `--gated` (and `--reason <text>`, on the request)
  the request is judged before it lands, as a judging of its source
  (design-owner-push-between-gates.md): step 0 checks it as above and the
  integrator starts its owner judging ahead of the source's ready tasks,
  never in place of a running judging and within the same caps
  (`--gates`, the CPU share). The candidate is the commit when M is in its
  history, else the owner's landing merge on M, built in the mirror; the
  base is M; the scenarios are the candidate's own copy of the mission's
  directory with `protected` emptied (the owner may change both), and no
  red check runs on the base (an owner's push is how a red `main` is
  fixed). A new contract's scenarios are unsolved on M (M has no such
  code), so they are objectives nobody solved and the regression rule does
  not apply to them: the gate does not reject the push for them, only for
  its checks and the scenarios M solves. A check `main` is red on is
  unsolved on M too: the owner's gate runs with `SPAWN_GATE_RED_UNSOLVED=1`
  (gate.sh), which says it and judges the rest, so a push during a red
  `main` is neither held nor refused for a red it did not cause (else it
  would have to go ungated). Only the tests `main` fails are unsolved: a
  test the candidate newly fails inside the red check, or a run of it that
  names no failed test, is the push's regression. The owner's gate also
  runs with `SPAWN_GATE_OWNER_CANDIDATE=1`: a regression that fails on M
  too, run again after the candidate's runs, is a validator's defect, not
  the push's, and the gate judges the rest (a task's gate gives no verdict
  then, a `host_fault` with evidence `validator_defect`). Accepted: under the landing lock, the
  candidate is pushed to `main` with a lease on M, and `source.moved`
  names `verdict` `accepted`, with the gate's `validator_defects`,
  `quarantined`, `passed` and `reused` when it names any (the
  `validator_defect` alert; a task's `task.gated` carries `passed` and
  `reused` likewise). Rejected (or a candidate without the
  mission's scenarios directory, `no_scenarios`): `source.push_refused`
  with `verdict` and the gate's `log`; nothing moves and no mission counts
  it. Both records of a judged request (`source.moved` accepted,
  `source.push_refused` rejected) carry `gate_ms`, the gate's wall clock,
  and `mission`, the id of the mission whose gate judged it (objective
  061): the scorecard's gate time counts the run for the fleet and for
  that mission (never the moved `missions`), beside the tasks' gatings. No verdict (the host's fault, an abort, which is a harness error, no
  room, a gate that could not run): no outcome is recorded, and the request is
  judged again after the host-fault backoff. When the gate ran, what it
  printed (stdout, then stderr marked `! `, its last 4 MiB) is kept as
  `<root>/no-verdict-logs/<ms>-request-<request>-<judging>.log` (the
  newest 32 kept, the older removed as each is written), and
  `source.push_no_verdict` {request, reason, log: that path or null,
  judging} is recorded, folded into no table and no outcome (the request
  stays pending); the journal's no-verdict line names the path too. It is
  judged again once: a gate that ends with no verdict for the `reason` the
  request's `source.push_no_verdict` just before it carries (the same
  failure on the same scenario; the log's path, new at every judging, is
  not part of it) is not a fault that passes, so the request is refused,
  `source.push_refused` with `verdict` `no_verdict`, `kept_log` (that
  judging's log) and the reason "no verdict twice for the same reason
  (<reason>): classify it (candidate failure, test defect or
  infrastructure) before requesting again", and its source goes on (on
  2026-11-11 request 131083 was judged three times for one deterministic
  fault, with nothing able to land behind it). The store's records decide,
  so an integrator that restarted between the two judgings refuses all the
  same. Another reason grows the backoff as before. Waiting for room and
  a gate that could not start are no fault of the request: they back off
  however often in a row and are never refused, and a judging that
  recorded nothing (the room check before the gate, a gate another
  integrator reaped) never counts.
  Every judging's gates, task or owner, whatever they end with, keep their
  files: when a gate's private directory in a judging's directory goes,
  its `stdout` (the per-scenario lines at its end), `stderr`, `verdict` and
  `status` (never its `env`) are copied first to
  `<root>/gate-logs/<judging>/<gate>/`, each its last 4 MiB; the newest 200
  judgings are kept, at most 2 GiB in all, the oldest removed first. The
  judging's records name that directory (`gate_logs`) once a gate is kept. With `--urgent` (gated or
  not; for a fix of a red `main`, D2) the request does not wait out a
  task's judging in flight on its source: that judging is stopped (its
  gate's process group killed, as at its wall clock) and records itself
  discarded by the request, `task.gated` `stale` with `found` `discarded`,
  the `request` and the owner's reason, counted nowhere; the task is
  judged again on the new `main`, and the request is applied or judged at
  the pass that reaps the stopped judging. An owner judging is never
  stopped for one. `spawn mission push --withdraw <request>` (owner only)
  withdraws a pending request, whether it waits, backs off or is being
  judged: it records `source.push_refused` {`request`, `reason`:
  "withdrawn by the owner"} under the landing lock and prints `{request,
  state, outcome}`, so `spawn mission pushes` and a `--wait`ing push see a
  refusal. A landing takes that lock and asks whether the request is still
  pending before it moves `main`, so a withdrawn request never lands: an
  owner judging in flight for it is stopped at the integrator's next pass
  (its gate killed, as a task's is for an urgent request) and whatever it
  judged, an acceptance too, is discarded. A request that ended already,
  or that does not exist, is an error and records nothing; so is one
  whose commit `main` already holds (its landing went out without its
  record, which the next pass writes: a refusal would contradict `main`),
  and so is `--withdraw` given with any other flag of a push. So the owner's landing is a record
  (the request, then the outcome that names it), and the integrator acts on
  that record, never on a `main` that changed under it ([design.md](design.md)
  §14). `spawn mission moved` still records a move the owner made behind
  Spawn's back; updating the controller (`deploy/`) stays the owner's step.

### 10.4 RSI as a mission

- **The RSI mission** is an objective mission: one with a backlog and a
  resident (`spawn mission configure --backlog <dir> --resident <name>`). On
  the live host:
  - source: `missions/spawn.git`;
  - gate: `rsi/scenarios`;
  - acceptance: `gate`;
  - budget: 45 minutes and 60 tool calls;
  - worker: the resident `rsi`, approved by a human.
- **Tasks come from the backlog** (`rsi/backlog/*.json`, in the controller:
  an objective committed to the owner's checkout is assigned only once
  deployed). A task in flight is judged with its objective's scenario in the
  mission's scenarios directory as configured now, not the path recorded at
  assignment.
- **The worker limit** N (`--workers N`, 1 to 16, stored as `"workers": N`
  in the mission's objectives; default 1) is how many of the mission's
  tasks may be in flight at once. A task is in flight from `task.assigned`
  until it lands or is cancelled (assigned, delivered, revise, reported,
  accepted, landing).
  - Each integrator pass fills the free slots with the open objectives
    that have no task in flight (an objective never has two), fewest
    attempts first: fewest yields, then fewest failed attempts since the
    last yield, then the lowest id. With no failures this is the lowest open
    objective, as before. `spawn_objective_assign` checks both rules again
    under the append lock, so a second integrator or a stale read cannot
    oversubscribe, while routing objective assignments through the agent
    grading catalog (`brief-v3` classification on objective title/prompt and
    continuation/rework preservation; see [agent-grading.md](./agent-grading.md)).
  - **No objective holds a slot for ever.** A failed attempt is an outcome
    that is evidence against the work, and only that: the gate's `rejected`
    or `incompatible` (the work, as delivered, failed a check or failed
    merged with main; the gate cannot tell a host's cause, and the yield's
    reasons reach the supervisor), or an abort the active-time budget made
    (a record that names `measure: "active"`, with the `active_minutes` and
    `tool_calls` it measured). A `conflict` never counts: main moved under
    the task (another landing, an owner batch), and a worker that cannot
    resolve it shows up as rejected or over budget. An abort without a
    measure never counts either: the old wall-clock budget's (it aborted
    tasks a restart had kept waiting) and the store's for a landing gone
    stale after its mission was done. A budget overrun is not one either:
    it says the task is bigger than its budget, not that the work failed;
    its record names the measure and the objective's next task continues
    the work (below). The host's outcomes (`no_room`, `host_fault`,
    `stale`, a gate `aborted`) count for nothing, as for the halt rules,
    and an `accepted` verdict ends the count. (On 2026-11-08 at 04:47,
    right after an owner batch, the earlier rule set aside tasks 166 and
    167, both reviewed and passing, on old wall-clock and tool-call aborts
    and on conflicts from the batch; each had one rejection.) Once an
    objective has `SPAWN_OBJECTIVE_ATTEMPTS` (3, a whole number from 1)
    failed attempts since its last yield, its task yields the slot as soon
    as it is being worked again (assigned, delivered or `revise`; a
    reported task waits for its gate, whose verdict may land it): the
    integrator stops its worker (`integrate-yield-<task>`) and records
    `task.cancelled` with `yielded`, `objective`, `failures`, `limit` and a
    one-line `reason` (the store adds `by: spawn-integrate` and refuses
    any other cancel by the integrator). Its work is set aside like any
    cancel's, no halt counts it, the supervisor's digest tells it once
    (`missions objective-yielded-<event id>`), and a resident's cancel
    notice gives the reason, and records from before an overrun was its
    own record still count as they were written (an abort over budget that
    was its objective's limit-th failed attempt carried `yielded`, the
    count and a `yield_reason`). The objective goes to the back of the
    queue. A task whose gate runs now, or that this pass
    aborted, does not yield; the slot an abort frees is filled from the
    next pass on, fewest attempts first, so the aborted objective is not
    re-issued at once. Before this, on 2026-11-07, three objectives whose
    tasks kept failing held mission 4's three slots for a day (an abort was
    followed by a new task for the same objective five seconds later), and
    objectives 025 to 034 were never assigned.
  - **The owner's own objectives are never assigned** (`"owner_judged":
    true` in a backlog file; `rsi/backlog/029.json` is one: its scenario
    needs a systemd user manager, which the gate's sandbox has none of). The
    integrator leaves them out when it fills slots and when it names the
    next objective, so such an objective costs no resident, no task and no
    retry loop (before this, a host fault with evidence `cannot_judge`
    re-issued it every backoff, forever). For order it still counts as open:
    an objective that comes `after` it waits until it is solved (the
    owner's work is what its dependents build on), and the backlog's order
    check reads it as any other. It stays open in its mission until it
    lands there (openness reads landings only), which is why the owner
    sees it listed rather than solved. That makes a dependent in another
    mission wait forever: 029's work lands in mission 7, so mission 4's
    029 never closes, and an `after: [29]` written into mission 4's
    backlog could never be satisfied. The order check would not see it
    either (029 is in the graph and open, so nothing looks wrong): a
    dependency is on a landing in the same mission. `spawn mission objectives` lists the
    owner-judged and the fleet's open objectives apart for every objective
    mission, and the board shows such an objective with status `owner`
    (`owner_judged: true`), its landings as any other's. A task for such an
    objective that is somehow in flight is judged as any other: the
    `cannot_judge` host fault and its backoff are unchanged (this is
    prevention, not a new verdict). The field is read by `load_backlog`;
    a value that is not a boolean (`null` included) is refused, never read
    as true or false.
  - **An objective already solved on the base.** A one-shot objective is
    solved once a task for it lands (below); it is also solved when its
    scenario passes at a base commit main already holds, because then no
    work of a task can change anything. Mission 4's task 182 (objective
    34) was rejected three times on 2026-11-08 — "the work changes nothing
    on main" twice, then "scenario already passes on the base" — after the
    owner batch had landed the behaviour already, and the third rejection
    halted the mission. Now: a gate that says the objective's scenario
    passes on the base (its base check, before any candidate build) is a
    fact about the objective, not about the task's work. The integrator
    records `objective.solved` {`id`: the mission, `objective`, `task`,
    `scenario`, `base`: the commit, `log`: the gate's}, stops the worker
    and ends the task as `task.cancelled` on that record (`solved`: its
    event id, `by: spawn-integrate`): no rejection, no failed attempt, no
    halt. The assignment rule reads `objective.solved` as a landing, so the
    objective is not assigned again, and an objective that names it in
    `after` is not held by it. A candidate whose tree is main's tree is not
    rejected at once in an objective mission either: it is judged, and the
    gate's base check answers the same question (cheaply: the base is
    cached by the deploy's warm-up). A gate verdict on the base that
    disagrees with itself is a host fault (§10.3), unrelated.
  - **Named order.** A backlog objective may name the objectives it comes
    after (`"after": [28]` in its JSON, versioned with the backlog): it is
    not assigned while one of them is open (one the backlog does not hold
    must have landed). The dependency is on a landing in this mission
    (openness reads the mission's own landings): an objective whose work
    lands elsewhere is never seen solved here, so a dependency on one
    waits forever, and the order check cannot see that (the objective is
    in the graph and open). Only 032 names one (after 028, whose wait paths
    its soak exercises); the owner's preferred order of 028-033 stays prose,
    since those prompts say each may be solved before or with the others.
  - Who gets which task: the resident takes a task whenever it has none
    open in the mission; every other task goes to a new per-task worker
    (kind `ephemeral`, agent `agent_worker_objective_<task>`, session
    `session_worker_objective_<task>`). It needs no approval, starts from
    the source's main as it is then (or from its objective's earlier work,
    below), keeps no context, and is stopped by the
    dispatcher once its task lands or is cancelled (`retire-task-<id>`). Its
    report reaches the supervisor's digest only in a review-mode mission;
    a gate-mode mission's reports do not (§10.3). With N = 1 this
    is one task at a time on the resident, as before.
  - Landing stays one at a time per mission: one candidate judged at a
    time, the lowest ready task first, main moved by compare-and-swap.
    Across missions the gate goes to the longest wait (§10.3), so a
    mission that is always ready does not starve another's. A task whose base is
    no longer main lands as the merge by main's rules; two in-flight
    objectives on the same lines make the second a `conflict`.
  - Budgets apply per task, in active time from its delivery (§10.3); the halt rules stay per
    mission (three rejections in a row across its objectives halt it;
    three of one objective's tasks yield that objective instead), and
    a halted mission gets no new task and no judging until a human resumes
    it.
  - The brief is the objective, the working rules, the work its earlier
    tasks left without landing, and the gate verdicts of its recent
    attempts, all read from the log at assignment. For each of the last
    three tasks of the objective that were cancelled, aborted or yielded:
    its id, its outcome and reason (its last gate verdict, abort or
    cancel), its last checkpoint (the latest `artifact.committed` of its
    session while it ran, not a set-aside), and its report, with the
    instruction to use the reports and not redo what they established (task
    161 rebuilt 2264 lines that task 155 had mostly written). The brief
    names no branch: a worker has no `.git` and cannot read one. The first
    prompt of a forked task (below) says its workspace already contains
    that work.
  - **A task over its budget keeps its work.** The integrator stops a
    delivered task over its mission's budget and records
    `task.over_budget` {`reason`, `tool_calls`, `elapsed_ms`, `budget` (the
    mission's, as it was), `branch`, `commit`, `diff`, and `continues` when
    it continues an overrun}; the record is written once the Stop's final
    checkpoint reached the source's `spawn/<agent>` (read from the source
    after the Stop, never assumed), so `commit` and `diff` (`git diff
    --stat main <commit>`) name the work; the wait lasts at most
    `SPAWN_KEPT_WAIT_SECS` (30, a whole number of seconds) after the
    checkpoint, and a push that failed is said so instead (`pushed: false`,
    in the log and in the next task's brief). It is never a `task.aborted`:
    the mission's `gate_aborts` and `gate_rejections` stay as they were, no
    run of either is broken or started, and the task leaves flight without
    a report or a gate. The objective's next task is assigned once that
    work is on the branch, its `task.assigned` names `continues` (the
    overrun task) and its record carries `budget`, and its brief names the
    overrun task (#id, its objective, the reason and the tool calls), the
    branch, the commit and the commit's diff stat, and says the work is in
    its workspace to continue, not to redo; it goes to a new per-task
    worker, whose workspace the dispatcher forks from that work as for any
    earlier task's work (a resident's own branch no longer holds it: its
    cancel set the work aside), so the worker starts on the work, not on
    main. Its budget is the mission's; it
    may be raised once, for that task only, to at most twice the budget.
    A **second overrun** of a task that continues one is recorded the same
    way and escalated to the supervisor once: a digest item tagged `host
    over-budget-<record>`, naming both tasks (#id each), the objective, the
    branch and its diff stat, the budget and the tool calls, and what can
    be done (split the objective, raise the budget with `spawn mission
    configure`, or take it over). The objective is then held: no task is
    issued for it until the mission is configured again (a
    `mission.configured` record newer than the overrun's), while the
    mission's other objectives take the freed slot. A third overrun after
    such a configure is treated as a first. Everything is read from the
    records (§14), never from the integrator's memory.
  - **An ephemeral task starts from that work.** When the newest earlier
    task of its objective that ended without landing left work (a
    checkpoint of its own while it ran, before its set-aside if it was a
    resident's), the dispatcher, before the task's Start, creates the
    worker's branch at that task's last such checkpoint (never the
    set-aside, nor the merge of main a resident's next delivery put on top
    of it), and merges main into it the way a worker's delivery does
    (storage.md, "Main into a workspace"): cleanly, as a `base_merged`
    checkpoint; or, on a conflict, in the workspace mounted for the merge,
    with markers where they show, committed (a `base_merged` checkpoint
    naming no file) so the branch carries them and the Start's mount keeps
    the pending merge, the workspace unmounted again; a conflict markers
    cannot show leaves main's copies under `.spawn/main/`, which the
    Start's mount puts back. The Start's prompt says the workspace already
    contains that task's work (its commit) and what the merge did (the
    files it changed, the files with markers to resolve first, or the
    copies). A checkpoint that holds no change main lacks (in git: nothing
    between its merge base with main and it) is no work: the task starts
    from main. A fork that fails holds the Start, so the task does not
    start from main over work it should continue; its first failure is
    recorded (`task.fork_failed`, once per task: the predecessor, its
    commit, the error), and once that record is `SPAWN_FORK_GIVE_UP_SECS`
    old (300, under the delivery stall's 600) the Start goes without the
    fork, from the branch as far as the fork got or from main, its prompt
    saying so and pointing to the brief's reports. With no such earlier
    work its branch forks from main at the Start, as before; a resident
    keeps its own branch. (A worker's workspace is an export without
    `.git`: it cannot read another branch, so the work has to be in its
    tree.)
  - The task's scenario is `rsi/scenarios/<id>.sh`.
  - A one-shot objective is solved once a task for it lands. A recurring one
    (`recurring`, `metric`, `target`) stays open until the metric of its
    latest landing meets the target; the metric comes from the gate's
    verdict, never from scenario output.
- **The RSI page** reads the objective missions: `/api/objectives` lists each open
  objective mission with its backlog's objectives (status, landed metric
  values), its tasks with their `task.gated` verdicts, the halt reason and
  the baseline (the shape is documented in `crates/spawn-cli/src/serve.rs`);
  every view of the backlog (`/api/objectives`, `/api/overview`'s RSI summary,
  `spawn mission objectives`) reads the integrator's records as the
  integrator does: an objective with an `objective.solved` record of its
  own mission is solved, as a landing makes it, and its row names the record
  (`solved_on_base`: `{event, task, base}`, null otherwise; the page says
  "solved on the base at <base>, task #<task>"), and `next_objective` is
  what `objectives::choose` gives one free slot from the integrator's own
  inputs (the landings with those records, the objectives with a task in
  flight, the attempts and the held objectives), null when it would assign
  nothing. Mission 4's objective 34, solved on the base on 2026-11-08, was
  still shown open and next three days later before this;
  `/api/tasks/<n>/gate` answers one task, `/api/objectives/<m>/<n>`
  one objective's whole prompt; `/api/objectives/pane` shows the
  resident's terminal.
- **One gate at a time, for now.** A judging (fetch, gate, landing) runs
  on a thread of its own. A mission has at most one in flight, and so does
  a source (missions on one source share its mirror and its main); while
  it runs, the integrator's passes leave that source's mirror alone (no
  landing, fetch or judging on it), but every mission still gets its
  halts, budget and objective assignments. At
  most `--gates N` (`SPAWN_INTEGRATE_GATES`) run at once across missions,
  and **N is 1**: more than one gate is unsafe until each gate has a test
  database server of its own (GitHub issue #6, which also covers the
  server state candidate code leaves behind). Today every gate reaches the
  same test PostgreSQL server, and its gate login is a superuser, so one
  gate's candidate code can disturb another gate's base runs (an
  "unsolved" is cached, and a regression then lands) or flake another
  mission's candidate toward its halt. The integrator refuses N > 1 (up to
  8) unless `SPAWN_TEST_DATABASE_PER_GATE=1` states that the server is per
  gate. A mission whose scenarios directory holds the file
  `no-test-database` (its owner's statement that its gate needs no test
  database, as Supply PA's tests run on PGlite) gets no
  SPAWN_TEST_DATABASE_URL, so its candidate code cannot reach the shared
  server: its judgings do not count against N and run beside a Spawn gate,
  still one per mission and per source. **CPU room.** The integrator
  gives every gate its `SPAWN_GATE_JOBS` (scenarios at once), never
  gate.sh's own default: the owner's `SPAWN_GATE_JOBS` when set; 1 for a
  gate on the shared test server; otherwise the host's cores less
  `SPAWN_GATE_WORKER_CORES` (cores kept for the workers and the host; a
  whole number from 0 to cores - 1, default a quarter of the cores, at
  least 1), divided among the gates that can run at once (one per source,
  those on the shared server at most N), at most 8. A gate starts only
  when its jobs, the running gates' jobs and the kept cores fit within the
  cores, or when no gate runs; otherwise it waits for a later pass, said
  once, like a gate over N: no record and nothing counted against the task
  (unlike `no_room`, there is nothing to measure: the integrator knows the
  jobs it gave). A setting that does not parse stops the integrator from
  starting, naming it. On 2026-11-08, with `SPAWN_GATE_PG_BIN` set, each
  gate took half of the 9 cores; two or three gates and the workers made a
  load of 18-20, and task 172's gate failed on its base. **Host scenario
  slots.** The jobs above bound one gate; the host's whole scenario budget
  is separate and `scripts/rsi/gate.sh` takes it itself: every scenario
  run, from every gate, first takes one of the host's slots. The number is
  the host's own statement in `$HOME/.cache/spawn/gate-slots/host_jobs`,
  which the deploy writes from the integrator's gate environment and the
  owner may edit; without it, `SPAWN_GATE_HOST_JOBS`, then the gate's
  `SPAWN_GATE_JOBS`, then the hand-run default min(8, cores/2). The file
  outranks the environment so a hand-run shell cannot raise the cap, and a
  gate started with a larger value cannot exceed it; the gate only reads
  the file. The slots themselves are files under
  `$HOME/.cache/spawn/gate-slots`, locked by flock on an fd of the run's own
  subshell. The claim is in the gate, not here, so an integrator's gate, an
  owner's hand-run and the deploy's base-cache warm-up share one budget
  with nothing to configure; the fd's close releases a slot, so a killed
  gate loses none, and a gate with two runs holds two slots (one each). A
  waiting run says so while it waits ("waiting for a host scenario slot"),
  and its block then records how long it waited and the slot it took, the
  way the room no-room fact records disk. The build hook, the test listing
  and the snapshot take no slot (one each per gate, and gates are few). Without the budget, on 2026-11-08 an owner
  gate plus a fleet gate, and after the deploy a warm-up plus a fleet gate,
  ran four heavy scenarios on 9 cores, and two tests with real-clock
  windows — `a_terminal_hosts_its_runner_only_while_the_launch_lives_in_it`
  (a dead tmux pane's exit status unrecorded after 30 s) and
  `dispatch::resume_replay::a_resume_that_succeeded_and_died_young` — failed
  under it and rejected tasks 183, 184 and others.
  **The base warm after a landing.** A gate judges against main, so after
  every landing the next gate on that source had no base results cached and
  judged the base again (36% of 54 gates' time on 2026-11-08; 1158 s for
  the first gate after one landing). So the integrator warms it: after a
  `task.landed` (recorded in the judging's thread and handed to the main
  thread) and after an owner's `source.moved`, it starts one
  `gate.sh --base-only <mirror> <new main> <scenarios>` in the background,
  never waited for, its exit decides nothing (its log is
  `~/.cache/spawn/gate-warm/<commit>.log`, its line in the integrator's
  output). One at a time: a warm main moved past is abandoned (TERM, then
  KILL after 10 s), what it finished stays cached for its commit, and a
  commit is warmed once. A `--base-only` run is the lowest claimant of the
  host's slots: it takes one only when no gate is waiting for one (a demand
  lock beside the slots), runs one job, and builds with
  `CARGO_BUILD_JOBS=1`, so its whole footprint is at most one slot no gate
  wanted and one compiler job; a gate that arrives while it holds a slot
  waits out that one job, two runs at most (no preemption), and shares the base results
  through the base cache's per-key locks. It cannot outlive the integrator
  unaccounted for: it runs under `setpriv --pdeathsig TERM`, in the
  integrator's process tree and cgroup, and a starting integrator stops a
  predecessor's warm only when `gate-warm/warm.pid` still names it (this
  user, the recorded start time, `gate.sh --base-only` of the recorded
  commit). `SPAWN_INTEGRATE_WARM=0` turns it off. The deploy's own warm-up
  is the same `--base-only` run and claims as low. A warm's build hook
  and test listing build at `CARGO_BUILD_JOBS=1` inside the sandbox; any
  other gate builds with cargo's own default unless the owner sets
  `SPAWN_GATE_BUILD_JOBS`. A warm takes its slot before a base entry's
  lock, and that lock only without waiting (busy: another gate judges the
  key, and the warm skips it), so a gate never waits on a key a warm holds
  while the warm waits for a slot. Each judging records its own verdict and landing, which the next
  pass reads; finished judgings are reaped before a pass reads the
  missions. A judging that panics before any record of its own is
  recorded `aborted` with the reason, so the three-aborts halt bounds a
  bug. So is a base or a candidate that is empty or whitespace (evidence
  `parameter_error`), before any git work or gate run, and never turned
  `stale` by a main that moved. One that returns an error records nothing: the error is shown to
  the operator and a later pass judges again (a git that cannot run a
  merge is the host's problem, not the work's: objective 008).
  TERM kills the gate with the process (no record), and the next
  integrator judges the task again from nothing, when its gates run as
  its children. When a systemd user manager answers, each gate runs in a
  unit of its own (gate v2 step 2, design-gate-survives-restart.md): a
  stop or a restart stops no gate, the next integrator adopts each one
  still running and records it once, and deploy/deploy.sh stops such an
  integrator at once, without a drain (deploy/controller.sh keeps the
  controller a running judging's gate came from). A drain does not: a
  drain request (SIGUSR1, or `spawn integrate --drain` with the same
  root, which waits until that integrator is gone) records
  `integrator.draining` {`gates`, `bound_s`}, starts no gate, owner
  judging or base warm from then on, keeps its passes going otherwise,
  and once its gates recorded and their accepted tasks landed records
  `integrator.drained` and exits 0. At `SPAWN_INTEGRATE_DRAIN_SECS`
  (default 7200, from the draining record) it kills the gates still
  running (their process groups; nothing is recorded for them), records
  `integrator.drain_expired` naming their tasks (gates in units are not
  its processes: they keep running, the record says `left_running`, and
  the next integrator adopts them), exits 0, and the dispatcher tells the
  supervisor once (`host
  integrator-drain-expired-<event id>`). One integrator per database: its
  advisory lock belongs to the connection, kept while any gate runs.
