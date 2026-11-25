//! The text of a PDF attachment, made by a short-lived child process: this
//! program's own file started as `hxprobe pdf-text`. The PDF library never
//! runs inside the server; a crash, an exit status other than 0, a timeout
//! or too much output is "no text" for that one call.
//!
//! The child gets the bytes on its stdin and nothing else: no argument
//! beyond `pdf-text`, an empty environment, `/` as its working directory,
//! stderr discarded, no descriptor beyond its standard three, and a process
//! group of its own, which is killed whole when a limit is passed.

use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::mcp::client;

/// How long the child may take.
pub const PDF_DEADLINE: Duration = Duration::from_secs(20);
/// Most bytes of text read from the child.
pub const PDF_MAX_TEXT: usize = 16_000_000;

/// How the child ended, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Exit status 0.
    Ok,
    /// Another exit status.
    Exit,
    /// Ended by a signal.
    Signal,
    /// Killed at the deadline.
    Timeout,
    /// Killed after more than the allowed output.
    TooMuch,
    /// Could not be started.
    NotStarted,
}

impl Ended {
    /// The word for the log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Ended::Ok => "ok",
            Ended::Exit => "exit",
            Ended::Signal => "signal",
            Ended::Timeout => "timeout",
            Ended::TooMuch => "too much output",
            Ended::NotStarted => "not started",
        }
    }
}

/// The text of `bytes`, from `<program> pdf-text`; `None` with the way it
/// failed.
pub fn text_with(
    program: &Path,
    bytes: Arc<Vec<u8>>,
    deadline: Duration,
    max_text: usize,
) -> (Option<String>, Ended) {
    run(program, bytes, deadline, max_text, None)
}

/// [`text_with`]; `ends`, when given, gets the name of each helper thread
/// as it ends (tests).
fn run(
    program: &Path,
    bytes: Arc<Vec<u8>>,
    deadline: Duration,
    max_text: usize,
    ends: Option<mpsc::Sender<&'static str>>,
) -> (Option<String>, Ended) {
    let started = Instant::now();
    let mut cmd = Command::new(program);
    // No other argument, ever: the child reads only its stdin.
    cmd.arg("pdf-text")
        .env_clear()
        .current_dir("/")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    client::inherit_only_std(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return (None, Ended::NotStarted);
    };
    let pid = child.id();
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        stop(&mut child, pid);
        return (None, Ended::NotStarted);
    };

    // The helper threads are never joined: once the group is dead their
    // pipes close and they end on their own.
    let writer_end = ends.clone();
    thread::spawn(move || {
        let _end = Report(writer_end, "writer");
        // A child that stops reading gives a write error; nothing to do.
        let _ = stdin.write_all(&bytes);
        drop(stdin);
        drop(bytes);
    });
    let (tx, rx) = mpsc::channel();
    let limit = u64::try_from(max_text)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    thread::spawn(move || {
        let _end = Report(ends, "reader");
        let mut out = Vec::new();
        let _ = stdout.take(limit).read_to_end(&mut out);
        let _ = tx.send(out);
    });

    let Ok(out) = rx.recv_timeout(deadline.saturating_sub(started.elapsed())) else {
        stop(&mut child, pid);
        return (None, Ended::Timeout);
    };
    if out.len() > max_text {
        stop(&mut child, pid);
        return (None, Ended::TooMuch);
    }
    // The output ended; wait for the exit, still within the deadline.
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if started.elapsed() < deadline => thread::sleep(POLL),
            Ok(None) => {
                stop(&mut child, pid);
                return (None, Ended::Timeout);
            }
            // Reaped elsewhere: its group may not be ours to kill any more.
            Err(_) => return (None, Ended::Exit),
        }
    };
    if status.signal().is_some() {
        (None, Ended::Signal)
    } else if status.success() {
        (String::from_utf8(out).ok(), Ended::Ok)
    } else {
        (None, Ended::Exit)
    }
}

