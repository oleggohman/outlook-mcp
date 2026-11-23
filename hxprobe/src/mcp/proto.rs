//! The protocol between `hxprobe mcp` and `hxprobe service`, and helpers
//! both sides share.
//!
//! One request per connection on the Unix socket `<cache>/service.sock`:
//! the client writes one JSON object on one line (at most
//! [`MAX_REQUEST_BYTES`]), reads one line, closes. Unknown keys are ignored
//! on both sides; replies only ever gain keys. The `status` and `shutdown`
//! requests and the keys `ok`, `version` and `pid` of the status reply never
//! change, so a newer client can always identify and stop an older service.
//!
//! Requests: `{"op":"status"}`, `{"op":"touch"}`, `{"op":"refresh"}`,
//! `{"op":"shutdown","version":"<the service's version>"}`. Nothing but
//! these requests and the status (counts, times, paths, versions) goes
//! over the socket.

use std::cmp::Ordering;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

/// Version of this program, as the service reports it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// File names in the cache directory.
pub const SOCKET_NAME: &str = "service.sock";
pub const LOCK_NAME: &str = "service.lock";
pub const LOG_NAME: &str = "service.log";
pub const LOG_OLD_NAME: &str = "service.log.1";

/// Longest request line, in bytes, without its newline.
pub const MAX_REQUEST_BYTES: usize = 4096;
/// Longest reply line a client reads, in bytes.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Default time without requests after which the service exits, in
/// seconds; 0 never exits.
pub const DEFAULT_IDLE_SECS: u64 = 1800;
/// Period of the service's policy loop.
pub const TICK: Duration = Duration::from_secs(1);
/// Client connect, read and write timeout for `status`, `touch` and
/// `shutdown`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Client read timeout for `refresh`, which waits for a build.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(300);
/// Service read and write timeout per connection.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections the service serves at once; more are closed at once.
pub const MAX_CONNECTIONS: usize = 64;
/// How long a client waits for a service it started to answer.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Probe period right after a start, for [`FAST_PROBES`] probes.
pub const FAST_PROBE: Duration = Duration::from_millis(20);
pub const FAST_PROBES: u32 = 10;
/// Probe period after the fast probes.
pub const SLOW_PROBE: Duration = Duration::from_millis(50);
/// A service that won the lock but stays silent this long is started again.
pub const SILENT_PEER: Duration = Duration::from_secs(2);
/// Wait for an old service to be gone (upgrade, `--stop`), and its period.
pub const GONE_WAIT: Duration = Duration::from_secs(5);
pub const GONE_POLL: Duration = Duration::from_millis(20);
/// Wait of a tool call for the first build ever, and its period.
pub const FIRST_BUILD_WAIT: Duration = Duration::from_secs(50);
pub const FIRST_BUILD_POLL: Duration = Duration::from_millis(250);
/// No new start this long after a failed start, in seconds.
pub const START_BLOCK_SECS: i64 = 30;
/// No new start this long after a service exited for a permission error.
pub const PERMISSION_BLOCK_SECS: i64 = 10;
/// Shortest time between two automatic builds of an unusable database.
pub const UNUSABLE_REBUILD_SECS: i64 = 60;
/// `service.log` larger than this is rotated before a service starts.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// A request to the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Status,
    Touch,
    Refresh,
    /// Obeyed only when `version` is the service's own.
    Shutdown {
        version: String,
    },
}

impl Request {
    /// Parse one request line (a trailing newline is allowed).
    pub fn parse(line: &str) -> Result<Request, String> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line.len() > MAX_REQUEST_BYTES {
            return Err("request too long".into());
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
        let obj = v.as_object().ok_or("not a JSON object")?;
        match obj.get("op").and_then(Value::as_str) {
            Some("status") => Ok(Request::Status),
            Some("touch") => Ok(Request::Touch),
            Some("refresh") => Ok(Request::Refresh),
            Some("shutdown") => match obj.get("version").and_then(Value::as_str) {
                Some(v) => Ok(Request::Shutdown { version: v.into() }),
                None => Err("shutdown without a version".into()),
            },
            Some(op) => Err(format!("unknown op {op:?}")),
            None => Err("no op".into()),
        }
    }

