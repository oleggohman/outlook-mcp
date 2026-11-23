//! The MCP server's private cache: a copy of the store exported to SQLite and
//! prepared for queries (`mail.db` in a directory only the owner can read).
//!
//! Layout of the directory (mode 0700, owned by the current user):
//! - `lock`: held with an exclusive `flock` for the whole of a build, so two
//!   servers sharing the directory build one after the other;
//! - `store.copy-<pid>`: the private copy of the store during a build;
//! - `mail.db.tmp-<pid>`: the database during a build;
//! - `mail.db`: the finished database.
//!
//! Every file is mode 0600. A failed build removes its temporary files and
//! leaves an existing `mail.db` untouched.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use memmap2::Mmap;
use rusqlite::{params, Connection};

use crate::export::{self, ExportStats};
use crate::header;
use crate::item;
use crate::kv::Store;
use crate::mcp::text::{cap, html_to_text, lossy};
use crate::props_gen::TYPES;

/// Name of the prepared database in the cache directory.
pub const DB_NAME: &str = "mail.db";
const LOCK_NAME: &str = "lock";
const COPY_PREFIX: &str = "store.copy-";
const TMP_PREFIX: &str = "mail.db.tmp-";

/// Attempts at copying a store that changes while it is copied.
const COPY_ATTEMPTS: usize = 3;

/// Longest body text put in the search index, in chars.
const MAX_FTS_BODY: usize = 200_000;

/// Type tables the queries read that the export creates only when the store
/// holds such an object.
const OPTIONAL_TABLES: [&str; 2] = ["AttachmentHeader", "AppointmentHeader"];

const PREPARE_SQL: &str = "
CREATE INDEX mcp_message_sent ON \"MessageHeader\"(\"SentTime\");
CREATE INDEX mcp_message_view ON \"MessageHeader\"(\"View\");
CREATE INDEX mcp_attachment_message ON \"AttachmentHeader\"(\"MessageHeader\");
CREATE TABLE mcp_events(guid TEXT PRIMARY KEY, start_unix INTEGER, end_unix INTEGER);
CREATE VIRTUAL TABLE mcp_fts USING fts5(guid UNINDEXED, subject, sender, preview, body,
                                        tokenize = 'unicode61');
CREATE TABLE mcp_meta(key TEXT PRIMARY KEY, value TEXT);
";

/// What one successful [`Cache::build`] did.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Length of the live store when it was copied.
    pub store_len: u64,
    /// Modification time of the live store when it was copied, in ns.
    pub store_mtime_ns: i64,
    pub stats: ExportStats,
    pub copy_ms: u64,
    pub export_ms: u64,
    pub prepare_ms: u64,
}

/// The cache directory of one store.
#[derive(Debug)]
pub struct Cache {
    dir: PathBuf,
    store: PathBuf,
    /// Number of [`Cache::build`] calls, failed ones included.
    pub builds: u64,
}

impl Cache {
    /// The cache for `store` in `dir`. Creates `dir` if needed and checks it
    /// (see [`ensure_dir`]); builds nothing.
    pub fn new(dir: &Path, store: &Path) -> Result<Cache, String> {
        ensure_dir(dir)?;
        Ok(Cache {
            dir: dir.to_path_buf(),
            store: store.to_path_buf(),
            builds: 0,
        })
    }

    /// Path of the prepared database.
    pub fn db_path(&self) -> PathBuf {
        self.dir.join(DB_NAME)
    }

    /// Path of the live store.
    pub fn store_path(&self) -> &Path {
        &self.store
    }