/// How often the exit of a child whose output ended is checked.
const POLL: Duration = Duration::from_millis(10);

/// Kill the group of the child `pid`, not yet reaped (so the id cannot have
/// been reused), then reap it, so no child is left behind.
fn stop(child: &mut Child, pid: u32) {
    if !client::kill_group(pid) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Sends its name over the channel when dropped: the end of a helper
/// thread, seen by tests.
struct Report(Option<mpsc::Sender<&'static str>>, &'static str);

impl Drop for Report {
    fn drop(&mut self) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(self.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::process_lock;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// A temp directory of the test, removed at the end.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Dir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxpdf-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(std::fs::canonicalize(&p).unwrap())
        }

        /// A stand-in `name` (mode 0700) running `body` under `/bin/sh`.
        /// It is run once with the argument `warm`, which ends it at once:
        /// the first start of a new file takes about 250 ms while the
        /// system checks it, which would eat a short deadline.
        fn stand_in(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(
                &p,
                format!("#!/bin/sh\n[ \"$1\" = warm ] && exit 0\n{body}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
            let warm = Command::new(&p)
                .arg("warm")
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(warm.success());
            p
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SECOND: Duration = Duration::from_secs(1);

    /// The printing stand-in: records its input, arguments, environment
    /// and working directory, then prints a fixed text.
    fn printing(d: &Dir) -> PathBuf {
        d.stand_in(
            "print",
            &format!(
                "/bin/cat > '{in_}'\n\
                 echo \"$#:$1\" > '{args}'\n\
                 /usr/bin/env > '{env}'\n\
                 pwd -P > '{cwd}'\n\
                 exec /usr/bin/printf 'text of the pdf'",
                in_ = d.file("in").display(),
                args = d.file("args").display(),
                env = d.file("env").display(),
                cwd = d.file("cwd").display(),
            ),
        )
    }

    /// [`run`] with a channel for the thread ends; asserts that both helper
    /// threads end within a second after it returned.
    fn run_ended(
        program: &Path,
        bytes: Arc<Vec<u8>>,
        deadline: Duration,
        max_text: usize,
    ) -> (Option<String>, Ended) {
        let (tx, rx) = mpsc::channel();
        let r = run(program, bytes, deadline, max_text, Some(tx));
        let mut ended = vec![
            rx.recv_timeout(SECOND)
                .expect("a helper thread did not end"),
            rx.recv_timeout(SECOND)
                .expect("a helper thread did not end"),
        ];
        ended.sort_unstable();
        assert_eq!(ended, ["reader", "writer"]);
        r
    }

    /// Whether process `pid` is gone.
    fn gone(pid: libc::pid_t) -> bool {
        // SAFETY: kill() with signal 0 only checks that `pid` exists.
        let r = unsafe { libc::kill(pid, 0) };
        r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn pid_in(path: &Path) -> libc::pid_t {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn printing_stand_in_gives_its_text_and_gets_only_the_bytes() {
        let _p = process_lock();
        let d = Dir::new();
        let prog = printing(&d);
        let bytes: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        let r = run_ended(&prog, Arc::new(bytes.clone()), PDF_DEADLINE, PDF_MAX_TEXT);
        assert_eq!(r, (Some("text of the pdf".into()), Ended::Ok));
        assert_eq!(std::fs::read(d.file("in")).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(d.file("args")).unwrap(),
            "1:pdf-text\n"
        );
        let env = std::fs::read_to_string(d.file("env")).unwrap();
        assert!(!env.lines().any(|l| l.starts_with("HOME=")), "{env}");
        assert!(!env.lines().any(|l| l.starts_with("PATH=")), "{env}");
        assert_eq!(std::fs::read_to_string(d.file("cwd")).unwrap(), "/\n");
    }

    #[test]
    fn failed_ends_give_no_text() {
        let _p = process_lock();
        let d = Dir::new();
        let bytes = Arc::new(b"%PDF-1.4".to_vec());
        let exit3 = d.stand_in("exit3", "/bin/cat > /dev/null\nexit 3");
        assert_eq!(
            run_ended(&exit3, bytes.clone(), PDF_DEADLINE, PDF_MAX_TEXT),
            (None, Ended::Exit)
        );
        let signal = d.stand_in("signal", "exec /bin/kill -KILL $$");
        assert_eq!(
            run_ended(&signal, bytes.clone(), PDF_DEADLINE, PDF_MAX_TEXT),
            (None, Ended::Signal)
        );
        let not_utf8 = d.stand_in(
            "bad",
            "/bin/cat > /dev/null\nexec /usr/bin/printf '\\377\\376'",
        );
        assert_eq!(
            run_ended(&not_utf8, bytes.clone(), PDF_DEADLINE, PDF_MAX_TEXT).0,
            None
        );
        let noise = d.stand_in(
            "noise",
            "/bin/cat > /dev/null\necho noise on stdout\necho noise on stderr >&2\nexit 1",
        );
        assert_eq!(
            run_ended(&noise, bytes.clone(), PDF_DEADLINE, PDF_MAX_TEXT),
            (None, Ended::Exit)
        );
        assert_eq!(
            text_with(
                &d.file("no-such-program"),
                bytes,
                PDF_DEADLINE,
                PDF_MAX_TEXT
            ),
            (None, Ended::NotStarted)
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let _p = process_lock();
        let d = Dir::new();
        let pid_file = d.file("pid");
        let prog = d.stand_in(
            "hang",
            &format!("echo $$ > '{}'\nexec /bin/sleep 60", pid_file.display()),
        );
        let started = Instant::now();
        let r = run_ended(
            &prog,
            Arc::new(b"%PDF-".to_vec()),
            Duration::from_millis(300),
            PDF_MAX_TEXT,
        );
        assert_eq!(r, (None, Ended::Timeout));
        assert!(started.elapsed() < SECOND + Duration::from_millis(300));
        assert!(gone(pid_in(&pid_file)), "the child is still running");
    }

    #[test]
    fn a_child_that_never_reads_does_not_block_the_caller() {
        let _p = process_lock();
        let d = Dir::new();
        let prog = d.stand_in("deaf", "exec /bin/sleep 60");
        let started = Instant::now();
        let r = run_ended(
            &prog,
            Arc::new(vec![b'x'; 5_000_000]),
            Duration::from_millis(300),
            PDF_MAX_TEXT,
        );
        assert_eq!(r, (None, Ended::Timeout));
        assert!(started.elapsed() < SECOND + Duration::from_millis(300));
    }

    #[test]
    fn too_much_output_is_killed() {
        let _p = process_lock();
        let d = Dir::new();
        let pid_file = d.file("pid");
        let prog = d.stand_in(
            "yes",
            &format!("echo $$ > '{}'\nexec /usr/bin/yes", pid_file.display()),
        );
        let started = Instant::now();
        let r = run_ended(&prog, Arc::new(b"%PDF-".to_vec()), PDF_DEADLINE, 64 * 1024);
        assert_eq!(r, (None, Ended::TooMuch));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(gone(pid_in(&pid_file)), "the child is still running");
    }

    #[test]
    fn the_bytes_are_shared_not_copied() {
        let _p = process_lock();
        let d = Dir::new();
        let prog = printing(&d);
        let bytes = Arc::new(vec![7u8; 1_000_000]);
        let r = text_with(&prog, Arc::clone(&bytes), PDF_DEADLINE, PDF_MAX_TEXT);
        assert_eq!(r.1, Ended::Ok);
        let started = Instant::now();
        while Arc::strong_count(&bytes) != 1 {
            assert!(started.elapsed() < SECOND, "the writer kept the bytes");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn ended_words_for_the_log() {
        assert_eq!(Ended::Ok.as_str(), "ok");
        assert_eq!(Ended::TooMuch.as_str(), "too much output");
    }
}
