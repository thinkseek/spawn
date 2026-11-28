//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands, one row each of [`COMMANDS`] (`spawn <command> --help` prints
//! a command's verbs and flags, from its module's own usage):
//!
//! - `init-db` installs the SQL schema.
//! - `status` prints a single-command fleet/task/alert overview (`--json` for JSON).
//! - `say` sends human intervention input to an agent with auto-resolved session and key.
//! - `submit` writes one command to the queue; `ack` answers one from inside
//!   an agent's pane.
//! - `tick` runs one Bridge and Observer pass.
//! - `loop` runs the fleet until killed: Bridge, Observer, dispatcher and
//!   the supervisor's liveness rule.
//! - `mission`, `task`, `resident` manage the mission layer; `objective`
//!   retires an objective; `verify` records an objective's verification
//!   after its landing and lists what is landed and not verified.
//! - `integrate` lands gated missions' work and assigns objectives (RSI).
//! - `serve` serves the dashboard and the HTTP API: reads, and a person's
//!   writes (docs/api.md lists every route).
//! - `scorecard` measures every dimension of the fleet at once, from the
//!   record alone (docs/design.md §15).
//! - `fleet` prints every session with its judgement; `tail` one session's
//!   judgement, events, judgements and commands.
//! - `node` joins, lists, drains and removes the machines Spawn may use
//!   (docs/fleet-architecture.md, "Runtime layer: nodes").
//! - `alerts` prints the alerts Spawn raised, once or as they are recorded;
//!   `alert` records the owner's judgement of one, or an alert itself
//!   (docs/fleet-architecture.md, "Alerts").
//! - `backup` dumps, bundles and drills the fleet's record off this host
//!   (docs/storage.md, "Where state lives"); `store` puts the transcript
//!   archive and the native sessions in the off-host object store.
//! - `mask` and `mask-hook` replace known secret values in what a program
//!   prints.
//! - `commit-map` loads a map of rewritten commit ids.
//! - `features` scans and checks the feature registry
//!   (docs/feature-map/registry.json) and associates files with it.
//! - `grade` keeps the grading catalog (docs/agent-grading.md).
//! - `prov` indexes the Git history into file provenance and reads it back
//!   (docs/provenance.md).
//! - `scratch` claims, renews, releases and sweeps scratch directories.
//! - `run` starts the engine inside the terminal, records its exit, and
//!   forwards its exit code.
//! - `auth` manages unified decoupled auth profiles (`relay`, `native_login`,
//!   `cloud_iam`, `local`) across engines without storing plaintext secrets.
//! - `version` prints `spawn <crate version>`.
//!
//! The nouns are as in docs/glossary.md.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its runner needs.
//!
//! This file only looks the command word up in [`COMMANDS`] and hands off,
//! or prints the help the table holds: command bodies live
//! in [`commands`], environment and store access in [`config`], and the
//! Bridge/Observer assembly in [`runtime`].

#![forbid(unsafe_code)]

mod ack;
mod alerts;
mod auth;
mod backup;
mod commands;
mod commit_map;
mod commit_now;
mod config;
mod config_file;
mod context;
mod core_lines_sample;
mod cost;
mod deliver;
mod disk;
mod dispatch;
mod feature_inventory;
mod gate;
mod grading;
mod integrate;
mod judge_watch;
mod main_merge;
mod managed_sources;
mod mask;
mod memory;
mod missions;
mod nodes;
mod objectives;
mod provenance;
mod push;
mod recover;
mod resolve;
mod rewrite_guard;
mod runtime;
mod scorecard;
mod scratch_owner;
mod scratch_sweep;
mod serve;
mod set_aside;
mod sources;
mod storage;
mod storage_bytes;
mod storage_cutover;
mod supervise;
mod toml_parser;
mod verify;

// Re-exported at the crate root, where the command modules import them.
pub(crate) use config::{Flags, env_or, env_value, open_store};

use commands::{engine, inspect, loops, queue};

/// The contract's line of commands, read verbatim by the operator scenario
/// (rsi/scenarios/check-j05-operator.sh) and by tests/help.rs: it never
/// changes, and the commands after it are on lines of their own.
const USAGE: &str = "usage: spawn <init-db|submit|tick|loop|mission|task|resident|integrate|serve|fleet|tail|run|version>";

