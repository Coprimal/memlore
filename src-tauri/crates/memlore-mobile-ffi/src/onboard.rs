//! Joining a cloud vault with the recovery phrase on the phone: onboarding
//! at first launch, the in-session re-pair after another device rotated the
//! vault (or an empty phone vault joining one), and the re-pair recheck.
//! The phrase and the password are zeroized on drop and never logged.

use memlore_engine::db;
use memlore_engine::sync::cloud_config::configured_cloud_provider;
use memlore_engine::sync::force_re_pair::{
    apply_recheck_decision, complete_force_re_pair_inner, confirm_post_clear_verification,
    detect_force_re_pair, resolve_force_re_pair_outcome, ForceRePairDecision,
};
use memlore_engine::sync::gdrive_provider::GDriveProvider;
use memlore_engine::sync::guard::SyncInProgressGuard;
use memlore_engine::sync::pending_session::{self, PendingCloudSession};
use memlore_engine::sync::{CloudProvider, LocalSyncProvider};
use memlore_engine::utils::boot_file;
use memlore_engine::vault::onboard::{
    onboard_complete_inner, onboard_validate_passphrase_inner, WRONG_RECOVERY_PHRASE_ERROR,
};
use memlore_engine::vault::startup::StartupMode;
use memlore_engine::vault::validate_password_length;
use zeroize::Zeroizing;

use crate::{format_err, io, runtime, MemloreEngine, MemloreError, DEVICE_NAME};

/// The engine's wrong-phrase text is a contract string: it maps to
/// `WrongPhrase`, anything else stays an `Io` error.
fn phrase_err(e: String) -> MemloreError {
    if e == WRONG_RECOVERY_PHRASE_ERROR {
        MemloreError::WrongPhrase
    } else {
        io(e)
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// Check the phrase against the cloud vault of `session_id` (from
    /// `drive_connect_complete`), changing nothing. Only where a join could
    /// follow: an empty first launch (the onboarding guard, same error) or
    /// an unlocked vault (the re-pair may pre-check a phrase), so a locked
    /// or populated vault is no phrase oracle.
    pub fn onboard_validate_phrase(
        &self,
        session_id: String,
        phrase: String,
    ) -> Result<(), MemloreError> {
        let phrase = Zeroizing::new(phrase);
        if !self.key_state.is_initialized().map_err(io)? {
            self.require_empty_first_launch()?;
        }
        let provider = self.join_provider(Some(&session_id))?;
        runtime()?
            .block_on(onboard_validate_passphrase_inner(&provider, &phrase))
            .map_err(phrase_err)
    }

    /// Join the cloud vault of `session_id` at first launch: this phone gets
    /// `password`, its device slot and the Drive settings, and is unlocked
    /// afterwards (`startup_mode` reports `PasswordLocked`, as after
    /// `create_vault`). Refused with `VaultNotEmpty`, before anything is
    /// written, unless this is a first launch with no entry at all (trashed
    /// ones count, decision 4).
    pub fn onboard_complete(
        &self,
        session_id: String,
        phrase: String,
        password: String,
    ) -> Result<(), MemloreError> {
        let phrase = Zeroizing::new(phrase);
        let password = Zeroizing::new(password);
        self.require_empty_first_launch()?;
        validate_password_length(&password).map_err(format_err)?;
        let provider = self.join_provider(Some(&session_id))?;
        let runtime = runtime()?;
        self.end_lock_session();

        // The desktop's connection swap: the engine takes the real
        // connection while it rekeys; a migrated placeholder answers
        // meanwhile.
        let real_conn = {
            let placeholder = rusqlite::Connection::open_in_memory()
                .map_err(|e| io(format!("Failed to create placeholder connection: {e}")))?;
            db::schema::migrate(&placeholder)
                .map_err(|e| io(format!("Failed to migrate placeholder: {e}")))?;
            db::set_encryption_mode(&placeholder, db::EncryptionMode::Unset)
                .map_err(|e| io(format!("Failed to seed placeholder mode: {e}")))?;
            let mut guard = self.state.lock().map_err(io)?;
            // The guard again, on the connection about to be handed over:
            // an entry written since the first check refuses here.
            if !db::count_all_entries(&guard).is_ok_and(|n| n == 0) {
                return Err(MemloreError::VaultNotEmpty);
            }
            std::mem::replace(&mut *guard, placeholder)
        };
        let boot_path = boot_file::boot_path(&self.db_path);
        let result = runtime.block_on(onboard_complete_inner(
            real_conn,
            &self.key_state,
            &provider,
            &phrase,
            &password,
            DEVICE_NAME,
            Some(&boot_path),
            Some(session_id),
            None,
            &self.hooks,
        ));
        match result {
            Ok(conn) => {
                self.state.set_connection(conn).map_err(io)?;
                *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner()) =
                    StartupMode::PasswordLocked;
                self.rearm_sync_on_launch();
                Ok(())
            }
            Err((conn, e)) => {
                // The engine hands the real connection back on every error.
                if let Err(restore) = self.state.set_connection(conn) {
                    log::error!("onboard_complete: the connection was not restored ({restore})");
                }
                Err(phrase_err(e))
            }
        }
    }

    /// Re-pair this unlocked phone with the cloud vault through the phrase
    /// (after another device rotated it, or to let an empty phone vault join
    /// it): the vault takes the cloud's keys and `password`. Keeps the
    /// phone's entries (no entry guard here).
    pub fn re_pair(&self, phrase: String, password: String) -> Result<(), MemloreError> {
        let phrase = Zeroizing::new(phrase);
        let password = Zeroizing::new(password);
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        validate_password_length(&password).map_err(format_err)?;
        let provider = self.join_provider(None)?;
        let boot_path = boot_file::boot_path(&self.db_path);
        runtime()?
            .block_on(complete_force_re_pair_inner(
                &self.state,
                &self.key_state,
                &provider,
                &phrase,
                &password,
                Some(&boot_path),
                &self.hooks,
            ))
            .map_err(phrase_err)
    }

    /// Check again whether the cloud still needs a re-pair, like the
    /// desktop `recheck_force_re_pair`: `true` when the block was cleared
    /// (the vault verified in sync), `false` when it stands, there is no
    /// cloud to check, or a sync is running.
    pub fn recheck_re_pair(&self) -> Result<bool, MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        let Ok(provider) = self.join_provider(None) else {
            return Ok(false);
        };
        // No push may start between the clear and the second check.
        let Some(_guard) = SyncInProgressGuard::try_acquire() else {
            return Ok(false);
        };
        let runtime = runtime()?;
        let decision = resolve_force_re_pair_outcome(
            runtime.block_on(detect_force_re_pair(&provider, &self.state)),
        );
        let was_verified = matches!(decision, ForceRePairDecision::Verified);
        let cleared = {
            let conn = self.state.lock().map_err(io)?;
            apply_recheck_decision(&conn, decision).map_err(io)?
        };
        if cleared && was_verified {
            let second = runtime.block_on(detect_force_re_pair(&provider, &self.state));
            let conn = self.state.lock().map_err(io)?;
            return confirm_post_clear_verification(&conn, second).map_err(io);
        }
        Ok(cleared)
    }
}

