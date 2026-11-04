//! UniFFI surface of `memlore-engine` for the mobile app (proc-macro mode).
//!
//! FFI surface rule: every data method of [`MemloreEngine`] runs behind the
//! lock gate ([`MemloreEngine::with_unlocked`]) and returns
//! [`MemloreError::Locked`] while the vault is locked; calls that read or
//! write journal data take the second-lock view and the open invisible vault
//! from the Rust-held lock session ([`MemloreEngine::with_scope`]), and
//! listing uses the same locked/invisible-aware query as the desktop list
//! command. Raw `ops::*_impl`, `db::*`, the AI credential readers and the
//! `*_inner` vault helpers are only called from here, never exported. Error
//! messages carry no key material.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use memlore_core::envelope::{open_entry, EnvelopeError};
use memlore_core::key_state::ContentKeyList;
use memlore_core::outbox::MAX_YJS_DOC_BYTES;
use memlore_engine::ai::indexer::EntryIndexer;
use memlore_engine::db::{self, EntrySort, EntryTimeRange, LockFilter, LockedView};
use memlore_engine::ops::entries::{
    maybe_detect_language_after_save, maybe_mark_entry_embedding_dirty_after_save,
    save_entry_content_merged_impl,
};
use memlore_engine::sync::connect::persist_connect_settings;
use memlore_engine::sync::gdrive_provider::GDriveSession;
use memlore_engine::sync::pending_session;
use memlore_engine::sync::schedule::LastEditState;
use memlore_engine::utils::boot_file::{self, UnlockMethod};
use memlore_engine::utils::encryption::{unwrap_key, KdfParams};
use memlore_engine::vault::biometric::BiometricUnlockError;
use memlore_engine::vault::startup::StartupMode;
use memlore_engine::vault::{self, VaultPlatformHooks};
use memlore_engine::{AppState, EncryptionKeyState};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use crate::drive::PendingOAuth;
use crate::keystore::{DriveTokenStore, OsKeyStore};
use crate::sync::{MobileCredentialStore, MobileScheduler, SyncReport};

pub use drive::{ConnectOutcomeFfi, DriveConnectStartFfi, DriveStatusFfi};
pub use entries::{EntryDetailFfi, EntryPageFfi, EntryRowFfi, TrashRowFfi};
pub use locks::LockStateFfi;
pub use organize::{JournalFfi, TagFfi};
pub use search::{SearchFiltersFfi, SearchResultFfi};
pub use sync::{SyncBlockedFfi, SyncObserver, SyncPhaseFfi, SyncStatusFfi};
pub use templates::TemplateFfi;
pub use versions::VersionFfi;

mod drive;
mod entries;
mod keystore;
mod locks;
mod onboard;
mod organize;
mod search;
mod sync;
mod templates;
mod versions;

uniffi::setup_scaffolding!();

/// Process-wide runtime for the engine's async code. Never dropped: a JS
/// reload that creates a second engine reuses it.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The FFI runtime, for driving the engine's async flows from a blocking
/// FFI call (never from inside the runtime).
fn runtime() -> Result<&'static Runtime, MemloreError> {
    RUNTIME
        .get()
        .ok_or_else(|| io("The sync runtime is not running".to_string()))
}

/// Device name written at first-time setup (the desktop derives one from the
/// platform the same way).
const DEVICE_NAME: &str = if cfg!(target_os = "ios") {
    "iOS device"
} else if cfg!(target_os = "android") {
    "Android device"
} else {
    "This device"
};

/// Exactly what `vault::unlock::unlock_with_password` returns when the
/// password wrap does not authenticate.
const INVALID_PASSWORD: &str = "Invalid password";

/// Seconds in the background before the app locks, in the encrypted DB
/// settings. Device-local (each phone picks its own): it must NEVER be added
/// to `SYNCABLE_SETTING_KEYS` (memlore-engine `db/queries.rs`).
const AUTO_LOCK_SETTING: &str = "mobile_auto_lock_seconds";
/// Immediate, 1, 5 and 15 minutes.
const AUTO_LOCK_CHOICES: [u32; 4] = [0, 60, 300, 900];
/// The default, and what any missing or unexpected stored value reads as.
const AUTO_LOCK_DEFAULT: u32 = 60;

/// The synced setting desktop writes as `"true"` when locked items are shown
/// redacted (`LockedView::Covered`) instead of left out.
const SHOW_EXISTENCE_SETTING: &str = "second_lock_show_existence";

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MemloreError {
    #[error("the vault is locked")]
    Locked,
    #[error("wrong password")]
    WrongPassword,
    #[error("{msg}")]
    Io { msg: String },
    #[error("{msg}")]
    Format { msg: String },
    /// Face ID unlock cannot be used now (not enabled, or found broken and
    /// turned off); the user unlocks with the password.
    #[error("{msg}")]
    BiometricDisabled { msg: String },
    /// The user dismissed the Face ID prompt. Nothing changed.
    #[error("biometric unlock was cancelled")]
    BiometricCancelled,
    /// The entry, journal or tag does not exist or is not listed on mobile
    /// (deleted, locked or invisible). Never says which.
    #[error("not found")]
    NotFound,
    /// A tag rename to a name another tag holds, a deleted one included
    /// (no merge, as on desktop).
    #[error("a tag with that name already exists")]
    NameTaken,
    /// The journal is the last one mobile lists; it cannot be deleted.
    #[error("cannot delete the last journal")]
    LastJournal,
    /// The entry or journal is locked behind the second lock, shown redacted
    /// (Covered); the user reveals it with the second-lock password.
    #[error("the second lock is locked")]
    SecondLockRequired,
    /// The phone vault holds entries (trashed ones included), so it never
    /// joins another cloud vault: onboarding, and a Drive connect that finds
    /// a different vault, are refused and leave it as it was.
    #[error("this phone's vault already holds entries")]
    VaultNotEmpty,
    /// The recovery phrase does not open the cloud vault.
    #[error("wrong recovery phrase")]
    WrongPhrase,
}

fn io(msg: String) -> MemloreError {
    MemloreError::Io { msg }
}

fn format_err(msg: impl Into<String>) -> MemloreError {
    MemloreError::Format { msg: msg.into() }
}

