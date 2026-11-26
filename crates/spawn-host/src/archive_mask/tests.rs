//! The collector's mask (objective 175): every value made up, every home a
//! fake one in scratch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use spawn_core::port::ArchiveMask;

use super::{CollectorMask, INSTALLED_TOOL, SANDBOX_ONLY, Tool, lines, unquote};
use crate::Scratch;
use crate::home::Home;

/// A stand-in tool: it reads `spec.json` beside it and prints it as the
/// collector would, each source's stamp (and each watched directory's
/// mtime, each probed path's existence) read now, from the descriptor it
/// read the source through.
const TOOL: &str = r#"
import json, os
here = os.path.dirname(os.path.abspath(__file__))
spec = json.load(open(os.path.join(here, "spec.json")))
out = {"sources": [], "entries": spec.get("entries", []), "missing": spec.get("missing", []),
       "unreadable": spec.get("unreadable", []), "accepted": [],
       "accepted_paths": spec.get("accepted_paths", []), "watch": [], "probe": []}
for path in spec.get("sources", []):
    with open(path, "rb") as f:
        st = os.fstat(f.fileno())
        f.read()
    source = {"path": path, "size": st.st_size, "mtime_ns": st.st_mtime_ns, "dev": st.st_dev,
              "ino": st.st_ino, "ctime_ns": st.st_ctime_ns}
    if spec.get("old"):
        del source["ctime_ns"]
    out["sources"].append(source)
for path in spec.get("watch", []):
    out["watch"].append({"path": path, "mtime_ns": os.stat(path).st_mtime_ns if os.path.exists(path) else None})
for path in spec.get("probe", []):
    out["probe"].append({"path": path, "exists": os.path.exists(path)})
with open(os.path.join(here, "runs"), "a") as f:
    f.write("run\n")
print(json.dumps(out))
"#;

struct World {
    scratch: Scratch,
}

impl World {
    fn new(name: &str) -> Self {
        let world = Self {
            scratch: Scratch::new(name),
        };
        std::fs::write(world.tool(), TOOL).expect("tool");
        world
    }

    fn tool(&self) -> PathBuf {
        self.scratch.path().join("locations.py")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.scratch.path().join(name)
    }

    fn spec(&self, spec: &Value) {
        std::fs::write(self.path("spec.json"), spec.to_string()).expect("spec");
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.path("runs")).map_or(0, |text| text.lines().count())
    }

    fn mask(&self) -> CollectorMask {
        CollectorMask::with_tool(self.tool())
    }
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// `text` masked by the mask `source` gives.
fn masked(source: &CollectorMask, text: &str) -> String {
    let mask = source.mask().expect("the mask");
    mask.text(text)
}

const TOKEN: &str = "made-up/token+0123456789abcdef";
const LINKED: &str = "made-up/linked=4242424242";
const LEAF: &str = "made-up-json-leaf-5555666677";

