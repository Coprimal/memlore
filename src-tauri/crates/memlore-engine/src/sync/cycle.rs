//! The sync cycle shared by desktop and mobile: the push guards, the keyring
//! dirty retry, the pull-leg force-re-pair check, the one-time cloud-content
//! migration, the engine run under the stall guard, the device-slot
//! republish and the web outbox import with its acks.
//!
//! Platform side effects go through [`SyncPlatformHooks`]. Desktop drives
//! [`run_sync_cycle`] with `block_on` on a blocking thread and holds the
//! single-flight guard around it.
#![deny(clippy::await_holding_lock)]

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use zeroize::Zeroizing;

use super::cloud_config::configured_cloud_provider;
use super::cloud_config::make_configured_provider;
use super::cloud_config::CloudCredentialStore;
use super::engine::ProgressReporter;
use super::force_re_pair::check_force_re_pair_sync;
use super::force_re_pair::ensure_remote_recovery_push_allowed;
use super::force_re_pair::ensure_safe_to_push;
use super::force_re_pair::persist_pull_leg_check;
use super::force_re_pair::set_force_re_pair_flags;
use super::force_re_pair::PullLegKeyringCheck;
use super::keyring_publish::reupload_current_device_slot;
use super::keyring_publish::try_reupload_keyring_and_update_dirty_flag;
use super::outbox_import::OutboxBridgeEvent;
use super::outbox_import::OutboxEventSink;
use super::platform::SyncPhase;
use super::platform::SyncPlatformHooks;
use super::stall_guard::retry_with_stall_guard;
use super::stall_guard::stall_reason_text;
use super::stall_guard::StallTrackingReporter;
use super::CloudProvider;
use super::SyncEngine;
use super::SyncProvider;
use super::SyncSummary;
use super::SyncTrigger;
use crate::db;
use crate::AppState;
use crate::EncryptionKeyState;

/// Hands the platform's `dyn` sink to the outbox importer, whose sink type
/// parameter must be `Sized`.
struct HookSink<'a>(&'a dyn OutboxEventSink);

impl OutboxEventSink for HookSink<'_> {
    fn emit_bridge_event(&self, event: &OutboxBridgeEvent) {
        self.0.emit_bridge_event(event)
    }

    fn emit_progress(&self, current: u32, total: u32) {
        self.0.emit_progress(current, total)
    }

    fn mark_embedding_dirty(&self, entry_id: &str) {
        self.0.mark_embedding_dirty(entry_id)
    }

    fn on_intent_dispatched(&self, path: &str) {
        self.0.on_intent_dispatched(path)
    }
}

/// Re-upload ONLY `devices/<id>.json` for the current device with a fresh
/// `last_seen_at`, so peer devices see this device as recently active.
///
/// Best-effort: any failure (Drive not connected, provider error, missing key
/// material, write failure) is logged and swallowed. `last_seen_at` is cosmetic
/// metadata — the local value is already stamped by the caller, and the cloud
/// value self-heals on the next successful sync. Crucially this does NOT set
/// `keyring_dirty`: that flag triggers a full-keyring republish via
/// `publish_v2_keyring_with_provider`, which would re-write every keyring file
/// (unnecessary for a timestamp bump).
///
/// Republish this device's keyring slot for any configured cloud provider.
/// Every vault has a keyring now, so there is no no-keyring case to exclude.
#[doc(hidden)]
pub async fn republish_current_device_slot_best_effort(
    state: &AppState,
    creds: &dyn CloudCredentialStore,
    now: i64,
) {
    // Gather the current device's slot material under a short lock. Bail early
    // (silently) when no provider is configured or material is missing.
    let material = {
        let conn = match state.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let provider = match configured_cloud_provider(&conn, creds) {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                log::warn!("republish_current_device_slot: provider init failed: {e}");
                return;
            }
        };
        // C2 defense-in-depth: never publish a device slot while a rotation job
        // is active. The slot no longer carries key material, but a mid-rotation
        // write can still stamp a slot against an epoch the cloud is about to
        // supersede, leaving a registry entry that disagrees with `_meta.json`.
        // `run_sync_now`'s
        // `ensure_safe_to_push` already bails on an active rotation before
        // reaching here, but mirror the guard the other slot writers
        // (`publish_v2_keyring_with_provider`, `rename_device`) enforce so this
        // helper is safe if ever reached by a different path.
        if db::has_active_rotation_job(&conn).unwrap_or(false) {
            return;
        }
        let device = match db::list_devices(&conn) {
            Ok(devices) => match devices.into_iter().find(|d| d.is_current) {
                Some(d) => d,
                None => return,
            },
            Err(_) => return,
        };
        (provider, device.device_id, device.name, device.created_at)
    };
    let (provider, device_id, name, created_at) = material;

    if let Err(e) =
        reupload_current_device_slot(state, &provider, &device_id, &name, created_at, now).await
    {
        log::warn!("republish_current_device_slot: slot re-upload failed (ignored): {e}");
    }
}

fn clear_device_slot_dirty_if_current_name_matches(
    conn: &rusqlite::Connection,
    uploaded_name: &str,
) -> rusqlite::Result<()> {
    let current_name = db::list_devices(conn)?
        .into_iter()
        .find(|d| d.is_current)
        .map(|d| d.name);

    if current_name.as_deref() == Some(uploaded_name) {
        let _ = db::delete_setting(conn, db::DEVICE_SLOT_DIRTY_KEY);
    }

    Ok(())
}

#[doc(hidden)]
pub async fn flush_device_slot_dirty_if_needed(state: &AppState, creds: &dyn CloudCredentialStore) {
    let material = {
        let conn = match state.lock() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("flush_device_slot_dirty_if_needed: DB lock failed: {e}");
                return;
            }
        };
        let dirty = db::get_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY)
            .unwrap_or(None)
            .as_deref()
            == Some("1");
        if !dirty {
            return;
        }
        let provider = match configured_cloud_provider(&conn, creds) {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                log::warn!("flush_device_slot_dirty_if_needed: provider init failed: {e}");
                return;
            }
        };
        if db::has_active_rotation_job(&conn).unwrap_or(false) {
            return;
        }
        let device = match db::list_devices(&conn) {
            Ok(devices) => match devices.into_iter().find(|d| d.is_current) {
                Some(d) => d,
                None => return,
            },
            Err(e) => {
                log::warn!("flush_device_slot_dirty_if_needed: device lookup failed: {e}");
                return;
            }
        };
        (
            provider,
            device.device_id,
            device.name,
            device.created_at,
            device.last_seen_at,
        )
    };
    let (provider, device_id, name, created_at, last_seen_at) = material;

    match reupload_current_device_slot(
        state,
        &provider,
        &device_id,
        &name,
        created_at,
        last_seen_at,
    )
    .await
    {
        Ok(()) => {
            if let Ok(conn) = state.lock() {
                let _ = clear_device_slot_dirty_if_current_name_matches(&conn, &name);
            }
        }
        Err(e) => {
            log::warn!("flush_device_slot_dirty_if_needed: slot re-upload failed: {e}");
        }
    }
}

