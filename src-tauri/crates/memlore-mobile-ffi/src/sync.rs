//! Mobile sync (M2a): the Drive credential store the engine's sync and
//! connect flows read through, the platform hooks of the engine's sync
//! cycle, the status and events Swift observes, the scheduler's
//! memory-only state, and the one background sync task that `lock()` ends
//! before the keys go.
//!
//! Every exported call returns at once: the cycle runs as a task on the FFI
//! runtime, off the caller's queue, and reports through [`SyncObserver`].
//!
//! Cancel safety (plan task 5.6, ADR 13): `lock()`, `delete_vault` and
//! `drive_disconnect` abort the task, which drops the cycle's future at its
//! next await point. Every await of `run_sync_cycle` is cloud I/O (or a
//! `tokio::fs` call, which finishes on its blocking thread anyway), and no
//! DB transaction or mutex guard spans one (the engine denies
//! `clippy::await_holding_lock`), so a drop has the effect of an I/O error
//! at that await, minus the error branch. Every step after the engine
//! attempt already survives that: the keyring dirty re-upload writes in
//! crash-safe order and clears the dirty flag only after success; the cloud
//! content migration clears its flag last; the device-slot republish is
//! best-effort and its dirty flag is cleared after the write; the outbox
//! import records each intent in the same DB transaction that applies it;
//! the republish push marks rows synced one by one; the acks file is one
//! write that the next import rewrites. So a drop at worst redoes work on
//! the next sync. Abort is kept over a cooperative stop: it also drops the
//! cycle's key copies at once.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use memlore_engine::ai::indexer::EntryIndexer;
use memlore_engine::db;
use memlore_engine::ops::entries::maybe_mark_entry_embedding_dirty_after_save;
use memlore_engine::sync::cloud_config::{CloudCredentialStore, GDriveClientConfig};
use memlore_engine::sync::connect::{
    clear_gdrive_settings, reset_local_sync_state, startup_keyring_reconcile_if_needed,
    GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY,
};
use memlore_engine::sync::cycle::run_sync_cycle;
use memlore_engine::sync::engine::{
    CatchupProgressEvent, ProgressReporter, SyncProgressEvent, SyncProgressPhase,
};
use memlore_engine::sync::force_re_pair::get_force_re_pair_status_inner;
use memlore_engine::sync::gdrive_provider::GDriveSession;
use memlore_engine::sync::guard::SyncInProgressGuard;
use memlore_engine::sync::outbox_import::{OutboxBridgeEvent, OutboxEventSink};
use memlore_engine::sync::platform::{CloudConnectHooks, SyncPhase, SyncPlatformHooks};
use memlore_engine::sync::schedule::{
    evaluate_with_error_cursor, interval_hold_until_ms, record_failure, unix_now_s, RetryState,
    DEBOUNCE_WINDOW, SCHEDULER_SYNC_TRIGGER, TICK,
};
use memlore_engine::sync::{SyncSummary, SyncTrigger};
use memlore_engine::{AppState, EncryptionKeyState};
use zeroize::Zeroizing;

use crate::drive::MobileConnectHooks;
use crate::keystore::DriveTokenStore;
use tokio::task::JoinHandle;

use crate::{io, MemloreEngine, MemloreError, RUNTIME};

/// How long `lock()` waits for a cancelled sync task to end before it
/// clears the keys anyway.
const SYNC_CANCEL_WAIT: Duration = Duration::from_secs(2);

/// The mobile [`CloudCredentialStore`]: the refresh token lives in the
/// [`DriveTokenStore`] (the iOS Keychain), never in the vault DB, so every
/// method ignores `conn`. The OAuth client is the iOS client id Swift passes
/// to `configure_drive`; an iOS client has no secret.
pub(crate) struct MobileCredentialStore {
    tokens: Arc<dyn DriveTokenStore>,
    client_id: Mutex<Option<String>>,
}

impl MobileCredentialStore {
    pub(crate) fn new(tokens: Arc<dyn DriveTokenStore>) -> Self {
        Self {
            tokens,
            client_id: Mutex::new(None),
        }
    }

    pub(crate) fn set_client_id(&self, client_id: String) {
        *self.client_id.lock().unwrap_or_else(|p| p.into_inner()) = Some(client_id);
    }

    /// The configured OAuth client id, `None` before `configure_drive`.
    pub(crate) fn client_id(&self) -> Option<String> {
        self.client_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(|id| !id.is_empty())
    }

    pub(crate) fn tokens(&self) -> &dyn DriveTokenStore {
        self.tokens.as_ref()
    }
}

impl CloudCredentialStore for MobileCredentialStore {
    fn client_config(&self, _conn: &rusqlite::Connection) -> Result<GDriveClientConfig, String> {
        let client_id = self
            .client_id()
            .ok_or_else(|| "Google Drive is not configured on this device".to_string())?;
        Ok(GDriveClientConfig {
            client_id,
            client_secret: None,
        })
    }

    fn load_refresh_token(
        &self,
        _conn: &rusqlite::Connection,
    ) -> Result<Option<Zeroizing<String>>, String> {
        self.tokens.load()
    }

    fn store_refresh_token(&self, _conn: &rusqlite::Connection, token: &str) -> Result<(), String> {
        self.tokens.store(token)
    }

    fn clear_refresh_token(&self, _conn: &rusqlite::Connection) -> Result<(), String> {
        self.tokens.remove()
    }
}

/// The scheduler's memory-only state: the failure backoff cursor, the
/// on-launch rule (on mobile: the first tick after an unlock) and the
/// keyring reconcile owed to the first sync after an unlock.
#[derive(Default)]
pub(crate) struct MobileScheduler {
    pub(crate) retry: RetryState,
    pub(crate) on_launch_done: bool,
    pub(crate) reconcile_pending: bool,
}

/// Lifecycle phase of the last sync (the desktop `SyncPhase`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Enum)]
pub enum SyncPhaseFfi {
    #[default]
    Idle,
    Syncing,
    Synced,
    Error,
}

impl From<SyncPhase> for SyncPhaseFfi {
    fn from(phase: SyncPhase) -> Self {
        match phase {
            SyncPhase::Idle => Self::Idle,
            SyncPhase::Syncing => Self::Syncing,
            SyncPhase::Synced => Self::Synced,
            SyncPhase::Error => Self::Error,
        }
    }
}

