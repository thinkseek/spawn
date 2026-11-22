# Feature Map Library Extraction & Module Boundary Analysis

- **Initiative**: `t559u` (Slice B: `t559u-slice-b-coverage`)
- **Cross-references**: `t559u-slice-a-mindmap`, `t559u-slice-c-storage-keys`, `t567u-multi-machine-distributed`
- **Decision**: **Establish clean internal library seams inside `spawn-core` now; do NOT prematurely split or publish an external repository/crate/package before consumers stabilize.**

## 1. Question Under Evaluation

As Spawn's feature map expands from a mission status dashboard (`#/features`) into a full-project structural graph with:
1. Interactive mindmap visualization (Slice A),
2. File-universe coverage, wild-file ("野文件") scanning, and graph consistency checks (Slice B / `t565u`),
3. Content-derived storage keys and verifiable snapshot manifests (`fm-cas/v1`, Slice C), and
4. Future multi-machine control-plane metadata designs (`t567u`/`t568u`),

should the feature-map engine remain an integrated module inside Spawn's workspace (`crates/spawn-core` + `crates/spawn-cli`), or be extracted immediately into a standalone reusable library repository / published crate?

## 2. Architectural Comparison

| Dimension | Option A: Integrated Monolith (in `spawn-cli` only) | Option B: Internal Pure Core Seam (in `spawn-core` + adapters in `spawn-cli`) *(Recommended Now)* | Option C: Premature Standalone External Crate / Repo |
| --- | --- | --- | --- |
| **Pure logic vs IO separation** | Mixed; filesystem/PG/HTTP coupled with coverage rules | Strict: `spawn-core::feature_coverage` has **zero IO**, zero PG, zero HTTP | Strict, across repo boundary |
| **CLI & HTTP reuse** | Shared inside one binary only | Both CLI (`spawn features`) and HTTP (`serve.rs`) call the exact same pure functions and types | Requires version bumps & lockfile churn on every schema tweak |
| **Offline / gated CI fit** | Zero friction | Zero friction; compiles inside existing offline Cargo workspace (`Cargo.lock` unchanged) | Breaks offline vendored workflow; adds cross-repo release/gate coordination |
| **Future extractability** | High rewrite cost later | **Drop-in extraction**: moving `feature_coverage.rs` + `storage_keys` to a dedicated workspace crate `feature-map-core` is a mechanical file move with zero API change | Already split, even while schema v1 is still evolving across Slices A/B/C |
| **Schema evolution velocity** | Fast, risk of drift | Fast AND schema-checked (`schema_version: 1`, unit + integration fixtures) | Slow; forces premature semver freezes across 3 active parallel slices |
| **Multi-project reuse** | Impossible without copy-paste | Immediate reuse by any Rust consumer via path/git dependency once stabilized | Immediate external reuse, at high premature maintenance cost |

## 3. Concrete Internal Seam Design (Implemented in Slice B)

To obtain the architectural benefits of a reusable library *without* the tax of premature repository splitting, Spawn enforces four clean internal boundaries:

### 3.1 Pure Graph & Coverage Core (`spawn_core::feature_coverage`)

- **Owned types**: `BoundaryPolicy`, `ExclusionRule`, `FeatureRegistry`, `FeatureNode`, `FeatureKind`, `FeatureRelation`, `RelationKind`, `FeatureProvenance`, `InventoryEntry`, `ScanReport`, `ScanCounts`, `WildFileFinding`, `StaleFileReference`, `ExcludedFileEntry`, `BidirectionalIndex`, `ConsistencyReport`, `AssociationUpdate`.
- **Owned operations**:
  - `normalize_repo_path(&str) -> Result<String, String>`
  - `match_exclusion(&BoundaryPolicy, &str) -> Option<&ExclusionRule>`
  - `validate_registry(&FeatureRegistry) -> ConsistencyReport`
  - `scan_coverage(&FeatureRegistry, &[InventoryEntry], i64) -> ScanReport`
  - `associate_file(&mut FeatureRegistry, &str, &[String], &[InventoryEntry]) -> Result<AssociationUpdate, String>`
- **Hard constraint**: Zero `std::fs`, zero `std::process`, zero network/DB imports, zero `unsafe`. Any host adapter (local filesystem, git tree object, remote agent inventory, or in-memory test fixture) feeds `&[InventoryEntry]` and receives deterministic JSON-serializable values.

### 3.2 Host IO, CLI & HTTP Adapter (`spawn_cli::feature_inventory` + `serve.rs`)

- **Owned responsibilities**:
  - Root-bounded, symlink-safe directory walking (`collect_inventory`), git-tracked detection (`git ls-files -z`), and heavy excluded directory collapsing (`.git/`, `target/`, `node_modules/`).
  - Atomic JSON registry persistence (`save_registry_atomic`: temp file + `fsync` + `rename`).
  - CLI subcommands (`spawn features scan|associate|check`) and HTTP endpoints (`GET /api/features/coverage`, `POST /api/features/scan`, `POST /api/features/associate`) honoring `serve.rs` auth/origin guards.
- **Explicit non-responsibility**: Never duplicates coverage math, exclusion matching, or graph validation rules.

### 3.3 Content-Derived Storage Keys & Manifest Seam (Slice C — `fm-cas/v1`)

- Slice C defines deterministic, content-derived storage keys and verifiable manifests (`fm-cas/v1`) for feature snapshots and artifacts.
- Notice `InventoryEntry` already carries `sha256: Option<String>` and `FeatureRegistry` is deterministically serialized JSON (`schema_version: 1`): Slice B's scan inputs/outputs compose cleanly with Slice C's content-addressed key/manifest format without circular dependencies.

