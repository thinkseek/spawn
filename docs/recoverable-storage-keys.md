# Recoverable Content-Addressed Storage & Deterministic Keys (`fm-cas/v1`)

This specification defines the `fm-cas/v1` recoverable content-addressed storage
(CAS) engine implemented in `crates/spawn-cli/src/backup/recoverable.rs` and
exposed through `spawn backup cas`.

`fm-cas/v1` is strictly additive: it coexists beside existing `spawn backup`
(`postgres/`, `bundles/`, `.backup-sync-manifest.json`) and `spawn store` v2
(`transcripts/`, `sessions/`) without altering any legacy format, bucket, or
object layout.

## 1. Goals & Granularity

The engine addresses two requirements simultaneously:

1. **Exact, self-verifying disaster recovery** of repository/workspace trees
   including permissions, nanosecond modification times, symlinks, empty
   directories, empty files, arbitrary binary files, and large multi-chunk
   files.
2. **First-class Feature-Map preservation** binding features, typed feature
   relations, bidirectional file-to-feature associations, boundaries/exclusions,
   and provenance references directly into immutable, content-addressed
   snapshots.

### Smallest Independently Recoverable Units

| Unit | Addressable Key Prefix | Storage Path (under `<store>/fm-cas/v1/`) | Purpose |
| --- | --- | --- | --- |
| **Content Blob** | `fm-cas/v1/blob/sha256/<hex>` | `blobs/<aa>/<hex>` | Raw bytes of a regular file `<= chunk_size` (default 4 MiB) |
| **Content Chunk** | `fm-cas/v1/chunk/sha256/<hex>` | `chunks/<aa>/<hex>` | Fixed-size chunk (`<= chunk_size`) of a regular file `> chunk_size` |
| **Symlink Target Blob** | `fm-cas/v1/symlink/sha256/<hex>` | `blobs/sym/<aa>/<hex>` | Exact UTF-8 target string of a symbolic link (never followed) |
| **Feature-Map Graph Snapshot** | `fm-cas/v1/fm-graph/sha256/<hex>` | `graphs/<aa>/<hex>.json` | Canonical JSON of features, relations, boundaries, and provenance |
| **Snapshot Manifest Root** | `fm-cas/v1/snapshot/sha256/<hex>` | `snapshots/<aa>/<hex>.json` | Immutable manifest binding entries, metadata, parent, and FM graph |

Files larger than `chunk_size` (default `4 * 1024 * 1024` bytes = 4 MiB) are
stored as ordered chunks under `chunks/` rather than duplicated as a whole
blob, while still recording their whole-file `blob_key` in the manifest entry
so whole-file identity, deduplication, rename tracking, and end-to-end
reassembly verification remain uniform across all file sizes.

## 2. Deterministic Key Derivation & Domain Separation

Every key is deterministically derived with **domain-separated SHA-256** (64
lowercase hex characters). Random UUIDs and raw filesystem paths are never used
as storage keys.

Length-prefixed binary objects prepend an 8-byte big-endian unsigned byte
length (`u64_be(len)`) immediately after the NUL-terminated domain tag to
prevent extension ambiguities.

| Key Kind | Formula | String Representation |
| --- | --- | --- |
| `blob_key` | `SHA-256("spawn.cas.v1.blob\0" \|\| u64_be(len) \|\| raw_bytes)` | `fm-cas/v1/blob/sha256/<hex>` |
| `chunk_key` | `SHA-256("spawn.cas.v1.chunk\0" \|\| u64_be(len) \|\| chunk_bytes)` | `fm-cas/v1/chunk/sha256/<hex>` |
| `symlink_blob_key` | `SHA-256("spawn.cas.v1.symlink\0" \|\| u64_be(len) \|\| target_utf8_bytes)` | `fm-cas/v1/symlink/sha256/<hex>` |
| `logical_file_id` | `SHA-256("spawn.cas.v1.logical\0" \|\| canonical_posix_path_utf8)` | `fm-cas/v1/logical/sha256/<hex>` |
| `fm_graph_key` | `SHA-256("spawn.cas.v1.fm-graph\0" \|\| canonical_json_bytes)` | `fm-cas/v1/fm-graph/sha256/<hex>` |
| `snapshot_key` | `SHA-256("spawn.cas.v1.snapshot\0" \|\| canonical_manifest_body_bytes)` | `fm-cas/v1/snapshot/sha256/<hex>` |

