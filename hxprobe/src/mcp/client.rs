//! The client side of the refresh service, used by `hxprobe mcp`: make
//! sure a service answers for the cache directory (start one as a
//! detached child, replace an older one), then send the request.
//!
//! A client never takes, creates or removes `service.lock`; it learns that
//! an old service is gone from its pid ([`proto::pid_gone`]).

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::mcp::proto::{self, ExchangeError, Request, ServiceStatus, ShutdownReply};
use crate::mcp::{cache, log, service};

/// The timeouts of a [`Client`] (see [`Client::with_timeouts`]).
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    /// How long a started service may take to answer.
    start: Duration,
    /// Timeout of one `status`, `touch` or `shutdown` exchange.
    probe: Duration,
    /// How long an old service may take to be gone after `shutdown`.
    stop_wait: Duration,
    /// Timeout of a `refresh` exchange, which waits for a build.
    refresh: Duration,
}

/// Talks to the refresh service of one cache directory; starts it when
/// none answers and replaces an older one. See [`proto::Refresher`].
#[derive(Debug)]
pub struct Client {
    dir: PathBuf,
    sock: PathBuf,
    /// The store of this server, as the service reports its own.
    store: PathBuf,
    program: PathBuf,
    /// What a denial text names; its program defaults to `program`.
    remedy: proto::Remedy,
    refresh_secs: u64,
    idle_secs: u64,
    /// Services this process started and has not seen exit.
    children: Vec<Child>,
    /// The pid of the service whose replies were accepted last.
    known_pid: Option<u32>,
    /// No start before this time (unix seconds), after a failed start.
    blocked_until: Option<i64>,
    /// The error of the failed start, returned while starts are blocked.
    last_error: Option<String>,
    /// When this process last started a service (unix seconds).
    last_start_unix: Option<i64>,
    /// A service this process started exited because it was denied the
    /// store, and none it started has answered since.
    own_denied: bool,
    timeouts: Timeouts,
    /// The clock of the start timeout: [`Instant::now`]; tests replace it.
    clock: fn() -> Instant,
}

/// How often a reply may lead to another round (a replaced or refusing
/// service) before the client gives up.
const MAX_ROUNDS: u32 = 3;

impl Client {
    /// A client for the service of cache directory `dir` serving `store`,
    /// started from `program` with `--refresh-secs` and `--idle-secs`. An
    /// error when the socket path does not fit (see [`proto::socket_path`]).
    pub fn new(
        dir: &Path,
        store: &Path,
        program: PathBuf,
        refresh_secs: u64,
        idle_secs: u64,
    ) -> Result<Client, String> {
        let dir = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let sock = proto::socket_path(&dir)?;
        // The service reports its store as a canonical path.
        let store = std::fs::canonicalize(store)
            .or_else(|_| std::path::absolute(store))
            .map_err(|e| format!("{}: {e}", store.display()))?;
        Ok(Client {
            dir,
            sock,
            store,
            program,
            remedy: proto::Remedy::default(),
            refresh_secs,
            idle_secs,
            children: Vec::new(),
            known_pid: None,
            blocked_until: None,
            last_error: None,
            last_start_unix: None,
            own_denied: false,
            timeouts: Timeouts {
                start: proto::START_TIMEOUT,
                probe: proto::REQUEST_TIMEOUT,
                stop_wait: proto::GONE_WAIT,
                refresh: proto::REFRESH_TIMEOUT,
            },
            clock: Instant::now,
        })
    }

    /// Shorter timeouts, for tests: the start timeout, the timeout of one
    /// `status`, `touch` or `shutdown` exchange, and the wait for an old
    /// service to be gone.
    pub fn with_timeouts(
        mut self,
        start: Duration,
        probe: Duration,
        stop_wait: Duration,
    ) -> Client {
        self.timeouts = Timeouts {
            start,
            probe,
            stop_wait,
            ..self.timeouts
        };
        self
    }

    /// What the error of a denied start names (see [`proto::denied_text`]);
    /// without a program it names the program the service is started from.
    pub fn with_remedy(mut self, remedy: proto::Remedy) -> Client {
        self.remedy = remedy;
        self
    }

    /// Send `req` to the service, starting or replacing it first when
    /// needed; the service's status reply. A `refresh` whose build failed
    /// is an error with the build's error text.
    pub fn call(&mut self, req: &Request, now: i64) -> Result<ServiceStatus, String> {
        self.call_round(req, now, 0)
    }

    /// [`Client::call`], in its `round`-th attempt.
    fn call_round(&mut self, req: &Request, now: i64, round: u32) -> Result<ServiceStatus, String> {
        if round > MAX_ROUNDS {
            return Err(format!(
                "the refresh service changed while it was asked; see {}",
                self.log_path().display()
            ));
        }
        self.reap();
        match proto::exchange(&self.sock, req, self.timeout(req)) {
            Ok(v) => return self.accept(req, &v, now, round),
            Err(ExchangeError::BadReply) => return Err(self.unreadable()),
            Err(ExchangeError::Timeout) if *req == Request::Refresh => {
                return Err(self.refresh_pending())
            }
            Err(ExchangeError::NotRunning | ExchangeError::Timeout) => {}
        }
        if let Some(e) = self.blocked(now) {
            return Err(e);
        }
        let start = self.children.is_empty()
            && self
                .last_start_unix
                .is_none_or(|t| now - t >= proto::SILENT_PEER.as_secs() as i64);
        self.start_and_probe(req, now, round, start)
    }