    /// The request as one line, newline included.
    pub fn to_line(&self) -> String {
        let v = match self {
            Request::Status => json!({"op": "status"}),
            Request::Touch => json!({"op": "touch"}),
            Request::Refresh => json!({"op": "refresh"}),
            Request::Shutdown { version } => json!({"op": "shutdown", "version": version}),
        };
        format!("{v}\n")
    }
}

/// The status reply of the service (to `status`, `touch` and `refresh`).
/// Only `ok`, `version` and `pid` are required when parsing; every other
/// key may be missing (a service of another version).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    pub ok: bool,
    /// The error of a failed `refresh` build (with `ok` false).
    pub error: Option<String>,
    pub version: String,
    pub pid: u32,
    pub store_path: Option<String>,
    pub building: bool,
    pub db_usable: bool,
    /// `built_unix` of the usable `mail.db`; `None` when there is none.
    pub built_unix: Option<i64>,
    /// `None` when there is no database or the store cannot be checked.
    pub store_changed: Option<bool>,
    pub last_attempt_unix: Option<i64>,
    pub last_error: Option<String>,
    pub refresh_secs: Option<u64>,
    pub idle_secs: Option<u64>,
    pub started_unix: Option<i64>,
    pub builds: u64,
}

impl ServiceStatus {
    /// The reply as a JSON object; every key is present.
    /// `error` is present only when set.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "ok": self.ok,
            "version": self.version,
            "pid": self.pid,
            "store_path": self.store_path,
            "building": self.building,
            "db_usable": self.db_usable,
            "built_unix": self.built_unix,
            "store_changed": self.store_changed,
            "last_attempt_unix": self.last_attempt_unix,
            "last_error": self.last_error,
            "refresh_secs": self.refresh_secs,
            "idle_secs": self.idle_secs,
            "started_unix": self.started_unix,
            "builds": self.builds,
        });
        if let Some(e) = &self.error {
            v["error"] = json!(e);
        }
        v
    }

    /// Read a status reply; fails without `ok`, `version` or `pid`.
    pub fn from_json(v: &Value) -> Result<ServiceStatus, String> {
        let o: &Map<String, Value> = v.as_object().ok_or("status reply is not an object")?;
        let text = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
        let int = |k: &str| o.get(k).and_then(Value::as_i64);
        let uint = |k: &str| o.get(k).and_then(Value::as_u64);
        let flag = |k: &str| o.get(k).and_then(Value::as_bool);
        Ok(ServiceStatus {
            ok: flag("ok").ok_or("status reply without ok")?,
            error: text("error"),
            version: text("version").ok_or("status reply without version")?,
            pid: uint("pid")
                .and_then(|p| u32::try_from(p).ok())
                .ok_or("status reply without pid")?,
            store_path: text("store_path"),
            building: flag("building").unwrap_or(false),
            db_usable: flag("db_usable").unwrap_or(false),
            built_unix: int("built_unix"),
            store_changed: flag("store_changed"),
            last_attempt_unix: int("last_attempt_unix"),
            last_error: text("last_error"),
            refresh_secs: uint("refresh_secs"),
            idle_secs: uint("idle_secs"),
            started_unix: int("started_unix"),
            builds: uint("builds").unwrap_or(0),
        })
    }

    /// The reply to a `refresh` whose build failed: `ok` false and `error`.
    pub fn failed(self, error: &str) -> ServiceStatus {
        ServiceStatus {
            ok: false,
            error: Some(error.to_string()),
            ..self
        }
    }
}

/// Reply to a `shutdown` with the service's own version.
pub fn stopping_reply() -> Value {
    json!({"ok": true, "stopping": true})
}

