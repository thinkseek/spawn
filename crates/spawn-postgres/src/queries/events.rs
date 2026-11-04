//! SQL of the event log ([`crate::event_log`]).

/// Append one event through `spawn_append`, which assigns its positions.
/// `$1` agent, `$2` session, `$3` execution, `$4` source,
/// `$5` source event id, `$6` kind.
pub(crate) const APPEND: &str = "select out_id, out_seq, out_at, out_inserted
                   from spawn_append($1, $2, $3, $4, $5, $6)";

/// One session's events after a sequence. `$1` session, `$2` after, `$3` limit.
pub(crate) const READ_SESSION: &str = concat!(
    "select ",
    event_record_columns!(),
    "
                   from spawn_events
                  where session_id = $1 and session_seq > $2
                  order by session_seq
                  limit $3"
);

/// Events of every session after a global position. `$1` after, `$2` limit.
pub(crate) const READ_GLOBAL: &str = concat!(
    "select ",
    event_record_columns!(),
    "
                   from spawn_events
                  where id > $1
                  order by id
                  limit $2"
);
