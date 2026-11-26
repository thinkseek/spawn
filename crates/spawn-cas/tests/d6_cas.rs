//! D6 Phase 0 CAS layout freeze, FastCDC v1 chunking, canonical Merkle
//! trees/snapshots, persistent `O(|diff| + depth)` mutations, Git OID
//! projection, and cross-language golden fixture verification.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_cas::{
    CHUNK_AVG, CHUNK_MAX, CHUNK_MIN, CHUNKER_FASTCDC_V1, CasError, ChunkIndex, ChunkRef, Digest,
    GIT_POINTER_THRESHOLD, GitOid, GitOidCache, NS_ARTIFACTS, NS_BASE_CACHE, NS_BLOB, NS_TREE,
    Quotas, Snapshot, Store, Tree, TreeEntry, TreeEntryKind, TreeMutation, empty_tree_digest,
    fastcdc_gear_table, git_blob_oid_of_bytes, git_pointer_bytes, next_chunk_len, project_blob_oid,
    project_snapshot_oid, project_tree_oid, sha256_of,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-cas-d6-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct DirNode {
    children: BTreeMap<Vec<u8>, DirChild>,
}

enum DirChild {
    Dir(DirNode),
    Leaf(TreeEntryKind, [u8; 32], u64),
}

fn insert_dir_path(
    node: &mut DirNode,
    path: &[Vec<u8>],
    kind: TreeEntryKind,
    id: [u8; 32],
    size: u64,
) {
    if path.len() == 1 {
        node.children
            .insert(path[0].clone(), DirChild::Leaf(kind, id, size));
        return;
    }
    let entry = node
        .children
        .entry(path[0].clone())
        .or_insert_with(|| DirChild::Dir(DirNode::default()));
    let DirChild::Dir(sub) = entry else {
        panic!("path conflict in repository tree");
    };
    insert_dir_path(sub, &path[1..], kind, id, size);
}

fn write_dir_node(store: &Store, node: &DirNode) -> Digest {
    let mut entries = Vec::with_capacity(node.children.len());
    for (name, child) in &node.children {
        match child {
            DirChild::Dir(sub) => {
                let sub_digest = write_dir_node(store, sub);
                entries.push(TreeEntry::dir(name.clone(), &sub_digest));
            }
            DirChild::Leaf(kind, id, size) => {
                entries.push(TreeEntry {
                    kind: *kind,
                    name: name.clone(),
                    id: *id,
                    size: *size,
                });
            }
        }
    }
    store.put_tree(&Tree { entries }).expect("put tree")
}

fn splitmix64_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed;
    while out.len() < len {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/d6-cas-v1")
}

fn verify_golden_fastcdc_and_chunks(dir: &Path, manifest: &serde_json::Value, store: &Store) {
    let gear = fastcdc_gear_table();
    assert_eq!(
        format!("{:016x}", gear[0]),
        manifest["fastcdc_gear_0_hex"].as_str().expect("gear_0")
    );
    assert_eq!(
        format!("{:016x}", gear[255]),
        manifest["fastcdc_gear_255_hex"].as_str().expect("gear_255")
    );

    let seed_str = manifest["prng_seed_hex"]
        .as_str()
        .expect("seed")
        .strip_prefix("0x")
        .expect("hex");
    let seed = u64::from_str_radix(seed_str, 16).expect("parse seed");
    let payload_len =
        usize::try_from(manifest["prng_payload_len"].as_u64().expect("len")).expect("len");
    let payload = splitmix64_bytes(payload_len, seed);
    assert_eq!(
        sha256_of(&payload).to_string(),
        manifest["prng_payload_sha256"].as_str().expect("sha256")
    );

    let first_cut = next_chunk_len(&payload[..CHUNK_MAX]);
    let stored = store
        .put_blob(NS_BLOB, &mut payload.as_slice())
        .expect("put_blob");
    assert!(stored.chunked);
    assert_eq!(stored.digest, sha256_of(&payload));
    assert_eq!(
        stored.chunk_count,
        manifest["chunks"].as_array().expect("chunks").len()
    );

    let chunk_index = store.read_chunk_index(&stored.digest).expect("chunk index");
    assert_eq!(
        usize::try_from(chunk_index.chunks[0].len).expect("u32"),
        first_cut
    );
    let expected_chunks_bin = fs::read(dir.join("chunks.bin")).expect("chunks.bin");
    let encoded_chunks_bin = chunk_index.encode().expect("encode chunks");
    assert_eq!(encoded_chunks_bin, expected_chunks_bin);
    assert_eq!(
        sha256_of(&encoded_chunks_bin).to_string(),
        manifest["chunks_bin_sha256"]
            .as_str()
            .expect("chunks_bin_sha256")
    );
    assert_eq!(
        ChunkIndex::decode(&expected_chunks_bin).expect("decode chunks"),
        chunk_index
    );
    for (got, want) in chunk_index
        .chunks
        .iter()
        .zip(manifest["chunks"].as_array().expect("chunks"))
    {
        assert_eq!(got.id.to_string(), want["sha256"].as_str().expect("sha"));
        assert_eq!(u64::from(got.len), want["len"].as_u64().expect("chunk len"));
    }
}

