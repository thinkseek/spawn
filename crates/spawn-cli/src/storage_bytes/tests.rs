use super::{
    BOUND_SECS, DAY_MS, Flight, RETRY_MS, Sampler, Source, World, live_database, quiet_hold,
    run_script, script_input, script_sources, session_backups, source_hold, sources,
    test_databases,
};
use crate::core_lines_sample::Reading;
use crate::serve::core_lines::StorageBytesSettings;
use crate::storage::tests::{file, scratch};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

const MIB: u64 = 1_048_576;

/// The sources a reading's thread would give, the live database aside.
fn answer() -> Map<String, Value> {
    let mut sources = Map::new();
    sources.insert(
        "transcripts".into(),
        json!({"bytes": 512, "files": 2, "unreadable": 0, "seconds": 0.1}),
    );
    sources.insert(
        "build_cache".into(),
        json!({"bytes": MIB, "files": 1, "unreadable": 0, "seconds": 0.1}),
    );
    sources.insert("scratch".into(), json!({"not_measured": "no root to read"}));
    sources.insert(
        "session_backups".into(),
        json!({"not_measured": "no status file is configured"}),
    );
    sources.insert("test_databases".into(), json!({"bytes": 0, "databases": 0}));
    sources
}

/// A made-up world: the newest reading's time and the database's clock,
/// the holds, the settings, and what a started reading does.
struct Fake {
    newest_at: Option<i64>,
    db_now: i64,
    readings_error: Option<String>,
    holds: Result<Value, String>,
    settings: StorageBytesSettings,
    /// A started reading waits for this before it answers (none: at once).
    slow: Option<Receiver<()>>,
    started: usize,
    readings_read: usize,
    holds_read: usize,
    stops: Vec<Arc<AtomicBool>>,
    written: Vec<Value>,
}

impl Fake {
    fn new(newest_at: Option<i64>, db_now: i64) -> Self {
        Self {
            newest_at,
            db_now,
            readings_error: None,
            holds: Ok(json!({"now_ms": db_now, "holds": []})),
            settings: StorageBytesSettings::default(),
            slow: None,
            started: 0,
            readings_read: 0,
            holds_read: 0,
            stops: Vec::new(),
            written: Vec::new(),
        }
    }
}

impl World for Fake {
    fn readings(&mut self) -> Result<Value, String> {
        self.readings_read += 1;
        if let Some(error) = &self.readings_error {
            return Err(error.clone());
        }
        Ok(json!({"now_ms": self.db_now,
                  "newest": self.newest_at.map(|at| json!({"at_ms": at, "sources": {}}))}))
    }

    fn holds(&mut self) -> Result<Value, String> {
        self.holds_read += 1;
        self.holds.clone()
    }

    fn settings(&mut self) -> Result<StorageBytesSettings, String> {
        Ok(self.settings.clone())
    }

    fn start(&mut self, _settings: &StorageBytesSettings) -> Flight {
        self.started += 1;
        let stop = Arc::new(AtomicBool::new(false));
        self.stops.push(Arc::clone(&stop));
        let reading = match self.slow.take() {
            Some(wait) => Reading::spawn(move || {
                let _ = wait.recv();
                Ok(answer())
            }),
            None => Reading::done(Ok(answer())),
        };
        Flight { reading, stop }
    }

    fn live_database(&mut self) -> Value {
        json!({"bytes": 300})
    }

    fn write(&mut self, record: &Value) -> Result<i64, String> {
        self.written.push(record.clone());
        if let Some(at) = record.get("type").and(Some(self.db_now)) {
            self.newest_at = Some(at);
        }
        Ok(1)
    }
}

/// Passes from `now` until the reading in flight is written or 2 s pass.
fn until_written(
    sampler: &mut Sampler,
    world: &mut Fake,
    now: &mut i64,
) -> Result<Option<String>, String> {
    for _ in 0..200 {
        let line = sampler.check_with(*now, world);
        if !world.written.is_empty() || line.is_err() {
            return line;
        }
        std::thread::sleep(Duration::from_millis(10));
        *now += 10;
    }
    Ok(None)
}