/// Each kind of place is read and masked: a raw span on a line (the file
/// split at a lone CR), a percent-encoded span decoded then cut by `sub`, a
/// JSON leaf by its sorted-key pointer, and a short password's pair shape.
#[test]
fn each_kind_of_place_is_read_and_masked() {
    let world = World::new("mask-places");
    let cred = world.path("spawn.env");
    let encoded = LINKED.replace('/', "%2F").replace('=', "%3D");
    std::fs::write(
        &cred,
        format!(
            "NOTE=x\rAPI_TOKEN={TOKEN}\nLINK=https://h/?k={encoded}\nDB=postgres://bb:mUp5x@h/d\n"
        ),
    )
    .expect("cred");
    let doc = world.path("creds.json");
    std::fs::write(
        &doc,
        json!({"zeta": {"token": LEAF}, "alpha": 1}).to_string(),
    )
    .expect("doc");
    let link_line = format!("LINK=https://h/?k={encoded}");
    let at = link_line.find("?k=").expect("k") + 3;
    world.spec(&json!({
        "sources": [shown(&cred), shown(&doc)],
        "entries": [
            {"path": shown(&cred), "line": 1, "ptr": null, "t": "raw",
             "span": [10, 10 + TOKEN.len()], "name": "spawn.env:API_TOKEN"},
            {"path": shown(&cred), "line": 2, "ptr": null, "t": "pct",
             "span": [at, link_line.len()], "sub": [0, LINKED.len()], "name": "spawn.env:LINK"},
            {"path": shown(&cred), "line": 3, "ptr": null, "t": "short", "form": "pair",
             "pre": "", "post": "@", "k": 0,
             "user": {"t": "raw", "span": [14, 16]}, "pw": {"t": "raw", "span": [17, 22]},
             "name": "spawn.env:DB"},
            {"path": shown(&doc), "line": null, "ptr": [1, 0], "t": "raw",
             "span": [0, LEAF.len()], "name": "creds.json:token"},
        ],
    }));
    let source = world.mask();
    let text = format!(
        "the token is {TOKEN}; pct {}; linked {LINKED}; leaf {LEAF}; db postgres://bb:mUp5x@127.0.0.1/d",
        TOKEN.replace('/', "%2F").replace('+', "%2B")
    );
    let said = masked(&source, &text);
    for marker in [
        "spawn.env:API_TOKEN",
        "spawn.env:LINK",
        "spawn.env:DB",
        "creds.json:token",
    ] {
        assert!(
            said.contains(&format!("[REDACTED:{marker}]")),
            "{marker}: {said}"
        );
    }
    assert!(
        !said.contains("made-up") && !said.contains("mUp5x"),
        "{said}"
    );
}

/// A place that does not hold is an error naming the source, never a
/// value: no such line, a span past its text, a leaf that is not a string,
/// a pointer that names nothing, a source listed unreadable or missing, an
/// older collector's stamp, a source with no stamp.
#[test]
fn a_place_that_does_not_hold_is_an_error_naming_the_source_only() {
    let world = World::new("mask-errors");
    let cred = world.path("spawn.env");
    std::fs::write(&cred, format!("API_TOKEN={TOKEN}\n")).expect("cred");
    let doc = world.path("creds.json");
    std::fs::write(&doc, json!({"a": {"b": 7}}).to_string()).expect("doc");
    let entry = |extra: Value| {
        let mut entry = json!({"path": shown(&cred), "line": 0, "ptr": null, "t": "raw",
                               "span": [10, 10 + TOKEN.len()], "name": "spawn.env:API_TOKEN"});
        for (key, value) in extra.as_object().expect("object") {
            entry[key] = value.clone();
        }
        entry
    };
    let cases = [
        json!({"sources": [shown(&cred)], "entries": [entry(json!({"line": 5}))]}),
        json!({"sources": [shown(&cred)], "entries": [entry(json!({"span": [10, 999]}))]}),
        json!({"sources": [shown(&doc)], "entries": [{"path": shown(&doc), "line": null, "ptr": [0, 0],
               "t": "raw", "span": [0, 1], "name": "doc"}]}),
        json!({"sources": [shown(&doc)], "entries": [{"path": shown(&doc), "line": null, "ptr": [3],
               "t": "raw", "span": [0, 1], "name": "doc"}]}),
        json!({"sources": [], "entries": [], "unreadable": [shown(&cred)]}),
        json!({"sources": [], "entries": [], "missing": [shown(&cred)]}),
        json!({"sources": [shown(&cred)], "entries": [entry(json!({}))], "old": true}),
        json!({"sources": [], "entries": [entry(json!({}))]}),
    ];
    for spec in cases {
        world.spec(&spec);
        let error = world
            .mask()
            .mask()
            .err()
            .unwrap_or_else(|| panic!("refused: {spec}"));
        assert!(!error.contains("made-up"), "{error}");
        assert!(
            error.contains(&shown(&cred)) || error.contains(&shown(&doc)),
            "names the source: {error}"
        );
    }
}

