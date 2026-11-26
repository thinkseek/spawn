//! Local HTTP/1.1-over-UDS IPC server (`spawn-ipc-v1`, Option B2; `docs/spawn-ipc-v1.md`).
//!
//! Binds `$XDG_RUNTIME_DIR/spawn/core.sock` (or `--ipc-sock <path>`) atomically
//! (`flock` on `<sock>.lock` -> bind temp socket -> `chmod 0600` -> `rename`),
//! verifies Linux `SO_PEERCRED` (`peer_uid == geteuid()`), derives role-scoped
//! PostgreSQL URLs for `spawn_reader` / `spawn_human` / `spawn_node_gateway`
//! once at startup, drops the owner URL immediately, and isolates the four
//! admission pools: `reads(16)`, `writes(4)`, `node(16)`, `events(2)`.

mod events;
mod http;
mod node;
mod routes;
#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use spawn_postgres::{PostgresStore, role_url};
use zeroize::Zeroizing;

use self::http::{ReadOutcome, read_request, write_json_error};
use self::routes::handle_connection_request;
use crate::serve::STATEMENT_TIMEOUT;
use crate::{Flags, env_value};

pub(super) const READS_POOL_SIZE: usize = 16;
pub(super) const WRITES_POOL_SIZE: usize = 4;
pub(super) const NODE_POOL_SIZE: usize = 16;
pub(super) const EVENTS_POOL_SIZE: usize = 2;

const IDLE_TIMEOUT: Duration = Duration::from_mins(1);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

static BIND_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Scoped role URLs derived once at startup; owner `SPAWN_DATABASE_URL` is
/// never retained on [`IpcState`].
pub(super) struct RoleUrls {
    pub(super) reader: Zeroizing<String>,
    pub(super) human: Zeroizing<String>,
    pub(super) node_gateway: Zeroizing<String>,
}

impl RoleUrls {
    pub(super) fn from_owner(owner_url: &str) -> Result<Self, String> {
        let reader = Zeroizing::new(role_url(owner_url, "spawn_reader")?);
        let human = Zeroizing::new(role_url(owner_url, "spawn_human")?);
        let node_gateway = Zeroizing::new(role_url(owner_url, "spawn_node_gateway")?);
        Ok(Self {
            reader,
            human,
            node_gateway,
        })
    }

    pub(super) fn from_explicit(reader: String, human: String, node_gateway: String) -> Self {
        Self {
            reader: Zeroizing::new(reader),
            human: Zeroizing::new(human),
            node_gateway: Zeroizing::new(node_gateway),
        }
    }
}

pub(super) struct PoolGate {
    max: usize,
    active: Mutex<usize>,
    cond: Condvar,
}

impl PoolGate {
    fn new(max: usize) -> Self {
        Self {
            max,
            active: Mutex::new(0),
            cond: Condvar::new(),
        }
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<PoolPermit> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if *active >= self.max {
            return None;
        }
        *active += 1;
        Some(PoolPermit {
            gate: Arc::clone(self),
        })
    }
}

pub(super) struct PoolPermit {
    gate: Arc<PoolGate>,
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *active = active.saturating_sub(1);
        self.gate.cond.notify_one();
    }
}

pub(super) struct RoleConnPool {
    url: Zeroizing<String>,
    conns: Mutex<Vec<PostgresStore>>,
}

pub(crate) struct CheckedOutConn<'a> {
    pool: &'a RoleConnPool,
    inner: Option<PostgresStore>,
}

impl std::ops::Deref for CheckedOutConn<'_> {
    type Target = PostgresStore;

    fn deref(&self) -> &Self::Target {
        self.inner
            .as_ref()
            .expect("checked-out connection present until drop")
    }
}

impl Drop for CheckedOutConn<'_> {
    fn drop(&mut self) {
        if let Some(store) = self.inner.take()
            && let Ok(mut guard) = self.pool.conns.lock()
            && guard.len() < 16
        {
            guard.push(store);
        }
    }
}

