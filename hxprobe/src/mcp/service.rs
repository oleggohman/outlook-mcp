//! `hxprobe service`: the one process per cache directory that builds
//! `mail.db`.
//!
//! [`Core`] is the refresh policy as a synchronous state machine: it never
//! touches a socket, a thread or the clock; every method takes the time as
//! `now` (unix seconds). The work it decides on is done through a
//! [`Backend`].

use std::io;
use std::sync::Arc;

use serde_json::Value;

use crate::mcp::cache::{DbInfo, FileId};
use crate::mcp::proto::{self, Request, ServiceStatus, UNUSABLE_REBUILD_SECS, VERSION};

/// `last_error` while a build runs; it stays when the build never reports
/// its end.
const BUILD_UNFINISHED: &str = "the last cache build did not finish (internal error)";

/// What the service works on: the live store and `mail.db`.
pub trait Backend {
    /// Length and modification time (ns) of the live store.
    fn store_stamp(&self) -> io::Result<(u64, i64)>;
    /// Identity of the `mail.db` file, without opening it; `None` when
    /// there is none.
    fn db_id(&self) -> Option<FileId>;
    /// Identity and build information of `mail.db` when it is usable for
    /// the store; `None` otherwise. Opens the database and closes it again.
    fn db(&self) -> Option<(FileId, DbInfo)>;
    /// Build `mail.db` from the store.
    fn build(&self, now: i64) -> Result<(), String>;
    /// Whether opening the store for reading fails for lack of permission.
    fn store_denied(&self) -> bool;
}

/// Why the service ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// No request for `idle_secs`.
    Idle,
    /// A `shutdown` request with the service's own version.
    Shutdown,
    /// `SIGTERM` or `SIGINT`.
    Signal,
    /// `service.sock` was removed or replaced.
    SocketGone,
    /// The store cannot be read for lack of permission.
    Permission,
}

/// Why a build starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildReason {
    /// There is no usable database.
    First,
    /// The store changed and a client is active.
    Changed,
    /// A `refresh` request.
    Refresh,
}

impl BuildReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BuildReason::First => "first",
            BuildReason::Changed => "changed",
            BuildReason::Refresh => "refresh",
        }
    }
}

/// What the service has to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Call [`Core::build_started`], then run [`Backend::build`] and report
    /// its end with [`Core::build_finished`].
    Build(BuildReason),
    /// Nothing until the next request, build end or tick.
    Wait,
    /// Stop serving.
    Exit(Exit),
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Send this at once.
    Send(Value),
    /// Wait until [`Core::generation`] reaches this value, then send
    /// [`Core::refresh_reply`].
    AfterBuild(u64),
}

/// The result of [`Core::handle`].
#[derive(Debug, Clone, PartialEq)]
pub struct Handled {
    pub reply: Reply,
    /// A build the request started ([`Core::build_started`] is already
    /// done): the caller runs [`Backend::build`].
    pub build: Option<BuildReason>,
}

/// The refresh policy of the service.
#[derive(Debug)]
pub struct Core<B> {
    backend: Arc<B>,
    store_path: String,
    refresh_secs: u64,
    /// 0: never exit for lack of requests.
    idle_secs: u64,
    started_unix: i64,
    building: bool,
    /// A `touch` arrived since the last build started.
    touched: bool,
    /// A `refresh` waits and no build runs.
    force: bool,
    last_attempt_unix: Option<i64>,
    last_error: Option<String>,
    /// Start, any request, end of a build.
    last_activity_unix: i64,
    /// Number of finished builds, failed ones included.
    generation: u64,
    last_result: Option<Result<(), String>>,
    stopping: Option<Exit>,
    /// The `mail.db` examined last: its identity (`None`: no file) and its
    /// build information (`None`: not usable).
    db_cache: Option<(Option<FileId>, Option<DbInfo>)>,
}

