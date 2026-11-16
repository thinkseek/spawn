//! Files below a root, never through a link (objective 175 R7, R8): every
//! home a fake one in scratch, every value made up.

use std::path::{Path, PathBuf};

use spawn_core::id::{AgentId, WorkspaceId};
use spawn_core::port::{TerminalHost, TranscriptArchive};

use super::{Beneath, between_open_and_canonical};
use crate::home::Home;
use crate::{LocalTranscriptArchive, Scratch, TmuxHost};

const NATIVE: &str = "0b7c1d2e-0000-4000-8000-000000000001";

struct World {
    scratch: Scratch,
    home: Home,
}

impl World {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let home = scratch.path().join("home");
        std::fs::create_dir_all(home.join("secret")).expect("home");
        std::fs::write(home.join("secret/kept.jsonl"), b"the home's own\n").expect("home file");
        let home = Home {
            path: Some(std::fs::canonicalize(&home).expect("canonical")),
            tmpfs: false,
        };
        Self { scratch, home }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.scratch.path().join(name)
    }

    fn home(&self) -> PathBuf {
        self.home.path.clone().expect("home")
    }

    /// A workspace root outside the home, made.
    fn root(&self) -> PathBuf {
        let root = self.path("ws");
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    /// The home's directory holds nothing but what it held.
    fn home_untouched(&self) {
        let names: Vec<String> = std::fs::read_dir(self.home().join("secret"))
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["kept.jsonl"], "nothing written into the home");
    }
}

fn agent() -> AgentId {
    AgentId::new("agent_local").expect("agent")
}

/// Both archives over `root`: unpinned (the live loop's) and pinned (a
/// stand-in's).
fn archives(world: &World, root: &Path) -> Vec<(&'static str, LocalTranscriptArchive)> {
    vec![
        (
            "unpinned",
            LocalTranscriptArchive::for_loop(Beneath::new(root)),
        ),
        (
            "pinned",
            LocalTranscriptArchive::for_loop(Beneath::pinned(root, &world.home).expect("pinned")),
        ),
    ]
}

#[test]
fn an_archive_whose_directory_links_into_the_home_appends_nothing_there() {
    let world = World::new("beneath-archive-link");
    let root = world.root();
    std::os::unix::fs::symlink(world.home().join("secret"), root.join(".transcripts"))
        .expect("link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 0, b"made-up line\n")
            .expect_err(what)
            .0;
        assert!(error.contains("not followed"), "{what}: {error}");
        let error = archive.copies(&agent(), NATIVE).expect_err(what).0;
        assert!(error.contains("not followed"), "{what}: {error}");
    }
    world.home_untouched();
}

#[test]
fn an_agent_directory_or_a_copy_that_links_into_the_home_is_refused() {
    let world = World::new("beneath-agent-link");
    let root = world.root();
    let archive_dir = root.join(".transcripts");
    std::fs::create_dir_all(&archive_dir).expect("archive");
    std::os::unix::fs::symlink(world.home().join("secret"), archive_dir.join("agent_local"))
        .expect("link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 0, b"x\n")
            .expect_err(what)
            .0;
        assert!(error.contains("not followed"), "{what}: {error}");
    }
    std::fs::remove_file(archive_dir.join("agent_local")).expect("unlink");
    std::fs::create_dir(archive_dir.join("agent_local")).expect("agent");
    std::os::unix::fs::symlink(
        world.home().join("secret/kept.jsonl"),
        archive_dir.join(format!("agent_local/{NATIVE}.jsonl")),
    )
    .expect("copy link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 15, b"x\n")
            .expect_err(what)
            .0;
        assert!(error.contains("not followed"), "{what}: {error}");
        let error = archive.read(&agent(), NATIVE, 0, 0, 100).expect_err(what).0;
        assert!(error.contains("not followed"), "{what}: {error}");
    }
    world.home_untouched();
    assert_eq!(
        std::fs::read(world.home().join("secret/kept.jsonl")).expect("read"),
        b"the home's own\n"
    );
}

#[test]
fn a_magic_link_into_the_home_is_not_followed() {
    let world = World::new("beneath-magic");
    let root = world.root();
    let through_proc = PathBuf::from(format!(
        "/proc/self/root{}",
        world.home().join("secret").display()
    ));
    std::os::unix::fs::symlink(&through_proc, root.join(".transcripts")).expect("link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 0, b"x\n")
            .expect_err(what)
            .0;
        assert!(error.contains("not followed"), "{what}: {error}");
    }
    world.home_untouched();
}

