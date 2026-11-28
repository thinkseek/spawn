//! Local HTTP/1.1-over-UDS IPC server (`spawn-ipc-v1`, Option B2; `docs/spawn-ipc-v1.md`)
//! and unified bounded PostgreSQL role connection pools shared with TCP `spawn serve`.
//!
//! Binds `$XDG_RUNTIME_DIR/spawn/core.sock` (or `--ipc-sock <path>`) atomically
//! (`flock` on `<sock>.lock` -> bind temp socket -> `chmod 0600` -> `rename`),
//! verifies Linux `SO_PEERCRED` (`peer_uid == geteuid()`), derives role-scoped
//! PostgreSQL URLs for `spawn_reader` / `spawn_human` / `spawn_node_gateway`
//! once at startup, drops the owner URL immediately, and isolates the four
//! admission pools with 1C/1G safe defaults: `reads(6)`, `writes(2)`, `node(6)`, `events(2)`.

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
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};
use spawn_core::port::StoreError;
use spawn_postgres::{PostgresStore, role_url_with_passfile};
use zeroize::Zeroizing;

use self::http::{ReadOutcome, read_request, write_json_error};
use self::routes::handle_connection_request;
use crate::serve::STATEMENT_TIMEOUT;
use crate::{Flags, env_value};

pub(crate) const ENV_READS_POOL: &str = "SPAWN_SERVE_READS_POOL";
pub(crate) const ENV_WRITES_POOL: &str = "SPAWN_SERVE_WRITES_POOL";
pub(crate) const ENV_NODE_POOL: &str = "SPAWN_SERVE_NODE_POOL";
pub(crate) const ENV_EVENTS_POOL: &str = "SPAWN_SERVE_EVENTS_POOL";
pub(crate) const POOL_BUSY_PREFIX: &str = "busy: ";

pub(crate) const READS_POOL_SIZE: usize = 6;
pub(crate) const WRITES_POOL_SIZE: usize = 2;
pub(crate) const NODE_POOL_SIZE: usize = 6;
pub(crate) const EVENTS_POOL_SIZE: usize = 2;
pub(crate) const DEFAULT_POOL_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

const IDLE_TIMEOUT: Duration = Duration::from_mins(1);
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

static BIND_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Pool concurrency limits and queue wait timeout shared by TCP `spawn serve`
/// and UDS `spawn serve --ipc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PoolConfig {
    pub(crate) reads: usize,
    pub(crate) writes: usize,
    pub(crate) node: usize,
    pub(crate) events: usize,
    pub(crate) wait_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            reads: READS_POOL_SIZE,
            writes: WRITES_POOL_SIZE,
            node: NODE_POOL_SIZE,
            events: EVENTS_POOL_SIZE,
            wait_timeout: DEFAULT_POOL_WAIT_TIMEOUT,
        }
    }
}

impl PoolConfig {
    /// Parse `SPAWN_SERVE_{READS,WRITES,NODE,EVENTS}_POOL` fail-closed:
    /// any set value must parse cleanly as an integer `>= 1`.
    pub(crate) fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Ok(Self {
            reads: parse_pool_size(&env, ENV_READS_POOL, READS_POOL_SIZE)?,
            writes: parse_pool_size(&env, ENV_WRITES_POOL, WRITES_POOL_SIZE)?,
            node: parse_pool_size(&env, ENV_NODE_POOL, NODE_POOL_SIZE)?,
            events: parse_pool_size(&env, ENV_EVENTS_POOL, EVENTS_POOL_SIZE)?,
            wait_timeout: DEFAULT_POOL_WAIT_TIMEOUT,
        })
    }

    pub(crate) fn from_process_env() -> Result<Self, String> {
        Self::from_env(|name| std::env::var(name).ok())
    }
}

fn parse_pool_size(
    env: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: usize,
) -> Result<usize, String> {
    let Some(raw) = env(name) else {
        return Ok(default);
    };
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value >= 1)
        .ok_or_else(|| format!("{name}={raw:?} must be an integer >= 1"))
}

/// Scoped role URLs derived once at startup; owner `SPAWN_DATABASE_URL` is
/// never retained on [`IpcState`] or [`crate::serve::Shared`].
pub(super) struct RoleUrls {
    pub(super) reader: Zeroizing<String>,
    pub(super) human: Zeroizing<String>,
    pub(super) node_gateway: Zeroizing<String>,
}