/// Why sync cannot push until the user acts.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SyncBlockedFfi {
    /// The cloud vault was rotated by another device (or could not be
    /// verified): re-pair with the phrase (`re_pair`).
    NeedsRePair { reason: String },
    /// An authoritative recovery on another device moved the cloud ahead of
    /// this phone: nothing is pushed.
    Fenced,
    /// Google Drive must be connected again.
    NeedsReconnect,
}

/// The sync state Settings and the list show.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncStatusFfi {
    pub phase: SyncPhaseFfi,
    /// A sync task is running.
    pub running: bool,
    pub blocked: Option<SyncBlockedFfi>,
    /// The last sync's error, while `phase` is `Error`.
    pub error: Option<String>,
    pub last_sync_ms: Option<i64>,
    /// Entries waiting to be pushed.
    pub pending_count: u64,
}

/// Sync events for the app, implemented in Swift. Called from the sync
/// task's thread (or a runtime thread after a cancel), never from the
/// caller's queue; an implementation must not call back into the engine
/// synchronously. Every sync ends with an `on_status` whose `running` is
/// `false`.
#[uniffi::export(with_foreign)]
pub trait SyncObserver: Send + Sync {
    /// `step` is the desktop progress key (`pushing-entries`,
    /// `pulling-entries`, ..., `catch-up`, `importing-web-edits`).
    fn on_progress(&self, step: String, done: u64, total: u64);
    fn on_status(&self, status: SyncStatusFfi);
    /// Entries changed under the app (a pull or a web edit): reload lists.
    fn on_entries_changed(&self);
}

/// The last sync's outcome, memory only. `blocked` is read from the DB
/// (re-pair, reconnect), except `fenced`, which only the last cycle knows.
#[derive(Default)]
struct SyncRun {
    phase: SyncPhaseFfi,
    error: Option<String>,
    fenced: bool,
}

/// The observer Swift set and the last sync's outcome.
#[derive(Default)]
pub(crate) struct SyncReport {
    observer: Mutex<Option<Arc<dyn SyncObserver>>>,
    run: Mutex<SyncRun>,
}

impl SyncReport {
    fn observer(&self) -> Option<Arc<dyn SyncObserver>> {
        self.observer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn update(&self, f: impl FnOnce(&mut SyncRun)) {
        f(&mut self.run.lock().unwrap_or_else(|p| p.into_inner()));
    }

    /// The status as of now, read from `conn` and the memory outcome.
    fn status(&self, conn: &rusqlite::Connection, running: bool) -> Result<SyncStatusFfi, String> {
        let (phase, error, fenced) = {
            let run = self.run.lock().unwrap_or_else(|p| p.into_inner());
            (run.phase, run.error.clone(), run.fenced)
        };
        let scope_upgrade = db::get_setting(conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY)
            .map_err(|e| e.to_string())?
            .as_deref()
            == Some("1");
        let blocked = match get_force_re_pair_status_inner(conn)? {
            Some(reason) => Some(SyncBlockedFfi::NeedsRePair { reason }),
            None if scope_upgrade => Some(SyncBlockedFfi::NeedsReconnect),
            None => fenced.then_some(SyncBlockedFfi::Fenced),
        };
        Ok(SyncStatusFfi {
            phase,
            running,
            blocked,
            error,
            last_sync_ms: db::get_last_sync_at(conn)
                .map_err(|e| e.to_string())?
                .map(|secs| secs.saturating_mul(1000)),
            pending_count: db::count_pending_entries(conn).map_err(|e| e.to_string())?,
        })
    }

    /// The observer, if any, and the status to send it (read from
    /// `state`). No lock is held once this returns.
    fn snapshot(
        &self,
        state: &AppState,
        running: bool,
    ) -> Option<(Arc<dyn SyncObserver>, SyncStatusFfi)> {
        let observer = self.observer()?;
        match state.lock().and_then(|conn| self.status(&conn, running)) {
            Ok(status) => Some((observer, status)),
            Err(e) => {
                log::warn!("sync status: not read ({e})");
                None
            }
        }
    }

    /// Send the status to the observer, if any, on this (the sync task's)
    /// thread.
    fn publish(&self, state: &AppState, running: bool) {
        if let Some((observer, status)) = self.snapshot(state, running) {
            observer.on_status(status);
        }
    }
}

/// The engine's errors for a push the recovery fence refuses: a cloud
/// recovery generation or marker ahead of this phone, or a local recovery
/// in progress (`force_re_pair::ensure_safe_to_push_with_recovery_permit`,
/// `recovery::authorize_recovery_push`).
const FENCE_ERRORS: [&str; 4] = [
    "recovery generation mismatch",
    "authoritative recovery marker blocks normal sync",
    "recovery owner permit",
    "authoritative_recovery_in_progress",
];

fn is_fenced(error: &str) -> bool {
    FENCE_ERRORS.iter().any(|fence| error.contains(fence))
}

/// The desktop progress key of `phase` (its kebab-case serde name).
fn progress_step(phase: SyncProgressPhase) -> String {
    let mut step = String::new();
    for c in format!("{phase:?}").chars() {
        if c.is_ascii_uppercase() && !step.is_empty() {
            step.push('-');
        }
        step.push(c.to_ascii_lowercase());
    }
    step
}

/// Engine progress to the observer: reports and catch-up, never
/// heartbeats (they only feed the engine's stall guard).
struct MobileProgressReporter(Option<Arc<dyn SyncObserver>>);

impl ProgressReporter for MobileProgressReporter {
    fn report(&self, event: SyncProgressEvent) {
        if let Some(observer) = &self.0 {
            observer.on_progress(
                progress_step(event.phase),
                event.current.into(),
                event.total.into(),
            );
        }
    }

    fn report_catchup_progress(&self, event: CatchupProgressEvent) {
        if let Some(observer) = &self.0 {
            observer.on_progress("catch-up".to_string(), event.pulled, event.total);
        }
    }
}

/// The web outbox import's events: entries changed, import progress, and
/// the embedding mark the desktop's sink makes.
struct MobileOutboxSink<'a> {
    state: &'a AppState,
    indexer: Option<&'a EntryIndexer>,
    observer: Option<Arc<dyn SyncObserver>>,
}

impl OutboxEventSink for MobileOutboxSink<'_> {
    fn emit_bridge_event(&self, _event: &OutboxBridgeEvent) {
        if let Some(observer) = &self.observer {
            observer.on_entries_changed();
        }
    }

    fn emit_progress(&self, current: u32, total: u32) {
        if let Some(observer) = &self.observer {
            observer.on_progress(
                progress_step(SyncProgressPhase::ImportingWebEdits),
                current.into(),
                total.into(),
            );
        }
    }

    fn mark_embedding_dirty(&self, entry_id: &str) {
        if let Some(indexer) = self.indexer {
            maybe_mark_entry_embedding_dirty_after_save(self.state, indexer, entry_id);
        }
    }
}

