//! UniFFI surface of `memlore-engine` for the mobile app (proc-macro mode).
//!
//! FFI surface rule: every data method of [`MemloreEngine`] runs behind the
//! lock gate ([`MemloreEngine::with_unlocked`]) and returns
//! [`MemloreError::Locked`] while the vault is locked; listing uses the same
//! locked/invisible-aware query as the desktop list command. Raw `ops::*_impl`,
//! `db::*`, the AI credential readers and the `*_inner` vault helpers are only
//! called from here, never exported. Error messages carry no key material.

use std::path::PathBuf;
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
use memlore_engine::utils::boot_file::{self, UnlockMethod};
use memlore_engine::utils::encryption::KdfParams;
use memlore_engine::vault::biometric::BiometricUnlockError;
use memlore_engine::vault::startup::StartupMode;
use memlore_engine::vault::{self, VaultPlatformHooks};
use memlore_engine::{AppState, EncryptionKeyState};
use tokio::runtime::Runtime;
use zeroize::Zeroizing;

use crate::keystore::OsKeyStore;

pub use entries::{EntryDetailFfi, EntryPageFfi, EntryRowFfi, TrashRowFfi};
pub use organize::{JournalFfi, TagFfi};
pub use search::{SearchFiltersFfi, SearchResultFfi};
pub use templates::TemplateFfi;
pub use versions::VersionFfi;

mod entries;
mod keystore;
mod organize;
mod search;
mod templates;
mod versions;

uniffi::setup_scaffolding!();

/// Process-wide runtime for the engine's async code. Never dropped: a JS
/// reload that creates a second engine reuses it.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

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
/// iOS Keychain), no post-unlock sweep, no pending cloud session.
struct MobileVaultHooks {
    keystore: Arc<dyn OsKeyStore>,
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

    fn consume_pending_cloud_session(
        &self,
        _conn: &rusqlite::Connection,
        _session_id: &str,
    ) -> Option<Result<(), String>> {
        None
    }
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
}

#[uniffi::export]
impl MemloreEngine {
    /// Inject the app version and the runtime, then open the vault at
    /// `app_data_dir/memlore.db` the way the desktop `setup` does.
    #[uniffi::constructor]
    pub fn new(app_data_dir: String, app_version: String) -> Result<Arc<Self>, MemloreError> {
        Self::with_keystore(app_data_dir, app_version, keystore::default_keystore())
    }

    pub fn startup_mode(&self) -> StartupModeFfi {
        (*self.startup_mode.lock().unwrap_or_else(|p| p.into_inner())).into()
    }

    /// First-time setup in one step (the desktop splits it across two screens):
    /// create the vault with a password unlock, no cloud, and return the
    /// 24-word recovery phrase. The vault is unlocked afterwards, and
    /// `startup_mode` reports `PasswordLocked` (what a restart would find).
    pub fn create_vault(&self, password: String) -> Result<String, MemloreError> {
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
        })
    }

    /// Clear the keys from memory, like the desktop `lock_encryption`, then
    /// close the database (decision 10): the keyed SQLCipher connection is
    /// replaced by the password-locked placeholder a cold start opens, so
    /// the Face ID flag still answers. Only for a created vault (boot file
    /// in password mode): before setup the live connection is the one
    /// `create_vault` rekeys, and it holds no user data.
    ///
    /// Never fails open: the keys go first, and if the placeholder cannot
    /// replace the connection it stays open behind the lock gate. That is
    /// logged, but no logger is installed on mobile yet.
    // TODO(later): see docs/LATER.md ("Mobile M1 notes", Phase 2 review suggestions).
    pub fn lock(&self) -> Result<(), MemloreError> {
        self.key_state.clear_key().map_err(io)?;
        let closed = boot_file::load(&boot_file::boot_path(&self.db_path)).and_then(|boot| {
            if boot.mode != db::EncryptionMode::Password {
                return Ok(());
            }
            let placeholder = vault::startup::password_locked_placeholder(&boot)?;
            self.state.set_connection(placeholder)
        });
        if let Err(e) = closed {
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
        vault::biometric::unlock_with_os_kek(
            &self.db_path,
            &self.state,
            &self.key_state,
            &self.hooks,
        )
        .map_err(biometric_unlock_err)
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
        self.with_unlocked(|conn| Ok(u64::from(list_page(conn, 1)?.total)))
    }

    /// The entry's Yjs document (full state), empty when it has no body yet.
    /// Only entries the list shows open: a locked or invisible entry is
    /// "not found" (mobile has no reveal session and no invisible vault).
    pub fn load_entry_yjs(&self, id: String) -> Result<Vec<u8>, MemloreError> {
        self.with_unlocked(|conn| {
            require_listed_entry(conn, &id)?;
            Ok(db::get_entry_content(conn, &id)
                .map_err(|e| io(e.to_string()))?
                .unwrap_or_default())
        })
    }

    /// The editor's save, exactly like the desktop `save_entry_content`
    /// command: `full_state` is the editor's whole document
    /// (`Y.encodeStateAsUpdate`, never an incremental update) and it is
    /// MERGED into the stored one, so an edit that landed meanwhile (MCP
    /// append, outbox import) survives. Then the same post-save hooks, minus
    /// the desktop-only `last_edit` debounce clock.
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
        self.with_unlocked(|conn| {
            require_listed_entry(conn, &id)?;
            save_entry_content_merged_impl(conn, &id, &full_state, &content_text, &preview_text)
                .map_err(io)
        })?;
        self.after_entry_save(&id)
    }
}

