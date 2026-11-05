//! Force re-pair: detect a vault rotated on another device, persist and clear
//! the fail-closed `FORCE_RE_PAIR_REQUIRED` flag, complete the re-pair with
//! the recovery phrase, and the push guards every sync write path checks.
//!
//! Helpers that build the configured provider themselves take a
//! [`CloudCredentialStore`]; the others take a ready provider.
use std::path::Path;

use super::cloud_config::configured_cloud_provider;
use super::cloud_config::CloudCredentialStore;
use crate::db;
use crate::vault::onboard::onboard_complete_inner;
use crate::vault::onboard::onboard_validate_passphrase_inner;
use crate::vault::VaultPlatformHooks;
use crate::AppState;
use crate::EncryptionKeyState;

/// Result from `detect_force_re_pair` when a mismatch is found.
#[derive(Debug)]
#[doc(hidden)]
pub struct ForceRePairMismatch {
    pub reason: String,
    pub cloud_epoch: u64,
}

/// Non-error outcome of [`detect_force_re_pair`].
///
/// `Verified` and `Skipped` are kept apart because only `Verified` is a
/// positive statement about the keyring.  Collapsing both into "clean" is what
/// makes clearing the fail-closed flag unsafe: a missing `_meta.json` would
/// look identical to a fingerprint that actually matched.
#[derive(Debug)]
#[doc(hidden)]
pub enum ForceRePairCheck {
    /// Cloud fingerprint matched local truth AND epochs were equal.  This is
    /// the only outcome that proves the vault is in sync.
    Verified,
    /// Nothing to compare — no cloud keyring exists yet, or this device has no
    /// local fingerprint (setup incomplete).  NOT a verification: the keyring
    /// may still be rotated.  Callers must not treat this as proof of health.
    Skipped,
    /// Cloud vault was rotated; user must re-pair.
    Mismatch(ForceRePairMismatch),
}

/// Decision produced by [`resolve_force_re_pair_outcome`].
///
/// Separating this from the async I/O makes the routing logic unit-testable
/// without requiring a real Drive provider.
#[doc(hidden)]
pub enum ForceRePairDecision {
    /// `Ok(Verified)` — fingerprints match; normal reconnect may proceed AND a
    /// stale flag may be cleared (see [`clear_force_re_pair`]).
    Verified,
    /// `Ok(Skipped)` — no comparison was possible.  Reconnect may proceed, but
    /// an existing flag MUST be left in place.
    Skipped,
    /// `Ok(Mismatch(_))` — cloud vault was rotated; user must re-pair.
    NeedsRepair(ForceRePairMismatch),
    /// `Err(_)` — keyring check was inconclusive (I/O failure).
    ///
    /// Fail-closed: treated as a hard error so an unverified keyring cannot
    /// reach a pushable connected state.  The caller must set
    /// `FORCE_RE_PAIR_REQUIRED=1` to block push until the user re-pairs.
    ///
    /// `is_auth` carries the TYPED [`SyncError::Auth`] verdict from
    /// [`DetectError`] — dead credentials say nothing about vault integrity, so
    /// that case routes to "reconnect Drive" instead of the mnemonic re-pair.
    /// It must never be re-derived by substring-matching `message`, which
    /// embeds cloud-controlled text.
    Inconclusive { message: String, is_auth: bool },
}

/// Map the raw `detect_force_re_pair` result to a [`ForceRePairDecision`].
///
/// This is a pure function with no side effects.  Callers are responsible for
/// acting on the decision (setting DB flags, emitting events, returning errors).
#[doc(hidden)]
pub fn resolve_force_re_pair_outcome(
    result: Result<ForceRePairCheck, DetectError>,
) -> ForceRePairDecision {
    match result {
        Ok(ForceRePairCheck::Verified) => ForceRePairDecision::Verified,
        Ok(ForceRePairCheck::Skipped) => ForceRePairDecision::Skipped,
        Ok(ForceRePairCheck::Mismatch(mismatch)) => ForceRePairDecision::NeedsRepair(mismatch),
        Err(e) => ForceRePairDecision::Inconclusive {
            message: e.message,
            is_auth: e.is_auth,
        },
    }
}

/// Persist the `FORCE_RE_PAIR_REQUIRED=1` flag and reason that block push
/// after an `Inconclusive` keyring check.
///
/// This is the load-bearing fail-closed side-effect shared by every call site
/// that handles [`ForceRePairDecision::Inconclusive`].  Extracting it means:
/// - Both call sites inherit loud-error-on-write-failure for free.
/// - A test can call this directly: deleting the production writes breaks the
///   test, proving the flag is actually set (not just replicated in the test).
///
/// Returns `Err` when either `set_setting` write fails so callers that CAN
/// propagate (e.g. Tauri commands returning `Result<_, String>`) do so, making
/// the failure visible in the command error path as well as the log.
#[doc(hidden)]
pub fn apply_inconclusive_force_re_pair(
    conn: &rusqlite::Connection,
    reason: &str,
) -> Result<(), String> {
    // Never downgrade a `vault_rotated` reason. The reason is provenance, not a
    // status line: `recheck_force_re_pair` clears an auth-failed flag only when
    // the reason is `keyring_check_inconclusive`, so overwriting a rotation
    // reason here would let the sequence
    //   vault_rotated → one inconclusive check → auth failure → clear
    // erase protection for a vault that really did move.
    if reason == "keyring_check_inconclusive" {
        let existing = db::get_setting(conn, db::FORCE_RE_PAIR_REASON)
            .map_err(|e| format!("apply_inconclusive_force_re_pair: read reason: {e}"))?;
        if existing.as_deref() == Some("vault_rotated") {
            log::warn!(
                "apply_inconclusive_force_re_pair: keeping existing vault_rotated reason \
                 (an inconclusive check must not downgrade a known rotation)"
            );
            return db::set_setting(conn, db::FORCE_RE_PAIR_REQUIRED, "1").map_err(|e| {
                let msg = format!("CRITICAL: failed to set FORCE_RE_PAIR_REQUIRED: {e}");
                log::error!("{msg}");
                msg
            });
        }
    }
    set_force_re_pair_flags(conn, reason).map_err(|e| {
        let msg = format!(
            "CRITICAL: failed to write fail-closed force-re-pair flags — \
             push may not be blocked: {e}"
        );
        log::error!("{msg}");
        msg
    })
}

/// Persist `FORCE_RE_PAIR_REQUIRED=1` and `FORCE_RE_PAIR_REASON` in ONE
/// transaction.
///
/// Every mismatch writer used to issue the two `set_setting` calls as separate
/// autocommit writes; a failure between them left the flag set with a stale
/// reason (or a fresh reason without the flag). Both rows move together or not
/// at all — the write-side mirror of [`clear_force_re_pair`].
#[doc(hidden)]
pub fn set_force_re_pair_flags(conn: &rusqlite::Connection, reason: &str) -> Result<(), String> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| format!("set_force_re_pair_flags: begin: {e}"))?;
    let result = db::set_setting(conn, db::FORCE_RE_PAIR_REQUIRED, "1")
        .map_err(|e| format!("set_force_re_pair_flags: set FORCE_RE_PAIR_REQUIRED: {e}"))
        .and_then(|_| {
            db::set_setting(conn, db::FORCE_RE_PAIR_REASON, reason)
                .map_err(|e| format!("set_force_re_pair_flags: set FORCE_RE_PAIR_REASON: {e}"))
        });
    match result {
        // Same COMMIT-failure discipline as `clear_force_re_pair`: a failed
        // COMMIT leaves the transaction open on this shared connection, so roll
        // back rather than let later reads see half-applied writes.
        Ok(()) => conn.execute_batch("COMMIT").map_err(|e| {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                log::error!("set_force_re_pair_flags: rollback after failed commit failed: {rb}");
            }
            format!("set_force_re_pair_flags: commit: {e}")
        }),
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                log::error!("set_force_re_pair_flags: rollback failed: {rb}");
            }
            Err(e)
        }
    }
}

/// Why a keyring check could not reach a verdict.
///
/// `is_auth` is derived from the TYPED [`SyncError::Auth`] variant, never from
/// substring-matching the rendered message.  That distinction is load-bearing:
/// the message embeds cloud-controlled data (`KeyringMetaV2` uses
/// `deny_unknown_fields`, so serde echoes an attacker-chosen field name into the
/// error), and a `_meta.json` carrying a field named `GDRIVE_TOKEN_REVOKED` would
/// otherwise be misread as "auth is dead" and skip the fail-closed flag while the
/// token was still valid — letting push proceed on an unverified keyring.
#[derive(Debug)]
#[doc(hidden)]
pub struct DetectError {
    pub message: String,
    /// True only for [`SyncError::Auth`] — Drive rejected our credentials.
    pub is_auth: bool,
    /// True only for [`SyncError::Serialization`] on the `_meta.json` read —
    /// the file EXISTS but is unparseable or semantically invalid. Unlike a
    /// network blip this is a positive integrity signal, so the best-effort
    /// sync pull leg treats it as fail-closed instead of swallowing it.
    pub is_corrupt: bool,
}

impl DetectError {
    /// Non-auth failure (I/O, serialization, state lock). Fail-closed.
    fn other(message: String) -> Self {
        Self {
            message,
            is_auth: false,
            is_corrupt: false,
        }
    }
}

impl From<String> for DetectError {
    fn from(message: String) -> Self {
        Self::other(message)
    }
}

