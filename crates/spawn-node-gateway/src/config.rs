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
//! | `SPAWN_NODE_GATEWAY_WORKERS` | `6` | bounded worker threads & PostgreSQL connections |

use std::ffi::OsStr;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Environment variable holding the systemd `LoadCredential=` runtime directory.
pub const ENV_CREDENTIALS_DIRECTORY: &str = "CREDENTIALS_DIRECTORY";
/// Keep credential file ID for the restricted `spawn_node_gateway` PostgreSQL URL.
pub const CREDENTIAL_DATABASE_URL: &str = "spawn-node-gateway.database-url";
/// Exit status (`EX_CONFIG`) returned when a Keep credential file fails validation.
pub const CREDENTIAL_EXIT_CODE: u8 = 78;

/// Configuration error carrying a process exit status (`78` on Keep credential faults, `1` otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Human-readable error diagnostic (never contains secret bytes).
    pub message: String,
    /// Exit status code for the gateway process.
    pub exit_code: u8,
}

/// Environment variable holding the restricted `spawn_node_gateway` PostgreSQL connection URL.
pub const ENV_DATABASE_URL: &str = "SPAWN_NODE_GATEWAY_DATABASE_URL";
/// Environment variable holding the path to the primary's PKCS#8 Ed25519 key file.
pub const ENV_KEY_FILE: &str = "SPAWN_NODE_GATEWAY_KEY_FILE";
/// Environment variable overriding the HTTP listen socket address.
pub const ENV_LISTEN: &str = "SPAWN_NODE_GATEWAY_LISTEN";
/// Environment variable holding the artifact storage root directory.
pub const ENV_ARTIFACTS: &str = "SPAWN_NODE_ARTIFACTS";
/// Environment variable holding the job result directory.
pub const ENV_RESULTS: &str = "SPAWN_NODE_RESULTS";
/// Environment variable listing the comma-separated cell names served by this gateway.
pub const ENV_CELLS: &str = "SPAWN_NODE_GATEWAY_CELLS";
/// Environment variable setting an optional reverse-proxy URL path prefix.
pub const ENV_PUBLIC_PREFIX: &str = "SPAWN_NODE_GATEWAY_PUBLIC_PREFIX";
/// Environment variable overriding the worker thread and DB connection pool count.
pub const ENV_WORKERS: &str = "SPAWN_NODE_GATEWAY_WORKERS";

/// Default listen socket address when [`ENV_LISTEN`] is unset.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:18096";
/// Default cell name when [`ENV_CELLS`] is unset.
pub const DEFAULT_CELL: &str = "c0";
/// Default worker thread (and per-worker DB connection) count safe for 1C/1G.
pub const DEFAULT_WORKERS: usize = 6;

/// Everything the gateway reads at start.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Never printed: it holds the login's password.
    pub database_url: String,
    /// Path to the primary's PKCS#8 Ed25519 key file.
    pub key_file: PathBuf,
    /// TCP socket address the HTTP server binds to.
    pub listen: SocketAddr,
    /// Root directory storing content-addressed artifact blobs and manifests.
    pub artifacts: PathBuf,
    /// Directory storing execution log and metadata files.
    pub results: PathBuf,
    /// Cell identifiers served by this gateway instance.
    pub cells: Vec<String>,
    /// Empty, or `/` and path characters without a trailing `/`.
    pub public_prefix: String,
    /// Number of synchronous worker threads (each holds at most one DB connection).
    pub workers: usize,
    /// Optional libpq passfile path resolved from `PGPASSFILE` or `$HOME/.pgpass`.
    pub passfile: Option<PathBuf>,
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
            .field("workers", &self.workers)
            .field("passfile", &self.passfile)
            .finish()
    }
}

impl Config {
    /// The configuration of `env` (a variable's value by name) and the
    /// command line's arguments after the program name.
    pub fn from_env(env: impl Fn(&str) -> Option<String>, args: &[String]) -> Result<Self, String> {
        Self::parse_with_db_override(env, None, args)
    }

