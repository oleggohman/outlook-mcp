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
//! leaves an existing `mail.db` untouched. An existing `mail.db` or `lock`
//! is used only when it is a regular file owned by the current user (see
//! [`check_private`]).

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use memmap2::Mmap;
use rusqlite::{params, Connection, OpenFlags};

use crate::export::{self, ExportStats};
use crate::header;
use crate::item;
use crate::kv::Store;
use crate::mcp::log;
use crate::mcp::proto::{self, Refresher, ServiceStatus};
use crate::mcp::text::{cap, html_to_text, iso, lossy};
use crate::props_gen::TYPES;

/// Name of the prepared database in the cache directory.
pub const DB_NAME: &str = "mail.db";
const LOCK_NAME: &str = "lock";
const COPY_PREFIX: &str = "store.copy-";
const TMP_PREFIX: &str = "mail.db.tmp-";

/// Version of what [`prepare`] adds to the export; bump it whenever that
/// changes, so databases of an older version are rebuilt.
pub const FORMAT_VERSION: &str = "3";

/// Default shortest time between two builds of a changed store, in seconds.
pub const DEFAULT_REFRESH_SECS: u64 = 900;

/// Name of the store file in a profile directory.
const STORE_NAME: &str = "HxStore.hxd";

/// Outlook's profiles directory, relative to `$HOME`.
const PROFILES_DIR: &str = "Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles";

/// Attempts at copying a store that changes while it is copied.
const COPY_ATTEMPTS: usize = 3;

/// How long [`remove_dead_leftovers`] tries a held build lock before it
/// gives up, and how often: a process that just exited may still hold it
/// for a moment.
const LEFTOVER_LOCK_RETRY: Duration = Duration::from_millis(250);
const LEFTOVER_LOCK_POLL: Duration = Duration::from_millis(10);

/// `last_error` while a build runs; it stays when the build panics.
pub(crate) const BUILD_UNFINISHED: &str = "the last cache build did not finish (internal error)";

/// Error of a call in service mode that found no database and waited
/// [`proto::FIRST_BUILD_WAIT`] for the service's first build in vain.
pub const FIRST_BUILD_PENDING: &str =
    "the mail cache is being built for the first time; call again in a few seconds";

/// Error of a call in service mode when `mail.db` has another format
/// version and the service is newer than this server.
pub const OLDER_SERVER: &str = "this server is older than the mail cache; restart the MCP client";

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

/// The `mcp_meta` rows of a database, by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta(BTreeMap<String, String>);

impl Meta {
    /// The value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The value of `key` as an integer.
    pub fn int(&self, key: &str) -> Option<i64> {
        self.get(key)?.parse().ok()
    }

    /// `(store_len, store_mtime_ns)` of the store the database was built from.
    fn stamp(&self) -> Option<(u64, i64)> {
        Some((
            self.get("store_len")?.parse().ok()?,
            self.int("store_mtime_ns")?,
        ))
    }
}

/// The state of a [`Cache`], for the `store_status` tool.
#[derive(Debug)]
pub struct Status<'a> {
    pub store_path: &'a Path,
    /// `mcp_meta` of the database in use; `None` when there is none.
    pub meta: Option<&'a Meta>,
    /// Whether the live store differs from the one the database in use was
    /// built from; `None` when there is no database.
    pub store_changed: Option<bool>,
    /// Error of the last build attempt, if it failed. In service mode: the
    /// service's `last_error`, else why the service is not available.
    pub last_error: Option<&'a str>,
    pub last_attempt_unix: Option<i64>,
    /// The refresh service as the last request found it; `None` when the
    /// cache builds by itself (no refresher).
    pub service: Option<&'a ServiceView>,
}

/// The refresh service as a [`Cache`] in service mode last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceView {
    /// The service answered the last request.
    pub running: bool,
    pub building: bool,
    pub version: Option<String>,
    pub pid: Option<u32>,
    /// The service's `last_error`, or why it is not available.
    pub last_error: Option<String>,
    pub last_attempt_unix: Option<i64>,
}

impl ServiceView {
    /// The view after a request that got `reply`.
    fn of(reply: &Result<ServiceStatus, String>) -> ServiceView {
        match reply {
            Ok(st) => ServiceView {
                running: true,
                building: st.building,
                version: Some(st.version.clone()),
                pid: Some(st.pid),
                // The service sets this text when a build starts; it is an
                // error only when the build is over (one that broke off).
                last_error: st
                    .last_error
                    .clone()
                    .filter(|e| !(st.building && e == BUILD_UNFINISHED)),
                last_attempt_unix: st.last_attempt_unix,
            },
            Err(e) => ServiceView {
                last_error: Some(e.clone()),
                ..ServiceView::default()
            },
        }
    }
}

/// The cache directory of one store, and the database in use.
///
/// Refresh policy (see [`Cache::connection_at`]): the database is rebuilt
/// lazily, when the live store changed and the last build (or attempt) is at
/// least `refresh_secs` old. A failed build keeps the previous database in
/// use.
#[derive(Debug)]
pub struct Cache {
    dir: PathBuf,
    store: PathBuf,
    refresh_secs: i64,
    /// Read-only connection to `mail.db`, with its `mcp_meta`.
    db: Option<(Connection, Meta)>,
    last_attempt_unix: Option<i64>,
    last_error: Option<String>,
    /// Number of builds this value started, failed ones included. A build
    /// call that finds an up-to-date `mail.db` after taking the lock (made
    /// by another process sharing the directory) and reuses it builds
    /// nothing and is not counted.
    pub builds: u64,
    /// Identity of the `mail.db` file `db` was opened from.
    db_id: Option<FileId>,
    /// Identity of a `mail.db` that [`open_db`] refused, so the same file
    /// is not tried again on every call.
    rejected_id: Option<FileId>,
    /// Pause after the store copy of every build (see
    /// [`Cache::with_build_delay`]).
    build_delay: Option<Duration>,
    /// The refresh service that builds `mail.db` instead of this value
    /// (service mode, see [`Cache::with_refresher`]).
    refresher: Option<Box<dyn Refresher + Send>>,
    /// What the last request to the refresher found.
    service: Option<ServiceView>,
    /// How long a call without a database waits for the service's first
    /// build, and how often it looks.
    first_build_wait: (Duration, Duration),
    /// Called at the start of every build, with the lock held.
    #[cfg(test)]
    pub(crate) build_hook: Option<fn()>,
    /// Called by [`Cache::reopen_if_replaced`] between reading the file's
    /// identity and opening it, with the cache directory.
    #[cfg(test)]
    reopen_hook: Option<fn(&Path)>,
}

/// Identity of a file, to notice that it was replaced: a new file put in
/// place by `rename` has another inode, a file changed in place another
/// length or time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i64,
}

/// The identity of `path` itself (a symbolic link is not followed); `None`
/// when it does not exist or cannot be examined.
pub fn file_id(path: &Path) -> Option<FileId> {
    let m = std::fs::symlink_metadata(path).ok()?;
    Some(FileId {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mtime_ns: m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
    })
}

/// What a usable `mail.db` says about its build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbInfo {
    /// `built_unix` of `mcp_meta`.
    pub built_unix: Option<i64>,
    /// `(store_len, store_mtime_ns)` of the store it was built from.
    pub stamp: Option<(u64, i64)>,
}

/// [`DbInfo`] of `mail.db` in `dir` when it is usable for `store` (see
/// [`open_db`]); `None` otherwise. `store` is resolved as [`Cache::new`]
/// does. The connection is closed before this returns.
pub fn db_info(dir: &Path, store: &Path) -> Option<DbInfo> {
    let (_, meta) = open_db(&dir.join(DB_NAME), &resolve_store(store)).ok()?;
    Some(DbInfo {
        built_unix: meta.int("built_unix"),
        stamp: meta.stamp(),
    })
}

/// `store` as an absolute path without symbolic links (only made absolute
/// when it does not exist yet).
fn resolve_store(store: &Path) -> PathBuf {
    std::fs::canonicalize(store)
        .or_else(|_| std::path::absolute(store))
        .unwrap_or_else(|_| store.to_path_buf())
}

impl Cache {
    /// The cache for `store` in `dir`. Creates `dir` if needed and checks it
    /// (see [`ensure_dir`]); builds nothing. The refresh interval is
    /// [`DEFAULT_REFRESH_SECS`].
    ///
    /// `store` is resolved once, here, to an absolute path without symbolic
    /// links (only made absolute when it does not exist yet), so the
    /// `store_path` a database is kept under names one file whatever the
    /// working directory.
    pub fn new(dir: &Path, store: &Path) -> Result<Cache, String> {
        ensure_dir(dir)?;
        let store = resolve_store(store);
        Ok(Cache {
            dir: dir.to_path_buf(),
            store,
            refresh_secs: DEFAULT_REFRESH_SECS as i64,
            db: None,
            last_attempt_unix: None,
            last_error: None,
            builds: 0,
            db_id: None,
            rejected_id: None,
            build_delay: None,
            refresher: None,
            service: None,
            first_build_wait: (proto::FIRST_BUILD_WAIT, proto::FIRST_BUILD_POLL),
            #[cfg(test)]
            build_hook: None,
            #[cfg(test)]
            reopen_hook: None,
        })
    }

