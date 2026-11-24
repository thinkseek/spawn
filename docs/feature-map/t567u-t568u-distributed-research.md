# Multi-Machine Distributed Compute & Storage Research (`t567u` / `t568u`)

> **Status**: **STAGED RESEARCH & DESIGN ONLY — NOT A PRODUCTION IMPLEMENTATION.**
>
> - This initiative (`t567u-multi-machine-distributed`) is a **distinct staged research/design initiative**, explicitly **NOT a 4th slice of `t559u`** and **NOT a production cluster conversion**.
> - Live production Spawn remains single-host until owner sign-off and isolated PoC gates pass.
> - **Permanent hardware exclusion**: Sold machine **`mac16`** is permanently excluded from all topologies, inventories, quorum calculations, and PoC plans.

## Relation to `t559u`

`t567u` / `t568u` is tracked in `docs/feature-map/registry.json` as a separate `research-initiative` node (`t567u-multi-machine-distributed`) linked to:
- `t559u-feature-map-initiative` (`relates_to`): distinct initiative sharing feature-map visibility.
- `t559u-slice-b-coverage` (`depends_on`): leverages the pure core/CLI/API library boundary (`spawn-core::feature_coverage`).
- `t559u-slice-c-storage-keys` (`depends_on`): builds directly on content-derived storage keys and immutable manifests (`fm-cas/v1`) for cross-machine artifact and workspace transfer without coupling bulk bytes into consensus.

---

## Stage 1: Requirements & Failure Model

### 1.1 Target Fleet Profile (Home / Personal Multi-Machine)

Spawn runs on owner-controlled machines (e.g. always-on Linux workstation/server plus intermittently available laptops/Macs; **never `mac16`**). Unlike a datacenter Kubernetes cluster, this environment exhibits:
- **Asymmetric uptime**: one or more anchor nodes run 24/7 while developer laptops sleep, change Wi-Fi networks, or go offline for hours.
- **Heterogeneous compute**: differing CPU core counts, RAM, OS (Linux/macOS), local caches (`target/`, `node_modules/`), and engine toolchains.
- **No shared POSIX SAN/NFS**: machines communicate over LAN or encrypted overlay (WireGuard/Tailscale/mTLS).

### 1.2 Hard Architectural Boundary: Control Plane vs Data Plane vs Execution

A core design invariant for `t567u`/`t568u`:

> **Consensus is for control-plane metadata ONLY — NEVER bulk file replication and NEVER job execution.**

| Plane | Responsibility | Technology Fit |
| --- | --- | --- |
| **Control-plane metadata** | Leader lease / epoch fencing, mission/task assignments, command queue, machine membership, immutable manifest pointers (`fm-cas/v1` roots) | Replicated metadata store or fenced lease + PostgreSQL |
| **Bulk data & artifact plane** | Git packs, workspace checkpoints, transcripts, build artifacts, large binaries | Content-addressed object transfer (`fm-cas/v1` manifests + chunk transfer / git fetch-push) |
| **Compute execution plane** | Running coding engines (`claude`), running gate scenarios, building/testing | Local worker daemon per machine with explicit resource leases and idempotent execution reporting |

### 1.3 Failure Model & Required Guarantees

