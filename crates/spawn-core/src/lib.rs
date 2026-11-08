//! Spawn domain: types, ports, event normalization, Agent Bridge and
//! Observer logic. This crate performs no IO.

pub mod bridge;
pub mod command;
#[cfg(feature = "testing")]
pub mod contract;
pub mod event;
pub mod id;
pub mod ingest;
#[cfg(feature = "testing")]
pub mod memory;
pub mod mission;
pub mod observation;
pub mod observe;
pub mod port;
pub mod redact;
pub mod screen;
pub mod shell;
pub mod time;
pub mod transcripts;
