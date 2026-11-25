//! Command-line behaviour of `hxprobe mcp`: protocol, framing and exit codes
//! on a header-only synthetic store. Content is checked by the unit tests.
//!
//! Every test works in its own directory under the system temp dir and
//! removes it when done. A child that does not exit within [`TIMEOUT`] is
//! killed and the test fails.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{mpsc, Mutex};
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

/// Start `cmd`. Every child of this file is started under one lock: on
/// macOS a pipe becomes close-on-exec in a step after its creation, so a child
/// that another test started meanwhile could inherit it and keep, say, the
/// stdin of a server open.
fn spawn(cmd: &mut Command) -> Child {
    static SPAWN: Mutex<()> = Mutex::new(());
    let _g = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    cmd.spawn().unwrap()
}

/// [`Command::output`], started by [`spawn`].
fn output(cmd: &mut Command) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd).wait_with_output().unwrap()
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
    let mut child = Running(spawn(&mut cmd));
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

/// A property value of [`typed_item`].
enum Prop<'a> {
    Text(&'a str),
    Int(i64),
    /// A reference to the object `guid` of type `type_id`.
    Ref(u16, [u8; 16]),
}

/// A `u32 size` prefix and an object of the schema type `type_name` with
/// `guid` and `values`, laid out as the store lays out objects: text of data
/// type 7 as UTF-16LE in heap B, other text as UTF-8 in heap A, integers and
/// references in the fixed part.
fn typed_item(type_name: &str, guid: [u8; 16], values: &[(&str, Prop)]) -> Vec<u8> {
    let t = hxprobe::props_gen::TYPES
        .iter()
        .find(|t| t.name == type_name)
        .unwrap();
    let mut item = vec![0u8; t.fixed_len as usize];
    let (mut heap_a, mut heap_b) = (Vec::new(), Vec::new());
    for (name, v) in values {
        let p = t.props.iter().find(|p| p.name == *name).unwrap();
        let at = p.offset as usize;
        match v {
            Prop::Text(s) => {
                let (heap, data, flag) = if p.dt == 7 {
                    let d: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
                    (&mut heap_b, d, 0x8000_0000u32)
                } else {
                    (&mut heap_a, s.as_bytes().to_vec(), 0)
                };
                item[at..at + 4].copy_from_slice(&(heap.len() as u32).to_le_bytes());
                item[at + 4..at + 8].copy_from_slice(&(data.len() as u32 | flag).to_le_bytes());
                heap.extend_from_slice(&data);
            }
            Prop::Int(i) => {
                let size = usize::from(p.size);
                item[at..at + size].copy_from_slice(&i.to_le_bytes()[..size]);
            }
            Prop::Ref(type_id, g) => {
                item[at..at + 2].copy_from_slice(&1u16.to_le_bytes());
                item[at + 2..at + 4].copy_from_slice(&type_id.to_le_bytes());
                item[at + 4..at + 20].copy_from_slice(g);
            }
        }
    }
    if !heap_a.is_empty() {
        item[0x68..0x6c].copy_from_slice(&(heap_a.len() as u32).to_le_bytes());
    }
    let fixed_len = item.len() as u16;
    item.extend_from_slice(&heap_a);
    item.extend_from_slice(&heap_b);
    let size = item.len() as u32;
    item[0..2].copy_from_slice(&5u16.to_le_bytes());
    item[2..4].copy_from_slice(&fixed_len.to_le_bytes());
    item[4..8].copy_from_slice(&size.to_le_bytes());
    item[10..12].copy_from_slice(&t.type_id.to_le_bytes());
    item[12..28].copy_from_slice(&guid);
    let mut out = size.to_le_bytes().to_vec();
    out.extend_from_slice(&item);
    out
}

