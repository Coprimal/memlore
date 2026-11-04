//! Stall guard for a sync cycle: a [`ProgressReporter`] wrapper that stamps
//! the last progress time, and the inactivity / hard-cap timeout that races
//! the cycle against it. Moved from the desktop `commands::sync` so the
//! mobile peer runs the same guard.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::engine::{CatchupProgressEvent, ProgressReporter, SyncProgressEvent};

/// Wraps another [`ProgressReporter`] and stamps `last_progress_ms` on every
/// `report()` and `heartbeat()` call before forwarding to the inner
/// reporter (`report()`) or doing nothing further (`heartbeat()`, which has
/// no IPC-facing counterpart). `run_sync_now`'s stall guard reads
/// `last_progress_ms` to tell "slow because there is a lot of legitimate
/// work" apart from "hung" — see [`with_stall_guard`].
#[doc(hidden)]
pub struct StallTrackingReporter {
    pub inner: Arc<dyn ProgressReporter>,
    pub last_progress_ms: Arc<AtomicU64>,
    pub baseline: std::time::Instant,
}

impl StallTrackingReporter {
    fn stamp(&self) {
        let now = self.baseline.elapsed().as_millis() as u64;
        self.last_progress_ms.store(now, Ordering::SeqCst);
    }
}

impl ProgressReporter for StallTrackingReporter {
    fn report(&self, event: SyncProgressEvent) {
        self.stamp();
        self.inner.report(event);
    }

    fn report_catchup_progress(&self, event: CatchupProgressEvent) {
        self.stamp();
        self.inner.report_catchup_progress(event);
    }

    fn heartbeat(&self) {
        self.stamp();
    }
}

/// Returned by [`with_stall_guard`] / [`retry_with_stall_guard`] when the
/// guarded future was aborted before completing — distinct from "still
/// working, just slow."
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStalled {
    /// No progress signal (`report`/`heartbeat`, via [`StallTrackingReporter`])
    /// was recorded for the full stall window — the engine appears to have
    /// gone silent.
    NoProgress,
    /// The attempt ran past the hard cap even though progress kept
    /// flowing. A backstop against a workload that keeps the stall check
    /// satisfied forever (e.g. an endless stream of cheap items over a slow
    /// link) — durable per-item progress (pull commits per chunk, push
    /// marks each entry synced) makes killing the attempt here safe.
    HardCapExceeded,
}

/// Human-readable reason text shared between the retry-boundary log line
/// and the final user-facing error string, so the two can never drift.
#[doc(hidden)]
pub fn stall_reason_text(reason: SyncStalled, stall: Duration, hard_cap: Duration) -> String {
    match reason {
        SyncStalled::NoProgress => format!("no progress for {}s", stall.as_secs()),
        SyncStalled::HardCapExceeded => {
            format!("exceeded the {}-minute hard cap", hard_cap.as_secs() / 60)
        }
    }
}

/// Race `fut` against a stall (inactivity) timeout AND an absolute hard cap:
/// every 5s tick the guard checks (a) how long it's been since
/// `last_progress_ms` last advanced (per `now_ms`) — if that gap exceeds
/// `stall`, the guard aborts with [`SyncStalled::NoProgress`] even though
/// `fut` is still running; (b) how long the attempt has run in total — if
/// that exceeds `hard_cap`, the guard aborts with
/// [`SyncStalled::HardCapExceeded`] regardless of how recently progress was
/// stamped. A future that keeps stamping `last_progress_ms` — via a
/// [`StallTrackingReporter`]'s `report`/`heartbeat` calls — can run for as
/// long as `hard_cap` without tripping the stall check, but never longer.
///
/// `last_progress_ms` is stamped to `now_ms()` once at guard start (also
/// used as the hard-cap's `attempt_start`), so a future whose first
/// progress signal doesn't arrive until partway through its work is still
/// measured from a sane baseline.
///
/// `now_ms` is injected (rather than reading `SystemTime`/`Instant`
/// directly) so tests can drive a `tokio::time::Instant`-based clock under
/// `#[tokio::test(start_paused = true)]` and advance it deterministically
/// with `tokio::time::advance` — production passes a real-time clock based
/// on `std::time::Instant`.
#[doc(hidden)]
pub async fn with_stall_guard<T>(
    fut: impl std::future::Future<Output = T>,
    last_progress_ms: Arc<AtomicU64>,
    now_ms: impl Fn() -> u64,
    stall: Duration,
    hard_cap: Duration,
) -> Result<T, SyncStalled> {
    let attempt_start = now_ms();
    last_progress_ms.store(attempt_start, Ordering::SeqCst);
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    // `interval`'s first tick fires immediately; consume it up front so the
    // first real stall check happens after a full interval has elapsed, not
    // instantly at guard start.
    ticker.tick().await;
    loop {
        tokio::select! {
            result = &mut fut => return Ok(result),
            _ = ticker.tick() => {
                let now = now_ms();
                let last = last_progress_ms.load(Ordering::SeqCst);
                if now.saturating_sub(last) > stall.as_millis() as u64 {
                    return Err(SyncStalled::NoProgress);
                }
                if now.saturating_sub(attempt_start) > hard_cap.as_millis() as u64 {
                    return Err(SyncStalled::HardCapExceeded);
                }
            }
        }
    }
}

