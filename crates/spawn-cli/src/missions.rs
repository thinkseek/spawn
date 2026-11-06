//! `spawn mission`, `spawn task` and `spawn resident`: the mission layer's
//! commands (docs/fleet-architecture.md §3.2).
//!
//! Humans run them with the owner's login, the supervisor with its own; the
//! SQL functions behind them decide who may do what (only a human approves a
//! resident). Every command prints JSON.

use serde_json::{Value, json};
use spawn_core::event::ModelProfile;

use crate::{Flags, open_store};

const USAGE: &str = "usage:
  spawn mission add --text <mission> [--source <git repo path or URL>] [<settings>]
  spawn mission configure --id N <settings>
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--backlog <dir> --resident <name> [--workers N]]   (owner only)
                (--workers: objective tasks in flight at once, 1 to 16, default 1: the
                resident takes one, per-task workers the rest)
  spawn mission resume --id N
  spawn mission push --source <path> --commit <commit> [--repo <dir>] [--wait <seconds>]
      (owner: land a commit on a mission source's main. <commit> is resolved in the git
       repository of the current directory (--repo: another), or is already in the source;
       it is put in the source at refs/spawn-owner/<hash>, no branch moves, and the request
       is recorded (source.push_requested): prints {request, source, commit}. The running
       integrator applies it between its own steps, under the landing hold: main moves to
       the commit (a fast-forward) or to a merge of it, and one source.moved naming the
       request moves the baselines; a conflict is source.push_refused. Nothing halts, no
       gate is stopped, and spawn-integrate is never stopped for it. --wait: wait that long
       for the outcome and print it; exit 0 applied, 1 refused, 3 still pending)
  spawn mission moved --source <path> --from <commit> --to <commit>
      (owner: main was moved behind Spawn's back, forward; record it so the missions take
       the new main as their baseline instead of halting)
  spawn mission list [--limit N]   (every task with its stages)
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> --brief <text>
                    [--model default|cheap]   (cheap: every model on DeepSeek)
  spawn task accept --id N [--revision R]
  spawn task reject --id N [--revision R] --note <what to fix>
      (R: the report's revision as read; refused once the task has moved on)
  spawn task cancel --id N
      (a delivered task's resident is stopped at once, its work set aside on its branch,
       and it is told before its next task; an ephemeral worker is stopped)
  spawn task stages --id N   (each stage with its verdict, time, and how long since the one before)
  spawn resident propose --name <name> --charter <text>
  spawn resident approve --name <name>
  spawn resident retire --name <name>";

fn print(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn id(flags: &Flags, name: &str) -> Result<i64, String> {
    let text = flags
        .get(name)
        .ok_or_else(|| format!("--{name} is required\n{USAGE}"))?;
    text.parse()
        .map_err(|_| format!("--{name} must be a number, not {text}"))
}

fn text(flags: &Flags, name: &str) -> Result<String, String> {
    flags
        .get(name)
        .ok_or_else(|| format!("--{name} is required\n{USAGE}"))
}

/// The `--revision` of the report a review judged, if named: without it,
/// the report as it is now.
fn revision(flags: &Flags) -> Result<Option<i32>, String> {
    flags
        .get("revision")
        .map(|text| {
            text.parse()
                .map_err(|_| format!("--revision must be a number, not {text}"))
        })
        .transpose()
}

/// The `--model` profile a task runs on, `default` when unset.
fn model(flags: &Flags) -> Result<ModelProfile, String> {
    let Some(text) = flags.get("model") else {
        return Ok(ModelProfile::Default);
    };
    ModelProfile::parse(&text)
        .ok_or_else(|| format!("--model is default or cheap, not {text}\n{USAGE}"))
}

fn split(rest: &[String]) -> Result<(&str, Flags), String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    Ok((verb.as_str(), Flags::parse(rest)?))
}

/// An absolute path for a directory flag, resolved against the working
/// directory.
fn directory(flags: &Flags, name: &str) -> Result<Option<String>, String> {
    let Some(text) = flags.get(name) else {
        return Ok(None);
    };
    let path = std::path::absolute(&text).map_err(|error| format!("--{name}: {error}"))?;
    if !path.is_dir() {
        return Err(format!("--{name} {} is not a directory", path.display()));
    }
    Ok(Some(path.display().to_string()))
}

/// A mission's landing settings (docs/fleet-architecture.md §10.3-10.4).
struct Settings {
    gate: Option<String>,
    accept: String,
    budget: Option<Value>,
    objectives: Option<Value>,
}

impl Settings {
    fn from(flags: &Flags) -> Result<Self, String> {
        let number = |name: &str| -> Result<Option<i64>, String> {
            flags
                .get(name)
                .map(|text| {
                    text.parse()
                        .map_err(|_| format!("--{name} must be a number"))
                })
                .transpose()
        };
        let mut budget = serde_json::Map::new();
        if let Some(minutes) = number("minutes")? {
            budget.insert("minutes".to_string(), json!(minutes));
        }
        if let Some(calls) = number("tool-calls")? {
            budget.insert("tool_calls".to_string(), json!(calls));
        }
        let workers = number("workers")?;
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
                let mut objectives = json!({"backlog": backlog, "resident": resident});
                // Absent means 1, the default: a mission configured without
                // it stores exactly what it did before.
                if let (Some(workers), Some(map)) = (workers, objectives.as_object_mut()) {
                    map.insert("workers".to_string(), json!(workers));
                }
                Some(objectives)
            }
            (None, None) if workers.is_none() => None,
            (None, None) => {
                return Err(format!(
                    "--workers goes with --backlog and --resident\n{USAGE}"
                ));
            }
            _ => return Err(format!("--backlog and --resident go together\n{USAGE}")),
        };
        Ok(Self {
            gate: directory(flags, "gate")?,
            accept: flags.get("accept").unwrap_or_else(|| "review".to_string()),
            budget: (!budget.is_empty()).then_some(Value::Object(budget)),
            objectives,
        })
    }

    fn apply(&self, store: &spawn_postgres::PostgresStore, mission: i64) -> Result<(), String> {
        store
            .mission_configure(
                mission,
                self.gate.as_deref(),
                &self.accept,
                self.budget.as_ref(),
                self.objectives.as_ref(),
            )
            .map_err(|error| error.to_string())
    }
}

