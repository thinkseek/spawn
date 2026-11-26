//! Isolated `herdr-core` companion CLI/daemon entry point for Spawn.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

fn main() {
    std::process::exit(herdr_core::cli::main());
}