/// A block of page `page` holding `value` as one literal-only LZ4 sequence.
fn block(page: u64, value: &[u8]) -> Vec<u8> {
    use hxprobe::block::{crc32, HEADER, MAGIC};
    let mut payload = vec![(value.len().min(15) as u8) << 4];
    if value.len() >= 15 {
        let mut rest = value.len() - 15;
        while rest >= 255 {
            payload.push(0xff);
            rest -= 255;
        }
        payload.push(rest as u8);
    }
    payload.extend_from_slice(value);
    let key = page.to_le_bytes();
    let mut b = vec![0u8; HEADER];
    b[0x08..0x10].copy_from_slice(&MAGIC.to_le_bytes());
    b[0x10..0x14].copy_from_slice(&(key.len() as u32).to_le_bytes());
    b[0x14..0x18].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    b[0x18..0x1c].copy_from_slice(&(value.len() as u32).to_le_bytes());
    b[0x1c..0x20].copy_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&key);
    b.extend_from_slice(&payload);
    let body = crc32(&b[0x08..]);
    b[0x04..0x08].copy_from_slice(&body.to_le_bytes());
    let head = crc32(&b[0x04..HEADER]);
    b[0x00..0x04].copy_from_slice(&head.to_le_bytes());
    b
}

/// A store of `items`, one block per item on pages 1, 2, ...
fn store_of(items: &[Vec<u8>]) -> Vec<u8> {
    let mut s = empty_store();
    for (i, it) in items.iter().enumerate() {
        s.extend_from_slice(&block(i as u64 + 1, it));
        s.resize(s.len().next_multiple_of(512), 0);
    }
    s
}

/// The folder of attachment files inside a profile directory.
const ATT_DIR: &str = "Files/S0/3/Attachments/0";
/// The message the attachments of [`attachment_store`] belong to.
const ATT_MESSAGE: [u8; 16] = [0x21; 16];

/// A store with one downloaded attachment record per `(guid, file name,
/// display name, content type, size)`, each of [`ATT_MESSAGE`], named
/// `~/Files/S0/3/Attachments/0/<file name>`.
fn attachment_store(atts: &[([u8; 16], &str, &str, &str, usize)]) -> Vec<u8> {
    let items: Vec<Vec<u8>> = atts
        .iter()
        .map(|(guid, file, name, ct, size)| {
            let stored = format!("~/{ATT_DIR}/{file}");
            typed_item(
                "AttachmentHeader",
                *guid,
                &[
                    ("Filename", Prop::Text(&stored)),
                    ("DisplayName", Prop::Text(name)),
                    ("ContentType", Prop::Text(ct)),
                    ("Size", Prop::Int(*size as i64)),
                    ("InlineStatus", Prop::Int(0)),
                    ("DownloadStatus", Prop::Int(2)),
                    ("MessageHeader", Prop::Ref(0xc9, ATT_MESSAGE)),
                ],
            )
        })
        .collect();
    store_of(&items)
}

/// Lowercase hex of a guid.
fn hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

/// `text` shows no directory of the test (`dir` as given or canonical),
/// no part of a stored file name and no `file:` address.
fn assert_no_path(text: &str, dir: &Path) {
    let real = std::fs::canonicalize(dir).unwrap();
    for bad in [
        dir.to_string_lossy().as_ref(),
        real.to_string_lossy().as_ref(),
        "Files/",
        "~/",
        "file:",
    ] {
        assert!(!text.contains(bad), "{bad:?} in {text}");
    }
}

