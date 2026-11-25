//! Process tests of `hxprobe service` on a header-only synthetic store.
//!
//! Every test works in its own short directory under the system temp dir,
//! passes `--store`, `--cache-dir` and `HOME` inside it, and removes it when
//! done. Before the directory is removed, every service still holding its
//! `service.lock` is killed. The tests take one mutex, so they run one
//! after another; they bind Unix sockets and start processes, which a
//! sandbox may forbid ("Operation not permitted").

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hxprobe::mcp::client::Client;
use hxprobe::mcp::proto::{self, ExchangeError, Refresher, Request, LOCK_NAME, SOCKET_NAME};
use serde_json::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest socket path the tests accept, below every platform's limit.
const MAX_SOCKET_PATH: usize = 90;
const REQ: Duration = Duration::from_secs(2);

/// Taken by every test: one test at a time.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A store with a valid header and no blocks.
fn empty_store() -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[..8].copy_from_slice(hxprobe::header::MAGIC);
    s[8] = b'i';
    s[0x38..0x40].copy_from_slice(&4096u64.to_le_bytes());
    s
}

/// A short scratch directory holding the store (`HxStore.hxd`), the cache
/// directory (`c`) and the service's stderr (`service.err`). On drop it
/// stops a service that still holds `c/service.lock`, then removes itself.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "hxs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let d = Dir(p);
        let sock = d.sock();
        assert!(
            sock.as_os_str().len() <= MAX_SOCKET_PATH,
            "the test socket path {} is longer than {MAX_SOCKET_PATH} bytes; set a shorter TMPDIR",
            sock.display()
        );
        let _ = std::fs::remove_dir_all(&d.0);
        std::fs::create_dir_all(&d.0).unwrap();
        std::fs::write(d.store(), empty_store()).unwrap();
        d
    }

    fn store(&self) -> PathBuf {
        self.0.join("HxStore.hxd")
    }

    fn cache(&self) -> PathBuf {
        self.0.join("c")
    }

    fn sock(&self) -> PathBuf {
        self.cache().join(SOCKET_NAME)
    }

    fn err(&self) -> PathBuf {
        self.0.join("service.err")
    }

    /// The service's stderr so far.
    fn log(&self) -> String {
        std::fs::read_to_string(self.err()).unwrap_or_default()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        stop_lock_holder(&self.cache());
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kill the process that holds `<cache>/service.lock`, if any, and wait
/// until it is gone. The pid is read from the file, never asked from the
/// service (a hung one would not answer); it is used only while the lock is
/// held, so a pid of a long-gone service is never signalled.
fn stop_lock_holder(cache: &std::path::Path) {
    let path = cache.join(LOCK_NAME);
    let Ok(f) = std::fs::File::open(&path) else {
        return;
    };
    // SAFETY: flock() on a descriptor owned by `f` for the whole call.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return;
    }
    let pid = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<u32>().ok());
    let Some(pid) = pid.filter(|p| *p > 1) else {
        return;
    };
    // SAFETY: plain system call on a pid read from the lock file.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let started = Instant::now();
    while !proto::pid_gone(pid) && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A started `hxprobe service`; killed and waited for on drop.
struct Service(Child);

