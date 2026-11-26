//! The transcript archive's credential mask (objective 175): built from the
//! places the value collector gives, never from a value handed over (values
//! may not pass through a file, a pipe or an argument, cc-astra A51).
//!
//! [`CollectorMask`] runs `python3 -I -B <tool>` (the installed
//! [`INSTALLED_TOOL`]; [`TOOL_LIMIT`]) and reads its JSON: per source a
//! path and the stamp of the descriptor the collector read it through
//! (`size`, `mtime_ns`, `dev`, `ino`, `ctime_ns`, every field required, so an
//! older collector's answer is refused); per value a path, a line (the file
//! split as the collector's read() splits it: `\r\n`, a lone `\r`, `\n` and
//! NUL) or a pointer of sorted-key and list indexes into a JSON source, a
//! byte span, a transform (`raw`; `pct`: the span percent-decoded as
//! Python's `urllib.parse.unquote` does, then `sub`; `short`: a short URL
//! password's shape by its `form`, `pair`, `b64` or `word`, from `user` and
//! `pw` places and the collector's literal `pre`/`post` text) and a name.
//! Each source is opened here, its fstat before and after the read compared
//! with the collector's stamp (a same-size rewrite with its mtime set back
//! moves its ctime), and every place read from it. A source missing or
//! unreadable, a tool that refuses or answers no JSON, a stamp that is not
//! the file's, or a place that is not there is an error naming the path,
//! never a value. Every rule of what is a credential stays in the
//! collector; the values go into the [`Mask`] in memory only.
//!
//! The mask is kept with the stamps it was built from. Each call stats every
//! source, every accepted-missing path (`accepted_paths`), every watched
//! directory (`watch`, by mtime) and every probed path (`probe`, by
//! existence), and runs the tool again when one differs, the last run is
//! [`REBUILD_AFTER`] old, or the last call failed. [`ArchiveMask::current`]
//! is true only for the mask kept now while none of these changed.
//!
//! A stand-in tool (`SPAWN_ARCHIVE_LOCATIONS_TOOL`) is honoured only in a
//! gate's sandbox (`SPAWN_GATE_SANDBOX=1`, exactly), and even there not when
//! a root it would mask for (the workspace root, the transcripts directory)
//! is inside the account's home from the password database, unless that
//! home is a tmpfs here ([`crate::home`]): a root is judged where its links
//! lead. Refused, no tool runs and every mask is refused, so nothing is
//! copied. The guarantee covers live roots inside the account's home; a
//! launcher with root, or one that binds the live tree elsewhere, is
//! privileged tampering, which this does not stand against.

use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use spawn_core::mask::Mask;
use spawn_core::port::ArchiveMask;
use spawn_core::transcripts::ARCHIVE_MARKER;

use crate::home::Home;

/// The default directory where the lead installs redaction helpers.
pub const INSTALLED_DIR: &str = "/usr/local/lib/spawn/redact";

/// The value collector's location tool, installed by the lead.
pub const INSTALLED_TOOL: &str = "/usr/local/lib/spawn/redact/emit-locations.py";

/// The standalone collector module used as a fallback when `emit-locations.py` is absent.
pub const INSTALLED_COLLECTOR: &str = "/usr/local/lib/spawn/redact/secret-values.py";

/// Inline loader invoked only when `secret-values.py` is present without `emit-locations.py`.
const COLLECTOR_LOADER: &str = "import importlib.util, json, os, sys\n\
spec = importlib.util.spec_from_file_location('secret_values', sys.argv[1])\n\
mod = importlib.util.module_from_spec(spec)\n\
spec.loader.exec_module(mod)\n\
print(json.dumps(mod.locate(os.path.expanduser('~'))))\n";

/// How long one run of the tool may take.
pub const TOOL_LIMIT: Duration = Duration::from_secs(30);

/// How old a kept mask may be before an archive attempt builds it again.
pub const REBUILD_AFTER: Duration = Duration::from_mins(10);

/// Why a named stand-in is refused outside a gate's sandbox.
pub const SANDBOX_ONLY: &str = "SPAWN_ARCHIVE_LOCATIONS_TOOL is honoured only in a gate's \
    sandbox (SPAWN_GATE_SANDBOX=1): no location tool is run";