impl<B: Backend> Core<B> {
    /// The policy for `store_path`, started at `now`.
    pub fn new(
        backend: B,
        store_path: String,
        refresh_secs: u64,
        idle_secs: u64,
        now: i64,
    ) -> Core<B> {
        Core {
            backend: Arc::new(backend),
            store_path,
            refresh_secs,
            idle_secs,
            started_unix: now,
            building: false,
            touched: false,
            force: false,
            last_attempt_unix: None,
            last_error: None,
            last_activity_unix: now,
            generation: 0,
            last_result: None,
            stopping: None,
            db_cache: None,
        }
    }

    /// The backend, to run a build outside the policy's lock.
    pub fn backend(&self) -> Arc<B> {
        Arc::clone(&self.backend)
    }

    /// Number of finished builds, failed ones included.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What to do at `now`: evaluated after every request, after every
    /// build and on every tick.
    pub fn poll(&mut self, now: i64) -> Action {
        if let Some(e) = self.stopping {
            return Action::Exit(e);
        }
        if self.building {
            return Action::Wait;
        }
        if self.force {
            return Action::Build(BuildReason::Refresh);
        }
        match self.db() {
            None => {
                // An attempt time after `now` does not count. After a
                // successful build that left no usable database, wait a
                // little: another program may be replacing it with its own.
                let recent = |secs: i64| {
                    self.last_attempt_unix
                        .is_some_and(|at| (0..secs).contains(&now.saturating_sub(at)))
                };
                let blocked = if self.last_error.is_some() {
                    recent(secs(self.refresh_secs))
                } else {
                    recent(UNUSABLE_REBUILD_SECS)
                };
                if !blocked {
                    return Action::Build(BuildReason::First);
                }
            }
            Some(db) => match self.backend.store_stamp() {
                // A stamp that cannot be read counts as not changed.
                Err(_) => {
                    if self.backend.store_denied() {
                        return self.stop(Exit::Permission);
                    }
                }
                Ok(stamp) => {
                    let changed = db.stamp != Some(stamp);
                    // A time after `now` would put the rebuild off for good.
                    let past = |t: Option<i64>| t.filter(|t| *t <= now).unwrap_or(i64::MIN);
                    let since = past(db.built_unix).max(past(self.last_attempt_unix));
                    if changed
                        && self.touched
                        && now.saturating_sub(since) >= secs(self.refresh_secs)
                    {
                        return Action::Build(BuildReason::Changed);
                    }
                }
            },
        }
        if self.idle_secs > 0 && now.saturating_sub(self.last_activity_unix) >= secs(self.idle_secs)
        {
            return self.stop(Exit::Idle);
        }
        Action::Wait
    }

    /// Record `reason` as the end of the service.
    fn stop(&mut self, reason: Exit) -> Action {
        Action::Exit(*self.stopping.get_or_insert(reason))
    }

    /// Record the start of a build that [`Core::poll`] asked for.
    pub fn build_started(&mut self, now: i64) {
        self.building = true;
        self.touched = false;
        self.force = false;
        self.last_attempt_unix = Some(now);
        // Kept when the build never reports its end.
        self.last_error = Some(BUILD_UNFINISHED.to_string());
    }

    /// Record the end of a build. A failure for lack of permission on the
    /// store ends the service.
    pub fn build_finished(&mut self, result: Result<(), String>, now: i64) {
        self.building = false;
        self.last_error = result.as_ref().err().cloned();
        if result.is_err() && self.stopping.is_none() && self.backend.store_denied() {
            self.stopping = Some(Exit::Permission);
        }
        self.last_result = Some(result);
        self.generation += 1;
        self.last_activity_unix = now;
    }

    /// Apply a request received at `now`. `touch` and `refresh` may start
    /// a build; `refresh` waits for the running build or the one it starts.
    pub fn handle(&mut self, req: &Request, now: i64) -> Handled {
        self.last_activity_unix = now;
        let mut build = None;
        let reply = match req {
            Request::Status => Reply::Send(self.status().to_json()),
            Request::Touch => {
                self.touched = true;
                build = self.start_if_due(now);
                Reply::Send(self.status().to_json())
            }
            Request::Refresh => {
                if !self.building {
                    self.force = true;
                }
                build = self.start_if_due(now);
                Reply::AfterBuild(self.generation + 1)
            }
            Request::Shutdown { version } if version == VERSION => {
                self.stopping.get_or_insert(Exit::Shutdown);
                Reply::Send(proto::stopping_reply())
            }
            Request::Shutdown { .. } => Reply::Send(proto::version_mismatch_reply(VERSION)),
        };
        Handled { reply, build }
    }