    /// Copy the store, export the copy and prepare it, then put the result
    /// in place as `mail.db`. `now` (unix seconds) sets the export's tick
    /// window and `built_unix`.
    ///
    /// Holds the directory's lock throughout. Any error removes the
    /// temporary files and leaves an existing `mail.db` as it was.
    pub fn build(&mut self, now: i64) -> Result<BuildInfo, String> {
        self.builds += 1;
        ensure_dir(&self.dir)?;
        let _lock = lock(&self.dir)?;
        remove_leftovers(&self.dir);

        let pid = std::process::id();
        let copy_path = self.dir.join(format!("{COPY_PREFIX}{pid}"));
        let tmp_path = self.dir.join(format!("{TMP_PREFIX}{pid}"));
        let _temps = TempFiles(vec![copy_path.clone(), tmp_path.clone()]);

        let started = Instant::now();
        let (store_len, store_mtime_ns) = copy_store(&self.store, &copy_path)?;
        let copy_ms = ms_since(started);

        let at = Instant::now();
        let file = File::open(&copy_path).map_err(|e| format!("open the store copy: {e}"))?;
        // SAFETY: the copy is private to this build (owner-only directory,
        // held lock, per-process name), so nothing changes it while mapped.
        let mm = unsafe { Mmap::map(&file) }.map_err(|e| format!("map the store copy: {e}"))?;
        header::check(&mm).map_err(|_| {
            format!(
                "{} is not a store file (no HxStore header)",
                self.store.display()
            )
        })?;
        let store = Store::load(&mm);
        create_private(&tmp_path).map_err(|e| format!("create {}: {e}", tmp_path.display()))?;
        let conn = Connection::open(&tmp_path).map_err(|e| format!("open the database: {e}"))?;
        // The work file is thrown away on any failure, so it needs no
        // journal; sorts stay in memory instead of spilling to `$TMPDIR`.
        conn.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY;",
        )
        .map_err(|e| format!("configure the database: {e}"))?;
        let window = item::tick_window_at(now.max(0) as u64);
        let stats =
            export::export(&store, &mm, &conn, window).map_err(|e| format!("export: {e}"))?;
        drop(mm);
        drop(file);
        let export_ms = ms_since(at);

        let at = Instant::now();
        let counts = prepare(&conn, now).map_err(|e| format!("prepare: {e}"))?;
        let prepare_ms = ms_since(at);

        let info = BuildInfo {
            store_len,
            store_mtime_ns,
            stats,
            copy_ms,
            export_ms,
            prepare_ms,
        };
        write_meta(&conn, &meta_entries(now, &self.store, &info))
            .map_err(|e| format!("write build info: {e}"))?;
        conn.close()
            .map_err(|(_, e)| format!("close the database: {e}"))?;
        std::fs::rename(&tmp_path, self.db_path())
            .map_err(|e| format!("replace {}: {e}", self.db_path().display()))?;

        eprintln!(
            "hxprobe mcp: cache built in {} ms (copy {copy_ms}, export {export_ms}, prepare \
             {prepare_ms}); {} objects, {} messages indexed, {} events",
            ms_since(started),
            info.stats.objects,
            counts.messages,
            counts.events
        );
        Ok(info)
    }
}

/// Milliseconds since `t`.
fn ms_since(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

/// Make `dir` a usable cache directory: create it (mode 0700; missing
/// parents with default modes) if it does not exist, then accept it only if
/// it is a real directory owned by the current user. A mode wider than 0700
/// is tightened to 0700.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    match std::fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            match DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                // Created by someone else meanwhile: checked below.
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("create cache directory {shown}: {e}")),
            }
        }
        Err(e) => return Err(format!("cache directory {shown}: {e}")),
    }
    let m = std::fs::symlink_metadata(dir).map_err(|e| format!("cache directory {shown}: {e}"))?;
    if m.file_type().is_symlink() {
        return Err(format!(
            "cache directory {shown} is a symbolic link; refusing to use it"
        ));
    }
    if !m.is_dir() {
        return Err(format!("cache directory {shown} is not a directory"));
    }
    // SAFETY: getuid() has no preconditions and cannot fail.
    if m.uid() != unsafe { libc::getuid() } {
        return Err(format!(
            "cache directory {shown} is owned by another user; refusing to use it"
        ));
    }
    if m.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("restrict cache directory {shown}: {e}"))?;
    }
    Ok(())
}

