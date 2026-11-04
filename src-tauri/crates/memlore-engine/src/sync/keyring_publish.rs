//! Keyring publish and re-upload helpers: write the V2 keyring
//! (`_recovery.json`, `_content.json`, `devices/<id>.json`, `_meta.json`)
//! from local key material, and track the `keyring_dirty` retry flag.
//!
//! Helpers that build the configured provider themselves take a
//! [`CloudCredentialStore`]; the others take a ready provider.
use super::cloud_config::configured_cloud_provider;
use super::cloud_config::CloudCredentialStore;
use super::force_re_pair::force_re_pair_required;
use crate::db;
use crate::vault::setup::V2KeyringPublishContext;
use crate::AppState;
use crate::EncryptionKeyState;

/// Generation written into keyring `_meta.json` and the mutation fence.
///
/// Connect may observe `control.recovery_generation = N` while the local DB is
/// still `0`. Prefer an explicit connect-time candidate over the stale local
/// value so delayed publish never plants `_meta.json.recovery_generation=0`
/// against a higher control generation.
fn resolve_keyring_publish_generation(
    local_generation: u64,
    override_generation: Option<u64>,
) -> u64 {
    override_generation.unwrap_or(local_generation)
}

// ─── Keyring publish helpers ─────────────────────────────────────────────────

/// Extract the v1 content key's `wrapped_content` and `content_fingerprint`
/// from local DB settings so they can be included in `_content.json`.
///
/// `WRAPPED_CONTENT_LIST` stores the compact epoch-list:
/// `[(epoch_u32_be_4bytes)(aes_gcm_60bytes)…]` hex-encoded as a single string.
/// For epoch 1 the first 128 hex chars are: 8 hex (epoch=1) + 120 hex (blob).
/// The 120-hex blob is exactly `AES-GCM(master, v1)` — what `_content.json`
/// calls `wrapped_content`.
///
/// `CLOUD_CONTENT_FINGERPRINT` already holds `key_fingerprint(derive_sync_key(v1))`
/// which equals `content_fingerprint` for epoch 1.
#[doc(hidden)]
pub fn extract_v1_content_for_publish(
    conn: &rusqlite::Connection,
) -> Result<(String, String), String> {
    // Each slot in the compact list is 128 hex chars (8 epoch + 120 wrapped).
    const SLOT_HEX: usize = 128;
    const EPOCH_HEX: usize = 8;

    let list_hex = db::get_setting(conn, db::WRAPPED_CONTENT_LIST)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "WRAPPED_CONTENT_LIST absent — cannot publish _content.json".to_string())?;

    if list_hex.len() < SLOT_HEX {
        return Err(format!(
            "WRAPPED_CONTENT_LIST too short ({} hex chars, expected >= {SLOT_HEX})",
            list_hex.len()
        ));
    }

    // The list is ordered by ascending epoch. Slot 0 = epoch 1 for a freshly
    // onboarded vault that has never done a content-key rotation.
    let slot0 = &list_hex[..SLOT_HEX];
    let wrapped_content_v1 = slot0[EPOCH_HEX..].to_owned(); // 120 hex chars

    let content_fingerprint_v1 = db::get_setting(conn, db::CLOUD_CONTENT_FINGERPRINT)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "CLOUD_CONTENT_FINGERPRINT absent — cannot publish _content.json".to_string()
        })?;

    Ok((wrapped_content_v1, content_fingerprint_v1))
}

// ─── Keyring re-upload stubs (TODO Phase 3) ──────────────────────────────────

/// Attempt to re-upload the V2 keyring to Drive using the stored session.
/// Best-effort — callers handle `Err` by setting `keyring_dirty`.
///
/// Reads all necessary key material from local settings, reconstructs the Drive
/// session, and publishes the V2 keyring (`_recovery.json`, `devices/<id>.json`,
/// `_meta.json`). Returns `Err` when Drive is not connected or key material is
/// absent — callers MUST NOT clear `keyring_dirty` on error.
#[doc(hidden)]
pub async fn try_reupload_keyring(
    state: &AppState,
    key_state: &EncryptionKeyState,
    creds: &dyn CloudCredentialStore,
) -> Result<(), String> {
    // Read all key material and device info from local settings.
    let ctx = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        let devices = db::list_devices(&conn).map_err(|e| e.to_string())?;
        let device = devices
            .into_iter()
            .find(|d| d.is_current)
            .ok_or_else(|| "no current device".to_string())?;
        let recovery_wrapped = db::get_setting(&conn, db::RECOVERY_WRAPPED_MASTER_KEY)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no recovery_wrapped_master".to_string())?;
        let master_fingerprint = db::get_setting(&conn, db::CLOUD_MASTER_FINGERPRINT)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no cloud_master_fingerprint".to_string())?;
        let created_at = db::get_setting(&conn, db::RECOVERY_PASSPHRASE_CREATED_AT)
            .map_err(|e| e.to_string())?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let (wrapped_content_v1, content_fingerprint_v1) = extract_v1_content_for_publish(&conn)?;
        V2KeyringPublishContext {
            recovery_wrapped,
            device_id: device.device_id,
            device_name: device.name,
            master_fingerprint,
            created_at,
            device_created_at: device.created_at,
            last_seen_at: device.last_seen_at,
            wrapped_content_v1,
            content_fingerprint_v1,
        }
    };

    let provider = {
        let conn = state.lock()?;
        configured_cloud_provider(&conn, creds)?
            .ok_or_else(|| "No cloud provider connected".to_string())?
    };

    publish_v2_keyring_with_provider(&provider, state, &ctx, key_state).await
}