| Failure Mode | Required System Behavior |
| --- | --- |
| **Intermittent / offline laptop** | Non-voting compute worker; going offline does **not** threaten control-plane quorum. Leases expire deterministically; in-flight tasks are safely reassigned or resumed once reconnected. |
| **Leader / coordinator crash** | New leader acquires higher monotonic fencing epoch/term. Stale leader writes are rejected by the fencing token before mutating task or landing state. |
| **Network partition** | Minority partition **must** stop accepting control-plane mutations (CP over AP for task assignment & `main` landing). Read-only cached status is visibly marked stale. |
| **Quorum loss** | With `< floor(N/2) + 1` voting members reachable, control-plane writes halt safely rather than risking dual-leader split-brain. |
| **Split-brain fencing** | Every mutating RPC (`claim`, `finish`, `land`, `promote`) carries `(term, lease_id)` or compare-and-swap version. Stale epochs are rejected server-side AND at the git ref CAS boundary (`git push --force-with-lease` / atomic ref update). |
| **Failover & idempotent retries** | All commands carry deterministic idempotency keys (extending Spawn's existing `IdempotencyKey` and `spawn_submit` semantics). Retrying after timeout or leader switch never double-spawns an engine or double-lands a commit. |
| **Recovery after crash/reboot** | Reconnecting worker reconciles local tmux/workspace state against control-plane authoritative lease before resuming or reporting completion. |
| **Membership changes** | Explicit owner-authorized join/drain/remove workflow (single-step joint consensus or explicit voting-set update). Laptops default to **non-voting workers**; only always-on anchors hold voting membership. |

---

## Stage 2: Candidate Architectures & Mature Consensus Comparison

We deliberately avoid writing custom consensus protocols. Below is an evaluation of mature candidate architectures strictly for the **control-plane metadata layer**.

### 2.1 Candidate Comparison Matrix

| Architecture | Consensus / HA Mechanism | Operational Footprint | Home-Fleet Intermittent-Node Fit | Spawn Rust Sync-Codebase Fit | Main Tradeoff |
| --- | --- | --- | --- | --- | --- |
| **1. Single-coordinator + Fenced Lease on PostgreSQL** *(Baseline evolution)* | Single PG primary + monotonic lease/epoch table; remote workers connect over mTLS | Very low (already deployed in Spawn) | Excellent — laptops are pure stateless workers; zero quorum fragility | Native (`spawn-postgres` already exists, zero async runtime needed) | Primary PG host is a single point of failure for control-plane writes (manual/backup failover) |
| **2. PostgreSQL + Patroni (backed by etcd/Raft)** | Patroni manages PG synchronous/async streaming replication; etcd Raft holds leader lock | Medium-High (3-node etcd + 2+ PG nodes) | Moderate — requires 3 always-on voting hosts for etcd quorum | High — Spawn keeps existing SQL schema & `spawn-postgres` | Needs >= 3 always-on hosts to avoid quorum loss when a laptop sleeps |
| **3. Raft / `etcd` (metadata KV) + Object Store** | Battle-tested Raft (Go binary) for leases, membership, and small metadata | Medium (3 voting nodes) | Moderate — voting set must exclude laptops | Moderate — requires replacing or dual-writing PG SQL views/queries | Poor fit for Spawn's rich relational timeline/scorecard SQL queries without a secondary read DB |
| **4. Embedded Raft (`openraft` in Rust)** | Library-embedded Raft state machine inside `spawn` daemon | Low external binaries, high internal complexity | Moderate — still needs 3 always-on voters | Poor-Moderate — `openraft` requires `tokio` async runtime (violates Spawn's current sync-only design) + snapshot/compaction maintenance | Highest engineering risk; must still maintain custom state machine, snapshots, and query layer |
| **5. NATS JetStream** | Raft-backed streams & KV buckets inside NATS cluster | Low-Medium (single Go binary, 3 voters for HA) | Good for pub/sub & ephemeral worker heartbeats; voters still need 3 always-on nodes | Moderate — adds broker ops + async/client protocol; does not replace relational event analytics | Great for dispatch/notifications, weak as sole analytical SQL store |
| **6. SQLite + LiteFS** | FUSE-level SQLite page replication with Consul/static primary lease | Low-Medium (Linux FUSE required) | Poor on macOS (FUSE friction) and single-writer still requires lease authority | Low — Spawn already relies on PostgreSQL functions, roles (`spawn_reader`, `spawn_human`), and JSONB queries | Loses PG RBAC/role isolation and macOS parity |

### 2.2 Evaluation Conclusions (Stage 2)

1. **No custom Raft/Paxos**: Never implement custom consensus.
2. **Voting membership rule**: With fewer than 3 always-on machines (especially with `mac16` permanently retired), a 3-node Raft quorum cannot tolerate an offline laptop. Therefore:
   - **Near-term multi-machine step**: **Option 1 (Single-coordinator PostgreSQL + strict monotonic epoch fencing + mTLS remote workers + `fm-cas/v1` content-addressed transfer)** solves multi-machine compute dispatch immediately with zero quorum fragility.
   - **True HA control-plane step** (only once >= 3 always-on anchors exist): **Option 2 (PostgreSQL + Patroni + 3-node etcd)** preserves 100% of Spawn's PostgreSQL SQL schema, RBAC roles, and sync Rust client while delegating leader election to mature Raft (`etcd`).
3. **Bulk bytes stay out of consensus**: Whether Option 1 or Option 2 holds control-plane metadata, workspace blobs, checkpoints, and transcripts move strictly via `fm-cas/v1` content-addressed manifests + chunk sync (or git object transfer), referenced from metadata by digest only.

---

## Stage 3: Small Isolated PoC Designs

Each PoC answers **one concrete question**, runs strictly in an **isolated sandbox** (never touching live Spawn databases, live systemd units, or production repositories), and carries explicit pass/fail criteria.

### PoC 1 — Machine Identity & mTLS / Authenticated Connectivity
- **Concrete question**: Can a remote worker authenticate to the control plane with per-machine client certificates (or WireGuard + scoped machine token) and be revoked cleanly without restarting other workers?
- **Isolated setup**: Local ephemeral CA + 2 simulated worker processes on loopback/container namespaces (`worker-a`, `worker-b`; never `mac16`).
- **Acceptance criteria**:
  - Valid client cert registers `machine_id`, capabilities (OS, arch, CPU, RAM, engines), and passes heartbeat.
  - Revoked cert or mismatched `machine_id` is rejected with `401/403` before reaching command claim.
  - Zero credentials written to git or feature registry.
- **Failure criteria**: Any unauthenticated claim succeeds, or revoking `worker-b` disrupts `worker-a`.

### PoC 2 — Workload Placement & Resource Accounting
- **Concrete question**: Given heterogeneous workers with declared CPU/RAM/engine/repo-cache capacities, can a deterministic pure placement function (in `spawn-core`) assign tasks without oversubscribing any machine and gracefully handle an offline worker?
- **Isolated setup**: Pure unit/simulation harness with 3 simulated machines (1 always-on Linux anchor, 1 macOS workstation with intermittent offline windows, 1 constrained small host).
- **Acceptance criteria**:
  - Placement respects hard constraints (required OS/toolchain, available CPU/RAM slots) and prefers warm-cache affinity.
  - When a worker misses 3 heartbeats, its lease expires, capacity is zeroed, and queued tasks route to healthy eligible workers.
  - Zero double-assignment across 1,000 randomized tick/disconnect sequences.
- **Failure criteria**: Any worker exceeds declared slot cap, or a task stays wedged indefinitely after a worker vanishes.

### PoC 3 — Compute Dispatch vs Shared Storage Boundary (`fm-cas/v1` Manifests)
- **Concrete question**: Can a worker execute a task on a machine that does **not** share a filesystem with the coordinator by pulling only the referenced `fm-cas/v1` workspace manifest/chunks and uploading only changed content-addressed chunks + result manifest?
- **Isolated setup**: Two isolated temp directories (`coord-cas/`, `worker-cas/`) over local HTTP/file transport using Slice C's `fm-cas/v1` content-derived keys.
- **Acceptance criteria**:
  - Identical files deduplicate by content hash (zero redundant byte transfer on second task).
  - Corrupted chunk transfer fails SHA-256 verification before materializing into the worker workspace.
  - Control-plane record stores only the small manifest root key (`fm-cas/v1:...`), never bulk file bytes.
- **Failure criteria**: Any silent corruption, path traversal outside workspace root, or bulk blob stored inside metadata DB.

### PoC 4 — Leader Election, Epoch Fencing & Split-Brain Prevention
- **Concrete question**: Under a simulated network partition where an old leader believes it still holds the lease while a new leader is elected, do fencing tokens reject 100% of stale writes?
- **Isolated setup**: Isolated 3-node local `etcd` (or PG lease table with simulated network delay/partition via proxy) + two coordinator instances (`coord-1`, `coord-2`).
- **Acceptance criteria**:
  - Partitioning `coord-1` triggers lease expiry and election of `coord-2` with `epoch = N + 1`.
  - Delayed writes from `coord-1` carrying `epoch = N` are rejected with `STALE_EPOCH` at both metadata store and git ref CAS.
  - Re-healing the partition causes `coord-1` to step down cleanly; zero duplicate landings or dual-claimed tasks.
- **Failure criteria**: Even 1 stale-epoch write succeeds after `epoch = N + 1` is issued.

---

### Built: Multi-Hop Interconnection over TCP / UDP (issue #85, `cap-node-registry`)

The node registry (multi-host P1) no longer assumes one ssh hop. What is built, and what it leaves to the PoCs above:

- **Transports**: `ssh` (as before; `--via` becomes jump hosts through a nested `ProxyCommand`, every jump with the same nine hardening `-o` options, since `-J` gives jump hosts none), `tcp` and `udp` to a node's own `spawn node serve` (`std::net` only, no new crate). Modes: `direct`, `relay` (through hops), `hole_punch` (a label for a path the node opened by speaking first).
- **Multi-hop**: `via` is the ordered chain of joined nodes from the coordinator (at most 8). A `forward` envelope carries the remaining hops; each relay pops the next, speaks its scheme (TCP↔UDP mixed chains work), passes on less time and a nonce of its own, and checks the relayed reply names the inner request. `spawn_node_record` refuses a hop not joined, a self-loop, a duplicate and any transitive loop through the hops' own `via`, under the append lock.
- **Upward registration**: `spawn node register` sends a node's own inventory up to a coordinator's `spawn node serve --register-store`, directly or through relays, with retry and one nonce (recorded once); every field is checked as the writer checks it.
- **Probing**: the heartbeat resolves each node's route from the membership on every read and probes ssh, tcp, udp and relayed nodes alike; spoofed or stale replies (another nonce) are never taken; every exchange is bounded in time, frame size and hops.
- **Authentication**: with `SPAWN_NODE_TOKEN` set, every request carries `auth`, HMAC-SHA256 keyed by the token over its op, nonce, time and the fields it acts on (the token itself never on the wire), refused outside 120 s of the receiver's clock; a `register` always needs it, and `spawn node serve --register-store` refuses to start without a token.
- **Bounds against abuse**: each nonce runs once (udp resends dropped while in flight, answered from a 125 s cache after, before any worker slot is taken), so a probe through udp relays to a dead hop costs one resend loop, not a multiplying fan-out; a relay rebuilds the inner request from allowed fields only, never connects to loopback, link-local, unspecified, broadcast or multicast addresses (IPv4-mapped included) unless `--allow-loopback-forward`, and says every failure to reach a hop in the same words (no port-scan oracle); 64 requests per listener, 16 per peer address, 2 s for a tcp request line. `/api/nodes` withholds `ssh`, `endpoint` and `note`.
- **Not built (still PoC 1)**: encryption of the tcp/udp wire and per-machine identities (one shared fleet token; no single machine can be revoked); the transport belongs on a trusted overlay (open owner decision 2 below). No node runs work yet (P1).

## Stage 4: Discarded Alternatives, Evidence & Unresolved Owner Decisions

### 4.1 Discarded Alternatives (and Why)

1. **Custom Raft / Paxos implementation in Rust**: Rejected. High correctness risk, redundant with mature `etcd`/Patroni, and forces async runtime sprawl into Spawn's synchronous codebase.
2. **Replicating bulk workspaces/artifacts through Raft/etcd**: Rejected. Raft logs are designed for small control-plane metadata; pushing MB–GB build/workspace blobs causes election timeouts, WAL bloat, and OOMs. Bulk bytes belong strictly in `fm-cas/v1` CAS + git packs.
3. **Shared NFS / SSHFS workspace mounts across home machines**: Rejected. Brittle over Wi-Fi/WAN, poor `git`/SQLite locking semantics, and stalls local shells when a peer sleeps.
4. **Including `mac16` in any cluster or quorum topology**: Permanently rejected. `mac16` has been sold and must never appear in inventories, configs, or quorum calculations.
5. **Making `t567u`/`t568u` a 4th slice of `t559u`**: Rejected. `t559u` delivers local feature-map capabilities (mindmap, coverage/wild-file scan, content-derived storage keys); distributed multi-machine execution is a distinct research/design track with its own risk profile and owner gates.

### 4.2 Unresolved Owner Decisions (Blocking Any Future Production Build)

Before any production implementation begins after isolated PoCs succeed, the owner must explicitly decide:

1. **Anchor machine count**: Are there >= 3 always-on hosts available to run a true Raft quorum (Patroni + etcd), or should Phase 1 target **Option 1** (single always-on PostgreSQL coordinator + fenced mTLS remote workers)?
2. **Network overlay choice**: Standardize remote worker connectivity on Tailscale/Headscale, raw WireGuard, or direct LAN mTLS?
3. **Secrets distribution policy**: Do remote workers ever hold model API keys locally, or must all inference route through the coordinator's credential-isolated relay (`scripts/model-relay.py`)?
4. **Gate execution locality**: May remote workers run black-box integration gates, or must landing gates remain pinned to the primary anchor host?
5. **Go/No-Go sign-off**: Explicit owner approval required after reviewing PoC 1–4 evidence before promoting any component out of isolated sandboxes.