/// The open `lock` file of `dir`, locked exclusively; the lock is released
/// when the file is closed.
fn lock(dir: &Path) -> Result<File, String> {
    let path = dir.join(LOCK_NAME);
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    loop {
        // SAFETY: flock() on a descriptor owned by `f` for the whole call.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != ErrorKind::Interrupted {
            return Err(format!("lock {}: {e}", path.display()));
        }
    }
}

/// Remove the temporary files of earlier builds (and their SQLite side
/// files). Called with the lock held, so no build is using them.
fn remove_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(COPY_PREFIX) || name.starts_with(TMP_PREFIX);
        if ours && e.file_type().is_ok_and(|t| !t.is_dir()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Files removed (with their SQLite side files) when dropped.
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let mut name = p.as_os_str().to_owned();
                name.push(suffix);
                let _ = std::fs::remove_file(name);
            }
        }
    }
}

/// Create the empty file `path`, mode 0600; fails if it exists.
fn create_private(path: &Path) -> std::io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(drop)
}

/// `(len, mtime in ns)` of `path`.
fn stamp(path: &Path) -> std::io::Result<(u64, i64)> {
    let m = std::fs::metadata(path)?;
    Ok((
        m.len(),
        m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
    ))
}

/// Copy the store `src` to `dst` (mode 0600) and return the source's
/// `(len, mtime in ns)`. The copy is accepted only when the source looked
/// the same before and after it; otherwise it is retried, up to
/// [`COPY_ATTEMPTS`] times in all.
fn copy_store(src: &Path, dst: &Path) -> Result<(u64, i64), String> {
    let shown = src.display();
    for _ in 0..COPY_ATTEMPTS {
        let before = stamp(src).map_err(|e| format!("store {shown}: {e}"))?;
        let _ = std::fs::remove_file(dst);
        std::fs::copy(src, dst).map_err(|e| format!("copy the store {shown}: {e}"))?;
        // The copy takes the source's mode.
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("restrict the store copy: {e}"))?;
        let after = stamp(src).map_err(|e| format!("store {shown}: {e}"))?;
        let copied = std::fs::metadata(dst)
            .map_err(|e| format!("store copy: {e}"))?
            .len();
        if before == after && copied == before.0 {
            return Ok(before);
        }
    }
    Err("the store kept changing during the copy".into())
}

/// Rows written by [`prepare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareCounts {
    /// Rows of `mcp_fts` (one per message).
    pub messages: usize,
    /// Rows of `mcp_events`.
    pub events: usize,
}

/// Add what the server's queries need to a fresh export, in one
/// transaction: the optional type tables when missing, indexes, the event
/// times (`mcp_events`), the full-text index (`mcp_fts`) and an empty
/// `mcp_meta`. `now` sets the tick window that event times must fall in.
pub fn prepare(conn: &Connection, now: i64) -> rusqlite::Result<PrepareCounts> {
    let tx = conn.unchecked_transaction()?;
    for name in OPTIONAL_TABLES {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |r| r.get(0),
        )?;
        if !exists {
            let t = TYPES
                .iter()
                .find(|t| t.name == name)
                .expect("every optional table has a schema");
            tx.execute_batch(&export::type_table_sql(t))?;
        }
    }
    tx.execute_batch(PREPARE_SQL)?;
    let events = fill_events(&tx, item::tick_window_at(now.max(0) as u64))?;
    tx.execute_batch("CREATE INDEX mcp_events_start ON mcp_events(start_unix)")?;
    let messages = fill_fts(&tx)?;
    tx.commit()?;
    Ok(PrepareCounts { messages, events })
}