impl Service {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Its exit status, which must come within `timeout`.
    fn exit(&mut self, timeout: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(st) = self.0.try_wait().unwrap() {
                return st;
            }
            assert!(
                started.elapsed() < timeout,
                "the service did not exit within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start `hxprobe service --store <store> --cache-dir <cache> <args>` with
/// `HOME` in `d`, `env` set and `configure` applied; stderr is appended to
/// `d.err()`.
fn spawn_service_with(
    d: &Dir,
    args: &[&str],
    env: &[(&str, &str)],
    configure: impl FnOnce(&mut Command),
) -> Service {
    let err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(d.err())
        .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.arg("service")
        .arg("--store")
        .arg(d.store())
        .arg("--cache-dir")
        .arg(d.cache())
        .args(args)
        .env_remove("HXPROBE_SERVICE")
        .env_remove("HXPROBE_TEST_BUILD_DELAY_MS")
        .env("HOME", &d.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err);
    for (k, v) in env {
        cmd.env(k, v);
    }
    configure(&mut cmd);
    Service(cmd.spawn().unwrap())
}

fn spawn_service(d: &Dir, args: &[&str], env: &[(&str, &str)]) -> Service {
    spawn_service_with(d, args, env, |_| {})
}

/// One request to the service of `d`.
fn request(d: &Dir, r: &Request) -> Result<Value, ExchangeError> {
    proto::exchange(&d.sock(), r, REQ)
}

/// Poll `cond` every 10 ms until it holds; fails the test after `timeout`.
fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let started = Instant::now();
    while !cond() {
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The status of the service of `d`, once it answers.
fn answering(d: &Dir) -> Value {
    let mut v = None;
    wait_until("the service to answer", Duration::from_secs(10), || {
        v = request(d, &Request::Status).ok();
        v.is_some()
    });
    v.unwrap()
}

/// Run the program with `args` (nothing else), within 10 s.
fn run(args: &[&str]) -> (ExitStatus, String, String) {
    run_with(args, |_| {})
}

/// Run `hxprobe service <flag> --cache-dir <cache>` with `HOME` in `d`
/// (no `--store`), within 10 s; also how long it took.
fn client(d: &Dir, flag: &str) -> (ExitStatus, String, String, Duration) {
    let cache = d.cache();
    let started = Instant::now();
    let (st, out, err) = run_with(
        &["service", flag, "--cache-dir", cache.to_str().unwrap()],
        |cmd| {
            cmd.env_remove("HXPROBE_TEST_BUILD_DELAY_MS")
                .env("HOME", &d.0);
        },
    );
    (st, out, err, started.elapsed())
}

/// [`run`], with `configure` applied to the command.
fn run_with(args: &[&str], configure: impl FnOnce(&mut Command)) -> (ExitStatus, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.args(args).env_remove("HXPROBE_SERVICE");
    configure(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = String::new();
    let mut err = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let mut s = Service(child);
    (s.exit(Duration::from_secs(10)), out, err)
}

// --- a normal life ---

#[test]
fn started_service_answers_builds_and_logs() {
    let _s = serial();
    let d = Dir::new();
    let svc = spawn_service(&d, &[], &[]);
    let v = answering(&d);
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], VERSION);
    assert_eq!(v["pid"], svc.pid());
    let store = std::fs::canonicalize(d.store()).unwrap();
    assert_eq!(v["store_path"], store.to_str().unwrap());
    assert_eq!(v["idle_secs"], proto::DEFAULT_IDLE_SECS);
    // The first build runs at start, without a touch.
    wait_until("the first build", Duration::from_secs(10), || {
        request(&d, &Request::Status).is_ok_and(|v| v["db_usable"] == true && v["builds"] == 1)
    });
    assert!(d.cache().join("mail.db").is_file());
    assert_eq!(
        std::fs::read_to_string(d.cache().join(LOCK_NAME)).unwrap(),
        format!("{} {VERSION}\n", svc.pid())
    );
    let log = d.log();
    let prefix = format!(" hxprobe service[{}]: ", svc.pid());
    let listening: Vec<&str> = log.lines().filter(|l| l.contains("listening")).collect();
    assert_eq!(listening.len(), 1, "{log}");
    // `<YYYY-MM-DDThh:mm:ssZ> hxprobe service[<pid>]: listening ...`
    let line = listening[0];
    assert_eq!(line.find(&prefix), Some(20), "{line}");
    assert!(line.as_bytes()[..20].ends_with(b"Z"), "{line}");
    assert!(log.lines().all(|l| l.contains(&prefix)), "{log}");
    assert!(log.contains("cache built in"), "{log}");
}

#[test]
fn idle_service_exits_and_removes_its_socket() {
    let _s = serial();
    let d = Dir::new();
    let mut svc = spawn_service(&d, &["--idle-secs", "1"], &[]);
    // No request is sent: one that came late would find the service gone.
    // That it served is in its log.
    let st = svc.exit(Duration::from_secs(20));
    assert_eq!(st.code(), Some(0));
    assert!(!d.sock().exists());
    assert!(d.log().contains("listening on"), "{}", d.log());
    assert!(d.log().contains("idle"), "{}", d.log());
}

#[test]
fn removing_the_cache_directory_ends_the_service() {
    let _s = serial();
    let d = Dir::new();
    let mut svc = spawn_service(&d, &[], &[]);
    answering(&d);
    // The first build writes into the directory; removing it meanwhile
    // would race with that build.
    wait_until("the first build", Duration::from_secs(10), || {
        request(&d, &Request::Status).is_ok_and(|v| v["builds"] == 1)
    });
    std::fs::remove_dir_all(d.cache()).unwrap();
    let st = svc.exit(Duration::from_secs(3));
    assert_eq!(st.code(), Some(0));
}

#[test]
fn version_flag_prints_the_version() {
    // Serial too: a child started while another test's listening socket
    // is not close-on-exec yet would inherit it.
    let _s = serial();
    let (st, out, err) = run(&["--version"]);
    assert_eq!(st.code(), Some(0), "{err}");
    assert_eq!(out, format!("hxprobe {VERSION}\n"));
}

/// The release version, as a literal: a forgotten bump fails here.
#[test]
fn version_is_0_3_0() {
    let _s = serial();
    let (st, out, err) = run(&["--version"]);
    assert_eq!(st.code(), Some(0), "{err}");
    assert_eq!(out, "hxprobe 0.3.0\n");
    let d = Dir::new();
    let _svc = spawn_service(&d, &[], &[]);
    assert_eq!(answering(&d)["version"], "0.3.0");
}

// --- exit codes ---

#[test]
fn second_service_on_the_same_directory_exits_75() {
    let _s = serial();
    let d = Dir::new();
    let first = spawn_service(&d, &[], &[]);
    answering(&d);
    let mut second = spawn_service(&d, &[], &[]);
    let st = second.exit(Duration::from_secs(2));
    assert_eq!(st.code(), Some(75));
    let v = request(&d, &Request::Status).unwrap();
    assert_eq!(v["pid"], first.pid());
    let log = d.log();
    let lost: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(&format!("[{}]", second.pid())))
        .collect();
    assert_eq!(lost.len(), 1, "{log}");
    assert!(lost[0].contains("another service holds the lock"), "{log}");
}

#[test]
fn unreadable_store_exits_77_with_one_log_line() {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root can read a file with mode 000");
        return;
    }
    let _s = serial();
    let d = Dir::new();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(d.store(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut svc = spawn_service(&d, &[], &[]);
    let st = svc.exit(Duration::from_secs(5));
    assert_eq!(st.code(), Some(77));
    let log = d.log();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("permission"), "{log}");
    assert!(!d.sock().exists());
}

#[test]
fn start_errors_exit_1() {
    let _s = serial();
    // A missing store.
    let d = Dir::new();
    std::fs::remove_file(d.store()).unwrap();
    let mut svc = spawn_service(&d, &[], &[]);
    assert_eq!(svc.exit(Duration::from_secs(5)).code(), Some(1));
    assert!(!d.cache().exists());

    // A cache directory that is a regular file.
    let d = Dir::new();
    std::fs::write(d.cache(), b"x").unwrap();
    let mut svc = spawn_service(&d, &[], &[]);
    assert_eq!(svc.exit(Duration::from_secs(5)).code(), Some(1));
    assert!(d.log().contains("not a directory"), "{}", d.log());
}

#[test]
fn usage_errors_exit_2() {
    let _s = serial();
    // `spawn_service` always passes `--store`, which `--status` and
    // `--stop` do not take.
    for args in [
        &["--bogus"][..],
        &["--idle-secs", "soon"],
        &["--idle-secs"],
        &["--status"],
        &["--stop"],
    ] {
        let d = Dir::new();
        let mut svc = spawn_service(&d, args, &[]);
        assert_eq!(svc.exit(Duration::from_secs(5)).code(), Some(2), "{args:?}");
        assert!(!d.cache().exists(), "{args:?}");
    }
}

// --- signals ---

/// Start a service on `d`, send it `sig` once it answers, and check that
/// it exits 0 and removes its socket.
fn ends_on_signal(d: &Dir, sig: libc::c_int, configure: impl FnOnce(&mut Command)) {
    let mut svc = spawn_service_with(d, &[], &[], configure);
    answering(d);
    // SAFETY: plain system call on the pid of our own child.
    assert_eq!(unsafe { libc::kill(svc.pid() as libc::pid_t, sig) }, 0);
    let st = svc.exit(Duration::from_secs(3));
    assert_eq!(st.code(), Some(0), "signal {sig}: {st:?} {:?}", st.signal());
    assert!(!d.sock().exists());
    assert!(d.log().contains("signal"), "{}", d.log());
}

#[test]
fn sigterm_and_sigint_end_the_service() {
    let _s = serial();
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let d = Dir::new();
        ends_on_signal(&d, sig, |_| {});
    }
}

#[test]
fn sigterm_ends_the_service_also_when_started_ignored() {
    let _s = serial();
    let d = Dir::new();
    ends_on_signal(&d, libc::SIGTERM, |cmd| {
        // SAFETY: signal() is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
    });
}

// --- `--status` and `--stop` ---

#[test]
fn status_prints_the_reply_as_one_line() {
    let _s = serial();
    let d = Dir::new();
    let svc = spawn_service(&d, &[], &[]);
    answering(&d);
    let (st, out, err, _) = client(&d, "--status");
    assert_eq!(st.code(), Some(0), "{err}");
    assert!(out.ends_with('\n'), "{out:?}");
    assert_eq!(out.lines().count(), 1, "{out:?}");
    let v: Value = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], VERSION);
    assert_eq!(v["pid"], svc.pid());
}

