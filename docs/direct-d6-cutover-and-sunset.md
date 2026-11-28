# Direct D6 cutover and legacy-backend sunset

Status: executable runbook and retirement inventory, audited against `origin/main`
`1aac7b9e40dcee591016fe81ca26cc23953bb633`. **The current main has not switched
its default yet**: `crates/spawn-cli/src/config.rs::workspace_backend` still maps an
unset `SPAWN_WORKSPACE_BACKEND` to `WorkspaceBackend::Git`, and
`crates/spawn-host/src/workspace.rs::WorkspaceBackend` still has five variants.
This document does not claim those Wave-1/2 changes have landed.

There is no external compatibility promise to preserve. Cut over once to the
converged version; do not add aliases, dual writes, runtime fallback, or a second
transitional state machine. Back up first, initialize a genuinely empty target,
and start Always-On writers only after every proof below passes.

## 1. One-step production cutover

### Preconditions

1. Land and deploy the Wave-1/2 pure-D6 implementation and its benchmark gates.
2. Configure `spawn-backup-verify.service` including its separate drill database.
   A backup whose object merely appears in a listing is not enough: this unit
   restores the newest encrypted dump and opens the newest source/transcript
   bundle (`crates/spawn-cli/src/backup.rs::verify` and `verify_bundle`).
3. Provision a new, empty PostgreSQL database/schema. Do not point the tool at the
   live database. The tool checks `current_database()`, `current_schema()`, and
   that the selected schema has no `pg_class` or `pg_proc` entries.
4. Provision a new D6 CAS root. Do not reuse a CAS whose `.gc-fence` is bound to
   the old database identity (`docs/d6-cas-v1.md`). A clean DB plus an old,
   bound CAS is not a clean cutover.
5. Export the target owner URL into `SPAWN_CLEAN_DATABASE_URL` through the normal
   secret manager and set `SPAWN_CLEAN_EXPECT_DATABASE` to its exact database
   name. Never put either value in shell history, logs, tickets, or unit files
   readable by other users.

### Offline preview (default)

```sh
deploy/clean-remigrate-db.sh --dry-run \
  --archive-dir /absolute/new/cutover-archive
```

The default invocation and `--dry-run` execute no command, read no credential,
and create no file. Unknown arguments, duplicate/conflicting modes, relative
archive paths, and invalid schema identifiers fail closed.

### Execute

```sh
deploy/clean-remigrate-db.sh --execute \
  --archive-dir /absolute/new/cutover-archive \
  --confirm-clean-remigrate=EXPECTED_DATABASE
```

The confirmation must exactly equal `SPAWN_CLEAN_EXPECT_DATABASE`; it is a
non-secret database identifier, not a URL. The ordered hard gates are:

1. `systemctl --user start --wait spawn-backup.service` must succeed.
2. `spawn-backup-bundle.service` must succeed, preserving transcripts and source
   history.
3. `spawn-backup-verify.service` must succeed, proving the latest encrypted DB
   object restores and the latest transcript/source bundle opens. Any failure
   stops before archive creation, writer shutdown, or target access.
4. `spawn node list --json` is written as `nodes.json` mode `0600` under a mode
   `0700`, new/empty archive directory. This file is operational metadata, not a
   public artifact. No token/private key is requested or printed.
5. Stop and prove `inactive`: `spawn-loop.service`, `spawn-integrate.service`,
   `spawn-serve.service`, `spawn-core.service`, and `spawn-node-gateway.service`.
   Stopping only HTTP leaves the loop/integrator writing and is not a cutover.
6. Prove the expected target identity and empty schema, then run `spawn init-db`
   with the target URL supplied only through its environment. The script never
   drops a live schema; destruction is outside its authority.
7. Verify all of the following or exit nonzero:
   - `spawn_events.commit_xid` has PostgreSQL type `xid8`;
   - `spawn_events`, `spawn_snapshots`, `spawn_refs`, `spawn_ref_log`,
     `prov_changes`, `prov_entries`, and `prov_edges` exist;
   - the three provenance tables' update/delete and truncate triggers, both
     snapshot guards, and both ref-log guards exist;
   - `spawn_login_role` resolves and `pg_roles.rolcanlogin` is true for
     `spawn_supervisor`, `spawn_reader`, `spawn_human`, and
     `spawn_node_gateway`.