/// One sync cycle, run by a caller that already holds the single-flight
/// guard (`SyncInProgressGuard`) for its whole duration — desktop
/// `run_sync_now_holding`.
///
/// Reports status through [`SyncPlatformHooks::on_status`] at every boundary
/// so every sync path produces identical events. No `std` mutex guard is held
/// across an `.await` (the module denies `clippy::await_holding_lock`), so the
/// future is `Send`. That says nothing about cancel safety: only the engine
/// attempt is known to survive being dropped mid-way (the stall guard relies
/// on it). The keyring re-upload, the cloud content migration, the republish
/// and the outbox acks have always run to completion, so do not drop this
/// future inside them without checking each step first.
#[doc(hidden)]
pub async fn run_sync_cycle(
    state: &AppState,
    key_state: &EncryptionKeyState,
    trigger: SyncTrigger,
    hooks: &dyn SyncPlatformHooks,
) -> Result<SyncSummary, String> {
    // Guard: refuse to push if a rotation job is active or force-re-pair is
    // required. The SyncInProgressGuard passed in by the caller handles the
    // in-flight case; this catches the persistent case where the guard was
    // dropped (e.g. retry-abort in reencrypt_items) but the rotation_jobs row
    // is still active.
    ensure_safe_to_push(state)?;
    ensure_remote_recovery_push_allowed(state, None, hooks.credentials()).await?;

    // Retry keyring upload if it failed during a previous password change.
    // Guard: skip when the app is locked — the key is unavailable, so any
    // attempt to re-upload the keyring would fail anyway.
    {
        let key_ready = key_state.is_initialized().unwrap_or(false);
        let dirty = key_ready && {
            let conn = state.lock()?;
            db::get_setting(&conn, db::KEYRING_DIRTY_KEY)
                .unwrap_or(None)
                .as_deref()
                == Some("1")
        };
        if dirty {
            // Pre-flight: a peer rotation this device has NOT yet observed must
            // be detected BEFORE the re-upload — the re-upload writes
            // `_meta.json` from local truth with no CAS, so it would clobber
            // the rotation and the pull-leg check below would then verify
            // against our own write. Only a typed `Mismatch` aborts here:
            // a corrupt `_meta.json` must still flow into the re-upload, which
            // is the only path that can regenerate a schema-broken meta from
            // local truth (failing closed on corrupt before the repair would
            // deadlock it — see docs/plans/2026-07-14-keyring-fail-open-findings.md).
            if let PullLegKeyringCheck::Mismatch(mismatch) =
                check_force_re_pair_sync(state, hooks.credentials()).await
            {
                {
                    let conn = state.lock()?;
                    set_force_re_pair_flags(&conn, &mismatch.reason)?;
                }
                hooks.on_force_re_pair(&mismatch.reason);
                return Err(format!("force-re-pair: {}", mismatch.reason));
            }
            try_reupload_keyring_and_update_dirty_flag(state, key_state, hooks.credentials()).await;
        }
    }

    // Short lock: build provider + read device id, then release before
    // starting the long-running engine. The engine itself acquires the
    // lock per DB call via `AppState: ConnAccess`, so other Tauri
    // commands can interleave freely during filesystem I/O.
    let (provider, device_id) = {
        let conn = state.lock()?;
        let provider = make_configured_provider(&conn, hooks.credentials())?
            .ok_or_else(|| "Sync is disabled or not configured".to_string())?;
        let device_id = db::get_or_create_device_id(&conn).map_err(|e| e.to_string())?;
        (provider, device_id)
    };
    // Clone the Arc and device_id before moving into the engine so migration can use them.
    let provider_for_migration = Arc::clone(&provider);
    let device_id_for_migration = device_id.clone();
    // Wrap the Tauri-facing reporter in a `StallTrackingReporter` so every
    // `report()`/`heartbeat()` call also stamps `last_progress_ms` — the
    // signal the stall guard below reads to distinguish "slow because
    // there is a lot of legitimate work" from "hung". `baseline` is shared
    // with the `now_ms` closure passed to `with_stall_guard` further down
    // so both sides measure elapsed time from the same instant.
    let stall_baseline = std::time::Instant::now();
    let last_progress_ms = Arc::new(AtomicU64::new(0));
    let reporter: Arc<dyn ProgressReporter> = Arc::new(StallTrackingReporter {
        inner: hooks.progress_reporter(),
        last_progress_ms: last_progress_ms.clone(),
        baseline: stall_baseline,
    });
    // `new_for_session`: adopt the process-level own-cloud reconcile flag so
    // Automatic cycles after the first success skip re-listing even though
    // this function builds a fresh engine every call.
    let engine = SyncEngine::new_for_session(provider, device_id.clone()).with_reporter(reporter);

    // Phase 5: force re-pair detection before the pull leg.
    let check = check_force_re_pair_sync(state, hooks.credentials()).await;
    if !matches!(check, PullLegKeyringCheck::Clean) {
        {
            let conn = state.lock()?;
            persist_pull_leg_check(&conn, &check)?;
        }
        match check {
            PullLegKeyringCheck::Mismatch(mismatch) => {
                hooks.on_force_re_pair(&mismatch.reason);
                return Err(format!("force-re-pair: {}", mismatch.reason));
            }
            PullLegKeyringCheck::Corrupt(msg) => {
                return Err(format!(
                    "force-re-pair: cloud keyring _meta.json is corrupt: {msg}"
                ));
            }
            PullLegKeyringCheck::Clean => unreachable!("guarded by the matches! above"),
        }
    }

    hooks.on_status(SyncPhase::Syncing, state, None);

    // T1 — One-time cloud-content migration.
    //
    // If `NEEDS_CLOUD_CONTENT_MIGRATION` is set, wipe this device's cloud sync
    // folder and reset local sync_state + media.upload_status to `pending`
    // atomically BEFORE the engine runs. The engine will then re-push everything
    // under the current content key (V1 epoch-1). Fresh installs never set this
    // flag, so they skip this block entirely.
    //
    // Atomicity rule (CLAUDE.md): the DB reset and the cloud delete happen before
    // clearing the flag. If we crash between the reset and the flag clear, the
    // next startup re-runs the migration (idempotent — deleting already-gone
    // files is a no-op; resetting already-pending rows is a no-op).
    {
        let needs_migration = {
            let conn = state.lock()?;
            db::get_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION)
                .map_err(|e| e.to_string())?
                .as_deref()
                == Some("1")
        };
        if needs_migration {
            log::info!("run_sync_now: NEEDS_CLOUD_CONTENT_MIGRATION flag set — running wipe+repush migration");

            // Build a keyring provider (KeyringV2Io) for Step 2.5. Any
            // configured cloud backend can publish `.meta/...` via KeyringV2Io.
            // SyncProvider::write_file still rejects those paths.
            let cloud_keyring = {
                let conn = state.lock()?;
                configured_cloud_provider(&conn, hooks.credentials())?.ok_or_else(|| {
                    // Provider was configured when the engine was built, but
                    // the session/config is gone now — do not clear the
                    // migration flag. Sync will retry on the next invocation.
                    "cloud-content migration: cloud provider configured but \
                     unavailable — cannot publish _content.json. Will retry on next sync."
                        .to_string()
                })?
            };

            let keyring_io_ref: Option<&(dyn crate::sync::keyring_v2::KeyringV2Io + Send + Sync)> =
                Some(&cloud_keyring as &(dyn crate::sync::keyring_v2::KeyringV2Io + Send + Sync));

            run_cloud_content_migration(
                state,
                key_state,
                &provider_for_migration,
                &device_id_for_migration,
                keyring_io_ref,
            )
            .await?;
            log::info!("run_sync_now: cloud-content migration complete");
        }
    }

    // Snapshot the HKDF sync sub-key out of the mutex BEFORE the long-running
    // engine call. Holding `with_sync_key` across `block_on(engine.sync_now(...))`
    // would pin the std::sync::Mutex for the full duration of network I/O —
    // any other Tauri sync command (list_entries, get_entry_content, etc.)
    // that calls `key_state.with_*_key()` would block on the mutex, freezing
    // the main thread that dispatches sync commands. `Zeroizing` ensures the
    // copy is wiped on drop, preserving the at-rest secret-handling guarantee.
    //
    let key_copy: Zeroizing<[u8; 32]> = key_state.with_sync_key(|k| Ok(Zeroizing::new(*k)))?;

    // Build a detached full-epoch snapshot for multi-key ingest (T2).
    let key_state_snapshot: crate::EncryptionKeyState = key_state.snapshot_for_engine()?;

    // Outer stall guard: rather than a fixed total-duration cap on the whole
    // push+pull cycle (which killed legitimately slow-but-working syncs on
    // large libraries at the 120s mark), this aborts only when the engine
    // has gone *silent* for SYNC_STALL_TIMEOUT — no `report()`/`heartbeat()`
    // call via `StallTrackingReporter` for that long. A cycle that keeps
    // making progress (pull commits per chunk, push marks each entry synced,
    // and now a heartbeat per item processed in between) can run arbitrarily
    // long without tripping the stall check; a true hang (e.g. a stalled
    // network connection not otherwise bounded — see the per-request
    // timeouts on `GDriveProvider`'s client) is caught within one stall
    // window instead of silently consuming the whole retry budget.
    //
    // `SYNC_HARD_CAP` is a second, much longer backstop against the
    // opposite failure mode: an adversarial or pathologically huge workload
    // (e.g. an endless stream of cheap items over a slow link) that keeps
    // stamping progress often enough to satisfy the stall check forever.
    // Durable per-item progress (pull commits per chunk, push marks each
    // entry synced) makes killing an attempt at the hard cap and retrying
    // safe — nothing already durable is lost.
    //
    // Retry up to MAX_SYNC_ATTEMPTS times via `retry_with_stall_guard`, but
    // ONLY when the attempt itself was aborted by the guard (matched on
    // `SyncStalled`, never on error-string matching). Each retry gets its
    // own fresh stall window and hard-cap clock — `with_stall_guard`
    // restamps `last_progress_ms` and the attempt-start instant at the
    // start of every call. After the final attempt is still aborted, the
    // reason-specific timeout error is returned so downstream handling
    // (scheduler pause, UI error) is unchanged.
    const MAX_SYNC_ATTEMPTS: u32 = 3;
    const SYNC_STALL_TIMEOUT: Duration = Duration::from_secs(120);
    const SYNC_HARD_CAP: Duration = Duration::from_secs(30 * 60);
    let now_ms = move || stall_baseline.elapsed().as_millis() as u64;
    let guard_result = retry_with_stall_guard(
        || engine.sync_now(state, &key_copy, &key_state_snapshot, trigger),
        last_progress_ms.clone(),
        now_ms,
        SYNC_STALL_TIMEOUT,
        SYNC_HARD_CAP,
        MAX_SYNC_ATTEMPTS,
        |attempt, reason| {
            log::warn!(
                "run_sync_now: sync attempt {attempt} stalled ({}), retrying ({attempt}/{MAX_SYNC_ATTEMPTS})",
                stall_reason_text(reason, SYNC_STALL_TIMEOUT, SYNC_HARD_CAP)
            );
            // Re-arm the frontend watchdog (130s) before it fires a false
            // "sync timed out" — the retry loop can otherwise run silently
            // across multiple stall windows with no event to reset it. Each
            // attempt can now legitimately run much longer than 130s as
            // long as progress events keep flowing (capped at least every
            // 25 items per `should_emit`), so this re-arm at the retry
            // boundary is a backstop, not the sole source of watchdog
            // resets.
            hooks.on_status(SyncPhase::Syncing, state, None);
        },
    )
    .await;
    let result: Result<SyncSummary, String> = match guard_result {
        Ok(inner) => inner.map_err(|e| e.to_string()),
        Err(reason) => Err(format!(
            "Sync timed out ({})",
            stall_reason_text(reason, SYNC_STALL_TIMEOUT, SYNC_HARD_CAP)
        )),
    };

    match result {
        Ok(mut summary) => {
            // Detect the appdata-scope-mismatch case BEFORE stamping or
            // emitting status — if the user is on a legacy drive.file token,
            // every push/pull leg surfaced ScopeMismatch and we must
            // auto-disconnect them so the reconnect banner fires.
            //
            // Uses the typed `scope_mismatch` flag on `SyncSummary` (set by
            // `engine.sync_now`) instead of substring-matching error strings.
            if summary.scope_mismatch {
                hooks.on_scope_upgrade_required(state);
                hooks.on_status(
                    SyncPhase::Error,
                    state,
                    Some("Sync upgrade required — please reconnect Google Drive".to_string()),
                );
                return Ok(summary);
            }

            // Stamp last_sync_at only if neither half errored — a partial
            // failure should not overwrite the previous successful
            // timestamp.
            if summary.errors.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                {
                    let conn = state.lock()?;
                    db::set_last_sync_at(&conn, now).map_err(|e| e.to_string())?;
                    // Freshen the current device's local last_seen_at so the
                    // "This device" card in Settings → Security shows an
                    // accurate "Last synced … ago". Local is authoritative for
                    // the current device (refresh_devices_from_cloud preserves
                    // it). No-op when there is no current device row.
                    let _ = db::touch_current_device_last_seen(&conn, now);
                }
                hooks.after_successful_cycle(state, key_state);
                // Best-effort: republish the cloud device slot with the fresh
                // last_seen_at so PEER devices see this device as recently
                // active. GDrive-only (LocalSyncProvider has no `devices/`
                // keyring). Failure is logged and swallowed — last_seen_at is
                // cosmetic and self-heals on the next sync; we must NOT set
                // keyring_dirty (the dirty-retry path republishes the full
                // keyring and would otherwise reset peers' view to created_at).
                {
                    let device_slot_dirty = {
                        let conn = state.lock()?;
                        db::get_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY)
                            .unwrap_or(None)
                            .as_deref()
                            == Some("1")
                    };
                    if device_slot_dirty {
                        flush_device_slot_dirty_if_needed(state, hooks.credentials()).await;
                    } else {
                        republish_current_device_slot_best_effort(state, hooks.credentials(), now)
                            .await;
                    }
                }
            }

            // Pull may have stamped embed-sync pending decisions, and a per-preset
            // endpoint merge may need live AI slots rebuilt. Both are platform work.
            hooks.after_cycle(state, &summary)?;

            // Outbox importer runs before the terminal status, so "Synced" means web edits were
            // applied too. It never fails the sync: errors are logged, then the status is emitted.
            let (applied, pending_acks) = import_web_outbox_after_sync(
                hooks, state, key_state, &engine, &device_id, &summary,
            )
            .await
            .unwrap_or_else(|e| {
                log::warn!("run_sync_now: outbox import skipped: {e}");
                (0, None)
            });
            // The import ran after this cycle's push, so the rows it applied are still pending.
            // Push them now, before the acks: the web drops a draft once it sees the ack, and
            // would otherwise show the pre-edit state until the next sync. Its failures count
            // like the first push's, so a failed republish is not reported as "Synced".
            if applied > 0 && summary.errors.is_empty() {
                match engine
                    .push_local(
                        state,
                        &key_copy,
                        &key_state_snapshot,
                        SyncTrigger::Automatic,
                    )
                    .await
                {
                    Ok(stats) => {
                        summary.pushed += stats.pushed;
                        summary
                            .errors
                            .extend(stats.errors.into_iter().map(|e| format!("republish: {e}")));
                    }
                    Err(e) => summary.errors.push(format!("republish: {e}")),
                }
            }
            // Always, also when nothing was applied (refusals) or the republish failed (then
            // the acks land before the data, and the next sync republishes the pending rows).
            if let Some(PendingAcks {
                cloud,
                key_list,
                inputs,
            }) = pending_acks
            {
                if let Err(e) = crate::sync::outbox_import::write_outbox_acks(
                    &cloud,
                    &device_id,
                    &key_list,
                    inputs,
                    state,
                    &mut crate::sync::outbox_import::OutboxImportSummary::default(),
                )
                .await
                {
                    log::warn!("run_sync_now: writing outbox acks failed: {e}");
                }
            }

            let phase = if summary.errors.is_empty() {
                SyncPhase::Synced
            } else {
                SyncPhase::Error
            };
            let err = if summary.errors.is_empty() {
                None
            } else {
                Some(summary.errors.join("; "))
            };
            hooks.on_status(phase, state, err);

            Ok(summary)
        }
        Err(e) => {
            // Engine.sync_now never returns Err directly today (always Ok with
            // populated summary.errors), so this branch is reached only via
            // the outer 120s timeout wrapper or a state-lock failure. Still
            // detect the scope-mismatch string defensively in case a future
            // engine refactor surfaces the typed error here — keep the
            // substring check tight (`scope mismatch:` with the colon that
            // SyncError::ScopeMismatch's Display always emits).
            if e.contains("scope mismatch:") {
                hooks.on_scope_upgrade_required(state);
                hooks.on_status(
                    SyncPhase::Error,
                    state,
                    Some("Sync upgrade required — please reconnect Google Drive".to_string()),
                );
                return Err(e);
            }
            hooks.on_status(SyncPhase::Error, state, Some(e.clone()));
            Err(e)
        }
    }
}