    /// Set the shortest time between two builds of a changed store; 0
    /// rebuilds whenever the store changed.
    pub fn with_refresh_secs(mut self, secs: u64) -> Cache {
        self.refresh_secs = i64::try_from(secs).unwrap_or(i64::MAX);
        self
    }

    /// Pause for `delay` in every build right after the store copy, with
    /// the lock held and the temporary files present. A test aid: it lets
    /// a test stop a process in the middle of a build.
    pub fn with_build_delay(mut self, delay: Duration) -> Cache {
        self.build_delay = Some(delay);
        self
    }

    /// Service mode: never build; `refresher` (the refresh service) builds
    /// `mail.db`, and this value only reads it, reopening it when it was
    /// replaced (see [`Cache::connection_at`], [`Cache::force_refresh_at`]
    /// and [`Cache::status`]).
    pub fn with_refresher(mut self, refresher: Box<dyn Refresher + Send>) -> Cache {
        self.refresher = Some(refresher);
        self
    }

    /// In service mode, how long a call without a database waits for the
    /// first build (default [`proto::FIRST_BUILD_WAIT`]) and how often it
    /// looks (default [`proto::FIRST_BUILD_POLL`]).
    pub fn with_first_build_wait(mut self, wait: Duration, poll: Duration) -> Cache {
        self.first_build_wait = (wait, poll);
        self
    }

    /// The database to query at `now` (unix seconds), built or rebuilt
    /// first when needed:
    /// - no usable `mail.db` (missing, not a private regular file, not a
    ///   database, no readable `mcp_meta`, another `format_version` or
    ///   another store path) -> build, unless a build failed less than
    ///   `refresh_secs` ago, in which case that error is returned. A
    ///   `mail.db` opened here that was built from the store as it is now
    ///   clears the error of an earlier attempt;
    /// - the live store differs from the one `mail.db` was built from and
    ///   the last build or attempt is at least `refresh_secs` old ->
    ///   rebuild; if that fails the old database stays in use. A build or
    ///   attempt time after `now` does not count;
    /// - otherwise the open database is used.
    ///
    /// Before deciding, a `mail.db` that another process put in place since
    /// it was opened here is opened instead (see
    /// [`Cache::reopen_if_replaced`]), so the decision is made on the file
    /// on disk.
    ///
    /// In service mode (see [`Cache::with_refresher`]) it never builds: it
    /// sends `touch` (which may start a build in the service, and never
    /// waits for one), opens `mail.db` again when it was replaced, and
    /// answers from it. Without a usable database: the error when the
    /// service is not available; otherwise it waits for the first build
    /// (see [`Cache::wait_for_first_build`]).
    pub fn connection_at(&mut self, now: i64) -> Result<&Connection, String> {
        if let Some(r) = self.refresher.as_mut() {
            let reply = r.touch(now);
            self.service = Some(ServiceView::of(&reply));
            self.reopen_if_replaced();
            if self.db.is_none() {
                // The service is not available: nothing to wait for.
                reply?;
                self.wait_for_first_build(now)?;
            }
            return self
                .db
                .as_ref()
                .map(|(c, _)| c)
                .ok_or_else(|| "no database".to_string());
        }
        if self.reopen_if_replaced() {
            if let Some((_, meta)) = &self.db {
                // Up to date without a build of this value (another process
                // built it, or it could not be opened right after a build):
                // the error of the earlier attempt is over.
                if stamp(&self.store)
                    .ok()
                    .is_some_and(|s| meta.stamp() == Some(s))
                {
                    self.last_error = None;
                }
            }
        }
        match &self.db {
            None => {
                if let (Some(at), Some(e)) = (self.last_attempt_unix, &self.last_error) {
                    // An attempt time after `now` does not count.
                    if (0..self.refresh_secs).contains(&now.saturating_sub(at)) {
                        return Err(e.clone());
                    }
                }
                self.attempt(now, true)?;
            }
            Some((_, meta)) => {
                let changed = stamp(&self.store).ok() != meta.stamp();
                // A time after `now` would put the rebuild off for good.
                let past = |t: Option<i64>| t.filter(|t| *t <= now).unwrap_or(i64::MIN);
                let since = past(meta.int("built_unix")).max(past(self.last_attempt_unix));
                if changed && now.saturating_sub(since) >= self.refresh_secs {
                    // On failure the old database stays in use; the error
                    // is kept in `last_error`.
                    let _ = self.attempt(now, true);
                }
            }
        }
        self.db
            .as_ref()
            .map(|(c, _)| c)
            .ok_or_else(|| "no database".to_string())
    }

    /// Rebuild now, whatever the interval and even when the store did not
    /// change (the `refresh` tool). On failure the old database stays in
    /// use.
    ///
    /// In service mode: send `refresh` (the service builds, or joins its
    /// running build, and replies when it is over), then open the new
    /// `mail.db`. A failed build is the error; the old database stays.
    pub fn force_refresh_at(&mut self, now: i64) -> Result<(), String> {
        let Some(r) = self.refresher.as_mut() else {
            return self.attempt(now, false);
        };
        let reply = r.refresh(now);
        self.reopen_if_replaced();
        reply.map(|st| self.service = Some(ServiceView::of(&Ok(st))))
    }