/// `(start, end)` unix seconds of a `TimeRangeUtc` value: 32 hex chars, two
/// little-endian tick values. `None` when malformed, when either tick is
/// outside `window` or when the end is before the start.
fn time_range(hex: &str, window: (u64, u64)) -> Option<(i64, i64)> {
    let b = hex.as_bytes();
    if b.len() != 32 || !b.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        // All ASCII (checked above), so any byte index is a char boundary.
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    let start = u64::from_le_bytes(bytes[..8].try_into().ok()?);
    let end = u64::from_le_bytes(bytes[8..].try_into().ok()?);
    let ok = |t: u64| (window.0..=window.1).contains(&t);
    (ok(start) && ok(end) && end >= start)
        .then(|| (item::ticks_to_unix(start), item::ticks_to_unix(end)))
}

/// Fill `mcp_events` from `"AppointmentHeader".TimeRangeUtc`.
fn fill_events(conn: &Connection, window: (u64, u64)) -> rusqlite::Result<usize> {
    let mut sel = conn.prepare("SELECT guid, \"TimeRangeUtc\" FROM \"AppointmentHeader\"")?;
    let mut ins = conn.prepare(
        "INSERT OR IGNORE INTO mcp_events(guid, start_unix, end_unix) VALUES (?1, ?2, ?3)",
    )?;
    let mut rows = sel.query([])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        let (Some(guid), Some(range)) = (lossy(r.get_ref(0)?), lossy(r.get_ref(1)?)) else {
            continue;
        };
        if let Some((start, end)) = time_range(&range, window) {
            n += ins.execute(params![guid, start, end])?;
        }
    }
    Ok(n)
}

/// Fill `mcp_fts` with one row per message: subject, sender (name and
/// address), preview and the body as text (HTML body, else the first body
/// part), capped at [`MAX_FTS_BODY`] chars.
fn fill_fts(conn: &Connection) -> rusqlite::Result<usize> {
    let mut sel = conn.prepare(
        "SELECT guid, Subject, SenderDisplayName, SenderEmailAddress, Preview, body_html,
                body_first
         FROM messages",
    )?;
    let mut ins = conn.prepare(
        "INSERT INTO mcp_fts(guid, subject, sender, preview, body) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut rows = sel.query([])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        let text = |i: usize| -> rusqlite::Result<Option<String>> { Ok(lossy(r.get_ref(i)?)) };
        let sender: Vec<String> = [text(2)?, text(3)?]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();
        let body = match text(5)? {
            Some(b) => Some(b),
            None => text(6)?,
        };
        let body = body.map_or_else(String::new, |b| cap(&html_to_text(&b), MAX_FTS_BODY).0);
        ins.execute(params![
            text(0)?,
            text(1)?,
            sender.join(" "),
            text(4)?,
            body
        ])?;
        n += 1;
    }
    Ok(n)
}

/// The `mcp_meta` rows of a build.
fn meta_entries(now: i64, store: &Path, info: &BuildInfo) -> Vec<(&'static str, String)> {
    let s = &info.stats;
    vec![
        ("built_unix", now.to_string()),
        ("store_len", info.store_len.to_string()),
        ("store_mtime_ns", info.store_mtime_ns.to_string()),
        ("store_path", store.to_string_lossy().into_owned()),
        ("values", s.values.to_string()),
        ("objects", s.objects.to_string()),
        ("schema_objects", s.schema_objects.to_string()),
        ("generic_objects", s.generic_objects.to_string()),
        ("duplicates", s.duplicates.to_string()),
        ("superseded", s.superseded.to_string()),
        ("fixed_len_mismatch", s.fixed_len_mismatch.to_string()),
        ("no_guid", s.no_guid.to_string()),
        ("fields", s.fields.to_string()),
        ("collections", s.collections.to_string()),
        ("skipped_items", s.skipped_items.to_string()),
        ("skipped_fields", s.skipped_fields.to_string()),
        ("copy_ms", info.copy_ms.to_string()),
        ("export_ms", info.export_ms.to_string()),
        ("prepare_ms", info.prepare_ms.to_string()),
    ]
}

