//! End-to-end isolated round-trip integration tests for `spawn backup cas` (`fm-cas/v1`).
//!
//! Exercises the compiled `spawn` binary (`CARGO_BIN_EXE_spawn`) against
//! isolated temporary directories without touching any live store, database,
//! or R2 bucket.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("spawn-cas-it-{name}-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_spawn(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(args)
        .output()
        .expect("execute spawn binary")
}

fn run_spawn_json(args: &[&str]) -> Value {
    let output = run_spawn(args);
    assert!(
        output.status.success(),
        "spawn {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid json from spawn {:?}: {error}\nstdout: {}",
            args,
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn write_file(path: &Path, bytes: &[u8], mode: u32, mtime_secs: u64, mtime_nanos: u32) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    let mut file = File::create(path).expect("create file");
    file.write_all(bytes).expect("write bytes");
    file.sync_all().expect("sync file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mtime = UNIX_EPOCH + Duration::new(mtime_secs, mtime_nanos);
    file.set_modified(mtime).expect("set_modified");
}

fn read_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

fn read_mtime_ns(path: &Path) -> u128 {
    fs::symlink_metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn blob_disk_path(store: &Path, blob_key: &str) -> PathBuf {
    let digest = blob_key
        .strip_prefix("fm-cas/v1/blob/sha256/")
        .expect("blob key prefix");
    store
        .join("fm-cas/v1/blobs")
        .join(&digest[..2])
        .join(digest)
}

fn chunk_disk_path(store: &Path, chunk_key: &str) -> PathBuf {
    let digest = chunk_key
        .strip_prefix("fm-cas/v1/chunk/sha256/")
        .expect("chunk key prefix");
    store
        .join("fm-cas/v1/chunks")
        .join(&digest[..2])
        .join(digest)
}

fn graph_disk_path(store: &Path, graph_key: &str) -> PathBuf {
    let digest = graph_key
        .strip_prefix("fm-cas/v1/fm-graph/sha256/")
        .expect("graph key prefix");
    store
        .join("fm-cas/v1/graphs")
        .join(&digest[..2])
        .join(format!("{digest}.json"))
}

fn snapshot_disk_path(store: &Path, snap_key: &str) -> PathBuf {
    let digest = snap_key
        .strip_prefix("fm-cas/v1/snapshot/sha256/")
        .expect("snapshot key prefix");
    store
        .join("fm-cas/v1/snapshots")
        .join(&digest[..2])
        .join(format!("{digest}.json"))
}

fn write_sample_fm(path: &Path) {
    let fm = json!({
        "schema_version": "fm-cas/v1",
        "features": [
            {
                "id": "core-storage",
                "title": "Recoverable Storage Core",
                "summary": "Deterministic content-addressed storage",
                "files": [
                    { "path": "src/lib.rs", "status": "present" },
                    { "path": "bin/tool.sh", "status": "present" },
                    { "path": "docs/external-spec.md", "status": "external" }
                ],
                "provenance": [
                    {
                        "kind": "objective",
                        "reference": "objective:4/559",
                        "title": "Slice C storage keys"
                    }
                ]
            },
            {
                "id": "binary-payloads",
                "title": "Binary and Large File Handling",
                "parent_id": "core-storage",
                "files": [
                    { "path": "data/all_bytes.bin", "status": "present" },
                    { "path": "data/large_multi_chunk.bin", "status": "present" },
                    { "path": "data/empty.dat", "status": "present" }
                ],
                "provenance": []
            }
        ],
        "relations": [
            { "from": "binary-payloads", "to": "core-storage", "kind": "depends_on" }
        ],
        "boundaries": {
            "include_roots": ["src", "bin", "data"],
            "exclude_globs": ["target/**"]
        }
    });
    fs::write(path, serde_json::to_vec_pretty(&fm).unwrap()).expect("write fm json");
}

#[test]
#[allow(clippy::too_many_lines)]
fn deterministic_keys_and_full_round_trip_restore_with_large_multichunk() {
    let scratch = ScratchDir::new("roundtrip");
    let src = scratch.path().join("src_tree");
    let store_a = scratch.path().join("store_a");
    let store_b = scratch.path().join("store_b");
    let fm_file = scratch.path().join("fm.json");
    let restored = scratch.path().join("restored_tree");
    let restored_fm = scratch.path().join("restored_fm.json");

    let secs = 1_750_000_000u64;
    let nanos = 123_456_789u32;
    let expected_mtime_ns = u128::from(secs) * 1_000_000_000 + u128::from(nanos);

    // Regular text file (0o644)
    write_file(
        &src.join("src/lib.rs"),
        b"pub fn answer() -> u32 { 42 }\n",
        0o644,
        secs,
        nanos,
    );
    // Executable script (0o755)
    write_file(
        &src.join("bin/tool.sh"),
        b"#!/bin/sh\nexit 0\n",
        0o755,
        secs,
        nanos,
    );
    // Empty file
    write_file(&src.join("data/empty.dat"), b"", 0o644, secs, nanos);
    // Arbitrary binary file containing every byte 0x00..=0xFF
    let all_bytes: Vec<u8> = (0u8..=255).collect();
    write_file(
        &src.join("data/all_bytes.bin"),
        &all_bytes,
        0o644,
        secs,
        nanos,
    );
    // Large multi-chunk file: 4 MiB + 128 KiB -> 2 deterministic chunks
    let large_len = 4 * 1024 * 1024 + 128 * 1024;
    let large_payload: Vec<u8> = (0..large_len)
        .map(|i| u8::try_from((i * 31) % 256).unwrap())
        .collect();
    write_file(
        &src.join("data/large_multi_chunk.bin"),
        &large_payload,
        0o644,
        secs,
        nanos,
    );
    // Nested empty directory
    fs::create_dir_all(src.join("empty_dirs/nested/leaf")).expect("create empty dirs");
    // Relative symlink (never followed during snapshot)
    #[cfg(unix)]
    std::os::unix::fs::symlink("../src/lib.rs", src.join("bin/lib_link")).expect("symlink");

    write_sample_fm(&fm_file);

    // 1. `key-of` deterministically computes blob_key, chunks, and logical_file_id
    let key_report = run_spawn_json(&[
        "backup",
        "cas",
        "key-of",
        "--file",
        src.join("data/large_multi_chunk.bin").to_str().unwrap(),
        "--logical-path",
        "data/large_multi_chunk.bin",
        "--json",
    ]);
    assert_eq!(key_report["schema_version"], "fm-cas/v1");
    assert_eq!(key_report["size"], large_len);
    assert_eq!(key_report["chunks"].as_array().unwrap().len(), 2);
    assert!(
        key_report["blob_key"]
            .as_str()
            .unwrap()
            .starts_with("fm-cas/v1/blob/sha256/")
    );
    assert!(
        key_report["logical_file_id"]
            .as_str()
            .unwrap()
            .starts_with("fm-cas/v1/logical/sha256/")
    );

    // 2. Two independent stores snapshot the same tree -> identical keys!
    let snap_a = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store_a.to_str().unwrap(),
        "--fm-graph",
        fm_file.to_str().unwrap(),
        "--json",
    ]);
    let snap_b = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store_b.to_str().unwrap(),
        "--fm-graph",
        fm_file.to_str().unwrap(),
        "--json",
    ]);
    let snap_key = snap_a["snapshot_key"].as_str().unwrap().to_string();
    let fm_key = snap_a["fm_graph_key"].as_str().unwrap().to_string();
    assert_eq!(snap_a["snapshot_key"], snap_b["snapshot_key"]);
    assert_eq!(snap_a["fm_graph_key"], snap_b["fm_graph_key"]);
    assert_eq!(snap_a["chunks_written"], 2);

    // 3. Verify snapshot
    let verify_out = run_spawn_json(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store_a.to_str().unwrap(),
        "--snapshot",
        &snap_key,
        "--json",
    ]);
    assert_eq!(verify_out["verified"], true);
    assert_eq!(verify_out["chunks_verified"], 2);
    assert_eq!(verify_out["features_verified"], 2);

    // 4. Restore into fresh isolated directory + restore FM graph
    let restore_out = run_spawn_json(&[
        "backup",
        "cas",
        "restore",
        "--store",
        store_a.to_str().unwrap(),
        "--snapshot",
        &snap_key,
        "--into",
        restored.to_str().unwrap(),
        "--restore-fm",
        restored_fm.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(restore_out["snapshot_key"], snap_key);
    assert_eq!(restore_out["fm_graph_key"], fm_key);

    // Exact byte-for-byte content verification
    assert_eq!(
        fs::read(restored.join("src/lib.rs")).unwrap(),
        b"pub fn answer() -> u32 { 42 }\n"
    );
    assert_eq!(
        fs::read(restored.join("bin/tool.sh")).unwrap(),
        b"#!/bin/sh\nexit 0\n"
    );
    assert_eq!(fs::read(restored.join("data/empty.dat")).unwrap(), b"");
    assert_eq!(
        fs::read(restored.join("data/all_bytes.bin")).unwrap(),
        all_bytes
    );
    assert_eq!(
        fs::read(restored.join("data/large_multi_chunk.bin")).unwrap(),
        large_payload
    );
    assert!(restored.join("empty_dirs/nested/leaf").is_dir());

    // Modes & mtimes preserved
    assert_eq!(read_mode(&restored.join("src/lib.rs")), 0o644);
    assert_eq!(read_mode(&restored.join("bin/tool.sh")), 0o755);
    assert_eq!(
        read_mtime_ns(&restored.join("src/lib.rs")),
        expected_mtime_ns
    );
    assert_eq!(
        read_mtime_ns(&restored.join("bin/tool.sh")),
        expected_mtime_ns
    );

    #[cfg(unix)]
    assert_eq!(
        fs::read_link(restored.join("bin/lib_link")).unwrap(),
        PathBuf::from("../src/lib.rs")
    );

    // Restored FM graph and manifest bidirectional associations match
    let fm_restored: Value = serde_json::from_slice(&fs::read(&restored_fm).unwrap()).unwrap();
    assert_eq!(fm_restored["features"].as_array().unwrap().len(), 2);
    let manifest: Value =
        serde_json::from_slice(&fs::read(snapshot_disk_path(&store_a, &snap_key)).unwrap())
            .unwrap();
    let lib_entry = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "src/lib.rs")
        .unwrap();
    assert_eq!(lib_entry["feature_ids"], json!(["core-storage"]));
    assert_eq!(lib_entry["blob_key"].as_str().unwrap().len(), 86);
    assert_eq!(
        manifest["unsupported_semantics"]["hardlink_inodes"],
        "restored_as_independent_files_sharing_blob_key"
    );

    // 5. Restoring into a non-empty target directory MUST be refused
    let refuse = run_spawn(&[
        "backup",
        "cas",
        "restore",
        "--store",
        store_a.to_str().unwrap(),
        "--snapshot",
        &snap_key,
        "--into",
        restored.to_str().unwrap(),
    ]);
    assert!(!refuse.status.success());
    assert!(
        String::from_utf8_lossy(&refuse.stderr).contains("non-empty"),
        "stderr: {}",
        String::from_utf8_lossy(&refuse.stderr)
    );
}