/// A stand-in tool accepted, and the roots it masks for: said by the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandIn {
    pub tool: PathBuf,
    pub roots: Vec<PathBuf>,
}

impl StandIn {
    /// The loop's one line saying it (R6).
    #[must_use]
    pub fn said(&self) -> String {
        let roots: Vec<String> = self
            .roots
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        format!(
            "spawn: WARNING: the transcript archive is masked by the stand-in location tool {} \
             (SPAWN_GATE_SANDBOX=1), not the installed collector, for the roots {}",
            self.tool.display(),
            roots.join(" and ")
        )
    }
}

/// A source's stamp: the five fields of the descriptor it was read through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime_ns: i128,
    dev: u64,
    ino: u64,
    ctime_ns: i128,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            size: metadata.len(),
            mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
            dev: metadata.dev(),
            ino: metadata.ino(),
            ctime_ns: i128::from(metadata.ctime()) * 1_000_000_000
                + i128::from(metadata.ctime_nsec()),
        }
    }

    fn from_json(source: &Value) -> Option<Self> {
        let number = |name: &str| source.get(name).and_then(Value::as_i64).map(i128::from);
        Some(Self {
            size: source.get("size")?.as_u64()?,
            mtime_ns: number("mtime_ns")?,
            dev: source.get("dev")?.as_u64()?,
            ino: source.get("ino")?.as_u64()?,
            ctime_ns: number("ctime_ns")?,
        })
    }
}

fn mtime_ns(metadata: &std::fs::Metadata) -> i128 {
    i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

/// What a kept mask was built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Stamps {
    sources: Vec<(PathBuf, Stamp)>,
    accepted: Vec<PathBuf>,
    watch: Vec<(PathBuf, Option<i128>)>,
    probe: Vec<(PathBuf, bool)>,
}

impl Stamps {
    /// Whether anything it was built from changed: a source's stamp (by
    /// stat), an accepted path that exists, a watched directory's mtime, a
    /// probed path's existence.
    fn changed(&self) -> bool {
        self.sources.iter().any(|(path, stamp)| {
            std::fs::metadata(path).map_or(true, |metadata| Stamp::of(&metadata) != *stamp)
        }) || self
            .accepted
            .iter()
            .any(|path| path.symlink_metadata().is_ok())
            || self.watch.iter().any(|(path, mtime)| {
                std::fs::metadata(path)
                    .ok()
                    .map(|metadata| mtime_ns(&metadata))
                    != *mtime
            })
            || self
                .probe
                .iter()
                .any(|(path, exists)| path.exists() != *exists)
    }
}

/// The mask kept, with what it was built from and when.
struct Kept {
    mask: Arc<Mask>,
    stamps: Stamps,
    built: Instant,
}

/// Which tool runs, or why none does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tool {
    Run(PathBuf),
    Collector(PathBuf),
    Refused(String),
}

fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn resolve_installed_tool(installed_dir: &Path, exe_dir: Option<&Path>) -> Tool {
    for candidate_dir in std::iter::once(installed_dir).chain(exe_dir) {
        let emitter = candidate_dir.join("emit-locations.py");
        if emitter.is_file() {
            return Tool::Run(emitter);
        }
        let collector = candidate_dir.join("secret-values.py");
        if collector.is_file() {
            return Tool::Collector(collector);
        }
    }
    Tool::Run(installed_dir.join("emit-locations.py"))
}

/// The credential mask the value collector's places give (module docs).
pub struct CollectorMask {
    tool: Tool,
    kept: Mutex<Option<Kept>>,
}

impl CollectorMask {
    /// The loop's mask, for `roots` (the workspace root and the transcripts
    /// directory), as the environment configures it: the stand-in it
    /// accepted, if one, to be said and its roots pinned.
    #[must_use]
    pub fn from_env(roots: &[PathBuf]) -> (Self, Option<StandIn>) {
        let tool = std::env::var_os("SPAWN_ARCHIVE_LOCATIONS_TOOL")
            .filter(|tool| !tool.is_empty())
            .map(PathBuf::from);
        let sandbox = std::env::var("SPAWN_GATE_SANDBOX").ok();
        Self::configured(tool, sandbox.as_deref(), roots, &Home::of_account())
    }