### 3.4 Pure Frontend Read Model (`web/src/featureCoverage.ts` vs `featureCoverageApi.ts`)

- Following Spawn's existing `features.ts` / `featuresApi.ts` convention, normalization and display helpers live in a pure TypeScript module testable directly with `node --test`, completely decoupled from `fetch` and React components.

## 4. Versioning & Stability Contract

- **Registry format**: `schema_version: 1` is verified on load (`load_registry` rejects unknown versions explicitly rather than silently dropping fields).
- **Additive evolution within v1**: New optional fields (`#[serde(default)]`) and new `FeatureKind` / `RelationKind` variants may be added while maintaining backward compatibility for v1 readers; breaking structural changes increment `schema_version` to `2` with an explicit migration tool.

## 5. Clear Trigger Criteria for Future External Extraction

Do **not** split out an external repository or publish a standalone crate until **all** of the following hold:
1. Slices A (mindmap), B (coverage/wild-file scan), and C (`fm-cas/v1` storage keys) have merged and stabilized on `main` through at least one full release cycle without breaking schema v1.
2. A second independent repository (for example, Supply PA or Flake Doctor, or a standalone developer CLI) actively consumes the registry/scanner outside Spawn's workspace.
3. The query/graph/coverage/storage-key traits have proven stable under at least two distinct IO adapters (e.g., local working tree + git bare repo tree / CAS manifest reader).

At that point, extracting `spawn_core::feature_coverage` + Slice C's `fm-cas/v1` types into a standalone workspace crate (`crates/feature-map-core`) and later an external repository will require zero rewrites to CLI, HTTP, or UI consumers.

## 6. Integration Confirmation (`t586u` / `t587u` on `t559u-fm-integrate`)

Integrating all three parallel slices onto `t559u-fm-integrate` (from live `main` `6c5d22782`) empirically confirms **Option B (Internal Pure Core Seam inside Spawn's workspace)** over premature external extraction:

1. **Zero merge conflicts & zero dependency churn**: Slices A (`25d294eeb`), B (`f4ac04887`), and C (`2104c1bf1`) merged cleanly (`ort`) across 30 files with **zero** edits to `Cargo.toml`, `Cargo.lock`, `web/package.json`, `web/pnpm-lock.yaml`, or protected paths (`rsi/`, `scripts/`, `deploy/`).
2. **Clean acyclic module seams verified by `cargo clippy --all-targets -- -D warnings`**:
   - Pure domain coverage core (`spawn_core::feature_coverage`): zero IO, zero PostgreSQL, zero HTTP, zero `unsafe`.
   - Mind-map schema & read endpoint (`spawn_cli::serve::feature_map`) + coverage CLI/HTTP adapter (`spawn_cli::feature_inventory`, `serve.rs`, `main.rs`).
   - Content-addressed recoverable storage engine (`spawn_cli::backup::recoverable`, `fm-cas/v1`) exposed additively under `spawn backup cas`, ready to consume `FeatureRegistry` / `FmGraphSnapshot` and `InventoryEntry` hashes without introducing crate cycles.
   - Pure TypeScript read/graph models (`web/src/features.ts`, `web/src/featuresGraph.ts`, `web/src/featureCoverage.ts`) decoupled from API adapters (`featuresApi.ts`, `featureCoverageApi.ts`) and React UI views (`#/features` Mind Map linked directly to `#/coverage` Wild-File Scan).
3. **Real drift detection across slices**: Running `spawn features scan` and `cargo test -p spawn-core --test feature_coverage` on the combined `t559u-fm-integrate` tree immediately caught the 8 newly-added Slice A + Slice C files (`docs/feature-map-mindmap.md`, `web/src/featuresGraph.ts`, `web/src/components/features/FeatureMindMap.tsx`, `web/src/components/features/FeatureInspector.tsx`, `web/test/featuresGraph.test.ts`, `crates/spawn-cli/src/backup/recoverable.rs`, `crates/spawn-cli/tests/recoverable_storage.rs`, `docs/recoverable-storage-keys.md`) as unassociated ("wild") files relative to Slice B's isolated `registry.json` — validating the scanner's end-to-end drift contract and establishing the exact handoff to `t559u-coverage` (`t559u-fm-audit-coverage`).
4. **Local initiative linkage record (no live API writes, no invented live IDs)**:
   - `t559u` (Mind map, coverage & storage keys): local slices `t559u-fm-mindmap` (`25d294eeb`), `t559u-fm-coverage` (`f4ac04887`), `t559u-fm-storage-keys` (`2104c1bf1`); integration branch `t559u-fm-integrate`; audit branches `t559u-fm-audit-coverage` and `t559u-fm-audit-storage`.
   - `t565u` (One-click unassociated-file / "野文件" scan): tracked under `t559u` in `docs/feature-map/registry.json` (`t565u-wild-file-scan`) and linked 1-click from `#/features` to `#/coverage`.
   - `t567u` / `t568u` (Multi-machine distributed compute & storage): remains a **separate staged research/design initiative** (`t567u-multi-machine-distributed`, `kind: "research-initiative"`), linked to `t559u`, Slice B's library/CLI/API seam, and Slice C's `fm-cas/v1` keys — not a production build, not a cluster conversion, and permanently excluding sold `mac16`.
   - Live `/api/features` remains untouched until ordinary dual independent review, full serial gate, and owner deploy approval succeed.