    /// The reply `v` to `req`: checked when it comes from a service not
    /// seen before (its store, then its version; an older one is replaced,
    /// unless this program is denied the store, or a service this process
    /// started was).
    fn accept(
        &mut self,
        req: &Request,
        v: &serde_json::Value,
        now: i64,
        round: u32,
    ) -> Result<ServiceStatus, String> {
        let st = ServiceStatus::from_json(v).map_err(|_| self.unreadable())?;
        if self.known_pid != Some(st.pid) {
            if let Some(theirs) = st.store_path.as_deref() {
                if Path::new(theirs) != self.store {
                    return Err(format!(
                        "this cache directory serves another store: {theirs}; use another --cache-dir"
                    ));
                }
            }
            if proto::cmp_versions(&st.version, proto::VERSION) == Ordering::Less {
                // This program is denied the store, while an older one
                // (another file, perhaps with Full Disk Access) can read it:
                // keep that one instead of stopping it for a start that
                // fails. This process runs the program file its service
                // would be started from, so its own attempt to open the
                // store tells before any start; `own_denied` covers a
                // service that was denied all the same. Safe only while the
                // cache format is unchanged (`cache::FORMAT_VERSION` "3"
                // since 0.2.0); a release that changes the format has to
                // revisit this branch.
                if !(self.own_denied || cache::store_denied(&self.store)) {
                    return self.replace_older(req, &st, now, round);
                }
                log(&format!(
                    "keeping refresh service {} (pid {}): this program is denied the store",
                    st.version, st.pid
                ));
            } else {
                log(&format!(
                    "using refresh service {} (pid {})",
                    st.version, st.pid
                ));
            }
            self.known_pid = Some(st.pid);
        }
        // A service this process started answers: its program can read
        // the store.
        if self.children.iter().any(|c| c.id() == st.pid) {
            self.own_denied = false;
        }
        self.blocked_until = None;
        self.last_error = None;
        if !st.ok {
            return Err(st
                .error
                .unwrap_or_else(|| "the refresh service reported an error".into()));
        }
        Ok(st)
    }

    /// Stop the older service `old`, wait until it is gone, start this
    /// program's service and send it `req`.
    fn replace_older(
        &mut self,
        req: &Request,
        old: &ServiceStatus,
        now: i64,
        round: u32,
    ) -> Result<ServiceStatus, String> {
        log(&format!(
            "replacing refresh service {} (pid {})",
            old.version, old.pid
        ));
        let shutdown = Request::Shutdown {
            version: old.version.clone(),
        };
        match proto::exchange(&self.sock, &shutdown, self.timeouts.probe) {
            Ok(v) => match ShutdownReply::from_json(&v) {
                ShutdownReply::Stopping => {}
                // Replaced meanwhile, or it will not stop: look again.
                _ => return self.call_round(req, now, round + 1),
            },
            // Gone meanwhile: the wait below returns at once.
            Err(ExchangeError::NotRunning) => {}
            Err(e) => {
                return Err(self.block(now, proto::START_BLOCK_SECS, self.not_stopped(old, &e)))
            }
        }
        // Only the pid tells that it is gone: it removes its socket before
        // it exits and releases `service.lock`.
        if !proto::wait_gone(old.pid, self.timeouts.stop_wait) {
            match proto::exchange(&self.sock, &Request::Status, self.timeouts.probe) {
                // No socket or nobody listening: a start can take the lock.
                Err(ExchangeError::NotRunning) => {}
                Ok(_) | Err(_) => {
                    let e = self.not_stopped(old, &"it did not stop in time");
                    return Err(self.block(now, proto::START_BLOCK_SECS, e));
                }
            }
        }
        self.reap();
        self.start_and_probe(req, now, round + 1, true)
    }

    /// Start a service when `start`, then send `req` until a service
    /// answers or the start timeout passed.
    fn start_and_probe(
        &mut self,
        req: &Request,
        now: i64,
        round: u32,
        start: bool,
    ) -> Result<ServiceStatus, String> {
        let deadline = (self.clock)() + self.timeouts.start;
        // The child started here while it runs, and when the last start
        // was. Without a start now (a service of this process is still
        // there, or one was started a moment ago), that is the earlier
        // start: when nobody answers, the loop below starts one as soon as
        // none of this process is left and the silent-peer delay passed.
        let mut child: Option<u32> = None;
        let mut spawned_at = if start {
            let (pid, at) = self.spawn(now)?;
            child = Some(pid);
            at
        } else {
            let limit = proto::SILENT_PEER.as_secs();
            let ago = self.last_start_unix.map_or(limit, |t| {
                u64::try_from(now.saturating_sub(t)).map_or(0, |s| s.min(limit))
            });
            let at = Instant::now();
            at.checked_sub(Duration::from_secs(ago)).unwrap_or(at)
        };
        let mut probes = 0u32;
        loop {
            if let Some(pid) = child {
                match self.child_outcome(pid) {
                    StartOutcome::Running => {}
                    // A peer took the lock; its service should answer.
                    StartOutcome::Lost => child = None,
                    StartOutcome::Denied => {
                        self.own_denied = true;
                        let e = self.denied();
                        return Err(self.block(now, proto::PERMISSION_BLOCK_SECS, e));
                    }
                    StartOutcome::Failed(status) => {
                        let e = format!(
                            "the refresh service exited at start ({status}); see {}",
                            self.log_path().display()
                        );
                        return Err(self.block(now, proto::START_BLOCK_SECS, e));
                    }
                }
            }
            // The peer that won the lock stays silent, or the service that
            // was there is gone: start again.
            if child.is_none() && spawned_at.elapsed() >= proto::SILENT_PEER && self.no_children() {
                let (pid, at) = self.spawn(now)?;
                (child, spawned_at) = (Some(pid), at);
            }
            match proto::exchange(&self.sock, req, self.timeout(req)) {
                Ok(v) => return self.accept(req, &v, now, round),
                Err(ExchangeError::BadReply) => return Err(self.unreadable()),
                Err(ExchangeError::Timeout) if *req == Request::Refresh => {
                    return Err(self.refresh_pending())
                }
                Err(ExchangeError::NotRunning | ExchangeError::Timeout) => {}
            }
            if (self.clock)() >= deadline {
                let e = format!(
                    "the refresh service did not start; see {}",
                    self.log_path().display()
                );
                return Err(self.block(now, proto::START_BLOCK_SECS, e));
            }
            std::thread::sleep(if probes < proto::FAST_PROBES {
                proto::FAST_PROBE
            } else {
                proto::SLOW_PROBE
            });
            probes += 1;
        }
    }