#[test]
fn a_reading_waits_for_every_owner_hold() {
    let dir = scratch("quiet");
    let quiet = dir.join("owner-gate.state");
    let held = |settings: StorageBytesSettings, holds: Value| {
        let mut world = Fake::new(None, 10 * DAY_MS);
        world.settings = settings;
        world.holds = Ok(holds);
        let mut sampler = Sampler::default();
        let line = sampler.check_with(1000, &mut world);
        (line, world.started)
    };
    let quiet_set = StorageBytesSettings {
        quiet_hold_file: Some(quiet.clone()),
        ..StorageBytesSettings::default()
    };
    let none = json!({"holds": []});
    // The quiet-hold file set but not there, then not saying ENDED.
    for text in [None, Some("IN PROGRESS fd at 10:00\nENDED\n"), Some("")] {
        if let Some(text) = text {
            std::fs::write(&quiet, text).expect("the quiet-hold file");
        }
        let (line, started) = held(quiet_set.clone(), none.clone());
        assert_eq!(started, 0, "{text:?}");
        let line = line.expect("a line, not an error").expect("said");
        assert!(
            line.starts_with("storage bytes: not measured (owner gate): the quiet-hold file"),
            "{line}"
        );
    }
    // ENDED on its first line: no hold from the file.
    std::fs::write(&quiet, "ENDED fd at 10:05\n").expect("the quiet-hold file");
    assert_eq!(quiet_hold(Some(&quiet)), None);
    assert_eq!(held(quiet_set.clone(), none.clone()).1, 1);
    // No file set: no such check.
    assert_eq!(quiet_hold(None), None);
    assert_eq!(held(StorageBytesSettings::default(), none.clone()).1, 1);
    // Any owner's hold on any source.
    let holds = json!({"now_ms": 1, "holds": [{"source": "spawn", "hold": 3, "expires_ms": 9}]});
    assert_eq!(
        source_hold(&holds).as_deref(),
        Some("the owner holds spawn")
    );
    let (line, started) = held(quiet_set, holds);
    assert_eq!(started, 0);
    assert!(
        line.expect("a line")
            .expect("said")
            .contains("(owner gate): the owner holds spawn")
    );

    // Held: nothing is read again for ten minutes, then it looks again.
    let mut world = Fake::new(None, 10 * DAY_MS);
    world.holds = Ok(json!({"holds": [{"source": "spawn"}]}));
    let mut sampler = Sampler::default();
    assert!(sampler.check_with(0, &mut world).is_ok());
    assert_eq!(sampler.check_with(RETRY_MS - 1, &mut world), Ok(None));
    assert_eq!(world.readings_read, 1);
    world.holds = Ok(json!({"holds": []}));
    assert!(sampler.check_with(RETRY_MS, &mut world).is_ok());
    assert_eq!(world.started, 1);
    assert!(world.written.len() <= 1);

    // A hold that begins while the script runs stops it; nothing is recorded.
    let (release, wait) = std::sync::mpsc::channel();
    let mut world = Fake::new(None, 10 * DAY_MS);
    world.slow = Some(wait);
    let mut sampler = Sampler::default();
    assert!(sampler.check_with(0, &mut world).is_ok());
    assert_eq!(world.started, 1);
    world.holds = Ok(json!({"holds": [{"source": "spawn"}]}));
    let stopped = sampler.check_with(5_000, &mut world);
    assert!(
        stopped.as_ref().is_err_and(
            |line| line.contains("stopped the reading: (owner gate) the owner holds spawn")
        ),
        "{stopped:?}"
    );
    assert!(
        world.stops[0].load(Ordering::SeqCst),
        "its script told to stop"
    );
    let _ = release.send(());
    assert!(world.written.is_empty());
}

