//! The password of a URL that holds none, from an explicit passfile path
//! passed by the caller boundary (objective 109, part 1 of A1: e.g.
//! `PGPASSFILE` when set, else `$HOME/.pgpass`, resolved outside the core).
//!
//! A line is `host:port:database:user:password`; `*` in any of the first
//! four fields matches anything, `\:` and `\\` stand for `:` and `\`, and a
//! line starting with `#` is a comment. The first matching line wins. Host
//! is the URL's TCP host by name or its socket directory by path, and
//! `localhost` also matches a connection over a default socket directory;
//! port is the URL's (default 5432); database defaults to the resolved user
//! when absent; user resolves as `URL user -> explicit default_user -> OS
//! process user fallback via whoami`, written back into `postgres::Config` so
//! driver startup, `.pgpass` matching, and reconnects always agree.
//!
//! A file its group or others can read or write is not used, as libpq
//! refuses it: said once on stderr, by its path. A URL that holds a
//! password, the empty one included, is used as it is: the lookup returns
//! before reading anything. No password is ever printed, and read buffers
//! are zeroized on drop.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::SecretString;

/// Resolve libpq passfile path from explicit boundary environment values
/// (`PGPASSFILE` when non-empty, else `$HOME/.pgpass` when `HOME` is non-empty).
#[must_use]
pub fn resolve_passfile_path(pgpassfile: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = pgpassfile.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    home.filter(|value| !value.is_empty())
        .map(|home_dir| Path::new(home_dir).join(".pgpass"))
}

/// The port a URL without one connects to.
const DEFAULT_PORT: u16 = 5432;

/// The socket directories a connection without a TCP host may use, which
/// libpq's `localhost` line also matches.
const DEFAULT_SOCKET_DIRS: [&str; 3] = ["/tmp", "/var/run/postgresql", "/run/postgresql"];

/// The passfiles already said to be refused: each is said once.
static REFUSED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Error constructing a `postgres::Config` with passfile password lookup.
///
/// Never embeds raw URLs, credentials, or passfile secrets in [`Display`].
#[derive(Debug)]
pub(crate) enum PassfileConfigError {
    /// The database connection string failed to parse as a `postgres::Config`.
    InvalidUrl(postgres::Error),
    /// An explicitly configured passfile existed but could not be read or decoded.
    PassfileRead { path: PathBuf, reason: String },
}

impl std::fmt::Display for PassfileConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(formatter, "{error}"),
            Self::PassfileRead { path, reason } => {
                write!(formatter, "invalid passfile {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for PassfileConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::PassfileRead { .. } => None,
        }
    }
}

/// Resolve the effective PostgreSQL login user identically for driver startup,
/// `.pgpass` matching, and reconnects: explicit URL `user`, else caller-supplied
/// `default_user`, else OS process user (`whoami`).
fn resolve_effective_user(config: &postgres::Config, default_user: Option<&str>) -> Option<String> {
    if let Some(user) = config.get_user().filter(|value| !value.is_empty()) {
        return Some(user.to_string());
    }
    if let Some(user) = default_user.filter(|value| !value.is_empty()) {
        return Some(user.to_string());
    }
    whoami::username().ok().filter(|value| !value.is_empty())
}

/// `url` parsed as the driver reads it, reading a password from `passfile`
/// when the URL itself carries none, with unified user resolution across
/// driver startup, passfile matching, and reconnects.
///
/// # Errors
///
/// When the URL does not parse or an existing passfile cannot be read/decoded.
pub(crate) fn config(
    url: &str,
    passfile: Option<&Path>,
) -> Result<postgres::Config, PassfileConfigError> {
    config_with_default_user(url, passfile, None)
}

