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
| `vcs` | `.git/**` | Git internal object store, worktree `.git` pointers, and repository metadata |
| `build-cache` | `target/**`, `web/dist/**`, `**/.cache/**`, `**/__pycache__/**` | Derived, rebuildable build and tool caches |
| `dependency-cache` | `**/node_modules/**`, `**/.pnpm-store/**` | Third-party installed packages and local pnpm store, not authored project files |
| `runtime-state` | `.spawn/**`, `.claude/**`, `rsi/.gate-owner/**` | Ephemeral agent/engine execution markers, local worktrees, and gitignored gate-owner leases |
| `secrets` | `**/.env`, `**/.env.*`, `**/*.env`, `**/*.key`, `**/*.pem`, `**/*.gpg`, `**/secrets/**` | Credentials, private keys, and encrypted secrets are never crawled, hashed, or indexed |
| `editor-temp` | `**/.DS_Store`, `**/*.swp`, `**/*.swo`, `**/*~`, `**/*.rs.bk`, `Cargo.lock.bak` | OS metadata and transient editor/formatter backup files |
| `transient` | `**/*.log`, `**/*.tmp`, `**/*.part` | Transient diagnostic logs, scratch files, and partial downloads |

**Hard invariants on exclusions**:
- Excluded paths are reported in `excluded_files` with their `rule_id`, `category`, and `reason`.
- Excluded paths are **NEVER** counted in `counts.in_scope` or `counts.covered`.
- Excluded paths are **NEVER** listed in `unassociated_files`.
- Any directory matching a whole-subtree (`/**`) exclusion rule — including `secrets/`, `rsi/.gate-owner/`, `.git/`, `target/`, `node_modules/`, `.pnpm-store/`, `web/dist/`, `.spawn/`, `.claude/`, `.cache/`, and `__pycache__/` — is collapsed immediately via `match_directory_exclusion` without calling `read_dir` inside it.
- Attempting to associate an excluded or secret path via CLI or API is rejected with an error; listing an excluded path in `registry.json` fails `validate_registry` (`consistency.invalid_paths`) and is never counted as covered or stale.
- The filesystem walker refuses symlinks escaping the project root, never traverses symlinked directories, and never crawls `$HOME` or reads secret file contents.

### 2.3 Semantic Association Rules (No Catch-All Buckets, No Fabricated Links)

A green `covered == in_scope` counter is **not** sufficient on its own. Associations in `docs/feature-map/registry.json` must satisfy these semantic rules:
1. **No directory-glob catch-all buckets**: Every capability/subsystem node must represent a genuine functional capability. In particular, RSI objective backlogs (`rsi/backlog/*.json`) and verification scenarios (`rsi/scenarios/**`) are mapped to the specific Spawn capabilities they specify or verify rather than dumped wholesale into generic directory buckets; feature-specific documentation and systemd units are linked to their owning capabilities rather than a generic docs/deploy catch-all.
2. **Organizational `domain` roots own zero direct files**: Top-level `domain` nodes organize child capabilities/subsystems via `contains` / `parent` links; concrete files live on genuine capability/subsystem/initiative/slice nodes.
3. **No fabricated placeholder links on unmerged parallel slices**: If a parallel initiative slice (such as `t559u-slice-a-mindmap` on branch `t559u-fm-mindmap` or `t559u-slice-c-storage-keys` on `t559u-fm-storage-keys`) has not yet merged its implementation files into the current branch, its `files` list contains only files genuinely present and authored for that seam on this branch (or `[]` with an explicit `status_note`), never unrelated placeholder files.

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
- `orphan_features`: non-root feature kinds (`capability`, `slice`, `subsystem`) that are not reachable from any valid root (`domain`, `initiative`, `research-initiative`) via valid `parent` or `contains` hierarchy edges (catching direct orphans, children of orphans, dangling/self-parent orphans, and isolated cycles).
- `invalid_paths`: malformed, absolute, traversal (`..`), duplicate-within-feature, or excluded file paths listed inside `feature.files`.

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