    /// Parse gateway configuration using `env` and `args`, preferring a Keep
    /// `$CREDENTIALS_DIRECTORY/spawn-node-gateway.database-url` file when present.
    ///
    /// If `$CREDENTIALS_DIRECTORY/spawn-node-gateway.database-url` exists and is
    /// empty, unreadable, non-UTF-8, group/world-accessible, contains embedded
    /// newlines/NULs, or is a symlink, this fails closed with [`CREDENTIAL_EXIT_CODE`]
    /// (`78`) without falling back to [`ENV_DATABASE_URL`].
    pub fn from_env_with_credentials(
        env: impl Fn(&str) -> Option<String>,
        args: &[String],
    ) -> Result<Self, ConfigError> {
        let db_override =
            match env(ENV_CREDENTIALS_DIRECTORY).filter(|dir| !dir.trim().is_empty()) {
                Some(dir) => read_credential_value(Path::new(&dir), CREDENTIAL_DATABASE_URL)
                    .map_err(|message| ConfigError {
                        message,
                        exit_code: CREDENTIAL_EXIT_CODE,
                    })?,
                None => None,
            };
        Self::parse_with_db_override(env, db_override, args).map_err(|message| ConfigError {
            message,
            exit_code: 1,
        })
    }

    fn parse_with_db_override(
        env: impl Fn(&str) -> Option<String>,
        db_override: Option<String>,
        args: &[String],
    ) -> Result<Self, String> {
        let required = |name: &str| {
            env(name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{name} is not set"))
        };
        let database_url = match db_override {
            Some(url) => url,
            None => required(ENV_DATABASE_URL)?,
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
        let workers = match env(ENV_WORKERS) {
            None => DEFAULT_WORKERS,
            Some(raw) => raw
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|value| *value >= 1)
                .ok_or_else(|| format!("{ENV_WORKERS}={raw:?} must be an integer >= 1"))?,
        };
        let pgpassfile = env("PGPASSFILE");
        let home = env("HOME");
        let passfile = spawn_postgres::resolve_passfile_path(
            pgpassfile.as_deref().map(OsStr::new),
            home.as_deref().map(OsStr::new),
        );
        Ok(Self {
            database_url,
            key_file: required(ENV_KEY_FILE)?.into(),
            listen,
            artifacts: required(ENV_ARTIFACTS)?.into(),
            results: required(ENV_RESULTS)?.into(),
            cells,
            public_prefix: normalize_prefix(&public_prefix)?,
            workers,
            passfile,
        })
    }
}

fn read_credential_value(cred_dir: &Path, id: &str) -> Result<Option<String>, String> {
    let path = cred_dir.join(id);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("credential {id}: {error}")),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("credential {id} must be a regular file"));
    }
    if meta.len() == 0 {
        return Err(format!("credential {id} is empty"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "credential {id} has mode {mode:o}: must be 0400 or 0600"
            ));
        }
    }
    let bytes = fs::read(&path).map_err(|error| format!("credential {id}: {error}"))?;
    let raw =
        String::from_utf8(bytes).map_err(|_| format!("credential {id} is not valid UTF-8"))?;
    let trimmed = if let Some(rest) = raw.strip_suffix('\n') {
        rest.strip_suffix('\r').unwrap_or(rest)
    } else {
        &raw
    };
    if trimmed.is_empty() {
        return Err(format!("credential {id} is empty"));
    }
    if trimmed.contains(['\n', '\r', '\0']) {
        return Err(format!(
            "credential {id} contains embedded newline or NUL byte"
        ));
    }
    Ok(Some(trimmed.to_string()))
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
        assert_eq!(config.workers, DEFAULT_WORKERS);
        assert_eq!(DEFAULT_WORKERS, 6);
        assert!(
            !format!("{config:?}").contains("pw@"),
            "the URL is never printed"
        );

        let mut pairs = BASE.to_vec();
        pairs.push(("PGPASSFILE", "/run/secrets/pgpass"));
        pairs.push((ENV_CELLS, "c0, c1"));
        pairs.push((ENV_PUBLIC_PREFIX, "/env"));
        pairs.push((ENV_LISTEN, "0.0.0.0:9"));
        pairs.push((ENV_WORKERS, " 12 "));
        let args = ["--public-prefix".to_string(), "/gw/".to_string()];
        let config = Config::from_env(env(&pairs), &args).unwrap();
        assert_eq!(config.cells, ["c0", "c1"]);
        assert_eq!(config.public_prefix, "/gw");
        assert_eq!(config.listen.port(), 9);
        assert_eq!(config.workers, 12);
        assert_eq!(
            config.passfile.as_deref(),
            Some(Path::new("/run/secrets/pgpass"))
        );

        let mut home_pairs = BASE.to_vec();
        home_pairs.push(("HOME", "/home/gw"));
        let home_cfg = Config::from_env(env(&home_pairs), &[]).unwrap();
        assert_eq!(
            home_cfg.passfile.as_deref(),
            Some(Path::new("/home/gw/.pgpass"))
        );
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

        for bad_workers in [
            "",
            "   ",
            "0",
            "-1",
            "abc",
            "1.5",
            "999999999999999999999999",
        ] {
            let mut pairs = BASE.to_vec();
            pairs.push((ENV_WORKERS, bad_workers));
            let err = Config::from_env(env(&pairs), &[]).expect_err("bad workers");
            assert!(err.contains(ENV_WORKERS), "{bad_workers:?}: {err}");
        }
    }

    #[test]
    fn load_credential_database_url_and_fail_closed_checks() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("spawn-gw-cred-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let cred_dir = dir.to_string_lossy().to_string();

        // When CREDENTIALS_DIRECTORY has no credential file, fall back gracefully to env.
        let mut pairs = BASE.to_vec();
        pairs.push((ENV_CREDENTIALS_DIRECTORY, &cred_dir));
        let cfg = Config::from_env_with_credentials(env(&pairs), &[]).expect("fallback");
        assert_eq!(cfg.database_url, "postgres://gw:pw@localhost/spawn");

        // When credential file exists (0400), it takes precedence over ENV_DATABASE_URL.
        let keep_pw = "keep_secret_pass_4921";
        let keep_url = format!("postgres://gw:{keep_pw}@127.0.0.1/keep_db");
        let cred_path = dir.join(CREDENTIAL_DATABASE_URL);
        fs::write(&cred_path, format!("{keep_url}\n")).expect("write");
        fs::set_permissions(&cred_path, fs::Permissions::from_mode(0o400)).expect("chmod 400");
        let cfg = Config::from_env_with_credentials(env(&pairs), &[]).expect("keep url");
        assert_eq!(cfg.database_url, keep_url);
        assert!(!format!("{cfg:?}").contains(keep_pw));

        // Fail-closed cases exit 78 (never falling back to ENV_DATABASE_URL and never leaking value).
        fs::set_permissions(&cred_path, fs::Permissions::from_mode(0o644)).expect("chmod 644");
        let err_mode = Config::from_env_with_credentials(env(&pairs), &[]).unwrap_err();
        assert_eq!(err_mode.exit_code, CREDENTIAL_EXIT_CODE);
        assert!(err_mode.message.contains(CREDENTIAL_DATABASE_URL));
        assert!(!err_mode.message.contains(keep_pw));

        fs::set_permissions(&cred_path, fs::Permissions::from_mode(0o600)).expect("chmod 600");
        fs::write(&cred_path, "\n").expect("write empty newline");
        let err_empty = Config::from_env_with_credentials(env(&pairs), &[]).unwrap_err();
        assert_eq!(err_empty.exit_code, CREDENTIAL_EXIT_CODE);

        fs::write(&cred_path, "postgres://a\npostgres://b\n").expect("write multi");
        let err_multi = Config::from_env_with_credentials(env(&pairs), &[]).unwrap_err();
        assert_eq!(err_multi.exit_code, CREDENTIAL_EXIT_CODE);

        fs::remove_file(&cred_path).expect("rm");
        let target = dir.join("sym-target");
        fs::write(&target, &keep_url).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod");
        std::os::unix::fs::symlink(&target, &cred_path).expect("symlink");
        let err_sym = Config::from_env_with_credentials(env(&pairs), &[]).unwrap_err();
        assert_eq!(err_sym.exit_code, CREDENTIAL_EXIT_CODE);
        assert!(!err_sym.message.contains(keep_pw));

        let _ = fs::remove_dir_all(&dir);
    }
}