/// The mask is kept while nothing it was built from changes, and built
/// again (the tool run again) when a source's stamp changes (a rotation;
/// a same-size rewrite with its old mtime, whose ctime moved), an accepted
/// path appears, a watched directory's mtime changes, or a probed path
/// appears. A mask is current until then.
#[test]
fn the_mask_is_built_again_when_what_it_was_built_from_changes() {
    let world = World::new("mask-again");
    let cred_dir = world.path("cred");
    std::fs::create_dir(&cred_dir).expect("dir");
    let cred = cred_dir.join("spawn.env");
    std::fs::write(&cred, format!("API_TOKEN={TOKEN}\n")).expect("cred");
    let base = world.path("wsroot");
    std::fs::create_dir_all(base.join("a")).expect("base");
    let accepted = world.path("accepted.env");
    world.spec(&json!({
        "sources": [shown(&cred)],
        "entries": [{"path": shown(&cred), "line": 0, "ptr": null, "t": "raw",
                     "span": [10, 10 + TOKEN.len()], "name": "spawn.env:API_TOKEN"}],
        "accepted_paths": [shown(&accepted)],
        "watch": [shown(&cred_dir), shown(&base)],
        "probe": [shown(&base.join("a/.claude"))],
    }));
    let source = world.mask();
    let first = source.mask().expect("built");
    assert_eq!(world.runs(), 1);
    assert!(Arc::ptr_eq(&first, &source.mask().expect("kept")), "kept");
    assert_eq!(world.runs(), 1, "no run while nothing changed");
    assert!(source.current(&first));
    // `settles`: the answer built again no longer names what changed (an
    // accepted path the stand-in still lists as missing never settles).
    let again = |what: &str, settles: bool, change: &dyn Fn()| {
        let before = source.mask().expect("built");
        let runs = world.runs();
        change();
        assert!(!source.current(&before), "{what}: no longer current");
        let after = source.mask().expect("built again");
        assert!(!Arc::ptr_eq(&before, &after), "{what}: a new mask");
        assert_eq!(world.runs(), runs + 1, "{what}: the tool ran again");
        assert_eq!(source.current(&after), settles, "{what}");
    };
    // A same-size rewrite, its mtime set back: only the ctime moves.
    again("a same-size rewrite with its old mtime", true, &|| {
        let old = std::fs::metadata(&cred)
            .expect("stat")
            .modified()
            .expect("mtime");
        let rotated = TOKEN.replace('0', "9");
        std::fs::write(&cred, format!("API_TOKEN={rotated}\n")).expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&cred)
            .and_then(|file| file.set_modified(old))
            .expect("mtime set back");
    });
    again("an accepted path appearing", false, &|| {
        std::fs::write(&accepted, "X=1\n").expect("accepted");
    });
    std::fs::remove_file(&accepted).expect("gone again");
    let _ = source.mask().expect("built");
    again("a watched directory changing", true, &|| {
        std::fs::write(cred_dir.join("spawn.env.bak"), "old\n").expect("bak");
    });
    again("a probed path appearing", true, &|| {
        std::fs::create_dir(base.join("a/.claude")).expect(".claude");
    });
    // The rewritten value is masked by the mask built again.
    let said = masked(&source, &format!("say {}", TOKEN.replace('0', "9")));
    assert!(said.contains("[REDACTED:spawn.env:API_TOKEN]"), "{said}");
}