#[test]
fn a_root_inside_the_home_is_refused_and_one_outside_is_pinned() {
    let world = World::new("beneath-pin-home");
    let inside = world.home().join("ws");
    std::fs::create_dir_all(&inside).expect("inside");
    let error = Beneath::pinned(&inside, &world.home).expect_err("inside the home");
    assert!(error.contains("inside the account's home"), "{error}");
    // Through a link from outside: judged where it leads.
    let link = world.path("innocent");
    std::os::unix::fs::symlink(&inside, &link).expect("link");
    assert!(Beneath::pinned(&link, &world.home).is_err());
    let outside = Beneath::pinned(world.root(), &world.home).expect("outside");
    assert!(outside.is_pinned());
    let tmpfs = Home {
        tmpfs: true,
        ..world.home.clone()
    };
    assert!(Beneath::pinned(&inside, &tmpfs).is_ok(), "a tmpfs home");
}

#[test]
fn a_root_swapped_for_a_link_after_it_was_pinned_is_refused() {
    let world = World::new("beneath-swap");
    let root = world.root();
    let pinned = Beneath::pinned(&root, &world.home).expect("pinned");
    std::fs::rename(&root, world.path("ws-away")).expect("away");
    std::os::unix::fs::symlink(world.home().join("secret"), &root).expect("link");
    let error = pinned
        .write_new(Path::new("x.jsonl"), b"x\n")
        .expect_err("swapped");
    assert!(error.contains("not followed"), "{error}");
    world.home_untouched();
}

#[test]
fn a_root_replaced_between_its_open_and_its_resolution_is_refused() {
    let world = World::new("beneath-between");
    let root = world.root();
    let away = world.path("ws-away");
    let elsewhere = world.path("ws-other");
    std::fs::create_dir(&elsewhere).expect("other");
    let (from, to, other) = (root.clone(), away.clone(), elsewhere.clone());
    between_open_and_canonical(
        &root,
        Box::new(move || {
            std::fs::rename(&from, &to).expect("away");
            std::fs::rename(&other, &from).expect("replaced");
        }),
    );
    let error = Beneath::pinned(&root, &world.home).expect_err("replaced");
    assert!(error.contains("replaced while it was pinned"), "{error}");
}

#[test]
fn a_missing_root_is_pinned_at_its_deepest_directory_and_walked_below_it() {
    let world = World::new("beneath-missing");
    let root = world.path("projects/deeper");
    let pinned = Beneath::pinned(&root, &world.home).expect("pinned");
    assert_eq!(pinned.read(Path::new("p/n.jsonl")).expect("read"), None);
    assert!(
        pinned
            .write_new(Path::new("p/n.jsonl"), b"line\n")
            .expect("written")
    );
    assert_eq!(
        std::fs::read(root.join("p/n.jsonl")).expect("there"),
        b"line\n"
    );
    // Made below it later through a link: not followed.
    let other = world.path("projects2");
    std::os::unix::fs::symlink(world.home().join("secret"), world.path("projects2")).expect("link");
    let through = Beneath::pinned(other.join("p"), &world.home);
    assert!(through.is_err(), "its deepest directory is in the home");
}

#[test]
fn files_below_a_root_are_made_listed_moved_linked_and_removed() {
    let world = World::new("beneath-files");
    let below = Beneath::pinned(world.root(), &world.home).expect("pinned");
    let dir = below.make_dir(Path::new("a/b")).expect("made");
    below
        .replace_whole(&dir, "r.json", b"{}\n", true)
        .expect("replaced");
    below
        .replace_whole(&dir, "r.json", b"{\"n\":1}\n", false)
        .expect("again");
    assert_eq!(
        below.read(Path::new("a/b/r.json")).expect("read"),
        Some(b"{\"n\":1}\n".to_vec())
    );
    let (mut file, made) = below
        .open_append(&dir, "c.jsonl", true)
        .expect("open")
        .expect("made");
    assert!(made);
    std::io::Write::write_all(&mut file, b"one\n").expect("write");
    assert!(
        below
            .open_append(&dir, "none.jsonl", false)
            .expect("open")
            .is_none()
    );
    let mut listed = below.list(Path::new("a/b")).expect("list");
    listed.sort();
    assert_eq!(
        listed,
        [
            ("c.jsonl".to_string(), false),
            ("r.json".to_string(), false)
        ]
    );
    assert_eq!(
        below.list(Path::new("a")).expect("list"),
        [("b".to_string(), true)]
    );
    assert!(
        below
            .write_new(Path::new("a/b/n.jsonl"), b"new\n")
            .expect("new")
    );
    assert!(
        !below
            .write_new(Path::new("a/b/n.jsonl"), b"other\n")
            .expect("there")
    );
    assert!(
        below
            .replace_grown(Path::new("a/b/n.jsonl"), b"new\nmore\n", b"new\n")
            .expect("grown")
    );
    assert!(
        !below
            .replace_grown(Path::new("a/b/n.jsonl"), b"x\n\n\n\n\n\n\n\n\n\n", b"new\n")
            .expect("not it")
    );
    assert!(
        below
            .replace_grown(Path::new("a/b/n.jsonl"), b"n\n", b"new\nmore\n")
            .is_err(),
        "never shorter"
    );
    assert!(
        below.read(Path::new("../x")).is_err() && below.read(Path::new("/etc/hostname")).is_err()
    );
    // No temporary file is left behind.
    assert!(
        std::fs::read_dir(world.root().join("a/b"))
            .expect("list")
            .all(|entry| !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with('.'))
    );
}

