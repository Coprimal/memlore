//! iCloud Drive on mobile (M2b): the folder the user picked, behind the
//! [`FolderAccess`] Swift implements. Swift owns the picker, the
//! security-scoped bookmark, the scope and the placeholder prefetch; Rust
//! runs the engine's plain-I/O `LocalSyncProvider` (kind `icloud`) on the
//! path `prepare` hands out, exactly what the desktop does in the same
//! folder.
//!
//! Before every use of an iCloud folder (connect, each sync, the joins with
//! the phrase) `prepare` runs first, never under the DB lock and never on
//! an async runtime thread. The folder is never created here: a root that
//! is gone fails with `FolderUnavailable` instead of being recreated
//! empty by the engine's `create_dir_all`. Every path that drops the
//! connection calls `forget`, so no bookmark (a standing grant) outlives
//! it. Nothing here logs more of a path than its last component.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memlore_engine::db;
use memlore_engine::sync::cloud_config::LocalConfig;
use memlore_engine::sync::guard::{SyncInProgressGuard, SYNC_IN_PROGRESS_ERR};
use memlore_engine::sync::pending_session::PendingCloudSession;
use memlore_engine::sync::{CloudProvider, LocalSyncProvider};
use memlore_engine::vault::startup::StartupMode;

use crate::drive::{random_hex, ConnectOutcomeFfi};
use crate::{io, MemloreEngine, MemloreError};

/// The `sync_provider` of an iCloud Drive folder. The folder checks apply
/// to it only: kind `local` (test-only on mobile) runs without a
/// [`FolderAccess`].
pub(crate) const ICLOUD: &str = "icloud";

/// The engine's refusal of a non-empty folder without a recovery control
/// (`complete_cloud_connect`): a folder that holds something else.
const NOT_A_SYNC_FOLDER: &str = "RECOVERY_CONTROL_MISSING_ON_NONEMPTY_CLOUD";

/// What [`FolderAccess::prepare`] found.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FolderPrepareFfi {
    /// The folder resolved and its scope is open: use `root_path` (it may
    /// differ from the stored one after a move).
    Ready { root_path: String },
    /// No folder: never picked, forgotten, deleted, in the Trash, iCloud
    /// signed out, or any failure on the Swift side.
    Gone,
}

/// The picked iCloud Drive folder, implemented in Swift. Called on an FFI
/// caller's thread or a blocking runtime thread, never under the DB lock;
/// an implementation must not call back into the engine. Infallible: a
/// Swift failure is `Gone`.
#[uniffi::export(with_foreign)]
pub trait FolderAccess: Send + Sync {
    /// Resolve the bookmark, keep its security scope open, refuse a folder
    /// that is gone and start downloading placeholders (without waiting).
    fn prepare(&self) -> FolderPrepareFfi;
    /// Drop the bookmark and close its scope.
    fn forget(&self);
}

#[uniffi::export]
impl MemloreEngine {
    /// Where the iCloud folder comes from. Replaces the previous one. At a
    /// first launch it forgets at once: a bookmark restored from a backup
    /// belongs to no vault on this phone.
    pub fn set_folder_access(&self, access: Arc<dyn FolderAccess>) {
        *self.folder_access.lock().unwrap_or_else(|p| p.into_inner()) = Some(access.clone());
        let first_launch = *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner())
            == StartupMode::FirstLaunch;
        // Once per process: a second registration (a JS reload creates a new
        // engine) must not drop a folder picked during this first launch.
        static FIRST_LAUNCH_FORGET: std::sync::Once = std::sync::Once::new();
        if first_launch {
            FIRST_LAUNCH_FORGET.call_once(|| access.forget());
        }
    }

    /// Connect the folder [`FolderAccess`] holds, with the same outcomes
    /// as a Drive connect. The folder must exist (it is never created): a
    /// missing one is `FolderUnavailable`, one holding something other
    /// than a sync folder is `NotASyncFolder`. Once the folder is prepared,
    /// the bookmark is forgotten whenever the connect does not leave a
    /// connection or an onboarding behind (`NoCloudVault`, any error). A
    /// refusal before that (a running sync, `Locked`) keeps it, so a retry
    /// can still connect. Needs first launch or an unlocked vault.
    pub fn icloud_connect(&self) -> Result<ConnectOutcomeFfi, MemloreError> {
        let guard = SyncInProgressGuard::try_acquire()
            .ok_or_else(|| io(SYNC_IN_PROGRESS_ERR.to_string()))?;
        let local_mode = self.connect_mode()?;
        let result = self.icloud_connect_inner(guard, local_mode);
        if matches!(result, Ok(ConnectOutcomeFfi::NoCloudVault) | Err(_)) {
            self.forget_folder();
        }
        result
    }
}