    /// Start a build when [`Core::poll`] asks for one.
    fn start_if_due(&mut self, now: i64) -> Option<BuildReason> {
        match self.poll(now) {
            Action::Build(reason) => {
                self.build_started(now);
                Some(reason)
            }
            _ => None,
        }
    }

    /// The reply to a `refresh` whose build has finished: the status, with
    /// `ok` false and the error when that build failed.
    pub fn refresh_reply(&mut self) -> Value {
        let st = self.status();
        match &self.last_result {
            Some(Err(e)) => st.failed(e).to_json(),
            _ => st.to_json(),
        }
    }

    /// The status reply.
    pub fn status(&mut self) -> ServiceStatus {
        let db = self.db();
        let store_changed =
            db.and_then(|d| self.backend.store_stamp().ok().map(|s| d.stamp != Some(s)));
        ServiceStatus {
            ok: true,
            error: None,
            version: VERSION.to_string(),
            pid: std::process::id(),
            store_path: Some(self.store_path.clone()),
            building: self.building,
            db_usable: db.is_some(),
            built_unix: db.and_then(|d| d.built_unix),
            store_changed,
            last_attempt_unix: self.last_attempt_unix,
            last_error: self.last_error.clone(),
            refresh_secs: Some(self.refresh_secs),
            idle_secs: Some(self.idle_secs),
            started_unix: Some(self.started_unix),
            builds: self.generation,
        }
    }

    /// Build information of the usable `mail.db`, read again only when the
    /// file's identity changed (also when it was not usable).
    fn db(&mut self) -> Option<DbInfo> {
        let id = self.backend.db_id();
        if let Some((cached, info)) = self.db_cache {
            if cached == id {
                return info;
            }
        }
        let entry = match id {
            None => (None, None),
            Some(_) => match self.backend.db() {
                Some((id, info)) => (Some(id), Some(info)),
                None => (id, None),
            },
        };
        self.db_cache = Some(entry);
        entry.1
    }
}

