//! The cloud connect and disconnect flow, shared by the desktop app and the
//! mobile FFI.
//!
//! [`complete_cloud_connect`] is the provider-agnostic part of a connect: it
//! runs after the platform has finished its own setup (the desktop's OAuth
//! loopback exchange or folder pick). Everything it cannot do itself (the
//! progress hint, the in-memory Drive session, where the refresh token lives)
//! goes through [`CloudConnectHooks`]. Each hook call stands in for one call
//! the desktop flow made before the move, at the same position and with the
//! same arguments.

use serde::Deserialize;
use serde::Serialize;

use super::cloud_config::configured_cloud_provider;
use super::cloud_config::CloudCredentialStore;
use super::force_re_pair::apply_inconclusive_force_re_pair;
use super::force_re_pair::clear_force_re_pair;
use super::force_re_pair::clear_stale_flag_for_empty_cloud;
use super::force_re_pair::detect_force_re_pair;
use super::force_re_pair::force_re_pair_required;
use super::force_re_pair::resolve_force_re_pair_outcome;
use super::force_re_pair::set_force_re_pair_flags;
use super::force_re_pair::ForceRePairCheck;
use super::force_re_pair::ForceRePairDecision;
use super::force_re_pair::ForceRePairMismatch;
use super::keyring_publish::apply_dirty_flag_result;
use super::keyring_publish::extract_v1_content_for_publish;
use super::keyring_publish::publish_v2_keyring_with_provider_for_generation;
use super::keyring_publish::try_reupload_keyring_with_provider;
use super::pending_session;
use super::pending_session::PendingCloudSession;
use super::pending_session::PendingDriveSession;
use super::platform::CloudConnectHooks;
use super::platform::SyncPlatformHooks;
use super::CloudProvider;
use crate::db;
use crate::vault::setup::V2KeyringPublishContext;
use crate::AppState;
use crate::EncryptionKeyState;

/// Cached Drive storage usage in bytes. Refreshed on connect and via
/// `gdrive_refresh_storage_quota`. Persisted so `gdrive_get_status` can
/// return a stable value across app restarts without an extra API call.
#[doc(hidden)]
pub const GDRIVE_STORAGE_USED_KEY: &str = "gdrive_storage_used";
#[doc(hidden)]
pub const GDRIVE_STORAGE_TOTAL_KEY: &str = "gdrive_storage_total";
/// Cached sum of bytes this app owns in Drive `appDataFolder` (Memlore-only).
#[doc(hidden)]
pub const GDRIVE_STORAGE_APP_USED_KEY: &str = "gdrive_storage_app_used";

/// Settings key persisted when an automatic scope-mismatch disconnect runs.
/// The frontend renders the reconnect banner while this is true; clearing
/// it is the responsibility of `gdrive_complete_connect` (on successful
/// reconnect) or of the user dismissing the banner via
/// `clear_sync_scope_upgrade_required`.
#[doc(hidden)]
pub const GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY: &str = "gdrive_scope_upgrade_required";

/// Typed outcome of `gdrive_complete_connect`.
///
/// The `Err` path is reserved for true failures (network, auth, etc.).
/// Routing outcomes that require specific frontend screens are returned as
/// `Ok(variant)` so the frontend can handle them with a type-safe match.
///
/// Frontend re-fetches `gdriveGetStatus()` after a `Ready` outcome.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GdriveConnectOutcome {
    /// OAuth + keyring reconcile succeeded. Frontend re-fetches status.
    Ready,
    /// Cloud has no V2 keyring yet — this device must run first-time setup
    /// (password mode only, cloud keyring absent).
    NeedsFirstTimeSetup,
    /// Cloud has a V2 vault, local mode is Unset — must run onboarding.
    NeedsOnboarding,
    /// Cloud vault epoch/fingerprint differs from local — force re-pair.
    /// Phase 5 will wire this to a ForceRePairScreen.
    NeedsForceRePair { reason: String, cloud_epoch: u64 },
    /// V1 keyring was present and wiped — must reconnect.
    V1WipedReconnectRequired,
}

/// Clear persisted gdrive session settings (refresh token, folder id, email)
/// and the `sync_connected` flag. Does NOT clear `gdrive_client_id` or
/// `gdrive_client_secret` — those are app-level OAuth credentials that
/// survive a disconnect so the user can reconnect without re-running the
/// dev setup script.
#[doc(hidden)]
pub fn clear_gdrive_settings(
    conn: &rusqlite::Connection,
    creds: &dyn CloudCredentialStore,
) -> Result<(), String> {
    creds.clear_refresh_token(conn)?;
    let keys = [
        "gdrive_root_folder_id",
        "gdrive_email",
        "sync_connected",
        "sync_provider",
        "sync_config_json",
        GDRIVE_STORAGE_USED_KEY,
        GDRIVE_STORAGE_TOTAL_KEY,
        GDRIVE_STORAGE_APP_USED_KEY,
    ];
    for key in &keys {
        db::delete_setting(conn, key).map_err(|e| e.to_string())?;
    }
    // Flip the scheduler off — there is no provider left to sync against.
    // Mirrors the invariant enforced by `db::clear_sync_provider`.
    db::set_sync_enabled(conn, false).map_err(|e| e.to_string())?;
    Ok(())
}