/// Re-upload the V2 keyring using a pre-built provider.
///
/// Reads key material from local settings and calls [`publish_v2_keyring_with_provider`].
/// Uploads all three keyring files (meta, recovery, device slot). Used by Phase 5 rotation;
/// for rename-only use `reupload_current_device_slot` which preserves epoch and recovery.
/// Returns `Err` when key material is absent — callers set `keyring_dirty` on error.
#[allow(dead_code)] // Phase 5 rotation will use this for full keyring republish.
#[doc(hidden)]
pub async fn try_reupload_keyring_with_provider<P: crate::sync::keyring_v2::KeyringV2Io>(
    state: &AppState,
    provider: &P,
    key_state: &EncryptionKeyState,
) -> Result<(), String> {
    let ctx = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        let devices = db::list_devices(&conn).map_err(|e| e.to_string())?;
        let device = devices
            .into_iter()
            .find(|d| d.is_current)
            .ok_or_else(|| "no current device".to_string())?;
        let recovery_wrapped = db::get_setting(&conn, db::RECOVERY_WRAPPED_MASTER_KEY)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no recovery_wrapped_master".to_string())?;
        let master_fingerprint = db::get_setting(&conn, db::CLOUD_MASTER_FINGERPRINT)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no cloud_master_fingerprint".to_string())?;
        let created_at = db::get_setting(&conn, db::RECOVERY_PASSPHRASE_CREATED_AT)
            .map_err(|e| e.to_string())?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let (wrapped_content_v1, content_fingerprint_v1) = extract_v1_content_for_publish(&conn)?;
        V2KeyringPublishContext {
            recovery_wrapped,
            device_id: device.device_id,
            device_name: device.name,
            master_fingerprint,
            created_at,
            device_created_at: device.created_at,
            last_seen_at: device.last_seen_at,
            wrapped_content_v1,
            content_fingerprint_v1,
        }
    };

    publish_v2_keyring_with_provider(provider, state, &ctx, key_state).await
}

/// Write ONLY `devices/<device_id>.json` for the current device after a rename.
///
/// Unlike [`try_reupload_keyring_with_provider`] this does NOT touch
/// `_meta.json` (epoch preserved) or `_recovery.json` (unchanged by rename).
/// It preserves `created_at` from the devices table and sets `last_seen_at` to
/// the provided value (caller passes `now`).
#[doc(hidden)]
pub async fn reupload_current_device_slot<P: crate::sync::keyring_v2::KeyringV2Io>(
    state: &AppState,
    provider: &P,
    device_id: &str,
    name: &str,
    created_at: i64,
    last_seen_at: i64,
) -> Result<(), String> {
    use crate::sync::keyring_v2::{
        io::write_device_slot,
        types::{DeviceSlotV2, KEYRING_V2_VERSION},
    };

    // Same primitive-level gate as `publish_v2_keyring_with_provider`: a flagged
    // device must not write ANY keyring file from local truth — recreating a
    // device slot that a peer's rotation removed re-opens the clobber hole. The
    // gate lives here rather than at the callers (`rename_device`, the
    // device-slot-dirty flush, the last-seen republish) so a new caller cannot
    // silently bypass it. Callers' error paths set `device_slot_dirty`, so the
    // upload is retried automatically after the user re-pairs.
    let local_generation = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        if force_re_pair_required(&conn) {
            return Err(
                "reupload_current_device_slot: force-re-pair required — refusing to \
                 write a device slot from unverified local state"
                    .to_string(),
            );
        }
        db::get_sync_recovery_generation(&conn).map_err(|e| e.to_string())?
    };
    provider.configure_recovery_fence(local_generation, None);

    let slot = DeviceSlotV2 {
        version: KEYRING_V2_VERSION,
        device_id: device_id.to_string(),
        name: name.to_string(),
        created_at,
        last_seen_at,
    };
    slot.validate()
        .map_err(|e| format!("reupload_current_device_slot: invalid slot: {e}"))?;
    write_device_slot(provider, &slot)
        .await
        .map_err(|e| format!("reupload_current_device_slot: write failed: {e}"))
}

/// Update `keyring_dirty` based on a completed upload attempt.
/// Clears the flag on success; sets it on failure. Best-effort — never panics.
#[doc(hidden)]
pub fn apply_dirty_flag_result(result: Result<(), String>, state: &AppState) {
    match result {
        Ok(()) => {
            if let Ok(c) = state.lock() {
                if let Err(e) = db::delete_setting(&c, db::KEYRING_DIRTY_KEY) {
                    log::warn!("keyring: failed to clear dirty flag: {e}");
                }
            }
            log::info!("keyring: re-upload succeeded, dirty flag cleared");
        }
        Err(e) => {
            log::warn!("keyring: re-upload failed ({e}), setting keyring_dirty for retry");
            if let Ok(c) = state.lock() {
                if let Err(e2) = db::set_setting(&c, db::KEYRING_DIRTY_KEY, "1") {
                    log::warn!("keyring: failed to set dirty flag: {e2}");
                }
            }
        }
    }
}