    /// The mask as `tool` (`SPAWN_ARCHIVE_LOCATIONS_TOOL`) and `sandbox`
    /// (`SPAWN_GATE_SANDBOX`) configure it, for `roots`, against `home`.
    #[must_use]
    pub fn configured(
        tool: Option<PathBuf>,
        sandbox: Option<&str>,
        roots: &[PathBuf],
        home: &Home,
    ) -> (Self, Option<StandIn>) {
        Self::configured_with_dirs(
            tool,
            sandbox,
            roots,
            home,
            Path::new(INSTALLED_DIR),
            current_exe_dir().as_deref(),
        )
    }

    /// Like [`Self::configured`], with explicit `installed_dir` and `exe_dir`
    /// fallback search roots.
    #[must_use]
    pub fn configured_with_dirs(
        tool: Option<PathBuf>,
        sandbox: Option<&str>,
        roots: &[PathBuf],
        home: &Home,
        installed_dir: &Path,
        exe_dir: Option<&Path>,
    ) -> (Self, Option<StandIn>) {
        let Some(tool) = tool else {
            return (
                Self {
                    tool: resolve_installed_tool(installed_dir, exe_dir),
                    kept: Mutex::new(None),
                },
                None,
            );
        };
        if sandbox != Some("1") {
            return (Self::refused(SANDBOX_ONLY.to_string()), None);
        }
        for root in roots {
            if let Some(why) = home.refuses(&judged(root), root) {
                return (
                    Self::refused(format!(
                        "SPAWN_ARCHIVE_LOCATIONS_TOOL is refused for this loop: {why}; a \
                         stand-in serves only roots outside the account's home (or a tmpfs \
                         home, a gate's sandbox): no location tool is run"
                    )),
                    None,
                );
            }
        }
        let stand_in = StandIn {
            tool: tool.clone(),
            roots: roots.to_vec(),
        };
        (Self::with_tool(tool), Some(stand_in))
    }

    /// The mask `tool`'s places give.
    #[must_use]
    pub fn with_tool(tool: PathBuf) -> Self {
        Self {
            tool: Tool::Run(tool),
            kept: Mutex::new(None),
        }
    }

    /// The mask a standalone `secret-values.py` collector module gives by
    /// invoking `locate(os.path.expanduser("~"))` in-process.
    #[must_use]
    pub fn with_collector(collector: PathBuf) -> Self {
        Self {
            tool: Tool::Collector(collector),
            kept: Mutex::new(None),
        }
    }

    /// Verifies that the configured location tool is permitted, exists as a
    /// regular file, and parses cleanly under `python3 -I -B`.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic message when a stand-in was refused, the tool script
    /// is missing, or `python3` cannot parse the script.
    pub fn preflight(&self) -> Result<(), String> {
        let path = match &self.tool {
            Tool::Refused(why) => return Err(why.clone()),
            Tool::Run(path) | Tool::Collector(path) => path,
        };
        if !path.is_file() {
            return Err(format!(
                "archive mask location tool {} is missing or not a regular file",
                path.display()
            ));
        }
        let status = Command::new("python3")
            .args([
                "-I",
                "-B",
                "-c",
                "import ast, sys; ast.parse(open(sys.argv[1], 'rb').read(), filename=sys.argv[1])",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                format!(
                    "archive mask preflight could not run python3 on {}: {error}",
                    path.display()
                )
            })?;
        if !status.success() {
            return Err(format!(
                "archive mask location tool {} failed python3 syntax check ({status})",
                path.display()
            ));
        }
        Ok(())
    }

