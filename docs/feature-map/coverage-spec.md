# Feature Map Coverage & Unassociated-File ("野文件") Scan Specification

- **Initiative**: `t559u` (Slice B: `t559u-slice-b-coverage`, Owner Requirement `t565u`)
- **Registry path**: `docs/feature-map/registry.json` (`schema_version: 1`)
- **Pure core**: `crates/spawn-core/src/feature_coverage.rs` (zero IO, reusable library module)
- **IO / CLI / HTTP adapter**: `crates/spawn-cli/src/feature_inventory.rs`, `crates/spawn-cli/src/serve.rs`
- **Dashboard UI**: `#/coverage` (`web/src/components/features/CoveragePage.tsx`, `WildFileScanPanel.tsx`)

## 1. Purpose

The capability map (`GET /api/features`, `#/features`) answers *how far each capability has got* (planned → implemented → verified → live). File coverage (`GET /api/features/coverage`, `POST /api/features/scan`, `#/coverage`, `spawn features check`) answers the complementary structural question:

> **Is every in-scope file in this repository genuinely associated with at least one feature, or are there unassociated ("wild" / 野文件) files, stale links, or broken graph relations?**

## 2. Disclosed Project File-Universe Boundary

Coverage claims are meaningless unless the file universe is disclosed explicitly. `BoundaryPolicy` in `docs/feature-map/registry.json` declares both what is **in scope** and what is **explicitly excluded**.

### 2.1 In-scope file categories

1. `tracked-source`: Rust crates (`crates/**`), frontend TypeScript/React/CSS (`web/src/**`), and scripts (`scripts/**`).
2. `tests`: Rust integration/unit test files, frontend tests (`web/test/**`), shell/Python test scripts, and test fixtures.
3. `docs`: Architecture, design, storage, RSI, landing notes, and feature-map documentation (`docs/**`, `README*.md`).
4. `config`: Workspace manifests and tool configuration (`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore`, `web/package.json`, `web/pnpm-lock.yaml`, `web/tsconfig.json`, `web/vite.config.ts`).
5. `assets`: Static web entrypoints, notices, licenses, and data snapshots.
6. `deploy-and-scripts`: Deployment automation, systemd units, backup/restore scripts, and operational tooling (`deploy/**`, `scripts/**`).
7. `rsi-scenarios-and-backlogs`: RSI objective backlogs (`rsi/backlog/**`) and verification/nightly gate scenarios (`rsi/scenarios/**`).
8. `untracked-in-scope-artifacts`: Any newly created untracked file on disk inside the project root that does not match an explicit exclusion rule. Including untracked in-scope files ensures a brand-new wild file is immediately caught by the one-click scan before it is ever committed.

### 2.2 Explicit exclusion policy (never counted as covered)

Every excluded path must match a named `ExclusionRule { id, pattern, category, reason }`:

| Category | Rule IDs / Patterns | Reason |
| --- | --- | --- |
| `vcs` | `.git/**` | Git internal object store and repository metadata |
| `build-cache` | `target/**`, `web/dist/**`, `**/.cache/**`, `**/__pycache__/**` | Derived, rebuildable build and tool caches |
| `dependency-cache` | `**/node_modules/**` | Third-party installed packages, not authored project files |
| `runtime-state` | `.spawn/**`, `.claude/**` | Ephemeral agent/engine execution markers and local worktrees |
| `secrets` | `**/.env`, `**/.env.*`, `**/*.env`, `**/*.key`, `**/*.pem`, `**/secrets/**` | Credentials and private keys are never crawled, hashed, or indexed |
| `editor-temp` | `**/.DS_Store`, `**/*.swp` | OS metadata and transient editor swap files |
| `transient` | `**/*.log`, `**/*.tmp` | Transient diagnostic logs and scratch files |

**Hard invariants on exclusions**:
- Excluded paths are reported in `excluded_files` with their `rule_id`, `category`, and `reason`.
- Excluded paths are **NEVER** counted in `counts.in_scope` or `counts.covered`.
- Excluded paths are **NEVER** listed in `unassociated_files`.
- Attempting to associate an excluded or secret path via CLI or API is rejected with an error.
- The filesystem walker refuses symlinks escaping the project root and never crawls `$HOME` or reads secret file contents.

## 3. Data Model & Invariants

### 3.1 Bidirectional Many-to-Many Index