impl RoleUrls {
    pub(super) fn from_owner(owner_url: &str) -> Result<Self, String> {
        let passfile = crate::config::pg_passfile();
        let reader = Zeroizing::new(role_url_with_passfile(
            owner_url,
            "spawn_reader",
            passfile.as_deref(),
        )?);
        let human = Zeroizing::new(role_url_with_passfile(
            owner_url,
            "spawn_human",
            passfile.as_deref(),
        )?);
        let node_gateway = Zeroizing::new(role_url_with_passfile(
            owner_url,
            "spawn_node_gateway",
            passfile.as_deref(),
        )?);
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

/// Closed failure mode for bounded [`RoleConnPool`] admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PoolError {
    Busy(String),
    Backend(String),
}

impl PoolError {
    pub(crate) fn status_and_message(self) -> (u16, String) {
        match self {
            Self::Busy(message) => (503, message),
            Self::Backend(message) => (500, message),
        }
    }
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(msg) | Self::Backend(msg) => f.write_str(msg),
        }
    }
}

struct PoolState {
    idle: Vec<PostgresStore>,
    in_use: usize,
}

/// Unified, bounded, warm-reusing PostgreSQL connection pool for one role URL.
/// Enforces `idle.len() + in_use <= max`, finite queue waiting on saturation,
/// automatic eviction when [`PoolSlot::mark_broken`] / [`CheckedOutConn::mark_broken`]
/// or panic unwind / unhealthy backend check triggers, and closing evicted sockets
/// before releasing admission slots.
pub(crate) struct RoleConnPool {
    name: &'static str,
    url: Zeroizing<String>,
    max: usize,
    wait_timeout: Duration,
    state: Mutex<PoolState>,
    cond: Condvar,
    #[cfg(test)]
    open_conns: AtomicUsize,
    #[cfg(test)]
    peak_open_conns: AtomicUsize,
}

/// Bounded concurrency reservation on a [`RoleConnPool`]; connects lazily on
/// [`Self::ensure_connected`], reuses warm idle connections, and returns healthy
/// connections to the pool on drop (or evicts and closes broken/panicking ones
/// before waking waiting borrowers).
pub(crate) struct PoolSlot {
    pool: Arc<RoleConnPool>,
    conn: Option<PostgresStore>,
    reused: bool,
    broken: AtomicBool,
}

impl PoolSlot {
    pub(crate) fn ensure_connected(&mut self) -> Result<&PostgresStore, PoolError> {
        if self.conn.is_none() {
            let popped = {
                let mut guard = self
                    .pool
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                guard.idle.pop()
            };
            if let Some(store) = popped {
                self.reused = true;
                self.conn = Some(store);
            } else {
                let store = PostgresStore::connect_with_options(
                    &self.pool.url,
                    &crate::config::pg_connect_options(Some(STATEMENT_TIMEOUT)),
                )
                .map_err(|e| PoolError::Backend(e.to_string()))?;
                #[cfg(test)]
                self.pool.record_open_conn();
                self.reused = false;
                self.conn = Some(store);
            }
        }
        Ok(self
            .conn
            .as_ref()
            .expect("connection populated by ensure_connected"))
    }

    pub(crate) fn into_connected(mut self) -> Result<CheckedOutConn, PoolError> {
        self.ensure_connected()?;
        Ok(CheckedOutConn { slot: self })
    }

    pub(crate) fn mark_broken(&self) {
        self.broken.store(true, Ordering::Release);
    }

    pub(crate) fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Acquire)
    }

    pub(crate) fn was_reused(&self) -> bool {
        self.reused
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    pub(crate) fn store(&self) -> &PostgresStore {
        self.conn
            .as_ref()
            .expect("pooled store slot present until drop")
    }
}

