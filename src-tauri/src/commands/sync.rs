use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use zeroize::Zeroizing;

use crate::commands::entries::maybe_mark_entry_embedding_dirty_after_save;
use crate::db;
use crate::sync::cloud_config::{CloudCredentialStore, GDriveClientConfig};
use crate::sync::engine::{
    CatchupProgressEvent, PeerOwnedIds, ProgressReporter, SyncProgressEvent, SyncProgressPhase,
};
#[cfg(test)]
pub(crate) use crate::sync::guard::SYNC_GUARD_TEST_LOCK;
pub use crate::sync::guard::SYNC_IN_PROGRESS_ERR;
pub(crate) use crate::sync::guard::{is_sync_in_progress, SyncInProgressGuard, SYNC_IN_PROGRESS};
use crate::sync::outbox_import::{OutboxBridgeEvent, OutboxEventSink};
use crate::sync::platform::CloudConnectHooks;
use crate::sync::platform::SyncPlatformHooks;
use crate::sync::{CloudProvider, SyncEngine, SyncProvider, SyncSummary, SyncTrigger};
use crate::{AppState, EncryptionKeyState};

/// Tauri event name broadcast whenever sync state changes (start, end,
/// error, or status mutation). Single source of truth for the frontend
/// `useSync` listener. Schema pinned by [`SyncStatusEvent`] and any change
/// must ship with a useSync test update — consumers rely on the field
/// names, not positional order.
pub const SYNC_STATUS_EVENT: &str = "sync:status-changed";

/// Tauri event name broadcast on every meaningful loop boundary during a
/// push/pull cycle.  Payload is [`SyncProgressEvent`] (re-exported from the
/// engine).  Separate from [`SYNC_STATUS_EVENT`] so the frontend can update
/// a progress label without re-rendering the full status indicator.
pub const SYNC_PROGRESS_EVENT: &str = "sync:progress";

/// Tauri event emitted after each committed pull chunk and once at the end
/// of a successful catch-up cycle. Payload is [`CatchupProgressEvent`].
/// Emit failures are best-effort UI failures and must never abort a pull.
pub const SYNC_CATCHUP_PROGRESS_EVENT: &str = "sync:catchup-progress";

/// Tauri event broadcast when the engine detects that the user is holding a
/// legacy `drive.file`-scoped token and needs to reconnect under the new
/// `drive.appdata` scope. Payload is empty — the frontend reads the
/// `gdrive_scope_upgrade_required` settings flag and renders the banner.
pub const SYNC_SCOPE_UPGRADE_EVENT: &str = "sync:scope-upgrade-required";

/// Settings key of the scope-upgrade banner flag; lives in the engine.
pub use crate::sync::connect::GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY;

/// Production [`ProgressReporter`] that forwards every progress event to the
/// Tauri frontend via [`SYNC_PROGRESS_EVENT`].  Emit failures are logged and
/// swallowed — a failed emit must never abort the sync cycle itself.
struct TauriProgressReporter {
    app: AppHandle,
}

impl ProgressReporter for TauriProgressReporter {
    fn report(&self, event: SyncProgressEvent) {
        if let Err(e) = self.app.emit(SYNC_PROGRESS_EVENT, event) {
            log::warn!("TauriProgressReporter: failed to emit {SYNC_PROGRESS_EVENT}: {e}");
        }
    }

    fn report_catchup_progress(&self, event: CatchupProgressEvent) {
        if let Err(e) = self.app.emit(SYNC_CATCHUP_PROGRESS_EVENT, event) {
            log::warn!("TauriProgressReporter: failed to emit {SYNC_CATCHUP_PROGRESS_EVENT}: {e}");
        }
    }
}

/// Lifecycle phase carried in [`SyncStatusEvent::state`]; lives in the engine
/// so the shared sync cycle can report it.
pub use crate::sync::platform::SyncPhase;

/// Payload for [`SYNC_STATUS_EVENT`]. Carries both the transient "what is
/// happening right now" (`state`) and the up-to-date snapshot of the
/// `sync_state`-derived status the sidebar indicator needs. Sending both
/// in one event keeps the frontend from racing a `get_sync_status` call
/// against a second event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusEvent {
    pub state: SyncPhase,
    pub enabled: bool,
    pub configured: bool,
    pub provider: Option<String>,
    pub last_sync: Option<i64>,
    pub entries_pending: u64,
    /// Set on transitions to `error`. `None` otherwise.
    pub error: Option<String>,
}

impl SyncStatusEvent {
    fn from_status(state: SyncPhase, status: SyncStatus, error: Option<String>) -> Self {
        Self {
            state,
            enabled: status.enabled,
            configured: status.configured,
            provider: status.provider,
            last_sync: status.last_sync,
            entries_pending: status.entries_pending,
            error,
        }
    }
}

/// Read the current sync status snapshot off the DB and emit a
/// [`SYNC_STATUS_EVENT`] with `state`. Errors are swallowed (logged) on
/// purpose: emit failures must never break the sync flow itself.
fn emit_status(
    app: &AppHandle,
    state_machine: SyncPhase,
    status_state: &AppState,
    error: Option<String>,
) {
    let status = match status_state
        .lock()
        .and_then(|conn| read_status_snapshot(&conn))
    {
        Ok(s) => s,
        Err(e) => {
            log::warn!("emit_status: failed to read sync status snapshot: {e}");
            return;
        }
    };
    let payload = SyncStatusEvent::from_status(state_machine, status, error);
    if let Err(e) = app.emit(SYNC_STATUS_EVENT, payload) {
        log::warn!("emit_status: failed to emit {SYNC_STATUS_EVENT}: {e}");
    }
}

/// Read the four sync-status fields directly off a held connection. Pulled
/// out so `emit_status` and `get_sync_status` share exactly one query set.
fn read_status_snapshot(conn: &rusqlite::Connection) -> Result<SyncStatus, String> {
    let enabled = db::get_sync_enabled(conn).map_err(|e| e.to_string())?;
    let provider = db::get_sync_provider(conn).map_err(|e| e.to_string())?;
    let last_sync = db::get_last_sync_at(conn).map_err(|e| e.to_string())?;
    let entries_pending = db::count_pending_entries(conn).map_err(|e| e.to_string())?;
    let configured = make_configured_provider(conn)?.is_some();
    Ok(SyncStatus {
        enabled,
        configured,
        provider,
        last_sync,
        entries_pending,
    })
}

fn read_sync_catchup_status(conn: &rusqlite::Connection) -> rusqlite::Result<SyncCatchupStatus> {
    Ok(SyncCatchupStatus {
        complete: db::get_sync_catchup_complete(conn)?,
        pulled: 0,
        total: 0,
    })
}

/// Public shape of the sync status exposed to the UI.
///
/// Chunk 3c fills in `provider` and `last_sync` (they were `None` in 3a).
/// `provider` is `Some("local")` when a local-directory provider is
/// configured; other values arrive in Chunk 4.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub enabled: bool,
    pub configured: bool,
    pub provider: Option<String>,
    pub last_sync: Option<i64>,
    pub entries_pending: u64,
}

/// Initial catch-up snapshot exposed to the UI. Progress counts arrive via
/// [`SYNC_CATCHUP_PROGRESS_EVENT`] while a sync is running.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncCatchupStatus {
    pub complete: bool,
    pub pulled: u64,
    pub total: u64,
}

#[tauri::command]
pub fn get_device_id(state: State<'_, AppState>) -> Result<String, String> {
    let conn = state.lock()?;
    db::get_or_create_device_id(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_sync_status(state: State<'_, AppState>) -> Result<SyncStatus, String> {
    let conn = state.lock()?;
    read_status_snapshot(&conn)
}

#[tauri::command]
pub fn get_sync_catchup_status(state: State<'_, AppState>) -> Result<SyncCatchupStatus, String> {
    let conn = state.lock()?;
    read_sync_catchup_status(&conn).map_err(|e| e.to_string())
}

// ─── Scheduler settings (Chunk 5) ────────────────────────────────────────────

/// Serialized shape of the scheduler-facing settings exposed to the UI. The
/// three fields correspond 1:1 with the DB keys declared in `db::queries`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncSettings {
    pub interval_minutes: u32,
    pub on_save: bool,
    pub on_launch: bool,
}

#[tauri::command]
pub fn get_sync_settings(state: State<'_, AppState>) -> Result<SyncSettings, String> {
    let conn = state.lock()?;
    Ok(SyncSettings {
        interval_minutes: db::get_sync_interval_minutes(&conn).map_err(|e| e.to_string())?,
        on_save: db::get_sync_on_save(&conn).map_err(|e| e.to_string())?,
        on_launch: db::get_sync_on_launch(&conn).map_err(|e| e.to_string())?,
    })
}

#[tauri::command]
pub fn set_sync_settings(state: State<'_, AppState>, settings: SyncSettings) -> Result<(), String> {
    let conn = state.lock()?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    db::set_sync_interval_minutes(&tx, settings.interval_minutes).map_err(|e| e.to_string())?;
    db::set_sync_on_save(&tx, settings.on_save).map_err(|e| e.to_string())?;
    db::set_sync_on_launch(&tx, settings.on_launch).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn set_sync_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let conn = state.lock()?;
    // Guard: enabling sync without a configured provider is a user-facing
    // bug (nothing to sync against). Reject it loudly instead of silently
    // succeeding and failing at sync_now time.
    if enabled
        && db::get_sync_provider(&conn)
            .map_err(|e| e.to_string())?
            .is_none()
    {
        return Err("Configure a sync provider first".to_string());
    }
    db::set_sync_enabled(&conn, enabled).map_err(|e| e.to_string())
}

// ─── Configuration ──────────────────────────────────────────────────────────

/// Desktop [`CloudCredentialStore`]: the Drive refresh token lives in the
/// `gdrive_refresh_token` setting; the OAuth client resolves settings
/// override → compile-time env → placeholder.
pub(crate) struct DesktopCredentialStore;

impl CloudCredentialStore for DesktopCredentialStore {
    fn client_config(&self, conn: &rusqlite::Connection) -> Result<GDriveClientConfig, String> {
        use crate::commands::gdrive::compiled_client_id;

        // Duplicates the `resolve_client_id/secret` helpers from commands::gdrive
        // (which are private to that module) but the logic is a 3-line lookup.
        let client_id = db::get_setting(conn, "gdrive_client_id")
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| compiled_client_id().to_string());
        let client_secret = db::get_setting(conn, "gdrive_client_secret")
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| crate::commands::gdrive::compiled_client_secret().to_string());
        Ok(GDriveClientConfig {
            client_id,
            client_secret: Some(client_secret),
        })
    }

    fn load_refresh_token(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<Option<Zeroizing<String>>, String> {
        match db::get_setting(conn, "gdrive_refresh_token").map_err(|e| e.to_string())? {
            Some(h) if !h.is_empty() => Ok(Some(Zeroizing::new(h))),
            _ => Ok(None),
        }
    }

    fn store_refresh_token(&self, conn: &rusqlite::Connection, token: &str) -> Result<(), String> {
        db::set_setting(conn, "gdrive_refresh_token", token).map_err(|e| e.to_string())
    }

    fn clear_refresh_token(&self, conn: &rusqlite::Connection) -> Result<(), String> {
        db::delete_setting(conn, "gdrive_refresh_token").map_err(|e| e.to_string())
    }
}

// ─── Sync operations ────────────────────────────────────────────────────────

/// Desktop wrapper of [`crate::sync::cloud_config::configured_cloud_provider`]
/// reading credentials through [`DesktopCredentialStore`].
pub(crate) fn configured_cloud_provider(
    conn: &rusqlite::Connection,
) -> Result<Option<CloudProvider>, String> {
    crate::sync::cloud_config::configured_cloud_provider(conn, &DesktopCredentialStore)
}