### Optional Blinded Remote Transport Key

To replicate `fm-cas/v1` objects onto shared or zero-plaintext-oracle remote
object stores (such as `spawn store`'s R2 keep/work buckets) without leaking
inner content hashes or object kinds:

```text
transport_key(master_key, canonical_key) =
    hex(HMAC-SHA256(master_key, "spawn.cas.v1.transport\0" || canonical_key))
```

- Deterministic per `(master_key, canonical_key)` across all hosts sharing the
  replication key.
- Blinds the inner SHA-256 digest and object type against dictionary/confirmation
  attacks on R2 object listings.
- Preserves exact deduplication and idempotent `PUT If-None-Match: *` upload
  semantics.

## 3. Versioning & Canonicalization

### Explicit Versioning

- Every key carries `fm-cas/v1/` in its prefix.
- Every `FmGraphSnapshot`, `SnapshotManifest`, and `SnapshotIndex` embeds
  `"schema_version": "fm-cas/v1"`.
- Future format changes MUST introduce a new version prefix (e.g. `fm-cas/v2`)
  and new domain tags (`spawn.cas.v2.*`), keeping `fm-cas/v1` objects readable
  and verifiable indefinitely without rewriting historical snapshots.

### Canonical POSIX Paths

All paths stored in `SnapshotEntry.path`, `FmFileAssoc.path`, and
`logical_file_id` inputs are repo-relative canonical POSIX paths:

- `/` separator only; valid UTF-8; no NUL bytes.
- No leading `/` or trailing `/`.
- No empty components (`a//b`), `.` components, or `..` traversal components.
- `.git` path components are skipped during source tree capture and strictly
  refused during path canonicalization and restore.

### Canonical JSON Encoding

All hashed JSON objects (`FmGraphSnapshot` and `SnapshotManifestBody`) are
serialized via `canonical_json_bytes`:

1. All unordered collections are deterministically sorted and deduplicated:
   - `entries` sorted by `path`
   - `entry.feature_ids` sorted lexicographically
   - `skipped_specials` sorted by `(path, kind)`
   - `fm_graph.features` sorted by `id`
   - `feature.files` and `feature.provenance` sorted lexicographically
   - `fm_graph.relations` sorted by `(from, to, kind)`
   - `boundaries.include_roots` and `boundaries.exclude_globs` sorted
   - Ordered sequences (`chunks`) remain strictly ordered by `index` (`0, 1, ...`).
2. JSON object keys are sorted recursively in UTF-8 lexicographical order.
3. Compact UTF-8 output (no insignificant whitespace, no trailing newline).
4. Wall-clock timestamps (`recorded_at_ms`) live strictly in the mutable
   `snapshots.json` index, never inside `SnapshotManifestBody`, guaranteeing
   that identical inputs across independent hosts/runs yield byte-for-byte
   identical `blob_key`, `fm_graph_key`, and `snapshot_key`.

## 4. Identity Model: Blob vs. Logical File vs. Snapshot Membership

`fm-cas/v1` separates three orthogonal identities:

1. **Blob Identity (`blob_key` / `symlink_blob_key`)**: identifies immutable
   content bytes. Two files with identical bytes share one stored blob on disk.
2. **Logical File Identity (`logical_file_id`)**: identifies a canonical
   logical path inside a repository tree (`src/lib.rs` vs `src/copy.rs`). Two
   duplicate files have the same `blob_key` and distinct `logical_file_id`s.
3. **Snapshot Membership (`snapshot_key` + `entries[]`)**: binds a set of
   `(logical_file_id, blob_key, metadata, feature_ids)` tuples at a point in
   lineage (`parent_snapshot_key`).

### Rename Detection Across Snapshots

When `--parent <snapshot-key>` is supplied:

- Entries whose `logical_file_id` existed in the parent snapshot and is absent
  from the new snapshot are indexed by content key (`blob_key` or
  `symlink_blob_key`).
- Newly appearing `logical_file_id`s matching a disappeared content key record
  `renamed_from: Some("<old-canonical-path>")` deterministically (matching
  lexicographically smallest disappeared path first).
- No new blob bytes are written for a pure rename (`blobs_written == 0`).

## 5. Schemas

### Snapshot Manifest (`fm-cas/v1/snapshots/<aa>/<digest>.json`)

```json
{
  "snapshot_key": "fm-cas/v1/snapshot/sha256/<64-hex>",
  "schema_version": "fm-cas/v1",
  "chunk_size": 4194304,
  "parent_snapshot_key": "fm-cas/v1/snapshot/sha256/<64-hex>",
  "fm_graph_key": "fm-cas/v1/fm-graph/sha256/<64-hex>",
  "entries": [
    {
      "path": "bin/tool.sh",
      "logical_file_id": "fm-cas/v1/logical/sha256/<64-hex>",
      "kind": "regular",
      "mode": 493,
      "mtime_ns": 1750000000123456789,
      "size": 17,
      "blob_key": "fm-cas/v1/blob/sha256/<64-hex>",
      "renamed_from": null,
      "feature_ids": ["core-storage"]
    }
  ],
  "skipped_specials": [
    { "path": "run/ctrl.sock", "kind": "socket" }
  ],
  "unsupported_semantics": {
    "posix_acls": "not_preserved",
    "extended_attributes": "not_preserved",
    "hardlink_inodes": "restored_as_independent_files_sharing_blob_key",
    "special_files": "skipped_and_recorded_in_skipped_specials",
    "uid_gid_ownership": "not_preserved_non_root_restore",
    "ctime_and_birthtime": "not_preserved"
  },
  "compatibility": {
    "namespace": "fm-cas/v1",
    "legacy_areas_untouched": [
      ".backup-sync-manifest.json",
      "bundles/",
      "postgres/",
      "sessions/",
      "transcripts/"
    ]
  }
}
```

### Feature-Map Graph Snapshot (`fm-cas/v1/graphs/<aa>/<digest>.json`)

```json
{
  "schema_version": "fm-cas/v1",
  "features": [
    {
      "id": "core-storage",
      "title": "Recoverable Storage Core",
      "summary": "Deterministic content-addressed storage",
      "parent_id": null,
      "files": [
        { "path": "src/lib.rs", "status": "present" },
        { "path": "docs/external.md", "status": "external" }
      ],
      "provenance": [
        {
          "kind": "objective",
          "reference": "objective:4/559",
          "title": "Slice C storage keys",
          "url": null
        }
      ]
    }
  ],
  "relations": [
    { "from": "cli-layer", "to": "core-storage", "kind": "depends_on" }
  ],
  "boundaries": {
    "include_roots": ["src", "bin"],
    "exclude_globs": ["target/**"]
  }
}
```

Supported relation kinds: `parent_of`, `depends_on`, `implements`, `verifies`,
`related_to`. Supported file association statuses: `present`, `external`,
`missing`.

## 6. Supported vs. Unsupported Restore Semantics

### Preserved & Restored

- Repo-relative canonical POSIX paths (`path`)
- Entry kinds: `regular`, `directory` (including nested empty directories), `symlink`
- POSIX permission mode bits (`mode & 0o777`, preserving `0o755`/`0o644`/`0o444`/`0o555`
  while stripping setuid/setgid/sticky bits `0o7000` for restore safety; applied
  to regular files after setting `mtime_ns` and to directories in reverse depth
  order after setting directory `mtime_ns` so restrictive modes like `0o444` and
  `0o555` do not block child creation or timestamp restoration)
- Nanosecond modification timestamps (`mtime_ns`, including pre-1970 negative
  timestamps, restored on regular files via `File::set_modified` and on
  directories and symlinks via `utimensat(..., AT_SYMLINK_NOFOLLOW)` so
  `snapshot -> restore -> re-snapshot` reproduces the identical `snapshot_key`
  on nanosecond-resolution POSIX filesystems)
- Exact raw bytes for empty files, binary files (`0x00..=0xFF`), and multi-chunk
  files (captured via `O_NOFOLLOW` + single-fd `fstat` with size-drift checks)
- Exact UTF-8 symlink target strings (never followed during snapshot; verified
  against `symlink_blob_key` and validated against single-hop and multi-hop
  symlink-chain root escapes and symlink loops before restore)
- Complete `FmGraphSnapshot` object and bidirectional `entry.feature_ids`

### Explicitly Unsupported (Encoded in `unsupported_semantics`)

- **POSIX ACLs**: not preserved.
- **Extended attributes (`xattr`)**: not preserved.
- **Hardlink inode sharing**: restored as independent regular files sharing one
  `blob_key` in the CAS store.
- **Special files** (FIFOs, Unix sockets, block/character devices): never
  opened or copied; explicitly skipped and logged in `skipped_specials`.
- **UID/GID ownership**: not preserved (designed for unprivileged non-root
  restore).
- **ctime / birthtime**: not preserved (kernel-managed metadata).

## 7. Integrity, Failure Modes & Concurrency Guarantees

### Corruption & Missing Object Detection

- Every `verify` and `restore` operation opens the store **strictly read-only**
  (`CasStore::open_read_only`, never creating directories or mutating `--store`,
  safe on read-only disaster-recovery mounts) and performs **full pre-flight
  verification** before writing any restored output:
  1. Recomputes and verifies `snapshot_key` over canonical `SnapshotManifestBody`,
     enforces strict schema parsing (`deny_unknown_fields`, `chunk_size > 0`),
     and verifies exact canonical byte equality of the manifest on disk (catching
     trailing whitespace, unknown fields, unsorted entries, and duplicate tags).
  2. Validates `parent_snapshot_key` syntax (when present) and rejects
     self-parenting; intentionally does not require the parent snapshot object
     to exist locally so single-snapshot delta restores remain independent.
  3. Recomputes and verifies `fm_graph_key` and exact canonical bytes of
     `FmGraphSnapshot` (when present).
  4. Verifies FM graph structural & bidirectional consistency:
     - Unique non-empty feature IDs; no self-loops; no dangling `parent_id` or
       relation endpoints; no conflicting `FileAssocStatus` on the same path.
     - Acyclic parent hierarchy (`parent_id` + `parent_of`) and acyclic
       `depends_on` graph.
     - Every `entry.feature_ids` element exists in the FM graph and references
       that entry's path.
     - Every feature file with `status == present` exists in `entries[]` and
       lists the feature ID back (`external` and `missing` entries may be absent
       from `entries[]`).
  5. Verifies per-entry invariants (mode mask `<= 0o777`, no path hierarchy
     collisions such as a file at `a` alongside `a/b`, valid `renamed_from`,
     strict per-kind payload fields, and consistent `entry.size` across all
     entries deduplicating the same `blob_key`).
  6. Verifies every regular blob, chunk (`0 < chunk.length <= chunk_size`,
     `chunk_key`, and whole-file reassembled `blob_key`), and symlink blob
     (`symlink_blob_key`, exact `entry.size`, and UTF-8 target equality).
- Any 1-bit flip, truncation, tampering, or missing object returns a structured
  `CasError` naming the exact key and all affected logical paths.

### Atomic Commits, Concurrent Writers & Crash Recovery

- Every immutable object (`blob`, `symlink`, `chunk`, `fm-graph`, `snapshot`)
  is written to a unique staging path `fm-cas/v1/staging/.tmp-<pid>-<seq>`,
  flushed with `sync_all()`, and committed with write-once hard-link semantics
  followed by a parent-directory `fsync` (falling back to atomic `rename` only
  when hard links are unsupported on the filesystem, after verifying no corrupt
  destination object exists).
- When a target object already exists (deduplication or concurrent writer
  race), the writer **re-reads and verifies the on-disk object's hash** (and
  exact canonical manifest bytes) rather than blindly trusting it, surfacing
  pre-existing disk corruption immediately.