/// A source rewritten between the tool's answer and this read (the same
/// size, its mtime set back) is not the tool's file: refused, and the next
/// call runs the tool again.
#[test]
fn a_source_changed_after_the_answer_is_refused_and_read_again() {
    let world = World::new("mask-after");
    let cred = world.path("spawn.env");
    std::fs::write(&cred, format!("API_TOKEN={TOKEN}\n")).expect("cred");
    let rewrite = format!(
        "{}\nimport os\np = {:?}\nst = os.stat(p)\nopen(p, 'r+b').write(open(p, 'rb').read().replace(b'0', b'9'))\nos.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))\n",
        TOOL.replace("print(json.dumps(out))", "answer = json.dumps(out)"),
        shown(&cred)
    ) + "print(answer)\n";
    std::fs::write(world.tool(), rewrite).expect("tool");
    world.spec(&json!({
        "sources": [shown(&cred)],
        "entries": [{"path": shown(&cred), "line": 0, "ptr": null, "t": "raw",
                     "span": [10, 10 + TOKEN.len()], "name": "spawn.env:API_TOKEN"}],
    }));
    let source = world.mask();
    let error = source.mask().err().expect("refused");
    assert!(
        error.contains("is not the file the location tool read"),
        "{error}"
    );
    assert!(
        error.contains(&shown(&cred)) && !error.contains("made-up"),
        "{error}"
    );
    std::fs::write(world.tool(), TOOL).expect("tool");
    let said = masked(&source, &format!("say {}", TOKEN.replace('0', "9")));
    assert!(said.contains("[REDACTED:"), "{said}");
}

#[test]
fn lines_are_split_as_the_collector_splits_them() {
    assert_eq!(
        lines(b"a\r\nb\rc\nd\0e"),
        vec![b"a".as_slice(), b"b", b"c", b"d", b"e"]
    );
    assert_eq!(lines(b"x\n"), vec![b"x".as_slice(), b""]);
    assert_eq!(lines(b""), vec![b"".as_slice()]);
}

#[test]
fn percent_decoding_is_pythons() {
    assert_eq!(unquote(b"a%2Fb%2bc"), b"a/b+c");
    assert_eq!(unquote(b"100%"), b"100%");
    assert_eq!(unquote(b"%zz%4"), b"%zz%4");
    assert_eq!(unquote(b"%C3%A9"), "é".as_bytes());
    assert_eq!(unquote(b"%FF"), "\u{fffd}".as_bytes());
}

/// A home in scratch, not a tmpfs.
fn fake_home(world: &World) -> Home {
    let home = world.path("home");
    std::fs::create_dir_all(&home).expect("home");
    Home {
        path: Some(std::fs::canonicalize(&home).expect("canonical")),
        tmpfs: false,
    }
}

fn refused_with(mask: &CollectorMask) -> Option<String> {
    match &mask.tool {
        Tool::Refused(why) => Some(why.clone()),
        Tool::Run(_) | Tool::Collector(_) => None,
    }
}

/// R5: a stand-in is honoured only with SPAWN_GATE_SANDBOX=1, exactly;
/// otherwise no tool runs and the mask is refused, saying so. With none
/// named, the installed tool.
#[test]
fn a_stand_in_is_honoured_only_in_a_gates_sandbox() {
    let world = World::new("mask-sandbox");
    let home = fake_home(&world);
    let roots = [world.path("ws"), world.path("projects")];
    let tool = Some(world.tool());
    for sandbox in [None, Some(""), Some("0"), Some("yes"), Some("1 ")] {
        let (mask, stand_in) = CollectorMask::configured(tool.clone(), sandbox, &roots, &home);
        assert_eq!(stand_in, None, "{sandbox:?}");
        assert_eq!(
            mask.mask().err().as_deref(),
            Some(SANDBOX_ONLY),
            "{sandbox:?}"
        );
    }
    assert_eq!(world.runs(), 0, "no tool ran");
    let (mask, stand_in) = CollectorMask::configured(tool, Some("1"), &roots, &home);
    assert!(refused_with(&mask).is_none());
    let stand_in = stand_in.expect("accepted");
    assert_eq!(
        stand_in.said(),
        format!(
            "spawn: WARNING: the transcript archive is masked by the stand-in location tool {} \
             (SPAWN_GATE_SANDBOX=1), not the installed collector, for the roots {} and {}",
            world.tool().display(),
            roots[0].display(),
            roots[1].display()
        )
    );
    let (mask, stand_in) = CollectorMask::configured(None, Some("1"), &roots, &home);
    assert_eq!(stand_in, None);
    assert!(matches!(&mask.tool, Tool::Run(tool) if tool == Path::new(INSTALLED_TOOL)));
}