impl Drop for PoolSlot {
    fn drop(&mut self) {
        let conn = self.conn.take();
        let mut evict = self.broken.load(Ordering::Acquire) || std::thread::panicking();
        if !evict
            && let Some(ref store) = conn
            && store.backend_pid().is_err()
        {
            evict = true;
        }
        // Close evicted or broken sockets *before* decrementing `in_use` and waking
        // a waiting borrower so transient open sockets never exceed `self.pool.max`.
        let reusable = if evict {
            if let Some(store) = conn {
                drop(store);
                #[cfg(test)]
                self.pool.record_close_conn();
            }
            None
        } else {
            conn
        };
        let overflow = {
            let mut guard = self
                .pool
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(store) = reusable {
                if guard.idle.len() + guard.in_use <= self.pool.max {
                    guard.idle.push(store);
                    guard.in_use = guard.in_use.saturating_sub(1);
                    self.pool.cond.notify_one();
                    None
                } else {
                    Some(store)
                }
            } else {
                guard.in_use = guard.in_use.saturating_sub(1);
                self.pool.cond.notify_one();
                None
            }
        };
        if let Some(store) = overflow {
            drop(store);
            #[cfg(test)]
            self.pool.record_close_conn();
            let mut guard = self
                .pool
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.in_use = guard.in_use.saturating_sub(1);
            self.pool.cond.notify_one();
        }
    }
}

/// Connected lease checked out from a [`RoleConnPool`], auto-returned (or
/// evicted if marked broken, unhealthy, or unwound by panic) on drop.
pub(crate) struct CheckedOutConn {
    slot: PoolSlot,
}

impl CheckedOutConn {
    pub(crate) fn mark_broken(&self) {
        self.slot.mark_broken();
    }

    pub(crate) fn was_reused(&self) -> bool {
        self.slot.reused
    }

    #[cfg(test)]
    pub(crate) fn was_reused_for_test(&self) -> bool {
        self.slot.reused
    }
}

impl std::ops::Deref for CheckedOutConn {
    type Target = PostgresStore;

    fn deref(&self) -> &Self::Target {
        self.slot
            .conn
            .as_ref()
            .expect("checked-out connection present until drop")
    }
}

