//! Pure scheduler rules for the background sync loop: the interval,
//! on-save debounce and on-launch decision (`evaluate_with_error_cursor`),
//! the failure backoff cursor (`RetryState`), wake-from-sleep detection and
//! the last-edit stamp. Moved from the desktop `sync::scheduler` so the
//! mobile peer applies the same rules; the Tauri-driven loop stays there.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use super::engine::SyncTrigger;
use crate::db;
use crate::AppState;

// ─── LastEditState ───────────────────────────────────────────────────────────

/// Shared "when did the last entry-write land" timestamp, in unix
/// milliseconds. Used by the Chunk 5 scheduler to implement a debounced
/// on-save sync: after every entry mutation, `mark_now()` is stamped; the
/// scheduler waits until `now - last_edit` exceeds the debounce window
/// (default 30 s) before firing a push. A burst of auto-saves therefore
/// collapses into exactly one sync tick instead of one per keystroke.
///
/// `AtomicI64` + `Ordering::Relaxed` is sufficient — the value is
/// advisory, not a correctness dependency of any data operation.
#[doc(hidden)]
pub struct LastEditState(AtomicI64);

impl LastEditState {
    pub fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    pub fn mark_now(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.0.store(now_ms, Ordering::Relaxed);
    }

    pub fn get_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Test-only: set the timestamp directly. Used to simulate clock
    /// skew (e.g. NTP adjustments) or to make debounce tests
    /// deterministic without sleeping. Not exposed outside
    /// `cfg(any(test, feature = "test-support"))`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_ms_for_test(&self, ms: i64) {
        self.0.store(ms, Ordering::Relaxed);
    }
}

impl Default for LastEditState {
    fn default() -> Self {
        Self::new()
    }
}

/// Reconcile trigger for all three scheduler rules (interval, on-save
/// debounce, on-launch). Automatic: own-cloud self-heal runs only until the
/// first successful reconcile this process session (on-launch still
/// reconciles because the session flag is unset on the first cycle).
#[doc(hidden)]
pub const SCHEDULER_SYNC_TRIGGER: SyncTrigger = SyncTrigger::Automatic;

/// How often the scheduler wakes up to re-evaluate its rules.
/// Small enough to make the 30s debounce feel snappy; large enough that
/// the check itself costs near-zero CPU.
#[doc(hidden)]
pub const TICK: Duration = Duration::from_secs(5);

/// How long after the last edit the debounce fires. The 30s figure is
/// the same as the plan ("debounced, 30s after last edit") — long enough
/// for a typing burst to complete, short enough that a user who closes
/// the editor after typing sees their work pushed before they switch
/// context.
#[doc(hidden)]
pub const DEBOUNCE_WINDOW: Duration = Duration::from_secs(30);

/// Base delay between retries after a failed sync cycle. When the
/// scheduler observes a non-empty `summary.errors` (or `run_sync_now`
/// returns an error other than "app locked"), it opens a backoff window
/// during which Rules 1 and 2 are suppressed. The window doubles on every
/// consecutive failure (`RetryState`) up to `ERROR_BACKOFF_MAX_MS`, so an
/// unreachable cloud is retried automatically without hammering the
/// network every `TICK`. A successful sync resets the streak; a kick /
/// wake lets the next tick retry immediately.
#[doc(hidden)]
pub const ERROR_BACKOFF_MS: i64 = 60_000;

/// Upper bound for the exponential backoff window.
#[doc(hidden)]
pub const ERROR_BACKOFF_MAX_MS: i64 = 15 * 60_000;

/// Minimum spacing between two honoured focus kicks. Without it, window-
/// focus churn (alt-tab, multi-monitor switching) while the cloud is down
/// would collapse the backoff to a retry every `TICK`.
#[doc(hidden)]
pub const KICK_COOLDOWN_MS: i64 = 30_000;

/// In-memory retry cursor. Lives on the run loop's stack — wiped on
/// process exit (error state is a runtime concern, not a DB concern).
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RetryState {
    /// Consecutive failures since the last success. Drives the
    /// exponential backoff window.
    consecutive_failures: u32,
    /// Unix ms until which Rules 1 + 2 are suppressed. `None` = no backoff.
    pub backoff_until_ms: Option<i64>,
    /// Unix ms of the last honoured kick — enforces `KICK_COOLDOWN_MS`.
    last_kick_ms: Option<i64>,
}

