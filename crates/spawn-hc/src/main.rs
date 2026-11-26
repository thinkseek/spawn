//! Isolated `herdr-core` companion CLI/daemon entry point for Spawn.

fn main() {
    std::process::exit(herdr_core::cli::main());
}