impl RoleConnPool {
    pub(crate) fn new(
        name: &'static str,
        url: Zeroizing<String>,
        max: usize,
        wait_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            url,
            max: max.max(1),
            wait_timeout,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                in_use: 0,
            }),
            cond: Condvar::new(),
            #[cfg(test)]
            open_conns: AtomicUsize::new(0),
            #[cfg(test)]
            peak_open_conns: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    fn record_open_conn(&self) {
        let now = self.open_conns.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_open_conns.fetch_max(now, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn record_close_conn(&self) {
        self.open_conns.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn acquire_slot(self: &Arc<Self>, timeout: Duration) -> Result<PoolSlot, PoolError> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.in_use >= self.max {
            if timeout.is_zero() {
                return Err(PoolError::Busy(format!(
                    "busy: {} pool saturated (limit {})",
                    self.name, self.max
                )));
            }
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(PoolError::Busy(format!(
                        "busy: {} pool saturated (limit {})",
                        self.name, self.max
                    )));
                }
                let (next, wait_res) = self
                    .cond
                    .wait_timeout(guard, remaining)
                    .unwrap_or_else(PoisonError::into_inner);
                guard = next;
                if guard.in_use < self.max {
                    break;
                }
                if wait_res.timed_out() {
                    return Err(PoolError::Busy(format!(
                        "busy: {} pool saturated (limit {})",
                        self.name, self.max
                    )));
                }
            }
        }
        guard.in_use += 1;
        Ok(PoolSlot {
            pool: Arc::clone(self),
            conn: None,
            reused: false,
            broken: AtomicBool::new(false),
        })
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> Result<PoolSlot, PoolError> {
        self.acquire_slot(self.wait_timeout)
    }

    pub(crate) fn checkout(self: &Arc<Self>) -> Result<CheckedOutConn, PoolError> {
        self.acquire_slot(self.wait_timeout)?.into_connected()
    }

    /// Verify reused idle sockets with `pg_backend_pid()` *before* dispatching SQL
    /// so stale/half-open backends are evicted without post-dispatch replay.
    pub(crate) fn checkout_verified(self: &Arc<Self>) -> Result<CheckedOutConn, PoolError> {
        for _ in 0..=self.max {
            let conn = self.checkout()?;
            if !conn.was_reused() || conn.backend_pid().is_ok() {
                return Ok(conn);
            }
            conn.mark_broken();
            drop(conn);
        }
        self.checkout()
    }

    #[cfg(test)]
    pub(crate) fn try_acquire_slot(self: &Arc<Self>) -> Option<PoolSlot> {
        self.acquire_slot(Duration::ZERO).ok()
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize) {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (guard.in_use, guard.idle.len())
    }

    #[cfg(test)]
    pub(crate) fn open_conns(&self) -> usize {
        self.open_conns.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn peak_open_conns(&self) -> usize {
        self.peak_open_conns.load(Ordering::Acquire)
    }
}

/// Run a strictly read-only `op` on a pooled connection, transparently retrying
/// once on a verified fresh connection when a reused idle backend fails `Unreachable`.
pub(crate) fn with_read<T>(
    pool: &Arc<RoleConnPool>,
    mut op: impl FnMut(&PostgresStore) -> Result<T, StoreError>,
) -> Result<T, (u16, String)> {
    let conn = pool.checkout().map_err(PoolError::status_and_message)?;
    match op(&conn) {
        Ok(value) => Ok(value),
        Err(first_err) => {
            conn.mark_broken();
            let retry_fresh = conn.was_reused() && matches!(first_err, StoreError::Unreachable(_));
            drop(conn);
            if !retry_fresh {
                return Err((500, first_err.to_string()));
            }
            let fresh = pool
                .checkout_verified()
                .map_err(PoolError::status_and_message)?;
            op(&fresh).map_err(|err| {
                fresh.mark_broken();
                (500, err.to_string())
            })
        }
    }
}

/// Run a non-idempotent or state-mutating `op` (`FnOnce`) at most ONCE after
/// verifying the connection is alive *before* SQL dispatch. Never replays after
/// SQL dispatch, and evicts broken connections immediately on error.
pub(crate) fn with_write<T>(
    pool: &Arc<RoleConnPool>,
    op: impl FnOnce(&PostgresStore) -> Result<T, StoreError>,
) -> Result<T, (u16, String)> {
    let conn = pool
        .checkout_verified()
        .map_err(PoolError::status_and_message)?;
    op(&conn).map_err(|err| {
        conn.mark_broken();
        (500, err.to_string())
    })
}

fn with_schema_search_path(url: &str, schema: &str) -> String {
    let sep = if url.contains('?') { "&" } else { "?" };
    format!("{url}{sep}options=-csearch_path%3D{schema}")
}

/// Unified control-plane pool container used by both TCP `spawn serve` and
/// UDS `spawn serve --ipc`.
pub(crate) struct ServePools {
    pub(crate) reader: Arc<RoleConnPool>,
    pub(crate) human: Option<Arc<RoleConnPool>>,
    pub(crate) node: Option<Arc<RoleConnPool>>,
    pub(crate) events: Arc<RoleConnPool>,
    pub(crate) events_hub: Arc<crate::serve::stream::Hub>,
}

impl ServePools {
    pub(super) fn from_urls(urls: &RoleUrls, config: PoolConfig) -> Arc<Self> {
        Arc::new(Self {
            reader: RoleConnPool::new(
                "reads",
                urls.reader.clone(),
                config.reads,
                config.wait_timeout,
            ),
            human: Some(RoleConnPool::new(
                "writes",
                urls.human.clone(),
                config.writes,
                config.wait_timeout,
            )),
            node: Some(RoleConnPool::new(
                "node",
                urls.node_gateway.clone(),
                config.node,
                config.wait_timeout,
            )),
            events: RoleConnPool::new(
                "events",
                urls.reader.clone(),
                config.events,
                config.wait_timeout,
            ),
            events_hub: Arc::new(crate::serve::stream::Hub::new(
                crate::serve::stream::CLIENT_BUFFER,
            )),
        })
    }

    pub(crate) fn from_serve_env(
        owner_url: Option<&str>,
        reader_override: Option<&str>,
        config: PoolConfig,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(owner) = owner_url.filter(|s| !s.trim().is_empty()) else {
            let Some(reader) = reader_override.filter(|s| !s.trim().is_empty()) else {
                return Ok(None);
            };
            let reader_url = Zeroizing::new(reader.to_string());
            return Ok(Some(Arc::new(Self {
                reader: RoleConnPool::new(
                    "reads",
                    reader_url.clone(),
                    config.reads,
                    config.wait_timeout,
                ),
                human: None,
                node: None,
                events: RoleConnPool::new("events", reader_url, config.events, config.wait_timeout),
                events_hub: Arc::new(crate::serve::stream::Hub::new(
                    crate::serve::stream::CLIENT_BUFFER,
                )),
            })));
        };
        let mut urls = RoleUrls::from_owner(owner)?;
        if let Some(reader) = reader_override.filter(|s| !s.trim().is_empty()) {
            urls.reader = Zeroizing::new(reader.to_string());
        }
        Ok(Some(Self::from_urls(&urls, config)))
    }

    pub(crate) fn ensure_stream_listener(self: &Arc<Self>) {
        let events_pool = Arc::clone(&self.events);
        self.events_hub.start_arc(move || {
            events_pool
                .checkout_verified()
                .map_err(|err| err.to_string())
        });
    }

    pub(super) fn human_pool(&self) -> &Arc<RoleConnPool> {
        self.human
            .as_ref()
            .expect("IPC server always initializes human pool")
    }

    pub(super) fn node_pool(&self) -> &Arc<RoleConnPool> {
        self.node
            .as_ref()
            .expect("IPC server always initializes node pool")
    }
}

impl Drop for ServePools {
    fn drop(&mut self) {
        self.events_hub.stop();
    }
}

pub(super) struct IpcState {
    pub(super) pools: Arc<ServePools>,
    pub(super) cas_root: PathBuf,
    pub(super) epoch: String,
}

impl IpcState {
    #[cfg(test)]
    fn with_test_wait_timeout(
        urls: RoleUrls,
        cas_root: PathBuf,
        schema: Option<&str>,
        wait_timeout: Duration,
    ) -> Self {
        let config = PoolConfig {
            wait_timeout,
            ..PoolConfig::default()
        };
        Self::with_config(urls, cas_root, schema, config)
    }

    fn with_config(
        mut urls: RoleUrls,
        cas_root: PathBuf,
        schema: Option<&str>,
        config: PoolConfig,
    ) -> Self {
        if let Some(schema) = schema {
            urls = RoleUrls::from_explicit(
                with_schema_search_path(&urls.reader, schema),
                with_schema_search_path(&urls.human, schema),
                with_schema_search_path(&urls.node_gateway, schema),
            );
        }
        let pools = ServePools::from_urls(&urls, config);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let epoch = format!("boot-{now_ms:x}-{}", std::process::id());
        Self {
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

/// Off Linux there is no `SO_PEERCRED` in rustix, and this crate reads no
/// peer credential through unsafe code: the peer is not checked, and the
/// socket's mode (0600, set by [`bind_uds`] before it is renamed into
/// place) is what keeps other users out.
#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Linux check's signature, so the caller is one for both"
)]
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
    let (server, _pools) =
        spawn_ipc_server_with_config(sock_path, urls, cas_root, schema, PoolConfig::default())?;
    Ok(server)
}

#[cfg(test)]
pub(crate) fn spawn_ipc_server_with_config(
    sock_path: PathBuf,
    urls: RoleUrls,
    cas_root: PathBuf,
    schema: Option<&str>,
    config: PoolConfig,
) -> Result<(IpcServer, Arc<ServePools>), String> {
    let (listener, lock_file) = bind_uds(&sock_path)?;
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("set blocking: {e}"))?;
    let state = Arc::new(IpcState::with_config(urls, cas_root, schema, config));
    let pools = Arc::clone(&state.pools);
    let shutdown = Arc::new(AtomicBool::new(false));
    let accept_shutdown = Arc::clone(&shutdown);
    let handle = std::thread::Builder::new()
        .name("ipc-accept".to_string())
        .spawn(move || accept_loop(&listener, &state, &accept_shutdown))
        .map_err(|e| format!("spawn ipc-accept: {e}"))?;
    Ok((
        IpcServer {
            sock_path,
            shutdown,
            accept_thread: Some(handle),
            _lock_file: lock_file,
        },
        pools,
    ))
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
    let pool_config = PoolConfig::from_process_env()?;
    let owner_url = env_value("SPAWN_DATABASE_URL")
        .ok_or_else(|| "SPAWN_DATABASE_URL is required at IPC server startup".to_string())?;
    let urls = RoleUrls::from_owner(&owner_url)?;
    drop(owner_url);

    let (listener, lock_file) = bind_uds(&sock_path)?;
    let _keep_lock = lock_file;
    println!(
        "serve ipc: unix://{} cas {} pools reads={} writes={} node={} events={}",
        sock_path.display(),
        cas_root.display(),
        pool_config.reads,
        pool_config.writes,
        pool_config.node,
        pool_config.events
    );
    let state = Arc::new(IpcState::with_config(urls, cas_root, None, pool_config));
    let shutdown = Arc::new(AtomicBool::new(false));
    accept_loop(&listener, &state, &shutdown);
    Ok(0)
}