fn verify_golden_merkle_and_pointer(dir: &Path, manifest: &serde_json::Value, store: &Store) {
    let z_blob = store
        .put_blob(NS_BLOB, &mut &b"z content\n"[..])
        .expect("z");
    let a_file = store.put_blob(NS_BLOB, &mut &b"hello\n"[..]).expect("af");
    let dot_file = store.put_blob(NS_BLOB, &mut &b"world\n"[..]).expect("df");
    let sym = store.put_blob(NS_BLOB, &mut &b"a/z.txt"[..]).expect("sym");
    let run_sh = store
        .put_blob(NS_BLOB, &mut &b"#!/bin/sh\necho ok\n"[..])
        .expect("run");

    let child_tree =
        Tree::from_entries(vec![TreeEntry::file("z.txt", &z_blob.digest, 10)]).expect("child tree");
    let expected_child_tree = fs::read(dir.join("child_tree.bin")).expect("child_tree.bin");
    assert_eq!(child_tree.encode().expect("enc"), expected_child_tree);
    assert_eq!(Tree::decode(&expected_child_tree).expect("dec"), child_tree);
    let child_tree_digest = store.put_tree(&child_tree).expect("put child tree");
    assert_eq!(
        child_tree_digest.to_string(),
        manifest["child_tree_bin_sha256"]
            .as_str()
            .expect("child_tree_bin_sha256")
    );

    let gitlink_bytes = hex_to_20("112233445566778899aabbccddeeff0012345678").expect("20-byte oid");
    let root_tree = Tree::from_entries(vec![
        TreeEntry::executable("run.sh", &run_sh.digest, 18),
        TreeEntry::symlink("a_file", &sym.digest, 7),
        TreeEntry::file("a-file", &a_file.digest, 6),
        TreeEntry::dir("a", &child_tree_digest),
        TreeEntry::gitlink("sub", &gitlink_bytes).expect("gitlink"),
        TreeEntry::file("a.file", &dot_file.digest, 6),
    ])
    .expect("root tree");
    let expected_tree = fs::read(dir.join("tree.bin")).expect("tree.bin");
    assert_eq!(root_tree.encode().expect("encode"), expected_tree);
    assert_eq!(Tree::decode(&expected_tree).expect("decode"), root_tree);
    let root_tree_digest = store.put_tree(&root_tree).expect("put tree");
    assert_eq!(
        root_tree_digest.to_string(),
        manifest["tree_bin_sha256"].as_str().expect("tree sha256")
    );

    let expected_snap = fs::read(dir.join("snapshot.bin")).expect("snapshot.bin");
    let snap = Snapshot::decode(&expected_snap).expect("decode snapshot");
    assert_eq!(snap.encode().expect("encode snapshot"), expected_snap);
    assert_eq!(
        sha256_of(&expected_snap).to_string(),
        manifest["snapshot_bin_sha256"]
            .as_str()
            .expect("snapshot sha256")
    );
    let snap_digest = store.put_merkle_snapshot(&snap).expect("put snap");
    assert_eq!(
        store.read_merkle_snapshot(&snap_digest).expect("read snap"),
        snap
    );

    let mut oid_cache = GitOidCache::new();
    let child_oid = project_tree_oid(store, &child_tree_digest, &mut oid_cache).expect("child");
    assert_eq!(
        child_oid.to_hex(),
        manifest["child_tree_git_oid"].as_str().expect("child oid")
    );
    let tree_oid = project_tree_oid(store, &root_tree_digest, &mut oid_cache).expect("tree");
    assert_eq!(
        tree_oid.to_hex(),
        manifest["tree_git_oid"].as_str().expect("tree oid")
    );
    let parent_oid = GitOid::parse_hex(
        manifest["parent_snapshot_git_oid"]
            .as_str()
            .expect("parent git oid"),
    )
    .expect("parse parent oid");
    let snap_oid =
        project_snapshot_oid(store, &snap, &[parent_oid], &mut oid_cache).expect("snap oid");
    assert_eq!(
        snap_oid.to_hex(),
        manifest["snapshot_git_oid"].as_str().expect("snap oid")
    );

    let oversized_digest = Digest::parse(
        manifest["oversized_dummy_digest"]
            .as_str()
            .expect("oversized digest"),
    )
    .expect("digest");
    let oversized_size = manifest["oversized_size"].as_u64().expect("oversized_size");
    assert!(oversized_size > GIT_POINTER_THRESHOLD);
    let pointer = git_pointer_bytes(&oversized_digest, oversized_size);
    let expected_pointer = fs::read(dir.join("pointer-100mib.bin")).expect("pointer.bin");
    assert_eq!(pointer, expected_pointer);
    assert_eq!(
        project_blob_oid(store, &oversized_digest, oversized_size)
            .expect("pointer oid")
            .to_hex(),
        manifest["pointer_git_oid"].as_str().expect("pointer oid")
    );
    assert_eq!(
        git_blob_oid_of_bytes(&pointer).to_hex(),
        manifest["pointer_git_oid"].as_str().expect("pointer oid")
    );
}