    /// Start a service; its pid and when. A failure blocks starts.
    fn spawn(&mut self, now: i64) -> Result<(u32, Instant), String> {
        self.last_start_unix = Some(now);
        log(&format!(
            "starting the refresh service: {}",
            self.program.display()
        ));
        let spawned = cache::ensure_dir(&self.dir).and_then(|()| {
            let log =
                open_log(&self.dir).map_err(|e| format!("{}: {e}", self.log_path().display()))?;
            let (refresh, idle) = (self.refresh_secs.to_string(), self.idle_secs.to_string());
            let args: [&OsStr; 9] = [
                OsStr::new("service"),
                OsStr::new("--store"),
                self.store.as_os_str(),
                OsStr::new("--cache-dir"),
                self.dir.as_os_str(),
                OsStr::new("--refresh-secs"),
                OsStr::new(&refresh),
                OsStr::new("--idle-secs"),
                OsStr::new(&idle),
            ];
            spawn_detached(&self.program, &args, &self.dir, log)
                .map_err(|e| format!("cannot start {}: {e}", self.program.display()))
        });
        match spawned {
            Ok(child) => {
                let pid = child.id();
                self.children.push(child);
                Ok((pid, Instant::now()))
            }
            Err(e) => Err(self.block(now, proto::START_BLOCK_SECS, e)),
        }
    }

    /// What became of the child `pid` this process started; an exited one
    /// is no longer kept.
    fn child_outcome(&mut self, pid: u32) -> StartOutcome {
        let Some(i) = self.children.iter().position(|c| c.id() == pid) else {
            return StartOutcome::Failed("status unknown".into());
        };
        let outcome = start_outcome(self.children[i].try_wait());
        if outcome != StartOutcome::Running {
            // Waited for already: `try_wait` returned its status (or
            // found it gone).
            self.children.retain(|c| c.id() != pid);
        }
        outcome
    }

    /// Forget the children that exited; one whose status cannot be read
    /// counts as exited.
    fn reap(&mut self) {
        self.children
            .retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }

    /// Whether no service this process started is left.
    fn no_children(&mut self) -> bool {
        self.reap();
        self.children.is_empty()
    }

    /// The remembered error while starts are blocked at `now`.
    fn blocked(&self, now: i64) -> Option<String> {
        match self.blocked_until {
            Some(t) if now < t => self.last_error.clone(),
            _ => None,
        }
    }

    /// Block starts for `secs` from `now` with error `e`; returns `e`.
    fn block(&mut self, now: i64, secs: i64, e: String) -> String {
        log(&format!("refresh service not available: {e}"));
        self.blocked_until = Some(now + secs);
        self.last_error = Some(e.clone());
        e
    }

    fn timeout(&self, req: &Request) -> Duration {
        match req {
            Request::Refresh => self.timeouts.refresh,
            _ => self.timeouts.probe,
        }
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(proto::LOG_NAME)
    }

    /// The error of a `refresh` that got no reply in time: a service
    /// answered the connection and its build goes on, so this is neither a
    /// reason to start one nor to block starts.
    fn refresh_pending(&self) -> String {
        format!(
            "the rebuild did not finish within {} s and may still be running; \
             call store_status later",
            self.timeouts.refresh.as_secs()
        )
    }

    /// The error of a service that exited for a permission error.
    fn denied(&self) -> String {
        let remedy = proto::Remedy {
            program: Some(self.remedy.program.clone().unwrap_or(self.program.clone())),
            note: self.remedy.note.clone(),
        };
        proto::denied_text(proto::Denied::Service, &remedy, &self.dir)
    }

    fn unreadable(&self) -> String {
        format!(
            "{}; see {}",
            ExchangeError::BadReply,
            self.log_path().display()
        )
    }

    fn not_stopped(&self, old: &ServiceStatus, why: &dyn std::fmt::Display) -> String {
        format!(
            "the older refresh service {} (pid {}) did not stop: {why}; see {}",
            old.version,
            old.pid,
            self.log_path().display()
        )
    }
}

impl proto::Refresher for Client {
    fn touch(&mut self, now: i64) -> Result<ServiceStatus, String> {
        self.call(&Request::Touch, now)
    }

    fn status(&mut self, now: i64) -> Result<ServiceStatus, String> {
        self.call(&Request::Status, now)
    }

    fn refresh(&mut self, now: i64) -> Result<ServiceStatus, String> {
        self.call(&Request::Refresh, now)
    }

    /// The canonical store path the server resolved after it started: the
    /// one a service is started with and must report from now on.
    fn set_store(&mut self, store: &Path) {
        self.store = store.to_path_buf();
    }
}

/// The program to start the service from: this program's own file.
pub fn service_program() -> Result<PathBuf, String> {
    program_from(
        std::env::current_exe(),
        std::env::args_os().next().as_deref(),
    )
}

/// [`service_program`] from the path of the running program and
/// `argv[0]`. On Linux the path reads `<path> (deleted)` once the file was
/// replaced; `argv[0]`, made absolute, is used then when it is an existing
/// file.
fn program_from(exe: io::Result<PathBuf>, argv0: Option<&OsStr>) -> Result<PathBuf, String> {
    let exe = exe.map_err(|e| format!("cannot find this program's file: {e}"))?;
    if !exe.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Ok(exe);
    }
    argv0
        .and_then(|a| std::path::absolute(a).ok())
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            "this program's file was replaced while it runs; restart the MCP client".into()
        })
}

/// What a started service's exit status says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StartOutcome {
    Running,
    /// Exit 75: another service holds the lock.
    Lost,
    /// Exit 77: the store cannot be read.
    Denied,
    /// Any other exit, or a status that cannot be read; the status.
    Failed(String),
}

/// [`StartOutcome`] of a `try_wait` result; an error (`ECHILD`) is an exit
/// with an unknown status.
fn start_outcome(r: io::Result<Option<ExitStatus>>) -> StartOutcome {
    match r {
        Ok(None) => StartOutcome::Running,
        Ok(Some(st)) => match st.code() {
            Some(service::EXIT_BUSY) => StartOutcome::Lost,
            Some(service::EXIT_PERMISSION) => StartOutcome::Denied,
            _ => StartOutcome::Failed(st.to_string()),
        },
        Err(_) => StartOutcome::Failed("status unknown".into()),
    }
}