- `fm-cas/v1/snapshots.json` is updated under an exclusive advisory lock
  (`fm-cas/v1/.cas.lock`) via `staging/.tmp-* -> sync_all -> rename -> dir fsync`,
  and `read_index` validates `schema_version == "fm-cas/v1"`.
- Stray `.tmp-*` files left by killed/interrupted runs live exclusively under
  `fm-cas/v1/staging/`, are ignored by all readers and verifiers, and
  `sweep_staging` checks `file_type().is_file()` (never following symlinks) and
  skips `.tmp-<pid>-<seq>` files owned by a currently live PID on this host.

### Restore Target Safety & Rollback

- Validates `--restore-fm` (presence of `fm_graph`, non-existence via `lstat`
  including dangling symlinks, and non-overlap with `--into`) in pre-flight
  before creating `--into`.
- Refuses to restore into a non-empty directory, a non-directory path, or any
  symlink (including dangling symlinks or symlinks to empty directories).
- Refuses any path traversal (`..`, absolute paths, `.git` components) and
  symbolically resolves all manifest symlinks (up to 40 hops) to refuse
  absolute targets, single-hop or multi-hop symlink-chain root escapes, `.git`
  targets, and symlink loops.
- Restores directories first, regular files second (checking no parent
  component is a symlink, setting `mtime_ns` before `chmod`), symlinks third
  (with `utimensat(..., AT_SYMLINK_NOFOLLOW)`), and directory `mtime_ns` +
  modes last in explicit reverse depth order.
