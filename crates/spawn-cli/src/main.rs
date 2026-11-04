//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands:
//!
//! - `init-db` installs the SQL schema.
//! - `submit` writes one command to the queue.
//! - `tick` runs one Bridge and Observer pass.
//! - `evolve` advances the recursive self-improvement loop by one step.
//! - `status` prints the latest observation per session.
//! - `hook` ingests one Claude Code hook payload from standard input.
//! - `run` starts the engine inside the terminal, records its exit, and
//!   forwards its exit code.
//! - `version` prints `spawn <crate version>`.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.
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
mod evolve;
mod flags;
mod missions;
mod runtime;
mod serve;
mod sources;
mod supervise;
mod trace;

// Re-exported at the crate root, where the command modules import them.
pub(crate) use config::{env_or, env_value, open_store};
pub(crate) use flags::Flags;

use commands::{admin, engine, inspect, judging, loops, queue};

const USAGE: &str = "usage: spawn <init-db|submit|tick|loop|mission|task|resident|dispatch|corpus|judge|evolve|rsi-loop|serve|status|fleet|tail|hook|run|version>";

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
        "init-db" => admin::init_db(&Flags::parse(&rest)?),
        "submit" => queue::submit(&Flags::parse(&rest)?),
        "tick" => loops::tick(&Flags::parse(&rest)?),
        "loop" => loops::resident_loop(&Flags::parse(&rest)?),
        "serve" => serve::serve(&Flags::parse(&rest)?),
        "evolve" => loops::evolve_once(&Flags::parse(&rest)?),
        "rsi-loop" => loops::rsi_loop(&Flags::parse(&rest)?),
        "dispatch" => queue::dispatch_once(&rest),
        "corpus" => judging::corpus(&rest),
        "mission" => missions::mission(&rest),
        "charter" => {
            println!("{}", supervise::current_charter());
            Ok(0)
        }
        "task" => missions::task(&rest),
        "resident" => missions::resident(&rest),
        "judge" => judging::judge(&Flags::parse(&rest)?),
        "status" => inspect::status(&Flags::parse(&rest)?),
        "fleet" => inspect::fleet(),
        "tail" => inspect::tail(&Flags::parse(&rest)?),
        "hook" => engine::hook(),
        "run" => engine::run(&rest),
        "version" => admin::version(&rest),
        other => Err(format!("unknown command {other}")),
    }
}