#[test]
fn a_link_that_stays_below_the_root_is_refused_too() {
    let world = World::new("beneath-relative");
    let root = world.root();
    std::fs::create_dir_all(root.join("inside")).expect("inside");
    std::fs::write(root.join("inside/copy.jsonl"), b"same\n").expect("copy");
    std::fs::create_dir_all(root.join(".transcripts/agent_local")).expect("agent");
    std::os::unix::fs::symlink(
        "../../inside/copy.jsonl",
        root.join(format!(".transcripts/agent_local/{NATIVE}.jsonl")),
    )
    .expect("link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 5, b"more\n")
            .expect_err(what)
            .0;
        assert!(error.contains("not followed"), "{what}: {error}");
    }
    assert_eq!(
        std::fs::read(root.join("inside/copy.jsonl")).expect("read"),
        b"same\n"
    );
}

#[test]
fn a_copy_hard_linked_to_a_file_elsewhere_is_never_written_or_read() {
    let world = World::new("beneath-hard");
    let root = world.root();
    std::fs::create_dir_all(root.join(".transcripts/agent_local")).expect("agent");
    let outside = world.path("outside.jsonl");
    std::fs::write(&outside, b"kept\n").expect("outside");
    std::fs::hard_link(
        &outside,
        root.join(format!(".transcripts/agent_local/{NATIVE}.jsonl")),
    )
    .expect("hard link");
    for (what, archive) in archives(&world, &root) {
        let error = archive
            .append(&agent(), NATIVE, 0, 5, b"more\n")
            .expect_err(what)
            .0;
        assert!(
            error.contains("hard links: not followed"),
            "{what}: {error}"
        );
        let error = archive.read(&agent(), NATIVE, 0, 0, 10).expect_err(what).0;
        assert!(
            error.contains("hard links: not followed"),
            "{what}: {error}"
        );
    }
    assert_eq!(std::fs::read(&outside).expect("read"), b"kept\n");
}

/// A projects root pinned in a terminal host, and a workspace for it.
fn host_over(world: &World, projects: &Path) -> (TmuxHost, WorkspaceId) {
    let host = TmuxHost::local("tmux")
        .with_files_beneath(Beneath::pinned(projects, &world.home).expect("pinned"));
    let workspace = WorkspaceId::new(format!("w:{}", world.root().display())).expect("workspace");
    (host, workspace)
}

#[test]
fn a_transcript_hard_linked_to_a_file_elsewhere_is_not_read() {
    let world = World::new("beneath-transcript-hard");
    let projects = world.path("projects");
    std::fs::create_dir_all(projects.join("-w")).expect("project");
    let transcript = projects.join("-w/n.jsonl");
    std::fs::write(&transcript, b"line\n").expect("transcript");
    let (host, workspace) = host_over(&world, &projects);
    let path = transcript.display().to_string();
    assert_eq!(
        host.read_from(&workspace, &path, 0).expect("read"),
        Some(b"line\n".to_vec())
    );
    std::fs::hard_link(&transcript, world.path("second-name")).expect("hard link");
    let error = host.read_from(&workspace, &path, 0).expect_err("refused").0;
    assert!(error.contains("hard links: not followed"), "{error}");
}