#[test]
fn the_scripts_input_lists_the_sources_in_their_counting_order() {
    let settings = StorageBytesSettings {
        scratch_roots: vec![PathBuf::from("/data/tmp")],
        scratch_exclude: vec![PathBuf::from("/data/tmp/ccd-pg")],
        ..StorageBytesSettings::default()
    };
    let all = sources(
        Some(Path::new("/ws")),
        PathBuf::from("/home/a/.claude/projects"),
        Some(PathBuf::from("/home/a/.cache")),
        &settings,
    );
    assert_eq!(
        script_input(&all, BOUND_SECS),
        json!({"bound_secs": 1200, "sources": [
            {"name": "transcripts", "roots": ["/ws/.transcripts", "/home/a/.claude/projects"], "exclude": []},
            {"name": "build_cache", "roots": ["/home/a/.cache"], "exclude": []},
            {"name": "scratch", "roots": ["/data/tmp"], "exclude": ["/data/tmp/ccd-pg"]},
        ]})
    );
    // No build cache and no scratch roots: neither is sent, each says why.
    let some = sources(
        None,
        PathBuf::from("/p"),
        None,
        &StorageBytesSettings::default(),
    );
    assert_eq!(
        script_input(&some, 7),
        json!({"bound_secs": 7, "sources": [{"name": "transcripts", "roots": ["/p"], "exclude": []}]})
    );
    let entries = script_sources(
        &some,
        &Ok(
            json!({"sources": [{"name": "transcripts", "bytes": 5, "files": 1, "unreadable": 0, "seconds": 0.5}]}),
        ),
    );
    assert_eq!(entries["transcripts"]["bytes"], 5);
    assert_eq!(
        entries["build_cache"],
        json!({"not_measured": "no root to read"})
    );
    assert_eq!(
        entries["scratch"],
        json!({"not_measured": "no root to read"})
    );
    assert_eq!(
        entries.keys().collect::<Vec<_>>(),
        ["build_cache", "scratch", "transcripts"],
        "the three, by name"
    );
}

#[test]
fn a_source_over_its_bound_or_unread_is_not_measured() {
    let rooted = |name: &'static str| Source {
        name,
        roots: vec![PathBuf::from("/r")],
        exclude: Vec::new(),
    };
    let three = [
        rooted("transcripts"),
        rooted("build_cache"),
        rooted("scratch"),
    ];
    let entries = script_sources(
        &three,
        &Ok(json!({"sources": [
            {"name": "transcripts", "bytes": 9, "files": 3, "unreadable": 1, "seconds": 0.2},
            {"name": "scratch", "over_bound": true, "files": 75, "seconds": 1200.4},
        ]})),
    );
    assert_eq!(
        entries["transcripts"],
        json!({"bytes": 9, "files": 3, "unreadable": 1, "seconds": 0.2})
    );
    assert_eq!(
        entries["scratch"],
        json!({"not_measured": "over its bound of 1200 s (75 files read)"}),
        "never a partial count"
    );
    assert!(entries["build_cache"]["not_measured"].is_string());
    assert!(entries["build_cache"].get("bytes").is_none());
    // The script failed: every source it reads is not measured, with why.
    let failed = script_sources(
        &three,
        &Err("stopped after its limit of 3600 s".to_string()),
    );
    for name in ["transcripts", "build_cache", "scratch"] {
        assert_eq!(
            failed[name],
            json!({"not_measured": "the extents script: stopped after its limit of 3600 s"})
        );
    }
    // The session backups: their status file's estimated bytes, or why not.
    assert!(session_backups(None)["not_measured"].is_string());
    let dir = scratch("backups-status");
    let status = dir.join("status.json");
    std::fs::write(&status, r#"{"estimated_bytes": 4096}"#).expect("status");
    assert_eq!(session_backups(Some(&status)), json!({"bytes": 4096}));
    std::fs::write(&status, "{").expect("status");
    assert!(session_backups(Some(&status))["not_measured"].is_string());
    // The live database.
    assert_eq!(
        live_database(Ok(json!({"bytes": 77}))),
        json!({"bytes": 77})
    );
    assert!(
        live_database(Err("down".to_string()))["not_measured"]
            .as_str()
            .is_some_and(|why| why.contains("down"))
    );
}