/// R6: whatever the marker says, a stand-in is refused for a root inside
/// the account's home (and not said accepted); accepted for roots outside
/// it, or when the home is a tmpfs; refused when the home is unknown; a
/// root is judged where its links lead.
#[test]
fn a_stand_in_is_refused_for_roots_inside_the_account_s_home_whatever_the_marker() {
    let world = World::new("mask-home");
    let home = fake_home(&world);
    let home_path = home.path.clone().expect("home");
    let live = home_path.join(".local/share/spawn-rsi/ws");
    let outside = [world.path("ws"), world.path("projects")];
    let tool = || Some(world.tool());
    let (mask, stand_in) = CollectorMask::configured(
        tool(),
        Some("1"),
        &[live.clone(), outside[1].clone()],
        &home,
    );
    assert_eq!(stand_in, None, "not said accepted");
    let why = mask.mask().err().expect("refused");
    assert!(why.contains("inside the account's home"), "{why}");
    assert!(why.contains(&live.display().to_string()), "{why}");
    let (_, stand_in) = CollectorMask::configured(tool(), Some("1"), &outside, &home);
    assert!(stand_in.is_some(), "outside the home");
    let tmpfs = Home {
        tmpfs: true,
        ..home.clone()
    };
    let (_, stand_in) =
        CollectorMask::configured(tool(), Some("1"), std::slice::from_ref(&live), &tmpfs);
    assert!(stand_in.is_some(), "a tmpfs home, a gate's sandbox");
    let unknown = Home {
        path: None,
        tmpfs: false,
    };
    let (_, stand_in) = CollectorMask::configured(tool(), Some("1"), &outside, &unknown);
    assert_eq!(stand_in, None, "an unknown home refuses");
    // A root outside the home by its name, inside it where its link leads.
    let link = world.path("innocent");
    std::os::unix::fs::symlink(&home_path, &link).expect("link");
    let (_, stand_in) = CollectorMask::configured(tool(), Some("1"), &[link.join("ws")], &home);
    assert_eq!(stand_in, None, "judged where its link leads");
}

/// The live loop's two roots, in the real account's home, refuse a stand-in
/// (unless that home is a tmpfs here: a gate's sandbox).
#[test]
fn the_live_loop_s_roots_refuse_a_stand_in() {
    let home = Home::of_account();
    let Some(path) = home.path.clone() else {
        return;
    };
    let roots = [
        path.join(".local/share/spawn-rsi/ws"),
        path.join(".claude/projects"),
    ];
    let world = World::new("mask-live");
    for root in &roots {
        let (_, stand_in) = CollectorMask::configured(
            Some(world.tool()),
            Some("1"),
            std::slice::from_ref(root),
            &home,
        );
        assert_eq!(stand_in.is_some(), home.tmpfs, "{}", root.display());
    }
}