/// The commands the contract's line does not hold, each on a line of its own
/// after it. A hand copy: each line is its command's own usage line, or a
/// summary of its verbs (`spawn grade a|b|c ...`), as the tests below hold
/// it to [`COMMANDS`].
const MORE_USAGE: &str = "       spawn scorecard [--mission N] [--since <time>] [--until <time>] [--json] [--root <dir>]\n       \
     spawn node add <name> --ssh <[user@]host|ssh://[user@]host[:port]> [--via <hop>[,<hop>...]] [--note <text>]\n       \
     spawn node list [--json]\n       \
     spawn node drain|undrain|remove <name>\n       \
     spawn node enroll-token <name> [--cell <cell>] [--primary-pubkey <base64url> | --primary-pubkey-file <path>]\n       \
     spawn node revoke <name>\n       \
     spawn node probe <name> [--op inventory|beat] [--json]\n       \
     spawn alerts --once [--json] [--since <90s|30m|2h|1d|ms>]\n       \
     spawn alerts --follow [--json] [--since <event id>]\n       \
     spawn alert judge <key> [<key>...] right|false|stale|duplicate\n       \
     spawn alert record < <one alert.fired or alert.cleared JSON object>\n       \
     spawn backup dump|bundle|verify [--root <dir>]\n       \
     spawn backup restore-drill [--root <dir>] [--into <dir>] [--keep] [--only missions|controllers|transcripts|sync]\n       \
     spawn backup cas snapshot|restore|verify|key-of [...]\n       \
     spawn store put --area transcripts|sessions [--from <dir>] [--root <dir>]\n       \
     spawn store plan --area <area> [--from <dir>] [--root <dir>] [--list <file>]\n       \
     spawn store get --area <area> --path <file> --into <dir> [--root <dir>]\n       \
     spawn store list|verify --area <area> [--root <dir>]\n       \
     spawn storage cutover --status|--to d6|--rollback git|--verify [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--reason <text>] [--json]\n       \
     spawn storage reconcile [--root <dir>] [--cas-dir <dir>] [--reason <prefix>] [--json]\n       \
     spawn storage gc [--root <dir>] [--cas-dir <dir>] [--grace-secs <secs>] [--json]\n       \
     spawn storage verify-bundle --bundle <output-path> [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--json]\n       \
     spawn ack <command id> done|declined [<why>]\n       \
     spawn mask --values <file>\n       \
     spawn mask-hook --values <file>\n       \
     spawn verify <mission>/<objective> --by <name> --how <text> --result pass|fail\n       \
     spawn verify list\n       \
     spawn objective retire --mission <m> --objective <n> --kind on-main|renumbered|obsolete --by <name> --reason <text>\n       \
     spawn scratch claim <dir> --agent <name> --objective <mission>/<objective> [--build <path>]... [--lease-hours <h>]\n       \
     spawn scratch renew <dir> [--lease-hours <h>]\n       \
     spawn scratch release <dir>\n       \
     spawn scratch sweep [--excuse <pid>:<start ticks>]... [--apply --expect-digest <digest>]\n       \
     spawn commit-map load <file> --source <git dir>\n       \
     spawn features scan|check [--root <dir>] [--registry <file>] [--json]\n       \
     spawn features associate --path <file> --feature <id> [--feature <id>]... [--root <dir>] [--registry <file>] [--json]\n       \
     spawn features graph [--root <dir>] [--registry <file>] [--level file|unit|feature] [--from <id|path>] [--to <id|path>] [--feature <id>] [--json]\n       \
     spawn grade register|list|exam|onboard|lock-grade|unlock-grade|enable|disable|calibrate|upkeep|init-topdown|status ...   (the grading catalog: spawn grade --help for each verb's flags)\n       \
     spawn prov index|who|show|blast|revert|split|compose|snapshot|refs|rollback|durability ...   (file provenance: spawn prov --help for each verb's flags)\n       \
     spawn source init|list|show|sync|export ...   (internal source and metadata store: spawn source --help for each verb's flags)\n       \
     spawn config check [--config <path>] [--json]\n       \
     spawn config effective [--config <path>] [--json]\n       \
     spawn config init --data-dir <dir> [--name <name>] [--output <file>] [--from-host]\n       \
     spawn config docker-plan [--config <path>] [--data-dir <dir>] [--image <image>] [--json]\n       \
     spawn doctor [--config <path>] [--root <dir>] [--json]\n       \
     spawn status [--json]\n       \
     spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--json]\n       \
     spawn auth list [--config <path>] [--json]\n       \
     spawn auth add-relay <name> --url <url> (--key-env <VAR> | --key-file <PATH>) [--engines claude,codex] [--tiers L1,L2,L3] [--config <path>] [--json]\n       \
     spawn auth login <agy|claude|codex> [--profile <name>] [--config <path>]\n       \
     spawn auth check [--config <path>] [--json]";

