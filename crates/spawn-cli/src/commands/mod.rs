//! One module per command group; `main` parses the command word and calls in.
//!
//! Commands with a larger domain of their own (`serve`, `evolve`, `mission`,
//! `task`, `resident`, `charter`) live in their crate-level modules.

pub(crate) mod admin;
pub(crate) mod engine;
pub(crate) mod inspect;
pub(crate) mod judging;
pub(crate) mod loops;
pub(crate) mod queue;
