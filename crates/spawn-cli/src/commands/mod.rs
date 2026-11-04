//! One module per command group; `main` parses the command word and calls in.
//!
//! Commands with a larger domain of their own (`serve`, `integrate`,
//! `mission`, `task`, `resident`, `corpus`, `judge`, `charter`) live in their
//! crate-level modules; `init-db` and `version` are in `main`.

pub(crate) mod chunks;
pub(crate) mod engine;
pub(crate) mod inspect;
pub(crate) mod loops;
pub(crate) mod queue;