/// Seconds as a time difference.
fn secs(s: u64) -> i64 {
    i64::try_from(s).unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    const NOW: i64 = 1_800_000_000;
    const STAMP: (u64, i64) = (100, 5_000);
    const STAMP2: (u64, i64) = (200, 9_000);
    const STORE: &str = "/s/HxStore.hxd";
    const DAY: i64 = 86_400;

    /// Scripted state of a [`FakeBackend`].
    #[derive(Debug)]
    pub(crate) struct FakeState {
        /// `None`: the stamp cannot be read.
        pub stamp: Option<(u64, i64)>,
        pub db_id: Option<FileId>,
        /// `None`: the file at `db_id` is not usable.
        pub db: Option<DbInfo>,
        pub denied: bool,
        /// `Some(e)`: builds fail with `e`.
        pub fail: Option<String>,
        /// Calls of `build`.
        pub builds: u64,
        /// Calls of `db`.
        pub db_reads: u64,
    }

    /// A backend whose answers the test sets.
    #[derive(Debug)]
    pub(crate) struct FakeBackend(pub Mutex<FakeState>);

    impl FakeBackend {
        pub fn new() -> FakeBackend {
            FakeBackend(Mutex::new(FakeState {
                stamp: Some(STAMP),
                db_id: None,
                db: None,
                denied: false,
                fail: None,
                builds: 0,
                db_reads: 0,
            }))
        }

        pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
            f(&mut self.0.lock().unwrap());
        }

        pub fn get<T>(&self, f: impl FnOnce(&FakeState) -> T) -> T {
            f(&self.0.lock().unwrap())
        }
    }

    impl Backend for FakeBackend {
        fn store_stamp(&self) -> io::Result<(u64, i64)> {
            self.get(|s| s.stamp)
                .ok_or_else(|| io::Error::other("no stamp"))
        }

        fn db_id(&self) -> Option<FileId> {
            self.get(|s| s.db_id)
        }

        fn db(&self) -> Option<(FileId, DbInfo)> {
            let mut s = self.0.lock().unwrap();
            s.db_reads += 1;
            s.db_id.zip(s.db)
        }

        fn build(&self, _now: i64) -> Result<(), String> {
            let mut s = self.0.lock().unwrap();
            s.builds += 1;
            match &s.fail {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }

        fn store_denied(&self) -> bool {
            self.get(|s| s.denied)
        }
    }

    fn id(n: u64) -> FileId {
        FileId {
            dev: 1,
            ino: n,
            len: 4096,
            mtime_ns: 0,
        }
    }

    /// A usable database (file `ino`) built at `built` from `stamp`.
    fn put_db(core: &Core<FakeBackend>, ino: u64, built: i64, stamp: (u64, i64)) {
        core.backend().set(|s| {
            s.db_id = Some(id(ino));
            s.db = Some(DbInfo {
                built_unix: Some(built),
                stamp: Some(stamp),
            });
        });
    }

    fn core(refresh_secs: u64, idle_secs: u64) -> Core<FakeBackend> {
        Core::new(
            FakeBackend::new(),
            STORE.into(),
            refresh_secs,
            idle_secs,
            NOW,
        )
    }

    /// A core with an up-to-date database built at `NOW`.
    fn fresh(refresh_secs: u64, idle_secs: u64) -> Core<FakeBackend> {
        let c = core(refresh_secs, idle_secs);
        put_db(&c, 1, NOW, STAMP);
        c
    }

    /// Run one build from `start` to `end` as the runtime does.
    fn run_build(c: &mut Core<FakeBackend>, start: i64, end: i64) {
        c.build_started(start);
        let r = c.backend().build(start);
        c.build_finished(r, end);
    }

    fn touch(c: &mut Core<FakeBackend>, now: i64) -> Handled {
        c.handle(&Request::Touch, now)
    }

    fn sent(h: &Handled) -> &Value {
        match &h.reply {
            Reply::Send(v) => v,
            r => panic!("expected an immediate reply, got {r:?}"),
        }
    }

    // --- poll ---

    #[test]
    fn no_database_builds_at_start_without_a_touch() {
        let mut c = core(900, 1800);
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        // Also when a file exists that is not usable.
        let mut c = core(900, 1800);
        c.backend().set(|s| s.db_id = Some(id(1)));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
    }

    #[test]
    fn unchanged_store_never_builds() {
        let mut c = fresh(900, 0);
        for t in [NOW, NOW + 900, NOW + DAY, NOW + 30 * DAY] {
            let h = touch(&mut c, t);
            assert_eq!(h.build, None);
            assert_eq!(c.poll(t), Action::Wait);
        }
    }

    #[test]
    fn changed_store_without_a_touch_never_builds() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        for t in [NOW, NOW + 900, NOW + DAY, NOW + 30 * DAY] {
            assert_eq!(c.poll(t), Action::Wait);
        }
    }

    #[test]
    fn changed_store_with_a_touch_waits_for_the_interval() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        let h = touch(&mut c, NOW + 10);
        assert_eq!(h.build, None);
        assert_eq!(c.poll(NOW + 10), Action::Wait);
        assert_eq!(c.poll(NOW + 899), Action::Wait);
        // The touch is remembered until the interval has passed.
        assert_eq!(c.poll(NOW + 900), Action::Build(BuildReason::Changed));
    }

    #[test]
    fn a_touch_at_the_interval_starts_the_build() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        let h = touch(&mut c, NOW + 900);
        assert_eq!(h.build, Some(BuildReason::Changed));
        assert_eq!(sent(&h)["building"], true);
    }

    #[test]
    fn refresh_secs_zero_builds_on_the_first_touch() {
        let mut c = fresh(0, 0);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(c.poll(NOW), Action::Wait);
        let h = touch(&mut c, NOW);
        assert_eq!(h.build, Some(BuildReason::Changed));
    }

    #[test]
    fn a_touch_during_a_build_counts_for_the_next_round() {
        for touch_during in [true, false] {
            let mut c = fresh(900, 0);
            c.backend().set(|s| s.stamp = Some(STAMP2));
            assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
            if touch_during {
                assert_eq!(touch(&mut c, NOW + 901).build, None);
            }
            assert_eq!(c.poll(NOW + 902), Action::Wait);
            let r = c.backend().build(NOW + 900);
            // The build saw STAMP2; the store changed again meanwhile.
            put_db(&c, 2, NOW + 900, STAMP2);
            c.backend().set(|s| s.stamp = Some(STAMP));
            c.build_finished(r, NOW + 905);
            assert_eq!(c.poll(NOW + 905), Action::Wait);
            let want = if touch_during {
                Action::Build(BuildReason::Changed)
            } else {
                Action::Wait
            };
            assert_eq!(
                c.poll(NOW + 1800),
                want,
                "touch during build: {touch_during}"
            );
        }
    }

    #[test]
    fn database_or_attempt_time_after_now_does_not_block() {
        // A database built "in the future".
        let mut c = core(900, 0);
        put_db(&c, 1, NOW + 10 * DAY, STAMP);
        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(touch(&mut c, NOW).build, Some(BuildReason::Changed));

        // A failed attempt "in the future".
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = Some(STAMP2);
            s.fail = Some("boom".into());
        });
        run_build(&mut c, NOW + 10 * DAY, NOW + 10 * DAY);
        // Only the database's own time counts.
        assert_eq!(touch(&mut c, NOW + 899).build, None);
        assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
    }

    #[test]
    fn unreadable_store_stamp_builds_nothing() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.stamp = None);
        assert_eq!(touch(&mut c, NOW + 10 * DAY).build, None);
        assert_eq!(c.poll(NOW + 20 * DAY), Action::Wait);
        assert_eq!(c.status().store_changed, None);
    }

    // --- failures and exits ---

    #[test]
    fn failed_first_build_keeps_the_error_and_waits_for_the_interval() {
        let mut c = core(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        run_build(&mut c, NOW, NOW + 2);
        assert_eq!(c.status().last_error.as_deref(), Some("boom"));
        assert_eq!(touch(&mut c, NOW + 899).build, None);
        assert_eq!(c.poll(NOW + 899), Action::Wait);
        assert_eq!(c.status().last_error.as_deref(), Some("boom"));
        assert_eq!(c.poll(NOW + 900), Action::Build(BuildReason::First));
        assert_eq!(c.backend().get(|s| s.builds), 1);
    }

    #[test]
    fn failed_build_keeps_the_old_database_and_waits_for_the_interval() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = Some(STAMP2);
            s.fail = Some("boom".into());
        });
        assert_eq!(touch(&mut c, NOW + 900).build, Some(BuildReason::Changed));
        let r = c.backend().build(NOW + 900);
        c.build_finished(r, NOW + 903);
        let st = c.status();
        assert_eq!(st.last_error.as_deref(), Some("boom"));
        assert!(st.db_usable);
        assert_eq!(st.built_unix, Some(NOW));
        assert_eq!(touch(&mut c, NOW + 1000).build, None);
        assert_eq!(c.poll(NOW + 1799), Action::Wait);
        assert_eq!(c.poll(NOW + 1800), Action::Build(BuildReason::Changed));
    }

    #[test]
    fn unusable_database_after_a_successful_build_is_rebuilt_once_per_minute() {
        for file in [false, true] {
            let mut c = core(900, 0);
            if file {
                // A file that the build left but that is not usable.
                c.backend().set(|s| s.db_id = Some(id(7)));
            }
            assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
            run_build(&mut c, NOW, NOW + 1);
            assert_eq!(c.status().last_error, None);
            assert_eq!(c.poll(NOW + 59), Action::Wait, "file: {file}");
            assert_eq!(c.poll(NOW + 60), Action::Build(BuildReason::First));
        }
    }

    #[test]
    fn store_permission_error_exits_at_poll() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| {
            s.stamp = None;
            s.denied = true;
        });
        assert_eq!(c.poll(NOW), Action::Exit(Exit::Permission));
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Permission));
    }

    #[test]
    fn store_permission_error_exits_after_a_failed_build() {
        let mut c = core(900, 0);
        c.backend()
            .set(|s| s.fail = Some("permission denied".into()));
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        c.build_started(NOW);
        c.backend().set(|s| s.denied = true);
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 1);
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Permission));

        // A failure for another reason does not end the service.
        let mut c = core(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        run_build(&mut c, NOW, NOW + 1);
        assert_eq!(c.poll(NOW + 1), Action::Wait);
    }

    #[test]
    fn idle_exit_counts_from_the_last_request() {
        let mut c = fresh(900, 100);
        assert_eq!(c.poll(NOW + 99), Action::Wait);
        c.handle(&Request::Status, NOW + 50);
        assert_eq!(c.poll(NOW + 149), Action::Wait);
        assert_eq!(c.poll(NOW + 150), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_exit_waits_for_a_build_and_counts_from_its_end() {
        let mut c = core(900, 100);
        assert_eq!(c.poll(NOW), Action::Build(BuildReason::First));
        c.build_started(NOW);
        assert_eq!(c.poll(NOW + 500), Action::Wait);
        put_db(&c, 1, NOW, STAMP);
        c.build_finished(Ok(()), NOW + 500);
        assert_eq!(c.poll(NOW + 599), Action::Wait);
        assert_eq!(c.poll(NOW + 600), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_exit_also_without_a_database_after_a_failed_build() {
        let mut c = core(900, 100);
        c.backend().set(|s| s.fail = Some("boom".into()));
        run_build(&mut c, NOW, NOW + 1);
        assert_eq!(c.poll(NOW + 100), Action::Wait);
        assert_eq!(c.poll(NOW + 101), Action::Exit(Exit::Idle));
    }

    #[test]
    fn idle_secs_zero_never_exits() {
        let mut c = fresh(900, 0);
        assert_eq!(c.poll(NOW + 3650 * DAY), Action::Wait);
    }

    // --- requests ---

    #[test]
    fn touch_returns_the_status() {
        let mut c = fresh(900, 0);
        let h = touch(&mut c, NOW + 5);
        assert_eq!(h.build, None);
        let v = sent(&h);
        assert_eq!(v["ok"], true);
        assert_eq!(v["building"], false);
        assert_eq!(v["version"], VERSION);

        // A touch that starts a build says so; the build is not run by
        // the core.
        let mut c = core(900, 0);
        let h = touch(&mut c, NOW);
        assert_eq!(h.build, Some(BuildReason::First));
        assert_eq!(sent(&h)["building"], true);
        assert_eq!(c.backend().get(|s| s.builds), 0);
        assert_eq!(c.poll(NOW), Action::Wait);
    }

    #[test]
    fn refresh_builds_whatever_the_interval_and_returns_the_generation() {
        let mut c = fresh(900, 0);
        let h = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(h.build, Some(BuildReason::Refresh));
        assert_eq!(h.reply, Reply::AfterBuild(1));
        assert_eq!(c.status().last_attempt_unix, Some(NOW + 1));
        let r = c.backend().build(NOW + 1);
        c.build_finished(r, NOW + 2);
        assert_eq!(c.generation(), 1);
        let v = c.refresh_reply();
        assert_eq!(v["ok"], true);
        assert_eq!(v["builds"], 1);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn refresh_joins_a_running_build() {
        let mut c = fresh(900, 0);
        let a = c.handle(&Request::Refresh, NOW);
        let b = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(a.build, Some(BuildReason::Refresh));
        assert_eq!(b.build, None);
        assert_eq!(a.reply, Reply::AfterBuild(1));
        assert_eq!(b.reply, Reply::AfterBuild(1));
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 2);
        // No second build for the second request.
        assert_eq!(c.poll(NOW + 2), Action::Wait);
        assert_eq!(c.backend().get(|s| s.builds), 1);

        // A refresh during a build started by a touch joins it too.
        let mut c = core(900, 0);
        assert_eq!(touch(&mut c, NOW).build, Some(BuildReason::First));
        let h = c.handle(&Request::Refresh, NOW + 1);
        assert_eq!(h.build, None);
        assert_eq!(h.reply, Reply::AfterBuild(1));
    }

    #[test]
    fn refresh_reply_of_a_failed_build() {
        let mut c = fresh(900, 0);
        c.backend().set(|s| s.fail = Some("boom".into()));
        assert_eq!(c.handle(&Request::Refresh, NOW).reply, Reply::AfterBuild(1));
        let r = c.backend().build(NOW);
        c.build_finished(r, NOW + 1);
        let v = c.refresh_reply();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "boom");
        assert_eq!(v["last_error"], "boom");
        assert_eq!(v["db_usable"], true);
        assert_eq!(v["pid"], std::process::id());
    }

    #[test]
    fn shutdown_with_the_own_version_stops() {
        let mut c = fresh(900, 0);
        let h = c.handle(
            &Request::Shutdown {
                version: VERSION.into(),
            },
            NOW,
        );
        assert_eq!(h.reply, Reply::Send(proto::stopping_reply()));
        assert_eq!(h.build, None);
        assert_eq!(c.poll(NOW), Action::Exit(Exit::Shutdown));

        // Also while a build runs.
        let mut c = core(900, 0);
        c.build_started(NOW);
        c.handle(
            &Request::Shutdown {
                version: VERSION.into(),
            },
            NOW + 1,
        );
        assert_eq!(c.poll(NOW + 1), Action::Exit(Exit::Shutdown));
    }

    #[test]
    fn shutdown_with_another_version_is_refused() {
        let mut c = fresh(900, 0);
        let h = c.handle(
            &Request::Shutdown {
                version: "0.0.1".into(),
            },
            NOW,
        );
        assert_eq!(h.reply, Reply::Send(proto::version_mismatch_reply(VERSION)));
        assert_eq!(c.poll(NOW), Action::Wait);
    }

    #[test]
    fn status_fields() {
        let mut c = fresh(900, 1800);
        let h = c.handle(&Request::Status, NOW + 3);
        assert_eq!(h.build, None);
        let st = ServiceStatus::from_json(sent(&h)).unwrap();
        assert_eq!(
            st,
            ServiceStatus {
                ok: true,
                error: None,
                version: VERSION.into(),
                pid: std::process::id(),
                store_path: Some(STORE.into()),
                building: false,
                db_usable: true,
                built_unix: Some(NOW),
                store_changed: Some(false),
                last_attempt_unix: None,
                last_error: None,
                refresh_secs: Some(900),
                idle_secs: Some(1800),
                started_unix: Some(NOW),
                builds: 0,
            }
        );

        c.backend().set(|s| s.stamp = Some(STAMP2));
        assert_eq!(c.status().store_changed, Some(true));

        // During a build, and after it.
        c.build_started(NOW + 10);
        let st = c.status();
        assert!(st.building);
        assert_eq!(st.last_attempt_unix, Some(NOW + 10));
        assert_eq!(st.last_error.as_deref(), Some(BUILD_UNFINISHED));
        c.build_finished(Ok(()), NOW + 12);
        let st = c.status();
        assert!(!st.building);
        assert_eq!(st.last_error, None);
        assert_eq!(st.builds, 1);

        // No database.
        let st = core(900, 1800).status();
        assert!(!st.db_usable);
        assert_eq!(st.built_unix, None);
        assert_eq!(st.store_changed, None);
    }

    #[test]
    fn database_info_is_read_again_only_when_the_file_changed() {
        let mut c = fresh(900, 0);
        for t in 0..5 {
            c.poll(NOW + t);
            c.status();
        }
        assert_eq!(c.backend().get(|s| s.db_reads), 1);
        put_db(&c, 2, NOW + 10, STAMP2);
        c.poll(NOW + 10);
        assert_eq!(c.backend().get(|s| s.db_reads), 2);
        assert_eq!(c.status().built_unix, Some(NOW + 10));

        // An unusable file is not opened again on every poll either.
        let mut c = core(900, 0);
        c.backend().set(|s| s.db_id = Some(id(3)));
        run_build(&mut c, NOW, NOW);
        for t in 1..5 {
            c.poll(NOW + t);
        }
        assert_eq!(c.backend().get(|s| s.db_reads), 1);
    }
}