#[test]
fn duplicates_and_renames_via_parent_snapshot() {
    let scratch = ScratchDir::new("dup-rename");
    let src = scratch.path().join("src");
    let store = scratch.path().join("store");

    write_file(
        &src.join("copy_one.txt"),
        b"shared content bytes",
        0o644,
        1_750_000_010,
        0,
    );
    write_file(
        &src.join("copy_two.txt"),
        b"shared content bytes",
        0o644,
        1_750_000_010,
        0,
    );
    write_file(
        &src.join("before.txt"),
        b"unique file for rename",
        0o644,
        1_750_000_010,
        0,
    );

    let snap1 = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(snap1["blobs_written"], 2);
    assert_eq!(snap1["blobs_deduplicated"], 1);
    let snap1_key = snap1["snapshot_key"].as_str().unwrap().to_string();

    // Rename before.txt -> subdir/after.txt
    fs::create_dir_all(src.join("subdir")).unwrap();
    fs::rename(src.join("before.txt"), src.join("subdir/after.txt")).unwrap();

    let snap2 = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--parent",
        &snap1_key,
        "--json",
    ]);
    assert_eq!(snap2["renamed_count"], 1);
    assert_eq!(snap2["blobs_written"], 0);
    let snap2_key = snap2["snapshot_key"].as_str().unwrap().to_string();

    let m1: Value =
        serde_json::from_slice(&fs::read(snapshot_disk_path(&store, &snap1_key)).unwrap()).unwrap();
    let m2: Value =
        serde_json::from_slice(&fs::read(snapshot_disk_path(&store, &snap2_key)).unwrap()).unwrap();

    let before = m1["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "before.txt")
        .unwrap();
    let after = m2["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "subdir/after.txt")
        .unwrap();
    assert_eq!(before["blob_key"], after["blob_key"]);
    assert_ne!(before["logical_file_id"], after["logical_file_id"]);
    assert_eq!(after["renamed_from"], "before.txt");
}

#[test]
#[allow(clippy::too_many_lines)]
fn detects_bit_flip_corruption_tampering_and_missing_blobs() {
    let scratch = ScratchDir::new("corrupt");
    let src = scratch.path().join("src");
    let store = scratch.path().join("store");
    let fm_file = scratch.path().join("fm.json");

    write_file(
        &src.join("src/lib.rs"),
        b"pub fn check() {}\n",
        0o644,
        1_750_000_020,
        0,
    );
    write_file(
        &src.join("bin/tool.sh"),
        b"#!/bin/sh\n",
        0o755,
        1_750_000_020,
        0,
    );
    write_file(&src.join("data/empty.dat"), b"", 0o644, 1_750_000_020, 0);
    write_file(
        &src.join("data/all_bytes.bin"),
        &[1, 2, 3, 4],
        0o644,
        1_750_000_020,
        0,
    );
    let large_payload = vec![0x42u8; 4 * 1024 * 1024 + 64];
    write_file(
        &src.join("data/large_multi_chunk.bin"),
        &large_payload,
        0o644,
        1_750_000_020,
        0,
    );
    write_sample_fm(&fm_file);

    let snap = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--fm-graph",
        fm_file.to_str().unwrap(),
        "--json",
    ]);
    let snap_key = snap["snapshot_key"].as_str().unwrap().to_string();
    let fm_key = snap["fm_graph_key"].as_str().unwrap().to_string();
    let manifest_path = snapshot_disk_path(&store, &snap_key);
    let manifest_bytes = fs::read(&manifest_path).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();

    let lib_blob_key = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "src/lib.rs")
        .unwrap()["blob_key"]
        .as_str()
        .unwrap()
        .to_string();
    let chunk0_key = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "data/large_multi_chunk.bin")
        .unwrap()["chunks"][0]["chunk_key"]
        .as_str()
        .unwrap()
        .to_string();

    // 1. 1-bit flip in blob -> verify & restore fail naming key + path; target untouched
    let lib_blob_path = blob_disk_path(&store, &lib_blob_key);
    let orig_blob = fs::read(&lib_blob_path).unwrap();
    let mut bad_blob = orig_blob.clone();
    bad_blob[0] ^= 0x01;
    fs::write(&lib_blob_path, &bad_blob).unwrap();

    let failed_verify = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!failed_verify.status.success());
    let stderr = String::from_utf8_lossy(&failed_verify.stderr);
    assert!(stderr.contains(&lib_blob_key), "stderr: {stderr}");
    assert!(stderr.contains("src/lib.rs"), "stderr: {stderr}");

    let abort_target = scratch.path().join("aborted_restore");
    let failed_restore = run_spawn(&[
        "backup",
        "cas",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
        "--into",
        abort_target.to_str().unwrap(),
    ]);
    assert!(!failed_restore.status.success());
    assert!(
        !abort_target.exists(),
        "failed pre-flight restore must not create partial output dir"
    );

    // 2. Missing blob -> verify fails naming exact key & affected path
    fs::remove_file(&lib_blob_path).unwrap();
    let missing_out = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!missing_out.status.success());
    let missing_err = String::from_utf8_lossy(&missing_out.stderr);
    assert!(
        missing_err.contains("missing object"),
        "stderr: {missing_err}"
    );
    assert!(missing_err.contains(&lib_blob_key), "stderr: {missing_err}");
    assert!(missing_err.contains("src/lib.rs"), "stderr: {missing_err}");
    fs::write(&lib_blob_path, &orig_blob).unwrap();

    // 3. 1-bit flip in chunk -> detected with chunk key + logical path
    let c_path = chunk_disk_path(&store, &chunk0_key);
    let orig_chunk = fs::read(&c_path).unwrap();
    let mut bad_chunk = orig_chunk.clone();
    bad_chunk[1024] ^= 0x04;
    fs::write(&c_path, &bad_chunk).unwrap();
    let chunk_fail = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!chunk_fail.status.success());
    let chunk_err = String::from_utf8_lossy(&chunk_fail.stderr);
    assert!(chunk_err.contains(&chunk0_key), "stderr: {chunk_err}");
    assert!(
        chunk_err.contains("data/large_multi_chunk.bin"),
        "stderr: {chunk_err}"
    );
    fs::write(&c_path, &orig_chunk).unwrap();

    // 4. Tampered FM graph JSON -> detected
    let g_path = graph_disk_path(&store, &fm_key);
    let orig_graph = fs::read(&g_path).unwrap();
    let mut tampered_graph = String::from_utf8(orig_graph.clone()).unwrap();
    tampered_graph = tampered_graph.replace("Recoverable Storage Core", "Tampered Storage Core");
    fs::write(&g_path, tampered_graph.as_bytes()).unwrap();
    let fm_fail = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!fm_fail.status.success());
    assert!(String::from_utf8_lossy(&fm_fail.stderr).contains(&fm_key));
    fs::write(&g_path, &orig_graph).unwrap();

    // 5. Tampered manifest JSON -> detected
    let mut tampered_manifest = String::from_utf8(manifest_bytes.clone()).unwrap();
    tampered_manifest = tampered_manifest.replace("src/lib.rs", "src/evil.rs");
    fs::write(&manifest_path, tampered_manifest.as_bytes()).unwrap();
    let m_fail = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!m_fail.status.success());
    assert!(String::from_utf8_lossy(&m_fail.stderr).contains(&snap_key));
    fs::write(&manifest_path, &manifest_bytes).unwrap();

    // Clean store verifies again
    let ok = run_spawn_json(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
        "--json",
    ]);
    assert_eq!(ok["verified"], true);
}