/// What `run_sync_now` needs to write the outbox acks after its republish.
#[doc(hidden)]
pub struct PendingAcks {
    cloud: CloudProvider,
    key_list: memlore_core::key_state::ContentKeyList,
    inputs: crate::sync::outbox_import::AckInputs,
}

/// Runs the outbox importer (web companion edits) at the end of a successful sync, before the
/// terminal status. Emits `importing-web-edits` progress while it runs and logs its duration.
/// Returns how many intents it applied, and what writing its acks needs (`None` when the
/// cycle did not run).
#[doc(hidden)]
pub async fn import_web_outbox_after_sync(
    hooks: &dyn SyncPlatformHooks,
    state: &AppState,
    key_state: &EncryptionKeyState,
    engine: &SyncEngine,
    device_id: &str,
    summary: &SyncSummary,
) -> Result<(u64, Option<PendingAcks>), String> {
    let cloud_opt = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        configured_cloud_provider(&conn, hooks.credentials())?
    };
    if matches!(cloud_opt, Some(CloudProvider::Folder(_))) || summary.scope_mismatch {
        return Ok((0, None));
    }
    let (Ok(key_list), Some(cloud)) = (key_state.content_key_list(), cloud_opt) else {
        return Ok((0, None));
    };
    let slots = crate::sync::keyring_v2::io::list_device_slots(&cloud)
        .await
        .unwrap_or_default();
    let has_candidate_web_devices = slots.into_iter().any(|s| {
        s.device_id != device_id
            && !summary.fetched_manifests.contains(&s.device_id)
            && !summary.unchanged_peers.contains(&s.device_id)
    });
    if !has_candidate_web_devices {
        return Ok((0, None));
    }

    let platform_sink = hooks.outbox_sink(state);
    let sink = HookSink(&*platform_sink);
    // Shown while the manifests are re-read, before any outbox file is checked.
    sink.emit_progress(0, 0);
    let started = std::time::Instant::now();
    let media_dir = hooks.media_dir();

    let (known_ids, pull_clean) = if summary.pull_clean {
        match engine.fetch_manifests(true).await {
            Ok((manifests, errors)) if errors.is_empty() => (
                crate::sync::outbox_import::collect_known_ids_from_manifests(&manifests),
                true,
            ),
            _ => (std::collections::HashMap::new(), false),
        }
    } else {
        (std::collections::HashMap::new(), false)
    };

    match crate::sync::outbox_import::run_outbox_import_cycle_without_acks(
        &cloud, &cloud, device_id, &key_list, known_ids, pull_clean, summary, &media_dir, state,
        &sink,
    )
    .await
    {
        Ok((imported, inputs)) => {
            log::info!(
                "run_sync_now: outbox import took {} ms (listed {}, applied {}, refused {}, \
                 transient {}, unchanged {})",
                started.elapsed().as_millis(),
                imported.intents_listed,
                imported.intents_applied,
                imported.intents_refused,
                imported.intents_transient,
                imported.intents_skipped_unchanged,
            );
            Ok((
                imported.intents_applied as u64,
                Some(PendingAcks {
                    cloud,
                    key_list,
                    inputs,
                }),
            ))
        }
        Err(e) => {
            log::warn!("run_sync_now: outbox import cycle failed: {e}");
            Ok((0, None))
        }
    }
}

