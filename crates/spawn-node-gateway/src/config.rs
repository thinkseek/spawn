//! The gateway's configuration, from its environment (and
//! `--public-prefix`, which overrides the variable):
//!
//! | variable | default | |
//! | --- | --- | --- |
//! | `SPAWN_NODE_GATEWAY_DATABASE_URL` | required | the restricted `spawn_node_gateway` login's URL |
//! | `SPAWN_NODE_GATEWAY_KEY_FILE` | required | the primary's PKCS#8 Ed25519 key (0600; made when absent) |
//! | `SPAWN_NODE_GATEWAY_LISTEN` | `127.0.0.1:18096` | the address served |
//! | `SPAWN_NODE_ARTIFACTS` | required | `<algo>/<2 hex>/<hex>` and `registry-manifest.json` |
//! | `SPAWN_NODE_RESULTS` | required | `<job>.<epoch>/{log,meta.json}` |
//! | `SPAWN_NODE_GATEWAY_CELLS` | `c0` | the cells served, comma separated |
//! | `SPAWN_NODE_GATEWAY_PUBLIC_PREFIX` | empty | a path prefix a reverse proxy keeps |

use std::net::SocketAddr;
use std::path::PathBuf;

pub const ENV_DATABASE_URL: &str = "SPAWN_NODE_GATEWAY_DATABASE_URL";
pub const ENV_KEY_FILE: &str = "SPAWN_NODE_GATEWAY_KEY_FILE";
pub const ENV_LISTEN: &str = "SPAWN_NODE_GATEWAY_LISTEN";
pub const ENV_ARTIFACTS: &str = "SPAWN_NODE_ARTIFACTS";
pub const ENV_RESULTS: &str = "SPAWN_NODE_RESULTS";
pub const ENV_CELLS: &str = "SPAWN_NODE_GATEWAY_CELLS";
pub const ENV_PUBLIC_PREFIX: &str = "SPAWN_NODE_GATEWAY_PUBLIC_PREFIX";

pub const DEFAULT_LISTEN: &str = "127.0.0.1:18096";
pub const DEFAULT_CELL: &str = "c0";

/// Everything the gateway reads at start.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Never printed: it holds the login's password.
    pub database_url: String,
    pub key_file: PathBuf,
    pub listen: SocketAddr,
    pub artifacts: PathBuf,
    pub results: PathBuf,
    pub cells: Vec<String>,
    /// Empty, or `/` and path characters without a trailing `/`.
    pub public_prefix: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("database_url", &"<hidden>")
            .field("key_file", &self.key_file)
            .field("listen", &self.listen)
            .field("artifacts", &self.artifacts)
            .field("results", &self.results)
            .field("cells", &self.cells)
            .field("public_prefix", &self.public_prefix)
            .finish()
    }
}

impl Config {
    /// The configuration of `env` (a variable's value by name) and the
    /// command line's arguments after the program name.
    pub fn from_env(env: impl Fn(&str) -> Option<String>, args: &[String]) -> Result<Self, String> {
        let required = |name: &str| {
            env(name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{name} is not set"))
        };
        let mut public_prefix = env(ENV_PUBLIC_PREFIX).unwrap_or_default();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if let Some(value) = arg.strip_prefix("--public-prefix=") {
                public_prefix = value.to_string();
            } else if arg == "--public-prefix" {
                rest.next()
                    .ok_or("--public-prefix needs a value")?
                    .clone_into(&mut public_prefix);
            } else {
                return Err(format!("unknown argument {arg:?} (only --public-prefix)"));
            }
        }
        let listen = env(ENV_LISTEN)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen = listen
            .parse()
            .map_err(|_| format!("{ENV_LISTEN}={listen:?} is not host:port"))?;
        let cells: Vec<String> = env(ENV_CELLS)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_CELL.to_string())
            .split(',')
            .map(|cell| cell.trim().to_string())
            .collect();
        if cells.iter().any(|cell| !is_cell_id(cell)) {
            return Err(format!(
                "{ENV_CELLS}: a cell id is 1 to 64 of a-z A-Z 0-9 . _ -"
            ));
        }
        Ok(Self {
            database_url: required(ENV_DATABASE_URL)?,
            key_file: required(ENV_KEY_FILE)?.into(),
            listen,
            artifacts: required(ENV_ARTIFACTS)?.into(),
            results: required(ENV_RESULTS)?.into(),
            cells,
            public_prefix: normalize_prefix(&public_prefix)?,
        })
    }
}

fn is_cell_id(cell: &str) -> bool {
    (1..=64).contains(&cell.len())
        && cell
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A prefix as the gateway strips it: empty, or `/seg/seg` of the target
/// grammar's path characters, no trailing `/`, no `.` or `..` segment.
pub fn normalize_prefix(prefix: &str) -> Result<String, String> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return Ok(String::new());
    }
    let ok = prefix.starts_with('/')
        && prefix.len() <= 200
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && prefix[1..]
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if ok {
        Ok(prefix.to_string())
    } else {
        Err(format!("public prefix {prefix:?} is not /path/segments"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const BASE: [(&str, &str); 4] = [
        (ENV_DATABASE_URL, "postgres://gw:pw@localhost/spawn"),
        (ENV_KEY_FILE, "/k/primary.pk8"),
        (ENV_ARTIFACTS, "/a"),
        (ENV_RESULTS, "/r"),
    ];

    #[test]
    fn defaults_and_overrides() {
        let config = Config::from_env(env(&BASE), &[]).unwrap();
        assert_eq!(config.listen, DEFAULT_LISTEN.parse().unwrap());
        assert_eq!(config.cells, ["c0"]);
        assert_eq!(config.public_prefix, "");
        assert!(!format!("{config:?}").contains("pw@"), "the URL is never printed");

        let mut pairs = BASE.to_vec();
        pairs.push((ENV_CELLS, "c0, c1"));
        pairs.push((ENV_PUBLIC_PREFIX, "/env"));
        pairs.push((ENV_LISTEN, "0.0.0.0:9"));
        let args = ["--public-prefix".to_string(), "/gw/".to_string()];
        let config = Config::from_env(env(&pairs), &args).unwrap();
        assert_eq!(config.cells, ["c0", "c1"]);
        assert_eq!(config.public_prefix, "/gw");
        assert_eq!(config.listen.port(), 9);
    }

    #[test]
    fn missing_and_bad_values_are_refused() {
        assert!(Config::from_env(env(&BASE[1..]), &[]).is_err());
        for bad in ["gw", "/a/../b", "/a//b", "/a%2f"] {
            assert!(normalize_prefix(bad).is_err(), "{bad}");
        }
        let mut pairs = BASE.to_vec();
        pairs.push((ENV_CELLS, "c0,"));
        assert!(Config::from_env(env(&pairs), &[]).is_err());
        assert!(Config::from_env(env(&BASE), &["--listen".into()]).is_err());
    }
}