/// Open `service.log` of `dir` for appending, created 0600; rotate it to
/// `service.log.1` first when it is larger than [`proto::LOG_ROTATE_BYTES`].
/// An existing one that is not a private regular file is removed and
/// created again; a symbolic link is never followed.
fn open_log(dir: &Path) -> io::Result<File> {
    let path = dir.join(proto::LOG_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if cache::check_private(&m).is_err() => std::fs::remove_file(&path)?,
        Ok(m) if m.len() > proto::LOG_ROTATE_BYTES => {
            std::fs::rename(&path, dir.join(proto::LOG_OLD_NAME))?
        }
        _ => {}
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
}

/// Most descriptors [`spawn_detached`] marks close-on-exec.
const MAX_FDS: libc::c_int = 4096;

/// Start `program` with `args` in a session of its own, stdin null and
/// stdout and stderr to `log`, in `dir`. It inherits no descriptor beyond
/// these three: std marks a new descriptor close-on-exec only after
/// creating it on some systems, so one made by another thread meanwhile
/// would leak into the child.
fn spawn_detached(program: &Path, args: &[&OsStr], dir: &Path, log: File) -> io::Result<Child> {
    let limit = fd_limit();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir(dir);
    // SAFETY: the closure calls only async-signal-safe functions (setsid,
    // fcntl) and allocates nothing. Descriptors are marked, not closed:
    // `Command` reports a failed exec through a close-on-exec pipe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            for fd in 3..limit {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// The soft limit of open descriptors, at most [`MAX_FDS`].
fn fd_limit() -> libc::c_int {
    // SAFETY: rlimit is plain data; all zeroes is a valid value.
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `rl` is a writable rlimit for the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0
        || rl.rlim_cur == libc::RLIM_INFINITY
    {
        return MAX_FDS;
    }
    libc::c_int::try_from(rl.rlim_cur).map_or(MAX_FDS, |n| n.min(MAX_FDS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::process_lock;
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    const NOW: i64 = 1_800_000_000;
    const SHORT: Duration = Duration::from_millis(300);
    /// A start timeout no started script comes near, also under load.
    const LONG: Duration = Duration::from_secs(5);
    /// Well before [`LONG`]: a call that returned at a script's exit.
    const AT_ONCE: Duration = Duration::from_secs(2);

    /// The pid file of the child whose exit [`after_exit_clock`] waits
    /// for, and how often that clock has seen the child gone.
    static EXIT_GATE: Mutex<Option<(PathBuf, u32)>> = Mutex::new(None);

    /// Arms [`after_exit_clock`] while it lives.
    struct ExitGate;

    impl ExitGate {
        fn set(pid_file: PathBuf) -> ExitGate {
            *EXIT_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some((pid_file, 0));
            ExitGate
        }

        /// Whether the clock has jumped past every timeout.
        fn passed() -> bool {
            EXIT_GATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|(_, seen)| *seen >= 2)
        }
    }

    impl Drop for ExitGate {
        fn drop(&mut self) {
            *EXIT_GATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// A start-timeout clock: real time until it has seen the child of
    /// [`EXIT_GATE`] gone at two of its readings, an hour later from then.
    /// The client reads it once before the start (no pid file yet) and then
    /// at the end of each probe round, after it looked at its child; so by
    /// the second reading that saw the child gone, the client has seen the
    /// exit too.
    fn after_exit_clock() -> Instant {
        let now = Instant::now();
        let mut gate = EXIT_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let Some((pid_file, seen)) = gate.as_mut() else {
            return now;
        };
        let gone = std::fs::read_to_string(&*pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(proto::pid_gone);
        if gone && *seen < 2 {
            *seen += 1;
        }
        if *seen >= 2 {
            now + Duration::from_secs(3600)
        } else {
            now
        }
    }

    /// A short scratch directory with a store file and the cache directory
    /// `c`; removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxc-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(p.join("c")).unwrap();
            std::fs::set_permissions(p.join("c"), std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(p.join("HxStore.hxd"), b"store").unwrap();
            TestDir(p)
        }

        fn cache(&self) -> PathBuf {
            self.0.join("c")
        }

        fn store(&self) -> PathBuf {
            std::fs::canonicalize(self.0.join("HxStore.hxd")).unwrap()
        }

        fn sock(&self) -> PathBuf {
            self.cache().join(proto::SOCKET_NAME)
        }

        /// A shell script `name` that records each run in `c/runs`, then
        /// runs `body`.
        fn script(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, format!("#!/bin/sh\necho run >> runs\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }

        /// How often a script ran.
        fn runs(&self) -> usize {
            std::fs::read_to_string(self.cache().join("runs"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }

        fn client(&self, program: PathBuf) -> Client {
            Client::new(&self.cache(), &self.store(), program, 900, 60)
                .unwrap()
                .with_timeouts(SHORT, SHORT, SHORT)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A service played by the test on `sock`: answers every request with
    /// `reply(request)` and records the requests. A `shutdown` that
    /// `reply` accepts removes the socket and ends the play.
    struct Played {
        requests: Arc<Mutex<Vec<Request>>>,
        done: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Played {
        fn start(sock: &Path, reply: impl Fn(&Request) -> String + Send + 'static) -> Played {
            let listener = UnixListener::bind(sock).unwrap();
            listener.set_nonblocking(true).unwrap();
            let sock = sock.to_path_buf();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let done = Arc::new(AtomicBool::new(false));
            let (seen, stop) = (Arc::clone(&requests), Arc::clone(&done));
            let thread = thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok((s, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    s.set_nonblocking(false).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut line = String::new();
                    if BufReader::new(&s).read_line(&mut line).is_err() {
                        continue;
                    }
                    let Ok(req) = Request::parse(&line) else {
                        continue;
                    };
                    seen.lock().unwrap().push(req.clone());
                    let out = reply(&req);
                    let _ = (&s).write_all(out.as_bytes());
                    if matches!(req, Request::Shutdown { .. }) && out.contains("stopping") {
                        let _ = std::fs::remove_file(&sock);
                        return;
                    }
                }
            });
            Played {
                requests,
                done,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for Played {
        fn drop(&mut self) {
            self.done.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// A status reply line of a played service.
    fn status_line(version: &str, pid: u32, store: &Path) -> String {
        format!(
            "{}\n",
            json!({"ok": true, "version": version, "pid": pid,
                   "store_path": store.to_str().unwrap(), "building": false,
                   "db_usable": true, "builds": 0})
        )
    }

    /// The pid of a process that existed and is gone.
    fn gone_pid() -> u32 {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    fn shutdown(version: &str) -> Request {
        Request::Shutdown {
            version: version.into(),
        }
    }

    // --- version and store decisions ---

    #[test]
    fn equal_and_newer_services_are_used_without_shutdown() {
        let _p = process_lock();
        for version in [proto::VERSION, "99.0.0"] {
            let t = TestDir::new();
            let store = t.store();
            let v = version.to_string();
            let played = Played::start(&t.sock(), move |_| status_line(&v, 4242, &store));
            let mut c = t.client(t.script("svc", "exit 3"));
            let st = c.call(&Request::Touch, NOW).unwrap();
            assert_eq!(st.version, version);
            assert_eq!(st.pid, 4242);
            // A known service is asked again without a second check.
            c.call(&Request::Touch, NOW).unwrap();
            drop(c);
            assert_eq!(played.requests(), [Request::Touch, Request::Touch]);
            assert_eq!(t.runs(), 0);
        }
    }

    #[test]
    fn older_service_is_asked_to_stop_with_its_own_version() {
        let _p = process_lock();
        let t = TestDir::new();
        let store = t.store();
        let pid = gone_pid();
        let played = Played::start(&t.sock(), move |r| match r {
            Request::Shutdown { .. } => "{\"ok\":true,\"stopping\":true}\n".into(),
            _ => status_line("0.0.1", pid, &store),
        });
        // The replacement cannot start: its error is the result. The start
        // timeout leaves the script all the time it needs to run.
        let mut c = t
            .client(t.script("svc", "exit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains("exit status: 3"), "{e}");
        assert_eq!(played.requests(), [Request::Touch, shutdown("0.0.1")]);
        assert_eq!(t.runs(), 1);
    }

    #[test]
    fn older_service_is_kept_at_first_contact_while_the_store_is_denied() {
        let _p = process_lock();
        // SAFETY: geteuid() has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            // Mode 000 does not deny root.
            return;
        }
        let t = TestDir::new();
        let store = t.store();
        let mode = |m| std::fs::set_permissions(&store, std::fs::Permissions::from_mode(m));
        let reported = store.clone();
        let played = Played::start(&t.sock(), move |r| match r {
            Request::Shutdown { .. } => "{\"ok\":true,\"stopping\":true}\n".into(),
            _ => status_line("0.0.1", 4242, &reported),
        });
        let mut c = t
            .client(t.script("svc", "exit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        // This program cannot read the store: its own service could not
        // either, so the older one is used as it is.
        mode(0o000).unwrap();
        let first = c.call(&Request::Touch, NOW);
        let second = c.call(&Request::Status, NOW + 1);
        mode(0o600).unwrap();
        let st = first.unwrap();
        assert_eq!((st.version.as_str(), st.pid), ("0.0.1", 4242));
        assert_eq!(second.unwrap().pid, 4242);
        assert_eq!(played.requests(), [Request::Touch, Request::Status]);
        assert_eq!(t.runs(), 0);
        assert!(!c.own_denied);
        drop(played);
        std::fs::remove_file(t.sock()).unwrap();

        // The store can be read again: an older service not seen before is
        // replaced as usual.
        let (reported, gone) = (store.clone(), gone_pid());
        let played = Played::start(&t.sock(), move |r| match r {
            Request::Shutdown { .. } => "{\"ok\":true,\"stopping\":true}\n".into(),
            _ => status_line("0.0.1", gone, &reported),
        });
        let e = c.call(&Request::Touch, NOW + 2).unwrap_err();
        assert!(e.contains("exit status: 3"), "{e}");
        assert_eq!(played.requests(), [Request::Touch, shutdown("0.0.1")]);
        assert_eq!(t.runs(), 1);
    }

    #[test]
    fn service_of_another_store_is_an_error_and_not_stopped() {
        let _p = process_lock();
        let t = TestDir::new();
        let played = Played::start(&t.sock(), |_| {
            status_line("0.0.1", 4242, Path::new("/other/HxStore.hxd"))
        });
        let mut c = t.client(t.script("svc", "exit 3"));
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(
            e.contains("serves another store: /other/HxStore.hxd"),
            "{e}"
        );
        assert!(e.contains("--cache-dir"), "{e}");
        // Every call checks again.
        assert!(c.call(&Request::Touch, NOW).is_err());
        assert_eq!(played.requests(), [Request::Touch, Request::Touch]);
        assert_eq!(t.runs(), 0);
    }

    #[test]
    fn set_store_changes_the_store_a_service_must_report_and_is_started_with() {
        use proto::Refresher;
        let _p = process_lock();
        let t = TestDir::new();
        std::fs::write(t.0.join("Other.hxd"), b"store").unwrap();
        let other = std::fs::canonicalize(t.0.join("Other.hxd")).unwrap();
        let theirs = other.clone();
        let played = Played::start(&t.sock(), move |_| {
            status_line(proto::VERSION, 4242, &theirs)
        });
        let mut c = t.client(t.script("svc", "exit 3"));
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains("serves another store"), "{e}");
        c.set_store(&other);
        assert_eq!(c.call(&Request::Touch, NOW).unwrap().pid, 4242);
        drop(played);
        assert_eq!(t.runs(), 0);

        // A service started after `set_store` is given the new path.
        let t = TestDir::new();
        let mut c = t
            .client(t.script("svc", "printf '%s\\n' \"$@\" > args\nexit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        c.set_store(&other);
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains("exit status: 3"), "{e}");
        let args = std::fs::read_to_string(t.cache().join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let at = args.iter().position(|a| *a == "--store").unwrap();
        assert_eq!(args[at + 1], other.to_str().unwrap());
    }

    #[test]
    fn bad_reply_is_a_service_error_and_starts_nothing() {
        let _p = process_lock();
        let t = TestDir::new();
        let played = Played::start(&t.sock(), |_| "garbage\n".into());
        let mut c = t.client(t.script("svc", "exit 3"));
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains("unreadable"), "{e}");
        assert!(e.contains(proto::LOG_NAME), "{e}");
        assert_eq!(played.requests(), [Request::Touch]);
        assert_eq!(t.runs(), 0);
    }

    #[test]
    fn failed_refresh_is_the_build_error() {
        let _p = process_lock();
        let t = TestDir::new();
        let store = t.store();
        let _played = Played::start(&t.sock(), move |_| {
            let mut v: Value =
                serde_json::from_str(&status_line(proto::VERSION, 4242, &store)).unwrap();
            v["ok"] = json!(false);
            v["error"] = json!("export: broken");
            format!("{v}\n")
        });
        let mut c = t.client(t.script("svc", "exit 3"));
        assert_eq!(
            c.call(&Request::Refresh, NOW).unwrap_err(),
            "export: broken"
        );
    }

    #[test]
    fn refresh_that_gets_no_reply_in_time_is_a_pending_rebuild_and_starts_nothing() {
        let _p = process_lock();
        let t = TestDir::new();
        let store = t.store();
        // A service whose build outlasts the client's wait.
        let played = Played::start(&t.sock(), move |r| {
            if *r == Request::Refresh {
                thread::sleep(Duration::from_millis(400));
            }
            status_line(proto::VERSION, 4242, &store)
        });
        let mut c = t.client(t.script("svc", "exit 3"));
        c.timeouts.refresh = Duration::from_millis(100);
        let e = c.call(&Request::Refresh, NOW).unwrap_err();
        assert!(e.contains("still be running"), "{e}");
        assert!(e.contains("store_status"), "{e}");
        assert!(!e.contains("did not start"), "{e}");
        assert_eq!(t.runs(), 0);
        // Starts are not blocked, and the service is still used.
        assert_eq!(c.blocked_until, None);
        thread::sleep(Duration::from_millis(400));
        c.call(&Request::Touch, NOW).unwrap();
        assert_eq!(played.requests(), [Request::Refresh, Request::Touch]);
        assert_eq!(t.runs(), 0);
    }

    // --- back-offs ---

    #[test]
    fn no_new_start_within_30_s_of_a_failed_start() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut c = t
            .client(t.script("svc", "exit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        let started = Instant::now();
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        // At the exit, well before the start timeout.
        assert!(started.elapsed() < AT_ONCE, "{:?}", started.elapsed());
        assert!(e.contains("exit status: 3"), "{e}");
        assert!(e.contains(proto::LOG_NAME), "{e}");
        assert_eq!(t.runs(), 1);
        assert_eq!(
            c.call(&Request::Touch, NOW + proto::START_BLOCK_SECS - 1),
            Err(e)
        );
        assert_eq!(t.runs(), 1);
        assert!(c
            .call(&Request::Touch, NOW + proto::START_BLOCK_SECS)
            .is_err());
        assert_eq!(t.runs(), 2);
    }

    #[test]
    fn permission_exit_is_reported_at_once_and_blocks_starts_for_10_s() {
        let _p = process_lock();
        let t = TestDir::new();
        let program = t.script("svc", "exit 77");
        let mut c = t
            .client(program.clone())
            .with_timeouts(Duration::from_secs(5), SHORT, SHORT);
        let started = Instant::now();
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let remedy = proto::Remedy {
            program: Some(program),
            note: None,
        };
        assert_eq!(
            e,
            proto::denied_text(proto::Denied::Service, &remedy, &t.cache())
        );
        assert_eq!(t.runs(), 1);
        assert_eq!(
            c.call(&Request::Touch, NOW + proto::PERMISSION_BLOCK_SECS - 1),
            Err(e)
        );
        assert_eq!(t.runs(), 1);
        assert!(c
            .call(&Request::Touch, NOW + proto::PERMISSION_BLOCK_SECS)
            .is_err());
        assert_eq!(t.runs(), 2);
    }

    #[test]
    fn older_service_is_kept_after_a_denied_own_start() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut c = t
            .client(t.script("svc", "exit 77"))
            .with_timeouts(LONG, SHORT, SHORT);
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains(proto::DENIED), "{e}");
        assert!(c.own_denied);
        assert_eq!(t.runs(), 1);
        // An older service started by another client: used as it is, also
        // within the start block and after it.
        let store = t.store();
        let played = Played::start(&t.sock(), move |r| match r {
            Request::Shutdown { .. } => "{\"ok\":true,\"stopping\":true}\n".into(),
            _ => status_line("0.0.1", 4242, &store),
        });
        let st = c.call(&Request::Touch, NOW + 1).unwrap();
        assert_eq!((st.version.as_str(), st.pid), ("0.0.1", 4242));
        let later = NOW + proto::PERMISSION_BLOCK_SECS + 100;
        assert_eq!(c.call(&Request::Status, later).unwrap().pid, 4242);
        assert_eq!(c.call(&Request::Touch, later).unwrap().pid, 4242);
        drop(c);
        assert_eq!(
            played.requests(),
            [Request::Touch, Request::Status, Request::Touch]
        );
        assert_eq!(t.runs(), 1);
    }

    #[test]
    fn own_service_accepted_after_a_denial_replaces_older_services_again() {
        let _p = process_lock();
        let t = TestDir::new();
        // First run: denied. Second run: a service that the test plays under
        // the script's pid. Later runs: a start that fails.
        let program = t.script(
            "svc",
            "n=$(wc -l < runs)\n\
             [ \"$n\" -eq 1 ] && exit 77\n\
             [ \"$n\" -eq 2 ] && { echo $$ > pid; exec sleep 30; }\n\
             exit 3",
        );
        let mut c = t.client(program).with_timeouts(LONG, SHORT, SHORT);
        assert!(c
            .call(&Request::Touch, NOW)
            .unwrap_err()
            .contains(proto::DENIED));
        assert!(c.own_denied);

        // An older service is kept while it runs.
        let store = t.store();
        let older = Played::start(&t.sock(), move |_| status_line("0.0.1", 4242, &store));
        assert_eq!(c.call(&Request::Touch, NOW + 1).unwrap().pid, 4242);
        drop(older);
        std::fs::remove_file(t.sock()).unwrap();

        // It is gone; this reader's own start now succeeds.
        let (sock, pid_file, store) = (t.sock(), t.cache().join("pid"), t.store());
        let own = thread::spawn(move || {
            let started = Instant::now();
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                {
                    break pid;
                }
                assert!(started.elapsed() < LONG, "the own service did not start");
                thread::sleep(Duration::from_millis(5));
            };
            Played::start(&sock, move |_| status_line(proto::VERSION, pid, &store))
        });
        let later = NOW + proto::PERMISSION_BLOCK_SECS + 100;
        let st = c.call(&Request::Touch, later).unwrap();
        let own = own.join().unwrap();
        assert_eq!(st.version, proto::VERSION);
        assert!(!c.own_denied);
        assert_eq!(t.runs(), 2);
        // The own service ends; an older one appears and is replaced again.
        for child in &mut c.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(own);
        std::fs::remove_file(t.sock()).unwrap();
        let (store, gone) = (t.store(), gone_pid());
        let older = Played::start(&t.sock(), move |r| match r {
            Request::Shutdown { .. } => "{\"ok\":true,\"stopping\":true}\n".into(),
            _ => status_line("0.0.1", gone, &store),
        });
        let e = c.call(&Request::Touch, later + 100).unwrap_err();
        assert!(e.contains("exit status: 3"), "{e}");
        assert_eq!(older.requests(), [Request::Touch, shutdown("0.0.1")]);
        assert_eq!(t.runs(), 3);
    }

    #[test]
    fn lost_lock_keeps_probing_until_the_start_timeout() {
        let _p = process_lock();
        let t = TestDir::new();
        // The script leaves its pid; the start timeout passes only once the
        // client has seen it exit.
        let mut c = t
            .client(t.script("svc", "echo $$ > pid\nexit 75"))
            .with_timeouts(LONG, SHORT, SHORT);
        c.clock = after_exit_clock;
        let _gate = ExitGate::set(t.cache().join("pid"));
        let started = Instant::now();
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        let took = started.elapsed();
        assert!(took < AT_ONCE, "{took:?}");
        // Still probing after the exit: the error came from the clock
        // passing the start timeout.
        assert!(ExitGate::passed());
        assert!(e.contains("did not start"), "{e}");
        assert!(e.contains(proto::LOG_NAME), "{e}");
        // No second start inside the silent-peer delay.
        assert_eq!(t.runs(), 1);
        // The exited child is not kept.
        assert!(c.children.is_empty());
        // Blocked: answered at once with the same error.
        assert_eq!(c.call(&Request::Touch, NOW + 1), Err(e));
        assert_eq!(t.runs(), 1);
    }

    #[test]
    fn call_right_after_a_start_starts_again_once_the_silent_peer_delay_passed() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut c = t
            .client(t.script("svc", "exit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        // A service was started a second ago and is gone already.
        c.last_start_unix = Some(NOW - 1);
        let started = Instant::now();
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        let took = started.elapsed();
        assert!(e.contains("exit status: 3"), "{e}");
        assert_eq!(t.runs(), 1);
        // Not before the rest of the delay.
        assert!(took >= Duration::from_millis(900), "{took:?}");
    }

    #[test]
    fn call_while_the_own_service_ends_starts_a_new_one_when_it_is_gone() {
        let _p = process_lock();
        let t = TestDir::new();
        let mut c = t
            .client(t.script("svc", "exit 3"))
            .with_timeouts(LONG, SHORT, SHORT);
        // The service this client started long ago has closed its socket
        // and exits in a moment.
        c.last_start_unix = Some(NOW - 1000);
        c.children
            .push(Command::new("sleep").arg("0.3").spawn().unwrap());
        let e = c.call(&Request::Touch, NOW).unwrap_err();
        assert!(e.contains("exit status: 3"), "{e}");
        assert_eq!(t.runs(), 1);
        assert!(c.children.is_empty());
    }

    #[test]
    fn start_outcome_of_exit_statuses() {
        let code = |c: i32| Ok(Some(ExitStatus::from_raw(c << 8)));
        assert_eq!(start_outcome(Ok(None)), StartOutcome::Running);
        assert_eq!(start_outcome(code(75)), StartOutcome::Lost);
        assert_eq!(start_outcome(code(77)), StartOutcome::Denied);
        match start_outcome(code(3)) {
            StartOutcome::Failed(s) => assert!(s.contains('3'), "{s}"),
            other => panic!("{other:?}"),
        }
        match start_outcome(code(0)) {
            StartOutcome::Failed(s) => assert!(s.contains('0'), "{s}"),
            other => panic!("{other:?}"),
        }
        match start_outcome(Err(io::Error::from_raw_os_error(libc::ECHILD))) {
            StartOutcome::Failed(s) => assert!(s.contains("status unknown"), "{s}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exited_children_are_not_kept() {
        let _p = process_lock();
        let t = TestDir::new();
        let store = t.store();
        let _played = Played::start(&t.sock(), move |_| status_line(proto::VERSION, 1, &store));
        let mut c = t.client(t.script("svc", "exit 3"));
        // One child that exits by itself, one whose status was taken by
        // someone else (`try_wait` then fails with ECHILD).
        let exited = Command::new("true").spawn().unwrap();
        let reaped = Command::new("true").spawn().unwrap();
        let mut status = 0;
        // SAFETY: waits for our own child; std has not reaped it.
        let r = unsafe { libc::waitpid(reaped.id() as libc::pid_t, &mut status, 0) };
        assert_eq!(r, reaped.id() as libc::pid_t);
        assert!(reaped_fails(&reaped));
        c.children.push(exited);
        c.children.push(reaped);
        thread::sleep(Duration::from_millis(100));
        c.call(&Request::Touch, NOW).unwrap();
        assert!(c.children.is_empty());
    }

    /// Whether `try_wait` of `child` fails, without changing it.
    fn reaped_fails(child: &Child) -> bool {
        let mut status = 0;
        // SAFETY: WNOHANG on our own child's pid only asks.
        unsafe { libc::waitpid(child.id() as libc::pid_t, &mut status, libc::WNOHANG) == -1 }
    }

    // --- the program ---

    #[test]
    fn service_program_is_this_program() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(service_program(), Ok(exe.clone()));
        assert_eq!(program_from(Ok(exe.clone()), None), Ok(exe));
    }

    #[test]
    fn replaced_program_falls_back_to_an_existing_argv0() {
        let t = TestDir::new();
        let file = t.script("prog", "exit 0");
        let deleted = PathBuf::from(format!("{} (deleted)", file.display()));
        assert_eq!(
            program_from(Ok(deleted.clone()), Some(file.as_os_str())),
            Ok(file.clone())
        );
        // A relative argv[0] is made absolute.
        let rel = std::path::absolute("Cargo.toml").unwrap();
        assert_eq!(
            program_from(Ok(deleted.clone()), Some(OsStr::new("Cargo.toml"))),
            Ok(rel)
        );
        for argv0 in [None, Some(OsStr::new("/no/such/program"))] {
            let e = program_from(Ok(deleted.clone()), argv0).unwrap_err();
            assert!(e.contains("restart the MCP client"), "{e}");
        }
        assert!(program_from(Err(io::Error::other("x")), Some(file.as_os_str())).is_err());
    }

    // --- the log file ---

    #[test]
    fn log_is_created_private_without_following_a_link() {
        let t = TestDir::new();
        let target = t.0.join("elsewhere");
        std::fs::write(&target, b"keep").unwrap();
        let log = t.cache().join(proto::LOG_NAME);
        std::os::unix::fs::symlink(&target, &log).unwrap();
        let mut f = open_log(&t.cache()).unwrap();
        f.write_all(b"line\n").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let m = std::fs::symlink_metadata(&log).unwrap();
        assert!(m.file_type().is_file());
        assert_eq!(m.mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&log).unwrap(), b"line\n");
    }

    #[test]
    fn log_is_appended_to_below_the_limit_and_rotated_above() {
        let t = TestDir::new();
        let log = t.cache().join(proto::LOG_NAME);
        let old = t.cache().join(proto::LOG_OLD_NAME);
        open_log(&t.cache()).unwrap().write_all(b"one\n").unwrap();
        open_log(&t.cache()).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"one\ntwo\n");
        assert!(!old.exists());

        std::fs::write(&old, b"older").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&log).unwrap();
        f.set_len(proto::LOG_ROTATE_BYTES).unwrap();
        drop(f);
        // At the limit: still appended to.
        open_log(&t.cache()).unwrap().write_all(b"x").unwrap();
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            proto::LOG_ROTATE_BYTES + 1
        );
        assert_eq!(std::fs::read(&old).unwrap(), b"older");
        // Above it: rotated, replacing the older generation.
        open_log(&t.cache()).unwrap().write_all(b"new\n").unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"new\n");
        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            proto::LOG_ROTATE_BYTES + 1
        );
    }

    // --- spawning ---

    #[test]
    fn detached_child_does_not_inherit_a_listening_socket() {
        let _p = process_lock();
        let t = TestDir::new();
        let path = t.0.join("extra.sock");
        let extra = UnixListener::bind(&path).unwrap();
        // As if another thread created it and was not yet done marking it
        // close-on-exec.
        // SAFETY: fcntl() on a descriptor owned by `extra`.
        assert_eq!(
            unsafe { libc::fcntl(extra.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        let log = open_log(&t.cache()).unwrap();
        let mut child = spawn_detached(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new("echo up; exec sleep 5")],
            &t.cache(),
            log,
        )
        .unwrap();
        // `spawn` returns when the child's exec has begun; the kernel may
        // still be closing its close-on-exec descriptors. Once the child
        // runs its own code, that is over.
        let log = t.cache().join(proto::LOG_NAME);
        let started = Instant::now();
        while !std::fs::read_to_string(&log).unwrap().contains("up") {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(5));
        }
        drop(extra);
        let r = UnixStream::connect(&path);
        let _ = child.kill();
        let _ = child.wait();
        assert!(r.is_err(), "the child kept the listening socket open");
    }

    #[test]
    fn spawning_a_missing_program_fails() {
        let _p = process_lock();
        let t = TestDir::new();
        let log = open_log(&t.cache()).unwrap();
        assert!(spawn_detached(Path::new("/no/such/program"), &[], &t.cache(), log).is_err());
    }

    #[test]
    fn detached_child_runs_in_its_own_session_in_the_cache_directory() {
        let _p = process_lock();
        let t = TestDir::new();
        let log = open_log(&t.cache()).unwrap();
        let mut child = spawn_detached(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new("pwd; echo err >&2; sleep 5")],
            &t.cache(),
            log,
        )
        .unwrap();
        let pid = child.id() as libc::pid_t;
        let log = t.cache().join(proto::LOG_NAME);
        let started = Instant::now();
        while std::fs::read_to_string(&log).unwrap().lines().count() < 2 {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: getsid() on our own child.
        let sid = unsafe { libc::getsid(pid) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(sid, pid);
        let text = std::fs::read_to_string(&log).unwrap();
        let cwd = std::fs::canonicalize(t.cache()).unwrap();
        assert!(text.contains(cwd.to_str().unwrap()), "{text}");
        assert!(text.contains("err"), "{text}");
    }

    #[test]
    fn socket_path_too_long_is_refused() {
        let long = PathBuf::from(format!("/{}", "d".repeat(proto::socket_path_limit())));
        let e = Client::new(&long, Path::new("/s"), PathBuf::from("/p"), 900, 60).unwrap_err();
        assert!(e.contains("too long"), "{e}");
    }
}