#[test]
fn golden_d6_cas_v1_fixtures_match_independent_spec() {
    let dir = fixtures_dir();
    let manifest_bytes = fs::read(dir.join("manifest.json")).expect("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("parse manifest");
    let scratch = Scratch::new("golden");
    let store = Store::open(scratch.join("cas")).expect("open");
    verify_golden_fastcdc_and_chunks(&dir, &manifest, &store);
    verify_golden_merkle_and_pointer(&dir, &manifest, &store);
}

#[test]
fn fastcdc_boundaries_streaming_corruption_and_legacy_oversized_objects() {
    const {
        assert!(CHUNK_AVG > CHUNK_MIN && CHUNK_AVG < CHUNK_MAX);
        assert!(CHUNKER_FASTCDC_V1 == 1);
    }

    let scratch = Scratch::new("fastcdc");
    let store = Store::open(scratch.join("cas")).expect("open");

    let empty = store.put_blob(NS_BLOB, &mut &b""[..]).expect("empty");
    assert!(!empty.chunked);
    assert_eq!(empty.size, 0);
    assert_eq!(store.read_blob(&empty.digest).expect("read empty"), b"");
    assert!(store.has_blob(&empty.digest));

    let exact_4m = splitmix64_bytes(CHUNK_MAX, 0x1111);
    let stored_exact = store
        .put_blob(NS_BLOB, &mut exact_4m.as_slice())
        .expect("4m");
    assert!(!stored_exact.chunked);
    assert_eq!(stored_exact.chunk_count, 1);
    assert!(store.has(&stored_exact.digest));
    assert!(!store.chunk_index_path(&stored_exact.digest).exists());

    let just_over = splitmix64_bytes(CHUNK_MAX + 1, 0x2222);
    let stored_over = store
        .put_blob(NS_BLOB, &mut just_over.as_slice())
        .expect("4m+1");
    assert!(stored_over.chunked);
    assert!(stored_over.chunk_count >= 2);
    assert!(!store.has(&stored_over.digest));
    assert!(store.has_blob(&stored_over.digest));
    assert!(store.chunk_index_path(&stored_over.digest).is_file());
    assert_eq!(
        store.read_blob(&stored_over.digest).expect("read 4m+1"),
        just_over
    );

    let ten_mib = splitmix64_bytes(10 * 1024 * 1024, 0x3333_4444);
    let stored_10m = store
        .put_blob(NS_BLOB, &mut ten_mib.as_slice())
        .expect("10m");
    assert!(stored_10m.chunked);
    let idx = store.read_chunk_index(&stored_10m.digest).expect("index");
    assert!(idx.chunks.len() >= 3);
    for (at, c) in idx.chunks.iter().enumerate() {
        let len = usize::try_from(c.len).expect("u32");
        assert!(len <= CHUNK_MAX);
        if at + 1 < idx.chunks.len() {
            assert!(len >= CHUNK_MIN, "non-final chunk {at} smaller than min");
        }
    }
    let mut streamed = Vec::new();
    let written = store
        .stream_blob(&stored_10m.digest, &mut streamed)
        .expect("stream 10m");
    assert_eq!(written, u64::try_from(ten_mib.len()).expect("len"));
    assert_eq!(streamed, ten_mib);

    let bad_chunk_path = store.object_path(&idx.chunks[1].id);
    let mut corrupt_bytes = fs::read(&bad_chunk_path).expect("read chunk");
    corrupt_bytes[17] ^= 0x01;
    fs::set_permissions(&bad_chunk_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    fs::write(&bad_chunk_path, corrupt_bytes).expect("write corrupt");
    assert!(
        store
            .stream_blob(&stored_10m.digest, &mut Vec::new())
            .is_err_and(|e| e.is_mismatch())
    );
    assert!(!bad_chunk_path.exists());

    let legacy_bytes = splitmix64_bytes(CHUNK_MAX + 512, 0x9999);
    let legacy_digest = sha256_of(&legacy_bytes);
    let legacy_path = store.object_path(&legacy_digest);
    fs::create_dir_all(legacy_path.parent().expect("parent")).expect("mkdir");
    fs::write(&legacy_path, &legacy_bytes).expect("write legacy");
    assert_eq!(
        store.read_blob(&legacy_digest).expect("read legacy"),
        legacy_bytes
    );
}

#[test]
fn persistent_namespaces_survive_zero_quotas_and_sync_store_succeeds() {
    let scratch = Scratch::new("gc-persist");
    let store = Store::open(scratch.join("cas")).expect("open");

    let tree_digest = store.put_empty_tree().expect("empty tree");
    let blob = store
        .put_blob(NS_BLOB, &mut &b"persistent blob bytes"[..])
        .expect("blob");
    let artifact = store
        .put_bytes(NS_ARTIFACTS, b"ephemeral artifact")
        .expect("artifact");
    let base_cache = store
        .put_bytes(NS_BASE_CACHE, b"ephemeral base cache")
        .expect("base cache");

    store.sync_store().expect("sync_store");

    let zero_all = Quotas::from([
        (NS_TREE.to_string(), 0),
        (NS_BLOB.to_string(), 0),
        (NS_ARTIFACTS.to_string(), 0),
        (NS_BASE_CACHE.to_string(), 0),
    ]);
    let report = store.gc(&zero_all).expect("gc");
    assert!(!store.has(&artifact));
    assert!(!store.has(&base_cache));
    assert!(store.has(&tree_digest));
    assert!(store.has(&blob.digest));
    assert_eq!(report.kept_pinned, 0);
}

#[test]
fn ephemeral_gc_never_deletes_object_shared_with_persistent_namespaces() {
    let scratch = Scratch::new("gc-shared-persist");
    let store = Store::open(scratch.join("cas")).expect("open");

    let shared_blob_bytes = b"shared artifact and persistent blob content";
    let shared_blob = store
        .put_bytes(NS_ARTIFACTS, shared_blob_bytes)
        .expect("artifact shared blob");
    let blob_res = store
        .put_blob(NS_BLOB, &mut shared_blob_bytes.as_slice())
        .expect("persistent shared blob");
    assert_eq!(blob_res.digest, shared_blob);

    let shared_empty = store.put_bytes(NS_ARTIFACTS, b"").expect("artifact empty");
    let persistent_empty = store
        .put_blob(NS_BLOB, &mut &b""[..])
        .expect("persistent empty");
    assert_eq!(persistent_empty.digest, shared_empty);

    let empty_tree_bytes = Tree { entries: vec![] }.encode().expect("encode tree");
    let shared_tree = store.put_empty_tree().expect("persistent empty tree");
    let artifact_tree = store
        .put_bytes(NS_ARTIFACTS, &empty_tree_bytes)
        .expect("artifact same bytes as tree");
    assert_eq!(artifact_tree, shared_tree);

    let ephemeral_only = store
        .put_bytes(NS_ARTIFACTS, b"only in artifacts")
        .expect("ephemeral only");

    let report = store
        .gc(&Quotas::from([(NS_ARTIFACTS.to_string(), 0)]))
        .expect("gc artifacts=0");
    assert_eq!(report.namespace_use.get(NS_ARTIFACTS), Some(&0));
    assert!(!store.has(&ephemeral_only));
    assert!(!store.access_path(NS_ARTIFACTS, &shared_blob).exists());
    assert!(!store.access_path(NS_ARTIFACTS, &shared_empty).exists());
    assert!(!store.access_path(NS_ARTIFACTS, &shared_tree).exists());

    for (digest, expected) in [
        (&shared_blob, shared_blob_bytes.as_slice()),
        (&shared_empty, &b""[..]),
        (&shared_tree, empty_tree_bytes.as_slice()),
    ] {
        assert!(store.has(digest), "shared object {digest} lost during gc");
        assert_eq!(store.read(digest).expect("read shared object"), expected);
    }
    assert!(store.access_path(NS_BLOB, &shared_blob).is_file());
    assert!(store.access_path(NS_BLOB, &shared_empty).is_file());
    assert!(store.access_path(NS_TREE, &shared_tree).is_file());
}

#[test]
fn merkle_tree_and_snapshot_validation_rejects_malformed_inputs() {
    let sample_id = sha256_of(b"sample").as_bytes();
    let sample_digest = Digest::from_raw_sha256(sample_id);

    for bad_name in [
        &b""[..],
        b".",
        b"..",
        b"a/b",
        b"a\0b",
        b".git",
        b".GIT",
        b".git.",
        b".Git ",
    ] {
        let tree = Tree {
            entries: vec![TreeEntry {
                kind: TreeEntryKind::File,
                name: bad_name.to_vec(),
                id: sample_id,
                size: 1,
            }],
        };
        assert!(matches!(tree.encode(), Err(CasError::BadFormat(_))));
    }

    let unsorted = Tree {
        entries: vec![
            TreeEntry::file("b", &sample_digest, 1),
            TreeEntry::file("a", &sample_digest, 1),
        ],
    };
    assert!(matches!(unsorted.encode(), Err(CasError::BadFormat(_))));

    let mut bad_wire = b"spawn-tree-v1\n".to_vec();
    bad_wire.extend_from_slice(&2_u32.to_be_bytes());
    for name in [b"b", b"a"] {
        bad_wire.push(b'f');
        bad_wire.extend_from_slice(&1_u16.to_be_bytes());
        bad_wire.extend_from_slice(name);
        bad_wire.extend_from_slice(&sample_id);
        bad_wire.extend_from_slice(&1_u64.to_be_bytes());
    }
    assert!(matches!(
        Tree::decode(&bad_wire),
        Err(CasError::BadFormat(_))
    ));

    let empty_dir = Tree {
        entries: vec![TreeEntry::dir("sub", &empty_tree_digest())],
    };
    assert!(matches!(empty_dir.encode(), Err(CasError::BadFormat(_))));

    let bad_dir_size = Tree {
        entries: vec![TreeEntry {
            kind: TreeEntryKind::Dir,
            name: b"sub".to_vec(),
            id: sample_id,
            size: 1,
        }],
    };
    assert!(matches!(bad_dir_size.encode(), Err(CasError::BadFormat(_))));

    assert!(matches!(
        TreeEntry::gitlink("sub", &[1_u8; 19]),
        Err(CasError::BadFormat(_))
    ));
    let mut bad_pad = [0_u8; 32];
    bad_pad[31] = 1;
    let bad_gitlink = Tree {
        entries: vec![TreeEntry {
            kind: TreeEntryKind::Gitlink,
            name: b"sub".to_vec(),
            id: bad_pad,
            size: 20,
        }],
    };
    assert!(matches!(bad_gitlink.encode(), Err(CasError::BadFormat(_))));

    let snap = Snapshot {
        tree: sample_digest.clone(),
        parents: vec![],
        commit_body: b"author x\n\nhi\n".to_vec(),
    };
    let mut snap_bytes = snap.encode().expect("encode");
    snap_bytes.push(b'!');
    assert!(matches!(
        Snapshot::decode(&snap_bytes),
        Err(CasError::BadFormat(_))
    ));
    let leading_zero = format!("spawn-snapshot-v1\ntree {sample_digest}\ncommit 01\na");
    assert!(matches!(
        Snapshot::decode(leading_zero.as_bytes()),
        Err(CasError::BadFormat(_))
    ));

    let bad_idx = ChunkIndex {
        file_id: sample_digest.clone(),
        file_size: 99,
        chunker: CHUNKER_FASTCDC_V1,
        chunks: vec![ChunkRef {
            id: sample_digest,
            len: 50,
        }],
    };
    assert!(matches!(bad_idx.encode(), Err(CasError::BadFormat(_))));
}

#[test]
fn merkle_mutation_never_touches_unrelated_subtrees_and_prunes_empty_dirs() {
    let scratch = Scratch::new("merkle-diff");
    let store = Store::open(scratch.join("cas")).expect("open");

    let f1 = store.put_blob(NS_BLOB, &mut &b"leaf1"[..]).expect("f1");
    let f2 = store.put_blob(NS_BLOB, &mut &b"leaf2"[..]).expect("f2");
    let f3 = store.put_blob(NS_BLOB, &mut &b"leaf3"[..]).expect("f3");

    let empty = store.put_empty_tree().expect("empty");
    let v1 = store
        .apply_tree_mutations(
            &empty,
            &[
                TreeMutation::Upsert {
                    path: vec![
                        b"untouched".to_vec(),
                        b"deep".to_vec(),
                        b"keep.txt".to_vec(),
                    ],
                    kind: TreeEntryKind::File,
                    id: f1.digest.as_bytes(),
                    size: f1.size,
                },
                TreeMutation::Upsert {
                    path: vec![b"work".to_vec(), b"a.txt".to_vec()],
                    kind: TreeEntryKind::File,
                    id: f2.digest.as_bytes(),
                    size: f2.size,
                },
                TreeMutation::Upsert {
                    path: vec![b"ephemeral".to_vec(), b"tmp.txt".to_vec()],
                    kind: TreeEntryKind::File,
                    id: f3.digest.as_bytes(),
                    size: f3.size,
                },
            ],
        )
        .expect("v1");

    let v1_root = store.read_tree(&v1).expect("read v1");
    let untouched_entry = v1_root
        .entries
        .iter()
        .find(|e| e.name == b"untouched")
        .expect("untouched dir");
    let untouched_digest = untouched_entry.digest().expect("digest");
    fs::remove_file(store.object_path(&untouched_digest)).expect("remove untouched tree");

    let v2 = store
        .apply_tree_mutations(
            &v1,
            &[
                TreeMutation::Upsert {
                    path: vec![b"work".to_vec(), b"b.txt".to_vec()],
                    kind: TreeEntryKind::Executable,
                    id: f3.digest.as_bytes(),
                    size: f3.size,
                },
                TreeMutation::Remove {
                    path: vec![b"ephemeral".to_vec(), b"tmp.txt".to_vec()],
                },
            ],
        )
        .expect("v2");

    let v2_root = store.read_tree(&v2).expect("read v2");
    assert_eq!(
        v2_root
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect::<Vec<_>>(),
        ["untouched", "work"]
    );
    assert_eq!(v2_root.entries[0].id, untouched_entry.id);
}

#[test]
fn real_git_repository_head_and_merge_commit_match_40_hex_oid() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if repo_root.join(".git").exists() {
        verify_repo_head_oid(&repo_root);
    }
    verify_two_parent_merge_commit_oid();
}

fn split_raw_commit(raw_commit: &[u8]) -> (Vec<GitOid>, Vec<u8>) {
    let mut parent_oids = Vec::new();
    let mut body_start = 0;
    for line in raw_commit.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b"tree ") {
            body_start += line.len();
        } else if let Some(rest) = line.strip_prefix(b"parent ") {
            let hex = std::str::from_utf8(rest).expect("utf8").trim();
            parent_oids.push(GitOid::parse_hex(hex).expect("parent oid"));
            body_start += line.len();
        } else {
            break;
        }
    }
    (parent_oids, raw_commit[body_start..].to_vec())
}

