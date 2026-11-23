//! `hxprobe service`: the one process per cache directory that builds
//! `mail.db`.
//!
//! [`Core`] is the refresh policy as a synchronous state machine: it never
//! touches a socket, a thread or the clock; every method takes the time as
//! `now` (unix seconds). The work it decides on is done through a
//! [`Backend`].
//!
//! [`serve`] is the runtime around it: the socket server, the build
//! threads and the policy loop. [`InstanceLock`] makes the service the only
//! one of its cache directory.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::mcp::cache::{self, DbInfo, FileId, BUILD_UNFINISHED};
use crate::mcp::log;
use crate::mcp::proto::{self, Request, ServiceStatus, UNUSABLE_REBUILD_SECS, VERSION};

/// What the service works on: the live store and `mail.db`.
pub trait Backend {
    /// Length and modification time (ns) of the live store.
    fn store_stamp(&self) -> io::Result<(u64, i64)>;
    /// Identity of the `mail.db` file, without opening it; `None` when
    /// there is none.
    fn db_id(&self) -> Option<FileId>;
    /// Identity and build information of `mail.db` when it is usable for
    /// the store; `None` otherwise. Opens the database and closes it again.
    fn db(&self) -> Option<(FileId, DbInfo)>;
    /// Build `mail.db` from the store.
    fn build(&self, now: i64) -> Result<(), String>;
    /// Whether opening the store for reading fails for lack of permission.
    fn store_denied(&self) -> bool;
}

/// The real [`Backend`]: the store and `mail.db` in the cache directory.
#[derive(Debug)]
pub struct CacheBackend {
    dir: PathBuf,
    store: PathBuf,
}

impl CacheBackend {
    /// The backend for `store` with the cache directory `dir`.
    pub fn new(dir: &Path, store: &Path) -> CacheBackend {
        CacheBackend {
            dir: dir.to_path_buf(),
            store: store.to_path_buf(),
        }
    }
}

impl Backend for CacheBackend {
    fn store_stamp(&self) -> io::Result<(u64, i64)> {
        cache::stamp(&self.store)
    }

    fn db_id(&self) -> Option<FileId> {
        cache::file_id(&self.dir.join(cache::DB_NAME))
    }

    fn db(&self) -> Option<(FileId, DbInfo)> {
        // Accepted only when the file was the same before and after it was
        // read; one more try when it was replaced meanwhile.
        for _ in 0..2 {
            let before = self.db_id()?;
            let info = cache::db_info(&self.dir, &self.store);
            if self.db_id() == Some(before) {
                return info.map(|i| (before, i));
            }
        }
        None
    }

    fn build(&self, now: i64) -> Result<(), String> {
        cache::Cache::new(&self.dir, &self.store)?
            .build(now)
            .map(drop)
    }

    fn store_denied(&self) -> bool {
        File::open(&self.store).is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
    }
}

/// Why the service ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// No request for `idle_secs`.
    Idle,
    /// A `shutdown` request with the service's own version.
    Shutdown,
    /// `SIGTERM` or `SIGINT`.
    Signal,
    /// `service.sock` was removed or replaced.
    SocketGone,
    /// The store cannot be read for lack of permission.
    Permission,
}

/// Exit status of `hxprobe service` when another service holds
/// `service.lock`.
pub const EXIT_BUSY: i32 = 75;
/// Exit status of `hxprobe service` when the store cannot be read for lack
/// of permission.
pub const EXIT_PERMISSION: i32 = 77;

impl Exit {
    /// The exit status of the process.
    pub fn code(self) -> i32 {
        match self {
            Exit::Permission => EXIT_PERMISSION,
            Exit::Idle | Exit::Shutdown | Exit::Signal | Exit::SocketGone => 0,
        }
    }

    /// The reason, for the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Exit::Idle => "idle",
            Exit::Shutdown => "shutdown requested",
            Exit::Signal => "signal",
            Exit::SocketGone => "socket removed",
            Exit::Permission => "the store cannot be read: permission denied",
        }
    }
}

/// Why a build starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildReason {
    /// There is no usable database.
    First,
    /// The store changed and a client is active.
    Changed,
    /// A `refresh` request.
    Refresh,
}

impl BuildReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BuildReason::First => "first",
            BuildReason::Changed => "changed",
            BuildReason::Refresh => "refresh",
        }
    }
}

/// What the service has to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Call [`Core::build_started`], then run [`Backend::build`] and report
    /// its end with [`Core::build_finished`].
    Build(BuildReason),
    /// Nothing until the next request, build end or tick.
    Wait,
    /// Stop serving.
    Exit(Exit),
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Send this at once.
    Send(Value),
    /// Wait until [`Core::generation`] reaches this value, then send
    /// [`Core::refresh_reply`].
    AfterBuild(u64),
}

/// The result of [`Core::handle`].
#[derive(Debug, Clone, PartialEq)]
pub struct Handled {
    pub reply: Reply,
    /// A build the request started ([`Core::build_started`] is already
    /// done): the caller runs [`Backend::build`].
    pub build: Option<BuildReason>,
}

/// The refresh policy of the service.
#[derive(Debug)]
pub struct Core<B> {
    backend: Arc<B>,
    store_path: String,
    refresh_secs: u64,
    /// 0: never exit for lack of requests.
    idle_secs: u64,
    started_unix: i64,
    building: bool,
    /// A `touch` arrived since the last build started.
    touched: bool,
    /// A `refresh` waits and no build runs.
    force: bool,
    last_attempt_unix: Option<i64>,
    last_error: Option<String>,
    /// Start, any request, end of a build.
    last_activity_unix: i64,
    /// Number of finished builds, failed ones included.
    generation: u64,
    last_result: Option<Result<(), String>>,
    stopping: Option<Exit>,
    /// The `mail.db` examined last: its identity (`None`: no file) and its
    /// build information (`None`: not usable).
    db_cache: Option<(Option<FileId>, Option<DbInfo>)>,
}

impl<B: Backend> Core<B> {
    /// The policy for `store_path`, started at `now`.
    pub fn new(
        backend: B,
        store_path: String,
        refresh_secs: u64,
        idle_secs: u64,
        now: i64,
    ) -> Core<B> {
        Core {
            backend: Arc::new(backend),
            store_path,
            refresh_secs,
            idle_secs,
            started_unix: now,
            building: false,
            touched: false,
            force: false,
            last_attempt_unix: None,
            last_error: None,
            last_activity_unix: now,
            generation: 0,
            last_result: None,
            stopping: None,
            db_cache: None,
        }
    }

    /// The backend, to run a build outside the policy's lock.
    pub fn backend(&self) -> Arc<B> {
        Arc::clone(&self.backend)
    }