A single project file frequently serves multiple genuine features (for example, `crates/spawn-cli/src/main.rs`, `crates/spawn-cli/src/serve.rs`, `web/src/App.tsx`, `Cargo.toml`, `docs/storage.md`). The registry natively supports many-to-many mappings and materializes both directions on every scan:

- `bidirectional_index.feature_to_files`: `BTreeMap<FeatureId, Vec<Path>>`
- `bidirectional_index.file_to_features`: `BTreeMap<Path, Vec<FeatureId>>`
- `counts.multi_feature_files`: exact count of in-scope files linked to `>= 2` genuine feature nodes.

### 3.2 Strict Separation of Drift Classes

| Class | Meaning | Count Field | Never Conflated With |
| --- | --- | --- | --- |
| **Unassociated ("wild" / 野文件)** | File exists on disk, is in scope, and has `0` feature associations | `counts.unassociated_wild` | Stale refs or exclusions |
| **Stale / missing reference** | Feature node links to a path absent from current inventory | `counts.stale_references` | Existing wild files |
| **Explicit exclusion** | Observed path matches a disclosed `ExclusionRule` | `counts.explicit_exclusions` | `in_scope`, `covered`, or wild files |

Exact accounting identities that hold on every scan:
- `counts.covered + counts.unassociated_wild == counts.in_scope`
- `counts.in_scope + counts.explicit_exclusions == counts.total_seen`

### 3.3 Registry Graph Consistency Checks

`validate_registry(&FeatureRegistry)` checks:
- `duplicate_feature_ids`: duplicate node IDs.
- `dangling_relations`: `parent` or `relations[].target` naming a non-existent feature ID.
- `self_links`: a feature whose `parent` or relation targets itself.
- `cycle_errors`: directed cycles in `parent`, `depends_on`, or `contains`.
- `orphan_features`: non-root feature kinds (`capability`, `slice`, `subsystem`) with neither a `parent` nor an incoming `contains` relation. Top-level `domain`, `initiative`, and `research-initiative` nodes are valid roots.
- `invalid_paths`: malformed, absolute, traversal (`..`), or excluded file paths listed inside `feature.files`.

### 3.4 Non-Destructive Safety Guarantee

No code path in `spawn-core`, `spawn-cli`, `serve.rs`, or the Web UI ever deletes or modifies project source files on disk. `associate_file` only validates and atomically updates `docs/feature-map/registry.json` (temp file + `sync_all` + `rename`).

## 4. CLI, HTTP API & UI Contract

### 4.1 CLI (`spawn features`)

```bash
spawn features scan [--root <dir>] [--registry <file>] [--json]
spawn features associate --path <repo-rel-path> --feature <id> [--feature <id>...] [--root <dir>] [--registry <file>] [--json]
spawn features check [--root <dir>] [--registry <file>] [--json]
```

- `scan`: walks `root`, evaluates `registry`, prints human summary or JSON `ScanReport`, exits `0`.
- `associate`: validates `path` is normalized, in-scope, and exists on disk, verifies all `--feature` IDs exist, persists `registry.json` atomically, and prints the update.
- `check`: exits `0` **only** when `unassociated_wild == 0`, `stale_references == 0`, and `consistency.is_clean()`; exits `1` otherwise.

### 4.2 HTTP API (`spawn serve`)

Configured via optional `SPAWN_FEATURE_ROOT` (default: working directory) and `SPAWN_FEATURE_REGISTRY` (default: `<root>/docs/feature-map/registry.json`). All routes sit behind `serve.rs`'s existing read/write guards (`/api/features/` continues returning `404`).

- `GET /api/features/coverage`: returns `{ root, registry_path, report: ScanReport, features: CoverageFeatureSummary[] }`.
- `POST /api/features/scan`: triggers a fresh one-click scan (read-only on disk; enforces same-origin when tokenless) and returns the same JSON payload with an updated `scanned_at_ms`.
- `POST /api/features/associate`: accepts `{"path": "<rel-path>", "features": ["<id>", ...]}` under `serve.rs`'s full `write_body` guard, validates & persists atomically, and returns `{ root, registry_path, report, features, update }`.

### 4.3 Web UI (`#/coverage`)

- Accessible from the sidebar ("File coverage") and mobile nav ("Coverage").
- Provides a one-click **"Scan unassociated files (野文件)"** button, disclosed boundary & exclusion policy viewer, six strictly separated metric tiles, per-wild-file multi-select feature linker (many-to-many), stale-reference table with rename candidates, and initiative/consistency overview displaying `t559u` slices and `t567u/t568u` (badged *"Staged research/design — not production"*).
