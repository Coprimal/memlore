//! Joining an existing cloud vault with the recovery phrase (onboarding and
//! force re-pair), and the offline "Forgot password" unwrap. Moved from the
//! desktop `commands/crypto.rs` (mobile M2a plan, task 1.4). The OS keystore
//! and the pending cloud session go through [`VaultPlatformHooks`].

use std::path::Path;

use zeroize::Zeroizing;

use crate::db;
use crate::utils::boot_file::BootFile;
use crate::utils::encryption::{
    derive_sqlcipher_key, derive_sync_key, encode_content_key_list, generate_encryption_salt,
    key_fingerprint, unwrap_content_key, wrap_content_key, wrap_key_with, KdfParams,
};

use super::{encode_hex, validate_password_length, VaultPlatformHooks};

/// The user-facing error for a recovery phrase that does not unwrap the
/// cloud vault's recovery slot.
pub const WRONG_RECOVERY_PHRASE_ERROR: &str =
    "Wrong recovery phrase. Check each word and try again.";

/// Validate the mnemonic against the cloud vault.
///
/// 1. Validate mnemonic syntax (24 words, valid BIP39 checksum).
/// 2. Read `_recovery.json` from Drive. Absent → vault corrupted.
/// 3. Derive `recovery_key` from mnemonic via HKDF.
/// 4. Unwrap `recovery_slot.wrapped_master` using `recovery_key`. Failure → wrong phrase.
/// 5. Read `_meta.json`; verify `key_fingerprint(master_key) == meta.master_fingerprint`.
#[doc(hidden)]
pub async fn onboard_validate_passphrase_inner<P: crate::sync::keyring_v2::KeyringV2Io>(
    provider: &P,
    mnemonic: &str,
) -> Result<(), String> {
    use crate::sync::keyring_v2::io as kio;
    use crate::utils::recovery;

    // 1. Validate mnemonic syntax.
    let parsed = recovery::validate_recovery_mnemonic(mnemonic)?;

    // 2. Read recovery slot.
    let recovery_slot = kio::read_recovery(provider)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "Cloud vault is missing the recovery slot — vault may be corrupted".to_string()
        })?;

    // 3. Derive recovery key.
    let recovery_key = recovery::derive_recovery_key(&parsed);

    // 4. Unwrap master key using raw AES-256-GCM (no Argon2id — recovery_key is
    // already 32 bytes of strong HKDF output). The recovery slot was created as
    // AES-GCM(recovery_key, master_key) in begin_first_time_setup_inner.
    let master_key = unwrap_master_with_recovery_key(&recovery_slot.wrapped_master, &*recovery_key)
        .map_err(|_| WRONG_RECOVERY_PHRASE_ERROR.to_string())?;

    // 5. Verify fingerprint against meta.
    let meta = kio::read_meta(provider)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Cloud vault _meta.json is missing".to_string())?;

    let computed_fp = hex::encode(crate::utils::encryption::key_fingerprint(&*master_key));
    if computed_fp != meta.master_fingerprint {
        return Err(
            "Could not verify the cloud vault — the recovery phrase may not match this vault."
                .to_string(),
        );
    }

    Ok(())
}