#[test]
fn mcp_reads_text_image_and_pdf_attachments() {
    let d = Dir::new("attachment");
    let content = "Quarterly budget figures\r\nsecond line\n";
    let guid = [0x91; 16];
    let (image_guid, pdf_guid) = ([0x92; 16], [0x93; 16]);
    let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
    image.extend((0..1000u32).map(|i| (i * 7) as u8));
    let mut pdf = b"%PDF-1.7\n".to_vec();
    pdf.extend((0..2001u32).map(|i| (i * 11) as u8));
    std::fs::write(
        d.path("HxStore.hxd"),
        attachment_store(&[
            (
                guid,
                "notes[1].txt",
                "Budget notes",
                "text/plain",
                content.len(),
            ),
            (
                image_guid,
                "chart[2].png",
                "Chart",
                "image/png",
                image.len(),
            ),
            (
                pdf_guid,
                "report[3].pdf",
                "Report",
                "application/pdf",
                pdf.len(),
            ),
        ]),
    )
    .unwrap();
    std::fs::create_dir_all(d.path(ATT_DIR)).unwrap();
    std::fs::write(d.path(ATT_DIR).join("notes[1].txt"), content).unwrap();
    std::fs::write(d.path(ATT_DIR).join("chart[2].png"), &image).unwrap();
    std::fs::write(d.path(ATT_DIR).join("report[3].pdf"), &pdf).unwrap();

    let mut input = request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "test", "version": "0"}}),
    );
    input += "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
    input += &request(
        2,
        "tools/call",
        json!({"name": "list_attachments", "arguments": {"message_guid": hex(&ATT_MESSAGE)}}),
    );
    for (id, g) in [(3, guid), (4, image_guid), (5, pdf_guid)] {
        input += &request(
            id,
            "tools/call",
            json!({"name": "get_attachment", "arguments": {"guid": hex(&g)}}),
        );
    }
    let store = d.path("HxStore.hxd");
    let cache = d.path("cache");
    let (status, out, err) = mcp(
        &[
            Path::new("--store"),
            &store,
            Path::new("--cache-dir"),
            &cache,
        ],
        &[("HOME", &d.0)],
        input.as_bytes(),
    );
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    let lines = responses(&out);
    assert_eq!(lines.len(), 5, "{out}");

    let list = tool_result(&lines[1]);
    assert_eq!(list["total"], 3, "{list}");
    assert_eq!(list["items"][0]["guid"], hex(&guid));
    assert_eq!(list["items"][0]["filename"], "notes[1].txt");

    let content_items = lines[2]["result"]["content"].as_array().unwrap();
    assert_eq!(content_items.len(), 1, "{}", lines[2]);
    assert_eq!(content_items[0]["type"], "text");
    let v = tool_result(&lines[2]);
    assert_eq!(v["kind"], "text");
    assert_eq!(v["text"], content.replace('\r', ""));
    assert_eq!(v["filename"], "notes[1].txt");
    assert_eq!(v["file_size"], content.len());

    // An image and a PDF: the JSON, then the bytes as base64 of the length
    // the file's size gives.
    for (line, kind, len, item, data) in [
        (
            &lines[3],
            "image",
            image.len(),
            "image",
            &lines[3]["result"]["content"][1]["data"],
        ),
        (
            &lines[4],
            "pdf",
            pdf.len(),
            "resource",
            &lines[4]["result"]["content"][1]["resource"]["blob"],
        ),
    ] {
        let items = line["result"]["content"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{line}");
        assert_eq!(tool_result(line)["kind"], kind);
        assert_eq!(items[1]["type"], item);
        assert_eq!(data.as_str().unwrap().len(), len.div_ceil(3) * 4, "{kind}");
    }

    assert_no_path(&out, &d.0);
    let err = err.to_lowercase();
    for word in ["notes[1]", "quarterly", "budget", "figures", "second line"] {
        assert!(!err.contains(word), "{word:?} in stderr: {err}");
    }
}

// --- the hidden `pdf-text` subcommand ---

/// The line of text of [`one_page_pdf`].
const PDF_LINE: &str = "Quarterly figures are attached";

/// A one-page PDF with [`PDF_LINE`] in Helvetica: four objects, a
/// cross-reference table with their offsets, a trailer, and an
/// uncompressed content stream.
fn one_page_pdf() -> Vec<u8> {
    let content = format!("BT /F1 24 Tf 72 720 Td ({PDF_LINE}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for o in offsets {
        table += &format!("{o:010} 00000 n \n");
    }
    table += &format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );
    pdf.extend_from_slice(table.as_bytes());
    pdf
}

/// Run `hxprobe pdf-text <extra>` with `input` on stdin (then closed) and
/// an empty environment; exit status, stdout and stderr.
fn pdf_text(extra: &[&str], input: &[u8]) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.arg("pdf-text")
        .args(extra)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running(spawn(&mut cmd));
    let out = drain(child.0.stdout.take().unwrap());
    let err = drain(child.0.stderr.take().unwrap());
    {
        let mut stdin = child.0.stdin.take().unwrap();
        let _ = stdin.write_all(input);
    }
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        assert!(started.elapsed() < TIMEOUT, "pdf-text did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, out.join().unwrap(), err.join().unwrap())
}

#[test]
fn pdf_text_gives_the_text_of_a_pdf() {
    let (status, out, err) = pdf_text(&[], &one_page_pdf());
    assert_eq!(
        status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&err)
    );
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.trim(), PDF_LINE, "{text:?}");
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}