/// The mobile [`SyncPlatformHooks`]: events to the [`SyncObserver`],
/// blocked states into the status, no deferred desktop work. Nothing here
/// blocks on a future: the hooks run inside the task's own future.
pub(crate) struct MobileSyncHooks<'a> {
    connect: MobileConnectHooks<'a>,
    report: &'a SyncReport,
    media_dir: PathBuf,
    indexer: Option<&'a EntryIndexer>,
    /// The cycle reported a terminal status (synced or error).
    terminal: AtomicBool,
}

impl<'a> MobileSyncHooks<'a> {
    pub(crate) fn new(
        connect: MobileConnectHooks<'a>,
        report: &'a SyncReport,
        media_dir: PathBuf,
        indexer: Option<&'a EntryIndexer>,
    ) -> Self {
        Self {
            connect,
            report,
            media_dir,
            indexer,
            terminal: AtomicBool::new(false),
        }
    }

    /// After the cycle: a cycle that failed before reporting (the push
    /// guards, the keyring checks) still ends in an error status, with the
    /// fence recognised; entries a pull brought in are announced.
    fn finish(&self, state: &AppState, result: &Result<SyncSummary, String>) {
        match result {
            Ok(summary) => {
                if summary.pulled > 0 {
                    if let Some(observer) = self.report.observer() {
                        observer.on_entries_changed();
                    }
                }
            }
            Err(e) => {
                self.report.update(|run| run.fenced = is_fenced(e));
                if !self.terminal.load(Ordering::SeqCst) {
                    self.on_status(SyncPhase::Error, state, Some(e.clone()));
                }
            }
        }
    }
}

impl CloudConnectHooks for MobileSyncHooks<'_> {
    fn on_connect_progress(&self, session_id: &str, step: &str) {
        self.connect.on_connect_progress(session_id, step);
    }

    fn set_live_session(&self, session: Option<GDriveSession>) {
        self.connect.set_live_session(session);
    }

    fn credentials(&self) -> &dyn CloudCredentialStore {
        self.connect.credentials()
    }
}

impl SyncPlatformHooks for MobileSyncHooks<'_> {
    fn progress_reporter(&self) -> Arc<dyn ProgressReporter> {
        Arc::new(MobileProgressReporter(self.report.observer()))
    }

    fn on_status(&self, phase: SyncPhase, state: &AppState, error: Option<String>) {
        if matches!(phase, SyncPhase::Synced | SyncPhase::Error) {
            self.terminal.store(true, Ordering::SeqCst);
        }
        self.report.update(|run| {
            run.phase = phase.into();
            run.error = error;
            if phase == SyncPhase::Synced {
                run.fenced = false;
            }
        });
        self.report.publish(state, true);
    }

    /// Nothing to do: the cycle (or the unlock reconcile, which the cycle
    /// always follows) already persisted the flags, and the error status
    /// the blocked cycle ends with reads them into `blocked`.
    fn on_force_re_pair(&self, _reason: &str) {}

    /// The desktop's scope-upgrade disconnect: requeue every synced row
    /// (its own transaction, first, so a crash before the clear only
    /// repeats this), then clear the Drive settings and set the reconnect
    /// flag in one transaction.
    fn on_scope_upgrade_required(&self, state: &AppState) {
        let result = state.lock().and_then(|conn| {
            reset_local_sync_state(&conn).map_err(|e| e.to_string())?;
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            clear_gdrive_settings(&tx, self.credentials())?;
            db::set_setting(&tx, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY, "1")
                .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => self.set_live_session(None),
            Err(e) => log::warn!("scope upgrade: the Drive settings were not cleared ({e})"),
        }
    }

    fn after_successful_cycle(&self, _state: &AppState, _key_state: &EncryptionKeyState) {}

    fn after_cycle(&self, _state: &AppState, _summary: &SyncSummary) -> Result<(), String> {
        Ok(())
    }

    fn outbox_sink<'b>(&'b self, state: &'b AppState) -> Box<dyn OutboxEventSink + 'b> {
        Box::new(MobileOutboxSink {
            state,
            indexer: self.indexer,
            observer: self.report.observer(),
        })
    }

    fn media_dir(&self) -> PathBuf {
        self.media_dir.clone()
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// Where sync events go. Replaces the previous observer.
    pub fn set_sync_observer(&self, observer: Arc<dyn SyncObserver>) {
        *self
            .sync_report
            .observer
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(observer);
    }

    /// Start a sync now and return at once (the observer reports it).
    /// `manual` (the user asked) also resets the failure backoff, like the
    /// desktop "Sync now". `false` when a sync is already running: that one
    /// reports instead. `Locked` while the keys are not loaded.
    pub fn sync_now(self: Arc<Self>, manual: bool) -> Result<bool, MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        if manual {
            self.scheduler().retry.reset();
            self.start_sync(SyncTrigger::Manual, false)
        } else {
            self.start_sync(SCHEDULER_SYNC_TRIGGER, true)
        }
    }

    /// One scheduler tick (every `TICK`, 5 s, while the app is active and
    /// unlocked): the desktop rules (on launch, on-save debounce, interval,
    /// failure backoff) decide whether a sync starts. `true` when one did.
    pub fn sync_tick(self: Arc<Self>) -> Result<bool, MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        let now_ms = unix_now_s().saturating_mul(1000);
        let (mut on_launch_done, backoff_until_ms) = {
            let scheduler = self.scheduler();
            (scheduler.on_launch_done, scheduler.retry.backoff_until_ms)
        };
        let decision = evaluate_with_error_cursor(
            &self.state,
            &self.last_edit,
            &mut on_launch_done,
            DEBOUNCE_WINDOW,
            backoff_until_ms,
        );
        self.scheduler().on_launch_done = on_launch_done;
        let reason = match decision {
            Ok(Some(reason)) => reason,
            Ok(None) => return Ok(false),
            Err(e) => {
                log::warn!("sync scheduler: evaluate failed: {e}");
                return Ok(false);
            }
        };
        log::info!("sync scheduler: firing sync ({reason:?})");
        let started = self.start_sync(SCHEDULER_SYNC_TRIGGER, true)?;
        if !started {
            // Another sync holds the slot: hold the rules for one tick, not
            // a failure (desktop run_loop).
            let tick_ms = i64::try_from(TICK.as_millis()).unwrap_or(i64::MAX);
            self.scheduler()
                .retry
                .hold_until(interval_hold_until_ms(now_ms, tick_ms));
        }
        Ok(started)
    }

    /// The app came to the foreground: let the next tick retry a failed
    /// sync before its backoff ends, at most once per `KICK_COOLDOWN_MS`.
    pub fn sync_kick(&self) {
        let now_ms = unix_now_s().saturating_mul(1000);
        self.scheduler().retry.kick(now_ms);
    }

    /// End a running sync, if any: abort it and wait at most
    /// `SYNC_CANCEL_WAIT` (the same path `lock()` uses). Keys, settings and
    /// the Drive token are untouched. Called when the app goes to the
    /// background without locking yet (ADR §13); the next foreground tick
    /// starts a fresh cycle.
    pub fn sync_cancel(&self) {
        drop(self.cancel_sync_and_wait());
    }

    /// The sync status as of now.
    pub fn sync_status(&self) -> Result<SyncStatusFfi, MemloreError> {
        let running = self
            .sync_task
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        self.with_unlocked(|conn| self.sync_report.status(conn, running).map_err(io))
    }
}