#[test]
fn status_without_a_service_exits_1() {
    let _s = serial();
    let d = Dir::new();
    let (st, out, err, _) = client(&d, "--status");
    assert_eq!(st.code(), Some(1), "{err}");
    assert_eq!(out, "");
    assert!(err.contains("not running"), "{err}");
    // A client creates nothing.
    assert!(!d.cache().exists());
}

#[test]
fn stop_ends_a_service_nobody_waited_for_at_once() {
    let _s = serial();
    let d = Dir::new();
    // Not waited for until the stop is over, as under a server that stays
    // open: the stopped service is a zombie meanwhile.
    let mut svc = spawn_service(&d, &[], &[]);
    answering(&d);
    let (st, _, err, took) = client(&d, "--stop");
    assert_eq!(st.code(), Some(0), "{err}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert_eq!(
        request(&d, &Request::Status),
        Err(ExchangeError::NotRunning)
    );
    // Left in place: only the service touches it.
    assert!(d.cache().join(LOCK_NAME).is_file());
    assert_eq!(svc.exit(Duration::from_secs(5)).code(), Some(0));
    assert!(d
        .log()
        .contains(&format!("shutdown requested by version {VERSION}")));
}

#[test]
fn stop_without_a_service_exits_0_and_creates_nothing() {
    let _s = serial();
    let d = Dir::new();
    let (st, _, err, _) = client(&d, "--stop");
    assert_eq!(st.code(), Some(0), "{err}");
    assert!(err.contains("not running"), "{err}");
    assert!(!d.cache().exists());

    // A cache directory without a service: `service.lock` is not created.
    std::fs::create_dir(d.cache()).unwrap();
    let (st, _, err, _) = client(&d, "--stop");
    assert_eq!(st.code(), Some(0), "{err}");
    assert!(!d.cache().join(LOCK_NAME).exists());
}