/// Complete onboarding: derive master key, write device slot, PRAGMA rekey.
///
/// Returns the post-rekey `Connection`.
///
/// `session_id` mirrors the outer command parameter — see `onboard_complete`
/// docs for the full contract. `unlock_method` mirrors
/// `confirm_first_time_setup_inner`'s parameter of the same name — see
/// `resolve_unlock_method_with_os_kek`. `hooks` carries the platform side
/// effects: the OS keystore and the pending cloud session.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn onboard_complete_inner<P: crate::sync::keyring_v2::KeyringV2Io>(
    conn: rusqlite::Connection,
    key_state: &crate::EncryptionKeyState,
    provider: &P,
    mnemonic: &str,
    new_local_password: &str,
    device_name: &str,
    boot_path: Option<&Path>,
    session_id: Option<String>,
    unlock_method: Option<&str>,
    hooks: &dyn VaultPlatformHooks,
) -> Result<rusqlite::Connection, (rusqlite::Connection, String)> {
    use crate::sync::keyring_v2::io as kio;
    use crate::sync::keyring_v2::types::{DeviceSlotV2, KEYRING_V2_VERSION};
    use crate::utils::recovery;

    let (initial_control, initial_control_revision) =
        match crate::sync::recovery::read_versioned_sync_control(provider).await {
            Ok((control, revision)) if control.recovery_lease.is_none() => (control, revision),
            Ok(_) => return Err((
                conn,
                "An authoritative recovery is active on this cloud vault; retry after it finishes."
                    .to_string(),
            )),
            Err(error) => return Err((conn, format!("Cloud recovery control: {error}"))),
        };
    provider.configure_recovery_fence(initial_control.recovery_generation, None);

    // 0. Declare the pending session binding — consumed just before first DB
    //    write (step 4) so transient network / AES failures (steps 1-3) leave
    //    the session intact for a user retry, matching the behaviour of
    //    confirm_first_time_setup_inner.

    // 1. Re-derive master key (stateless — avoids in-memory stash).
    let parsed = match recovery::validate_recovery_mnemonic(mnemonic) {
        Ok(p) => p,
        Err(e) => return Err((conn, format!("mnemonic: {e}"))),
    };

    let recovery_slot = match kio::read_recovery(provider).await {
        Ok(Some(slot)) => slot,
        Ok(None) => return Err((conn, "Cloud vault is missing the recovery slot".to_string())),
        Err(e) => return Err((conn, e.to_string())),
    };

    let recovery_key = recovery::derive_recovery_key(&parsed);

    // Raw AES-256-GCM unwrap — no Argon2id, recovery_key is already HKDF output.
    let master_key =
        match unwrap_master_with_recovery_key(&recovery_slot.wrapped_master, &*recovery_key) {
            Ok(k) => k,
            Err(_) => return Err((conn, WRONG_RECOVERY_PHRASE_ERROR.to_string())),
        };

    let meta = match kio::read_meta(provider).await {
        Ok(Some(m)) => m,
        Ok(None) => return Err((conn, "Cloud vault _meta.json is missing".to_string())),
        Err(e) => return Err((conn, e.to_string())),
    };
    if meta.recovery_generation != initial_control.recovery_generation {
        return Err((
            conn,
            format!(
                "Recovery generation mismatch: control={}, keyring_meta={}",
                initial_control.recovery_generation, meta.recovery_generation
            ),
        ));
    }

    let computed_fp = hex::encode(crate::utils::encryption::key_fingerprint(&*master_key));
    if computed_fp != meta.master_fingerprint {
        return Err((
            conn,
            "Could not verify the cloud vault — the recovery phrase may not match this vault."
                .to_string(),
        ));
    }

    // 2. Derive new device KEK.
    let kek_salt = generate_encryption_salt();
    let kek_salt_hex = hex::encode(&kek_salt);

    // Wrap master_key with Argon2id(new_local_password, kek_salt).
    //
    // A freshly onboarded device has no prior KDF preset to inherit (the preset
    // is per-device and not stored in the cloud keyring). Default to FAST — the
    // app default for new key material (matches first-time setup). Using the
    // self-describing format keeps the displayed strength consistent with the
    // choice instead of silently reporting High.
    let wrapped_master_hex =
        match wrap_key_with(&*master_key, new_local_password, &kek_salt, KdfParams::FAST) {
            Ok(hex) => hex,
            Err(e) => return Err((conn, format!("wrap_key failed: {e}"))),
        };

    // 3. Get device_id (create if not exists).
    let device_id = match db::get_or_create_device_id(&conn) {
        Ok(id) => id,
        Err(e) => return Err((conn, e.to_string())),
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let master_fingerprint = hex::encode(crate::utils::encryption::key_fingerprint(&*master_key));

    // Phase 3 T1 (joiner path): derive the content-key list and per-device db_key.
    //
    // Try to read `_content.json` from the cloud. Present (Phase 5+ vault):
    //   decode the entries, unwrap each content key with master_key.
    // Absent (pre-Phase-5 vault or bootstrap): fall back to v1=master_key seed,
    //   which matches what existing cloud entries were encrypted under.
    //   This fallback does NOT set NEEDS_CLOUD_CONTENT_MIGRATION (that flag is
    //   for migrating an existing CREATOR install whose cloud _content.json is
    //   absent; a joining device doesn't own the migration decision).
    //
    // In both cases generate a fresh local db_key (never published to cloud).
    let (joiner_content_keys, joiner_latest_epoch, joiner_encoded_list) = {
        match kio::read_content(provider).await {
            Ok(Some(cloud_list)) => {
                // Cloud _content.json present but empty entries is invalid: we cannot
                // determine which keys are in use, and the `.expect("latest_epoch must
                // be in map")` below would panic on cloud-supplied data. Reject early.
                if cloud_list.entries.is_empty() {
                    return Err((
                        conn,
                        "onboard_complete: _content.json has no entries — \
                         the file appears corrupt or was tampered with; please contact \
                         the vault owner to republish their keyring"
                            .to_string(),
                    ));
                }
                // Unwrap each entry with master_key.
                let mut map: std::collections::BTreeMap<u32, Zeroizing<[u8; 32]>> =
                    std::collections::BTreeMap::new();
                for entry in &cloud_list.entries {
                    let blob = match hex::decode(&entry.wrapped_content) {
                        Ok(b) => b,
                        Err(e) => {
                            return Err((
                                conn,
                                format!(
                                    "onboard_complete: _content.json entry epoch={} \
                                     wrapped_content hex decode failed: {e}",
                                    entry.epoch
                                ),
                            ))
                        }
                    };
                    let key = match unwrap_content_key(&master_key, &blob) {
                        Ok(k) => k,
                        Err(e) => {
                            return Err((
                                conn,
                                format!(
                                    "onboard_complete: unwrap content key epoch={} \
                                     failed: {e}",
                                    entry.epoch
                                ),
                            ))
                        }
                    };
                    // Cross-check: the unwrapped key must produce the same fingerprint
                    // as the creator wrote in content_fingerprint.  A mismatch means
                    // the list is corrupt or was written by a different master key —
                    // fail loudly rather than silently using the wrong key.
                    let computed_fp = hex::encode(key_fingerprint(&*derive_sync_key(&*key)));
                    if computed_fp != entry.content_fingerprint {
                        return Err((
                            conn,
                            format!(
                                "onboard_complete: content key fingerprint mismatch at \
                                 epoch={} (expected {}, computed {})",
                                entry.epoch, entry.content_fingerprint, computed_fp
                            ),
                        ));
                    }
                    map.insert(entry.epoch, key);
                }
                let latest = cloud_list.latest_epoch;
                // Re-encode in our compact local codec under master_key for storage.
                let encoded = match encode_content_key_list(&master_key, &map) {
                    Ok(s) => s,
                    Err(e) => return Err((conn, format!("encode_content_key_list failed: {e}"))),
                };
                log::info!(
                    "onboard_complete: loaded content-key list from cloud \
                     ({} epochs, latest={latest})",
                    map.len()
                );
                (map, latest, encoded)
            }
            Ok(None) => {
                // _content.json absent — vault pre-dates Phase 5.
                // Seed with v1=master_key so existing cloud entries remain decryptable.
                // Do NOT set NEEDS_CLOUD_CONTENT_MIGRATION here — only the creator
                // (or T3 migration) sets that flag.
                let mut map: std::collections::BTreeMap<u32, Zeroizing<[u8; 32]>> =
                    std::collections::BTreeMap::new();
                map.insert(1u32, Zeroizing::new(*master_key));
                let encoded = match encode_content_key_list(&master_key, &map) {
                    Ok(s) => s,
                    Err(e) => return Err((conn, format!("encode_content_key_list failed: {e}"))),
                };
                log::info!(
                    "onboard_complete: _content.json absent — seeding v1=master \
                     (pre-Phase-5 vault)"
                );
                (map, 1u32, encoded)
            }
            Err(e) => {
                // Transient cloud read failure (network blip, 5xx, I/O error).
                //
                // IMPORTANT: Err ≠ absent.  Ok(None) means the file genuinely does not
                // exist (legacy pre-Phase-5 vault) and v1=master is correct.  An Err
                // here means we could not determine whether _content.json exists, so we
                // must NOT assume it is absent.  Falling back to v1=master on a vault
                // that actually has a random v1 content key would silently succeed with
                // the wrong key — every entry decryption would fail with no error.
                // Propagate so the user retries when the transient condition clears.
                return Err((
                    conn,
                    format!(
                        "onboard_complete: failed to read _content.json from cloud ({e}); \
                         please retry"
                    ),
                ));
            }
        }
    };

    // Generate a per-device random db_key (never published to cloud).
    let mut joiner_db_key = Zeroizing::new([0u8; 32]);
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, joiner_db_key.as_mut());

    // Wrap db_key under the password KEK.
    let joiner_kek = match crate::utils::encryption::derive_encryption_key_with(
        new_local_password,
        &kek_salt,
        KdfParams::FAST,
    ) {
        Ok(k) => k,
        Err(e) => return Err((conn, format!("derive KEK for db_key wrap failed: {e}"))),
    };
    let joiner_wrapped_db_key_hex = match wrap_content_key(&*joiner_kek, &*joiner_db_key) {
        Ok(blob) => hex::encode(&blob),
        Err(e) => return Err((conn, format!("wrap db_key failed: {e}"))),
    };

    // Cloud fingerprint is key_fingerprint(derive_sync_key(latest_content_key)).
    let joiner_cloud_fp_hex = {
        let latest_key = match joiner_content_keys.get(&joiner_latest_epoch) {
            Some(k) => k,
            None => {
                return Err((
                    conn,
                    format!(
                        "onboard_complete: _content.json latest_epoch={} not found in \
                         decoded key map — the file is corrupt or was tampered with",
                        joiner_latest_epoch
                    ),
                ))
            }
        };
        hex::encode(key_fingerprint(&*derive_sync_key(latest_key)))
    };

    // 4a. Rollback closure — undoes all DB writes if boot file or PRAGMA rekey fails.
    let rollback_settings = |conn: &rusqlite::Connection, device_id: &str| {
        log::warn!("onboard_complete: rekey failed, rolling back settings");
        let _ = db::delete_setting(conn, db::WRAPPED_ENCRYPTION_KEY_KEY);
        let _ = db::delete_setting(conn, db::KEK_SALT_KEY);
        let _ = db::delete_setting(conn, "password_hash");
        let _ = db::set_encryption_mode(conn, db::EncryptionMode::Unset);
        let _ = db::delete_setting(conn, db::KEYRING_V2_LOCAL_EPOCH);
        let _ = db::delete_setting(conn, db::CLOUD_MASTER_FINGERPRINT);
        let _ = db::delete_setting(conn, db::RECOVERY_PASSPHRASE_CREATED_AT);
        let _ = db::delete_setting(conn, db::RECOVERY_WRAPPED_MASTER_KEY);
        // Phase 3 T1: also roll back content-key settings.
        let _ = db::delete_setting(conn, db::WRAPPED_CONTENT_LIST);
        let _ = db::delete_setting(conn, db::CONTENT_KEY_EPOCH);
        let _ = db::delete_setting(conn, db::CLOUD_CONTENT_FINGERPRINT);
        // Paired with `cleanup_os_kek` on every failure path below, so the
        // Keychain item and this setting are rolled back together — otherwise
        // a boot-file failure after an os_kek join would leave the setting
        // reading `true` with the Keychain item already removed.
        let _ = db::delete_setting(conn, super::BIOMETRIC_ENABLED_SETTING);
        let _ = conn.execute("DELETE FROM devices WHERE device_id = ?1", [device_id]);
    };

    // Write all settings to DB.
    // Helper macro: on error, return Err((conn, msg)) after running rollback.
    // rollback_settings borrows &conn; the borrow ends before conn moves into the tuple.
    macro_rules! db_write {
        ($expr:expr) => {
            if let Err(e) = $expr {
                return Err((conn, e.to_string()));
            }
        };
    }

    db_write!(db::set_setting(
        &conn,
        db::WRAPPED_ENCRYPTION_KEY_KEY,
        &wrapped_master_hex
    ));
    db_write!(db::set_setting(&conn, db::KEK_SALT_KEY, &kek_salt_hex));
    db_write!(db::set_password_hash(&conn, new_local_password));
    db_write!(db::set_encryption_mode(&conn, db::EncryptionMode::Password));
    db_write!(db::set_setting(
        &conn,
        db::KEYRING_V2_LOCAL_EPOCH,
        &meta.epoch.to_string()
    ));
    db_write!(db::set_setting(
        &conn,
        db::CLOUD_MASTER_FINGERPRINT,
        &master_fingerprint
    ));
    // Persist the VAULT creation time from the cloud meta so any later keyring
    // republish from this peer (e.g. the keyring_dirty retry) writes the correct
    // `created_at` into `_recovery.json` / `_meta.json` instead of the `0`
    // fallback. The vault-creator path seeds this from its own `now_secs`; a
    // peer inherits it from the cloud meta it just validated against.
    db_write!(db::set_setting(
        &conn,
        db::RECOVERY_PASSPHRASE_CREATED_AT,
        &meta.created_at.to_string()
    ));
    // Persist recovery_wrapped so delayed Drive connect can publish if needed.
    db_write!(db::set_setting(
        &conn,
        db::RECOVERY_WRAPPED_MASTER_KEY,
        &recovery_slot.wrapped_master
    ));

    // Phase 3 T1 (joiner): persist the content-key list and epoch.
    db_write!(db::set_setting(
        &conn,
        db::WRAPPED_CONTENT_LIST,
        &joiner_encoded_list
    ));
    db_write!(db::set_setting(
        &conn,
        db::CONTENT_KEY_EPOCH,
        &joiner_latest_epoch.to_string()
    ));
    db_write!(db::set_setting(
        &conn,
        db::CLOUD_CONTENT_FINGERPRINT,
        &joiner_cloud_fp_hex
    ));

    // Upsert this device row (is_current = true).
    db_write!(db::upsert_device(
        &conn,
        &db::DeviceRow {
            device_id: device_id.clone(),
            name: device_name.to_string(),
            created_at: now_secs,
            last_seen_at: now_secs,
            is_current: true,
            is_revoked: false,
        }
    ));
    db_write!(db::set_current_device(&conn, &device_id));

    // Also insert other known device slots from Drive as non-current rows,
    // then prune local rows absent from the cloud listing so stale/revoked
    // peers don't accumulate on this machine.
    match kio::list_device_slots(provider).await {
        Ok(slots) => {
            for slot in &slots {
                if slot.device_id == device_id {
                    continue; // Skip this device — already inserted above.
                }
                let _ = db::upsert_device(
                    &conn,
                    &db::DeviceRow {
                        device_id: slot.device_id.clone(),
                        name: slot.name.clone(),
                        created_at: slot.created_at,
                        last_seen_at: slot.last_seen_at,
                        is_current: false,
                        is_revoked: false,
                    },
                );
            }
            // Converge: drop any non-current local row absent from cloud.
            // The current device (device_id, is_current=1) is always kept.
            let keep_ids: Vec<String> = slots.iter().map(|s| s.device_id.clone()).collect();
            if let Err(e) = db::prune_devices_not_in(&conn, &keep_ids) {
                log::warn!("onboard_complete: prune_devices_not_in failed ({e}), continuing");
            }
        }
        Err(e) => {
            log::warn!("onboard_complete: list_device_slots failed ({e}), continuing");
        }
    }

    // Resolve unlock method (Phase 3) — before the boot-file write so an
    // os_kek failure aborts before any irreversible on-disk change. Resolved
    // outside the boot-path block because `biometric_unlock_enabled` below is
    // DB state, not boot-file state, and must be written on every path that
    // resolves a method. With no explicit request this is `(Password, None)`
    // and never touches the Keychain.
    let (resolved_unlock_method, os_kek_wrapped_master_hex) =
        match super::setup::resolve_unlock_method_with_os_kek(unlock_method, &master_key, hooks) {
            Ok(v) => v,
            Err(e) => return Err((conn, e)),
        };
    let cleanup_os_kek = || {
        if os_kek_wrapped_master_hex.is_some() {
            let _ = hooks.remove_os_kek();
        }
    };

    // Mirror the resolved unlock method into `biometric_unlock_enabled` — the
    // setting every management surface reads. `enable_biometric_unlock` is the
    // only other writer and join never calls it, so without this an os_kek
    // chosen here would wrap the master invisibly and unmanageably. Rolled
    // back by `rollback_settings`, alongside `cleanup_os_kek`, so the Keychain
    // and the setting can never disagree in either direction. Not routed
    // through `db_write!` — that macro returns without removing the os_kek.
    if let Err(e) = hooks.sync_biometric_setting_for_unlock_method(&conn, resolved_unlock_method) {
        rollback_settings(&conn, &device_id);
        cleanup_os_kek();
        return Err((conn, e));
    }

    // 5. Write boot file + PRAGMA rekey (production path only).
    // Phase 3 T1: rekey DB to db_key-derived (not master-derived).
    if let Some(bp) = boot_path {
        let sqlcipher_key = derive_sqlcipher_key(&*joiner_db_key);
        // T5: also wrap joiner_db_key under master_key for recover/biometric paths.
        let joiner_master_wrapped_db_key_hex = match wrap_content_key(&master_key, &*joiner_db_key)
        {
            Ok(blob) => hex::encode(&blob),
            Err(e) => {
                rollback_settings(&conn, &device_id);
                cleanup_os_kek();
                return Err((conn, format!("wrap master_wrapped_db_key failed: {e}")));
            }
        };

        if let Err(e) = crate::utils::boot_file::save(
            &BootFile {
                mode: db::EncryptionMode::Password,
                unlock_method: resolved_unlock_method,
                kek_salt: Some(kek_salt_hex.clone()),
                wrapped_key: Some(wrapped_master_hex.clone()),
                os_kek_wrapped_master: os_kek_wrapped_master_hex.clone(),
                // Persist the recovery slot locally so this device can later
                // reset a forgotten password offline via the recovery phrase.
                recovery_wrapped: Some(recovery_slot.wrapped_master.clone()),
                wrapped_content_list: Some(joiner_encoded_list.clone()),
                wrapped_db_key: Some(joiner_wrapped_db_key_hex.clone()),
                master_wrapped_db_key: Some(joiner_master_wrapped_db_key_hex),
                migration_in_progress: None,
            },
            bp,
        ) {
            rollback_settings(&conn, &device_id);
            cleanup_os_kek();
            // rollback borrow ends here; now we can move conn into the Err tuple.
            return Err((conn, format!("Failed to write boot file: {e}")));
        }

        let pre_rekey = conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .and_then(|_| conn.pragma_update(None, "journal_mode", "DELETE"));
        if let Err(e) = pre_rekey {
            rollback_settings(&conn, &device_id);
            let _ = crate::utils::boot_file::save(&BootFile::default(), bp);
            cleanup_os_kek();
            // rollback + boot_file borrows end here; now move conn.
            return Err((conn, format!("Pre-rekey WAL flush failed: {e}")));
        }
        if let Err(e) = conn.pragma_update(
            None,
            "rekey",
            format!("x'{}'", hex::encode(&*sqlcipher_key)),
        ) {
            rollback_settings(&conn, &device_id);
            let _ = crate::utils::boot_file::save(&BootFile::default(), bp);
            cleanup_os_kek();
            // rollback + boot_file borrows end here; now move conn.
            return Err((conn, format!("PRAGMA rekey failed: {e}")));
        }
    }

    // 6. Load key into state.
    // Phase 3 T1: use set_content_state with the full content-key list.
    if let Err(e) = key_state.set_content_state(
        joiner_content_keys,
        joiner_latest_epoch,
        joiner_db_key,
        Zeroizing::new(*master_key),
    ) {
        return Err((conn, e));
    }

    // 7. TOCTOU guard, then upload this device's slot + meta bump.
    //
    // `meta` was snapshotted at the top of this function, and the two cloud
    // writes below are unconditional overwrites — Drive has no CAS. If a peer
    // rotated the vault while the user was typing the phrase, the master key
    // validated in step 1 is already stale: writing our slot back would
    // recreate a slot the rotation removed, and the meta bump would roll
    // `_meta.json` back to the pre-rotation fingerprint/epoch. Re-read and
    // abort instead — the caller keeps the force-re-pair guard in place and the
    // user retries with the vault's CURRENT phrase. (The local DB was already
    // rekeyed above, but only to this device's private db_key; a retried
    // re-pair simply rekeys again.)
    // `content_epoch` is compared too: a content-key rotation/migration that
    // bumps only the content epoch invalidates the content-key list decoded in
    // step 3, and writing the old snapshot back would roll the cloud's
    // content history down.
    let current_meta = match kio::read_meta(provider).await {
        Ok(Some(current)) => {
            if current.master_fingerprint != meta.master_fingerprint
                || current.epoch != meta.epoch
                || current.content_epoch != meta.content_epoch
            {
                return Err((
                    conn,
                    "The cloud vault was rotated by another device while re-pairing. \
                     Please retry with the vault's current recovery phrase."
                        .to_string(),
                ));
            }
            current
        }
        Ok(None) => {
            return Err((
                conn,
                "The cloud vault _meta.json disappeared while re-pairing; please retry."
                    .to_string(),
            ));
        }
        Err(e) => {
            return Err((
                conn,
                format!(
                    "Could not re-verify the cloud vault before publishing ({e}); please retry."
                ),
            ));
        }
    };
    let (control, control_revision) =
        match crate::sync::recovery::read_versioned_sync_control(provider).await {
            Ok((control, revision)) if control.recovery_lease.is_none() => (control, revision),
            Ok(_) => {
                return Err((
                    conn,
                    "An authoritative recovery started while re-pairing; retry after it finishes."
                        .to_string(),
                ))
            }
            Err(error) => return Err((conn, error.to_string())),
        };
    if control != initial_control || control_revision != initial_control_revision {
        return Err((
            conn,
            "Cloud recovery control changed while re-pairing; please retry.".to_string(),
        ));
    }
    if current_meta.recovery_generation != control.recovery_generation {
        return Err((
            conn,
            "Cloud keyring generation changed while re-pairing; please retry.".to_string(),
        ));
    }
    if control.recovery_generation > 0 {
        if let Err(e) = db::set_sync_recovery_generation(&conn, control.recovery_generation) {
            return Err((conn, e.to_string()));
        }
    }
    // Re-pair keeps local journal data (and thus any prior `sync_push_state`
    // rows) but adopts the cloud write namespace (`generations/g-N/...`).
    // Stale surface hashes would make hash-gate skip publishing into the new
    // namespace; re-arm Automatic reconcile so DeviceRoot self-heal can run.
    if let Err(e) = crate::sync::engine::invalidate_surface_push_state(&conn) {
        return Err((conn, e.to_string()));
    }
    provider.configure_recovery_fence(control.recovery_generation, None);

    let device_slot = DeviceSlotV2 {
        version: KEYRING_V2_VERSION,
        device_id: device_id.clone(),
        name: device_name.to_string(),
        created_at: now_secs,
        last_seen_at: now_secs,
    };
    match kio::write_device_slot(provider, &device_slot).await {
        Ok(()) => {
            log::info!("onboard_complete: device slot uploaded for {device_id}");
        }
        Err(e) => {
            log::warn!("onboard_complete: device slot upload failed ({e}), setting keyring_dirty");
            let _ = db::set_setting(&conn, db::KEYRING_DIRTY_KEY, "1");
        }
    }

    // 8. Bump _meta.json.updated_at (best-effort — epoch unchanged). Built
    //    from `current_meta` (the freshly re-read copy), NOT the validation
    //    snapshot, so any field the guard does not compare can never be
    //    rolled back by this write.
    let updated_meta = crate::sync::keyring_v2::types::KeyringMetaV2 {
        updated_at: now_secs,
        ..current_meta
    };
    if let Err(e) = kio::write_meta(provider, &updated_meta).await {
        log::warn!("onboard_complete: _meta.json updated_at bump failed ({e})");
    }

    // 9. Consume pending Drive session + persist sync settings — AFTER rekey.
    //    Consume happens HERE (not before mode-flip) so any rollback from a
    //    boot-file / PRAGMA-rekey failure does NOT burn the pending session.
    //    By this point all user-correctable failures (wrong recovery phrase,
    //    transient cloud reads, fingerprint mismatch) are behind us — only
    //    pure I/O remains and rekey already succeeded.
    //
    //    If session_id is supplied but the pending entry is gone (TTL expired
    //    during user typing, app restart between rekey and this branch), treat
    //    as best-effort skip — same as a persist failure: user lands in
    //    password mode without Drive; they reconnect from Settings → Sync.
    //    Returning PENDING_DRIVE_SESSION_EXPIRED here is no longer possible —
    //    the DB is already committed to Password mode.
    if let Some(sid) = session_id {
        match hooks.consume_pending_cloud_session(&conn, &sid) {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                log::warn!("onboard_complete: persist_connect_settings failed (non-fatal): {e}");
            }
            None => {
                log::warn!(
                    "onboard_complete: pending Drive session expired or missing for session_id; \
                     user will need to reconnect Drive from Settings → Sync"
                );
            }
        }
    }

    // 10. Remove the placeholder default "My Journal" if it is the SOLE
    //     journal in the DB AND has zero entries.
    //
    //     Why: schema migration unconditionally seeds a default journal so
    //     fresh installs are usable. But the Card-3 onboarding flow joins
    //     an existing V2 cloud vault; the user's real journals will arrive
    //     on the next sync. Without this cleanup, the picker would show
    //     the user's cloud journals AND a stray empty "My Journal" that
    //     never existed on the originating device.
    //
    //     Guard: only drop if it is the unique journal AND has no entries.
    //     This is defensive — never destroys a journal a user has written
    //     into, even in the unlikely offline-then-onboard scenario.
    if let Err(e) = remove_default_journal_if_unused(&conn) {
        log::warn!("onboard_complete: default-journal cleanup failed (non-fatal): {e}");
    }

    Ok(conn)
}