/// Clear the force-re-pair flags after a *positively verified* keyring check.
///
/// Only call this from a [`ForceRePairDecision::Verified`] arm reached WITHOUT a
/// preceding keyring re-upload — see [`recheck_force_re_pair`].  Two ways to get
/// this wrong, both fail-open:
///
/// - Clearing on [`ForceRePairDecision::Skipped`]: "no cloud `_meta.json`" and
///   "no local fingerprint" are not proof the vault is in sync, so a legitimate
///   `vault_rotated` flag could be wiped by a transient absence.
/// - Clearing on a `Verified` that follows a `keyring_dirty` re-upload: the
///   re-upload rewrites cloud `_meta.json` from local truth, so the device would
///   be verifying against its own write.  That is why the reconcile arms in
///   `startup_keyring_reconcile_if_needed` deliberately do NOT clear.
///
/// Given those two rules, clearing is safe regardless of which reason originally
/// set the flag: a real mismatch cannot reach `Verified` against uncorrupted
/// cloud state, because a differing fingerprint or newer cloud epoch routes to
/// `Mismatch` in `detect_force_re_pair`.
///
/// Without this, the fail-closed flag set by an `Inconclusive` check (e.g. a
/// transient Drive I/O error) would be sticky forever — the user would be stuck
/// on the re-pair screen even after connectivity recovered.
/// Both deletes run in one transaction. Two autocommit deletes could drop
/// `FORCE_RE_PAIR_REQUIRED` (the push guard) and then fail on the reason, leaving
/// push unblocked while the command reports an error — the flags must move
/// together or not at all.
#[doc(hidden)]
pub fn clear_force_re_pair(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| format!("clear_force_re_pair: begin: {e}"))?;
    let result = db::delete_setting(conn, db::FORCE_RE_PAIR_REQUIRED)
        .map_err(|e| format!("clear_force_re_pair: delete FORCE_RE_PAIR_REQUIRED: {e}"))
        .and_then(|_| {
            db::delete_setting(conn, db::FORCE_RE_PAIR_REASON)
                .map_err(|e| format!("clear_force_re_pair: delete FORCE_RE_PAIR_REASON: {e}"))
        });
    match result {
        // A failed COMMIT leaves the transaction open, and this same connection
        // would then read the deletes as applied — push unblocked while the caller
        // was told the clear failed. Roll back so the guard is restored.
        Ok(()) => conn.execute_batch("COMMIT").map_err(|e| {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                log::error!("clear_force_re_pair: rollback after failed commit failed: {rb}");
            }
            format!("clear_force_re_pair: commit: {e}")
        }),
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                log::error!("clear_force_re_pair: rollback failed: {rb}");
            }
            Err(e)
        }
    }
}

/// Clear a stale `keyring_check_inconclusive` flag when the cloud holds no
/// COMMITTED keyring.  Returns `true` when a flag was cleared.
///
/// `cloud_keyring_present` must come from a typed `NotFound` on `_meta.json` read
/// with a live token — `gdrive_complete_connect` establishes exactly that at its
/// probe, where auth/network/quota failures return `Err` and abort the connect
/// instead. "Couldn't read the keyring" must never arrive here as `false`. The
/// flag is taken as a parameter rather than re-probed so that a caller holding a
/// present keyring cannot reach the clear at all.
///
/// Precise about what absence proves: `builder::publish_initial_v2_keyring` writes
/// `_meta.json` LAST, as the commit marker, so its absence means "no committed
/// keyring" — not "the folder is empty". A peer mid-initial-publish may already
/// have written `_recovery.json` and a device slot. That race exists today for
/// every unflagged device (which takes this same publish branch with no gate at
/// all) and is tracked as the missing-CAS root cause in
/// `docs/plans/2026-07-14-keyring-fail-open-findings.md`. This function does not
/// widen the race window or its mechanism; it does let one more class of device
/// (inconclusive-flagged ones, previously deadlocked) into a race unflagged
/// devices already run.
///
/// Not fail-open. The publish gate in `publish_v2_keyring_with_provider` exists to
/// stop a stale device from clobbering a peer's rotation — and a COMMITTED peer
/// rotation always lands `_meta.json`, so it routes to the reconcile branch and is
/// still caught there as `vault_rotated`. What is left here has no committed peer
/// state to clobber, so the gate protects nothing while blocking the only path
/// that rebuilds the vault. Without this, a device whose flag was written by a dead
/// OAuth token can never recover once the cloud keyring is gone: publish is gated,
/// the recheck reads no `_meta.json` and returns `Skipped`, and the mnemonic
/// re-pair needs a `_recovery.json` that no longer exists.
///
/// Allowlist, not denylist: only the exact reason `keyring_check_inconclusive` is
/// cleared, so an unknown or misspelled reason fails closed. `vault_rotated` in
/// particular stays put — a peer really did move the vault, and this device's key
/// material is known-stale whatever the cloud currently holds.
#[doc(hidden)]
pub fn clear_stale_flag_for_empty_cloud(
    conn: &rusqlite::Connection,
    cloud_keyring_present: bool,
) -> Result<bool, String> {
    if cloud_keyring_present {
        return Ok(false);
    }
    if !force_re_pair_required(conn) {
        return Ok(false);
    }
    let reason = db::get_setting(conn, db::FORCE_RE_PAIR_REASON)
        .map_err(|e| format!("clear_stale_flag_for_empty_cloud: read reason: {e}"))?;
    if reason.as_deref() != Some("keyring_check_inconclusive") {
        log::warn!(
            "clear_stale_flag_for_empty_cloud: cloud keyring absent but flag reason is {reason:?} \
             — leaving the flag in place"
        );
        return Ok(false);
    }
    clear_force_re_pair(conn)?;
    log::warn!(
        "clear_stale_flag_for_empty_cloud: cloud keyring is absent and the flag came from an \
         inconclusive check — clearing it so this device can republish the vault"
    );
    Ok(true)
}