/// Write `entries` to `mcp_meta`.
fn write_meta(conn: &Connection, entries: &[(&str, String)]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut ins = tx.prepare("INSERT OR REPLACE INTO mcp_meta(key, value) VALUES (?1, ?2)")?;
        for (k, v) in entries {
            ins.execute(params![k, v])?;
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::guid_hex;
    use crate::testutil::{
        mail_store, mail_store_without_calendar, MAIL_EVENTS, MAIL_EVENT_TIMES, MAIL_MSGS, NOW,
    };
    use rusqlite::OpenFlags;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW_I: i64 = NOW as i64;

    /// A fresh directory under the system temp dir, removed on drop (also
    /// when the test fails).
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> TestDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "hxprobe-cache-test-{}-{}-{name}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }

        /// A store file holding `bytes`.
        fn store(&self, bytes: &[u8]) -> PathBuf {
            let p = self.0.join("HxStore.hxd");
            std::fs::write(&p, bytes).unwrap();
            p
        }

        fn cache_dir(&self) -> PathBuf {
            self.0.join("cache")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Names of the files in `dir`, sorted.
    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn open(p: &Path) -> Connection {
        Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// A cache in `t` built from `store`.
    fn built(t: &TestDir, store: &[u8]) -> (Cache, BuildInfo) {
        let mut c = Cache::new(&t.cache_dir(), &t.store(store)).unwrap();
        let info = c.build(NOW_I).unwrap();
        (c, info)
    }

    #[test]
    fn build_creates_private_dir_and_database() {
        let t = TestDir::new("build");
        let (c, info) = built(&t, &mail_store());
        assert_eq!(mode(&t.cache_dir()), 0o700);
        assert_eq!(mode(&c.db_path()), 0o600);
        assert_eq!(mode(&t.cache_dir().join("lock")), 0o600);
        assert_eq!(files(&t.cache_dir()), ["lock", "mail.db"]);
        assert_eq!(c.builds, 1);
        assert_eq!(info.store_len, mail_store().len() as u64);
        assert!(info.stats.schema_objects > 0);
    }

    #[test]
    fn build_fills_fts_events_indexes_and_meta() {
        let t = TestDir::new("content");
        let (c, info) = built(&t, &mail_store());
        let db = open(&c.db_path());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 4);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM mcp_fts"), 4);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM \"AttachmentHeader\""), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM \"AppointmentHeader\""), 2);

        let events: Vec<(String, i64, i64)> = db
            .prepare("SELECT guid, start_unix, end_unix FROM mcp_events ORDER BY start_unix")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let want: Vec<(String, i64, i64)> = MAIL_EVENTS
            .iter()
            .zip(MAIL_EVENT_TIMES)
            .map(|(g, (s, e))| (guid_hex(g), s, e))
            .collect();
        assert_eq!(events, want);

        let indexes: Vec<String> = db
            .prepare(
                "SELECT tbl_name || '(' || name || ')' FROM sqlite_master
                 WHERE type = 'index' AND name LIKE 'mcp_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            indexes,
            [
                "AttachmentHeader(mcp_attachment_message)",
                "mcp_events(mcp_events_start)",
                "MessageHeader(mcp_message_sent)",
                "MessageHeader(mcp_message_view)",
            ]
        );

        let meta = |k: &str| -> String {
            db.query_row("SELECT value FROM mcp_meta WHERE key = ?1", [k], |r| {
                r.get(0)
            })
            .unwrap_or_else(|_| panic!("no meta key {k}"))
        };
        assert_eq!(meta("built_unix"), NOW_I.to_string());
        assert_eq!(meta("store_len"), info.store_len.to_string());
        assert_eq!(meta("store_mtime_ns"), info.store_mtime_ns.to_string());
        assert_eq!(meta("store_path"), c.store_path().to_string_lossy());
        assert_eq!(
            meta("schema_objects"),
            info.stats.schema_objects.to_string()
        );
        for k in [
            "values",
            "objects",
            "generic_objects",
            "duplicates",
            "superseded",
            "fixed_len_mismatch",
            "no_guid",
            "fields",
            "collections",
            "skipped_items",
            "skipped_fields",
            "copy_ms",
            "export_ms",
            "prepare_ms",
        ] {
            assert!(meta(k).parse::<u64>().is_ok(), "{k}");
        }
    }

    #[test]
    fn fts_rows_hold_text_bodies() {
        let t = TestDir::new("fts");
        let (c, _) = built(&t, &mail_store());
        let db = open(&c.db_path());
        let fts_guids = |q: &str| -> Vec<String> {
            let mut st = db
                .prepare("SELECT guid FROM mcp_fts WHERE mcp_fts MATCH ?1 ORDER BY guid")
                .unwrap();
            st.query_map([q], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        // Body only, HTML converted.
        assert_eq!(fts_guids("body:attached"), [guid_hex(&MAIL_MSGS[0])]);
        // The binary body is read lossily.
        assert_eq!(fts_guids("body:zebra"), [guid_hex(&MAIL_MSGS[2])]);
        assert_eq!(fts_guids("subject:שלום"), [guid_hex(&MAIL_MSGS[1])]);
        assert_eq!(fts_guids("body:בבוקר"), [guid_hex(&MAIL_MSGS[1])]);
        assert_eq!(
            fts_guids("sender:\"bob@example.org\""),
            [guid_hex(&MAIL_MSGS[2])]
        );
        assert_eq!(fts_guids("sender:Roe"), [guid_hex(&MAIL_MSGS[2])]);
        assert_eq!(fts_guids("preview:budget"), [guid_hex(&MAIL_MSGS[0])]);
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM mcp_fts WHERE guid = ?1",
                [guid_hex(&MAIL_MSGS[0])],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body.as_deref(), Some("The budget numbers are attached."));
        // The message without MessageData has a row with an empty body.
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM mcp_fts WHERE guid = ?1",
                [guid_hex(&MAIL_MSGS[3])],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.unwrap_or_default().is_empty());
    }

    #[test]
    fn store_without_calendar_gets_empty_tables() {
        let t = TestDir::new("nocal");
        let (c, _) = built(&t, &mail_store_without_calendar());
        let db = open(&c.db_path());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM \"AttachmentHeader\""), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM \"AppointmentHeader\""), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM mcp_events"), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM mcp_fts"), 4);
    }

    #[test]
    fn non_store_file_fails_and_keeps_the_old_database() {
        let t = TestDir::new("bad");
        let (mut c, _) = built(&t, &mail_store());
        let before = std::fs::read(c.db_path()).unwrap();
        t.store(b"this is not a store file, only some text that is long enough");
        let e = c.build(NOW_I + 60).unwrap_err();
        assert!(e.contains("store"), "{e}");
        assert_eq!(c.builds, 2);
        assert_eq!(std::fs::read(c.db_path()).unwrap(), before);
        assert_eq!(files(&t.cache_dir()), ["lock", "mail.db"]);
    }

    #[test]
    fn missing_store_fails_without_leftovers() {
        let t = TestDir::new("missing");
        let mut c = Cache::new(&t.cache_dir(), &t.0.join("nope.hxd")).unwrap();
        assert!(c.build(NOW_I).is_err());
        assert_eq!(files(&t.cache_dir()), ["lock"]);
    }

    #[test]
    fn leftovers_of_a_dead_run_are_removed() {
        let t = TestDir::new("stale");
        let dir = t.cache_dir();
        let mut c = Cache::new(&dir, &t.store(&mail_store())).unwrap();
        for f in [
            "store.copy-999",
            "mail.db.tmp-999",
            "mail.db.tmp-999-journal",
            "mail.db.tmp-999-wal",
            "mail.db.tmp-999-shm",
        ] {
            std::fs::write(dir.join(f), b"stale").unwrap();
        }
        // Not ours: kept.
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        c.build(NOW_I).unwrap();
        assert_eq!(files(&dir), ["lock", "mail.db", "notes.txt"]);
    }

    #[test]
    fn symlink_and_regular_file_are_refused() {
        let t = TestDir::new("refuse");
        let store = t.store(&mail_store());
        let real = t.0.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = Cache::new(&link, &store).unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        let file = t.0.join("file");
        std::fs::write(&file, b"x").unwrap();
        let e = Cache::new(&file, &store).unwrap_err();
        assert!(e.contains("not a directory"), "{e}");
    }

    #[test]
    fn wide_directory_is_tightened() {
        let t = TestDir::new("wide");
        let dir = t.cache_dir();
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        Cache::new(&dir, &t.store(&mail_store())).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn missing_parents_are_created() {
        let t = TestDir::new("parents");
        let dir = t.0.join("a").join("b").join("cache");
        let mut c = Cache::new(&dir, &t.store(&mail_store())).unwrap();
        c.build(NOW_I).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert!(c.db_path().is_file());
    }

    #[test]
    fn rebuild_replaces_the_database() {
        let t = TestDir::new("rebuild");
        let (mut c, _) = built(&t, &mail_store_without_calendar());
        t.store(&mail_store());
        c.build(NOW_I + 60).unwrap();
        assert_eq!(c.builds, 2);
        let db = open(&c.db_path());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM mcp_events"), 2);
        assert_eq!(files(&t.cache_dir()), ["lock", "mail.db"]);
    }

    #[test]
    fn events_outside_the_window_or_reversed_are_skipped() {
        let conn = Connection::open_in_memory().unwrap();
        let mm = mail_store_without_calendar();
        crate::export::export(
            &crate::kv::Store::load(&mm),
            &mm,
            &conn,
            crate::item::tick_window_at(NOW),
        )
        .unwrap();
        let ticks = |s: i64| (crate::item::UNIX_EPOCH_TICKS as i64 + s * 10_000_000) as u64;
        let hex = |a: u64, b: u64| -> String {
            let mut v = a.to_le_bytes().to_vec();
            v.extend_from_slice(&b.to_le_bytes());
            guid_hex(&v.try_into().unwrap())
        };
        prepare_tables(&conn);
        let rows: [(&str, Option<String>); 6] = [
            ("aa", Some(hex(ticks(NOW_I), ticks(NOW_I + 3600)))),
            ("bb", Some(hex(ticks(NOW_I + 3600), ticks(NOW_I)))),
            ("cc", Some(hex(0, ticks(NOW_I)))),
            ("dd", Some(hex(ticks(NOW_I), u64::MAX))),
            ("ee", Some("xyz".into())),
            ("ff", None),
        ];
        for (g, v) in &rows {
            conn.execute(
                "INSERT INTO \"AppointmentHeader\"(guid, TimeRangeUtc) VALUES (?1, ?2)",
                rusqlite::params![g, v],
            )
            .unwrap();
        }
        // A BLOB of the same text is accepted too.
        conn.execute(
            "INSERT INTO \"AppointmentHeader\"(guid, TimeRangeUtc) VALUES ('gg', CAST(?1 AS BLOB))",
            [hex(ticks(NOW_I), ticks(NOW_I))],
        )
        .unwrap();
        prepare(&conn, NOW_I).unwrap();
        let got: Vec<String> = conn
            .prepare("SELECT guid FROM mcp_events ORDER BY guid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, ["aa", "gg"]);
    }

    /// Create the optional type tables the way [`prepare`] does, so a test
    /// can insert rows before calling it.
    fn prepare_tables(conn: &Connection) {
        let t = crate::props_gen::TYPES
            .iter()
            .find(|t| t.name == "AppointmentHeader")
            .unwrap();
        conn.execute_batch(&crate::export::type_table_sql(t))
            .unwrap();
    }
}