/// Upload keyring via the live Drive session and update `keyring_dirty`.
/// Called by `change_password` and `run_sync_now` — both need the real provider.
#[doc(hidden)]
pub async fn try_reupload_keyring_and_update_dirty_flag(
    state: &AppState,
    key_state: &EncryptionKeyState,
    creds: &dyn CloudCredentialStore,
) {
    let result = try_reupload_keyring(state, key_state, creds).await;
    apply_dirty_flag_result(result, state);
}

/// Build V2 keyring structs from a [`V2KeyringPublishContext`] and publish
/// them via the given provider in crash-safe order
/// (`_recovery.json` → `devices/<id>.json` → `_meta.json`).
///
/// C2: Refuses if an active rotation job is present — between PRAGMA rekey and
/// the settings transaction, settings still point to K_old.  Publishing then
/// would push a stale device slot to the cloud.  When refused, the caller's
/// `keyring_dirty` flag stays set so the publish is automatically retried after
/// the rotation completes.
///
/// F1 fix: When `key_state` is initialized with the full content-key list the
/// function reads `WRAPPED_CONTENT_LIST` and `CONTENT_KEY_EPOCH` from the DB and
/// publishes **all** epochs to `_content.json`.  This prevents a password-change
/// or keyring-dirty re-upload from rolling back the cloud content-key history to
/// epoch 1 after a rotation.  When `key_state` is not yet initialized (initial
/// first-time-setup path, or tests that create a fresh state), it falls back to
/// the epoch-1 data carried in `ctx`.
///
/// Extracted so tests can exercise the full publish path with an
/// [`InMemoryKeyringProvider`] without needing a real GDrive session.
#[doc(hidden)]
pub async fn publish_v2_keyring_with_provider<P>(
    provider: &P,
    state: &AppState,
    ctx: &V2KeyringPublishContext,
    key_state: &EncryptionKeyState,
) -> Result<(), String>
where
    P: crate::sync::keyring_v2::KeyringV2Io,
{
    publish_v2_keyring_with_provider_for_generation(provider, state, ctx, key_state, None).await
}