#[test]
#[allow(clippy::too_many_lines)]
fn rejects_inconsistent_fm_graphs_and_preserves_legacy_backup_layout() {
    let scratch = ScratchDir::new("fm-and-legacy");
    let src = scratch.path().join("src");
    let store = scratch.path().join("store");

    write_file(
        &src.join("app.rs"),
        b"fn main() {}\n",
        0o644,
        1_750_000_030,
        0,
    );

    // Plant simulated legacy spawn backup / store directories & manifest
    let legacy_files = [
        (
            "postgres/hourly/spawn-20261122-000000.dump.gpg",
            b"legacy-pg-dump".as_slice(),
        ),
        (
            "bundles/full-20261122.tar.zst.gpg",
            b"legacy-bundle".as_slice(),
        ),
        (
            "transcripts/manifests/host/acct/000000000001.json.gpg",
            b"legacy-tr".as_slice(),
        ),
        (
            "sessions/manifests/host/acct/000000000001.json.gpg",
            b"legacy-sess".as_slice(),
        ),
        (
            ".backup-sync-manifest.json",
            b"{\"legacy\":true}".as_slice(),
        ),
    ];
    for (rel, content) in &legacy_files {
        let p = store.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
    }

    // Plant stray staging temp file from an interrupted backup run
    let staging = store.join("fm-cas/v1/staging");
    fs::create_dir_all(&staging).unwrap();
    let stray_tmp = staging.join(".tmp-12345-0");
    fs::write(&stray_tmp, b"interrupted partial write").unwrap();

    // Inconsistent FM graphs: dangling relation, cycle, missing file ref
    let bad_graphs = [
        json!({
            "schema_version": "fm-cas/v1",
            "features": [{"id": "a", "title": "A", "files": [{"path": "app.rs"}]}],
            "relations": [{"from": "a", "to": "ghost", "kind": "depends_on"}]
        }),
        json!({
            "schema_version": "fm-cas/v1",
            "features": [
                {"id": "a", "title": "A", "parent_id": "b"},
                {"id": "b", "title": "B", "parent_id": "a"}
            ]
        }),
        json!({
            "schema_version": "fm-cas/v1",
            "features": [{"id": "a", "title": "A", "files": [{"path": "missing.rs"}]}]
        }),
    ];
    for (idx, bad) in bad_graphs.iter().enumerate() {
        let fm_path = scratch.path().join(format!("bad_fm_{idx}.json"));
        fs::write(&fm_path, serde_json::to_vec(bad).unwrap()).unwrap();
        let out = run_spawn(&[
            "backup",
            "cas",
            "snapshot",
            "--source",
            src.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--fm-graph",
            fm_path.to_str().unwrap(),
        ]);
        assert!(
            !out.status.success(),
            "bad FM graph {idx} should have been rejected"
        );
    }

    // Concurrent CLI snapshot invocations against the same store (with stray temp file present)
    let mut handles = Vec::new();
    for worker in 0..4u32 {
        let store_clone = store.clone();
        let worker_src = scratch.path().join(format!("worker_src_{worker}"));
        write_file(
            &worker_src.join("common.txt"),
            b"shared across procs",
            0o644,
            1_750_000_040,
            0,
        );
        write_file(
            &worker_src.join(format!("only_{worker}.txt")),
            format!("worker {worker} bytes").as_bytes(),
            0o644,
            1_750_000_040,
            0,
        );
        handles.push(thread::spawn(move || {
            let snap = run_spawn_json(&[
                "backup",
                "cas",
                "snapshot",
                "--source",
                worker_src.to_str().unwrap(),
                "--store",
                store_clone.to_str().unwrap(),
                "--json",
            ]);
            let key = snap["snapshot_key"].as_str().unwrap().to_string();
            let ver = run_spawn_json(&[
                "backup",
                "cas",
                "verify",
                "--store",
                store_clone.to_str().unwrap(),
                "--snapshot",
                &key,
                "--json",
            ]);
            assert_eq!(ver["verified"], true);
            key
        }));
    }

    let snap_keys: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Index contains all 4 concurrent snapshots cleanly
    let index: Value =
        serde_json::from_slice(&fs::read(store.join("fm-cas/v1/snapshots.json")).unwrap()).unwrap();
    let indexed_keys: Vec<&str> = index["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["snapshot_key"].as_str())
        .collect();
    for key in &snap_keys {
        assert!(
            indexed_keys.contains(&key.as_str()),
            "snapshot index missing {key}"
        );
    }

    // Legacy files are 100% untouched byte-for-byte
    for (rel, expected) in &legacy_files {
        assert_eq!(
            fs::read(store.join(rel)).unwrap().as_slice(),
            *expected,
            "legacy file {rel} was modified"
        );
    }
}

#[cfg(unix)]
fn chmod_tree(path: &Path, dir_mode: u32, file_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).unwrap();
    if meta.file_type().is_dir() {
        // Ensure readable/traversable before descending when restoring permissions.
        if dir_mode & 0o200 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(dir_mode)).unwrap();
        }
        for entry in fs::read_dir(path).unwrap() {
            chmod_tree(&entry.unwrap().path(), dir_mode, file_mode);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(dir_mode)).unwrap();
    } else if meta.file_type().is_file() {
        fs::set_permissions(path, fs::Permissions::from_mode(file_mode)).unwrap();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn cli_read_only_store_and_round_trip_resnapshot_determinism() {
    let scratch = ScratchDir::new("ro-det");
    let src = scratch.path().join("src");
    let store = scratch.path().join("store");
    let store_re = scratch.path().join("store_re");
    let fm_file = scratch.path().join("fm.json");
    let restored = scratch.path().join("restored");
    let restored_fm = scratch.path().join("restored_fm.json");

    let secs = 1_750_000_050u64;
    let nanos = 987_654_321u32;
    write_file(
        &src.join("src/lib.rs"),
        b"pub fn answer() -> u32 { 42 }\n",
        0o644,
        secs,
        nanos,
    );
    write_file(
        &src.join("bin/tool.sh"),
        b"#!/bin/sh\nexit 0\n",
        0o755,
        secs,
        nanos,
    );
    write_file(&src.join("data/empty.dat"), b"", 0o644, secs, nanos);
    write_file(
        &src.join("data/all_bytes.bin"),
        &(0u8..=255).collect::<Vec<_>>(),
        0o444,
        secs,
        nanos,
    );
    let large_len = 4 * 1024 * 1024 + 4096;
    let large_payload: Vec<u8> = (0..large_len)
        .map(|i| u8::try_from((i * 17) % 256).unwrap())
        .collect();
    write_file(
        &src.join("data/large_multi_chunk.bin"),
        &large_payload,
        0o644,
        secs,
        nanos,
    );
    fs::create_dir_all(src.join("empty/nested")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../src/lib.rs", src.join("bin/lib_link")).unwrap();
    write_sample_fm(&fm_file);

    let snap1 = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--fm-graph",
        fm_file.to_str().unwrap(),
        "--json",
    ]);
    let snap1_key = snap1["snapshot_key"].as_str().unwrap().to_string();

    // Freeze the entire store read-only (`0o555` dirs, `0o444` files).
    #[cfg(unix)]
    chmod_tree(&store, 0o555, 0o444);

    // `verify` and `restore` MUST succeed against a strictly read-only store!
    let verify_ro = run_spawn_json(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap1_key,
        "--json",
    ]);
    assert_eq!(verify_ro["verified"], true);

    let restore_ro = run_spawn_json(&[
        "backup",
        "cas",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap1_key,
        "--into",
        restored.to_str().unwrap(),
        "--restore-fm",
        restored_fm.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(restore_ro["snapshot_key"], snap1_key);

    // Unfreeze store so ScratchDir cleanup succeeds.
    #[cfg(unix)]
    chmod_tree(&store, 0o755, 0o644);

    // Re-snapshotting the restored tree + restored FM graph yields the EXACT same snapshot_key!
    let snap2 = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        restored.to_str().unwrap(),
        "--store",
        store_re.to_str().unwrap(),
        "--fm-graph",
        restored_fm.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(snap2["snapshot_key"], snap1["snapshot_key"]);
    assert_eq!(snap2["fm_graph_key"], snap1["fm_graph_key"]);

    // Verifying against a fresh empty directory must NOT create `fm-cas/`!
    let empty_store = scratch.path().join("empty_store");
    fs::create_dir_all(&empty_store).unwrap();
    let miss = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        empty_store.to_str().unwrap(),
        "--snapshot",
        &snap1_key,
    ]);
    assert!(!miss.status.success());
    assert!(
        !empty_store.join("fm-cas").exists(),
        "read-only verify must not create fm-cas directory in store"
    );
}

