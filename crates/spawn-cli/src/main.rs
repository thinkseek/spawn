//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands:
//!
//! - `init-db` installs the SQL schema.
//! - `submit` writes one command to the queue.
//! - `tick` runs one Bridge and Observer pass.
//! - `integrate` lands gated missions' work and assigns objectives (RSI).
//! - `status` prints the latest observation per session.
//! - `hook` accepts and ignores a hook payload from an engine started by an
//!   older build; the Bridge reads engines' transcripts instead.
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
mod corpus;
mod deliver;
mod dispatch;
mod gate;
mod integrate;
mod missions;
mod objectives;
mod runtime;
mod serve;
mod sources;
mod supervise;

// Re-exported at the crate root, where the command modules import them.
pub(crate) use config::{Flags, env_or, env_value, open_store};

use commands::{engine, inspect, loops, queue};

const USAGE: &str = "usage: spawn <init-db|submit|tick|loop|mission|task|resident|dispatch|integrate|corpus|judge|serve|status|fleet|tail|hook|run|version>";

/// Usage of `spawn dispatch`, the one command that mixes a bare switch with
/// valued flags.
pub(crate) const DISPATCH_USAGE: &str = "usage: spawn dispatch [--dry-run] [--root <dir>]";

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
        return Err(USAGE.to_string());
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
        "dispatch" => queue::dispatch_once(&rest),
        "integrate" => integrate::integrate(&rest),
        "corpus" => corpus::command(&rest),
        "mission" => missions::mission(&rest),
        "charter" => {
            println!("{}", supervise::current_charter());
            Ok(0)
        }
        "task" => missions::task(&rest),
        "resident" => missions::resident(&rest),
        "judge" => corpus::judge(&Flags::parse(&rest)?),
        "status" => inspect::status(&Flags::parse(&rest)?),
        "fleet" => inspect::fleet(),
        "tail" => inspect::tail(&Flags::parse(&rest)?),
        "hook" => Ok(engine::hook()),
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
