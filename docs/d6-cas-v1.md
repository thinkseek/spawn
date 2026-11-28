# D6 CAS Format Contract (`d6-cas-v1`)

> **Status**: Phase 0 Frozen Contract (Master Issue [#113](https://github.com/thinkseek/spawn/issues/113) Decision D6, Fine Item 7, and Comment #6 Layout Erratum) + Phase 4 Runtime Wiring & Reversible Cutover (PRs #123–#153).
> **Reference Implementation & Golden Vectors**: [`crates/spawn-cas/`](../crates/spawn-cas/) and [`crates/spawn-cas/testdata/d6-cas-v1/`](../crates/spawn-cas/testdata/d6-cas-v1/) (landed in PR #115 at commit `9754ad18`).
> Where this document and `crates/spawn-cas/testdata/d6-cas-v1/` disagree, the golden fixtures win.

---

## 1. Content Addressing & Hash Algorithm

- **Single Frozen Algorithm (`sha256`)**: Every CAS address uses `sha256:<64 lowercase hex>` (`Algo::Sha256`).
- **BLAKE3 Permanently Removed**: `Algo::Blake3` and unprefixed 64-hex digests are rejected everywhere (`CasError::BadDigest`). Keeping SHA-256 only guarantees Go `spawn-node` compatibility under `CGO_ENABLED=0` using standard-library `crypto/sha256` alongside Rust `#![forbid(unsafe_code)]`.
- **Logical vs. Physical Separation**:
  - **Logical layer** (`blob`, `spawn-tree-v1`, `spawn-snapshot-v1`, `manifest-v1`, `tree_sha256_v1`): A file's content identity is always the SHA-256 of its complete unchunked bytes. Trees and manifests never reference physical chunk indices or chunk digests.
  - **Physical layer** (`chunk-index/sha256/<2 hex>/<file hex>`): Files larger than `CHUNK_MAX` (4 MiB) are split via FastCDC v1 into chunks stored as ordinary CAS blobs under `objects/sha256/`, indexed by the whole-file digest. Go `spawn-node` never needs a FastCDC chunker—only chunk-index parsing and per-chunk SHA-256 verification.
- **No Type Inference from Raw Bytes**: All CAS objects are stored at the SHA-256 of their raw bytes and verified on every write (`Store::put`, `Store::commit`, `Store::publish`, `Store::put_blob`) and read (`Store::read`, `Store::stream_blob`). Semantic types are assigned top-down from typed PostgreSQL roots (`spawn_snapshots`, `spawn_refs`), backed by explicit magic headers on structured CAS formats.

---

## 2. On-Disk Directory Layout & Comment #6 Erratum

```text
<cas>/.spawn-cas                          # Store::LAYOUT marker: "spawn-cas v1\n" (CAS_MARKER)
<cas>/objects/sha256/<2 hex>/<64 hex>     # Immutable objects (blobs, chunks, trees, snapshots, manifests), mode 0444
<cas>/chunk-index/sha256/<2 hex>/<64 hex> # Store::chunk_index_path — spawn-chunks-v1 index for blobs > 4 MiB
<cas>/pins/<owner>/<name>                 # Atomic pin files; <cas>/pins/.lock (flock) serializes pin vs GC
<cas>/access/<ns>/sha256/<64 hex>         # LRU mtime records per namespace
<cas>/tmp/                                # Staging & resumable partial files; swept past TMP_MAX_AGE (7 days)
```

### Layout Erratum (`chunk-index/` vs `index/`, Issue #113 Comment #6)

- Initial drafts named `<cas>/index/` for physical chunk indexes. That path collides with legacy shared CAS roots because historical `cas_dedup` shards wrote `<cas>/index` as a **regular file** (explicitly guarded by `crates/spawn-cas/tests/store.rs::legacy_shards_are_adopted_in_place_without_rehashing`), causing `EISDIR` / `CasError::NotPlain`.
- **Frozen Path**: Both Rust (`Store::chunk_index_path` / `Store::LAYOUT = ["objects", "chunk-index", "pins", "access", "tmp"]`) and Go `cas` use **`chunk-index/sha256/<2 hex>/<file hex>`**.

### Safety & Crash-Only Invariants

- All objects and chunk-index files are opened with `O_NOFOLLOW | O_NONBLOCK`; symlinks and special files are rejected (`CasError::NotPlain`).
- Non-empty directories lacking `.spawn-cas` are rejected (`CasError::NotCas`).
- Writes stage in `<cas>/tmp/`, verify digests, set mode `0444`, and `rename` atomically into place.

---

## 3. Namespaces, Durability Barrier & Garbage Collection

### Namespaces (`crates/spawn-cas/src/gc.rs`)

| Namespace Constant | Wire Name | Eviction Policy | Purpose |
| --- | --- | --- | --- |
| `NS_ARTIFACTS` | `artifacts` | LRU (default 20 GiB) | Job artifacts, controller/registry packs |
| `NS_BASE_CACHE` | `base-cache` | LRU (default 50 GiB) | Layer 0 `base/<commit>` caches |
| `NS_BACKUP` | `backup` | Explicit / unquoted by default | Backup staging cache |
| `NS_DATA` | `data` | Explicit / pin-protected | Data snapshots (`manifest-v1`) |
| `NS_TREE` | `tree` | **Persistent (never LRU-evicted)** | Canonical `spawn-tree-v1` & `spawn-snapshot-v1` objects |
| `NS_BLOB` | `blob` | **Persistent (never LRU-evicted)** | File contents & FastCDC chunks referenced by trees |

### Persistent Publish Order (`Store::sync_store` & `refs.journal`)

1. Write chunks (if `> 4 MiB`), `chunk-index`, file blobs, child-to-root trees, and snapshot object.
2. Call `Store::sync_store()` (single `syncfs` on Linux) so all new objects and indexes are durable on disk before PostgreSQL references them.
3. Commit the PostgreSQL transaction (`spawn_snapshot_record` / `spawn_ref_cas`). `spawn_snapshot_record` returns the canonical snapshot id: when a concurrent writer already recorded the same Git commit (same `git_oid`, `tree`, `parents`, `project`) under another id or `created_at_ms`, it returns that row's id (first writer's `created_at_ms` wins) instead of raising `23505` on `spawn_snapshots_git_oid_key`, and the ref move must name the returned id. Mismatched content still raises `23505`.
4. Append and `fsync` the storage root's `refs.journal` (used for fast-forward reconciliation across backup restore windows).

### Single-Owner GC & Fence

- Rust host `spawn-cas` holds exclusive GC authority on the shared CAS root (under `flock` on `<cas>/pins/.lock`). Go `spawn-node` workers never run GC on shared host CAS and register newly written blobs under the shared lock.
- GC marks roots from live PostgreSQL `spawn_refs` and removes unpinned persistent objects only after **2 consecutive unreachable sweeps** and **> 24 hours** age (`TMP_MAX_AGE = 7 * 24h` for abandoned `tmp/` entries).
- **GC Fence**: Each CAS root binds its originating PostgreSQL cluster identity; GC refuses to run if pointed at a restore-drill/scratch database or if PostgreSQL trails `refs.journal`.

---

## 4. Canonical Binary Encodings

All multi-byte integers are unsigned big-endian (`u16be`, `u32be`, `u64be`).

### 4.1 `spawn-tree-v1` (`TREE_MAGIC = "spawn-tree-v1\n"`, 14 bytes)

```text
"spawn-tree-v1\n"                          [14 bytes]
entry_count                                u32be
repeat entry_count times:
  kind                                     u8 ('f' | 'x' | 'l' | 'd' | 'g')
  name_len                                 u16be
  name                                     [name_len bytes]
  id                                       [32 bytes raw digest or zero-padded gitlink OID]
  size                                     u64be
```

- **Kinds**:
  - `'f'` (regular non-executable file, Git `100644`): `id = sha256(file_bytes)`, `size = file_len`.
  - `'x'` (regular executable file, Git `100755`): `id = sha256(file_bytes)`, `size = file_len`.
  - `'l'` (symlink target, Git `120000`, never dereferenced): `id = sha256(target_bytes)`, `size = target_len`.
  - `'d'` (non-empty subdirectory, Git `040000`): `id = child tree sha256`, `size = 0`.
  - `'g'` (submodule gitlink, Git `160000`): `id = left-aligned OID padded with trailing 0x00 bytes to 32 bytes`, `size = oid_len` (`20` or `32`).
- **Validation Rules**:
  - `entries` sorted **strictly ascending** by raw byte sequence of `name`; duplicate names forbidden.
  - Entry `name` must be non-empty (`<= u16::MAX`), free of `'/'` and `0x00`, not `.` or `..`, and not case-insensitively equal to `.git`.
  - Empty subdirectories forbidden (a tree containing zero entries exists only as the canonical empty root tree `empty_tree_digest()`, never referenced inside `'d'` entries).
  - No inline file payloads; encoded tree size `<= CHUNK_MAX` (4,194,304 bytes).

### 4.2 `spawn-snapshot-v1` (`SNAPSHOT_MAGIC = "spawn-snapshot-v1\n"`, 18 bytes)

```text
"spawn-snapshot-v1\n"
"tree sha256:<64 lowercase hex>\n"
repeat 0..N times (in Git first-parent order):
  "parent sha256:<64 lowercase hex>\n"
"commit <decimal byte length of commit_body>\n"
<commit_body verbatim raw bytes>
```

- Preserves everything in the Git commit object after `tree`/`parent` lines (`author`, `committer`, `gpgsig`, custom headers, blank line, commit message) so bidirectional Git OID projection is exact.
- Maximum encoded snapshot object size `<= CHUNK_MAX` (4,194,304 bytes).

### 4.3 `spawn-chunks-v1` (`CHUNKS_MAGIC = "spawn-chunks-v1\n"`, 16 bytes)

Stored at `<cas>/chunk-index/sha256/<2 hex>/<file_id hex>`:

```text
"spawn-chunks-v1\n"                        [16 bytes]
file_id                                    [32 bytes raw SHA-256 of complete file]
file_size                                  u64be
chunker                                    u8 (CHUNKER_FASTCDC_V1 = 1)
count                                      u32be
repeat count times:
  chunk_id                                 [32 bytes raw SHA-256 of chunk]
  len                                      u32be
```

- Requires `sum(chunk.len) == file_size`, `1 <= chunk.len <= CHUNK_MAX`, and every `chunk_id` stored as an ordinary blob under `<cas>/objects/sha256/<2 hex>/<chunk_id hex>`.

---

## 5. FastCDC v1 Specification (`chunker = 1`)

- **Size Bounds**:
  - `CHUNK_MIN = 262_144` (256 KiB)
  - `CHUNK_AVG = 1_048_576` (1 MiB)
  - `CHUNK_MAX = 4_194_304` (4 MiB) — hard ceiling for every single CAS object write (`objects/`).
- **Gear Table (`fastcdc_gear_table`)**:
  For each byte value `i` in `0..=255`:
  `gear[i] = u64::from_be_bytes(sha256(b"spawn-fastcdc-v1" || [i])[0..8])`.
- **Cut-Point Evaluation**:
  For a chunk starting at offset 0 of remaining bytes `buf`:
  1. If `buf.len() <= CHUNK_MIN`, return `buf.len()`.
  2. Set rolling fingerprint `fp: u64 = 0` (the first `CHUNK_MIN` bytes do not mutate `fp`).
  3. For each byte `b` at index `CHUNK_MIN + offset` (`len = CHUNK_MIN + offset + 1`, stopping at `limit = min(buf.len(), CHUNK_MAX)`):
     - `fp = (fp << 1).wrapping_add(gear[b])`
     - If `len == CHUNK_MAX`, cut at `CHUNK_MAX`.
     - If `len < CHUNK_AVG` and `(fp & 0xFFFF_FC00_0000_0000) == 0`, cut at `len`.
     - If `len >= CHUNK_AVG` and `(fp & 0xFFFF_C000_0000_0000) == 0`, cut at `len`.
  4. Otherwise cut at `limit`.

---

## 6. Git OID Projection & `GIT_POINTER_THRESHOLD`

To preserve existing 40-hex `^[0-9a-f]{40}$` database columns (`spawn_commit_map`, `Spawn-Head`, controller paths, and remote node bundles) without using SHA-1 as a CAS key:

- **`sha1` Kept in `Cargo.lock`**: Used strictly inside `crates/spawn-cas/src/git_oid.rs` for boundary Git OID projection; prohibited for identity or deduplication.
- **Immutable Pointer Threshold (`GIT_POINTER_THRESHOLD = 104_857_600` bytes / 100 MiB)**:
  - Blobs `<= 100 MiB`: `project_blob_oid` streams bytes as Git `"blob <len>\0" || content`.
  - Blobs `> 100 MiB`: `project_blob_oid` hashes the deterministic pointer text:
    ```text
    spawn-pointer-v1\nsha256:<64 hex>\nsize <decimal len>\n
    ```
    via `git_blob_oid_of_bytes`, never reading `SPAWN_MAX_FILE_MB` into format identity.
- **Trees & Snapshots**: `project_tree_oid` and `project_snapshot_oid` emit standard Git tree sorting (`'/'` suffix appended conceptually for `'d'` entries during name comparison), Git modes (`100644`, `100755`, `120000`, `40000`, `160000`), and `"commit <len>\0"` envelopes, verified against `git mktree` / `git commit-tree` in `crates/spawn-cas/tests/d6_cas.rs`.

---

## 7. Cross-Language Golden Vectors (`d6-cas-v1`)

From [`crates/spawn-cas/testdata/d6-cas-v1/manifest.json`](../crates/spawn-cas/testdata/d6-cas-v1/manifest.json):

| Artifact | Content / Parameter | Expected Digest / Git OID |
| --- | --- | --- |
| PRNG payload (`4,500,000 B`) | `seed = 0xd6ca500000000001` | `sha256:0ccaae4305982d0cd0f1cee834881529401c97b0b892d3e2df7b4630c00b9733` |
| FastCDC chunks (`len`) | `1673558`, `1124851`, `1701591` | `chunks.bin`: `sha256:259114546bd73422f09adda871d973c02e9cc486ab707eb8d94146f840dcf0eb` |
| `fastcdc_gear_table()` | `gear[0]` / `gear[255]` | `5c00726f6e53438d` / `b89aa1d6e03c08da` |
| `child_tree.bin` | `spawn-tree-v1` subtree | `sha256:c16775f31b313ba938438cb4171b938ca806bd44d515b509ef9c6599f6c9b1c8` (`git_oid`: `1796849b16126f89e019a42e46cd230a0f1af5f2`) |
| `tree.bin` | `spawn-tree-v1` root | `sha256:b71871a89314f5692d116601867087bbf9c346b06bb642ea3fcee9a43c237f04` (`git_oid`: `15d93bf21d816cb56df8cc24a88924baa258f2ea`) |
| `snapshot.bin` | `spawn-snapshot-v1` | `sha256:8af322720a065f56189fdeaf56f179f9723048fd650dee075796d10579a55930` (`git_oid`: `3cd44f5bc0e4390b9d8a4b2c73640b9ecd330628`) |
| `parent_snapshot` | Parent snapshot | `sha256:471a4bf181250d3c686b9fdcf7408606bbe106d1838dbaf204d678b8bcef5581` (`git_oid`: `bfeffdef1dcffa8384d823138a3ed8369f256044`) |
| `pointer-100mib.bin` | `size = 104857601` (`> 100 MiB`) | `sha256:00671b57af3a04d3b7e0db1cf050b443f63bea02cf444a3b866f66c6604cc990` (`git_oid`: `66718ac288c87df8bde087664ce9ccf7410dd0d5`) |

---

## 8. Multi-CAS Format Boundary (Issue #113 Safeguard #2)

Spawn maintains strict physical and domain separation across all storage subsystems:

1. **D6 Host CAS (`spawn-cas`, this document)**: Lives at `<cas>/{objects,chunk-index,pins,access,tmp}` with marker `.spawn-cas`. Owns `artifacts`, `base-cache`, `backup`, `data`, and persistent `tree`/`blob`. Adopts `<cas>/<2 hex>/<64 hex>` legacy shards via `Store::adopt_legacy_shards` while leaving legacy `<cas>/index` regular files intact.
2. **`fm-cas/v1` Recoverable Backup CAS (`crates/spawn-cli/src/backup/recoverable.rs`, [`docs/recoverable-storage-keys.md`](./recoverable-storage-keys.md))**: Strictly namespaced under `<store_root>/fm-cas/v1/` with `spawn.cas.v1.*` domain tags and its own `.cas.lock` / `snapshots.json`. Never overlaps runtime host CAS roots; future incompatible backup versions must advance to `fm-cas/v2`.
3. **`spawn store` Off-Host Transcript & Session Segments (`crates/spawn-cli/src/backup/store/`)**: Upload-only encrypted backup store (`transcripts/`, `sessions/`) keyed by HMAC-SHA256 of plain segments under `.json.gpg` manifests. Completely independent of D6 runtime CAS.
4. **Tombstoned `Spawn-Large` Git Trailer (`crates/spawn-host/src/workspace_rules.rs::LARGE_FILE_TRAILER`, [`docs/provenance.md`](./provenance.md))**: Permanently tombstoned. D6 snapshots and commits must **never** reintroduce or interpret `Spawn-Large` commit trailers (large files live transparently in D6 `spawn-chunks-v1` + `chunk-index/`, projected at Git borders via `spawn-pointer-v1`).

---

## 9. Runtime Configuration, Backend Compatibility & Bounded L1 `tmpfs` Tier

`git` remains the default backend when `SPAWN_WORKSPACE_BACKEND` is unset/empty, and `auto` selects `git` on clean roots while preserving existing `.spawn-workspace-backend`, `.d6/`, or `.sources/` / workspace directories in both [`spawn-host`](../crates/spawn-host/src/workspace.rs) and [`spawn-cli`](../crates/spawn-cli/src/config.rs); pure-Rust `d6` is selected explicitly via `SPAWN_WORKSPACE_BACKEND=d6` or cutover (`spawn storage cutover`).

### Environment Contract (`crates/spawn-cli/src/config.rs`)

| Environment Variable | Accepted Values / Type | Default | Semantics |
| --- | --- | --- | --- |
| `SPAWN_WORKSPACE_BACKEND` | `git`, `d6`, `auto` | `git` | Selects `LocalWorkspaceStore` backend; `git` retains the default working copy mode, `d6` opts into pure-Rust `D6Workspace` with zero `git` subprocesses or `.sources/*.git` mirror clones, and `auto` follows existing root markers/`.d6`/workspaces else `git` |
| `SPAWN_D6_CAS_DIR` | Absolute directory path | `<root>/.d6/cas` | Overrides CAS Merkle + `refs.journal` root (`CasMerkleStore::open`) |
| `SPAWN_D6_TMPFS_ROOT` | Absolute directory path | `None` (disk-only Layer 0) | Opts into bounded L1 `tmpfs` upperdir mounting (`<tmpfs_root>/<branch>`) |
| `SPAWN_D6_TMPFS_BUDGET_MB` | Positive integer (`u64 > 0`) | `2048` MiB when `SPAWN_D6_TMPFS_ROOT` is set | Hard host-wide L1 RAM ceiling across admitted active branches |

Invalid paths (relative `SPAWN_D6_CAS_DIR` or `SPAWN_D6_TMPFS_ROOT`) or non-positive `SPAWN_D6_TMPFS_BUDGET_MB` fail closed during startup (`WorkspaceSettings::from_env` / `WorkspaceSettings::peek`).

### Root Backend Compatibility Matrix (`backends_compatible`)

[`claim_root` / `root_backend` / `backends_compatible`](../crates/spawn-host/src/workspace.rs) and `WorkspaceSettings::from_env` enforce strict isolation before opening or creating `.d6/cas`:

| Existing Root Marker | Requested Backend | Outcome | Notes |
| --- | --- | --- | --- |
| Clean / empty root | unset, `git`, or `auto` | **`git`** | Claims clean root for `GitWorkspace` (default until separate gated Wave-1 cutover) |
| Clean / empty root | `d6` | **`d6`** | Opts clean root into native `D6Workspace` |
| `git` (`.spawn-workspace-backend`, `.sources/`, or existing workspaces) | `auto` or `git` | **`git`** | Preserves existing `GitWorkspace` root until explicit cutover |
| `git` (`.spawn-workspace-backend`, `.sources/`, or existing workspaces) | `d6` | **Refused** | Pure `d6` cannot share a root with `git` without explicit cutover (`WorkspaceSettings::from_env` refuses before creating `.d6/cas`) |
| `d6` (`.spawn-workspace-backend` or `.d6/`) | `git` | **Refused** | Pure `d6` root cannot be claimed by `git` without lossless rollback |

Retired markers (`cow`, `overlay`, `d6-shadow`) and unmarked legacy `.cow/`, `.overlay/`, or `.d6-shadow/` directories fail closed in `root_backend` rather than silently mapping to `git` or `d6`.

### Bounded L1 `tmpfs` Admission & Pressure Policy (`crates/spawn-host/src/d6_workspace.rs`)

When `L1TmpfsPolicy` is active (`SPAWN_D6_TMPFS_ROOT` set):

- **Host Memory Reserve Floor**: `max(min(64 GiB, total RAM / 2), 15% total RAM)` (`DEFAULT_L1_MIN_HOST_RESERVE_BYTES = 64 GiB`, `DEFAULT_L1_HOST_RESERVE_PCT = 15`). The absolute minimum is capped at half of host RAM, so a 128 GiB host keeps 64 GiB, a 1 TiB host keeps 15% (153.6 GiB), an 18 GiB host reserves 9 GiB, and a 16 GiB host reserves 8 GiB.
- **Deterministic Disk Fallback (`L1Admission::FallbackDisk`)**: Mounts transparently under disk `<root>/<branch>` instead of `tmpfs` on `Disabled`, `HostMemoryBelowReserve`, `BudgetExceeded` (over global or per-workspace quota), `TmpfsFreeSpaceInsufficient` (`statvfs`), or runtime `TmpfsUnavailable`.
- **Pressure & Periodic Flush**:
  - `usage_pct >= 80%` (`DEFAULT_L1_FLUSH_THRESHOLD_PCT = 80`) or elapsed `>= 60_000 ms` (`DEFAULT_L1_FLUSH_INTERVAL_MS = 60s`) triggers `flush_l1_workspace` into CAS Merkle + PostgreSQL `RefLedger`.
  - Usage pressure (`UsageThreshold`) demotes the mounted branch back to disk and reclaims its L1 RAM quota; `unmount` and `discard` also release reserved L1 bytes back to `0`.

---

## 10. Reversible Cutover, Border Mirror & Crash-Recovery Runbook

1. **7-Step Lossless Project Cutover (`crates/spawn-host/src/d6_cutover.rs::ProjectCutover`)**:
   - **Step 1 (`step1_backfill_history`)**: Import commit DAG, trees, FastCDC chunks, and blobs into CAS + `spawn_snapshots` (`spawn_refs` untouched).
   - **Step 2 (`step2_pause_dispatch_and_verify_clean`)**: Pause dispatch, fail closed if any `.merge/*` or pending markers remain (`assert_zero_pending_merges`), and audit `.gitattributes` (`export-ignore`, `export-subst`, `eol`) via `audit_export_attributes`.
   - **Step 3 (`step3_import_tail_commits`)**: Import tail commits landed during drain.
   - **Step 4 (`step4_commit_refs_and_flip_mode`)**: Commit non-WIP refs (`main`, branches, tags) to `spawn_refs`, append `refs.journal`, and switch project mode to `ProjectStorageMode::D6Cas`.
   - **Step 5 (`step5_verify_parity`)**: Prove `RefLedger git_oid == CAS projection == git rev-parse` for every ref (`verify_cutover_parity`).
   - **Step 6 (`step6_configure_readonly_border`)**: Install `receive.deny*` configs and `hooks/pre-receive` on the bare Git border mirror so direct Git pushes fail closed.
   - **Step 7 (`step7_resume_dispatch`)**: Resume mission dispatch on native `d6` (`ProjectStorageMode::D6Cas`).
3. **External Export & Lossless Rollback (`GitBorderAdapter` & `rollback_to_git`)**:
   - At any time after cutover, `GitBorderAdapter::export_project` materializes all live `spawn_refs` (`<= 100 MiB` blobs streamed directly, `> 100 MiB` via `spawn-pointer-v1`) into an external bare repository with byte-exact tree/commit OID parity verified by `git fsck --full --strict`.
   - `ProjectCutover::rollback_to_git` exports all post-cutover D6 refs back to the project's bare repo, lifts read-only receive locks, verifies `verify_cutover_parity`, and restores `ProjectStorageMode::Git` cleanly.
4. **GC Fence & `refs.journal` Crash Recovery**:
   - Bind each persistent CAS root once via `bind_store_gc_fence` (`.gc-fence` records `db_system_id`, `db_oid`, `schema_name`).
   - Restore-drill or scratch databases fail closed with `CasError::GcFenceMismatch`; lagging PostgreSQL restores fail closed with `CasError::GcFenceBehindJournal`.
   - After restoring PostgreSQL from backup, run `reconcile_refs_journal` (`spawn storage reconcile`) to idempotently fast-forward missing `spawn_ref_cas` / `spawn_ref_land_main` transitions (ignoring any torn trailing JSON line) until `pg_latest_ref_seq >= journal_latest_seq`, then allow 2-round / `> 24h` persistent GC (`run_persistent_gc_for_ledger` / `spawn storage gc`).
   - Run offline R2 `git bundle` export -> `git bundle verify` -> `git fsck --full --strict` -> blank-CAS import verification via `verify_bundle_snapshot_roundtrip` (`spawn storage verify-bundle --bundle <output-path>`).
5. **Operator Storage CLI (`spawn storage`)**:
   - `spawn storage cutover --status [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--json]`: Read-only inspection (never creates `.spawn-cas`) reporting `.spawn-workspace-backend`, `.merge/*` cleanliness, CAS initialization, PostgreSQL identity, `.gc-fence` alignment, bare-repo border status, `spawn_refs` count, and `<cas_dir>/.cutover-state.json`.
   - `spawn storage cutover --to d6 [--project <name>] --repo <bare repo> [--root <dir>] [--cas-dir <dir>] [--reason <text>] [--json]`: Pre-checks clean `.merge/*`, binds/checks `.gc-fence`, executes `ProjectCutover` steps 1..7, persists `<cas_dir>/.cutover-state.json` (including partial phase on post-Step-4 failure), and atomically flips `<root>/.spawn-workspace-backend` to `d6`.
   - `spawn storage cutover --verify [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--json]`: Read-only parity + border + GC-fence + `.merge/*` + workspace-marker audit; exits non-zero if any check fails or post-cutover ref state drifts.
   - `spawn storage cutover --rollback git [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--reason <text>] [--json]`: Verifies CAS + `.gc-fence` + persisted `d6_cas` state, runs `ProjectCutover::from_state(...).rollback_to_git(...)` to export live refs and restore bare-repo writability, updates `.cutover-state.json` to `rolled_back`, and flips `<root>/.spawn-workspace-backend` back to `git` unless another project sharing `<root>` stays on `d6_cas`.
   - `spawn storage reconcile [--root <dir>] [--cas-dir <dir>] [--reason <prefix>] [--json]`: Fails closed unless CAS is initialized and `<cas_dir>/.gc-fence` matches live PostgreSQL (`db_system_id`, `db_oid`, `schema_name`), then invokes `spawn_host::reconcile_refs_journal` to replay trailing `refs.journal` entries (`seq > pg_latest_ref_seq`) idempotently until PG catches up with `refs.journal`; exits non-zero (fail closed) if `gc_fence_aligned` remains `false`.
   - `spawn storage gc [--root <dir>] [--cas-dir <dir>] [--grace-secs <secs>] [--json]`: Fails closed unless CAS is initialized, `<cas_dir>/.gc-fence` matches live PostgreSQL, and `pg_latest_ref_seq >= journal_latest_seq`, then runs `spawn_host::run_persistent_gc_for_ledger` (defaulting grace to `DEFAULT_PERSISTENT_GC_GRACE = 86400s` = 24h; minimum `--grace-secs >= 1`, refusing `0`) across all live refs in PostgreSQL.
   - `spawn storage verify-bundle --bundle <output-path> [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--json]`: Fails closed unless CAS is initialized, `<cas_dir>/.gc-fence` aligns with live PostgreSQL, and `--bundle` is a non-existing output file path outside `--cas-dir` and any bare repository with zero symlinks along its full ancestor path (including `link/../x.bundle`); runs `spawn_host::verify_bundle_snapshot_roundtrip` via an exclusive random `0700` staging directory + create-new publication and reports `bundle_bytes`, `snapshots_verified`, `refs_verified`, and any `> 100 MiB` `local_only_large_files`.
6. **Worker Head Preservation (`D6Workspace::pin_worker_head`)**:
   - Both `LocalWorkspaceStore::pin_worker_head` (`Self::D6`) and live CLI worker pinning (`crates/spawn-cli/src/sources.rs::d6_pin_worker_head`) route directly through pure-Rust `D6Workspace::pin_worker_head(branch, directory, target_refs)` with zero Git subprocesses under `<cas_root>/.refs-journal.lock`.
   - Validates `branch` and every `target` in `target_refs` against strict pure-Rust Git ref-name rules under `refs/spawn/...` only, refuses symlink workspace directories and symlinked `.git` entries fail-closed via the workspace scanner, and never reads through `.git` pointer files (matching `D6Workspace::checkpoint` behavior).
   - Scans working-tree files directly into CAS (`NS_BLOB` + `NS_TREE` + `spawn-snapshot-v1`), capturing any files checked out in the working tree while excluding `.git/` internal metadata and honoring `.git/info/exclude` for regular `.git` directories.
   - Always derives from the live `branch` snapshot when present (ignoring stale worker refs left behind by prior tasks; `discard` also resets stale worker refs back to `branch`), and falls back to persisted `refs/spawn/workers/<branch>/head` only when `directory` is `None`/absent and no `branch` snapshot exists; publishes PG `RefLedger` first, followed by `refs.journal` append with retry self-healing if a prior `refs.journal` append failed after PG commit (without mutating `refs/heads/*`).