- Wraps the write phase in a rollback guard: if any mid-restore I/O or
  integrity error occurs, partial restored files/directories (including any
  already-restricted `0o555` directories) are cleaned up cleanly.

## 8. Known & Remaining Limitations (Honest Scope of `fm-cas/v1`)

1. **Fixed-size chunking (not content-defined chunking / FastCDC)**: files
   `> 4 MiB` are split at fixed `chunk_size` offsets. Inserting or deleting
   bytes near the beginning of a large file shifts subsequent chunk boundaries
   and causes later chunks to hash differently.
2. **Metadata-sensitive `snapshot_key` & cross-OS / cross-FS drift**:
   `snapshot_key` hashes `mtime_ns` and `mode` for files, directories, and
   symlinks. Restoring onto a coarser-timestamp filesystem (e.g. HFS+ 1 s,
   FAT32 2 s) or moving across OSes with different symlink mode semantics
   (Linux always reports `0o777` for symlinks; macOS reflects `umask`) changes
   `snapshot_key` upon re-snapshot, even though all `blob_key`, `chunk_key`,
   `symlink_blob_key`, and `fm_graph_key` values remain 100% identical.
3. **Case-insensitive or Unicode-normalizing filesystems**: snapshots captured
   on case-sensitive Linux containing paths differing only by case (e.g.
   `Foo.rs` and `foo.rs`) or NFC/NFD form cannot both materialize on a
   case-insensitive restore target; `create_new` refuses silent overwrite and
   rolls back cleanly.