#[test]
fn the_test_databases_count_each_database_once() {
    let read = |url: &str| -> Result<Value, String> {
        match url {
            "u1" | "u1-again" => Ok(json!({"name": "spawn_test", "bytes": 100, "schemas": 3})),
            "u2" => Ok(json!({"name": "spawn_fleet_test", "bytes": 40, "schemas": 1})),
            _ => Err("could not connect".to_string()),
        }
    };
    let databases = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(role, url)| ((*role).to_string(), (*url).to_string()))
            .collect()
    };
    assert_eq!(
        test_databases(
            &databases(&[("test", "u1"), ("fleet_test", "u2"), ("gate", "u1-again")]),
            read
        ),
        json!({"bytes": 140, "databases": 2})
    );
    assert_eq!(
        test_databases(&[], read),
        json!({"bytes": 0, "databases": 0})
    );
    let unread = test_databases(
        &databases(&[("test", "u1"), ("fleet_test", "postgres://secret@x/y")]),
        read,
    );
    let why = unread["not_measured"].as_str().expect("not measured");
    assert!(
        why.contains("fleet_test") && why.contains("could not connect"),
        "{why}"
    );
    assert!(!why.contains("secret"), "never its URL: {why}");
}

/// A fake extents script under `dir` with `body`.
fn fake(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("a fake script");
    path
}

#[test]
fn the_script_is_run_bounded_and_stoppable() {
    let dir = scratch("fake-extents");
    let go = AtomicBool::new(false);
    let limit = Duration::from_mins(1);
    let answering = fake(
        &dir,
        "answer.py",
        "import json, sys\nconfig = json.load(sys.stdin)\nprint(json.dumps({\"sources\": [{\"name\": s[\"name\"], \"bytes\": 1} for s in config[\"sources\"]]}))\n",
    );
    let input =
        json!({"bound_secs": 5, "sources": [{"name": "scratch", "roots": ["/x"], "exclude": []}]});
    assert_eq!(
        run_script(&answering, &input, limit, &go),
        Ok(json!({"sources": [{"name": "scratch", "bytes": 1}]}))
    );
    let failing = fake(
        &dir,
        "fail.py",
        "import sys\nprint('the walk broke', file=sys.stderr)\nsys.exit(3)\n",
    );
    let failed = run_script(&failing, &input, limit, &go);
    assert!(
        failed
            .as_ref()
            .is_err_and(|why| why.contains("the walk broke")),
        "{failed:?}"
    );
    let chatty = fake(&dir, "chatty.py", "print('hello')\n");
    assert_eq!(
        run_script(&chatty, &input, limit, &go),
        Err("it gave no JSON answer".to_string())
    );
    // Its limit: stopped, and the pass's thread never waits past it.
    let sleeping = fake(&dir, "sleep.py", "import time\ntime.sleep(60)\n");
    let begun = std::time::Instant::now();
    let over = run_script(&sleeping, &input, Duration::from_millis(500), &go);
    assert!(
        over.as_ref().is_err_and(|why| why.contains("limit")),
        "{over:?}"
    );
    assert!(begun.elapsed() < Duration::from_secs(20));
    // A stop (a hold began): stopped at once.
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let setter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag.store(true, Ordering::SeqCst);
    });
    let begun = std::time::Instant::now();
    let stopped = run_script(&sleeping, &input, limit, &stop);
    let _ = setter.join();
    assert_eq!(stopped, Err("stopped by a hold".to_string()));
    assert!(begun.elapsed() < Duration::from_secs(20));
}

/// The real script, as the loop runs it.
fn real_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/storage-extents.py")
}

