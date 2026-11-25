//! The planning commands of provenance (PV3a, decisions B4): `revert
//! --plan`, `split --plan --paths` and `compose --plan`. Each prints the
//! exact tree its result would have and the commands that make it, and
//! executes nothing: no ref is written, and not one object either. What a
//! plan computes (the merges it tries, the trees it builds) is written in a
//! throwaway [`View`] of the repository, removed with it.
//!
//! A revert, a split or a compose is always carried out as an ordinary
//! candidate branch: a task's workspace runs the commands, the task is
//! reported, and the gate judges and lands it like any other work. There is
//! no second landing path.
//!
//! - `revert`: the tree `git revert --no-commit [-m 1] <result>` leaves on
//!   `onto` (main by default): the three-way merge of `onto` and the
//!   Change's base, from its result.
//! - `split --paths <path>[,<path>...]`: the Change in two. The first part
//!   is its base with the named paths (a file, or every path the Change
//!   touched under a directory) as its result holds them; the second is its
//!   result. The granularity is the file: a part of one file (`<path>:<line>`
//!   or `<path>:<from>-<to>`) is refused, for a binary file always and, in
//!   this version, for a text file too (decisions B4 and B7).
//! - `compose <change>...`: the Changes applied on `onto` in the order
//!   given, as `git cherry-pick --no-commit [-m 1]` applies them one after
//!   the other. Whole Changes only: a part of a file is refused as for a
//!   split, and a set of whole files is a split's to make first.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::index::{self, RawDiff};

/// What every plan says of how it is carried out.
pub(crate) const THROUGH_THE_GATE: &str = "Run the commands in a task's workspace and report \
     the task: the candidate is judged and landed by the gate like any other work. Nothing was \
     written: no ref, no object.";

/// The identity of the commits a [`View`] makes between a compose's steps,
/// which are removed with it.
const PLAN_IDENTITY: (&str, &str) = ("spawn-prov-plan", "plan@localhost");

/// Views made by this process, for a name of their own.
static VIEWS: AtomicU64 = AtomicU64::new(0);

/// A throwaway git directory that holds no object of its own and reads
/// every object of another (`objects/info/alternates`): whatever is written
/// through it (a merge's tree, a plan's index, a hook's `main`) stays in it,
/// and it is removed when dropped. The repository it reads is never written.
pub(crate) struct View {
    dir: PathBuf,
}

impl View {
    /// A view of the repository at `git_dir`.
    pub(crate) fn of(git_dir: &str) -> Result<Self, String> {
        let objects = output_of(
            crate::sources::controller_git().args([
                "--git-dir",
                git_dir,
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ]),
            None,
            "rev-parse",
        )?;
        let objects = String::from_utf8_lossy(&objects).trim().to_string();
        if objects.is_empty() {
            return Err(format!("{git_dir} names no objects directory"));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-prov-view-{}-{nanos:x}-{}",
            std::process::id(),
            VIEWS.fetch_add(1, Ordering::Relaxed)
        ));
        // Dropped (and so removed) from here on, whatever fails next.
        let view = Self { dir };
        output_of(
            crate::sources::controller_git()
                .args(["init", "--quiet", "--bare", "--template="])
                .arg(&view.dir),
            None,
            "init",
        )?;
        let info = view.dir.join("objects").join("info");
        std::fs::create_dir_all(&info)
            .map_err(|error| format!("create {}: {error}", info.display()))?;
        std::fs::write(info.join("alternates"), format!("{objects}\n"))
            .map_err(|error| format!("write the view's alternates: {error}"))?;
        Ok(view)
    }

    /// The view's git directory.
    pub(crate) fn path(&self) -> String {
        self.dir.to_string_lossy().to_string()
    }

    /// The index file a plan builds a tree in.
    fn index_file(&self) -> PathBuf {
        self.dir.join("plan-index")
    }

    /// git in the view, as it ended (any exit code).
    fn run(&self, arguments: &[&str], index: bool, input: Option<&[u8]>) -> Result<Output, String> {
        let mut command = crate::sources::controller_git();
        command.arg("--git-dir").arg(&self.dir).args(arguments);
        if index {
            command.env("GIT_INDEX_FILE", self.index_file());
        }
        let (name, email) = PLAN_IDENTITY;
        command
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email);
        run_with_input(&mut command, input)
    }

    /// git's stdout in the view, or its failure.
    pub(crate) fn git(&self, arguments: &[&str]) -> Result<Vec<u8>, String> {
        let output = self.run(arguments, false, None)?;
        checked(output, arguments.first().copied().unwrap_or_default())
    }

    /// git's stdout in the view with the plan's index, fed `input`.
    fn git_indexed(&self, arguments: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let output = self.run(arguments, true, input)?;
        checked(output, arguments.first().copied().unwrap_or_default())
    }

    /// git's stdout in the view as one trimmed line.
    fn line(&self, arguments: &[&str]) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.git(arguments)?)
            .trim()
            .to_string())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `command` run with `input` on its standard input (none: null).
