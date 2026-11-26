//! The Git walker's pure parts: identity (decisions B2), paths, subjects,
//! trailers, raw diffs, classes and the targets of `show`.

use spawn_postgres::provenance::ProvEntryClass;

use super::index::{
    Checkpoint, RawDiff, Repo, Trailers, base32, change_id, checkpoint_execution, classify,
    domain_of, edit_branch_key, edit_task_key, import_key, land_key, parse_commit, parse_header,
    parse_raw, stored_path, trailers,
};
use super::{Target, path_and_line, target};

#[test]
fn base32_is_rfc_4648_in_lowercase_without_padding() {
    // RFC 4648 §10's vectors, lowercased and unpadded.
    for (input, expected) in [
        ("", ""),
        ("f", "my"),
        ("fo", "mzxq"),
        ("foo", "mzxw6"),
        ("foob", "mzxw6yq"),
        ("fooba", "mzxw6ytb"),
        ("foobar", "mzxw6ytboi"),
    ] {
        assert_eq!(base32(input.as_bytes()), expected, "{input:?}");
    }
}

#[test]
fn a_change_id_is_derived_from_its_domain_and_key_as_b2_says() {
    use sha2::{Digest, Sha256};
    let domain = domain_of("/srv/spawn.git");
    assert_eq!(domain, "git:/srv/spawn.git");
    let key = edit_task_key(581);
    assert_eq!(key, "edit:task:581");
    let mut bytes = b"spawn-change-v1\0".to_vec();
    bytes.extend_from_slice(domain.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(key.as_bytes());
    let expected = format!("c{}", &base32(&Sha256::digest(&bytes))[..26]);
    let id = change_id(&domain, &key);
    assert_eq!(id, expected);
    assert_eq!(id.len(), 27);
    assert_eq!(id, change_id(&domain, &key), "the same every time");
    assert_ne!(id, change_id(&domain_of("/srv/other.git"), &key));
    assert_ne!(id, change_id(&domain, &edit_task_key(582)));
    assert_eq!(
        target(&id),
        Ok(Target::Change(id.clone())),
        "show takes it as a change id"
    );
    assert_eq!(edit_branch_key("spawn/a", "abc"), "edit:branch:spawn/a:abc");
    assert_eq!(land_key(7, "abc"), "land:7:abc");
    assert_eq!(import_key("abc"), "import:abc");
}

#[test]
fn a_path_is_stored_as_git_holds_it_unless_it_must_be_quoted() {
    assert_eq!(stored_path(b"src/main.rs"), "src/main.rs");
    assert_eq!(stored_path("docs/中文.md".as_bytes()), "docs/中文.md");
    assert_eq!(stored_path(b"a b\\c"), "a b\\c");
    assert_eq!(stored_path(b"a\nb"), "\"a\\nb\"");
    assert_eq!(stored_path(b"tab\there"), "\"tab\\there\"");
    assert_eq!(stored_path(b"\"quoted"), "\"\\\"quoted\"");
    assert_eq!(stored_path(b"bad\xffbyte"), "\"bad\\377byte\"");
    assert_eq!(stored_path(b"bell\x07"), "\"bell\\007\"");
}

#[test]
fn only_a_checkpoints_subject_names_its_execution() {
    assert_eq!(
        checkpoint_execution("spawn checkpoint exec_12\n"),
        Some(Checkpoint {
            execution: Some("exec_12".to_string())
        })
    );
    assert_eq!(
        checkpoint_execution("spawn checkpoint\n\nSpawn-Large: aa"),
        Some(Checkpoint { execution: None })
    );
    for other in [
        "spawn: merge main abc into agent_a",
        "Land spawn checkpoint exec_1 (task 3)",
        "spawn checkpointed",
        "",
    ] {
        assert_eq!(checkpoint_execution(other), None, "{other:?}");
    }
}

#[test]
fn trailers_are_read_from_the_last_paragraph_only() {
    let head = "a".repeat(40);
    let message = format!(
        "Title (objective 3, task 9)\n\nWhat changed\n    Spawn-Task: 1\n\nSpawn-Task: 9\nSpawn-Objective: 3\nSpawn-Head: {head}\n"
    );
    assert_eq!(
        trailers(&message),
        Trailers {
            task: Some(9),
            head: Some(head.clone()),
        }
    );
    // A report's line is indented by the integrator, and a body paragraph
    // is never the trailers.
    let forged = format!("Title\n\nSpawn-Task: 1\nSpawn-Head: {head}\n\nThe end.");
    assert_eq!(trailers(&forged), Trailers::default());
    assert_eq!(trailers("Spawn-Task: 4"), Trailers::default());
    let short = "Title\n\nSpawn-Task: 4\nSpawn-Head: abc";
    assert_eq!(
        trailers(short),
        Trailers {
            task: Some(4),
            head: None,
        }
    );
}

#[test]
fn a_raw_diff_is_parsed_with_every_byte_of_its_paths() {
    let zero = "0".repeat(40);
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    let listing = format!(
        ":000000 100644 {zero} {a} A\0new file\0:100644 100755 {a} {a} M\0bin/run\0:100644 000000 {b} {zero} D\0gone\n\0"
    );
    let diffs = parse_raw(listing.as_bytes()).expect("parsed");
    assert_eq!(diffs.len(), 3);
    assert_eq!(diffs[0].path, b"new file");
    assert_eq!(
        (diffs[0].old.as_deref(), diffs[0].new.as_deref()),
        (None, Some(a.as_str()))
    );
    assert_eq!(diffs[1].old_mode, "100644");
    assert_eq!(diffs[1].new_mode, "100755");
    assert_eq!(diffs[2].path, b"gone\n");
    assert_eq!(diffs[2].new, None);
    assert!(parse_raw(b"nonsense\0x\0").is_err());
    assert_eq!(parse_raw(b"").expect("empty"), Vec::new());
}

fn diff(path: &str, old: Option<&str>, new: Option<&str>, modes: (&str, &str)) -> RawDiff {
    RawDiff {
        path: path.as_bytes().to_vec(),
        old_mode: modes.0.to_string(),
        new_mode: modes.1.to_string(),
        old: old.map(str::to_string),
        new: new.map(str::to_string),
    }
}

#[test]
fn a_class_is_mechanical_for_a_mode_alone_generated_for_the_list_and_the_attribute_wins() {
    let plain = ("100644", "100644");
    let content = diff("src/a.rs", Some("1"), Some("2"), plain);
    assert_eq!(classify(&content, None), ProvEntryClass::Content);
    let mode = diff("bin/run", Some("1"), Some("1"), ("100644", "100755"));
    assert_eq!(classify(&mode, None), ProvEntryClass::Mechanical);
    for generated in [
        "Cargo.lock",
        "crates/x/Cargo.lock",
        "docs/feature-map/registry.json",
    ] {
        let entry = diff(generated, Some("1"), Some("2"), plain);
        assert_eq!(
            classify(&entry, None),
            ProvEntryClass::Generated,
            "{generated}"
        );
        assert_eq!(classify(&entry, Some("content")), ProvEntryClass::Content);
    }
    let lookalike = diff("docs/Cargo.lock.md", Some("1"), Some("2"), plain);
    assert_eq!(classify(&lookalike, None), ProvEntryClass::Content);
    assert_eq!(
        classify(&content, Some("generated")),
        ProvEntryClass::Generated
    );
    assert_eq!(
        classify(&content, Some("mechanical")),
        ProvEntryClass::Mechanical
    );
    assert_eq!(classify(&content, Some("opaque")), ProvEntryClass::Content);
}

#[test]
fn a_commit_object_gives_its_parents_tree_time_and_message() {
    let body = b"tree 1111\nparent aaaa\nparent bbbb\nauthor A <a@x> 1700000000 +0000\ncommitter C <c@x> 1700000123 +0200\n\nSubject\n\nBody\n";
    let commit = parse_commit("cccc", body).expect("parsed");
    assert_eq!(commit.parents, vec!["aaaa".to_string(), "bbbb".to_string()]);
    assert_eq!(commit.tree, "1111");
    assert_eq!(commit.at_ms, 1_700_000_123_000);
    assert_eq!(commit.message, "Subject\n\nBody\n");
    assert!(parse_commit("dddd", b"parent aaaa\n\nx").is_err());
}

#[test]
fn show_names_a_change_a_task_or_a_full_commit() {
    let commit = "A".repeat(40);
    assert_eq!(target("581"), Ok(Target::Task(581)));
    assert_eq!(target(&commit), Ok(Target::Commit("a".repeat(40))));
    for bad in ["", "abc1234", "c0000", "1234567890123456789", "task 3"] {
        assert!(target(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn who_reads_a_trailing_line_number() {
    assert_eq!(
        path_and_line("src/a.rs:12"),
        Ok(("src/a.rs".to_string(), Some(12)))
    );
    assert_eq!(
        path_and_line("src/a.rs"),
        Ok(("src/a.rs".to_string(), None))
    );
    assert_eq!(
        path_and_line("odd:name.rs"),
        Ok(("odd:name.rs".to_string(), None))
    );
    assert!(path_and_line("src/a.rs:0").is_err());
}

#[test]
fn a_cat_file_answer_is_read_from_its_end_so_a_name_with_spaces_can_be_missing() {
    let oid = "a".repeat(40);
    assert_eq!(
        parse_header(&format!("{oid} tree 123\n")),
        Ok(Some((oid.clone(), "tree".to_string(), 123)))
    );
    for missing in [
        format!("{oid}:a dir missing\n"),
        format!("{oid}:a dir with three spaces missing\n"),
        format!("{oid}:x missing\n"),
        "abc ambiguous\n".to_string(),
    ] {
        assert_eq!(parse_header(&missing), Ok(None), "{missing:?}");
    }
    for bad in ["", "nonsense\n", "a b c d\n", "oid blob notasize\n"] {
        assert!(parse_header(bad).is_err(), "{bad:?}");
    }
}

/// The regression of a fold (a Change over 10,000 entries) that adds or
/// removes a top-level directory whose name holds a space: the directory's
/// tree is asked of the side that lacks it, and git's `missing` answer
/// must read as absent, not as an error that stops every later run.
#[test]
fn a_missing_top_level_directory_with_spaces_in_its_name_is_absent() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let dir = std::env::temp_dir().join(format!(
        "spawn-prov-subtree-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("has space")).expect("a work tree");
    let result = std::panic::catch_unwind(|| {
        let git = |arguments: &[&str]| {
            let output = spawn_host::test_git_command()
                .arg("-C")
                .arg(&dir)
                .args(arguments)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {arguments:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("has space/x"), "x\n").expect("a file");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "one"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let mut repo = Repo::open(&dir.join(".git").display().to_string()).expect("the repo");
        assert!(repo.subtree(&commit, "has space").expect("asked").is_some());
        assert_eq!(repo.subtree(&commit, "no such dir").expect("asked"), None);
        assert_eq!(repo.subtree(&commit, "has space/x").expect("asked"), None);
        // The process still answers after a missing name.
        assert!(repo.subtree(&commit, "has space").expect("asked").is_some());
    });
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// PV3b: a data ref's identity, its names, and the diff of two manifests.
mod data_domain {
    use spawn_cas::{Manifest, ManifestEntry, sha256_of};
    use spawn_postgres::provenance::ProvEntryClass;

    use super::super::data::{
        ROOT_PATH, change_id_of, check_ref, diff, domain_of, entries, parse_digest, snapshot_args,
    };
    use super::super::index::{FOLD_OVER, change_id};
    use super::super::{Target, target};

    fn manifest(files: &[(&str, &str)]) -> Manifest {
        Manifest::from_entries(
            files
                .iter()
                .map(|(path, text)| ManifestEntry {
                    path: path.as_bytes().to_vec(),
                    digest: sha256_of(text.as_bytes()),
                    size: Some(text.len() as u64),
                })
                .collect(),
        )
        .expect("a manifest")
    }

    #[test]
    fn a_data_refs_change_id_has_domain_and_key_data_ref() {
        assert_eq!(domain_of("clean"), "data:clean");
        assert_eq!(change_id_of("clean"), change_id("data:clean", "data:clean"));
        assert_ne!(change_id_of("clean"), change_id_of("merge"));
        assert_eq!(
            target("data:clean"),
            Ok(Target::Change(change_id_of("clean")))
        );
    }

    #[test]
    fn refs_are_plain_names_and_unprefixed_digests_are_refused_everywhere() {
        for good in ["clean", "merge-3", "shards_v2.1", "_x"] {
            check_ref(good).expect(good);
        }
        let hex = "ab".repeat(32);
        for bad in ["", "-x", "a:b", "a/b", "../x", hex.as_str()] {
            assert!(check_ref(bad).is_err(), "{bad:?}");
        }
        assert!(
            parse_digest(&hex)
                .expect_err("unprefixed")
                .contains("unprefixed")
        );
        assert!(target(&hex).expect_err("unprefixed").contains("unprefixed"));
        let snapshot = format!("sha256:{hex}");
        assert_eq!(target(&snapshot), Ok(Target::Snapshot(snapshot.clone())));
        assert_eq!(
            parse_digest(&snapshot).expect("prefixed").to_string(),
            snapshot
        );
        assert!(target("md5:abc").is_err());
    }

    #[test]
    fn snapshot_takes_one_directory_one_ref_and_repeated_inputs() {
        let words = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let args = snapshot_args(&words(&[
            "data/clean",
            "--ref",
            "clean",
            "--input",
            "raw",
            "--input",
            "sha256:00",
            "--code",
            "abc",
            "--config",
            "a.toml",
            "--task",
            "7",
            "--digest-only",
        ]))
        .expect("parsed");
        assert_eq!(args.dir, "data/clean");
        assert_eq!(args.name, "clean");
        assert_eq!(args.inputs, ["raw", "sha256:00"]);
        assert_eq!((args.code.len(), args.config.len()), (1, 1));
        assert_eq!(args.task, Some(7));
        assert!(args.digest_only);
        assert!(snapshot_args(&words(&["d"])).is_err(), "--ref is required");
        assert!(snapshot_args(&words(&["--ref", "x"])).is_err());
        assert!(snapshot_args(&words(&["a", "b", "--ref", "x"])).is_err());
        assert!(snapshot_args(&words(&["a", "--ref", "x", "--bogus", "1"])).is_err());
    }

    #[test]
    fn entries_are_the_paths_that_differ_and_fold_past_the_limit() {
        let before = manifest(&[("a", "1"), ("b", "2"), ("d/x", "3")]);
        let after = manifest(&[("a", "1"), ("c", "4"), ("d/x", "5")]);
        let found = diff(&before, &after);
        let paths: Vec<&[u8]> = found.iter().map(|diff| diff.path.as_slice()).collect();
        assert_eq!(paths, [&b"b"[..], b"c", b"d/x"]);
        assert_eq!(found[0].after, None);
        assert_eq!(found[1].before, None);
        assert_eq!(found[2].size, Some(1));
        let rows = entries(&before, &after, Some(9));
        assert!(
            rows.iter()
                .all(|row| row.origin == Some(9) && row.class == ProvEntryClass::Content)
        );
        assert!(diff(&after, &after).is_empty());

        // Past FOLD_OVER: one tree row per top-level directory, its digests
        // the directory's own manifests.
        let names: Vec<(String, String)> = (0..=FOLD_OVER)
            .map(|n| {
                (
                    format!("{}/{n}", if n % 2 == 0 { "even" } else { "odd" }),
                    n.to_string(),
                )
            })
            .collect();
        let mut files: Vec<(&str, &str)> = names
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        files.push(("top", "t"));
        let big = manifest(&files);
        let rows = entries(&Manifest::default(), &big, None);
        let shown: Vec<(&str, ProvEntryClass)> = rows
            .iter()
            .map(|row| (row.path.as_str(), row.class))
            .collect();
        assert_eq!(
            shown,
            [
                ("even", ProvEntryClass::Tree),
                ("odd", ProvEntryClass::Tree),
                ("top", ProvEntryClass::Content)
            ]
        );
        assert_eq!(
            rows[0].after.as_deref(),
            Some(big.subtree(b"even").digest().to_string().as_str())
        );
        assert_eq!(rows[0].before, None);

        // A flat directory over the cap: one tree row of the root.
        let flat_names: Vec<String> = (0..FOLD_OVER + 5).map(|n| format!("f{n:05}")).collect();
        let flat = manifest(
            &flat_names
                .iter()
                .map(|name| (name.as_str(), name.as_str()))
                .collect::<Vec<_>>(),
        );
        let rows = entries(&Manifest::default(), &flat, Some(3));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, ROOT_PATH);
        assert_eq!(rows[0].class, ProvEntryClass::Tree);
        assert_eq!(rows[0].before, None);
        assert_eq!(rows[0].after, Some(flat.digest().to_string()));
        assert_eq!(rows[0].origin, Some(3));
        // Back to empty: the root row's after is absent.
        let rows = entries(&flat, &Manifest::default(), None);
        assert_eq!((rows.len(), rows[0].after.as_deref()), (1, None));

        // More than FOLD_OVER top-level directories: never over the cap.
        let deep_names: Vec<String> = (0..FOLD_OVER + 5).map(|n| format!("d{n:05}/x")).collect();
        let deep = manifest(
            &deep_names
                .iter()
                .map(|name| (name.as_str(), name.as_str()))
                .collect::<Vec<_>>(),
        );
        let rows = entries(&Manifest::default(), &deep, None);
        assert!(rows.len() <= FOLD_OVER, "{} rows", rows.len());
        assert_eq!(
            (rows[0].path.as_str(), rows[0].class),
            (ROOT_PATH, ProvEntryClass::Tree)
        );
    }
}

/// PV4: the dispatcher's directive lines, and the durability report's rules
/// (decisions B6, D4) on rows made by hand.
mod pv4 {
    use serde_json::{Value, json};
    use spawn_postgres::provenance::{
        ProvAttributionSource, ProvBoundary, ProvChangeKind, ProvChangeRecord, ProvEdgeKind,
        ProvEdgeRecord, ProvEntryClass, ProvEntryRecord, ProvSupersedes,
    };

    use super::super::durability::{Input, Window, tally, text};
    use super::super::index::{Directive, DirectiveKind, SupersedeReason, directives};

    #[test]
    fn only_whole_directive_lines_of_a_brief_are_directives() {
        let brief = "Objective 7: undo the bad change\n\
                     \n\
                     Reverts: task 11\n\
                     - fixes: task #12.\n\
                     * SUPERSEDES:  TASK 13 (Refactor)\n\
                     supersedes: task 14 (planned)\n\
                     supersedes: task 15 (requirement)\n\
                     supersedes: task 16 (format)\n\
                     reverts: task 11\n\
                     supersedes: task 17\n\
                     supersedes: task 18 (defect)\n\
                     The worker said `reverts: task 19` in its report.\n\
                     reverts: task 20 and 21\n\
                     fixes: task 0\n\
                     fixes: task 22x\n\
                     fixes: tasks 23\n\
                     reverts task 24\n\
                     > reverts: task 25\n";
        let found = directives(brief);
        assert_eq!(
            found,
            [
                Directive {
                    kind: DirectiveKind::Reverts,
                    task: 11
                },
                Directive {
                    kind: DirectiveKind::Fixes,
                    task: 12
                },
                Directive {
                    kind: DirectiveKind::Supersedes(SupersedeReason::Refactor),
                    task: 13
                },
                Directive {
                    kind: DirectiveKind::Supersedes(SupersedeReason::Planned),
                    task: 14
                },
                Directive {
                    kind: DirectiveKind::Supersedes(SupersedeReason::Requirement),
                    task: 15
                },
                Directive {
                    kind: DirectiveKind::Supersedes(SupersedeReason::Format),
                    task: 16
                },
            ],
            "a repeat is once; no reason, a defect reason, prose and junk are none"
        );
        assert!(directives("").is_empty());
        assert!(directives("reverts:").is_empty());
        assert!(directives("reverts: task").is_empty());
    }

    const DOMAIN: &str = "git:/srv/durability.git";

    fn change(
        id: i64,
        kind: ProvChangeKind,
        task: Option<i64>,
        preset: Option<&str>,
    ) -> ProvChangeRecord {
        ProvChangeRecord {
            id,
            change_id: format!("c{id}"),
            rev: 1,
            kind,
            domain: DOMAIN.to_string(),
            agent_id: format!("agent_{}", task.unwrap_or(0)),
            execution_id: None,
            task_id: task,
            preset_id: preset.map(str::to_string),
            model_label: None,
            transcript_ref: None,
            attributed: ProvAttributionSource::Host,
            base: "git1:b".to_string(),
            result: format!("git1:{id}"),
            boundary: if kind == ProvChangeKind::Land {
                ProvBoundary::Land
            } else {
                ProvBoundary::Checkpoint
            },
            at_ms: id * 1000,
        }
    }

    fn land(id: i64, task: i64, preset: &str) -> ProvChangeRecord {
        change(id, ProvChangeKind::Land, Some(task), Some(preset))
    }

    fn entry(change: i64, path: &str, origin: Option<i64>) -> ProvEntryRecord {
        ProvEntryRecord {
            change,
            path: path.to_string(),
            before: None,
            after: Some(format!("git1:{change}{path}")),
            size: Some(1),
            class: ProvEntryClass::Content,
            origin,
        }
    }

    fn edge(id: i64, src: &ProvChangeRecord, detail: Value) -> ProvSupersedes {
        ProvSupersedes {
            edge: ProvEdgeRecord {
                id,
                kind: ProvEdgeKind::Supersedes,
                src: src.id,
                dst: "c0".to_string(),
                detail,
                at_ms: src.at_ms,
            },
            src: src.clone(),
        }
    }

    fn defect(target: i64, evidence: &str) -> Value {
        json!({"reason": "defect", "evidence": evidence, "target_task": target})
    }

    fn row<'a>(report: &'a Value, preset: &str) -> &'a Value {
        report["presets"]
            .as_array()
            .expect("presets")
            .iter()
            .find(|row| row["preset_id"] == preset)
            .unwrap_or_else(|| panic!("no row for {preset}: {report}"))
    }

    /// Rules 1 to 4 and 6: the task is the unit, charged to the preset of
    /// its landing; at most one liability per task; only level 1 and 2
    /// count, `fixes:` is reported apart, non-defect reasons and unverified
    /// reverts never count; a task's own edge is no liability.
    #[test]
    fn only_confirmed_defects_count_once_per_task_on_the_preset_of_its_landing() {
        let l11 = land(1, 11, "opus");
        let l12 = land(2, 12, "sonnet");
        let l13 = land(3, 13, "sonnet");
        let l20 = land(4, 20, "opus");
        let l21 = land(5, 21, "haiku");
        let l22 = land(6, 22, "haiku");
        let mut input = Input {
            lands: vec![
                l11.clone(),
                l12.clone(),
                l13.clone(),
                l20.clone(),
                l21.clone(),
                l22.clone(),
            ],
            ..Input::default()
        };
        input
            .entries
            .insert(1, vec![entry(1, "a.rs", None), entry(1, "b.rs", None)]);
        input.entries.insert(2, vec![entry(2, "c.rs", None)]);
        input.entries.insert(3, vec![entry(3, "d.rs", None)]);
        input.supersedes = vec![
            // Task 11: reverted (level 1) twice and bisected once: one.
            edge(1, &l20, defect(11, "level1_revert")),
            edge(2, &l21, defect(11, "level1_revert")),
            edge(3, &l22, defect(11, "level2_bisect")),
            // Task 12: a level-2 defect, and a fixes: reported apart.
            edge(4, &l20, defect(12, "level2_bisect")),
            edge(5, &l21, defect(12, "dispatcher_fixes")),
            // Task 13: fixes only, a refactor, an unverified revert.
            edge(6, &l20, defect(13, "dispatcher_fixes")),
            edge(
                7,
                &l21,
                json!({"reason": "refactor", "evidence": "dispatcher_supersedes", "target_task": 13}),
            ),
            edge(8, &l22, defect(13, "unverified_revert")),
            // A task's own: nothing.
            edge(9, &l22, defect(22, "level1_revert")),
        ];
        let report = tally(&input, Window::default());
        assert_eq!(report["shadow"], true);
        assert_eq!(report["window"], json!({"landings": 50, "days": 14}));
        let opus = row(&report, "opus");
        assert_eq!(
            (
                &opus["landed_tasks"],
                &opus["confirmed_defects"],
                &opus["reverted_tasks"],
                &opus["fixes_reported"]
            ),
            (&json!(2), &json!(1), &json!(1), &json!(0)),
            "{report}"
        );
        let sonnet = row(&report, "sonnet");
        assert_eq!(
            (
                &sonnet["landed_tasks"],
                &sonnet["confirmed_defects"],
                &sonnet["reverted_tasks"],
                &sonnet["fixes_reported"]
            ),
            (&json!(2), &json!(1), &json!(0), &json!(2)),
            "{report}"
        );
        let haiku = row(&report, "haiku");
        assert_eq!(haiku["confirmed_defects"], 0, "{report}");
        assert_eq!(report["unattributed"], 0);
        assert_eq!(
            report["not_scored"],
            json!({"refactor": 1, "unverified_revert": 1, "same_task": 1})
        );
        assert_eq!(report["defects"].as_array().map(Vec::len), Some(4));
        // No score of lines, files or Changes, and no survival.
        for row in report["presets"].as_array().expect("rows") {
            let mut keys: Vec<&str> = row
                .as_object()
                .expect("a row")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "confirmed_defects",
                    "fixes_reported",
                    "landed_tasks",
                    "preset_id",
                    "reverted_tasks"
                ]
            );
        }
        assert!(text(&report).contains("shadow only"));
    }

    /// D4: 50 landings or 14 days after the task's landing, whichever ends
    /// first; both are flags.
    #[test]
    fn the_window_is_fifty_landings_or_fourteen_days_whichever_ends_first() {
        let day = 86_400_000;
        let target = land(1, 11, "opus");
        let mut lands = vec![target.clone()];
        for id in 2..=60 {
            lands.push(land(id, 100 + id, "other"));
        }
        let late = {
            let mut late = land(3, 103, "other");
            late.at_ms = target.at_ms + 15 * day;
            late
        };
        lands[2] = late.clone();
        let mut input = Input {
            lands: lands.clone(),
            ..Input::default()
        };
        input.entries.insert(1, vec![entry(1, "a.rs", None)]);
        // The 51st landing after it: out; the 15th day: out.
        input.supersedes = vec![
            edge(1, &lands[51], defect(11, "level1_revert")),
            edge(2, &late, defect(11, "level1_revert")),
        ];
        let report = tally(&input, Window::default());
        assert_eq!(row(&report, "opus")["confirmed_defects"], 0, "{report}");
        assert_eq!(report["not_scored"], json!({"out_of_window": 2}));
        // The 50th landing, on day 1: in.
        input.supersedes = vec![edge(1, &lands[50], defect(11, "level1_revert"))];
        let report = tally(&input, Window::default());
        assert_eq!(row(&report, "opus")["confirmed_defects"], 1, "{report}");
        // A narrower window by flag.
        let report = tally(
            &input,
            Window {
                landings: 10,
                days: 14,
            },
        );
        assert_eq!(row(&report, "opus")["confirmed_defects"], 0, "{report}");
        let report = tally(
            &input,
            Window {
                landings: 100,
                days: 20,
            },
        );
        assert_eq!(row(&report, "opus")["confirmed_defects"], 1);
        let mut later = input;
        later.supersedes = vec![edge(2, &late, defect(11, "level1_revert"))];
        let report = tally(
            &later,
            Window {
                landings: 50,
                days: 20,
            },
        );
        assert_eq!(row(&report, "opus")["confirmed_defects"], 1, "{report}");
    }

    /// Rule 5: a composed landing's liability follows `origin` to the task
    /// whose revision the content came from; when the paths name more than
    /// one task (or a revision with no task), nobody is charged and the
    /// event is unattributed.
    #[test]
    fn a_composed_landing_is_charged_through_origin_or_to_nobody() {
        let edit_31 = change(10, ProvChangeKind::Edit, Some(31), Some("sonnet"));
        let edit_32 = change(11, ProvChangeKind::Edit, Some(32), Some("haiku"));
        let edit_none = change(12, ProvChangeKind::Edit, None, None);
        let composed = land(20, 30, "opus");
        let mixed = land(21, 33, "opus");
        let orphan = land(22, 34, "opus");
        let fixer = land(23, 40, "opus");
        let mut input = Input {
            lands: vec![
                composed.clone(),
                mixed.clone(),
                orphan.clone(),
                fixer.clone(),
            ],
            ..Input::default()
        };
        for change in [&edit_31, &edit_32, &edit_none] {
            input.origins.insert(change.id, change.clone());
        }
        // Task 30 composed task 31's revision alone: charged to 31, whose
        // preset its revision captured (31 never landed itself).
        input.entries.insert(20, vec![entry(20, "x.rs", Some(10))]);
        // Task 33 holds 31's and 32's: ambiguous.
        input.entries.insert(
            21,
            vec![entry(21, "x.rs", Some(10)), entry(21, "y.rs", Some(11))],
        );
        // Task 34 holds a revision with no task: ambiguous.
        input.entries.insert(22, vec![entry(22, "z.rs", Some(12))]);
        input.supersedes = vec![
            edge(1, &fixer, defect(30, "level1_revert")),
            edge(2, &fixer, defect(33, "level1_revert")),
            edge(3, &fixer, defect(34, "level2_bisect")),
        ];
        let report = tally(&input, Window::default());
        assert_eq!(report["unattributed"], 2, "{report}");
        let defects = report["defects"].as_array().expect("defects");
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0]["charged_task"], 31);
        assert_eq!(defects[0]["preset_id"], "sonnet");
        assert_eq!(row(&report, "sonnet")["confirmed_defects"], 1);
        assert_eq!(
            row(&report, "opus")["confirmed_defects"],
            0,
            "nobody else is charged"
        );
        assert_eq!(row(&report, "opus")["landed_tasks"], 4);
    }
}