4. **Single-host POSIX locking & PID-namespace scope**: `.cas.lock` advisory
   locks and `sweep_staging` PID-liveness checks assume a single local POSIX
   host/PID namespace. Do not point multiple hosts or containers in distinct
   PID namespaces at one mutable local store over NFS/SMB; use remote
   content-addressed replication (`t567u`/`t568u`) instead.
5. **Content-only `--parent` rename heuristic**: rename detection matches
   disappeared content keys to newly appeared paths in lexicographical order.
   Deleting one empty (or duplicate-content) file while adding another
   unrelated file with identical bytes is recorded as a rename, and a file that
   is simultaneously renamed and edited is recorded as a delete + add.
6. **Parent snapshot not required locally for single-snapshot `verify`/`restore`**:
   `verify` checks `parent_snapshot_key` format and rejects self-parenting, but
   intentionally allows verifying/restoring a snapshot without fetching its
   parent lineage.
7. **No local garbage collection or local encryption-at-rest**: local
   `fm-cas/v1` stores are append-only plaintext-addressed caches; pruning
   unreferenced blobs and remote sealing (`transport_key` + AEAD/GPG) are
   separate lifecycle/transport layers.
8. **Absolute / root-escaping symlinks**: `snapshot` faithfully records any
   UTF-8 symlink target and `verify` validates its content hash, while
   `restore` strictly refuses absolute, escaping, `.git`-targeting, or looping
   symlinks with no unsafe override flag in v1.