fn run_with_input(command: &mut std::process::Command, input: Option<&[u8]>) -> Result<Output, String> {
    use std::io::Write as _;
    let Some(input) = input else {
        return command
            .output()
            .map_err(|error| format!("run git: {error}"));
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run git: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("git has no stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("run git: {error}"))?;
    let _ = writer.join();
    Ok(output)
}

fn output_of(
    command: &mut std::process::Command,
    input: Option<&[u8]>,
    name: &str,
) -> Result<Vec<u8>, String> {
    checked(run_with_input(command, input)?, name)
}

fn checked(output: Output, name: &str) -> Result<Vec<u8>, String> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// One Change as a plan applies it: its `result` commit and the commit its
/// diff is from (`base`, the result's first parent), and whether the result
/// is a merge (a landing: `-m 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pick {
    /// `change_id#rev`, as the answers name it.
    pub(crate) label: String,
    pub(crate) base: String,
    pub(crate) result: String,
    pub(crate) merge: bool,
}

impl Pick {
    /// `-m 1 ` for a merge, nothing otherwise.
    fn mainline(&self) -> &'static str {
        if self.merge { "-m 1 " } else { "" }
    }
}

/// What a three-way merge in a view came to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Merged {
    Clean(String),
    /// The paths in conflict.
    Conflict(Vec<String>),
}