fn ingest_tree_into_cas(repo_root: &Path, ls_tree: &[u8], store: &Store) -> Digest {
    let mut batch_input = Vec::new();
    for rec in ls_tree.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = rec.iter().position(|&b| b == b'\t').expect("tab");
        let meta = std::str::from_utf8(&rec[..tab]).expect("utf8");
        let mut parts = meta.split(' ');
        let mode = parts.next().expect("mode");
        let _kind = parts.next().expect("kind");
        let oid_hex = parts.next().expect("oid");
        if mode != "160000" {
            batch_input.extend_from_slice(oid_hex.as_bytes());
            batch_input.push(b'\n');
        }
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "--batch"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cat-file");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&batch_input)
        .expect("write batch");
    let output = child.wait_with_output().expect("cat-file output");
    assert!(output.status.success());

    let raw = &output.stdout;
    let mut cursor = 0;
    let mut root_dir = DirNode::default();
    for rec in ls_tree.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = rec.iter().position(|&b| b == b'\t').expect("tab");
        let meta = std::str::from_utf8(&rec[..tab]).expect("utf8");
        let segments: Vec<Vec<u8>> = rec[tab + 1..]
            .split(|&b| b == b'/')
            .map(<[u8]>::to_vec)
            .collect();
        let mut parts = meta.split(' ');
        let mode = parts.next().expect("mode");
        let _kind = parts.next().expect("kind");
        let oid_hex = parts.next().expect("oid");
        if mode == "160000" {
            let oid = GitOid::parse_hex(oid_hex).expect("oid");
            let mut id = [0_u8; 32];
            id[..20].copy_from_slice(oid.as_bytes());
            insert_dir_path(&mut root_dir, &segments, TreeEntryKind::Gitlink, id, 20);
            continue;
        }
        let rel_nl = raw[cursor..]
            .iter()
            .position(|&b| b == b'\n')
            .expect("hdr nl");
        let hdr = std::str::from_utf8(&raw[cursor..cursor + rel_nl]).expect("hdr utf8");
        cursor += rel_nl + 1;
        let size: usize = hdr
            .rsplit_once(' ')
            .expect("hdr space")
            .1
            .parse()
            .expect("blob size");
        let mut content = &raw[cursor..cursor + size];
        cursor += size + 1;

        let kind = match mode {
            "100644" => TreeEntryKind::File,
            "100755" => TreeEntryKind::Executable,
            "120000" => TreeEntryKind::Symlink,
            other => panic!("unexpected git mode {other}"),
        };
        let stored = store.put_blob(NS_BLOB, &mut content).expect("store blob");
        insert_dir_path(
            &mut root_dir,
            &segments,
            kind,
            stored.digest.as_bytes(),
            stored.size,
        );
    }
    write_dir_node(store, &root_dir)
}