/// Issue #102: when no stand-in is given, resolution prefers installed
/// `emit-locations.py`, then installed `secret-values.py` (`Collector`), then
/// `exe_dir`'s copies, falling back to `INSTALLED_TOOL`. Both `Collector` and
/// `preflight` work cleanly without printing values or needing `__pycache__`.
#[test]
fn configured_with_dirs_prefers_emitter_then_collector_then_exe_dir() {
    let world = World::new("mask-resolve");
    let home = fake_home(&world);
    let roots = [world.path("ws"), world.path("projects")];
    let installed = world.path("installed");
    let exe = world.path("exe");
    std::fs::create_dir_all(&installed).expect("installed dir");
    std::fs::create_dir_all(&exe).expect("exe dir");

    // 1. Neither directory has any script: falls back to installed/emit-locations.py
    let (mask, stand_in) =
        CollectorMask::configured_with_dirs(None, None, &roots, &home, &installed, Some(&exe));
    assert_eq!(stand_in, None);
    assert_eq!(mask.tool, Tool::Run(installed.join("emit-locations.py")));
    assert!(
        mask.preflight()
            .unwrap_err()
            .contains("missing or not a regular file")
    );

    // 2. Exe dir has secret-values.py only: selects Tool::Collector from exe_dir
    let exe_collector = exe.join("secret-values.py");
    std::fs::write(
        &exe_collector,
        "def locate(home, root='/'):\n    return {}\n",
    )
    .expect("exe collector");
    let (mask, _) =
        CollectorMask::configured_with_dirs(None, None, &roots, &home, &installed, Some(&exe));
    assert_eq!(mask.tool, Tool::Collector(exe_collector.clone()));
    mask.preflight().expect("exe collector preflight passes");

    // 3. Exe dir gains emit-locations.py: prefers emit-locations.py over secret-values.py
    let exe_emitter = exe.join("emit-locations.py");
    std::fs::write(&exe_emitter, "print('{}')\n").expect("exe emitter");
    let (mask, _) =
        CollectorMask::configured_with_dirs(None, None, &roots, &home, &installed, Some(&exe));
    assert_eq!(mask.tool, Tool::Run(exe_emitter));

    // 4. Installed dir has secret-values.py: wins over exe dir
    let inst_collector = installed.join("secret-values.py");
    std::fs::write(
        &inst_collector,
        "def locate(home, root='/'):\n    return {}\n",
    )
    .expect("inst collector");
    let (mask, _) =
        CollectorMask::configured_with_dirs(None, None, &roots, &home, &installed, Some(&exe));
    assert_eq!(mask.tool, Tool::Collector(inst_collector));

    // 5. Installed dir gains emit-locations.py: highest priority
    let inst_emitter = installed.join("emit-locations.py");
    std::fs::write(&inst_emitter, "print('{}')\n").expect("inst emitter");
    let (mask, _) =
        CollectorMask::configured_with_dirs(None, None, &roots, &home, &installed, Some(&exe));
    assert_eq!(mask.tool, Tool::Run(inst_emitter));
}

#[test]
fn with_collector_invokes_secret_values_locate_and_masks_values() {
    let world = World::new("mask-collector");
    let cred = world.path("cred.env");
    std::fs::write(&cred, format!("API_TOKEN={TOKEN}\n")).expect("cred");
    let collector = world.path("secret-values.py");
    let cred_json = serde_json::to_string(&cred.display().to_string()).expect("path json");
    let script = format!(
        r#"import os
def locate(home, root="/"):
    path = {cred_json}
    with open(path, "rb") as f:
        st = os.fstat(f.fileno())
        f.read()
    return {{
        "sources": [{{"path": path, "size": st.st_size, "mtime_ns": st.st_mtime_ns,
                      "dev": st.st_dev, "ino": st.st_ino, "ctime_ns": st.st_ctime_ns}}],
        "entries": [{{"path": path, "line": 0, "ptr": None, "t": "raw",
                      "span": [10, {end}], "name": "cred.env:API_TOKEN"}}],
        "missing": [],
        "unreadable": [],
        "accepted": [],
        "accepted_paths": [],
        "watch": [],
        "probe": [],
    }}
"#,
        end = 10 + TOKEN.len(),
    );
    std::fs::write(&collector, script).expect("collector module");
    let source = CollectorMask::with_collector(collector);
    source.preflight().expect("collector preflight");
    let out = masked(&source, &format!("header {TOKEN} footer"));
    assert!(out.contains("[REDACTED:cred.env:API_TOKEN]"), "{out}");
    assert!(!out.contains(TOKEN), "{out}");
}

#[test]
fn preflight_detects_syntax_errors_and_refused_stand_in() {
    let world = World::new("mask-preflight-err");
    let bad = world.path("broken.py");
    std::fs::write(&bad, "def broken(:\n").expect("broken script");
    let bad_mask = CollectorMask::with_tool(bad);
    assert!(bad_mask.preflight().unwrap_err().contains("syntax check"));

    let (refused, _) = CollectorMask::configured(
        Some(world.tool()),
        Some("0"),
        &[world.path("ws")],
        &fake_home(&world),
    );
    assert_eq!(refused.preflight().unwrap_err(), SANDBOX_ONLY);
}