/// What the whole usage says last, after the commands.
const HELP_HINT: &str =
    "       spawn <command> --help, or spawn help <command>: that command's verbs and flags";

/// `spawn init-db`'s usage.
const INIT_DB_USAGE: &str = "usage: spawn init-db\n      \
                             (installs or updates the SQL schema; safe to run again)";

/// `spawn version`'s usage.
const VERSION_USAGE: &str = "usage: spawn version   (also spawn --version or spawn -V)";

/// One command: its canonical name, its other spellings, its usage and what
/// runs it.
struct Command {
    /// The word `dispatch` matches, and help shows.
    name: &'static str,
    /// Other words for the same row: one implementation, two spellings, so
    /// an alias can never grow a code path of its own.
    aliases: &'static [&'static str],
    /// The module's own usage constants, the only copies: a command's help
    /// is the lines of them that name it ([`own_entries`]).
    usage: &'static [&'static str],
    /// The command, given the words after its name.
    run: fn(&[String]) -> Result<i32, String>,
}

/// Every command, in one table: [`dispatch`] looks the first word up in it,
/// and help reads each command's usage from it. None of the retired words
/// (`judge`, `hook`, `dispatch`, `charter`, `corpus`) is a name or
/// an alias here, so each stays an unknown command.
const COMMANDS: &[Command] = &[
    Command {
        name: "init-db",
        aliases: &[],
        usage: &[INIT_DB_USAGE],
        run: init_db,
    },
    Command {
        name: "status",
        aliases: &[],
        usage: &[inspect::STATUS_USAGE],
        run: |rest| inspect::status(&Flags::parse_with_switches(rest, &["json"])?),
    },
    Command {
        name: "say",
        aliases: &[],
        usage: &[queue::SAY_USAGE],
        run: queue::say,
    },
    Command {
        name: "submit",
        aliases: &[],
        usage: &[queue::USAGE],
        run: |rest| queue::submit(&Flags::parse(rest)?),
    },
    Command {
        name: "tick",
        aliases: &[],
        usage: &[loops::TICK_USAGE],
        run: |rest| loops::tick(&Flags::parse(rest)?),
    },
    Command {
        name: "loop",
        aliases: &[],
        usage: &[loops::LOOP_USAGE],
        run: |rest| {
            long_lived();
            loops::resident_loop(&Flags::parse(rest)?)
        },
    },
    Command {
        name: "serve",
        aliases: &[],
        usage: &[serve::USAGE],
        run: |rest| {
            long_lived();
            serve::command(rest)
        },
    },
    Command {
        name: "scorecard",
        aliases: &[],
        usage: &[scorecard::USAGE],
        run: scorecard::command,
    },
    Command {
        name: "integrate",
        aliases: &[],
        usage: &[integrate::USAGE],
        run: |rest| {
            long_lived();
            integrate::integrate(rest)
        },
    },
    Command {
        name: "mission",
        aliases: &[],
        usage: &[missions::USAGE],
        run: missions::mission,
    },
    Command {
        name: "task",
        aliases: &[],
        usage: &[missions::USAGE],
        run: missions::task,
    },
    Command {
        name: "resident",
        aliases: &[],
        usage: &[missions::USAGE],
        run: missions::resident,
    },
    Command {
        name: "node",
        aliases: &[],
        usage: &[nodes::USAGE],
        run: nodes::command,
    },
    Command {
        name: "fleet",
        aliases: &[],
        usage: &[inspect::FLEET_USAGE],
        run: |_| inspect::fleet(),
    },
    Command {
        name: "alerts",
        aliases: &[],
        usage: &[alerts::USAGE],
        run: alerts::command,
    },
    Command {
        name: "alert",
        aliases: &[],
        usage: &[alerts::JUDGE_USAGE],
        run: alerts::alert,
    },
    Command {
        name: "backup",
        aliases: &[],
        usage: &[backup::USAGE, backup::CAS_USAGE],
        run: backup::command,
    },
    Command {
        name: "store",
        aliases: &[],
        usage: &[backup::STORE_USAGE],
        run: backup::store_command,
    },
    Command {
        name: "storage",
        aliases: &[],
        usage: &[storage_cutover::USAGE],
        run: storage_cutover::command,
    },
    Command {
        name: "ack",
        aliases: &[],
        usage: &[ack::USAGE],
        run: ack::command,
    },
    Command {
        name: "mask",
        aliases: &[],
        usage: &[mask::MASK_USAGE],
        run: mask::mask_command,
    },
    Command {
        name: "mask-hook",
        aliases: &[],
        usage: &[mask::HOOK_USAGE],
        run: mask::hook_command,
    },
    Command {
        name: "verify",
        aliases: &[],
        usage: &[verify::USAGE],
        run: verify::command,
    },
    Command {
        name: "objective",
        aliases: &[],
        usage: &[objectives::RETIRE_USAGE],
        run: objectives::command,
    },
    Command {
        name: "commit-map",
        aliases: &[],
        usage: &[commit_map::USAGE],
        run: commit_map::command,
    },
    Command {
        name: "features",
        aliases: &[],
        usage: &[feature_inventory::USAGE],
        run: feature_inventory::command,
    },
    Command {
        name: "grade",
        aliases: &[],
        usage: &[grading::USAGE],
        run: grading::command,
    },
    Command {
        name: "prov",
        aliases: &[],
        usage: &[provenance::USAGE],
        run: provenance::command,
    },
    Command {
        name: "source",
        aliases: &[],
        usage: &[managed_sources::USAGE],
        run: managed_sources::command,
    },
    Command {
        name: "scratch",
        aliases: &[],
        usage: &[scratch_owner::USAGE],
        run: scratch_owner::command,
    },
    Command {
        name: "tail",
        aliases: &[],
        usage: &[inspect::TAIL_USAGE],
        run: |rest| inspect::tail(&Flags::parse(rest)?),
    },
    Command {
        name: "run",
        aliases: &[],
        usage: &[engine::USAGE],
        run: engine::run,
    },
    Command {
        name: "config",
        aliases: &[],
        usage: &[config_file::CONFIG_USAGE],
        run: |rest| config_file::config_command(None, rest),
    },
    Command {
        name: "doctor",
        aliases: &[],
        usage: &[config_file::DOCTOR_USAGE],
        run: |rest| config_file::doctor_command(None, rest),
    },
    Command {
        name: "auth",
        aliases: &[],
        usage: &[auth::USAGE],
        run: auth::command,
    },
    Command {
        name: "version",
        aliases: &["--version", "-V"],
        usage: &[VERSION_USAGE],
        run: version,
    },
];

