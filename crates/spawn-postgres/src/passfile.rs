//! The password of a URL that holds none, from a passfile, as libpq reads
//! one (objective 109, part 1 of A1): `PGPASSFILE` when set, else
//! `$HOME/.pgpass`.
//!
//! A line is `host:port:database:user:password`; `*` in any of the first
//! four fields matches anything, `\:` and `\\` stand for `:` and `\`, and a
//! line starting with `#` is a comment. The first matching line wins. Host
//! is the URL's TCP host by name or its socket directory by path, and
//! `localhost` also matches a connection over a default socket directory;
//! port is the URL's (default 5432); database and user are the URL's, the
//! user defaulting as the driver defaults it and the database to the user.
//!
//! A file its group or others can read or write is not used, as libpq
//! refuses it: said once on stderr, by its path. A URL that holds a
//! password, the empty one included, is used as it is: the lookup returns
//! before reading anything. No password is ever printed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The port a URL without one connects to.
const DEFAULT_PORT: u16 = 5432;

/// The socket directories a connection without a TCP host may use, which
/// libpq's `localhost` line also matches.
const DEFAULT_SOCKET_DIRS: [&str; 3] = ["/tmp", "/var/run/postgresql", "/run/postgresql"];

/// The passfiles already said to be refused: each is said once.
static REFUSED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// `url` parsed as the driver reads it, with the passfile's password when
/// it holds none.
///
/// # Errors
///
/// When the URL does not parse.
pub(crate) fn config(url: &str) -> Result<postgres::Config, postgres::Error> {
    let mut config: postgres::Config = url.parse()?;
    fill(&mut config);
    Ok(config)
}

/// Give `config` the passfile's password when it has none.
pub(crate) fn fill(config: &mut postgres::Config) {
    if config.get_password().is_some() {
        return;
    }
    if let Some(path) = passfile_path() {
        fill_from(config, &path);
    }
}

/// Give `config`, which has no password, the password of the passfile at
/// `path` when a line matches.
fn fill_from(config: &mut postgres::Config, path: &Path) {
    if let Some(password) = lookup(path, &Target::of(config)) {
        config.password(password);
    }
}

/// `PGPASSFILE` when set and not empty, else `$HOME/.pgpass`.
fn passfile_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PGPASSFILE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| Path::new(&home).join(".pgpass"))
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
    database: String,
    user: String,
}

impl Target {
    /// The first host and port of `config`, its database and user.
    fn of(config: &postgres::Config) -> Self {
        let (host, socket) = match config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(name)) => (name.clone(), false),
            Some(postgres::config::Host::Unix(path)) => (path.display().to_string(), true),
            None => (String::from("localhost"), true),
        };
        let default_socket = socket
            && (host == "localhost"
                || DEFAULT_SOCKET_DIRS
                    .iter()
                    .any(|dir| Path::new(&host) == Path::new(dir)));
        let user = config
            .get_user()
            .map(str::to_string)
            .or_else(|| whoami::username().ok())
            .unwrap_or_default();
        Self {
            host,
            default_socket,
            port: config
                .get_ports()
                .first()
                .copied()
                .unwrap_or(DEFAULT_PORT)
                .to_string(),
            database: config
                .get_dbname()
                .map_or_else(|| user.clone(), str::to_string),
            user,
        }
    }
}

/// The password of the first line of the passfile at `path` matching
/// `target`; `None` when the file is missing, refused or has no match.
fn lookup(path: &Path, target: &Target) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            refused(path);
            return None;
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    password_in(&text, target)
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
fn password_in(text: &str, target: &Target) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('#') || line.is_empty() {
            return None;
        }
        let fields = fields(line);
        let [host, port, database, user, password] = fields.as_slice() else {
            return None;
        };
        let host_matches = matches(host, &target.host)
            || (target.default_socket && host.text == "localhost" && !host.escaped);
        (host_matches
            && matches(port, &target.port)
            && matches(database, &target.database)
            && matches(user, &target.user))
        .then(|| password.text.clone())
    })
}

/// One field of a line, unescaped, and whether it was spelt with an escape
/// (an escaped `\*` is a literal star, not the wildcard).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    text: String,
    escaped: bool,
}

/// `field` matches `value`: the wildcard `*` matches anything.
fn matches(field: &Field, value: &str) -> bool {
    (field.text == "*" && !field.escaped) || field.text == value
}