/// Reply to a `shutdown` with another version than `own`.
pub fn version_mismatch_reply(own: &str) -> Value {
    json!({"ok": false, "error": "version mismatch", "version": own})
}

/// Reply to anything that is not a request.
pub fn bad_request_reply() -> Value {
    json!({"ok": false, "error": "bad request"})
}

/// What a reply to `shutdown` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownReply {
    /// The service stops.
    Stopping,
    /// Refused: the service's version is another one.
    VersionMismatch { version: String },
    /// Any other refusal, with its error text.
    Refused(String),
}

impl ShutdownReply {
    pub fn from_json(v: &Value) -> ShutdownReply {
        let error = v.get("error").and_then(Value::as_str);
        if v.get("ok") == Some(&Value::Bool(true)) && v.get("stopping") == Some(&Value::Bool(true))
        {
            return ShutdownReply::Stopping;
        }
        match (error, v.get("version").and_then(Value::as_str)) {
            (Some("version mismatch"), Some(version)) => ShutdownReply::VersionMismatch {
                version: version.into(),
            },
            (Some(e), _) => ShutdownReply::Refused(e.into()),
            (None, _) => ShutdownReply::Refused(format!("unexpected reply {v}")),
        }
    }
}

/// Compare two versions `major.minor.patch` numerically; a suffix after
/// `-` or `+` is ignored, and a version that cannot be read counts as
/// equal to any other.
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    }
}

/// `(major, minor, patch)` of `v`, without a suffix after `-` or `+`.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut next = || -> Option<u64> {
        let p = parts.next()?;
        // `parse` would accept a leading `+`.
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Longest socket path, in bytes: the size of `sockaddr_un.sun_path` minus
/// its terminating NUL.
pub fn socket_path_limit() -> usize {
    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    std::mem::size_of_val(&addr.sun_path) - 1
}

/// The absolute path of `service.sock` in `dir`; an error when it does not
/// fit in `sockaddr_un.sun_path` (see [`socket_path_limit`]).
pub fn socket_path(dir: &Path) -> Result<PathBuf, String> {
    let dir = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let p = dir.join(SOCKET_NAME);
    let len = p.as_os_str().len();
    if len > socket_path_limit() {
        return Err(format!(
            "the socket path {} is too long ({len} bytes, at most {})",
            p.display(),
            socket_path_limit()
        ));
    }
    Ok(p)
}

/// Why [`exchange`] got no reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    /// No socket, not a socket owned by this user, or the connection was
    /// refused: no service runs.
    NotRunning,
    /// Connected, but no reply in time.
    Timeout,
    /// The reply ended without a newline or is not a JSON object: an error
    /// of the service.
    BadReply,
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExchangeError::NotRunning => "the refresh service is not running",
            ExchangeError::Timeout => "the refresh service did not answer in time",
            ExchangeError::BadReply => "the refresh service gave an unreadable reply",
        })
    }
}

/// Send `req` to the service listening on `sock` and read its one-line
/// reply, all within `timeout`.
pub fn exchange(sock: &Path, req: &Request, timeout: Duration) -> Result<Value, ExchangeError> {
    let deadline = Instant::now() + timeout;
    let m = std::fs::symlink_metadata(sock).map_err(|_| ExchangeError::NotRunning)?;
    // SAFETY: getuid() has no preconditions and cannot fail.
    if !m.file_type().is_socket() || m.uid() != unsafe { libc::getuid() } {
        return Err(ExchangeError::NotRunning);
    }
    let mut s = UnixStream::connect(sock).map_err(|_| ExchangeError::NotRunning)?;
    let timed_out =
        |e: &std::io::Error| matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut);
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(ExchangeError::Timeout)
    };
    let first = remaining()?;
    s.set_write_timeout(Some(first))
        .and_then(|()| s.set_read_timeout(Some(first)))
        .map_err(|_| ExchangeError::Timeout)?;
    s.write_all(req.to_line().as_bytes()).map_err(|e| {
        if timed_out(&e) {
            ExchangeError::Timeout
        } else {
            // The service closed the connection before reading it.
            ExchangeError::BadReply
        }
    })?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // Shorten the read timeout to what is left. This fails on some
        // systems once the peer has closed its end; the read then returns
        // at once, and the timeout set above still bounds it otherwise.
        let _ = s.set_read_timeout(Some(remaining()?));
        match s.read(&mut buf) {
            Ok(0) => return Err(ExchangeError::BadReply),
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if let Some(end) = reply.iter().position(|&b| b == b'\n') {
                    reply.truncate(end);
                    break;
                }
                if reply.len() > MAX_REPLY_BYTES {
                    return Err(ExchangeError::BadReply);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if timed_out(&e) => return Err(ExchangeError::Timeout),
            Err(_) => return Err(ExchangeError::BadReply),
        }
    }
    match serde_json::from_slice::<Value>(&reply) {
        Ok(v) if v.is_object() => Ok(v),
        _ => Err(ExchangeError::BadReply),
    }
}

