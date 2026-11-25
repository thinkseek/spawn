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