fn verify_repo_head_oid(repo_root: &Path) {
    let head_oid = git_stdout(repo_root, &["rev-parse", "HEAD"]);
    let raw_commit = git_bytes(repo_root, &["cat-file", "commit", &head_oid]);
    let ls_tree = git_bytes(repo_root, &["ls-tree", "-r", "-z", "HEAD"]);
    let (parent_oids, commit_body) = split_raw_commit(&raw_commit);

    let scratch = Scratch::new("real-head");
    let store = Store::open(scratch.join("cas")).expect("open");
    let root_tree = ingest_tree_into_cas(repo_root, &ls_tree, &store);
    let snapshot = Snapshot {
        tree: root_tree,
        parents: parent_oids
            .iter()
            .enumerate()
            .map(|(i, _)| sha256_of(format!("parent-{i}").as_bytes()))
            .collect(),
        commit_body,
    };
    let mut cache = GitOidCache::new();
    let projected =
        project_snapshot_oid(&store, &snapshot, &parent_oids, &mut cache).expect("project");
    assert_eq!(projected.to_hex(), head_oid);
}

fn setup_domain_collision_roots(
    store: &Store,
    bare: &Path,
) -> (Digest, Digest, Digest, String, String) {
    let leaf_bytes = b"hello\n";
    let leaf_blob = store
        .put_blob(NS_BLOB, &mut leaf_bytes.as_slice())
        .expect("leaf blob");
    let child_tree = Tree {
        entries: vec![TreeEntry {
            kind: TreeEntryKind::File,
            name: b"hello.txt".to_vec(),
            id: leaf_blob.digest.as_bytes(),
            size: leaf_blob.size,
        }],
    };
    let child_bytes = child_tree.encode().expect("encode child tree");
    let child_tree_digest = store.put_tree(&child_tree).expect("put child tree");
    let child_file_blob = store
        .put_blob(NS_BLOB, &mut child_bytes.as_slice())
        .expect("put file with identical bytes");
    assert_eq!(child_file_blob.digest, child_tree_digest);

    let root_dir_first = store
        .put_tree(&Tree {
            entries: vec![
                TreeEntry::dir(b"a_dir".to_vec(), &child_tree_digest),
                TreeEntry {
                    kind: TreeEntryKind::File,
                    name: b"z_child_tree.bin".to_vec(),
                    id: child_file_blob.digest.as_bytes(),
                    size: child_file_blob.size,
                },
            ],
        })
        .expect("root dir first");
    let root_file_first = store
        .put_tree(&Tree {
            entries: vec![
                TreeEntry {
                    kind: TreeEntryKind::File,
                    name: b"a_child_tree.bin".to_vec(),
                    id: child_file_blob.digest.as_bytes(),
                    size: child_file_blob.size,
                },
                TreeEntry::dir(b"z_dir".to_vec(), &child_tree_digest),
            ],
        })
        .expect("root file first");

    let git_hello = git_stdout_stdin(bare, &["hash-object", "-w", "--stdin"], leaf_bytes);
    let git_same_blob = git_stdout_stdin(bare, &["hash-object", "-w", "--stdin"], &child_bytes);
    let git_child_tree = git_stdout_stdin(
        bare,
        &["mktree"],
        format!("100644 blob {git_hello}\thello.txt\n").as_bytes(),
    );
    let git_dir_first = git_stdout_stdin(
        bare,
        &["mktree"],
        format!(
            "040000 tree {git_child_tree}\ta_dir\n100644 blob {git_same_blob}\tz_child_tree.bin\n"
        )
        .as_bytes(),
    );
    let git_file_first = git_stdout_stdin(
        bare,
        &["mktree"],
        format!(
            "100644 blob {git_same_blob}\ta_child_tree.bin\n040000 tree {git_child_tree}\tz_dir\n"
        )
        .as_bytes(),
    );
    (
        child_tree_digest,
        root_dir_first,
        root_file_first,
        git_dir_first,
        git_file_first,
    )
}