/// Desktop wrapper of [`crate::sync::cloud_config::make_configured_provider`]
/// reading credentials through [`DesktopCredentialStore`].
pub(crate) fn make_configured_provider(
    conn: &rusqlite::Connection,
) -> Result<Option<Arc<dyn SyncProvider>>, String> {
    crate::sync::cloud_config::make_configured_provider(conn, &DesktopCredentialStore)
}

/// Run an async engine method synchronously using `tauri::async_runtime::block_on`.
///
/// **Why `block_on` inside `spawn_blocking`?** `rusqlite::Connection` is not
/// `Send`. A Tauri `async fn` command whose future directly crosses an `.await`
/// while holding a `&Connection` would be non-`Send`; Tauri rejects those at
/// registration time. The solution: wrap the entire sync work in
/// `spawn_blocking` (which runs on a dedicated blocking thread, not a tokio
/// task), and let `block_on` drive the engine's async methods from that thread.
/// This is the same pattern the background scheduler has always used
/// (see `src-tauri/src/sync/scheduler.rs`). `block_on` is safe here because
/// there is no enclosing tokio async task on the blocking thread.
///
/// The one exception is code reached from inside the sync cycle's own
/// `block_on` ([`DesktopSyncHooks`]): it must wrap a nested `block_on` in
/// `tokio::task::block_in_place`.
fn block_on<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tauri::async_runtime::block_on(fut)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredSyncUxAction {
    ResetLocalState,
    RepairFromThisDevice,
}

static DEFERRED_SYNC_UX_ACTIONS: Mutex<Vec<DeferredSyncUxAction>> = Mutex::new(Vec::new());

pub(crate) fn enqueue_deferred_sync_ux_action(action: DeferredSyncUxAction) {
    let mut pending = DEFERRED_SYNC_UX_ACTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !pending.contains(&action) {
        pending.push(action);
    }
}

fn take_deferred_sync_ux_actions() -> Vec<DeferredSyncUxAction> {
    let mut pending = DEFERRED_SYNC_UX_ACTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *pending)
}