/// `url` parsed as the driver reads it, reading a password from `passfile`
/// when the URL itself carries none and writing any resolved user (`config.user`,
/// explicit `default_user`, or OS fallback) back into `postgres::Config` so
/// driver startup, passfile matching, and reconnects always agree.
///
/// # Errors
///
/// When the URL does not parse or an existing passfile cannot be read/decoded.
pub(crate) fn config_with_default_user(
    url: &str,
    passfile: Option<&Path>,
    default_user: Option<&str>,
) -> Result<postgres::Config, PassfileConfigError> {
    let mut config: postgres::Config = url.parse().map_err(PassfileConfigError::InvalidUrl)?;
    let resolved_user = resolve_effective_user(&config, default_user);
    if config.get_user().is_none()
        && let Some(user) = resolved_user.as_deref()
    {
        config.user(user);
    }
    if config.get_password().is_none()
        && let Some(path) = passfile
    {
        fill_from(&mut config, path, resolved_user.as_deref())?;
    }
    Ok(config)
}

/// Give `config`, which has no password, the password of the passfile at
/// `path` when a line matches, propagating typed passfile read/decode errors.
fn fill_from(
    config: &mut postgres::Config,
    path: &Path,
    default_user: Option<&str>,
) -> Result<(), PassfileConfigError> {
    if let Some(password) = lookup(path, &Target::of(config, default_user))? {
        config.password(password.expose_secret());
    }
    Ok(())
}

/// What a passfile line is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// The TCP host's name, or the socket directory's path.
    host: String,
    /// Whether the connection is over a default socket directory, which a
    /// `localhost` line also matches.
    default_socket: bool,
    port: String,
    database: Option<String>,
    user: Option<String>,
}

impl Target {
    /// The effective host/hostaddr and port of `config`, its database and user.
    fn of(config: &postgres::Config, fallback_user: Option<&str>) -> Self {
        let (host, socket) = match config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(name)) => (name.clone(), false),
            Some(postgres::config::Host::Unix(path)) => (path.display().to_string(), true),
            None => match config.get_hostaddrs().first() {
                Some(ip) => (ip.to_string(), false),
                None => (String::from("localhost"), true),
            },
        };
        let default_socket = socket
            && (host == "localhost"
                || DEFAULT_SOCKET_DIRS
                    .iter()
                    .any(|dir| Path::new(&host) == Path::new(dir)));
        let user = config.get_user().map(str::to_string).or_else(|| {
            fallback_user
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
        let database = config
            .get_dbname()
            .map(str::to_string)
            .or_else(|| user.clone());
        Self {
            host,
            default_socket,
            port: config
                .get_ports()
                .first()
                .copied()
                .unwrap_or(DEFAULT_PORT)
                .to_string(),
            database,
            user,
        }
    }
}

/// The password of the first line of the passfile at `path` matching
/// `target`; `Ok(None)` when the file does not exist, is not a regular file,
/// has group/world permissions, or has no matching line. Propagates typed read/
/// UTF-8 decode errors so callers never degrade corrupt passfiles into missing-password
/// misses.
fn lookup(path: &Path, target: &Target) -> Result<Option<SecretString>, PassfileConfigError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(PassfileConfigError::PassfileRead {
                path: path.to_path_buf(),
                reason: error.to_string(),
            });
        }
    };
    if !meta.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            refused(path);
            return Ok(None);
        }
    }
    let raw = std::fs::read_to_string(path).map_err(|error| PassfileConfigError::PassfileRead {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let text = SecretString::new(raw);
    Ok(password_in(text.expose_secret(), target))
}

/// Say once that the passfile at `path` is not used, by its path.
fn refused(path: &Path) {
    let mut said = REFUSED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if said.insert(path.to_path_buf()) {
        eprintln!(
            "spawn: passfile {} has group or world access; not used (chmod 600)",
            path.display()
        );
    }
}

/// The password of the first line of `text` matching `target`.
fn password_in(text: &str, target: &Target) -> Option<SecretString> {
    text.lines().find_map(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('#') || line.is_empty() {
            return None;
        }
        let fields = fields(line);
        let [host, port, database, user, password] = fields.as_slice() else {
            return None;
        };
        let host_matches = matches_str(host, &target.host)
            || (target.default_socket && host.text.expose_secret() == "localhost" && !host.escaped);
        (host_matches
            && matches_str(port, &target.port)
            && matches_opt(database, target.database.as_deref())
            && matches_opt(user, target.user.as_deref()))
        .then(|| password.text.clone())
    })
}