/// A service played by the test: holds `service.lock` (naming `pid`),
/// accepts connections and never replies.
struct SilentService {
    /// Held for the life of the played service.
    _lock: std::fs::File,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SilentService {
    fn start(d: &Dir, pid: u32) -> SilentService {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::create_dir(d.cache()).unwrap();
        std::fs::set_permissions(d.cache(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(d.cache().join(LOCK_NAME))
            .unwrap();
        // SAFETY: flock() on a descriptor owned by `lock`.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        writeln!(lock, "{pid} {VERSION}").unwrap();
        let listener = std::os::unix::net::UnixListener::bind(d.sock()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = std::sync::Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                if let Ok((s, _)) = listener.accept() {
                    held.push(s);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        SilentService {
            _lock: lock,
            done,
            thread: Some(thread),
        }
    }

    /// Whether `service.lock` is still held by this test.
    fn lock_held(&self, d: &Dir) -> bool {
        let f = std::fs::File::open(d.cache().join(LOCK_NAME)).unwrap();
        // SAFETY: flock() on a descriptor owned by `f`.
        let free = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
        !free
    }
}

impl Drop for SilentService {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A stand-in process (`sleep 60`) whose pid a played service reports;
/// killed and waited for on drop.
fn stand_in() -> Service {
    Service(
        Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn stop_of_a_service_that_does_not_answer_exits_1_naming_its_pid() {
    let _s = serial();
    let d = Dir::new();
    let pid = stand_in();
    let played = SilentService::start(&d, pid.pid());
    let (st, _, err, took) = client(&d, "--stop");
    assert_eq!(st.code(), Some(1), "{err}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    assert!(err.contains(&pid.pid().to_string()), "{err}");
    // Neither taken nor removed nor rewritten.
    assert!(played.lock_held(&d));
    assert_eq!(
        std::fs::read_to_string(d.cache().join(LOCK_NAME)).unwrap(),
        format!("{} {VERSION}\n", pid.pid())
    );
    assert!(!proto::pid_gone(pid.pid()));
}

// --- builds: kill during a build, leftovers, stale socket ---

/// `(dev, ino)` and content of `<cache>/mail.db`.
fn db_file(d: &Dir) -> ((u64, u64), Vec<u8>) {
    use std::os::unix::fs::MetadataExt;
    let p = d.cache().join("mail.db");
    let m = std::fs::symlink_metadata(&p).unwrap();
    ((m.dev(), m.ino()), std::fs::read(&p).unwrap())
}

/// Names of the temporary build files in the cache directory.
fn temps(d: &Dir) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d.cache())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("store.copy-") || n.starts_with("mail.db.tmp-"))
        .collect();
    v.sort();
    v
}

/// Build `mail.db` with a service that is stopped again, then give the
/// store another modification time, so the next touch rebuilds. The
/// database's `built_unix`.
fn old_database(d: &Dir) -> i64 {
    let mut svc = spawn_service(d, &[], &[]);
    let mut built = None;
    wait_until("the first build", Duration::from_secs(10), || {
        built = request(d, &Request::Status)
            .ok()
            .filter(|v| v["db_usable"] == true)
            .and_then(|v| v["built_unix"].as_i64());
        built.is_some()
    });
    let (st, _, err, _) = client(d, "--stop");
    assert_eq!(st.code(), Some(0), "{err}");
    assert_eq!(svc.exit(Duration::from_secs(5)).code(), Some(0));
    let t = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(d.store())
        .unwrap()
        .set_modified(t)
        .unwrap();
    built.unwrap()
}

/// Whether the program under test reads `HXPROBE_TEST_BUILD_DELAY_MS`: only
/// a debug build does (the tests and the program are built alike).
const BUILD_DELAY: bool = cfg!(debug_assertions);

/// A service whose builds pause a minute after the store copy (far longer
/// than a test takes to stop it, also on a loaded machine), in the middle
/// of a build started by a touch on the changed store.
fn delayed_build(d: &Dir) -> Service {
    let svc = spawn_service(
        d,
        &["--refresh-secs", "0"],
        &[("HXPROBE_TEST_BUILD_DELAY_MS", "60000")],
    );
    answering(d);
    let v = request(d, &Request::Touch).unwrap();
    assert_eq!(v["building"], true, "{v}");
    let copy = d.cache().join(format!("store.copy-{}", svc.pid()));
    wait_until("the store copy", Duration::from_secs(20), || copy.exists());
    let v = request(d, &Request::Status).unwrap();
    assert_eq!(v["building"], true, "{v}");
    svc
}

#[test]
fn killed_during_a_build_keeps_the_database_and_the_next_start_cleans_up() {
    if !BUILD_DELAY {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    let built = old_database(&d);
    let before = db_file(&d);
    let mut svc = delayed_build(&d);
    let pid = svc.pid();
    // SAFETY: plain system call on the pid of our own child.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    assert_eq!(
        svc.exit(Duration::from_secs(5)).signal(),
        Some(libc::SIGKILL)
    );
    assert!(db_file(&d) == before, "mail.db was replaced or changed");
    assert_eq!(temps(&d), [format!("store.copy-{pid}")]);

    let next = spawn_service(&d, &[], &[]);
    let v = answering(&d);
    assert_eq!(v["pid"], next.pid());
    assert!(temps(&d).is_empty(), "{:?}", temps(&d));
    assert_eq!(v["db_usable"], true, "{v}");
    assert_eq!(v["built_unix"], built, "{v}");
    assert_eq!(v["builds"], 0, "{v}");
    assert!(db_file(&d) == before, "mail.db was replaced or changed");
}

#[test]
fn shutdown_or_sigterm_during_a_build_exits_at_once_and_cleans_up() {
    if !BUILD_DELAY {
        return;
    }
    let _s = serial();
    for by_signal in [false, true] {
        let d = Dir::new();
        old_database(&d);
        let before = db_file(&d);
        let mut svc = delayed_build(&d);
        let started = Instant::now();
        if by_signal {
            // SAFETY: plain system call on the pid of our own child.
            assert_eq!(
                unsafe { libc::kill(svc.pid() as libc::pid_t, libc::SIGTERM) },
                0
            );
        } else {
            let shutdown = Request::Shutdown {
                version: VERSION.into(),
            };
            let v = request(&d, &shutdown).unwrap();
            assert_eq!(v["stopping"], true, "{v}");
        }
        let st = svc.exit(Duration::from_secs(2));
        assert_eq!(st.code(), Some(0), "signal {by_signal}: {st:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(temps(&d).is_empty(), "{:?}", temps(&d));
        assert!(db_file(&d) == before, "mail.db was replaced or changed");
        assert!(!d.sock().exists());
    }
}

#[test]
fn stale_socket_does_not_prevent_a_start() {
    use std::os::unix::fs::PermissionsExt;
    let _s = serial();
    let d = Dir::new();
    std::fs::create_dir(d.cache()).unwrap();
    std::fs::set_permissions(d.cache(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Bound and closed: the file stays, nobody listens, nobody holds the
    // lock.
    drop(std::os::unix::net::UnixListener::bind(d.sock()).unwrap());
    assert_eq!(
        request(&d, &Request::Status),
        Err(ExchangeError::NotRunning)
    );
    let svc = spawn_service(&d, &[], &[]);
    let v = answering(&d);
    // The new service answers on the path (a new socket file may get the
    // inode number of the removed one).
    assert_eq!(v["pid"], svc.pid());
    let listening = d
        .log()
        .lines()
        .filter(|l| l.contains("listening on"))
        .count();
    assert_eq!(listening, 1, "{}", d.log());
}

// --- the client: start and upgrade ---

/// The current time, in unix seconds.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A [`Client`] for `d` that starts this crate's program.
fn new_client(d: &Dir) -> Client {
    new_client_with(d, 900)
}

/// [`new_client`] with the service's `--refresh-secs`.
fn new_client_with(d: &Dir, refresh_secs: u64) -> Client {
    Client::new(
        &d.cache(),
        &d.store(),
        PathBuf::from(env!("CARGO_BIN_EXE_hxprobe")),
        refresh_secs,
        30,
    )
    .unwrap()
}

/// `<cache>/service.log` so far: the log of services a client started.
fn service_log(d: &Dir) -> String {
    std::fs::read_to_string(d.cache().join(proto::LOG_NAME)).unwrap_or_default()
}

/// Lines of `<cache>/service.log` that contain `what`.
fn log_lines(d: &Dir, what: &str) -> usize {
    service_log(d).lines().filter(|l| l.contains(what)).count()
}

const LOST: &str = "another service holds the lock";

#[test]
fn client_starts_a_detached_service() {
    let _s = serial();
    let d = Dir::new();
    let mut c = new_client(&d);
    let st = c.touch(unix_now()).unwrap();
    assert_eq!(st.version, VERSION);
    let lock = std::fs::read_to_string(d.cache().join(LOCK_NAME)).unwrap();
    assert_eq!(lock, format!("{} {VERSION}\n", st.pid));
    drop(c);
    // Lives on after the client is gone.
    let v = request(&d, &Request::Status).unwrap();
    assert_eq!(v["pid"], st.pid);
    // SAFETY: getsid() has no preconditions.
    assert_eq!(
        unsafe { libc::getsid(st.pid as libc::pid_t) },
        st.pid as libc::pid_t
    );
    // Its stderr is the log.
    assert_eq!(log_lines(&d, "listening on"), 1, "{}", service_log(&d));
    assert_eq!(
        log_lines(&d, &format!("[{}]", st.pid)),
        service_log(&d).lines().count()
    );
}

#[test]
fn two_clients_starting_at_once_end_with_one_service() {
    let _s = serial();
    let d = Dir::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let pids: Vec<u32> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..2)
            .map(|_| {
                let b = std::sync::Arc::clone(&barrier);
                let mut c = new_client(&d);
                s.spawn(move || {
                    b.wait();
                    c.touch(unix_now()).unwrap().pid
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(pids[0], pids[1]);
    assert_eq!(log_lines(&d, "listening on"), 1, "{}", service_log(&d));
    // A loser, if one started, must have given up before the test ends:
    // it would otherwise take the lock once the winner is killed.
    let losers: Vec<u32> = own_children()
        .into_iter()
        .filter(|p| *p != pids[0] && !proto::pid_gone(*p))
        .collect();
    for p in &losers {
        wait_until("a losing service to exit", Duration::from_secs(5), || {
            proto::pid_gone(*p)
        });
    }
    assert!(log_lines(&d, LOST) >= losers.len(), "{}", service_log(&d));
}

/// Pids of the child processes of this test process.
fn own_children() -> Vec<u32> {
    let out = Command::new("pgrep")
        .arg("-P")
        .arg(std::process::id().to_string())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[test]
fn client_gives_up_on_a_service_that_never_answers() {
    let _s = serial();
    let d = Dir::new();
    let pid = stand_in();
    let played = SilentService::start(&d, pid.pid());
    let mut c = new_client(&d).with_timeouts(
        Duration::from_millis(300),
        Duration::from_millis(300),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let e = c.touch(unix_now()).unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(e.contains(proto::LOG_NAME), "{e}");
    // The service it started lost against the lock holder, which kept it.
    wait_until(
        "the started service to give up",
        Duration::from_secs(5),
        || log_lines(&d, LOST) == 1,
    );
    assert!(played.lock_held(&d));
    assert_eq!(
        std::fs::read_to_string(d.cache().join(LOCK_NAME)).unwrap(),
        format!("{} {VERSION}\n", pid.pid())
    );
}

/// What a played old service does after it accepted a `shutdown`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnShutdown {
    /// As a real service: the socket goes at once, the lock 300 ms later,
    /// and only then the process (the stand-in, killed and not waited for).
    Exit,
    /// The socket and the lock go at once; the stand-in never exits.
    Hang,
}

/// A service played by the test: holds `service.lock` naming the pid of a
/// stand-in, answers `status` and `touch` with `version` and records the
/// requests.
struct Played {
    requests: std::sync::Arc<Mutex<Vec<Request>>>,
    /// After an `Exit`: whether `service.log` was still absent when the
    /// stand-in was killed (no service was started before it was gone).
    log_absent_at_kill: std::sync::Arc<Mutex<Option<bool>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pid: u32,
}

impl Played {
    fn start(d: &Dir, version: &'static str, on_shutdown: OnShutdown) -> Played {
        use std::io::{BufRead, Write};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use std::sync::Arc;
        let _ = std::fs::create_dir(d.cache());
        std::fs::set_permissions(d.cache(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut stand_in = stand_in();
        let pid = stand_in.pid();
        let mut lock = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(d.cache().join(LOCK_NAME))
                .unwrap(),
        );
        let f = lock.as_mut().unwrap();
        // SAFETY: flock() on a descriptor owned by `f`.
        assert_eq!(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        f.set_len(0).unwrap();
        writeln!(f, "{pid} {version}").unwrap();
        let sock = d.sock();
        let log = d.cache().join(proto::LOG_NAME);
        let store = std::fs::canonicalize(d.store()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log_absent_at_kill = Arc::new(Mutex::new(None));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, absent, stop) = (
            Arc::clone(&requests),
            Arc::clone(&log_absent_at_kill),
            Arc::clone(&done),
        );
        let thread = std::thread::spawn(move || {
            let mut listener = Some(listener);
            while !stop.load(Ordering::SeqCst) {
                let Some(Ok((s, _))) = listener.as_ref().map(|l| l.accept()) else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut line = String::new();
                if std::io::BufReader::new(&s).read_line(&mut line).is_err() {
                    continue;
                }
                let Ok(req) = Request::parse(&line) else {
                    continue;
                };
                seen.lock().unwrap().push(req.clone());
                let stopping = req
                    == Request::Shutdown {
                        version: version.into(),
                    };
                let reply = if stopping {
                    serde_json::json!({"ok": true, "stopping": true})
                } else {
                    serde_json::json!({"ok": true, "version": version, "pid": pid,
                        "store_path": store.to_str().unwrap(), "building": false,
                        "db_usable": true, "builds": 0})
                };
                let _ = (&s).write_all(format!("{reply}\n").as_bytes());
                drop(s);
                if !stopping {
                    continue;
                }
                // The socket goes first, as `cmd_service` does.
                listener = None;
                let _ = std::fs::remove_file(&sock);
                if on_shutdown == OnShutdown::Exit {
                    std::thread::sleep(Duration::from_millis(300));
                    lock = None;
                    *absent.lock().unwrap() = Some(!log.exists());
                    let _ = stand_in.0.kill();
                } else {
                    lock = None;
                }
            }
            drop(lock);
            drop(stand_in);
        });
        Played {
            requests,
            log_absent_at_kill,
            done,
            thread: Some(thread),
            pid,
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

fn old_shutdown() -> Request {
    Request::Shutdown {
        version: "0.0.1".into(),
    }
}

#[test]
fn client_replaces_an_older_service_once_it_is_gone() {
    let _s = serial();
    let d = Dir::new();
    // The store changed after the database was built: the service rebuilds
    // it only after a touch.
    old_database(&d);
    let played = Played::start(&d, "0.0.1", OnShutdown::Exit);
    let mut c = new_client_with(&d, 0);
    let started = Instant::now();
    let st = c.touch(unix_now()).unwrap();
    let took = started.elapsed();
    assert_eq!(st.version, VERSION);
    assert_ne!(st.pid, played.pid);
    // Not the wait for a service that does not go away (5 s); the bound
    // leaves room for a loaded machine.
    assert!(took < Duration::from_secs(4), "took {took:?}");
    assert_eq!(played.requests(), [Request::Touch, old_shutdown()]);
    assert_eq!(*played.log_absent_at_kill.lock().unwrap(), Some(true));
    assert!(proto::pid_gone(played.pid));
    assert_eq!(log_lines(&d, "listening on"), 1, "{}", service_log(&d));
    assert_eq!(log_lines(&d, LOST), 0, "{}", service_log(&d));
    // The new service got the touch: it rebuilds the changed store. (Its
    // `built_unix` may equal the old one: both are whole seconds.)
    wait_until("the rebuild", Duration::from_secs(10), || {
        request(&d, &Request::Status).is_ok_and(|v| v["builds"] == 1 && v["building"] == false)
    });
    assert!(
        service_log(&d).contains("build started (changed)"),
        "{}",
        service_log(&d)
    );
}

#[test]
fn client_starts_after_the_limit_when_an_old_service_left_its_socket_and_lock() {
    let _s = serial();
    let d = Dir::new();
    let played = Played::start(&d, "0.0.1", OnShutdown::Hang);
    let wait = Duration::from_millis(300);
    let mut c = new_client_with(&d, 900).with_timeouts(proto::START_TIMEOUT, REQ, wait);
    let started = Instant::now();
    let st = c.touch(unix_now()).unwrap();
    assert!(started.elapsed() >= wait, "{:?}", started.elapsed());
    assert_eq!(st.version, VERSION);
    assert_eq!(played.requests(), [Request::Touch, old_shutdown()]);
    assert!(!proto::pid_gone(played.pid));
    assert_eq!(log_lines(&d, "listening on"), 1, "{}", service_log(&d));
    assert_eq!(log_lines(&d, LOST), 0, "{}", service_log(&d));
}

#[test]
fn client_uses_a_newer_service_and_never_stops_it() {
    let _s = serial();
    let d = Dir::new();
    let played = Played::start(&d, "99.0.0", OnShutdown::Exit);
    let mut c = new_client(&d);
    let st = c.touch(unix_now()).unwrap();
    assert_eq!(st.version, "99.0.0");
    assert_eq!(st.pid, played.pid);
    assert_eq!(played.requests(), [Request::Touch]);
    // Nothing was started.
    assert!(!d.cache().join(proto::LOG_NAME).exists());
}

// --- `hxprobe mcp` in service mode ---

/// A running `hxprobe mcp`: requests go to its stdin, responses are read
/// from its stdout by a thread; its stderr is appended to a file in the
/// test directory. Killed and waited for on drop.
struct Mcp {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
    err: PathBuf,
    next_id: i64,
}

/// How long a response of `hxprobe mcp` may take.
const RESPONSE: Duration = Duration::from_secs(60);

/// Start `hxprobe mcp --store <store> --cache-dir <cache> <args>` in service
/// mode (`HXPROBE_SERVICE` removed) with `HOME` in `d` and `configure`
/// applied.
fn spawn_mcp_with(
    d: &Dir,
    store: &std::path::Path,
    cache: &std::path::Path,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Mcp {
    spawn_mcp_on(d, Some(store), cache, args, configure)
}

/// [`spawn_mcp_with`]; without a `store` there is no `--store`, and the
/// server looks up the default store under `HOME`, which is `d`.
fn spawn_mcp_on(
    d: &Dir,
    store: Option<&std::path::Path>,
    cache: &std::path::Path,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Mcp {
    use std::io::BufRead;
    static N: AtomicUsize = AtomicUsize::new(0);
    let err =
        d.0.join(format!("mcp-{}.err", N.fetch_add(1, Ordering::Relaxed)));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.arg("mcp");
    if let Some(store) = store {
        cmd.arg("--store").arg(store);
    }
    cmd.arg("--cache-dir")
        .arg(cache)
        .args(args)
        .env_remove("HXPROBE_SERVICE")
        .env_remove("HXPROBE_TEST_BUILD_DELAY_MS")
        .env("HOME", &d.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&err).unwrap());
    configure(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
        // End of file: the sender is dropped, the receiver sees it.
    });
    Mcp {
        child,
        stdin,
        lines,
        err,
        next_id: 1,
    }
}

/// [`spawn_mcp_with`] on the store and cache directory of `d`.
fn spawn_mcp(d: &Dir) -> Mcp {
    spawn_mcp_with(d, &d.store(), &d.cache(), &[], |_| {})
}

impl Mcp {
    /// Write a `tools/call` of `name` without waiting for the response.
    fn send(&mut self, name: &str) {
        use std::io::Write;
        let req = serde_json::json!({"jsonrpc": "2.0", "id": self.next_id,
            "method": "tools/call", "params": {"name": name, "arguments": {}}});
        self.next_id += 1;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next response.
    fn response(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(RESPONSE)
            .unwrap_or_else(|e| panic!("no response from hxprobe mcp: {e}\n{}", self.log()));
        serde_json::from_str(&line).unwrap()
    }

    /// The result of the tool `name`, which must succeed.
    fn tool(&mut self, name: &str) -> Value {
        self.send(name);
        let r = self.response();
        assert_eq!(r["result"]["isError"], false, "{r}\n{}", self.log());
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// The error text of the tool `name`, which must fail.
    fn tool_error(&mut self, name: &str) -> String {
        self.send(name);
        let r = self.response();
        assert_eq!(r["result"]["isError"], true, "{r}");
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The server's stderr so far.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }

    /// Close stdin; stdout must reach end of file and the server exit,
    /// both within [`RESPONSE`].
    fn close(&mut self) -> ExitStatus {
        drop(self.stdin.take());
        match self.lines.recv_timeout(RESPONSE) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("stdout did not reach end of file: {other:?}"),
        }
        let started = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(started.elapsed() < RESPONSE, "hxprobe mcp did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait until every service that lost the lock against the one that
/// answers has said so in `service.log`: each start of a server (the
/// "starting" lines in `logs`) beyond the one that listens.
fn wait_for_losers(d: &Dir, logs: &[String]) {
    let starts: usize = logs
        .iter()
        .map(|l| l.matches("starting the refresh service").count())
        .sum();
    let listening = log_lines(d, "listening on");
    wait_until("losing services to exit", Duration::from_secs(5), || {
        log_lines(d, LOST) + listening >= starts
    });
}

#[test]
fn mcp_session_in_service_mode() {
    let _s = serial();
    let d = Dir::new();
    let mut m = spawn_mcp(&d);
    // The first call ever waits for the first build.
    let st = m.tool("store_status");
    assert!(st["built"].is_string(), "{st}");
    assert_eq!(st["last_error"], Value::Null, "{st}");
    assert_eq!(st["messages"], 0);
    assert_eq!(st["service"]["running"], true, "{st}");
    assert_eq!(st["service"]["version"], VERSION);
    assert_eq!(st["service"]["last_error"], Value::Null);
    let pid = st["service"]["pid"].as_u64().unwrap();
    let list = m.tool("list_messages");
    assert_eq!(list["total"], 0);
    assert!(list.get("warning").is_none(), "{list}");
    assert_eq!(m.tool("list_events")["total"], 0);
    let log = m.log();
    assert!(
        log.contains(&format!(
            "starting the refresh service: {}",
            env!("CARGO_BIN_EXE_hxprobe")
        )),
        "{log}"
    );
    // Stdout ends with the server although the service lives on.
    assert_eq!(m.close().code(), Some(0), "{}", m.log());
    let v = request(&d, &Request::Status).unwrap();
    assert_eq!(v["pid"], pid);
    assert_eq!(v["builds"], 1, "{v}");
}

#[test]
fn two_servers_started_at_once_share_one_service() {
    let _s = serial();
    let d = Dir::new();
    let mut a = spawn_mcp(&d);
    let mut b = spawn_mcp(&d);
    a.send("store_status");
    b.send("store_status");
    let (ra, rb) = (a.response(), b.response());
    let pid = |r: &Value| -> Value {
        let v: Value =
            serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["service"]["running"], true, "{v}");
        v["service"]["pid"].clone()
    };
    assert_eq!(pid(&ra), pid(&rb));
    assert_eq!(log_lines(&d, "listening on"), 1, "{}", service_log(&d));
    wait_for_losers(&d, &[a.log(), b.log()]);
}

#[test]
fn refresh_gives_a_later_build_that_an_open_server_reopens() {
    let _s = serial();
    let d = Dir::new();
    let mut a = spawn_mcp(&d);
    let first = a.tool("store_status")["built"].clone();
    assert!(first.is_string(), "{first}");
    // `built` has whole seconds.
    std::thread::sleep(Duration::from_millis(1100));
    let mut b = spawn_mcp(&d);
    let refreshed = b.tool("refresh");
    let later = refreshed["built"].clone();
    assert!(
        later.as_str().unwrap() > first.as_str().unwrap(),
        "{first} {later}"
    );
    assert_eq!(refreshed["service"]["running"], true);
    // The server kept open serves the new file on its next call.
    assert_eq!(a.tool("store_status")["built"], later);
    assert!(a.log().contains("reopened mail.db"), "{}", a.log());
    let v = request(&d, &Request::Status).unwrap();
    assert_eq!(v["builds"], 2, "{v}");
}

/// Whether the tests run as root, for whom mode 000 does not deny reading.
fn is_root() -> bool {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Set the mode of the store of `d`.
fn store_mode(d: &Dir, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(d.store(), std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn unreadable_store_with_a_database_is_served_and_names_the_permission() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    let built = old_database(&d);
    store_mode(&d, 0o000);
    let mut m = spawn_mcp(&d);
    let st = m.tool("store_status");
    let list = m.tool("list_messages");
    store_mode(&d, 0o600);
    assert_eq!(st["built"], hxprobe::mcp::text::iso(built), "{st}");
    let e = st["last_error"].as_str().unwrap();
    assert!(e.contains("permission denied"), "{e}");
    assert_remedy(&d, e);
    assert_eq!(st["service"]["running"], false, "{st}");
    assert_eq!(list["total"], 0);
    // The older data comes with a warning that says why.
    let w = list["warning"].as_str().unwrap_or_else(|| panic!("{list}"));
    assert!(w.starts_with("mail data is from "), "{w}");
    assert!(
        w.contains("cannot read the store: permission denied"),
        "{w}"
    );
}

/// `e` names the steps under Full Disk Access with the test program and
/// the stop command with the test's cache directory.
fn assert_remedy(d: &Dir, e: &str) {
    let program = std::fs::canonicalize(env!("CARGO_BIN_EXE_hxprobe")).unwrap();
    assert!(e.contains("Full Disk Access"), "{e}");
    assert!(e.contains(&format!("\"{}\"", program.display())), "{e}");
    assert!(
        e.contains(&format!("--cache-dir '{}'", d.cache().display())),
        "{e}"
    );
}

#[test]
fn unreadable_store_without_a_database_is_a_tool_error() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    store_mode(&d, 0o000);
    let mut m = spawn_mcp(&d);
    let started = Instant::now();
    let e = m.tool_error("list_messages");
    store_mode(&d, 0o600);
    assert!(e.contains("permission denied"), "{e}");
    assert_remedy(&d, &e);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn server_denied_the_store_keeps_an_older_service_from_the_first_contact() {
    if is_root() {
        return;
    }
    /// Gives the store mode 0600 back on drop, also when the test fails.
    struct Readable<'a>(&'a Dir);
    impl Drop for Readable<'_> {
        fn drop(&mut self) {
            store_mode(self.0, 0o600);
        }
    }
    let _s = serial();
    let d = Dir::new();
    old_database(&d);
    let played = Played::start(&d, "0.0.1", OnShutdown::Exit);
    store_mode(&d, 0o000);
    let _readable = Readable(&d);
    let mut m = spawn_mcp(&d);
    // This program cannot read the store, so its own service could not
    // either: the working older service is not stopped, not even once.
    let list = m.tool("list_messages");
    assert!(list.get("warning").is_none(), "{list}\n{}", m.log());
    assert_eq!(played.requests(), [Request::Touch]);
    m.tool("list_messages");
    let st = m.tool("store_status");
    assert_eq!(st["service"]["running"], true, "{st}");
    assert_eq!(st["service"]["version"], "0.0.1", "{st}");
    assert_eq!(st["service"]["pid"], played.pid, "{st}");
    assert_eq!(st["last_error"], Value::Null, "{st}");
    assert!(
        !played
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Shutdown { .. })),
        "{:?}",
        played.requests()
    );
    assert!(
        m.log().contains("keeping refresh service 0.0.1"),
        "{}",
        m.log()
    );
    // No service of this program was started.
    assert_eq!(service_log(&d), "");
}

/// Sets the mode of a directory and gives it mode 0700 back on drop, also
/// when the test fails, so the temp dir can be removed.
struct Locked(PathBuf);

impl Locked {
    fn new(dir: &std::path::Path, mode: u32) -> Locked {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        Locked(dir.to_path_buf())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A store in the directory `s` of `d`, which the returned guard makes
/// mode 000 (nothing in it can be examined).
fn store_in_a_locked_directory(d: &Dir) -> (PathBuf, Locked) {
    let s = d.0.join("s");
    std::fs::create_dir(&s).unwrap();
    std::fs::write(s.join("HxStore.hxd"), empty_store()).unwrap();
    let lock = Locked::new(&s, 0o000);
    (s.join("HxStore.hxd"), lock)
}

#[test]
fn store_that_cannot_be_examined_makes_the_service_exit_77() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    let (store, _lock) = store_in_a_locked_directory(&d);
    let cache = d.cache();
    let (st, _, err) = run_with(
        &[
            "service",
            "--store",
            store.to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
        ],
        |cmd| {
            cmd.env("HOME", &d.0);
        },
    );
    assert_eq!(st.code(), Some(77), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.contains("permission denied"), "{err}");
    assert!(!d.sock().exists());
}

#[test]
fn server_serves_a_store_that_cannot_be_examined_and_names_the_remedy() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    let (store, _lock) = store_in_a_locked_directory(&d);
    let mut m = spawn_mcp_with(&d, &store, &d.cache(), &[], |_| {});
    let e = m.tool_error("list_messages");
    assert!(
        e.starts_with("the refresh service cannot read the store: permission denied"),
        "{e}\n{}",
        m.log()
    );
    assert_remedy(&d, &e);
}

#[test]
fn server_whose_default_lookup_is_denied_names_the_remedy_and_starts_no_service() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    // The only profile is not the one the lookup guesses, and the profiles
    // directory cannot be listed: the guess names no file.
    let profiles =
        d.0.join("Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles");
    let store = profiles.join("Work").join("HxStore.hxd");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::write(&store, empty_store()).unwrap();
    let lock = Locked::new(&profiles, 0o300);
    let mut m = spawn_mcp_on(&d, None, &d.cache(), &[], |_| {});
    let e = m.tool_error("list_messages");
    assert!(
        e.starts_with("the refresh service cannot read the store: permission denied"),
        "{e}\n{}",
        m.log()
    );
    assert_remedy(&d, &e);
    let st = m.tool("store_status");
    assert_eq!(st["last_error"], e.as_str(), "{st}");
    assert_eq!(st["service"]["running"], false, "{st}");
    // No service was started on the guess.
    assert_eq!(service_log(&d), "");
    assert!(!d.sock().exists());
    assert!(
        !m.log().contains("starting the refresh service"),
        "{}",
        m.log()
    );
    // After the grant the next call is answered: no start was blocked.
    drop(lock);
    let list = m.tool("list_messages");
    assert_eq!(list["total"], 0, "{list}");
    assert!(list.get("warning").is_none(), "{list}");
    let st = m.tool("store_status");
    let canonical = std::fs::canonicalize(&store).unwrap();
    assert_eq!(st["store_path"], canonical.to_str().unwrap(), "{st}");
    assert_eq!(st["service"]["running"], true, "{st}");
    assert_eq!(st["last_error"], Value::Null, "{st}");
}

#[test]
fn server_started_without_access_recovers_after_the_grant_without_a_restart() {
    if is_root() {
        return;
    }
    let _s = serial();
    let d = Dir::new();
    let (store, lock) = store_in_a_locked_directory(&d);
    // The store is named through a symbolic link to its directory, so the
    // path given differs from the one the service reports.
    std::os::unix::fs::symlink(d.0.join("s"), d.0.join("l")).unwrap();
    let linked = d.0.join("l").join("HxStore.hxd");
    let mut m = spawn_mcp_with(&d, &linked, &d.cache(), &[], |_| {});
    let e = m.tool_error("list_messages");
    assert!(
        e.contains("cannot read the store: permission denied"),
        "{e}"
    );
    drop(lock);
    // The start block of the server lasts 10 s on its own clock.
    let started = Instant::now();
    let result = loop {
        m.send("list_messages");
        let r = m.response();
        if r["result"]["isError"] == false {
            break r;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{r}\n{}",
            m.log()
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let v: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["total"], 0, "{v}");
    assert!(v.get("warning").is_none(), "{v}");
    let st = m.tool("store_status");
    let canonical = std::fs::canonicalize(&store).unwrap();
    assert_eq!(st["store_path"], canonical.to_str().unwrap(), "{st}");
    assert_eq!(st["service"]["running"], true, "{st}");
    assert_eq!(st["last_error"], Value::Null, "{st}");
}

#[test]
fn too_long_socket_path_means_fallback_mode() {
    let _s = serial();
    let d = Dir::new();
    let cache = d.0.join("x".repeat(120));
    let mut m = spawn_mcp_with(&d, &d.store(), &cache, &[], |_| {});
    let st = m.tool("store_status");
    assert_eq!(st["service"], Value::Null, "{st}");
    assert!(st["built"].is_string(), "{st}");
    assert!(!cache.join(SOCKET_NAME).exists());
    assert!(!cache.join(LOCK_NAME).exists());
    assert_eq!(m.close().code(), Some(0));
    let log = m.log();
    assert_eq!(
        log.lines().filter(|l| l.contains("fallback")).count(),
        1,
        "{log}"
    );
    assert!(log.contains("too long"), "{log}");
}

#[test]
fn second_store_on_the_same_cache_directory_is_refused() {
    let _s = serial();
    let d = Dir::new();
    let mut a = spawn_mcp(&d);
    let built = a.tool("store_status")["built"].clone();
    let db = db_file(&d);
    let other = d.0.join("Other.hxd");
    std::fs::write(&other, empty_store()).unwrap();
    let mut b = spawn_mcp_with(&d, &other, &d.cache(), &[], |_| {});
    let e = b.tool_error("list_messages");
    let served = std::fs::canonicalize(d.store()).unwrap();
    assert!(
        e.contains(&format!("serves another store: {}", served.display())),
        "{e}"
    );
    assert!(e.contains("--cache-dir"), "{e}");
    // Nothing was rebuilt for the second store.
    let v = request(&d, &Request::Status).unwrap();
    assert_eq!(v["builds"], 1, "{v}");
    assert!(db_file(&d) == db, "mail.db was replaced");
    assert_eq!(a.tool("store_status")["built"], built);
}

#[test]
fn server_started_with_sigchld_ignored_still_starts_a_service() {
    let _s = serial();
    let d = Dir::new();
    let mut m = spawn_mcp_with(&d, &d.store(), &d.cache(), &[], |cmd| {
        // SAFETY: signal() is async-signal-safe; nothing else runs here.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                Ok(())
            });
        }
    });
    let st = m.tool("store_status");
    assert_eq!(st["service"]["running"], true, "{st}\n{}", m.log());
    assert_eq!(st["last_error"], Value::Null, "{st}");
    assert!(st["built"].is_string(), "{st}");
}

#[test]
fn stop_with_a_server_kept_open_and_the_next_call_starts_a_new_service() {
    let _s = serial();
    let d = Dir::new();
    let mut m = spawn_mcp(&d);
    let pid = m.tool("store_status")["service"]["pid"].clone();
    // The service is an unreaped child of the open server: `--stop` must not
    // wait for the 5 s limit.
    let (st, _, err, took) = client(&d, "--stop");
    assert_eq!(st.code(), Some(0), "{err}");
    assert!(took < Duration::from_secs(2), "took {took:?}");
    assert_eq!(
        request(&d, &Request::Status),
        Err(ExchangeError::NotRunning)
    );
    // Inside the 2 s in which a server starts no second service: the call
    // waits for the rest of them, then starts one.
    let next = m.tool("store_status");
    assert_eq!(next["service"]["running"], true, "{next}\n{}", m.log());
    assert_ne!(next["service"]["pid"], pid);
    assert_eq!(next["last_error"], Value::Null, "{next}");
}