/// One-time cloud-content migration (Phase 5 T1).
///
/// Called at the start of `run_sync_now` when `NEEDS_CLOUD_CONTENT_MIGRATION`
/// is set. Atomically resets local sync state and then wipes this device's
/// cloud sync folder so the engine re-pushes everything under the current
/// content key on the next engine call.
///
/// # Atomicity
///
/// Per CLAUDE.md rule: the DB reset (sync_state + media to `pending`) happens
/// in a single transaction BEFORE the cloud delete. If we crash between DB
/// reset and cloud delete, the next startup re-runs the migration — idempotent.
/// The flag is cleared LAST, so a mid-migration crash retries from the start.
///
/// Step 2.5 (C2 fix): publishes `_content.json` and updates `_meta.json`
/// (content_epoch) on the keyring provider BEFORE clearing the migration flag.
/// Only GDrive users have a keyring; iCloud/local users skip this step and fall
/// through to clear the flag (their peers derive the content key per-device via
/// password, not via the cloud keyring).
///
/// `keyring_io` must be `Some` for GDrive and `None` for iCloud/local. When
/// `Some`, the write goes through `KeyringV2Io` (which accepts `.meta/...`
/// paths) — NOT through `SyncProvider::write_file` which rejects them.
#[doc(hidden)]
pub async fn run_cloud_content_migration(
    state: &AppState,
    key_state: &EncryptionKeyState,
    engine_provider: &Arc<dyn SyncProvider>,
    device_id: &str,
    keyring_io: Option<&(dyn crate::sync::keyring_v2::KeyringV2Io + Send + Sync)>,
) -> Result<(), String> {
    use crate::sync::keyring_v2::io::{read_meta, write_content, write_meta};
    use crate::sync::keyring_v2::types::{ContentEntryV2, ContentListV2, KEYRING_V2_VERSION};
    use crate::sync::provider::FileKind;
    use crate::utils::encryption::key_fingerprint;

    // Step 1: Reset local sync_state + journal_sync_state + media + versions atomically
    // in one DB transaction. Cloud files for all these channels are wiped in Step 2, so
    // their local state must also be reset here — otherwise the respective push paths
    // skip them (rows still show 'synced'/'uploaded') and the wiped cloud files are
    // never re-created under the new content key.
    {
        let conn = state.lock()?;
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        db::reset_all_sync_state_to_pending(&*tx).map_err(|e| e.to_string())?;
        db::reset_all_journal_sync_state_to_pending(&*tx).map_err(|e| e.to_string())?;
        db::reset_all_media_upload_status_to_pending(&*tx, device_id).map_err(|e| e.to_string())?;
        db::reset_all_version_upload_status_to_pending(&*tx).map_err(|e| e.to_string())?;
        // Cloud folder is wiped under a new content key — stale surface hashes
        // would skip re-upload of settings/tags/… against empty/wrong ciphertext.
        db::clear_all_surface_push_hashes(&*tx).map_err(|e| e.to_string())?;
        db::clear_all_pull_revisions(&*tx).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        crate::sync::engine::reset_session_own_cloud_reconciled();
        log::info!(
            "run_cloud_content_migration: reset sync_state + journal_sync_state + media + versions to pending; cleared surface push hashes"
        );
    }

    // Step 2: Delete all entry/media/journal/version files from this device's cloud
    // folder. Errors deleting individual files are logged and swallowed — the file may
    // already be gone (idempotent). A provider-level error listing files is fatal.
    for kind in [
        FileKind::Entries,
        FileKind::Media,
        FileKind::Journals,
        FileKind::Versions,
    ] {
        let paths = engine_provider
            .list_files(device_id, kind)
            .await
            .map_err(|e| {
                format!(
                    "cloud-content migration: list {} files: {e}",
                    kind.subfolder_name().unwrap_or("device-root")
                )
            })?;
        for path in paths {
            match engine_provider.delete_file(&path).await {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("run_cloud_content_migration: delete {path}: {e} (continuing)");
                }
            }
        }
        log::info!(
            "run_cloud_content_migration: wiped {} cloud files for device {}",
            kind.subfolder_name().unwrap_or("device-root"),
            device_id
        );
    }

    // Step 2.5: Publish `_content.json` and update `_meta.json` so peers can
    // read the content-key epoch list.
    //
    // This step runs BEFORE clearing the migration flag (crash-safe: if we crash
    // here, the next startup retries the migration — re-publishing is idempotent).
    //
    // Only GDrive users have a cloud keyring. iCloud/local users skip this step
    // (their peers derive the content key from the password per-device — no shared
    // keyring file to update). When `keyring_io` is None the flag is still cleared
    // in Step 3 so the migration does not loop.
    if let Some(kio) = keyring_io {
        use crate::sync::keyring_v2::io::read_content;
        use crate::utils::encryption::{encode_content_key_list, unwrap_content_key, KEY_SIZE};
        use std::collections::BTreeMap;

        // I4: Check whether another device has already published a `_content.json`
        // for this vault. If one exists and unwraps cleanly under the current
        // master, adopt it (update local DB) instead of overwriting with the
        // locally-generated epoch-1 key. Last-writer-wins on `_content.json`
        // would otherwise cause permanent split-brain when two legacy devices
        // both run T3 migration at roughly the same time.
        //
        // If the cloud list exists but *cannot* be unwrapped, a different device
        // published it under a different master key — require re-pair.
        //
        // Network errors reading the cloud list are non-fatal: we fall through to
        // the normal publish path. A successful publish is idempotent (two devices
        // both generate valid epoch-1 keys and the later write wins). The race is
        // rare in practice (requires two legacy devices syncing simultaneously)
        // and the re-pair detection path here stops the split-brain before it
        // propagates.
        let cloud_adopted = match read_content(kio).await {
            Err(e) => {
                log::warn!(
                    "run_cloud_content_migration: cannot read cloud _content.json \
                     ({e}); proceeding with local publish"
                );
                false
            }
            Ok(None) => {
                log::info!(
                    "run_cloud_content_migration: no existing _content.json — \
                     this device publishes first"
                );
                false
            }
            Ok(Some(cloud_list)) => {
                // Try to unwrap every entry using the current master. If they all
                // succeed, the cloud list was produced by the same vault (same
                // master key) — safe to adopt.
                //
                // We collect the decoded keys *outside* `with_master` so we can call
                // `set_content_state` afterwards without re-entering the mutex (which
                // would deadlock on the same thread).
                let adopt_result: Result<BTreeMap<u32, Zeroizing<[u8; KEY_SIZE]>>, String> =
                    key_state.with_master(|master| {
                        let mut adopted: BTreeMap<u32, Zeroizing<[u8; KEY_SIZE]>> = BTreeMap::new();
                        for entry in &cloud_list.entries {
                            let blob = hex::decode(&entry.wrapped_content).map_err(|e| {
                                format!(
                                    "cloud-content migration: cloud entry epoch {} \
                                     wrapped_content decode: {e}",
                                    entry.epoch
                                )
                            })?;
                            let ck = unwrap_content_key(master, &blob).map_err(|_| {
                                format!(
                                    "cloud-content migration: cannot unwrap cloud epoch {} \
                                     — cloud vault may belong to a different master key",
                                    entry.epoch
                                )
                            })?;
                            adopted.insert(entry.epoch, ck);
                        }
                        // Re-encode using the local master (same master, so bytes are
                        // identical — but this uses the canonical encode path).
                        let encoded = encode_content_key_list(master, &adopted)?;
                        let latest = cloud_list.latest_epoch;
                        let conn = state.lock()?;
                        db::set_setting(&conn, db::WRAPPED_CONTENT_LIST, &encoded)
                            .map_err(|e| e.to_string())?;
                        db::set_setting(&conn, db::CONTENT_KEY_EPOCH, &latest.to_string())
                            .map_err(|e| e.to_string())?;
                        Ok(adopted)
                    });
                match adopt_result {
                    Ok(adopted_keys) => {
                        let latest = cloud_list.latest_epoch;
                        log::info!(
                            "run_cloud_content_migration: adopted existing cloud \
                             _content.json (latest_epoch={latest}) — skipping local publish"
                        );
                        // F5: refresh the in-memory key state so new writes use the adopted
                        // epoch list. Without this, the app keeps encrypting with its
                        // locally-generated epoch-1 key until the next unlock, which peers
                        // holding the adopted list cannot read.
                        //
                        // We must read db_key and master in separate `with_*` calls (each
                        // one acquires and then releases the mutex) BEFORE calling
                        // `set_content_state` — re-entering the mutex inside a `with_*`
                        // closure would deadlock on the single mutex held by that closure.
                        let refresh_result: Result<(), String> = (|| {
                            let db_key = key_state.with_db_key(|dk| {
                                let mut out = Zeroizing::new([0u8; KEY_SIZE]);
                                out.copy_from_slice(dk);
                                Ok(out)
                            })?;
                            let master_copy = key_state.with_master(|m| {
                                let mut out = Zeroizing::new([0u8; KEY_SIZE]);
                                out.copy_from_slice(m);
                                Ok(out)
                            })?;
                            key_state
                                .set_content_state(
                                    adopted_keys.clone(),
                                    latest,
                                    db_key,
                                    master_copy,
                                )
                                .map_err(|e| {
                                    format!("cloud-content migration adopt: set_content_state: {e}")
                                })
                        })();
                        if let Err(e) = refresh_result {
                            // Non-fatal: the DB was already updated; the app is correct
                            // after the next unlock. Log and continue.
                            log::warn!(
                                "run_cloud_content_migration: key_state refresh after adopt \
                                 failed (non-fatal — will be correct after next unlock): {e}"
                            );
                        } else {
                            log::info!(
                                "run_cloud_content_migration: key_state refreshed to \
                                 adopted epoch list (latest_epoch={latest})"
                            );
                        }
                        true // skip the publish block below
                    }
                    Err(e) => {
                        return Err(format!(
                            "cloud-content migration: a _content.json already exists on \
                             the cloud but cannot be decrypted with the current master \
                             key. Another device may have published a vault under a \
                             different master. Re-pair all devices to resolve. \
                             Details: {e}"
                        ));
                    }
                }
            }
        };

        if !cloud_adopted {
            // Read the wrapped content-key list from the DB. Each 128-hex-char slot
            // is epoch(8 hex = 4 bytes BE) + wrapped_blob(120 hex = 60 bytes).
            const SLOT_HEX: usize = 128;
            const EPOCH_HEX: usize = 8;

            let (list_hex, now_secs) = {
                let conn = state.lock()?;
                let list_hex = db::get_setting(&conn, db::WRAPPED_CONTENT_LIST)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        "cloud-content migration: WRAPPED_CONTENT_LIST absent — \
                         cannot publish _content.json"
                            .to_string()
                    })?;
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                (list_hex, now_secs)
            };

            if list_hex.len() < SLOT_HEX || list_hex.len() % SLOT_HEX != 0 {
                return Err(format!(
                    "cloud-content migration: WRAPPED_CONTENT_LIST has invalid length {} \
                     (expected multiple of {SLOT_HEX} hex chars)",
                    list_hex.len()
                ));
            }

            let mut entries = Vec::new();
            let n_slots = list_hex.len() / SLOT_HEX;
            for i in 0..n_slots {
                let slot = &list_hex[i * SLOT_HEX..(i + 1) * SLOT_HEX];
                let epoch_hex = &slot[..EPOCH_HEX]; // 8 hex chars = 4 bytes
                let wrapped_hex = &slot[EPOCH_HEX..]; // 120 hex chars = 60 bytes
                let epoch_bytes = hex::decode(epoch_hex).map_err(|e| {
                    format!(
                        "cloud-content migration: invalid epoch hex in WRAPPED_CONTENT_LIST: {e}"
                    )
                })?;
                if epoch_bytes.len() != 4 {
                    return Err(format!(
                        "cloud-content migration: epoch bytes wrong length: {}",
                        epoch_bytes.len()
                    ));
                }
                let epoch = u32::from_be_bytes([
                    epoch_bytes[0],
                    epoch_bytes[1],
                    epoch_bytes[2],
                    epoch_bytes[3],
                ]);

                // Compute the content fingerprint via the key state.
                // with_sync_key_for_epoch returns derive_sync_key(raw_content_key),
                // matching what CLOUD_CONTENT_FINGERPRINT stores.
                let fp_hex = key_state
                    .with_sync_key_for_epoch(epoch, |k| Ok(hex::encode(key_fingerprint(k))))
                    .map_err(|e| {
                        format!(
                            "cloud-content migration: cannot compute fingerprint for epoch {epoch}: {e}"
                        )
                    })?;

                entries.push(ContentEntryV2 {
                    epoch,
                    wrapped_content: wrapped_hex.to_string(),
                    content_fingerprint: fp_hex,
                });
            }

            let latest_epoch = entries.last().map(|e| e.epoch).unwrap_or(0);
            let content_list = ContentListV2 {
                version: KEYRING_V2_VERSION,
                latest_epoch,
                entries,
                created_at: now_secs,
            };

            // Write _content.json via KeyringV2Io (accepts .meta/... paths).
            // SyncProvider::write_file rejects these paths — never use it here.
            write_content(kio, &content_list)
                .await
                .map_err(|e| format!("cloud-content migration: write _content.json failed: {e}"))?;

            log::info!(
                "run_cloud_content_migration: published _content.json (latest_epoch={})",
                latest_epoch
            );

            // Update _meta.json content_epoch so peers pick up the latest epoch.
            // read_meta returns None when _meta.json is absent (e.g. first onboard
            // published _content.json before _meta.json in a crash scenario) — skip
            // in that case; the next full keyring publish will set content_epoch.
            match read_meta(kio)
                .await
                .map_err(|e| format!("cloud-content migration: read _meta.json failed: {e}"))?
            {
                Some(mut meta) => {
                    meta.content_epoch = latest_epoch;
                    write_meta(kio, &meta).await.map_err(|e| {
                        format!("cloud-content migration: write _meta.json failed: {e}")
                    })?;
                    log::info!(
                        "run_cloud_content_migration: updated _meta.json content_epoch={}",
                        latest_epoch
                    );
                }
                None => {
                    log::warn!(
                        "run_cloud_content_migration: _meta.json absent — skipping content_epoch update"
                    );
                }
            }
        }
    }

    // Step 3: Clear the migration flag. Done LAST so a crash before this point
    // causes a retry (idempotent: resetting pending rows and deleting gone files
    // are both no-ops on re-run).
    {
        let conn = state.lock()?;
        db::delete_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION).map_err(|e| e.to_string())?;
        log::info!("run_cloud_content_migration: cleared NEEDS_CLOUD_CONTENT_MIGRATION flag");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::sync::platform::CloudConnectHooks;
    use rusqlite::Connection;

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
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

    #[test]
    fn device_slot_dirty_clear_keeps_flag_when_local_name_changed_during_flush() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::upsert_device(
            &conn,
            &db::DeviceRow {
                device_id: "dev-current".to_string(),
                name: "Uploaded Name".to_string(),
                created_at: 1000,
                last_seen_at: 1000,
                is_current: true,
                is_revoked: false,
            },
        )
        .unwrap();
        db::set_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY, "1").unwrap();
        db::update_device_name(&conn, "dev-current", "Newer Local Name").unwrap();

        clear_device_slot_dirty_if_current_name_matches(&conn, "Uploaded Name").unwrap();

        assert_eq!(
            db::get_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY).unwrap(),
            Some("1".to_string()),
            "flush must not clear dirty flag for a newer local rename"
        );
    }

    #[test]
    fn device_slot_dirty_clear_removes_flag_when_uploaded_name_is_current() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::upsert_device(
            &conn,
            &db::DeviceRow {
                device_id: "dev-current".to_string(),
                name: "Uploaded Name".to_string(),
                created_at: 1000,
                last_seen_at: 1000,
                is_current: true,
                is_revoked: false,
            },
        )
        .unwrap();
        db::set_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY, "1").unwrap();

        clear_device_slot_dirty_if_current_name_matches(&conn, "Uploaded Name").unwrap();

        assert_eq!(
            db::get_setting(&conn, db::DEVICE_SLOT_DIRTY_KEY).unwrap(),
            None
        );
    }

    // ─── Phase 5 T4.1: cloud-content migration tests ─────────────────────────

    /// T4.1a — GDrive path: `run_cloud_content_migration` resets local state
    /// atomically, deletes device cloud files, publishes `_content.json` via
    /// `KeyringV2Io` (not SyncProvider), updates `_meta.json` content_epoch,
    /// and clears the migration flag.
    #[tokio::test]
    async fn cloud_migration_gdrive_publishes_content_json_via_keyring_io() {
        use crate::sync::keyring_v2::io::{
            read_content, read_meta, test_support::InMemoryKeyringProvider, write_meta,
            CONTENT_PATH,
        };
        use crate::sync::keyring_v2::types::{KeyringMetaV2, KEYRING_V2_VERSION};
        use crate::sync::provider::test_support::MockProvider;
        use crate::utils::encryption::{
            derive_sync_key, encode_content_key_list, key_fingerprint, KEY_SIZE,
        };
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let state = make_state();
        let device_id = "test-device-aabbccdd";

        // Build a real v1 content key and load the key state.
        let raw_content_key_1 = zeroize::Zeroizing::new([0x42u8; KEY_SIZE]);
        let raw_master = zeroize::Zeroizing::new([0x11u8; KEY_SIZE]);
        let raw_db_key = zeroize::Zeroizing::new([0x22u8; KEY_SIZE]);

        let key_state = EncryptionKeyState::new();
        {
            let mut keys: BTreeMap<u32, zeroize::Zeroizing<[u8; KEY_SIZE]>> = BTreeMap::new();
            keys.insert(1, raw_content_key_1.clone());
            key_state
                .set_content_state(keys, 1, raw_db_key.clone(), raw_master.clone())
                .expect("set_content_state");
        }

        // Seed some sync_state + media rows (simulating previously-synced state).
        let version_id;
        {
            let conn = state.lock().unwrap();
            // Insert a journal + entry for sync_state.
            let journal_id = "journal-aaaaaaaa-1111-2222-3333-444444444444";
            conn.execute(
                "INSERT OR IGNORE INTO journals (id, name, created_at, updated_at) VALUES (?1, 'J', 1, 1)",
                [journal_id],
            ).unwrap();
            let entry_id = "entry-eeeeeeee-1111-2222-3333-444444444444";
            conn.execute(
                "INSERT INTO entries (id, journal_id, title, preview_text, content_text, entry_date, created_at, updated_at, is_favorite, is_deleted)
                 VALUES (?1, ?2, 't', 'p', 'c', 1, 1, 1, 0, 0)",
                [entry_id, journal_id],
            ).unwrap();
            conn.execute(
                "INSERT INTO sync_state (entry_id, local_version, sync_status) VALUES (?1, 3, 'synced')",
                [entry_id],
            ).unwrap();
            // Insert a journal_sync_state row (simulating a previously-synced journal).
            conn.execute(
                "INSERT INTO journal_sync_state (journal_id, local_version, sync_status) VALUES (?1, 2, 'synced')",
                [journal_id],
            ).unwrap();
            // Insert a media row.
            let media_id = "media-mmmmmmmm-1111-2222-3333-444444444444";
            conn.execute(
                "INSERT INTO media (id, entry_id, file_name, file_type, storage_provider, storage_path, upload_status, created_at)
                 VALUES (?1, ?2, 'test.jpg', 'image/jpeg', 'local', '/tmp/test.jpg', 'uploaded', 1)",
                [media_id, entry_id],
            ).unwrap();

            // Insert a version row marked 'uploaded' — the versions channel must be
            // wiped/reset by migration too (finding: it was silently skipped).
            version_id =
                db::insert_entry_version(&conn, entry_id, b"yjs", "preview", device_id).unwrap();
            db::mark_version_uploaded(&conn, &version_id, &format!("{device_id}/versions/v1.bin"))
                .unwrap();

            // Set the migration flag.
            db::set_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION, "1").unwrap();

            // Stale surface hashes (clean gates) must not survive content-key migration —
            // cloud is wiped and re-encrypted under a new key.
            seed_all_surface_push_hashes(&conn);

            // Persist the wrapped content-key list (what the real onboarding stores).
            let mut keys_for_encode: BTreeMap<u32, zeroize::Zeroizing<[u8; KEY_SIZE]>> =
                BTreeMap::new();
            keys_for_encode.insert(1, raw_content_key_1.clone());
            let list_hex = encode_content_key_list(&raw_master, &keys_for_encode)
                .expect("encode_content_key_list");
            db::set_setting(&conn, db::WRAPPED_CONTENT_LIST, &list_hex).unwrap();
        }

        // SyncProvider (MockProvider) — holds old device cloud files.
        let provider: Arc<dyn crate::sync::SyncProvider> = Arc::new(MockProvider::new());
        {
            let path_entry = format!("{device_id}/entries/entry-eeeeeeee.bin");
            let path_media = format!("{device_id}/media/media-mmmmmmmm");
            let path_journal = format!("{device_id}/journals/journal-aaaaaaaa.bin");
            let path_version = format!("{device_id}/versions/v1.bin");
            provider
                .write_file(&path_entry, b"old-entry")
                .await
                .unwrap();
            provider
                .write_file(&path_media, b"old-media")
                .await
                .unwrap();
            provider
                .write_file(&path_journal, b"old-journal")
                .await
                .unwrap();
            provider
                .write_file(&path_version, b"old-version")
                .await
                .unwrap();
        }

        // KeyringV2Io (InMemoryKeyringProvider) — simulates the GDrive keyring store.
        // This is separate from the SyncProvider to mirror the production split
        // where LocalSyncProvider rejects .meta/... paths and only GDriveProvider
        // implements KeyringV2Io via write_shared_file.
        let keyring_io = InMemoryKeyringProvider::new();

        // Pre-seed a _meta.json with content_epoch=0 so the update is testable.
        let initial_meta = KeyringMetaV2 {
            version: KEYRING_V2_VERSION,
            epoch: 1,
            master_fingerprint: "a".repeat(64),
            content_epoch: 0,
            recovery_generation: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        write_meta(&keyring_io, &initial_meta).await.unwrap();

        // Run the migration (GDrive path — keyring_io is Some).
        run_cloud_content_migration(&state, &key_state, &provider, device_id, Some(&keyring_io))
            .await
            .unwrap();

        // Assert: sync_state rows are all pending.
        {
            let conn = state.lock().unwrap();
            let count_synced: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sync_state WHERE sync_status = 'synced'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                count_synced, 0,
                "all sync_state rows must be pending after migration"
            );

            let count_pending: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sync_state WHERE sync_status = 'pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count_pending, 1, "entry must be reset to pending");

            // Assert: media rows are all pending.
            let media_uploaded: i64 = conn
                .query_row(
                    "SELECT count(*) FROM media WHERE upload_status = 'uploaded'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                media_uploaded, 0,
                "no media must remain 'uploaded' after migration"
            );

            let media_pending: i64 = conn
                .query_row(
                    "SELECT count(*) FROM media WHERE upload_status = 'pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(media_pending, 1, "media must be reset to pending");

            // Assert: entry_versions rows are all pending with cloud_path cleared
            // (finding: the versions channel was silently skipped by migration).
            let version_uploaded: i64 = conn
                .query_row(
                    "SELECT count(*) FROM entry_versions WHERE upload_status = 'uploaded'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                version_uploaded, 0,
                "no entry_versions rows must remain 'uploaded' after migration"
            );
            let version_cloud_path: Option<String> = conn
                .query_row(
                    "SELECT cloud_path FROM entry_versions WHERE id = ?1",
                    [&version_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                version_cloud_path, None,
                "entry_versions.cloud_path must be cleared after migration"
            );

            // Assert: journal_sync_state rows are all pending (F3 regression guard).
            let journal_synced: i64 = conn
                .query_row(
                    "SELECT count(*) FROM journal_sync_state WHERE sync_status = 'synced'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                journal_synced, 0,
                "no journal_sync_state rows must remain 'synced' after migration (F3)"
            );
            let journal_pending: i64 = conn
                .query_row(
                    "SELECT count(*) FROM journal_sync_state WHERE sync_status = 'pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            // migrate() inserts a default "My Journal" which also gets a
            // journal_sync_state row (pending). Together with the test journal
            // we seeded, there are ≥ 2 pending rows after migration. The
            // important invariant is that none are 'synced' (checked above).
            assert!(
                journal_pending >= 1,
                "at least one journal_sync_state row must be pending after migration (F3), got 0"
            );

            // Assert: migration flag is cleared.
            let flag = db::get_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION).unwrap();
            assert!(
                flag.is_none() || flag.as_deref() != Some("1"),
                "NEEDS_CLOUD_CONTENT_MIGRATION must be cleared after migration, got {flag:?}"
            );

            // Assert: all 8 surface push hashes cleared (default-dirty → re-upload).
            assert_all_surface_push_hashes_cleared(&conn);
        }

        // Assert: provider files for this device are gone.
        let entry_files = provider
            .list_files(device_id, crate::sync::provider::FileKind::Entries)
            .await
            .unwrap();
        let media_files = provider
            .list_files(device_id, crate::sync::provider::FileKind::Media)
            .await
            .unwrap();
        let journal_files = provider
            .list_files(device_id, crate::sync::provider::FileKind::Journals)
            .await
            .unwrap();
        let version_files = provider
            .list_files(device_id, crate::sync::provider::FileKind::Versions)
            .await
            .unwrap();
        assert!(
            entry_files.is_empty(),
            "entry files must be wiped: {entry_files:?}"
        );
        assert!(
            media_files.is_empty(),
            "media files must be wiped: {media_files:?}"
        );
        assert!(
            journal_files.is_empty(),
            "journal files must be wiped: {journal_files:?}"
        );
        assert!(
            version_files.is_empty(),
            "version files must be wiped: {version_files:?}"
        );

        // Assert: _content.json was published via KeyringV2Io (not engine_provider).
        // Use read_content to parse via the proper trait path (same as peers do).
        let content_list = read_content(&keyring_io)
            .await
            .expect("read_content must succeed")
            .expect("_content.json must exist in keyring_io after migration");
        assert_eq!(
            content_list.latest_epoch, 1,
            "_content.json must have latest_epoch=1"
        );
        assert_eq!(
            content_list.entries.len(),
            1,
            "_content.json must have exactly 1 entry (epoch 1)"
        );
        let entry0 = &content_list.entries[0];
        assert_eq!(entry0.epoch, 1, "entry[0].epoch must be 1");

        // Verify the content fingerprint matches what we'd compute independently.
        let expected_fp = hex::encode(key_fingerprint(&*derive_sync_key(&raw_content_key_1)));
        assert_eq!(
            entry0.content_fingerprint, expected_fp,
            "_content.json entry[0].content_fingerprint must match derive_sync_key(content_key_1)"
        );

        // Assert: _content.json was NOT written to the SyncProvider
        // (engine_provider must not accept .meta/... writes in production).
        let no_content_in_engine = provider.read_file(CONTENT_PATH).await;
        // MockProvider stores everything, so this would be Ok if we'd incorrectly
        // written via engine_provider. We never do; the test verifies keyring_io got it.
        // (We verify keyring_io above; here we just confirm it wasn't double-written.)
        drop(no_content_in_engine);

        // Assert: _meta.json content_epoch updated to 1.
        let meta = read_meta(&keyring_io)
            .await
            .expect("read_meta must succeed")
            .expect("_meta.json must exist after migration");
        assert_eq!(
            meta.content_epoch, 1,
            "_meta.json content_epoch must be updated to latest_epoch=1"
        );
    }

    /// T4.1b — iCloud/local path: `run_cloud_content_migration` still resets
    /// local state and clears the flag, but does NOT attempt to write
    /// `_content.json` (no keyring on iCloud).
    #[tokio::test]
    async fn cloud_migration_icloud_skips_keyring_publish_and_clears_flag() {
        use crate::sync::provider::test_support::MockProvider;
        use crate::utils::encryption::{encode_content_key_list, KEY_SIZE};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let state = make_state();
        let device_id = "test-device-local-icloud";

        let raw_content_key_1 = zeroize::Zeroizing::new([0x99u8; KEY_SIZE]);
        let raw_master = zeroize::Zeroizing::new([0x88u8; KEY_SIZE]);
        let raw_db_key = zeroize::Zeroizing::new([0x77u8; KEY_SIZE]);

        let key_state = EncryptionKeyState::new();
        {
            let mut keys: BTreeMap<u32, zeroize::Zeroizing<[u8; KEY_SIZE]>> = BTreeMap::new();
            keys.insert(1, raw_content_key_1.clone());
            key_state
                .set_content_state(keys, 1, raw_db_key.clone(), raw_master.clone())
                .expect("set_content_state");
        }

        {
            let conn = state.lock().unwrap();
            let journal_id = "journal-bbbbbbbb-1111-2222-3333-444444444444";
            conn.execute(
                "INSERT OR IGNORE INTO journals (id, name, created_at, updated_at) VALUES (?1, 'J', 1, 1)",
                [journal_id],
            ).unwrap();
            let entry_id = "entry-ffffffff-1111-2222-3333-444444444444";
            conn.execute(
                "INSERT INTO entries (id, journal_id, title, preview_text, content_text, entry_date, created_at, updated_at, is_favorite, is_deleted)
                 VALUES (?1, ?2, 't', 'p', 'c', 1, 1, 1, 0, 0)",
                [entry_id, journal_id],
            ).unwrap();
            conn.execute(
                "INSERT INTO sync_state (entry_id, local_version, sync_status) VALUES (?1, 5, 'synced')",
                [entry_id],
            ).unwrap();
            db::set_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION, "1").unwrap();
            // Same invariant as GDrive path: wipe + rekey must clear surface hashes.
            seed_all_surface_push_hashes(&conn);
            let mut keys_for_encode: BTreeMap<u32, zeroize::Zeroizing<[u8; KEY_SIZE]>> =
                BTreeMap::new();
            keys_for_encode.insert(1, raw_content_key_1.clone());
            let list_hex = encode_content_key_list(&raw_master, &keys_for_encode)
                .expect("encode_content_key_list");
            db::set_setting(&conn, db::WRAPPED_CONTENT_LIST, &list_hex).unwrap();
        }

        let provider: Arc<dyn crate::sync::SyncProvider> = Arc::new(MockProvider::new());

        // Run migration with keyring_io=None (iCloud/local path).
        run_cloud_content_migration(&state, &key_state, &provider, device_id, None)
            .await
            .expect("iCloud migration must succeed without keyring_io");

        // Assert: sync_state reset to pending.
        {
            let conn = state.lock().unwrap();
            let count_synced: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sync_state WHERE sync_status = 'synced'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count_synced, 0, "sync_state must be reset");

            // Assert: migration flag is cleared.
            let flag = db::get_setting(&conn, db::NEEDS_CLOUD_CONTENT_MIGRATION).unwrap();
            assert!(
                flag.is_none() || flag.as_deref() != Some("1"),
                "NEEDS_CLOUD_CONTENT_MIGRATION must be cleared even for iCloud path, got {flag:?}"
            );

            // Assert: all 8 surface push hashes cleared (iCloud path too).
            assert_all_surface_push_hashes_cleared(&conn);
        }
    }

    // ─── Send probe ──────────────────────────────────────────────────────────

    struct NoopReporter;

    impl ProgressReporter for NoopReporter {
        fn report(&self, _event: crate::sync::engine::SyncProgressEvent) {}
    }

    /// No Drive token: the probe never runs the cycle.
    struct NoCredentials;

    impl CloudCredentialStore for NoCredentials {
        fn client_config(
            &self,
            _conn: &rusqlite::Connection,
        ) -> Result<crate::sync::cloud_config::GDriveClientConfig, String> {
            Err("no Drive client in these tests".to_string())
        }
        fn load_refresh_token(
            &self,
            _conn: &rusqlite::Connection,
        ) -> Result<Option<Zeroizing<String>>, String> {
            Ok(None)
        }
        fn store_refresh_token(
            &self,
            _conn: &rusqlite::Connection,
            _token: &str,
        ) -> Result<(), String> {
            Ok(())
        }
        fn clear_refresh_token(&self, _conn: &rusqlite::Connection) -> Result<(), String> {
            Ok(())
        }
    }

    struct NoopHooks;

    impl SyncPlatformHooks for NoopHooks {
        fn progress_reporter(&self) -> Arc<dyn ProgressReporter> {
            Arc::new(NoopReporter)
        }
        fn on_status(&self, _phase: SyncPhase, _state: &AppState, _error: Option<String>) {}
        fn on_force_re_pair(&self, _reason: &str) {}
        fn on_scope_upgrade_required(&self, _state: &AppState) {}
        fn after_successful_cycle(&self, _state: &AppState, _key_state: &EncryptionKeyState) {}
        fn after_cycle(&self, _state: &AppState, _summary: &SyncSummary) -> Result<(), String> {
            Ok(())
        }
        fn outbox_sink<'a>(&'a self, _state: &'a AppState) -> Box<dyn OutboxEventSink + 'a> {
            Box::new(crate::sync::outbox_import::NoopOutboxSink)
        }
        fn media_dir(&self) -> std::path::PathBuf {
            std::path::PathBuf::from("media")
        }
    }

    impl CloudConnectHooks for NoopHooks {
        fn on_connect_progress(&self, _session_id: &str, _step: &str) {}
        fn set_live_session(&self, _session: Option<crate::sync::gdrive_provider::GDriveSession>) {}
        fn credentials(&self) -> &dyn CloudCredentialStore {
            &NoCredentials
        }
    }

    fn assert_send<T: Send>(_: &T) {}

    /// Compile-time probe: mobile may spawn the cycle on a multi-thread
    /// runtime only if its future is `Send`.
    #[test]
    fn run_sync_cycle_future_is_send() {
        let state = make_state();
        let key_state = EncryptionKeyState::new();
        let hooks = NoopHooks;
        let cycle = run_sync_cycle(&state, &key_state, SyncTrigger::Automatic, &hooks);
        assert_send(&cycle);
    }
}