#[test]
fn a_project_directory_that_links_into_the_home_is_not_read_restored_or_replaced() {
    let world = World::new("beneath-project-link");
    let projects = world.path("projects");
    std::fs::create_dir_all(&projects).expect("projects");
    std::os::unix::fs::symlink(world.home().join("secret"), projects.join("-w")).expect("link");
    let (host, workspace) = host_over(&world, &projects);
    let path = projects.join("-w/kept.jsonl").display().to_string();
    let read = host.read_from(&workspace, &path, 0).expect_err("read").0;
    assert!(read.contains("not followed"), "{read}");
    let new = projects.join("-w/new.jsonl").display().to_string();
    assert!(host.write_new(&workspace, &new, b"restored\n").is_err());
    assert!(
        host.replace_grown(
            &workspace,
            &path,
            b"the home's own\nmore\n",
            b"the home's own\n"
        )
        .is_err()
    );
    world.home_untouched();
    assert_eq!(
        std::fs::read(world.home().join("secret/kept.jsonl")).expect("read"),
        b"the home's own\n"
    );
}

#[test]
fn a_stand_in_mask_pins_its_roots_and_the_installed_one_pins_none() {
    let world = World::new("beneath-pins");
    let root = world.root();
    let projects = world.path("projects");
    let stand_in = crate::archive_mask::StandIn {
        tool: world.path("locations.py"),
        roots: vec![root.clone(), projects.clone()],
    };
    let pinned = crate::archive_mask::loop_roots(Some(&stand_in), &root, &projects, &world.home)
        .expect("pinned");
    assert!(pinned.workspace.is_pinned());
    assert!(pinned.projects.as_ref().is_some_and(Beneath::is_pinned));
    let installed =
        crate::archive_mask::loop_roots(None, &root, &projects, &world.home).expect("plain");
    assert!(!installed.workspace.is_pinned());
    assert!(installed.projects.is_none());
}

#[test]
fn the_loop_s_archive_keeps_the_pin_of_its_workspace_root() {
    let world = World::new("beneath-loop-pin");
    let root = world.root();
    let archive =
        LocalTranscriptArchive::for_loop(Beneath::pinned(&root, &world.home).expect("pinned"));
    archive
        .append(&agent(), NATIVE, 0, 0, b"one\n")
        .expect("appended");
    std::fs::rename(&root, world.path("ws-away")).expect("away");
    std::fs::create_dir_all(world.home().join("secret/.transcripts")).expect("decoy");
    std::os::unix::fs::symlink(world.home().join("secret"), &root).expect("link");
    let error = archive
        .append(&agent(), NATIVE, 0, 4, b"two\n")
        .expect_err("swapped")
        .0;
    assert!(error.contains("not followed"), "{error}");
    assert!(
        std::fs::read_dir(world.home().join("secret/.transcripts"))
            .expect("list")
            .next()
            .is_none(),
        "nothing written through the swapped root"
    );
}

/// R8: the projects root inside a workspace: a native transcript below it
/// is read through the pinned projects root (a second hard link refused, a
/// plain one read), a workspace file outside it still by the workspace's
/// reader, and a projects root swapped after the pin refused.
#[test]
fn a_native_transcript_inside_the_workspace_is_read_through_the_pinned_projects_root() {
    let world = World::new("beneath-r8");
    let workspace_dir = world.root().join("agent_one");
    let projects = workspace_dir.join("projects");
    std::fs::create_dir_all(projects.join("-w")).expect("projects");
    let transcript = projects.join("-w/n.jsonl");
    std::fs::write(&transcript, b"line\n").expect("transcript");
    std::fs::write(workspace_dir.join("status"), b"0\n").expect("status");
    let host = TmuxHost::local("tmux")
        .with_files_beneath(Beneath::pinned(&projects, &world.home).expect("pinned"));
    let workspace = WorkspaceId::new(format!("w:{}", workspace_dir.display())).expect("workspace");
    let path = transcript.display().to_string();
    assert_eq!(
        host.read_from(&workspace, &path, 0).expect("read"),
        Some(b"line\n".to_vec())
    );
    let status = workspace_dir.join("status").display().to_string();
    assert_eq!(
        host.read_from(&workspace, &status, 0).expect("read"),
        Some(b"0\n".to_vec())
    );
    std::fs::hard_link(&transcript, world.path("outside-native")).expect("hard link");
    let error = host.read_from(&workspace, &path, 0).expect_err("refused").0;
    assert!(error.contains("hard links: not followed"), "{error}");
    std::fs::remove_file(world.path("outside-native")).expect("unlink");
    std::fs::rename(&projects, workspace_dir.join("projects-away")).expect("away");
    std::os::unix::fs::symlink(workspace_dir.join("projects-away"), &projects).expect("link");
    let error = host.read_from(&workspace, &path, 0).expect_err("swapped").0;
    assert!(error.contains("not followed"), "{error}");
}