impl MemloreEngine {
    fn scheduler(&self) -> std::sync::MutexGuard<'_, MobileScheduler> {
        self.scheduler.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Forget the last sync's outcome (phase, error, fence) and the
    /// scheduler's state (backoff, on-launch and reconcile flags): a deleted
    /// vault's must not steer the next one.
    pub(crate) fn reset_sync_memory(&self) {
        self.sync_report.update(|run| *run = SyncRun::default());
        *self.scheduler() = MobileScheduler::default();
    }

    /// The hooks of this engine's sync cycle.
    pub(crate) fn sync_hooks(&self) -> MobileSyncHooks<'_> {
        MobileSyncHooks::new(
            self.connect_hooks(),
            &self.sync_report,
            self.db_path.with_file_name("media"),
            Some(&self.indexer),
        )
    }

    /// Spawn the sync task for `trigger`; `false` when another sync holds
    /// the slot. `record` feeds its outcome to the failure backoff (the
    /// scheduler's runs; a manual sync does not, as on desktop).
    fn start_sync(
        self: &Arc<Self>,
        trigger: SyncTrigger,
        record: bool,
    ) -> Result<bool, MemloreError> {
        let engine = Arc::clone(self);
        self.spawn_sync_task(async move { engine.run_sync_task(trigger, record).await })
    }

    /// The task body: the keyring reconcile owed since the last unlock
    /// (which fails closed on an inconclusive check, as at desktop
    /// startup), then one cycle, its final status and the backoff
    /// bookkeeping of the desktop scheduler loop.
    async fn run_sync_task(&self, trigger: SyncTrigger, record: bool) {
        let hooks = self.sync_hooks();
        let reconcile = self.scheduler().reconcile_pending;
        if reconcile {
            startup_keyring_reconcile_if_needed(&self.state, &self.key_state, &hooks).await;
            // Cleared only once done: a cancelled reconcile runs again.
            self.scheduler().reconcile_pending = false;
        }
        let result = run_sync_cycle(&self.state, &self.key_state, trigger, &hooks).await;
        hooks.finish(&self.state, &result);
        if record {
            record_outcome(&mut self.scheduler().retry, &result);
        }
    }

    /// Run `fut` as the one background sync task on the FFI runtime, off
    /// the caller's queue, holding the engine's single-sync guard
    /// (`SYNC_IN_PROGRESS`) until the task ends or is cancelled. `false`,
    /// and nothing spawned, while any sync holds the guard; `Locked` while
    /// the keys are not loaded. Once `fut` is done (whatever its outcome)
    /// the observer gets a last status with `running: false`; a cancel
    /// sends its own.
    ///
    /// The key check, the spawn and the stored handle all happen under the
    /// `sync_task` mutex, which `lock()` holds from the cancel to the key
    /// clear: a sync either starts before the lock (and is cancelled) or
    /// finds the keys gone. The task itself never takes that mutex.
    pub(crate) fn spawn_sync_task(
        self: &Arc<Self>,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> Result<bool, MemloreError> {
        let runtime = RUNTIME
            .get()
            .ok_or_else(|| io("The sync runtime is not running".to_string()))?;
        let mut slot = self.sync_task.lock().unwrap_or_else(|p| p.into_inner());
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        let Some(guard) = SyncInProgressGuard::try_acquire() else {
            return Ok(false);
        };
        let engine = Arc::clone(self);
        let handle = runtime.spawn(async move {
            {
                let _guard = guard;
                fut.await;
            }
            // After the guard is free: an observer that starts the next
            // sync at once is not refused.
            engine.sync_report.publish(&engine.state, false);
        });
        *slot = Some(handle);
        Ok(true)
    }

    /// End the running sync task, if any: abort it (its future, its key
    /// copies and the single-sync guard it holds are dropped at its next
    /// await point; safe at every one, see the module doc) and wait up to
    /// [`SYNC_CANCEL_WAIT`] for it to finish. Never fails: on a timeout the
    /// caller clears the keys anyway, and the cycle then fails for want of
    /// them. A cancelled sync leaves the phase idle, not "syncing", and the
    /// observer is told (`running: false`) on a runtime thread, never on the
    /// caller's queue. After a timed-out wait that `running: false` can
    /// arrive while the task is still winding down: the abort only lands at
    /// its next await.
    ///
    /// Returns the held (now empty) `sync_task` slot: no sync can start
    /// until the caller drops it (`lock()` keeps it to the key clear).
    pub(crate) fn cancel_sync_and_wait(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        let mut slot = self.sync_task.lock().unwrap_or_else(|p| p.into_inner());
        let Some(handle) = slot.take() else {
            return slot;
        };
        handle.abort();
        self.sync_report.update(|run| {
            if run.phase == SyncPhaseFfi::Syncing {
                run.phase = SyncPhaseFfi::Idle;
            }
        });
        let deadline = Instant::now() + SYNC_CANCEL_WAIT;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                log::error!("the sync task did not end within {SYNC_CANCEL_WAIT:?} of its cancel");
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if let (Some((observer, status)), Some(runtime)) =
            (self.sync_report.snapshot(&self.state, false), RUNTIME.get())
        {
            drop(runtime.spawn_blocking(move || observer.on_status(status)));
        }
        slot
    }
}