fn biometric_unlock_err(e: BiometricUnlockError) -> MemloreError {
    match e {
        BiometricUnlockError::Disabled(msg) => MemloreError::BiometricDisabled { msg },
        BiometricUnlockError::Cancelled => MemloreError::BiometricCancelled,
        BiometricUnlockError::Failed(msg) => io(msg),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StartupModeFfi {
    FirstLaunch,
    PasswordLocked,
    BootFileCorrupt,
}

impl From<StartupMode> for StartupModeFfi {
    fn from(mode: StartupMode) -> Self {
        match mode {
            StartupMode::FirstLaunch => Self::FirstLaunch,
            StartupMode::PasswordLocked => Self::PasswordLocked,
            StartupMode::BootFileCorrupt => Self::BootFileCorrupt,
        }
    }
}

/// One opened `<device>/entries/<id>.bin` sync file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EntryFrameFfi {
    /// `serde_json` of the frozen `EntryMetadata` (memlore-core `metadata.rs`).
    pub metadata_json: String,
    /// Yjs full-state bytes, empty when the entry has no body yet.
    pub yjs: Vec<u8>,
}

/// Mobile side of the vault lifecycle: the os_kek lives in `keystore` (the
/// iOS Keychain), no post-unlock sweep; a pending cloud session persists
/// through `credentials` (the Drive token goes to the Keychain).
struct MobileVaultHooks {
    keystore: Arc<dyn OsKeyStore>,
    credentials: Arc<MobileCredentialStore>,
}

impl VaultPlatformHooks for MobileVaultHooks {
    fn wipe_biometric_keychain(&self) -> Result<(), String> {
        self.keystore.remove()
    }

    fn post_unlock(&self, _state: &AppState) {}

    fn is_os_kek_available(&self) -> bool {
        self.keystore.is_available()
    }

    fn generate_os_kek(&self) -> Result<Zeroizing<[u8; 32]>, String> {
        // Any fresh 32-byte `OsRng` key will do: reusing memlore-core's
        // content-key generator keeps `rand` out of this crate.
        let os_kek = memlore_core::encryption::generate_content_key();
        self.keystore.store(&os_kek)?;
        Ok(os_kek)
    }

    fn get_os_kek(&self) -> Result<Zeroizing<[u8; 32]>, vault::OsKekError> {
        self.keystore.retrieve()
    }

    fn remove_os_kek(&self) -> Result<(), String> {
        self.keystore.remove()
    }

    /// Same mirror as the desktop: the setting says whether the vault can
    /// unlock with the OS keystore.
    fn sync_biometric_setting_for_unlock_method(
        &self,
        conn: &rusqlite::Connection,
        method: UnlockMethod,
    ) -> Result<(), String> {
        let enabled = matches!(method, UnlockMethod::Both | UnlockMethod::OsKek);
        db::set_setting(
            conn,
            vault::BIOMETRIC_ENABLED_SETTING,
            if enabled { "true" } else { "false" },
        )
        .map_err(|e| e.to_string())
    }

    /// Same as the desktop hook, with the mobile credential store.
    fn consume_pending_cloud_session(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
    ) -> Option<Result<(), String>> {
        pending_session::consume(session_id)
            .map(|pending| persist_connect_settings(conn, &pending, self.credentials.as_ref()))
    }
}

/// The second-lock reveal and the open invisible vault. Memory only: every
/// vault lock and unlock starts it empty, so nothing survives a restart.
#[derive(Default)]
struct LockSession {
    revealed: bool,
    vault_id: Option<String>,
}

/// What one gated call may see, snapshotted by [`MemloreEngine::with_scope`].
struct Scope {
    /// A second-lock verifier exists.
    enabled: bool,
    view: LockedView,
    vault: Option<String>,
}

#[derive(uniffi::Object)]
pub struct MemloreEngine {
    db_path: PathBuf,
    state: AppState,
    key_state: EncryptionKeyState,
    startup_mode: Mutex<StartupMode>,
    /// The hooks every vault call of this engine gets.
    hooks: MobileVaultHooks,
    /// The one indexer of this engine, like the desktop's managed
    /// `EntryIndexer` (stub embedder until a provider is configured).
    // TODO(later): see docs/LATER.md (nothing drains the embedding-dirty rows
    // this queues once settings sync brings an embed provider).
    indexer: EntryIndexer,
    lock_session: Mutex<LockSession>,
    /// The Drive refresh token (Keychain) and the OAuth client id. Shared
    /// with `hooks`, which persists a pending cloud session through it.
    credentials: Arc<MobileCredentialStore>,
    /// The Drive session of the last connect (memory only), like the
    /// desktop's managed `GDriveSessionState`.
    live_session: Mutex<Option<GDriveSession>>,
    /// The one Google sign-in between `drive_connect_begin` and
    /// `drive_connect_complete` (`drive.rs`).
    oauth: Mutex<Option<PendingOAuth>>,
    /// The running background sync, which `lock()` ends before the keys go
    /// (`sync.rs`).
    sync_task: Mutex<Option<JoinHandle<()>>>,
    /// The scheduler's backoff cursor, on-launch rule and owed keyring
    /// reconcile.
    scheduler: Mutex<MobileScheduler>,
    /// The sync observer and the last sync's outcome.
    sync_report: SyncReport,
    /// When the editor last saved: the on-save debounce clock.
    last_edit: LastEditState,
}

#[uniffi::export]
impl MemloreEngine {
    /// Inject the app version and the runtime, then open the vault at
    /// `app_data_dir/memlore.db` the way the desktop `setup` does.
    #[uniffi::constructor]
    pub fn new(app_data_dir: String, app_version: String) -> Result<Arc<Self>, MemloreError> {
        Self::with_stores(
            app_data_dir,
            app_version,
            keystore::default_keystore(),
            keystore::default_token_store(),
        )
    }

    /// The Google OAuth client id of this app (the iOS client: no secret).
    /// Drive cannot connect or sync until it is set.
    pub fn configure_drive(&self, client_id: String) {
        self.credentials.set_client_id(client_id);
    }

    pub fn startup_mode(&self) -> StartupModeFfi {
        (*self.startup_mode.lock().unwrap_or_else(|p| p.into_inner())).into()
    }

    /// First-time setup in one step (the desktop splits it across two screens):
    /// create the vault with a password unlock, no cloud, and return the
    /// 24-word recovery phrase. The vault is unlocked afterwards, and
    /// `startup_mode` reports `PasswordLocked` (what a restart would find).
    pub fn create_vault(&self, password: String) -> Result<String, MemloreError> {
        self.end_lock_session();
        let password = Zeroizing::new(password);
        vault::validate_password_length(&password).map_err(format_err)?;
        let handle = {
            let conn = self.state.lock().map_err(io)?;
            vault::setup::begin_first_time_setup_inner(
                &conn,
                &password,
                DEVICE_NAME,
                KdfParams::FAST,
            )
            .map_err(io)?
        };
        let answers: Vec<String> = handle
            .challenge_indices
            .iter()
            .map(|&i| handle.mnemonic[i].clone())
            .collect();
        vault::setup::confirm_first_time_setup(
            &self.db_path,
            &self.state,
            &self.key_state,
            &handle.setup_id,
            &answers,
            &password,
            None,
            None,
            &self.hooks,
        )
        .map_err(io)?;
        *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner()) = StartupMode::PasswordLocked;
        Ok(handle.mnemonic.join(" "))
    }

    pub fn unlock(&self, password: String) -> Result<(), MemloreError> {
        self.end_lock_session();
        let password = Zeroizing::new(password);
        vault::unlock::unlock_with_password(
            &self.db_path,
            &self.state,
            &self.key_state,
            &password,
            &self.hooks,
        )
        .map_err(|e| {
            if e == INVALID_PASSWORD {
                MemloreError::WrongPassword
            } else {
                io(e)
            }
        })?;
        self.rearm_sync_on_launch();
        Ok(())
    }

    /// Clear the keys from memory, like the desktop `lock_encryption`, then
    /// close the database (decision 10): the keyed SQLCipher connection is
    /// replaced by the password-locked placeholder a cold start opens, so
    /// the Face ID flag still answers. Only for a created vault (boot file
    /// in password mode): before setup the live connection is the one
    /// `create_vault` rekeys, and it holds no user data.
    ///
    /// The lock session (reveal, invisible vault) ends first, before the
    /// keys and the connection swap, so neither a failed key clear nor a
    /// failed swap can keep it.
    ///
    /// Never fails open: the session and the keys go first, and if the
    /// placeholder cannot replace the connection it stays open behind the
    /// lock gate. That is logged, but no logger is installed on mobile yet.
    ///
    /// A running sync is ended before the keys are cleared (bounded wait,
    /// see `sync.rs`), and no new one can start until they are (the sync
    /// slot stays held), so no key copy outlives the lock. Nor does a Drive
    /// credential in memory: the live session and an unfinished sign-in go
    /// (after an unlock the session is rebuilt from the Keychain).
    // TODO(later): see docs/LATER.md ("Mobile M1 notes", Phase 2 review suggestions).
    pub fn lock(&self) -> Result<(), MemloreError> {
        if let Err(e) = self.lock_and_close()? {
            log::error!("lock: the connection was not replaced, data calls stay locked ({e})");
        }
        Ok(())
    }

    /// Whether Face ID can unlock this vault: the setting AND the boot file's
    /// os_kek wrap. Answers while locked too (the boot file is plaintext).
    pub fn is_biometric_unlock_enabled(&self) -> bool {
        self.state
            .lock()
            .map(|conn| vault::biometric::biometric_unlock_enabled(&self.db_path, &conn))
            .unwrap_or(false)
    }

    /// Turn Face ID unlock on: a new os_kek in the Keychain wraps the master
    /// into the boot file. Needs an unlocked vault.
    // Not `with_unlocked`: the engine takes the connection and the master
    // itself, and the key state must not be re-entered.
    pub fn enable_biometric(&self) -> Result<(), MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        vault::biometric::enable_biometric_unlock(
            &self.db_path,
            &self.state,
            &self.key_state,
            &self.hooks,
        )
        .map_err(io)
    }

    /// Turn Face ID unlock off: the boot file's wrap first, then the Keychain
    /// item, then the setting. Idempotent. Needs an unlocked vault: while
    /// locked the connection is the placeholder, so the setting would not
    /// reach the vault.
    pub fn disable_biometric(&self) -> Result<(), MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        vault::biometric::disable_biometric_unlock(&self.db_path, &self.state, &self.hooks)
            .map_err(io)
    }

    /// Unlock with the Face ID-protected os_kek instead of the password.
    /// Refused while unlocked: it would replace the live connection.
    pub fn unlock_with_biometric(&self) -> Result<(), MemloreError> {
        if self.key_state.is_initialized().map_err(io)? {
            return Err(io("The vault is already unlocked".to_string()));
        }
        self.end_lock_session();
        vault::biometric::unlock_with_os_kek(
            &self.db_path,
            &self.state,
            &self.key_state,
            &self.hooks,
        )
        .map_err(biometric_unlock_err)?;
        self.rearm_sync_on_launch();
        Ok(())
    }

    /// Delete the phone vault for good and return to first launch. Needs
    /// the vault password, checked against the boot file first: a wrong one
    /// changes nothing. Then the running sync ends, the vault locks, its
    /// files (DB, `-wal`, `-shm`, boot file, conversion leftovers) and the
    /// media folder go, and both Keychain items (os_kek, Drive token) are
    /// wiped. The memory state of the deleted vault goes too (the lock drops
    /// the Drive session and sign-in; the last sync's outcome and the
    /// scheduler's state are reset). The vault folder itself stays: Swift
    /// set its data protection class and backup exclusion, and new files
    /// inherit them. If the open database cannot be closed, it stops there
    /// with an error: locked, every file kept.
    pub fn delete_vault(&self, password: String) -> Result<(), MemloreError> {
        let password = Zeroizing::new(password);
        self.verify_password(&password)?;
        // A keyed connection still open would outlive the unlink of its
        // files and could later remove the new vault's `-wal` by path: stop
        // before any file goes.
        self.lock_and_close()?
            .map_err(|e| io(format!("The vault could not be closed for deletion: {e}")))?;
        let removed = remove_vault_files(&self.db_path);
        let os_kek = self.hooks.keystore.remove();
        let token = self.credentials.tokens().remove();
        self.reset_sync_memory();
        // Re-detect whatever is left, even after a failed removal, so the
        // engine never keeps answering for a vault that is half gone.
        let (conn, mode) = startup(
            &self.db_path,
            &self.key_state,
            &self.hooks,
            &self.credentials,
        )?;
        self.state.set_connection(conn).map_err(io)?;
        *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
        removed.and(os_kek).and(token).map_err(io)
    }

    /// The auto-lock interval: 0 (immediate), 60, 300 or 900 seconds. Fails
    /// closed: a missing, unparsable or unexpected stored value reads as the
    /// 1-minute default, never as "never lock".
    pub fn auto_lock_seconds(&self) -> Result<u32, MemloreError> {
        self.with_unlocked(|conn| {
            let stored = db::get_setting(conn, AUTO_LOCK_SETTING).map_err(|e| io(e.to_string()))?;
            Ok(stored
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|s| AUTO_LOCK_CHOICES.contains(s))
                .unwrap_or(AUTO_LOCK_DEFAULT))
        })
    }

    /// Store the auto-lock interval; anything but 0, 60, 300 or 900 seconds
    /// is a `Format` error.
    pub fn set_auto_lock_seconds(&self, seconds: u32) -> Result<(), MemloreError> {
        self.with_unlocked(|conn| {
            if !AUTO_LOCK_CHOICES.contains(&seconds) {
                return Err(format_err(format!(
                    "auto-lock must be 0, 60, 300 or 900 seconds, not {seconds}"
                )));
            }
            db::set_setting(conn, AUTO_LOCK_SETTING, &seconds.to_string())
                .map_err(|e| io(e.to_string()))
        })
    }

    /// Entries the list shows (same filters as [`Self::list_entries`]
    /// without a journal).
    pub fn entry_count(&self) -> Result<u64, MemloreError> {
        self.with_scope(|conn, scope| Ok(u64::from(list_page(conn, scope, 1)?.total)))
    }

    /// The entry's Yjs document (full state), empty when it has no body yet.
    /// Only entries the list shows readably open (see
    /// [`require_listed_entry`]): a locked entry needs the reveal, an
    /// invisible one its open vault.
    pub fn load_entry_yjs(&self, id: String) -> Result<Vec<u8>, MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            Ok(db::get_entry_content(conn, &id)
                .map_err(|e| io(e.to_string()))?
                .unwrap_or_default())
        })
    }

    /// The editor's save, exactly like the desktop `save_entry_content`
    /// command: `full_state` is the editor's whole document
    /// (`Y.encodeStateAsUpdate`, never an incremental update) and it is
    /// MERGED into the stored one, so an edit that landed meanwhile (MCP
    /// append, outbox import) survives. Then the same post-save hooks and
    /// the `last_edit` debounce clock of the on-save sync.
    pub fn save_entry_yjs(
        &self,
        id: String,
        full_state: Vec<u8>,
        content_text: String,
        preview_text: String,
    ) -> Result<(), MemloreError> {
        if full_state.len() > MAX_YJS_DOC_BYTES {
            return Err(format_err(
                "Yjs document exceeds maximum allowed size (10 MiB)",
            ));
        }
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            save_entry_content_merged_impl(conn, &id, &full_state, &content_text, &preview_text)
                .map_err(io)
        })?;
        self.after_entry_save(&id)?;
        // Stamped only after the write committed, like the desktop.
        self.last_edit.mark_now();
        Ok(())
    }
}

impl MemloreEngine {
    /// [`Self::with_stores`] with a fresh in-memory Drive token store.
    #[cfg(test)]
    fn with_keystore(
        app_data_dir: String,
        app_version: String,
        keystore: Arc<dyn OsKeyStore>,
    ) -> Result<Arc<Self>, MemloreError> {
        Self::with_stores(
            app_data_dir,
            app_version,
            keystore,
            Arc::new(keystore::MemoryTokenStore::default()),
        )
    }

