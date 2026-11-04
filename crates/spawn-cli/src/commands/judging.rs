//! `corpus export` and `judge --replay`: the judgement corpus.

use std::io::Write;

use crate::config::open_store;
use crate::corpus;
use crate::flags::Flags;

/// `spawn corpus export [--since-hours N]`: model judgements with their
/// outcomes, one JSON line each, on stdout.
pub(crate) fn corpus(rest: &[String]) -> Result<i32, String> {
    let Some(("export", rest)) = rest.split_first().map(|(word, rest)| (word.as_str(), rest))
    else {
        return Err("usage: spawn corpus export [--since-hours N]".to_string());
    };
    let flags = Flags::parse(rest)?;
    let hours: i64 = flags.get("since-hours").map_or(Ok(24 * 365), |text| {
        text.parse()
            .map_err(|_| format!("bad --since-hours {text}"))
    })?;
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let store = open_store()?;
    let mut out = std::io::stdout().lock();
    for record in corpus::export(&store, now.saturating_sub(hours.saturating_mul(3_600_000)))? {
        let line = serde_json::to_string(&record).map_err(|error| error.to_string())?;
        writeln!(out, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(0)
}

/// `spawn judge --replay <corpus.jsonl>`: which tier the current rules leave
/// each recorded episode to, and how well they agree.
pub(crate) fn judge(flags: &Flags) -> Result<i32, String> {
    let path = flags.required("replay")?;
    let records = corpus::read(&path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&corpus::replay(&records)).unwrap_or_default()
    );
    Ok(0)
}