    /// A mask that is never had, saying `why`.
    fn refused(why: String) -> Self {
        Self {
            tool: Tool::Refused(why),
            kept: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Kept>> {
        self.kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `kept` may be used now.
    fn fresh(kept: &Kept) -> bool {
        kept.built.elapsed() < REBUILD_AFTER && !kept.stamps.changed()
    }
}

impl ArchiveMask for CollectorMask {
    fn mask(&self) -> Result<Arc<Mask>, String> {
        if let Tool::Refused(why) = &self.tool {
            return Err(why.clone());
        }
        let mut kept = self.lock();
        if let Some(held) = kept.as_ref()
            && Self::fresh(held)
        {
            return Ok(held.mask.clone());
        }
        // Stale, never built, or the last call failed: built again.
        *kept = None;
        let answer = run(&self.tool)?;
        let (values, stamps) = build(&answer)?;
        let mask = Arc::new(Mask::new(&values, ARCHIVE_MARKER));
        *kept = Some(Kept {
            mask: mask.clone(),
            stamps,
            built: Instant::now(),
        });
        Ok(mask)
    }

    fn current(&self, mask: &Arc<Mask>) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(|kept| Arc::ptr_eq(&kept.mask, mask) && Self::fresh(kept))
    }
}

/// The loop's roots as its files are reached ([`crate::beneath`]): the
/// workspace root, which the archive is reached from, and with a stand-in
/// the engines' projects root too, both pinned; with the installed tool
/// the workspace root is walked unpinned and the projects root is read as
/// before (the user's own links are theirs).
pub struct LoopRoots {
    pub workspace: crate::beneath::Beneath,
    pub projects: Option<crate::beneath::Beneath>,
}

/// [`LoopRoots`] for `stand_in` (the one [`CollectorMask::configured`]
/// accepted, if any).
///
/// # Errors
///
/// Why a stand-in's root may not be pinned: paths only.
pub fn loop_roots(
    stand_in: Option<&StandIn>,
    workspace_root: &Path,
    projects: &Path,
    home: &Home,
) -> Result<LoopRoots, String> {
    use crate::beneath::Beneath;
    match stand_in {
        None => Ok(LoopRoots {
            workspace: Beneath::new(workspace_root),
            projects: None,
        }),
        Some(_) => Ok(LoopRoots {
            workspace: Beneath::pinned(workspace_root, home)?,
            projects: Some(Beneath::pinned(projects, home)?),
        }),
    }
}

/// `root` where its links lead: its deepest existing directory resolved,
/// the rest of it as written.
fn judged(root: &Path) -> PathBuf {
    let mut base = root.to_path_buf();
    let mut rest = Vec::new();
    while !base.exists() {
        let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
            break;
        };
        rest.push(name.to_os_string());
        base = parent.to_path_buf();
    }
    let mut judged = std::fs::canonicalize(&base).unwrap_or(base);
    for name in rest.iter().rev() {
        judged.push(name);
    }
    judged
}

/// The tool's answer: `python3 -I -B <tool>` (or inline loader calling
/// `secret-values.py::locate`), no stdin, bounded by [`TOOL_LIMIT`].
/// Its stderr is never shown (only how it ended).
fn run(tool: &Tool) -> Result<Value, String> {
    let mut cmd = Command::new("python3");
    cmd.arg("-I").arg("-B");
    let shown = match tool {
        Tool::Run(path) => {
            cmd.arg(path);
            path.display().to_string()
        }
        Tool::Collector(path) => {
            cmd.arg("-c").arg(COLLECTOR_LOADER).arg(path);
            path.display().to_string()
        }
        Tool::Refused(why) => return Err(why.clone()),
    };
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("the location tool {shown} could not be run: {error}"))?;
    let mut stdout = child.stdout.take();
    let (send, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut bytes);
        }
        let _ = send.send(bytes);
    });
    let deadline = Instant::now() + TOOL_LIMIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the location tool {shown} ran past {} s",
                    TOOL_LIMIT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("the location tool {shown}: {error}")),
        }
    };
    let bytes = answer
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| format!("the location tool {shown}'s answer did not end in time"))?;
    if !status.success() {
        return Err(format!("the location tool {shown} exited {status}"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| format!("the location tool {shown} answered no JSON"))
}

/// The strings of the answer's list `name`, as paths.
fn paths(answer: &Value, name: &str) -> Vec<PathBuf> {
    answer[name]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().or_else(|| item["path"].as_str()))
        .map(PathBuf::from)
        .collect()
}

/// Named values, `(name, value)`.
type Values = Vec<(String, Vec<u8>)>;

