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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.arg("mcp")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = Running(cmd.spawn().unwrap());
    let out = drain(child.0.stdout.take().unwrap());
    let err = drain(child.0.stderr.take().unwrap());
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
    let err = String::from_utf8_lossy(&err.join().unwrap()).into_owned();
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
    assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 9);
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
        vec![Path::new("--colour"), Path::new("red")],
    ] {
        let (status, out, err) = mcp(&args, &[("HOME", &d.0)], b"");
        assert_eq!(status.code(), Some(2), "{args:?}: {err}");
        assert!(out.is_empty());
    }
}
