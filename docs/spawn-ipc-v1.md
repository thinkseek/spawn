# Local IPC Protocol Contract (`spawn-ipc-v1`, Option B2)

> **Status**: Frozen Wire Contract, Live Split Deployment (Master Issue [#113](https://github.com/thinkseek/spawn/issues/113) Decision D3, Option B2 — deployed via `spawn-core.service`, `spawn-node-edge.service`, `spawn-node-gw.service`, and `spawn-node-out.service`).
> **Golden Fixtures**: [`crates/spawn-core/testdata/spawn-ipc-v1/`](../crates/spawn-core/testdata/spawn-ipc-v1/).
> **API Byte Contract**: Every `/api/*` route's response schema and status semantics remain solely owned by [`docs/api.md`](./api.md) and Rust `serve`.

---

## 1. Transport, Socket Lifecycle & Authentication

Option B2 uses **HTTP/1.1 over a UNIX domain stream socket (`AF_UNIX`, `SOCK_STREAM`)** between Go edge/gateway units (`spawn-node front`, `spawn-node-edge`, `spawn-node-gw`) and the local Rust core (`spawn-core`).

- **Never TCP**: `spawn-core` must **never** bind a TCP listener for IPC or internal RPC. All local IPC travels over UNIX domain sockets.
- **Canonical Socket Path**: `$XDG_RUNTIME_DIR/spawn/core.sock` (parent directory mode `0700` owned by the service UID).
- **Atomic Bind Sequence (Rust `spawn-core`)**:
  1. Acquire an exclusive `flock` on a sibling `.lock` file in the socket directory so two restarts never race on the socket path.
  2. Bind a temporary sibling socket path.
  3. Set socket permissions to **`0600`** (`fchmod`/`chmod` before publishing).
  4. Atomically **`rename`** the temporary socket onto `core.sock`.
- **Mutual UID Verification (No Bearer Token over UDS)**:
  - **Dialer (Go)**: Calls `Lstat` on `core.sock` before dialing; refuses symlinks, non-sockets, wrong UID, or any group/world permission bits (`mode & 0o077 != 0`).
  - **Listener (Rust Linux)**: Checks `SO_PEERCRED` (`rustix::net::sockopt::socket_peercred`) on accepted connections and verifies `peer_uid == geteuid()`, closing mismatched connections immediately.
- **No `ActorContext` Over IPC**: IPC requests carry no database role header or `ActorContext`. PostgreSQL `by` / principal attribution is enforced strictly by the database connection's login role selected via the static Route → PG Role table (§4).
- **Least-Privilege Startup & Split Systemd Discipline**:
  - `deploy/systemd/spawn-core.service` runs `spawn serve --ipc --ipc-sock %t/spawn/core.sock --cas ...`, deriving scoped URLs for `spawn_reader`, `spawn_human`, and `spawn_node_gateway` once at startup (via `role_url`), then **dropping the owner `SPAWN_DATABASE_URL`** from the IPC server state.
  - `deploy/systemd/spawn-node-edge.service` runs `spawn-node front --serve edge --core-sock %t/spawn/core.sock --listen ${SPAWN_SERVE_ADDRESS} --web ${SPAWN_WEB_DIR}` (serving the edge HTTP/SSE front and externalized PWA assets), and `deploy/systemd/spawn-node-gw.service` runs `spawn-node front --serve node --core-sock %t/spawn/core.sock ...` (the signed node gateway).
  - `deploy/systemd/spawn-node-out.service` runs `spawn-node front --serve outbound --out-sock %t/spawn/out.sock`, holding the Jev upstream key only via `LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key` (`RestartPreventExitStatus=243 78`, `SPAWN_OUT_ALLOWLIST=https://api.typesafe.ai/v1/systemone`) and exposing local unary `POST /out/v1/jev/call` on `$XDG_RUNTIME_DIR/spawn/out.sock` (`0600`). `spawn-jev`'s `validate_out_sock` checks UID/socket mode (`0600`, rejecting symlinks, foreign UIDs, and `mode & 0o077 != 0`) before dialing `SPAWN_OUT_SOCK`.
  - All Go systemd units (`spawn-node-edge.service`, `spawn-node-gw.service`, `spawn-node-out.service`) **must not** load `spawn.env` and **must have zero `*DATABASE_URL*` variables** in `os.Environ()`. `scripts/keep/credential-health.sh` permanently checks this via `go_unit_pg_isolation` and supports `unix://<socket>|<http-path>` UDS health probes (expanding `%t/` and `~/`).

---

## 2. Framing, Connection Pools & Hard Size Caps

| Rule | Value | Refusal Status |
| --- | --- | --- |
| Protocol | HTTP/1.1 over UDS | `400 bad_request` |
| Transfer encoding on requests | **Forbidden** (`Content-Length` mandatory on all non-empty bodies and all non-GET requests) | `411 length_required` |
| Max request body size | **`4 MiB` (`4_194_304` bytes)** (`MAX_IPC_BODY_BYTES`) | `413 payload_too_large` |
| Max NDJSON line size (`/ipc/v1/events`) | **`4 MiB` (`4_194_304` bytes)** (`MAX_NDJSON_LINE_BYTES`) | Close stream with `resync` / `slow_consumer` |
| `Idempotency-Key` header | **Mandatory on every non-GET request** (`1..=256` bytes, no control characters; must equal JSON `key` when both are sent) | `400 bad_request` |
| Go UDS `IdleConnTimeout` | **`30s`** (strictly less than Rust server idle timeout **`60s`** to avoid half-closed keep-alive races) | — |
| Rust UDS stream write timeout | **`10s`** (`WRITE_TIMEOUT`) + bounded `SyncSender` channel | Drop slow consumer (`slow_consumer`) |

### 4 Isolated Admission Pools (Semaphores & DB Connection Pools)

To guarantee dashboard read storms or SSE catch-ups never starve node heartbeats or human writes:

| Pool | Max Concurrency / Slots | Used By |
| --- | --- | --- |
| `reads` | **`16`** | All 33 canonical `GET /api/*` routes + out-of-table read-only `GET\|HEAD /api/features/truth` and `/api/episodes` + `GET /ipc/v1/hello` (`spawn_reader`) |
| `writes` | **`4`** | All 9 `POST /api/*` routes (`spawn_human` for 7 SQL writes; `none` for 2 feature-map routes) |
| `node` | **`16`** | All 6 `POST /ipc/v1/node/*` routes (`spawn_node_gateway`) |
| `events` | **`2`** | `GET /ipc/v1/events` long-lived NDJSON upstream subscription (`spawn_reader`) |

---

## 3. Canonical Endpoints

### 3.1 `GET /ipc/v1/hello`

Liveness, schema/protocol version, and event-stream baseline probe:

```json
{
  "proto": "spawn-ipc-v1",
  "build": "<binary version or git rev>",
  "schema": 1,
  "epoch": "<boot epoch identifier>",
  "latest_id": 12345
}
```

### 3.2 `GET|POST /api/*` (42 Canonical Routes)

Exact HTTP byte passthrough (`docs/api.md` contract preserved 1-to-1; Go `front` forwards via `httputil.ReverseProxy` over `net/http.Transport` UDS dialer):

- **33 Canonical `GET` routes** (same status, `ETag`, `Cache-Control`, JSON bytes).
- **2 Out-of-Table Read-Only `GET|HEAD` routes** (`/api/features/truth` and `/api/episodes`, intentionally outside `routes.json` / `docs/api.md` while contract-locked in `crates/spawn-core/tests/spawn_ipc_v1.rs`): dispatched on `reads_gate` (`reads` pool) under `spawn_reader`, rejecting any non-`GET`/`HEAD` method with `405 Method Not Allowed` (`Allow: GET`).
- **9 Canonical `POST` routes** (`/api/sessions/<id>/{input,key,restart,reassign}`, `/api/missions/<id>/resume`, `/api/master/message`, `/api/approvals/<session_id>/act`, `/api/features/{scan,associate}`).
- Legacy aliases (14 patterns) are handled/retired according to Phase 2a policy and never alter canonical behavior.

### 3.3 `POST /ipc/v1/node/*` (Typed Node Ledger Bridge)

Typed JSON bridges backing Go `spawn-node-gw` (`:18096`) verify-then-commit steps (matching `crates/spawn-postgres/src/remote_jobs.rs`):

| Endpoint | Request Body | Purpose / PG Procedure |
| --- | --- | --- |
| `POST /ipc/v1/node/key` | `{"node_id": "<64 hex>"}` | Cached/ledger node public-key lookup (`spawn_reader` / `spawn_node_gateway` key lookup for verify-then-commit step 1) |
| `POST /ipc/v1/node/enroll` | `{"token_sha256_hex", "pubkey_b64", "boot_id", "spec_sha256", "spec"}` | `spawn_node_gw_enroll` |
| `POST /ipc/v1/node/beat` | `{"node_id", "boot_id", "seq", "beat"}` | `spawn_node_gw_beat` (heartbeat, lease fencing, up to 2 grants; note Rust generates canonical `Grant` bytes, Go signs) |
| `POST /ipc/v1/node/spec` | `{"node_id", "boot_id", "spec_sha256", "spec"}` | `spawn_node_gw_spec` |
| `POST /ipc/v1/node/result` | `{"node_id", "job_id", "lease_epoch", "result"}` | `spawn_node_gw_result` |
| `POST /ipc/v1/node/cas` | `{"action": "register" \| "pin" \| "stat", "digest": "sha256:<64 hex>", "namespace": "artifacts", ...}` | Host-side CAS metadata/pin registration under `spawn-cas` `pins/.lock`; returns `{"status": "ok" \| "missing", ...}` |

All 6 endpoints require `Idempotency-Key` and `Content-Length <= 4 MiB`. Large artifact streaming (`GET /node/v1/artifacts/<digest>`) and raw result log files are served locally on filesystem from shared host CAS/results directories without crossing `core.sock`.

### 3.4 `GET /ipc/v1/events?after=<id>|tail=<n>` (NDJSON Event Stream)

Long-lived newline-delimited JSON upstream stream (one JSON object per `\n`-terminated line, each `<= 4 MiB`), consumed by Go `front` edge units (`>= 4096`-entry ring buffer + single-mutex client snapshot) to serve fan-out SSE `/api/stream`:

- **`ready`** (emitted first): `{"type":"ready","latest_id":<u64>,"recorded_at_ms":<i64>,"replayed":<usize>}`
- **`event`** (shape identical to `/api/events` & SSE `data:` frame):
  `{"type":"event","id":<u64>,"agent_id":<str>,"session_id":<str>,"execution_id":<str|null>,"source":<str>,"event_name":<str>,"detail":<json>,"recorded_at_ms":<i64>}`
- **`resync`** (cursor behind / ahead / reset / overflow):
  `{"type":"resync","reason":"behind"|"cursor_ahead"|"stream_reset"|"slow_consumer","after_id":<u64|null>,"latest_id":<u64|null>,"recorded_at_ms":<i64|null>}`
- **`hb`** (heartbeat every `5s` during quiet periods):
  `{"type":"hb","latest_id":<u64>,"recorded_at_ms":<i64>}`

#### Cursor & Gap Semantics

- Every cursor carries **`(id, recorded_at_ms)`**.
- **Rollback sequence skips are NOT gaps**: Aborted PostgreSQL transactions consume `spawn_events.id` sequence values without committing visible rows (`schema.sql` advisory lock serializes visible commit order). Consumers **must never** treat `next_id > prev_id + 1` as a missed event; `resync` triggers only on backward/mismatched `recorded_at_ms`, R2 backup restore reset, `after` outside retention, or `slow_consumer`.

---

## 4. Static Route → PostgreSQL Role → Pool Matrix

This table is locked by compile/unit tests (`crates/spawn-cli/src/serve.rs::ipc_route_table_covers_every_canonical_route` and `crates/spawn-core/tests/spawn_ipc_v1.rs`). Neither `owner` nor `spawn_supervisor` is ever valid in `routes.json`.

| Route Pattern(s) | Method | Pool (`Concurrency`) | PG Role | `Idempotency-Key` |
| --- | --- | --- | --- | --- |
| `/ipc/v1/hello` | `GET` | `reads` (`16`) | `spawn_reader` | Not required |
| All 33 canonical `GET /api/*` routes (`docs/api.md`) | `GET` | `reads` (`16`) | `spawn_reader` | Not required |
| `/api/features/truth`, `/api/episodes` (out-of-table contract) | `GET` \| `HEAD` | `reads` (`16`) | `spawn_reader` | Not required (`405 Allow: GET` otherwise) |
| `/ipc/v1/events` | `GET` | `events` (`2`) | `spawn_reader` | Not required |
| `/api/sessions/<id>/input` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/sessions/<id>/key` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/sessions/<id>/restart` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/sessions/<id>/reassign` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/missions/<id>/resume` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/master/message` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/approvals/<session_id>/act` | `POST` | `writes` (`4`) | `spawn_human` | **Mandatory** |
| `/api/features/scan` | `POST` | `writes` (`4`) | `none` | **Mandatory** |
| `/api/features/associate` | `POST` | `writes` (`4`) | `none` | **Mandatory** |
| `/ipc/v1/node/key` | `POST` | `node` (`16`) | `spawn_node_gateway` | **Mandatory** |
| `/ipc/v1/node/enroll` | `POST` | `node` (`16`) | `spawn_node_gateway` | **Mandatory** |
| `/ipc/v1/node/beat` | `POST` | `node` (`16`) | `spawn_node_gateway` | **Mandatory** |
| `/ipc/v1/node/spec` | `POST` | `node` (`16`) | `spawn_node_gateway` | **Mandatory** |
| `/ipc/v1/node/result` | `POST` | `node` (`16`) | `spawn_node_gateway` | **Mandatory** |
| `/ipc/v1/node/cas` | `POST` | `node` (`16`) | `none` | **Mandatory** |