impl MemloreEngine {
    fn icloud_connect_inner(
        &self,
        guard: SyncInProgressGuard,
        local_mode: db::EncryptionMode,
    ) -> Result<ConnectOutcomeFfi, MemloreError> {
        let root = checked_root(self.folder_access().map(|access| access.prepare()))?;
        self.connect_folder_kind(guard, &random_hex(), local_mode, ICLOUD, root)
            .map_err(|e| match e {
                MemloreError::Io { msg } if msg == NOT_A_SYNC_FOLDER => {
                    MemloreError::NotASyncFolder
                }
                other => other,
            })
    }

    /// The folder connect shared by iCloud and the `local` test kind: the
    /// provider-agnostic tail with a folder cloud at `root`. The engine
    /// stamps the recovery fence on the connect's provider itself; the
    /// cleanup handle is stamped by the probe, as for Drive.
    pub(crate) fn connect_folder_kind(
        &self,
        guard: SyncInProgressGuard,
        session_id: &str,
        local_mode: db::EncryptionMode,
        kind: &str,
        root: PathBuf,
    ) -> Result<ConnectOutcomeFfi, MemloreError> {
        let folder = || CloudProvider::Folder(LocalSyncProvider::new(&root));
        let pending = PendingCloudSession::Folder {
            kind: kind.to_string(),
            root_path: root.to_string_lossy().into_owned(),
        };
        self.finish_connect(guard, session_id, local_mode, folder(), folder(), pending)
    }