/// Like [`publish_v2_keyring_with_provider`], but may pin recovery generation.
///
/// Pass `Some(candidate)` during connect delayed-publish when the cloud control
/// generation is already N but the local DB has not adopted N yet. Without
/// this, `_meta.json` would be planted at local `0` and every subsequent
/// joiner / force-re-pair would see `RECOVERY_GENERATION_MISMATCH`.
#[doc(hidden)]
pub async fn publish_v2_keyring_with_provider_for_generation<P>(
    provider: &P,
    state: &AppState,
    ctx: &V2KeyringPublishContext,
    key_state: &EncryptionKeyState,
    recovery_generation_override: Option<u64>,
) -> Result<(), String>
where
    P: crate::sync::keyring_v2::KeyringV2Io,
{
    let local_generation = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        db::get_sync_recovery_generation(&conn).map_err(|e| e.to_string())?
    };
    let publish_generation =
        resolve_keyring_publish_generation(local_generation, recovery_generation_override);
    provider.configure_recovery_fence(publish_generation, None);

    // C2: Do not publish while a rotation is in progress.
    // Also refuse when force-re-pair is required: this device's key material is
    // not known to match the cloud vault, and every write below comes from LOCAL
    // truth with no epoch/CAS precondition — publishing would clobber a peer's
    // rotation and roll `_meta.json` back to this device's stale epoch.
    //
    // The gate lives here, in the primitive, rather than at the callers: the
    // caller set (`change_password`, `rename_device`, the keyring-dirty retry,
    // the "no cloud keyring" reconnect branch) is easy to extend and each new
    // caller would otherwise silently reopen the hole.
    //
    // `complete_force_re_pair` is unaffected — it publishes via
    // `onboard_complete_inner` → `kio::write_meta`, not through this primitive —
    // so the legitimate recovery path is not gated by the flag it clears.
    {
        let conn = state.lock().map_err(|e| e.to_string())?;
        if db::has_active_rotation_job(&conn).map_err(|e| e.to_string())? {
            log::warn!(
                "publish_v2_keyring_with_provider: rotation active, refusing to publish device slot"
            );
            return Err("Cannot publish keyring while a rotation is in progress. \
                 Complete or resume the rotation first."
                .to_string());
        }
        if force_re_pair_required(&conn) {
            log::warn!(
                "publish_v2_keyring_with_provider: force-re-pair required, refusing to publish \
                 — this device must not overwrite the cloud keyring from stale local state"
            );
            return Err("Cannot publish keyring while re-pair is required. \
                 Re-pair this device first."
                .to_string());
        }
    }

    use crate::sync::keyring_v2::{
        builder::publish_initial_v2_keyring,
        io::write_content,
        types::{
            ContentEntryV2, ContentListV2, DeviceSlotV2, KeyringMetaV2, RecoverySlotV2,
            KEYRING_V2_VERSION,
        },
    };
    use crate::utils::encryption::key_fingerprint;

    let recovery_slot = RecoverySlotV2 {
        version: KEYRING_V2_VERSION,
        wrapped_master: ctx.recovery_wrapped.clone(),
        created_at: ctx.created_at,
    };

    let device_slot = DeviceSlotV2 {
        version: KEYRING_V2_VERSION,
        device_id: ctx.device_id.clone(),
        name: ctx.device_name.clone(),
        created_at: ctx.device_created_at,
        last_seen_at: ctx.last_seen_at,
    };

    // Build the content list.  When key_state is fully initialized, read the
    // complete WRAPPED_CONTENT_LIST from the DB so all epochs survive the
    // re-upload.  When not (initial setup / tests with fresh state), fall back
    // to the epoch-1 data in ctx — the DB may not yet hold WRAPPED_CONTENT_LIST.
    const SLOT_HEX: usize = 128;
    const EPOCH_HEX: usize = 8;

    let (content_list, content_epoch) = if key_state.is_initialized().unwrap_or(false) {
        // Full path: read every slot from the DB, compute per-epoch fingerprints.
        let (list_hex, db_content_epoch) = {
            let conn = state.lock().map_err(|e| e.to_string())?;
            let hex = db::get_setting(&conn, db::WRAPPED_CONTENT_LIST)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty());
            let epoch = db::get_setting(&conn, db::CONTENT_KEY_EPOCH)
                .map_err(|e| e.to_string())?
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1);
            (hex, epoch)
        };

        match list_hex {
            Some(hex) if hex.len() >= SLOT_HEX && hex.len() % SLOT_HEX == 0 => {
                let n_slots = hex.len() / SLOT_HEX;
                let mut entries = Vec::with_capacity(n_slots);
                for i in 0..n_slots {
                    let slot = &hex[i * SLOT_HEX..(i + 1) * SLOT_HEX];
                    let epoch_bytes = hex::decode(&slot[..EPOCH_HEX]).map_err(|e| {
                        format!("publish_v2_keyring: bad epoch hex in slot {i}: {e}")
                    })?;
                    if epoch_bytes.len() != 4 {
                        return Err(format!(
                            "publish_v2_keyring: epoch bytes wrong length in slot {i}"
                        ));
                    }
                    let epoch = u32::from_be_bytes([
                        epoch_bytes[0],
                        epoch_bytes[1],
                        epoch_bytes[2],
                        epoch_bytes[3],
                    ]);
                    let wrapped_hex = slot[EPOCH_HEX..].to_owned();
                    // Fingerprint: key_fingerprint(derive_sync_key(content_key)) —
                    // matching the reader formula used everywhere else.
                    let fp_hex = key_state
                        .with_sync_key_for_epoch(epoch, |k| Ok(hex::encode(key_fingerprint(k))))
                        .map_err(|e| {
                            format!(
                                "publish_v2_keyring: cannot compute fingerprint for epoch {epoch}: {e}"
                            )
                        })?;
                    entries.push(ContentEntryV2 {
                        epoch,
                        wrapped_content: wrapped_hex,
                        content_fingerprint: fp_hex,
                    });
                }
                let latest_epoch = entries.last().map(|e| e.epoch).unwrap_or(db_content_epoch);
                let cl = ContentListV2 {
                    version: KEYRING_V2_VERSION,
                    latest_epoch,
                    entries,
                    created_at: ctx.created_at,
                };
                (cl, db_content_epoch)
            }
            _ => {
                // DB has no list yet — fall back to epoch-1 from ctx.
                log::debug!(
                    "publish_v2_keyring_with_provider: WRAPPED_CONTENT_LIST absent/invalid, \
                     using ctx epoch-1 fallback"
                );
                let cl = ContentListV2 {
                    version: KEYRING_V2_VERSION,
                    latest_epoch: 1,
                    entries: vec![ContentEntryV2 {
                        epoch: 1,
                        wrapped_content: ctx.wrapped_content_v1.clone(),
                        content_fingerprint: ctx.content_fingerprint_v1.clone(),
                    }],
                    created_at: ctx.created_at,
                };
                (cl, 1u32)
            }
        }
    } else {
        // key_state not initialized (initial setup or no-encryption mode): use
        // the epoch-1 data from ctx.
        let cl = ContentListV2 {
            version: KEYRING_V2_VERSION,
            latest_epoch: 1,
            entries: vec![ContentEntryV2 {
                epoch: 1,
                wrapped_content: ctx.wrapped_content_v1.clone(),
                content_fingerprint: ctx.content_fingerprint_v1.clone(),
            }],
            created_at: ctx.created_at,
        };
        (cl, 1u32)
    };

    // Read the master-key epoch (rotation counter) from local settings for
    // _meta.json.  Default to 1 when absent (fresh vault / initial setup).
    // Recovery generation uses `publish_generation` (override or local) so a
    // connect-time candidate is never clobbered by stale local 0.
    let master_epoch: u64 = {
        let conn = state.lock().map_err(|e| e.to_string())?;
        db::get_setting(&conn, db::KEYRING_V2_LOCAL_EPOCH)
            .map_err(|e| e.to_string())?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1)
    };
    let recovery_generation = publish_generation;

    let meta = KeyringMetaV2 {
        version: KEYRING_V2_VERSION,
        epoch: master_epoch,
        master_fingerprint: ctx.master_fingerprint.clone(),
        content_epoch,
        recovery_generation,
        created_at: ctx.created_at,
        updated_at: ctx.created_at,
    };

    // I1: write _content.json BEFORE _meta.json so that if we crash between
    // content-write and meta-write the joiner still sees a consistent state:
    // no _meta.json → joiner retries later; _content.json present but no
    // _meta.json → content is orphaned until meta is written.  Writing after
    // _meta.json would mean a joiner that reads _meta.json (content_epoch=1) but
    // finds no _content.json would fall back to v1=master → decryption failure.
    write_content(provider, &content_list)
        .await
        .map_err(|e| format!("publish_v2_keyring: write _content.json failed: {e}"))?;

    publish_initial_v2_keyring(provider, &recovery_slot, &device_slot, &meta)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::sync::cloud_config::GDriveClientConfig;
    use crate::sync::force_re_pair::apply_inconclusive_force_re_pair;
    use crate::sync::force_re_pair::clear_stale_flag_for_empty_cloud;
    use crate::sync::force_re_pair::set_force_re_pair_flags;
    use rusqlite::Connection;

    /// No Drive token: these tests stop before a Drive session is built.
    struct NoCredentials;

    impl CloudCredentialStore for NoCredentials {
        fn client_config(
            &self,
            _conn: &rusqlite::Connection,
        ) -> Result<GDriveClientConfig, String> {
            Err("no Drive client in these tests".to_string())
        }
        fn load_refresh_token(
            &self,
            _conn: &rusqlite::Connection,
        ) -> Result<Option<zeroize::Zeroizing<String>>, String> {
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

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    fn make_publish_ctx() -> V2KeyringPublishContext {
        V2KeyringPublishContext {
            // recovery_wrapped: 120 hex chars (nonce=24 + ct=64 + tag=32)
            recovery_wrapped: "ab".repeat(60),
            device_id: "11111111-2222-3333-4444-555555555555".to_string(),
            device_name: "Test Mac".to_string(),
            // master_fingerprint: 64 hex chars (HMAC-SHA256)
            master_fingerprint: "a1".repeat(32),
            // Three distinct values so the test can prove each landed in the
            // right place: vault created_at (recovery/meta), device join time
            // (device slot created_at), and last_seen_at (device slot).
            created_at: 1_700_000_000,
            device_created_at: 1_700_005_555,
            last_seen_at: 1_700_009_999,
            // wrapped_content_v1: 120 hex chars (AES-GCM(master, v1))
            wrapped_content_v1: "cc".repeat(60),
            // content_fingerprint_v1: 64 hex chars (HMAC-SHA256)
            content_fingerprint_v1: "dd".repeat(32),
        }
    }

    #[test]
    fn delayed_publish_generation_prefers_candidate_over_stale_local_zero() {
        // control=N, local still 0, no meta yet — must plant meta at N, not 0.
        assert_eq!(resolve_keyring_publish_generation(0, Some(5)), 5);
        assert_eq!(resolve_keyring_publish_generation(3, Some(3)), 3);
        // No override → local DB wins (normal publish paths).
        assert_eq!(resolve_keyring_publish_generation(0, None), 0);
        assert_eq!(resolve_keyring_publish_generation(7, None), 7);
    }

    /// I2: publish_v2_keyring_with_provider writes _meta.json, _recovery.json,
    /// and devices/<id>.json to the provider.
    #[tokio::test]
    async fn publish_v2_keyring_with_provider_writes_all_three_files() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::io::{read_device_slot, read_meta, read_recovery};

        let provider = InMemoryKeyringProvider::new();
        let state = make_state(); // no active rotation job — publish should succeed
        {
            let conn = state.lock().unwrap();
            db::set_sync_recovery_generation(&conn, 7).unwrap();
        }
        let ctx = make_publish_ctx();
        let key_state = EncryptionKeyState::new();

        publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state)
            .await
            .expect("publish must succeed");

        // Recovery slot must exist and match.
        let recovery = read_recovery(&provider)
            .await
            .expect("read_recovery failed")
            .expect("_recovery.json must exist");
        assert_eq!(recovery.wrapped_master, ctx.recovery_wrapped);

        // Device slot must exist and match.
        let device = read_device_slot(&provider, &ctx.device_id)
            .await
            .expect("read_device_slot failed")
            .expect("device slot must exist");
        assert_eq!(device.name, ctx.device_name);
        {
            // Read RAW bytes off the provider — asserting via the parsed
            // `DeviceSlotV2` would be meaningless, since that struct cannot
            // carry these fields anymore by construction.
            use crate::sync::keyring_v2::KeyringV2Io;
            let raw = provider
                .read_file(&crate::sync::keyring_v2::io::device_slot_path(
                    &ctx.device_id,
                ))
                .await
                .unwrap();
            let raw_text = String::from_utf8_lossy(&raw);
            assert!(
                !raw_text.contains("wrapped_master") && !raw_text.contains("kek_salt"),
                "device slot must carry no key material: {raw_text}"
            );
        }
        // Device slot created_at must come from device_created_at (the local
        // devices row), NOT the vault created_at — they are distinct concepts.
        assert_eq!(device.created_at, ctx.device_created_at);
        // last_seen_at must come from ctx.last_seen_at, not created_at.
        assert_eq!(device.last_seen_at, ctx.last_seen_at);

        // Recovery slot created_at must be the VAULT created_at.
        assert_eq!(recovery.created_at, ctx.created_at);

        // Meta must exist and match. created_at/updated_at are the vault time.
        let meta = read_meta(&provider)
            .await
            .expect("read_meta failed")
            .expect("_meta.json must exist");
        assert_eq!(meta.epoch, 1);
        assert_eq!(meta.master_fingerprint, ctx.master_fingerprint);
        assert_eq!(meta.recovery_generation, 7);
        assert_eq!(meta.created_at, ctx.created_at);
    }

    /// Delayed connect publish: local DB still at generation 0 while control is
    /// already N. Explicit override must plant `_meta.json.recovery_generation=N`
    /// (not stale local 0).
    #[tokio::test]
    async fn delayed_publish_uses_candidate_generation_not_stale_local_zero() {
        use crate::sync::keyring_v2::io::read_meta;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let provider = InMemoryKeyringProvider::new();
        let state = make_state();
        // Local deliberately left at 0 — the pre-Ready connect state.
        {
            let conn = state.lock().unwrap();
            assert_eq!(db::get_sync_recovery_generation(&conn).unwrap(), 0);
        }
        let ctx = make_publish_ctx();
        let key_state = EncryptionKeyState::new();

        publish_v2_keyring_with_provider_for_generation(
            &provider,
            &state,
            &ctx,
            &key_state,
            Some(5),
        )
        .await
        .expect("publish with candidate generation must succeed");

        let meta = read_meta(&provider)
            .await
            .expect("read_meta failed")
            .expect("_meta.json must exist");
        assert_eq!(
            meta.recovery_generation, 5,
            "delayed publish must use control candidate, not local 0"
        );
        // Local DB must remain unadopted until Ready commits generation.
        {
            let conn = state.lock().unwrap();
            assert_eq!(db::get_sync_recovery_generation(&conn).unwrap(), 0);
        }
    }

    /// Without override, publish still reads generation from local DB.
    #[tokio::test]
    async fn publish_without_override_uses_local_generation() {
        use crate::sync::keyring_v2::io::read_meta;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let provider = InMemoryKeyringProvider::new();
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_recovery_generation(&conn, 2).unwrap();
        }
        let ctx = make_publish_ctx();
        let key_state = EncryptionKeyState::new();

        publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state)
            .await
            .expect("publish must succeed");

        let meta = read_meta(&provider)
            .await
            .expect("read_meta")
            .expect("meta");
        assert_eq!(meta.recovery_generation, 2);
    }

    /// F1: after a key rotation (epoch 1 → 2), a keyring re-upload must publish
    /// BOTH epochs to `_content.json` and set `_meta.json.content_epoch = 2`.
    ///
    /// This is the regression test for F1: when key_state is initialized with the
    /// full content-key list, publish_v2_keyring_with_provider must read
    /// WRAPPED_CONTENT_LIST from the DB (which holds all epochs) rather than
    /// only writing the epoch-1 data from ctx.
    #[tokio::test]
    async fn publish_keyring_after_rotation_writes_all_epochs_to_content_list() {
        use crate::sync::keyring_v2::io::{
            read_content, read_meta, test_support::InMemoryKeyringProvider,
        };
        use crate::utils::encryption::{
            derive_sync_key, encode_content_key_list, key_fingerprint, wrap_content_key,
        };
        use rand::{rngs::OsRng, RngCore};
        use std::collections::BTreeMap;
        use zeroize::Zeroizing;

        // Generate master key + two independent content keys.
        let mut master_raw = [0u8; 32];
        OsRng.fill_bytes(&mut master_raw);
        let master = Zeroizing::new(master_raw);

        let mut ck1_raw = [0u8; 32];
        OsRng.fill_bytes(&mut ck1_raw);
        let ck1 = Zeroizing::new(ck1_raw);

        let mut ck2_raw = [0u8; 32];
        OsRng.fill_bytes(&mut ck2_raw);
        let ck2 = Zeroizing::new(ck2_raw);

        // Build WRAPPED_CONTENT_LIST (compact hex: epoch(8 hex) + wrapped(120 hex) per slot).
        let mut key_map: BTreeMap<u32, Zeroizing<[u8; 32]>> = BTreeMap::new();
        key_map.insert(1, ck1.clone());
        key_map.insert(2, ck2.clone());
        let list_hex = encode_content_key_list(&master, &key_map).expect("encode_content_key_list");

        // Compute expected fingerprints: key_fingerprint(derive_sync_key(ck)).
        let fp1 = hex::encode(key_fingerprint(&*derive_sync_key(&*ck1)));
        let fp2 = hex::encode(key_fingerprint(&*derive_sync_key(&*ck2)));

        // Wrap ck1 for the ctx (epoch-1 fallback path — should NOT be used).
        let wrapped_v1 = hex::encode(wrap_content_key(&master, &ck1).expect("wrap_content_key"));

        // Build ctx (epoch-1 values only — these should be ignored when key_state is set).
        let ctx = V2KeyringPublishContext {
            recovery_wrapped: "ab".repeat(60),
            device_id: "11111111-2222-3333-4444-555555555555".to_string(),
            device_name: "Test Mac".to_string(),
            master_fingerprint: "a1".repeat(32),
            created_at: 1_700_000_000,
            device_created_at: 1_700_005_555,
            last_seen_at: 1_700_009_999,
            wrapped_content_v1: wrapped_v1,
            content_fingerprint_v1: fp1.clone(),
        };

        // Set up state with WRAPPED_CONTENT_LIST (epoch 1+2) in DB.
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::WRAPPED_CONTENT_LIST, &list_hex).unwrap();
            db::set_setting(&conn, db::CONTENT_KEY_EPOCH, "2").unwrap();
            db::upsert_device(
                &conn,
                &crate::db::DeviceRow {
                    device_id: ctx.device_id.clone(),
                    name: ctx.device_name.clone(),
                    created_at: ctx.created_at,
                    last_seen_at: ctx.created_at,
                    is_current: true,
                    is_revoked: false,
                },
            )
            .unwrap();
            db::set_current_device(&conn, &ctx.device_id).unwrap();
        }

        // Initialize key_state with both epochs.
        let key_state = EncryptionKeyState::new();
        key_state
            .set_content_state(key_map, 2, ck2.clone(), master.clone())
            .expect("set_content_state");

        let provider = InMemoryKeyringProvider::new();
        publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state)
            .await
            .expect("publish must succeed");

        // _content.json must have 2 entries.
        let content = read_content(&provider)
            .await
            .expect("read_content failed")
            .expect("_content.json must exist");
        assert_eq!(
            content.entries.len(),
            2,
            "_content.json must contain both epochs after rotation"
        );
        assert_eq!(content.latest_epoch, 2, "latest_epoch must be 2");
        assert_eq!(content.entries[0].epoch, 1, "first entry epoch must be 1");
        assert_eq!(content.entries[1].epoch, 2, "second entry epoch must be 2");
        assert_eq!(
            content.entries[0].content_fingerprint, fp1,
            "epoch-1 fingerprint mismatch"
        );
        assert_eq!(
            content.entries[1].content_fingerprint, fp2,
            "epoch-2 fingerprint mismatch"
        );

        // _meta.json must have content_epoch = 2.
        let meta = read_meta(&provider)
            .await
            .expect("read_meta failed")
            .expect("_meta.json must exist");
        assert_eq!(
            meta.content_epoch, 2,
            "_meta.json.content_epoch must be 2 after rotation"
        );
    }

    // ─── I1: try_reupload_keyring — full implementation tests ────────────────

    fn seed_password_mode_state(state: &AppState, ctx: &V2KeyringPublishContext) {
        let conn = state.lock().unwrap();
        db::set_encryption_mode(&conn, db::EncryptionMode::Password).unwrap();
        // Local password-wrap material — unrelated to the (metadata-only) cloud
        // device slot, so it is seeded directly rather than sourced from ctx.
        db::set_setting(&conn, db::WRAPPED_ENCRYPTION_KEY_KEY, &"cd".repeat(67)).unwrap();
        db::set_setting(&conn, db::KEK_SALT_KEY, &"ef".repeat(16)).unwrap();
        db::set_setting(
            &conn,
            db::RECOVERY_WRAPPED_MASTER_KEY,
            &ctx.recovery_wrapped,
        )
        .unwrap();
        db::set_setting(&conn, db::CLOUD_MASTER_FINGERPRINT, &ctx.master_fingerprint).unwrap();
        db::set_setting(
            &conn,
            db::RECOVERY_PASSPHRASE_CREATED_AT,
            &ctx.created_at.to_string(),
        )
        .unwrap();
        // Seed content-key material needed by extract_v1_content_for_publish.
        // Build a minimal compact list: epoch=1 (8 hex) + wrapped_content (120 hex) = 128 hex.
        let fake_slot = format!("{:08x}{}", 1u32, &ctx.wrapped_content_v1);
        db::set_setting(&conn, db::WRAPPED_CONTENT_LIST, &fake_slot).unwrap();
        db::set_setting(
            &conn,
            db::CLOUD_CONTENT_FINGERPRINT,
            &ctx.content_fingerprint_v1,
        )
        .unwrap();
        db::upsert_device(
            &conn,
            &db::DeviceRow {
                device_id: ctx.device_id.clone(),
                name: ctx.device_name.clone(),
                created_at: ctx.created_at,
                last_seen_at: ctx.created_at,
                is_current: true,
                is_revoked: false,
            },
        )
        .unwrap();
        db::set_current_device(&conn, &ctx.device_id).unwrap();
    }

    /// I1: try_reupload_keyring returns Err (no session) when no cloud
    /// provider is connected, so keyring_dirty is NOT cleared by
    /// apply_dirty_flag_result.
    #[tokio::test]
    async fn try_reupload_keyring_returns_err_when_not_connected() {
        let state = make_state();
        let key_state = EncryptionKeyState::new();
        let ctx = make_publish_ctx();
        seed_password_mode_state(&state, &ctx);
        // No configured provider → configured_cloud_provider returns None.

        let result = try_reupload_keyring(&state, &key_state, &NoCredentials).await;
        assert!(
            result.is_err(),
            "expected Err when no cloud provider is connected, got Ok"
        );
        assert!(
            result.unwrap_err().contains("No cloud provider connected"),
            "error should mention that no cloud provider is connected"
        );
    }

    /// I1: try_reupload_keyring returns Err when recovery_wrapped_master is
    /// absent from settings (incomplete local state).
    #[tokio::test]
    async fn try_reupload_keyring_returns_err_when_key_material_missing() {
        let state = make_state();
        let key_state = EncryptionKeyState::new();
        // Do NOT seed RECOVERY_WRAPPED_MASTER_KEY.
        {
            let conn = state.lock().unwrap();
            db::set_encryption_mode(&conn, db::EncryptionMode::Password).unwrap();
        }

        let result = try_reupload_keyring(&state, &key_state, &NoCredentials).await;
        assert!(result.is_err(), "expected Err when key material is missing");
    }

    // ─── C2: publish gate during active rotation ─────────────────────────────

    fn insert_active_rotation_job(state: &AppState) {
        let conn = state.lock().unwrap();
        db::insert_rotation_job(&conn, "fp-old", "fp-new", 1, 2, None).unwrap();
        // Default state is "enumerate" — that's an active job.
    }

    /// C2: publish_v2_keyring_with_provider refuses when a rotation is active.
    /// This is the central gate — all external publish paths route through it.
    #[tokio::test]
    async fn publish_v2_keyring_refuses_during_active_rotation() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let state = make_state();
        let ctx = make_publish_ctx();
        insert_active_rotation_job(&state);

        let provider = InMemoryKeyringProvider::new();
        let key_state = EncryptionKeyState::new();
        let result = publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state).await;
        assert!(result.is_err(), "expected Err during active rotation");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("rotation is in progress"),
            "error must mention rotation: {msg}"
        );
    }

    /// C2: try_reupload_keyring_with_provider also refuses when a rotation is active
    /// (via the publish_v2_keyring_with_provider gate).
    #[tokio::test]
    async fn try_reupload_keyring_refuses_during_active_rotation() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let state = make_state();
        let ctx = make_publish_ctx();
        seed_password_mode_state(&state, &ctx);
        insert_active_rotation_job(&state);

        let provider = InMemoryKeyringProvider::new();
        let key_state = EncryptionKeyState::new();
        let result = try_reupload_keyring_with_provider(&state, &provider, &key_state).await;
        assert!(result.is_err(), "expected Err during active rotation");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("rotation is in progress"),
            "error must mention rotation: {msg}"
        );
    }

    // ─── A3: reupload_current_device_slot gate ───────────────────────────────

    /// A flagged device must not recreate a device slot that a peer's rotation
    /// removed — the gate lives in the primitive so no caller can bypass it.
    #[tokio::test]
    async fn reupload_current_device_slot_refuses_when_force_re_pair_required() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::KeyringV2Io;

        let state = make_state();
        {
            let conn = state.lock().unwrap();
            set_force_re_pair_flags(&conn, "vault_rotated").unwrap();
        }
        let provider = InMemoryKeyringProvider::new();
        let err = reupload_current_device_slot(
            &state,
            &provider,
            "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
            "Stale Device",
            1_700_000_000,
            1_700_000_001,
        )
        .await
        .expect_err("a flagged device must not write its device slot");
        assert!(err.contains("force-re-pair"), "got: {err}");
        assert!(
            provider
                .list_files(".meta/keyring")
                .await
                .unwrap()
                .is_empty(),
            "nothing may reach the cloud while the flag stands"
        );
    }

    /// A flagged device must refuse to publish its keyring at the primitive.
    #[tokio::test]
    async fn publish_v2_keyring_refuses_when_force_re_pair_required() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        let key_state = EncryptionKeyState::default();
        let provider = InMemoryKeyringProvider::new();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive").unwrap();
        }

        let ctx = make_publish_ctx();
        let result = publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state).await;
        assert!(
            result.is_err(),
            "a flagged device must not publish — doing so clobbers a peer's rotation \
             and lets the next check verify against its own write"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.to_lowercase().contains("re-pair"),
            "the refusal must name the cause: {msg}"
        );
    }

    /// End-to-end link: the flag this helper clears must be the SAME flag the
    /// publish gate reads. Without this, the helper could clear a different row
    /// and every unit test above would still pass while the deadlock survived.
    #[tokio::test]
    async fn clearing_the_stale_flag_lets_the_publish_gate_rebuild_the_vault() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        let key_state = EncryptionKeyState::default();
        let provider = InMemoryKeyringProvider::new();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive").unwrap();
            clear_stale_flag_for_empty_cloud(&conn, false).expect("clear the stale flag");
        }

        let ctx = make_publish_ctx();
        let result = publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state).await;

        assert!(
            result.is_ok(),
            "once the stale flag is cleared the gate must let this device republish \
             the vault into the empty cloud: {result:?}"
        );
    }

    /// The mirror of the test above: `vault_rotated` survives the helper, so the
    /// publish gate must still refuse. This is the property that keeps the
    /// empty-cloud escape hatch from becoming a way around a real rotation.
    #[tokio::test]
    async fn a_vault_rotated_flag_still_blocks_publish_after_the_helper_runs() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        let key_state = EncryptionKeyState::default();
        let provider = InMemoryKeyringProvider::new();
        {
            let conn = state.lock().unwrap();
            set_force_re_pair_flags(&conn, "vault_rotated").unwrap();
            clear_stale_flag_for_empty_cloud(&conn, false).expect("the helper must not error");
        }

        let ctx = make_publish_ctx();
        let result = publish_v2_keyring_with_provider(&provider, &state, &ctx, &key_state).await;

        assert!(
            result.is_err(),
            "a genuinely rotated vault must keep this device from publishing stale key material"
        );
    }
}
