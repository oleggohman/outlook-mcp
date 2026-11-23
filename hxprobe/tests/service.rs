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

use hxprobe::mcp::proto::{self, ExchangeError, Request, LOCK_NAME, SOCKET_NAME};
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
    answering(&d);
    let st = svc.exit(Duration::from_secs(5));
    assert_eq!(st.code(), Some(0));
    assert!(!d.sock().exists());
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
    let (st, out, err) = run(&["--version"]);
    assert_eq!(st.code(), Some(0), "{err}");
    assert_eq!(out, format!("hxprobe {VERSION}\n"));
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

/// A service whose builds pause 5 s after the store copy, in the middle of
/// a build started by a touch on the changed store.
fn delayed_build(d: &Dir) -> Service {
    let svc = spawn_service(
        d,
        &["--refresh-secs", "0"],
        &[("HXPROBE_TEST_BUILD_DELAY_MS", "5000")],
    );
    answering(d);
    let v = request(d, &Request::Touch).unwrap();
    assert_eq!(v["building"], true, "{v}");
    let copy = d.cache().join(format!("store.copy-{}", svc.pid()));
    wait_until("the store copy", Duration::from_secs(5), || copy.exists());
    let v = request(d, &Request::Status).unwrap();
    assert_eq!(v["building"], true, "{v}");
    svc
}

#[test]
fn killed_during_a_build_keeps_the_database_and_the_next_start_cleans_up() {
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _s = serial();
    let d = Dir::new();
    std::fs::create_dir(d.cache()).unwrap();
    std::fs::set_permissions(d.cache(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Bound and closed: the file stays, nobody listens, nobody holds the
    // lock.
    drop(std::os::unix::net::UnixListener::bind(d.sock()).unwrap());
    let stale = std::fs::symlink_metadata(d.sock()).unwrap().ino();
    assert_eq!(
        request(&d, &Request::Status),
        Err(ExchangeError::NotRunning)
    );
    let svc = spawn_service(&d, &[], &[]);
    let v = answering(&d);
    assert_eq!(v["pid"], svc.pid());
    assert_ne!(std::fs::symlink_metadata(d.sock()).unwrap().ino(), stale);
}