/// A line split at its unescaped colons into at most five fields, the last
/// the rest of the line; `\x` stands for `x`.
fn fields(line: &str) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut current = Field {
        text: String::new(),
        escaped: false,
    };
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.text.push(next);
                    current.escaped = true;
                }
            }
            ':' if fields.len() < 4 => {
                fields.push(std::mem::replace(
                    &mut current,
                    Field {
                        text: String::new(),
                        escaped: false,
                    },
                ));
            }
            c => current.text.push(c),
        }
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod tests {
    use super::{Target, fields, fill_from, lookup, password_in};

    fn target(host: &str, socket: bool) -> Target {
        Target {
            host: host.into(),
            default_socket: socket,
            port: "5432".into(),
            database: "spawn".into(),
            user: "ckl".into(),
        }
    }

    #[test]
    fn the_first_matching_line_wins_past_comments_and_other_users() {
        let text = "# a comment\nhost:5432:spawn:other:nope\n\
                    host:5432:spawn:ckl:first\nhost:5432:spawn:ckl:second\n";
        assert_eq!(
            password_in(text, &target("host", false)).as_deref(),
            Some("first")
        );
        assert_eq!(password_in(text, &target("elsewhere", false)), None);
    }

    #[test]
    fn a_star_matches_anything_and_an_escaped_star_only_a_star() {
        let tcp = target("db.example", false);
        assert_eq!(password_in("*:*:*:*:pw", &tcp).as_deref(), Some("pw"));
        assert_eq!(password_in("\\*:*:*:*:pw", &tcp), None);
        assert_eq!(
            password_in("*:5433:*:*:pw\n*:5432:spawn:*:here", &tcp).as_deref(),
            Some("here")
        );
    }

    #[test]
    fn escapes_stand_for_a_colon_and_a_backslash() {
        assert_eq!(
            password_in("*:*:*:*:a\\:b\\\\c", &target("h", false)).as_deref(),
            Some("a:b\\c")
        );
        assert_eq!(
            password_in("h\\:x:*:*:*:pw", &target("h:x", false)).as_deref(),
            Some("pw")
        );
        assert_eq!(
            fields("a:b:c:d:e:f").len(),
            5,
            "the password keeps its colons"
        );
        assert_eq!(
            password_in("*:*:*:*:e:f", &target("h", false)).as_deref(),
            Some("e:f")
        );
    }

    #[test]
    fn localhost_matches_a_default_socket_and_a_path_its_own_socket() {
        let socket = target("/var/run/postgresql", true);
        assert_eq!(
            password_in("localhost:5432:spawn:ckl:pw", &socket).as_deref(),
            Some("pw")
        );
        assert_eq!(
            password_in("/var/run/postgresql:5432:spawn:ckl:pw", &socket).as_deref(),
            Some("pw")
        );
        let other = target("/srv/sock", false);
        assert_eq!(password_in("localhost:5432:spawn:ckl:pw", &other), None);
        assert_eq!(
            password_in("localhost:5432:spawn:ckl:pw", &target("127.0.0.1", false)),
            None
        );
    }

    #[test]
    fn a_short_line_is_no_match() {
        assert_eq!(password_in("host:5432:spawn", &target("host", false)), None);
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
        let refused = lookup(&path, &target("h", false));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let used = lookup(&path, &target("h", false));
        let missing = lookup(&path.with_extension("none"), &target("h", false));
        let _ = std::fs::remove_file(&path);
        assert_eq!(refused, None);
        assert_eq!(used.as_deref(), Some("pw"));
        assert_eq!(missing, None);
    }

    #[test]
    fn a_target_is_the_urls_first_host_port_database_and_user() {
        let config: postgres::Config = "postgres://ckl@db.example:6000/spawn"
            .parse()
            .expect("parses");
        let got = Target::of(&config);
        assert_eq!(
            (got.host.as_str(), got.default_socket),
            ("db.example", false)
        );
        assert_eq!(got.port, "6000");
        assert_eq!((got.database.as_str(), got.user.as_str()), ("spawn", "ckl"));
        let config: postgres::Config = "host=/var/run/postgresql user=ckl".parse().expect("parses");
        let got = Target::of(&config);
        assert!(got.default_socket);
        assert_eq!(got.port, "5432");
        assert_eq!(got.database, "ckl", "the database defaults to the user");
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
        let target = Target::of(&full);
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
            escaped(&target.database),
            escaped(&target.user),
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
        fill_from(&mut without, &path);
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
}