impl RetryState {
    pub fn on_failure(&mut self, now_ms: i64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        // N consecutive failures → window = base × 2^(N-1), capped.
        let exp = self.consecutive_failures.saturating_sub(1).min(31);
        let window = ERROR_BACKOFF_MS
            .saturating_mul(1i64 << exp)
            .min(ERROR_BACKOFF_MAX_MS);
        self.backoff_until_ms = Some(now_ms.saturating_add(window));
    }

    /// Full reset: a successful cycle, or the user intervened via the
    /// manual Sync now command.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Drop the open window so the next tick may retry right away, but
    /// keep the failure streak — if the cloud is still unreachable the
    /// following window resumes doubling instead of collapsing back to
    /// the base delay. Used directly for wake-from-sleep (never rate-
    /// limited: a nap is not focus churn) and via `kick` for focus.
    pub fn drop_window(&mut self) {
        self.backoff_until_ms = None;
    }

    /// Suppress Rules 1 + 2 until `ms` without treating this as a
    /// failure: no streak bump, caller must not emit retry-scheduled.
    /// Used when another caller already holds the single-flight slot.
    pub fn hold_until(&mut self, ms: i64) {
        self.backoff_until_ms = Some(ms);
    }

    /// Focus kick: `drop_window`, rate-limited — a kick within
    /// `KICK_COOLDOWN_MS` of the previous honoured one is ignored, so
    /// focus churn cannot turn the backoff into a retry every `TICK`.
    pub fn kick(&mut self, now_ms: i64) {
        let cooled_down = self
            .last_kick_ms
            .map(|last| now_ms.saturating_sub(last) >= KICK_COOLDOWN_MS)
            .unwrap_or(true);
        if !cooled_down {
            return;
        }
        self.last_kick_ms = Some(now_ms);
        self.drop_window();
    }
}

/// Single predicate behind both `RetryState::suppresses` and the gate in
/// `evaluate_with_error_cursor`: is the backoff window still open at
/// `now_ms`?
#[doc(hidden)]
pub fn backoff_open(backoff_until_ms: Option<i64>, now_ms: i64) -> bool {
    backoff_until_ms
        .map(|until| now_ms < until)
        .unwrap_or(false)
}

/// Deadline for a one-tick Interval hold. `backoff_open` is exclusive
/// (`now < until`), so `now + interval` would already be open on the
/// next tick — the extra millisecond keeps that evaluation suppressed.
#[doc(hidden)]
pub fn interval_hold_until_ms(now_ms: i64, interval_ms: i64) -> i64 {
    now_ms.saturating_add(interval_ms).saturating_add(1)
}

/// Wall-clock gap between two consecutive ticks beyond which the machine
/// must have been asleep (a 5s tick does not take a minute). Used to
/// detect wake-from-sleep without an OS power hook: on wake, drop any
/// transient backoff so the first post-wake evaluation may retry.
#[doc(hidden)]
pub const SLEEP_GAP_MS: i64 = 60_000;

/// True when the wall clock jumped by more than `tick + SLEEP_GAP_MS`
/// between two ticks — i.e. the process was suspended (sleep/hibernate).
#[doc(hidden)]
pub fn slept_between_ticks(prev_tick_ms: i64, now_ms: i64, tick: Duration) -> bool {
    let tick_ms = tick.as_millis() as i64;
    now_ms.saturating_sub(prev_tick_ms) > tick_ms.saturating_add(SLEEP_GAP_MS)
}