/// The values the answer places and the stamps the mask is kept with.
fn build(answer: &Value) -> Result<(Values, Stamps), String> {
    if !answer.is_object() || !answer["sources"].is_array() || !answer["entries"].is_array() {
        return Err("the location tool's answer is not {sources, entries, ...}".to_string());
    }
    if let Some(path) = paths(answer, "unreadable").first() {
        return Err(format!(
            "the location tool could not read {}: nothing is copied until it can",
            path.display()
        ));
    }
    if let Some(path) = paths(answer, "missing").first() {
        return Err(format!(
            "{} is missing (the location tool expects it): nothing is copied until it is there",
            path.display()
        ));
    }
    let mut stamps = Stamps::default();
    for source in answer["sources"].as_array().into_iter().flatten() {
        let path = PathBuf::from(source["path"].as_str().unwrap_or_default());
        let stamp = Stamp::from_json(source).ok_or_else(|| {
            format!(
                "the location tool gave {} without its whole stamp (size, mtime_ns, dev, ino, \
                 ctime_ns): an older collector, refused",
                path.display()
            )
        })?;
        stamps.sources.push((path, stamp));
    }
    stamps.accepted = paths(answer, "accepted_paths");
    if let Some(relative) = stamps.accepted.iter().find(|path| !path.is_absolute()) {
        return Err(format!(
            "the location tool's accepted path {} is not absolute",
            relative.display()
        ));
    }
    for watched in answer["watch"].as_array().into_iter().flatten() {
        let path = PathBuf::from(watched["path"].as_str().unwrap_or_default());
        let mtime = watched["mtime_ns"].as_i64().map(i128::from);
        stamps.watch.push((path, mtime));
    }
    for probed in answer["probe"].as_array().into_iter().flatten() {
        let path = PathBuf::from(probed["path"].as_str().unwrap_or_default());
        stamps
            .probe
            .push((path, probed["exists"].as_bool().unwrap_or(false)));
    }
    let mut read: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut values = Vec::new();
    for entry in answer["entries"].as_array().into_iter().flatten() {
        let path = PathBuf::from(entry["path"].as_str().unwrap_or_default());
        let bytes = if let Some((_, bytes)) = read.iter().find(|(at, _)| *at == path) {
            bytes.clone()
        } else {
            let stamp = stamps
                .sources
                .iter()
                .find(|(at, _)| *at == path)
                .map(|(_, stamp)| *stamp)
                .ok_or_else(|| {
                    format!(
                        "the location tool places a value in {} but gives no stamp of it",
                        path.display()
                    )
                })?;
            let bytes = read_stamped(&path, stamp)?;
            read.push((path.clone(), bytes.clone()));
            bytes
        };
        let name = entry["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| format!("a place in {} has no name", path.display()))?;
        let text = place_text(&bytes, entry, &path)?;
        values.push((name.to_string(), value_at(&text, entry, &path)?));
    }
    Ok((values, stamps))
}

/// The file at `path`, read through a descriptor whose stamp before and
/// after the read is `stamp`.
fn read_stamped(path: &Path, stamp: Stamp) -> Result<Vec<u8>, String> {
    let shown = path.display();
    let not_it = || {
        format!(
            "{shown} is not the file the location tool read (its size, mtime, device, inode or \
             ctime differs): the mask is built again"
        )
    };
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("{shown}: cannot be read: {error}"))?;
    let before = file
        .metadata()
        .map_err(|error| format!("stat {shown}: {error}"))?;
    if Stamp::of(&before) != stamp {
        return Err(not_it());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("{shown}: cannot be read: {error}"))?;
    let after = file
        .metadata()
        .map_err(|error| format!("stat {shown}: {error}"))?;
    if Stamp::of(&after) != stamp {
        return Err(not_it());
    }
    Ok(bytes)
}

/// `bytes` split as the collector's read() splits a file: at `\r\n`, a
/// lone `\r`, `\n` and NUL.
#[must_use]
pub fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => {
                lines.push(&bytes[start..at]);
                at += 2;
                start = at;
            }
            b'\r' | b'\n' | 0 => {
                lines.push(&bytes[start..at]);
                at += 1;
                start = at;
            }
            _ => at += 1,
        }
    }
    lines.push(&bytes[start..]);
    lines
}