/// `spawn mission ...`
pub(crate) fn mission(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "add" => {
            // Checked here for a clear error; spawn_mission_add holds the
            // same rule for every caller.
            let sources = match flags.get("source") {
                Some(source) => json!([crate::sources::MissionSource::parse(&source)?.as_str()]),
                None => json!([]),
            };
            let settings = Settings::from(&flags)?;
            // spawn_mission_configure refuses it too, but only once the
            // mission exists.
            if settings.gate.is_some()
                && sources
                    .get(0)
                    .and_then(Value::as_str)
                    .is_some_and(|source| !source.starts_with('/'))
            {
                return Err(
                    "a gate lands only on a source that is a directory on this host; agents' \
                     branches never reach a remote one"
                        .to_string(),
                );
            }
            let id = store
                .mission_add(&text(&flags, "text")?, &sources, "human")
                .map_err(err)?;
            if settings.gate.is_some() {
                settings.apply(&store, id)?;
            }
            print(&json!({ "mission": id }));
        }
        "configure" => {
            let mission = id(&flags, "id")?;
            let settings = Settings::from(&flags)?;
            settings.apply(&store, mission)?;
            print(
                &json!({ "mission": mission, "gate": settings.gate, "accept": settings.accept,
                           "objectives": settings.objectives }),
            );
        }
        "resume" => {
            let mission = id(&flags, "id")?;
            store.mission_resume(mission).map_err(err)?;
            print(&json!({ "mission": mission, "resumed": true }));
        }
        "push" => return push(&store, &flags),
        "moved" => {
            let source = text(&flags, "source")?;
            crate::sources::MissionSource::parse(&source)?;
            let from = text(&flags, "from")?;
            let to = text(&flags, "to")?;
            // Forward only: a main that dropped landed work is a halt, not a deploy.
            let forward = crate::sources::controller_git()
                .args([
                    "--git-dir",
                    &source,
                    "merge-base",
                    "--is-ancestor",
                    &from,
                    &to,
                ])
                .status()
                .map_err(|error| format!("git: {error}"))?;
            if !forward.success() {
                return Err(format!("{to} does not descend from {from} in {source}"));
            }
            let missions = store.source_moved(&source, &from, &to).map_err(err)?;
            print(&json!({ "source": source, "to": to, "missions": missions }));
        }
        "list" => {
            let limit = flags
                .get("limit")
                .and_then(|text| text.parse().ok())
                .unwrap_or(20);
            print(&store.mission_board(limit).map_err(err)?);
        }
        "close" => {
            let mission = id(&flags, "id")?;
            store
                .mission_close(
                    mission,
                    &text(&flags, "state")?,
                    &flags.get("summary").unwrap_or_default(),
                )
                .map_err(err)?;
            print(&json!({ "mission": mission, "closed": true }));
        }
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// `spawn mission push`: put the owner's commit in the source at
/// `refs/spawn-owner/<hash>` and record the request the integrator applies
/// (docs/fleet-architecture.md §10.3, "The owner's landings"). Exit 0, or,
/// with `--wait`, 0 when applied, 1 when refused, 3 when still pending.
fn push(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i32, String> {
    let source = text(flags, "source")?;
    crate::sources::MissionSource::parse(&source)?;
    if !source.starts_with('/') {
        return Err(format!(
            "its source {source} is remote: agents' branches reach only a directory on this \
             host, so nothing lands on it"
        ));
    }
    let wanted = text(flags, "commit")?;
    let wait = flags
        .get("wait")
        .map(|text| {
            text.parse::<u64>()
                .map_err(|_| format!("--wait must be a number of seconds, not {text}"))
        })
        .transpose()?;
    let repo = match directory(flags, "repo")? {
        Some(repo) => repo,
        None => std::env::current_dir()
            .map_err(|error| format!("the current directory: {error}"))?
            .display()
            .to_string(),
    };
    let (commit, reference) = put_in_source(&source, &repo, &wanted)?;
    let request = store
        .source_push_request(&source, &commit, &reference)
        .map_err(|error| error.to_string())?;
    let mut answer = json!({ "request": request, "source": source, "commit": commit });
    let Some(seconds) = wait else {
        print(&answer);
        return Ok(0);
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let outcome = loop {
        let outcome = store
            .source_push_outcome(request)
            .map_err(|error| error.to_string())?;
        if outcome.is_some() || std::time::Instant::now() >= deadline {
            break outcome;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    let (state, code) = match outcome
        .as_ref()
        .and_then(|record| record.get("type"))
        .and_then(Value::as_str)
    {
        Some("source.moved") => ("applied", 0),
        Some(_) => ("refused", 1),
        None => ("pending", 3),
    };
    answer["state"] = json!(state);
    answer["outcome"] = outcome.unwrap_or(Value::Null);
    print(&answer);
    Ok(code)
}

/// Resolve `wanted` in the owner's repository `repo` (else in the source,
/// which may have it already) and put it in `source` at
/// `refs/spawn-owner/<hash>`, without touching any branch: `(hash, ref)`.
fn put_in_source(source: &str, repo: &str, wanted: &str) -> Result<(String, String), String> {
    use crate::integrate::owner::{full_hash, owner_ref};
    use crate::sources::controller_git;

    let resolve = |place: &[&str]| -> Option<String> {
        let output = controller_git()
            .args(place)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{wanted}^{{commit}}"),
            ])
            .output()
            .ok()?;
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && full_hash(&hash)).then_some(hash)
    };
    if let Some(commit) = resolve(&["-C", repo]) {
        let reference = owner_ref(&commit);
        let pushed = controller_git()
            .args([
                "-C",
                repo,
                "push",
                "--quiet",
                "--",
                source,
                &format!("{commit}:{reference}"),
            ])
            .output()
            .map_err(|error| format!("run git push: {error}"))?;
        if !pushed.status.success() {
            return Err(format!(
                "git push of {commit} to {source} ({reference}): {}",
                String::from_utf8_lossy(&pushed.stderr).trim()
            ));
        }
        return Ok((commit, reference));
    }
    let Some(commit) = resolve(&["--git-dir", source]) else {
        return Err(format!(
            "{wanted} is not a commit in {repo} nor in the source {source}"
        ));
    };
    let reference = owner_ref(&commit);
    crate::sources::git(&["--git-dir", source, "update-ref", &reference, &commit])
        .map_err(|error| error.to_string())?;
    Ok((commit, reference))
}

/// `spawn task ...`
pub(crate) fn task(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "assign" => {
            let model = model(&flags)?;
            let task = store
                .task_assign(
                    id(&flags, "mission")?,
                    &text(&flags, "kind")?,
                    &flags.get("worker").unwrap_or_default(),
                    &text(&flags, "brief")?,
                    model.as_str(),
                )
                .map_err(err)?;
            print(&json!({ "task": task, "model": model.as_str() }));
        }
        "accept" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, true, "", revision(&flags)?)
                .map_err(err)?;
            print(&json!({ "task": task, "accepted": true }));
        }
        "reject" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, false, &text(&flags, "note")?, revision(&flags)?)
                .map_err(err)?;
            print(&json!({ "task": task, "sent_back": true }));
        }
        "cancel" => {
            let task = id(&flags, "id")?;
            store.task_cancel(task).map_err(err)?;
            print(&json!({ "task": task, "cancelled": true }));
        }
        "stages" => print(&store.task_stages(id(&flags, "id")?).map_err(err)?),
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// `spawn resident ...`
pub(crate) fn resident(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let name = text(&flags, "name")?;
    match verb {
        "propose" => store
            .resident_propose(&name, &text(&flags, "charter")?)
            .map_err(err)?,
        "approve" => store.resident_approve(&name).map_err(err)?,
        "retire" => store.resident_retire(&name).map_err(err)?,
        _ => return Err(USAGE.to_string()),
    }
    print(&json!({ "resident": name, verb: true }));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use spawn_core::event::ModelProfile;

    use super::model;
    use crate::Flags;

    fn flags(words: &[&str]) -> Flags {
        let words: Vec<String> = words.iter().map(ToString::to_string).collect();
        Flags::parse(&words).expect("valid flags")
    }

    #[test]
    fn a_task_runs_on_the_default_profile_unless_model_names_another() {
        assert_eq!(model(&flags(&[])), Ok(ModelProfile::Default));
        assert_eq!(
            model(&flags(&["--model", "cheap"])),
            Ok(ModelProfile::Cheap)
        );
        assert_eq!(
            model(&flags(&["--model", "default"])),
            Ok(ModelProfile::Default)
        );
        let refused = model(&flags(&["--model", "opus"])).expect_err("not a profile");
        assert!(refused.contains("default or cheap"), "{refused}");
    }

    /// A scratch world: a bare source with one commit on main, and the
    /// owner's clone of it. `(base, source, owner)`.
    fn owner_world(tag: &str) -> (std::path::PathBuf, String, String) {
        use crate::sources::git;
        let base = std::env::temp_dir().join(format!("spawn-push-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        crate::integrate::tests::commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("source");
        let owner = base.join("owner").to_string_lossy().to_string();
        git(&["clone", "-q", &source, &owner]).expect("the owner's clone");
        (base, source, owner)
    }

    /// `spawn mission push` resolves the commit where the owner works and
    /// puts it in the source at `refs/spawn-owner/<hash>`, moving no branch;
    /// a commit already in the source (by a name it knows) gets the same
    /// ref; anything else is refused before any request is recorded.
    #[test]
    fn the_owners_commit_waits_in_the_source_and_no_branch_moves() {
        use crate::sources::git;
        let (base, source, owner) = owner_world("put");
        let main = git(&["--git-dir", &source, "rev-parse", "refs/heads/main"]).expect("main");
        std::fs::write(std::path::Path::new(&owner).join("CONTRACT.md"), "v1\n").expect("write");
        crate::integrate::tests::commit_in(&owner, "owner: the contract");
        let head = git(&["-C", &owner, "rev-parse", "HEAD"]).expect("head");

        let (commit, reference) = super::put_in_source(&source, &owner, "HEAD").expect("put");
        assert_eq!(commit, head, "resolved in the owner's repository");
        assert_eq!(reference, format!("refs/spawn-owner/{head}"));
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &reference]).expect("the ref"),
            head
        );
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", "refs/heads/main"]).expect("main"),
            main,
            "no branch moves"
        );

        // Already in the source, named as the source knows it, from a
        // directory that is no repository.
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("a directory");
        let (known, known_ref) =
            super::put_in_source(&source, &elsewhere.to_string_lossy(), &main).expect("known");
        assert_eq!(known, main);
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &known_ref]).expect("the ref"),
            main
        );

        for wanted in ["no-such-branch", "--all", "0123456789abcdef0123456789abcdef01234567"] {
            let refused = super::put_in_source(&source, &owner, wanted).expect_err(wanted);
            assert!(refused.contains("is not a commit"), "{wanted}: {refused}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
