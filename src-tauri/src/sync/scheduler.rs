//! Background sync scheduler (Chunk 5).
//!
//! Runs on a tokio task spawned in `run()` at app setup. Wakes up on a
//! short fine-grained tick (`TICK`) and decides whether to fire a sync
//! based on three rules:
//!
//! 1. **Interval fire.** `now - last_sync >= sync_interval_minutes`.
//! 2. **On-save (debounce).** `last_edit > 0` and `now - last_edit >=
//!    DEBOUNCE_WINDOW` and `last_edit > last_debounce_fire` — i.e. the
//!    editor went quiet for DEBOUNCE_WINDOW, and the debounce has not
//!    already fired for this edit burst.
//! 3. **On-launch.** A one-shot fire the first time the scheduler ticks
//!    after start, gated on the `sync_on_launch` setting.
//!
//! Rules 1 and 2 are independent — a user can disable on-save and still
//! get interval syncs; a user can shrink the interval and still get
//! on-save debouncing.
//!
//! ## Failure handling
//!
//! Every failed cycle opens an exponential backoff window
//! (`ERROR_BACKOFF_MS` … `ERROR_BACKOFF_MAX_MS`) during which Rules 1 + 2
//! are suppressed, then the scheduler retries automatically — there is NO
//! sticky "paused until the user clicks Sync now" state. A persistent
//! error (revoked token, force re-pair) therefore costs one failed request
//! every 15 min at most, which is negligible, while a transient one (Wi-Fi
//! still reconnecting after wake, DNS blip, Drive 5xx) heals itself without
//! user action. Deciding "persistent vs transient" from the rendered error
//! strings was deliberately rejected: those strings can embed peer/cloud-
//! derived text (`… parse: {e}`), and a substring match on them would let
//! a poisoned payload freeze auto-sync (the same class of bug fixed once
//! already for `is_auth` in `commands/gdrive.rs`).
//!
//! The open window is dropped early by a kick (`request_retry_now` —
//! window focus, rate-limited by `KICK_COOLDOWN_MS`), by a detected
//! wake-from-sleep (`slept_between_ticks`), or reset entirely by the
//! manual `sync_now` command (`reset_backoff`).
//!
//! ## Non-goals
//!
//! - Jitter: the tick is short enough that jitter is unnecessary.
//! - Persistent cursor: "when did we last push" is derivable from the
//!   `last_sync_at` setting already.
//!
//! ## Lock discipline
//!
//! The scheduler must **not** hold any mutex across an `.await`. All DB
//! reads happen inside a short synchronous block. The actual `run_sync_now`
//! call is synchronous (it wraps its own `block_on` inside a
//! `tauri::async_runtime::spawn_blocking`) so the task never yields while
//! holding the `AppState` mutex.
//!
//! ## Locked-app handling
//!
//! If the encryption key is not initialized (app is locked),
//! `run_sync_now` returns an error containing the phrase "Encryption key
//! not initialized". The scheduler swallows exactly this error class
//! silently — every other error is logged at `warn`. Bursting log noise
//! while the lock screen sits idle would bury real problems.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};

use memlore_engine::sync::schedule::{
    evaluate_with_error_cursor, interval_hold_until_ms, record_failure, slept_between_ticks,
    unix_now_s, RetryState, DEBOUNCE_WINDOW, SCHEDULER_SYNC_TRIGGER, TICK,
};

use crate::commands::sync::run_sync_now;
use crate::{AppState, EncryptionKeyState, LastEditState};

/// Tauri event emitted when the scheduler opens a backoff window after a
/// failed cycle, or clears it on a successful one. The frontend can show
/// "Retry in X min" while the window is open.
pub const SYNC_RETRY_SCHEDULED_EVENT: &str = "sync:retry-scheduled";

/// Payload for [`SYNC_RETRY_SCHEDULED_EVENT`]. `None` = backoff cleared.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncRetryScheduledPayload {
    /// Unix milliseconds at which the scheduler will next attempt a sync.
    /// `None` means no backoff is active (cleared on success / manual sync).
    pub retry_at_ms: Option<i64>,
}