#[test]
fn the_extents_script_gives_no_partial_count_past_its_bound() {
    let dir = scratch("extents-bound");
    file(&dir.join("full/a.bin"), 64 * 1024);
    file(&dir.join("full/b/c.bin"), 64 * 1024);
    std::fs::create_dir_all(dir.join("empty")).expect("an empty root");
    let go = AtomicBool::new(false);
    let limit = Duration::from_mins(1);
    let source = |root: &Path| Source {
        name: "scratch",
        roots: vec![root.to_path_buf()],
        exclude: Vec::new(),
    };
    let full = [source(&dir.join("full"))];
    let over = run_script(
        &real_script(),
        &script_input(&full, 0)
            .as_object()
            .map(|object| {
                let mut object = object.clone();
                object.insert("bound_secs".into(), json!(-1));
                Value::Object(object)
            })
            .expect("an object"),
        limit,
        &go,
    )
    .expect("the script's answer");
    let entry = &over["sources"][0];
    assert_eq!(entry["over_bound"], true, "{over}");
    assert!(entry.get("bytes").is_none(), "no partial count: {over}");
    assert!(script_sources(&full, &Ok(over.clone()))["scratch"]["not_measured"].is_string());
    let empty = [source(&dir.join("empty"))];
    let read = run_script(
        &real_script(),
        &script_input(&empty, BOUND_SECS),
        limit,
        &go,
    )
    .expect("the answer");
    assert_eq!(read["sources"][0]["bytes"], 0, "{read}");
    assert_eq!(read["sources"][0]["files"], 0, "{read}");
    // Within its bound, the files are counted (each extent once).
    let read = run_script(&real_script(), &script_input(&full, BOUND_SECS), limit, &go)
        .expect("the answer");
    assert_eq!(read["sources"][0]["files"], 2, "{read}");
    assert!(
        !read.to_string().contains(&*dir.display().to_string()),
        "never a path: {read}"
    );
}