    /// The state of the cache; checks the live store's size and time. In
    /// service mode `last_error` and `last_attempt_unix` are the service's.
    pub fn status(&self) -> Status<'_> {
        let meta = self.db.as_ref().map(|(_, m)| m);
        let (last_error, last_attempt_unix) = match (&self.refresher, &self.service) {
            (None, _) => (self.last_error.as_deref(), self.last_attempt_unix),
            (Some(_), Some(v)) => (v.last_error.as_deref(), v.last_attempt_unix),
            (Some(_), None) => (None, None),
        };
        Status {
            store_path: &self.store,
            meta,
            store_changed: meta.map(|m| stamp(&self.store).ok() != m.stamp()),
            last_error,
            last_attempt_unix,
            service: self.service.as_ref(),
        }
    }

    /// Service mode, no usable `mail.db`: wait for the service's first
    /// build. Every poll interval open `mail.db` when it appeared and ask
    /// the service's status, for at most the first-build wait (see
    /// [`Cache::with_first_build_wait`]). Fails at once when the service is
    /// not available, when it reports a failed build (`last_error` while
    /// not building), or when it is newer than this server and `mail.db`
    /// has another format version (a database this server cannot read);
    /// with [`FIRST_BUILD_PENDING`] when the wait is over.
    fn wait_for_first_build(&mut self, now: i64) -> Result<(), String> {
        let (wait, poll) = self.first_build_wait;
        let deadline = Instant::now() + wait;
        loop {
            if let Some(v) = &self.service {
                if !v.running {
                    return Err(v.last_error.clone().unwrap_or_default());
                }
                let newer = v.version.as_deref().is_some_and(|ver| {
                    proto::cmp_versions(ver, proto::VERSION) == std::cmp::Ordering::Greater
                });
                if newer && has_other_format(&self.db_path()) {
                    return Err(OLDER_SERVER.into());
                }
                if let (false, Some(e)) = (v.building, &v.last_error) {
                    return Err(e.clone());
                }
            }
            if Instant::now() >= deadline {
                return Err(FIRST_BUILD_PENDING.into());
            }
            std::thread::sleep(poll);
            if self.reopen_if_replaced() {
                return Ok(());
            }
            let Some(r) = self.refresher.as_mut() else {
                return Err("no refresh service".into());
            };
            let reply = r.status(now);
            self.service = Some(ServiceView::of(&reply));
        }
    }

    /// Open `mail.db` when it is not the file the database in use was
    /// opened from (or nothing is open); true when a new connection is now
    /// in use. The file's identity is read before and after opening it;
    /// the new connection is accepted only when both are equal (otherwise
    /// the file was replaced meanwhile and it is tried once more). When
    /// that fails twice, or [`open_db`] refuses the file, the connection in
    /// use stays; a refused file is not tried again until it changes.
    fn reopen_if_replaced(&mut self) -> bool {
        let path = self.db_path();
        let on_disk = file_id(&path);
        if on_disk.is_none()
            || (self.db.is_some() && on_disk == self.db_id)
            || on_disk == self.rejected_id
        {
            return false;
        }
        for _ in 0..2 {
            let Some(before) = file_id(&path) else {
                return false;
            };
            #[cfg(test)]
            if let Some(hook) = self.reopen_hook {
                hook(&self.dir);
            }
            match open_db(&path, &self.store) {
                Ok(db) => {
                    if file_id(&path) == Some(before) {
                        let built = db.1.int("built_unix").map_or_else(|| "?".into(), iso);
                        let verb = if self.db.is_some() {
                            "reopened"
                        } else {
                            "opened"
                        };
                        log(&format!("{verb} {DB_NAME} (built {built})"));
                        // Releases the old file, if any.
                        self.db = Some(db);
                        self.db_id = Some(before);
                        self.rejected_id = None;
                        return true;
                    }
                }
                Err(_) => {
                    self.rejected_id = Some(before);
                    return false;
                }
            }
        }
        false
    }

    /// One build attempt at `now`, recorded in `last_attempt_unix` and
    /// `last_error`. `reuse`: accept an up-to-date `mail.db` found after
    /// taking the lock instead of building.
    fn attempt(&mut self, now: i64, reuse: bool) -> Result<(), String> {
        self.last_attempt_unix = Some(now);
        // Set first, so a build that panics is recorded as failed too and
        // is not repeated before `refresh_secs` have passed.
        self.last_error = Some(BUILD_UNFINISHED.to_string());
        match self.build_locked(now, reuse) {
            Ok(_) => {
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                log(&format!("cache build failed: {e}"));
                self.last_error = Some(e.clone());
                Err(e)
            }
        }
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
    /// temporary files and leaves an existing `mail.db` as it was. On
    /// success the new database is the one in use. Always builds; the
    /// lazy path of [`Cache::connection_at`] may reuse a database another
    /// process just built instead.
    pub fn build(&mut self, now: i64) -> Result<BuildInfo, String> {
        self.build_locked(now, false)
            .map(|info| info.expect("a build without reuse always builds"))
    }

    /// [`Cache::build`]; with `reuse`, first (holding the lock) check
    /// whether `mail.db` is usable and either was built from the store as
    /// it is now or was built less than `refresh_secs` before `now` (by
    /// another process, while this one waited for the lock; a build time
    /// after `now` does not count), and if so open it and return `None`
    /// without building.
    fn build_locked(&mut self, now: i64, reuse: bool) -> Result<Option<BuildInfo>, String> {
        ensure_dir(&self.dir)?;
        let _lock = lock(&self.dir)?;
        if reuse {
            // Builds take the lock, so the file cannot be replaced here.
            let id = file_id(&self.db_path());
            if let Ok((conn, meta)) = open_db(&self.db_path(), &self.store) {
                let current = stamp(&self.store)
                    .ok()
                    .is_some_and(|s| meta.stamp() == Some(s));
                let young = meta
                    .int("built_unix")
                    .is_some_and(|t| t <= now && now.saturating_sub(t) < self.refresh_secs);
                if current || young {
                    self.db = Some((conn, meta));
                    self.db_id = id;
                    return Ok(None);
                }
            }
        }
        self.builds += 1;
        #[cfg(test)]
        if let Some(hook) = self.build_hook {
            hook();
        }
        remove_leftovers(&self.dir);

        let pid = std::process::id();
        let copy_path = self.dir.join(format!("{COPY_PREFIX}{pid}"));
        let tmp_path = self.dir.join(format!("{TMP_PREFIX}{pid}"));
        let _temps = TempFiles(vec![copy_path.clone(), tmp_path.clone()]);

        let started = Instant::now();
        let (store_len, store_mtime_ns) = copy_store(&self.store, &copy_path)?;
        let copy_ms = ms_since(started);
        if let Some(delay) = self.build_delay {
            std::thread::sleep(delay);
        }

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
        // Written with `synchronous=OFF`: flush it before it replaces the
        // database in use.
        File::open(&tmp_path)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("flush the database: {e}"))?;
        std::fs::rename(&tmp_path, self.db_path())
            .map_err(|e| format!("replace {}: {e}", self.db_path().display()))?;
        // The old database is gone from the directory: stop using it even
        // if the new one cannot be opened now (the next call tries again).
        self.db = None;
        self.db_id = None;
        let id = file_id(&self.db_path());
        self.db = Some(
            open_db(&self.db_path(), &self.store)
                .map_err(|e| format!("the new database is in place, but: {e}"))?,
        );
        self.db_id = id;

        log(&format!(
            "cache built in {} ms (copy {copy_ms}, export {export_ms}, prepare {prepare_ms}); \
             {} objects, {} messages indexed, {} events",
            ms_since(started),
            info.stats.objects,
            counts.messages,
            counts.events
        ));
        Ok(Some(info))
    }
}

/// `HxStore.hxd` of the default Outlook profile: under the profiles
/// directory of `$HOME` (see [`default_store_path_in`]).
pub fn default_store_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    default_store_path_in(&Path::new(&home).join(PROFILES_DIR))
}

