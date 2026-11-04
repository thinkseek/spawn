//! SQL of the leased command queue ([`crate::command_queue`]).

/// `$1` principal, `$2` idempotency key, `$3` agent, `$4` session, `$5` action.
pub(crate) const SUBMIT: &str = "select out_id from spawn_submit($1, $2, $3, $4, $5)";

/// Lease the next command, if any. `$1` worker, `$2` lease in milliseconds.
pub(crate) const CLAIM: &str = "select out_id, out_attempt, out_agent, out_session, out_action
                   from spawn_claim($1, $2)";

/// Close a lease; returns `done`, `already` or `stale`.
/// `$1` command, `$2` attempt, `$3` succeeded, `$4` error.
pub(crate) const FINISH: &str = "select spawn_finish($1, $2, $3, $4)";