impl MemloreEngine {
    /// [`Self::new`] with the os_kek kept in `keystore` (tests pass a fake).
    fn with_keystore(
        app_data_dir: String,
        app_version: String,
        keystore: Arc<dyn OsKeyStore>,
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
        let hooks = MobileVaultHooks { keystore };
        let (conn, mode) =
            vault::startup::startup_init(&db_path, &key_state, &hooks).map_err(io)?;
        Ok(Arc::new(Self {
            db_path,
            state: AppState::new(conn),
            key_state,
            startup_mode: Mutex::new(mode),
            hooks,
            indexer: EntryIndexer::with_stub(),
        }))
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

    /// The desktop post-save hooks (language, embedding), minus its
    /// `last_edit` debounce clock. Call it outside the lock gate, after the
    /// connection guard is released: the embedding hook takes the connection
    /// itself.
    fn after_entry_save(&self, id: &str) -> Result<(), MemloreError> {
        {
            let conn = self.state.lock().map_err(io)?;
            maybe_detect_language_after_save(&conn, id);
        }
        maybe_mark_entry_embedding_dirty_after_save(&self.state, &self.indexer, id);
        Ok(())
    }
}

/// The per-entry gate of every entry call: the same entries [`list_page`]
/// shows (live, not effectively locked, not invisible), anything else is
/// [`MemloreError::NotFound`].
fn require_listed_entry(conn: &rusqlite::Connection, id: &str) -> Result<db::Entry, MemloreError> {
    let entry = db::get_entry(conn, id).map_err(|e| io(e.to_string()))?;
    let visible =
        db::is_entry_visible_for_active_vault(conn, id, None).map_err(|e| io(e.to_string()))?;
    match entry {
        Some(e) if visible && !e.is_deleted && !e.is_locked => Ok(e),
        _ => Err(MemloreError::NotFound),
    }
}

/// The per-journal gate of every call that reads a journal or writes into
/// it: live, visible without an invisible vault, and not locked. Anything
/// else is [`MemloreError::NotFound`]: moving an entry into a hidden journal
/// would hide the entry.
fn require_listed_journal(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<db::Journal, MemloreError> {
    let journal = db::get_journal(conn, id).map_err(|e| io(e.to_string()))?;
    let visible =
        db::is_journal_visible_for_active_vault(conn, id, None).map_err(|e| io(e.to_string()))?;
    match journal {
        Some(j) if visible && !j.is_deleted && !j.is_locked => Ok(j),
        _ => Err(MemloreError::NotFound),
    }
}

/// One page of the desktop "all entries" list query, with the filters a
/// session without a revealed lock or an open invisible vault gets.
fn list_page(
    conn: &rusqlite::Connection,
    page: u32,
) -> Result<db::PagedResult<db::Entry>, MemloreError> {
    db::list_all_entries_paged_with_locked_view(
        conn,
        EntrySort::Newest,
        EntryTimeRange::All,
        None,
        1,
        page,
        LockedView::Hidden,
        None,
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
    use crate::keystore::MemoryKeyStore;
    use yrs::updates::decoder::Decode;
    use yrs::{ReadTxn, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    /// A valid full state that differs from `base` (an editor typed into it).
    fn edited(base: &[u8]) -> Vec<u8> {
        typed(base, "must not land")
    }

    /// `base` plus a paragraph holding `text`, as an editor would save it.
    fn typed(base: &[u8], text: &str) -> Vec<u8> {
        let doc = yrs::Doc::with_client_id(7);
        let fragment = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v1(base).unwrap()).unwrap();
        let paragraph = fragment.push_back(&mut txn, XmlElementPrelim::new("paragraph", []));
        paragraph.push_back(&mut txn, XmlTextPrelim::new(text));
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// Save `text` into a listed entry through the mobile save.
    fn write_text(engine: &MemloreEngine, id: &str, text: &str) {
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

    /// First launch wipes a stale Keychain item, a password-locked launch
    /// keeps it: an os_kek left by an earlier install is never paired with a
    /// new vault, and a relaunch never loses the Face ID key of the vault
    /// that is there.
    #[test]
    fn first_launch_wipes_a_stale_keychain_item_a_password_locked_launch_keeps_it() {
        let fresh = tempfile::TempDir::new().unwrap();
        let stale = Arc::new(MemoryKeyStore::default());
        stale.store(&[7u8; 32]).unwrap();
        let engine = MemloreEngine::with_keystore(
            fresh.path().to_string_lossy().into_owned(),
            "0.0.0-test".into(),
            stale.clone(),
        )
        .unwrap();
        assert_eq!(engine.startup_mode(), StartupModeFfi::FirstLaunch);
        assert!(
            matches!(stale.retrieve(), Err(vault::OsKekError::NotFound)),
            "first launch kept a stale Keychain item"
        );

        let dir = tempfile::TempDir::new().unwrap();
        let keystore = Arc::new(MemoryKeyStore::default());
        let open = || {
            MemloreEngine::with_keystore(
                dir.path().to_string_lossy().into_owned(),
                "0.0.0-test".into(),
                keystore.clone(),
            )
            .unwrap()
        };
        let engine = open();
        engine.create_vault("password12".to_string()).unwrap();
        engine.enable_biometric().unwrap();
        drop(engine);

        let engine = open();
        assert_eq!(engine.startup_mode(), StartupModeFfi::PasswordLocked);
        assert!(
            keystore.retrieve().is_ok(),
            "password-locked launch wiped the Face ID key"
        );
    }
}
