//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands:
//!
//! - `init-db` installs the SQL schema.
//! - `submit` writes one command to the queue.
//! - `tick` runs one Bridge and Observer pass.
//! - `loop` runs the fleet until killed: Bridge, Observer, dispatcher and
//!   the supervisor's liveness rule.
//! - `mission`, `task`, `resident` manage the mission layer.
//! - `integrate` lands gated missions' work and assigns objectives (RSI).
//! - `serve` serves the read-only dashboard and API.
//! - `scorecard` measures every dimension of the fleet at once, from the
//!   record alone (docs/design.md §15).
//! - `fleet` prints every session with its judgement; `tail` one session's
//!   judgement, events, judgements and commands.
//! - `run` starts the engine inside the terminal, records its exit, and
//!   forwards its exit code.
//! - `version` prints `spawn <crate version>`.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its runner needs.
//!
//! This file only parses the command word and hands off: command bodies live
//! in [`commands`], environment and store access in [`config`], and the
//! Bridge/Observer assembly in [`runtime`].

mod commands;
mod config;
mod context;
mod deliver;
mod disk;
mod dispatch;
mod gate;
mod integrate;
mod main_merge;
mod missions;
mod objectives;
mod push;
mod runtime;
mod scorecard;
mod serve;
mod set_aside;
mod sources;
mod supervise;

// Re-exported at the crate root, where the command modules import them.
pub(crate) use config::{Flags, env_or, env_value, open_store};

use commands::{engine, inspect, loops, queue};

/// The commands, as the black-box contract (scripts/blackbox/scenarios/
/// 010-cli-contract.sh) reads them verbatim.
const USAGE: &str = "usage: spawn <init-db|submit|tick|loop|mission|task|resident|integrate|serve|fleet|tail|run|version>";

/// The commands the contract's line does not hold, each on a line of its own
/// after it.
const MORE_USAGE: &str = "       spawn scorecard [--mission N] [--since <time>] [--until <time>] [--json] [--root <dir>]";

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

pub(crate) fn dispatch() -> Result<i32, String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err(format!("{USAGE}\n{MORE_USAGE}"));
    };
    let rest: Vec<String> = arguments.collect();
    match command.as_str() {
        "init-db" => {
            open_store()?.migrate().map_err(|error| error.to_string())?;
            println!("spawn: schema installed");
            Ok(0)
        }
        "submit" => queue::submit(&Flags::parse(&rest)?),
        "tick" => loops::tick(&Flags::parse(&rest)?),
        "loop" => loops::resident_loop(&Flags::parse(&rest)?),
        "serve" => serve::serve(&Flags::parse(&rest)?),
        "scorecard" => scorecard::command(&rest),
        "integrate" => integrate::integrate(&rest),
        "mission" => missions::mission(&rest),
        "task" => missions::task(&rest),
        "resident" => missions::resident(&rest),
        "fleet" => inspect::fleet(),
        "tail" => inspect::tail(&Flags::parse(&rest)?),
        "run" => engine::run(&rest),
        "version" => {
            if let Some(word) = rest.first() {
                return Err(format!("unexpected argument {word}"));
            }
            println!("spawn {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => Err(format!("unknown command {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{MORE_USAGE, USAGE};

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
        for removed in ["status", "dispatch", "charter", "hook", "corpus", "judge"] {
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
}