fn main() {
    let code = match dispatch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("spawn: {error}");
            1
        }
    };
    std::process::exit(code);
}

/// What a long-lived process (`loop`, `serve`, `integrate`) does first: it
/// holds the owner's database URL and tokens in its environment, and the
/// engines run as the same user, so it makes itself not dumpable
/// ([`spawn_host::undumpable`]): its `/proc/<pid>/environ` is then
/// unreadable to them. The programs it starts (engines, gates, git) are
/// dumpable again from their exec. Where the kernel refuses, it says so and
/// goes on.
fn long_lived() {
    if let Err(error) = spawn_host::undumpable() {
        eprintln!("spawn: {error}; this process's environment stays readable to its user");
    }
}

/// `spawn init-db`: install or update the schema.
fn init_db(_rest: &[String]) -> Result<i32, String> {
    open_store()?.migrate().map_err(|error| error.to_string())?;
    println!("spawn: schema installed");
    Ok(0)
}

/// `spawn version`, `spawn --version`, `spawn -V`.
fn version(rest: &[String]) -> Result<i32, String> {
    if let Some(word) = rest.first() {
        return Err(format!("unexpected argument {word}"));
    }
    println!("spawn {}", env!("CARGO_PKG_VERSION"));
    Ok(0)
}

/// The row a word names, by its name or an alias.
fn find(word: &str) -> Option<&'static Command> {
    COMMANDS
        .iter()
        .find(|command| command.name == word || command.aliases.contains(&word))
}

/// The commands of the contract's line, in its order.
fn contract_commands() -> impl Iterator<Item = &'static str> {
    USAGE
        .split_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .unwrap_or_default()
        .split('|')
}

/// One invocation in a usage text: its synopsis (`spawn <command> ...`,
/// without `usage:`) and the lines after it that explain it.
struct Entry {
    synopsis: &'static str,
    notes: Vec<&'static str>,
}

/// The invocations of a usage text, in its order.
fn entries(usage: &'static str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in usage.lines() {
        let text = line.trim_start();
        let text = text.strip_prefix("usage:").map_or(text, str::trim_start);
        if text.starts_with("spawn ") {
            entries.push(Entry {
                synopsis: text,
                notes: Vec::new(),
            });
        } else if let Some(entry) = entries.last_mut()
            && !text.is_empty()
        {
            entry.notes.push(line);
        }
    }
    entries
}

