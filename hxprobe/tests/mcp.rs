//! Command-line behaviour of `hxprobe mcp`: protocol, framing and exit codes
//! on a header-only synthetic store. Content is checked by the unit tests.
//!
//! Every test works in its own directory under the system temp dir and
//! removes it when done. A child that does not exit within [`TIMEOUT`] is
//! killed and the test fails.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A scratch directory removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("hxprobe-mcp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A store with a valid header and no blocks.
fn empty_store() -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[..8].copy_from_slice(hxprobe::header::MAGIC);
    s[8] = b'i';
    s[0x38..0x40].copy_from_slice(&4096u64.to_le_bytes());
    s
}

/// Kills the child on drop, so a failed assertion never leaves it running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drain(mut r: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = r.read_to_end(&mut v);
        v
    })
}

/// Run `hxprobe mcp <args>` with `input` on stdin (then closed) and `env`
/// set; exit status, stdout and stderr. Fails the test after [`TIMEOUT`].
fn mcp(args: &[&Path], env: &[(&str, &Path)], input: &[u8]) -> (ExitStatus, String, String) {
    mcp_with(args, input, true, |cmd| {
        for (k, v) in env {
            cmd.env(k, v);
        }
    })
}

/// [`mcp`] with the command changed by `configure` before it starts. With
/// `read_stderr` false the read end of the child's stderr is closed at once
/// (every later write to it fails) and the returned stderr is empty.
fn mcp_with(
    args: &[&Path],
    input: &[u8],
    read_stderr: bool,
    configure: impl FnOnce(&mut Command),
) -> (ExitStatus, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.arg("mcp")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The in-process mode: these tests start no refresh service.
        .env("HXPROBE_SERVICE", "0")
        .env_remove("HXPROBE_TEST_BUILD_DELAY_MS");
    configure(&mut cmd);
    let mut child = Running(cmd.spawn().unwrap());
    let out = drain(child.0.stdout.take().unwrap());
    let stderr = child.0.stderr.take().unwrap();
    let err = if read_stderr {
        Some(drain(stderr))
    } else {
        drop(stderr);
        None
    };
    {
        let mut stdin = child.0.stdin.take().unwrap();
        // The child may exit before reading (bad options): ignore EPIPE.
        let _ = stdin.write_all(input);
        // Dropping stdin closes it.
    }
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.0.kill();
            panic!("hxprobe mcp did not exit within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = String::from_utf8(out.join().unwrap()).expect("stdout is UTF-8");
    let err = err.map_or_else(String::new, |e| {
        String::from_utf8_lossy(&e.join().unwrap()).into_owned()
    });
    (status, out, err)
}

fn request(id: i64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    )
}

/// The parsed JSON text of a `tools/call` result.
fn tool_result(r: &Value) -> Value {
    assert_eq!(r["result"]["isError"], false, "{r}");
    serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn mcp_session_on_an_empty_store() {
    let d = Dir::new("session");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    let cache = d.path("cache");

    let mut input = String::new();
    input += &request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "test", "version": "0"}}),
    );
    input += "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
    input += &request(2, "tools/list", json!({}));
    input += &request(3, "tools/call", json!({"name": "store_status"}));
    input += &request(
        4,
        "tools/call",
        json!({"name": "list_messages", "arguments": {}}),
    );
    input += &request(
        5,
        "tools/call",
        json!({"name": "list_events", "arguments": {"since": "2020-01-01"}}),
    );
    input += "{not json\n";

    let (status, out, err) = mcp(
        &[
            Path::new("--store"),
            &store,
            Path::new("--cache-dir"),
            &cache,
        ],
        &[],
        input.as_bytes(),
    );
    assert_eq!(status.code(), Some(0), "stderr: {err}");

    let lines: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect();
    assert!(out.ends_with('\n'));
    let ids: Vec<&Value> = lines.iter().map(|r| &r["id"]).collect();
    assert_eq!(
        ids,
        [
            &json!(1),
            &json!(2),
            &json!(3),
            &json!(4),
            &json!(5),
            &Value::Null
        ]
    );

    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "hxprobe-outlook");
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 10);
    let status_v = tool_result(&lines[2]);
    assert_eq!(status_v["messages"], 0);
    assert_eq!(status_v["last_error"], Value::Null);
    assert!(status_v["built"].is_string());
    assert_eq!(tool_result(&lines[3])["total"], 0);
    assert_eq!(tool_result(&lines[4])["total"], 0);
    assert_eq!(lines[5]["error"]["code"], -32700);

    let db = std::fs::metadata(cache.join("mail.db")).expect("mail.db");
    assert_eq!(db.permissions().mode() & 0o777, 0o600);
    let dir = std::fs::metadata(&cache).unwrap();
    assert_eq!(dir.permissions().mode() & 0o777, 0o700);
}