#[test]
fn git_oid_projection_separates_tree_and_blob_cache_domains() {
    let scratch = Scratch::new("oid-domain-cache");
    let bare = scratch.join("repo.git");
    fs::create_dir_all(&bare).expect("mkdir");
    assert!(
        Command::new("git")
            .args(["init", "--bare"])
            .arg(&bare)
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .status()
            .expect("git init")
            .success()
    );

    let store = Store::open(scratch.join("cas")).expect("open");
    let (child_digest, root_dir_first, root_file_first, git_dir_first, git_file_first) =
        setup_domain_collision_roots(&store, &bare);

    let mut cache_a = GitOidCache::new();
    assert_eq!(
        project_tree_oid(&store, &root_dir_first, &mut cache_a)
            .expect("dir first")
            .to_hex(),
        git_dir_first
    );
    assert_eq!(
        project_tree_oid(&store, &root_file_first, &mut cache_a)
            .expect("file first after dir")
            .to_hex(),
        git_file_first
    );
    assert_ne!(
        cache_a.trees.get(&child_digest),
        cache_a.blobs.get(&child_digest)
    );

    let mut cache_b = GitOidCache::new();
    assert_eq!(
        project_tree_oid(&store, &root_file_first, &mut cache_b)
            .expect("file first")
            .to_hex(),
        git_file_first
    );
    assert_eq!(
        project_tree_oid(&store, &root_dir_first, &mut cache_b)
            .expect("dir first after file")
            .to_hex(),
        git_dir_first
    );
}