/// Drive [`with_stall_guard`] across up to `max_attempts` attempts, calling
/// `make_attempt()` fresh each time — the same retry semantics
/// `run_sync_now` had inline, extracted so the attempt/retry glue itself is
/// unit-testable without a real Tauri `AppHandle`.
///
/// `on_retry(attempt, reason)` fires after every non-final aborted attempt
/// (never after the last) — callers use it to log and re-arm the frontend
/// watchdog. Returns `Ok(T)` from whichever attempt completes, or
/// `Err(reason)` from the final attempt if every attempt was aborted.
#[doc(hidden)]
pub async fn retry_with_stall_guard<T, Fut>(
    mut make_attempt: impl FnMut() -> Fut,
    last_progress_ms: Arc<AtomicU64>,
    now_ms: impl Fn() -> u64,
    stall: Duration,
    hard_cap: Duration,
    max_attempts: u32,
    mut on_retry: impl FnMut(u32, SyncStalled),
) -> Result<T, SyncStalled>
where
    Fut: std::future::Future<Output = T>,
{
    let mut attempt: u32 = 1;
    loop {
        match with_stall_guard(
            make_attempt(),
            last_progress_ms.clone(),
            &now_ms,
            stall,
            hard_cap,
        )
        .await
        {
            Ok(res) => return Ok(res),
            Err(reason) if attempt < max_attempts => {
                on_retry(attempt, reason);
                attempt += 1;
            }
            Err(reason) => return Err(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── with_stall_guard ──────────────────────────────────────────────────
    //
    // `start_paused = true` makes `tokio::time` (sleep/interval/advance) run
    // on a virtual clock — no real waiting. `now_ms` is built from
    // `tokio::time::Instant::now()` so `tokio::time::advance` moves both the
    // guard's internal ticker and the injected clock in lockstep.

    /// A hard cap so generous it never fires within these tests' windows —
    /// used whenever a test only cares about the stall (inactivity) check.
    fn no_effective_hard_cap() -> Duration {
        Duration::from_secs(24 * 60 * 60)
    }

    #[tokio::test(start_paused = true)]
    async fn with_stall_guard_completes_ok_for_a_future_that_finishes_quickly() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;

        let result = with_stall_guard(
            async { 42 },
            last_progress_ms,
            now_ms,
            Duration::from_secs(20),
            no_effective_hard_cap(),
        )
        .await;

        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test(start_paused = true)]
    async fn with_stall_guard_errs_when_no_progress_is_ever_stamped() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;
        let stall = Duration::from_secs(20);

        // A future that never resolves and never stamps progress.
        let guard = tokio::spawn(with_stall_guard(
            std::future::pending::<()>(),
            last_progress_ms,
            now_ms,
            stall,
            no_effective_hard_cap(),
        ));

        // Advance well past the stall window; the guard's 5s ticker must
        // notice the silence and abort before this returns.
        tokio::time::advance(stall + Duration::from_secs(10)).await;

        let result = guard.await.expect("guard task must not panic");
        assert_eq!(
            result,
            Err(SyncStalled::NoProgress),
            "expected NoProgress, got {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_stall_guard_survives_a_future_that_keeps_stamping_progress() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;
        let stall = Duration::from_secs(20);

        // Runs for 3x the stall window (6 steps of 10s = 60s), but stamps
        // progress every step — well inside the 20s stall window each time —
        // so the guard must let it run to completion instead of aborting.
        let progress_for_stamping = last_progress_ms.clone();
        let slow_but_progressing = async move {
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_secs(10)).await;
                progress_for_stamping
                    .store(baseline.elapsed().as_millis() as u64, Ordering::SeqCst);
            }
            "done"
        };

        let guard = tokio::spawn(with_stall_guard(
            slow_but_progressing,
            last_progress_ms,
            now_ms,
            stall,
            no_effective_hard_cap(),
        ));

        tokio::time::advance(Duration::from_secs(65)).await;

        let result = guard.await.expect("guard task must not panic");
        assert_eq!(result.unwrap(), "done");
    }

    #[tokio::test(start_paused = true)]
    async fn with_stall_guard_errs_with_hard_cap_exceeded_even_with_constant_progress() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;
        // A generous stall window (never trips) paired with a short hard
        // cap: a future that stamps progress on every tick would satisfy
        // `with_stall_guard_survives_...` above forever, but must still be
        // killed once the hard cap elapses.
        let stall = Duration::from_secs(24 * 60 * 60);
        let hard_cap = Duration::from_secs(30);

        let progress_for_stamping = last_progress_ms.clone();
        let stamps_forever: std::future::Pending<()> = {
            // Spawn a sibling task that keeps stamping progress forever;
            // the guarded future itself never resolves (mirrors a
            // genuinely endless workload with a slow-but-steady trickle of
            // progress).
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    progress_for_stamping
                        .store(baseline.elapsed().as_millis() as u64, Ordering::SeqCst);
                }
            });
            std::future::pending()
        };

        let guard = tokio::spawn(with_stall_guard(
            stamps_forever,
            last_progress_ms,
            now_ms,
            stall,
            hard_cap,
        ));

        tokio::time::advance(hard_cap + Duration::from_secs(10)).await;

        let result = guard.await.expect("guard task must not panic");
        assert_eq!(
            result,
            Err(SyncStalled::HardCapExceeded),
            "expected HardCapExceeded, got {result:?}"
        );
    }

    // ── retry_with_stall_guard ────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn retry_with_stall_guard_succeeds_after_one_stalled_attempt() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;
        let stall = Duration::from_secs(20);

        // Attempt 1 never stamps progress and never resolves → stalls.
        // Attempt 2 resolves immediately → succeeds.
        let attempt_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let make_attempt = {
            let attempt_count = attempt_count.clone();
            move || {
                let n = attempt_count.fetch_add(1, Ordering::SeqCst);
                let fut: std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>> =
                    if n == 0 {
                        Box::pin(std::future::pending())
                    } else {
                        Box::pin(async { "ok" })
                    };
                fut
            }
        };

        let retry_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let on_retry = {
            let retry_count = retry_count.clone();
            move |_attempt: u32, _reason: SyncStalled| {
                retry_count.fetch_add(1, Ordering::SeqCst);
            }
        };

        let guard = tokio::spawn(retry_with_stall_guard(
            make_attempt,
            last_progress_ms,
            now_ms,
            stall,
            no_effective_hard_cap(),
            3,
            on_retry,
        ));

        tokio::time::advance(stall + Duration::from_secs(10)).await;

        let result = guard.await.expect("guard task must not panic");
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(
            retry_count.load(Ordering::SeqCst),
            1,
            "on_retry should fire exactly once (after the one stalled attempt)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_with_stall_guard_errs_after_max_attempts_all_stall() {
        let last_progress_ms = Arc::new(AtomicU64::new(0));
        let baseline = tokio::time::Instant::now();
        let now_ms = move || baseline.elapsed().as_millis() as u64;
        let stall = Duration::from_secs(20);
        let max_attempts = 3u32;

        let retry_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let on_retry = {
            let retry_count = retry_count.clone();
            move |_attempt: u32, _reason: SyncStalled| {
                retry_count.fetch_add(1, Ordering::SeqCst);
            }
        };

        let guard = tokio::spawn(retry_with_stall_guard(
            || std::future::pending::<()>(),
            last_progress_ms,
            now_ms,
            stall,
            no_effective_hard_cap(),
            max_attempts,
            on_retry,
        ));

        // Every attempt gets its own fresh stall window, so the total time
        // to exhaust all attempts is roughly max_attempts * (stall + a tick).
        tokio::time::advance(stall * max_attempts + Duration::from_secs(30)).await;

        let result = guard.await.expect("guard task must not panic");
        assert_eq!(result, Err(SyncStalled::NoProgress));
        assert_eq!(
            retry_count.load(Ordering::SeqCst),
            max_attempts - 1,
            "on_retry must fire for every non-final attempt, never for the last"
        );
    }
}