#[test]
fn cli_rejects_symlink_chain_escape_manifest_whitespace_tampering_and_bad_restore_fm() {
    let scratch = ScratchDir::new("sec-checks");
    let src = scratch.path().join("src");
    let store = scratch.path().join("store");

    fs::create_dir_all(src.join("a/b")).unwrap();
    write_file(&src.join("ok.txt"), b"ok\n", 0o644, 1_750_000_060, 0);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../..", src.join("a/b/jump")).unwrap();
        std::os::unix::fs::symlink("jump/../../etc/passwd", src.join("a/b/escape")).unwrap();
    }

    let snap = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--json",
    ]);
    let snap_key = snap["snapshot_key"].as_str().unwrap().to_string();

    // 1. Symlink chain escape refused on restore without creating target dir.
    #[cfg(unix)]
    {
        let escape_target = scratch.path().join("escape_out");
        let out = run_spawn(&[
            "backup",
            "cas",
            "restore",
            "--store",
            store.to_str().unwrap(),
            "--snapshot",
            &snap_key,
            "--into",
            escape_target.to_str().unwrap(),
        ]);
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("escapes restore root"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!escape_target.exists());
    }

    // 2. Trailing whitespace / extra field tampering in manifest rejected by verify.
    let manifest_path = snapshot_disk_path(&store, &snap_key);
    let orig = fs::read(&manifest_path).unwrap();
    let mut padded = orig.clone();
    padded.push(b' ');
    fs::write(&manifest_path, &padded).unwrap();
    let ws_fail = run_spawn(&[
        "backup",
        "cas",
        "verify",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &snap_key,
    ]);
    assert!(!ws_fail.status.success());
    fs::write(&manifest_path, &orig).unwrap();

    // 3. `--restore-fm` requested on snapshot without FM graph fails pre-flight without creating `--into`.
    let clean_src = scratch.path().join("clean_src");
    write_file(&clean_src.join("hi.txt"), b"hi", 0o644, 1_750_000_070, 0);
    let clean_snap = run_spawn_json(&[
        "backup",
        "cas",
        "snapshot",
        "--source",
        clean_src.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--json",
    ]);
    let clean_key = clean_snap["snapshot_key"].as_str().unwrap().to_string();
    let no_fm_target = scratch.path().join("no_fm_out");
    let no_fm_fail = run_spawn(&[
        "backup",
        "cas",
        "restore",
        "--store",
        store.to_str().unwrap(),
        "--snapshot",
        &clean_key,
        "--into",
        no_fm_target.to_str().unwrap(),
        "--restore-fm",
        scratch.path().join("fm_out.json").to_str().unwrap(),
    ]);
    assert!(!no_fm_fail.status.success());
    assert!(
        !no_fm_target.exists(),
        "failed --restore-fm pre-flight must not create --into directory"
    );
}