fn verify_two_parent_merge_commit_oid() {
    let scratch = Scratch::new("merge-oracle");
    let bare = scratch.join("repo.git");
    fs::create_dir_all(&bare).expect("mkdir");
    assert!(
        Command::new("git")
            .args(["init", "--bare"])
            .arg(&bare)
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .status()
            .expect("git init")
            .success()
    );
    let tree_oid = git_stdout_stdin(&bare, &["mktree"], b"");
    let mut p1 = git_commit_tree(&bare, &tree_oid, &[], "first parent\n");
    let mut p2 = git_commit_tree(&bare, &tree_oid, &[], "second parent\n");
    let merge = git_commit_tree(
        &bare,
        &tree_oid,
        &[&p1, &p2],
        "merge commit\n\nSpawn-Task: 42\n",
    );
    let raw_merge = git_bytes(&bare, &["cat-file", "commit", &merge]);
    let (parent_oids, commit_body) = split_raw_commit(&raw_merge);
    p1 = parent_oids[0].to_hex();
    p2 = parent_oids[1].to_hex();
    assert_eq!(parent_oids.len(), 2);

    let store = Store::open(scratch.join("cas")).expect("open");
    let empty = store.put_empty_tree().expect("empty");
    let snap = Snapshot {
        tree: empty,
        parents: vec![sha256_of(p1.as_bytes()), sha256_of(p2.as_bytes())],
        commit_body,
    };
    let mut cache = GitOidCache::new();
    let projected =
        project_snapshot_oid(&store, &snap, &parent_oids, &mut cache).expect("project merge");
    assert_eq!(projected.to_hex(), merge);
}

fn hex_to_20(s: &str) -> Result<[u8; 20], CasError> {
    Ok(*GitOid::parse_hex(s)?.as_bytes())
}

fn git_commit_tree(repo: &Path, tree: &str, parents: &[&str], msg: &str) -> String {
    let mut args = vec!["commit-tree", tree];
    for p in parents {
        args.push("-p");
        args.push(p);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Spawn")
        .env("GIT_AUTHOR_EMAIL", "spawn@thinkseek.dev")
        .env("GIT_AUTHOR_DATE", "1795700000 +0000")
        .env("GIT_COMMITTER_NAME", "Spawn")
        .env("GIT_COMMITTER_EMAIL", "spawn@thinkseek.dev")
        .env("GIT_COMMITTER_DATE", "1795700000 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn commit-tree");
    let mut child = out;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(msg.as_bytes())
        .expect("write msg");
    let finished = child.wait_with_output().expect("commit-tree output");
    assert!(finished.status.success());
    String::from_utf8(finished.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

fn git_stdout_stdin(repo: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("stdin write");
    let out = child.wait_with_output().expect("output");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let bytes = git_bytes(repo, args);
    String::from_utf8(bytes).expect("utf8").trim().to_string()
}

fn git_bytes(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