#[test]
fn pdf_text_of_a_damaged_pdf_fails_silently() {
    let mut bad = b"%PDF-1.7\n".to_vec();
    bad.extend((0..5000u32).map(|i| (i * 13) as u8));
    let (status, out, err) = pdf_text(&[], &bad);
    assert!(!status.success(), "{status:?}");
    assert!(out.is_empty(), "{out:?}");
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
}

#[test]
fn pdf_text_of_empty_input_fails() {
    let (status, out, _) = pdf_text(&[], b"");
    assert!(!status.success(), "{status:?}");
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn pdf_text_with_an_argument_fails_without_reading() {
    // Stdin stays open and nothing is written: a subcommand that read its
    // input would wait for ever.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
    cmd.args(["pdf-text", "/etc/hosts"])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running(spawn(&mut cmd));
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "pdf-text with an argument waited for input"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(2));
    let mut out = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn usage_does_not_name_pdf_text() {
    let out = output(&mut Command::new(env!("CARGO_BIN_EXE_hxprobe")));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("usage:"), "{all}");
    assert!(!all.contains("pdf-text"), "{all}");
}

// --- a PDF the whole way: server, child, library ---

/// Standard base64 with padding, decoded; `None` for anything else.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for chunk in b.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = n << 6 | ABC.iter().position(|&x| x == c)? as u32;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// A running `hxprobe mcp` (fallback mode) on the store of `d`, asked one
/// request at a time.
struct Session {
    child: Running,
    stdin: Option<std::process::ChildStdin>,
    /// The lines of its stdout, read by a thread of their own.
    lines: mpsc::Receiver<String>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    next_id: i64,
}

impl Session {
    fn start(d: &Dir) -> Session {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hxprobe"));
        cmd.arg("mcp")
            .arg("--store")
            .arg(d.path("HxStore.hxd"))
            .arg("--cache-dir")
            .arg(d.path("cache"))
            .env("HXPROBE_SERVICE", "0")
            .env("HOME", &d.0)
            .env_remove("HXPROBE_TEST_BUILD_DELAY_MS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Running(spawn(&mut cmd));
        let stdin = child.0.stdin.take();
        let stdout = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if tx.send(line + "\n").is_err() {
                    break;
                }
            }
        });
        let stderr = Some(drain(child.0.stderr.take().unwrap()));
        let mut s = Session {
            child,
            stdin,
            lines,
            stderr,
            next_id: 1,
        };
        s.send(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}),
        );
        s
    }

    fn pid(&self) -> u32 {
        self.child.0.id()
    }

    /// The response line to one request, as text. Fails the test when none
    /// comes within [`TIMEOUT`].
    fn send_raw(&mut self, method: &str, params: Value) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(request(id, method, params).as_bytes())
            .unwrap();
        stdin.flush().unwrap();
        self.lines
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("no response to {method} within {TIMEOUT:?}: {e}"))
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        let line = self.send_raw(method, params);
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    fn attachment(&mut self, args: Value) -> (Value, String) {
        let line = self.send_raw(
            "tools/call",
            json!({"name": "get_attachment", "arguments": args}),
        );
        (serde_json::from_str(&line).unwrap(), line)
    }

    /// Close stdin, wait for the exit; the status and stderr.
    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let started = Instant::now();
        let status = loop {
            if let Some(s) = self.child.0.try_wait().unwrap() {
                break s;
            }
            assert!(started.elapsed() < TIMEOUT, "hxprobe mcp did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        let err = self.stderr.take().unwrap().join().unwrap();
        (status, String::from_utf8_lossy(&err).into_owned())
    }
}