    /// Number of finished builds, failed ones included.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What to do at `now`: evaluated after every request, after every
    /// build and on every tick.
    pub fn poll(&mut self, now: i64) -> Action {
        if let Some(e) = self.stopping {
            return Action::Exit(e);
        }
        if self.building {
            return Action::Wait;
        }
        if self.force {
            return Action::Build(BuildReason::Refresh);
        }
        match self.db() {
            None => {
                // An attempt time after `now` does not count. After a
                // successful build that left no usable database, wait a
                // little: another program may be replacing it with its own.
                let recent = |secs: i64| {
                    self.last_attempt_unix
                        .is_some_and(|at| (0..secs).contains(&now.saturating_sub(at)))
                };
                let blocked = if self.last_error.is_some() {
                    recent(secs(self.refresh_secs))
                } else {
                    recent(UNUSABLE_REBUILD_SECS)
                };
                if !blocked {
                    return Action::Build(BuildReason::First);
                }
            }
            Some(db) => match self.backend.store_stamp() {
                // A stamp that cannot be read counts as not changed.
                Err(_) => {
                    if self.backend.store_denied() {
                        return self.stop(Exit::Permission);
                    }
                }
                Ok(stamp) => {
                    let changed = db.stamp != Some(stamp);
                    // A time after `now` would put the rebuild off for good.
                    let past = |t: Option<i64>| t.filter(|t| *t <= now).unwrap_or(i64::MIN);
                    let since = past(db.built_unix).max(past(self.last_attempt_unix));
                    if changed
                        && self.touched
                        && now.saturating_sub(since) >= secs(self.refresh_secs)
                    {
                        return Action::Build(BuildReason::Changed);
                    }
                }
            },
        }
        if self.idle_secs > 0 && now.saturating_sub(self.last_activity_unix) >= secs(self.idle_secs)
        {
            return self.stop(Exit::Idle);
        }
        Action::Wait
    }

    /// Record `reason` as the end of the service.
    fn stop(&mut self, reason: Exit) -> Action {
        Action::Exit(*self.stopping.get_or_insert(reason))
    }

    /// Record the start of a build that [`Core::poll`] asked for.
    pub fn build_started(&mut self, now: i64) {
        self.building = true;
        self.touched = false;
        self.force = false;
        self.last_attempt_unix = Some(now);
        // Kept when the build never reports its end.
        self.last_error = Some(BUILD_UNFINISHED.to_string());
    }

    /// Record the end of a build. A failure for lack of permission on the
    /// store ends the service.
    pub fn build_finished(&mut self, result: Result<(), String>, now: i64) {
        self.building = false;
        self.last_error = result.as_ref().err().cloned();
        if result.is_err() && self.stopping.is_none() && self.backend.store_denied() {
            self.stopping = Some(Exit::Permission);
        }
        self.last_result = Some(result);
        self.generation += 1;
        self.last_activity_unix = now;
    }

    /// Apply a request received at `now`. `touch` and `refresh` may start
    /// a build; `refresh` waits for the running build or the one it starts.
    pub fn handle(&mut self, req: &Request, now: i64) -> Handled {
        self.last_activity_unix = now;
        let mut build = None;
        let reply = match req {
            Request::Status => Reply::Send(self.status().to_json()),
            Request::Touch => {
                self.touched = true;
                build = self.start_if_due(now);
                Reply::Send(self.status().to_json())
            }
            Request::Refresh => {
                if !self.building {
                    self.force = true;
                }
                build = self.start_if_due(now);
                Reply::AfterBuild(self.generation + 1)
            }
            Request::Shutdown { version } if version == VERSION => {
                self.stopping.get_or_insert(Exit::Shutdown);
                Reply::Send(proto::stopping_reply())
            }
            Request::Shutdown { .. } => Reply::Send(proto::version_mismatch_reply(VERSION)),
        };
        Handled { reply, build }
    }

    /// Start a build when [`Core::poll`] asks for one.
    fn start_if_due(&mut self, now: i64) -> Option<BuildReason> {
        match self.poll(now) {
            Action::Build(reason) => {
                self.build_started(now);
                Some(reason)
            }
            _ => None,
        }
    }

    /// The reply to a `refresh` whose build has finished: the status, with
    /// `ok` false and the error when that build failed.
    pub fn refresh_reply(&mut self) -> Value {
        let st = self.status();
        match &self.last_result {
            Some(Err(e)) => st.failed(e).to_json(),
            _ => st.to_json(),
        }
    }

    /// The status reply.
    pub fn status(&mut self) -> ServiceStatus {
        let db = self.db();
        let store_changed =
            db.and_then(|d| self.backend.store_stamp().ok().map(|s| d.stamp != Some(s)));
        ServiceStatus {
            ok: true,
            error: None,
            version: VERSION.to_string(),
            pid: std::process::id(),
            store_path: Some(self.store_path.clone()),
            building: self.building,
            db_usable: db.is_some(),
            built_unix: db.and_then(|d| d.built_unix),
            store_changed,
            last_attempt_unix: self.last_attempt_unix,
            last_error: self.last_error.clone(),
            refresh_secs: Some(self.refresh_secs),
            idle_secs: Some(self.idle_secs),
            started_unix: Some(self.started_unix),
            builds: self.generation,
        }
    }

    /// Build information of the usable `mail.db`, read again only when the
    /// file's identity changed (also when it was not usable).
    fn db(&mut self) -> Option<DbInfo> {
        let id = self.backend.db_id();
        if let Some((cached, info)) = self.db_cache {
            if cached == id {
                return info;
            }
        }
        let entry = match id {
            None => (None, None),
            Some(_) => match self.backend.db() {
                Some((id, info)) => (Some(id), Some(info)),
                None => (id, None),
            },
        };
        self.db_cache = Some(entry);
        entry.1
    }
}

/// Seconds as a time difference.
fn secs(s: u64) -> i64 {
    i64::try_from(s).unwrap_or(i64::MAX)
}

// --- runtime ---

/// How long [`InstanceLock::acquire`] tries a held lock before it gives up,
/// and how often. A service that just exited may hold its lock for a moment
/// after it is seen as gone.
const LOCK_RETRY: Duration = Duration::from_millis(250);
const LOCK_RETRY_POLL: Duration = Duration::from_millis(10);

/// `service.lock` could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// Another process holds it.
    Busy,
    /// It could not be opened, locked or written.
    Failed(String),
}

/// The single-instance lock `service.lock`, held for the life of the
/// service. The kernel releases it when the process ends.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Take `service.lock` in `dir` with an exclusive `flock` and write
    /// `<pid> <version>` into it. A held lock is tried again for up to
    /// [`LOCK_RETRY`] before this returns [`LockError::Busy`]. An existing
    /// file that is not a private regular file (a symbolic link, say) is
    /// removed and created again; a symbolic link is never followed.
    pub fn acquire(dir: &Path) -> Result<InstanceLock, LockError> {
        let path = dir.join(proto::LOCK_NAME);
        let failed =
            |what: &str, e: io::Error| LockError::Failed(format!("{what} {}: {e}", path.display()));
        if std::fs::symlink_metadata(&path).is_ok_and(|m| cache::check_private(&m).is_err()) {
            std::fs::remove_file(&path).map_err(|e| failed("remove", e))?;
        }
        // Not truncated here: the file is the record of the holder until
        // this process holds the lock.
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| failed("open", e))?;
        let deadline = Instant::now() + LOCK_RETRY;
        loop {
            // SAFETY: flock() on a descriptor owned by `f` for the whole call.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock if Instant::now() >= deadline => {
                    return Err(LockError::Busy)
                }
                io::ErrorKind::WouldBlock => thread::sleep(LOCK_RETRY_POLL),
                _ => return Err(failed("lock", e)),
            }
        }
        f.set_len(0)
            .and_then(|()| f.write_all(format!("{} {VERSION}\n", std::process::id()).as_bytes()))
            .map_err(|e| failed("write", e))?;
        Ok(InstanceLock { _file: f })
    }
}