/// The command a synopsis names: its second word.
fn command_of(synopsis: &str) -> Option<&str> {
    synopsis.split_whitespace().nth(1)
}

/// The verbs a synopsis names: the word after the command, split at `|`,
/// unless that word is a flag, a placeholder or a note (`--once`, `<id>`,
/// `NAME`, `(also ...`).
fn verbs(synopsis: &str) -> Vec<&str> {
    match synopsis.split_whitespace().nth(2) {
        Some(word) if word.starts_with(|c: char| c.is_ascii_lowercase()) => {
            word.split('|').collect()
        }
        _ => Vec::new(),
    }
}

/// The invocations of a command's own usage that name it: a usage shared by
/// several commands (`mission`, `task`, `resident`) shows each its own.
fn own_entries(command: &Command) -> Vec<Entry> {
    command
        .usage
        .iter()
        .flat_map(|usage| entries(usage))
        .filter(|entry| command_of(entry.synopsis) == Some(command.name))
        .collect()
}

/// Each verb a command's own usage names, once, in its order.
fn own_verbs(command: &Command) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = Vec::new();
    for entry in own_entries(command) {
        for verb in verbs(entry.synopsis) {
            if !all.contains(&verb) {
                all.push(verb);
            }
        }
    }
    all
}

/// Where the database setting comes from (GitHub #63, part 3).
fn database_note() -> String {
    format!(
        "Most commands need SPAWN_DATABASE_URL; {}.",
        config::DATABASE_URL_SOURCE
    )
}

