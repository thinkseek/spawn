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