9. **Non-UTF-8 paths & symlink targets**: rejected at snapshot time rather than
   lossily transcoded, ensuring unambiguous canonical JSON and POSIX paths.

## 9. CLI Reference

```bash
# Capture a snapshot (with optional FM graph and parent snapshot for rename tracking)
spawn backup cas snapshot \
  --source <dir> \
  --store <dir> \
  [--fm-graph <file>] \
  [--parent <snapshot-key>] \
  [--json]

# Verify snapshot manifest, FM graph consistency, and every blob/chunk/symlink
spawn backup cas verify \
  --store <dir> \
  --snapshot <snapshot-key> \
  [--json]

# Restore a verified snapshot into a fresh empty directory (and optional FM graph file)
spawn backup cas restore \
  --store <dir> \
  --snapshot <snapshot-key> \
  --into <dir> \
  [--restore-fm <file>] \
  [--json]

# Deterministically inspect keys for a single file or symlink
spawn backup cas key-of \
  --file <path> \
  [--logical-path <rel-path>] \
  [--json]
```

## 10. Legacy Coexistence & Multi-Machine Replication (`t567u` / `t568u`)

### Non-Destructive Coexistence

All `fm-cas/v1` state lives strictly inside `<store_root>/fm-cas/v1/`. Existing
`spawn backup` (`dump`, `bundle`, `verify`, `restore-drill`) and `spawn store`
(`put`, `get`, `list`, `verify`) commands, buckets, and paths (`postgres/`,
`bundles/`, `transcripts/`, `sessions/`, `.backup-sync-manifest.json`) remain
completely untouched.

### Bridging to Multi-Machine Replication (`t567u` / `t568u`)

`fm-cas/v1` provides the exact content-addressed foundation needed by
multi-machine replication (`t567u` / `t568u`):

1. **Zero-coordination deduplication across hosts**: because `blob_key`,
   `chunk_key`, `fm_graph_key`, and `snapshot_key` are purely content-derived
   with canonical JSON and no host/clock entropy inside manifests, two hosts
   snapshotting identical artifacts converge on identical keys.
2. **Delta replication by missing-key set difference**: syncing a snapshot
   from host A to host B (or via R2) requires only transferring the manifest,
   optional FM graph, and the subset of `blob_key` / `chunk_key` /
   `symlink_blob_key` objects not already present on the destination.
3. **Zero-plaintext-oracle remote transport**: mapping each canonical inner key
   through `transport_key(master_key, canonical_key)` before sealing (GPG/AEAD)
   and uploading to R2 (`PUT If-None-Match: *`) matches `spawn store` v2's
   threat model while keeping inner verification and cross-host deduplication
   100% deterministic.
