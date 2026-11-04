//! `init-db` and `version`.

use crate::config::open_store;
use crate::flags::Flags;

/// `spawn version`: print `spawn <crate version>`.
pub(crate) fn version(rest: &[String]) -> Result<i32, String> {
    if let Some(word) = rest.first() {
        return Err(format!("unexpected argument {word}"));
    }
    println!("spawn {}", env!("CARGO_PKG_VERSION"));
    Ok(0)
}

/// `spawn init-db`: install the SQL schema.
pub(crate) fn init_db(_flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    store.migrate().map_err(|error| error.to_string())?;
    println!("spawn: schema installed");
    Ok(0)
}