/// The children of process `pid`, by `pgrep -P`.
fn children_of(pid: u32) -> String {
    let out = output(Command::new("/usr/bin/pgrep").args(["-P", &pid.to_string()]));
    // 0: some found; 1: none. Anything else is a failure of pgrep, not "no
    // children".
    let expected = if out.stdout.is_empty() { 1 } else { 0 };
    assert_eq!(
        out.status.code(),
        Some(expected),
        "pgrep: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A profile directory with the PDF `data` as the attachment
/// `report[3].pdf` of guid `[0x95; 16]`.
fn pdf_profile(name: &str, data: &[u8]) -> Dir {
    let d = Dir::new(name);
    std::fs::write(
        d.path("HxStore.hxd"),
        attachment_store(&[(
            [0x95; 16],
            "report[3].pdf",
            "Report",
            "application/pdf",
            data.len(),
        )]),
    )
    .unwrap();
    std::fs::create_dir_all(d.path(ATT_DIR)).unwrap();
    std::fs::write(d.path(ATT_DIR).join("report[3].pdf"), data).unwrap();
    d
}

#[test]
fn mcp_sends_a_pdf_as_text_and_bytes_through_the_real_child() {
    let pdf = one_page_pdf();
    let d = pdf_profile("pdf-whole-way", &pdf);
    let mut s = Session::start(&d);
    let g = hex(&[0x95; 16]);

    let (r, line) = s.attachment(json!({ "guid": g }));
    let items = r["result"]["content"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{r}");
    let v = tool_result(&r);
    assert_eq!(v["kind"], "pdf");
    assert_eq!(v["text"].as_str().unwrap().trim(), PDF_LINE, "{v}");
    assert_eq!(v["notes"], json!([]), "{v}");
    assert_eq!(items[1]["type"], "resource");
    assert_eq!(
        items[1]["resource"]["uri"],
        format!("hxprobe://attachment/{g}")
    );
    assert_eq!(items[1]["resource"]["mimeType"], "application/pdf");
    let blob = items[1]["resource"]["blob"].as_str().unwrap();
    assert_eq!(base64_decode(blob).unwrap(), pdf);
    assert_no_path(&line, &d.0);

    let (r, line) = s.attachment(json!({ "guid": g, "format": "text" }));
    assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1, "{r}");
    let v = tool_result(&r);
    assert_eq!(v["text"].as_str().unwrap().trim(), PDF_LINE, "{v}");
    assert_eq!(v["sent"], Value::Null);
    assert_no_path(&line, &d.0);

    let (status, _) = s.finish();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn mcp_leaves_no_pdf_child_and_logs_no_content() {
    let pdf = one_page_pdf();
    let d = pdf_profile("pdf-left-behind", &pdf);
    let mut s = Session::start(&d);
    let g = hex(&[0x95; 16]);
    for args in [
        json!({ "guid": g }),
        json!({ "guid": g, "format": "text" }),
        json!({ "guid": g, "offset": 5 }),
    ] {
        let (r, _) = s.attachment(args);
        assert_eq!(r["result"]["isError"], false, "{r}");
    }
    // Every child of the server was reaped before its call returned.
    assert_eq!(children_of(s.pid()), "");

    let (status, err) = s.finish();
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    for line in err.lines() {
        assert!(
            line.starts_with("hxprobe mcp: "),
            "not a line of the server: {line:?}"
        );
    }
    assert!(err.contains("pdf child ok"), "{err}");
    let lower = err.to_lowercase();
    for word in ["report[3]", "quarterly", "figures", "attached"] {
        assert!(!lower.contains(word), "{word:?} in stderr: {err}");
    }
}

#[test]
fn mcp_answers_on_after_a_damaged_pdf() {
    let mut bad = b"%PDF-1.7\n".to_vec();
    bad.extend((0..5000u32).map(|i| (i * 13) as u8));
    let d = pdf_profile("pdf-damaged", &bad);
    let mut s = Session::start(&d);
    let g = hex(&[0x95; 16]);

    let (r, _) = s.attachment(json!({ "guid": g }));
    let items = r["result"]["content"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{r}");
    let v = tool_result(&r);
    assert_eq!(v["text"], Value::Null, "{v}");
    assert_eq!(
        v["notes"],
        json!([
            "no text could be extracted from this file; its bytes are in the next content item"
        ])
    );
    let blob = items[1]["resource"]["blob"].as_str().unwrap();
    assert_eq!(base64_decode(blob).unwrap(), bad);

    // The next call is answered.
    let r = s.send(
        "tools/call",
        json!({"name": "list_attachments", "arguments": {"message_guid": hex(&ATT_MESSAGE)}}),
    );
    assert_eq!(tool_result(&r)["total"], 1, "{r}");
    assert_eq!(children_of(s.pid()), "");
    let (status, err) = s.finish();
    assert_eq!(status.code(), Some(0), "stderr: {err}");
    assert!(
        err.contains("pdf child exit") || err.contains("pdf child signal"),
        "{err}"
    );
}