/// The socket file a listener was bound to, by path and identity.
#[derive(Debug, Clone)]
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketFile {
    /// The socket file of `listener`, as it is now.
    pub fn of(listener: &UnixListener) -> io::Result<SocketFile> {
        let addr = listener.local_addr()?;
        let path = addr
            .as_pathname()
            .ok_or_else(|| io::Error::other("the listener has no path"))?
            .to_path_buf();
        let m = std::fs::symlink_metadata(&path)?;
        Ok(SocketFile {
            path,
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the path still names this socket file (not removed, not
    /// replaced).
    pub fn is_present(&self) -> bool {
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.dev() == self.dev && m.ino() == self.ino)
    }

    /// Remove the socket file when it still is this one; whether it did.
    pub fn remove_if_ours(&self) -> bool {
        self.is_present() && std::fs::remove_file(&self.path).is_ok()
    }
}

/// Bind the service's listening socket at `path`: remove whatever stale
/// file is there (only the holder of `service.lock` calls this), bind with
/// `umask(0o077)`, then set the socket to 0600.
pub fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            return Err(io::Error::other(format!(
                "{} is a directory",
                path.display()
            )))
        }
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // SAFETY: umask() has no preconditions and cannot fail.
    let old = unsafe { libc::umask(0o077) };
    let bound = UnixListener::bind(path);
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    let listener = bound?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Asks [`serve`] to end, and wakes its policy loop. Clones share one
/// state.
#[derive(Debug, Clone, Default)]
pub struct Stop(Arc<(Mutex<StopState>, Condvar)>);

#[derive(Debug, Default)]
struct StopState {
    exit: Option<Exit>,
    /// Something changed since the policy loop last looked.
    woken: bool,
}

impl Stop {
    pub fn new() -> Stop {
        Stop::default()
    }

    /// End [`serve`] with `exit`; the first request wins.
    pub fn request(&self, exit: Exit) {
        let mut s = relock(&self.0 .0);
        s.exit.get_or_insert(exit);
        self.0 .1.notify_all();
    }

    /// The requested end, if any.
    pub fn requested(&self) -> Option<Exit> {
        relock(&self.0 .0).exit
    }

    /// Make the policy loop look at the state now.
    fn wake(&self) {
        relock(&self.0 .0).woken = true;
        self.0 .1.notify_all();
    }

    /// Wait until an end is requested, the loop is woken, or `timeout`
    /// passed; the requested end, if any.
    fn wait(&self, timeout: Duration) -> Option<Exit> {
        let s = relock(&self.0 .0);
        let (mut s, _) = self
            .0
             .1
            .wait_timeout_while(s, timeout, |s| s.exit.is_none() && !s.woken)
            .unwrap_or_else(|e| e.into_inner());
        s.woken = false;
        s.exit
    }
}

/// Lock `m`, also when a thread panicked while holding it.
fn relock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The clock of [`serve`]: unix seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// What the threads of [`serve`] share.
struct Shared<B> {
    core: Mutex<Core<B>>,
    /// Notified at the end of every build and when `serve` ends.
    built: Condvar,
    clock: Clock,
    tick: Duration,
    stop: Stop,
    /// `serve` is ending: threads stop waiting.
    closing: AtomicBool,
}

/// Serve requests on `listener` and run the policy of `core` until it, a
/// `shutdown` request or `stop` ends it; the reason.
///
/// The calling thread runs the policy: it polls `core` after every request,
/// after every build and every `tick`, starts builds, and checks every
/// tick that the socket file is still the one bound (removed or replaced:
/// [`Exit::SocketGone`]). An accept thread serves each connection on a
/// thread of its own (at most [`proto::MAX_CONNECTIONS`] at once, each with
/// [`proto::CONNECTION_TIMEOUT`]); each build runs on a thread of its own.
///
/// On return the accept thread has ended and `listener` is closed; a
/// running build and open connections are not waited for. Starts no signal
/// handling.
pub fn serve<B>(
    core: Core<B>,
    listener: UnixListener,
    clock: Clock,
    tick: Duration,
    stop: Stop,
) -> Exit
where
    B: Backend + Send + Sync + 'static,
{
    let socket = SocketFile::of(&listener).ok();
    let shared = Arc::new(Shared {
        core: Mutex::new(core),
        built: Condvar::new(),
        clock,
        tick,
        stop,
        closing: AtomicBool::new(false),
    });
    // The accept thread waits on the listener and on this pair, which
    // ends it at once also when the socket file is gone.
    let wake = UnixStream::pair().ok();
    let (wake_tx, wake_rx) = match wake {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    let accept = {
        let sh = Arc::clone(&shared);
        thread::Builder::new()
            .name("accept".into())
            .spawn(move || accept_loop(&sh, listener, wake_rx))
    };
    let exit = match &accept {
        Err(e) => {
            log(&format!("cannot start the accept thread: {e}"));
            Exit::SocketGone
        }
        Ok(_) => policy_loop(&shared, socket.as_ref()),
    };
    shared.closing.store(true, AtomicOrdering::SeqCst);
    {
        // Under the lock, so no waiter misses it between check and wait.
        let _core = relock(&shared.core);
        shared.built.notify_all();
    }
    if let Some(mut w) = wake_tx {
        let _ = w.write_all(b"x");
    }
    if let Ok(h) = accept {
        let _ = h.join();
    }
    exit
}

/// The policy thread of [`serve`]; returns the end.
fn policy_loop<B>(shared: &Arc<Shared<B>>, socket: Option<&SocketFile>) -> Exit
where
    B: Backend + Send + Sync + 'static,
{
    loop {
        if let Some(e) = shared.stop.requested() {
            return e;
        }
        let now = (shared.clock)();
        {
            let mut core = relock(&shared.core);
            match core.poll(now) {
                Action::Exit(e) => return e,
                Action::Build(reason) => {
                    core.build_started(now);
                    start_build(shared, &mut core, reason, now);
                }
                Action::Wait => {}
            }
        }
        if !socket.is_some_and(SocketFile::is_present) {
            return Exit::SocketGone;
        }
        if let Some(e) = shared.stop.wait(shared.tick) {
            return e;
        }
    }
}

/// Run a build that `core` recorded as started at `now` on a thread of its
/// own; a panic counts as a failed build.
fn start_build<B>(shared: &Arc<Shared<B>>, core: &mut Core<B>, reason: BuildReason, now: i64)
where
    B: Backend + Send + Sync + 'static,
{
    log(&format!("build started ({})", reason.as_str()));
    let backend = core.backend();
    let sh = Arc::clone(shared);
    let started = thread::Builder::new().name("build".into()).spawn(move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| backend.build(now)))
            .unwrap_or_else(|_| Err(BUILD_UNFINISHED.to_string()));
        if let Err(e) = &result {
            log(&format!("build failed: {e}"));
        }
        let end = (sh.clock)();
        relock(&sh.core).build_finished(result, end);
        sh.built.notify_all();
        sh.stop.wake();
    });
    if let Err(e) = started {
        let msg = format!("cannot start a build: {e}");
        log(&msg);
        core.build_finished(Err(msg), now);
        shared.built.notify_all();
    }
}