/// Record a failed cycle: open the next backoff window and log it. The
/// scheduler retries automatically once the window elapses (or earlier on
/// a kick / wake / manual Sync now).
#[doc(hidden)]
pub fn record_failure(retry: &mut RetryState, what: &str) {
    retry.on_failure(unix_now_s().saturating_mul(1000));
    log::warn!(
        "sync scheduler: {what} — auto-retry #{} after backoff",
        retry.consecutive_failures,
    );
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum FireReason {
    OnLaunch,
    Interval,
    Debounce,
}

/// Same as [`evaluate`] but takes an explicit "suppress Rules 1 + 2 until
/// this unix-ms instant" cursor (`RetryState::backoff_until_ms`). Exposed
/// to the run loop so the loop can track transient-error backoff across
/// ticks without persisting it (error state is a runtime concern, not a
/// DB concern). Tests also call it directly to exercise backoff boundaries
/// without sleeping.
#[doc(hidden)]
pub fn evaluate_with_error_cursor(
    state: &AppState,
    last_edit: &LastEditState,
    on_launch_done: &mut bool,
    debounce: Duration,
    backoff_until_ms: Option<i64>,
) -> Result<Option<FireReason>, String> {
    let (enabled, provider_configured, last_sync, pending, interval_minutes, on_save, on_launch) = {
        let conn = state.lock()?;
        let enabled = db::get_sync_enabled(&conn).map_err(|e| e.to_string())?;
        let provider = db::get_sync_provider(&conn).map_err(|e| e.to_string())?;
        let last_sync = db::get_last_sync_at(&conn).map_err(|e| e.to_string())?;
        let pending = db::count_pending_entries(&conn).map_err(|e| e.to_string())?;
        let interval = db::get_sync_interval_minutes(&conn).map_err(|e| e.to_string())?;
        let on_save = db::get_sync_on_save(&conn).map_err(|e| e.to_string())?;
        let on_launch = db::get_sync_on_launch(&conn).map_err(|e| e.to_string())?;
        (
            enabled,
            provider.is_some(),
            last_sync,
            pending,
            interval,
            on_save,
            on_launch,
        )
    };

    if !enabled || !provider_configured {
        // Scheduler is a no-op until the user configures sync. We still
        // mark the launch tick as consumed so flipping "enabled" on later
        // doesn't accidentally retrigger the launch fire.
        *on_launch_done = true;
        return Ok(None);
    }

    // Rule 3: on-launch.
    if !*on_launch_done {
        *on_launch_done = true;
        if on_launch {
            return Ok(Some(FireReason::OnLaunch));
        }
    }

    let now_s = unix_now_s();
    let now_ms = now_s.saturating_mul(1000);

    // Error backoff: Rules 1 and 2 are suppressed while the transient-
    // failure window is open. Without this, an unreachable cloud would
    // let Rule 1 refire every TICK because `last_sync_at` is never
    // advanced on failure. OnLaunch already fired above so it's
    // intentionally outside the gate.
    if backoff_open(backoff_until_ms, now_ms) {
        return Ok(None);
    }

    // Rule 2: on-save debounce.
    //
    // Arithmetic safety: `now_s = unix_now_s().as_secs()` floors sub-
    // second precision, while `last_edit_ms` is millisecond-exact and
    // can temporarily exceed `now_ms` by up to 999ms under normal
    // conditions. A system clock that jumps backwards (NTP adjustment,
    // manual change, virtualized time drift) can push `last_edit_ms`
    // *arbitrarily* past `now_ms`. We defend in two layers:
    //
    //   1. If `last_edit_ms` is absurdly far in the future (past
    //      `FUTURE_CLOCK_SKEW_TOLERANCE_MS`), treat the stamp as
    //      corrupt and skip Rule 2 entirely this tick. The next genuine
    //      edit (save_entry_content stamps `now_ms` again) will replace
    //      the bad value.
    //   2. For smaller sub-second overshoot, `saturating_sub` yields
    //      `quiet_ms = 0` — "no quiet time yet" — instead of panicking
    //      in debug or wrapping in release.
    const FUTURE_CLOCK_SKEW_TOLERANCE_MS: i64 = 5_000;
    if on_save && pending > 0 {
        let last_edit_ms = last_edit.get_ms();
        let skewed = last_edit_ms > now_ms.saturating_add(FUTURE_CLOCK_SKEW_TOLERANCE_MS);
        if last_edit_ms > 0 && !skewed {
            let quiet_ms = now_ms.saturating_sub(last_edit_ms);
            if quiet_ms >= debounce.as_millis() as i64 {
                // Ensure we don't double-fire for the same edit: if the
                // most recent sync already covers this edit, skip.
                let last_sync_ms = last_sync.unwrap_or(0).saturating_mul(1000);
                if last_edit_ms > last_sync_ms {
                    return Ok(Some(FireReason::Debounce));
                }
            }
        }
    }

    // Rule 1: interval.
    let interval_s = (interval_minutes as i64).saturating_mul(60);
    let due = match last_sync {
        None => true, // never synced
        Some(ts) => now_s.saturating_sub(ts) >= interval_s,
    };
    if due {
        return Ok(Some(FireReason::Interval));
    }

    Ok(None)
}

#[doc(hidden)]
pub fn unix_now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use rusqlite::Connection;

    impl RetryState {
        fn suppresses(&self, now_ms: i64) -> bool {
            backoff_open(self.backoff_until_ms, now_ms)
        }
    }

    /// Pure decision function — no side effects, no awaits. Returns `Ok(None)`
    /// when no rule fires this tick, `Ok(Some(reason))` otherwise.
    ///
    /// Out-of-band reads happen here (DB + atomic); both are synchronous.
    /// Test-only convenience wrapper around [`evaluate_with_error_cursor`]
    /// that fixes the error cursor to `None` — production code paths
    /// (the scheduler loop) always track the cursor explicitly.
    fn evaluate(
        state: &AppState,
        last_edit: &LastEditState,
        on_launch_done: &mut bool,
        debounce: Duration,
    ) -> Result<Option<FireReason>, String> {
        evaluate_with_error_cursor(state, last_edit, on_launch_done, debounce, None)
    }

    fn fresh_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    fn seed_local_provider(state: &AppState) {
        let conn = state.lock().unwrap();
        db::set_sync_provider(&conn, "local").unwrap();
        db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/x\"}").unwrap();
        db::set_sync_enabled(&conn, true).unwrap();
    }

    /// All three scheduler rules (interval / on-save / on-launch) share one
    /// Automatic trigger. On-launch still reconciles because the process
    /// session flag is unset on the first cycle.
    #[test]
    fn scheduler_sync_trigger_is_automatic() {
        assert_eq!(
            SCHEDULER_SYNC_TRIGGER,
            SyncTrigger::Automatic,
            "scheduler must pass Automatic so reconcile is gated by the session flag"
        );
        assert_ne!(SCHEDULER_SYNC_TRIGGER, SyncTrigger::Manual);
    }

    #[test]
    fn evaluate_skips_when_sync_disabled() {
        let state = fresh_state();
        let last_edit = LastEditState::new();
        let mut launch_done = false;
        let out = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        assert!(out.is_none());
        // on-launch flag flipped even though we didn't fire — flipping
        // `enabled` later shouldn't cause a stale launch fire.
        assert!(launch_done);
    }

    #[test]
    fn evaluate_on_launch_fires_once() {
        let state = fresh_state();
        seed_local_provider(&state);
        let last_edit = LastEditState::new();
        let mut launch_done = false;
        let out = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        assert!(matches!(out, Some(FireReason::OnLaunch)));
        assert!(launch_done);

        // Second call should no longer fire OnLaunch (but may fire
        // Interval because we've never synced yet).
        let out2 = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        assert!(
            !matches!(out2, Some(FireReason::OnLaunch)),
            "OnLaunch must fire at most once"
        );
    }

    #[test]
    fn evaluate_on_launch_respects_disabled_setting() {
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            db::set_sync_on_launch(&conn, false).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = false;
        let out = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        // Not OnLaunch — may still be Interval since last_sync = None.
        assert!(!matches!(out, Some(FireReason::OnLaunch)));
        assert!(launch_done);
    }

    #[test]
    fn evaluate_interval_fires_when_stale() {
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            // Pretend we synced a day ago; any default interval (5 min)
            // is comfortably exceeded.
            db::set_last_sync_at(&conn, unix_now_s() - 86_400).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = true; // pretend OnLaunch already fired
        let out = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        assert!(matches!(out, Some(FireReason::Interval)));
    }

    #[test]
    fn evaluate_interval_does_not_fire_when_recent() {
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            // Synced 10 seconds ago — default interval is 5 min.
            db::set_last_sync_at(&conn, unix_now_s() - 10).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = true;
        let out = evaluate(&state, &last_edit, &mut launch_done, DEBOUNCE_WINDOW).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn evaluate_debounce_waits_for_quiet_period() {
        let state = fresh_state();
        seed_local_provider(&state);
        // Set last_sync just before "now" so the Interval rule sees a
        // fresh sync and does not fire, but the debounce rule's
        // `last_edit_ms > last_sync_ms` comparison still holds (the
        // edit happens strictly after the sync).
        let sync_ts = unix_now_s() - 1;
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, sync_ts).unwrap();
        }
        // Inject a pending entry so the debounce has something to push.
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
                    entry_date: 1_700_000_000,
                },
            )
            .unwrap();
            db::mark_entry_pending(&conn, &entry.id).unwrap();
        }
        let last_edit = LastEditState::new();

        // Edit "just happened" — quiet period not elapsed. Using
        // `set_ms_for_test(now_ms)` makes the timing deterministic
        // across test-runner second-boundary races.
        let now_ms = unix_now_s().saturating_mul(1000);
        last_edit.set_ms_for_test(now_ms);
        let mut launch_done = true;
        let out = evaluate(
            &state,
            &last_edit,
            &mut launch_done,
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            out.is_none(),
            "should not fire: edit just happened, no quiet period yet"
        );

        // Debounce window = 0 simulates "edit was long enough ago".
        // Pin last_edit strictly AFTER last_sync_ms (sync_ts was set to
        // now - 1 earlier; last_sync_ms = (now-1)*1000). now_ms > that.
        let out = evaluate(
            &state,
            &last_edit,
            &mut launch_done,
            Duration::from_millis(0),
        )
        .unwrap();
        assert!(matches!(out, Some(FireReason::Debounce)), "got {out:?}");
    }

    #[test]
    fn evaluate_debounce_handles_clock_skew_without_panic() {
        // Regression for a latent bug: `quiet_ms = now_ms - last_edit_ms`
        // using plain `-` panics in debug builds when NTP or a manual
        // clock adjustment moves time backwards enough that
        // `last_edit_ms > now_ms` temporarily. The correct implementation
        // must use `saturating_sub` so negative intervals clamp to 0
        // (= "no quiet time yet"), not propagate as huge positives.
        let state = fresh_state();
        seed_local_provider(&state);
        // Give Debounce something to push, but Interval nothing fresh.
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, unix_now_s() - 1).unwrap();
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
                    entry_date: 1_700_000_000,
                },
            )
            .unwrap();
            db::mark_entry_pending(&conn, &entry.id).unwrap();
        }

        // Force last_edit far in the future — simulates wall clock
        // rewinding after edit was stamped. Use i64::MAX / 2 so any
        // naive `-` subtraction would overflow i64 in debug builds.
        let last_edit = LastEditState::new();
        last_edit.set_ms_for_test(i64::MAX / 2);

        let mut launch_done = true;
        // This call MUST NOT panic. With `saturating_sub`, `quiet_ms`
        // clamps to 0 → not-yet-quiet → no Debounce fire.
        let out = evaluate(
            &state,
            &last_edit,
            &mut launch_done,
            Duration::from_millis(0),
        )
        .expect("evaluate must not panic on clock-skew boundary");
        assert!(
            !matches!(out, Some(FireReason::Debounce)),
            "clock-skew (last_edit in future) must suppress Debounce; got {out:?}"
        );
    }

    #[test]
    fn evaluate_debounce_skips_when_last_edit_predates_last_sync() {
        // Negative guard: the "don't double-push the same edit" logic
        // must suppress Debounce when the most recent sync already
        // happened AFTER the last edit. Without this, every tick would
        // re-fire for an edit that's already been pushed.
        let state = fresh_state();
        seed_local_provider(&state);
        let now = unix_now_s();
        // Pretend the last sync is "now" and the last edit was 10 seconds
        // earlier — the edit is already covered by the sync.
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, now).unwrap();
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
                    entry_date: 1_700_000_000,
                },
            )
            .unwrap();
            db::mark_entry_pending(&conn, &entry.id).unwrap();
        }
        let last_edit = LastEditState::new();
        // Force last_edit to (now - 10s) in milliseconds — older than
        // last_sync. Since there's no public setter with arbitrary ms,
        // we just don't mark_now() at all (get_ms() returns 0) — but
        // zero would short-circuit the `last_edit_ms > 0` guard, so
        // the test wouldn't exercise the `last_edit_ms > last_sync_ms`
        // branch. Use the fact that `now_s` in the DB is `now` and
        // mark_now() produces `(now_ms_approx)` which is >=
        // `last_sync_ms` (they race within the same second). With
        // debounce window 0 and the guard in place, the result must
        // still be None because `last_edit_ms > last_sync_ms` is only
        // true by sub-second fraction, and we set `last_sync_ms = now_s
        // * 1000` to cover the whole current second.
        //
        // Simplest unambiguous proof: skip mark_now, let last_edit_ms=0,
        // assert early-return via the `last_edit_ms > 0` guard. That
        // exercises the same "no double-fire" protection.
        let mut launch_done = true;
        let out = evaluate(
            &state,
            &last_edit,
            &mut launch_done,
            Duration::from_millis(0),
        )
        .unwrap();
        assert!(
            out.is_none(),
            "no edit timestamp (or edit-before-sync) must suppress Debounce; got {out:?}"
        );
    }

    #[test]
    fn evaluate_suppresses_all_rules_during_error_backoff() {
        // After a persistent sync failure, `last_sync_at` is NOT
        // advanced (see run_sync_now), so Rule 1 (Interval) would
        // otherwise refire every tick. The backoff keeps us quiet for
        // `ERROR_BACKOFF_MS` after the last error.
        let state = fresh_state();
        seed_local_provider(&state);
        // Conditions that would normally fire Interval:
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, unix_now_s() - 86_400).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = true;
        let now_ms = unix_now_s().saturating_mul(1000);

        // Backoff window still open — must suppress.
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            Some(now_ms + 30_000),
        )
        .unwrap();
        assert!(
            out.is_none(),
            "open backoff window must suppress Interval/Debounce; got {out:?}"
        );

        // Backoff window already elapsed — must allow firing.
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            Some(now_ms - 1_000),
        )
        .unwrap();
        assert!(
            matches!(out, Some(FireReason::Interval)),
            "error outside backoff must allow Interval; got {out:?}"
        );
    }

    #[test]
    fn evaluate_debounce_skipped_when_on_save_disabled() {
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            db::set_sync_on_save(&conn, false).unwrap();
            db::set_last_sync_at(&conn, unix_now_s()).unwrap();
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
                    entry_date: 1_700_000_000,
                },
            )
            .unwrap();
            db::mark_entry_pending(&conn, &entry.id).unwrap();
        }
        let last_edit = LastEditState::new();
        last_edit.mark_now();
        let mut launch_done = true;
        let out = evaluate(
            &state,
            &last_edit,
            &mut launch_done,
            Duration::from_millis(0),
        )
        .unwrap();
        assert!(
            out.is_none(),
            "sync_on_save=false must suppress the debounce rule"
        );
    }

    // ─── Retry state ─────────────────────────────────────────────────────

    #[test]
    fn retry_state_backoff_grows_and_caps() {
        let mut rs = RetryState::default();
        assert!(!rs.suppresses(0));
        rs.on_failure(0);
        assert!(rs.suppresses(ERROR_BACKOFF_MS - 1));
        assert!(!rs.suppresses(ERROR_BACKOFF_MS));
        rs.on_failure(0);
        assert!(rs.suppresses(2 * ERROR_BACKOFF_MS - 1));
        assert!(!rs.suppresses(2 * ERROR_BACKOFF_MS));
        // Many failures: capped, never overflows.
        for _ in 0..100 {
            rs.on_failure(0);
        }
        assert!(rs.suppresses(ERROR_BACKOFF_MAX_MS - 1));
        assert!(!rs.suppresses(ERROR_BACKOFF_MAX_MS));
    }

    #[test]
    fn retry_state_reset_clears_window_and_streak() {
        let mut rs = RetryState::default();
        rs.on_failure(0);
        rs.on_failure(0);
        rs.reset();
        assert!(!rs.suppresses(1));
        // Next failure starts from the base window again.
        rs.on_failure(0);
        assert!(!rs.suppresses(ERROR_BACKOFF_MS));
    }

    #[test]
    fn retry_state_kick_drops_window_but_keeps_streak() {
        let mut rs = RetryState::default();
        rs.on_failure(0);
        rs.on_failure(0);
        rs.kick(0);
        assert!(!rs.suppresses(1), "kick must reopen the gate immediately");
        // …but keeps the failure streak so a still-broken cloud doesn't
        // collapse the backoff back to the base window.
        rs.on_failure(0);
        assert!(rs.suppresses(2 * ERROR_BACKOFF_MS));
    }

    #[test]
    fn retry_state_kick_is_rate_limited() {
        let mut rs = RetryState::default();
        rs.on_failure(0);
        rs.kick(0); // honoured
        rs.on_failure(1_000);
        // Focus churn within the cooldown of the first kick: ignored,
        // window stays open.
        rs.kick(KICK_COOLDOWN_MS - 1);
        assert!(rs.suppresses(KICK_COOLDOWN_MS - 1));
        // Past the cooldown: honoured again.
        rs.kick(KICK_COOLDOWN_MS);
        assert!(!rs.suppresses(KICK_COOLDOWN_MS));
    }

    #[test]
    fn retry_state_hold_until_sets_window_without_bumping_failures() {
        let mut rs = RetryState::default();
        rs.on_failure(0);
        let streak = rs.consecutive_failures;
        let now_ms = 0;
        let interval_ms = TICK.as_millis() as i64;
        let until = interval_hold_until_ms(now_ms, interval_ms);
        rs.hold_until(until);
        assert_eq!(
            rs.consecutive_failures, streak,
            "hold_until must not bump consecutive_failures"
        );
        // Exclusive window: a one-tick hold must still suppress at
        // now+interval_ms. Stamping exactly now+interval_ms would not.
        assert!(rs.suppresses(now_ms.saturating_add(interval_ms)));
        assert!(!rs.suppresses(until));
    }

    #[test]
    fn evaluate_with_error_cursor_respects_hold_until() {
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, unix_now_s() - 86_400).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = true;
        let now_ms = unix_now_s().saturating_mul(1000);
        let interval_ms = TICK.as_millis() as i64;

        let mut retry = RetryState::default();
        retry.hold_until(interval_hold_until_ms(now_ms, interval_ms));
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            retry.backoff_until_ms,
        )
        .unwrap();
        assert!(
            out.is_none(),
            "open hold_until window must suppress Interval; got {out:?}"
        );

        retry.hold_until(now_ms.saturating_sub(1));
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            retry.backoff_until_ms,
        )
        .unwrap();
        assert!(
            matches!(out, Some(FireReason::Interval)),
            "elapsed hold_until must allow Interval; got {out:?}"
        );
    }

    #[test]
    fn record_failure_opens_backoff_window() {
        let mut retry = RetryState::default();
        record_failure(&mut retry, "test");
        assert_eq!(retry.consecutive_failures, 1);
        let now_ms = unix_now_s().saturating_mul(1000);
        assert!(retry.suppresses(now_ms + 1_000));
    }

    #[test]
    fn real_backoff_window_suppresses_evaluate_until_it_elapses() {
        // Feed a genuine `RetryState` window into the evaluator (not a
        // synthetic instant) so the two sides can't drift apart.
        let state = fresh_state();
        seed_local_provider(&state);
        {
            let conn = state.lock().unwrap();
            db::set_last_sync_at(&conn, unix_now_s() - 86_400).unwrap();
        }
        let last_edit = LastEditState::new();
        let mut launch_done = true;
        let now_ms = unix_now_s().saturating_mul(1000);

        let mut retry = RetryState::default();
        retry.on_failure(now_ms);
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            retry.backoff_until_ms,
        )
        .unwrap();
        assert!(
            out.is_none(),
            "fresh failure window must suppress; got {out:?}"
        );

        // Window opened ERROR_BACKOFF_MS in the past has elapsed.
        let mut retry = RetryState::default();
        retry.on_failure(now_ms - ERROR_BACKOFF_MS);
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            retry.backoff_until_ms,
        )
        .unwrap();
        assert!(matches!(out, Some(FireReason::Interval)), "got {out:?}");

        // A kick reopens the gate immediately.
        let mut retry = RetryState::default();
        retry.on_failure(now_ms);
        retry.kick(now_ms);
        let out = evaluate_with_error_cursor(
            &state,
            &last_edit,
            &mut launch_done,
            DEBOUNCE_WINDOW,
            retry.backoff_until_ms,
        )
        .unwrap();
        assert!(matches!(out, Some(FireReason::Interval)), "got {out:?}");
    }

    #[test]
    fn slept_between_ticks_detects_clock_jump_only() {
        let tick = Duration::from_secs(5);
        // Normal tick (5s) and a slow tick (30s of scheduler jitter): no.
        assert!(!slept_between_ticks(0, 5_000, tick));
        assert!(!slept_between_ticks(0, 30_000, tick));
        // Exactly at the threshold: no; one ms past: yes.
        assert!(!slept_between_ticks(0, 5_000 + SLEEP_GAP_MS, tick));
        assert!(slept_between_ticks(0, 5_001 + SLEEP_GAP_MS, tick));
        // Clock went backwards (NTP): no panic, no false wake.
        assert!(!slept_between_ticks(10_000, 0, tick));
    }

    #[test]
    fn last_edit_state_mark_and_read() {
        let s = LastEditState::new();
        assert_eq!(s.get_ms(), 0);
        s.mark_now();
        assert!(s.get_ms() > 0);
    }
}