/// Act on the SECOND detect taken after a `Verified` clear (remote TOCTOU
/// guard). Takes the RAW detect result (not the resolved decision) so the
/// typed `is_corrupt` verdict is not lost:
///
/// - `Mismatch` re-arms the flag — the rotation landed inside the detect→clear
///   window.
/// - A corrupt `_meta.json` re-arms it too (same fail-closed policy as the
///   pull leg): the cloud turned unverifiable inside the window.
/// - Everything else leaves the clear in place: a transient failure here must
///   not re-block push (the reconcile paths re-detect on their own), and
///   `Skipped`/`Verified` carry no rotation evidence.
#[doc(hidden)]
pub fn confirm_post_clear_verification(
    conn: &rusqlite::Connection,
    second: Result<ForceRePairCheck, DetectError>,
) -> Result<bool, String> {
    match second {
        Ok(ForceRePairCheck::Mismatch(mismatch)) => {
            set_force_re_pair_flags(conn, &mismatch.reason)?;
            log::warn!(
                "recheck_force_re_pair: vault rotated during the recheck window ({}) — \
                 flag re-armed",
                mismatch.reason
            );
            Ok(false)
        }
        Err(e) if e.is_corrupt => {
            apply_inconclusive_force_re_pair(conn, "keyring_check_inconclusive")?;
            log::warn!(
                "recheck_force_re_pair: _meta.json turned corrupt during the recheck window \
                 ({}) — flag re-armed",
                e.message
            );
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Act on a recheck [`ForceRePairDecision`]: clear the flag on `Verified`, leave
/// it standing otherwise.
///
/// Split out from [`recheck_force_re_pair`] so the dispatch that actually carries
/// the safety property is unit-testable without a live Drive provider.  Moving the
/// `clear_force_re_pair` call into any arm other than `Verified` must break a test.
#[doc(hidden)]
pub fn apply_recheck_decision(
    conn: &rusqlite::Connection,
    decision: ForceRePairDecision,
) -> Result<bool, String> {
    match decision {
        ForceRePairDecision::Verified => {
            clear_force_re_pair(conn)?;
            log::info!("recheck_force_re_pair: fingerprint verified — stale flag cleared");
            Ok(true)
        }
        ForceRePairDecision::NeedsRepair(mismatch) => {
            // Upgrade the reason to the rotation we just observed. Without this, a
            // flag first written as `keyring_check_inconclusive` would keep that
            // reason even after a real rotation is detected here — and a later auth
            // failure would then clear it as if it had only ever been inconclusive.
            // `apply_inconclusive_force_re_pair` refuses the reverse downgrade, so
            // once upgraded the reason stays put. The transactional write also
            // re-asserts `FORCE_RE_PAIR_REQUIRED=1` so both rows stay consistent.
            set_force_re_pair_flags(conn, &mismatch.reason)
                .map_err(|e| format!("recheck_force_re_pair: upgrade reason: {e}"))?;
            log::info!(
                "recheck_force_re_pair: mismatch stands ({}) — re-pair still required",
                mismatch.reason
            );
            Ok(false)
        }
        ForceRePairDecision::Skipped => {
            // Nothing to compare — not a verification, so the flag stands.
            log::info!("recheck_force_re_pair: nothing to compare — flag stands");
            Ok(false)
        }
        ForceRePairDecision::Inconclusive {
            message: e,
            is_auth,
        } => {
            // Dead Drive auth cannot verify the keyring — but a flag whose reason is
            // `keyring_check_inconclusive` was itself written by a check that failed
            // this same way, and such a flag is otherwise inescapable: every retry
            // fails auth identically. Clear it and let the user reconnect Drive.
            //
            // Not fail-open: a genuine rotation carries reason `vault_rotated`, and
            // `apply_inconclusive_force_re_pair` refuses to downgrade that reason, so
            // a real rotation cannot masquerade as an inconclusive one. Push stays
            // blocked meanwhile (no valid token), and `gdrive_complete_connect`
            // re-runs the fingerprint check on reconnect — re-setting the flag if the
            // vault really did move.
            let reason = db::get_setting(conn, db::FORCE_RE_PAIR_REASON)
                .map_err(|err| format!("recheck_force_re_pair: read reason: {err}"))?;
            if is_auth && reason.as_deref() == Some("keyring_check_inconclusive") {
                clear_force_re_pair(conn)?;
                log::warn!(
                    "recheck_force_re_pair: Drive auth revoked and the flag came from an \
                     inconclusive check — clearing it; reconnect Drive to resume syncing"
                );
                return Ok(true);
            }
            log::warn!("recheck_force_re_pair: check inconclusive ({e}) — flag stands");
            Err(format!("recheck_force_re_pair: keyring check failed: {e}"))
        }
    }
}

/// Compare the cloud `_meta.json` fingerprint + epoch against local settings.
///
/// Returns `Mismatch` when:
/// - The cloud fingerprint does not match local `cloud_master_fingerprint`, OR
/// - The cloud epoch is strictly greater than the local epoch.
///
/// The fingerprint is the integrity signal — a mismatch means another device
/// rotated the vault even if the epoch hasn't advanced (e.g. a same-epoch
/// rotation). The epoch is the freshness signal — a newer epoch also requires
/// re-pair to pick up the new master key.  Using `||` ensures neither signal
/// is silently ignored.
///
/// Returns `Verified` only when a real comparison ran and both signals agreed.
/// Returns `Skipped` when there was nothing to compare — this is deliberately
/// distinct from `Verified` so callers cannot mistake "couldn't check" for
/// "checked and healthy" when deciding to clear the fail-closed flag.
/// Returns `Err` on I/O failure.
///
/// This helper has no side effects — callers decide whether to set settings
/// and emit events.
#[doc(hidden)]
pub async fn detect_force_re_pair<P>(
    provider: &P,
    state: &AppState,
) -> Result<ForceRePairCheck, DetectError>
where
    P: crate::sync::keyring_v2::KeyringV2Io,
{
    use crate::sync::keyring_v2::io as kio;

    let meta = match kio::read_meta(provider).await {
        Ok(Some(m)) => m,
        // No keyring yet — nothing to compare. Not a verification.
        Ok(None) => return Ok(ForceRePairCheck::Skipped),
        Err(e) => {
            // Two conditions, and BOTH are needed — see `DetectError`:
            //
            // 1. The typed `SyncError::Auth` variant. Cloud file content surfaces as
            //    `Serialization`/`Io`, never `Auth`, so an attacker-authored
            //    `_meta.json` cannot reach this arm however it is worded.
            // 2. The `GDRIVE_TOKEN_REVOKED` marker. `SyncError::Auth` alone is far too
            //    broad: `gdrive_provider.rs` maps EVERY non-scope refresh failure —
            //    network drop, 5xx, parse error — to `Auth`, so trusting the variant
            //    alone would let a transient blip pose as a revoked token and clear the
            //    fail-closed flag. The marker is hardcoded in `gdrive_oauth.rs` on 401 /
            //    invalid_grant only, and never echoes server-supplied text.
            //
            // Anything else stays `is_auth = false` → fail-closed.
            let is_auth = matches!(&e, crate::sync::provider::SyncError::Auth(msg)
                if msg.contains("GDRIVE_TOKEN_REVOKED"));
            // Corrupt-vs-transient: `Serialization` means the bytes were read
            // but do not form a valid `KeyringMetaV2` — the pull leg's
            // best-effort path must not swallow that (see
            // `check_force_re_pair_sync`).
            let is_corrupt = matches!(&e, crate::sync::provider::SyncError::Serialization(_));
            return Err(DetectError {
                message: format!("detect_force_re_pair: read _meta.json: {e}"),
                is_auth,
                is_corrupt,
            });
        }
    };

    let (local_fp, local_epoch) = {
        let conn = state.lock()?;
        let fp = db::get_setting(&conn, db::CLOUD_MASTER_FINGERPRINT).map_err(|e| e.to_string())?;
        let epoch = db::get_setting(&conn, db::KEYRING_V2_LOCAL_EPOCH)
            .map_err(|e| e.to_string())?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        (fp, epoch)
    };

    // No local fingerprint means this device hasn't completed setup yet — skip.
    // Not a verification: we have no local truth to compare against.
    let Some(local_fp_str) = local_fp else {
        return Ok(ForceRePairCheck::Skipped);
    };

    // Mismatch: fingerprint differs OR epochs disagree → another device rotated,
    // or the cloud was rolled back to an older `_meta.json`.
    // Using || ensures a fingerprint change without an epoch advance is still detected.
    // `!=` (not `>`) so a REPLAYED older epoch is also a mismatch: `Verified` is a
    // positive claim that cloud and local agree, and it authorizes clearing the
    // fail-closed flag — a rollback must never be able to buy that.
    if meta.master_fingerprint != local_fp_str || meta.epoch != local_epoch {
        return Ok(ForceRePairCheck::Mismatch(ForceRePairMismatch {
            reason: "vault_rotated".to_string(),
            cloud_epoch: meta.epoch,
        }));
    }

    Ok(ForceRePairCheck::Verified)
}

/// Whether `FORCE_RE_PAIR_REQUIRED` is currently set.
///
/// Read errors are reported as `true` (fail-closed): callers use this to decide
/// whether to withhold a cloud write, and "we could not tell" must not authorize
/// the write.
#[doc(hidden)]
pub fn force_re_pair_required(conn: &rusqlite::Connection) -> bool {
    match db::get_setting(conn, db::FORCE_RE_PAIR_REQUIRED) {
        Ok(v) => v.as_deref() == Some("1"),
        Err(e) => {
            log::error!("force_re_pair_required: read failed ({e}) — assuming required");
            true
        }
    }
}

/// Read the persisted `force_re_pair_required` / `force_re_pair_reason` pair.
///
/// Returns `Some(reason)` when the vault was rotated on another device and
/// this device has not yet completed re-pairing. Returns `None` otherwise.
///
/// The flag survives app restarts (it lives in the DB settings table), so
/// without this command the frontend Zustand `useForceRePair` store reverts
/// to its default `false` after every relaunch and the user is silently
/// dropped back into the (now-broken) lock-screen flow. The frontend calls
/// this on app mount and hydrates the store from the result.
#[doc(hidden)]
pub fn get_force_re_pair_status_inner(
    conn: &rusqlite::Connection,
) -> Result<Option<String>, String> {
    let required = db::get_setting(conn, db::FORCE_RE_PAIR_REQUIRED)
        .map_err(|e| e.to_string())?
        .filter(|v| !v.is_empty());
    if required.as_deref() != Some("1") {
        return Ok(None);
    }
    let reason = db::get_setting(conn, db::FORCE_RE_PAIR_REASON)
        .map_err(|e| e.to_string())?
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "Vault was rotated on another device.".to_string());
    Ok(Some(reason))
}

/// Clear force-re-pair flags and re-assert sync-connected / sync-enabled.
///
/// Re-writes `sync_provider` only when `prior_sync_provider` is `Some`, so a
/// folder-provider install (`"local"` / `"icloud"`) is not overwritten with
/// `"gdrive"`. A missing prior value is left untouched.
#[doc(hidden)]
pub fn clear_force_re_pair_and_reassert_sync(
    conn: &rusqlite::Connection,
    prior_sync_provider: Option<&str>,
) {
    let _ = db::delete_setting(conn, db::FORCE_RE_PAIR_REQUIRED);
    let _ = db::delete_setting(conn, db::FORCE_RE_PAIR_REASON);
    if let Some(provider) = prior_sync_provider {
        let _ = db::set_setting(conn, "sync_provider", provider);
    }
    let _ = db::set_setting(conn, "sync_connected", "true");
    let _ = db::set_sync_enabled(conn, true);
}

/// Complete a forced re-pair after the vault was rotated by another device.
///
/// This is equivalent to Phase 3's `onboard_complete` but for a device that
/// is already initialized — it replaces the master key with the rotated one.
///
/// Steps:
/// 1. Validate mnemonic; unwrap `_recovery.json` → `master_key_new`.
/// 2. Verify fingerprint matches `_meta.json.master_fingerprint`.
/// 3. Generate new `kek_salt`, wrap with `new_local_password`.
/// 4. Write boot file + PRAGMA rekey.
/// 5. Upload new `devices/<id>.json`.
/// 6. Clear `force_re_pair_required` and `force_re_pair_reason`.
///
/// The caller validates the new password's length and resolves `provider`
/// and `boot_path` first (the desktop command does both before calling
/// this); everything from the passphrase check on happens here.
#[doc(hidden)]
pub async fn complete_force_re_pair_inner<P: crate::sync::keyring_v2::KeyringV2Io>(
    state: &AppState,
    key_state: &EncryptionKeyState,
    provider: &P,
    recovery_mnemonic: &str,
    new_local_password: &str,
    boot_path: Option<&Path>,
    hooks: &dyn VaultPlatformHooks,
) -> Result<(), String> {
    // Validate passphrase BEFORE touching the DB connection. Steps 1-3 of
    // onboard_complete_inner only need the provider + mnemonic — they never
    // touch the DB. By bailing here on a wrong passphrase we keep AppState
    // completely untouched, so the user's retry still finds the real DB.
    onboard_validate_passphrase_inner(provider, recovery_mnemonic).await?;

    // Read the current device name AND the current `sync_provider` BEFORE
    // swapping the connection. The previous shape swapped first, then
    // queried — but the placeholder we swapped in was a bare `:memory:`
    // connection with no schema, so `db::list_devices` blew up with `no
    // such table: devices` and the user saw "no such table" instead of a
    // re-pair flow. Read first, swap second, and migrate the placeholder so
    // any other code that hits the state lock during the rekey window sees
    // a usable (if empty) DB. `sync_provider` is captured here so the
    // post-success re-assert writes the same value back (skip when unset)
    // instead of hardcoding `"gdrive"`.
    let (device_name, prior_sync_provider) = {
        let conn = state.lock()?;
        let device_name = db::list_devices(&conn)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|d| d.is_current)
            .map(|d| d.name)
            .unwrap_or_else(|| "My Device".to_string());
        let prior_sync_provider = db::get_sync_provider(&conn).map_err(|e| e.to_string())?;
        (device_name, prior_sync_provider)
    };

    // Extract the real DB connection to pass to the inner function.
    // Pattern mirrors onboard_complete: swap in a fully-migrated placeholder,
    // work on real conn, swap back.
    let real_conn = {
        let placeholder = rusqlite::Connection::open_in_memory()
            .map_err(|e| format!("placeholder conn failed: {e}"))?;
        db::schema::migrate(&placeholder)
            .map_err(|e| format!("Failed to migrate placeholder: {e}"))?;
        db::set_encryption_mode(&placeholder, db::EncryptionMode::Unset)
            .map_err(|e| format!("Failed to seed placeholder mode: {e}"))?;
        let mut guard = state.lock()?;
        std::mem::replace(&mut *guard, placeholder)
    };

    let result = onboard_complete_inner(
        real_conn,
        key_state,
        provider,
        recovery_mnemonic,
        new_local_password,
        &device_name,
        boot_path,
        None, // no pending Drive session from the force-re-pair path
        None, // unlock_method (existing test — defaults to password-only)
        hooks,
    )
    .await;

    match result {
        Ok(real_conn) => {
            // Swap real connection back.
            state.set_connection(real_conn)?;

            // Clear force re-pair flags AND re-assert the sync-provider /
            // sync-enabled settings. `persist_connect_settings` wrote both
            // earlier in `gdrive_complete_connect`, but the user-visible
            // "Enable sync" toggle reads `sync_provider` for its disabled
            // prop. If we hit any flow that landed force-re-pair without
            // persist_connect_settings running first (or a stale snapshot
            // taken before the connect), the toggle would stay frozen-off
            // even though the vault is healthy. Writes are idempotent.
            // Re-assert the provider captured before the swap so a folder
            // install (`local` / `icloud`) is not overwritten with `gdrive`.
            {
                let conn = state.lock()?;
                clear_force_re_pair_and_reassert_sync(&conn, prior_sync_provider.as_deref());
            }

            Ok(())
        }
        Err((real_conn, e)) => {
            // Restore the real connection so the placeholder doesn't linger in
            // AppState. The real conn is always valid (it has the correct key,
            // whatever state it was in before the swap), so swapping it back
            // allows the user to retry without the app being stuck on an empty
            // placeholder DB. The passphrase was validated before the swap, so
            // wrong-passphrase cases never reach here; any error here is a
            // post-swap inner error (DB write / rekey). Retry may succeed if
            // the failure was transient (I/O, Drive timeout, etc.).
            let _ = state.set_connection(real_conn);
            Err(e)
        }
    }
}

/// Unified guard for all sync write paths.
///
/// Checks both conditions that must be clear before pushing any envelope to
/// the cloud:
///
/// 1. **Force-re-pair**: another device rotated the vault and this device must
///    re-pair before syncing. Detected via the persisted `FORCE_RE_PAIR_REQUIRED`
///    setting.
/// 2. **Active rotation job**: a rotation is in progress or paused (e.g. after
///    the retry-abort path in `reencrypt_items`). Pushing K_old envelopes over
///    files already encrypted with K_new causes permanent data loss. The
///    persistent `rotation_jobs` table catches this even after the in-memory
///    `SyncInProgressGuard` has been dropped.
///
/// Called at the earliest possible point in every push entry path
/// (`run_push_entry`, `run_sync_now`) before any I/O is performed.
#[doc(hidden)]
pub fn ensure_safe_to_push_with_recovery_permit(
    state: &AppState,
    permit: Option<&crate::sync::recovery::RecoveryOwnerPermit>,
) -> Result<(), String> {
    let conn = state.lock()?;
    // Force-re-pair check (cheaper — single settings row read).
    if db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some("1")
    {
        let reason_raw = db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
            .unwrap_or(None)
            .unwrap_or_default();
        let (reason_class, human_msg) = if reason_raw == "keyring_check_inconclusive" {
            ("inconclusive", "The cloud keyring could not be verified")
        } else {
            ("rotated", "The vault was rotated on another device")
        };
        log::warn!("ensure_safe_to_push: blocked — force_re_pair_required=1 reason={reason_raw}");
        return Err(format!(
            "FORCE_RE_PAIR_REQUIRED reason={reason_class}: {human_msg}. Re-pair before syncing."
        ));
    }
    // Persistent rotation-job check (catches paused/aborted-with-resume jobs
    // where the in-memory SyncInProgressGuard was already dropped).
    if db::has_active_rotation_job(&conn).map_err(|e| e.to_string())? {
        log::warn!(
            "ensure_safe_to_push: blocked — active rotation job exists (paused or in-flight)"
        );
        return Err("A vault rotation is in progress or needs to resume. \
             Wait for it to finish or click Resume in Settings."
            .to_string());
    }
    if let Some(job) = db::find_active_sync_recovery_job(&conn).map_err(|e| e.to_string())? {
        let allowed = permit.is_some_and(|permit| {
            let local_device_id = db::get_or_create_device_id(&conn).ok();
            permit.job_id == job.id
                && permit.operation == job.operation
                && i64::try_from(permit.recovery_generation).ok() == Some(job.recovery_generation)
                && local_device_id.as_deref() == Some(permit.owner_device_id.as_str())
        });
        if !allowed {
            return Err("authoritative_recovery_in_progress".to_string());
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn ensure_safe_to_push(state: &AppState) -> Result<(), String> {
    ensure_safe_to_push_with_recovery_permit(state, None)
}

#[doc(hidden)]
pub async fn ensure_remote_recovery_push_allowed(
    state: &AppState,
    permit: Option<&crate::sync::recovery::RecoveryOwnerPermit>,
    creds: &dyn CloudCredentialStore,
) -> Result<(), String> {
    let (provider, local_generation) = {
        let conn = state.lock()?;
        let Some(provider) = configured_cloud_provider(&conn, creds)? else {
            return Ok(());
        };
        (
            provider,
            db::get_sync_recovery_generation(&conn).map_err(|e| e.to_string())?,
        )
    };
    crate::sync::recovery::check_provider_recovery_push(&provider, local_generation, permit)
        .await
        .map_err(|e| e.to_string())
}

// ─── Force re-pair detection helper ──────────────────────────────────────────

/// Outcome of the pull-leg keyring check in [`check_force_re_pair_sync`].
#[doc(hidden)]
pub enum PullLegKeyringCheck {
    /// Cloud vault was rotated by another device — caller must persist the
    /// force-re-pair flag and abort the sync.
    Mismatch(ForceRePairMismatch),
    /// `_meta.json` exists but is corrupt/unparseable — a positive integrity
    /// signal, not a transient failure. Caller must fail closed.
    Corrupt(String),
    /// Verified, skipped, or a transient session/network failure — nothing for
    /// the pull leg to act on. Transient errors stay best-effort by policy:
    /// failing closed here would block push on every dropped connection during
    /// a background sync.
    Clean,
}

/// Persist the fail-closed verdict of the pull-leg keyring check.
///
/// `Mismatch` records the rotation; `Corrupt` fails closed via
/// `apply_inconclusive_force_re_pair` — a corrupt `_meta.json` means the vault
/// cannot be verified, so pushing would write on top of unverifiable state.
///
/// Split from `run_sync_now` so the fail-closed wiring is unit-testable:
/// re-routing `Corrupt` to the swallow path (or dropping the flag write) must
/// break a test, not just silently re-open the fail-open this fixed.
#[doc(hidden)]
pub fn persist_pull_leg_check(
    conn: &rusqlite::Connection,
    check: &PullLegKeyringCheck,
) -> Result<(), String> {
    match check {
        PullLegKeyringCheck::Mismatch(mismatch) => set_force_re_pair_flags(conn, &mismatch.reason),
        PullLegKeyringCheck::Corrupt(_) => {
            apply_inconclusive_force_re_pair(conn, "keyring_check_inconclusive")
        }
        PullLegKeyringCheck::Clean => Ok(()),
    }
}

/// Attempt to detect a force re-pair condition at sync time.
///
/// Builds the configured cloud backend (`local` / `icloud` / `gdrive`) via
/// [`configured_cloud_provider`] so a folder vault is checked the same way as
/// Drive. Session/auth/network problems are swallowed (`Clean`) — the
/// unlock-time reconcile and the connect flow own fail-closed handling for
/// those. Missing/unconfigured provider is also `Clean` (not a typed
/// NotFound): there is no vault to compare, and treating that as absence
/// would let the dirty-keyring reupload publish over an unverified cloud.
/// A corrupt `_meta.json` is NOT swallowed: the bytes were read successfully
/// and do not form a valid keyring, so pushing on top of it would trust an
/// unverifiable vault.
#[doc(hidden)]
pub async fn check_force_re_pair_sync(
    state: &AppState,
    creds: &dyn CloudCredentialStore,
) -> PullLegKeyringCheck {
    let provider = match state.lock() {
        Ok(conn) => match configured_cloud_provider(&conn, creds) {
            Ok(Some(p)) => p,
            // Disabled, unconfigured, or a session rebuild failure — fail
            // open for availability. Do not map "no provider" to NotFound.
            _ => return PullLegKeyringCheck::Clean,
        },
        Err(_) => return PullLegKeyringCheck::Clean,
    };
    match detect_force_re_pair(&provider, state).await {
        Ok(ForceRePairCheck::Mismatch(m)) => PullLegKeyringCheck::Mismatch(m),
        // Verified / Skipped → nothing to report. The pull leg never clears flags.
        Ok(_) => PullLegKeyringCheck::Clean,
        Err(e) if e.is_corrupt => PullLegKeyringCheck::Corrupt(e.message),
        // Transient (network / auth / lock) — best-effort by policy.
        Err(_) => PullLegKeyringCheck::Clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::sync::cloud_config::GDriveClientConfig;
    use crate::sync::cloud_config::LocalConfig;
    use crate::sync::LocalSyncProvider;
    use rusqlite::Connection;

    /// Mirrors the desktop store: the refresh token lives in the
    /// `gdrive_refresh_token` setting, the client comes from a fixed
    /// placeholder.
    struct DbSettingCredentialStore;

    impl CloudCredentialStore for DbSettingCredentialStore {
        fn client_config(&self, _conn: &Connection) -> Result<GDriveClientConfig, String> {
            Ok(GDriveClientConfig {
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

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    // ─── I4: detect_force_re_pair || semantics ───────────────────────────────

    /// I4: detect_force_re_pair fires when fingerprint differs even without epoch advance.
    #[tokio::test]
    async fn detect_force_re_pair_triggers_on_fingerprint_change_without_epoch_advance() {
        use crate::sync::keyring_v2::io::{test_support::InMemoryKeyringProvider, write_meta};
        use crate::sync::keyring_v2::types::{KeyringMetaV2, KEYRING_V2_VERSION};

        let provider = InMemoryKeyringProvider::new();
        let state = make_state();

        let local_fp = "aa".repeat(32); // 64 hex chars
        let cloud_fp = "bb".repeat(32); // different fingerprint
        let shared_epoch = 5u64;

        // Store local epoch + fingerprint.
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::CLOUD_MASTER_FINGERPRINT, &local_fp).unwrap();
            db::set_setting(&conn, db::KEYRING_V2_LOCAL_EPOCH, &shared_epoch.to_string()).unwrap();
        }

        // Cloud has SAME epoch but DIFFERENT fingerprint.
        let meta = KeyringMetaV2 {
            version: KEYRING_V2_VERSION,
            epoch: shared_epoch, // same epoch — bug was && instead of ||
            master_fingerprint: cloud_fp,
            content_epoch: 0,
            recovery_generation: 0,
            created_at: 0,
            updated_at: 0,
        };
        write_meta(&provider, &meta).await.unwrap();

        let result = detect_force_re_pair(&provider, &state).await.unwrap();
        assert!(
            matches!(result, ForceRePairCheck::Mismatch(_)),
            "detect_force_re_pair must fire on fingerprint mismatch even without epoch advance \
             — and must never report Verified, which would clear the fail-closed flag"
        );
    }

    // ─── I5: resolve_force_re_pair_outcome (fail-closed) ────────────────────

    /// I5-a (RED→GREEN): Ok(Verified) maps to Verified — normal reconnect is allowed.
    #[test]
    fn resolve_force_re_pair_outcome_verified_on_ok_verified() {
        let decision = resolve_force_re_pair_outcome(Ok(ForceRePairCheck::Verified));
        assert!(
            matches!(decision, ForceRePairDecision::Verified),
            "Ok(Verified) must map to Verified"
        );
    }

    /// Ok(Skipped) must map to Skipped, NOT Verified.
    ///
    /// Regression guard for fail-open: `Skipped` means "nothing to compare"
    /// (no cloud `_meta.json`, or no local fingerprint). If it collapsed into
    /// `Verified`, the Verified arm would clear the fail-closed flag on the
    /// strength of a check that never actually ran.
    #[test]
    fn resolve_force_re_pair_outcome_skipped_is_not_verified() {
        let decision = resolve_force_re_pair_outcome(Ok(ForceRePairCheck::Skipped));
        assert!(
            matches!(decision, ForceRePairDecision::Skipped),
            "Ok(Skipped) must map to Skipped — collapsing it into Verified is fail-open"
        );
    }

    /// I5-b (RED→GREEN): Ok(Mismatch(_)) maps to NeedsRepair — vault was rotated.
    #[test]
    fn resolve_force_re_pair_outcome_needs_repair_on_ok_mismatch() {
        let mismatch = ForceRePairMismatch {
            reason: "vault rotated".to_string(),
            cloud_epoch: 7,
        };
        let decision = resolve_force_re_pair_outcome(Ok(ForceRePairCheck::Mismatch(mismatch)));
        match decision {
            ForceRePairDecision::NeedsRepair(m) => {
                assert_eq!(m.reason, "vault rotated");
                assert_eq!(m.cloud_epoch, 7);
            }
            other => panic!(
                "Ok(Mismatch) must map to NeedsRepair, got {:?}",
                match other {
                    ForceRePairDecision::Verified => "Verified",
                    ForceRePairDecision::Skipped => "Skipped",
                    ForceRePairDecision::Inconclusive { .. } => "Inconclusive",
                    ForceRePairDecision::NeedsRepair(_) => unreachable!(),
                }
            ),
        }
    }

    /// I5-c (RED→GREEN): Err(_) maps to Inconclusive — fail-closed, not Clean.
    ///
    /// This is the regression test for the bug: previously Err was treated as
    /// "log and continue" (same as Clean). After the fix it must NOT be Clean.
    #[test]
    fn resolve_force_re_pair_outcome_inconclusive_on_err() {
        let decision = resolve_force_re_pair_outcome(Err(DetectError::other(
            "read _meta.json: network error".to_string(),
        )));
        match decision {
            ForceRePairDecision::Inconclusive { message, is_auth } => {
                assert!(
                    message.contains("network error"),
                    "Inconclusive must preserve the error message: {message}"
                );
                assert!(!is_auth, "a plain I/O error must not be typed as auth");
            }
            ForceRePairDecision::Verified | ForceRePairDecision::Skipped => {
                panic!("Err must NOT map to a clean outcome — that is the fail-open bug we fixed");
            }
            ForceRePairDecision::NeedsRepair(_) => {
                panic!("Err must not map to NeedsRepair (no mismatch data available)");
            }
        }
    }

    // ─── Stale-flag self-heal: detect_force_re_pair 3-state + clear ─────────

    const TEST_FP: &str = "aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";

    async fn seed_cloud_meta(
        provider: &crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider,
        fingerprint: &str,
        epoch: u64,
    ) {
        let meta = crate::sync::keyring_v2::types::KeyringMetaV2 {
            version: crate::sync::keyring_v2::types::KEYRING_V2_VERSION,
            epoch,
            master_fingerprint: fingerprint.to_string(),
            content_epoch: 1,
            recovery_generation: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        crate::sync::keyring_v2::io::write_meta(provider, &meta)
            .await
            .expect("seed cloud _meta.json");
    }

    fn seed_local_keyring(state: &AppState, fingerprint: &str, epoch: u64) {
        let conn = state.lock().unwrap();
        db::set_setting(&conn, db::CLOUD_MASTER_FINGERPRINT, fingerprint).unwrap();
        db::set_setting(&conn, db::KEYRING_V2_LOCAL_EPOCH, &epoch.to_string()).unwrap();
    }

    /// Matching fingerprint + equal epoch → Verified (a real comparison ran).
    #[tokio::test]
    async fn detect_force_re_pair_verified_when_fingerprint_matches() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let provider = InMemoryKeyringProvider::new();
        seed_cloud_meta(&provider, TEST_FP, 3).await;

        let check = detect_force_re_pair(&provider, &state).await.unwrap();
        assert!(
            matches!(check, ForceRePairCheck::Verified),
            "matching fingerprint + equal epoch must be Verified"
        );
    }

    /// No cloud `_meta.json` → Skipped, NOT Verified.
    ///
    /// This is the fail-open guard: if this returned Verified, the caller would
    /// clear a legitimate force-re-pair flag purely because the keyring was
    /// momentarily absent.
    #[tokio::test]
    async fn detect_force_re_pair_skipped_when_no_cloud_meta() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let provider = InMemoryKeyringProvider::new(); // no _meta.json

        let check = detect_force_re_pair(&provider, &state).await.unwrap();
        assert!(
            matches!(check, ForceRePairCheck::Skipped),
            "absent _meta.json must be Skipped — treating it as Verified is fail-open"
        );
    }

    /// No local fingerprint (setup incomplete) → Skipped, NOT Verified.
    #[tokio::test]
    async fn detect_force_re_pair_skipped_when_no_local_fingerprint() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        let provider = InMemoryKeyringProvider::new();
        seed_cloud_meta(&provider, TEST_FP, 3).await;

        let check = detect_force_re_pair(&provider, &state).await.unwrap();
        assert!(
            matches!(check, ForceRePairCheck::Skipped),
            "missing local fingerprint must be Skipped — nothing to compare against"
        );
    }

    /// Same fingerprint but a NEWER cloud epoch → Mismatch, never Verified.
    ///
    /// Guards the epoch half of the `||` in `detect_force_re_pair`: without this,
    /// weakening `>` to `>=` or dropping the clause would leave the suite green
    /// while letting a rotated-but-same-fingerprint vault clear the flag.
    #[tokio::test]
    async fn detect_force_re_pair_mismatch_when_cloud_epoch_is_newer() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let provider = InMemoryKeyringProvider::new();
        seed_cloud_meta(&provider, TEST_FP, 4).await; // same fp, cloud ahead

        let check = detect_force_re_pair(&provider, &state).await.unwrap();
        match check {
            ForceRePairCheck::Mismatch(m) => assert_eq!(m.cloud_epoch, 4),
            _ => panic!("newer cloud epoch must be Mismatch, never Verified"),
        }
    }

    // ─── force_re_pair_required: gates the keyring re-upload ─────────────────

    /// Flag set → the re-upload gate must report "required" and withhold the push.
    #[test]
    fn force_re_pair_required_true_when_flag_set() {
        let state = make_state();
        let conn = state.lock().unwrap();
        apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive").unwrap();
        assert!(
            force_re_pair_required(&conn),
            "a flagged device must be reported as needing re-pair so the re-upload is skipped"
        );
    }

    /// No flag → the re-upload may proceed as before.
    #[test]
    fn force_re_pair_required_false_when_flag_absent() {
        let state = make_state();
        let conn = state.lock().unwrap();
        assert!(
            !force_re_pair_required(&conn),
            "an unflagged device must not be blocked from re-uploading its keyring"
        );
    }

    // ─── apply_recheck_decision: only Verified may clear ─────────────────────

    /// Seed a set force-re-pair flag, then run the decision dispatch.
    /// Returns (cleared_result, flag_still_set).
    fn run_recheck(decision: ForceRePairDecision) -> (Result<bool, String>, bool) {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")
                .expect("seed the flag");
        }
        let conn = state.lock().unwrap();
        let result = apply_recheck_decision(&conn, decision);
        let still_set = db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
            .unwrap()
            .as_deref()
            == Some("1");
        (result, still_set)
    }

    /// Verified → clears the flag and reports true.
    #[test]
    fn apply_recheck_decision_clears_flag_on_verified() {
        let (result, still_set) = run_recheck(ForceRePairDecision::Verified);
        assert_eq!(result, Ok(true), "Verified must report the flag as cleared");
        assert!(!still_set, "Verified must clear FORCE_RE_PAIR_REQUIRED");
    }

    /// Skipped → must NOT clear. This is the fail-open guard for the dispatch:
    /// "nothing to compare" is not proof the vault is healthy.
    #[test]
    fn apply_recheck_decision_keeps_flag_on_skipped() {
        let (result, still_set) = run_recheck(ForceRePairDecision::Skipped);
        assert_eq!(result, Ok(false), "Skipped must not report a clear");
        assert!(
            still_set,
            "Skipped must leave FORCE_RE_PAIR_REQUIRED set — clearing it is fail-open"
        );
    }

    /// NeedsRepair → must NOT clear; a genuine rotation still requires re-pair.
    #[test]
    fn apply_recheck_decision_keeps_flag_on_needs_repair() {
        let (result, still_set) =
            run_recheck(ForceRePairDecision::NeedsRepair(ForceRePairMismatch {
                reason: "vault_rotated".to_string(),
                cloud_epoch: 9,
            }));
        assert_eq!(result, Ok(false), "NeedsRepair must not report a clear");
        assert!(still_set, "NeedsRepair must leave the flag set");
    }

    /// NeedsRepair must UPGRADE an inconclusive reason to the observed rotation.
    ///
    /// Otherwise the flag keeps its `keyring_check_inconclusive` provenance after a
    /// real rotation is detected, and a later auth failure clears it as if it had
    /// only ever been inconclusive.
    #[test]
    fn apply_recheck_decision_upgrades_reason_on_needs_repair() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive").unwrap();
        }
        let conn = state.lock().unwrap();
        apply_recheck_decision(
            &conn,
            ForceRePairDecision::NeedsRepair(ForceRePairMismatch {
                reason: "vault_rotated".to_string(),
                cloud_epoch: 9,
            }),
        )
        .unwrap();

        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("vault_rotated"),
            "a detected rotation must overwrite the inconclusive provenance, else a later \
             auth failure would clear a flag that now protects a real rotation"
        );

        // And the upgraded reason must now resist the auth-clear.
        let result = apply_recheck_decision(
            &conn,
            ForceRePairDecision::Inconclusive {
                message: "GDRIVE_TOKEN_REVOKED: revoked".to_string(),
                is_auth: true,
            },
        );
        assert!(
            result.is_err(),
            "after the upgrade, an auth failure must not clear the flag"
        );
    }

    /// Inconclusive → must NOT clear, and must surface an error (fail-closed).
    #[test]
    fn apply_recheck_decision_keeps_flag_and_errors_on_inconclusive() {
        let (result, still_set) = run_recheck(ForceRePairDecision::Inconclusive {
            message: "network error".to_string(),
            is_auth: false,
        });
        assert!(
            result.is_err(),
            "Inconclusive must surface an error, not a silent clear"
        );
        assert!(
            still_set,
            "a non-auth Inconclusive must leave the flag set (fail-closed)"
        );
    }

    /// Inconclusive from revoked auth + an inconclusive-origin flag → clear it.
    ///
    /// This is the escape from the misclassification: the flag was written by a
    /// check that failed on auth, and every retry fails the same way, so it can
    /// never lift on its own. The user must reconnect Drive, not re-pair.
    #[test]
    fn apply_recheck_decision_clears_inconclusive_flag_when_auth_revoked() {
        let (result, still_set) = run_recheck(ForceRePairDecision::Inconclusive {
            message: "detect_force_re_pair: read _meta.json: auth: GDRIVE_TOKEN_REVOKED: \
                      Refresh token expired or revoked."
                .to_string(),
            is_auth: true,
        });
        assert_eq!(
            result,
            Ok(true),
            "an auth-revoked inconclusive flag must be cleared, not reported as an error"
        );
        assert!(
            !still_set,
            "the flag must be cleared so the user can reconnect Drive instead of re-pairing"
        );
    }

    /// A `vault_rotated` flag must survive an auth failure — it is a real rotation,
    /// not the misclassification, so only a mnemonic re-pair may lift it.
    #[test]
    fn apply_recheck_decision_keeps_vault_rotated_flag_when_auth_revoked() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REASON, "vault_rotated").unwrap();
        }
        let conn = state.lock().unwrap();
        let result = apply_recheck_decision(
            &conn,
            ForceRePairDecision::Inconclusive {
                message: "GDRIVE_TOKEN_REVOKED: revoked".to_string(),
                is_auth: true,
            },
        );
        assert!(
            result.is_err(),
            "a vault_rotated flag must not be cleared by an auth failure"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                .unwrap()
                .as_deref(),
            Some("1"),
            "vault_rotated must survive — clearing it would be fail-open"
        );
    }

    // ─── A4: set_force_re_pair_flags (transactional mismatch write) ─────────

    /// Both rows must land together — the flag without the reason (or vice
    /// versa) is exactly the half-state the transactional writer exists to
    /// prevent.
    #[test]
    fn set_force_re_pair_flags_writes_both_rows() {
        let state = make_state();
        let conn = state.lock().unwrap();
        set_force_re_pair_flags(&conn, "vault_rotated").unwrap();
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("vault_rotated")
        );
    }

    /// The transactional writer must FAIL LOUDLY when the write cannot land.
    /// `gdrive_complete_connect` relies on this: it propagates the error
    /// instead of returning the typed `NeedsForceRePair` outcome with no
    /// persisted guard (which would leave push unblocked after a restart).
    #[test]
    fn set_force_re_pair_flags_errors_when_settings_table_is_gone() {
        let state = make_state();
        let conn = state.lock().unwrap();
        conn.execute_batch("DROP TABLE settings").unwrap();
        assert!(
            set_force_re_pair_flags(&conn, "vault_rotated").is_err(),
            "a failed flag write must surface as Err, never silent success"
        );
    }

    // ─── B4: confirm_post_clear_verification (remote TOCTOU guard) ──────────

    /// A rotation observed by the post-clear detect must re-arm the flag —
    /// otherwise the stale `Verified` that raced the rotation has erased it.
    #[test]
    fn confirm_post_clear_verification_rearms_flag_on_needs_repair() {
        let state = make_state();
        let conn = state.lock().unwrap();
        let result = confirm_post_clear_verification(
            &conn,
            Ok(ForceRePairCheck::Mismatch(ForceRePairMismatch {
                reason: "vault_rotated".to_string(),
                cloud_epoch: 7,
            })),
        );
        assert_eq!(
            result,
            Ok(false),
            "a re-armed flag must report 'not cleared'"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                .unwrap()
                .as_deref(),
            Some("1"),
            "the rotation landed inside the detect→clear window — flag must be re-armed"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("vault_rotated")
        );
    }

    /// Non-rotation outcomes on the second detect must leave the clear in
    /// place: re-arming on a transient failure would make the recovery path
    /// self-defeating (the flag it just cleared came from such a failure).
    #[test]
    fn confirm_post_clear_verification_keeps_clear_on_non_rotation_outcomes() {
        for second in [
            Ok(ForceRePairCheck::Verified),
            Ok(ForceRePairCheck::Skipped),
            Err(DetectError::other("network blip".to_string())),
        ] {
            let state = make_state();
            let conn = state.lock().unwrap();
            let result = confirm_post_clear_verification(&conn, second);
            assert_eq!(result, Ok(true));
            assert!(
                db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                    .unwrap()
                    .is_none(),
                "only a rotation or corrupt meta may re-arm the flag"
            );
        }
    }

    /// A `_meta.json` that turned corrupt inside the detect→clear window must
    /// re-arm the flag — same fail-closed policy as the pull leg (A1). The
    /// typed `is_corrupt` verdict must not be lost between detect and confirm.
    #[test]
    fn confirm_post_clear_verification_rearms_flag_on_corrupt_meta() {
        let state = make_state();
        let conn = state.lock().unwrap();
        let result = confirm_post_clear_verification(
            &conn,
            Err(DetectError {
                message: "detect_force_re_pair: read _meta.json: serialization: parse error"
                    .to_string(),
                is_auth: false,
                is_corrupt: true,
            }),
        );
        assert_eq!(result, Ok(false), "corrupt meta must report 'not cleared'");
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                .unwrap()
                .as_deref(),
            Some("1"),
            "the flag must be re-armed when the cloud turns unverifiable mid-recheck"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("keyring_check_inconclusive")
        );
    }

    // ─── A1: corrupt-vs-transient classification in DetectError ─────────────

    /// A `_meta.json` that exists but cannot parse is an integrity signal, not
    /// a transient failure — `check_force_re_pair_sync` fails closed on it.
    #[tokio::test]
    async fn detect_force_re_pair_marks_unparseable_meta_as_corrupt() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::KeyringV2Io;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let provider = InMemoryKeyringProvider::new();
        provider
            .write_file(".meta/keyring/_meta.json", b"{ not json at all")
            .await
            .expect("seed corrupt meta");

        let err = detect_force_re_pair(&provider, &state)
            .await
            .expect_err("corrupt meta must not verify");
        assert!(
            err.is_corrupt,
            "a Serialization failure on _meta.json must be flagged corrupt: {}",
            err.message
        );
    }

    /// A transient provider failure must NOT be classified as corrupt — the
    /// best-effort sync pull leg would otherwise block push on every dropped
    /// connection.
    #[tokio::test]
    async fn detect_force_re_pair_transient_failure_is_not_corrupt() {
        struct NetworkFailingIo;

        #[async_trait::async_trait]
        impl crate::sync::keyring_v2::KeyringV2Io for NetworkFailingIo {
            async fn read_file(
                &self,
                _path: &str,
            ) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
                Err(crate::sync::provider::SyncError::Network(
                    "connection reset".to_string(),
                ))
            }
            async fn write_file(
                &self,
                _path: &str,
                _data: &[u8],
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn delete_file(
                &self,
                _path: &str,
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn list_files(
                &self,
                _prefix: &str,
            ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
                Ok(vec![])
            }
        }

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let err = detect_force_re_pair(&NetworkFailingIo, &state)
            .await
            .expect_err("network failure is inconclusive");
        assert!(!err.is_corrupt, "transient failures must stay best-effort");
        assert!(!err.is_auth);
    }

    /// A malicious `_meta.json` must NOT be able to forge an auth verdict.
    ///
    /// `KeyringMetaV2` uses `deny_unknown_fields`, so serde echoes an unknown
    /// field NAME — fully cloud-controlled — into the error message. When the
    /// classifier substring-matched that message, a field named
    /// `GDRIVE_TOKEN_REVOKED` made a corrupt keyring look like dead auth, and the
    /// fail-closed flag was skipped while the token was still valid: push would
    /// proceed on an unverified keyring. Classification now reads the typed
    /// `SyncError` variant, so the injected marker is inert.
    #[tokio::test]
    async fn detect_force_re_pair_rejects_auth_marker_injected_via_cloud_meta() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;
        use crate::sync::keyring_v2::KeyringV2Io;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);
        let provider = InMemoryKeyringProvider::new();
        // Attacker-authored _meta.json: the unknown field name is the marker.
        provider
            .write_file(
                ".meta/keyring/_meta.json",
                br#"{"GDRIVE_TOKEN_REVOKED":"x","version":2,"epoch":3,"master_fingerprint":"aa","content_epoch":1,"created_at":0,"updated_at":0}"#,
            )
            .await
            .expect("seed malicious meta");

        let err = detect_force_re_pair(&provider, &state)
            .await
            .expect_err("a corrupt _meta.json must not parse");
        assert!(
            err.message.contains("GDRIVE_TOKEN_REVOKED"),
            "precondition: the cloud-controlled marker really does reach the message: {}",
            err.message
        );
        assert!(
            !err.is_auth,
            "a serialization failure must never be classified as auth just because the \
             cloud injected the marker — that skips the fail-closed flag (fail-open)"
        );
    }

    /// A real `SyncError::Auth` from the provider IS classified as auth.
    #[tokio::test]
    async fn detect_force_re_pair_classifies_typed_auth_error() {
        struct AuthFailingIo;

        #[async_trait::async_trait]
        impl crate::sync::keyring_v2::KeyringV2Io for AuthFailingIo {
            async fn read_file(
                &self,
                _path: &str,
            ) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
                Err(crate::sync::provider::SyncError::Auth(
                    "GDRIVE_TOKEN_REVOKED: Refresh token expired or revoked.".to_string(),
                ))
            }
            async fn write_file(
                &self,
                _path: &str,
                _data: &[u8],
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn delete_file(
                &self,
                _path: &str,
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn list_files(
                &self,
                _prefix: &str,
            ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
                Ok(vec![])
            }
        }

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);

        let err = detect_force_re_pair(&AuthFailingIo, &state)
            .await
            .expect_err("auth failure must surface as an error");
        assert!(
            err.is_auth,
            "a typed SyncError::Auth must be classified as auth so the user is sent to \
             reconnect Drive rather than the mnemonic re-pair screen"
        );
    }

    /// A `SyncError::Auth` WITHOUT the revoked marker must stay fail-closed.
    ///
    /// `gdrive_provider.rs` maps every non-scope refresh failure — network drop,
    /// 5xx, parse error — to `SyncError::Auth`. Trusting the variant alone would
    /// let a transient blip pose as a revoked token and clear the flag.
    #[tokio::test]
    async fn detect_force_re_pair_does_not_classify_generic_auth_as_revoked() {
        struct GenericAuthFailingIo;

        #[async_trait::async_trait]
        impl crate::sync::keyring_v2::KeyringV2Io for GenericAuthFailingIo {
            async fn read_file(
                &self,
                _path: &str,
            ) -> Result<Vec<u8>, crate::sync::provider::SyncError> {
                // What a network drop during token refresh actually produces.
                Err(crate::sync::provider::SyncError::Auth(
                    "refresh failed: connection reset by peer".to_string(),
                ))
            }
            async fn write_file(
                &self,
                _path: &str,
                _data: &[u8],
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn delete_file(
                &self,
                _path: &str,
            ) -> Result<(), crate::sync::provider::SyncError> {
                Ok(())
            }
            async fn list_files(
                &self,
                _prefix: &str,
            ) -> Result<Vec<String>, crate::sync::provider::SyncError> {
                Ok(vec![])
            }
        }

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 3);

        let err = detect_force_re_pair(&GenericAuthFailingIo, &state)
            .await
            .expect_err("the read failed, so this must be an error");
        assert!(
            !err.is_auth,
            "a generic Auth error carries no revoked marker — treating it as a revoked \
             token would clear the fail-closed flag on a transient network blip"
        );
    }

    /// A replayed OLDER cloud epoch must be a Mismatch, not Verified.
    ///
    /// `Verified` authorizes clearing the fail-closed flag, so accepting a
    /// rollback would let a stale `_meta.json` erase a real `vault_rotated` flag.
    #[tokio::test]
    async fn detect_force_re_pair_mismatch_when_cloud_epoch_is_older() {
        use crate::sync::keyring_v2::io::test_support::InMemoryKeyringProvider;

        let state = make_state();
        seed_local_keyring(&state, TEST_FP, 5);
        let provider = InMemoryKeyringProvider::new();
        seed_cloud_meta(&provider, TEST_FP, 2).await; // cloud rolled BACK

        let check = detect_force_re_pair(&provider, &state).await.unwrap();
        assert!(
            matches!(check, ForceRePairCheck::Mismatch(_)),
            "an older cloud epoch is a rollback — it must never read as Verified"
        );
    }

    /// An inconclusive check must NOT downgrade an existing `vault_rotated` reason.
    ///
    /// Reason is provenance: `apply_recheck_decision` clears an auth-failed flag
    /// only when the reason is `keyring_check_inconclusive`. If a later
    /// inconclusive check could overwrite `vault_rotated`, the sequence
    /// rotation → inconclusive → auth failure would erase real protection.
    #[test]
    fn apply_inconclusive_force_re_pair_never_downgrades_vault_rotated() {
        let state = make_state();
        let conn = state.lock().unwrap();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REASON, "vault_rotated").unwrap();

        apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive").unwrap();

        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("vault_rotated"),
            "vault_rotated must survive an inconclusive check — downgrading it opens the \
             rotation → inconclusive → auth-clear fail-open"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REQUIRED)
                .unwrap()
                .as_deref(),
            Some("1"),
            "the flag itself must stay set"
        );
    }

    /// An empty cloud + a flag written by an inconclusive check is a deadlock:
    /// publish is gated by the flag, the recheck has no `_meta.json` to compare,
    /// and the mnemonic re-pair needs a `_recovery.json` that is gone. Clearing
    /// the flag is what breaks it — the empty cloud has no peer state to clobber.
    #[test]
    fn clear_stale_flag_for_empty_cloud_clears_an_inconclusive_flag() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")
                .expect("seed force-re-pair flags");
        }

        let conn = state.lock().unwrap();
        let cleared = clear_stale_flag_for_empty_cloud(&conn, false)
            .expect("clearing must succeed on a healthy db");

        assert!(cleared, "an inconclusive flag must be reported as cleared");
        assert!(
            !force_re_pair_required(&conn),
            "the flag must be gone so publish_v2_keyring_with_provider can rebuild the vault"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON).unwrap(),
            None,
            "the reason must be cleared alongside the flag"
        );
    }

    /// The empty-cloud escape hatch must not become a way around a real rotation:
    /// a peer moved the vault, so this device's key material is stale whatever the
    /// cloud currently holds. Only the mnemonic re-pair may clear `vault_rotated`.
    #[test]
    fn clear_stale_flag_for_empty_cloud_leaves_vault_rotated_standing() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            set_force_re_pair_flags(&conn, "vault_rotated").expect("seed force-re-pair flags");
        }

        let conn = state.lock().unwrap();
        let cleared =
            clear_stale_flag_for_empty_cloud(&conn, false).expect("the call must not error");

        assert!(!cleared, "a vault_rotated flag must never be cleared here");
        assert!(
            force_re_pair_required(&conn),
            "push must stay blocked after a genuine peer rotation"
        );
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("vault_rotated"),
            "the reason must survive so a later check cannot downgrade it"
        );
    }

    /// No flag set → nothing to clear, and the call must stay side-effect free.
    #[test]
    fn clear_stale_flag_for_empty_cloud_is_a_noop_without_a_flag() {
        let state = make_state();
        let conn = state.lock().unwrap();

        let cleared =
            clear_stale_flag_for_empty_cloud(&conn, false).expect("the call must not error");

        assert!(!cleared, "there was no flag to clear");
        assert!(
            !force_re_pair_required(&conn),
            "no flag must have been added"
        );
    }

    /// The whole safety argument is that a keyring is PRESENT in the cloud → the
    /// flag may be guarding a peer's committed rotation → never clear. Passing the
    /// probe result in (rather than re-reading it) is what makes this testable:
    /// misplacing the call into a keyring-present branch must not clear anything.
    #[test]
    fn clear_stale_flag_for_empty_cloud_never_clears_when_a_keyring_is_present() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")
                .expect("seed force-re-pair flags");
        }

        let conn = state.lock().unwrap();
        let cleared =
            clear_stale_flag_for_empty_cloud(&conn, true).expect("the call must not error");

        assert!(
            !cleared,
            "a present cloud keyring may be a peer's committed rotation — even an \
             inconclusive flag must stand"
        );
        assert!(
            force_re_pair_required(&conn),
            "push must stay blocked while the cloud holds a keyring this device has not verified"
        );
    }

    /// A read failure must propagate, not be swallowed into a silent "nothing
    /// cleared" — the caller hard-fails the connect on `Err` so the user can retry.
    #[test]
    fn clear_stale_flag_for_empty_cloud_errors_when_settings_table_is_gone() {
        let state = make_state();
        let conn = state.lock().unwrap();
        apply_inconclusive_force_re_pair(&conn, "keyring_check_inconclusive")
            .expect("seed force-re-pair flags");
        conn.execute_batch("DROP TABLE settings")
            .expect("drop settings");

        let result = clear_stale_flag_for_empty_cloud(&conn, false);

        assert!(
            result.is_err(),
            "a broken settings table must surface as Err"
        );
    }

    // ─── A1: persist_pull_leg_check (fail-closed wiring of the pull leg) ─────

    /// A corrupt cloud `_meta.json` must persist the fail-closed flag so that
    /// `ensure_safe_to_push` blocks — deleting the Corrupt wiring in
    /// `run_sync_now` (or routing it to Clean) must fail this test.
    #[test]
    fn persist_pull_leg_check_corrupt_blocks_push() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            persist_pull_leg_check(
                &conn,
                &PullLegKeyringCheck::Corrupt("_meta.json parse error".to_string()),
            )
            .expect("flag write must succeed");
        }
        let err = ensure_safe_to_push(&state).expect_err("corrupt meta must block push");
        assert!(
            err.contains("FORCE_RE_PAIR_REQUIRED"),
            "push must be blocked by the persisted flag, got: {err}"
        );
        let conn = state.lock().unwrap();
        assert_eq!(
            db::get_setting(&conn, db::FORCE_RE_PAIR_REASON)
                .unwrap()
                .as_deref(),
            Some("keyring_check_inconclusive"),
            "corrupt meta uses the inconclusive provenance so an auth-recheck can still clear it"
        );
    }

    #[test]
    fn persist_pull_leg_check_mismatch_sets_both_flags() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            persist_pull_leg_check(
                &conn,
                &PullLegKeyringCheck::Mismatch(ForceRePairMismatch {
                    reason: "vault_rotated".to_string(),
                    cloud_epoch: 5,
                }),
            )
            .expect("flag write must succeed");
        }
        let err = ensure_safe_to_push(&state).expect_err("mismatch must block push");
        assert!(err.contains("rotated"), "got: {err}");
    }

    #[test]
    fn persist_pull_leg_check_clean_is_a_noop() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            persist_pull_leg_check(&conn, &PullLegKeyringCheck::Clean).unwrap();
        }
        assert!(
            ensure_safe_to_push(&state).is_ok(),
            "a clean check must not block push"
        );
    }

    const FOLDER_REPAIR_LOCAL_FP: &str =
        "aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";
    const FOLDER_REPAIR_CLOUD_FP: &str =
        "bb11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66";

    fn configure_folder_sync(state: &AppState, root: &std::path::Path, leftover_drive_token: bool) {
        let cfg = serde_json::to_string(&LocalConfig {
            root_path: root.to_string_lossy().into_owned(),
        })
        .expect("serialize local sync config");
        let conn = state.lock().unwrap();
        db::set_sync_provider(&conn, "local").unwrap();
        db::set_sync_config_json(&conn, &cfg).unwrap();
        db::set_sync_enabled(&conn, true).unwrap();
        db::set_setting(&conn, db::CLOUD_MASTER_FINGERPRINT, FOLDER_REPAIR_LOCAL_FP).unwrap();
        db::set_setting(&conn, db::KEYRING_V2_LOCAL_EPOCH, "1").unwrap();
        if leftover_drive_token {
            db::set_setting(&conn, "gdrive_refresh_token", "1//leftover-drive-token").unwrap();
        }
    }

    async fn seed_rotated_folder_meta(root: &std::path::Path) {
        let folder = LocalSyncProvider::new(root);
        let meta = crate::sync::keyring_v2::types::KeyringMetaV2 {
            version: crate::sync::keyring_v2::types::KEYRING_V2_VERSION,
            epoch: 2,
            master_fingerprint: FOLDER_REPAIR_CLOUD_FP.to_string(),
            content_epoch: 1,
            recovery_generation: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        crate::sync::keyring_v2::io::write_meta(&folder, &meta)
            .await
            .expect("seed folder _meta.json");
    }

    fn assert_folder_vault_rotation(check: PullLegKeyringCheck) {
        match check {
            PullLegKeyringCheck::Mismatch(m) => {
                assert_eq!(m.reason, "vault_rotated");
                assert_eq!(m.cloud_epoch, 2);
            }
            PullLegKeyringCheck::Clean => panic!(
                "folder-backed rotation must run detect on the folder provider — \
                 Clean means the check bailed (no Drive token / leftover Drive session) \
                 instead of reading the folder vault"
            ),
            PullLegKeyringCheck::Corrupt(msg) => {
                panic!("expected folder vault Mismatch, got Corrupt: {msg}")
            }
        }
    }

    /// A local/iCloud folder vault must run `detect_force_re_pair` even when
    /// there is no Drive refresh token. Returning Clean here is the
    /// reconstruct_session / GDriveProvider::new_live hole: the dirty-keyring
    /// reupload would then publish local `_meta.json` over a peer rotation.
    #[tokio::test]
    async fn check_force_re_pair_sync_detects_folder_vault_rotation_without_drive_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = make_state();
        configure_folder_sync(&state, dir.path(), false);
        seed_rotated_folder_meta(dir.path()).await;

        assert_folder_vault_rotation(
            check_force_re_pair_sync(&state, &DbSettingCredentialStore).await,
        );
    }

    /// A leftover Drive refresh token from a prior Drive session must not
    /// divert this check onto Drive. Probe the configured folder; a Drive
    /// network/auth failure would otherwise swallow the rotation as Clean.
    #[tokio::test]
    async fn check_force_re_pair_sync_detects_folder_vault_rotation_despite_leftover_drive_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = make_state();
        configure_folder_sync(&state, dir.path(), true);
        seed_rotated_folder_meta(dir.path()).await;

        assert_folder_vault_rotation(
            check_force_re_pair_sync(&state, &DbSettingCredentialStore).await,
        );
    }

    // ─── ensure_safe_to_push gate (replaces ensure_not_force_re_pair) ────────

    /// C1/C2: ensure_safe_to_push returns Ok on a clean DB.
    #[test]
    fn ensure_safe_to_push_ok_when_neither_condition_holds() {
        let state = make_state();
        assert!(
            ensure_safe_to_push(&state).is_ok(),
            "must return Ok when no rotation job and no force-re-pair flag"
        );
    }

    /// C1/C2: ensure_safe_to_push blocks when force-re-pair is required.
    #[test]
    fn ensure_safe_to_push_blocks_when_force_re_pair_required() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
        }
        let result = ensure_safe_to_push(&state);
        assert!(
            result.is_err(),
            "expected Err when FORCE_RE_PAIR_REQUIRED=1"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("Re-pair before syncing"),
            "error must mention re-pair: {msg}"
        );
    }

    /// C1/C2: ensure_safe_to_push emits the 'inconclusive' reason class when
    /// FORCE_RE_PAIR_REASON = keyring_check_inconclusive.
    #[test]
    fn ensure_safe_to_push_reason_inconclusive() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
            db::set_setting(
                &conn,
                db::FORCE_RE_PAIR_REASON,
                "keyring_check_inconclusive",
            )
            .unwrap();
        }
        let msg = ensure_safe_to_push(&state).unwrap_err();
        assert!(
            msg.contains("FORCE_RE_PAIR_REQUIRED"),
            "must contain machine marker: {msg}"
        );
        assert!(
            msg.contains("reason=inconclusive"),
            "must encode inconclusive reason: {msg}"
        );
        assert!(
            msg.contains("Re-pair before syncing"),
            "must keep re-pair substring for existing tests: {msg}"
        );
    }

    /// C1/C2: ensure_safe_to_push emits the 'rotated' reason class for a real
    /// vault-rotation reason (any value other than keyring_check_inconclusive).
    #[test]
    fn ensure_safe_to_push_reason_rotated() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
            db::set_setting(&conn, db::FORCE_RE_PAIR_REASON, "vault_rotated_on_peer").unwrap();
        }
        let msg = ensure_safe_to_push(&state).unwrap_err();
        assert!(
            msg.contains("FORCE_RE_PAIR_REQUIRED"),
            "must contain machine marker: {msg}"
        );
        assert!(
            msg.contains("reason=rotated"),
            "must encode rotated reason: {msg}"
        );
        assert!(
            msg.contains("Re-pair before syncing"),
            "must keep re-pair substring for existing tests: {msg}"
        );
    }

    /// C1: ensure_safe_to_push blocks when an active rotation job exists
    /// (e.g. state = reencrypt — paused after retry-abort).
    #[test]
    fn ensure_safe_to_push_blocks_when_active_rotation_job_exists() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::insert_rotation_job(&conn, "fp-old", "fp-new", 1, 2, None).unwrap();
        }
        let result = ensure_safe_to_push(&state);
        assert!(
            result.is_err(),
            "expected Err when active rotation job exists"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("rotation"),
            "error must mention rotation: {msg}"
        );
    }

    /// C1: ensure_safe_to_push passes when the only rotation job is in done state.
    #[test]
    fn ensure_safe_to_push_ignores_done_and_aborted_rotation_jobs() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            let id = db::insert_rotation_job(&conn, "fp-old", "fp-new", 1, 2, None).unwrap();
            db::update_rotation_job_state(&conn, id, "done").unwrap();
        }
        assert!(
            ensure_safe_to_push(&state).is_ok(),
            "done rotation job must NOT block push"
        );
        // Also check aborted.
        {
            let conn = state.lock().unwrap();
            let id = db::insert_rotation_job(&conn, "fp-a", "fp-b", 3, 4, None).unwrap();
            db::update_rotation_job_state(&conn, id, "aborted").unwrap();
        }
        assert!(
            ensure_safe_to_push(&state).is_ok(),
            "aborted rotation job must NOT block push"
        );
    }

    /// C2: run_push_entry (via ensure_safe_to_push) blocks when an active
    /// rotation job exists, independently of the force-re-pair flag.
    ///
    /// The full run_push_entry requires a configured provider and AppHandle —
    /// not available in unit tests. We verify the guard directly, which is
    /// the seam called at the top of run_push_entry.
    #[test]
    fn run_push_entry_blocks_when_active_rotation_job_exists() {
        let state = make_state();
        {
            let conn = state.lock().unwrap();
            db::insert_rotation_job(&conn, "fp-old", "fp-new", 5, 6, None).unwrap();
        }
        let result = ensure_safe_to_push(&state);
        assert!(
            result.is_err(),
            "ensure_safe_to_push (called by run_push_entry) must block for active rotation job"
        );
    }

    // ─── get_force_re_pair_status ────────────────────────────────────────────

    #[test]
    fn get_force_re_pair_status_returns_none_when_flag_unset() {
        let conn = setup();
        assert_eq!(get_force_re_pair_status_inner(&conn).unwrap(), None);
    }

    #[test]
    fn get_force_re_pair_status_returns_reason_when_flag_set() {
        let conn = setup();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REASON, "vault_rotated_on_peer").unwrap();
        assert_eq!(
            get_force_re_pair_status_inner(&conn).unwrap(),
            Some("vault_rotated_on_peer".to_string())
        );
    }

    #[test]
    fn get_force_re_pair_status_returns_fallback_reason_when_reason_missing() {
        // Some callsites only set the flag and skip the reason (the V1 wipe
        // path used to be one — see gdrive.rs). The frontend still needs a
        // non-empty string so it can render the ForceRePairScreen — give it
        // a generic fallback instead of silently swallowing the trigger.
        let conn = setup();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "1").unwrap();
        let status = get_force_re_pair_status_inner(&conn).unwrap();
        assert!(status.is_some(), "flag is set, must report non-None");
        assert!(
            !status.unwrap().is_empty(),
            "reason must be a non-empty fallback string"
        );
    }

    #[test]
    fn get_force_re_pair_status_ignores_empty_flag_value() {
        let conn = setup();
        db::set_setting(&conn, db::FORCE_RE_PAIR_REQUIRED, "").unwrap();
        assert_eq!(get_force_re_pair_status_inner(&conn).unwrap(), None);
    }

    /// `complete_force_re_pair` re-asserts the `sync_provider` value that was
    /// stored before the connection swap. A folder-provider install must keep
    /// `"local"` instead of being overwritten with `"gdrive"`.
    #[test]
    fn complete_force_re_pair_keeps_stored_local_sync_provider() {
        let conn = setup();
        db::set_setting(&conn, "sync_provider", "local").unwrap();

        let prior = db::get_sync_provider(&conn).unwrap();
        clear_force_re_pair_and_reassert_sync(&conn, prior.as_deref());

        let provider = db::get_setting(&conn, "sync_provider")
            .unwrap()
            .expect("sync_provider must still be set");
        assert_eq!(
            provider, "local",
            "complete_force_re_pair must keep the stored sync_provider"
        );
    }
}