/// The merge of `ours` and `theirs` from `base`, by `attributes`' merge
/// rules (what a work tree checked out there reads), as `git revert` and
/// `git cherry-pick` merge.
fn merge(
    view: &View,
    attributes: &str,
    base: &str,
    ours: &str,
    theirs: &str,
) -> Result<Merged, String> {
    let output = view.run(
        &[
            &format!("--attr-source={attributes}"),
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            &format!("--merge-base={base}"),
            ours,
            theirs,
        ],
        false,
        None,
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let tree = lines.next().unwrap_or_default().trim().to_string();
    match output.status.code() {
        Some(0) => Ok(Merged::Clean(tree)),
        Some(1) => Ok(Merged::Conflict(
            lines
                .take_while(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
        )),
        _ => Err(format!(
            "git merge-tree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The paths that differ from `from` to `to` (commits or trees), each with
/// its digests before and after (`null`: absent).
fn changed(view: &View, from: &str, to: &str) -> Result<Vec<Value>, String> {
    Ok(diffs(view, from, to)?
        .iter()
        .map(|diff| {
            json!({
                "path": index::stored_path(&diff.path),
                "before": diff.old.as_deref().map(index::git1),
                "after": diff.new.as_deref().map(index::git1),
            })
        })
        .collect())
}

fn diffs(view: &View, from: &str, to: &str) -> Result<Vec<RawDiff>, String> {
    index::parse_raw(&view.git(&["diff-tree", "-r", "-z", "--no-renames", from, to])?)
}

/// `text` quoted for a POSIX shell.
pub(crate) fn shell_quoted(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&byte))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A literal pathspec for `path` (no glob, no magic), quoted for a shell.
fn pathspec(path: &str) -> String {
    shell_quoted(&format!(":(literal){path}"))
}

/// The command that checks the index holds `tree`.
fn check_tree(tree: &str) -> String {
    format!("test \"$(git write-tree)\" = {tree}")
}

/// A path as `--paths` names it: a whole file or directory, or a part of
/// one file (`<path>:<line>` or `<path>:<from>-<to>`), which is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wanted {
    Whole(String),
    Part { path: String, lines: String },
}

/// The paths of a `--paths` value: comma-separated, each a [`Wanted`].
pub(crate) fn wanted_paths(text: &str) -> Vec<Wanted> {
    let lines = |part: &str| {
        let mut bounds = part.splitn(2, '-');
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        bounds.next().is_some_and(digits) && bounds.next().is_none_or(digits)
    };
    text.split(',')
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(|word| match word.rsplit_once(':') {
            Some((path, part)) if !path.is_empty() && lines(part) => Wanted::Part {
                path: path.to_string(),
                lines: part.to_string(),
            },
            _ => Wanted::Whole(word.trim_end_matches('/').to_string()),
        })
        .collect()
}

/// Whether `path` is binary in the diff from `base` to `result`, as git
/// decides it (`--numstat` counts no lines). A path the diff does not hold
/// is read in `result`, then in `base`.
fn binary(view: &View, base: &str, result: &str, path: &str) -> bool {
    let spec = format!(":(literal){path}");
    let numstat = view
        .git(&["diff", "--numstat", "--no-renames", base, result, "--", &spec])
        .unwrap_or_default();
    let numstat = String::from_utf8_lossy(&numstat);
    if let Some(line) = numstat.lines().next() {
        return line.starts_with("-\t-\t");
    }
    // Untouched: git's own test against the empty tree.
    for commit in [result, base] {
        let listing = view
            .git(&[
                "diff",
                "--numstat",
                index::EMPTY_TREE,
                commit,
                "--",
                &spec,
            ])
            .unwrap_or_default();
        if let Some(line) = String::from_utf8_lossy(&listing).lines().next() {
            return line.starts_with("-\t-\t");
        }
    }
    false
}

/// The refusal of a part of one file in a `plan` (`split` or `compose`):
/// the granularity is the file, for a binary file always and, in this
/// version, for a text file too. Logged on stderr, so the refused requests
/// can be counted (decisions B7: the hunk-level split is built after three
/// in 30 days).
pub(crate) fn refusal(view: &View, plan: &str, pick: &Pick, path: &str, lines: &str) -> Value {
    let binary = binary(view, &pick.base, &pick.result, path);
    eprintln!(
        "prov: refused a partial-file {plan} of {path}:{lines} in {} (granularity: file{})",
        pick.label,
        if binary { ", binary" } else { "" }
    );
    json!({
        "plan": plan,
        "refused": true,
        "granularity": "file",
        "change": pick.label,
        "path": path,
        "lines": lines,
        "binary": binary,
        "reason": if binary {
            "a binary file is split and composed by whole file only"
        } else {
            "a split or compose inside one text file is not built in this version (decisions B4, \
             B7): name whole files"
        },
    })
}

/// `revert --plan`: what reverting `pick` on `onto` leaves (module doc).
pub(crate) fn revert(view: &View, onto: &str, pick: &Pick) -> Result<Value, String> {
    let merged = merge(view, onto, &pick.result, onto, &pick.base)?;
    let mut commands = vec![
        format!("git switch --detach {onto}"),
        format!("git revert --no-commit {}{}", pick.mainline(), pick.result),
    ];
    Ok(match merged {
        Merged::Clean(tree) => {
            commands.push(check_tree(&tree));
            commands.push(format!(
                "git commit -m {}",
                shell_quoted(&format!("Revert {}", pick.label))
            ));
            json!({
                "plan": "revert",
                "change": pick.label,
                "onto": onto,
                "base": pick.base,
                "result": pick.result,
                "tree": tree,
                "conflicts": [],
                "paths": changed(view, onto, &tree)?,
                "commands": commands,
                "note": THROUGH_THE_GATE,
            })
        }
        Merged::Conflict(paths) => json!({
            "plan": "revert",
            "change": pick.label,
            "onto": onto,
            "base": pick.base,
            "result": pick.result,
            "tree": null,
            "conflicts": paths,
            "commands": commands,
            "note": "The revert conflicts on these paths: resolve them in the task's workspace \
                     before reporting it.",
        }),
    })
}

/// `split --plan --paths`: `pick` in two, the `wanted` paths first (module
/// doc). Every path must be whole ([`refusal`] for a part, before this).
pub(crate) fn split(view: &View, pick: &Pick, wanted: &[String]) -> Result<Value, String> {
    let all = diffs(view, &pick.base, &pick.result)?;
    let mut chosen: Vec<&RawDiff> = Vec::new();
    for path in wanted {
        let under = format!("{path}/");
        let mut found = false;
        for diff in &all {
            let name = diff.path.as_slice();
            if name == path.as_bytes() || name.starts_with(under.as_bytes()) {
                found = true;
                if !chosen.iter().any(|seen| seen.path == diff.path) {
                    chosen.push(diff);
                }
            }
        }
        if !found {
            return Err(format!("{} does not touch {path}", pick.label));
        }
    }
    if chosen.len() == all.len() {
        return Err(format!(
            "--paths names every path {} touched: there is nothing to split",
            pick.label
        ));
    }
    let zero = all
        .iter()
        .find_map(|diff| diff.old.as_ref().or(diff.new.as_ref()))
        .map_or_else(|| "0".repeat(40), |oid| "0".repeat(oid.len()));
    let mut input = Vec::new();
    for diff in &chosen {
        match &diff.new {
            Some(oid) => input.extend_from_slice(format!("{} {oid}\t", diff.new_mode).as_bytes()),
            None => input.extend_from_slice(format!("0 {zero}\t").as_bytes()),
        }
        input.extend_from_slice(&diff.path);
        input.push(0);
    }
    view.git_indexed(&["read-tree", &pick.base], None)?;
    view.git_indexed(&["update-index", "-z", "--index-info"], Some(&input))?;
    let first = String::from_utf8_lossy(&view.git_indexed(&["write-tree"], None)?)
        .trim()
        .to_string();
    let result_tree = view.line(&["rev-parse", &format!("{}^{{tree}}", pick.result)])?;
    let first_paths: Vec<String> = chosen
        .iter()
        .map(|diff| index::stored_path(&diff.path))
        .collect();
    let rest: Vec<String> = all
        .iter()
        .filter(|diff| !chosen.iter().any(|seen| seen.path == diff.path))
        .map(|diff| index::stored_path(&diff.path))
        .collect();
    let specs: Vec<String> = wanted.iter().map(|path| pathspec(path)).collect();
    Ok(json!({
        "plan": "split",
        "change": pick.label,
        "granularity": "file",
        "base": pick.base,
        "result": pick.result,
        "parts": [
            {
                "paths": first_paths,
                "tree": first,
                "commands": [
                    format!("git switch --detach {}", pick.base),
                    format!("git checkout --no-overlay {} -- {}", pick.result, specs.join(" ")),
                    check_tree(&first),
                    format!("git commit -m {}", shell_quoted(&format!("Split 1/2 of {}", pick.label))),
                ],
            },
            {
                "paths": rest,
                "tree": result_tree,
                "commands": [
                    format!("git checkout --no-overlay {} -- :/", pick.result),
                    check_tree(&result_tree),
                    format!("git commit -m {}", shell_quoted(&format!("Split 2/2 of {}", pick.label))),
                ],
            },
        ],
        "note": THROUGH_THE_GATE,
    }))
}

/// `compose --plan`: `picks` applied on `onto` in order (module doc).
pub(crate) fn compose(view: &View, onto: &str, picks: &[Pick]) -> Result<Value, String> {
    let mut commands = vec![format!("git switch --detach {onto}")];
    let mut current = onto.to_string();
    let mut steps = Vec::new();
    for (step, pick) in picks.iter().enumerate() {
        commands.push(format!(
            "git cherry-pick --no-commit {}{}",
            pick.mainline(),
            pick.result
        ));
        match merge(view, onto, &pick.base, &current, &pick.result)? {
            Merged::Clean(tree) => {
                steps.push(json!({"change": pick.label, "tree": tree}));
                if step + 1 == picks.len() {
                    commands.push(check_tree(&tree));
                    let labels: Vec<&str> = picks.iter().map(|pick| pick.label.as_str()).collect();
                    commands.push(format!(
                        "git commit -m {}",
                        shell_quoted(&format!("Compose {}", labels.join(", ")))
                    ));
                    return Ok(json!({
                        "plan": "compose",
                        "onto": onto,
                        "changes": labels,
                        "steps": steps,
                        "tree": tree,
                        "conflicts": [],
                        "paths": changed(view, onto, &tree)?,
                        "commands": commands,
                        "note": THROUGH_THE_GATE,
                    }));
                }
                current = view.line(&[
                    "commit-tree",
                    "--no-gpg-sign",
                    &tree,
                    "-p",
                    &current,
                    "-m",
                    "plan step",
                ])?;
            }
            Merged::Conflict(paths) => {
                return Ok(json!({
                    "plan": "compose",
                    "onto": onto,
                    "changes": picks.iter().map(|pick| pick.label.clone()).collect::<Vec<_>>(),
                    "steps": steps,
                    "tree": null,
                    "conflicted_at": pick.label,
                    "conflicts": paths,
                    "commands": commands,
                    "note": "The compose conflicts at this Change on these paths: resolve them \
                             in the task's workspace before reporting it.",
                }));
            }
        }
    }
    Err("compose names no Change".to_string())
}

/// Whether `path` is a directory this process can hold a view under.
#[cfg(test)]
pub(crate) fn exists(view: &View) -> bool {
    Path::new(&view.dir).exists()
}
