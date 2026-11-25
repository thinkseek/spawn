# spawn-node v1 protocol (sections 2–3)

This is the wire between a node daemon and the gateway (`crates/spawn-node-gateway`).
It covers sections 2 (transport and trust) and 3 (the verbs) of the spawn-node v1 design.
The bytes are defined by `crates/spawn-node-proto`. Its golden fixtures (`crates/spawn-node-proto/testdata/node-protocol-v1/`) are shared with the Go daemon, and both sides test against them.
Where this text and the fixtures disagree, the fixtures win.

No JSON is canonicalised anywhere. Every signature covers the bytes as sent. A receiver hashes the bytes it received, then parses those same bytes.

## 2. Transport and trust

### 2.1 Route

Only the gateway faces the nodes. It listens on `SPAWN_NODE_GATEWAY_LISTEN` (default `127.0.0.1:18096`), usually behind a reverse proxy that terminates TLS.
When the proxy keeps a path prefix, the gateway strips it first: `--public-prefix /p`, or `SPAWN_NODE_GATEWAY_PUBLIC_PREFIX`. Only a whole prefix is stripped. A target that does not start with it is `400 bad_request`. Signatures cover the target **without** the prefix.

| Method | Target | Signed body | Body cap |
| --- | --- | --- | --- |
| `POST` | `/node/v1/enroll` | whole body (seq must be 0) | 64 KiB + 1 KiB |
| `POST` | `/node/v1/heartbeat` | whole body | 4096 B |
| `POST` | `/node/v1/spec` | whole body | 64 KiB |
| `POST` | `/node/v1/jobs/<job_id>/result` | the metadata line (without its `\n`) | 256 KiB + 1 + 32 MiB |
| `GET` | `/node/v1/artifacts/<algo>:<64 hex>` | empty | 0 |
| `GET` | `/node/v1/registry-manifest` | empty | 0 |

The gateway logs in to PostgreSQL as the restricted `spawn_node_gateway_<db>` login (`SPAWN_NODE_GATEWAY_DATABASE_URL`).
That login may call only `spawn_node_gw_enroll`, `spawn_node_gw_beat`, `spawn_node_gw_spec` and `spawn_node_gw_result`, and it can read no table.
Its role defaults are `statement_timeout 5s`, `lock_timeout 2s` and `idle_in_transaction_session_timeout 5s`.

### 2.2 Request envelope

Every request carries five headers, each exactly once:

| Header | Value |
| --- | --- |
| `Spawn-Node-Id` | 64 lowercase hex: SHA-256 of the node's raw Ed25519 public key |
| `Spawn-Node-Boot` | 32 lowercase hex; the first 16 digits are the node's persistent boot counter |
| `Spawn-Node-Seq` | decimal, strictly increasing within a boot; `0` for enroll and only for enroll |
| `Spawn-Node-Ts` | decimal Unix ms: the node's clock plus its learned offset |
| `Spawn-Node-Sig` | strict unpadded base64url of the 64-byte Ed25519 signature |

The structural rules below come before any signature check, and any failure is `400 bad_request`:

1. A target that is neither `/node/v1/...` nor a known route, or a method that is not exactly `GET` or `POST` for its route.
2. The target, byte for byte as sent (never percent-decoded), must match `^/node/v1/[A-Za-z0-9._:/-]{1,220}(\?[A-Za-z0-9._=&-]{1,200})?$`. It may contain no `%`, no `//`, and no `.` or `..` segment.
3. Decimals have no sign, no leading zero (except `0` itself) and no spaces, and are below 2^53.
4. Base64url is strict: the alphabet `A-Z a-z 0-9 - _`, no `=`, the canonical spelling, and the exact length.
5. A POST carries exactly one `Content-Length`, a canonical decimal, and no `Transfer-Encoding`. A GET carries no body. A body over its verb's cap is refused **before any body byte is read**. A result over its cap is `413 too_large`; any other verb over its cap is `400`.
6. JSON is strict: UTF-8 without a BOM, no duplicate key at any depth, integers only, and every integer below 2^53 in magnitude.

The signing input is eight ASCII lines, each ended by `\n`:

```
spawn-node-req-v1
<METHOD>
<target, without the public prefix>
<node_id>
<boot_id>
<seq>
<ts_ms>
<hex SHA-256 of the signed body>
```

A GET's signed body is empty (`e3b0c442...b855`).

### 2.3 Response signature

Every response the gateway itself produces is signed, success or refusal, including `GET /node/v1/registry-manifest`. The one exception is the body of an artifact GET, `GET /node/v1/artifacts/...` (`200`, `206`, and its `416`; section 3.6).
A signed response carries `Spawn-Primary-Key` (the primary's key id: hex SHA-256 of its raw public key) and `Spawn-Primary-Sig` (base64url). The signature covers seven lines:

```
spawn-node-resp-v1
<status>
<node_id>
<boot_id>
<seq>
<server_time_ms>
<hex SHA-256 of the response body>
```

Every signed body is JSON with `"server_time_ms"`, and that value equals line 6.
The daemon believes nothing unsigned. It checks the key id against the key it pinned at enroll and verifies the signature. Any failure is a transport failure, with no state change.
`server_time_ms` is the only clock a node adapts to.
A response to a request whose headers were malformed binds the headers that were well formed. A malformed one (or a repeated one) binds `""`, or `0` for the seq.

### 2.4 Refusals

The body is `{"v":1,"error":"<error>","server_time_ms":N}`, signed (`refusals.json` holds one golden row per line of this table).

| Status | `error` | When | Daemon |
| --- | --- | --- | --- |
| 400 | `bad_request` | malformed target, header, `Content-Length` or body; over a cap (not a result); a cell the gateway does not serve; a token pinning another primary; a log that does not match its metadata | log; never resend the same bytes |
| 401 | `bad_signature` | the signature does not verify | full-jitter backoff |
| 401 | `unknown_node` | not enrolled; at enroll: a token that is unknown, used or expired (`bad_token`), or a key another node holds (`key_in_use`) | backoff |
| 401 | `revoked` | the owner revoked the node | stop every job, exit 78, no restart loop |
| 409 | `clock_skew` | `\|ts_ms − server_time_ms\| > 120 000` | `offset = server_time_ms − local_now`; retry once at once |
| 409 | `replay` | heartbeat `seq ≤` the stored one | `seq = max(seq, local) + 1`; retry |
| 409 | `stale_boot` | a higher boot counter was seen for this key | exit non-zero (another process owns the identity) |
| 409 | `beat_first` | the gateway holds no key (or not this boot) for the node; the ledger has another boot | heartbeat now, then retry |
| 409 | `lease_fenced` | a result for an epoch that moved, or another result already stored for that epoch | keep `results/<job>.<epoch>/` until GC; stop advertising it |
| 413 | `too_large` | result metadata over 256 KiB or log over 32 MiB | truncate per section 3.5 and resend |
| 429 | `busy` | pacing (section 2.6); a second upload of the same result at once | backoff |
| 503 | `busy` | PostgreSQL unreachable or failing; a `registry-manifest.json` that is not a manifest | backoff |

Outside the table, a missing artifact or manifest is a signed `404 {"v":1,"error":"not_found",...}`. An artifact range that starts past the end is a `416` with `Content-Range: bytes */<len>` and an empty body. Like every artifact answer it is **unsigned**: it says nothing a node acts on beyond "fetch the whole file".

#### Order of checks

1. Strip the prefix. Check the target grammar and the route, the five headers (each once), seq 0 for enroll only, and `Content-Length`: `400` (or `413`), before any body byte is read.
2. Pacing: `429`.
3. Clock: `409 clock_skew`.
4. Read the signed body (a result's metadata line only) and check it as strict JSON: `400`.
5. Key cache (node id → public key and boot, 120 s TTL, at most 4096 entries). A hit verifies at once. A bad signature is `401 bad_signature` with **zero database calls**.
6. Run one `spawn_node_gw_*` call in a transaction of its own, **verify-then-commit**. The function's answer carries the node's key (standard base64). The gateway verifies the request against that key and commits only if the signature verifies and the answer could be built. Otherwise it rolls the whole call back and answers `401 bad_signature`. A verified call fills the cache. `revoked` and `unknown_node` evict.

### 2.5 Enrollment and key pinning

The owner makes a one-time token, `sne1.<b64u 32 random bytes>.<b64u first 16 bytes of SHA-256(primary public key)>`. The ledger keeps only the SHA-256 of the whole token string, for at most 10 minutes (`spawn_node_enroll_token`).

`POST /node/v1/enroll` (seq 0) carries `{"v":1,"token":"sne1...","pubkey":"<b64u raw key>","spec":{...}}`. The gateway then:

1. checks that the token pins this gateway's primary (`400` otherwise) and that the spec's cell is one it serves (`SPAWN_NODE_GATEWAY_CELLS`, default `c0`);
2. checks **proof of possession**: the request must be signed by `pubkey`, and `Spawn-Node-Id` must be its SHA-256 (`401 bad_signature`, no database call);
3. hashes the `spec` member's bytes exactly as received (`spec_sha256`, the same digest the node puts in its heartbeats);
4. calls `spawn_node_gw_enroll`, which consumes the token, pins the key and fences whatever an earlier key of the same name held;
5. answers `200 {"v":1,"server_time_ms":N,"primary_pubkey":"<b64u>"}`, signed.

The daemon accepts `primary_pubkey` only if its SHA-256 starts with the token's 16 pinned bytes. From then on it believes only responses signed by that key.

The primary key lives in `SPAWN_NODE_GATEWAY_KEY_FILE`, a PKCS#8 Ed25519 document of mode `0600`.
If the file is missing, the gateway makes it on first start (mode `0600`, written whole and synced). It refuses a file that others may read or write.

### 2.6 Heartbeat and pacing

Pacing uses token buckets **per source address and per `node_id`**: 20 requests per 10 s for the API verbs, and 200 per 10 s for artifact and manifest GETs. Over either bucket the answer is `429 busy`.
The source address is the TCP peer. When the peer is loopback (a reverse proxy on this host), it is the last `X-Forwarded-For` entry, the one that proxy appended.

`next_beat_ms` is 5 000 while the node holds a lease or a pending result, was just granted one, or its cell has a queued job. Otherwise it is 30 000.
A node coalesces edges closer than 500 ms into one beat. A lease lives 90 s (`lease_ttl_s`) and is renewed by every beat that lists it.

## 3. The verbs

### 3.1 Heartbeat

The request (at most 4096 bytes) contains `v`, `cell_id`, `state` (`idle|active|draining`), `spec_sha256`, `capacities` (pool → `{free,total}`), `tmpfs_budget_mb`, `leases` (`{job_id, lease_epoch, phase?, elapsed_s?}`), `lease_results_pending`, `warm_bases` (at most 4), optionally `load1_milli` and `mem_available_mb`, and `edge` (`boot|timer|slot_free|completed|grant|sync`).
A beat over 4 KB drops the leases' `phase` and `elapsed_s` first; one that still does not fit is refused locally.

The answer (`200`, signed) contains `v`, `server_time_ms`, `next_beat_ms`, `lease_ttl_s`, `need_spec`, `need_sync`, `renewed`, `fenced` (`{job_id, lease_epoch, reason}`), `acked_results`, `grants` (at most 2) and `drain`.

A new boot counter fences every lease of the old boot at once. A lease granted earlier but missing from a later beat was never received, so it is given back along with its attempt (`grant_lost`).

#### Grants

The gateway builds each grant from the ledger's lease. The grant contains `v`, `job_id`, `lease_epoch`, `node_id`, `kind` (`gate|build|de_batch`; a shadow gate runs as `gate`), `expires_ms` (the ledger's time + `lease_ttl_s`) and `attempt`. It also carries `cost` and, from the job's spec, `timeout_s`, `inputs`, `env`, `requires` and `artifact_urls`.
It signs the grant's **exact JSON bytes**, apart from the response, so a node can store them and verify them again after a crash:

```
spawn-node-grant-v1
<hex SHA-256 of the grant bytes>
```

On the wire a grant is `{"grant":"<b64u bytes>","sig":"<b64u>"}`, at most 8 KiB of grant.
The node verifies the signature, then parses those same bytes. It refuses a grant for another node, an expired one, or one whose `env` names a variable the node sets itself (`PATH`, `HOME`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`, `SPAWN_TEST_DATABASE_URL`, `SPAWN_GATE_PG_IMAGE`, `SPAWN_GATE_PG_BIN`).
The gateway drops a lease whose grant cannot be built, and logs it. The node never holds that lease, so its next beat gives it back.

### 3.2 Backoff and timeouts

Every refusal whose reaction is "backoff" (section 2.4), and every transport failure (no answer, an unsigned answer, or one whose signature does not verify), is retried with **Full Jitter**: the n-th retry waits a uniformly random time in `[0, min(cap, base · 2^n))`. The daemon owns `base` and `cap` (design §3.2), and a signed success resets `n`.
`429` and `503` are always `busy`, so a node cannot tell pacing from an outage, and backs off from both the same way.

The gateway bounds every wait, so slow or idle peers cannot hold its 16 workers:

| Wait | Bound |
| --- | --- |
| a kept-alive connection idle between requests (or before the first) | 5 s, then closed |
| a request head, from its first byte | 10 s in all (and at most 16 KiB, 64 headers), then closed |
| a request body | 10 s + its length at 256 KiB/s (a full 32 MiB result: about 138 s); no single read waits over 15 s |
| a refused body still arriving | after the signed refusal is written and the write side shut, at most 4 MiB or 2 s more is read and discarded, then the connection closes. A client still writing (Go's `net/http`) reads the refusal instead of `broken pipe` |
| a client not reading its answer | 60 s per write |
| one ledger call | the login's `statement_timeout` 5 s, `lock_timeout` 2 s; a connect gives up after 5 s (`503 busy`) |

### 3.3 Spec

`POST /node/v1/spec` (at most 64 KiB) carries `v`, `name`, `cell_id`, `labels`, `resources_total` (`cores`, `mem_mb`, `disk_mb`), `capacities_total`, `tmpfs_budget_mb`, `holds` and optionally `versions`.
The gateway hashes the body itself and never takes the node's word for `spec_sha256`.
The ledger takes it only from the node's current boot (`409 beat_first` otherwise). The answer is `200 {"v":1,"server_time_ms":N,"changed":bool}`.

### 3.4 Holds and scheduling

A node's `spec.holds` and a job's `requires` are maps over `registry` (`tree_sha256_v1` of the cargo registry), `web_lock` (SHA-256 of `web/package-lock.json`), `toolchain` (the `rust-toolchain.toml` channel) and `pg` (the gate's PostgreSQL major version).
A job is leased to a node only when `requires <@ holds`, key by key. Otherwise the answer's `need_sync` names what to fetch.
Grants are given only when the node is not drained or degraded, needs no spec, and does not say `draining`. They go in priority order and then by age. Each grant must fit the `free` capacities and tmpfs the beat reports, after the grants before it in the same answer. Jobs are taken with `SKIP LOCKED`, so two beating nodes never share a job.

A gate's inputs are fetched **delta-first**: the grant always lists the delta bundle (the candidate against its base) and the controller pack. It lists the baseline bundle only when the node needs the base. The ledger answers `needs_baseline: false` when the job's base commit is among the beat's `warm_bases`, and the gateway then leaves every `baseline_bundle` entry out of `artifact_urls` before it signs the grant.

### 3.5 Result framing

`POST /node/v1/jobs/<job_id>/result` carries one line of metadata JSON, one `\n`, then the raw log bytes (stdout, then stderr).
The signature covers the metadata line without its newline. `log_sha256` inside the line binds the log.

The metadata line contains `v`, `job_id`, `lease_epoch`, `exit_code` (or null), `signal` (number, name, or null), `timed_out`, `self_fenced`, `seconds`, `verdict_raw` (at most 64 KiB, or null), `stdout_bytes`, `log_bytes` and `log_sha256`. It may also contain `truncated`, `inputs`, `env` and `wrapper_error`.

The gateway:

1. reads the metadata line only (at most 256 KiB; `413` past it, `400` without a newline);
2. requires the node's cached key for this boot (`409 beat_first` on a miss, zero database calls) and verifies the signature (`401`);
3. checks that `job_id` is the path's, that the log's length (`Content-Length − line − 1`) is `log_bytes`, and that it is at most 32 MiB (`413`);
4. streams the log to `SPAWN_NODE_RESULTS/<job>.<epoch>/log.part`, hashing and counting as it goes and reading at most 32 MiB + 1. The log is never held in memory. A mismatch is `400` and removes the partial file;
5. syncs `log.part` and renames it to `log`, then writes `meta.json` (the line as received; synced, renamed), **before** the ledger call. The same result sent again rewrites nothing. A different result already stored for that epoch is `409 lease_fenced`, and its files are left untouched;
6. calls `spawn_node_gw_result` with the metadata plus `result_sha256` (SHA-256 of the line). True means this call, or an identical earlier one, completed the job; the answer is `200 {"v":1,"server_time_ms":N,"job_id":"...","lease_epoch":E}`. False means the lease moved: the files this request wrote are removed, and the answer is `409 lease_fenced`.

A node never sends a log over 32 MiB. It truncates each stream, stdout and stderr, to its first 4 MiB and last 4 MiB, with a marker line between them saying how much was cut, and sets `truncated: true`. `log_bytes`, `log_sha256` and `stdout_bytes` describe the bytes actually sent. A `413 too_large` (section 2.4) means the node did not truncate, and it truncates and resends.

### 3.6 Artifacts and the registry manifest

`GET /node/v1/artifacts/sha256:<hex>` serves `SPAWN_NODE_ARTIFACTS/sha256/<first 2 hex>/<hex>`.
It requires the node's cached key for its current boot (`409 beat_first` on a miss, zero database calls) and a valid signature.
It answers with `Content-Length` and `Accept-Ranges: bytes`. One `bytes` range gets `206` with `Content-Range`. Several ranges, or a malformed range, get the whole file. A range starting past the end gets `416`.
The artifact bodies (and the `416`) are **not signed**: a node believes an artifact only if it hashes to its address.
`blake3:` addresses parse but have no file in v1 (`400`).

`GET /node/v1/registry-manifest` is not an artifact. It needs the same cached key and signature, and it answers **signed JSON**, as section 2.3 requires of every gateway answer.
The gateway reads `SPAWN_NODE_ARTIFACTS/registry-manifest.json` (at most 1 MiB). The file must be a strict JSON object with `registry` (64 lowercase hex: the `tree_sha256_v1` of the cargo registry) and `pack` (`{"digest":"sha256:<hex>","size":N}`: the packed registry, itself fetched as an artifact).
The answer keeps the file's members, sets `"v":1` and `"server_time_ms":N`, and is `200` with `Content-Type: application/json`, `Spawn-Primary-Key` and `Spawn-Primary-Sig`.
A missing file is a signed `404 not_found`. A file that is not a manifest is logged and answered with a signed `503 busy`.

## Gateway configuration

| Variable | Default |
| --- | --- |
| `SPAWN_NODE_GATEWAY_DATABASE_URL` | required: the restricted gateway login |
| `SPAWN_NODE_GATEWAY_KEY_FILE` | required: PKCS#8 Ed25519, 0600, made when absent |
| `SPAWN_NODE_GATEWAY_LISTEN` | `127.0.0.1:18096` |
| `SPAWN_NODE_ARTIFACTS` | required |
| `SPAWN_NODE_RESULTS` | required (made when absent) |
| `SPAWN_NODE_GATEWAY_CELLS` | `c0` (comma separated) |
| `SPAWN_NODE_GATEWAY_PUBLIC_PREFIX` | empty (`--public-prefix` overrides it) |

The gateway is synchronous. It runs a std-only HTTP/1.1 server with 16 worker threads, and each thread holds its own PostgreSQL connection. It links no async runtime and has no unsafe code.
It does not use `tiny_http`, for two reasons. When `tiny_http` answers a request without reading its body, dropping that body's reader allocates the whole declared `Content-Length` at once. It also silently treats a malformed `Content-Length` as no body.