    /// [`Self::new`] with the os_kek kept in `keystore` and the Drive token
    /// in `tokens` (tests pass fakes).
    fn with_stores(
        app_data_dir: String,
        app_version: String,
        keystore: Arc<dyn OsKeyStore>,
        tokens: Arc<dyn DriveTokenStore>,
    ) -> Result<Arc<Self>, MemloreError> {
        if memlore_engine::ops::app_info::try_app_version().is_none() {
            memlore_engine::ops::app_info::set_app_version(Box::leak(app_version.into_boxed_str()));
        }
        if RUNTIME.get().is_none() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| io(format!("Failed to build the tokio runtime: {e}")))?;
            // A concurrent `new` may have won; its runtime is kept, ours dropped.
            let _ = RUNTIME.set(rt);
        }
        if let Some(rt) = RUNTIME.get() {
            memlore_engine::ai::runtime::set_runtime(rt.handle().clone());
        }

        let data_dir = PathBuf::from(app_data_dir);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| io(format!("Failed to create the app data dir: {e}")))?;
        let db_path = data_dir.join("memlore.db");
        let key_state = EncryptionKeyState::new();
        let credentials = Arc::new(MobileCredentialStore::new(tokens));
        let hooks = MobileVaultHooks {
            keystore,
            credentials: credentials.clone(),
        };
        let (conn, mode) = startup(&db_path, &key_state, &hooks, &credentials)?;
        Ok(Arc::new(Self {
            db_path,
            state: AppState::new(conn),
            key_state,
            startup_mode: Mutex::new(mode),
            hooks,
            indexer: EntryIndexer::with_stub(),
            lock_session: Mutex::new(LockSession::default()),
            credentials,
            live_session: Mutex::new(None),
            oauth: Mutex::new(None),
            sync_task: Mutex::new(None),
            scheduler: Mutex::new(MobileScheduler::default()),
            sync_report: SyncReport::default(),
            last_edit: LastEditState::new(),
        }))
    }

    /// The on-launch sync rule fires again after an unlock (on mobile an
    /// unlock is the "launch"), and the next sync first runs the keyring
    /// reconcile the desktop runs at startup.
    fn rearm_sync_on_launch(&self) {
        let mut scheduler = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
        scheduler.on_launch_done = false;
        scheduler.reconcile_pending = true;
    }

    /// The body of [`Self::lock`]: the outer error is a failed key clear,
    /// the inner one a connection that was not replaced by the placeholder
    /// (a created vault only; the keys are gone either way).
    fn lock_and_close(&self) -> Result<Result<(), String>, MemloreError> {
        let sync_slot = self.cancel_sync_and_wait();
        self.end_lock_session();
        *self.live_session.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.oauth.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.key_state.clear_key().map_err(io)?;
        drop(sync_slot);
        Ok(
            boot_file::load(&boot_file::boot_path(&self.db_path)).and_then(|boot| {
                if boot.mode != db::EncryptionMode::Password {
                    return Ok(());
                }
                let placeholder = vault::startup::password_locked_placeholder(&boot)?;
                self.state.set_connection(placeholder)
            }),
        )
    }

    /// Check `password` against the boot file's password wrap, without
    /// unlocking or changing anything. `WrongPassword` like `unlock`.
    fn verify_password(&self, password: &str) -> Result<(), MemloreError> {
        let boot = boot_file::load(&boot_file::boot_path(&self.db_path)).map_err(io)?;
        let (Some(wrapped), Some(salt)) = (boot.wrapped_key.as_deref(), boot.kek_salt.as_deref())
        else {
            return Err(io("There is no vault password to check".to_string()));
        };
        let salt = vault::decode_hex_salt(salt).map_err(io)?;
        unwrap_key(wrapped, password, &salt)
            .map(drop)
            .map_err(|_| MemloreError::WrongPassword)
    }

    /// The lock gate: `Locked` unless the keys are loaded, then run `f` on
    /// the DB connection while holding the key state, like the desktop
    /// commands' `key_state.with_key`.
    fn with_unlocked<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, MemloreError>,
    ) -> Result<T, MemloreError> {
        self.key_state
            .with_key(|_key| Ok(self.state.lock().map_err(io).and_then(|conn| f(&conn))))
            .map_err(|_| MemloreError::Locked)?
    }

    /// [`Self::with_unlocked`] plus the [`Scope`] of the call: `Revealed`
    /// only while the session is revealed AND the second lock is enabled (a
    /// verifier exists; disabled with locked rows left fails closed), else
    /// `Covered` when show-existence is on (read per call), else `Hidden`.
    /// A disabled second lock also ends the session's reveal, so a verifier
    /// restored later (sync, or a new desktop password) finds it closed.
    fn with_scope<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection, &Scope) -> Result<T, MemloreError>,
    ) -> Result<T, MemloreError> {
        self.with_unlocked(|conn| {
            let enabled = db::get_second_lock_verifier(conn)
                .map_err(|e| io(e.to_string()))?
                .is_some();
            let show_existence = db::get_setting(conn, SHOW_EXISTENCE_SETTING)
                .map_err(|e| io(e.to_string()))?
                .is_some_and(|v| v == "true");
            // Taken last and released at once; a poisoned session reads as
            // empty (fail closed).
            let (revealed, vault) = match self.lock_session.lock() {
                Ok(mut s) => {
                    if !enabled {
                        s.revealed = false;
                    }
                    (s.revealed, s.vault_id.clone())
                }
                Err(_) => (false, None),
            };
            let view = if revealed && enabled {
                LockedView::Revealed
            } else if show_existence {
                LockedView::Covered
            } else {
                LockedView::Hidden
            };
            f(
                conn,
                &Scope {
                    enabled,
                    view,
                    vault,
                },
            )
        })
    }

    /// Forget the reveal and the open invisible vault.
    fn end_lock_session(&self) {
        *self.lock_session.lock().unwrap_or_else(|p| p.into_inner()) = LockSession::default();
    }

    /// Run `f` on the lock session. Take no other lock inside `f`.
    fn update_lock_session(&self, f: impl FnOnce(&mut LockSession)) {
        f(&mut self.lock_session.lock().unwrap_or_else(|p| p.into_inner()));
    }

    /// The desktop post-save hooks (language, embedding). Call it outside
    /// the lock gate, after the connection guard is released: the embedding
    /// hook takes the connection itself.
    fn after_entry_save(&self, id: &str) -> Result<(), MemloreError> {
        {
            let conn = self.state.lock().map_err(io)?;
            maybe_detect_language_after_save(&conn, id);
        }
        maybe_mark_entry_embedding_dirty_after_save(&self.state, &self.indexer, id);
        Ok(())
    }
}

/// `startup_init` plus the Drive token half of its first-launch Keychain
/// wipe: a token left by an earlier install (the Keychain outlives an
/// uninstall) is never paired with a new vault. A password-locked or
/// corrupt-boot launch keeps it.
fn startup(
    db_path: &Path,
    key_state: &EncryptionKeyState,
    hooks: &MobileVaultHooks,
    credentials: &MobileCredentialStore,
) -> Result<(rusqlite::Connection, StartupMode), MemloreError> {
    let (conn, mode) = vault::startup::startup_init(db_path, key_state, hooks).map_err(io)?;
    if mode == StartupMode::FirstLaunch {
        if let Err(e) = credentials.tokens().remove() {
            log::warn!("first launch: the stale Drive token was not wiped ({e})");
        }
    }
    Ok((conn, mode))
}

/// Remove the vault's files next to `db_path`, and its media folder.
/// Conversion leftovers go first (startup recovery would restore a
/// `.db.bak` over a missing DB), then the boot file: without it the next
/// launch is a first launch whatever else is left.
fn remove_vault_files(db_path: &Path) -> Result<(), String> {
    let boot = boot_file::boot_path(db_path);
    let sibling = |suffix: &str| {
        let mut path = db_path.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    let files = [
        db_path.with_extension("db.bak"),
        db_path.with_extension("db.enc"),
        db_path.with_extension("db.plain"),
        boot.with_extension("boot.bak"),
        boot,
        db_path.to_path_buf(),
        sibling("-wal"),
        sibling("-shm"),
    ];
    for path in &files {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("Failed to delete a vault file: {e}"));
            }
            _ => {}
        }
    }
    match std::fs::remove_dir_all(db_path.with_file_name("media")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("Failed to delete the media folder: {e}"))
        }
        _ => Ok(()),
    }
}

/// The per-entry gate of every entry call: a live entry visible for
/// `scope.vault`, else [`MemloreError::NotFound`] in every view. An
/// effectively locked one (its own flag or its journal's) passes only when
/// revealed; Covered gives [`MemloreError::SecondLockRequired`], Hidden
/// `NotFound`.
fn require_listed_entry(
    conn: &rusqlite::Connection,
    scope: &Scope,
    id: &str,
) -> Result<db::Entry, MemloreError> {
    let entry = db::get_entry(conn, id).map_err(|e| io(e.to_string()))?;
    let visible = db::is_entry_visible_for_active_vault(conn, id, scope.vault.as_deref())
        .map_err(|e| io(e.to_string()))?;
    match entry {
        Some(e) if visible && !e.is_deleted => require_unlocked(scope, e.is_locked).map(|()| e),
        _ => Err(MemloreError::NotFound),
    }
}

/// [`require_listed_entry`] that lets a trashed entry through, for the lock
/// and invisible flag setters: the engine then refuses it with its own text
/// ("entry is in Trash; restore it to edit", desktop parity).
fn require_visible_entry(
    conn: &rusqlite::Connection,
    scope: &Scope,
    id: &str,
) -> Result<(), MemloreError> {
    let entry = db::get_entry(conn, id).map_err(|e| io(e.to_string()))?;
    let visible = db::is_entry_visible_for_active_vault(conn, id, scope.vault.as_deref())
        .map_err(|e| io(e.to_string()))?;
    match entry {
        Some(e) if visible => require_unlocked(scope, e.is_locked),
        _ => Err(MemloreError::NotFound),
    }
}

/// The per-journal gate of every call that reads a journal or writes into
/// it, with the rules of [`require_listed_entry`]: moving an entry into a
/// hidden journal would hide the entry.
fn require_listed_journal(
    conn: &rusqlite::Connection,
    scope: &Scope,
    id: &str,
) -> Result<db::Journal, MemloreError> {
    let journal = db::get_journal(conn, id).map_err(|e| io(e.to_string()))?;
    let visible = db::is_journal_visible_for_active_vault(conn, id, scope.vault.as_deref())
        .map_err(|e| io(e.to_string()))?;
    match journal {
        Some(j) if visible && !j.is_deleted => require_unlocked(scope, j.is_locked).map(|()| j),
        _ => Err(MemloreError::NotFound),
    }
}

/// The second-lock half of the gates, for a target that exists and is
/// visible.
fn require_unlocked(scope: &Scope, is_locked: bool) -> Result<(), MemloreError> {
    match (is_locked, scope.view) {
        (false, _) | (true, LockedView::Revealed) => Ok(()),
        (true, LockedView::Covered) => Err(MemloreError::SecondLockRequired),
        (true, LockedView::Hidden) => Err(MemloreError::NotFound),
    }
}

/// One page of the desktop "all entries" list query, for the view and the
/// open vault of `scope` (Covered rows come back redacted by the engine).
fn list_page(
    conn: &rusqlite::Connection,
    scope: &Scope,
    page: u32,
) -> Result<db::PagedResult<db::Entry>, MemloreError> {
    db::list_all_entries_paged_with_locked_view(
        conn,
        EntrySort::Newest,
        EntryTimeRange::All,
        None,
        1,
        page,
        scope.view,
        scope.vault.as_deref(),
        LockFilter::All,
    )
    .map_err(|e| io(e.to_string()))
}