/// `hxprobe mcp` fails before serving: exit 1, a message on stderr, nothing
/// on stdout.
fn assert_fails_early(args: &[&Path], env: &[(&str, &Path)]) {
    let input = request(1, "ping", json!({}));
    let (status, out, err) = mcp(args, env, input.as_bytes());
    assert_eq!(status.code(), Some(1), "stderr: {err}");
    assert!(out.is_empty(), "stdout: {out}");
    assert!(!err.trim().is_empty());
}

#[test]
fn mcp_with_a_missing_store_fails() {
    let d = Dir::new("missing");
    assert_fails_early(
        &[
            Path::new("--store"),
            Path::new("/nonexistent/HxStore.hxd"),
            Path::new("--cache-dir"),
            &d.path("cache"),
        ],
        &[],
    );
}

#[test]
fn mcp_with_a_directory_as_store_fails() {
    let d = Dir::new("dirstore");
    assert_fails_early(
        &[
            Path::new("--store"),
            &d.0,
            Path::new("--cache-dir"),
            &d.path("cache"),
        ],
        &[],
    );
}

#[test]
fn mcp_without_a_default_store_fails() {
    let d = Dir::new("nohome");
    assert_fails_early(&[], &[("HOME", &d.0)]);
}

#[test]
fn mcp_with_bad_options_fails() {
    let d = Dir::new("badopts");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    for args in [
        vec![Path::new("--store")],
        vec![Path::new("--refresh-secs"), Path::new("soon")],
        vec![Path::new("--idle-secs"), Path::new("soon")],
        vec![Path::new("--colour"), Path::new("red")],
    ] {
        let (status, out, err) = mcp(&args, &[("HOME", &d.0)], b"");
        assert_eq!(status.code(), Some(2), "{args:?}: {err}");
        assert!(out.is_empty());
    }
}

#[test]
fn mcp_without_home_and_cache_dir_fails() {
    let d = Dir::new("nocachedir");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    let input = request(1, "ping", json!({}));
    let (status, out, err) = mcp_with(
        &[Path::new("--store"), &store],
        input.as_bytes(),
        true,
        |cmd| {
            cmd.env_remove("HOME");
        },
    );
    assert_eq!(status.code(), Some(1), "stderr: {err}");
    assert!(out.is_empty(), "stdout: {out}");
    assert!(err.contains("--cache-dir"), "stderr: {err}");
}

/// The responses on `out`, one JSON value per line.
fn responses(out: &str) -> Vec<Value> {
    out.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect()
}

#[test]
fn mcp_serves_a_store_with_a_bad_header() {
    let d = Dir::new("badheader");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, vec![b'x'; 512]).unwrap();
    let cache = d.path("cache");
    let mut input = request(1, "ping", json!({}));
    input += &request(2, "tools/call", json!({"name": "store_status"}));
    input += &request(3, "tools/call", json!({"name": "list_messages"}));
    let (status, out, err) = mcp(
        &[
            Path::new("--store"),
            &store,
            Path::new("--cache-dir"),
            &cache,
        ],
        &[],
        input.as_bytes(),
    );
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    assert!(err.contains("warning"), "stderr: {err}");
    let lines = responses(&out);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["result"], json!({}));
    let status_v = tool_result(&lines[1]);
    assert_eq!(status_v["built"], Value::Null);
    assert_eq!(status_v["messages"], 0);
    assert!(status_v["last_error"].is_string(), "{status_v}");
    assert_eq!(lines[2]["result"]["isError"], true);
    assert!(!cache.join("mail.db").exists());
}

/// `store_path` reported by `store_status` of a server started in `cwd`
/// with `--store <store>` and the cache directory `cache`.
fn served_store_path(cwd: &Path, store: &Path, cache: &Path) -> String {
    let input = request(1, "tools/call", json!({"name": "store_status"}));
    let args = [Path::new("--store"), store, Path::new("--cache-dir"), cache];
    let (status, out, err) = mcp_with(&args, input.as_bytes(), true, |cmd| {
        cmd.current_dir(cwd);
    });
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    let v = tool_result(&responses(&out)[0]);
    assert_eq!(v["last_error"], Value::Null);
    assert!(v["built"].is_string());
    v["store_path"].as_str().unwrap().to_string()
}

#[test]
fn mcp_relative_store_paths_do_not_share_a_database() {
    let d = Dir::new("relative");
    let cache = d.path("cache");
    let mut seen = Vec::new();
    for name in ["a", "b"] {
        let dir = d.path(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("HxStore.hxd"), empty_store()).unwrap();
        let got = served_store_path(&dir, Path::new("HxStore.hxd"), &cache);
        let want = std::fs::canonicalize(dir.join("HxStore.hxd")).unwrap();
        assert_eq!(Path::new(&got), want);
        seen.push(got);
    }
    assert_ne!(seen[0], seen[1]);
}