/// Whether process `pid` is gone: it does not exist, or it is a zombie
/// (exited, not yet waited for). False when it runs or cannot be examined.
/// Sends no signal that acts on the process.
///
/// On macOS a process is reported as a zombie as soon as it starts to exit;
/// its `flock` locks can stay held for a fraction of a millisecond longer.
pub fn pid_gone(pid: u32) -> bool {
    // 0 and negative values name process groups for kill().
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return true;
    }
    is_zombie(pid)
}

/// Whether `pid` is a zombie, or vanished meanwhile.
#[cfg(target_os = "macos")]
fn is_zombie(pid: libc::pid_t) -> bool {
    // SAFETY: proc_bsdinfo is plain data; all zeroes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n == size {
        return info.pbi_status == libc::SZOMB;
    }
    n <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Whether `pid` is a zombie, or vanished meanwhile.
#[cfg(target_os = "linux")]
fn is_zombie(pid: libc::pid_t) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the command name, which is in parentheses and
        // may itself contain them.
        Ok(stat) => {
            stat.rfind(')')
                .and_then(|i| stat[i + 1..].split_whitespace().next())
                == Some("Z")
        }
        Err(e) => e.kind() == ErrorKind::NotFound,
    }
}

/// The service as an MCP server sees it.
pub trait Refresher: std::fmt::Debug {
    /// Tell the service a client call happened; may start a build in the
    /// background, never waits for one.
    fn touch(&mut self, now: i64) -> Result<ServiceStatus, String>;
    /// The service's status.
    fn status(&mut self, now: i64) -> Result<ServiceStatus, String>;
    /// Build now (or join the running build) and wait for it.
    fn refresh(&mut self, now: i64) -> Result<ServiceStatus, String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Mutex, MutexGuard};
    use std::thread;

    /// Taken by every test here that creates a socket or starts a process:
    /// a child started by one test while another creates a socket could
    /// inherit that socket before it is marked close-on-exec, and keep a
    /// "closed" listener alive.
    fn serial() -> MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh directory with a short path under the system temp dir,
    /// removed on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxp-{}-{}",
                std::process::id(),
                N.fetch_add(1, AtomicOrdering::Relaxed)
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

    // --- requests ---

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::Status,
            Request::Touch,
            Request::Refresh,
            Request::Shutdown {
                version: "0.2.0".into(),
            },
        ] {
            let line = r.to_line();
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1);
            assert_eq!(Request::parse(&line), Ok(r.clone()));
            assert_eq!(Request::parse(line.trim_end()), Ok(r));
        }
        assert_eq!(
            Request::Shutdown {
                version: "0.2.0".into()
            }
            .to_line(),
            "{\"op\":\"shutdown\",\"version\":\"0.2.0\"}\n"
        );
    }

    #[test]
    fn unknown_keys_in_requests_are_ignored() {
        assert_eq!(
            Request::parse(r#"{"op":"touch","client":"x","n":3}"#),
            Ok(Request::Touch)
        );
        assert_eq!(
            Request::parse(r#"{"version":"1.0.0","op":"shutdown","extra":null}"#),
            Ok(Request::Shutdown {
                version: "1.0.0".into()
            })
        );
    }

    #[test]
    fn bad_requests_are_errors() {
        for line in [
            r#"{"op":"build"}"#,
            r#"{"op":"shutdown"}"#,
            r#"{"op":"shutdown","version":2}"#,
            r#"{"op":3}"#,
            r#"{}"#,
            r#"["status"]"#,
            r#""status""#,
            "status",
            "",
            r#"{"op":"status""#,
        ] {
            assert!(Request::parse(line).is_err(), "{line}");
        }
        let pad = |n: usize| format!(r#"{{"op":"status","pad":"{}"}}"#, "x".repeat(n));
        let base = pad(0).len();
        assert_eq!(
            Request::parse(&pad(MAX_REQUEST_BYTES - base)),
            Ok(Request::Status)
        );
        assert!(Request::parse(&pad(MAX_REQUEST_BYTES - base + 1)).is_err());
    }

    // --- status and other replies ---

    fn full_status() -> ServiceStatus {
        ServiceStatus {
            ok: true,
            error: None,
            version: "0.2.0".into(),
            pid: 4242,
            store_path: Some("/abs/path/HxStore.hxd".into()),
            building: false,
            db_usable: true,
            built_unix: Some(1_795_000_000),
            store_changed: Some(false),
            last_attempt_unix: Some(1_795_000_000),
            last_error: None,
            refresh_secs: Some(900),
            idle_secs: Some(1800),
            started_unix: Some(1_794_999_000),
            builds: 1,
        }
    }

    #[test]
    fn status_has_every_key_of_the_protocol() {
        let v = full_status().to_json();
        assert_eq!(
            v,
            json!({"ok":true,"version":"0.2.0","pid":4242,
                   "store_path":"/abs/path/HxStore.hxd",
                   "building":false,"db_usable":true,"built_unix":1795000000,
                   "store_changed":false,"last_attempt_unix":1795000000,"last_error":null,
                   "refresh_secs":900,"idle_secs":1800,"started_unix":1794999000,"builds":1})
        );
        assert_eq!(ServiceStatus::from_json(&v), Ok(full_status()));
    }

    #[test]
    fn status_nulls_round_trip() {
        let s = ServiceStatus {
            built_unix: None,
            store_changed: None,
            last_attempt_unix: None,
            last_error: Some("copy failed".into()),
            building: true,
            db_usable: false,
            ..full_status()
        };
        let v = s.to_json();
        assert_eq!(v["built_unix"], Value::Null);
        assert_eq!(v["store_changed"], Value::Null);
        assert_eq!(v["last_attempt_unix"], Value::Null);
        assert_eq!(v["last_error"], "copy failed");
        assert_eq!(ServiceStatus::from_json(&v), Ok(s));
    }

    #[test]
    fn status_ignores_unknown_keys_and_needs_only_the_frozen_ones() {
        let mut v = full_status().to_json();
        v["future_key"] = json!({"a": [1, 2]});
        assert_eq!(ServiceStatus::from_json(&v), Ok(full_status()));

        let minimal =
            ServiceStatus::from_json(&json!({"ok":true,"version":"9.0.0","pid":7})).unwrap();
        assert_eq!(
            minimal,
            ServiceStatus {
                ok: true,
                version: "9.0.0".into(),
                pid: 7,
                ..ServiceStatus::default()
            }
        );
        for bad in [
            json!({"version":"0.2.0","pid":1}),
            json!({"ok":true,"pid":1}),
            json!({"ok":true,"version":"0.2.0"}),
            json!({"ok":true,"version":"0.2.0","pid":-1}),
            json!({"ok":"yes","version":"0.2.0","pid":1}),
            json!([1]),
        ] {
            assert!(ServiceStatus::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn failed_refresh_reply_keeps_the_status_keys() {
        let v = full_status().failed("export: broken").to_json();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "export: broken");
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["builds"], 1);
        let s = ServiceStatus::from_json(&v).unwrap();
        assert!(!s.ok);
        assert_eq!(s.error.as_deref(), Some("export: broken"));
        // A successful reply carries no `error` key.
        assert!(full_status().to_json().get("error").is_none());
    }

    #[test]
    fn shutdown_and_bad_request_replies() {
        assert_eq!(stopping_reply(), json!({"ok":true,"stopping":true}));
        assert_eq!(
            version_mismatch_reply("0.2.0"),
            json!({"ok":false,"error":"version mismatch","version":"0.2.0"})
        );
        assert_eq!(
            bad_request_reply(),
            json!({"ok":false,"error":"bad request"})
        );
        assert_eq!(
            ShutdownReply::from_json(&stopping_reply()),
            ShutdownReply::Stopping
        );
        assert_eq!(
            ShutdownReply::from_json(&version_mismatch_reply("0.3.1")),
            ShutdownReply::VersionMismatch {
                version: "0.3.1".into()
            }
        );
        assert_eq!(
            ShutdownReply::from_json(&bad_request_reply()),
            ShutdownReply::Refused("bad request".into())
        );
        assert!(matches!(
            ShutdownReply::from_json(&json!({"ok":true})),
            ShutdownReply::Refused(_)
        ));
    }

    // --- versions and the socket path ---

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(cmp_versions("0.2.0", "0.10.0"), Ordering::Less);
        assert_eq!(cmp_versions("0.10.0", "0.2.0"), Ordering::Greater);
        assert_eq!(cmp_versions("0.2.0", "0.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("1.0.0", "0.99.99"), Ordering::Greater);
        assert_eq!(cmp_versions("0.2.0-rc1", "0.2.0"), Ordering::Equal);
        assert_eq!(cmp_versions("0.2.1-rc1", "0.2.0"), Ordering::Greater);
        assert_eq!(cmp_versions("0.2.0+build5", "0.2.0"), Ordering::Equal);
        for bad in ["", "abc", "0.2", "0.2.x", "1.2.3.4", "-1.0.0"] {
            assert_eq!(cmp_versions(bad, "0.2.0"), Ordering::Equal, "{bad}");
            assert_eq!(cmp_versions("0.2.0", bad), Ordering::Equal, "{bad}");
        }
    }

    #[test]
    fn socket_path_limit_comes_from_sockaddr_un() {
        // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
        let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        assert_eq!(
            socket_path_limit(),
            std::mem::size_of_val(&addr.sun_path) - 1
        );
        #[cfg(target_os = "macos")]
        assert_eq!(socket_path_limit(), 103);
        #[cfg(target_os = "linux")]
        assert_eq!(socket_path_limit(), 107);
    }

    #[test]
    fn socket_path_is_accepted_at_the_limit_and_refused_beyond() {
        let tail = format!("/{SOCKET_NAME}").len();
        let dir = |n: usize| PathBuf::from(format!("/{}", "d".repeat(n)));
        let at = socket_path_limit() - tail - 1;
        let p = socket_path(&dir(at)).unwrap();
        assert_eq!(p.as_os_str().len(), socket_path_limit());
        assert_eq!(p, dir(at).join(SOCKET_NAME));
        let e = socket_path(&dir(at + 1)).unwrap_err();
        assert!(e.contains("too long"), "{e}");
    }

    #[test]
    fn socket_path_is_absolute() {
        let want = std::path::absolute("rel").unwrap().join(SOCKET_NAME);
        // The working directory may be too deep for a socket; the message
        // then names the absolute path that was measured.
        match socket_path(Path::new("rel")) {
            Ok(p) => assert_eq!(p, want),
            Err(e) => assert!(e.contains(&*want.to_string_lossy()), "{e}"),
        }
    }

    // --- exchange ---

    /// A listener on `sock` that serves one connection: reads one line,
    /// then writes `reply` (as is) and closes. Returns the line it read.
    fn serve_once(sock: &Path, reply: &'static str) -> thread::JoinHandle<String> {
        let l = UnixListener::bind(sock).unwrap();
        thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 1];
            while s.read(&mut b).unwrap() == 1 && b[0] != b'\n' {
                got.push(b[0]);
            }
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8(got).unwrap()
        })
    }

    const SHORT: Duration = Duration::from_millis(300);

    #[test]
    fn exchange_round_trip() {
        let _serial = serial();
        let t = TestDir::new();
        let h = serve_once(&t.sock(), "{\"ok\":true,\"version\":\"0.2.0\",\"pid\":5}\n");
        let v = exchange(&t.sock(), &Request::Touch, SHORT).unwrap();
        assert_eq!(v, json!({"ok":true,"version":"0.2.0","pid":5}));
        assert_eq!(h.join().unwrap(), r#"{"op":"touch"}"#);
    }

    #[test]
    fn exchange_without_a_service_is_not_running() {
        let _serial = serial();
        let t = TestDir::new();
        // Missing.
        assert_eq!(
            exchange(&t.sock(), &Request::Status, SHORT),
            Err(ExchangeError::NotRunning)
        );
        // Left behind by a listener that is gone: refused.
        drop(UnixListener::bind(t.sock()).unwrap());
        assert!(t.sock().exists());
        assert_eq!(
            exchange(&t.sock(), &Request::Status, SHORT),
            Err(ExchangeError::NotRunning)
        );
        // A regular file.
        std::fs::remove_file(t.sock()).unwrap();
        std::fs::write(t.sock(), b"x").unwrap();
        assert_eq!(
            exchange(&t.sock(), &Request::Status, SHORT),
            Err(ExchangeError::NotRunning)
        );
        // A symbolic link, even to a live socket.
        std::fs::remove_file(t.sock()).unwrap();
        let real = t.0.join("real.sock");
        let _l = UnixListener::bind(&real).unwrap();
        std::os::unix::fs::symlink(&real, t.sock()).unwrap();
        assert_eq!(
            exchange(&t.sock(), &Request::Status, SHORT),
            Err(ExchangeError::NotRunning)
        );
    }

    #[test]
    fn exchange_with_a_silent_peer_times_out() {
        let _serial = serial();
        let t = TestDir::new();
        let l = UnixListener::bind(t.sock()).unwrap();
        let h = thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            thread::sleep(Duration::from_millis(800));
            drop(s);
        });
        let started = Instant::now();
        assert_eq!(
            exchange(&t.sock(), &Request::Status, SHORT),
            Err(ExchangeError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_millis(700));
        h.join().unwrap();
    }

    #[test]
    fn exchange_with_a_bad_reply() {
        let _serial = serial();
        for reply in [
            "{\"ok\":true,\"version\":\"0.2.0\",\"pid\":5}",
            "not json\n",
            "[1,2]\n",
            "",
        ] {
            let t = TestDir::new();
            let h = serve_once(&t.sock(), reply);
            assert_eq!(
                exchange(&t.sock(), &Request::Status, SHORT),
                Err(ExchangeError::BadReply),
                "{reply:?}"
            );
            h.join().unwrap();
        }
    }

    // --- processes ---

    #[test]
    fn running_processes_are_not_gone() {
        let _serial = serial();
        assert!(!pid_gone(std::process::id()));
        // pid 1 exists and is never a zombie.
        assert!(!pid_gone(1));
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        assert!(!pid_gone(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn killed_child_is_gone_before_and_after_it_is_waited_for() {
        let _serial = serial();
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        child.kill().unwrap();
        // Not waited for: a zombie.
        let started = Instant::now();
        while !pid_gone(pid) {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "a killed child is not seen as gone"
            );
            thread::sleep(Duration::from_millis(10));
        }
        child.wait().unwrap();
        assert!(pid_gone(pid));
    }
}