/// Open one entry sync file with the content key it was sealed under
/// (64 hex chars). The same `memlore_core` decoder the desktop sync uses:
/// frame + schema version check, key fingerprint, then both inner blobs.
#[uniffi::export]
pub fn open_entry_frame(
    content_key_hex: String,
    bytes: Vec<u8>,
) -> Result<EntryFrameFfi, MemloreError> {
    let content_key_hex = Zeroizing::new(content_key_hex);
    let mut key = Zeroizing::new([0u8; 32]);
    hex::decode_to_slice(content_key_hex.as_bytes(), key.as_mut())
        .map_err(|_| format_err("content key must be 64 hex chars"))?;
    let list = ContentKeyList {
        keys: [(1, key)].into_iter().collect(),
        latest: 1,
        db_key: Zeroizing::new([0u8; 32]),
        master: Zeroizing::new([0u8; 32]),
    };
    let opened = open_entry(&list, &bytes).map_err(envelope_err)?;
    let metadata_json = String::from_utf8(opened.metadata_json)
        .map_err(|_| format_err("entry metadata is not UTF-8"))?;
    Ok(EntryFrameFfi {
        metadata_json,
        yjs: opened.yjs,
    })
}

fn envelope_err(e: EnvelopeError) -> MemloreError {
    format_err(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::{DriveTokenStore, MemoryKeyStore, MemoryTokenStore};
    use yrs::updates::decoder::Decode;
    use yrs::{ReadTxn, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    /// A valid full state that differs from `base` (an editor typed into it).
    fn edited(base: &[u8]) -> Vec<u8> {
        typed(base, "must not land")
    }

    /// `base` plus a paragraph holding `text`, as an editor would save it.
    pub(crate) fn typed(base: &[u8], text: &str) -> Vec<u8> {
        let doc = yrs::Doc::with_client_id(7);
        let fragment = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v1(base).unwrap()).unwrap();
        let paragraph = fragment.push_back(&mut txn, XmlElementPrelim::new("paragraph", []));
        paragraph.push_back(&mut txn, XmlTextPrelim::new(text));
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// Save `text` into a listed entry through the mobile save.
    pub(crate) fn write_text(engine: &MemloreEngine, id: &str, text: &str) {
        let base = engine.load_entry_yjs(id.to_string()).unwrap();
        engine
            .save_entry_yjs(id.to_string(), typed(&base, text), text.into(), text.into())
            .unwrap();
    }

    /// A created (so unlocked) vault whose os_kek lives in memory.
    fn unlocked_engine() -> (tempfile::TempDir, Arc<MemloreEngine>) {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = MemloreEngine::with_keystore(
            dir.path().to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            Arc::new(MemoryKeyStore::default()),
        )
        .unwrap();
        engine.create_vault("password12".to_string()).unwrap();
        (dir, engine)
    }

    /// Generic over the success type, so the records need no `Debug`.
    fn expect_not_found<T>(result: Result<T, MemloreError>, what: &str) {
        match result {
            Err(MemloreError::NotFound) => {}
            Err(e) => panic!("{what}: expected NotFound, got {e:?}"),
            Ok(_) => panic!("{what}: expected NotFound, got Ok"),
        }
    }

    /// Every stored field a refused entry call could have changed.
    #[derive(Debug, PartialEq)]
    struct StoredEntry {
        title: Option<String>,
        emotion: Option<String>,
        is_favorite: bool,
        entry_date: i64,
        journal_id: String,
        is_deleted: bool,
        tag_ids: Vec<String>,
        yjs: Option<Vec<u8>>,
        versions: i64,
    }

    fn stored_entry(engine: &MemloreEngine, id: &str) -> StoredEntry {
        let conn = engine.state.lock().unwrap();
        let e = db::get_entry_raw(&conn, id).unwrap().unwrap();
        StoredEntry {
            title: e.title,
            emotion: e.emotion,
            is_favorite: e.is_favorite,
            entry_date: e.entry_date,
            journal_id: e.journal_id,
            is_deleted: e.is_deleted,
            tag_ids: db::get_tag_ids_for_entry(&conn, id).unwrap(),
            yjs: db::get_entry_content(&conn, id).unwrap(),
            versions: conn
                .query_row(
                    "SELECT COUNT(*) FROM entry_versions WHERE entry_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap(),
        }
    }

    /// What the calls of `entry_calls_refuse_unlisted_entries` are made
    /// with. The journal and the tags are listed, so only the entry's own
    /// gate can refuse a call.
    struct Unlisted {
        entry: String,
        elsewhere: String,
        kept_tag: String,
        other_tag: String,
        base: Vec<u8>,
        version: String,
    }

    /// `require_listed_entry`: a locked entry, an entry of an invisible
    /// journal and a deleted entry are "not found" for every entry call, and
    /// no refused call changes the stored row, its tags, its document or its
    /// versions. The entry has text, so a snapshot that skipped the gate
    /// would write a version.
    #[test]
    fn entry_calls_refuse_unlisted_entries() {
        type Hide = fn(&rusqlite::Connection, &str);
        let cases: [(&str, Hide); 4] = [
            ("locked entry", |conn, id| {
                db::set_entry_locked(conn, id, true).unwrap()
            }),
            ("entry in a locked journal", |conn, id| {
                let journal_id = db::get_entry(conn, id).unwrap().unwrap().journal_id;
                db::set_journal_locked(conn, &journal_id, true).unwrap();
            }),
            ("invisible journal", |conn, id| {
                let journal_id = db::get_entry(conn, id).unwrap().unwrap().journal_id;
                let vault = db::insert_invisible_vault(conn, "verifier").unwrap();
                db::set_journal_invisible(conn, &journal_id, true, Some(&vault.id)).unwrap();
            }),
            ("deleted entry", |conn, id| {
                db::trash_entry(conn, id, 1).unwrap()
            }),
        ];
        type Call = fn(&MemloreEngine, &Unlisted) -> Result<(), MemloreError>;
        let calls: [(&str, Call); 14] = [
            ("get_entry", |e, u| e.get_entry(u.entry.clone()).map(drop)),
            ("set_entry_title", |e, u| {
                e.set_entry_title(u.entry.clone(), "Changed".into())
            }),
            ("set_entry_emotion", |e, u| {
                e.set_entry_emotion(u.entry.clone(), Some("good".into()))
            }),
            ("toggle_entry_favorite", |e, u| {
                e.toggle_entry_favorite(u.entry.clone()).map(drop)
            }),
            ("set_entry_date", |e, u| {
                e.set_entry_date(u.entry.clone(), 86_400)
            }),
            ("move_entry", |e, u| {
                e.move_entry(u.entry.clone(), u.elsewhere.clone())
            }),
            ("add_tag_to_entry", |e, u| {
                e.add_tag_to_entry(u.entry.clone(), u.other_tag.clone())
            }),
            ("remove_tag_from_entry", |e, u| {
                e.remove_tag_from_entry(u.entry.clone(), u.kept_tag.clone())
            }),
            ("load_entry_yjs", |e, u| {
                e.load_entry_yjs(u.entry.clone()).map(drop)
            }),
            ("save_entry_yjs", |e, u| {
                e.save_entry_yjs(u.entry.clone(), edited(&u.base), "x".into(), "x".into())
            }),
            ("trash_entry", |e, u| e.trash_entry(u.entry.clone())),
            ("list_entry_versions", |e, u| {
                e.list_entry_versions(u.entry.clone()).map(drop)
            }),
            ("get_entry_version_content", |e, u| {
                e.get_entry_version_content(u.entry.clone(), u.version.clone())
                    .map(drop)
            }),
            ("snapshot_entry_version", |e, u| {
                e.snapshot_entry_version(u.entry.clone()).map(drop)
            }),
        ];
        for (case, hide) in cases {
            let (_dir, engine) = unlocked_engine();
            let entry = engine.create_entry("Hidden".to_string(), None).unwrap();
            write_text(&engine, &entry, "secret words");
            let version = engine
                .snapshot_entry_version(entry.clone())
                .unwrap()
                .unwrap();
            let base = engine.load_entry_yjs(entry.clone()).unwrap();
            let (elsewhere, kept_tag, other_tag) = {
                let conn = engine.state.lock().unwrap();
                let elsewhere = db::create_journal(&conn, "Elsewhere", None).unwrap().id;
                let kept_tag = db::create_tag(&conn, "kept", None).unwrap().id;
                let other_tag = db::create_tag(&conn, "other", None).unwrap().id;
                db::add_tag_to_entry(&conn, &entry, &kept_tag).unwrap();
                hide(&conn, &entry);
                (elsewhere, kept_tag, other_tag)
            };
            let unlisted = Unlisted {
                entry,
                elsewhere,
                kept_tag,
                other_tag,
                base,
                version,
            };
            let before = stored_entry(&engine, &unlisted.entry);

            for (call, run) in calls {
                expect_not_found(run(&engine, &unlisted), &format!("{case}: {call}"));
            }
            assert_eq!(
                stored_entry(&engine, &unlisted.entry),
                before,
                "{case}: stored row changed"
            );
        }
    }

    /// `require_listed_journal`: a locked or invisible journal is "not found"
    /// as a target. Moving an entry into it would hide the entry, creating
    /// one would write an entry nobody can list, listing, counting or
    /// renaming it would read or touch a hidden journal, and deleting it
    /// would delete what mobile cannot see. A spare listed journal keeps the
    /// last-journal rule out of the way, so only the gate can refuse.
    #[test]
    fn hidden_journals_are_refused_as_targets() {
        type Hide = fn(&rusqlite::Connection, &str);
        let cases: [(&str, Hide); 2] = [
            ("locked journal", |conn, journal_id| {
                db::set_journal_locked(conn, journal_id, true).unwrap()
            }),
            ("invisible journal", |conn, journal_id| {
                let vault = db::insert_invisible_vault(conn, "verifier").unwrap();
                db::set_journal_invisible(conn, journal_id, true, Some(&vault.id)).unwrap();
            }),
        ];
        for (case, hide) in cases {
            let (_dir, engine) = unlocked_engine();
            let id = engine.create_entry("Listed".to_string(), None).unwrap();
            let (home, hidden, hidden_entry) = {
                let conn = engine.state.lock().unwrap();
                let home = db::get_entry(&conn, &id).unwrap().unwrap().journal_id;
                db::create_journal(&conn, "Spare", None).unwrap();
                let hidden = db::create_journal(&conn, "Hidden", None).unwrap().id;
                let hidden_entry = memlore_engine::ops::entries::create_entry_impl(
                    &conn, &hidden, None, None, None, 1,
                )
                .unwrap()
                .id;
                hide(&conn, &hidden);
                (home, hidden, hidden_entry)
            };
            // Every row, hidden ones included: `list_entries` cannot see an
            // entry written into a hidden journal.
            let all_entries = || {
                let conn = engine.state.lock().unwrap();
                conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
                    .unwrap()
            };
            let count = all_entries();

            expect_not_found(
                engine.move_entry(id.clone(), hidden.clone()),
                &format!("{case}: move_entry"),
            );
            let listed = engine.list_entries(None, 0).unwrap();
            assert_eq!(
                listed
                    .items
                    .iter()
                    .find(|row| row.id == id)
                    .map(|row| row.journal_id.clone()),
                Some(home),
                "{case}: the entry left its journal or the list"
            );

            expect_not_found(
                engine.create_entry("x".to_string(), Some(hidden.clone())),
                &format!("{case}: create_entry"),
            );
            assert_eq!(all_entries(), count, "{case}: create_entry wrote a row");

            expect_not_found(
                engine.list_entries(Some(hidden.clone()), 0),
                &format!("{case}: list_entries"),
            );

            expect_not_found(
                engine.rename_journal(hidden.clone(), "y".to_string()),
                &format!("{case}: rename_journal"),
            );
            let name = db::get_journal(&engine.state.lock().unwrap(), &hidden)
                .unwrap()
                .unwrap()
                .name;
            assert_eq!(name, "Hidden", "{case}: rename_journal renamed it");

            expect_not_found(
                engine.journal_entry_count(hidden.clone()),
                &format!("{case}: journal_entry_count"),
            );
            expect_not_found(
                engine.delete_journal(hidden.clone()),
                &format!("{case}: delete_journal"),
            );
            let conn = engine.state.lock().unwrap();
            assert!(
                !db::get_journal(&conn, &hidden).unwrap().unwrap().is_deleted,
                "{case}: delete_journal deleted the journal"
            );
            assert!(
                !db::get_entry_raw(&conn, &hidden_entry)
                    .unwrap()
                    .unwrap()
                    .is_deleted,
                "{case}: delete_journal deleted its entry"
            );
        }
    }

    /// The last LISTED journal cannot be deleted, even though a locked one
    /// exists (the engine's own check counts it). Otherwise the delete is
    /// desktop parity: the confirmation count and the cascade include the
    /// journal's locked entries.
    #[test]
    fn delete_journal_keeps_the_last_listed_journal() {
        let (_dir, engine) = unlocked_engine();
        let journals = engine.list_journals().unwrap();
        assert_eq!(journals.len(), 1, "a new vault has one journal");
        let home = journals[0].id.clone();
        let locked_journal = {
            let conn = engine.state.lock().unwrap();
            let id = db::create_journal(&conn, "Locked", None).unwrap().id;
            db::set_journal_locked(&conn, &id, true).unwrap();
            id
        };

        assert!(
            matches!(
                engine.delete_journal(home.clone()),
                Err(MemloreError::LastJournal)
            ),
            "the last listed journal was not refused"
        );
        {
            let conn = engine.state.lock().unwrap();
            assert!(!db::get_journal(&conn, &home).unwrap().unwrap().is_deleted);
            assert!(
                !db::get_journal(&conn, &locked_journal)
                    .unwrap()
                    .unwrap()
                    .is_deleted
            );
        }

        let second = engine.create_journal("Second".to_string()).unwrap().id;
        let listed = engine
            .create_entry("Listed".to_string(), Some(second.clone()))
            .unwrap();
        let locked = engine
            .create_entry("Locked".to_string(), Some(second.clone()))
            .unwrap();
        db::set_entry_locked(&engine.state.lock().unwrap(), &locked, true).unwrap();

        assert_eq!(engine.journal_entry_count(second.clone()).unwrap(), 2);
        engine.delete_journal(second.clone()).unwrap();
        let conn = engine.state.lock().unwrap();
        assert!(db::get_journal(&conn, &second).unwrap().unwrap().is_deleted);
        for id in [&listed, &locked] {
            assert!(
                db::get_entry_raw(&conn, id).unwrap().unwrap().is_deleted,
                "an entry of the deleted journal survived"
            );
        }
    }

    /// Mobile has no reveal session (until M3c): the listings and search
    /// leave out a locked journal, a tag used only on a locked entry, a
    /// locked, invisible or trashed entry and an entry of a locked journal;
    /// the trash and tag calls cannot touch what they do not list. What stays
    /// visible is listed, so an empty answer cannot pass.
    #[test]
    fn listings_hide_locked_journals_and_locked_only_tags() {
        let (_dir, engine) = unlocked_engine();
        let listed = engine.create_entry("Listed".to_string(), None).unwrap();
        let locked = engine.create_entry("Locked".to_string(), None).unwrap();
        let trashed = engine.create_entry("Trashed".to_string(), None).unwrap();
        let trashed_locked = engine
            .create_entry("Trashed locked".to_string(), None)
            .unwrap();
        let locked_journal = engine
            .create_journal("Locked journal".to_string())
            .unwrap()
            .id;
        let in_locked_journal = engine
            .create_entry(
                "In locked journal".to_string(),
                Some(locked_journal.clone()),
            )
            .unwrap();
        let invisible = engine.create_entry("Invisible".to_string(), None).unwrap();
        // Written while every entry is still listed.
        for id in [&listed, &locked, &trashed, &in_locked_journal, &invisible] {
            write_text(&engine, id, "Một thử thách được vượt qua");
        }
        let (home, unused, on_listed, locked_only) = {
            let conn = engine.state.lock().unwrap();
            let home = db::get_entry(&conn, &listed).unwrap().unwrap().journal_id;
            db::set_journal_locked(&conn, &locked_journal, true).unwrap();
            let vault = db::insert_invisible_vault(&conn, "verifier").unwrap();
            db::set_entry_invisible(&conn, &invisible, true, Some(&vault.id)).unwrap();
            let tag = |name: &str| db::create_tag(&conn, name, None).unwrap().id;
            let (unused, on_listed, locked_only) =
                (tag("unused"), tag("on-listed"), tag("locked-only"));
            db::add_tag_to_entry(&conn, &listed, &on_listed).unwrap();
            db::add_tag_to_entry(&conn, &locked, &locked_only).unwrap();
            db::set_entry_locked(&conn, &locked, true).unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            db::trash_entry(&conn, &trashed, now).unwrap();
            db::trash_entry(&conn, &trashed_locked, now).unwrap();
            db::set_entry_locked(&conn, &trashed_locked, true).unwrap();
            (home, unused, on_listed, locked_only)
        };

        let journals: Vec<String> = engine
            .list_journals()
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert!(journals.contains(&home), "{journals:?}");
        assert!(!journals.contains(&locked_journal), "locked journal listed");

        let tags: Vec<String> = engine
            .list_tags()
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(tags.contains(&unused), "unused tag missing");
        assert!(tags.contains(&on_listed), "tag of a listed entry missing");
        assert!(!tags.contains(&locked_only), "locked-only tag listed");

        let trash: Vec<String> = engine
            .list_trash()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert!(trash.contains(&trashed), "trashed entry missing");
        assert!(
            !trash.contains(&trashed_locked),
            "trashed locked entry listed"
        );
        expect_not_found(engine.purge_entry(trashed_locked.clone()), "purge_entry");
        assert_eq!(
            engine.empty_trash(vec![trashed_locked.clone()]).unwrap(),
            0,
            "empty_trash purged a locked entry"
        );
        // A purge keeps the row as a tombstone but clears `trashed_at`.
        let row = db::get_entry_raw(&engine.state.lock().unwrap(), &trashed_locked)
            .unwrap()
            .unwrap();
        assert!(
            row.is_deleted && row.trashed_at.is_some(),
            "the locked entry left Trash"
        );

        // Tags: a locked-only tag cannot be renamed or deleted, and a name
        // held by a deleted tag is taken (no merge).
        expect_not_found(
            engine.rename_tag(locked_only.clone(), "renamed".to_string()),
            "rename_tag",
        );
        expect_not_found(engine.delete_tag(locked_only.clone()), "delete_tag");
        let tag = db::get_tag_for_import(&engine.state.lock().unwrap(), &locked_only)
            .unwrap()
            .unwrap();
        assert_eq!(tag.name, "locked-only", "a refused call renamed the tag");
        assert!(!tag.is_deleted, "a refused call deleted the tag");
        let gone = engine.create_tag("gone".to_string()).unwrap().id;
        engine.delete_tag(gone).unwrap();
        assert!(
            matches!(
                engine.rename_tag(unused.clone(), "gone".to_string()),
                Err(MemloreError::NameTaken)
            ),
            "renaming to a deleted tag's name was not NameTaken"
        );

        // Search: an unaccented token finds the listed entry only.
        let found: Vec<String> = engine
            .search_entries("thu thach".to_string(), SearchFiltersFfi::default())
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(found, vec![listed.clone()], "search leaked or missed");
        assert!(
            engine
                .search_entries(" ".to_string(), SearchFiltersFfi::default())
                .unwrap()
                .is_empty(),
            "a blank query without filters listed entries"
        );
        let browsed: Vec<String> = engine
            .search_entries(
                String::new(),
                SearchFiltersFfi {
                    journal_ids: vec![home.clone()],
                    ..SearchFiltersFfi::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert!(browsed.contains(&listed), "blank query with a filter");
        assert!(!browsed.contains(&locked), "browse listed a locked entry");
        assert!(
            matches!(
                engine.search_entries(
                    "x".to_string(),
                    SearchFiltersFfi {
                        emotions: vec!["happy".to_string()],
                        ..SearchFiltersFfi::default()
                    },
                ),
                Err(MemloreError::Format { .. })
            ),
            "an unknown emotion was accepted"
        );

        // Templates: the seeds run on the FFI vault path and come first.
        let names: Vec<String> = engine
            .list_templates()
            .unwrap()
            .into_iter()
            .take(3)
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["blank", "daily-reflection", "morning-pages"]);
    }

    /// The vault password of [`unlocked_engine`].
    const VAULT_PASSWORD: &str = "password12";
    const SECOND_LOCK_PASSWORD: &str = "second-lock-pw";
    const WRONG_SECOND_LOCK_PASSWORD: &str = "wrong-lock-pw";
    const VAULT_A_PASSWORD: &str = "vault-a-pw";
    const VAULT_B_PASSWORD: &str = "vault-b-pw";

    /// Ids of every row `list_entries` shows (first page, all journals).
    fn listed_ids(engine: &MemloreEngine) -> Vec<String> {
        engine
            .list_entries(None, 0)
            .unwrap()
            .items
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    /// Ids of the search hits for `query`, without filters.
    fn search_ids(engine: &MemloreEngine, query: &str) -> Vec<String> {
        engine
            .search_entries(query.to_string(), SearchFiltersFfi::default())
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect()
    }

    /// A gated call refused for a locked target: `NotFound` under Hidden
    /// (its existence is not shown), `SecondLockRequired` under Covered.
    fn expect_refused<T>(result: Result<T, MemloreError>, view: db::LockedView, what: &str) {
        match (view, result) {
            (db::LockedView::Hidden, Err(MemloreError::NotFound))
            | (db::LockedView::Covered, Err(MemloreError::SecondLockRequired)) => {}
            (_, Err(e)) => panic!("{what} ({view:?}): refused with {e:?}"),
            (_, Ok(_)) => panic!("{what} ({view:?}): not refused"),
        }
    }

    /// What `second_lock_gate_fails_closed` reads: an ordinary entry, journal
    /// and tag, a locked entry with text, a version and a tag only it
    /// carries, a locked journal holding an entry, and a locked entry in
    /// Trash. The second lock is enabled and the session is not revealed.
    struct LockedFixture {
        home: String,
        ordinary: String,
        on_ordinary: String,
        locked: String,
        version: String,
        locked_only: String,
        locked_journal: String,
        in_locked_journal: String,
        trashed_locked: String,
    }

    fn locked_fixture(engine: &MemloreEngine) -> LockedFixture {
        // Written through the gate while every row is still listed.
        let ordinary = engine.create_entry("Ordinary".to_string(), None).unwrap();
        write_text(engine, &ordinary, "ordinary walk");
        let locked = engine.create_entry("Locked".to_string(), None).unwrap();
        write_text(engine, &locked, "secret thicket");
        let version = engine
            .snapshot_entry_version(locked.clone())
            .unwrap()
            .unwrap();
        let locked_journal = engine
            .create_journal("Locked journal".to_string())
            .unwrap()
            .id;
        let in_locked_journal = engine
            .create_entry(
                "In locked journal".to_string(),
                Some(locked_journal.clone()),
            )
            .unwrap();
        write_text(engine, &in_locked_journal, "secret meadow");
        let trashed_locked = engine
            .create_entry("Trashed secret".to_string(), None)
            .unwrap();

        // The verifier is written directly: the FFI setter would reveal.
        let conn = engine.state.lock().unwrap();
        memlore_engine::ops::second_lock::set_second_lock_password_impl(
            &conn,
            SECOND_LOCK_PASSWORD,
        )
        .unwrap();
        let home = db::get_entry(&conn, &ordinary).unwrap().unwrap().journal_id;
        let on_ordinary = db::create_tag(&conn, "on-ordinary", None).unwrap().id;
        let locked_only = db::create_tag(&conn, "locked-only", None).unwrap().id;
        db::add_tag_to_entry(&conn, &ordinary, &on_ordinary).unwrap();
        db::add_tag_to_entry(&conn, &locked, &locked_only).unwrap();
        db::set_entry_locked(&conn, &locked, true).unwrap();
        db::set_journal_locked(&conn, &locked_journal, true).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db::trash_entry(&conn, &trashed_locked, now).unwrap();
        db::set_entry_locked(&conn, &trashed_locked, true).unwrap();
        LockedFixture {
            home,
            ordinary,
            on_ordinary,
            locked,
            version,
            locked_only,
            locked_journal,
            in_locked_journal,
            trashed_locked,
        }
    }

    /// The second-lock gate across the three views. Hidden (default): a
    /// locked entry, a locked journal and a locked-only tag are not listed,
    /// counted or searchable, and every entry call is `NotFound`. Covered
    /// (`second_lock_show_existence` = "true", as desktop writes it): locked
    /// rows are listed redacted with `is_locked` and counted, entry calls are
    /// `SecondLockRequired`, search and the locked-only tag still leave them
    /// out, the locked journal is listed by name, Trash redacts. Revealed:
    /// content, versions, search and the tag, and the entry can be unlocked.
    /// Hidden and Covered, with an invisible vault open, refuse unlocking a
    /// locked entry or journal and moving it into the vault (which would
    /// clear its lock flag). In every view the last unlocked journal cannot
    /// be deleted, and no refused call changes the entry. Show-existence can
    /// only be turned on revealed (off always), and the re-lock minutes only
    /// shortened unless revealed.
    /// Then: a wrong password reveals nothing, and a second lock disabled
    /// while locked rows remain fails closed and ends the reveal, so a
    /// verifier restored later does not reopen locked content.
    #[test]
    fn second_lock_gate_fails_closed() {
        use memlore_engine::db::LockedView::{Covered, Hidden, Revealed};
        for view in [Hidden, Covered, Revealed] {
            let (_dir, engine) = unlocked_engine();
            let f = locked_fixture(&engine);
            match view {
                Hidden => {}
                Covered => db::set_setting(
                    &engine.state.lock().unwrap(),
                    "second_lock_show_existence",
                    "true",
                )
                .unwrap(),
                Revealed => assert!(
                    engine
                        .unlock_second_lock(SECOND_LOCK_PASSWORD.to_string())
                        .unwrap(),
                    "the right password did not reveal"
                ),
            }

            let listed = engine.list_entries(None, 0).unwrap();
            let row = |id: &str| listed.items.iter().find(|r| r.id == id).cloned();
            let total = engine.entry_count().unwrap();
            let ordinary = row(&f.ordinary).expect("the ordinary entry is not listed");
            assert!(
                !ordinary.is_locked && !ordinary.is_invisible,
                "{view:?}: the ordinary row is flagged"
            );
            let journals = engine.list_journals().unwrap();
            let journal = |id: &str| journals.iter().find(|j| j.id == id).cloned();
            assert!(
                journal(&f.home).is_some_and(|j| !j.is_locked && !j.is_invisible),
                "{view:?}: the home journal"
            );
            let tags: Vec<String> = engine
                .list_tags()
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect();
            assert!(tags.contains(&f.on_ordinary), "{view:?}: ordinary tag");
            let trash = engine.list_trash().unwrap();
            let trashed = trash.iter().find(|r| r.id == f.trashed_locked).cloned();
            assert_eq!(
                search_ids(&engine, "ordinary"),
                vec![f.ordinary.clone()],
                "{view:?}: search missed the ordinary entry"
            );
            let hits = search_ids(&engine, "thicket");
            // The guard counts unlocked journals in every view: a user is
            // never left with only journals that need a reveal.
            assert!(
                matches!(
                    engine.delete_journal(f.home.clone()),
                    Err(MemloreError::LastJournal)
                ),
                "{view:?}: the last unlocked journal was not refused"
            );

            match view {
                Hidden => {
                    assert_eq!(total, 1, "Hidden: the total counts locked entries");
                    assert_eq!(listed.total, 1, "Hidden: the list total");
                    assert!(row(&f.locked).is_none(), "Hidden: locked entry listed");
                    assert!(
                        row(&f.in_locked_journal).is_none(),
                        "Hidden: entry of a locked journal listed"
                    );
                    assert!(
                        journal(&f.locked_journal).is_none(),
                        "Hidden: locked journal listed"
                    );
                    assert!(trashed.is_none(), "Hidden: locked Trash row listed");
                    assert!(
                        matches!(
                            engine.set_second_lock_show_existence(true),
                            Err(MemloreError::SecondLockRequired)
                        ),
                        "Hidden: show-existence turned on without the reveal"
                    );
                    assert!(
                        !engine.lock_state().unwrap().show_existence,
                        "Hidden: a refused show-existence changed the setting"
                    );
                }
                Covered => {
                    assert_eq!(total, 3, "Covered: the total skips locked entries");
                    assert_eq!(listed.total, 3, "Covered: the list total");
                    for id in [&f.locked, &f.in_locked_journal] {
                        let r = row(id).expect("Covered: a locked entry is not listed");
                        assert!(r.is_locked, "Covered: row without is_locked");
                        assert_eq!(
                            (r.title, r.preview_text),
                            (None, None),
                            "Covered: a locked row is not redacted"
                        );
                    }
                    let j = journal(&f.locked_journal).expect("Covered: locked journal missing");
                    assert_eq!(j.name, "Locked journal");
                    assert!(j.is_locked, "Covered: journal without is_locked");
                    let t = trashed.expect("Covered: locked Trash row missing");
                    assert!(t.is_locked, "Covered: Trash row without is_locked");
                    assert_eq!(
                        (t.title, t.preview_text),
                        (None, None),
                        "Covered: the Trash row is not redacted"
                    );
                }
                Revealed => {
                    assert_eq!(total, 3, "Revealed: the total");
                    let r = row(&f.locked).expect("Revealed: locked entry missing");
                    assert!(r.is_locked, "Revealed: row without is_locked");
                    assert_eq!(r.title.as_deref(), Some("Locked"));
                    assert!(
                        row(&f.in_locked_journal).is_some(),
                        "Revealed: entry of a locked journal missing"
                    );
                    assert!(
                        journal(&f.locked_journal).is_some_and(|j| j.is_locked),
                        "Revealed: locked journal"
                    );
                    assert_eq!(
                        trashed.and_then(|t| t.title).as_deref(),
                        Some("Trashed secret"),
                        "Revealed: Trash row"
                    );
                }
            }

            if view == Revealed {
                let detail = engine.get_entry(f.locked.clone()).unwrap();
                assert!(
                    detail
                        .content_text
                        .as_deref()
                        .is_some_and(|t| t.contains("thicket")),
                    "Revealed: get_entry without content"
                );
                let loaded = engine.load_entry_yjs(f.locked.clone()).unwrap();
                let stored = db::get_entry_content(&engine.state.lock().unwrap(), &f.locked)
                    .unwrap()
                    .unwrap();
                assert_eq!(loaded, stored, "Revealed: load_entry_yjs");
                let versions: Vec<String> = engine
                    .list_entry_versions(f.locked.clone())
                    .unwrap()
                    .into_iter()
                    .map(|v| v.id)
                    .collect();
                assert_eq!(versions, vec![f.version.clone()], "Revealed: versions");
                engine
                    .get_entry_version_content(f.locked.clone(), f.version.clone())
                    .unwrap();
                assert_eq!(hits, vec![f.locked.clone()], "Revealed: search");
                assert!(tags.contains(&f.locked_only), "Revealed: tag missing");
                engine.set_entry_locked(f.locked.clone(), false).unwrap();
                assert!(
                    !db::get_entry_raw(&engine.state.lock().unwrap(), &f.locked)
                        .unwrap()
                        .unwrap()
                        .is_locked,
                    "Revealed: set_entry_locked(false) kept the flag"
                );
                engine.set_second_lock_show_existence(true).unwrap();
                assert!(engine.lock_state().unwrap().show_existence);
                engine.set_second_lock_auto_lock_minutes(0).unwrap();
                assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 0);
                // Unrevealed, any interval is shorter than never.
                engine.lock_second_lock();
                engine.set_second_lock_auto_lock_minutes(30).unwrap();
                assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 30);
            } else {
                let base = db::get_entry_content(&engine.state.lock().unwrap(), &f.locked)
                    .unwrap()
                    .unwrap();
                let before = stored_entry(&engine, &f.locked);
                // With a vault open, making a locked entry or journal
                // invisible would clear its lock flag (engine semantics):
                // only the gate stops an unrevealed caller.
                engine
                    .open_invisible_vault(VAULT_A_PASSWORD.to_string())
                    .unwrap();
                expect_refused(
                    engine.set_entry_locked(f.locked.clone(), false),
                    view,
                    "set_entry_locked(false)",
                );
                expect_refused(
                    engine.set_entry_invisible(f.locked.clone(), true),
                    view,
                    "set_entry_invisible(true)",
                );
                expect_refused(
                    engine.set_journal_locked(f.locked_journal.clone(), false),
                    view,
                    "set_journal_locked(false)",
                );
                expect_refused(
                    engine.set_journal_invisible(f.locked_journal.clone(), true),
                    view,
                    "set_journal_invisible(true)",
                );
                {
                    let conn = engine.state.lock().unwrap();
                    let e = db::get_entry_raw(&conn, &f.locked).unwrap().unwrap();
                    assert!(
                        e.is_locked && !e.is_invisible,
                        "{view:?}: a refused setter changed the entry's flags"
                    );
                    let j = db::get_journal(&conn, &f.locked_journal).unwrap().unwrap();
                    assert!(
                        j.is_locked && !j.is_invisible,
                        "{view:?}: a refused setter changed the journal's flags"
                    );
                }
                expect_refused(engine.get_entry(f.locked.clone()), view, "get_entry");
                expect_refused(
                    engine.get_entry(f.in_locked_journal.clone()),
                    view,
                    "get_entry of a locked journal's entry",
                );
                expect_refused(
                    engine.load_entry_yjs(f.locked.clone()),
                    view,
                    "load_entry_yjs",
                );
                expect_refused(
                    engine.save_entry_yjs(f.locked.clone(), edited(&base), "x".into(), "x".into()),
                    view,
                    "save_entry_yjs",
                );
                expect_refused(
                    engine.list_entry_versions(f.locked.clone()),
                    view,
                    "list_entry_versions",
                );
                expect_refused(
                    engine.get_entry_version_content(f.locked.clone(), f.version.clone()),
                    view,
                    "get_entry_version_content",
                );
                expect_refused(
                    engine.snapshot_entry_version(f.locked.clone()),
                    view,
                    "snapshot_entry_version",
                );
                assert_eq!(
                    stored_entry(&engine, &f.locked),
                    before,
                    "{view:?}: a refused call changed the entry"
                );
                assert!(hits.is_empty(), "{view:?}: search found a locked entry");
                assert!(
                    !tags.contains(&f.locked_only),
                    "{view:?}: locked-only tag listed"
                );
                // Unrevealed, the re-lock may only get shorter (from 5).
                for longer in [0, 15] {
                    assert!(
                        matches!(
                            engine.set_second_lock_auto_lock_minutes(longer),
                            Err(MemloreError::SecondLockRequired)
                        ),
                        "{view:?}: re-lock lengthened to {longer}"
                    );
                }
                assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 5);
                engine.set_second_lock_auto_lock_minutes(1).unwrap();
                assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 1);
                // Turning show-existence off only hides: always allowed.
                engine.set_second_lock_show_existence(false).unwrap();
                assert!(!engine.lock_state().unwrap().show_existence);
            }
        }

        // A wrong password returns false and reveals nothing.
        let (_dir, engine) = unlocked_engine();
        let f = locked_fixture(&engine);
        let before = stored_entry(&engine, &f.locked);
        assert!(
            !engine
                .unlock_second_lock(WRONG_SECOND_LOCK_PASSWORD.to_string())
                .unwrap(),
            "a wrong password revealed"
        );
        assert!(!engine.lock_state().unwrap().revealed);
        expect_not_found(
            engine.get_entry(f.locked.clone()),
            "get_entry after a wrong password",
        );
        assert!(!listed_ids(&engine).contains(&f.locked));
        assert_eq!(stored_entry(&engine, &f.locked), before);

        // The second lock disabled while locked rows exist (a verifier gone,
        // the lock flags left; reachable after sync): fail closed, even for
        // a session revealed before, and the reveal ends, so a verifier
        // restored later (a new desktop password) finds the session closed.
        // Once through a scoped call, once through `lock_state`.
        let restore_verifier = |password: &str| {
            let conn = engine.state.lock().unwrap();
            memlore_engine::ops::second_lock::set_second_lock_password_impl(&conn, password)
                .unwrap();
            db::set_entry_locked(&conn, &f.locked, true).unwrap();
        };
        assert!(engine
            .unlock_second_lock(SECOND_LOCK_PASSWORD.to_string())
            .unwrap());
        db::clear_second_lock_verifier(&engine.state.lock().unwrap()).unwrap();
        expect_not_found(
            engine.get_entry(f.locked.clone()),
            "get_entry with the second lock disabled",
        );
        assert!(!listed_ids(&engine).contains(&f.locked));
        restore_verifier("new-second-lock-pw");
        assert!(
            !engine.lock_state().unwrap().revealed,
            "a scoped call kept the reveal"
        );
        expect_not_found(
            engine.get_entry(f.locked.clone()),
            "get_entry after the verifier came back",
        );

        assert!(engine
            .unlock_second_lock("new-second-lock-pw".to_string())
            .unwrap());
        db::clear_second_lock_verifier(&engine.state.lock().unwrap()).unwrap();
        let state = engine.lock_state().unwrap();
        // `revealed` is the effective view (session AND enabled), not the
        // raw session flag.
        assert!(!state.enabled, "disabled second lock reported enabled");
        assert!(!state.revealed, "disabled second lock reported revealed");
        restore_verifier(SECOND_LOCK_PASSWORD);
        assert!(
            !engine.lock_state().unwrap().revealed,
            "lock_state kept the reveal"
        );
        expect_not_found(
            engine.get_entry(f.locked.clone()),
            "get_entry after the verifier came back",
        );
    }

    /// The reveal session ends on a vault lock (the unlock starts Hidden),
    /// on a second-lock re-lock, and on disable, which also clears every lock
    /// flag; a verifier that comes back later finds the session closed. The
    /// auto re-lock minutes fail closed: missing or garbage reads as 5.
    #[test]
    fn lock_session_ends_on_vault_lock_relock_and_disable() {
        let (_dir, engine) = unlocked_engine();
        let entry = engine.create_entry("Private".to_string(), None).unwrap();
        let journal = engine
            .create_journal("Private journal".to_string())
            .unwrap()
            .id;
        engine
            .set_second_lock_password(SECOND_LOCK_PASSWORD.to_string())
            .unwrap();
        engine.set_entry_locked(entry.clone(), true).unwrap();
        engine.set_journal_locked(journal.clone(), true).unwrap();
        let revealed = || engine.lock_state().unwrap().revealed;
        let reveal = || {
            assert!(engine
                .unlock_second_lock(SECOND_LOCK_PASSWORD.to_string())
                .unwrap());
            engine.get_entry(entry.clone()).unwrap();
        };

        reveal();
        engine.lock().unwrap();
        engine.unlock(VAULT_PASSWORD.to_string()).unwrap();
        assert!(!revealed(), "the vault lock kept the reveal");
        expect_not_found(engine.get_entry(entry.clone()), "after a vault lock");

        reveal();
        engine.lock_second_lock();
        assert!(!revealed(), "the re-lock kept the reveal");
        expect_not_found(engine.get_entry(entry.clone()), "after a re-lock");

        reveal();
        engine
            .disable_second_lock(SECOND_LOCK_PASSWORD.to_string())
            .unwrap();
        let state = engine.lock_state().unwrap();
        assert!(
            !state.enabled && !state.revealed,
            "disable left the lock on"
        );
        {
            let conn = engine.state.lock().unwrap();
            assert!(!db::get_entry_raw(&conn, &entry).unwrap().unwrap().is_locked);
            assert!(!db::get_journal(&conn, &journal).unwrap().unwrap().is_locked);
            // A verifier and a lock that come back (sync) find no session.
            memlore_engine::ops::second_lock::set_second_lock_password_impl(
                &conn,
                SECOND_LOCK_PASSWORD,
            )
            .unwrap();
            db::set_entry_locked(&conn, &entry, true).unwrap();
        }
        assert!(!revealed(), "disable kept the session");
        expect_not_found(engine.get_entry(entry.clone()), "after disable");

        assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 5, "missing");
        engine.set_second_lock_auto_lock_minutes(1).unwrap();
        assert_eq!(engine.lock_state().unwrap().auto_lock_minutes, 1);
        for raw in ["garbage", "7", "-1", ""] {
            {
                let conn = engine.state.lock().unwrap();
                db::set_setting(&conn, "second_lock_auto_lock_minutes", raw).unwrap();
                db::set_setting(&conn, "invisible_lock_auto_lock_minutes", raw).unwrap();
            }
            let state = engine.lock_state().unwrap();
            assert_eq!(state.auto_lock_minutes, 5, "stored {raw:?}");
            assert_eq!(state.invisible_auto_lock_minutes, 5, "stored {raw:?}");
        }
    }

    /// The invisible vault lives in the session: nothing can be made
    /// invisible without an open vault (the generic error, nothing changed),
    /// an unknown password silently opens an empty vault with the same shape
    /// as the right one, the right one lists its entries, closing or a vault
    /// lock hides them, a password change that merges into another vault
    /// keeps them listed, and removing the empty active vault closes it.
    #[test]
    fn invisible_vault_session_scopes_lists_and_is_cleared_on_lock() {
        use memlore_engine::ops::invisible_lock::UNABLE_TO_UPDATE;
        let (_dir, engine) = unlocked_engine();
        let plain = engine.create_entry("Plain".to_string(), None).unwrap();
        let secret = engine.create_entry("Secret".to_string(), None).unwrap();
        let vault_open = || engine.lock_state().unwrap().vault_open;
        let lists_secret = || listed_ids(&engine).contains(&secret);

        let before = stored_entry(&engine, &plain);
        let refused = engine.set_entry_invisible(plain.clone(), true);
        assert!(
            matches!(&refused, Err(MemloreError::Io { msg }) if msg == UNABLE_TO_UPDATE),
            "no vault open: {refused:?}"
        );
        assert!(
            !db::get_entry_raw(&engine.state.lock().unwrap(), &plain)
                .unwrap()
                .unwrap()
                .is_invisible,
            "no vault open: the entry was made invisible"
        );
        assert_eq!(stored_entry(&engine, &plain), before);
        // With no vault open the re-lock may only get shorter (from 5).
        for longer in [0, 15] {
            let refused = engine.set_invisible_auto_lock_minutes(longer);
            assert!(
                matches!(&refused, Err(MemloreError::Io { msg }) if msg == UNABLE_TO_UPDATE),
                "no vault open, {longer}: {refused:?}"
            );
        }
        engine.set_invisible_auto_lock_minutes(1).unwrap();
        assert_eq!(engine.lock_state().unwrap().invisible_auto_lock_minutes, 1);

        let _: () = engine
            .open_invisible_vault(VAULT_A_PASSWORD.to_string())
            .unwrap();
        assert!(vault_open());
        engine.set_invisible_auto_lock_minutes(0).unwrap();
        assert_eq!(engine.lock_state().unwrap().invisible_auto_lock_minutes, 0);
        engine.set_entry_invisible(secret.clone(), true).unwrap();
        let row = engine
            .list_entries(None, 0)
            .unwrap()
            .items
            .into_iter()
            .find(|r| r.id == secret)
            .expect("the open vault does not list its entry");
        assert!(row.is_invisible && !row.is_locked, "vault row flags");
        engine.close_invisible_vault();
        assert!(!vault_open());
        assert!(!lists_secret(), "a closed vault lists its entry");

        // Deniability: an unknown password opens a new empty vault, with the
        // same return shape as an existing one.
        let _: () = engine
            .open_invisible_vault(VAULT_B_PASSWORD.to_string())
            .unwrap();
        assert!(vault_open());
        assert_eq!(listed_ids(&engine), vec![plain.clone()]);

        engine
            .open_invisible_vault(VAULT_A_PASSWORD.to_string())
            .unwrap();
        assert!(lists_secret(), "the right password lists nothing");

        engine.lock().unwrap();
        engine.unlock(VAULT_PASSWORD.to_string()).unwrap();
        assert!(!vault_open(), "the vault lock kept the vault open");
        assert!(!lists_secret(), "the vault lock kept the vault open");

        // Changing A's password to B's merges A into B; the session follows.
        engine
            .open_invisible_vault(VAULT_A_PASSWORD.to_string())
            .unwrap();
        engine
            .change_invisible_vault_password(
                VAULT_A_PASSWORD.to_string(),
                VAULT_B_PASSWORD.to_string(),
            )
            .unwrap();
        assert!(vault_open(), "the merge closed the vault");
        assert!(lists_secret(), "the merge lost the active vault");
        engine.close_invisible_vault();
        engine
            .open_invisible_vault(VAULT_B_PASSWORD.to_string())
            .unwrap();
        assert!(lists_secret(), "the merged vault lost the entry");

        // A's old password now opens a new empty vault; removing empty
        // vaults removes it, closes it, and keeps B.
        engine
            .open_invisible_vault(VAULT_A_PASSWORD.to_string())
            .unwrap();
        assert!(!lists_secret());
        engine.remove_empty_invisible_vaults().unwrap();
        assert!(!vault_open(), "the removed vault stayed open");
        engine
            .open_invisible_vault(VAULT_B_PASSWORD.to_string())
            .unwrap();
        assert!(lists_secret(), "removing empty vaults removed a full one");
    }

    /// A version is only taken of a doc with text (the birth doc is blank),
    /// is listed with the stored bytes, and is read only through its own
    /// entry.
    #[test]
    fn snapshot_skips_blank_docs_and_versions_stay_scoped() {
        let (_dir, engine) = unlocked_engine();
        let id = engine.create_entry("Versioned".to_string(), None).unwrap();
        let other = engine.create_entry("Other".to_string(), None).unwrap();
        assert_eq!(engine.snapshot_entry_version(id.clone()).unwrap(), None);
        assert!(engine.list_entry_versions(id.clone()).unwrap().is_empty());

        write_text(&engine, &id, "First draft");
        let version = engine
            .snapshot_entry_version(id.clone())
            .unwrap()
            .expect("a doc with text is snapshotted");
        let listed = engine.list_entry_versions(id.clone()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, version);
        assert_eq!(listed[0].preview_text, "First draft");
        let stored = db::get_entry_content(&engine.state.lock().unwrap(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(
            engine
                .get_entry_version_content(id.clone(), version.clone())
                .unwrap(),
            stored
        );
        expect_not_found(
            engine.get_entry_version_content(other, version),
            "another entry's version",
        );
    }

    /// Face ID enable, lock, unlock reads the same data: the os_kek lands in
    /// the keystore, the lock closes the database (decision 10) while the flag
    /// still answers from the placeholder, the biometric unlock opens the same
    /// vault and is refused once unlocked, and disable empties the keystore.
    #[test]
    fn face_id_enable_lock_unlock_reads_the_same_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let keystore = Arc::new(MemoryKeyStore::default());
        let engine = MemloreEngine::with_keystore(
            dir.path().to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            keystore.clone(),
        )
        .unwrap();
        engine.create_vault("password12".to_string()).unwrap();
        let id = engine.create_entry("Kept".to_string(), None).unwrap();
        let count = engine.entry_count().unwrap();

        engine.enable_biometric().unwrap();
        engine.lock().unwrap();
        assert!(
            db::get_entry(&engine.state.lock().unwrap(), &id)
                .unwrap()
                .is_none(),
            "lock left the keyed connection open"
        );
        assert!(matches!(
            engine.enable_biometric(),
            Err(MemloreError::Locked)
        ));
        assert!(matches!(
            engine.disable_biometric(),
            Err(MemloreError::Locked)
        ));
        assert!(engine.is_biometric_unlock_enabled());

        engine.unlock_with_biometric().unwrap();
        assert_eq!(engine.entry_count().unwrap(), count);
        assert!(matches!(
            engine.unlock_with_biometric(),
            Err(MemloreError::Io { .. })
        ));

        engine.disable_biometric().unwrap();
        assert!(!engine.is_biometric_unlock_enabled());
        assert!(matches!(
            keystore.retrieve(),
            Err(vault::OsKekError::NotFound)
        ));
    }

    /// The auto-lock interval fails closed: a missing setting, a stored value
    /// that does not parse or is outside {0, 60, 300, 900} reads as the
    /// 1-minute default, never as "never"; `set` accepts only the four
    /// choices, and both calls are lock-gated.
    #[test]
    fn auto_lock_interval_fails_closed() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = MemloreEngine::with_keystore(
            dir.path().to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            Arc::new(MemoryKeyStore::default()),
        )
        .unwrap();
        engine.create_vault("password12".to_string()).unwrap();
        assert_eq!(engine.auto_lock_seconds().unwrap(), 60, "missing");

        for seconds in [0, 60, 300, 900] {
            engine.set_auto_lock_seconds(seconds).unwrap();
            assert_eq!(engine.auto_lock_seconds().unwrap(), seconds);
        }
        for raw in ["abc", "-1", "30", "3600", ""] {
            engine.set_auto_lock_seconds(300).unwrap();
            db::set_setting(&engine.state.lock().unwrap(), AUTO_LOCK_SETTING, raw).unwrap();
            assert_eq!(engine.auto_lock_seconds().unwrap(), 60, "stored {raw:?}");
        }
        for seconds in [30, 3600] {
            engine.set_auto_lock_seconds(300).unwrap();
            assert!(
                matches!(
                    engine.set_auto_lock_seconds(seconds),
                    Err(MemloreError::Format { .. })
                ),
                "set {seconds}"
            );
            assert_eq!(engine.auto_lock_seconds().unwrap(), 300, "set {seconds}");
        }

        engine.lock().unwrap();
        assert!(matches!(
            engine.auto_lock_seconds(),
            Err(MemloreError::Locked)
        ));
        assert!(matches!(
            engine.set_auto_lock_seconds(60),
            Err(MemloreError::Locked)
        ));
    }

    /// First launch wipes the stale Keychain items (the os_kek and the Drive
    /// refresh token), a password-locked launch keeps them: an item left by
    /// an earlier install is never paired with a new vault, and a relaunch
    /// never loses the Face ID key or the Drive sign-in of the vault that is
    /// there.
    #[test]
    fn first_launch_wipes_a_stale_keychain_item_a_password_locked_launch_keeps_it() {
        let fresh = tempfile::TempDir::new().unwrap();
        let stale = Arc::new(MemoryKeyStore::default());
        stale.store(&[7u8; 32]).unwrap();
        let stale_token = Arc::new(MemoryTokenStore::default());
        stale_token.store("stale-token-fixture").unwrap();
        let engine = MemloreEngine::with_stores(
            fresh.path().to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            stale.clone(),
            stale_token.clone(),
        )
        .unwrap();
        assert_eq!(engine.startup_mode(), StartupModeFfi::FirstLaunch);
        assert!(
            matches!(stale.retrieve(), Err(vault::OsKekError::NotFound)),
            "first launch kept a stale Keychain item"
        );
        assert!(
            stale_token.load().unwrap().is_none(),
            "first launch kept a stale Drive token"
        );

        let dir = tempfile::TempDir::new().unwrap();
        let keystore = Arc::new(MemoryKeyStore::default());
        let tokens = Arc::new(MemoryTokenStore::default());
        let open = || {
            MemloreEngine::with_stores(
                dir.path().to_string_lossy().into_owned(),
                "0.0.0-test".into(),
                keystore.clone(),
                tokens.clone(),
            )
            .unwrap()
        };
        let engine = open();
        engine.create_vault("password12".to_string()).unwrap();
        engine.enable_biometric().unwrap();
        tokens.store("token-fixture").unwrap();
        drop(engine);

        let engine = open();
        assert_eq!(engine.startup_mode(), StartupModeFfi::PasswordLocked);
        assert!(
            keystore.retrieve().is_ok(),
            "password-locked launch wiped the Face ID key"
        );
        assert!(
            tokens.load().unwrap().is_some(),
            "password-locked launch wiped the Drive token"
        );
    }

    /// Sets its flag when dropped: the future that owns it was dropped.
    struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// `lock()` ends a running sync before it clears the keys (fail
    /// closed): a sync that never completes, started through the same
    /// plumbing as the real one, holds the single-sync guard; after `lock()`
    /// its future is gone, the guard is free and the keys are cleared. No
    /// Drive credential outlives the lock either: the live session and an
    /// unfinished sign-in are gone (the Keychain rebuilds the session after
    /// an unlock).
    #[test]
    fn lock_ends_a_running_sync_and_clears_keys() {
        let _serial = crate::sync::test_support::serial();
        let (_dir, engine) = unlocked_engine();
        engine.configure_drive("fixture-client.apps.googleusercontent.com".to_string());
        engine.drive_connect_begin().unwrap();
        *engine.live_session.lock().unwrap() = Some(GDriveSession {
            access_token: "access-fixture".to_string(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            refresh_token: Zeroizing::new("refresh-fixture".to_string()),
            client_id: "fixture-client".to_string(),
            client_secret: None,
        });
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        engine
            .spawn_sync_task(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            })
            .unwrap();
        assert!(
            memlore_engine::sync::guard::is_sync_in_progress(),
            "the sync task does not hold the single-sync guard"
        );

        engine.lock().unwrap();
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "lock left the sync running"
        );
        assert!(
            !memlore_engine::sync::guard::is_sync_in_progress(),
            "lock left the single-sync guard taken"
        );
        assert!(!engine.key_state.is_initialized().unwrap());
        assert!(
            engine.live_session.lock().unwrap().is_none(),
            "lock kept the live Drive session"
        );
        assert!(
            engine.oauth.lock().unwrap().is_none(),
            "lock kept the unfinished sign-in"
        );
        assert!(
            matches!(
                engine.spawn_sync_task(std::future::pending::<()>()),
                Err(MemloreError::Locked)
            ),
            "a sync started after the lock"
        );
        assert!(!memlore_engine::sync::guard::is_sync_in_progress());
    }

    /// Delete the phone vault: a wrong password changes nothing (files,
    /// both Keychain items, startup mode, the open vault); the right one
    /// removes the vault files and the media folder, wipes the os_kek and
    /// the Drive token and returns to first launch, so a new vault can be
    /// created in its place, on disk.
    #[test]
    fn delete_vault_is_password_gated_and_wipes_everything() {
        let dir = tempfile::TempDir::new().unwrap();
        let keystore = Arc::new(MemoryKeyStore::default());
        let tokens = Arc::new(MemoryTokenStore::default());
        let open = || {
            MemloreEngine::with_stores(
                dir.path().to_string_lossy().into_owned(),
                "0.0.0-test".into(),
                keystore.clone(),
                tokens.clone(),
            )
            .unwrap()
        };
        let engine = open();
        engine.create_vault("password12".to_string()).unwrap();
        let id = engine.create_entry("Kept".to_string(), None).unwrap();
        engine.enable_biometric().unwrap();
        tokens.store("token-fixture").unwrap();
        let db_path = dir.path().join("memlore.db");
        let boot = boot_file::boot_path(&db_path);
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("photo.bin"), b"media").unwrap();

        assert!(matches!(
            engine.delete_vault("wrong-password".to_string()),
            Err(MemloreError::WrongPassword)
        ));
        assert!(db_path.exists() && boot.exists() && media.join("photo.bin").exists());
        assert!(
            keystore.retrieve().is_ok(),
            "wrong password wiped the os_kek"
        );
        assert!(
            tokens.load().unwrap().is_some(),
            "wrong password wiped the token"
        );
        assert_eq!(engine.startup_mode(), StartupModeFfi::PasswordLocked);
        assert!(
            engine.get_entry(id.clone()).is_ok(),
            "wrong password locked the vault"
        );

        engine.delete_vault("password12".to_string()).unwrap();
        assert!(!boot.exists(), "the boot file survived");
        assert!(!media.exists(), "the media folder survived");
        assert!(matches!(
            keystore.retrieve(),
            Err(vault::OsKekError::NotFound)
        ));
        assert!(tokens.load().unwrap().is_none(), "the Drive token survived");
        assert_eq!(engine.startup_mode(), StartupModeFfi::FirstLaunch);
        assert!(!engine.key_state.is_initialized().unwrap());
        assert!(matches!(engine.entry_count(), Err(MemloreError::Locked)));

        engine.create_vault("password34".to_string()).unwrap();
        expect_not_found(engine.get_entry(id.clone()), "old entry in the new vault");
        drop(engine);
        let engine = open();
        assert_eq!(engine.startup_mode(), StartupModeFfi::PasswordLocked);
        assert!(matches!(
            engine.unlock("password12".to_string()),
            Err(MemloreError::WrongPassword)
        ));
        engine.unlock("password34".to_string()).unwrap();
        expect_not_found(engine.get_entry(id), "old entry after a relaunch");
    }
}