/// The desktop scheduler loop's bookkeeping of one finished cycle: a clean
/// sync resets the backoff, a failed one (or one with errors) opens the
/// next window, and a locked vault is no failure (the unlock retries).
fn record_outcome(retry: &mut RetryState, result: &Result<SyncSummary, String>) {
    match result {
        Ok(summary) if summary.errors.is_empty() => retry.reset(),
        Ok(summary) => record_failure(
            retry,
            &format!("sync completed with {} error(s)", summary.errors.len()),
        ),
        Err(e) if e.contains("Encryption key not initialized") => {}
        Err(e) => record_failure(retry, &format!("sync failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use super::test_support::{
        cloud_tree, connect_folder, device_id, doc_xml, engine_at, serial, sync_and_wait,
        DesktopRole, Recorder,
    };
    use super::*;
    use crate::drive::ConnectOutcomeFfi;
    use memlore_core::keyring_types::{DeviceSlotV2, KeyringMetaV2};
    use memlore_core::sync_control::SyncControlV1;
    use memlore_engine::db;
    use memlore_engine::sync::force_re_pair::get_force_re_pair_status_inner;

    /// Title, texts and the Yjs document (as XML) of an entry row.
    fn stored(conn: &rusqlite::Connection, id: &str) -> (Option<String>, Option<String>, String) {
        let entry = db::get_entry(conn, id)
            .unwrap()
            .expect("the entry is missing");
        let yjs = db::get_entry_content(conn, id).unwrap().unwrap_or_default();
        (entry.title, entry.content_text, doc_xml(&yjs))
    }

    /// The bytes of the one cloud file whose path ends with `suffix`.
    fn cloud_file<'a>(tree: &'a BTreeMap<String, Vec<u8>>, suffix: &str) -> &'a [u8] {
        let mut hits = tree.iter().filter(|(path, _)| path.ends_with(suffix));
        let (_, bytes) = hits
            .next()
            .unwrap_or_else(|| panic!("no cloud file {suffix}"));
        assert!(hits.next().is_none(), "two cloud files end with {suffix}");
        bytes
    }

    fn phone_slot(engine: &MemloreEngine, cloud: &Path) -> DeviceSlotV2 {
        let id = device_id(&engine.state);
        let tree = cloud_tree(cloud);
        let slot: DeviceSlotV2 =
            serde_json::from_slice(cloud_file(&tree, &format!("devices/{id}.json"))).unwrap();
        slot.validate().unwrap();
        assert_eq!(slot.device_id, id);
        slot
    }

    /// Golden gate phone -> desktop: a phone vault made through the FFI,
    /// with entries waiting in `sync_state` (M3a) and a version row,
    /// connects to an empty cloud (the connect publishes its keyring) and
    /// syncs through `sync_now`. A desktop joining with the phrase through
    /// the engine's `onboard_complete_inner` then pulls every entry and the
    /// version, decrypted. The phone's device slot and `_meta.json` parse
    /// under the strict memlore-core keyring types.
    #[test]
    fn golden_phone_vault_reaches_a_desktop_joining_with_the_phrase() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        let recorder = Arc::new(Recorder::default());
        phone.set_sync_observer(recorder.clone());
        let phrase = phone.create_vault("password12".to_string()).unwrap();
        let mut ids = Vec::new();
        for (title, text) in [("First", "phone body one"), ("Second", "phone body two")] {
            let id = phone.create_entry(title.to_string(), None).unwrap();
            crate::tests::write_text(&phone, &id, text);
            ids.push(id);
        }
        let version = phone
            .snapshot_entry_version(ids[0].clone())
            .unwrap()
            .expect("no version row");
        assert_eq!(
            db::count_pending_entries(&phone.state.lock().unwrap()).unwrap(),
            2,
            "the entries are not waiting in sync_state"
        );
        assert_eq!(
            connect_folder(&phone, cloud.path(), "golden-phone").unwrap(),
            ConnectOutcomeFfi::Connected
        );

        let status = sync_and_wait(&phone);
        assert_eq!(status.phase, SyncPhaseFfi::Synced, "{status:?}");
        assert_eq!(status.blocked, None);
        assert_eq!(status.pending_count, 0, "the phone pushed nothing");
        assert_eq!(
            recorder.last_status().map(|s| (s.phase, s.running)),
            Some((SyncPhaseFfi::Synced, false)),
            "the observer missed the end of the sync"
        );
        phone_slot(&phone, cloud.path());
        let meta: KeyringMetaV2 =
            serde_json::from_slice(cloud_file(&cloud_tree(cloud.path()), "keyring/_meta.json"))
                .unwrap();
        assert_eq!(meta.version, 2);

        let desktop = DesktopRole::join(cloud.path(), &phrase, "password34");
        let summary = desktop.sync();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        let phone_conn = phone.state.lock().unwrap();
        let desktop_conn = desktop.state.lock().unwrap();
        for id in &ids {
            assert_eq!(stored(&desktop_conn, id), stored(&phone_conn, id));
        }
        assert_eq!(
            db::get_entry_version_blob_for_entry(&desktop_conn, &version, &ids[0]).unwrap(),
            db::get_entry_version_blob_for_entry(&phone_conn, &version, &ids[0]).unwrap(),
            "the version did not reach the desktop"
        );
    }

    /// Golden gate desktop -> phone: the desktop creates the vault and
    /// pushes; the phone onboards with the phrase through the FFI and, after
    /// one FFI sync, holds the same titles and content, and is told its
    /// entries changed.
    #[test]
    fn golden_desktop_vault_reaches_a_phone_onboarding_with_the_phrase() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let desktop = DesktopRole::first_launch();
        let phrase = desktop.create_vault("password12");
        desktop.connect(cloud.path(), "golden-desktop");
        let ids = [
            desktop.write_entry("Desk one", "desktop body one"),
            desktop.write_entry("Desk two", "desktop body two"),
        ];
        let summary = desktop.sync();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        let recorder = Arc::new(Recorder::default());
        phone.set_sync_observer(recorder.clone());
        let session_id = "golden-onboard";
        assert_eq!(
            connect_folder(&phone, cloud.path(), session_id).unwrap(),
            ConnectOutcomeFfi::NeedsOnboarding {
                session_id: session_id.to_string()
            }
        );
        phone
            .onboard_complete(session_id.to_string(), phrase, "password34".to_string())
            .unwrap();

        let status = sync_and_wait(&phone);
        assert_eq!(status.phase, SyncPhaseFfi::Synced, "{status:?}");
        assert_eq!(status.blocked, None);
        for (id, title) in ids.iter().zip(["Desk one", "Desk two"]) {
            assert_eq!(
                phone.get_entry(id.clone()).unwrap().title.as_deref(),
                Some(title)
            );
            assert_eq!(
                stored(&phone.state.lock().unwrap(), id),
                stored(&desktop.state.lock().unwrap(), id)
            );
        }
        assert!(
            recorder.entries_changed.load(Ordering::SeqCst) > 0,
            "the observer was not told the entries changed"
        );
    }

    /// A peer rotation blocks the phone until it re-pairs: after the
    /// desktop rotates the keys, the phone's sync stops before any push
    /// (`NeedsRePair`, nothing written to the cloud, the flag persisted).
    /// `re_pair` with the phrase writes the phone's device slot anew
    /// (`created_at` newer than before) and lets sync resume: an entry the
    /// desktop wrote under the new epoch decrypts on the phone.
    #[test]
    fn peer_rotation_blocks_phone_sync_until_re_pair() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let desktop = DesktopRole::first_launch();
        let phrase = desktop.create_vault("password12");
        desktop.connect(cloud.path(), "rotation-desktop");
        desktop.write_entry("Before rotation", "old epoch body");
        assert!(desktop.sync().errors.is_empty());

        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        let recorder = Arc::new(Recorder::default());
        phone.set_sync_observer(recorder.clone());
        let session_id = "rotation-onboard";
        connect_folder(&phone, cloud.path(), session_id).unwrap();
        phone
            .onboard_complete(
                session_id.to_string(),
                phrase.clone(),
                "password34".to_string(),
            )
            .unwrap();
        assert_eq!(sync_and_wait(&phone).phase, SyncPhaseFfi::Synced);
        let old_slot = phone_slot(&phone, cloud.path());

        desktop.rotate("password12", &phrase);
        let rotated = desktop.write_entry("After rotation", "new epoch body");
        let summary = desktop.sync();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let mine = phone
            .create_entry("Phone after rotation".to_string(), None)
            .unwrap();
        let before = cloud_tree(cloud.path());
        let status = sync_and_wait(&phone);
        assert!(
            matches!(status.blocked, Some(SyncBlockedFfi::NeedsRePair { .. })),
            "{status:?}"
        );
        assert_eq!(status.phase, SyncPhaseFfi::Error);
        assert_eq!(cloud_tree(cloud.path()), before, "a blocked phone wrote");
        assert!(
            get_force_re_pair_status_inner(&phone.state.lock().unwrap())
                .unwrap()
                .is_some(),
            "the re-pair flag was not persisted"
        );
        assert!(
            matches!(
                recorder.last_status().and_then(|s| s.blocked),
                Some(SyncBlockedFfi::NeedsRePair { .. })
            ),
            "the observer was not told"
        );

        // `created_at` is in seconds.
        std::thread::sleep(Duration::from_millis(1100));
        phone.re_pair(phrase, "password56".to_string()).unwrap();
        let new_slot = phone_slot(&phone, cloud.path());
        assert!(
            new_slot.created_at > old_slot.created_at,
            "the re-pair kept the old slot ({} <= {})",
            new_slot.created_at,
            old_slot.created_at
        );
        let status = sync_and_wait(&phone);
        assert_eq!(status.phase, SyncPhaseFfi::Synced, "{status:?}");
        assert_eq!(status.blocked, None);
        assert_eq!(status.pending_count, 0, "the phone's entry was not pushed");
        assert_eq!(
            stored(&phone.state.lock().unwrap(), &rotated),
            stored(&desktop.state.lock().unwrap(), &rotated),
            "the new-epoch entry did not decrypt on the phone"
        );
        assert!(phone.get_entry(mine).is_ok());
    }

    /// A cloud recovery generation ahead of the phone's fences its push:
    /// the sync reports `Fenced` and writes nothing to the cloud.
    #[test]
    fn newer_recovery_generation_fences_the_phone_push() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        let recorder = Arc::new(Recorder::default());
        phone.set_sync_observer(recorder.clone());
        phone.create_vault("password12".to_string()).unwrap();
        phone.create_entry("Before".to_string(), None).unwrap();
        connect_folder(&phone, cloud.path(), "fence-phone").unwrap();
        assert_eq!(sync_and_wait(&phone).phase, SyncPhaseFfi::Synced);

        let local = db::get_sync_recovery_generation(&phone.state.lock().unwrap()).unwrap();
        let control_path = cloud_tree(cloud.path())
            .into_keys()
            .find(|path| path.ends_with("control.json"))
            .expect("no recovery control in the cloud");
        let control_path = cloud.path().join(control_path);
        let mut control: SyncControlV1 =
            serde_json::from_slice(&std::fs::read(&control_path).unwrap()).unwrap();
        control.recovery_generation = local + 1;
        std::fs::write(&control_path, serde_json::to_vec_pretty(&control).unwrap()).unwrap();

        phone.create_entry("Fenced".to_string(), None).unwrap();
        let before = cloud_tree(cloud.path());
        let status = sync_and_wait(&phone);
        assert_eq!(status.blocked, Some(SyncBlockedFfi::Fenced), "{status:?}");
        assert_eq!(status.phase, SyncPhaseFfi::Error);
        assert_eq!(
            status.pending_count, 1,
            "the fenced entry was marked pushed"
        );
        assert_eq!(cloud_tree(cloud.path()), before, "a fenced phone wrote");
        assert_eq!(
            recorder.last_status().and_then(|s| s.blocked),
            Some(SyncBlockedFfi::Fenced),
            "the observer was not told"
        );
    }
}