/// The accept thread of [`serve`]: waits for connections (and for a byte
/// on `wake`), serves each on a thread of its own, and returns when
/// `closing` is set.
fn accept_loop<B>(shared: &Arc<Shared<B>>, listener: UnixListener, wake: Option<UnixStream>)
where
    B: Backend + Send + Sync + 'static,
{
    // Non-blocking, so a connection that vanished between the wait and
    // `accept` cannot block this thread.
    if listener.set_nonblocking(true).is_err() {
        log("cannot make the listener non-blocking");
    }
    let active = Arc::new(AtomicUsize::new(0));
    let mut fds = vec![libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    if let Some(w) = &wake {
        fds.push(libc::pollfd {
            fd: w.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    // Without the wake pair, `closing` is still seen within a tick.
    let timeout = if wake.is_some() {
        -1
    } else {
        libc::c_int::try_from(shared.tick.as_millis().max(1)).unwrap_or(1000)
    };
    while !shared.closing.load(AtomicOrdering::SeqCst) {
        // SAFETY: `fds` is a valid array of `fds.len()` entries whose
        // descriptors stay open for the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            thread::sleep(Duration::from_millis(10));
        }
        if shared.closing.load(AtomicOrdering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => admit(shared, stream, &active),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            // Out of descriptors, say: do not spin.
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Serve `stream` on a thread of its own, or close it at once when
/// [`proto::MAX_CONNECTIONS`] are served already.
fn admit<B>(shared: &Arc<Shared<B>>, stream: UnixStream, active: &Arc<AtomicUsize>)
where
    B: Backend + Send + Sync + 'static,
{
    if active.fetch_add(1, AtomicOrdering::SeqCst) >= proto::MAX_CONNECTIONS {
        active.fetch_sub(1, AtomicOrdering::SeqCst);
        return;
    }
    let sh = Arc::clone(shared);
    let count = Arc::clone(active);
    let spawned = thread::Builder::new()
        .name("connection".into())
        .spawn(move || {
            connection(&sh, stream);
            count.fetch_sub(1, AtomicOrdering::SeqCst);
        });
    if spawned.is_err() {
        active.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

/// One line read from a connection.
enum Line {
    Text(String),
    /// Over [`proto::MAX_REQUEST_BYTES`], or not UTF-8.
    Bad,
    /// Nothing before end of input, an error or the timeout.
    Nothing,
}

/// Read one request line, at most [`proto::MAX_REQUEST_BYTES`] and its
/// newline.
fn read_line(s: &UnixStream) -> Line {
    let mut buf = Vec::new();
    let limit = proto::MAX_REQUEST_BYTES as u64 + 2;
    match io::BufReader::new(s.take(limit)).read_until(b'\n', &mut buf) {
        Ok(0) | Err(_) => Line::Nothing,
        Ok(_) if !buf.ends_with(b"\n") && buf.len() > proto::MAX_REQUEST_BYTES => Line::Bad,
        Ok(_) => String::from_utf8(buf).map_or(Line::Bad, Line::Text),
    }
}

/// Write `v` as one line; errors are ignored (the client went away).
fn reply(mut s: &UnixStream, v: &Value) {
    let _ = s.write_all(format!("{v}\n").as_bytes());
}

/// Serve one connection: one request, one reply.
fn connection<B>(shared: &Arc<Shared<B>>, s: UnixStream)
where
    B: Backend + Send + Sync + 'static,
{
    // A listener's non-blocking mode can be inherited.
    if s.set_nonblocking(false).is_err()
        || s.set_read_timeout(Some(proto::CONNECTION_TIMEOUT)).is_err()
        || s.set_write_timeout(Some(proto::CONNECTION_TIMEOUT))
            .is_err()
    {
        return;
    }
    let req = match read_line(&s) {
        Line::Nothing => return,
        Line::Bad => return reply(&s, &proto::bad_request_reply()),
        Line::Text(t) => match Request::parse(&t) {
            Ok(r) => r,
            Err(_) => return reply(&s, &proto::bad_request_reply()),
        },
    };
    let now = (shared.clock)();
    let mut core = relock(&shared.core);
    if let Request::Shutdown { version } = &req {
        log(&format!("shutdown requested by version {version}"));
    }
    let handled = core.handle(&req, now);
    if let Some(reason) = handled.build {
        start_build(shared, &mut core, reason, now);
    }
    match handled.reply {
        Reply::Send(v) => {
            if matches!(req, Request::Shutdown { .. }) {
                // Written before the policy loop can see the stop (it needs
                // the lock), so the reply leaves before the process ends. A
                // reply this short never waits for the reader.
                reply(&s, &v);
                drop(core);
                shared.stop.wake();
            } else {
                drop(core);
                shared.stop.wake();
                reply(&s, &v);
            }
        }
        Reply::AfterBuild(n) => {
            shared.stop.wake();
            while core.generation() < n && !shared.closing.load(AtomicOrdering::SeqCst) {
                core = shared
                    .built
                    .wait_timeout(core, shared.tick)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if shared.closing.load(AtomicOrdering::SeqCst) {
                // The service ends; the build may never finish.
                return;
            }
            let v = core.refresh_reply();
            drop(core);
            reply(&s, &v);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    const NOW: i64 = 1_800_000_000;
    const STAMP: (u64, i64) = (100, 5_000);
    const STAMP2: (u64, i64) = (200, 9_000);
    const STORE: &str = "/s/HxStore.hxd";
    const DAY: i64 = 86_400;

    /// Scripted state of a [`FakeBackend`].
    #[derive(Debug)]
    pub(crate) struct FakeState {
        /// `None`: the stamp cannot be read.
        pub stamp: Option<(u64, i64)>,
        pub db_id: Option<FileId>,
        /// `None`: the file at `db_id` is not usable.
        pub db: Option<DbInfo>,
        pub denied: bool,
        /// `Some(e)`: builds fail with `e`.
        pub fail: Option<String>,
        /// Calls of `build`.
        pub builds: u64,
        /// Calls of `db`.
        pub db_reads: u64,
        /// `Some`: the next build blocks until the sender sends or is
        /// dropped.
        pub gate: Option<std::sync::mpsc::Receiver<()>>,
    }

    /// A backend whose answers the test sets.
    #[derive(Debug)]
    pub(crate) struct FakeBackend(pub Mutex<FakeState>);

    impl FakeBackend {
        pub fn new() -> FakeBackend {
            FakeBackend(Mutex::new(FakeState {
                stamp: Some(STAMP),
                db_id: None,
                db: None,
                denied: false,
                fail: None,
                builds: 0,
                db_reads: 0,
                gate: None,
            }))
        }

        pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
            f(&mut self.0.lock().unwrap());
        }

        pub fn get<T>(&self, f: impl FnOnce(&FakeState) -> T) -> T {
            f(&self.0.lock().unwrap())
        }
    }

    impl Backend for FakeBackend {
        fn store_stamp(&self) -> io::Result<(u64, i64)> {
            self.get(|s| s.stamp)
                .ok_or_else(|| io::Error::other("no stamp"))
        }

        fn db_id(&self) -> Option<FileId> {
            self.get(|s| s.db_id)
        }

        fn db(&self) -> Option<(FileId, DbInfo)> {
            let mut s = self.0.lock().unwrap();
            s.db_reads += 1;
            s.db_id.zip(s.db)
        }

        fn build(&self, _now: i64) -> Result<(), String> {
            let gate = {
                let mut s = self.0.lock().unwrap();
                s.builds += 1;
                s.gate.take()
            };
            // Blocks without holding the state, so the service can still
            // read it.
            if let Some(g) = gate {
                let _ = g.recv();
            }
            match &self.0.lock().unwrap().fail {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn store_denied(&self) -> bool {
            self.get(|s| s.denied)
        }
    }

    fn id(n: u64) -> FileId {
        FileId {
            dev: 1,
            ino: n,
            len: 4096,
            mtime_ns: 0,
        }
    }

    /// A usable database (file `ino`) built at `built` from `stamp`.
    fn put_db(core: &Core<FakeBackend>, ino: u64, built: i64, stamp: (u64, i64)) {
        core.backend().set(|s| {
            s.db_id = Some(id(ino));
            s.db = Some(DbInfo {
                built_unix: Some(built),
                stamp: Some(stamp),
            });
        });
    }

    fn core(refresh_secs: u64, idle_secs: u64) -> Core<FakeBackend> {
        Core::new(
            FakeBackend::new(),
            STORE.into(),
            refresh_secs,
            idle_secs,
            NOW,
        )
    }

    /// A core with an up-to-date database built at `NOW`.
    fn fresh(refresh_secs: u64, idle_secs: u64) -> Core<FakeBackend> {
        let c = core(refresh_secs, idle_secs);
        put_db(&c, 1, NOW, STAMP);
        c
    }

    /// Run one build from `start` to `end` as the runtime does.
    fn run_build(c: &mut Core<FakeBackend>, start: i64, end: i64) {
        c.build_started(start);
        let r = c.backend().build(start);
        c.build_finished(r, end);
    }

    fn touch(c: &mut Core<FakeBackend>, now: i64) -> Handled {
        c.handle(&Request::Touch, now)
    }

    fn sent(h: &Handled) -> &Value {
        match &h.reply {
            Reply::Send(v) => v,
            r => panic!("expected an immediate reply, got {r:?}"),
        }
    }

    // --- poll ---

    #[test]
    fn no_database_builds_at_start_without_a_touch() {
        let mut c = core(900, 1800);
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        // Also when a file exists that is not usable.
        let mut c = core(900, 1800);
        c.backend().set(|s| s.db_id = Some(id(1)));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
    }

    #[test]
    fn unchanged_store_never_builds() {
        let mut c = fresh(900, 0);
        for t in [NOW, NOW + 900, NOW + DAY, NOW + 30 * DAY] {
            let h = touch(&mut c, t);
            assert_eq!(h.build, None);
            assert_eq!(c.poll(t), Action::Wait);
        }
    }

    #[test]
    fn changed_store_without_a_touch_never_builds() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        for t in [NOW, NOW + 900, NOW + DAY, NOW + 30 * DAY] {
            assert_eq!(c.poll(t), Action::Wait);
        }
    }

    #[test]
    fn changed_store_with_a_touch_waits_for_the_interval() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        let h = touch(&mut c, NOW + 10);
        assert_eq!(h.build, None);
        assert_eq!(c.poll(NOW + 10), Action::Wait);
        assert_eq!(c.poll(NOW + 899), Action::Wait);
        // The touch is remembered until the interval has passed.
        assert_eq!(c.poll(NOW + 900), Action::Build(BuildReason::Changed));
    }

    #[test]
    fn a_touch_at_the_interval_starts_the_build() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        let h = touch(&mut c, NOW + 900);
        assert_eq!(h.build, Some(BuildReason::Changed));
        assert_eq!(sent(&h)["building"], true);
    }

    #[test]
    fn refresh_secs_zero_builds_on_the_first_touch() {
        let mut c = fresh(0, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(c.poll(NOW), Action::Wait);
        let h = touch(&mut c, NOW);
        assert_eq!(h.build, Some(BuildReason::Changed));
    }

    #[test]
    fn a_touch_during_a_build_counts_for_the_next_round() {
        for touch_during in [true, false] {
            let mut c = fresh(900, 0);
            c.backend().set(|s| s.stamp = Some(STAMP2));
            assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
            if touch_during {
                assert_eq!(touch(&mut c, NOW + 901).build, None);
            }
            assert_eq!(c.poll(NOW + 902), Action::Wait);
            let r = c.backend().build(NOW + 900);
            // The build saw STAMP2; the store changed again meanwhile.
            put_db(&c, 2, NOW + 900, STAMP2);
            c.backend().set(|s| s.stamp = Some(STAMP));
            c.build_finished(r, NOW + 905);
            assert_eq!(c.poll(NOW + 905), Action::Wait);
            let want = if touch_during {
                Action::Build(BuildReason::Changed)
            } else {
                Action::Wait
            };
            assert_eq!(
                c.poll(NOW + 1800),
                want,
                "touch during build: {touch_during}"
            );
        }
    }

    #[test]
    fn database_or_attempt_time_after_now_does_not_block() {
        // A database built "in the future".
        let mut c = core(900, 0);
        put_db(&c, 1, NOW + 10 * DAY, STAMP);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(touch(&mut c, NOW).build, Some(BuildReason::Changed));

        // A failed attempt "in the future".
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = Some(STAMP2);
            s.fail = Some("boom".into());
        });
        run_build(&mut c, NOW + 10 * DAY, NOW + 10 * DAY);
        // Only the database's own time counts.
        assert_eq!(touch(&mut c, NOW + 899).build, None);
        assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
    }

    #[test]
    fn unreadable_store_stamp_builds_nothing() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = None);
        assert_eq!(touch(&mut c, NOW + 10 * DAY).build, None);
        assert_eq!(c.poll(NOW + 20 * DAY), Action::Wait);
        assert_eq!(c.status().store_changed, None);
    }

    // --- failures and exits ---

    #[test]
    fn failed_first_build_keeps_the_error_and_waits_for_the_interval() {
        let mut c = core(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        run_build(&mut c, NOW, NOW + 2);
        assert_eq!(c.status().last_error.as_deref(), Some("boom"));
        assert_eq!(touch(&mut c, NOW + 899).build, None);
        assert_eq!(c.poll(NOW + 899), Action::Wait);
        assert_eq!(c.status().last_error.as_deref(), Some("boom"));
        assert_eq!(c.poll(NOW + 900), Action::Build(BuildReason::First));
        assert_eq!(c.backend().get(|s| s.builds), 1);
    }

    #[test]
    fn failed_build_keeps_the_old_database_and_waits_for_the_interval() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = Some(STAMP2);
            s.fail = Some("boom".into());
        });
        assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
        let r = c.backend().build(NOW + 900);
        c.build_finished(r, NOW + 903);
        let st = c.status();
        assert_eq!(st.last_error.as_deref(), Some("boom"));
        assert!(st.db_usable);
        assert_eq!(st.built_unix, Some(NOW));
        assert_eq!(touch(&mut c, NOW + 1000).build, None);
        assert_eq!(c.poll(NOW + 1799), Action::Wait);
        assert_eq!(c.poll(NOW + 1800), Action::Build(BuildReason::Changed));
    }

    #[test]
    fn unusable_database_after_a_successful_build_is_rebuilt_once_per_minute() {
        for file in [false, true] {
            let mut c = core(900, 0);
            if file {
                // A file that the build left but that is not usable.
                c.backend().set(|s| s.db_id = Some(id(7)));
            }
            assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
            run_build(&mut c, NOW, NOW + 1);
            assert_eq!(c.status().last_error, None);
            assert_eq!(c.poll(NOW + 59), Action::Wait, "file: {file}");
            assert_eq!(c.poll(NOW + 60), Action::Build(BuildReason::First));
        }
    }

    #[test]
    fn store_permission_error_exits_at_poll() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = None;
            s.denied = true;
        });
        assert_eq!(c.poll(NOW), Action::Exit(Exit::Permission));
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Permission));
    }

    #[test]
    fn store_permission_error_exits_after_a_failed_build() {
        let mut c = core(900, 0);
        c.backend()
            .set(|s| s.fail = Some("permission denied".into()));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        c.build_started(NOW);
        c.backend().set(|s| s.denied = true);
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 1);
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Permission));

        // A failure for another reason does not end the service.
        let mut c = core(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        run_build(&mut c, NOW, NOW + 1);
        assert_eq!(c.poll(NOW + 1), Action::Wait);
    }

    #[test]
    fn idle_exit_counts_from_the_last_request() {
        let mut c = fresh(900, 100);
        assert_eq!(c.poll(NOW + 99), Action::Wait);
        c.handle(&Request::Status, NOW + 50);
        assert_eq!(c.poll(NOW + 149), Action::Wait);
        assert_eq!(c.poll(NOW + 150), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_exit_waits_for_a_build_and_counts_from_its_end() {
        let mut c = core(900, 100);
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        c.build_started(NOW);
        assert_eq!(c.poll(NOW + 500), Action::Wait);
        put_db(&c, 1, NOW, STAMP);
        c.build_finished(Ok(()), NOW + 500);
        assert_eq!(c.poll(NOW + 599), Action::Wait);
        assert_eq!(c.poll(NOW + 600), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_exit_also_without_a_database_after_a_failed_build() {
        let mut c = core(900, 100);
        c.backend().set(|s| s.fail = Some("boom".into()));
        run_build(&mut c, NOW, NOW + 1);
        assert_eq!(c.poll(NOW + 100), Action::Wait);
        assert_eq!(c.poll(NOW + 101), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_secs_zero_never_exits() {
        let mut c = fresh(900, 0);
        assert_eq!(c.poll(NOW + 3650 * DAY), Action::Wait);
    }

    // --- requests ---

    #[test]
    fn touch_returns_the_status() {
        let mut c = fresh(900, 0);
        let h = touch(&mut c, NOW + 5);
        assert_eq!(h.build, None);
        let v = sent(&h);
        assert_eq!(v["ok"], true);
        assert_eq!(v["building"], false);
        assert_eq!(v["version"], VERSION);

        // A touch that starts a build says so; the build is not run by
        // the core.
        let mut c = core(900, 0);
        let h = touch(&mut c, NOW);
        assert_eq!(h.build, Some(BuildReason::First));
        assert_eq!(sent(&h)["building"], true);
        assert_eq!(c.backend().get(|s| s.builds), 0);
        assert_eq!(c.poll(NOW), Action::Wait);
    }

    #[test]
    fn refresh_builds_whatever_the_interval_and_returns_the_generation() {
        let mut c = fresh(900, 0);
        let h = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(h.build, Some(BuildReason::Refresh));
        assert_eq!(h.reply, Reply::AfterBuild(1));
        assert_eq!(c.status().last_attempt_unix, Some(NOW + 1));
        let r = c.backend().build(NOW + 1);
        c.build_finished(r, NOW + 2);
        assert_eq!(c.generation(), 1);
        let v = c.refresh_reply();
        assert_eq!(v["ok"], true);
        assert_eq!(v["builds"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn refresh_joins_a_running_build() {
        let mut c = fresh(900, 0);
        let a = c.handle(&Request::Refresh, NOW);
        let b = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(a.build, Some(BuildReason::Refresh));
        assert_eq!(b.build, None);
        assert_eq!(a.reply, Reply::AfterBuild(1));
        assert_eq!(b.reply, Reply::AfterBuild(1));
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 2);
        // No second build for the second request.
        assert_eq!(c.poll(NOW + 2), Action::Wait);
        assert_eq!(c.backend().get(|s| s.builds), 1);

        // A refresh during a build started by a touch joins it too.
        let mut c = core(900, 0);
        assert_eq!(touch(&mut c, NOW).build, Some(BuildReason::First));
        let h = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(h.build, None);
        assert_eq!(h.reply, Reply::AfterBuild(1));
    }

    #[test]
    fn refresh_reply_of_a_failed_build() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        assert_eq!(c.handle(&Request::Refresh, NOW).reply, Reply::AfterBuild(1));
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 1);
        let v = c.refresh_reply();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom");
        assert_eq!(v["last_error"], "boom");
        assert_eq!(v["db_usable"], true);
        assert_eq!(v["pid"], std::process::id());
    }

    #[test]
    fn shutdown_with_the_own_version_stops() {
        let mut c = fresh(900, 0);
        let h = c.handle(
            &Request::Shutdown {
                version: VERSION.into(),
            },
            NOW,
        );
        assert_eq!(h.reply, Reply::Send(proto::stopping_reply()));
        assert_eq!(h.build, None);
        assert_eq!(c.poll(NOW), Action::Exit(Exit::Shutdown));

        // Also while a build runs.
        let mut c = core(900, 0);
        c.build_started(NOW);
        c.handle(
            &Request::Shutdown {
                version: VERSION.into(),
            },
            NOW + 1,
        );
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Shutdown));
    }

    #[test]
    fn shutdown_with_another_version_is_refused() {
        let mut c = fresh(900, 0);
        let h = c.handle(
            &Request::Shutdown {
                version: "0.0.1".into(),
            },
            NOW,
        );
        assert_eq!(h.reply, Reply::Send(proto::version_mismatch_reply(VERSION)));
        assert_eq!(c.poll(NOW), Action::Wait);
    }

    #[test]
    fn status_fields() {
        let mut c = fresh(900, 1800);
        let h = c.handle(&Request::Status, NOW + 3);
        assert_eq!(h.build, None);
        let st = ServiceStatus::from_json(sent(&h)).unwrap();
        assert_eq!(
            st,
            ServiceStatus {
                ok: true,
                error: None,
                version: VERSION.into(),
                pid: std::process::id(),
                store_path: Some(STORE.into()),
                building: false,
                db_usable: true,
                built_unix: Some(NOW),
                store_changed: Some(false),
                last_attempt_unix: None,
                last_error: None,
                refresh_secs: Some(900),
                idle_secs: Some(1800),
                started_unix: Some(NOW),
                builds: 0,
            }
        );

        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(c.status().store_changed, Some(true));

        // During a build, and after it.
        c.build_started(NOW + 10);
        let st = c.status();
        assert!(st.building);
        assert_eq!(st.last_attempt_unix, Some(NOW + 10));
        assert_eq!(st.last_error.as_deref(), Some(BUILD_UNFINISHED));
        c.build_finished(Ok(()), NOW + 12);
        let st = c.status();
        assert!(!st.building);
        assert_eq!(st.last_error, None);
        assert_eq!(st.builds, 1);

        // No database.
        let st = core(900, 1800).status();
        assert!(!st.db_usable);
        assert_eq!(st.built_unix, None);
        assert_eq!(st.store_changed, None);
    }

    #[test]
    fn database_info_is_read_again_only_when_the_file_changed() {
        let mut c = fresh(900, 0);
        for t in 0..5 {
            c.poll(NOW + t);
            c.status();
        }
        assert_eq!(c.backend().get(|s| s.db_reads), 1);
        put_db(&c, 2, NOW + 10, STAMP2);
        c.poll(NOW + 10);
        assert_eq!(c.backend().get(|s| s.db_reads), 2);
        assert_eq!(c.status().built_unix, Some(NOW + 10));

        // An unusable file is not opened again on every poll either.
        let mut c = core(900, 0);
        c.backend().set(|s| s.db_id = Some(id(3)));
        run_build(&mut c, NOW, NOW);
        for t in 1..5 {
            c.poll(NOW + t);
        }
        assert_eq!(c.backend().get(|s| s.db_reads), 1);
    }

    // --- runtime ---

    use crate::mcp::proto::{exchange, ExchangeError, LOCK_NAME, SOCKET_NAME};
    use crate::testutil::process_lock;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    const TEST_TICK: Duration = Duration::from_millis(10);
    const REQ: Duration = Duration::from_secs(2);

    /// A fresh directory with a short path under the system temp dir,
    /// removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxv-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }

        fn sock(&self) -> PathBuf {
            self.0.join(SOCKET_NAME)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Poll `cond` every 5 ms until it holds; fails the test after
    /// `timeout`.
    fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
        let started = Instant::now();
        while !cond() {
            assert!(started.elapsed() < timeout, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether SIGTERM or SIGINT is blocked in the calling thread.
    fn stop_signals_blocked() -> bool {
        // SAFETY: plain data; the calls only read and write `set`.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut set);
            libc::sigismember(&set, libc::SIGTERM) == 1
                || libc::sigismember(&set, libc::SIGINT) == 1
        }
    }

    /// `serve` running on a thread with a fake backend, a test clock and a
    /// 10 ms tick.
    struct Running {
        sock: PathBuf,
        clock: Arc<AtomicI64>,
        stop: Stop,
        backend: Arc<FakeBackend>,
        /// The exit, and whether `serve` left the stop signals blocked.
        handle: Option<thread::JoinHandle<(Exit, bool)>>,
        gate: Option<mpsc::Sender<()>>,
    }

    impl Running {
        /// Serve `core` on `dir`; with `gated` the first build blocks until
        /// [`Running::release`].
        fn start(dir: &TestDir, core: Core<FakeBackend>, gated: bool) -> Running {
            let backend = core.backend();
            let gate = gated.then(|| {
                let (tx, rx) = mpsc::channel();
                backend.set(|s| s.gate = Some(rx));
                tx
            });
            let listener = bind_socket(&dir.sock()).unwrap();
            let clock = Arc::new(AtomicI64::new(NOW));
            let c = Arc::clone(&clock);
            let stop = Stop::new();
            let s = stop.clone();
            let handle = thread::spawn(move || {
                let exit = serve(
                    core,
                    listener,
                    Arc::new(move || c.load(Ordering::SeqCst)),
                    TEST_TICK,
                    s,
                );
                (exit, stop_signals_blocked())
            });
            Running {
                sock: dir.sock(),
                clock,
                stop,
                backend,
                handle: Some(handle),
                gate,
            }
        }

        fn req(&self, r: &Request) -> Result<Value, ExchangeError> {
            exchange(&self.sock, r, REQ)
        }

        /// Send `r` on another thread.
        fn req_async(&self, r: Request) -> thread::JoinHandle<Result<Value, ExchangeError>> {
            let sock = self.sock.clone();
            thread::spawn(move || exchange(&sock, &r, Duration::from_secs(10)))
        }

        fn release(&mut self) {
            self.gate.take();
        }

        fn builds(&self) -> u64 {
            self.backend.get(|s| s.builds)
        }

        /// The exit of `serve`, which must come within `within`; checks
        /// that its accept thread has ended and that it blocked no signal.
        fn exit(&mut self, within: Duration) -> Exit {
            let h = self.handle.take().unwrap();
            wait_until("serve to return", within, || h.is_finished());
            let (exit, blocked) = h.join().unwrap();
            assert!(!blocked, "serve blocked a stop signal");
            if self.sock.exists() {
                // The listener is closed.
                assert!(UnixStream::connect(&self.sock).is_err());
            }
            exit
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.request(Exit::Signal);
            self.release();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// A core with an up-to-date database, never idle.
    fn served(dir: &TestDir, gated: bool) -> Running {
        Running::start(dir, fresh(900, 0), gated)
    }

    // --- InstanceLock ---

    #[test]
    fn instance_lock_writes_pid_and_version_privately() {
        let t = TestDir::new();
        let _l = InstanceLock::acquire(&t.0).unwrap();
        let p = t.0.join(LOCK_NAME);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            format!("{} {VERSION}\n", std::process::id())
        );
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn instance_lock_is_busy_while_held_and_free_after_drop() {
        let t = TestDir::new();
        let first = InstanceLock::acquire(&t.0).unwrap();
        let started = Instant::now();
        assert_eq!(InstanceLock::acquire(&t.0).unwrap_err(), LockError::Busy);
        // Tried again for a while before giving up.
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
        // The loser did not change the holder's record.
        assert_eq!(
            std::fs::read_to_string(t.0.join(LOCK_NAME)).unwrap(),
            format!("{} {VERSION}\n", std::process::id())
        );
        drop(first);
        InstanceLock::acquire(&t.0).unwrap();
    }

    #[test]
    fn instance_lock_released_shortly_after_the_first_attempt_is_taken() {
        let t = TestDir::new();
        let first = InstanceLock::acquire(&t.0).unwrap();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(first);
        });
        InstanceLock::acquire(&t.0).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn instance_lock_replaces_a_symlink() {
        let t = TestDir::new();
        let target = t.0.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, t.0.join(LOCK_NAME)).unwrap();
        let _l = InstanceLock::acquire(&t.0).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let m = std::fs::symlink_metadata(t.0.join(LOCK_NAME)).unwrap();
        assert!(m.file_type().is_file());
        // A file open to others is replaced too.
        drop(_l);
        let p = t.0.join(LOCK_NAME);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _l = InstanceLock::acquire(&t.0).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
    }

    // --- serve: requests ---

    #[test]
    fn serve_answers_status_and_touch() {
        let _p = process_lock();
        let t = TestDir::new();
        let r = served(&t, false);
        for req in [Request::Status, Request::Touch] {
            let v = r.req(&req).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["version"], VERSION);
            assert_eq!(v["pid"], std::process::id());
            assert_eq!(v["store_path"], STORE);
            assert_eq!(v["building"], false);
        }
        assert_eq!(r.builds(), 0);
    }

    #[test]
    fn touch_on_a_stale_cache_starts_a_build_without_waiting_for_it() {
        let _p = process_lock();
        let t = TestDir::new();
        let core = fresh(0, 0);
        core.backend().set(|s| s.stamp = Some(STAMP2));
        let mut r = Running::start(&t, core, true);
        let v = r.req(&Request::Touch).unwrap();
        assert_eq!(v["building"], true);
        wait_until("the build", REQ, || r.builds() == 1);
        // The build is blocked; status still answers.
        let v = r.req(&Request::Status).unwrap();
        assert_eq!(v["building"], true);
        assert_eq!(v["builds"], 0);
        r.release();
        wait_until("the build end", REQ, || {
            r.req(&Request::Status).unwrap()["building"] == false
        });
        assert_eq!(r.req(&Request::Status).unwrap()["builds"], 1);
    }

    #[test]
    fn first_build_runs_at_start_and_status_answers_meanwhile() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = Running::start(&t, core(900, 0), true);
        wait_until("the first build", REQ, || r.builds() == 1);
        let v = r.req(&Request::Status).unwrap();
        assert_eq!(v["building"], true);
        assert_eq!(v["db_usable"], false);
        r.release();
    }

    #[test]
    fn two_refreshes_during_one_build_share_it() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = served(&t, true);
        let a = r.req_async(Request::Refresh);
        wait_until("the build", REQ, || r.builds() == 1);
        let b = r.req_async(Request::Refresh);
        // Both wait for the blocked build.
        thread::sleep(Duration::from_millis(100));
        assert!(!a.is_finished() && !b.is_finished());
        r.release();
        for h in [a, b] {
            let v = h.join().unwrap().unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["builds"], 1);
            assert_eq!(v["building"], false);
        }
        assert_eq!(r.builds(), 1);
    }

    #[test]
    fn failed_refresh_build_replies_with_the_error() {
        let _p = process_lock();
        let t = TestDir::new();
        let r = served(&t, false);
        r.backend.set(|s| s.fail = Some("boom".into()));
        let v = r.req(&Request::Refresh).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom");
        assert_eq!(v["last_error"], "boom");
        assert_eq!(v["db_usable"], true);
    }

    // --- serve: ends ---

    #[test]
    fn serve_returns_idle_when_the_clock_passes_idle_secs() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = Running::start(&t, fresh(900, 100), false);
        r.req(&Request::Status).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(!r.handle.as_ref().unwrap().is_finished());
        r.clock.store(NOW + 100, Ordering::SeqCst);
        assert_eq!(r.exit(REQ), Exit::Idle);
    }

    #[test]
    fn shutdown_ends_serve_even_during_a_build() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = served(&t, true);
        let refresh = r.req_async(Request::Refresh);
        wait_until("the build", REQ, || r.builds() == 1);
        // Another version is refused.
        let v = r
            .req(&Request::Shutdown {
                version: "0.0.1".into(),
            })
            .unwrap();
        assert_eq!(v, proto::version_mismatch_reply(VERSION));
        let v = r
            .req(&Request::Shutdown {
                version: VERSION.into(),
            })
            .unwrap();
        assert_eq!(v, json!({"ok": true, "stopping": true}));
        assert_eq!(r.exit(REQ), Exit::Shutdown);
        r.release();
        // The waiting refresh gets no reply.
        assert!(refresh.join().unwrap().is_err());
    }

    #[test]
    fn stop_request_ends_serve() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = served(&t, false);
        r.req(&Request::Status).unwrap();
        r.stop.request(Exit::Signal);
        assert_eq!(r.exit(Duration::from_millis(500)), Exit::Signal);
    }

    #[test]
    fn removed_socket_ends_serve() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut r = served(&t, false);
        r.req(&Request::Status).unwrap();
        std::fs::remove_file(t.sock()).unwrap();
        assert_eq!(r.exit(REQ), Exit::SocketGone);

        // Also when it was replaced by another socket.
        let t = TestDir::new();
        let mut r = served(&t, false);
        std::fs::remove_file(t.sock()).unwrap();
        let other = UnixListener::bind(t.sock()).unwrap();
        let h = r.handle.as_ref().unwrap();
        wait_until("serve to return", REQ, || h.is_finished());
        // The other listener is not serve's to close.
        drop(other);
        assert_eq!(r.exit(REQ), Exit::SocketGone);
    }

    // --- robustness ---

    #[test]
    fn bind_socket_replaces_a_stale_socket_and_is_private() {
        let _p = process_lock();
        let t = TestDir::new();
        drop(UnixListener::bind(t.sock()).unwrap());
        assert!(t.sock().exists());
        let r = served(&t, false);
        r.req(&Request::Status).unwrap();
        let m = std::fs::symlink_metadata(t.sock()).unwrap();
        assert_eq!(m.mode() & 0o777, 0o600);
    }

    #[test]
    fn bad_and_silent_clients_do_not_stop_serve() {
        let _p = process_lock();
        let t = TestDir::new();
        let r = served(&t, false);
        // Connects and sends nothing.
        let _silent = UnixStream::connect(t.sock()).unwrap();
        // A line over the limit.
        let mut s = UnixStream::connect(t.sock()).unwrap();
        s.set_read_timeout(Some(REQ)).unwrap();
        let long = format!("{{\"op\":\"status\",\"pad\":\"{}\"}}\n", "x".repeat(5000));
        let _ = s.write_all(long.as_bytes());
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        assert_eq!(reply, format!("{}\n", proto::bad_request_reply()));
        // Not a request.
        let mut s = UnixStream::connect(t.sock()).unwrap();
        s.set_read_timeout(Some(REQ)).unwrap();
        s.write_all(b"hello\n").unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, format!("{}\n", proto::bad_request_reply()));
        // Still served, while the silent client is connected.
        assert_eq!(r.req(&Request::Status).unwrap()["ok"], true);
    }

    #[test]
    fn connections_over_the_limit_are_closed_at_once() {
        let _p = process_lock();
        let t = TestDir::new();
        let r = served(&t, false);
        let held: Vec<UnixStream> = (0..proto::MAX_CONNECTIONS)
            .map(|_| UnixStream::connect(t.sock()).unwrap())
            .collect();
        let started = Instant::now();
        assert_eq!(r.req(&Request::Status), Err(ExchangeError::BadReply));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        wait_until("a free connection slot", REQ, || {
            r.req(&Request::Status).is_ok()
        });
    }
}