    /// The registered [`FolderAccess`], taken out of its mutex so no lock
    /// is held while Swift runs.
    pub(crate) fn folder_access(&self) -> Option<Arc<dyn FolderAccess>> {
        self.folder_access
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Drop the bookmark, if a [`FolderAccess`] is registered.
    pub(crate) fn forget_folder(&self) {
        if let Some(access) = self.folder_access() {
            access.forget();
        }
    }

    /// The root of a pending iCloud session (an onboarding), prepared now.
    pub(crate) fn prepare_pending_root(&self) -> Result<PathBuf, MemloreError> {
        checked_root(self.folder_access().map(|access| access.prepare()))
    }

    /// Before a configured iCloud folder is used by a blocking FFI call
    /// (re-pair, recheck): [`FolderAccess::prepare`] on this thread, then
    /// [`Self::adopt_folder_root`]. Nothing for any other provider.
    pub(crate) fn prepare_folder_root(&self) -> Result<(), MemloreError> {
        if !self.icloud_configured()? {
            return Ok(());
        }
        let answer = self.folder_access().map(|access| access.prepare());
        self.adopt_folder_root(answer)
    }

    /// [`Self::prepare_folder_root`] for the sync task: the foreign call on
    /// a blocking thread, so a `lock()` or a cancel lands at this await.
    pub(crate) async fn prepare_folder_root_async(&self) -> Result<(), MemloreError> {
        if !self.icloud_configured()? {
            return Ok(());
        }
        let answer = match self.folder_access() {
            // A panicked prepare reads as `Gone` (fail closed).
            Some(access) => tokio::task::spawn_blocking(move || access.prepare())
                .await
                .ok(),
            None => None,
        };
        self.adopt_folder_root(answer)
    }

    fn icloud_configured(&self) -> Result<bool, MemloreError> {
        let conn = self.state.lock().map_err(io)?;
        let provider = db::get_sync_provider(&conn).map_err(|e| io(e.to_string()))?;
        Ok(provider.as_deref() == Some(ICLOUD))
    }

    /// Take `prepare`'s answer for the configured folder: a usable root
    /// clears `NeedsFolder` and, when the folder moved, replaces the stored
    /// `root_path` (device-local, never synced); anything else is
    /// `FolderUnavailable` and `NeedsFolder` until a later `Ready` or a
    /// disconnect.
    fn adopt_folder_root(&self, answer: Option<FolderPrepareFfi>) -> Result<(), MemloreError> {
        let root = checked_root(answer);
        self.sync_report.set_needs_folder(root.is_err());
        let root = root?;
        let root_path = root.to_string_lossy().into_owned();
        let conn = self.state.lock().map_err(io)?;
        // Read again under this lock: a disconnect since the check leaves
        // nothing to rewrite.
        if db::get_sync_provider(&conn)
            .map_err(|e| io(e.to_string()))?
            .as_deref()
            != Some(ICLOUD)
        {
            return Ok(());
        }
        if stored_root_path(&conn)?.as_deref() == Some(root_path.as_str()) {
            return Ok(());
        }
        let json =
            serde_json::to_string(&LocalConfig { root_path }).map_err(|e| io(e.to_string()))?;
        db::set_sync_config_json(&conn, &json).map_err(|e| io(e.to_string()))?;
        log::info!(
            "icloud: the sync folder moved to …/{}",
            folder_name(&root).unwrap_or_default()
        );
        Ok(())
    }
}

/// The root in `prepare`'s answer, if it is an existing directory at an
/// absolute path. Never creates it. No answer (no [`FolderAccess`], or a
/// failed call) is `Gone`.
fn checked_root(answer: Option<FolderPrepareFfi>) -> Result<PathBuf, MemloreError> {
    match answer {
        Some(FolderPrepareFfi::Ready { root_path }) => {
            let root = PathBuf::from(root_path);
            if root.is_absolute() && root.is_dir() {
                Ok(root)
            } else {
                Err(MemloreError::FolderUnavailable)
            }
        }
        Some(FolderPrepareFfi::Gone) | None => Err(MemloreError::FolderUnavailable),
    }
}

/// The `root_path` of the stored folder sync config, if any.
pub(crate) fn stored_root_path(
    conn: &rusqlite::Connection,
) -> Result<Option<String>, MemloreError> {
    let Some(json) = db::get_sync_config_json(conn).map_err(|e| io(e.to_string()))? else {
        return Ok(None);
    };
    let config: LocalConfig = serde_json::from_str(&json).map_err(|e| io(e.to_string()))?;
    Ok(Some(config.root_path))
}

/// The last component of a folder path: all a status or a log shows.
pub(crate) fn folder_name(root: &Path) -> Option<String> {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use memlore_engine::db;

    use super::*;
    use crate::drive::ConnectOutcomeFfi;
    use crate::sync::test_support::{cloud_tree, engine_at, serial, sync_and_wait, DesktopRole};
    use crate::sync::{SyncBlockedFfi, SyncPhaseFfi};
    use crate::{MemloreEngine, MemloreError};

    /// A [`FolderAccess`] that answers what the test set and counts calls.
    struct FolderStub {
        prepares: AtomicUsize,
        forgets: AtomicUsize,
        next: Mutex<FolderPrepareFfi>,
    }

    impl FolderStub {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                prepares: AtomicUsize::new(0),
                forgets: AtomicUsize::new(0),
                next: Mutex::new(FolderPrepareFfi::Gone),
            })
        }

        fn answer(&self, answer: FolderPrepareFfi) {
            *self.next.lock().unwrap() = answer;
        }

        fn forgets(&self) -> usize {
            self.forgets.load(Ordering::SeqCst)
        }
    }

    impl FolderAccess for FolderStub {
        fn prepare(&self) -> FolderPrepareFfi {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            self.next.lock().unwrap().clone()
        }

        fn forget(&self) {
            self.forgets.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ready(path: &Path) -> FolderPrepareFfi {
        FolderPrepareFfi::Ready {
            root_path: path.to_string_lossy().into_owned(),
        }
    }

    /// A created (unlocked) phone vault in `dir` with a [`FolderStub`]
    /// registered after the setup, so the registration forgets nothing.
    fn phone_at(dir: &Path) -> (Arc<MemloreEngine>, Arc<FolderStub>) {
        let phone = engine_at(dir, Arc::default());
        phone.create_vault("password12".to_string()).unwrap();
        let access = FolderStub::new();
        phone.set_folder_access(access.clone());
        (phone, access)
    }

    fn setting(engine: &MemloreEngine, key: &str) -> Option<String> {
        db::get_setting(&engine.state.lock().unwrap(), key).unwrap()
    }

    fn stored_root(engine: &MemloreEngine) -> Option<String> {
        let json = setting(engine, "sync_config_json")?;
        let config: serde_json::Value = serde_json::from_str(&json).unwrap();
        config["root_path"].as_str().map(str::to_string)
    }

    /// Risk 4: a missing folder is never created (by the FFI or by the
    /// engine's `create_dir_all`), with or without a `FolderAccess`; a
    /// folder holding something else is refused as no sync folder and
    /// nothing is written into it; an empty folder connects a phone vault
    /// as kind `icloud` at that path.
    #[test]
    fn icloud_connect_never_creates_a_missing_root_and_refuses_a_non_vault_folder() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        phone.create_vault("password12".to_string()).unwrap();
        match phone.icloud_connect() {
            Err(MemloreError::FolderUnavailable) => {}
            other => panic!("no FolderAccess: expected FolderUnavailable, got {other:?}"),
        }
        let access = FolderStub::new();
        phone.set_folder_access(access.clone());

        let missing = cloud.path().join("Memlore");
        for answer in [ready(&missing), FolderPrepareFfi::Gone] {
            access.answer(answer.clone());
            match phone.icloud_connect() {
                Err(MemloreError::FolderUnavailable) => {}
                other => panic!("{answer:?}: expected FolderUnavailable, got {other:?}"),
            }
            assert!(
                !missing.exists(),
                "{answer:?}: the missing folder was created"
            );
        }

        let notes = cloud.path().join("Notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("notes.txt"), b"not a vault").unwrap();
        let before = cloud_tree(&notes);
        access.answer(ready(&notes));
        match phone.icloud_connect() {
            Err(MemloreError::NotASyncFolder) => {}
            other => panic!("a non-vault folder: expected NotASyncFolder, got {other:?}"),
        }
        assert_eq!(
            cloud_tree(&notes),
            before,
            "the refusal wrote into the folder"
        );
        assert_eq!(setting(&phone, "sync_provider"), None);

        let empty = cloud.path().join("Empty");
        std::fs::create_dir(&empty).unwrap();
        access.answer(ready(&empty));
        assert_eq!(
            phone.icloud_connect().unwrap(),
            ConnectOutcomeFfi::Connected
        );
        assert_eq!(setting(&phone, "sync_provider").as_deref(), Some("icloud"));
        assert_eq!(
            stored_root(&phone).as_deref(),
            Some(empty.to_string_lossy().as_ref())
        );
    }

    /// Risk 3: a bookmark is a standing grant, so every path that drops the
    /// iCloud connection forgets it, once: the first-launch registration
    /// (a bookmark restored from a backup), disconnect, vault deletion, a
    /// refusal of another vault (a phone holding an entry), a first-launch
    /// connect to a folder without a vault, and a cancelled onboarding.
    #[test]
    fn forget_runs_when_the_icloud_connection_is_dropped() {
        let _serial = serial();
        let fresh_dir = tempfile::TempDir::new().unwrap();
        let fresh = engine_at(fresh_dir.path(), Arc::default());
        let fresh_access = FolderStub::new();
        fresh.set_folder_access(fresh_access.clone());
        assert_eq!(fresh_access.forgets(), 1, "first launch kept the bookmark");

        let cloud = tempfile::TempDir::new().unwrap();
        let phone_dir = tempfile::TempDir::new().unwrap();
        let (phone, access) = phone_at(phone_dir.path());
        access.answer(ready(cloud.path()));
        // Refusals before the folder is touched keep the bookmark, so the
        // retry after a running sync or an unlock can still connect.
        {
            let _sync = SyncInProgressGuard::try_acquire().expect("a sync holds the guard");
            match phone.icloud_connect() {
                Err(MemloreError::Io { msg }) if msg == SYNC_IN_PROGRESS_ERR => {}
                other => panic!("a running sync: expected {SYNC_IN_PROGRESS_ERR}, got {other:?}"),
            }
        }
        assert_eq!(access.forgets(), 0, "a busy refusal forgot the bookmark");
        phone.lock().unwrap();
        match phone.icloud_connect() {
            Err(MemloreError::Locked) => {}
            other => panic!("a locked vault: expected Locked, got {other:?}"),
        }
        assert_eq!(access.forgets(), 0, "a locked refusal forgot the bookmark");
        phone.unlock("password12".to_string()).unwrap();
        assert_eq!(
            phone.icloud_connect().unwrap(),
            ConnectOutcomeFfi::Connected
        );
        assert_eq!(access.forgets(), 0, "a connect forgot the bookmark");
        phone.drive_disconnect().unwrap();
        assert_eq!(access.forgets(), 1, "disconnect kept the bookmark");
        phone.delete_vault("password12".to_string()).unwrap();
        assert_eq!(access.forgets(), 2, "deleting the vault kept the bookmark");

        let foreign = tempfile::TempDir::new().unwrap();
        let desktop = DesktopRole::first_launch();
        desktop.create_vault("password34");
        desktop.connect(foreign.path(), "forget-desktop");
        let holder_dir = tempfile::TempDir::new().unwrap();
        let (holder, holder_access) = phone_at(holder_dir.path());
        holder.create_entry("Mine".to_string(), None).unwrap();
        holder_access.answer(ready(foreign.path()));
        match holder.icloud_connect() {
            Err(MemloreError::VaultNotEmpty) => {}
            other => panic!("expected VaultNotEmpty, got {other:?}"),
        }
        assert_eq!(holder_access.forgets(), 1, "the refusal kept the bookmark");

        let empty = tempfile::TempDir::new().unwrap();
        fresh_access.answer(ready(empty.path()));
        assert_eq!(
            fresh.icloud_connect().unwrap(),
            ConnectOutcomeFfi::NoCloudVault
        );
        assert_eq!(fresh_access.forgets(), 2, "NoCloudVault kept the bookmark");
        fresh_access.answer(ready(foreign.path()));
        let ConnectOutcomeFfi::NeedsOnboarding { session_id } = fresh.icloud_connect().unwrap()
        else {
            panic!("a first launch did not offer the onboarding");
        };
        assert_eq!(fresh_access.forgets(), 2);
        fresh.drive_connect_cancel(session_id);
        assert_eq!(fresh_access.forgets(), 3, "the cancel kept the bookmark");
    }

    /// Risk 4 at sync time: a folder that moved is followed (the stored
    /// path is rewritten and the cycle pushes there); a folder that is gone
    /// ends the sync in `NeedsFolder` without recreating it, whether Swift
    /// says `Gone` or still hands out the old path. Disconnect clears it.
    #[test]
    fn a_sync_whose_folder_is_gone_fails_with_needs_folder_and_recreates_nothing() {
        let _serial = serial();
        let cloud = tempfile::TempDir::new().unwrap();
        let first = cloud.path().join("Memlore");
        let moved = cloud.path().join("Moved");
        std::fs::create_dir(&first).unwrap();
        let phone_dir = tempfile::TempDir::new().unwrap();
        let (phone, access) = phone_at(phone_dir.path());
        phone.create_entry("Before".to_string(), None).unwrap();
        access.answer(ready(&first));
        assert_eq!(
            phone.icloud_connect().unwrap(),
            ConnectOutcomeFfi::Connected
        );
        assert_eq!(sync_and_wait(&phone).phase, SyncPhaseFfi::Synced);

        std::fs::rename(&first, &moved).unwrap();
        access.answer(ready(&moved));
        phone
            .create_entry("After the move".to_string(), None)
            .unwrap();
        let status = sync_and_wait(&phone);
        assert_eq!(status.phase, SyncPhaseFfi::Synced, "{status:?}");
        assert_eq!(status.blocked, None);
        assert_eq!(
            status.pending_count, 0,
            "nothing was pushed to the moved folder"
        );
        assert_eq!(
            stored_root(&phone).as_deref(),
            Some(moved.to_string_lossy().as_ref())
        );

        std::fs::remove_dir_all(&moved).unwrap();
        for answer in [FolderPrepareFfi::Gone, ready(&moved)] {
            access.answer(answer.clone());
            let status = sync_and_wait(&phone);
            assert_eq!(status.phase, SyncPhaseFfi::Error, "{answer:?}: {status:?}");
            assert_eq!(
                status.blocked,
                Some(SyncBlockedFfi::NeedsFolder),
                "{answer:?}"
            );
            assert!(
                !moved.exists() && !first.exists(),
                "{answer:?}: the sync recreated the folder"
            );
        }
        phone.drive_disconnect().unwrap();
        assert_eq!(phone.sync_status().unwrap().blocked, None);
    }
}