/// The text a place's span is in: its line, or the JSON leaf its pointer
/// names (object keys in sorted order, list indexes).
fn place_text(bytes: &[u8], entry: &Value, path: &Path) -> Result<Vec<u8>, String> {
    let shown = path.display();
    if let Some(number) = entry["line"].as_u64() {
        let all = lines(bytes);
        return usize::try_from(number)
            .ok()
            .and_then(|number| all.get(number))
            .map(|line| line.to_vec())
            .ok_or_else(|| format!("{shown} has no line {number} that the location tool places"));
    }
    let Some(pointer) = entry["ptr"].as_array() else {
        return Err(format!(
            "a place in {shown} names neither a line nor a pointer"
        ));
    };
    let document: Value = serde_json::from_slice(bytes)
        .map_err(|_| format!("{shown} is not the JSON the location tool read"))?;
    let mut at = &document;
    for step in pointer {
        let index = step
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| format!("a pointer into {shown} is not indexes"))?;
        at = match at {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                keys.get(index).and_then(|key| map.get(*key))
            }
            Value::Array(items) => items.get(index),
            _ => None,
        }
        .ok_or_else(|| format!("{shown} has no place at the pointer the location tool gives"))?;
    }
    at.as_str()
        .map(|leaf| leaf.as_bytes().to_vec())
        .ok_or_else(|| format!("the leaf the location tool places in {shown} is not a string"))
}

/// `text[span]`, the span `[start, end]` in `place`.
fn span<'a>(text: &'a [u8], place: &Value, path: &Path) -> Result<&'a [u8], String> {
    let bounds = place["span"]
        .as_array()
        .filter(|bounds| bounds.len() == 2)
        .and_then(|bounds| Some((bounds[0].as_u64()?, bounds[1].as_u64()?)))
        .and_then(|(start, end)| Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?)))
        .ok_or_else(|| format!("a place in {} has no span", path.display()))?;
    text.get(bounds.0..bounds.1)
        .ok_or_else(|| format!("a place in {} runs past its text", path.display()))
}

/// `text` percent-decoded as Python's `urllib.parse.unquote` does: each
/// `%XX` of two hex digits a byte, anything else as it is, the result read
/// as UTF-8 with each invalid sequence replaced.
#[must_use]
pub fn unquote(text: &[u8]) -> Vec<u8> {
    let hex = |byte: u8| char::from(byte).to_digit(16);
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        if text[at] == b'%'
            && let (Some(high), Some(low)) = (
                text.get(at + 1).copied().and_then(hex),
                text.get(at + 2).copied().and_then(hex),
            )
        {
            out.push(u8::try_from(high * 16 + low).unwrap_or(0));
            at += 3;
        } else {
            out.push(text[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned().into_bytes()
}

/// The value one place gives: raw, percent-decoded, or a short password's
/// shape.
fn value_at(text: &[u8], place: &Value, path: &Path) -> Result<Vec<u8>, String> {
    let shown = path.display();
    match place["t"].as_str().unwrap_or("raw") {
        "raw" => Ok(span(text, place, path)?.to_vec()),
        "pct" => {
            let decoded = unquote(span(text, place, path)?);
            match place.get("sub") {
                Some(sub) if !sub.is_null() => {
                    let inner = serde_json::json!({"span": sub});
                    Ok(span(&decoded, &inner, path)?.to_vec())
                }
                _ => Ok(decoded),
            }
        }
        "short" => {
            let user = value_at(text, &place["user"], path)?;
            let password = value_at(text, &place["pw"], path)?;
            let pre = place["pre"].as_str().unwrap_or("").as_bytes();
            let post = place["post"].as_str().unwrap_or("").as_bytes();
            let pair = [user.as_slice(), b":", &password].concat();
            let middle = match place["form"].as_str().unwrap_or("pair") {
                "pair" => pair,
                "b64" => base64(&pair),
                "word" => password,
                other => {
                    return Err(format!(
                        "a short place in {shown} has the form {other:?}, which is not known"
                    ));
                }
            };
            Ok([pre, middle.as_slice(), post].concat())
        }
        other => Err(format!(
            "a place in {shown} has the transform {other:?}, which is not known"
        )),
    }
}

/// `bytes` in standard base64, padded.
fn base64(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]);
            } else {
                out.push(b'=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