/// One field of a line, unescaped and zeroized/redacted, and whether it was spelt
/// with an escape (an escaped `\*` is a literal star, not the wildcard).
#[derive(Debug, Clone)]
struct Field {
    text: SecretString,
    escaped: bool,
}

impl Field {
    fn from_raw(raw: String, escaped: bool) -> Self {
        Self {
            text: SecretString::new(raw),
            escaped,
        }
    }
}

/// `field` matches exact non-optional `value`: the wildcard `*` matches anything.
fn matches_str(field: &Field, value: &str) -> bool {
    let text = field.text.expose_secret();
    (text == "*" && !field.escaped) || text == value
}

/// `field` matches optional `value`: `*` matches even when absent, whereas a
/// concrete field only matches `Some(field)`.
fn matches_opt(field: &Field, value: Option<&str>) -> bool {
    let text = field.text.expose_secret();
    (text == "*" && !field.escaped) || value == Some(text)
}

/// A line split at its unescaped colons into at most five fields, the last
/// the rest of the line; `\x` stands for `x`.
fn fields(line: &str) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    escaped = true;
                }
            }
            ':' if fields.len() < 4 => {
                fields.push(Field::from_raw(std::mem::take(&mut current), escaped));
                escaped = false;
            }
            c => current.push(c),
        }
    }
    fields.push(Field::from_raw(current, escaped));
    fields
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{
        SecretString, Target, config_with_default_user, fields, fill_from, lookup, password_in,
    };

    fn target(host: &str, socket: bool) -> Target {
        Target {
            host: host.into(),
            default_socket: socket,
            port: "5432".into(),
            database: Some("spawn".into()),
            user: Some("ckl".into()),
        }
    }

    #[test]
    fn the_first_matching_line_wins_past_comments_and_other_users() {
        let text = "# a comment\nhost:5432:spawn:other:nope\n\
                    host:5432:spawn:ckl:first\nhost:5432:spawn:ckl:second\n";
        assert_eq!(
            password_in(text, &target("host", false))
                .as_ref()
                .map(SecretString::expose_secret),
            Some("first")
        );
        assert_eq!(
            password_in(text, &target("elsewhere", false))
                .as_ref()
                .map(SecretString::expose_secret),
            None
        );
    }

    #[test]
    fn a_star_matches_anything_and_an_escaped_star_only_a_star() {
        let tcp = target("db.example", false);
        assert_eq!(
            password_in("*:*:*:*:pw", &tcp)
                .as_ref()
                .map(SecretString::expose_secret),
            Some("pw")
        );
        assert!(password_in("\\*:*:*:*:pw", &tcp).is_none());
        assert_eq!(
            password_in("*:5433:*:*:pw\n*:5432:spawn:*:here", &tcp)
                .as_ref()
                .map(SecretString::expose_secret),
            Some("here")
        );
    }

    #[test]
    fn escapes_stand_for_a_colon_and_a_backslash() {
        assert_eq!(
            password_in("*:*:*:*:a\\:b\\\\c", &target("h", false))
                .as_ref()
                .map(SecretString::expose_secret),
            Some("a:b\\c")
        );
        assert_eq!(
            password_in("h\\:x:*:*:*:pw", &target("h:x", false))
                .as_ref()
                .map(SecretString::expose_secret),
            Some("pw")
        );
        let parsed = fields("a:b:c:d:e:secret_tail");
        assert_eq!(parsed.len(), 5, "the password keeps its colons");
        assert!(
            !format!("{parsed:?}").contains("secret_tail"),
            "Field debug output never prints secrets"
        );
        assert_eq!(
            password_in("*:*:*:*:e:f", &target("h", false))
                .as_ref()
                .map(SecretString::expose_secret),
            Some("e:f")
        );
    }

    #[test]
    fn localhost_matches_a_default_socket_and_a_path_its_own_socket() {
        let socket = target("/var/run/postgresql", true);
        assert_eq!(
            password_in("localhost:5432:spawn:ckl:pw", &socket)
                .as_ref()
                .map(SecretString::expose_secret),
            Some("pw")
        );
        assert_eq!(
            password_in("/var/run/postgresql:5432:spawn:ckl:pw", &socket)
                .as_ref()
                .map(SecretString::expose_secret),
            Some("pw")
        );
        let other = target("/srv/sock", false);
        assert!(password_in("localhost:5432:spawn:ckl:pw", &other).is_none());
        assert!(password_in("localhost:5432:spawn:ckl:pw", &target("127.0.0.1", false)).is_none());
    }

    #[test]
    fn a_short_line_is_no_match() {
        assert!(password_in("host:5432:spawn", &target("host", false)).is_none());
    }

    #[test]
    fn a_file_others_can_read_is_not_used_and_a_private_one_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = std::env::temp_dir().join(format!(
            "spawn-passfile-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::write(&path, "*:*:*:*:pw\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let refused = lookup(&path, &target("h", false)).expect("0644 refused returns Ok(None)");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let used = lookup(&path, &target("h", false)).expect("valid passfile");
        let missing =
            lookup(&path.with_extension("none"), &target("h", false)).expect("missing is Ok(None)");
        let _ = std::fs::remove_file(&path);
        assert!(refused.is_none());
        assert_eq!(used.as_ref().map(SecretString::expose_secret), Some("pw"));
        assert!(missing.is_none());
    }

    #[test]
    fn undecodable_mode_0600_passfile_returns_typed_read_error() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = std::env::temp_dir().join(format!(
            "spawn-passfile-badutf8-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::write(&path, [0xff_u8, 0xfe, b'\n']).expect("write invalid bytes");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let lookup_err = lookup(&path, &target("h", false))
            .expect_err("undecodable passfile must fail closed with PassfileRead");
        assert!(matches!(
            lookup_err,
            super::PassfileConfigError::PassfileRead { .. }
        ));
        let cfg_err = super::config("postgres://u@h/db", Some(&path))
            .expect_err("config() must propagate PassfileRead");
        assert!(matches!(
            cfg_err,
            super::PassfileConfigError::PassfileRead { .. }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_target_is_the_urls_first_host_port_database_and_user() {
        let config: postgres::Config = "postgres://ckl@db.example:6000/spawn"
            .parse()
            .expect("parses");
        let got = Target::of(&config, None);
        assert_eq!(
            (got.host.as_str(), got.default_socket),
            ("db.example", false)
        );
        assert_eq!(got.port, "6000");
        assert_eq!(
            (got.database.as_deref(), got.user.as_deref()),
            (Some("spawn"), Some("ckl"))
        );
        let config: postgres::Config = "host=/var/run/postgresql user=ckl".parse().expect("parses");
        let got = Target::of(&config, None);
        assert!(got.default_socket);
        assert_eq!(got.port, "5432");
        assert_eq!(
            got.database.as_deref(),
            Some("ckl"),
            "the database defaults to the user"
        );
    }

    #[test]
    fn url_without_user_never_falls_back_to_ambient_os_user() {
        use std::os::unix::fs::PermissionsExt as _;

        let url_no_user: postgres::Config = "postgres://db.example/spawn".parse().expect("parses");
        let target_no_user = Target::of(&url_no_user, None);
        assert_eq!(target_no_user.user, None);
        assert_eq!(target_no_user.database.as_deref(), Some("spawn"));
        assert!(
            password_in("db.example:5432:spawn::empty_user_pw", &target_no_user).is_none(),
            "absent user never matches empty user field"
        );
        assert_eq!(
            password_in("db.example:5432:spawn:*:wildcard_pw", &target_no_user)
                .as_ref()
                .map(SecretString::expose_secret),
            Some("wildcard_pw")
        );

        let url_neither: postgres::Config = "host=db.example".parse().expect("parses");
        let target_neither = Target::of(&url_neither, None);
        assert_eq!(target_neither.user, None);
        assert_eq!(target_neither.database, None);
        assert!(password_in("db.example:5432:::empty_pw", &target_neither).is_none());

        let with_explicit = Target::of(&url_neither, Some("alice"));
        assert_eq!(with_explicit.user.as_deref(), Some("alice"));
        assert_eq!(with_explicit.database.as_deref(), Some("alice"));

        let path = std::env::temp_dir().join(format!(
            "spawn-passfile-explicit-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::write(&path, "db.example:5432:alice:alice:explicit_user_pw\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let implicit = super::config("postgres://db.example/alice", Some(&path)).expect("parses");
        assert_eq!(implicit.get_password(), None);
        let explicit = config_with_default_user("host=db.example", Some(&path), Some("alice"))
            .expect("parses");
        assert_eq!(explicit.get_password(), Some(&b"explicit_user_pw"[..]));
        assert_eq!(explicit.get_user(), Some("alice"));

        let os_default = super::config("postgres://db.example/alice", None).expect("parses");
        let expected_os_user = whoami::username().ok().filter(|v| !v.is_empty());
        assert_eq!(
            os_default.get_user().map(str::to_string),
            expected_os_user,
            "config() resolves and populates the same OS user that startup uses when URL omits user"
        );
        let _ = std::fs::remove_file(&path);
    }

    struct TempScramEnv {
        admin: postgres::Client,
        temp_db: String,
        role_to_drop: Option<String>,
        dir: std::path::PathBuf,
    }
    impl Drop for TempScramEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            let _ = self.admin.batch_execute(&format!(
                "drop database if exists \"{}\" with (force)",
                self.temp_db
            ));
            if let Some(ref role) = self.role_to_drop {
                let ident = format!("\"{}\"", role.replace('"', "\"\""));
                let _ = self
                    .admin
                    .batch_execute(&format!("drop role if exists {ident}"));
            }
        }
    }

    fn current_db_user(store: &crate::PostgresStore) -> String {
        store
            .lock()
            .query_one("select current_user::text", &[])
            .expect("current_user")
            .get(0)
    }

    #[test]
    fn live_scram_connects_when_url_omits_username_via_mode_0600_passfile() {
        use std::os::unix::fs::PermissionsExt as _;

        let Ok(url) = std::env::var("SPAWN_TEST_DATABASE_URL") else {
            return;
        };
        let os_user = whoami::username().expect("process username");
        let full: postgres::Config = url.parse().expect("parse test url");
        let host = match full.get_hosts().first() {
            Some(postgres::config::Host::Tcp(h)) => h.clone(),
            Some(postgres::config::Host::Unix(p)) => p.display().to_string(),
            None => "localhost".to_string(),
        };
        let port = full.get_ports().first().copied().unwrap_or(5432);
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("admin connect");
        let nonce = format!(
            "{}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let temp_db = format!("spawn_pgpass_nouser_{nonce}");
        let role_pw = format!("pgpass_scram_pw_{nonce}");
        let role_exists: bool = admin
            .query_one(
                "select exists (select 1 from pg_roles where rolname = $1)",
                &[&os_user],
            )
            .expect("check role")
            .get(0);
        assert!(
            !role_exists || full.get_user() == Some(os_user.as_str()),
            "test cluster unexpectedly already has OS user role {os_user:?}"
        );
        let role_ident = format!("\"{}\"", os_user.replace('"', "\"\""));
        if !role_exists {
            admin
                .batch_execute(&format!(
                    "create role {role_ident} login password '{}'",
                    role_pw.replace('\'', "''")
                ))
                .expect("create OS-user role");
        }
        let pass_for_file = if role_exists {
            String::from_utf8_lossy(full.get_password().unwrap_or_default()).into_owned()
        } else {
            role_pw
        };
        admin
            .batch_execute(&format!("create database \"{temp_db}\" owner {role_ident}"))
            .expect("create temp db");
        let dir = std::env::temp_dir().join(format!("spawn-pgpass-nouser-{nonce}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pf = dir.join("pgpass");
        let _cleanup = TempScramEnv {
            admin,
            temp_db: temp_db.clone(),
            role_to_drop: (!role_exists).then(|| os_user.clone()),
            dir,
        };
        let line = format!(
            "{}:{port}:{}:{}:{}\n",
            escaped(&host),
            escaped(&temp_db),
            escaped(&os_user),
            escaped(&pass_for_file)
        );
        std::fs::write(&pf, line).expect("write pgpass");
        std::fs::set_permissions(&pf, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let omitted_user_url = if host.starts_with('/') {
            format!("postgres://{}:{port}/{temp_db}", host.replace('/', "%2F"))
        } else {
            format!("postgres://{host}:{port}/{temp_db}")
        };
        let opts = crate::ConnectOptions {
            passfile: Some(pf),
            statement_timeout: None,
        };
        let store = crate::PostgresStore::connect_with_options(&omitted_user_url, &opts)
            .expect("omitted-username URL must connect via passfile + OS user");
        assert_eq!(current_db_user(&store), os_user);

        let child = store
            .new_connection()
            .expect("new_connection reconnects cleanly without URL username");
        assert_eq!(current_db_user(&child), os_user);

        let _connector_store = store
            .connector(std::time::Duration::from_secs(5))
            .connect()
            .expect("StoreConnector connects without URL username");
    }

    /// A field written as a passfile writes it: `:` and `\` escaped.
    fn escaped(value: &str) -> String {
        value.replace('\\', "\\\\").replace(':', "\\:")
    }

    /// The test database's own login, its password taken out of the URL
    /// and put in a 0600 passfile line for its host, port, database and
    /// user: the connection logs in with the passfile's password. Nothing
    /// is printed but the outcome.
    #[test]
    fn a_url_without_its_password_logs_in_with_the_passfiles() {
        use std::os::unix::fs::PermissionsExt as _;
        let Ok(url) = std::env::var("SPAWN_TEST_DATABASE_URL") else {
            return;
        };
        let full: postgres::Config = url.parse().expect("the test URL parses");
        let Some(password) = full
            .get_password()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        else {
            return;
        };
        let target = Target::of(&full, None);
        let (Some(database), Some(user)) = (target.database.as_deref(), target.user.as_deref())
        else {
            return;
        };
        let path = std::env::temp_dir().join(format!(
            "spawn-passfile-login-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let host = if target.default_socket {
            "localhost".to_string()
        } else {
            escaped(&target.host)
        };
        let line = format!(
            "# spawn's test\n*:*:*:someone_else:nope\n{host}:{}:{}:{}:{}\n",
            target.port,
            escaped(database),
            escaped(user),
            escaped(&password)
        );
        std::fs::write(&path, line).expect("write the passfile");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let mut without = postgres::Config::new();
        for host in full.get_hosts() {
            match host {
                postgres::config::Host::Tcp(name) => without.host(name),
                postgres::config::Host::Unix(dir) => without.host_path(dir),
            };
        }
        for port in full.get_ports() {
            without.port(*port);
        }
        if let Some(user) = full.get_user() {
            without.user(user);
        }
        if let Some(database) = full.get_dbname() {
            without.dbname(database);
        }
        let before = without.get_password().is_none();
        fill_from(&mut without, &path, None).expect("fill_from valid passfile");
        let filled = without.get_password() == full.get_password();
        let connected = without.connect(postgres::NoTls).is_ok();
        let _ = std::fs::remove_file(&path);
        assert!(before, "the copy holds no password");
        assert!(
            filled,
            "the passfile's line matched the URL's own host, port, database and user"
        );
        assert!(connected, "the passfile's password logs in");
    }

    #[test]
    fn resolve_passfile_path_prefers_pgpassfile_then_home_pgpass() {
        use std::ffi::OsStr;
        use std::path::PathBuf;

        assert_eq!(
            super::resolve_passfile_path(
                Some(OsStr::new("/run/secrets/pgpass")),
                Some(OsStr::new("/home/u")),
            ),
            Some(PathBuf::from("/run/secrets/pgpass"))
        );
        assert_eq!(
            super::resolve_passfile_path(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.pgpass"))
        );
        assert_eq!(
            super::resolve_passfile_path(None, Some(OsStr::new("/home/u"))),
            Some(PathBuf::from("/home/u/.pgpass"))
        );
        assert_eq!(
            super::resolve_passfile_path(Some(OsStr::new("")), Some(OsStr::new(""))),
            None
        );
        assert_eq!(super::resolve_passfile_path(None, None), None);
    }
}