On every failure the writers remain stopped. On success they also remain
stopped deliberately. Update the deployed secret references and select a new
empty D6 CAS root through the normal secret/configuration path, run one more
`spawn init-db` smoke check there, then start in this order:

```sh
systemctl --user start spawn-serve.service spawn-core.service
systemctl --user start spawn-node-gateway.service
systemctl --user start spawn-loop.service spawn-integrate.service
```

Check service state and logs, run the D6/read-write smoke suite, and only then
enable the timers for Always-On operation. Do not automatically roll back to the
old backend: restore the verified backup into another clean target if recovery
is necessary.

Tests: `deploy/test-clean-remigrate-db.sh` mocks every `systemctl`, `spawn`, and
`psql` call. It covers offline dry-run, argument rejection, backup/verify
failure, live-writer refusal, nonempty-target refusal, `xid8`/role failures, and
the successful ordered path, and scans output/call logs for a made-up password.

## 2. Exact sunset inventory after Wave-1/2

Wave-1 must first make pure `D6` the only default in
`crates/spawn-cli/src/config.rs` and `crates/spawn-cli/src/sources.rs`, and Wave-2
must prove persistent D6 lifecycle, recovery, CAS/ledger integrity, and
Always-On load. **Delete only after those commits are in the same integrated
tree.** Do not leave a parser that accepts a backend which cannot be opened.

### Step A — remove entire legacy engines and their dedicated tests

| Delete | Current lines | Notes |
| --- | ---: | --- |
| `crates/spawn-host/src/cow.rs` | 3,950 | `CowWorkspace`, reflink baseline/prune machinery |
| `crates/spawn-host/src/overlay.rs` | 3,142 | `OverlayWorkspace`, layers/probes |
| `crates/spawn-host/src/d6_shadow.rs` | 2,076 | dual-track oracle, shadow-only outcomes/metrics |
| `crates/spawn-host/tests/cow.rs` | 1,895 | dedicated CoW suite |
| `crates/spawn-host/tests/overlay.rs` | 1,988 | dedicated overlay suite |

Subtotal: **13,051 deleted lines**. Keep general D6 tests in
`crates/spawn-host/tests/d6_workspace.rs`; edit rather than delete
`d6_layer0_cutover.rs` and `local_workspace.rs` because they also cover retained
D6/border behavior.

### Step B — collapse the closed backend ADTs and dispatch in one commit

In `crates/spawn-host/src/workspace.rs`:

- delete `WorkspaceBackend::{Cow, Overlay, D6Shadow}` and their `name`/`parse`
  arms; for a pure-D6 runtime, also remove `Git` as a selectable runtime backend
  rather than silently mapping it to D6;
- delete `D6ShadowOptions`, `LocalWorkspaceStore::{Cow, Overlay, D6Shadow}`,
  constructors `cow`, `overlay`, `d6_shadow`, accessors `shadow_metrics` and
  `d6_shadow_workspace`, and every corresponding `match` arm in `open`, `root`,
  `prune_unused`, `set_aside_when`, `commit_or_take_back`, `pin_worker_head`,
  `conflict_markers`, and the remaining dispatch methods;
- replace `WorkspaceBackend::auto_with` Cow/Git probing with the single D6
  construction path (or delete `auto*` entirely). Remove root compatibility
  between Git and D6Shadow; a clean root claims only D6;
- keep `D6StoreOptions`, `D6Workspace`, `DynRefLedger`, and D6 L1 accounting.

In `crates/spawn-host/src/lib.rs`, delete module exports for `cow`, `overlay`,
and `d6_shadow`, plus `CowWorkspace`, `OverlayWorkspace`, `ShadowD6Workspace`,
`ShadowOracleMetrics`, `ShadowTrackedSetMode`, `cow_clone_supported`,
`overlay_supported_in`, `sweep_probes`, and shadow outcome exports. Do not leave
`pub use` compatibility stubs.

In `crates/spawn-cli/src/config.rs` and `crates/spawn-cli/src/sources.rs`:

- delete CLI `WorkspaceBackend::{Cow, Overlay, D6Shadow}` and the probing/
  fallback/overlay-refusal branches;
- delete `D6RuntimeConfig.strict_fail_closed` and
  `SPAWN_D6_STRICT_FAIL_CLOSED`: strict behavior belongs to the one D6 core, not
  a shadow-only switch;
- delete `LocalWorkspaceStore::D6Shadow` reconstruction branches, Git-primary
  shadow roots, `.d6-shadow/{cas,work}` selection, and Cow-only artifact-dedup
  launch behavior. Keep content-addressed artifact dedup only if it has a
  backend-independent consumer; otherwise delete it rather than route D6 through
  Cow helpers;
- update `crates/spawn-cli/tests/config_toml.rs`,
  `crates/spawn-host/tests/local_workspace.rs`, and the shared portions of
  `crates/spawn-host/tests/d6_layer0_cutover.rs` to assert the single closed D6
  state, not five-state parse/compatibility matrices.

Estimated glue/test reduction: **1,000–1,600 lines**. Measure the final diff;
the estimate is not a substitute for symbol search.

### Step C — preserve only the actual Git border

Keep `crates/spawn-host/src/git_border.rs::GitBorderAdapter`,
`BorderRefTarget`, `git_process`, and the read-only border/GC-fence logic needed
for GitHub and git-CLI interchange. `git_oid` is a border mapping, not a second
workspace backend or permission to restore Git dispatch. Do **not** delete
`d6_cutover.rs`/`d6_layer0.rs` wholesale: they contain persistent cutover,
export, parity, and fence primitives. After one-step cutover, separately remove
unused rollback-only entry points if nothing in production calls them; do not
replace them with dead no-op implementations.

`crates/spawn-host/src/git.rs::GitWorkspace` may be deleted only after the final
caller search proves it is not needed by retained border/export tests. It is not
part of `GitBorderAdapter`, which directly uses `git_process`.

### Step D — retire obsolete documentation, not history

Remove or rewrite the five-backend statements in `docs/storage.md`,
`docs/architecture.md`, `docs/design.md`, `docs/glossary.md`,
`docs/d6-cas-v1.md`, `docs/upgrading.md`, `docs/fleet-architecture.md`, and
`docs/storage-benchmark.md`. `docs/cross-os-cow-storage.md` (424 lines) and
`docs/layerfs-v2-overlay.md` (419 lines) can move to clearly marked historical
research outside the operational manual, or be deleted: **843 lines**. Never
leave current commands recommending `cow`, `overlay`, or `d6-shadow`.

Total estimated net reduction after integrated cleanup: **about 14,500–16,000
lines**, excluding the new smaller pure-D6 assertions.

### Zero-omission retirement gate

Run this in the cleanup PR and require no production hits (historical documents
must be explicitly labeled if retained):

```sh
grep -RInE 'WorkspaceBackend::(Cow|Overlay|D6Shadow)|LocalWorkspaceStore::(Cow|Overlay|D6Shadow)|D6ShadowOptions|ShadowD6Workspace|SPAWN_D6_STRICT_FAIL_CLOSED|\.d6-shadow' crates deploy scripts
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p spawn-host
cargo test --locked -p spawn-cli
cargo test --locked -p spawn-core --test dependency_guard --test feature_coverage
SPAWN_BIN="$PWD/target/debug/spawn" sh rsi/scenarios/check-features.sh
```

Then run the complete D6 persistence/recovery suite, a clean-DB restore drill,
CAS GC-fence checks, GitBorderAdapter `git fsck --full --strict` export checks,
and the pinned Always-On benchmark before starting writers. A compile-only
cleanup is not cutover evidence.

## 3. Parallel-change boundaries

This tooling/runbook intentionally does not edit `crates/spawn-cas/`,
`crates/spawn-postgres/sql/schema.sql`, `crates/spawn-cli/src/config_file.rs`,
`crates/spawn-host/examples/storage_benchmark.rs`, or
`scripts/bench-always-on.sh`. The inventory names symbols for the integrated
Wave-1/2 cleanup; it does not perform that cleanup in this PR.