#[test]
fn the_extents_script_counts_a_directory_it_cannot_list() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("extents-unlisted");
    file(&dir.join("root/a.bin"), 4096);
    file(&dir.join("root/locked/b.bin"), 4096);
    let locked = dir.join("root/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let listable = std::fs::read_dir(&locked).is_ok();
    let go = AtomicBool::new(false);
    let sources = [
        Source {
            name: "scratch",
            roots: vec![dir.join("root")],
            exclude: Vec::new(),
        },
        Source {
            name: "build_cache",
            roots: vec![dir.join("absent")],
            exclude: Vec::new(),
        },
    ];
    let read = run_script(
        &real_script(),
        &script_input(&sources, BOUND_SECS),
        Duration::from_mins(1),
        &go,
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    let read = read.expect("the answer");
    if !listable {
        // Never measured low with no sign.
        assert_eq!(read["sources"][0]["unreadable"], 1, "{read}");
        assert_eq!(read["sources"][0]["files"], 1, "{read}");
    }
    // A root that does not exist counts nowhere.
    assert_eq!(
        read["sources"][1],
        json!({"name": "build_cache", "bytes": 0, "files": 0, "unreadable": 0,
               "seconds": read["sources"][1]["seconds"]})
    );
}

#[test]
fn a_reading_is_taken_daily_outside_every_hold() {
    let start = 5 * DAY_MS;
    // The newest reading is two days old: one is due at the first pass.
    let mut world = Fake::new(Some(start - 2 * DAY_MS), start);
    let mut sampler = Sampler::default();
    let mut now = 1_000;
    let begun = sampler.check_with(now, &mut world);
    assert!(
        begun
            .as_ref()
            .is_ok_and(|line| line.as_deref().is_some_and(|line| line.contains("reading"))),
        "{begun:?}"
    );
    let written = until_written(&mut sampler, &mut world, &mut now);
    assert!(
        written.as_ref().is_ok_and(|line| line
            .as_deref()
            .is_some_and(|line| line.starts_with("storage bytes: recorded"))),
        "{written:?}"
    );
    assert_eq!(world.written.len(), 1);
    let record = &world.written[0];
    assert_eq!(record["type"], "host.storage_bytes");
    assert_eq!(
        record["sources"]
            .as_object()
            .expect("sources")
            .keys()
            .collect::<Vec<_>>(),
        [
            "build_cache",
            "live_database",
            "scratch",
            "session_backups",
            "test_databases",
            "transcripts"
        ]
    );
    assert_eq!(record["sources"]["live_database"], json!({"bytes": 300}));
    // Not again within the day: no read of the store at all until then.
    let reads = world.readings_read;
    assert_eq!(sampler.check_with(now + DAY_MS - 1, &mut world), Ok(None));
    assert_eq!(world.readings_read, reads);
    // A loop started again within the day reads the newest and waits.
    let mut restarted = Sampler::default();
    world.db_now = start + 3_600_000;
    assert_eq!(restarted.check_with(10, &mut world), Ok(None));
    assert_eq!(world.started, 1, "no second reading");
    assert_eq!(
        restarted.check_with(10 + DAY_MS - 3_600_000 - 1, &mut world),
        Ok(None)
    );
    assert_eq!(
        world.readings_read,
        reads + 1,
        "looked once, then waits for the day's end"
    );
    // A day after the newest, by the database's clock: due again.
    world.db_now = start + DAY_MS;
    let again = restarted.check_with(10 + DAY_MS - 3_600_000, &mut world);
    assert!(again.is_ok_and(|line| line.is_some()));
    assert_eq!(world.started, 2);
    // During a hold, never: and nothing recorded.
    let mut held = Fake::new(None, start);
    held.holds = Ok(json!({"holds": [{"source": "spawn"}]}));
    let mut sampler = Sampler::default();
    for at in [0, RETRY_MS, 2 * RETRY_MS] {
        let line = sampler.check_with(at, &mut held);
        assert!(line.is_ok_and(|line| line.is_some_and(|line| line.contains("(owner gate)"))));
    }
    assert_eq!(held.started, 0);
    assert!(held.written.is_empty());
}

#[test]
fn a_reading_in_flight_stops_when_the_holds_cannot_be_read() {
    let (release, wait) = std::sync::mpsc::channel();
    let mut world = Fake::new(None, 10 * DAY_MS);
    world.slow = Some(wait);
    let mut sampler = Sampler::default();
    assert!(sampler.check_with(0, &mut world).is_ok());
    world.holds = Err("the store is down".to_string());
    let stopped = sampler.check_with(5_000, &mut world);
    assert_eq!(
        stopped,
        Err("storage bytes: stopped the reading: the holds could not be read: the store is down; trying again in 600 s".to_string())
    );
    assert!(
        world.stops[0].load(Ordering::SeqCst),
        "its script told to stop"
    );
    // The holds read again: no second reading beside the one still running.
    world.holds = Ok(json!({"holds": []}));
    assert_eq!(sampler.check_with(5_000 + RETRY_MS, &mut world), Ok(None));
    assert_eq!(world.started, 1);
    let _ = release.send(());
    let mut now = 5_000 + RETRY_MS;
    let mut started = false;
    for _ in 0..200 {
        now += 10;
        let _ = sampler.check_with(now, &mut world);
        if world.started == 2 {
            started = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(started, "a new reading once the old one ended");
    assert!(world.written.len() <= 1);
}

#[test]
fn a_store_that_cannot_be_read_is_said_once_per_retry() {
    let mut world = Fake::new(None, 10 * DAY_MS);
    world.readings_error = Some("connection reset".to_string());
    let mut sampler = Sampler::default();
    assert_eq!(
        sampler.check_with(0, &mut world),
        Err("storage bytes: the readings could not be read: connection reset; trying again in 600 s".to_string())
    );
    for at in [1, 1000, RETRY_MS - 1] {
        assert_eq!(sampler.check_with(at, &mut world), Ok(None));
    }
    assert_eq!(world.readings_read, 1);
    assert!(sampler.check_with(RETRY_MS, &mut world).is_err());
    assert_eq!(world.readings_read, 2);
    // The holds, the same way.
    world.readings_error = None;
    world.holds = Err("timeout".to_string());
    assert_eq!(
        sampler.check_with(2 * RETRY_MS, &mut world),
        Err(
            "storage bytes: the holds could not be read: timeout; trying again in 600 s"
                .to_string()
        )
    );
    assert_eq!(sampler.check_with(3 * RETRY_MS - 1, &mut world), Ok(None));
    assert_eq!(world.holds_read, 1);
    assert_eq!(world.started, 0);
}