/// Shared by the drive, onboarding and lock tests: engines on disk, the
/// folder-vault connect, and the serial lock for the process-wide
/// single-sync guard.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use memlore_engine::db;
    use memlore_engine::ops::entries::{create_entry_impl, save_entry_content_impl};
    use memlore_engine::sync::cloud_config::configured_cloud_provider;
    use memlore_engine::sync::connect::{complete_cloud_connect, GdriveConnectOutcome};
    use memlore_engine::sync::cycle::run_sync_cycle;
    use memlore_engine::sync::gdrive_provider::GDriveSession;
    use memlore_engine::sync::guard::{is_sync_in_progress, SyncInProgressGuard};
    use memlore_engine::sync::pending_session::PendingCloudSession;
    use memlore_engine::sync::recovery::{
        acquire_or_resume_cloud_cleanup_lease, release_recovery_lease,
    };
    use memlore_engine::sync::rotation::rotate::rotate_keys;
    use memlore_engine::sync::rotation::{publish_keyring, RecoverySource};
    use memlore_engine::sync::schedule::unix_now_s;
    use memlore_engine::sync::{CloudProvider, LocalSyncProvider, SyncSummary, SyncTrigger};
    use memlore_engine::utils::boot_file;
    use memlore_engine::utils::encryption::KdfParams;
    use memlore_engine::vault::onboard::onboard_complete_inner;
    use memlore_engine::vault::setup::{begin_first_time_setup_inner, confirm_first_time_setup};
    use memlore_engine::vault::startup::{startup_init, StartupMode};
    use memlore_engine::yjs_doc::build_entry_yjs;
    use memlore_engine::{AppState, EncryptionKeyState};

    use super::*;
    use crate::drive::{ConnectOutcomeFfi, MobileConnectHooks};
    use crate::keystore::{MemoryKeyStore, MemoryTokenStore};
    use crate::MobileVaultHooks;

    /// `SYNC_IN_PROGRESS` is process-wide: every test that takes it (a
    /// connect, the lock test) runs under this lock, or a parallel test sees
    /// `sync_in_progress`.
    static SYNC_GUARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
        SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// A first-launch engine in `dir` with memory Keychain fakes.
    pub(crate) fn engine_at(dir: &Path, tokens: Arc<MemoryTokenStore>) -> Arc<MemloreEngine> {
        MemloreEngine::with_stores(
            dir.to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            Arc::new(MemoryKeyStore::default()),
            tokens,
        )
        .unwrap()
    }

    /// The production connect tail (engine `complete_cloud_connect` and the
    /// mobile outcome mapping) against a folder vault at `root` instead of
    /// Drive. Take [`serial`] first.
    pub(crate) fn connect_folder(
        engine: &MemloreEngine,
        root: &Path,
        session_id: &str,
    ) -> Result<ConnectOutcomeFfi, MemloreError> {
        let guard = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
        let local_mode = engine.connect_mode()?;
        let folder = || CloudProvider::Folder(LocalSyncProvider::new(root));
        engine.finish_connect(
            guard,
            session_id,
            local_mode,
            folder(),
            folder(),
            PendingCloudSession::Folder {
                kind: "local".to_string(),
                root_path: root.to_string_lossy().into_owned(),
            },
        )
    }

    /// The device id of the vault behind `state`.
    pub(crate) fn device_id(state: &AppState) -> String {
        db::get_or_create_device_id(&state.lock().unwrap()).unwrap()
    }

    /// Wait for the running sync task to end. Take [`serial`] first.
    pub(crate) fn wait_for_sync() {
        let deadline = Instant::now() + Duration::from_secs(120);
        while is_sync_in_progress() {
            assert!(Instant::now() < deadline, "the sync did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A manual sync through `sync_now`, waited for; then the status.
    pub(crate) fn sync_and_wait(engine: &Arc<MemloreEngine>) -> SyncStatusFfi {
        assert!(
            Arc::clone(engine).sync_now(true).unwrap(),
            "another sync was running"
        );
        wait_for_sync();
        // The task outlives the guard by its last status.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = engine.sync_status().unwrap();
            if !status.running {
                return status;
            }
            assert!(Instant::now() < deadline, "the sync task did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// What a [`SyncObserver`] was told.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) statuses: Mutex<Vec<SyncStatusFfi>>,
        pub(crate) entries_changed: AtomicUsize,
    }

    impl Recorder {
        pub(crate) fn last_status(&self) -> Option<SyncStatusFfi> {
            self.statuses.lock().unwrap().last().cloned()
        }
    }

    impl SyncObserver for Recorder {
        fn on_progress(&self, _step: String, _done: u64, _total: u64) {}

        fn on_status(&self, status: SyncStatusFfi) {
            self.statuses.lock().unwrap().push(status);
        }

        fn on_entries_changed(&self) {
            self.entries_changed.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A desktop peer driven through the engine's own entry points (setup,
    /// connect, onboarding, rotation, the sync cycle), never through the
    /// FFI: its own vault on disk and its own cloud credentials.
    pub(crate) struct DesktopRole {
        _dir: tempfile::TempDir,
        db_path: PathBuf,
        pub(crate) state: AppState,
        key_state: EncryptionKeyState,
        vault: MobileVaultHooks,
        credentials: Arc<MobileCredentialStore>,
        live_session: Mutex<Option<GDriveSession>>,
        report: SyncReport,
    }

    impl DesktopRole {
        /// A desktop at first launch (no vault yet).
        pub(crate) fn first_launch() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let db_path = dir.path().join("memlore.db");
            let key_state = EncryptionKeyState::new();
            let credentials = Arc::new(MobileCredentialStore::new(Arc::new(
                MemoryTokenStore::default(),
            )));
            let vault = MobileVaultHooks {
                keystore: Arc::new(MemoryKeyStore::default()),
                credentials: credentials.clone(),
            };
            let (conn, mode) = startup_init(&db_path, &key_state, &vault).unwrap();
            assert_eq!(mode, StartupMode::FirstLaunch);
            Self {
                _dir: dir,
                db_path,
                state: AppState::new(conn),
                key_state,
                vault,
                credentials,
                live_session: Mutex::new(None),
                report: SyncReport::default(),
            }
        }

        fn hooks(&self) -> MobileSyncHooks<'_> {
            MobileSyncHooks::new(
                MobileConnectHooks {
                    credentials: &self.credentials,
                    live_session: &self.live_session,
                },
                &self.report,
                self.db_path.with_file_name("media"),
                None,
            )
        }

        /// First-time setup with a password; the recovery phrase.
        pub(crate) fn create_vault(&self, password: &str) -> String {
            let handle = {
                let conn = self.state.lock().unwrap();
                begin_first_time_setup_inner(&conn, password, "Desktop", KdfParams::FAST).unwrap()
            };
            let answers: Vec<String> = handle
                .challenge_indices
                .iter()
                .map(|&i| handle.mnemonic[i].clone())
                .collect();
            confirm_first_time_setup(
                &self.db_path,
                &self.state,
                &self.key_state,
                &handle.setup_id,
                &answers,
                password,
                None,
                None,
                &self.vault,
            )
            .unwrap();
            handle.mnemonic.join(" ")
        }

        fn complete_connect(&self, root: &Path, session_id: &str) -> GdriveConnectOutcome {
            let _guard = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
            let local_mode = db::get_encryption_mode(&self.state.lock().unwrap()).unwrap();
            let unlocked = self.key_state.is_initialized().unwrap();
            rt().block_on(complete_cloud_connect(
                &self.hooks(),
                &self.state,
                &self.key_state,
                session_id,
                CloudProvider::Folder(LocalSyncProvider::new(root)),
                PendingCloudSession::Folder {
                    kind: "local".to_string(),
                    root_path: root.to_string_lossy().into_owned(),
                },
                local_mode,
                unlocked,
                "",
            ))
            .unwrap()
        }

        /// Connect the created vault to the folder cloud at `root`.
        pub(crate) fn connect(&self, root: &Path, session_id: &str) {
            let outcome = self.complete_connect(root, session_id);
            assert!(
                matches!(outcome, GdriveConnectOutcome::Ready),
                "{outcome:?}"
            );
        }

        /// A first-launch desktop that connects to `root` and joins its vault
        /// with the phrase, through the engine's `onboard_complete_inner`.
        pub(crate) fn join(root: &Path, phrase: &str, password: &str) -> Self {
            let role = Self::first_launch();
            let session_id = "desktop-role-join";
            let outcome = role.complete_connect(root, session_id);
            assert!(
                matches!(outcome, GdriveConnectOutcome::NeedsOnboarding),
                "{outcome:?}"
            );
            let conn = std::mem::replace(
                &mut *role.state.lock().unwrap(),
                rusqlite::Connection::open_in_memory().unwrap(),
            );
            let generation = db::get_sync_recovery_generation(&conn).unwrap();
            let provider = CloudProvider::Folder(
                LocalSyncProvider::new(root).with_recovery_fence(generation, None),
            );
            let boot_path = boot_file::boot_path(&role.db_path);
            let conn = rt()
                .block_on(onboard_complete_inner(
                    conn,
                    &role.key_state,
                    &provider,
                    phrase,
                    password,
                    "Desktop",
                    Some(&boot_path),
                    Some(session_id.to_string()),
                    None,
                    &role.vault,
                ))
                .map_err(|(_, e)| e)
                .unwrap();
            role.state.set_connection(conn).unwrap();
            role
        }

        /// A new entry holding one paragraph of `text`; its id.
        pub(crate) fn write_entry(&self, title: &str, text: &str) -> String {
            let conn = self.state.lock().unwrap();
            let journal = db::list_journals(&conn, None).unwrap()[0].id.clone();
            let entry =
                create_entry_impl(&conn, &journal, Some(title), None, None, unix_now_s()).unwrap();
            let (yjs, content_text, preview_text) = build_entry_yjs(&[text], &[]);
            save_entry_content_impl(&conn, &entry.id, &yjs, &content_text, &preview_text).unwrap();
            entry.id
        }

        /// One sync cycle, holding the single-sync guard.
        pub(crate) fn sync(&self) -> SyncSummary {
            let _guard = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
            rt().block_on(run_sync_cycle(
                &self.state,
                &self.key_state,
                SyncTrigger::Manual,
                &self.hooks(),
            ))
            .unwrap()
        }

        /// Rotate the vault keys and publish the new keyring, keeping the
        /// recovery phrase (the desktop "Rotate master key").
        pub(crate) fn rotate(&self, password: &str, phrase: &str) {
            let _guard = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
            let provider =
                configured_cloud_provider(&self.state.lock().unwrap(), self.credentials.as_ref())
                    .unwrap()
                    .expect("no cloud provider");
            let boot_path = boot_file::boot_path(&self.db_path);
            rt().block_on(async {
                let (rotation_id, ctx, epoch) = rotate_keys(
                    &provider,
                    &self.state,
                    &self.key_state,
                    password,
                    None,
                    Some(phrase),
                    Some(&boot_path),
                )
                .await
                .unwrap();
                publish_keyring(
                    &provider,
                    &self.state,
                    rotation_id,
                    &ctx,
                    RecoverySource::Stashed,
                    None,
                    epoch,
                )
                .await
                .unwrap();
            });
            assert!(
                !db::has_active_rotation_job(&self.state.lock().unwrap()).unwrap(),
                "the rotation did not finish"
            );
        }

        /// Delete the cloud's payload under the engine's cleanup lease (the
        /// desktop "Delete cloud data"): the cloud keeps only its control,
        /// whose recovery generation is now one higher. The new generation.
        /// This desktop does not adopt it: drop the role afterwards.
        pub(crate) fn wipe_cloud(&self) -> u64 {
            let _guard = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
            let (generation, device_id) = {
                let conn = self.state.lock().unwrap();
                (
                    db::get_sync_recovery_generation(&conn).unwrap(),
                    db::get_or_create_device_id(&conn).unwrap(),
                )
            };
            let provider =
                configured_cloud_provider(&self.state.lock().unwrap(), self.credentials.as_ref())
                    .unwrap()
                    .expect("no cloud provider")
                    .with_recovery_fence(generation, None);
            rt().block_on(async {
                let permit = acquire_or_resume_cloud_cleanup_lease(&provider, &device_id)
                    .await
                    .unwrap();
                provider.configure_recovery_fence_authority(
                    permit.recovery_generation,
                    Some(permit.clone()),
                );
                provider.clear_cloud_preserving_control().await.unwrap();
                release_recovery_lease(&provider, &permit).await.unwrap();
                permit.recovery_generation
            })
        }
    }

    /// The FFI runtime, built here when no engine has made it yet.
    fn rt() -> &'static tokio::runtime::Runtime {
        RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
        })
    }

    /// The editor document (`default` fragment) of a Yjs full state, as XML.
    pub(crate) fn doc_xml(update: &[u8]) -> String {
        use yrs::updates::decoder::Decode;
        use yrs::{GetString, Transact};
        let doc = yrs::Doc::new();
        let fragment = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        if !update.is_empty() {
            txn.apply_update(yrs::Update::decode_v1(update).unwrap())
                .unwrap();
        }
        fragment.get_string(&txn)
    }

    /// Every file (with its bytes) and folder under `root`.
    pub(crate) fn cloud_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for item in std::fs::read_dir(dir).unwrap() {
                let path = item.unwrap().path();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if path.is_dir() {
                    out.insert(format!("{name}/"), Vec::new());
                    walk(root, &path, out);
                } else {
                    out.insert(name, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }
}