impl RoleConnPool {
    fn new(url: Zeroizing<String>) -> Self {
        Self {
            url,
            conns: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn checkout(&self) -> Result<CheckedOutConn<'_>, String> {
        if let Some(store) = self
            .conns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
        {
            return Ok(CheckedOutConn {
                pool: self,
                inner: Some(store),
            });
        }
        let store = PostgresStore::connect_with_statement_timeout(&self.url, STATEMENT_TIMEOUT)
            .map_err(|e| e.to_string())?;
        Ok(CheckedOutConn {
            pool: self,
            inner: Some(store),
        })
    }
}

fn with_schema_search_path(url: &str, schema: &str) -> String {
    let sep = if url.contains('?') { "&" } else { "?" };
    format!("{url}{sep}options=-csearch_path%3D{schema}")
}

pub(super) struct IpcPools {
    pub(super) reads_gate: Arc<PoolGate>,
    pub(super) writes_gate: Arc<PoolGate>,
    pub(super) node_gate: Arc<PoolGate>,
    pub(super) events_gate: Arc<PoolGate>,
    pub(super) reader: RoleConnPool,
    pub(super) human: RoleConnPool,
    pub(super) node: RoleConnPool,
}

impl IpcPools {
    fn new(urls: &RoleUrls) -> Self {
        Self {
            reads_gate: Arc::new(PoolGate::new(READS_POOL_SIZE)),
            writes_gate: Arc::new(PoolGate::new(WRITES_POOL_SIZE)),
            node_gate: Arc::new(PoolGate::new(NODE_POOL_SIZE)),
            events_gate: Arc::new(PoolGate::new(EVENTS_POOL_SIZE)),
            reader: RoleConnPool::new(urls.reader.clone()),
            human: RoleConnPool::new(urls.human.clone()),
            node: RoleConnPool::new(urls.node_gateway.clone()),
        }
    }
}

pub(super) struct IpcState {
    pub(super) urls: RoleUrls,
    pub(super) pools: IpcPools,
    pub(super) cas_root: PathBuf,
    pub(super) epoch: String,
}

impl IpcState {
    fn new(mut urls: RoleUrls, cas_root: PathBuf, schema: Option<&str>) -> Self {
        if let Some(schema) = schema {
            urls = RoleUrls::from_explicit(
                with_schema_search_path(&urls.reader, schema),
                with_schema_search_path(&urls.human, schema),
                with_schema_search_path(&urls.node_gateway, schema),
            );
        }
        let pools = IpcPools::new(&urls);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let epoch = format!("boot-{now_ms:x}-{}", std::process::id());
        Self {
            urls,
            pools,
            cas_root,
            epoch,
        }
    }
}

#[cfg(test)]
pub(crate) struct IpcServer {
    sock_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
    _lock_file: File,
}

#[cfg(test)]
impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.sock_path);
    }
}

pub(crate) fn default_socket_path() -> Result<PathBuf, String> {
    if let Some(explicit) = env_value("SPAWN_IPC_SOCK") {
        return Ok(PathBuf::from(explicit));
    }
    let runtime = env_value("XDG_RUNTIME_DIR").ok_or_else(|| {
        "neither --ipc-sock/SPAWN_IPC_SOCK nor XDG_RUNTIME_DIR is set".to_string()
    })?;
    Ok(PathBuf::from(runtime).join("spawn").join("core.sock"))
}

pub(crate) fn default_cas_root() -> PathBuf {
    if let Some(cas) = env_value("SPAWN_CAS_ROOT") {
        return PathBuf::from(cas);
    }
    PathBuf::from("/var/lib/spawn/cas")
}

