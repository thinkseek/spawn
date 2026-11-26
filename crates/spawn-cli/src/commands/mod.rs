//! One module per command group; `main` parses the command word and calls in.
//!
//! Includes fleet inspection (`inspect`: `status`, `fleet`, `tail`), command queue
//! submission (`queue`: `submit`, `say`), loops (`loops`), and execution (`engine`).
//! Commands with a larger domain of their own (`serve`, `integrate`,
//! `mission`, `task`, `resident`) live in their crate-level modules;
//! `init-db` and `version` are in `main`.

pub(crate) mod engine;
pub(crate) mod inspect;
pub(crate) mod loops;
pub(crate) mod queue;