/// Drop the schema-seeded placeholder "My Journal" if and only if it is the
/// sole journal in the DB and has no entries attached. Used by
/// `onboard_complete_inner` after joining an existing cloud vault — the
/// real journals will arrive on the next sync.
#[doc(hidden)]
pub fn remove_default_journal_if_unused(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let journals = db::list_journals(conn, None)?;
    if journals.len() != 1 {
        return Ok(());
    }
    let journal = &journals[0];
    let entry_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE journal_id = ?1",
        rusqlite::params![&journal.id],
        |row| row.get(0),
    )?;
    if entry_count > 0 {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM journals WHERE id = ?1",
        rusqlite::params![&journal.id],
    )?;
    Ok(())
}

/// Unwrap a master key using the 32-byte raw recovery key (AES-256-GCM,
/// no Argon2id step — the recovery key is already strong HKDF output).
///
/// `wrapped_hex` is the hex-encoded `nonce || ciphertext || tag` blob.
#[doc(hidden)]
pub fn unwrap_master_with_recovery_key(
    wrapped_hex: &str,
    recovery_key: &[u8; 32],
) -> Result<Zeroizing<[u8; 32]>, String> {
    use crate::utils::encryption::KEY_SIZE;
    let blob = hex::decode(wrapped_hex).map_err(|e| format!("Invalid wrapped hex: {e}"))?;
    let plaintext = crate::utils::encryption::decrypt_data(recovery_key, &blob)
        .map_err(|_| "AES-GCM tag check failed".to_string())?;
    if plaintext.len() != KEY_SIZE {
        return Err(format!(
            "Unwrapped key has wrong size: expected {KEY_SIZE}, got {}",
            plaintext.len()
        ));
    }
    let mut key = Zeroizing::new([0u8; KEY_SIZE]);
    key.copy_from_slice(&plaintext);
    Ok(key)
}