/// `HxStore.hxd` of the profile to use in the profiles directory `base`:
/// `Main Profile`, else `Main Identity`, else the only profile directory
/// that holds a store. `None` when there is no store or several candidates.
pub fn default_store_path_in(base: &Path) -> Option<PathBuf> {
    for name in ["Main Profile", "Main Identity"] {
        let p = base.join(name).join(STORE_NAME);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut found = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|e| e.path().join(STORE_NAME))
        .filter(|p| p.is_file());
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// Open `path` read-only and read its `mcp_meta`. Fails unless the database
/// is usable for `store`: a private regular file (see [`check_private`]),
/// readable `mcp_meta`, [`FORMAT_VERSION`] and the same store path.
fn open_db(path: &Path, store: &Path) -> Result<(Connection, Meta), String> {
    let m = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    check_private(&m).map_err(|e| format!("{} {e}", path.display()))?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    // Sorts of a query stay in memory instead of spilling to `$TMPDIR`.
    conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA query_only=ON;")
        .map_err(|e| format!("configure {}: {e}", path.display()))?;
    let meta = read_meta(&conn).map_err(|e| format!("read {}: {e}", path.display()))?;
    if meta.get("format_version") != Some(FORMAT_VERSION) {
        return Err(format!("{} has another format version", path.display()));
    }
    if meta.get("store_path") != Some(&*store.to_string_lossy()) {
        return Err(format!("{} was built from another store", path.display()));
    }
    Ok((conn, meta))
}

/// Whether `path` is a private database (see [`check_private`]) whose
/// `mcp_meta` names another format version than [`FORMAT_VERSION`].
fn has_other_format(path: &Path) -> bool {
    if !std::fs::symlink_metadata(path).is_ok_and(|m| check_private(&m).is_ok()) {
        return false;
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
    .and_then(|conn| read_meta(&conn).ok())
    .is_some_and(|m| m.get("format_version").is_some_and(|v| v != FORMAT_VERSION))
}

/// All rows of `mcp_meta`.
fn read_meta(conn: &Connection) -> rusqlite::Result<Meta> {
    let mut st = conn.prepare("SELECT key, value FROM mcp_meta")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(Meta(rows.collect::<rusqlite::Result<_>>()?))
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

/// Accept the metadata (not following a symbolic link) of a file found in
/// the cache directory only if it is a regular file owned by the current
/// user that neither group nor others can access. The directory may have
/// been open to others before [`ensure_dir`] tightened it, so a file in it
/// is not trusted for being there.
pub(crate) fn check_private(m: &std::fs::Metadata) -> Result<(), &'static str> {
    if !m.file_type().is_file() {
        return Err("is not a regular file");
    }
    // SAFETY: getuid() has no preconditions and cannot fail.
    if m.uid() != unsafe { libc::getuid() } {
        return Err("is owned by another user");
    }
    if m.mode() & 0o077 != 0 {
        return Err("is accessible by other users");
    }
    Ok(())
}

/// The open `lock` file of `dir`, locked exclusively; the lock is released
/// when the file is closed. An existing `lock` that is not a private
/// regular file (see [`check_private`]) is removed and created again; a
/// symbolic link is never followed.
fn lock(dir: &Path) -> Result<File, String> {
    let (f, path) = open_lock(dir)?;
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

/// The `lock` file of `dir`, open and not locked, and its path. An existing
/// one that is not a private regular file is removed and created again; a
/// symbolic link is never followed.
fn open_lock(dir: &Path) -> Result<(File, PathBuf), String> {
    let path = dir.join(LOCK_NAME);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| check_private(&m).is_err()) {
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    Ok((f, path))
}

/// Remove the temporary files that builds of dead processes left in `dir`
/// (a process killed during a build), when no build runs: only when the
/// build lock can be taken within [`LEFTOVER_LOCK_RETRY`]. Whether it did.
pub fn remove_dead_leftovers(dir: &Path) -> bool {
    let Ok((f, _)) = open_lock(dir) else {
        return false;
    };
    let deadline = Instant::now() + LEFTOVER_LOCK_RETRY;
    loop {
        // SAFETY: flock() on a descriptor owned by `f` for the whole call.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            remove_leftovers(dir);
            // Closing `f` releases the lock.
            return true;
        }
        match std::io::Error::last_os_error().kind() {
            ErrorKind::Interrupted => {}
            ErrorKind::WouldBlock if Instant::now() < deadline => {
                std::thread::sleep(LEFTOVER_LOCK_POLL)
            }
            _ => return false,
        }
    }
}

/// Remove the temporary files of this process's builds in `dir` (and their
/// SQLite side files), for a process that ends while a build of its own may
/// still run. Files of other processes are left alone.
pub fn remove_own_temps(dir: &Path) {
    let pid = std::process::id();
    drop(TempFiles(vec![
        dir.join(format!("{COPY_PREFIX}{pid}")),
        dir.join(format!("{TMP_PREFIX}{pid}")),
    ]));
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
pub(crate) fn stamp(path: &Path) -> std::io::Result<(u64, i64)> {
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
    copy_store_with(src, dst, stamp)
}

/// [`copy_store`] with `stamp` reading the source's `(len, mtime in ns)`.
fn copy_store_with(
    src: &Path,
    dst: &Path,
    mut stamp: impl FnMut(&Path) -> std::io::Result<(u64, i64)>,
) -> Result<(u64, i64), String> {
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
/// address), preview and the body as text (HTML body, else, when that is
/// NULL or empty, the first body part), capped at [`MAX_FTS_BODY`] chars.
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
        let body = match text(5)?.filter(|b| !b.is_empty()) {
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
        ("format_version", FORMAT_VERSION.to_string()),
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
        fake_status, mail_store, mail_store_without_calendar, FakeOp, FakeRefresher, MAIL_EVENTS,
        MAIL_EVENT_TIMES, MAIL_MSGS, NOW,
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
        assert_eq!(meta("format_version"), FORMAT_VERSION);
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
        // The non-text body is read lossily.
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

    #[test]
    fn empty_html_body_falls_back_to_the_first_body_part() {
        let conn = Connection::open_in_memory().unwrap();
        let mm = mail_store();
        crate::export::export(
            &crate::kv::Store::load(&mm),
            &mm,
            &conn,
            crate::item::tick_window_at(NOW),
        )
        .unwrap();
        for (i, html) in [(0, Some("")), (1, None)] {
            let n = conn
                .execute(
                    "UPDATE \"MessageData\" SET HTMLBodyBytes = ?1,
                            HTMLFirstBodyBytes = '<p>first <b>walrus</b></p>'
                     WHERE guid = (SELECT MessageData FROM \"MessageHeader\" WHERE guid = ?2)",
                    rusqlite::params![html, guid_hex(&MAIL_MSGS[i])],
                )
                .unwrap();
            assert_eq!(n, 1);
        }
        prepare(&conn, NOW_I).unwrap();
        let bodies: Vec<String> = conn
            .prepare("SELECT body FROM mcp_fts WHERE mcp_fts MATCH 'body:walrus' ORDER BY guid")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(bodies, ["first walrus", "first walrus"]);
    }

    // --- store discovery ---

    /// A profile directory `name` under `base`, with a store file if
    /// `with_store`.
    fn profile(base: &Path, name: &str, with_store: bool) {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        if with_store {
            std::fs::write(d.join("HxStore.hxd"), b"x").unwrap();
        }
    }

    #[test]
    fn default_store_prefers_main_profile_then_main_identity() {
        let t = TestDir::new("profiles");
        let base = t.0.join("profiles");
        profile(&base, "Other", true);
        profile(&base, "Main Identity", true);
        profile(&base, "Main Profile", true);
        assert_eq!(
            default_store_path_in(&base),
            Some(base.join("Main Profile").join("HxStore.hxd"))
        );
        std::fs::remove_file(base.join("Main Profile").join("HxStore.hxd")).unwrap();
        assert_eq!(
            default_store_path_in(&base),
            Some(base.join("Main Identity").join("HxStore.hxd"))
        );
    }

    #[test]
    fn default_store_takes_the_only_other_profile() {
        let t = TestDir::new("profiles1");
        let base = t.0.join("profiles");
        profile(&base, "Empty", false);
        profile(&base, "Work", true);
        assert_eq!(
            default_store_path_in(&base),
            Some(base.join("Work").join("HxStore.hxd"))
        );
        profile(&base, "Home", true);
        assert_eq!(default_store_path_in(&base), None);
    }

    #[test]
    fn default_store_none_without_profiles() {
        let t = TestDir::new("profiles0");
        assert_eq!(default_store_path_in(&t.0.join("missing")), None);
        let base = t.0.join("profiles");
        profile(&base, "Empty", false);
        assert_eq!(default_store_path_in(&base), None);
    }

    // --- refresh policy ---

    /// Set the store's modification time to `secs` after the epoch, so it
    /// looks changed without waiting.
    fn touch(p: &Path, secs: u64) {
        let f = OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    /// A cache in `t` over a [`mail_store`] file, nothing built yet.
    fn lazy(t: &TestDir, refresh_secs: u64) -> Cache {
        Cache::new(&t.cache_dir(), &t.store(&mail_store()))
            .unwrap()
            .with_refresh_secs(refresh_secs)
    }

    fn messages(c: &mut Cache, now: i64) -> i64 {
        count(
            c.connection_at(now).unwrap(),
            "SELECT COUNT(*) FROM messages",
        )
    }

    fn built_unix(c: &Cache) -> Option<i64> {
        c.status().meta.and_then(|m| m.int("built_unix"))
    }

    #[test]
    fn first_call_builds_and_unchanged_store_never_rebuilds() {
        let t = TestDir::new("first");
        let mut c = lazy(&t, 900);
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(c.builds, 1);
        for dt in [1, 900, 100_000] {
            messages(&mut c, NOW_I + dt);
        }
        assert_eq!(c.builds, 1);
        assert_eq!(built_unix(&c), Some(NOW_I));
        assert!(c.status().last_error.is_none());
        assert_eq!(c.status().store_changed, Some(false));
    }

    #[test]
    fn changed_store_rebuilds_only_after_the_interval() {
        let t = TestDir::new("interval");
        let mut c = lazy(&t, 900);
        messages(&mut c, NOW_I);
        touch(c.store_path(), 1_000_000);
        assert_eq!(c.status().store_changed, Some(true));
        messages(&mut c, NOW_I + 899);
        assert_eq!(c.builds, 1);
        messages(&mut c, NOW_I + 900);
        assert_eq!(c.builds, 2);
        assert_eq!(built_unix(&c), Some(NOW_I + 900));
        assert_eq!(c.status().store_changed, Some(false));
    }

    #[test]
    fn zero_interval_rebuilds_whenever_the_store_changed() {
        let t = TestDir::new("zero");
        let mut c = lazy(&t, 0);
        messages(&mut c, NOW_I);
        messages(&mut c, NOW_I);
        assert_eq!(c.builds, 1);
        touch(c.store_path(), 1_000_000);
        messages(&mut c, NOW_I);
        assert_eq!(c.builds, 2);
        messages(&mut c, NOW_I);
        assert_eq!(c.builds, 2);
    }

    #[test]
    fn second_cache_reuses_the_database() {
        let t = TestDir::new("reuse");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        assert_eq!(messages(&mut b, NOW_I + 5), 4);
        assert_eq!(b.builds, 0);
        assert_eq!(built_unix(&b), Some(NOW_I));
    }

    #[test]
    fn failed_rebuild_keeps_serving_and_waits_for_the_interval() {
        let t = TestDir::new("failkeep");
        let mut c = lazy(&t, 900);
        messages(&mut c, NOW_I);
        t.store(b"this is not a store file, only some text that is long enough");
        assert_eq!(messages(&mut c, NOW_I + 1000), 4);
        assert_eq!(c.builds, 2);
        let e = c.status().last_error.expect("last_error is set");
        assert!(e.contains("store"), "{e}");
        assert_eq!(messages(&mut c, NOW_I + 1010), 4);
        assert_eq!(c.builds, 2);
        assert_eq!(messages(&mut c, NOW_I + 1900), 4);
        assert_eq!(c.builds, 3);
        assert_eq!(files(&t.cache_dir()), ["lock", "mail.db"]);
    }

    #[test]
    fn failed_first_build_returns_the_error() {
        let t = TestDir::new("failfirst");
        let store = t.store(b"this is not a store file, only some text that is long enough");
        let mut c = Cache::new(&t.cache_dir(), &store).unwrap();
        let e = c.connection_at(NOW_I).unwrap_err();
        assert!(e.contains("store"), "{e}");
        assert_eq!(c.status().last_error, Some(e.as_str()));
        assert!(c.status().meta.is_none());
        assert_eq!(c.status().store_changed, None);
        // No retry inside the interval.
        assert!(c.connection_at(NOW_I + 10).is_err());
        assert_eq!(c.builds, 1);
        // Fixed store, interval passed: builds.
        t.store(&mail_store());
        assert_eq!(messages(&mut c, NOW_I + 900), 4);
        assert_eq!(c.builds, 2);
        assert!(c.status().last_error.is_none());
    }

    #[test]
    fn unreadable_store_does_not_clear_the_error() {
        let t = TestDir::new("nostamp");
        let store = t.store(b"this is not a store file, only some text that is long enough");
        let mut c = Cache::new(&t.cache_dir(), &store).unwrap();
        let e = c.connection_at(NOW_I).unwrap_err();
        // Another process leaves a database without the store's size, and
        // the store goes away: neither side has a stamp to compare.
        t.store(&mail_store());
        Cache::new(&t.cache_dir(), &store)
            .unwrap()
            .build(NOW_I)
            .unwrap();
        Connection::open(t.cache_dir().join("mail.db"))
            .unwrap()
            .execute("DELETE FROM mcp_meta WHERE key = 'store_len'", [])
            .unwrap();
        std::fs::remove_file(&store).unwrap();
        assert!(c.connection_at(NOW_I + 10).is_ok());
        assert_eq!(c.builds, 1);
        assert_eq!(c.status().last_error, Some(e.as_str()));
    }

    #[test]
    fn force_refresh_ignores_the_interval() {
        let t = TestDir::new("force");
        let mut c = lazy(&t, 900);
        messages(&mut c, NOW_I);
        c.force_refresh_at(NOW_I + 1).unwrap();
        assert_eq!(c.builds, 2);
        assert_eq!(built_unix(&c), Some(NOW_I + 1));
        assert_eq!(messages(&mut c, NOW_I + 2), 4);
        assert_eq!(c.builds, 2);
    }

    /// Build in `t`, then change `mail.db` with `sql` (read-write).
    fn built_then(t: &TestDir, sql: &str) -> PathBuf {
        let mut a = lazy(t, 900);
        messages(&mut a, NOW_I);
        let db = Connection::open(a.db_path()).unwrap();
        db.execute_batch(sql).unwrap();
        a.store_path().to_path_buf()
    }

    #[test]
    fn other_format_version_is_rebuilt_at_once() {
        let t = TestDir::new("format");
        let store = built_then(
            &t,
            "UPDATE mcp_meta SET value = '0' WHERE key = 'format_version'",
        );
        let mut b = Cache::new(&t.cache_dir(), &store).unwrap();
        assert_eq!(messages(&mut b, NOW_I + 1), 4);
        assert_eq!(b.builds, 1);
        let meta = b.status().meta.unwrap();
        assert_eq!(meta.get("format_version"), Some(FORMAT_VERSION));
    }

    #[test]
    fn database_of_another_store_is_rebuilt_at_once() {
        let t = TestDir::new("otherstore");
        built_then(&t, "SELECT 1");
        let other = t.0.join("Other.hxd");
        std::fs::write(&other, mail_store()).unwrap();
        let mut b = Cache::new(&t.cache_dir(), &other).unwrap();
        assert_eq!(messages(&mut b, NOW_I + 1), 4);
        assert_eq!(b.builds, 1);
        let meta = b.status().meta.unwrap();
        assert_eq!(
            meta.get("store_path"),
            Some(&*b.store_path().to_string_lossy())
        );
        assert_eq!(b.store_path(), std::fs::canonicalize(&other).unwrap());
    }

    #[test]
    fn corrupt_database_is_rebuilt() {
        let t = TestDir::new("corrupt");
        let store = t.store(&mail_store());
        std::fs::create_dir(t.cache_dir()).unwrap();
        std::fs::write(t.cache_dir().join(DB_NAME), [0xa5u8; 8192]).unwrap();
        let mut c = Cache::new(&t.cache_dir(), &store).unwrap();
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(c.builds, 1);
    }

    #[test]
    fn database_without_meta_is_rebuilt() {
        let t = TestDir::new("nometa");
        let store = t.store(&mail_store());
        std::fs::create_dir(t.cache_dir()).unwrap();
        Connection::open(t.cache_dir().join(DB_NAME))
            .unwrap()
            .execute_batch("CREATE TABLE x(a)")
            .unwrap();
        let mut c = Cache::new(&t.cache_dir(), &store).unwrap();
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(c.builds, 1);
    }

    #[test]
    fn second_cache_picks_up_a_rebuild_of_the_first() {
        let t = TestDir::new("twocaches");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        messages(&mut b, NOW_I);
        touch(a.store_path(), 1_000_000);
        messages(&mut a, NOW_I + 1000);
        assert_eq!(a.builds, 2);
        assert_eq!(messages(&mut b, NOW_I + 1000), 4);
        assert_eq!(b.builds, 0);
        assert_eq!(built_unix(&b), Some(NOW_I + 1000));
        assert_eq!(b.status().store_changed, Some(false));
    }

    // --- store path ---

    #[test]
    fn store_path_is_resolved_through_symbolic_links() {
        let t = TestDir::new("storelink");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        let real = std::fs::canonicalize(t.0.join("HxStore.hxd")).unwrap();
        assert_eq!(a.store_path(), real);
        let link = t.0.join("link.hxd");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // The same file under another name: the same database.
        let mut b = Cache::new(&t.cache_dir(), &link).unwrap();
        assert_eq!(b.store_path(), real);
        assert_eq!(messages(&mut b, NOW_I + 5), 4);
        assert_eq!(b.builds, 0);
    }

    #[test]
    fn missing_store_path_is_made_absolute() {
        let t = TestDir::new("storeabs");
        let c = Cache::new(&t.cache_dir(), Path::new("no-such-dir/HxStore.hxd")).unwrap();
        assert!(c.store_path().is_absolute());
        assert!(c.store_path().ends_with("no-such-dir/HxStore.hxd"));
    }

    // --- query connection ---

    #[test]
    fn query_connection_is_read_only_and_keeps_temp_data_in_memory() {
        let t = TestDir::new("pragmas");
        let mut c = lazy(&t, 900);
        let conn = c.connection_at(NOW_I).unwrap();
        // 2 is MEMORY.
        assert_eq!(count(conn, "PRAGMA temp_store"), 2);
        assert_eq!(count(conn, "PRAGMA query_only"), 1);
        assert!(conn.execute_batch("CREATE TABLE x(a)").is_err());
    }

    // --- a build that panics ---

    /// `connection_at`, with a panic caught: `None` when it panicked.
    fn caught(c: &mut Cache, now: i64) -> Option<Result<(), String>> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            c.connection_at(now).map(drop)
        }))
        .ok()
    }

    #[test]
    fn panicking_first_build_is_not_repeated_inside_the_interval() {
        let t = TestDir::new("panicfirst");
        let mut c = lazy(&t, 900);
        c.build_hook = Some(|| panic!("build hook"));
        assert_eq!(caught(&mut c, NOW_I), None);
        assert_eq!(c.builds, 1);
        assert_eq!(c.status().last_error, Some(BUILD_UNFINISHED));
        // Inside the interval: the recorded error, no new build.
        assert_eq!(
            caught(&mut c, NOW_I + 10),
            Some(Err(BUILD_UNFINISHED.to_string()))
        );
        assert_eq!(c.builds, 1);
        // The lock was released and the next build works.
        c.build_hook = None;
        assert_eq!(messages(&mut c, NOW_I + 900), 4);
        assert_eq!(c.builds, 2);
        assert!(c.status().last_error.is_none());
        assert_eq!(files(&t.cache_dir()), ["lock", "mail.db"]);
    }

    #[test]
    fn panicking_rebuild_keeps_serving_and_waits_for_the_interval() {
        let t = TestDir::new("panicre");
        let mut c = lazy(&t, 900);
        messages(&mut c, NOW_I);
        touch(c.store_path(), 1_000_000);
        c.build_hook = Some(|| panic!("build hook"));
        assert_eq!(caught(&mut c, NOW_I + 1000), None);
        assert_eq!(c.builds, 2);
        assert_eq!(messages(&mut c, NOW_I + 1010), 4);
        assert_eq!(c.builds, 2);
        assert_eq!(c.status().last_error, Some(BUILD_UNFINISHED));
    }

    // --- files found in the cache directory ---

    #[test]
    fn database_open_to_others_is_rebuilt() {
        let t = TestDir::new("widedb");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        std::fs::set_permissions(a.db_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        assert_eq!(messages(&mut b, NOW_I + 5), 4);
        assert_eq!(b.builds, 1);
        assert_eq!(mode(&b.db_path()), 0o600);
    }

    #[test]
    fn symlinked_database_is_not_used() {
        let t = TestDir::new("linkdb");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        let before = std::fs::read(a.db_path()).unwrap();
        // Another cache directory whose `mail.db` points at the first one.
        let dir = t.0.join("cache2");
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(a.db_path(), dir.join(DB_NAME)).unwrap();
        let mut b = Cache::new(&dir, a.store_path()).unwrap();
        assert_eq!(messages(&mut b, NOW_I + 5), 4);
        assert_eq!(b.builds, 1);
        let m = std::fs::symlink_metadata(b.db_path()).unwrap();
        assert!(m.file_type().is_file());
        assert_eq!(std::fs::read(a.db_path()).unwrap(), before);
    }

    #[test]
    fn symlinked_lock_is_replaced() {
        let t = TestDir::new("linklock");
        let target = t.0.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let dir = t.cache_dir();
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(LOCK_NAME)).unwrap();
        let mut c = Cache::new(&dir, &t.store(&mail_store())).unwrap();
        c.build(NOW_I).unwrap();
        let m = std::fs::symlink_metadata(dir.join(LOCK_NAME)).unwrap();
        assert!(m.file_type().is_file());
        assert_eq!(mode(&dir.join(LOCK_NAME)), 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        // A link to nothing is not followed either.
        std::fs::remove_file(dir.join(LOCK_NAME)).unwrap();
        std::os::unix::fs::symlink(t.0.join("absent"), dir.join(LOCK_NAME)).unwrap();
        c.build(NOW_I + 1).unwrap();
        assert!(!t.0.join("absent").exists());
    }

    #[test]
    fn build_time_in_the_future_does_not_block_a_rebuild() {
        let t = TestDir::new("future");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I + 100_000);
        touch(a.store_path(), 1_000_000);
        // Another server, whose clock is behind the build time.
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        assert_eq!(messages(&mut b, NOW_I), 4);
        assert_eq!(b.builds, 1);
        assert_eq!(built_unix(&b), Some(NOW_I));
        // The same value, after its own clock went back.
        touch(a.store_path(), 2_000_000);
        messages(&mut a, NOW_I + 50_000);
        assert_eq!(a.builds, 2);
    }

    #[test]
    fn failed_attempt_in_the_future_does_not_block_a_build() {
        let t = TestDir::new("futurefail");
        let store = t.store(b"this is not a store file, only some text that is long enough");
        let mut c = Cache::new(&t.cache_dir(), &store).unwrap();
        assert!(c.connection_at(NOW_I + 100_000).is_err());
        assert_eq!(c.builds, 1);
        // The clock went back: the attempt is not inside the interval.
        t.store(&mail_store());
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(c.builds, 2);
        assert!(c.status().last_error.is_none());
    }

    #[test]
    fn database_built_by_another_server_clears_the_error() {
        let t = TestDir::new("clearerr");
        let mut a = lazy(&t, 900);
        a.build_hook = Some(|| panic!("build hook"));
        assert_eq!(caught(&mut a, NOW_I), None);
        assert_eq!(a.status().last_error, Some(BUILD_UNFINISHED));
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        messages(&mut b, NOW_I + 5);
        // Inside the interval: the other server's database, no build here.
        assert_eq!(messages(&mut a, NOW_I + 10), 4);
        assert_eq!(a.builds, 1);
        assert!(a.status().last_error.is_none());
    }

    #[test]
    fn stale_database_of_another_server_keeps_the_error() {
        let t = TestDir::new("keeperr");
        let mut a = lazy(&t, 900);
        a.build_hook = Some(|| panic!("build hook"));
        assert_eq!(caught(&mut a, NOW_I), None);
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        messages(&mut b, NOW_I + 5);
        touch(a.store_path(), 1_000_000);
        // Served, but not built from the store as it is now.
        assert_eq!(messages(&mut a, NOW_I + 10), 4);
        assert_eq!(a.builds, 1);
        assert_eq!(a.status().last_error, Some(BUILD_UNFINISHED));
    }

    // --- copying a store that changes ---

    /// `copy_store_with` over a fixture store, with the source's stamp
    /// taken from `stamps` in turn; the result and the number of stamps
    /// read.
    fn copy_with_stamps(name: &str, stamps: &[(u64, i64)]) -> (Result<(u64, i64), String>, usize) {
        let t = TestDir::new(name);
        let src = t.store(&mail_store());
        let dst = t.0.join("copy");
        let mut n = 0;
        let r = copy_store_with(&src, &dst, |_| {
            n += 1;
            Ok(stamps[n - 1])
        });
        if r.is_ok() {
            assert_eq!(std::fs::read(&dst).unwrap(), mail_store());
            assert_eq!(mode(&dst), 0o600);
        }
        (r, n)
    }

    #[test]
    fn copy_is_retried_when_the_store_changed_meanwhile() {
        let len = mail_store().len() as u64;
        // Unchanged: one attempt.
        assert_eq!(
            copy_with_stamps("copy1", &[(len, 1), (len, 1)]),
            (Ok((len, 1)), 2)
        );
        // Changed during the first attempt, quiet during the second.
        assert_eq!(
            copy_with_stamps("copy2", &[(len, 1), (len, 2), (len, 2), (len, 2)]),
            (Ok((len, 2)), 4)
        );
        // A copy of another length than the source had is not accepted.
        let stamps = [(len + 1, 1), (len + 1, 1), (len, 2), (len, 2)];
        assert_eq!(copy_with_stamps("copy3", &stamps), (Ok((len, 2)), 4));
    }

    #[test]
    fn copy_gives_up_when_the_store_keeps_changing() {
        let len = mail_store().len() as u64;
        let stamps: Vec<(u64, i64)> = (0..2 * COPY_ATTEMPTS as i64).map(|i| (len, i)).collect();
        let (r, n) = copy_with_stamps("copy4", &stamps);
        assert_eq!(
            r,
            Err("the store kept changing during the copy".to_string())
        );
        assert_eq!(n, 2 * COPY_ATTEMPTS);
    }

    // --- file identity and reopen on replace ---

    #[test]
    fn file_id_tells_a_replaced_file_apart() {
        let t = TestDir::new("fileid");
        let a = t.0.join("a");
        let b = t.0.join("b");
        std::fs::write(&a, b"one").unwrap();
        let first = file_id(&a).expect("an existing file has an id");
        assert_eq!(file_id(&a), Some(first));
        // Same length and time, another file put in place by a rename.
        std::fs::write(&b, b"two").unwrap();
        touch(&a, 1_000_000);
        touch(&b, 1_000_000);
        let first = file_id(&a).unwrap();
        std::fs::rename(&b, &a).unwrap();
        assert_ne!(file_id(&a), Some(first));
        assert_eq!(file_id(&t.0.join("missing")), None);
    }

    #[test]
    fn file_id_does_not_follow_a_symbolic_link() {
        let t = TestDir::new("fileidlink");
        let target = t.0.join("target");
        std::fs::write(&target, b"some content").unwrap();
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let id = file_id(&link).unwrap();
        let m = std::fs::symlink_metadata(&link).unwrap();
        assert_eq!((id.dev, id.ino, id.len), (m.dev(), m.ino(), m.len()));
        assert_ne!(Some(id), file_id(&target));
        // A link to nothing still has an id of its own.
        let dangling = t.0.join("dangling");
        std::os::unix::fs::symlink(t.0.join("absent"), &dangling).unwrap();
        assert!(file_id(&dangling).is_some());
    }

    #[test]
    fn open_database_is_replaced_by_a_rebuild_of_another_cache() {
        let t = TestDir::new("reopen");
        let mut a = lazy(&t, 900);
        messages(&mut a, NOW_I);
        let mut b = Cache::new(&t.cache_dir(), a.store_path()).unwrap();
        messages(&mut b, NOW_I);
        a.build(NOW_I + 10).unwrap();
        // Inside the interval and the store unchanged: B serves A's file.
        assert_eq!(messages(&mut b, NOW_I + 20), 4);
        assert_eq!(b.builds, 0);
        assert_eq!(built_unix(&b), Some(NOW_I + 10));
        assert_eq!(b.db_id, file_id(&b.db_path()));
        assert!(b.db_id.is_some());
    }

    /// Rebuild `mail.db` in the cache directory `dir` (layout of
    /// [`TestDir`]) with another cache, built at `at`.
    fn rebuild_in(dir: &Path, at: i64) {
        let store = dir.parent().unwrap().join("HxStore.hxd");
        Cache::new(dir, &store).unwrap().build(at).unwrap();
    }

    #[test]
    fn file_replaced_once_while_opening_is_opened_again() {
        static N: AtomicUsize = AtomicUsize::new(0);
        let t = TestDir::new("raceonce");
        let mut b = lazy(&t, 900);
        messages(&mut b, NOW_I);
        rebuild_in(&t.cache_dir(), NOW_I + 10);
        b.reopen_hook = Some(|dir| {
            if N.fetch_add(1, Ordering::SeqCst) == 0 {
                rebuild_in(dir, NOW_I + 100);
            }
        });
        assert_eq!(messages(&mut b, NOW_I + 20), 4);
        assert_eq!(N.load(Ordering::SeqCst), 2);
        assert_eq!(built_unix(&b), Some(NOW_I + 100));
        assert_eq!(b.db_id, file_id(&b.db_path()));
        assert_eq!(b.builds, 1);
    }

    #[test]
    fn file_replaced_on_every_attempt_keeps_the_old_connection() {
        static N: AtomicUsize = AtomicUsize::new(0);
        let t = TestDir::new("raceever");
        let mut b = lazy(&t, 900);
        messages(&mut b, NOW_I);
        let old_id = b.db_id;
        assert!(old_id.is_some());
        rebuild_in(&t.cache_dir(), NOW_I + 10);
        b.reopen_hook = Some(|dir| {
            let n = N.fetch_add(1, Ordering::SeqCst) as i64;
            rebuild_in(dir, NOW_I + 100 * (n + 1));
        });
        assert_eq!(messages(&mut b, NOW_I + 20), 4);
        assert_eq!(N.load(Ordering::SeqCst), 2);
        assert_eq!(built_unix(&b), Some(NOW_I));
        assert_eq!(b.db_id, old_id);
        assert_ne!(b.db_id, file_id(&b.db_path()));
        // The next call, without the hook, picks up the newest file.
        b.reopen_hook = None;
        assert_eq!(messages(&mut b, NOW_I + 30), 4);
        assert_eq!(built_unix(&b), Some(NOW_I + 200));
        assert_eq!(b.db_id, file_id(&b.db_path()));
        assert_eq!(b.builds, 1);
    }

    #[test]
    fn rejected_replacement_keeps_the_old_connection_and_is_not_retried() {
        static N: AtomicUsize = AtomicUsize::new(0);
        let t = TestDir::new("rejected");
        let mut b = lazy(&t, 900);
        messages(&mut b, NOW_I);
        // Another program puts a database of another format in place.
        let dir = t.cache_dir();
        rebuild_in(&dir, NOW_I + 10);
        Connection::open(dir.join(DB_NAME))
            .unwrap()
            .execute(
                "UPDATE mcp_meta SET value = '0' WHERE key = 'format_version'",
                [],
            )
            .unwrap();
        b.reopen_hook = Some(|_| {
            N.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(messages(&mut b, NOW_I + 20), 4);
        assert_eq!(built_unix(&b), Some(NOW_I));
        assert_eq!(N.load(Ordering::SeqCst), 1);
        assert_eq!(messages(&mut b, NOW_I + 30), 4);
        assert_eq!(N.load(Ordering::SeqCst), 1);
        assert_eq!(b.builds, 1);
    }

    #[test]
    fn peer_database_younger_than_the_interval_is_reused_after_a_store_change() {
        let t = TestDir::new("peerreuse");
        // B holds an older database and is past its interval.
        let mut b = lazy(&t, 900);
        b.build(NOW_I - 1000).unwrap();
        let mut a = Cache::new(&t.cache_dir(), b.store_path()).unwrap();
        a.build(NOW_I).unwrap();
        touch(a.store_path(), 1_000_000);
        assert_eq!(messages(&mut b, NOW_I + 5), 4);
        assert_eq!(b.builds, 1);
        assert_eq!(built_unix(&b), Some(NOW_I));
    }

    /// A cache over the store of `t` that holds nothing, after another
    /// cache built at `built` and the store changed again.
    fn after_peer_build(t: &TestDir, built: i64) -> Cache {
        let mut a = lazy(t, 900);
        a.build(built).unwrap();
        touch(a.store_path(), 1_000_000);
        Cache::new(&t.cache_dir(), a.store_path())
            .unwrap()
            .with_refresh_secs(900)
    }

    #[test]
    fn build_reuses_a_peer_database_only_while_it_is_young() {
        let t = TestDir::new("reuseyoung");
        let mut b = after_peer_build(&t, NOW_I);
        b.attempt(NOW_I + 5, true).unwrap();
        assert_eq!(b.builds, 0);
        assert_eq!(built_unix(&b), Some(NOW_I));
        assert_eq!(b.db_id, file_id(&b.db_path()));

        let t = TestDir::new("reuseold");
        let mut b = after_peer_build(&t, NOW_I);
        b.attempt(NOW_I + 900, true).unwrap();
        assert_eq!(b.builds, 1);
        assert_eq!(built_unix(&b), Some(NOW_I + 900));

        let t = TestDir::new("reusefuture");
        let mut b = after_peer_build(&t, NOW_I + 100);
        b.attempt(NOW_I, true).unwrap();
        assert_eq!(b.builds, 1);
        assert_eq!(built_unix(&b), Some(NOW_I));

        let t = TestDir::new("reuseforce");
        let mut b = after_peer_build(&t, NOW_I);
        b.force_refresh_at(NOW_I + 5).unwrap();
        assert_eq!(b.builds, 1);
        assert_eq!(built_unix(&b), Some(NOW_I + 5));
        assert_eq!(b.db_id, file_id(&b.db_path()));
    }

    #[test]
    fn db_info_reads_a_usable_database_only() {
        let t = TestDir::new("dbinfo");
        let store = t.store(&mail_store());
        let dir = t.cache_dir();
        assert_eq!(db_info(&dir, &store), None);
        let mut c = Cache::new(&dir, &store).unwrap();
        let info = c.build(NOW_I).unwrap();
        let got = db_info(&dir, &store).expect("a usable database");
        assert_eq!(got.built_unix, Some(NOW_I));
        assert_eq!(got.stamp, Some((info.store_len, info.store_mtime_ns)));
        // The store under another name resolves to the same file.
        let link = t.0.join("link.hxd");
        std::os::unix::fs::symlink(&store, &link).unwrap();
        assert_eq!(db_info(&dir, &link), Some(got));
        // Another store: not usable.
        let other = t.0.join("Other.hxd");
        std::fs::write(&other, mail_store()).unwrap();
        assert_eq!(db_info(&dir, &other), None);
    }

    /// Temporary files of a build of process `pid` in `dir`, SQLite side
    /// files included.
    fn temps_of(dir: &Path, pid: &str) {
        for f in ["store.copy-", "mail.db.tmp-"] {
            std::fs::write(dir.join(format!("{f}{pid}")), b"stale").unwrap();
        }
        std::fs::write(dir.join(format!("mail.db.tmp-{pid}-journal")), b"stale").unwrap();
    }

    #[test]
    fn dead_leftovers_are_removed_while_the_build_lock_is_free() {
        let t = TestDir::new("dead");
        let dir = t.cache_dir();
        ensure_dir(&dir).unwrap();
        temps_of(&dir, "999");
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        assert!(remove_dead_leftovers(&dir));
        assert_eq!(files(&dir), ["lock", "notes.txt"]);
    }

    #[test]
    fn dead_leftovers_are_kept_while_another_holds_the_build_lock() {
        let t = TestDir::new("busy");
        let dir = t.cache_dir();
        ensure_dir(&dir).unwrap();
        temps_of(&dir, "999");
        let held = lock(&dir).unwrap();
        let started = Instant::now();
        assert!(!remove_dead_leftovers(&dir));
        // Tried for a moment before giving up.
        assert!(started.elapsed() >= LEFTOVER_LOCK_RETRY);
        assert_eq!(
            files(&dir),
            [
                "lock",
                "mail.db.tmp-999",
                "mail.db.tmp-999-journal",
                "store.copy-999"
            ]
        );
        // Released shortly after the first attempt: taken, removed.
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        assert!(remove_dead_leftovers(&dir));
        release.join().unwrap();
        assert_eq!(files(&dir), ["lock"]);
    }

    #[test]
    fn own_temps_only_are_removed() {
        let t = TestDir::new("own");
        let dir = t.cache_dir();
        ensure_dir(&dir).unwrap();
        let pid = std::process::id().to_string();
        temps_of(&dir, &pid);
        temps_of(&dir, "999");
        // Another process whose pid starts with ours.
        temps_of(&dir, &format!("{pid}0"));
        remove_own_temps(&dir);
        let mut want = vec![
            format!("mail.db.tmp-{pid}0"),
            format!("mail.db.tmp-{pid}0-journal"),
            "mail.db.tmp-999".to_string(),
            "mail.db.tmp-999-journal".to_string(),
            format!("store.copy-{pid}0"),
            "store.copy-999".to_string(),
        ];
        want.sort();
        assert_eq!(files(&dir), want);
    }

    #[test]
    fn build_delay_pauses_after_the_store_copy() {
        let t = TestDir::new("delay");
        let dir = t.cache_dir();
        let delay = std::time::Duration::from_millis(300);
        let mut c = Cache::new(&dir, &t.store(&mail_store()))
            .unwrap()
            .with_build_delay(delay);
        let started = Instant::now();
        let build = std::thread::spawn(move || c.build(NOW_I).map(drop));
        let copy = dir.join(format!("store.copy-{}", std::process::id()));
        while !copy.exists() {
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The copy exists during the pause, and the build is not over.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(copy.exists());
        assert!(!build.is_finished());
        build.join().unwrap().unwrap();
        assert!(started.elapsed() >= delay);
        assert_eq!(files(&dir), ["lock", "mail.db"]);
    }

    // --- service mode (thin reader) ---

    const WAIT: Duration = Duration::from_millis(300);

    /// A cache over a [`mail_store`] file in `t` that asks `fake` instead of
    /// building, with a short first-build wait.
    fn served(t: &TestDir, fake: &FakeRefresher) -> Cache {
        lazy(t, 900)
            .with_refresher(Box::new(fake.clone()))
            .with_first_build_wait(WAIT, Duration::from_millis(10))
    }

    /// A status reply of a service that is building and has no database.
    fn building() -> ServiceStatus {
        ServiceStatus {
            building: true,
            ..fake_status(None)
        }
    }

    #[test]
    fn service_mode_touches_once_per_call_and_never_builds() {
        let t = TestDir::new("svctouch");
        let fake = FakeRefresher::new(Ok(fake_status(Some(NOW_I))));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        for dt in 0..3 {
            assert_eq!(messages(&mut c, NOW_I + dt), 4);
        }
        assert_eq!(fake.counts(), (3, 0, 0));
        // A changed store past the interval: still the service's business.
        touch(c.store_path(), 1_000_000);
        assert_eq!(messages(&mut c, NOW_I + 100_000), 4);
        assert_eq!(fake.counts(), (4, 0, 0));
        assert_eq!(c.builds, 0);
        let st = c.status();
        let view = st.service.expect("a service view");
        assert!(view.running);
        assert!(!view.building);
        assert_eq!(view.version.as_deref(), Some(crate::mcp::proto::VERSION));
        assert_eq!(view.pid, Some(4242));
        assert_eq!(view.last_error, None);
        assert_eq!(st.last_error, None);
        assert_eq!(st.store_changed, Some(true));
    }

    #[test]
    fn unfinished_build_text_of_the_service_is_no_error_while_it_builds() {
        let t = TestDir::new("svcunfinished");
        let fake = FakeRefresher::new(Ok(ServiceStatus {
            building: true,
            last_error: Some(BUILD_UNFINISHED.into()),
            ..fake_status(Some(NOW_I))
        }));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        messages(&mut c, NOW_I);
        assert_eq!(c.status().last_error, None);
        assert_eq!(c.status().service.unwrap().last_error, None);
        // Once the build is over it is the error of a build that broke off.
        fake.script().reply = Ok(ServiceStatus {
            last_error: Some(BUILD_UNFINISHED.into()),
            ..fake_status(Some(NOW_I))
        });
        messages(&mut c, NOW_I + 1);
        assert_eq!(c.status().last_error, Some(BUILD_UNFINISHED));
    }

    #[test]
    fn service_mode_reopens_a_replaced_database() {
        let t = TestDir::new("svcreopen");
        let fake = FakeRefresher::new(Ok(fake_status(Some(NOW_I))));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        messages(&mut c, NOW_I + 1);
        assert_eq!(built_unix(&c), Some(NOW_I));
        rebuild_in(&t.cache_dir(), NOW_I + 50);
        assert_eq!(messages(&mut c, NOW_I + 60), 4);
        assert_eq!(built_unix(&c), Some(NOW_I + 50));
        assert_eq!(c.db_id, file_id(&c.db_path()));
        assert!(c.db_id.is_some());
        assert_eq!(c.builds, 0);
    }

    #[test]
    fn service_mode_keeps_the_old_connection_for_a_rejected_file() {
        static N: AtomicUsize = AtomicUsize::new(0);
        let t = TestDir::new("svcreject");
        let fake = FakeRefresher::new(Ok(fake_status(Some(NOW_I))));
        let mut c = served(&t, &fake);
        let dir = t.cache_dir();
        rebuild_in(&dir, NOW_I);
        messages(&mut c, NOW_I);
        rebuild_in(&dir, NOW_I + 10);
        Connection::open(dir.join(DB_NAME))
            .unwrap()
            .execute(
                "UPDATE mcp_meta SET value = '0' WHERE key = 'format_version'",
                [],
            )
            .unwrap();
        c.reopen_hook = Some(|_| {
            N.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(messages(&mut c, NOW_I + 20), 4);
        assert_eq!(messages(&mut c, NOW_I + 30), 4);
        assert_eq!(N.load(Ordering::SeqCst), 1);
        assert_eq!(built_unix(&c), Some(NOW_I));
        assert_eq!(c.builds, 0);
    }

    #[test]
    fn unavailable_service_with_a_database_serves_from_disk() {
        let t = TestDir::new("svcdown");
        let why = "the refresh service did not start; see service.log";
        let fake = FakeRefresher::new(Err(why.into()));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(c.builds, 0);
        let st = c.status();
        assert_eq!(st.last_error, Some(why));
        let view = st.service.expect("a service view");
        assert!(!view.running);
        assert!(!view.building);
        assert_eq!(view.last_error.as_deref(), Some(why));
        assert_eq!(view.version, None);
        assert_eq!(view.pid, None);
    }

    #[test]
    fn unavailable_service_without_a_database_is_the_error() {
        let t = TestDir::new("svcdownnodb");
        let why = "the refresh service cannot read the store: permission denied";
        let fake = FakeRefresher::new(Err(why.into()));
        let mut c = served(&t, &fake);
        let started = Instant::now();
        assert_eq!(c.connection_at(NOW_I).err().as_deref(), Some(why));
        assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
        assert_eq!(fake.counts(), (1, 0, 0));
        assert_eq!(c.builds, 0);
        assert!(!c.db_path().exists());
    }

    #[test]
    fn first_call_waits_for_the_first_build() {
        let t = TestDir::new("svcfirst");
        let fake = FakeRefresher::new(Ok(building()));
        let dir = t.cache_dir();
        fake.script().action = Some(Box::new(move |op, n| {
            if op == FakeOp::Status && n == 3 {
                rebuild_in(&dir, NOW_I);
                return Some(Ok(fake_status(Some(NOW_I))));
            }
            None
        }));
        let mut c = served(&t, &fake);
        assert_eq!(messages(&mut c, NOW_I), 4);
        assert_eq!(built_unix(&c), Some(NOW_I));
        let (touches, statuses, _) = fake.counts();
        assert_eq!(touches, 1);
        assert!((3..=4).contains(&statuses), "{statuses}");
        assert_eq!(c.builds, 0);
    }

    #[test]
    fn failed_first_build_of_the_service_is_the_error_at_once() {
        let t = TestDir::new("svcfirstfail");
        let fake = FakeRefresher::new(Ok(building()));
        fake.script().action = Some(Box::new(|op, n| {
            (op == FakeOp::Status && n == 2).then(|| {
                Ok(ServiceStatus {
                    last_error: Some("export: boom".into()),
                    ..fake_status(None)
                })
            })
        }));
        let mut c = served(&t, &fake);
        let started = Instant::now();
        assert_eq!(
            c.connection_at(NOW_I).err().as_deref(),
            Some("export: boom")
        );
        assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
        assert_eq!(fake.counts(), (1, 2, 0));
        // Reported with building false right away: no status request.
        let fake = FakeRefresher::new(Ok(ServiceStatus {
            last_error: Some("export: boom".into()),
            ..fake_status(None)
        }));
        let mut c = served(&t, &fake);
        assert_eq!(
            c.connection_at(NOW_I).err().as_deref(),
            Some("export: boom")
        );
        assert_eq!(fake.counts(), (1, 0, 0));
    }

    #[test]
    fn first_build_that_does_not_come_is_an_error_after_the_wait() {
        let t = TestDir::new("svcfirstslow");
        let fake = FakeRefresher::new(Ok(building()));
        let mut c = served(&t, &fake);
        let started = Instant::now();
        assert_eq!(
            c.connection_at(NOW_I).err().as_deref(),
            Some(FIRST_BUILD_PENDING)
        );
        assert!(started.elapsed() >= WAIT, "{:?}", started.elapsed());
        assert!(started.elapsed() < WAIT * 5, "{:?}", started.elapsed());
        assert!(fake.counts().1 >= 5, "{:?}", fake.counts());
    }

    #[test]
    fn database_of_a_newer_format_with_a_newer_service_asks_for_a_restart() {
        let t = TestDir::new("svcnewer");
        let newer = ServiceStatus {
            version: "99.0.0".into(),
            ..fake_status(Some(NOW_I))
        };
        let fake = FakeRefresher::new(Ok(newer));
        let mut c = served(&t, &fake);
        let dir = t.cache_dir();
        rebuild_in(&dir, NOW_I);
        Connection::open(dir.join(DB_NAME))
            .unwrap()
            .execute(
                "UPDATE mcp_meta SET value = '99' WHERE key = 'format_version'",
                [],
            )
            .unwrap();
        let started = Instant::now();
        assert_eq!(c.connection_at(NOW_I).err().as_deref(), Some(OLDER_SERVER));
        assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
        // With a service of this version the file is its to rebuild: wait.
        let fake = FakeRefresher::new(Ok(building()));
        let mut c = served(&t, &fake);
        assert_eq!(
            c.connection_at(NOW_I).err().as_deref(),
            Some(FIRST_BUILD_PENDING)
        );
    }

    #[test]
    fn force_refresh_asks_the_service_and_serves_the_new_database() {
        let t = TestDir::new("svcrefresh");
        let fake = FakeRefresher::new(Ok(fake_status(Some(NOW_I))));
        let dir = t.cache_dir();
        fake.script().action = Some(Box::new(move |op, _| {
            if op == FakeOp::Refresh {
                rebuild_in(&dir, NOW_I + 100);
            }
            None
        }));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        messages(&mut c, NOW_I);
        c.force_refresh_at(NOW_I + 100).unwrap();
        assert_eq!(fake.counts(), (1, 0, 1));
        // In use at once, before the next call.
        assert_eq!(built_unix(&c), Some(NOW_I + 100));
        assert_eq!(c.db_id, file_id(&c.db_path()));
        assert_eq!(c.builds, 0);
    }

    #[test]
    fn failed_refresh_of_the_service_is_the_error_and_keeps_the_database() {
        let t = TestDir::new("svcrefreshfail");
        let fake = FakeRefresher::new(Ok(fake_status(Some(NOW_I))));
        fake.script().refresh_reply = Some(Err("export: boom".into()));
        let mut c = served(&t, &fake);
        rebuild_in(&t.cache_dir(), NOW_I);
        messages(&mut c, NOW_I);
        assert_eq!(c.force_refresh_at(NOW_I + 5).unwrap_err(), "export: boom");
        assert_eq!(built_unix(&c), Some(NOW_I));
        assert_eq!(messages(&mut c, NOW_I + 6), 4);
        assert_eq!(fake.counts(), (2, 0, 1));
        assert_eq!(c.builds, 0);
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