/// Scheduler handle. Dropping or explicitly calling `stop()` flips the
/// cancellation flag; the background task exits on its next tick. Tokio's
/// `sleep` is cancellation-aware via the `tokio::select!` arm.
pub struct SyncScheduler {
    cancel: Arc<AtomicBool>,
    /// Set by `request_retry_now` (window focus). Consumed by the run loop
    /// on its next tick to drop an open backoff window (`RetryState::kick`).
    retry_kick: Arc<AtomicBool>,
    /// Set by `reset_backoff` (manual Sync now). Consumed by the run loop
    /// on its next tick to reset the failure streak (`RetryState::reset`).
    retry_reset: Arc<AtomicBool>,
}

impl SyncScheduler {
    /// Start the scheduler. Returns immediately — the loop runs on a
    /// detached tokio task until `stop()` is called or the handle is
    /// dropped.
    ///
    /// `app` is cloned into the task so the task owns its handle; the
    /// `AppHandle` is cheap to clone (it's an `Arc` internally).
    pub fn start(app: AppHandle) -> Arc<Self> {
        Self::start_with_tick(app, TICK, DEBOUNCE_WINDOW)
    }

    /// Test-only constructor: inject a shorter tick / debounce so tests
    /// don't have to wait minutes. Production should call `start`.
    pub fn start_with_tick(app: AppHandle, tick: Duration, debounce: Duration) -> Arc<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let retry_kick = Arc::new(AtomicBool::new(false));
        let retry_reset = Arc::new(AtomicBool::new(false));
        let signals = LoopSignals {
            cancel: Arc::clone(&cancel),
            retry_kick: Arc::clone(&retry_kick),
            retry_reset: Arc::clone(&retry_reset),
        };
        let app_for_task = app.clone();
        tauri::async_runtime::spawn(async move {
            run_loop(app_for_task, signals, tick, debounce).await;
        });
        Arc::new(Self {
            cancel,
            retry_kick,
            retry_reset,
        })
    }

    /// Signal cancellation. Safe to call multiple times; subsequent
    /// calls are no-ops.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Ask the scheduler to drop any open backoff window so its next tick
    /// re-evaluates the rules immediately (rate-limited by
    /// `KICK_COOLDOWN_MS`). Called when the window regains focus. A no-op
    /// when nothing failed. Wake-from-sleep is detected separately inside
    /// the run loop via `slept_between_ticks`.
    pub fn request_retry_now(&self) {
        self.retry_kick.store(true, Ordering::Relaxed);
    }

    /// Reset the failure backoff entirely (window + streak). Called by the
    /// manual `sync_now` command: the user intervened, so the scheduler
    /// must not stay suppressed afterwards.
    pub fn reset_backoff(&self) {
        self.retry_reset.store(true, Ordering::Relaxed);
    }
}

/// Atomic flags shared between the `SyncScheduler` handle and its loop.
struct LoopSignals {
    cancel: Arc<AtomicBool>,
    retry_kick: Arc<AtomicBool>,
    retry_reset: Arc<AtomicBool>,
}