/// Atomically bind `sock_path`: take exclusive `flock` on sibling `.lock`,
/// bind temporary socket, set mode `0600`, then `rename` onto `sock_path`.
pub(super) fn bind_uds(sock_path: &Path) -> Result<(UnixListener, File), String> {
    let parent = sock_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("invalid socket path {}", sock_path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod 0700 {}: {e}", parent.display()))?;

    let sock_name = sock_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid socket filename {}", sock_path.display()))?;
    let lock_path = parent.join(format!("{sock_name}.lock"));
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    flock(&lock_file, FlockOperation::LockExclusive)
        .map_err(|e| format!("flock {}: {e}", lock_path.display()))?;

    let seq = BIND_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{sock_name}.tmp.{}.{seq}", std::process::id()));
    let _ = fs::remove_file(&tmp_path);
    let listener =
        UnixListener::bind(&tmp_path).map_err(|e| format!("bind {}: {e}", tmp_path.display()))?;
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod 0600 {}: {e}", tmp_path.display()))?;
    fs::rename(&tmp_path, sock_path).map_err(|e| {
        format!(
            "rename {} -> {}: {e}",
            tmp_path.display(),
            sock_path.display()
        )
    })?;
    Ok((listener, lock_file))
}

#[cfg(target_os = "linux")]
pub(super) fn verify_peer_uid(stream: &UnixStream) -> Result<(), String> {
    let cred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| format!("SO_PEERCRED failed: {e}"))?;
    let expected = rustix::process::geteuid();
    if cred.uid != expected {
        return Err(format!(
            "peer uid {} does not match euid {}",
            cred.uid.as_raw(),
            expected.as_raw()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn verify_peer_uid(_stream: &UnixStream) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn spawn_ipc_server(
    sock_path: PathBuf,
    urls: RoleUrls,
    cas_root: PathBuf,
    schema: Option<&str>,
) -> Result<IpcServer, String> {
    let (listener, lock_file) = bind_uds(&sock_path)?;
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("set blocking: {e}"))?;
    let state = Arc::new(IpcState::new(urls, cas_root, schema));
    let shutdown = Arc::new(AtomicBool::new(false));
    let accept_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::Builder::new()
        .name("ipc-accept".to_string())
        .spawn(move || accept_loop(&listener, &state, &accept_shutdown))
        .map_err(|e| format!("spawn ipc-accept: {e}"))?;
    Ok(IpcServer {
        sock_path,
        shutdown,
        accept_thread: Some(handle),
        _lock_file: lock_file,
    })
}

fn accept_loop(listener: &UnixListener, state: &Arc<IpcState>, shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                if !shutdown.load(Ordering::Acquire) {
                    eprintln!("serve ipc: accept stopped: {err}");
                }
                return;
            }
        };
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let state = Arc::clone(state);
        let conn_shutdown = Arc::clone(shutdown);
        let _ = std::thread::Builder::new()
            .name("ipc-conn".to_string())
            .spawn(move || handle_client(stream, &state, &conn_shutdown));
    }
}

fn handle_client(stream: UnixStream, state: &Arc<IpcState>, shutdown: &AtomicBool) {
    if let Err(why) = verify_peer_uid(&stream) {
        eprintln!("serve ipc: rejected peer: {why}");
        return;
    }
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let Ok(mut writer_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    while !shutdown.load(Ordering::Acquire) {
        match read_request(&mut reader) {
            ReadOutcome::Eof => return,
            ReadOutcome::Error {
                status,
                code,
                message,
                close,
            } => {
                let _ = write_json_error(&mut writer_stream, status, code, &message, close);
                if close {
                    return;
                }
            }
            ReadOutcome::Request(req) => {
                match handle_connection_request(&req, &mut writer_stream, state, shutdown) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                }
            }
        }
    }
}

/// Entrypoint for `spawn serve --ipc [--ipc-sock <path>] [--cas <dir>]`.
pub(crate) fn run(flags: &Flags) -> Result<i32, String> {
    let sock_path = match flags.get("ipc-sock") {
        Some(path) => PathBuf::from(path),
        None => default_socket_path()?,
    };
    let cas_root = match flags.get("cas") {
        Some(path) => PathBuf::from(path),
        None => default_cas_root(),
    };
    crate::cost::Prices::from_env()?;
    crate::config::message_taken_ms();
    let owner_url = env_value("SPAWN_DATABASE_URL")
        .ok_or_else(|| "SPAWN_DATABASE_URL is required at IPC server startup".to_string())?;
    let urls = RoleUrls::from_owner(&owner_url)?;
    drop(owner_url);

    let (listener, lock_file) = bind_uds(&sock_path)?;
    let _keep_lock = lock_file;
    println!(
        "serve ipc: unix://{} cas {}",
        sock_path.display(),
        cas_root.display()
    );
    let state = Arc::new(IpcState::new(urls, cas_root, None));
    let shutdown = Arc::new(AtomicBool::new(false));
    accept_loop(&listener, &state, &shutdown);
    Ok(0)
}