/// Atomically persist the settings that mark a successful cloud connect.
///
/// - `GDrive`: encrypted refresh token, `sync_provider=gdrive`, connected
///   flag, and `sync_enabled`. The last one is what the UI reads to decide
///   whether to show "Sync off".
/// - `Folder`: `sync_provider=kind`, `sync_config_json={"root_path"}`
///   (Phase 2 `LocalConfig` shape), connected flag, and `sync_enabled`.
#[doc(hidden)]
pub fn persist_connect_settings(
    conn: &rusqlite::Connection,
    pending: &PendingCloudSession,
    creds: &dyn CloudCredentialStore,
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    match pending {
        PendingCloudSession::GDrive { session, .. } => {
            creds.store_refresh_token(&tx, &session.refresh_token)?;
            db::set_setting(&tx, "sync_provider", "gdrive").map_err(|e| e.to_string())?;
        }
        PendingCloudSession::Folder { kind, root_path } => {
            let json = serde_json::to_string(&serde_json::json!({ "root_path": root_path }))
                .map_err(|e| e.to_string())?;
            db::set_setting(&tx, "sync_provider", kind).map_err(|e| e.to_string())?;
            db::set_sync_config_json(&tx, &json).map_err(|e| e.to_string())?;
        }
    }
    db::set_setting(&tx, "sync_connected", "true").map_err(|e| e.to_string())?;
    db::set_sync_enabled(&tx, true).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Provider-agnostic remainder of the cloud connect pipeline.
///
/// Callers finish provider-specific setup (OAuth token exchange, folder
/// validation) then land here with a live [`CloudProvider`] and a
/// [`PendingCloudSession`].
#[doc(hidden)]
pub async fn complete_cloud_connect(
    hooks: &dyn CloudConnectHooks,
    state: &AppState,
    key_state: &EncryptionKeyState,
    session_id: &str,
    provider: CloudProvider,
    pending: PendingCloudSession,
    local_mode: db::EncryptionMode,
    _vault_unlocked: bool,
    _password: &str,
) -> Result<GdriveConnectOutcome, String> {
    let is_unset_mode = local_mode == db::EncryptionMode::Unset;

    let local_generation = {
        let conn = state.lock()?;
        db::get_sync_recovery_generation(&conn).map_err(|e| e.to_string())?
    };
    let provider = provider.with_recovery_fence(local_generation, None);

    hooks.on_connect_progress(session_id, "establishing_root");

    // Establish the stable root/control authority before creating any device
    // or generation namespace. An existing non-empty root without control is
    // ambiguous and must never be claimed by an unconditional bootstrap.
    let observed_control = crate::sync::recovery::read_sync_control(&provider)
        .await
        .map_err(|error| format!("Failed to read cloud recovery control: {error}"))?;
    let (root_folder_id, root_created) = provider
        .ensure_root_folder_only()
        .await
        .map_err(|error| format!("Failed to establish cloud root: {error}"))?;
    let control = match observed_control {
        Some(control) => control,
        None => {
            if !root_created
                && !provider
                    .root_is_empty(&root_folder_id)
                    .await
                    .map_err(|error| format!("Failed to inspect cloud root: {error}"))?
            {
                return Err("RECOVERY_CONTROL_MISSING_ON_NONEMPTY_CLOUD".to_string());
            }
            crate::sync::recovery::bootstrap_sync_control(&provider, chrono::Utc::now().timestamp())
                .await
                .map_err(|error| format!("Failed to initialize cloud recovery control: {error}"))?
        }
    };
    if control.recovery_lease.is_some() {
        return Err("AUTHORITATIVE_RECOVERY_IN_PROGRESS".to_string());
    }
    if control.recovery_generation < local_generation {
        return Err(format!(
            "RECOVERY_GENERATION_ROLLBACK: cloud={}, local={local_generation}",
            control.recovery_generation
        ));
    }
    let candidate_generation = control.recovery_generation;
    provider.configure_recovery_fence_authority(candidate_generation, None);

    // Versioned snapshot used for mid-flow revalidation (before delayed
    // publish and immediately before Ready). A peer that acquires a lease or
    // bumps generation after this point must fail this connect closed.
    let (control_snapshot, control_revision) =
        crate::sync::recovery::read_versioned_sync_control(&provider)
            .await
            .map_err(|error| format!("Failed to snapshot cloud recovery control: {error}"))?;
    if let Err(error) = assert_connect_control_snapshot_stable(
        candidate_generation,
        &control_revision,
        &control_snapshot,
        &control_revision,
    ) {
        // Snapshot itself inconsistent with the pre-versioned read (lease
        // race between the two reads).
        return Err(error);
    }
    // Prefer the versioned generation if it advanced (rare race); keep
    // candidate aligned with the revision we will revalidate against.
    let candidate_generation = control_snapshot.recovery_generation;
    provider.configure_recovery_fence_authority(candidate_generation, None);

    // Persist refresh token + sync_connected flag FIRST (atomically), so a
    // crash after this point leaves the DB in a valid state and the next
    // launch can lazily re-run ensure_folder_structure.
    //
    // CRASH WINDOW: a hard crash between this write and the key-swap DB
    // commit below leaves sync_connected=true with the pre-swap key. The
    // error-path rollback cannot catch a process kill. Healed by
    // startup_keyring_reconcile_if_needed which runs on each unlock.
    //
    // Unset-mode exception: do NOT persist here. Setup has not completed,
    // so there is no key material to store the token against yet.
    // resolve_unset_outcome_and_stash() stashes the session in
    // PENDING_DRIVE_SESSION; onboard_complete / first-time setup persists it
    // atomically once setup finishes.
    // Stamp the Drive root id discovered above onto the pending session so
    // persist / Unset-mode stash carry the same folder the fence just claimed.
    let pending = match pending {
        PendingCloudSession::GDrive { session, .. } => PendingCloudSession::GDrive {
            session,
            root_folder_id: root_folder_id.clone(),
        },
        other => other,
    };

    if !is_unset_mode {
        let conn = state.lock()?;
        persist_connect_settings(&conn, &pending, hooks.credentials())
            .map_err(|e| e.to_string())?;
    }

    // Get device_id for folder layout.
    let device_id = {
        let conn = state.lock()?;
        db::get_or_create_device_id(&conn).map_err(|e| e.to_string())?
    };

    hooks.on_connect_progress(session_id, "finalizing");

    // ── V1 keyring wipe (one-shot migration) ─────────────────────────────────
    //
    // If a V1 `.meta/keyring.json` is present on Drive, wipe it and return a
    // typed error so the frontend can prompt the user to reconnect. This is a
    // one-shot migration: once the V1 file is gone, subsequent connects skip
    // this block entirely.
    //
    // Ordering note: a V1 keyring is always wiped here. There is no longer a
    // mode-conflict case to order against — none-mode was removed, so every
    // device is password-mode. The V1 file is gone after the wipe.
    // Intercept V1 wipe: return typed outcome instead of propagating Err.
    match wipe_v1_keyring_if_present(&provider, state, hooks.credentials()).await {
        Ok(_) => {}
        Err(e) if e.starts_with("V1_WIPE_COMPLETE_RECONNECT_REQUIRED") => {
            return Ok(GdriveConnectOutcome::V1WipedReconnectRequired);
        }
        Err(e) => return Err(e),
    }

    // ── Pre-connect cloud-keyring probe ──────────────────────────────────────
    //
    // Always-encrypted: the cloud folder can only ever hold a `.meta/keyring.json`
    // / `_meta.json` (password-mode marker). The none-mode `.meta/mode.json`
    // marker and its mode-mismatch guard were removed — there is no none-mode
    // device left to conflict with.
    //
    // TODO Phase 3: detect V2 keyring presence via `_meta.json` instead of V1
    // `keyring.json`. For now we probe the V2 meta path as a best-effort check;
    // if `_meta.json` exists, we treat the cloud as password-mode. This replaces
    // the deleted V1 `keyring::download_from_drive` probe.
    //
    // I2: distinguish NotFound (absent) from real errors (auth / network / quota).
    // Previously all errors were treated as "not present", silently bypassing
    // the mode-mismatch guard on auth failures.
    let cloud_keyring_meta = {
        match crate::sync::keyring_v2::read_meta(&provider).await {
            Ok(meta) => meta,
            Err(e) => {
                if let Ok(conn) = state.lock() {
                    let _ = clear_gdrive_settings(&conn, hooks.credentials());
                }
                return Err(format!("Failed to probe cloud keyring: {e}"));
            }
        }
    };
    let cloud_keyring_present = cloud_keyring_meta.is_some();

    if is_unset_mode {
        // Local mode is Unset (welcome screen / fresh install).
        // Delegate to the extracted helper which stashes continuation outcomes
        // (NeedsOnboarding, NeedsFirstTimeSetup) in PENDING_DRIVE_SESSION and
        // returns terminal outcomes without stashing.
        //
        // No DB cleanup is needed here: persist_connect_settings was skipped for
        // Unset mode above, so the DB is still clean regardless of outcome.
        return Ok(resolve_unset_outcome_and_stash(
            session_id,
            pending,
            cloud_keyring_present,
        ));
    }

    if let Err(error) = validate_connect_generation_alignment(
        local_generation,
        candidate_generation,
        cloud_keyring_meta
            .as_ref()
            .map(|meta| meta.recovery_generation),
    ) {
        if let Ok(conn) = state.lock() {
            let _ = clear_gdrive_settings(&conn, hooks.credentials());
        }
        return Err(error);
    }

    // Only after mode/account/keyring authority validation may connect create
    // a generation-scoped device namespace.
    if let Err(e) = provider.ensure_folder_structure(&device_id).await {
        let (clear_local, best_effort_device_cleanup) = connect_folder_failure_cleanup_plan();
        if best_effort_device_cleanup {
            // Revalidate authority then delete the partial device namespace so
            // a failed connect does not leave orphan generations/g-N/<device>
            // under a recovery fence. Best-effort: never mask the original error.
            if let Err(cleanup_err) = provider
                .best_effort_delete_device_namespace(&device_id)
                .await
            {
                log::warn!(
                    "gdrive_complete_connect: best-effort device namespace cleanup failed: {cleanup_err}"
                );
            }
        }
        if clear_local {
            if let Ok(conn) = state.lock() {
                let _ = clear_gdrive_settings(&conn, hooks.credentials());
            }
        }
        return Err(e.to_string());
    }

    // ── V2 keyring publish on delayed connect (password mode only) ────────────
    //
    // When the user completes first-time setup offline (no Drive at that point),
    // `confirm_first_time_setup` skips the Drive upload. The local DB has all
    // key material, but the cloud has no keyring → no other device can onboard.
    //
    // C2 fix: if local is password mode AND the cloud has no `_meta.json` yet,
    // publish the initial V2 keyring now. If publish fails, set `keyring_dirty`
    // so the sync engine retries. Do NOT fail the whole connect on publish error.
    //
    // Recovery generation: pass the connect-time candidate explicitly so we
    // never plant `_meta.json.recovery_generation=0` while control is already N.
    // (Unset mode already returned above, so reaching here means Password mode.)
    if !cloud_keyring_present {
        log::info!(
            "gdrive_complete_connect: password mode, no cloud keyring — publishing V2 keyring"
        );

        // Revalidate before delayed publish — peer may have acquired a lease
        // or bumped generation during mode/keyring probes above.
        if let Err(error) =
            revalidate_connect_control_authority(&provider, candidate_generation, &control_revision)
                .await
        {
            if let Ok(conn) = state.lock() {
                let _ = clear_gdrive_settings(&conn, hooks.credentials());
            }
            return Err(error);
        }

        // The probe above established the absence of a COMMITTED cloud keyring
        // via a typed NotFound on a live token, so a stale inconclusive flag now
        // guards nothing — clear it before the publish gate reads it, or the
        // vault can never be rebuilt. Must run BEFORE the publish, not on its
        // success: gating it on the publish would leave the flag set, and the
        // `keyring_dirty && re_pair_required` guard in
        // `startup_keyring_reconcile_if_needed` then skips the retry forever.
        // See `clear_stale_flag_for_empty_cloud` for the full safety argument.
        //
        // Deliberately the one hard-fail in this best-effort block: a failed
        // clear is not a failed publish but a failed precondition, and the
        // publish below would be refused by the gate anyway — leaving the user
        // silently deadlocked. Surfacing the error lets them retry the connect.
        {
            let conn = state.lock()?;
            clear_stale_flag_for_empty_cloud(&conn, cloud_keyring_present)?;
        }

        let publish_ctx_result: Result<V2KeyringPublishContext, String> = (|| {
            let conn = state.lock().map_err(|e| e.to_string())?;
            let devices = db::list_devices(&conn).map_err(|e| e.to_string())?;
            let device = devices
                .into_iter()
                .find(|d| d.is_current)
                .ok_or_else(|| "no current device in local DB".to_string())?;
            let recovery_wrapped = db::get_setting(&conn, db::RECOVERY_WRAPPED_MASTER_KEY)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "no recovery_wrapped_master in settings".to_string())?;
            let master_fingerprint = db::get_setting(&conn, db::CLOUD_MASTER_FINGERPRINT)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "no cloud_master_fingerprint in settings".to_string())?;
            let created_at = db::get_setting(&conn, db::RECOVERY_PASSPHRASE_CREATED_AT)
                .map_err(|e| e.to_string())?
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let (wrapped_content_v1, content_fingerprint_v1) =
                extract_v1_content_for_publish(&conn)?;
            Ok(V2KeyringPublishContext {
                recovery_wrapped,
                device_id: device.device_id,
                device_name: device.name,
                master_fingerprint,
                created_at,
                device_created_at: device.created_at,
                last_seen_at: device.last_seen_at,
                wrapped_content_v1,
                content_fingerprint_v1,
            })
        })();

        match publish_ctx_result {
            Ok(ctx) => {
                match publish_v2_keyring_with_provider_for_generation(
                    &provider,
                    &state,
                    &ctx,
                    &key_state,
                    Some(candidate_generation),
                )
                .await
                {
                    Ok(()) => {
                        // Readback: planted meta must match control generation.
                        // Without this, a bug in publish would leave Ready with
                        // a mismatched vault that every joiner rejects.
                        match crate::sync::keyring_v2::read_meta(&provider).await {
                            Ok(Some(meta)) => {
                                if let Err(error) = assert_published_meta_matches_control(
                                    meta.recovery_generation,
                                    candidate_generation,
                                ) {
                                    log::error!(
                                        "gdrive_complete_connect: post-publish meta mismatch: {error}"
                                    );
                                    if let Ok(conn) = state.lock() {
                                        let _ = clear_gdrive_settings(&conn, hooks.credentials());
                                        let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
                                    }
                                    return Err(error);
                                }
                            }
                            Ok(None) => {
                                log::error!(
                                    "gdrive_complete_connect: publish reported Ok but _meta.json missing"
                                );
                                if let Ok(conn) = state.lock() {
                                    let _ = clear_gdrive_settings(&conn, hooks.credentials());
                                    let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
                                }
                                return Err(
                                    "RECOVERY_GENERATION_MISMATCH: _meta.json missing after publish"
                                        .to_string(),
                                );
                            }
                            Err(e) => {
                                log::error!(
                                    "gdrive_complete_connect: post-publish meta readback failed: {e}"
                                );
                                if let Ok(conn) = state.lock() {
                                    let _ = clear_gdrive_settings(&conn, hooks.credentials());
                                    let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
                                }
                                return Err(format!(
                                    "Failed to verify published keyring generation: {e}"
                                ));
                            }
                        }
                        log::info!("gdrive_complete_connect: V2 keyring published");
                        if let Ok(conn) = state.lock() {
                            let _ = db::delete_setting(&conn, db::KEYRING_DIRTY_KEY);
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "gdrive_complete_connect: V2 keyring publish failed ({e}), setting keyring_dirty"
                        );
                        if let Ok(conn) = state.lock() {
                            let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "gdrive_complete_connect: could not build publish context ({e}), setting keyring_dirty"
                );
                if let Ok(conn) = state.lock() {
                    let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
                }
            }
        }
    } else if cloud_keyring_present {
        // Password mode + cloud keyring present → normal reconnect (same device).
        // Phase 5: compare cloud fingerprint vs local to detect vault rotation by peer.
        match resolve_force_re_pair_outcome(detect_force_re_pair(&provider, &state).await) {
            ForceRePairDecision::NeedsRepair(reason) => {
                // Another device rotated the vault since this device last synced.
                // Surface a typed outcome so the frontend routes to ForceRePairScreen.
                // Propagate a flag-write failure: `sync_connected=true` was already
                // persisted above, so returning the typed outcome without the
                // persisted flag would leave push unblocked after a restart.
                {
                    let conn = state.lock()?;
                    set_force_re_pair_flags(&conn, &reason.reason)?;
                }
                // Store the FRESH session before returning: the follow-up
                // `complete_force_re_pair` resolves its provider warm-session
                // first, and a stale (possibly revoked) session left in managed
                // state would shadow the new token just persisted to the DB —
                // making the mnemonic re-pair fail even though the reconnect
                // succeeded.
                if let PendingCloudSession::GDrive { session, .. } = &pending {
                    hooks.set_live_session(Some(session.clone()));
                }
                return Ok(GdriveConnectOutcome::NeedsForceRePair {
                    reason: reason.reason,
                    cloud_epoch: reason.cloud_epoch,
                });
            }
            ForceRePairDecision::Verified => {
                log::info!("keyring_v2: fingerprint matches — normal reconnect");
            }
            ForceRePairDecision::Skipped => {
                // No comparison was possible — leave any existing flag untouched.
                log::info!("keyring_v2: fingerprint check skipped — nothing to compare");
            }
            ForceRePairDecision::Inconclusive {
                message: e,
                is_auth,
            } => {
                // Dead auth → surface the reconnect error without flagging re-pair.
                // The vault is not implicated, and the flag would outlive the cause.
                if is_auth {
                    log::warn!(
                        "keyring_v2: fingerprint check failed on Drive auth ({e}) — \
                         reconnect required, NOT force-re-pair"
                    );
                    return Err(format!("gdrive_complete_connect: {e}"));
                }
                // Fail-closed: an inconclusive keyring check must not leave the
                // connection in a pushable state. sync_connected=true was already
                // persisted above; set FORCE_RE_PAIR_REQUIRED so ensure_safe_to_push
                // blocks any sync until the user reconnects and the check succeeds.
                log::error!(
                    "keyring_v2: fingerprint check failed ({e}), \
                     blocking push until re-pair succeeds (fail-closed)"
                );
                if let Ok(conn) = state.lock() {
                    // Propagate: a write failure means the push-blocking flag was
                    // NOT set, so we must surface that rather than silently continuing.
                    apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")?;
                }
                return Err(format!(
                    "gdrive_complete_connect: keyring fingerprint check failed: {e}"
                ));
            }
        }
    }

    // Account email + quota from Drive `about` (best-effort — don't fail
    // the whole connect if the read-only endpoint is momentarily unavailable).
    //
    // Email MUST come from `about.user.emailAddress`, not OAuth2 userinfo:
    // we only request `drive.appdata` and never `openid`/`email`, so userinfo
    // returns nothing useful. The Memlore appDataFolder sum walks every file
    // and is deferred to `gdrive_refresh_storage_quota` when Settings opens.
    let about = match provider.as_gdrive() {
        Some(gdrive) => gdrive.fetch_drive_about().await.ok(),
        None => None,
    };
    let email = about.as_ref().and_then(|a| a.email.clone());
    let storage_used = about
        .as_ref()
        .and_then(|a| a.quota.usage.as_deref())
        .and_then(|u| u.parse::<u64>().ok());
    let storage_total = about
        .as_ref()
        .and_then(|a| a.quota.limit.as_deref())
        .and_then(|l| l.parse::<u64>().ok());

    // Final recovery-authority revalidation immediately before Ready. The
    // candidate_generation / control_revision snapshot is stale after probe
    // + optional publish + force-re-pair detect; a peer that acquired a lease
    // or bumped generation mid-flow must not leave this device Ready with a
    // live session at a superseded generation.
    if let Err(error) =
        revalidate_connect_control_authority(&provider, candidate_generation, &control_revision)
            .await
    {
        if let Ok(conn) = state.lock() {
            let _ = clear_gdrive_settings(&conn, hooks.credentials());
        }
        // Best-effort: do not leave a connected-looking device namespace if we
        // never committed Ready.
        if let Err(cleanup_err) = provider
            .best_effort_delete_device_namespace(&device_id)
            .await
        {
            log::warn!(
                "gdrive_complete_connect: Ready revalidate failed; device cleanup: {cleanup_err}"
            );
        }
        return Err(error);
    }

    // Second DB write — folder id + email + storage quota cache (best-effort
    // extras). Storage values are cached so `gdrive_get_status` can return
    // them after an app restart without a live API call.
    {
        let conn = state.lock()?;
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        if candidate_generation > 0 {
            db::set_sync_recovery_generation(&tx, candidate_generation)
                .map_err(|e| e.to_string())?;
        }
        if provider.as_gdrive().is_some() {
            db::set_setting(&tx, "gdrive_root_folder_id", &root_folder_id)
                .map_err(|e| e.to_string())?;
            if let Some(ref email) = email {
                db::set_setting(&tx, "gdrive_email", email).map_err(|e| e.to_string())?;
            }
            if let Some(used) = storage_used {
                db::set_setting(&tx, GDRIVE_STORAGE_USED_KEY, &used.to_string())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(total) = storage_total {
                db::set_setting(&tx, GDRIVE_STORAGE_TOTAL_KEY, &total.to_string())
                    .map_err(|e| e.to_string())?;
            }
        }
        // A fully successful reconnect is the canonical "scope upgrade
        // complete" signal — clear the banner flag in the same tx so the
        // user doesn't have to dismiss the banner manually. Deferred to
        // here (not the earlier persist_connect_settings block) so a
        // mid-flow failure leaves the banner up; the user reconnects again
        // and the flag clears on the eventual success.
        db::delete_setting(&tx, GDRIVE_SCOPE_UPGRADE_REQUIRED_KEY).map_err(|e| e.to_string())?;
        // Disconnect does not wipe local ledgers (token-only teardown). A
        // reconnect — especially to a different account / empty appdata
        // folder — must not keep hash-gated surfaces "clean" against a
        // cloud that no longer holds those blobs. Clear in the same tx as
        // the connect commit, then re-arm Automatic reconcile.
        db::clear_all_surface_push_hashes(&tx).map_err(|e| e.to_string())?;
        db::clear_all_pull_revisions(&tx).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        crate::sync::engine::reset_session_own_cloud_reconciled();
    }

    // Store Drive session in managed state (folder providers have none).
    if let PendingCloudSession::GDrive { session, .. } = pending {
        hooks.set_live_session(Some(session));
    }

    // Ready: frontend re-fetches status via gdriveGetStatus().
    Ok(GdriveConnectOutcome::Ready)
}

// ─── Unset-mode routing helper ────────────────────────────────────────────────

/// Determine the typed outcome for an Unset-mode connect and, for continuation
/// outcomes (`NeedsOnboarding`, `NeedsFirstTimeSetup`), stash the pending
/// cloud session in `PENDING_DRIVE_SESSION` so the follow-on onboard/setup
/// commands can atomically commit provider settings + mode without repeating
/// connect (OAuth exchange or folder pick).
///
///   - keyring present → existing V2 password vault → `NeedsOnboarding`.
///   - keyring absent → empty folder → `NeedsFirstTimeSetup`.
///
/// This function is intentionally state-free (no `AppState`, no DB, no I/O)
/// so it can be called from unit tests without an `AppHandle`.
#[doc(hidden)]
pub fn resolve_unset_outcome_and_stash(
    session_id: &str,
    pending: PendingCloudSession,
    cloud_keyring_present: bool,
) -> GdriveConnectOutcome {
    // Continuation outcomes: stash session for the follow-on setup command.
    let outcome = if cloud_keyring_present {
        // V2 vault present — user must onboard via recovery phrase.
        GdriveConnectOutcome::NeedsOnboarding
    } else {
        // Cloud is empty — this is a new vault; user must do first-time setup.
        GdriveConnectOutcome::NeedsFirstTimeSetup
    };

    pending_session::stash(
        session_id.to_string(),
        PendingDriveSession {
            session: pending,
            created_at: std::time::Instant::now(),
        },
    );

    outcome
}

#[doc(hidden)]
pub fn should_recreate_namespace_after_wipe(disconnect: bool) -> bool {
    !disconnect
}

#[doc(hidden)]
pub fn validate_connect_generation_alignment(
    local_generation: u64,
    control_generation: u64,
    keyring_meta_generation: Option<u64>,
) -> Result<u64, String> {
    if control_generation < local_generation {
        return Err(format!(
            "RECOVERY_GENERATION_ROLLBACK: cloud={control_generation}, local={local_generation}"
        ));
    }
    if let Some(meta_generation) = keyring_meta_generation {
        if meta_generation != control_generation {
            return Err(format!(
                "RECOVERY_GENERATION_MISMATCH: control={control_generation}, keyring_meta={meta_generation}"
            ));
        }
    }
    Ok(control_generation)
}

/// Fail closed when cloud recovery authority moved mid-connect.
///
/// Compares a snapshot taken after control bootstrap against a fresh
/// versioned read. Any active lease, generation bump, or revision change
/// means a peer raced us — Ready / delayed publish must not proceed.
#[doc(hidden)]
pub fn assert_connect_control_snapshot_stable(
    expected_generation: u64,
    expected_revision: &str,
    observed: &crate::sync::sync_control::SyncControlV1,
    observed_revision: &str,
) -> Result<(), String> {
    if observed.recovery_lease.is_some() {
        return Err("AUTHORITATIVE_RECOVERY_IN_PROGRESS".to_string());
    }
    if observed.recovery_generation != expected_generation {
        return Err(format!(
            "RECOVERY_CONTROL_CHANGED: generation expected={expected_generation}, observed={}",
            observed.recovery_generation
        ));
    }
    if observed_revision != expected_revision {
        return Err(format!(
            "RECOVERY_CONTROL_CHANGED: revision expected={expected_revision}, observed={observed_revision}"
        ));
    }
    Ok(())
}

/// Post-publish readback: planted meta must match control generation.
#[doc(hidden)]
pub fn assert_published_meta_matches_control(
    meta_generation: u64,
    control_generation: u64,
) -> Result<(), String> {
    if meta_generation != control_generation {
        return Err(format!(
            "RECOVERY_GENERATION_MISMATCH: control={control_generation}, keyring_meta={meta_generation}"
        ));
    }
    Ok(())
}

/// Re-read versioned `control.json` and reject if authority moved vs snapshot.
#[doc(hidden)]
pub async fn revalidate_connect_control_authority<P>(
    provider: &P,
    expected_generation: u64,
    expected_revision: &str,
) -> Result<(), String>
where
    P: crate::sync::keyring_v2::KeyringV2Io + ?Sized,
{
    let (control, revision) = crate::sync::recovery::read_versioned_sync_control(provider)
        .await
        .map_err(|error| format!("Failed to revalidate cloud recovery control: {error}"))?;
    assert_connect_control_snapshot_stable(
        expected_generation,
        expected_revision,
        &control,
        &revision,
    )
}

/// Local settings always clear on connect folder failure; device namespace is
/// best-effort cleaned when a partial create may have landed on Drive.
#[doc(hidden)]
pub fn connect_folder_failure_cleanup_plan() -> (bool, bool) {
    // (clear_local_settings, best_effort_delete_device_namespace)
    (true, true)
}

/// Folder providers never hold a Google OAuth token to revoke.
#[doc(hidden)]
pub fn disconnect_should_revoke_oauth(provider: Option<&CloudProvider>) -> bool {
    matches!(provider, Some(p) if p.kind() == "gdrive")
}

/// Clear persisted cloud-connect settings and the in-memory Drive session.
#[doc(hidden)]
pub fn apply_disconnect_clear(
    state: &AppState,
    hooks: &dyn CloudConnectHooks,
) -> Result<(), String> {
    {
        let conn = state.lock()?;
        clear_gdrive_settings(&conn, hooks.credentials())?;
    }
    hooks.set_live_session(None);
    Ok(())
}

/// Best-effort keyring reconcile at unlock, against the V2 keyring layout
/// (`_meta.json` + `devices/<id>.json`): re-uploads a dirty device slot and
/// detects a force re-pair. An inconclusive non-auth check sets the
/// fail-closed force re-pair flag, which the sync cycle's pre-flight does not
/// do, so every platform runs this after unlock.
#[doc(hidden)]
pub async fn startup_keyring_reconcile_if_needed(
    state: &AppState,
    key_state: &EncryptionKeyState,
    hooks: &dyn SyncPlatformHooks,
) {
    // Reconstruct the configured provider. If unavailable, skip silently —
    // reconcile is best-effort at unlock time.
    let provider = match (|| {
        let conn = state.lock()?;
        configured_cloud_provider(&conn, hooks.credentials())?
            .ok_or_else(|| "No cloud provider connected".to_string())
    })() {
        Ok(p) => p,
        Err(e) => {
            log::info!("startup_keyring_reconcile_if_needed: no cloud provider ({e}), skipping");
            return;
        }
    };

    // Check for pending keyring dirty flag first.
    let (keyring_dirty, re_pair_required) = {
        match state.lock() {
            Ok(conn) => (
                db::get_setting(&conn, db::KEYRING_DIRTY_KEY)
                    .ok()
                    .flatten()
                    .map(|v| v == "1")
                    .unwrap_or(false),
                force_re_pair_required(&conn),
            ),
            // Fail-closed on lock failure: skip the re-upload rather than publish
            // from a device whose flag state we could not read.
            Err(_) => (false, true),
        }
    };
    // A flagged device must not publish its keyring. `try_reupload_keyring_with_provider`
    // writes `_meta.json` from LOCAL truth with no epoch/CAS guard, so re-uploading here
    // would clobber a peer's rotation and make the fingerprint check below (and any later
    // `recheck_force_re_pair`) compare against this device's own write — turning a genuine
    // `vault_rotated` mismatch into a false `Verified`. The flag does not gate unlock in
    // password mode (the pre-unlock DB is an empty placeholder — see `lib.rs`), so this
    // path really does run with the flag set; the gate is load-bearing, not defensive.
    // Pre-detected rotation from the dirty pre-flight below; routed into the
    // main match so the NeedsRepair handling is not duplicated.
    let mut pre_detected: Option<ForceRePairMismatch> = None;

    if keyring_dirty && re_pair_required {
        log::warn!(
            "startup_keyring_reconcile_if_needed: keyring_dirty set but force-re-pair is \
             required — skipping re-upload so this device cannot clobber the cloud keyring"
        );
    } else if keyring_dirty {
        // Pre-flight: a peer rotation this device has NOT yet observed (flag
        // still unset) must be detected BEFORE the re-upload — the re-upload
        // writes `_meta.json` from local truth with no CAS, so it would
        // clobber the rotation and the fingerprint check below would verify
        // against our own write. Only a typed `Mismatch` aborts the re-upload:
        // a corrupt `_meta.json` must still flow into it, because the dirty
        // re-upload is the only path that can regenerate a schema-broken meta
        // from local truth (failing closed on corrupt before the repair would
        // deadlock it).
        match detect_force_re_pair(&provider, state).await {
            Ok(ForceRePairCheck::Mismatch(mismatch)) => {
                log::warn!(
                    "startup_keyring_reconcile_if_needed: rotation detected before dirty \
                     re-upload ({}) — skipping re-upload",
                    mismatch.reason
                );
                pre_detected = Some(mismatch);
            }
            _ => {
                log::info!(
                    "startup_keyring_reconcile_if_needed: keyring_dirty set, attempting re-upload"
                );
                let result = try_reupload_keyring_with_provider(state, &provider, key_state).await;
                apply_dirty_flag_result(result, state);
            }
        }
    }

    // Phase 5: fingerprint check — detect vault rotation by another device.
    let decision = match pre_detected {
        Some(mismatch) => ForceRePairDecision::NeedsRepair(mismatch),
        None => resolve_force_re_pair_outcome(detect_force_re_pair(&provider, state).await),
    };
    match decision {
        ForceRePairDecision::NeedsRepair(mismatch) => {
            log::warn!(
                "startup_keyring_reconcile_if_needed: force re-pair required — {}",
                mismatch.reason
            );
            // This function returns (), so a flag-write failure can only be
            // logged — loudly, since push is NOT blocked if the write failed.
            match state.lock() {
                Ok(conn) => {
                    if let Err(e) = set_force_re_pair_flags(&conn, &mismatch.reason) {
                        log::error!(
                            "CRITICAL: startup_keyring_reconcile_if_needed: failed to persist \
                             force-re-pair flags — push may not be blocked: {e}"
                        );
                    }
                }
                Err(e) => {
                    log::error!(
                        "CRITICAL: startup_keyring_reconcile_if_needed: state lock failed while \
                         persisting force-re-pair flags — push may not be blocked: {e}"
                    );
                }
            }
            // Emit the Tauri event so the frontend reacts immediately. This is
            // load-bearing now that reconcile runs deferred (fire-and-forget)
            // after unlock resolves: the UI may already have mounted and
            // hydrated `force_re_pair_required` before this flag was written,
            // so without the event a mismatch would only surface on next
            // restart. The `useForceRePair` listener handles `xj://force-re-pair`.
            hooks.on_force_re_pair(&mismatch.reason);
        }
        ForceRePairDecision::Verified => {
            log::info!("startup_keyring_reconcile_if_needed: fingerprint OK");
            // Deliberately does NOT clear a stale force-re-pair flag, even though
            // the fingerprint matched. The `keyring_dirty` re-upload above may have
            // just rewritten cloud `_meta.json` from local truth, so `Verified` here
            // can mean "this device verified against its own write" — clearing on
            // that would erase a legitimate `vault_rotated` flag (fail-open).
            // Stale-flag recovery lives in `recheck_force_re_pair`, which performs a
            // detect-only check with no re-upload ahead of it.
        }
        ForceRePairDecision::Skipped => {
            // Nothing to compare (no cloud keyring, or setup incomplete). Not a
            // verification — absence of a keyring is not proof the vault is healthy.
            log::info!("startup_keyring_reconcile_if_needed: fingerprint check skipped");
        }
        ForceRePairDecision::Inconclusive {
            message: e,
            is_auth,
        } => {
            // Fail-closed: an inconclusive check at startup must not let this
            // device proceed with push on an unverified keyring.  Set the
            // FORCE_RE_PAIR_REQUIRED flag so ensure_safe_to_push blocks until
            // the user re-pairs.  This function returns (), so we rely on the
            // loud log inside apply_inconclusive_force_re_pair for visibility.
            log::error!(
                "startup_keyring_reconcile_if_needed: fingerprint check inconclusive ({e})"
            );
            // Dead Drive auth is not a keyring-integrity signal — sending the user
            // to the mnemonic re-pair screen would be both wrong and inescapable
            // (every retry fails auth identically and re-sets the flag). The
            // existing GDRIVE_TOKEN_REVOKED reconnect prompt is the right recovery.
            // Push is already unreachable without a valid token.
            if is_auth {
                log::warn!(
                    "startup_keyring_reconcile_if_needed: Drive auth is revoked/expired — \
                     reconnect required, NOT force-re-pair; leaving the flag untouched"
                );
                return;
            }
            log::error!(
                "startup_keyring_reconcile_if_needed: blocking push until re-pair succeeds \
                 (fail-closed)"
            );
            if let Ok(conn) = state.lock() {
                if let Err(write_err) =
                    apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")
                {
                    log::error!(
                        "startup_keyring_reconcile_if_needed: could not write fail-closed flag \
                         — push may NOT be blocked: {write_err}"
                    );
                }
            }
        }
    }
}

// ─── V1 keyring wipe ─────────────────────────────────────────────────────────

/// One-shot V1 keyring wipe.
///
/// Probes `.meta/keyring.json` via `KeyringV2Io::read_file`. If not found,
/// returns `Ok(false)` (caller continues normally). If found:
///
/// 1. Deletes `.meta/keyring.json` from Drive (best-effort — errors logged).
/// 2. Clears all local encryption state (`wrapped_encryption_key`, `kek_salt`,
///    `password_hash`, `keyring_dirty`, `encryption_mode → Unset`).
/// 3. Calls `clear_gdrive_settings` to also clear OAuth tokens and
///    `sync_connected` — the cloud folder was just wiped so the user must
///    re-authorize (I1: prevents half-state with `sync_connected=true`).
/// 4. Calls `reset_local_sync_state` so entries are marked pending for upload
///    after reconnect (project rule: destructive cloud actions must reset local
///    sync state atomically).
/// 5. Returns `Err("V1_WIPE_COMPLETE_RECONNECT_REQUIRED: ...")` so
///    `gdrive_complete_connect` can return the typed error to the frontend —
///    that typed outcome (`v1_wiped_reconnect_required`) is what drives the
///    "reconnect required" banner in `GoogleDriveSettings.tsx`.
///
/// Also CLEARS any standing force-re-pair flag: the wipe resets the entire
/// local encryption state and disconnects Drive, so a leftover flag would
/// strand the user on `ForceRePairScreen` with no Drive session to re-pair
/// against (and no vault left to re-pair to).
#[doc(hidden)]
pub async fn wipe_v1_keyring_if_present(
    provider: &(impl crate::sync::keyring_v2::KeyringV2Io + Send + Sync),
    state: &AppState,
    creds: &dyn CloudCredentialStore,
) -> Result<bool, String> {
    const V1_PATH: &str = ".meta/keyring.json";

    // Probe V1 path. Ok(_) → present; Err(NotFound) → absent; other Err → skip wipe.
    let is_present = match provider.read_file(V1_PATH).await {
        Ok(_) => true,
        Err(crate::sync::provider::SyncError::NotFound(_)) => false,
        Err(e) => {
            log::warn!("v1_wipe: probe failed ({e}), skipping wipe");
            return Ok(false);
        }
    };
    if !is_present {
        return Ok(false);
    }

    log::warn!("v1_wipe: found legacy .meta/keyring.json — wiping for V2 schema migration");

    // C4: fail-loud on delete error — a partial wipe (local cleared, cloud not)
    // would leave the V1 file in place and repeat the wipe loop on every connect.
    provider
        .delete_file(V1_PATH)
        .await
        .map_err(|e| format!("v1_wipe: failed to delete .meta/keyring.json: {e}"))?;

    // Clear local encryption state so the user is prompted to re-setup.
    // C4: propagate all errors instead of swallowing with `let _ = ...`.
    let conn = state
        .lock()
        .map_err(|e| format!("v1_wipe: state lock failed: {e}"))?;
    db::delete_setting(&conn, db::WRAPPED_ENCRYPTION_KEY_KEY)
        .map_err(|e| format!("v1_wipe: clear wrapped_key: {e}"))?;
    db::delete_setting(&conn, db::KEK_SALT_KEY)
        .map_err(|e| format!("v1_wipe: clear kek_salt: {e}"))?;
    db::delete_setting(&conn, "password_hash")
        .map_err(|e| format!("v1_wipe: clear password_hash: {e}"))?;
    db::delete_setting(&conn, db::KEYRING_DIRTY_KEY)
        .map_err(|e| format!("v1_wipe: clear keyring_dirty: {e}"))?;
    db::set_encryption_mode(&conn, db::EncryptionMode::Unset)
        .map_err(|e| format!("v1_wipe: set encryption_mode unset: {e}"))?;
    // The vault this flag referred to no longer exists — clear it so the user
    // is routed to first-time setup, not a dead-end ForceRePairScreen.
    clear_force_re_pair(&conn).map_err(|e| format!("v1_wipe: clear force_re_pair: {e}"))?;
    // I1: also clear OAuth tokens so `sync_connected` / `gdrive_refresh_token`
    // are not left in a half-connected state. The cloud folder was just wiped;
    // the user must re-authorize to proceed.
    clear_gdrive_settings(&conn, creds).map_err(|e| format!("v1_wipe: clear gdrive: {e}"))?;
    // Reset sync_state rows so entries are marked pending upload after reconnect
    // (project rule: destructive cloud actions must atomically reset local sync state).
    reset_local_sync_state(&conn).map_err(|e| format!("v1_wipe: reset sync state: {e}"))?;

    Err("V1_WIPE_COMPLETE_RECONNECT_REQUIRED: \
         legacy keyring was wiped. Drive has been disconnected; \
         please reconnect and complete first-time setup."
        .to_string())
}

// ─── Sync reset ─────────────────────────────────────────────────────────────

/// Counts of rows flipped from synced/uploaded → pending.
/// camelCase via serde so the TS bindings get `entries`, `media`, `journals`.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCounts {
    pub entries: u64,
    pub media: u64,
    pub journals: u64,
}

/// Reset all locally-marked-synced rows back to pending so the next sync
/// cycle re-uploads them. Runs in a single transaction — partial reset is
/// worse than no reset. Only flips `synced` rows; leaves `conflict` and
/// `error` rows alone (they need user resolution, not silent reset). Media:
/// every `uploaded` row that still has a local original, own or downloaded
/// from a peer, is flipped, so a cloud wipe never leaves the last copy
/// evictable. Rows without a local original have nothing to upload and stay.
///
/// **Why a single transaction is load-bearing.** Without it, the sync
/// engine could interleave a write between the three UPDATEs, leaving
/// entries marked `pending` but media still `uploaded`. The next sync
/// would then push entries referencing media files whose remote copy is
/// gone, producing a cascade of "missing media" errors on every peer.
#[doc(hidden)]
pub fn reset_local_sync_state(conn: &rusqlite::Connection) -> rusqlite::Result<ResetCounts> {
    let tx = conn.unchecked_transaction()?;
    let entries = tx.execute(
        "UPDATE sync_state SET sync_status='pending', local_version=local_version+1 WHERE sync_status='synced'",
        [],
    )? as u64;
    let journals = tx.execute(
        "UPDATE journal_sync_state SET sync_status='pending', local_version=local_version+1 WHERE sync_status='synced'",
        [],
    )? as u64;
    let media = db::reset_uploaded_media_with_local_original_to_pending(&tx)? as u64;
    // Surface hashes are stale after any bulk requeue — cloud content may be
    // gone or re-keyed. Absent hash ⇒ default-dirty (full surface re-push).
    // Also re-arm Automatic reconcile so DeviceRoot self-heal runs again.
    db::clear_all_surface_push_hashes(&tx)?;
    db::clear_all_pull_revisions(&tx)?;
    tx.commit()?;
    crate::sync::engine::reset_session_own_cloud_reconciled();
    Ok(ResetCounts {
        entries,
        media,
        journals,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::sync::gdrive_provider::GDriveSession;
    use rusqlite::Connection;

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    fn gdrive_pending_token(refresh_token: &str) -> PendingCloudSession {
        PendingCloudSession::GDrive {
            session: GDriveSession {
                access_token: "at_test".to_string(),
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
                refresh_token: zeroize::Zeroizing::new(refresh_token.to_string()),
                client_id: "cid_test".to_string(),
                client_secret: Some("cs_test".to_string()),
            },
            root_folder_id: "root_test".to_string(),
        }
    }

    /// Mirrors the desktop store: the refresh token lives in the
    /// `gdrive_refresh_token` setting, the client comes from a fixed
    /// placeholder.
    struct DbSettingCredentialStore;

    impl CloudCredentialStore for DbSettingCredentialStore {
        fn client_config(
            &self,
            _conn: &Connection,
        ) -> Result<crate::sync::cloud_config::GDriveClientConfig, String> {
            Ok(crate::sync::cloud_config::GDriveClientConfig {
                client_id: "test-client-id".to_string(),
                client_secret: Some("test-client-secret".to_string()),
            })
        }

        fn load_refresh_token(
            &self,
            conn: &Connection,
        ) -> Result<Option<zeroize::Zeroizing<String>>, String> {
            Ok(db::get_setting(conn, "gdrive_refresh_token")
                .map_err(|e| e.to_string())?
                .filter(|t| !t.is_empty())
                .map(zeroize::Zeroizing::new))
        }

        fn store_refresh_token(&self, conn: &Connection, token: &str) -> Result<(), String> {
            db::set_setting(conn, "gdrive_refresh_token", token).map_err(|e| e.to_string())
        }

        fn clear_refresh_token(&self, conn: &Connection) -> Result<(), String> {
            db::delete_setting(conn, "gdrive_refresh_token").map_err(|e| e.to_string())
        }
    }

    #[test]
    fn wipe_keep_connection_recreates_namespace_but_disconnect_does_not() {
        assert!(should_recreate_namespace_after_wipe(false));
        assert!(!should_recreate_namespace_after_wipe(true));
    }

    #[test]
    fn connect_stages_higher_generation_until_identity_validation() {
        assert_eq!(
            validate_connect_generation_alignment(2, 5, Some(5)).unwrap(),
            5
        );
        assert!(validate_connect_generation_alignment(2, 5, Some(4)).is_err());
        assert_eq!(
            validate_connect_generation_alignment(2, 5, None).unwrap(),
            5
        );
        assert!(validate_connect_generation_alignment(5, 4, Some(4)).is_err());
    }

    #[test]
    fn connect_ready_rejects_when_control_generation_or_revision_or_lease_changes() {
        use crate::sync::keyring_v2::{RecoveryMarker, RECOVERY_MARKER_VERSION};
        use crate::sync::sync_control::{SyncControlV1, SYNC_CONTROL_VERSION};

        let base = SyncControlV1 {
            version: SYNC_CONTROL_VERSION,
            recovery_generation: 3,
            recovery_lease: None,
            updated_at: 10,
        };
        assert!(assert_connect_control_snapshot_stable(3, "rev-a", &base, "rev-a").is_ok());

        // Generation bumped mid-connect.
        let gen_bumped = SyncControlV1 {
            recovery_generation: 4,
            ..base.clone()
        };
        let err =
            assert_connect_control_snapshot_stable(3, "rev-a", &gen_bumped, "rev-a").unwrap_err();
        assert!(
            err.contains("RECOVERY_CONTROL_CHANGED"),
            "expected generation change error, got {err}"
        );

        // Revision changed (lease acquire CAS even if gen same is rare; still fail closed).
        let err = assert_connect_control_snapshot_stable(3, "rev-a", &base, "rev-b").unwrap_err();
        assert!(
            err.contains("RECOVERY_CONTROL_CHANGED"),
            "expected revision change error, got {err}"
        );

        // Lease appears mid-connect.
        let with_lease = SyncControlV1 {
            recovery_lease: Some(RecoveryMarker {
                version: RECOVERY_MARKER_VERSION,
                job_id: 1,
                owner_device_id: "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb".to_string(),
                operation: "local_to_cloud".to_string(),
                recovery_generation: 3,
                nonce: "0123456789abcdef".to_string(),
                created_at: 1,
                updated_at: 1,
            }),
            ..base
        };
        let err =
            assert_connect_control_snapshot_stable(3, "rev-a", &with_lease, "rev-a").unwrap_err();
        assert_eq!(err, "AUTHORITATIVE_RECOVERY_IN_PROGRESS");
    }

    #[test]
    fn published_meta_must_match_control_generation() {
        assert!(assert_published_meta_matches_control(5, 5).is_ok());
        let err = assert_published_meta_matches_control(0, 5).unwrap_err();
        assert!(
            err.contains("RECOVERY_GENERATION_MISMATCH"),
            "expected mismatch error, got {err}"
        );
    }

    #[test]
    fn connect_folder_failure_always_clears_local_and_attempts_device_cleanup() {
        let (clear_local, cleanup_device) = connect_folder_failure_cleanup_plan();
        assert!(clear_local, "must not leave Ready/sync_connected state");
        assert!(
            cleanup_device,
            "must best-effort delete partial device namespace"
        );
    }

    // ─── clear_gdrive_settings ───────────────────────────────────────────────

    #[test]
    fn clear_gdrive_settings_removes_every_known_key() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, "gdrive_refresh_token", "abc123").unwrap();
            db::set_setting(&conn, "gdrive_root_folder_id", "folder-id").unwrap();
            db::set_setting(&conn, "gdrive_email", "x@y.com").unwrap();
            db::set_setting(&conn, "sync_connected", "true").unwrap();
            db::set_setting(&conn, "sync_provider", "gdrive").unwrap();
            db::set_setting(&conn, GDRIVE_STORAGE_USED_KEY, "12345").unwrap();
            db::set_setting(&conn, GDRIVE_STORAGE_TOTAL_KEY, "67890").unwrap();
            db::set_setting(&conn, GDRIVE_STORAGE_APP_USED_KEY, "999").unwrap();
        }
        {
            let conn = state.lock().unwrap();
            clear_gdrive_settings(&conn, &DbSettingCredentialStore).unwrap();
        }
        let conn = state.lock().unwrap();
        for key in [
            "gdrive_refresh_token",
            "gdrive_root_folder_id",
            "gdrive_email",
            "sync_connected",
            "sync_provider",
            "sync_config_json",
            GDRIVE_STORAGE_USED_KEY,
            GDRIVE_STORAGE_TOTAL_KEY,
            GDRIVE_STORAGE_APP_USED_KEY,
        ] {
            assert!(
                db::get_setting(&conn, key).unwrap().is_none(),
                "{key} should be cleared"
            );
        }
    }

    // ─── persist_connect_settings ────────────────────────────────────────────

    #[test]
    fn persist_connect_settings_enables_sync() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            persist_connect_settings(
                &conn,
                &gdrive_pending_token("encrypted-hex"),
                &DbSettingCredentialStore,
            )
            .unwrap();
        }
        let conn = state.lock().unwrap();
        assert_eq!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .as_deref(),
            Some("encrypted-hex")
        );
        assert_eq!(
            db::get_sync_provider(&conn).unwrap().as_deref(),
            Some("gdrive")
        );
        assert_eq!(
            db::get_setting(&conn, "sync_connected").unwrap().as_deref(),
            Some("true")
        );
        assert!(
            db::get_sync_enabled(&conn).unwrap(),
            "sync_enabled must be true after a successful connect so the UI does not show \"Sync off\""
        );
    }

    #[test]
    fn persist_connect_settings_folder_writes_local_config_rows() {
        use crate::sync::pending_session::PendingCloudSession;

        let state = make_state();
        {
            let conn = state.lock().unwrap();
            persist_connect_settings(
                &conn,
                &PendingCloudSession::Folder {
                    kind: "icloud".to_string(),
                    root_path: "/tmp/xj-icloud-persist".to_string(),
                },
                &DbSettingCredentialStore,
            )
            .unwrap();
        }
        let conn = state.lock().unwrap();
        assert_eq!(
            db::get_sync_provider(&conn).unwrap().as_deref(),
            Some("icloud")
        );
        let json = db::get_sync_config_json(&conn)
            .unwrap()
            .expect("sync_config_json must be set");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["root_path"].as_str(), Some("/tmp/xj-icloud-persist"));
        assert_eq!(
            db::get_setting(&conn, "sync_connected").unwrap().as_deref(),
            Some("true")
        );
        assert!(db::get_sync_enabled(&conn).unwrap());
        assert!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .is_none(),
            "Folder persist must not write gdrive_refresh_token"
        );
    }

    // ─── wipe_v1_keyring_if_present ─────────────────────────────────────────

    struct FakeV1KeyringIo {
        /// When `Some`, return these bytes as the keyring.json content.
        /// When `None`, return NotFound.
        keyring_bytes: Option<Vec<u8>>,
        /// Track whether delete_file was called.
        delete_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeV1KeyringIo {
        fn present(bytes: Vec<u8>) -> Self {
            Self {
                keyring_bytes: Some(bytes),
                delete_called: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }

        fn absent() -> Self {
            Self {
                keyring_bytes: None,
                delete_called: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::sync::keyring_v2::KeyringV2Io for FakeV1KeyringIo {
        async fn read_file(&self, path: &str) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
            match &self.keyring_bytes {
                Some(b) => Ok(b.clone()),
                None => Err(crate::sync::provider::SyncError::NotFound(path.to_string())),
            }
        }

        async fn write_file(
            &self,
            _path: &str,
            _data: &[u8],
        ) -> Result<(), crate::sync::provider::SyncError> {
            Ok(())
        }

        async fn delete_file(&self, _path: &str) -> Result<(), crate::sync::provider::SyncError> {
            self.delete_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn list_files(
            &self,
            _prefix: &str,
        ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn v1_wipe_returns_ok_false_when_absent() {
        let state = make_state();
        let io = FakeV1KeyringIo::absent();
        let result = wipe_v1_keyring_if_present(&io, &state, &DbSettingCredentialStore).await;
        assert!(
            matches!(result, Ok(false)),
            "expected Ok(false) when no V1 keyring, got {result:?}"
        );
        assert!(
            !io.delete_called.load(std::sync::atomic::Ordering::SeqCst),
            "delete must not be called when no V1 keyring"
        );
    }

    #[tokio::test]
    async fn v1_wipe_deletes_file_and_resets_local_state_when_present() {
        let state = make_state();
        // Pre-load some encryption state to verify it gets cleared.
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::WRAPPED_ENCRYPTION_KEY_KEY, "deadbeef").unwrap();
            db::set_setting(&conn, db::KEK_SALT_KEY, "cafebabe").unwrap();
            db::set_setting(&conn, "password_hash", "hash123").unwrap();
            db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1").unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Password).unwrap();
            // Seed a STANDING force-re-pair flag: the wipe must clear it, or
            // the user is stranded on ForceRePairScreen with no Drive session.
            set_force_re_pair_flags(&conn, "vault_rotated").unwrap();
        }

        let io = FakeV1KeyringIo::present(b"old-keyring-bytes".to_vec());
        let result = wipe_v1_keyring_if_present(&io, &state, &DbSettingCredentialStore).await;

        // Must return Err with V1_WIPE_COMPLETE_RECONNECT_REQUIRED.
        assert!(
            matches!(&result, Err(e) if e.starts_with("V1_WIPE_COMPLETE_RECONNECT_REQUIRED")),
            "expected V1_WIPE_COMPLETE_RECONNECT_REQUIRED error, got {result:?}"
        );

        // delete_file must have been called.
        assert!(
            io.delete_called.load(std::sync::atomic::Ordering::SeqCst),
            "delete_file must be called on V1 keyring"
        );

        // Local encryption state must be reset.
        let conn = state.lock().unwrap();
        assert!(
            db::get_setting(&conn, db::WRAPPED_ENCRYPTION_KEY_KEY)
                .unwrap()
                .is_none(),
            "wrapped_encryption_key must be cleared"
        );
        assert!(
            db::get_setting(&conn, db::KEK_SALT_KEY).unwrap().is_none(),
            "kek_salt must be cleared"
        );
        assert!(
            db::get_setting(&conn, "password_hash").unwrap().is_none(),
            "password_hash must be cleared"
        );
        assert!(
            db::get_setting(&conn, db::KEYRING_DIRTY_KEY)
                .unwrap()
                .is_none(),
            "keyring_dirty must be cleared"
        );
        assert_eq!(
            db::get_encryption_mode(&conn).unwrap(),
            db::EncryptionMode::Unset,
            "encryption_mode must be reset to Unset"
        );
        // B3/B5 invariant: the wipe just cleared the OAuth token, so it must
        // NEVER leave the force-re-pair flag set — a set flag with no Drive
        // session would strand the user on ForceRePairScreen with no way out.
        assert_eq!(
            crate::sync::force_re_pair::get_force_re_pair_status_inner(&conn).unwrap(),
            None,
            "v1_wipe must not leave force-re-pair required"
        );
        assert!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .is_none(),
            "v1_wipe must not write a force_re_pair_reason breadcrumb (nothing reads it)"
        );
    }

    #[tokio::test]
    async fn revalidate_connect_control_authority_rejects_mid_flow_lease() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::{RecoveryMarker, RECOVERY_MARKER_VERSION};
        use crate::sync::recovery::{
            acquire_recovery_lease, bootstrap_sync_control, read_versioned_sync_control,
        };
        use crate::sync::sync_control::{SyncControlV1, SYNC_CONTROL_VERSION};

        let provider = InMemoryKeyringProvider::new();
        bootstrap_sync_control(&provider, 1).await.unwrap();
        // Bump control generation to 1 with no lease so a later acquire is gen+1.
        {
            use crate::sync::keyring_v2::KeyringV2Io;
            let bumped = SyncControlV1 {
                version: SYNC_CONTROL_VERSION,
                recovery_generation: 1,
                recovery_lease: None,
                updated_at: 2,
            };
            provider
                .write_file(
                    crate::sync::sync_control::SYNC_CONTROL_DRIVE_PATH,
                    &serde_json::to_vec(&bumped).unwrap(),
                )
                .await
                .unwrap();
        }
        let (control, revision) = read_versioned_sync_control(&provider).await.unwrap();
        assert_eq!(control.recovery_generation, 1);
        assert!(
            revalidate_connect_control_authority(&provider, 1, &revision)
                .await
                .is_ok()
        );

        let mut marker = RecoveryMarker {
            version: RECOVERY_MARKER_VERSION,
            job_id: 1,
            owner_device_id: "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb".to_string(),
            operation: "local_to_cloud".to_string(),
            recovery_generation: 2,
            nonce: "0123456789abcdef".to_string(),
            created_at: 1,
            updated_at: 1,
        };
        // acquire_recovery_lease requires gen = control+1
        marker.recovery_generation = 2;
        acquire_recovery_lease(&provider, &marker).await.unwrap();

        let err = revalidate_connect_control_authority(&provider, 1, &revision)
            .await
            .unwrap_err();
        assert!(
            err.contains("AUTHORITATIVE_RECOVERY_IN_PROGRESS")
                || err.contains("RECOVERY_CONTROL_CHANGED"),
            "expected lease/control change rejection, got {err}"
        );
    }

    /// I1: after V1 wipe, OAuth tokens and sync_connected must also be cleared
    /// (not just encryption state), so the app is not left in a half-state.
    #[tokio::test]
    async fn v1_wipe_clears_oauth_tokens_and_sync_connected() {
        let state = make_state();
        // Pre-load OAuth tokens as if Drive was connected.
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, "gdrive_refresh_token", "tok-abc123").unwrap();
            db::set_setting(&conn, "gdrive_root_folder_id", "folder-xyz").unwrap();
            db::set_setting(&conn, "sync_connected", "true").unwrap();
            db::set_setting(&conn, db::WRAPPED_ENCRYPTION_KEY_KEY, "deadbeef").unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Password).unwrap();
        }

        let io = FakeV1KeyringIo::present(b"old-keyring".to_vec());
        let result = wipe_v1_keyring_if_present(&io, &state, &DbSettingCredentialStore).await;

        assert!(
            matches!(&result, Err(e) if e.starts_with("V1_WIPE_COMPLETE_RECONNECT_REQUIRED")),
            "expected V1_WIPE_COMPLETE_RECONNECT_REQUIRED error, got {result:?}"
        );

        let conn = state.lock().unwrap();
        // OAuth tokens must be gone.
        assert!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .is_none(),
            "gdrive_refresh_token must be cleared"
        );
        assert!(
            db::get_setting(&conn, "gdrive_root_folder_id")
                .unwrap()
                .is_none(),
            "gdrive_root_folder_id must be cleared"
        );
        // sync_connected must be gone.
        let sync_connected = db::get_setting(&conn, "sync_connected").unwrap();
        assert!(
            sync_connected.is_none() || sync_connected.as_deref() == Some("false"),
            "sync_connected must be cleared or false after wipe, got {sync_connected:?}"
        );
        // Encryption mode must be unset.
        assert_eq!(
            db::get_encryption_mode(&conn).unwrap(),
            db::EncryptionMode::Unset,
            "encryption_mode must be Unset after wipe"
        );
        // B3/B5 invariant: token cleared ⇒ force-re-pair flag must NOT be set,
        // otherwise the user lands on ForceRePairScreen with no Drive session
        // to re-pair against (no route out from a cold start).
        assert_eq!(
            crate::sync::force_re_pair::get_force_re_pair_status_inner(&conn).unwrap(),
            None,
            "v1_wipe must never clear the Drive token while leaving force-re-pair set"
        );
    }

    // ─── C4: wipe_v1_keyring_if_present — fail-loud on delete error ──────────

    struct FailingDeleteIo;

    #[async_trait::async_trait]
    impl crate::sync::keyring_v2::KeyringV2Io for FailingDeleteIo {
        async fn read_file(
            &self,
            _path: &str,
        ) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
            // Always returns content — V1 keyring is "present".
            Ok(b"keyring-bytes".to_vec())
        }

        async fn write_file(
            &self,
            _path: &str,
            _data: &[u8],
        ) -> Result<(), crate::sync::provider::SyncError> {
            Ok(())
        }

        async fn delete_file(&self, path: &str) -> Result<(), crate::sync::provider::SyncError> {
            Err(crate::sync::provider::SyncError::Io(format!(
                "forced delete error for {path}"
            )))
        }

        async fn list_files(
            &self,
            _prefix: &str,
        ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
            Ok(vec![])
        }
    }

    /// C4: wipe_v1_keyring_if_present must return Err when the cloud delete
    /// fails — previously it swallowed delete errors and returned Ok(false),
    /// leaving the V1 file in place and repeating the loop on every connect.
    #[tokio::test]
    async fn v1_wipe_returns_err_when_delete_fails() {
        let state = make_state();
        let io = FailingDeleteIo;
        let result = wipe_v1_keyring_if_present(&io, &state, &DbSettingCredentialStore).await;
        assert!(
            matches!(&result, Err(e) if e.contains("v1_wipe: failed to delete")),
            "expected a v1_wipe delete error, got {result:?}"
        );
    }

    // ─── resolve_unset_outcome_and_stash ────────────────────────────────────

    /// Helper: build a minimal GDriveSession for tests.
    fn make_gdrive_session(tag: &str) -> GDriveSession {
        GDriveSession {
            access_token: format!("at_{tag}"),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            refresh_token: zeroize::Zeroizing::new(format!("rt_{tag}")),
            client_id: format!("cid_{tag}"),
            client_secret: Some(format!("cs_{tag}")),
        }
    }

    /// Test 1: NeedsOnboarding for Unset — session is stashed, DB untouched.
    ///
    /// Simulates: cloud keyring present (V2 vault exists).
    /// The Unset path must NOT call persist_connect_settings, so the DB
    /// remains clean. The pending map must hold the stashed session.
    #[test]
    fn needs_onboarding_for_unset_stashes_session_and_does_not_persist() {
        use crate::sync::pending_session;
        let _serial = pending_session::tests::serial();
        pending_session::reset();

        let state = make_state();
        // Ensure encryption_mode is Unset (default after migration, but explicit).
        {
            let conn = state.lock().unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Unset).unwrap();
        }

        let session = make_gdrive_session("onboard");
        let session_id = "test-onboard-sid";

        let outcome = resolve_unset_outcome_and_stash(
            session_id,
            crate::sync::pending_session::PendingCloudSession::GDrive {
                session,
                root_folder_id: "root-folder-id".to_string(),
            },
            true, // cloud keyring present → NeedsOnboarding
        );

        // Outcome must be NeedsOnboarding.
        assert!(
            matches!(outcome, GdriveConnectOutcome::NeedsOnboarding),
            "expected NeedsOnboarding, got {outcome:?}"
        );

        // DB must have no sync_connected or gdrive_refresh_token.
        let conn = state.lock().unwrap();
        let sync_connected = db::get_setting(&conn, "sync_connected").unwrap();
        assert!(
            sync_connected.is_none() || sync_connected.as_deref() == Some("false"),
            "sync_connected must not be set, got {sync_connected:?}"
        );
        let token = db::get_setting(&conn, "gdrive_refresh_token").unwrap();
        assert!(token.is_none(), "gdrive_refresh_token must not be set");

        // Pending map must hold the stashed session.
        let pending = pending_session::consume(session_id);
        assert!(pending.is_some(), "pending session must be stashed");
        let pending = pending.unwrap();
        match pending {
            crate::sync::pending_session::PendingCloudSession::GDrive {
                session,
                root_folder_id,
            } => {
                assert_eq!(root_folder_id, "root-folder-id");
                assert_eq!(session.access_token, "at_onboard");
            }
            crate::sync::pending_session::PendingCloudSession::Folder { .. } => {
                panic!("expected GDrive pending session")
            }
        }
    }

    /// Test 2: NeedsFirstTimeSetup for Unset — session is stashed, DB untouched.
    ///
    /// Simulates: cloud is empty (no keyring).
    #[test]
    fn needs_first_time_setup_for_unset_stashes_session_and_does_not_persist() {
        use crate::sync::pending_session;
        let _serial = pending_session::tests::serial();
        pending_session::reset();

        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Unset).unwrap();
        }

        let session = make_gdrive_session("setup");
        let session_id = "test-setup-sid";

        let outcome = resolve_unset_outcome_and_stash(
            session_id,
            crate::sync::pending_session::PendingCloudSession::GDrive {
                session,
                root_folder_id: "root-folder-setup".to_string(),
            },
            false, // no cloud keyring → NeedsFirstTimeSetup
        );

        assert!(
            matches!(outcome, GdriveConnectOutcome::NeedsFirstTimeSetup),
            "expected NeedsFirstTimeSetup, got {outcome:?}"
        );

        // DB must remain clean.
        let conn = state.lock().unwrap();
        let sync_connected = db::get_setting(&conn, "sync_connected").unwrap();
        assert!(
            sync_connected.is_none() || sync_connected.as_deref() == Some("false"),
            "sync_connected must not be set"
        );
        assert!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .is_none(),
            "gdrive_refresh_token must not be set"
        );

        // Pending map must hold the session.
        let pending = pending_session::consume(session_id);
        assert!(
            pending.is_some(),
            "pending session must be stashed for NeedsFirstTimeSetup"
        );
        match pending.unwrap() {
            crate::sync::pending_session::PendingCloudSession::GDrive {
                root_folder_id, ..
            } => assert_eq!(root_folder_id, "root-folder-setup"),
            crate::sync::pending_session::PendingCloudSession::Folder { .. } => {
                panic!("expected GDrive pending session")
            }
        }
    }

    /// Test 4: V1WipedReconnectRequired for Unset — nothing stashed, DB untouched.
    ///
    /// The V1 wipe path in `gdrive_complete_connect` returns early before the
    /// Unset branch — wipe_v1_keyring_if_present already covers this. This
    /// test confirms the pending map stays empty for that outcome by calling
    /// the wipe helper directly and asserting `consume` returns None.
    #[tokio::test]
    async fn v1_wiped_for_unset_does_not_stash_or_persist() {
        use crate::sync::pending_session;
        let _serial = pending_session::tests::serial();
        pending_session::reset();

        let state = make_state();
        {
            let conn = state.lock().unwrap();
            // Start in Unset mode (welcome screen).
            db::set_encryption_mode(&conn, db::EncryptionMode::Unset).unwrap();
        }

        let session_id = "test-v1wipe-sid";

        // Simulate a V1 keyring being present — wipe returns the terminal error.
        let io = FakeV1KeyringIo::present(b"v1-keyring".to_vec());
        let result = wipe_v1_keyring_if_present(&io, &state, &DbSettingCredentialStore).await;
        assert!(
            matches!(&result, Err(e) if e.starts_with("V1_WIPE_COMPLETE_RECONNECT_REQUIRED")),
            "expected V1_WIPE_COMPLETE_RECONNECT_REQUIRED, got {result:?}"
        );

        // The wipe path never calls resolve_unset_outcome_and_stash, so the
        // pending map must remain empty.
        assert!(
            pending_session::consume(session_id).is_none(),
            "pending map must be empty after V1 wipe"
        );

        // DB must not have sync_connected or gdrive_refresh_token
        // (persist_connect_settings was gated on !is_unset_mode).
        let conn = state.lock().unwrap();
        assert!(
            db::get_setting(&conn, "sync_connected").unwrap().is_none(),
            "sync_connected must not be set after V1 wipe in Unset mode"
        );
        assert!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .is_none(),
            "gdrive_refresh_token must not be set after V1 wipe in Unset mode"
        );
    }

    /// Test 5 (regression): Password mode persist_connect_settings is unchanged.
    ///
    /// This existing test (`persist_connect_settings_enables_sync`) already
    /// covers the password-mode path. This test is added as an explicit
    /// regression marker to confirm the DB is written for Password mode.
    #[test]
    fn password_mode_connect_still_persists_immediately() {
        // This test mirrors `persist_connect_settings_enables_sync` to
        // explicitly document the regression expectation for the gated block.
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Password).unwrap();
        }

        // Simulate the persist block (is_unset_mode = false → block runs).
        {
            let conn = state.lock().unwrap();
            persist_connect_settings(
                &conn,
                &gdrive_pending_token("fake-token-password"),
                &DbSettingCredentialStore,
            )
            .unwrap();
        }

        let conn = state.lock().unwrap();
        assert_eq!(
            db::get_setting(&conn, "sync_connected").unwrap().as_deref(),
            Some("true"),
            "sync_connected must be true after password-mode connect"
        );
        assert_eq!(
            db::get_setting(&conn, "gdrive_refresh_token")
                .unwrap()
                .as_deref(),
            Some("fake-token-password"),
            "gdrive_refresh_token must be persisted for password mode"
        );
    }

    // ─── reset_local_sync_state tests ────────────────────────────────────────

    /// Helper: insert a sync_state row with given status.
    fn insert_sync_state(conn: &Connection, entry_id: &str, status: &str) {
        conn.execute(
            "INSERT INTO sync_state (entry_id, local_version, sync_status) VALUES (?1, 1, ?2)",
            rusqlite::params![entry_id, status],
        )
        .unwrap();
    }

    /// Helper: upsert a journal_sync_state row with given status.
    /// Uses INSERT OR REPLACE so it works even when migrate() already seeded
    /// a pending row for the default journal.
    fn insert_journal_sync_state(conn: &Connection, journal_id: &str, status: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO journal_sync_state (journal_id, local_version, sync_status) VALUES (?1, 1, ?2)",
            rusqlite::params![journal_id, status],
        )
        .unwrap();
    }

    /// Helper: insert a media row with given upload_status.
    fn insert_media(conn: &Connection, media_id: &str, entry_id: &str, upload_status: &str) {
        conn.execute(
            "INSERT INTO media (id, entry_id, file_name, file_type, storage_provider, storage_path, upload_status, sort_order, created_at) \
             VALUES (?1, ?2, 'test.jpg', 'image', 'local', '/tmp/test.jpg', ?3, 0, 0)",
            rusqlite::params![media_id, entry_id, upload_status],
        )
        .unwrap();
    }

    /// Test 1 (happy path): synced rows flip to pending, pending rows stay
    /// pending, and returned counts reflect only what was actually flipped.
    #[test]
    fn reset_flips_synced_to_pending_and_returns_correct_counts() {
        let state = make_state();
        let conn = state.lock().unwrap();

        // Get journal id from the default journal seeded by migrate.
        let jid: String = conn
            .query_row("SELECT id FROM journals LIMIT 1", [], |r| r.get(0))
            .unwrap();

        // Entries: 2 synced + 1 pending.
        let e1 = db::create_entry(
            &conn,
            db::CreateEntryParams {
                journal_id: &jid,
                title: None,
                content_text: None,
                preview_text: None,
                entry_date: 1_700_000_001,
            },
        )
        .unwrap();
        let e2 = db::create_entry(
            &conn,
            db::CreateEntryParams {
                journal_id: &jid,
                title: None,
                content_text: None,
                preview_text: None,
                entry_date: 1_700_000_002,
            },
        )
        .unwrap();
        let e3 = db::create_entry(
            &conn,
            db::CreateEntryParams {
                journal_id: &jid,
                title: None,
                content_text: None,
                preview_text: None,
                entry_date: 1_700_000_003,
            },
        )
        .unwrap();
        insert_sync_state(&conn, &e1.id, "synced");
        insert_sync_state(&conn, &e2.id, "synced");
        insert_sync_state(&conn, &e3.id, "pending");

        // Journals: 1 synced + 1 pending.
        insert_journal_sync_state(&conn, &jid, "synced");
        // A second fake journal id for the pending row.
        let fake_journal_id = "00000000-0000-0000-0000-000000000001";
        conn.execute(
            "INSERT INTO journals (id, name, color, sort_order, created_at, updated_at) VALUES (?1, 'J2', '#ff0000', 1, 0, 0)",
            rusqlite::params![fake_journal_id],
        )
        .unwrap();
        insert_journal_sync_state(&conn, fake_journal_id, "pending");

        // Media: 2 uploaded + 1 pending.
        insert_media(&conn, "m1", &e1.id, "uploaded");
        insert_media(&conn, "m2", &e2.id, "uploaded");
        insert_media(&conn, "m3", &e3.id, "pending");

        let counts = reset_local_sync_state(&conn).unwrap();

        assert_eq!(counts.entries, 2, "should flip 2 synced entries");
        assert_eq!(counts.journals, 1, "should flip 1 synced journal");
        assert_eq!(counts.media, 2, "should flip 2 uploaded media");

        // Verify all are now pending.
        let pending_entries: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sync_state WHERE sync_status='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending_entries, 3, "all 3 entry rows should be pending");

        let pending_media: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media WHERE upload_status='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending_media, 3, "all 3 media rows should be pending");
    }

    /// Test 2 (conflict preservation): a sync_state row with status='conflict'
    /// must NOT be flipped by reset — it needs user resolution.
    #[test]
    fn reset_leaves_conflict_rows_untouched() {
        let state = make_state();
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
                entry_date: 1_700_000_010,
            },
        )
        .unwrap();
        insert_sync_state(&conn, &entry.id, "conflict");

        let counts = reset_local_sync_state(&conn).unwrap();

        // Conflict entry must not be counted as flipped.
        assert_eq!(counts.entries, 0, "conflict row must not be counted");

        // Status must still be conflict.
        let status: String = conn
            .query_row(
                "SELECT sync_status FROM sync_state WHERE entry_id = ?1",
                rusqlite::params![entry.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "conflict", "conflict row must remain conflict");
    }

    /// Test 3 (error media preservation): a media row with upload_status='error'
    /// must NOT be flipped by reset — those errors need user resolution.
    #[test]
    fn reset_leaves_error_media_untouched() {
        let state = make_state();
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
                entry_date: 1_700_000_020,
            },
        )
        .unwrap();
        insert_media(&conn, "me1", &entry.id, "error");

        let counts = reset_local_sync_state(&conn).unwrap();

        assert_eq!(counts.media, 0, "error media must not be counted");

        let status: String = conn
            .query_row(
                "SELECT upload_status FROM media WHERE id = 'me1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "error", "error media must remain error");
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

    /// Every caller is user-triggered and the cloud-wipe flows use it, so a
    /// downloaded peer original (maybe the last copy) must re-upload rather
    /// than stay `'uploaded'` and evictable. Originless rows stay put.
    #[test]
    fn reset_local_sync_state_requeues_downloaded_peer_originals() {
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
        assert!(
            db::list_evictable_media(&conn)
                .unwrap()
                .iter()
                .any(|m| m.id == "downloaded"),
            "seed: the downloaded original starts evictable"
        );

        let counts = reset_local_sync_state(&conn).unwrap();

        assert_eq!(counts.media, 2);
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
        let evictable = db::list_evictable_media(&conn).unwrap();
        assert!(
            !evictable.iter().any(|m| m.id == "downloaded"),
            "a requeued downloaded original must not be evictable"
        );
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

    /// After a local reset (manual cloud wipe / wipe-cloud), every surface
    /// hash must be cleared so the next push re-uploads all 8 surfaces.
    #[test]
    fn reset_local_sync_state_clears_all_surface_push_hashes() {
        let state = make_state();
        let conn = state.lock().unwrap();
        seed_all_surface_push_hashes(&conn);

        reset_local_sync_state(&conn).unwrap();

        assert_all_surface_push_hashes_cleared(&conn);
    }
}