impl MemloreEngine {
    /// The onboarding guard (decision 4), only here and never in the
    /// engine: a first launch whose database holds no entry, trashed ones
    /// included. A count that cannot be read refuses.
    fn require_empty_first_launch(&self) -> Result<(), MemloreError> {
        let first_launch = *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner())
            == StartupMode::FirstLaunch;
        let empty = {
            let conn = self.state.lock().map_err(io)?;
            db::count_all_entries(&conn).is_ok_and(|n| n == 0)
        };
        if first_launch && empty {
            Ok(())
        } else {
            Err(MemloreError::VaultNotEmpty)
        }
    }

    /// The cloud to join, in the desktop's lookup order: the live Drive
    /// session, then the pending session of `session_id`, then the
    /// configured provider.
    fn join_provider(&self, session_id: Option<&str>) -> Result<CloudProvider, MemloreError> {
        let live = self
            .live_session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(session) = live {
            return GDriveProvider::new_live(session)
                .map(CloudProvider::GDrive)
                .map_err(io);
        }
        if let Some(pending) = session_id.and_then(pending_session::peek) {
            return match pending {
                PendingCloudSession::GDrive { session, .. } => GDriveProvider::new_live(session)
                    .map(CloudProvider::GDrive)
                    .map_err(io),
                PendingCloudSession::Folder { root_path, .. } => {
                    let generation = {
                        let conn = self.state.lock().map_err(io)?;
                        db::get_sync_recovery_generation(&conn).map_err(|e| io(e.to_string()))?
                    };
                    Ok(CloudProvider::Folder(
                        LocalSyncProvider::new(root_path).with_recovery_fence(generation, None),
                    ))
                }
            };
        }
        let conn = self.state.lock().map_err(io)?;
        configured_cloud_provider(&conn, self.credentials.as_ref())
            .map_err(io)?
            .ok_or_else(|| {
                io("No cloud provider is connected on this device — connect one first, then retry."
                    .to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use memlore_engine::db;
    use memlore_engine::utils::boot_file;

    use super::*;
    use crate::drive::ConnectOutcomeFfi;
    use crate::sync::test_support::{cloud_tree, connect_folder, engine_at, serial};
    use crate::{MemloreError, StartupModeFfi};

    /// Everything a refused onboarding must leave as it was.
    #[derive(Debug, PartialEq)]
    struct Untouched {
        mode: StartupModeFfi,
        unlocked: bool,
        entries: Vec<(String, bool)>,
        boot: Option<Vec<u8>>,
        cloud: BTreeMap<String, Vec<u8>>,
    }

    fn untouched(
        engine: &MemloreEngine,
        dir: &std::path::Path,
        cloud: &std::path::Path,
    ) -> Untouched {
        let entries = {
            let conn = engine.state.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT id, is_deleted FROM entries ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        Untouched {
            mode: engine.startup_mode(),
            unlocked: engine.key_state.is_initialized().unwrap(),
            entries,
            boot: std::fs::read(boot_file::boot_path(&dir.join("memlore.db"))).ok(),
            cloud: cloud_tree(cloud),
        }
    }

    /// Decision 4: onboarding never merges a phone vault holding entries
    /// (trashed ones included) into a cloud vault. Refused with
    /// `VaultNotEmpty` before anything is written: startup mode, keys,
    /// entry rows, boot file and the cloud vault (no device slot) stay as
    /// they were. Both halves of the guard are hit: a first-launch database
    /// that holds entries (the count), and a created vault, unlocked or
    /// locked (the startup mode: locked, the connection is an empty
    /// placeholder). The same session and phrase then onboard an empty
    /// first-launch phone, so only the guard refused.
    #[test]
    fn onboarding_refuses_a_phone_vault_with_entries_and_leaves_it_untouched() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let desktop_dir = tempfile::TempDir::new().unwrap();
        let desktop = engine_at(desktop_dir.path(), Arc::default());
        let phrase = desktop.create_vault("password12".to_string()).unwrap();
        assert_eq!(
            connect_folder(&desktop, cloud.path(), "onboard-desktop").unwrap(),
            ConnectOutcomeFfi::Connected
        );

        let fresh_dir = tempfile::TempDir::new().unwrap();
        let fresh = engine_at(fresh_dir.path(), Arc::default());
        let session_id = "onboard-refusal";
        assert_eq!(
            connect_folder(&fresh, cloud.path(), session_id).unwrap(),
            ConnectOutcomeFfi::NeedsOnboarding {
                session_id: session_id.to_string()
            }
        );

        let seeded_dir = tempfile::TempDir::new().unwrap();
        let seeded = engine_at(seeded_dir.path(), Arc::default());
        {
            let conn = seeded.state.lock().unwrap();
            let journal = db::list_journals(&conn, None).unwrap()[0].id.clone();
            for title in ["kept", "trashed"] {
                let entry = db::create_entry(
                    &conn,
                    db::CreateEntryParams {
                        journal_id: &journal,
                        title: Some(title),
                        content_text: None,
                        preview_text: None,
                        entry_date: 0,
                    },
                )
                .unwrap();
                if title == "trashed" {
                    db::trash_entry(&conn, &entry.id, 1).unwrap();
                }
            }
        }
        let created_dir = tempfile::TempDir::new().unwrap();
        let created = engine_at(created_dir.path(), Arc::default());
        created.create_vault("password34".to_string()).unwrap();
        let entry = created.create_entry("Mine".to_string(), None).unwrap();
        created.trash_entry(entry).unwrap();
        // Locked, the live connection is the empty placeholder: only the
        // startup mode tells this vault apart from a first launch.
        let locked_dir = tempfile::TempDir::new().unwrap();
        let locked = engine_at(locked_dir.path(), Arc::default());
        locked.create_vault("password34".to_string()).unwrap();
        let kept = locked.create_entry("Kept".to_string(), None).unwrap();
        locked.lock().unwrap();

        // No phrase oracle either: the check is refused the same way unless
        // the vault is unlocked (the in-session re-pair may pre-check).
        for (case, engine) in [
            ("first launch holding entries", &seeded),
            ("locked vault holding an entry", &locked),
        ] {
            match engine.onboard_validate_phrase(session_id.to_string(), phrase.clone()) {
                Err(MemloreError::VaultNotEmpty) => {}
                other => panic!("{case}: phrase check expected VaultNotEmpty, got {other:?}"),
            }
        }

        for (case, engine, dir) in [
            ("first launch holding entries", &seeded, seeded_dir.path()),
            ("created vault, trashed entry", &created, created_dir.path()),
            ("locked vault holding an entry", &locked, locked_dir.path()),
        ] {
            let before = untouched(engine, dir, cloud.path());
            match engine.onboard_complete(
                session_id.to_string(),
                phrase.clone(),
                "password56".to_string(),
            ) {
                Err(MemloreError::VaultNotEmpty) => {}
                other => panic!("{case}: expected VaultNotEmpty, got {other:?}"),
            }
            assert_eq!(untouched(engine, dir, cloud.path()), before, "{case}");
        }
        locked.unlock("password34".to_string()).unwrap();
        assert!(
            locked.get_entry(kept).is_ok(),
            "the locked vault lost its entry"
        );

        let cloud_before = cloud_tree(cloud.path());
        fresh
            .onboard_validate_phrase(session_id.to_string(), phrase.clone())
            .unwrap();
        fresh
            .onboard_complete(session_id.to_string(), phrase, "password56".to_string())
            .unwrap();
        assert_eq!(fresh.startup_mode(), StartupModeFfi::PasswordLocked);
        assert!(fresh.key_state.is_initialized().unwrap());
        assert_ne!(cloud_tree(cloud.path()), cloud_before, "no device slot");
        assert_eq!(
            db::get_setting(&fresh.state.lock().unwrap(), "sync_provider")
                .unwrap()
                .as_deref(),
            Some("local"),
            "onboarding did not persist the pending session"
        );
    }
}