/// One line per command of the contract's line: its one synopsis, or the
/// verbs it takes.
fn contract_index() -> String {
    contract_commands()
        .filter_map(find)
        .map(|command| match own_entries(command).as_slice() {
            [one] => format!("       {}", one.synopsis),
            _ => format!(
                "       spawn {} {} ...",
                command.name,
                own_verbs(command).join("|")
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The whole usage: the contract's line, a line for each of its commands,
/// the other commands, where each command's own help is, and where the
/// database setting comes from.
fn full_usage() -> String {
    format!(
        "{USAGE}\n{}\n{MORE_USAGE}\n{HELP_HINT}\n\n{}",
        contract_index(),
        database_note()
    )
}

/// One command's help: the contract's line, then the command's own usage,
/// then where the database setting comes from.
fn command_usage(command: &Command) -> String {
    let mut lines: Vec<String> = Vec::new();
    for entry in own_entries(command) {
        lines.push(format!("  {}", entry.synopsis));
        lines.extend(entry.notes.iter().map(|note| (*note).to_string()));
    }
    format!("{USAGE}\n\n{}\n\n{}", lines.join("\n"), database_note())
}

/// The help the words ask for: `spawn help <command>` and `spawn <command>
/// ... --help` print that command's; `spawn --help`, `spawn -h`, `spawn
/// help`, and help for a word that is no command, the whole usage.
fn help(command: &str, rest: &[String]) -> String {
    let named = match command {
        "help" => rest.first().map(String::as_str),
        "--help" | "-h" => None,
        other => Some(other),
    };
    named.and_then(find).map_or_else(full_usage, command_usage)
}

/// Whether the words ask for the usage: `spawn --help`, `spawn -h`, `spawn
/// help`, or `spawn <command> ... --help` (before a `--`, past which the
/// words are a program's). `spawn run` hands its words to the program it
/// starts, and `spawn ack` joins the words after its outcome into a free
/// why (`spawn ack 7 declined see --help`), so only their first word asks:
/// `spawn run --help`, `spawn ack --help`. Every other free text (a brief,
/// a reason, a note) is one flag value, a single word.
fn help_requested(command: &str, rest: &[String]) -> bool {
    if matches!(command, "--help" | "-h" | "help") {
        return true;
    }
    if matches!(command, "run" | "ack") {
        return rest.first().is_some_and(|word| word == "--help");
    }
    rest.iter()
        .take_while(|word| word.as_str() != "--")
        .any(|word| word == "--help")
}

const CONFIG_APPLIED_ENV: &str = "__SPAWN_CONFIG_APPLIED";

fn parse_global_config(
    arguments: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<Option<String>, String> {
    let mut cli_config: Option<String> = None;
    while let Some(arg) = arguments.peek() {
        if arg == "--config" || arg == "-c" {
            let flag = arguments.next().expect("peeked");
            let Some(value) = arguments.next() else {
                return Err(format!("{flag} needs a value"));
            };
            if value.starts_with('-') && value != "-" {
                return Err(format!("{flag} needs a value, found {value}"));
            }
            cli_config = Some(value);
        } else if let Some(value) = arg.strip_prefix("--config=") {
            if value.is_empty() {
                return Err("--config needs a value".to_string());
            }
            cli_config = Some(value.to_string());
            arguments.next();
        } else if let Some(value) = arg.strip_prefix("-c=") {
            if value.is_empty() {
                return Err("-c needs a value".to_string());
            }
            cli_config = Some(value.to_string());
            arguments.next();
        } else {
            break;
        }
    }
    Ok(cli_config)
}

pub(crate) fn dispatch() -> Result<i32, String> {
    let mut arguments = std::env::args().skip(1).peekable();
    let cli_config = parse_global_config(&mut arguments)?;
    let Some(command) = arguments.next() else {
        return Err(full_usage());
    };
    let rest: Vec<String> = arguments.collect();
    // The usage asked for is printed, opening no store.
    if help_requested(&command, &rest) {
        println!("{}", help(&command, &rest));
        return Ok(0);
    }
    match find(&command) {
        Some(row) => {
            if row.name == "config" {
                return config_file::config_command(cli_config.as_deref(), &rest);
            }
            if row.name == "doctor" {
                return config_file::doctor_command(cli_config.as_deref(), &rest);
            }
            if row.name == "auth" {
                return auth::command_with_config(cli_config.as_deref(), &rest);
            }
            let effective_config = cli_config.or_else(|| env_value("SPAWN_CONFIG"));
            if let Some(config_path) = effective_config
                && !matches!(row.name, "version" | "run" | "ack" | "mask" | "mask-hook")
                && env_value(CONFIG_APPLIED_ENV).as_deref() != Some(config_path.as_str())
            {
                return config_file::exec_with_config(
                    std::path::Path::new(&config_path),
                    &command,
                    &rest,
                    CONFIG_APPLIED_ENV,
                );
            }
            (row.run)(&rest)
        }
        None => Err(format!("unknown command {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMMANDS, Command, HELP_HINT, MORE_USAGE, USAGE, command_of, contract_commands, entries,
        find, full_usage, help, help_requested, missions, own_entries, own_verbs, verbs,
    };

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn help_is_asked_at_the_top_and_after_a_command_but_not_of_a_program() {
        for command in ["--help", "-h", "help"] {
            assert!(help_requested(command, &[]), "{command}");
        }
        assert!(help_requested("task", &words("--help")));
        assert!(help_requested("task", &words("assign --help")));
        assert!(help_requested("fleet", &words("--help")));
        assert!(help_requested("run", &words("--help")));
        assert!(help_requested("ack", &words("--help")));
        assert!(!help_requested("ack", &words("12 declined see --help")));
        assert!(!help_requested("ack", &words("12 done --help works now")));
        assert!(!help_requested("task", &words("accept --id 3")));
        assert!(!help_requested("run", &words("claude --help")));
        assert!(!help_requested("run", &words("-- claude --help")));
        assert!(!help_requested(
            "mask-hook",
            &words("--values f -- x --help")
        ));
        assert!(!help_requested("nonsense", &[]));
    }

    #[test]
    fn the_whole_usage_says_where_the_database_setting_comes_from() {
        let usage = full_usage();
        assert!(usage.starts_with(USAGE), "{usage}");
        assert!(usage.contains(MORE_USAGE), "{usage}");
        assert!(usage.contains("SPAWN_DATABASE_URL"), "{usage}");
        assert!(usage.contains("spawn.env"), "{usage}");
    }

    #[test]
    fn the_usage_names_the_backup_command() {
        assert!(
            MORE_USAGE.contains("spawn backup dump|bundle|verify"),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("backup"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_node_command_after_the_contracts_line() {
        for line in [
            "spawn node add <name> --ssh",
            "spawn node list [--json]",
            "spawn node drain|undrain|remove <name>",
            "spawn node enroll-token <name>",
            "spawn node revoke <name>",
            "spawn node probe <name>",
        ] {
            assert!(MORE_USAGE.contains(line), "{line}: {MORE_USAGE}");
        }
        for retired in [
            "spawn node serve",
            "spawn node register",
            "spawn node coord",
            "spawn node connect",
            "--tcp",
            "--udp",
            "--coord",
        ] {
            assert!(!MORE_USAGE.contains(retired), "{retired}: {MORE_USAGE}");
        }
        assert!(
            !USAGE.contains("node"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_alerts_command() {
        assert!(
            MORE_USAGE.contains("spawn alerts --once")
                && MORE_USAGE.contains("spawn alerts --follow"),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("alerts"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_owners_judgement_of_an_alert() {
        assert!(
            MORE_USAGE.contains("spawn alert judge <key> [<key>...] right|false|stale|duplicate"),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("judge"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_ack_after_the_contracts_line() {
        assert!(
            MORE_USAGE.contains("spawn ack <command id> done|declined [<why>]"),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("ack"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_features_graph_after_the_contracts_line() {
        assert!(
            MORE_USAGE.contains("spawn features scan|check ")
                && MORE_USAGE.contains("spawn features graph [--root <dir>]"),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("features"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_the_scorecard_on_a_line_of_its_own() {
        assert!(MORE_USAGE.trim_start().starts_with("spawn scorecard "));
        assert!(
            !USAGE.contains("scorecard"),
            "the contract's line stays as it was"
        );
    }

    #[test]
    fn the_usage_names_each_command_once_and_no_removed_one() {
        let list = USAGE
            .split_once('<')
            .and_then(|(_, rest)| rest.strip_suffix('>'))
            .expect("usage: spawn <a|b|…>");
        let words: Vec<&str> = list.split('|').collect();
        let mut unique = words.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), words.len(), "a command listed twice: {list}");
        for removed in ["dispatch", "charter", "hook", "corpus", "judge", "secret"] {
            assert!(
                !words.contains(&removed),
                "{removed} is no longer a command"
            );
        }
        for core in [
            "loop",
            "integrate",
            "mission",
            "task",
            "fleet",
            "tail",
            "run",
        ] {
            assert!(words.contains(&core), "{core} is a core command");
        }
    }

    /// A synopsis without the note after it (`   (owner only)`).
    fn bare(synopsis: &str) -> &str {
        synopsis
            .split_once("   (")
            .map_or(synopsis, |(bare, _)| bare)
            .trim_end()
    }

    /// The commands whose usage is on lines after the contract's.
    fn after_the_contracts_line() -> impl Iterator<Item = &'static Command> {
        COMMANDS
            .iter()
            .filter(|command| !contract_commands().any(|word| word == command.name))
    }

    /// The lines after the contract's line, trimmed.
    fn more_lines() -> impl Iterator<Item = &'static str> {
        MORE_USAGE.lines().map(str::trim)
    }

    #[test]
    fn every_line_after_the_contracts_line_is_its_commands_own_usage() {
        for line in more_lines() {
            let name = command_of(line).unwrap_or_else(|| panic!("no command: {line}"));
            let command = find(name).unwrap_or_else(|| panic!("{name} is no command: {line}"));
            let own = own_entries(command);
            let exact = own.iter().any(|entry| bare(entry.synopsis) == bare(line));
            let summary = bare(line).strip_suffix(" ...").unwrap_or(bare(line))
                == format!("spawn {name} {}", own_verbs(command).join("|"));
            assert!(
                exact || summary,
                "{line}\nis neither a line of spawn {name}'s own usage nor all its verbs:\n{}",
                own.iter()
                    .map(|entry| entry.synopsis)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    #[test]
    fn every_verb_of_a_command_after_the_contracts_line_is_on_its_lines() {
        for command in after_the_contracts_line() {
            let named: Vec<&str> = more_lines()
                .filter(|line| command_of(line) == Some(command.name))
                .flat_map(verbs)
                .collect();
            assert!(
                more_lines().any(|line| command_of(line) == Some(command.name)),
                "spawn {} has no line after the contract's",
                command.name
            );
            for verb in own_verbs(command) {
                assert!(
                    named.contains(&verb),
                    "spawn {} {verb} is in its usage but not after the contract's line",
                    command.name
                );
            }
        }
    }

    #[test]
    fn the_table_holds_every_command_of_the_usage_once_and_no_retired_word() {
        for word in contract_commands() {
            assert!(find(word).is_some(), "{word} is on the contract's line");
        }
        for line in more_lines() {
            let name = command_of(line).expect("a command");
            assert!(find(name).is_some(), "{name}: {line}");
        }
        let mut words: Vec<&str> = COMMANDS
            .iter()
            .flat_map(|command| {
                std::iter::once(command.name).chain(command.aliases.iter().copied())
            })
            .collect();
        let all = words.len();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), all, "a word names two rows: {words:?}");
        for removed in ["dispatch", "charter", "hook", "corpus", "judge", "secret"] {
            assert!(
                find(removed).is_none(),
                "{removed} stays an unknown command"
            );
        }
        assert_eq!(COMMANDS.len(), 36, "the commands: {words:?}");
    }

    #[test]
    fn the_usage_names_the_auth_command_after_the_contracts_line() {
        for line in [
            "spawn auth list [--config <path>] [--json]",
            "spawn auth add-relay <name> --url <url>",
            "spawn auth login <agy|claude|codex>",
            "spawn auth check [--config <path>] [--json]",
        ] {
            assert!(MORE_USAGE.contains(line), "{line}: {MORE_USAGE}");
        }
        assert!(
            !USAGE.contains("auth"),
            "the contract's line stays as it was: {USAGE}"
        );
    }

    #[test]
    fn the_usage_names_status_and_say_after_the_contracts_line() {
        assert!(MORE_USAGE.contains("spawn status [--json]"), "{MORE_USAGE}");
        assert!(
            MORE_USAGE.contains(
                "spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--json]"
            ),
            "{MORE_USAGE}"
        );
        assert!(
            !USAGE.contains("status") && !USAGE.contains("say"),
            "the contract's line stays as it was: {USAGE}"
        );
    }

    #[test]
    fn min_git_matches_config_module() {
        assert_eq!(super::config_file::MIN_GIT, super::config::MIN_GIT);
    }

    #[test]
    fn every_command_has_its_own_lines_in_its_usage() {
        for command in COMMANDS {
            assert!(
                !own_entries(command).is_empty(),
                "spawn {} has no line of its own in its usage",
                command.name
            );
        }
        // The one usage the mission layer shares is split whole among its
        // three commands.
        let shared = entries(missions::USAGE).len();
        let split: usize = ["mission", "task", "resident"]
            .into_iter()
            .map(|name| own_entries(find(name).expect(name)).len())
            .sum();
        assert_eq!(split, shared);
    }

    #[test]
    fn a_commands_help_is_the_contracts_line_then_its_own_usage() {
        let asked = vec!["--help".to_string()];
        for command in COMMANDS {
            let text = help(command.name, &asked);
            assert!(text.starts_with(USAGE), "{text}");
            assert!(
                text.contains("SPAWN_DATABASE_URL") && text.contains("spawn.env"),
                "{text}"
            );
            for entry in own_entries(command) {
                assert!(text.contains(entry.synopsis), "{}: {text}", command.name);
            }
            for line in text.lines().skip(1).map(str::trim) {
                if let Some(other) = line.strip_prefix("spawn ") {
                    assert!(
                        other.starts_with(&format!("{} ", command.name)) || other == command.name,
                        "spawn {}'s help names another command: {line}",
                        command.name
                    );
                }
            }
            assert_eq!(text, help("help", &[command.name.to_string()]));
        }
        let mission = help("mission", &asked);
        assert!(mission.contains("spawn mission add --text"), "{mission}");
        assert!(!mission.contains("spawn task assign"), "{mission}");
        let task = help("task", &["accept".to_string(), "--help".to_string()]);
        assert!(task.contains("spawn task accept --id N"), "{task}");
        assert!(!task.contains("spawn mission add"), "{task}");
        for whole in [
            help("--help", &[]),
            help("-h", &[]),
            help("help", &[]),
            help("help", &["nonsense".to_string()]),
            help("nonsense", &asked),
        ] {
            assert_eq!(whole, full_usage());
        }
    }

    #[test]
    fn the_whole_usage_has_a_line_per_contract_command_and_points_at_their_help() {
        let usage = full_usage();
        let lines: Vec<&str> = usage.lines().map(str::trim).collect();
        for word in contract_commands() {
            assert!(
                lines
                    .iter()
                    .any(|line| command_of(line) == Some(word) && line.starts_with("spawn ")),
                "{word}: {usage}"
            );
        }
        assert!(
            usage.contains(
                "spawn mission add|configure|objectives|pause|resume|push|pushes|moved|landing|deployed|hold|release|list|show|close ..."
            ),
            "{usage}"
        );
        assert!(usage.contains(HELP_HINT), "{usage}");
        assert!(usage.contains("spawn task assign|accept|"), "{usage}");
    }

    #[test]
    fn the_five_drifts_are_fixed() {
        for line in [
            "spawn backup cas snapshot|restore|verify|key-of",
            "spawn store plan --area",
            "[--only missions|controllers|transcripts|sync]",
            "spawn features associate --path <file> --feature <id>",
        ] {
            assert!(MORE_USAGE.contains(line), "{line}: {MORE_USAGE}");
        }
    }

    #[test]
    fn version_answers_to_its_flag_spellings() {
        for word in ["version", "--version", "-V"] {
            assert_eq!(find(word).map(|command| command.name), Some("version"));
        }
        assert!(!help_requested("--version", &[]));
    }
}