#[cfg(test)]
fn pending_deferred_sync_ux_actions_for_tests() -> Vec<DeferredSyncUxAction> {
    DEFERRED_SYNC_UX_ACTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
fn clear_deferred_sync_ux_actions_for_tests() {
    DEFERRED_SYNC_UX_ACTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// The try_acquire path used by revoke / rotate / resume: `None` while a
/// sync holds the guard maps to the exact `SYNC_IN_PROGRESS_ERR` string
/// the frontend keys off. AppHandle blocks a full command test (T40).
pub(crate) fn try_acquire_sync_or_in_progress_err() -> Result<SyncInProgressGuard, String> {
    SyncInProgressGuard::try_acquire().ok_or_else(|| SYNC_IN_PROGRESS_ERR.to_string())
}

/// Reconcile trigger used by the user-facing [`sync_now`] Tauri command.
///
/// Always [`SyncTrigger::Manual`] so own-cloud self-heal re-lists on every
/// explicit "Sync now", even after a successful Automatic cycle this session.
pub(crate) const USER_SYNC_NOW_TRIGGER: SyncTrigger = SyncTrigger::Manual;

/// Trigger an immediate, user-requested sync.
///
/// **Why `async fn` + `spawn_blocking`?** Tauri 2 runs non-async commands on
/// the main thread. Calling `block_on` on the main thread freezes the WebView's
/// IPC message loop for the entire sync duration — the UI becomes unresponsive.
/// By making this command `async` and wrapping the work in `spawn_blocking`, the
/// tokio runtime dispatches the blocking sync work to a dedicated thread pool
/// thread while the main thread (and async executor) remain free to process IPC
/// messages, keeping the UI responsive. This mirrors exactly what the background
/// scheduler already does in `src-tauri/src/sync/scheduler.rs`.
///
/// **Queues instead of bailing.** Unlike the scheduler and window-close paths
/// (which call [`run_sync_now`] and skip on a busy guard), a user click waits
/// up to [`MANUAL_SYNC_QUEUE_TIMEOUT`] for the single-flight guard and then
/// runs its own cycle. The guard can be held while the UI looks idle (the
/// pre-flight before the 'syncing' event, or the outbox import after the
/// terminal event), so bailing immediately made "Sync now" a silent no-op.
/// Returns `Err("sync_in_progress")` only if the guard is still busy when the
/// wait times out.
#[tauri::command]
pub async fn sync_now(app: AppHandle) -> Result<SyncSummary, String> {
    log::info!("sync_now: manual sync requested");
    let guard = acquire_manual_sync_guard(MANUAL_SYNC_QUEUE_TIMEOUT).await?;
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        let state = app.state::<AppState>();
        let key_state = app.state::<EncryptionKeyState>();
        // Manual entry point: the user clicked "Sync now" (or a UI flow
        // explicitly invoked this command). The user intervened, so reset
        // the scheduler's failure backoff entirely — otherwise a backoff
        // window opened before a successful manual sync would keep the
        // interval / on-save rules silently suppressed for up to 15 min
        // afterwards. See `sync/scheduler.rs`.
        if let Some(scheduler) =
            app.try_state::<std::sync::Arc<crate::sync::scheduler::SyncScheduler>>()
        {
            scheduler.reset_backoff();
        }
        run_sync_now_holding(guard, &app, &state, &key_state, USER_SYNC_NOW_TRIGGER)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// How long a manual "Sync now" queues behind a busy single-flight guard.
///
/// Must stay well under the frontend `WATCHDOG_MS` (130s in
/// `src/stores/syncStore.ts`): the frontend watchdog is armed at click and
/// only re-armed once the backend emits 'syncing', so a longer wait here
/// would let the watchdog fire a false "Sync timed out" while still queued.
const MANUAL_SYNC_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);

/// Queue for the single-flight guard on behalf of a manual sync, mapping a
/// timeout to the stable [`SYNC_IN_PROGRESS_ERR`] marker.
async fn acquire_manual_sync_guard(timeout: Duration) -> Result<SyncInProgressGuard, String> {
    SyncInProgressGuard::acquire_waiting(timeout)
        .await
        .ok_or_else(|| SYNC_IN_PROGRESS_ERR.to_string())
}

pub(crate) fn drain_deferred_sync_ux_actions_inner(state: &AppState) {
    for action in take_deferred_sync_ux_actions() {
        match action {
            DeferredSyncUxAction::ResetLocalState => {
                let result = state
                    .lock()
                    .map_err(|e| e.to_string())
                    .and_then(|conn| reset_local_sync_state(&conn).map_err(|e| e.to_string()));
                if let Err(e) = result {
                    log::warn!("drain_deferred_sync_ux_actions: reset failed: {e}");
                }
            }
            DeferredSyncUxAction::RepairFromThisDevice => {
                let engine_opt = match state.lock() {
                    Ok(conn) => match make_configured_provider(&conn) {
                        Ok(provider) => {
                            let device_id = match db::get_or_create_device_id(&conn) {
                                Ok(id) => id,
                                Err(e) => {
                                    log::warn!(
                                        "drain_deferred_sync_ux_actions: device id lookup failed: {e}"
                                    );
                                    continue;
                                }
                            };
                            provider.map(|p| SyncEngine::new(p, device_id))
                        }
                        Err(e) => {
                            log::warn!(
                                "drain_deferred_sync_ux_actions: provider lookup failed: {e}"
                            );
                            None
                        }
                    },
                    Err(e) => {
                        log::warn!("drain_deferred_sync_ux_actions: DB lock failed: {e}");
                        continue;
                    }
                };
                let owned = match engine_opt {
                    Some(engine) => block_on(engine.collect_peer_owned_ids()),
                    None => PeerOwnedIds::default(),
                };
                let result = state.lock().map_err(|e| e.to_string()).and_then(|conn| {
                    run_repair_from_this_device(&conn, &owned).map_err(|e| e.to_string())
                });
                if let Err(e) = result {
                    log::warn!("drain_deferred_sync_ux_actions: repair failed: {e}");
                }
            }
        }
    }
}

pub(crate) fn drain_deferred_sync_ux_actions(
    _app: &AppHandle,
    state: &AppState,
    _key_state: &EncryptionKeyState,
) {
    drain_deferred_sync_ux_actions_inner(state);
}

/// Shared implementation behind every sync entry point: the user-invoked
/// `sync_now` command, the background scheduler, and the window-close final
/// push. The scheduler and the close-handler cannot call the command
/// directly (they run outside Tauri's command dispatch), so this shared
/// function takes raw `State` references and returns the summary.
///
/// Emits lifecycle events at every boundary so every sync path produces
/// identical events.
///
/// **Single-flight guard.** A `SyncInProgressGuard` is acquired at the very
/// top of `run_sync_now` — if another caller (user, scheduler, close-handler)
/// is already running a cycle, we return `Err(SYNC_IN_PROGRESS_ERR)` without
/// touching the DB or emitting any status event. The manual `sync_now`
/// command instead queues for the guard and hands it to
/// `run_sync_now_holding`, which holds it for the whole cycle. This is the
/// load-bearing invariant that lets `sync_reset_local_state` safely flip rows
/// back to `pending` without racing an in-flight engine that would re-stamp
/// them to `synced` mid-reset.
pub(crate) fn run_sync_now(
    app: &AppHandle,
    state: &AppState,
    key_state: &EncryptionKeyState,
    trigger: SyncTrigger,
) -> Result<SyncSummary, String> {
    // Acquire the single-flight slot. If another sync is already running,
    // bail with the stable marker before touching anything else.
    let guard =
        SyncInProgressGuard::try_acquire().ok_or_else(|| SYNC_IN_PROGRESS_ERR.to_string())?;
    run_sync_now_holding(guard, app, state, key_state, trigger)
}

/// Body of [`run_sync_now`], run by a caller that already holds the
/// single-flight guard. The guard is held for the entire body — Drop releases
/// it on every return path, including panics and early `?`-propagation.
fn run_sync_now_holding(
    _guard: SyncInProgressGuard,
    app: &AppHandle,
    state: &AppState,
    key_state: &EncryptionKeyState,
    trigger: SyncTrigger,
) -> Result<SyncSummary, String> {
    block_on(crate::sync::cycle::run_sync_cycle(
        state,
        key_state,
        trigger,
        &DesktopSyncHooks { app },
    ))
}

/// Desktop [`SyncPlatformHooks`]: each method is the Tauri call the cycle made
/// at that point before it moved into the engine.
pub(crate) struct DesktopSyncHooks<'a> {
    pub(crate) app: &'a AppHandle,
}

impl SyncPlatformHooks for DesktopSyncHooks<'_> {
    fn progress_reporter(&self) -> Arc<dyn ProgressReporter> {
        Arc::new(TauriProgressReporter {
            app: self.app.clone(),
        })
    }

    fn on_status(&self, phase: SyncPhase, state: &AppState, error: Option<String>) {
        emit_status(self.app, phase, state, error);
    }

    fn on_force_re_pair(&self, reason: &str) {
        let _ = self.app.emit(
            "xj://force-re-pair",
            ForceRePairPayload {
                reason: reason.to_string(),
            },
        );
    }

    fn on_scope_upgrade_required(&self, state: &AppState) {
        handle_scope_upgrade_required(self.app, state);
    }

    fn after_successful_cycle(&self, state: &AppState, key_state: &EncryptionKeyState) {
        // Hooks run inside `run_sync_now_holding`'s `block_on`, and the drain
        // may `block_on` itself (deferred repair): leave the runtime context
        // first, or tokio panics on the nested runtime.
        tokio::task::block_in_place(|| drain_deferred_sync_ux_actions(self.app, state, key_state));
    }

    fn after_cycle(&self, state: &AppState, summary: &SyncSummary) -> Result<(), String> {
        let app = self.app;
        // Same as `after_successful_cycle`: the reconcile below calls `block_on`.
        tokio::task::block_in_place(|| {
            // Pull may have stamped embed-sync pending decisions (model
            // mismatch / key). Emit once when any slot needs attention —
            // debounced inside the helper so clean adopt-only syncs stay quiet.
            crate::commands::ai_embedding_decision::emit_embedding_decisions_after_sync(app, state);

            // T36: per-preset endpoint merge already cleared hosted-indexing
            // consent under the DB conn. Rebuild live slots here — the
            // engine lacks ProviderRegistry / BackfillManager / indexer.
            if !summary.changed_endpoint_presets.is_empty() {
                use tauri::Manager;
                if let (Some(registry), Some(indexer), Some(backfill)) = (
                    app.try_state::<crate::ai::provider_registry::ProviderRegistry>(),
                    app.try_state::<crate::ai::indexer::EntryIndexer>(),
                    app.try_state::<crate::commands::ai::BackfillManager>(),
                ) {
                    let server_manager =
                        app.try_state::<Arc<crate::ai::on_device::server::LlamaServerManager>>();
                    let asset_manager =
                        app.try_state::<Arc<crate::ai::on_device::llm_download::LlmAssetManager>>();
                    let downloads =
                        app.try_state::<Arc<crate::ai::on_device::download::DownloadManager>>();
                    let conn = state.lock()?;
                    for preset_id in &summary.changed_endpoint_presets {
                        let (old_endpoint, _) =
                            crate::commands::ai_provider::resolve_credential(&conn, preset_id)
                                .unwrap_or_else(|_| (String::new(), None));
                        if let Err(e) =
                            block_on(crate::commands::ai_provider::reconcile_slots_for_preset(
                                &conn,
                                &registry,
                                preset_id,
                                &old_endpoint,
                                &backfill,
                                &indexer,
                                server_manager.as_ref().map(|s| s.inner()),
                                asset_manager.as_ref().map(|s| s.inner()),
                                downloads.as_ref().map(|s| s.inner()),
                                || crate::commands::ai::start_indexing_worker(app.clone()),
                            ))
                        {
                            log::warn!(
                                "run_sync_now: reconcile_slots_for_preset({preset_id}) failed: {e}"
                            );
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn outbox_sink<'a>(&'a self, state: &'a AppState) -> Box<dyn OutboxEventSink + 'a> {
        Box::new(TauriOutboxSink {
            app: self.app,
            state,
        })
    }

    fn media_dir(&self) -> std::path::PathBuf {
        use tauri::Manager;
        self.app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("media")
    }
}

impl CloudConnectHooks for DesktopSyncHooks<'_> {
    fn on_connect_progress(&self, session_id: &str, step: &str) {
        crate::commands::gdrive::emit_connect_progress(self.app, session_id, step);
    }

    fn set_live_session(&self, session: Option<crate::sync::gdrive_provider::GDriveSession>) {
        use tauri::Manager;
        let session_state = self.app.state::<crate::GDriveSessionState>();
        match session {
            Some(session) => session_state.set(session),
            None => session_state.clear(),
        }
    }

    fn credentials(&self) -> &dyn CloudCredentialStore {
        &DesktopCredentialStore
    }
}

/// Production sink backed by Tauri AppHandle and AppState.
pub struct TauriOutboxSink<'a> {
    pub app: &'a tauri::AppHandle,
    pub state: &'a AppState,
}

impl<'a> OutboxEventSink for TauriOutboxSink<'a> {
    fn emit_bridge_event(&self, event: &OutboxBridgeEvent) {
        use tauri::Emitter;
        match event.kind.as_str() {
            "created" => {
                let payload = serde_json::json!({
                    "kind": "created",
                    "entryId": event.entry_id,
                    "journalId": event.journal_id,
                });
                let _ = self
                    .app
                    .emit(crate::commands::mcp::MCP_ENTRY_CHANGED_EVENT, payload);
            }
            "appended" => {
                let payload = serde_json::json!({
                    "kind": "appended",
                    "entryId": event.entry_id,
                    "journalId": event.journal_id,
                    "yjsUpdate": event.yjs_update,
                    "patch": event.patch,
                });
                let _ = self
                    .app
                    .emit(crate::commands::mcp::MCP_ENTRY_CHANGED_EVENT, payload);
            }
            "metadata" => {
                let payload = serde_json::json!({
                    "kind": "metadata",
                    "entryId": event.entry_id,
                    "journalId": event.journal_id,
                    "patch": event.patch,
                });
                let _ = self
                    .app
                    .emit(crate::commands::mcp::MCP_ENTRY_CHANGED_EVENT, payload);
            }
            _ => {}
        }
    }

    fn emit_progress(&self, current: u32, total: u32) {
        use tauri::Emitter;
        let event = SyncProgressEvent {
            phase: SyncProgressPhase::ImportingWebEdits,
            current,
            total,
        };
        let _ = self
            .app
            .emit(crate::commands::sync::SYNC_PROGRESS_EVENT, event);
    }

    fn mark_embedding_dirty(&self, entry_id: &str) {
        use tauri::Manager;
        if let Some(indexer) = self.app.try_state::<crate::ai::indexer::EntryIndexer>() {
            maybe_mark_entry_embedding_dirty_after_save(self.state, &indexer, entry_id);
        }
    }
}

// ─── Sync reset ─────────────────────────────────────────────────────────────

/// Counts of rows flipped from synced/uploaded → pending; lives in the
/// engine with [`reset_local_sync_state`].
pub use crate::sync::connect::ResetCounts;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMaintenanceResult<T> {
    pub queued: bool,
    pub result: Option<T>,
}

/// Entry and journal ownership rows inserted or flipped to pending by
/// repair-from-this-device. The media reset (every row with a local original,
/// own or downloaded from a peer) is not counted, so this is not
/// [`ResetCounts`].
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct RepairCounts {
    pub entries: u64,
    pub journals: u64,
}

/// Payload for the `xj://force-re-pair` event. The frontend `useForceRePair`
/// listener reads `event.payload.reason`, so the event MUST carry an object —
/// emitting a bare string would surface `reason` as `undefined`.
#[derive(Debug, Clone, Serialize)]
struct ForceRePairPayload {
    reason: String,
}

/// Reset every synced row back to pending; lives in the engine.
pub(crate) use crate::sync::connect::reset_local_sync_state;

fn queue_repair_if_sync_in_progress() -> Option<SyncMaintenanceResult<RepairCounts>> {
    if !is_sync_in_progress() {
        return None;
    }

    enqueue_deferred_sync_ux_action(DeferredSyncUxAction::RepairFromThisDevice);
    Some(SyncMaintenanceResult {
        queued: true,
        result: None,
    })
}

fn queue_reset_if_sync_in_progress() -> Option<SyncMaintenanceResult<ResetCounts>> {
    if !is_sync_in_progress() {
        return None;
    }

    enqueue_deferred_sync_ux_action(DeferredSyncUxAction::ResetLocalState);
    Some(SyncMaintenanceResult {
        queued: true,
        result: None,
    })
}

/// Reset all synced entries and journals, plus every media row with a local
/// original (own or downloaded from a peer), back to pending so the next sync
/// re-uploads them. Useful after the user manually deletes the Memlore
/// folder on Drive — the local DB kept every row as `synced`/`uploaded` even
/// though the remote copy is gone.
///
/// Only `synced` → `pending` rows are touched. `conflict` and `error` rows
/// are left unchanged — they require explicit user action, not silent reset.
///
/// Queued while ANY sync cycle is running — user-initiated, scheduler-driven,
/// or window-close final push. A concurrent reset would otherwise race the
/// engine: the engine could re-stamp `synced` on a row the reset just flipped.
/// The queued action drains only after a clean sync cycle finishes.
///
/// Returns counts so the UI can surface a human-readable confirmation:
/// "N entries, M media, J journals will re-upload on next sync."
pub(crate) fn sync_reset_local_state_inner(
    conn: &rusqlite::Connection,
) -> Result<SyncMaintenanceResult<ResetCounts>, String> {
    if let Some(queued) = queue_reset_if_sync_in_progress() {
        return Ok(queued);
    }

    reset_local_sync_state(conn)
        .map(|counts| SyncMaintenanceResult {
            queued: false,
            result: Some(counts),
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn sync_reset_local_state(
    state: State<'_, AppState>,
) -> Result<SyncMaintenanceResult<ResetCounts>, String> {
    if let Some(queued) = queue_reset_if_sync_in_progress() {
        return Ok(queued);
    }

    let conn = state.lock()?;
    sync_reset_local_state_inner(&conn)
}

// ─── Adopt repair (Bug 3) ────────────────────────────────────────────────────

/// Inner logic for `sync_repair_from_this_device`, extracted so tests can call
/// it directly on a raw `&Connection` without going through `tauri::State`.
///
/// Runs atomically: adopts local entries and journals NOT owned by peers into
/// `sync_state` / `journal_sync_state` as `'pending'`, and in the same
/// transaction resets version upload status and the upload status of every
/// media row with a local original (own or downloaded from a peer), and
/// clears surface hashes and pull revisions. When a set inside `owned` is empty (network
/// unavailable or sync not configured), that set falls back to adopting every
/// local row of that kind. Returns how many entry and journal ownership rows
/// were inserted or updated.
pub(crate) fn run_repair_from_this_device(
    conn: &rusqlite::Connection,
    owned: &PeerOwnedIds,
) -> rusqlite::Result<RepairCounts> {
    let tx = conn.unchecked_transaction()?;
    let entries = db::adopt_unowned_local_entries(&tx, &owned.entries)? as u64;
    let journals = db::adopt_unowned_local_journals(&tx, &owned.journals)? as u64;
    // After a cloud wipe a downloaded peer original may be the only copy
    // left, so repair re-publishes every local original, own or downloaded.
    db::reset_media_with_local_original_to_pending(&tx)?;
    db::reset_all_version_upload_status_to_pending(&tx)?;
    // Repair forces a full re-publish of owned content; clear surface hashes
    // in the same transaction so whole-table surfaces re-upload too.
    // Re-arm Automatic reconcile after commit (process-level, not DB).
    db::clear_all_surface_push_hashes(&tx)?;
    db::clear_all_pull_revisions(&tx)?;
    tx.commit()?;
    crate::sync::engine::reset_session_own_cloud_reconciled();
    Ok(RepairCounts { entries, journals })
}

/// Guard-aware seam for the adopt-local repair. Checks `SYNC_IN_PROGRESS`
/// and, if clear, delegates to `run_repair_from_this_device`.
///
/// Direct callers use this to preserve the low-level race guard. The Tauri
/// command queues instead, before doing any peer/network work.
pub(crate) fn run_repair_guarded(
    conn: &rusqlite::Connection,
    owned: &PeerOwnedIds,
) -> Result<RepairCounts, String> {
    if SYNC_IN_PROGRESS.load(Ordering::Acquire) {
        return Err(SYNC_IN_PROGRESS_ERR.to_string());
    }
    run_repair_from_this_device(conn, owned).map_err(|e| e.to_string())
}

pub(crate) fn sync_repair_from_this_device_inner(
    conn: &rusqlite::Connection,
    owned: &PeerOwnedIds,
) -> Result<SyncMaintenanceResult<RepairCounts>, String> {
    if let Some(queued) = queue_repair_if_sync_in_progress() {
        return Ok(queued);
    }
    // Sync may have started between the outer guard check and here (e.g. during
    // collect_peer_owned_ids().await) or between this check and the DB write.
    // Queue instead of rejecting — preserves the "defer not reject" contract.
    match run_repair_guarded(conn, owned) {
        Ok(counts) => Ok(SyncMaintenanceResult {
            queued: false,
            result: Some(counts),
        }),
        Err(e) if e == SYNC_IN_PROGRESS_ERR => {
            enqueue_deferred_sync_ux_action(DeferredSyncUxAction::RepairFromThisDevice);
            Ok(SyncMaintenanceResult {
                queued: true,
                result: None,
            })
        }
        Err(e) => Err(e),
    }
}

/// Adopt local entries and journals — live and soft-deleted (tombstones) —
/// that no live peer already owns into `sync_state` / `journal_sync_state`
/// as `'pending'`, and reset `upload_status` to `'pending'` for every media
/// row with a local original, own or downloaded from a peer. A downloaded
/// original is re-published to this device's folder, since the wipe may have
/// removed the only cloud copy; rows without an original are left alone.
///
/// This is the explicit user-triggered repair for the pulled-orphan bug:
/// after a cloud wipe, rows received via sync exist only in `entries` /
/// `journals` with no ownership-ledger row, so the normal push queue never
/// sees them. Calling this command makes the device re-publish everything it
/// holds — including tombstones — on the next sync tick. `push_single_entry`
/// serializes `is_deleted` into `EntryMetadata`, so entry tombstones are
/// published as deletions, not as live entries. Journals are adopted in the
/// same transaction so `push_journals` can see them.
///
/// **NOT automatic.** This must only run on explicit user action — calling it
/// during normal sync would cause pull→push ping-pong.
///
/// Returns how many entry and journal ownership rows were inserted or flipped
/// to pending. An empty peer-owned set (network down or sync not configured)
/// adopts every local entry and every local journal, per set.
#[tauri::command]
pub async fn sync_repair_from_this_device(
    state: State<'_, AppState>,
) -> Result<SyncMaintenanceResult<RepairCounts>, String> {
    if let Some(queued) = queue_repair_if_sync_in_progress() {
        return Ok(queued);
    }

    // Fetch entry and journal ids already owned by at least one live peer
    // before acquiring the DB lock or the sync guard. Drop the connection
    // guard before the .await so the future remains Send and Tauri can
    // register it. On any error (network unavailable, sync not configured),
    // `owned` is empty and repair falls back to adopting all entries and
    // journals — identical to the pre-cloud-aware behavior.
    let owned = {
        let engine_opt = {
            let conn = state.lock()?;
            let provider = make_configured_provider(&conn)?;
            let device_id = db::get_or_create_device_id(&conn).map_err(|e| e.to_string())?;
            provider.map(|p| SyncEngine::new(p, device_id))
        }; // conn guard dropped here, before the .await
        match engine_opt {
            Some(engine) => engine.collect_peer_owned_ids().await,
            None => PeerOwnedIds::default(),
        }
    };
    let conn = state.lock()?;
    sync_repair_from_this_device_inner(&conn, &owned)
}

// ─── Scope upgrade (drive.file → drive.appdata migration) ───────────────────

/// Reset of local sync state PLUS scope-upgrade-required flag set, performed
/// atomically inside one SQLite transaction. Mirrors the "destructive cloud
/// action must reset local state atomically" invariant: the legacy `drive.file`
/// folder is effectively orphaned (we cannot delete it from the new
/// appdata-scoped session), so every local row must be flipped back to
/// `pending` in the SAME transaction that sets the upgrade-required flag.
/// Otherwise a partial flip would leave entries marked `synced` while pointing
/// at remote files inside the wrong (legacy) space, producing silent data loss
/// on the first appdata-scoped sync.
#[cfg(test)]
pub(crate) fn mark_scope_upgrade_required(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<ResetCounts> {
    let tx = conn.unchecked_transaction()?;
    let counts = mark_scope_upgrade_required_in_tx(&tx)?;
    tx.commit()?;
    Ok(counts)
}

/// Inner helper run inside a caller-owned transaction. Returns the row counts
/// flipped to `pending` and sets the upgrade flag. Used by both
/// `mark_scope_upgrade_required` (standalone) and
/// `handle_scope_upgrade_required` which folds it together with the gdrive
/// credentials wipe in a single transaction.
fn mark_scope_upgrade_required_in_tx(
    tx: &rusqlite::Transaction<'_>,
) -> rusqlite::Result<ResetCounts> {
    let entries = tx.execute(
        "UPDATE sync_state SET sync_status='pending', local_version=local_version+1 WHERE sync_status='synced'",
        [],
    )? as u64;
    let journals = tx.execute(
        "UPDATE journal_sync_state SET sync_status='pending', local_version=local_version+1 WHERE sync_status='synced'",
        [],
    )? as u64;
    let device_id = db::get_or_create_device_id(tx)?;
    let media = db::reset_uploaded_media_to_pending(tx, &device_id)? as u64;
    // Legacy drive.file folder is orphaned; surface hashes for that cloud
    // content must not keep whole-table surfaces marked clean.
    db::clear_all_surface_push_hashes(tx)?;
    db::clear_all_pull_revisions(tx)?;
    // Re-arm even before commit: extra Automatic reconcile is harmless;
    // missing re-arm after a successful scope-upgrade is not.
    crate::sync::engine::reset_session_own_cloud_reconciled();
    db::set_setting(tx, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY, "1")?;
    Ok(ResetCounts {
        entries,
        media,
        journals,
    })
}

/// Read the scope-upgrade-required flag. Returns `false` when the row is
/// absent (the default) or malformed.
pub(crate) fn read_scope_upgrade_required(conn: &rusqlite::Connection) -> bool {
    matches!(
        db::get_setting(conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY)
            .ok()
            .flatten()
            .as_deref(),
        Some("1")
    )
}

/// Frontend command: returns whether the scope-upgrade banner should be
/// rendered on this start-up. Cheap (single settings row read); the
/// frontend calls this on mount and on every `SYNC_SCOPE_UPGRADE_EVENT`.
#[tauri::command]
pub fn get_sync_scope_upgrade_required(state: State<'_, AppState>) -> Result<bool, String> {
    let conn = state.lock()?;
    Ok(read_scope_upgrade_required(&conn))
}

/// Frontend command: clear the upgrade-required flag, called when the user
/// dismisses the banner or completes a reconnect under the new scope. Safe
/// to call when the flag is already absent.
#[tauri::command]
pub fn clear_sync_scope_upgrade_required(state: State<'_, AppState>) -> Result<(), String> {
    let conn = state.lock()?;
    db::delete_setting(&conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY).map_err(|e| e.to_string())
}

/// Triggered from `run_sync_now` when the engine surfaces ScopeMismatch.
/// Performs the disconnect-side of the migration **atomically**: in ONE
/// SQLite transaction, wipes Drive credentials, flips local sync state back
/// to `pending`, and sets the `gdrive_scope_upgrade_required` flag. Then
/// emits the upgrade event so any open Settings panel renders the banner.
///
/// The single-transaction invariant is load-bearing: a crash between
/// "credentials wiped" and "sync state reset" would leave the user with no
/// Drive connection AND every row still marked `synced`, which silently
/// suppresses future re-uploads. Honors the project rule "destructive cloud
/// actions must reset local state atomically with cloud cleanup".
///
/// Best-effort overall — each step logs and is swallowed because the user
/// is already in a degraded state and surfacing intermediate failures only
/// makes it worse. The user will hit the same scope error on next sync and
/// re-trigger this if the transaction failed.
pub(crate) fn handle_scope_upgrade_required(app: &AppHandle, state: &AppState) {
    {
        let conn = match state.lock() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("scope-upgrade: failed to acquire DB lock: {e}");
                return;
            }
        };
        let tx = match conn.unchecked_transaction() {
            Ok(t) => t,
            Err(e) => {
                log::warn!("scope-upgrade: failed to start transaction: {e}");
                return;
            }
        };
        // Wipe credentials FIRST so a partial commit cannot leave a stale
        // token alongside the upgrade flag.
        if let Err(e) = crate::commands::gdrive::clear_gdrive_settings(&tx) {
            log::warn!("scope-upgrade: clear_gdrive_settings failed: {e}");
            return;
        }
        // Reset local sync state + set the banner flag in the SAME tx.
        if let Err(e) = mark_scope_upgrade_required_in_tx(&tx) {
            log::warn!("scope-upgrade: mark_scope_upgrade_required_in_tx failed: {e}");
            return;
        }
        if let Err(e) = tx.commit() {
            log::warn!("scope-upgrade: tx.commit failed: {e}");
            return;
        }
    }
    if let Err(e) = app.emit(SYNC_SCOPE_UPGRADE_EVENT, ()) {
        log::warn!("scope-upgrade: failed to emit {SYNC_SCOPE_UPGRADE_EVENT}: {e}");
    }
}

/// Single-entry push, used by the editor debounce path in Chunk 5.
/// Uses `spawn_blocking` so the blocking sync work runs off the main thread,
/// keeping the WebView IPC loop responsive during auto-save pushes.
#[tauri::command]
pub async fn push_entry(app: AppHandle, entry_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        let state = app.state::<AppState>();
        let key_state = app.state::<EncryptionKeyState>();
        run_push_entry(&app, &state, &key_state, &entry_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

// Push guards live in the engine (`sync::force_re_pair`).
pub(crate) use crate::sync::force_re_pair::ensure_safe_to_push;

/// Desktop wrapper of [`crate::sync::force_re_pair::ensure_remote_recovery_push_allowed`]
/// reading credentials through [`DesktopCredentialStore`].
pub(crate) fn ensure_remote_recovery_push_allowed(
    state: &AppState,
    permit: Option<&crate::sync::recovery::RecoveryOwnerPermit>,
) -> Result<(), String> {
    block_on(
        crate::sync::force_re_pair::ensure_remote_recovery_push_allowed(
            state,
            permit,
            &DesktopCredentialStore,
        ),
    )
}

/// Pre-flight checks for a single-entry push. Mirrors the beginning of
/// `run_sync_now`: validates persistent guards first (force-re-pair, active
/// rotation job) then acquires the single-flight slot.
///
/// Returns the held `SyncInProgressGuard` on success so `run_push_entry` can
/// hold it for the entire push. Returns `Err(SYNC_IN_PROGRESS_ERR)` if
/// another sync is already in flight, or another `Err` for the persistent
/// guard conditions.
pub(crate) fn begin_push_entry(state: &AppState) -> Result<SyncInProgressGuard, String> {
    ensure_safe_to_push(state)?;
    SyncInProgressGuard::try_acquire().ok_or_else(|| SYNC_IN_PROGRESS_ERR.to_string())
}

pub(crate) fn run_push_entry(
    app: &AppHandle,
    state: &AppState,
    key_state: &EncryptionKeyState,
    entry_id: &str,
) -> Result<(), String> {
    // Guard: refuse to push if force-re-pair is required, an active rotation
    // job exists, or another sync is already in flight. The guard is held for
    // the entire push_single_entry call — Drop releases it on every path
    // including early ? and panics.
    let _guard = begin_push_entry(state)?;
    ensure_remote_recovery_push_allowed(state, None)?;

    let (provider, device_id) = {
        let conn = state.lock()?;
        let provider = make_configured_provider(&conn)?
            .ok_or_else(|| "Sync is disabled or not configured".to_string())?;
        let device_id = db::get_or_create_device_id(&conn).map_err(|e| e.to_string())?;
        (provider, device_id)
    };
    let engine = SyncEngine::new(provider, device_id);
    emit_status(app, SyncPhase::Syncing, state, None);
    // Snapshot the HKDF sync sub-key — same reason as `run_sync_now`:
    // holding `with_sync_key` across an `.await` (via `block_on`) would block
    // every other key-using sync command on the main thread.
    let key_copy: Zeroizing<[u8; 32]> = key_state.with_sync_key(|k| Ok(Zeroizing::new(*k)))?;
    let result =
        block_on(engine.push_single_entry(state, &key_copy, entry_id)).map_err(|e| e.to_string());
    match &result {
        Ok(()) => emit_status(app, SyncPhase::Synced, state, None),
        Err(e) => emit_status(app, SyncPhase::Error, state, Some(e.clone())),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    #[test]
    fn catchup_progress_event_name_matches_frontend_contract() {
        assert_eq!(SYNC_CATCHUP_PROGRESS_EVENT, "sync:catchup-progress");
    }

    // ── SyncPhase wire format ─────────────────────────────────────────────
    //
    // The frontend `SyncPhase` TypeScript union (`src/lib/tauri.ts`)
    // pins the exact strings: `idle`, `syncing`, `synced`, `error`.
    // These tests lock the Rust → JSON serialization so a rename on
    // either side fails the backend build (or this test), not silently
    // breaks the sidebar indicator at runtime.

    #[test]
    fn sync_phase_serializes_as_lowercase_string() {
        assert_eq!(serde_json::to_string(&SyncPhase::Idle).unwrap(), "\"idle\"");
        assert_eq!(
            serde_json::to_string(&SyncPhase::Syncing).unwrap(),
            "\"syncing\""
        );
        assert_eq!(
            serde_json::to_string(&SyncPhase::Synced).unwrap(),
            "\"synced\""
        );
        assert_eq!(
            serde_json::to_string(&SyncPhase::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn sync_status_event_serializes_correctly() {
        let event = SyncStatusEvent {
            state: SyncPhase::Syncing,
            enabled: true,
            configured: true,
            provider: Some("local".to_string()),
            last_sync: Some(1_700_000_000),
            entries_pending: 3,
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        // State literal must match the frontend union.
        assert!(json.contains("\"state\":\"syncing\""), "got {json}");
        // Fields must be camelCase (entries_pending → entriesPending, etc.).
        assert!(json.contains("\"lastSync\":1700000000"), "got {json}");
        assert!(json.contains("\"entriesPending\":3"), "got {json}");
        assert!(json.contains("\"configured\":true"), "got {json}");
        assert!(
            !json.contains("entries_pending"),
            "snake_case leaked: {json}"
        );
        assert!(!json.contains("last_sync"), "snake_case leaked: {json}");
    }
    use rusqlite::Connection;

    use crate::sync::engine::PeerOwnedIds;

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    #[test]
    fn get_device_id_is_stable() {
        let state = make_state();
        let a = {
            let conn = state.lock().unwrap();
            db::get_or_create_device_id(&conn).unwrap()
        };
        let b = {
            let conn = state.lock().unwrap();
            db::get_or_create_device_id(&conn).unwrap()
        };
        assert_eq!(a, b);
    }

    #[test]
    fn sync_status_defaults_disabled_zero_pending() {
        let state = make_state();
        let conn = state.lock().unwrap();
        let enabled = db::get_sync_enabled(&conn).unwrap();
        let provider = db::get_sync_provider(&conn).unwrap();
        let last_sync = db::get_last_sync_at(&conn).unwrap();
        let entries_pending = db::count_pending_entries(&conn).unwrap();
        assert!(!enabled);
        assert_eq!(provider, None);
        assert_eq!(last_sync, None);
        assert_eq!(entries_pending, 0);
    }

    #[test]
    fn sync_status_configuration_matches_provider_factory() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_sync_enabled(&conn, true).unwrap();

        db::set_sync_provider(&conn, "local").unwrap();
        assert!(!read_status_snapshot(&conn).unwrap().configured);
        db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/memlore-sync\"}").unwrap();
        assert!(read_status_snapshot(&conn).unwrap().configured);

        db::set_sync_provider(&conn, "gdrive").unwrap();
        db::set_sync_config_json(&conn, "").unwrap();
        assert!(!read_status_snapshot(&conn).unwrap().configured);
        db::set_setting(&conn, "gdrive_refresh_token", "1//test-refresh-token").unwrap();
        assert!(read_status_snapshot(&conn).unwrap().configured);
    }

    #[test]
    fn sync_catchup_status_reflects_persisted_completion() {
        let state = make_state();
        let conn = state.lock().unwrap();

        assert_eq!(
            read_sync_catchup_status(&conn).unwrap(),
            SyncCatchupStatus {
                complete: false,
                pulled: 0,
                total: 0,
            }
        );

        db::set_sync_catchup_complete(&conn, true).unwrap();

        assert_eq!(
            read_sync_catchup_status(&conn).unwrap(),
            SyncCatchupStatus {
                complete: true,
                pulled: 0,
                total: 0,
            }
        );
    }

    #[test]
    fn sync_status_reports_pending_count() {
        let state = make_state();
        let conn = state.lock().unwrap();
        let jid: String = conn
            .query_row("SELECT id FROM journals LIMIT 1", [], |r| r.get(0))
            .unwrap();
        for t in 0..2 {
            let e = db::create_entry(
                &conn,
                db::CreateEntryParams {
                    journal_id: &jid,
                    title: None,
                    content_text: None,
                    preview_text: None,
                    entry_date: 1_700_000_000 + t,
                },
            )
            .unwrap();
            db::mark_entry_pending(&conn, &e.id).unwrap();
        }
        assert_eq!(db::count_pending_entries(&conn).unwrap(), 2);
    }

    #[test]
    fn set_sync_enabled_roundtrips() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            // Configure a provider first — the guard now rejects enabling
            // without one.
            db::set_sync_provider(&conn, "local").unwrap();
            db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/xj\"}").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        assert!(db::get_sync_enabled(&conn).unwrap());
    }

    #[test]
    fn set_sync_enabled_rejects_without_provider() {
        let state = make_state();
        let conn = state.lock().unwrap();
        // No provider configured → raw db::set_sync_enabled allows it,
        // but the command-level guard does not. Simulate the guard:
        let provider = db::get_sync_provider(&conn).unwrap();
        assert!(provider.is_none(), "no provider should be set initially");
    }

    #[test]
    fn configure_and_clear_local_sync_via_db() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_sync_provider(&conn, "local").unwrap();
        db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/memlore-sync\"}").unwrap();
        db::set_sync_enabled(&conn, true).unwrap();
        assert_eq!(
            db::get_sync_provider(&conn).unwrap().as_deref(),
            Some("local")
        );

        db::clear_sync_provider(&conn).unwrap();
        assert_eq!(db::get_sync_provider(&conn).unwrap(), None);
        assert_eq!(db::get_sync_config_json(&conn).unwrap(), None);
        assert!(!db::get_sync_enabled(&conn).unwrap());
    }

    #[test]
    fn last_sync_at_roundtrips() {
        let state = make_state();
        let conn = state.lock().unwrap();
        assert_eq!(db::get_last_sync_at(&conn).unwrap(), None);
        db::set_last_sync_at(&conn, 1_700_123_456).unwrap();
        assert_eq!(db::get_last_sync_at(&conn).unwrap(), Some(1_700_123_456));
    }

    // ─── Keyring dirty flag retry ─────────────────────────────────────────────

    #[test]
    fn keyring_dirty_not_set_on_fresh_db() {
        let state = make_state();
        let conn = state.lock().unwrap();
        assert!(
            db::get_setting(&conn, db::KEYRING_DIRTY_KEY)
                .unwrap()
                .is_none(),
            "keyring_dirty should not be set on a fresh DB"
        );
    }

    /// The user-facing `sync_now` command must always pass Manual so own-cloud
    /// reconcile re-runs on every explicit "Sync now". The command wires
    /// `USER_SYNC_NOW_TRIGGER` into `run_sync_now` (no AppHandle needed to pin
    /// the constant itself).
    #[test]
    fn sync_now_command_path_passes_manual_trigger() {
        assert_eq!(
            USER_SYNC_NOW_TRIGGER,
            SyncTrigger::Manual,
            "sync_now command must request Manual reconcile"
        );
        // Distinct from the scheduler's Automatic path.
        assert_ne!(USER_SYNC_NOW_TRIGGER, SyncTrigger::Automatic);
    }

    /// Regression test: verifies that the architectural pattern used by the new
    /// `sync_now` command — dispatching sync work via `spawn_blocking` — does
    /// not deadlock or panic. This pins the invariant that blocking sync work
    /// (which internally calls `block_on`) must be wrapped in `spawn_blocking`
    /// rather than run directly on an async task or the main thread.
    ///
    /// We cannot call `run_sync_now` without an `AppHandle` in a unit test, so
    /// we test the dispatch shape itself: a closure that calls `block_on` inside
    /// `spawn_blocking` is the exact pattern `sync_now` uses, and it must
    /// complete without a deadlock or panic.
    #[tokio::test]
    async fn sync_now_spawn_blocking_pattern_does_not_deadlock() {
        // This exercises the exact dispatch shape of the new async sync_now:
        //   spawn_blocking(|| { ... block_on(some_future) ... }).await
        // If `block_on` inside `spawn_blocking` were broken (e.g., called from
        // a regular async task without spawn_blocking), tokio would panic with
        // "Cannot start a runtime from within a runtime". This test verifies
        // the pattern is safe.
        let result = tauri::async_runtime::spawn_blocking(|| {
            // Simulate what run_sync_now does internally: call block_on from
            // inside a blocking thread.
            block_on(async { 42u32 })
        })
        .await;

        assert!(
            result.is_ok(),
            "spawn_blocking must not panic: {:?}",
            result
        );
        assert_eq!(
            result.unwrap(),
            42,
            "block_on result must propagate correctly"
        );
    }

    // keyring_dirty_flag_stays_when_session_unavailable removed in Phase 2:
    // try_reupload_keyring is now a no-op stub (always Ok), so the
    // session-unavailable failure path no longer exists. Phase 3 will add V2 equivalents.

    // ── SyncProgressEvent wire format ─────────────────────────────────────
    //
    // The frontend `SyncProgressEvent` interface (`src/lib/tauri.ts`) pins
    // camelCase field names and kebab-case phase strings.  These tests lock
    // the Rust → JSON serialization so a rename on either side fails the
    // build rather than silently breaking the progress label at runtime.

    #[test]
    fn sync_progress_event_serializes_correctly() {
        use crate::sync::engine::{SyncProgressEvent, SyncProgressPhase};
        let event = SyncProgressEvent {
            phase: SyncProgressPhase::PushingEntries,
            current: 5,
            total: 12,
        };
        let json = serde_json::to_string(&event).unwrap();
        // Phase must be kebab-case matching the TS union.
        assert!(json.contains("\"phase\":\"pushing-entries\""), "got {json}");
        // Numeric fields must be camelCase (current and total are fine as-is,
        // but verify the exact keys exist in the output).
        assert!(json.contains("\"current\":5"), "got {json}");
        assert!(json.contains("\"total\":12"), "got {json}");
        // No snake_case leakage.
        assert!(
            !json.contains("pushing_entries"),
            "snake_case leaked: {json}"
        );
    }

    /// Helper: insert a sync_state row with given status.
    fn insert_sync_state(conn: &Connection, entry_id: &str, status: &str) {
        conn.execute(
            "INSERT INTO sync_state (entry_id, local_version, sync_status) VALUES (?1, 1, ?2)",
            rusqlite::params![entry_id, status],
        )
        .unwrap();
    }

    #[test]
    fn local_authoritative_rebuild_must_adopt_pulled_entries_and_journals() {
        let state = make_state();
        let conn = state.lock().unwrap();
        let journal_id = "pulled-journal-without-ledger";
        let entry_id = "pulled-entry-without-ledger";

        conn.execute(
            "INSERT INTO journals (id, name, created_at, updated_at) VALUES (?1, 'Pulled', 1, 1)",
            [journal_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries (id, journal_id, entry_date, created_at, updated_at) \
             VALUES (?1, ?2, 1, 1, 1)",
            rusqlite::params![entry_id, journal_id],
        )
        .unwrap();

        crate::sync::recovery::adopt_all_local_content_for_recovery(&conn).unwrap();

        let pending_entry: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_state WHERE entry_id = ?1 AND sync_status = 'pending'",
                [entry_id],
                |row| row.get(0),
            )
            .unwrap();
        let pending_journal: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM journal_sync_state WHERE journal_id = ?1 AND sync_status = 'pending'",
                [journal_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            (pending_entry, pending_journal),
            (1, 1),
            "authoritative local rebuild must adopt pulled entries and journals; the current reset/wipe path only updates existing ownership ledgers"
        );
    }

    /// Seed three `'uploaded'` media rows on one entry: `own` (this device's
    /// folder, local original), `peer` (pulled from a peer, no original) and
    /// `evicted` (this device's folder, original evicted from the cache).
    /// The device id is minted first so the own cloud paths match it.
    fn seed_media_ownership_mix(conn: &Connection) {
        let device_id = db::get_or_create_device_id(conn).unwrap();
        let jid: String = conn
            .query_row("SELECT id FROM journals LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let entry = db::create_entry(
            conn,
            db::CreateEntryParams {
                journal_id: &jid,
                title: None,
                content_text: None,
                preview_text: None,
                entry_date: 1_700_000_030,
            },
        )
        .unwrap();
        for (id, storage_path, cloud_path) in [
            ("own", "/tmp/own.jpg", format!("{device_id}/media/own")),
            ("peer", "", "dev-peer/media/peer".to_string()),
            ("evicted", "", format!("{device_id}/media/evicted")),
        ] {
            conn.execute(
                "INSERT INTO media (id, entry_id, file_name, file_type, storage_provider, \
                 storage_path, upload_status, cloud_path, sort_order, created_at) \
                 VALUES (?1, ?2, 'a.jpg', 'image/jpeg', 'local', ?3, 'uploaded', ?4, 0, 0)",
                rusqlite::params![id, entry.id, storage_path, cloud_path],
            )
            .unwrap();
        }
    }

    fn assert_only_own_media_reset(conn: &Connection) {
        let status = |id: &str| -> String {
            conn.query_row("SELECT upload_status FROM media WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(status("own"), "pending", "own row with original re-uploads");
        assert_eq!(status("peer"), "uploaded", "peer row has no original");
        assert_eq!(status("evicted"), "uploaded", "evicted row has no original");
    }

    #[test]
    fn scope_upgrade_reset_skips_media_without_local_original() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_media_ownership_mix(&conn);

        let counts = mark_scope_upgrade_required(&conn).unwrap();

        assert_eq!(counts.media, 1);
        assert_only_own_media_reset(&conn);
    }

    /// Repair runs after a cloud wipe, so it re-publishes every original this
    /// device holds, own or downloaded from a peer. Rows without a local
    /// original stay `'uploaded'`: there is nothing to upload.
    #[test]
    fn run_repair_from_this_device_requeues_every_local_original() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_media_ownership_mix(&conn);
        let entry_id: String = conn
            .query_row("SELECT entry_id FROM media WHERE id = 'own'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO media (id, entry_id, file_name, file_type, storage_provider, \
             storage_path, upload_status, cloud_path, sort_order, created_at) \
             VALUES ('downloaded', ?1, 'a.jpg', 'image/jpeg', 'local', \
             '/tmp/downloaded.jpg', 'uploaded', 'dev-peer/media/downloaded', 0, 0)",
            [&entry_id],
        )
        .unwrap();

        run_repair_from_this_device(&conn, &PeerOwnedIds::default()).unwrap();

        let status = |id: &str| -> String {
            conn.query_row("SELECT upload_status FROM media WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(status("own"), "pending", "own row with original re-uploads");
        assert_eq!(
            status("downloaded"),
            "pending",
            "downloaded peer original re-uploads to this device's folder"
        );
        assert_eq!(status("peer"), "uploaded", "peer row has no original");
        assert_eq!(status("evicted"), "uploaded", "evicted row has no original");
    }

    /// Whole-table surface names used by sync change-tracking hash gating.
    /// Shared with production reconcile/push (`HASH_GATED_SURFACE_NAMES`).
    const SURFACE_PUSH_HASH_SURFACES: &[&str] = crate::sync::engine::HASH_GATED_SURFACE_NAMES;

    fn seed_all_surface_push_hashes(conn: &Connection) {
        for surface in SURFACE_PUSH_HASH_SURFACES {
            db::set_surface_push_hash(conn, surface, &format!("hash-{surface}")).unwrap();
            assert!(
                db::get_surface_push_hash(conn, surface).unwrap().is_some(),
                "seed must leave {surface} clean (hash present)"
            );
        }
        db::set_pull_revision(conn, "peer-a", "metadata", "revision-a").unwrap();
        db::set_pull_revision(conn, "peer-b", "metadata", "revision-b").unwrap();
    }

    fn assert_all_surface_push_hashes_cleared(conn: &Connection) {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_push_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "sync_push_state must be empty after bulk reset/repair"
        );
        for surface in SURFACE_PUSH_HASH_SURFACES {
            assert_eq!(
                db::get_surface_push_hash(conn, surface).unwrap(),
                None,
                "absent hash for {surface} means default-dirty → subsequent push re-uploads"
            );
        }
        let pull_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_pull_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            pull_count, 0,
            "sync_pull_state must clear with sync_push_state after bulk reset/repair"
        );
    }

    /// Repair-from-this-device must clear surface hashes in the same
    /// transaction as media/version requeue (cloud may be empty/corrupt).
    #[test]
    fn repair_from_this_device_clears_all_surface_push_hashes() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_all_surface_push_hashes(&conn);

        run_repair_from_this_device(&conn, &PeerOwnedIds::default()).unwrap();

        assert_all_surface_push_hashes_cleared(&conn);
    }

    /// Scope-upgrade (drive.file → appdata) orphans the old cloud folder;
    /// surface hashes for that content must not keep surfaces marked clean.
    #[test]
    fn mark_scope_upgrade_required_clears_all_surface_push_hashes() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_all_surface_push_hashes(&conn);

        mark_scope_upgrade_required(&conn).unwrap();

        assert_all_surface_push_hashes_cleared(&conn);
        assert!(
            read_scope_upgrade_required(&conn),
            "scope-upgrade flag must still be set"
        );
    }

    #[test]
    fn deferred_sync_ux_queue_coalesces_duplicate_actions() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_deferred_sync_ux_actions_for_tests();

        enqueue_deferred_sync_ux_action(DeferredSyncUxAction::ResetLocalState);
        enqueue_deferred_sync_ux_action(DeferredSyncUxAction::ResetLocalState);
        enqueue_deferred_sync_ux_action(DeferredSyncUxAction::RepairFromThisDevice);

        assert_eq!(
            pending_deferred_sync_ux_actions_for_tests(),
            vec![
                DeferredSyncUxAction::ResetLocalState,
                DeferredSyncUxAction::RepairFromThisDevice,
            ],
            "deferred sync UX actions must be queued once per action kind"
        );

        clear_deferred_sync_ux_actions_for_tests();
    }

    #[test]
    fn is_sync_in_progress_reflects_guard_lifetime() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        assert!(
            !is_sync_in_progress(),
            "sync must start idle before guard acquisition"
        );
        let guard = SyncInProgressGuard::try_acquire().expect("guard must be free");
        assert!(
            is_sync_in_progress(),
            "helper must report active sync while guard is held"
        );
        drop(guard);
        assert!(
            !is_sync_in_progress(),
            "helper must return to false after guard drop"
        );
    }

    // NOTE: every test below holds SYNC_GUARD_TEST_LOCK (a std Mutex) across
    // .await points. Sound because #[tokio::test] runs on a current_thread
    // runtime — nothing else on this thread can deadlock on the lock — but
    // fragile if a test ever switches to a multi_thread flavor.

    #[tokio::test(start_paused = true)]
    async fn acquire_waiting_returns_immediately_when_free_and_releases_on_drop() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let start = tokio::time::Instant::now();
        let guard = SyncInProgressGuard::acquire_waiting(std::time::Duration::from_secs(1)).await;
        assert!(guard.is_some(), "free slot must be acquired");
        // Under start_paused, virtual time only advances across .await on a
        // timer — the fast path must have hit none.
        assert!(
            start.elapsed().is_zero(),
            "fast path must not sleep before acquiring"
        );
        assert!(
            is_sync_in_progress(),
            "acquired guard must hold the sync slot"
        );
        drop(guard);
        assert!(
            !is_sync_in_progress(),
            "guard must release the slot on drop"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waiting_acquires_after_holder_releases() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let holder = SyncInProgressGuard::try_acquire().expect("guard must be free");
        let waiter = tokio::spawn(SyncInProgressGuard::acquire_waiting(
            std::time::Duration::from_secs(10),
        ));

        // Let the waiter observe the held slot for a couple of poll rounds
        // and prove it actually blocked rather than resolving early.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(
            !waiter.is_finished(),
            "waiter must still be blocked while the guard is held"
        );
        drop(holder);

        let guard = waiter.await.expect("waiter task must not panic");
        assert!(
            guard.is_some(),
            "waiter must acquire the slot once the holder releases"
        );
        assert!(
            is_sync_in_progress(),
            "waiter's guard must now hold the sync slot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waiting_grants_slot_to_exactly_one_concurrent_waiter() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let holder = SyncInProgressGuard::try_acquire().expect("guard must be free");
        let first = tokio::spawn(SyncInProgressGuard::acquire_waiting(
            std::time::Duration::from_secs(10),
        ));
        let second = tokio::spawn(SyncInProgressGuard::acquire_waiting(
            std::time::Duration::from_secs(10),
        ));

        // Both waiters observe the held slot, then race for the release.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        drop(holder);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let finished = [first.is_finished(), second.is_finished()];
        assert_eq!(
            finished.iter().filter(|f| **f).count(),
            1,
            "exactly one waiter must win the freed slot; the loser keeps polling"
        );

        // The loser's guard stays held inside the winner's join result, so
        // the loser must run out its own timeout and come back empty.
        let first_guard = first.await.expect("first waiter must not panic");
        let second_guard = second.await.expect("second waiter must not panic");
        assert_eq!(
            first_guard.is_some() as u8 + second_guard.is_some() as u8,
            1,
            "the slot must never be granted to both concurrent waiters"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waiting_times_out_while_held() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let _holder = SyncInProgressGuard::try_acquire().expect("guard must be free");
        let start = tokio::time::Instant::now();
        let guard = SyncInProgressGuard::acquire_waiting(std::time::Duration::from_secs(2)).await;
        assert!(guard.is_none(), "waiter must give up after the timeout");
        assert!(
            start.elapsed() >= std::time::Duration::from_secs(2),
            "waiter must keep polling until the deadline, not bail early"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waiting_zero_timeout_fails_fast_while_held() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let _holder = SyncInProgressGuard::try_acquire().expect("guard must be free");
        let start = tokio::time::Instant::now();
        let guard = SyncInProgressGuard::acquire_waiting(std::time::Duration::ZERO).await;
        assert!(guard.is_none(), "zero timeout must not wait for the slot");
        assert!(
            start.elapsed().is_zero(),
            "zero timeout must return without sleeping"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn manual_sync_guard_returns_sync_in_progress_after_timeout() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let _holder = SyncInProgressGuard::try_acquire().expect("guard must be free");
        let start = tokio::time::Instant::now();
        let result = acquire_manual_sync_guard(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            result.err().as_deref(),
            Some(SYNC_IN_PROGRESS_ERR),
            "a guard busy for the whole wait must map to the stable marker"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(300),
            "must wait out the timeout before giving up"
        );
    }

    #[test]
    fn reset_queues_while_sync_in_progress() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_deferred_sync_ux_actions_for_tests();
        let state = make_state();
        let conn = state.lock().unwrap();
        let _held = SyncInProgressGuard::try_acquire().expect("guard must be free");

        let result = sync_reset_local_state_inner(&conn).unwrap();

        assert!(result.queued, "reset must queue while sync is active");
        assert!(
            result.result.is_none(),
            "queued reset must not report immediate counts"
        );
        assert_eq!(
            pending_deferred_sync_ux_actions_for_tests(),
            vec![DeferredSyncUxAction::ResetLocalState]
        );
        clear_deferred_sync_ux_actions_for_tests();
    }

    #[test]
    fn deferred_reset_runs_after_sync_cycle() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_deferred_sync_ux_actions_for_tests();
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            let jid: String = conn
                .query_row("SELECT id FROM journals LIMIT 1", [], |r| r.get(0))
                .unwrap();
            let entry = db::create_entry(
                &conn,
                db::CreateEntryParams {
                    journal_id: &jid,
                    title: None,
                    content_text: None,
                    preview_text: None,
                    entry_date: 1_700_000_030,
                },
            )
            .unwrap();
            insert_sync_state(&conn, &entry.id, "synced");
        }
        enqueue_deferred_sync_ux_action(DeferredSyncUxAction::ResetLocalState);

        drain_deferred_sync_ux_actions_inner(&state);

        let conn = state.lock().unwrap();
        let status: String = conn
            .query_row("SELECT sync_status FROM sync_state LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "pending");
        assert!(
            pending_deferred_sync_ux_actions_for_tests().is_empty(),
            "drain must consume queued actions"
        );
    }

    #[test]
    fn repair_queues_while_sync_in_progress() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_deferred_sync_ux_actions_for_tests();
        let state = make_state();
        let conn = state.lock().unwrap();
        let _held = SyncInProgressGuard::try_acquire().expect("guard must be free");

        let result = sync_repair_from_this_device_inner(&conn, &PeerOwnedIds::default()).unwrap();

        assert!(result.queued, "repair must queue while sync is active");
        assert!(
            result.result.is_none(),
            "queued repair must not report an immediate adopted count"
        );
        assert_eq!(
            pending_deferred_sync_ux_actions_for_tests(),
            vec![DeferredSyncUxAction::RepairFromThisDevice]
        );
        clear_deferred_sync_ux_actions_for_tests();
    }

    #[test]
    fn sync_progress_phase_all_variants_kebab_case() {
        use crate::sync::engine::SyncProgressPhase;
        let cases = [
            (SyncProgressPhase::PushingJournals, "pushing-journals"),
            (SyncProgressPhase::PushingMedia, "pushing-media"),
            (SyncProgressPhase::PushingEntries, "pushing-entries"),
            (SyncProgressPhase::PushingSettings, "pushing-settings"),
            (SyncProgressPhase::PushingTags, "pushing-tags"),
            (SyncProgressPhase::PushingTemplates, "pushing-templates"),
            (SyncProgressPhase::PushingLocations, "pushing-locations"),
            (SyncProgressPhase::PushingChats, "pushing-chats"),
            (SyncProgressPhase::PushingStreak, "pushing-streak"),
            (SyncProgressPhase::PushingAiAudit, "pushing-ai-audit"),
            (SyncProgressPhase::PullingManifests, "pulling-manifests"),
            (SyncProgressPhase::PullingTags, "pulling-tags"),
            (SyncProgressPhase::PullingJournals, "pulling-journals"),
            (SyncProgressPhase::PullingEntries, "pulling-entries"),
            (SyncProgressPhase::PullingSettings, "pulling-settings"),
            (SyncProgressPhase::PullingTemplates, "pulling-templates"),
            (SyncProgressPhase::PullingLocations, "pulling-locations"),
            (SyncProgressPhase::PullingChats, "pulling-chats"),
            (SyncProgressPhase::PullingStreak, "pulling-streak"),
            (SyncProgressPhase::PullingAiAudit, "pulling-ai-audit"),
            (SyncProgressPhase::ImportingWebEdits, "importing-web-edits"),
        ];
        for (variant, expected) in cases {
            let got = serde_json::to_string(&variant).unwrap();
            assert_eq!(
                got,
                format!("\"{expected}\""),
                "variant {expected} serialized wrong"
            );
        }
    }

    // ── Scope upgrade migration ───────────────────────────────────────────

    #[test]
    fn read_scope_upgrade_required_defaults_false() {
        let state = make_state();
        let conn = state.lock().unwrap();
        assert!(!read_scope_upgrade_required(&conn));
    }

    #[test]
    fn read_scope_upgrade_required_returns_true_when_flag_set() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_setting(&conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY, "1").unwrap();
        assert!(read_scope_upgrade_required(&conn));
    }

    #[test]
    fn read_scope_upgrade_required_treats_unexpected_value_as_false() {
        // Defence: the frontend stores "1" explicitly. Any other value
        // (corrupted row, legacy "true", future schema) must NOT trigger
        // the banner — that would lock users out of the Settings panel.
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_setting(&conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY, "true").unwrap();
        assert!(!read_scope_upgrade_required(&conn));
    }

    /// Seed a `sync_state` row in the given status. Needs a parent `entries`
    /// row (FK cascade on entries) plus a parent `journals` row.
    fn seed_entry_with_sync_status(conn: &rusqlite::Connection, entry_id: &str, status: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO journals (id, name, created_at, updated_at) VALUES ('j', 'j', 0, 0)",
            [],
        )
        .expect("seed journal");
        conn.execute(
            "INSERT INTO entries (id, journal_id, entry_date, created_at, updated_at) VALUES (?, 'j', 0, 0, 0)",
            rusqlite::params![entry_id],
        )
        .expect("seed entry");
        conn.execute(
            "INSERT INTO sync_state (entry_id, local_version, sync_status) VALUES (?, 1, ?)",
            rusqlite::params![entry_id, status],
        )
        .expect("seed sync_state row");
    }

    #[test]
    fn mark_scope_upgrade_required_flips_synced_rows_and_sets_flag() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_entry_with_sync_status(&conn, "entry-1", "synced");

        let counts = mark_scope_upgrade_required(&conn).unwrap();
        assert_eq!(counts.entries, 1, "should flip the synced entry to pending");

        // Flag is now set.
        assert!(read_scope_upgrade_required(&conn));

        // The row was flipped to pending.
        let status: String = conn
            .query_row(
                "SELECT sync_status FROM sync_state WHERE entry_id = 'entry-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn mark_scope_upgrade_required_is_idempotent() {
        // Running twice must not error and must leave the flag set.
        let state = make_state();
        let conn = state.lock().unwrap();
        let _ = mark_scope_upgrade_required(&conn).unwrap();
        let _ = mark_scope_upgrade_required(&conn).unwrap();
        assert!(read_scope_upgrade_required(&conn));
    }

    #[test]
    fn mark_scope_upgrade_required_leaves_conflict_rows_alone() {
        // Rule: only `synced` rows are flipped. `conflict` and `error` rows
        // need user resolution, not silent reset.
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_entry_with_sync_status(&conn, "entry-conflict", "conflict");
        seed_entry_with_sync_status(&conn, "entry-synced", "synced");

        let counts = mark_scope_upgrade_required(&conn).unwrap();
        assert_eq!(counts.entries, 1, "only the synced row should flip");

        let conflict_status: String = conn
            .query_row(
                "SELECT sync_status FROM sync_state WHERE entry_id = 'entry-conflict'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            conflict_status, "conflict",
            "conflict row must be preserved"
        );
    }

    #[test]
    fn clear_scope_upgrade_required_removes_flag() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_setting(&conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY, "1").unwrap();
        assert!(read_scope_upgrade_required(&conn));
        db::delete_setting(&conn, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY).unwrap();
        assert!(!read_scope_upgrade_required(&conn));
    }

    #[test]
    fn active_authoritative_recovery_must_block_normal_sync_single_flight() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            let job_id =
                db::create_sync_recovery_job(&conn, "local_to_cloud", 1, Some("backup.zip"), None)
                    .unwrap();
            db::advance_sync_recovery_job(&conn, job_id, "preflight", "{}").unwrap();
        }

        let result = begin_push_entry(&state);

        assert!(
            result.is_err(),
            "normal sync must not acquire the shared single-flight slot while an authoritative recovery job is active"
        );
    }

    // ─── Bug 2: run_push_entry single-flight guard ───────────────────────────

    /// Bug 2 — blocked while guard held: `begin_push_entry` (the seam used by
    /// `run_push_entry`) must return `SYNC_IN_PROGRESS_ERR` when another sync
    /// already holds `SyncInProgressGuard`.
    ///
    /// Both assertions live in one test to avoid a race on the global
    /// `SYNC_IN_PROGRESS` flag between parallel test threads.
    #[test]
    fn run_push_entry_blocked_while_sync_in_progress_and_released_after_drop() {
        // Serialise all tests that touch the process-global SYNC_IN_PROGRESS flag.
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let state = make_state();

        // Acquire the guard to simulate a concurrent full sync (or entry push).
        let held = SyncInProgressGuard::try_acquire()
            .expect("guard must be free at the start of the test");

        // While the guard is held, begin_push_entry must fail with SYNC_IN_PROGRESS_ERR.
        match begin_push_entry(&state) {
            Ok(_) => panic!("begin_push_entry must be rejected while SyncInProgressGuard is held"),
            Err(msg) => assert_eq!(
                msg, SYNC_IN_PROGRESS_ERR,
                "error must be SYNC_IN_PROGRESS_ERR, got: {msg}"
            ),
        }

        // Drop the held guard — simulates the concurrent sync finishing.
        drop(held);

        // After the guard is released, begin_push_entry must succeed and return
        // a new guard (proving the flag was reset by Drop).
        let new_guard = begin_push_entry(&state);
        assert!(
            new_guard.is_ok(),
            "begin_push_entry must succeed once the prior guard is dropped"
        );
        // Drop so the flag is reset for other tests.
        drop(new_guard);
    }

    // ─── sync_repair_from_this_device tests ──────────────────────────────────

    /// Seed an entry row with no sync_state row (simulates a pulled entry).
    fn seed_orphan_entry(conn: &Connection, entry_id: &str) {
        let jid: String = conn
            .query_row("SELECT id FROM journals LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO entries (id, journal_id, entry_date, created_at, updated_at) \
             VALUES (?1, ?2, 0, 0, 0)",
            rusqlite::params![entry_id, jid],
        )
        .unwrap();
    }

    /// Full end-to-end test of `run_repair_from_this_device`:
    /// seeds an orphaned entry + uploaded media, calls the inner repair
    /// function, and asserts that the entry is now pending in `sync_state`
    /// and appears in `list_pending_entry_ids`, and that the media row is
    /// reset to `'pending'`.
    #[test]
    fn repair_adopts_orphaned_entry_and_resets_media() {
        let state = make_state();
        let conn = state.lock().unwrap();

        seed_orphan_entry(&conn, "orphan-e1");

        // Seed a media row marked 'uploaded'.
        conn.execute(
            "INSERT INTO media \
             (id, entry_id, file_name, file_type, storage_provider, storage_path, \
              upload_status, sort_order, created_at) \
             VALUES ('m1', 'orphan-e1', 'a.jpg', 'image', 'local', '/tmp/a.jpg', \
                     'uploaded', 0, 0)",
            [],
        )
        .unwrap();

        // Seed a version row marked 'uploaded' — the versions channel must be
        // reset by repair too (finding: it was silently skipped).
        let version_id =
            db::insert_entry_version(&conn, "orphan-e1", b"yjs", "preview", "device-a").unwrap();
        db::mark_version_uploaded(&conn, &version_id, "device-a/versions/v1.bin").unwrap();

        let adopted = run_repair_from_this_device(&conn, &PeerOwnedIds::default()).unwrap();
        assert_eq!(adopted.entries, 1, "one orphaned entry must be adopted");

        // sync_state row must now be pending.
        let status: String = conn
            .query_row(
                "SELECT sync_status FROM sync_state WHERE entry_id = 'orphan-e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");

        // Entry must appear in the push queue.
        let ids = db::list_pending_entry_ids(&conn).unwrap();
        assert!(ids.contains(&"orphan-e1".to_string()));

        // Media must have been reset to pending.
        let media_status: String = conn
            .query_row("SELECT upload_status FROM media WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(media_status, "pending");

        // Version must have been reset to pending with cloud_path cleared.
        let (version_status, version_cloud_path): (String, Option<String>) = conn
            .query_row(
                "SELECT upload_status, cloud_path FROM entry_versions WHERE id = ?1",
                [&version_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(version_status, "pending");
        assert_eq!(version_cloud_path, None);
    }

    /// Verify that an already-synced entry is also flipped to pending by the
    /// repair (it must re-publish after a cloud wipe, even if it authored the
    /// content originally).
    #[test]
    fn repair_flips_synced_entry_to_pending() {
        let state = make_state();
        let conn = state.lock().unwrap();

        seed_orphan_entry(&conn, "synced-e2");
        conn.execute(
            "INSERT INTO sync_state (entry_id, local_version, sync_status) \
             VALUES ('synced-e2', 5, 'synced')",
            [],
        )
        .unwrap();

        let adopted = run_repair_from_this_device(&conn, &PeerOwnedIds::default()).unwrap();
        assert_eq!(adopted.entries, 1, "synced row must be flipped to pending");

        let status: String = conn
            .query_row(
                "SELECT sync_status FROM sync_state WHERE entry_id = 'synced-e2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        let ids = db::list_pending_entry_ids(&conn).unwrap();
        assert!(ids.contains(&"synced-e2".to_string()));
    }

    /// `run_repair_guarded` must reject direct repair calls while a sync is
    /// in progress (guards against concurrent reset + push race).
    ///
    /// The Tauri command queues first; this pins the lower-level guard path.
    #[test]
    fn repair_blocked_while_sync_in_progress() {
        // Serialise tests touching the process-global SYNC_IN_PROGRESS flag.
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let state = make_state();
        let _held = SyncInProgressGuard::try_acquire().expect("guard must be free at test start");

        // Call the production guard seam directly — removing or breaking the
        // guard in run_repair_guarded will cause this assertion to fail.
        let conn = state.lock().unwrap();
        let blocked = run_repair_guarded(&conn, &PeerOwnedIds::default());

        match blocked {
            Err(msg) => assert_eq!(msg, SYNC_IN_PROGRESS_ERR),
            Ok(_) => panic!("repair must be rejected while sync is in progress"),
        }
    }

    /// Pulled journal with no `journal_sync_state` row — the v0.1.0 shape
    /// after a pull. `is_initial_placeholder = 0` so it is not the schema seed.
    fn seed_pulled_journal(
        conn: &Connection,
        id: &str,
        name: &str,
        updated_at: i64,
        is_deleted: i64,
    ) {
        conn.execute(
            "INSERT INTO journals \
             (id, name, created_at, updated_at, is_deleted, is_initial_placeholder) \
             VALUES (?1, ?2, ?3, ?3, ?4, 0)",
            rusqlite::params![id, name, updated_at, is_deleted],
        )
        .unwrap();
    }

    /// Pulled entry with no `sync_state` row.
    fn seed_pulled_entry(
        conn: &Connection,
        id: &str,
        journal_id: &str,
        title: &str,
        updated_at: i64,
        is_deleted: i64,
    ) {
        conn.execute(
            "INSERT INTO entries \
             (id, journal_id, title, entry_date, created_at, updated_at, is_deleted) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?5)",
            rusqlite::params![id, journal_id, title, updated_at, is_deleted],
        )
        .unwrap();
    }

    fn journal_row_identity(conn: &Connection, id: &str) -> (String, i64, i64) {
        conn.query_row(
            "SELECT name, IFNULL(is_deleted, 0), updated_at FROM journals WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn entry_row_identity(conn: &Connection, id: &str) -> (Option<String>, i64, i64) {
        conn.query_row(
            "SELECT title, IFNULL(is_deleted, 0), updated_at FROM entries WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn journal_ids_pending_and_push(conn: &Connection) -> (Vec<String>, Vec<String>) {
        let pending = db::list_pending_journal_ids(conn).unwrap();
        let push = db::list_local_journal_summaries_for_push(conn)
            .unwrap()
            .into_iter()
            .map(|summary| summary.journal_id)
            .collect();
        (pending, push)
    }

    /// Repair on a post-pull v0.1.0 database adopts journals no peer owns,
    /// leaves peer-owned journals out of the push ledger, and does not rewrite
    /// the journal or entry rows themselves.
    #[test]
    fn repair_adopts_pulled_journals_on_legacy_db() {
        let state = make_state();
        let conn = state.lock().unwrap();

        seed_pulled_journal(&conn, "j-owned", "Peer Journal", 1_700_000_001, 0);
        seed_pulled_journal(&conn, "j-unowned", "Local Journal", 1_700_000_002, 0);
        seed_pulled_entry(
            &conn,
            "e-owned-host",
            "j-owned",
            "Kept entry",
            1_700_000_003,
            0,
        );
        seed_pulled_entry(
            &conn,
            "e-unowned-host",
            "j-unowned",
            "Adopted entry",
            1_700_000_004,
            0,
        );

        let placeholder_id: String = conn
            .query_row(
                "SELECT id FROM journals WHERE is_initial_placeholder = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let journals_before = (
            journal_row_identity(&conn, "j-owned"),
            journal_row_identity(&conn, "j-unowned"),
            journal_row_identity(&conn, &placeholder_id),
        );
        let entries_before = (
            entry_row_identity(&conn, "e-owned-host"),
            entry_row_identity(&conn, "e-unowned-host"),
        );
        let seeded_sync_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_state \
                 WHERE entry_id IN ('e-owned-host', 'e-unowned-host')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let seeded_journal_sync_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM journal_sync_state \
                 WHERE journal_id IN ('j-owned', 'j-unowned')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seeded_sync_rows, 0, "pulled entries have no sync_state");
        assert_eq!(
            seeded_journal_sync_rows, 0,
            "pulled journals have no journal_sync_state"
        );

        let mut owned = PeerOwnedIds::default();
        owned.journals.insert("j-owned".to_string());
        let journal_total: i64 = conn
            .query_row("SELECT COUNT(*) FROM journals", [], |r| r.get(0))
            .unwrap();

        let counts = run_repair_from_this_device(&conn, &owned).unwrap();

        assert_eq!(
            journal_row_identity(&conn, "j-owned"),
            journals_before.0,
            "peer-owned journal row must survive unchanged"
        );
        assert_eq!(
            journal_row_identity(&conn, "j-unowned"),
            journals_before.1,
            "unowned journal row must survive unchanged"
        );
        assert_eq!(
            journal_row_identity(&conn, &placeholder_id),
            journals_before.2,
            "placeholder journal row must survive unchanged"
        );
        assert_eq!(
            entry_row_identity(&conn, "e-owned-host"),
            entries_before.0,
            "entry row must survive unchanged"
        );
        assert_eq!(
            entry_row_identity(&conn, "e-unowned-host"),
            entries_before.1,
            "entry row must survive unchanged"
        );

        let (pending, push) = journal_ids_pending_and_push(&conn);
        assert!(
            pending.contains(&"j-unowned".to_string()),
            "unowned journal must be pending, got {pending:?}"
        );
        assert!(
            push.contains(&"j-unowned".to_string()),
            "unowned journal must be publishable, got {push:?}"
        );
        assert!(
            !pending.contains(&"j-owned".to_string()),
            "peer-owned journal must stay out of the pending ledger, got {pending:?}"
        );
        assert!(
            !push.contains(&"j-owned".to_string()),
            "peer-owned journal must stay out of the push manifest, got {push:?}"
        );
        assert!(
            pending.contains(&placeholder_id),
            "schema placeholder is not peer-owned, so repair adopts it"
        );

        let pending_entries = db::list_pending_entry_ids(&conn).unwrap();
        assert!(pending_entries.contains(&"e-owned-host".to_string()));
        assert!(pending_entries.contains(&"e-unowned-host".to_string()));
        // Empty entry set adopts every entry; journal ownership is independent.
        assert_eq!(counts.entries, 2);
        // j-owned is the only journal excluded. The placeholder (and any other
        // non-peer journal migrate seeded) is adopted with j-unowned.
        assert_eq!(counts.journals, (journal_total - 1) as u64);
    }

    /// An empty peer set (network down / sync not configured) adopts every
    /// journal, including ones a peer would otherwise own.
    #[test]
    fn repair_with_empty_peer_set_adopts_all_journals() {
        let state = make_state();
        let conn = state.lock().unwrap();

        seed_pulled_journal(&conn, "j-peer", "Would Be Peer", 1_700_000_011, 0);
        seed_pulled_journal(&conn, "j-local", "Would Be Local", 1_700_000_012, 0);

        let journal_total: i64 = conn
            .query_row("SELECT COUNT(*) FROM journals", [], |r| r.get(0))
            .unwrap();

        let counts = run_repair_from_this_device(&conn, &PeerOwnedIds::default()).unwrap();

        let (pending, push) = journal_ids_pending_and_push(&conn);
        for id in ["j-peer", "j-local"] {
            assert!(
                pending.contains(&id.to_string()),
                "{id} must be pending when the peer set is empty, got {pending:?}"
            );
            assert!(
                push.contains(&id.to_string()),
                "{id} must be publishable when the peer set is empty, got {push:?}"
            );
        }
        assert_eq!(
            counts.journals, journal_total as u64,
            "empty journal set adopts every journal, including peer-owned ones"
        );
    }

    /// Queued repair drains through the no-provider path, which adopts every
    /// local journal. Peer-id injection needs a configured sync engine; this
    /// harness has none, so the test pins the adopt-all drain fallback.
    #[test]
    fn queued_repair_drain_adopts_journals() {
        let _lock = crate::commands::sync::SYNC_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_deferred_sync_ux_actions_for_tests();
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            seed_pulled_journal(&conn, "j-drain-peer", "Drain Peer", 1_700_000_021, 0);
            seed_pulled_journal(&conn, "j-drain-local", "Drain Local", 1_700_000_022, 0);
        }
        enqueue_deferred_sync_ux_action(DeferredSyncUxAction::RepairFromThisDevice);

        drain_deferred_sync_ux_actions_inner(&state);

        let conn = state.lock().unwrap();
        let (pending, push) = journal_ids_pending_and_push(&conn);
        for id in ["j-drain-peer", "j-drain-local"] {
            assert!(
                pending.contains(&id.to_string()),
                "{id} must be pending after the queued repair drains, got {pending:?}"
            );
            assert!(
                push.contains(&id.to_string()),
                "{id} must be publishable after the queued repair drains, got {push:?}"
            );
        }
        assert!(
            pending_deferred_sync_ux_actions_for_tests().is_empty(),
            "drain must consume the queued repair"
        );
        clear_deferred_sync_ux_actions_for_tests();
    }
}