impl Drop for SyncScheduler {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

async fn run_loop(app: AppHandle, signals: LoopSignals, tick: Duration, debounce: Duration) {
    let LoopSignals {
        cancel,
        retry_kick,
        retry_reset,
    } = signals;
    let mut on_launch_done = false;
    // Failure retry cursor — wiped on process exit. A successful sync
    // resets it; a failure opens an exponential backoff window that
    // `evaluate_with_error_cursor` honours for Rules 1 & 2.
    let mut retry = RetryState::default();

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Stamp immediately before the sleep so the gap below measures the
        // sleep alone — never a slow sync cycle from the previous iteration.
        let before_sleep_ms = unix_now_s().saturating_mul(1000);
        tokio::time::sleep(tick).await;
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let now_ms = unix_now_s().saturating_mul(1000);

        // Manual Sync now: the user intervened — full reset. Consumed here
        // (not where the command runs) so it cannot race a cycle in flight.
        if retry_reset.swap(false, Ordering::Relaxed) {
            retry.reset();
        }

        // Wake-from-sleep (the wall clock jumped far past one tick, so the
        // machine was suspended): drop the open window so this evaluation
        // may retry — a backoff opened before the nap must not outlive it.
        if slept_between_ticks(before_sleep_ms, now_ms, tick) {
            retry.drop_window();
        }

        // Focus kick (rate-limited inside `kick`). Consumed here so a kick
        // that arrives while a sync is in flight is not lost.
        if retry_kick.swap(false, Ordering::Relaxed) {
            retry.kick(now_ms);
        }

        // All state reads happen via Tauri managed state pulled from the
        // AppHandle. We clone the `Arc`-backed states each iteration;
        // this is cheap and avoids holding references across the await.
        use tauri::Manager;
        let state = app.state::<AppState>();
        let last_edit = app.state::<LastEditState>();

        // Decide whether any rule fires. Unlocked read — DB access is
        // one short synchronous block, no await inside.
        let decision = match evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut on_launch_done,
            debounce,
            retry.backoff_until_ms,
        ) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("sync scheduler: evaluate failed: {e}");
                continue;
            }
        };

        let Some(reason) = decision else {
            continue;
        };

        log::info!("sync scheduler: firing sync ({reason:?})");

        // `run_sync_now` uses `tauri::async_runtime::block_on` inside,
        // which must not be called from inside an async context — wrap
        // in `spawn_blocking`. The wrapped call acquires the AppState
        // mutex only briefly, matching the same lock discipline the
        // manual `sync_now` command uses.
        let app_clone = app.clone();
        let join = tauri::async_runtime::spawn_blocking(move || {
            use tauri::Manager;
            let state = app_clone.state::<AppState>();
            let key_state = app_clone.state::<EncryptionKeyState>();
            run_sync_now(&app_clone, &state, &key_state, SCHEDULER_SYNC_TRIGGER)
        });

        match join.await {
            Ok(Ok(summary)) => {
                // Success clears the backoff. A "success with per-entry
                // errors" counts as a failure for backoff purposes — same
                // as `run_sync_now`'s last_sync_at logic.
                if summary.errors.is_empty() {
                    retry.reset();
                    emit_retry_scheduled(&app, None);
                } else {
                    record_failure(
                        &mut retry,
                        &format!("sync completed with {} error(s)", summary.errors.len()),
                    );
                    emit_retry_scheduled(&app, retry.backoff_until_ms);
                }
            }
            Ok(Err(e)) if e.contains("Encryption key not initialized") => {
                // Locked app — normal steady-state between sessions.
                // Do NOT stamp the error cursor: the user unlocking is
                // the natural retry trigger, and backoff would delay
                // the first post-unlock sync unnecessarily.
            }
            Ok(Err(e)) if e == crate::commands::sync::SYNC_IN_PROGRESS_ERR => {
                // Another caller holds the single-flight slot. Hold
                // Rules 1+2 for one tick so Interval cannot re-fire
                // every TICK while that caller still owns the slot.
                // Not a failure: no streak bump, no retry-scheduled
                // event (the in-flight sync is the work we'd have done).
                // +1ms: backoff_open is exclusive at the boundary, so
                // a hold of exactly one tick would expire on the next
                // tick and Interval would still fire every 5 s.
                let interval_ms = tick.as_millis() as i64;
                retry.hold_until(interval_hold_until_ms(now_ms, interval_ms));
            }
            Ok(Err(e)) => {
                record_failure(&mut retry, &format!("sync_now failed: {e}"));
                emit_retry_scheduled(&app, retry.backoff_until_ms);
            }
            Err(join_err) => {
                record_failure(
                    &mut retry,
                    &format!("spawn_blocking join error: {join_err}"),
                );
                emit_retry_scheduled(&app, retry.backoff_until_ms);
            }
        }
    }
}

/// Broadcast the retry schedule (or its absence) to the frontend.
fn emit_retry_scheduled(app: &AppHandle, retry_at_ms: Option<i64>) {
    let payload = SyncRetryScheduledPayload { retry_at_ms };
    if let Err(e) = app.emit(SYNC_RETRY_SCHEDULED_EVENT, payload) {
        log::warn!("emit_retry_scheduled: failed to emit: {e}");
    }
}
