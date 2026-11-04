//! Platform side effects of a sync cycle.
//!
//! [`crate::sync::cycle::run_sync_cycle`] is shared by the desktop app and the
//! mobile FFI. Everything it cannot do itself (UI events, the desktop's
//! deferred UX queue, AI slot reconcile, where media lives, where the Drive
//! refresh token lives) goes through [`SyncPlatformHooks`]. Each method stands
//! in for one call the desktop cycle made before the move, at the same
//! position and with the same arguments.
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use super::cloud_config::CloudCredentialStore;
use super::engine::ProgressReporter;
use super::outbox_import::OutboxEventSink;
use super::SyncSummary;
use crate::AppState;
use crate::EncryptionKeyState;

/// Lifecycle phase carried in the desktop `SyncStatusEvent::state`. Compile-time
/// typechecked here so a typo at an emission site fails to build rather
/// than silently producing a bad string the frontend ignores. The
/// `#[serde(rename_all = "lowercase")]` matches the Chunk 5 frontend
/// union `SyncPhase = 'idle' | 'syncing' | 'synced' | 'error'`; changing
/// either side must change the other in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncPhase {
    /// Nothing in flight; the app may or may not have synced before.
    Idle,
    /// A push/pull is currently running.
    Syncing,
    /// The most recent run finished without errors.
    Synced,
    /// The most recent run ended with `summary.errors` non-empty.
    Error,
}

/// Side effects a sync cycle hands to the platform.
///
/// Hooks run inside the cycle's future. On desktop that future is driven by
/// `block_on` on a blocking thread, so a hook that blocks on a future itself
/// must leave the runtime context first (`tokio::task::block_in_place`).
pub trait SyncPlatformHooks: Send + Sync {
    /// The reporter for engine progress events. The cycle wraps it in
    /// [`super::stall_guard::StallTrackingReporter`].
    fn progress_reporter(&self) -> Arc<dyn ProgressReporter>;

    /// Broadcast the sync status (`phase`, the status snapshot read from
    /// `state`, and `error` on failure).
    fn on_status(&self, phase: SyncPhase, state: &AppState, error: Option<String>);

    /// Tell the UI that a re-pair is required. The cycle has already persisted
    /// the force-re-pair flags.
    fn on_force_re_pair(&self, reason: &str);

    /// The cloud rejected the token's scope: disconnect and ask the user to
    /// reconnect.
    fn on_scope_upgrade_required(&self, state: &AppState);

    /// Runs after `last_sync_at` is stamped on a cycle without errors.
    fn after_successful_cycle(&self, state: &AppState, key_state: &EncryptionKeyState);

    /// Runs after every completed cycle (with or without errors), before the
    /// web outbox import. An `Err` aborts the cycle before the terminal status.
    fn after_cycle(&self, state: &AppState, summary: &SyncSummary) -> Result<(), String>;

    /// The sink for the web outbox import's events.
    fn outbox_sink<'a>(&'a self, state: &'a AppState) -> Box<dyn OutboxEventSink + 'a>;

    /// The directory that holds the vault's media files.
    fn media_dir(&self) -> PathBuf;

    /// Where the Drive OAuth client and refresh token come from.
    fn credentials(&self) -> &dyn CloudCredentialStore;
}