#[test]
fn mcp_symlinked_store_is_served_by_its_real_path() {
    let d = Dir::new("symlink");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    let link = d.path("link.hxd");
    std::os::unix::fs::symlink(&store, &link).unwrap();
    let got = served_store_path(&d.0, &link, &d.path("cache"));
    assert_eq!(Path::new(&got), std::fs::canonicalize(&store).unwrap());
}

#[test]
fn mcp_keeps_serving_when_stderr_is_closed() {
    let d = Dir::new("nostderr");
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    let cache = d.path("cache");
    let mut input = request(1, "tools/call", json!({"name": "store_status"}));
    input += &request(2, "tools/call", json!({"name": "list_messages"}));
    input += &request(3, "ping", json!({}));
    let args = [
        Path::new("--store"),
        &store,
        Path::new("--cache-dir"),
        &cache,
    ];
    let (status, out, _) = mcp_with(&args, input.as_bytes(), false, |_| {});
    assert_eq!(status.code(), Some(0));
    let lines = responses(&out);
    assert_eq!(lines.len(), 3);
    assert_eq!(tool_result(&lines[0])["last_error"], Value::Null);
    assert_eq!(tool_result(&lines[1])["total"], 0);
    assert_eq!(lines[2]["result"], json!({}));
}

/// Whether the tests run as root, for whom mode 000 does not deny.
fn is_root() -> bool {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Sets the mode of a file and gives it mode 0600 back on drop, also when
/// the test fails.
struct Mode(PathBuf);

impl Mode {
    fn set(p: &Path, mode: u32) -> Mode {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        Mode(p.to_path_buf())
    }
}

impl Drop for Mode {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o600));
    }
}

#[test]
fn mcp_fallback_on_an_unreadable_store_names_the_remedy() {
    if is_root() {
        return;
    }
    let program = std::fs::canonicalize(env!("CARGO_BIN_EXE_hxprobe")).unwrap();
    let note = "the shared program was not used";
    for with_note in [false, true] {
        let d = Dir::new(if with_note { "deniednote" } else { "denied" });
        let store = d.path("HxStore.hxd");
        std::fs::write(&store, empty_store()).unwrap();
        let cache = d.path("cache");
        let args = [
            Path::new("--store"),
            &store,
            Path::new("--cache-dir"),
            &cache,
        ];
        let input = request(1, "tools/call", json!({"name": "list_messages"}));
        let (status, out, err) = {
            let _m = Mode::set(&store, 0o000);
            mcp_with(&args, input.as_bytes(), true, |cmd| {
                cmd.env("HOME", &d.0);
                if with_note {
                    cmd.env("HXPROBE_INSTALL_NOTE", note);
                } else {
                    cmd.env_remove("HXPROBE_INSTALL_NOTE");
                }
            })
        };
        assert_eq!(status.code(), Some(0), "stderr: {err}");
        let lines = responses(&out);
        assert_eq!(lines[0]["result"]["isError"], true, "{}", lines[0]);
        let e = lines[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            e.starts_with("this server cannot read the store: permission denied"),
            "{e}"
        );
        assert!(e.contains(&format!("\"{}\"", program.display())), "{e}");
        assert!(!e.contains("--stop"), "{e}");
        if with_note {
            assert!(e.ends_with(&format!(" Note: {note}")), "{e}");
        } else {
            assert!(!e.contains(" Note: "), "{e}");
        }
    }
}

/// Run a `store_status` call in fallback mode (`configure` chooses how) and
/// check that no refresh service was started or asked.
fn assert_fallback(name: &str, args: &[&Path], configure: impl FnOnce(&mut Command)) {
    let d = Dir::new(name);
    let store = d.path("HxStore.hxd");
    std::fs::write(&store, empty_store()).unwrap();
    let cache = d.path("cache");
    let mut all = vec![
        Path::new("--store"),
        &store,
        Path::new("--cache-dir"),
        &cache,
    ];
    all.extend_from_slice(args);
    let input = request(1, "tools/call", json!({"name": "store_status"}));
    let (status, out, err) = mcp_with(&all, input.as_bytes(), true, |cmd| {
        cmd.env("HOME", &d.0);
        configure(cmd);
    });
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    let v = tool_result(&responses(&out)[0]);
    assert_eq!(v["service"], Value::Null, "{v}");
    assert_eq!(v["last_error"], Value::Null, "{v}");
    assert!(v["built"].is_string(), "{v}");
    for f in ["service.sock", "service.lock", "service.log"] {
        assert!(!cache.join(f).exists(), "{f} exists");
    }
    assert!(err.contains("fallback"), "stderr: {err}");
}

#[test]
fn mcp_with_hxprobe_service_0_runs_without_a_service() {
    assert_fallback("envzero", &[], |_| {});
}

#[test]
fn mcp_with_no_service_runs_without_a_service() {
    // Without the variable, so only the option can choose the mode.
    assert_fallback("noservice", &[Path::new("--no-service")], |cmd| {
        cmd.env_remove("HXPROBE_SERVICE");
    });
}