/// Unwrap the master key from a boot-file recovery slot using the 24-word
/// recovery phrase, then re-wrap it under a fresh password KEK.
///
/// Pure (no Tauri state) so it is unit-testable. Returns the verified master
/// key plus the new wrapping material the caller persists. Does NOT touch the
/// DB or boot file — the caller commits those after this succeeds AND after it
/// has independently verified the master key opens the real DB.
///
/// A wrong recovery phrase fails at the AES-GCM tag check inside
/// `unwrap_master_with_recovery_key`, surfaced as a generic error so we do not
/// leak whether the phrase or the slot was at fault.
#[doc(hidden)]
pub fn recover_master_and_rewrap(
    recovery_wrapped_hex: &str,
    mnemonic: &str,
    new_password: &str,
    params: KdfParams,
) -> Result<(Zeroizing<[u8; 32]>, String, String), String> {
    validate_password_length(new_password)?;

    let parsed = crate::utils::recovery::validate_recovery_mnemonic(mnemonic)?;
    let recovery_key = crate::utils::recovery::derive_recovery_key(&parsed);
    let master_key = unwrap_master_with_recovery_key(recovery_wrapped_hex, &recovery_key)
        .map_err(|_| "Recovery phrase does not match this vault".to_string())?;

    // Wrap the recovered master under a fresh password KEK, preserving the
    // vault's KDF strength (`params`, derived by the caller from the existing
    // boot-file blob) so a password recovery does not silently change it.
    let kek_salt = generate_encryption_salt();
    let new_wrapped = wrap_key_with(&master_key, new_password, &kek_salt, params)
        .map_err(|e| format!("wrap_key failed: {e}"))?;
    let new_kek_salt_hex = encode_hex(&kek_salt);

    Ok((master_key, new_wrapped, new_kek_salt_hex))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::vault::biometric::FakeKeystoreHooks;
    use crate::vault::setup::{
        begin_first_time_setup_inner, confirm_first_time_setup_inner, V2KeyringPublishContext,
    };
    use crate::EncryptionKeyState;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    /// Seed a complete V2 vault using `begin_first_time_setup_inner` +
    /// `confirm_first_time_setup_inner`. Returns `(conn, mnemonic_phrase,
    /// publish_ctx)`.
    fn seed_v2_vault(
        password: &str,
        device_name: &str,
    ) -> (Connection, String, V2KeyringPublishContext) {
        let conn = setup();
        let handle =
            begin_first_time_setup_inner(&conn, password, device_name, KdfParams::FAST).unwrap();
        let mnemonic_phrase = handle.mnemonic.join(" ");
        let answers: Vec<String> = handle
            .challenge_indices
            .iter()
            .map(|&i| handle.mnemonic[i].clone())
            .collect();
        let key_state = EncryptionKeyState::new();
        let (conn, pub_ctx) = confirm_first_time_setup_inner(
            conn,
            &key_state,
            &handle.setup_id,
            &answers,
            password,
            std::path::Path::new("/tmp/unused_onboard.db"),
            None,
            None,
            None,
            &FakeKeystoreHooks::default(),
        )
        .expect("confirm must succeed");
        (conn, mnemonic_phrase, pub_ctx)
    }

    /// Seed the in-memory provider with a real V2 keyring (all three files)
    /// from the publish context of `seed_v2_vault`, plus the sync control.
    ///
    /// The desktop fixture runs `publish_v2_keyring_with_provider` (desktop
    /// `commands/gdrive.rs`) with a fresh DB and a fresh key state. This is
    /// the branch that call takes: recovery generation 0, master epoch 1, and
    /// the epoch-1 content key from the context.
    async fn seed_provider_from_ctx(
        provider: &crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider,
        pub_ctx: &V2KeyringPublishContext,
    ) {
        use crate::sync::keyring_v2::builder::publish_initial_v2_keyring;
        use crate::sync::keyring_v2::io::write_content;
        use crate::sync::keyring_v2::types::{
            ContentEntryV2, ContentListV2, DeviceSlotV2, KeyringMetaV2, RecoverySlotV2,
            KEYRING_V2_VERSION,
        };
        use crate::sync::keyring_v2::KeyringV2Io;

        provider.configure_recovery_fence(0, None);
        let recovery_slot = RecoverySlotV2 {
            version: KEYRING_V2_VERSION,
            wrapped_master: pub_ctx.recovery_wrapped.clone(),
            created_at: pub_ctx.created_at,
        };
        let device_slot = DeviceSlotV2 {
            version: KEYRING_V2_VERSION,
            device_id: pub_ctx.device_id.clone(),
            name: pub_ctx.device_name.clone(),
            created_at: pub_ctx.device_created_at,
            last_seen_at: pub_ctx.last_seen_at,
        };
        let content_list = ContentListV2 {
            version: KEYRING_V2_VERSION,
            latest_epoch: 1,
            entries: vec![ContentEntryV2 {
                epoch: 1,
                wrapped_content: pub_ctx.wrapped_content_v1.clone(),
                content_fingerprint: pub_ctx.content_fingerprint_v1.clone(),
            }],
            created_at: pub_ctx.created_at,
        };
        let meta = KeyringMetaV2 {
            version: KEYRING_V2_VERSION,
            epoch: 1,
            master_fingerprint: pub_ctx.master_fingerprint.clone(),
            content_epoch: 1,
            recovery_generation: 0,
            created_at: pub_ctx.created_at,
            updated_at: pub_ctx.created_at,
        };
        write_content(provider, &content_list)
            .await
            .expect("write _content.json must succeed");
        publish_initial_v2_keyring(provider, &recovery_slot, &device_slot, &meta)
            .await
            .expect("publish must succeed");
        crate::sync::recovery::bootstrap_sync_control(provider, 1)
            .await
            .expect("sync control bootstrap must succeed");
    }

    // ─── Onboard: validate passphrase ────────────────────────────────────────

    /// Happy path: correct mnemonic → Ok(()).
    #[tokio::test]
    async fn onboard_validate_passphrase_correct_mnemonic_succeeds() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_conn, mnemonic, pub_ctx) = seed_v2_vault("validpass12", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let result = onboard_validate_passphrase_inner(&provider, &mnemonic).await;
        assert!(
            result.is_ok(),
            "correct mnemonic must succeed: {:?}",
            result.err()
        );
    }

    /// Wrong mnemonic → generic error, no info leak.
    #[tokio::test]
    async fn onboard_validate_passphrase_wrong_mnemonic_fails_generic() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_conn, _mnemonic, pub_ctx) = seed_v2_vault("validpass12", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Use a different (but valid) mnemonic by doing a second setup.
        let conn2 = setup();
        let handle2 =
            begin_first_time_setup_inner(&conn2, "differentpass12", "M2", KdfParams::FAST).unwrap();
        let wrong_mnemonic = handle2.mnemonic.join(" ");

        let result = onboard_validate_passphrase_inner(&provider, &wrong_mnemonic).await;
        assert!(result.is_err(), "wrong mnemonic must fail");
        let err = result.unwrap_err();
        // Must not leak internal details like "AES-GCM tag".
        assert!(
            err.contains("Wrong recovery phrase") || err.contains("Could not verify"),
            "error must be generic: {err}"
        );
        assert!(
            !err.contains("AES-GCM"),
            "error must not leak AES-GCM detail: {err}"
        );
    }

    /// Missing recovery slot → vault-corrupted error.
    #[tokio::test]
    async fn onboard_validate_passphrase_missing_recovery_slot_fails() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_conn, mnemonic, pub_ctx) = seed_v2_vault("validpass12", "M1");
        let provider = InMemoryKeyringProvider::new();
        // Publish only meta, skip recovery slot.
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::types::{KeyringMetaV2, KEYRING_V2_VERSION};
        let meta = KeyringMetaV2 {
            version: KEYRING_V2_VERSION,
            epoch: 1,
            master_fingerprint: pub_ctx.master_fingerprint.clone(),
            content_epoch: 0,
            recovery_generation: 0,
            created_at: 0,
            updated_at: 0,
        };
        kio::write_meta(&provider, &meta).await.unwrap();
        // No recovery slot written.

        let result = onboard_validate_passphrase_inner(&provider, &mnemonic).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("missing the recovery slot"),
            "must report missing recovery slot"
        );
    }

    /// A `master_fingerprint` mismatch (correct phrase, but `_meta.json` claims
    /// a different master than what `_recovery.json` actually unwraps to — e.g.
    /// a tampered or cross-vault `_meta.json`) must fail cleanly, not panic.
    /// Regression guard for the 🚨 in phase-2: this exercises the SAME
    /// `unwrap → compare fingerprint` path that protects `RecoverySlotV2`,
    /// proving the recovery-slot unwrap itself still works correctly even when
    /// the surrounding metadata is wrong.
    #[tokio::test]
    async fn onboard_validate_passphrase_master_fingerprint_mismatch_fails_cleanly() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::types::{KeyringMetaV2, KEYRING_V2_VERSION};

        let (_conn, mnemonic, pub_ctx) = seed_v2_vault("validpass12", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Tamper _meta.json: keep it well-formed, but point master_fingerprint
        // at a different (unrelated) key than the one _recovery.json actually
        // wraps. The mnemonic is genuinely correct — the mismatch is entirely
        // in the metadata, so decrypt succeeds and only the fingerprint compare
        // must fail.
        let mut meta = kio::read_meta(&provider).await.unwrap().unwrap();
        meta = KeyringMetaV2 {
            master_fingerprint: "f".repeat(64),
            ..meta
        };
        assert_eq!(meta.version, KEYRING_V2_VERSION);
        kio::write_meta(&provider, &meta).await.unwrap();

        let result = onboard_validate_passphrase_inner(&provider, &mnemonic).await;
        assert!(
            result.is_err(),
            "fingerprint mismatch must fail, not silently succeed"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("Could not verify"),
            "error must be the clean fingerprint-mismatch message, got: {err}"
        );
    }

    /// Garbage input (not 24 words) → rejected before any Drive I/O.
    #[tokio::test]
    async fn onboard_validate_passphrase_garbage_input_fails() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let provider = InMemoryKeyringProvider::new();
        let result = onboard_validate_passphrase_inner(&provider, "not a valid phrase").await;
        assert!(result.is_err(), "garbage input must fail");
    }

    // ─── Onboard: complete ────────────────────────────────────────────────────

    /// Happy path: onboard_complete_inner writes device slot, sets DB settings,
    /// keys the encryption key state.
    #[tokio::test]
    async fn onboard_complete_inner_happy_path() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None, // skip boot file in test
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner must succeed");

        // DB must have encryption_mode = Password.
        assert_eq!(
            db::get_encryption_mode(&new_conn).unwrap(),
            db::EncryptionMode::Password
        );

        // DB must have wrapped_encryption_key.
        let wrapped = db::get_setting(&new_conn, db::WRAPPED_ENCRYPTION_KEY_KEY)
            .unwrap()
            .expect("wrapped_encryption_key must be set");
        assert!(!wrapped.is_empty());

        // DB must have kek_salt.
        let kek_salt = db::get_setting(&new_conn, db::KEK_SALT_KEY)
            .unwrap()
            .expect("kek_salt must be set");
        assert!(!kek_salt.is_empty());

        // cloud_master_fingerprint must be set.
        let fp = db::get_setting(&new_conn, db::CLOUD_MASTER_FINGERPRINT)
            .unwrap()
            .expect("cloud_master_fingerprint must be set");
        assert_eq!(fp, pub_ctx.master_fingerprint);

        // key_state must be initialized.
        assert!(
            key_state.is_initialized().unwrap(),
            "key_state must be initialized after onboard_complete"
        );
    }

    /// Requirement 4/5(a): a fresh device joins using ONLY `_recovery.json` +
    /// `_content.json` (plus `_meta.json` for the fingerprint check) — no cloud
    /// device slot is needed at all. Deletes M1's device slot from the mock
    /// cloud before M2 joins, proving the join path never depends on any
    /// `devices/*.json` existing.
    #[tokio::test]
    async fn onboard_complete_inner_joins_with_zero_device_slots_present() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Remove every device slot from the mock cloud — simulates a vault
        // whose registry is (or has become) empty. Only _meta.json,
        // _recovery.json, and _content.json remain.
        kio::delete_device_slot(&provider, &pub_ctx.device_id)
            .await
            .unwrap();
        assert!(
            kio::list_device_slots(&provider).await.unwrap().is_empty(),
            "precondition: zero device slots before join"
        );

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None, // skip boot file in test
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("join must succeed with zero device slots present");

        assert_eq!(
            db::get_encryption_mode(&new_conn).unwrap(),
            db::EncryptionMode::Password
        );
        assert!(
            key_state.is_initialized().unwrap(),
            "key_state must be initialized after joining with no device slots"
        );

        // The join itself publishes its own (metadata-only) slot at the end —
        // confirms the registry is self-healing, not that a slot was required.
        let slots_after = kio::list_device_slots(&provider).await.unwrap();
        assert_eq!(slots_after.len(), 1, "M2's own slot is created by the join");
    }

    /// A2 TOCTOU guard: a peer rotation landing between the initial `_meta.json`
    /// snapshot and the final publish must abort the onboard instead of writing
    /// the (now stale) device slot + meta back — that write would clobber the
    /// rotation and roll the cloud back to the pre-rotation epoch.
    #[tokio::test]
    async fn onboard_complete_inner_aborts_when_vault_rotated_mid_flow() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::io::META_PATH;
        use crate::sync::keyring_v2::types::{KeyringMetaV2, KEYRING_V2_VERSION};
        use crate::sync::keyring_v2::KeyringV2Io;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Serves the original `_meta.json` on the FIRST read (the validation
        /// snapshot) and a rotated one on every later read (the pre-publish
        /// guard) — simulating a peer rotation inside the window.
        struct RotatingMetaProvider {
            inner: InMemoryKeyringProvider,
            meta_reads: AtomicUsize,
            rotated_meta: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl crate::sync::keyring_v2::KeyringV2Io for RotatingMetaProvider {
            async fn read_file(
                &self,
                path: &str,
            ) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
                if path == META_PATH && self.meta_reads.fetch_add(1, Ordering::SeqCst) >= 1 {
                    return Ok(self.rotated_meta.clone());
                }
                self.inner.read_file(path).await
            }
            async fn write_file(
                &self,
                path: &str,
                data: &[u8],
            ) -> Result<(), crate::sync::provider::SyncError> {
                self.inner.write_file(path, data).await
            }
            async fn delete_file(
                &self,
                path: &str,
            ) -> Result<(), crate::sync::provider::SyncError> {
                self.inner.delete_file(path).await
            }
            async fn list_files(
                &self,
                prefix: &str,
            ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
                self.inner.list_files(prefix).await
            }
            async fn read_versioned_file(
                &self,
                path: &str,
            ) -> Result<crate::sync::keyring_v2::VersionedFile, crate::sync::provider::SyncError>
            {
                self.inner.read_versioned_file(path).await
            }
        }

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let inner = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&inner, &pub_ctx).await;

        // Rotated meta: same fingerprint format, epoch advanced by a peer.
        let rotated = KeyringMetaV2 {
            version: KEYRING_V2_VERSION,
            epoch: 2,
            master_fingerprint: pub_ctx.master_fingerprint.clone(),
            content_epoch: 1,
            recovery_generation: 0,
            created_at: pub_ctx.created_at,
            updated_at: pub_ctx.created_at + 10,
        };
        let provider = RotatingMetaProvider {
            inner,
            meta_reads: AtomicUsize::new(0),
            rotated_meta: serde_json::to_vec(&rotated).unwrap(),
        };

        let devices_before = provider
            .inner
            .list_files(".meta/keyring/devices")
            .await
            .unwrap();

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let result = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await;

        let (_conn, err) = result.expect_err("a mid-flow rotation must abort the onboard");
        assert!(
            err.contains("rotated by another device"),
            "error must explain the rotation: {err}"
        );
        // No cloud write may have happened: the joiner's device slot must not
        // exist and the seeder's slot set must be unchanged.
        let devices_after = provider
            .inner
            .list_files(".meta/keyring/devices")
            .await
            .unwrap();
        assert_eq!(
            devices_after, devices_before,
            "the stale device slot must NOT be written after a detected rotation"
        );
    }

    /// After onboard_complete_inner, the placeholder default "My Journal"
    /// seeded by schema migration must be removed so the subsequent first
    /// sync can pull the user's real journals from the cloud without a
    /// stray empty journal lingering in the picker. Only removed if it is
    /// the SOLE journal AND has zero entries (defensive — never drops
    /// user-authored journals on re-onboard).
    #[tokio::test]
    async fn onboard_complete_inner_removes_default_empty_my_journal() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        // Verify the seeded default journal exists pre-onboard (schema
        // migration in setup() always inserts "My Journal" on fresh DBs).
        let pre = db::list_journals(&conn2, None).unwrap();
        assert_eq!(
            pre.len(),
            1,
            "setup() must seed exactly one default journal"
        );
        assert_eq!(pre[0].name, "My Journal");

        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner must succeed");

        // The default journal must be gone — cloud sync will populate the
        // user's real journals.
        let post = db::list_journals(&new_conn, None).unwrap();
        assert!(
            post.is_empty(),
            "default empty 'My Journal' must be removed after onboard; got: {post:?}"
        );
    }

    /// Defensive: if the user has authored entries in "My Journal" BEFORE
    /// onboard (unlikely but possible via offline workflow), the cleanup
    /// must NOT drop it.
    #[tokio::test]
    async fn onboard_complete_inner_preserves_my_journal_with_entries() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        // Insert one entry into the seeded "My Journal" so it has content.
        let seeded = db::list_journals(&conn2, None).unwrap();
        let jid = &seeded[0].id;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        conn2
            .execute(
                "INSERT INTO entries (id, journal_id, entry_date, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params!["entry-1", jid, now_secs, now_secs],
            )
            .unwrap();

        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner must succeed");

        // The journal with entries must still exist.
        let post = db::list_journals(&new_conn, None).unwrap();
        assert_eq!(
            post.len(),
            1,
            "journal with entries must be preserved; got: {post:?}"
        );
        assert_eq!(post[0].id, *jid);
    }

    /// After onboard_complete_inner, the device slot is uploaded to Drive.
    #[tokio::test]
    async fn onboard_complete_inner_uploads_device_slot() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        // Force a stable device_id for this test.
        let device_id = db::get_or_create_device_id(&conn2).unwrap();

        let key_state = EncryptionKeyState::new();
        onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 Device",
            None,
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("must succeed");

        // Drive must have the device slot.
        let slots = kio::list_device_slots(&provider).await.unwrap();
        let m2_slot = slots.iter().find(|s| s.device_id == device_id);
        assert!(
            m2_slot.is_some(),
            "Drive must contain M2 device slot after onboard"
        );
    }

    /// A peer must persist the vault creation time from the cloud `_meta.json`
    /// into `RECOVERY_PASSPHRASE_CREATED_AT`, so a later keyring republish from
    /// this peer writes the correct `created_at` instead of the `0` fallback.
    #[tokio::test]
    async fn onboard_complete_inner_persists_vault_created_at_from_cloud_meta() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::types::KeyringMetaV2;
        use crate::sync::keyring_v2::KeyringV2Io;

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Overwrite the cloud meta with a distinct, fixed vault creation time so
        // the assertion can't pass by coincidentally matching `now`.
        const VAULT_CREATED_AT: i64 = 1_650_000_000;
        let mut meta = kio::read_meta(&provider).await.unwrap().unwrap();
        meta = KeyringMetaV2 {
            created_at: VAULT_CREATED_AT,
            updated_at: VAULT_CREATED_AT,
            recovery_generation: 9,
            ..meta
        };
        kio::write_meta(&provider, &meta).await.unwrap();
        provider
            .write_file(
                crate::sync::sync_control::SYNC_CONTROL_DRIVE_PATH,
                &serde_json::to_vec(&crate::sync::sync_control::SyncControlV1 {
                    version: crate::sync::sync_control::SYNC_CONTROL_VERSION,
                    recovery_generation: 9,
                    recovery_lease: None,
                    updated_at: VAULT_CREATED_AT,
                })
                .unwrap(),
            )
            .await
            .unwrap();

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 Device",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("must succeed");

        let persisted = db::get_setting(&new_conn, db::RECOVERY_PASSPHRASE_CREATED_AT)
            .unwrap()
            .and_then(|s| s.parse::<i64>().ok());
        assert_eq!(
            persisted,
            Some(VAULT_CREATED_AT),
            "peer must persist the vault created_at from cloud meta, not its own onboard time"
        );
        let adopted_generation = db::get_sync_recovery_generation(&new_conn).unwrap();
        assert_eq!(adopted_generation, 9);
        assert!(
            crate::sync::recovery::authorize_recovery_push(None, 9, adopted_generation, None)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn onboard_complete_inner_rejects_stale_keyring_generation_before_publish() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;
        let mut meta = kio::read_meta(&provider).await.unwrap().unwrap();
        meta.recovery_generation = 3;
        kio::write_meta(&provider, &meta).await.unwrap();
        let slots_before = kio::list_device_slots(&provider).await.unwrap().len();

        let error = onboard_complete_inner(
            setup(),
            &EncryptionKeyState::new(),
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 Device",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect_err("stale keyring meta must be rejected");

        assert!(error.1.contains("generation mismatch"));
        assert_eq!(
            kio::list_device_slots(&provider).await.unwrap().len(),
            slots_before
        );
    }

    /// _meta.json epoch is unchanged after onboarding; only updated_at moves.
    #[tokio::test]
    async fn onboard_complete_inner_epoch_unchanged() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let original_meta = kio::read_meta(&provider).await.unwrap().unwrap();
        let original_epoch = original_meta.epoch;

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 Device",
            None,
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("must succeed");

        let updated_meta = kio::read_meta(&provider).await.unwrap().unwrap();
        assert_eq!(
            updated_meta.epoch, original_epoch,
            "_meta.json epoch must not change after onboarding"
        );
    }

    /// Multi-device: M1 sets up, M2 onboards, M3 onboards. Drive has three slots.
    #[tokio::test]
    async fn onboard_complete_inner_multi_device() {
        use crate::sync::keyring_v2::io as kio;
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::KeyringV2Io;

        // M1: first-time setup + publish.
        let (_conn1, mnemonic, pub_ctx) = seed_v2_vault("masterpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Count slots after M1 setup: should be exactly 1.
        let slots_after_m1 = kio::list_device_slots(&provider).await.unwrap();
        assert_eq!(slots_after_m1.len(), 1, "exactly 1 slot after M1 setup");

        // M2 onboards.
        let conn2 = setup();
        let ks2 = EncryptionKeyState::new();
        onboard_complete_inner(
            conn2,
            &ks2,
            &provider,
            &mnemonic,
            "m2password123",
            "M2 Device",
            None,
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("M2 onboard must succeed");

        let slots_after_m2 = kio::list_device_slots(&provider).await.unwrap();
        assert_eq!(slots_after_m2.len(), 2, "2 slots after M2 onboards");

        // M3 onboards.
        let conn3 = setup();
        let ks3 = EncryptionKeyState::new();
        onboard_complete_inner(
            conn3,
            &ks3,
            &provider,
            &mnemonic,
            "m3password123",
            "M3 Device",
            None,
            None, // no pending Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("M3 onboard must succeed");

        let slots_after_m3 = kio::list_device_slots(&provider).await.unwrap();
        assert_eq!(slots_after_m3.len(), 3, "3 slots after M3 onboards");

        // All three devices can unwrap to the same master key.
        let recovery_slot = kio::read_recovery(&provider).await.unwrap().unwrap();
        let mnemonic_parsed =
            crate::utils::recovery::validate_recovery_mnemonic(&mnemonic).unwrap();
        let recovery_key = crate::utils::recovery::derive_recovery_key(&mnemonic_parsed);
        let master_key =
            unwrap_master_with_recovery_key(&recovery_slot.wrapped_master, &*recovery_key)
                .expect("must unwrap");
        let expected_fp = hex::encode(crate::utils::encryption::key_fingerprint(&*master_key));
        // Each M1/M2/M3 device joined with a DIFFERENT local password, using only
        // `_recovery.json` + `_content.json` — no cloud device slot ever carried
        // key material (design §3/§4: metadata-only device slots, requirement 4).
        // Verify by matching the fingerprint stored in cloud meta (computed from
        // the original master) and asserting every slot is pure registry metadata.
        let meta = kio::read_meta(&provider).await.unwrap().unwrap();
        assert_eq!(
            meta.master_fingerprint, expected_fp,
            "meta fingerprint must match master"
        );
        // Read RAW bytes off the provider for each slot — asserting via the
        // parsed `DeviceSlotV2` would be meaningless, since that struct cannot
        // carry these fields anymore by construction.
        for slot in &slots_after_m3 {
            let raw = provider
                .read_file(&crate::sync::keyring_v2::io::device_slot_path(
                    &slot.device_id,
                ))
                .await
                .unwrap();
            let raw_text = String::from_utf8_lossy(&raw);
            assert!(
                !raw_text.contains("wrapped_master") && !raw_text.contains("kek_salt"),
                "device slot for {} must carry no key material: {raw_text}",
                slot.device_id
            );
        }
    }

    /// A freshly onboarded device has no prior KDF preset to inherit, so it must
    /// wrap under FAST. Asserts that the persisted `WRAPPED_ENCRYPTION_KEY_KEY`
    /// blob carries FAST params (134-hex self-describing format).
    #[tokio::test]
    async fn onboard_complete_inner_wraps_under_fast() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None, // skip boot file in test
            None, // no Drive session
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner must succeed");

        let wrapped = db::get_setting(&new_conn, db::WRAPPED_ENCRYPTION_KEY_KEY)
            .unwrap()
            .expect("wrapped key must be set");
        let blob = hex::decode(&wrapped).unwrap();
        assert_eq!(
            crate::utils::encryption::parse_kek_params(&blob)
                .unwrap()
                .m_cost,
            KdfParams::FAST.m_cost,
            "onboarded device must wrap under FAST"
        );
    }

    /// Test 3: When session_id is None, inner flips encryption_mode=Password
    /// but does NOT write any sync settings.
    #[tokio::test]
    async fn onboard_complete_inner_without_session_id_skips_sync_persistence() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let conn2 = setup();
        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            conn2,
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None, // skip boot file in test
            None, // no session_id
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner without session_id must succeed");

        // encryption_mode must be Password (normal onboarding completes).
        assert_eq!(
            db::get_encryption_mode(&new_conn).unwrap(),
            db::EncryptionMode::Password,
            "encryption_mode must be Password"
        );

        // No sync settings should be written.
        assert!(
            db::get_setting(&new_conn, "sync_connected")
                .unwrap()
                .is_none(),
            "sync_connected must NOT be set when session_id is None"
        );
        assert!(
            db::get_setting(&new_conn, "gdrive_refresh_token")
                .unwrap()
                .is_none(),
            "gdrive_refresh_token must NOT be set when session_id is None"
        );
        assert!(
            !db::get_sync_enabled(&new_conn).unwrap(),
            "sync_enabled must NOT be true when session_id is None"
        );
    }

    /// I2: `onboard_complete_inner` must return `Err` (not panic) when
    /// `_content.json` exists but has an empty `entries` array.  An attacker or
    /// corrupt cloud file can plant such a file; the old `.expect()` would panic.
    #[tokio::test]
    async fn onboard_complete_inner_empty_content_list_returns_err_not_panic() {
        use crate::sync::keyring_v2::{
            io::{test_support::InMemoryKeyringProvider, write_content},
            types::{ContentListV2, KEYRING_V2_VERSION},
        };

        // Build a minimal provider with a valid _meta.json + _recovery.json
        // but an empty _content.json.
        let (_conn_seed, mnemonic, pub_ctx) = seed_v2_vault("testpass9999", "SeedDevice");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        // Overwrite _content.json with an empty entries list (latest_epoch=0).
        let empty_content = ContentListV2 {
            version: KEYRING_V2_VERSION,
            latest_epoch: 0,
            entries: vec![],
            created_at: 0,
        };
        write_content(&provider, &empty_content)
            .await
            .expect("write empty content list must succeed");

        let conn = setup();
        let ks = EncryptionKeyState::new();
        let result = onboard_complete_inner(
            conn,
            &ks,
            &provider,
            &mnemonic,
            "newpass9999",
            "Joiner",
            None,
            None,
            None, // unlock_method (existing test — defaults to password-only)
            &FakeKeystoreHooks::default(),
        )
        .await;

        assert!(
            result.is_err(),
            "onboard_complete_inner must return Err when _content.json has empty entries"
        );
        let (_conn, err) = result.unwrap_err();
        assert!(
            err.contains("no entries") || err.contains("empty") || err.contains("corrupt"),
            "error must mention the empty/corrupt content list; got: {err}"
        );
    }

    /// C1: `onboard_validate_passphrase_inner` succeeds when the correct phrase
    /// for THIS vault is supplied — the happy path guard for both commands.
    #[tokio::test]
    async fn c1_vault_match_accepts_correct_phrase() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let (_conn, mnemonic, pub_ctx) = seed_v2_vault("goodpass12345", "DeviceOK");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let result = onboard_validate_passphrase_inner(&provider, &mnemonic).await;
        assert!(
            result.is_ok(),
            "vault-match must accept the correct phrase; err: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn phase3_onboard_complete_records_biometric_setting_for_resolved_method() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        let (_seed_conn, mnemonic, pub_ctx) = seed_v2_vault("seedpass123", "M1");
        let provider = InMemoryKeyringProvider::new();
        seed_provider_from_ctx(&provider, &pub_ctx).await;

        let key_state = EncryptionKeyState::new();
        let new_conn = onboard_complete_inner(
            setup(),
            &key_state,
            &provider,
            &mnemonic,
            "newdevpassword12",
            "M2 MacBook",
            None,
            None,
            None, // unlock_method → resolves to Password
            &FakeKeystoreHooks::default(),
        )
        .await
        .expect("onboard_complete_inner must succeed");

        assert_eq!(
            db::get_setting(&new_conn, crate::vault::BIOMETRIC_ENABLED_SETTING).unwrap(),
            Some("false".to_string()),
            "join must state the resolved unlock method in the setting, not leave it absent"
        );
    }
}
