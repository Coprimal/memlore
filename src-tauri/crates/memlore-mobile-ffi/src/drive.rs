//! Google Drive on mobile (M2a): sign-in through the system browser with
//! the PKCE verifier and the state held here, the connect with its mobile
//! outcome, disconnect and status. Tokens, the verifier and the state never
//! cross to Swift or JS: the authorization URL goes out, the one-time
//! callback URL comes back. Nothing here logs a URL, code, token, verifier
//! or state.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use memlore_engine::db;
use memlore_engine::sync::cloud_config::CloudCredentialStore;
use memlore_engine::sync::connect::{
    apply_disconnect_clear, complete_cloud_connect, GdriveConnectOutcome,
};
use memlore_engine::sync::force_re_pair::{clear_force_re_pair, get_force_re_pair_status_inner};
use memlore_engine::sync::gdrive_oauth::{
    build_authorization_url, exchange_code, generate_pkce, parse_redirect_callback,
    GoogleOAuthConfig, PkceVerifier,
};
use memlore_engine::sync::gdrive_provider::{GDriveProvider, GDriveSession};
use memlore_engine::sync::guard::{SyncInProgressGuard, SYNC_IN_PROGRESS_ERR};
use memlore_engine::sync::pending_session::{self, PendingCloudSession};
use memlore_engine::sync::platform::CloudConnectHooks;
use memlore_engine::sync::recovery::read_sync_control;
use memlore_engine::sync::{CloudProvider, SyncProvider};
use memlore_engine::vault::startup::StartupMode;
use tokio::runtime::Runtime;
use zeroize::Zeroizing;

use crate::sync::MobileCredentialStore;
use crate::{io, runtime, MemloreEngine, MemloreError};

/// How long a sign-in started by `drive_connect_begin` stays usable.
const OAUTH_TTL: Duration = Duration::from_secs(10 * 60);

/// Every Google iOS OAuth client id ends with this; its reversed form is
/// the app's redirect scheme.
const CLIENT_ID_SUFFIX: &str = ".apps.googleusercontent.com";

const NOT_CONFIGURED: &str = "Google Drive is not configured on this device";

/// The one sign-in between `drive_connect_begin` and
/// `drive_connect_complete`. Memory only, never logged.
pub(crate) struct PendingOAuth {
    session_id: String,
    verifier: PkceVerifier,
    state: Zeroizing<String>,
    redirect_uri: String,
    client_id: String,
    created_at: Instant,
}

/// What the app needs to open the Google sign-in: pass `auth_url` and
/// `redirect_uri` to the browser session, then hand its callback URL to
/// `drive_connect_complete` with `session_id`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DriveConnectStartFfi {
    pub session_id: String,
    pub auth_url: String,
    /// `com.googleusercontent.apps.<id>:/oauth2redirect`.
    pub redirect_uri: String,
}

/// Where a Drive connect leaves the app.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectOutcomeFfi {
    /// The vault is connected (an empty Drive got this vault's keyring).
    Connected,
    /// First launch and Drive holds a vault: onboard with the phrase
    /// (`onboard_validate_phrase`, `onboard_complete`) under `session_id`.
    NeedsOnboarding { session_id: String },
    /// First launch and Drive holds no vault: create one here, then connect
    /// Drive in Settings.
    NoCloudVault,
    /// The vault is connected but blocked until the re-pair with the phrase
    /// (`re_pair`): this empty phone vault joins a different cloud vault,
    /// or this phone reconnected to its own vault after a peer rotated it
    /// (its entries are kept).
    JoinWithPhrase,
}

/// Settings > Sync, read from the vault's settings only (no network).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DriveStatusFfi {
    pub connected: bool,
    pub email: Option<String>,
    pub last_sync_ms: Option<i64>,
    /// Entries waiting to be pushed.
    pub pending_count: u64,
    /// Why sync is blocked until a re-pair with the phrase, if it is.
    pub blocked: Option<String>,
}

/// The mobile [`CloudConnectHooks`]: no progress events (the sign-in sheet
/// shows its own), the live session in memory, the Keychain credentials.
pub(crate) struct MobileConnectHooks<'a> {
    pub(crate) credentials: &'a MobileCredentialStore,
    pub(crate) live_session: &'a Mutex<Option<GDriveSession>>,
}

impl CloudConnectHooks for MobileConnectHooks<'_> {
    fn on_connect_progress(&self, _session_id: &str, _step: &str) {}

    fn set_live_session(&self, session: Option<GDriveSession>) {
        *self.live_session.lock().unwrap_or_else(|p| p.into_inner()) = session;
    }

    fn credentials(&self) -> &dyn CloudCredentialStore {
        self.credentials
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// Start a Google sign-in (PKCE): the URL to open and its session. A
    /// new start replaces an unfinished one.
    pub fn drive_connect_begin(&self) -> Result<DriveConnectStartFfi, MemloreError> {
        let client_id = self
            .credentials
            .client_id()
            .ok_or_else(|| io(NOT_CONFIGURED.to_string()))?;
        let redirect_uri =
            ios_redirect_uri(&client_id).ok_or_else(|| io(NOT_CONFIGURED.to_string()))?;
        let (verifier, challenge) = generate_pkce();
        let state = Zeroizing::new(random_hex());
        let session_id = random_hex();
        let auth_url = build_authorization_url(
            &client_id,
            &redirect_uri,
            &state,
            &challenge,
            &GoogleOAuthConfig::default(),
        )
        .map_err(io)?
        .to_string();
        *self.oauth.lock().unwrap_or_else(|p| p.into_inner()) = Some(PendingOAuth {
            session_id: session_id.clone(),
            verifier,
            state,
            redirect_uri: redirect_uri.clone(),
            client_id,
            created_at: Instant::now(),
        });
        Ok(DriveConnectStartFfi {
            session_id,
            auth_url,
            redirect_uri,
        })
    }

    /// Finish the sign-in with the browser's callback URL: exchange the
    /// code (no client secret), connect the vault and map the outcome for
    /// the phone. A phone vault holding entries never joins a different
    /// cloud vault: that connect is undone and refused with `VaultNotEmpty`.
    /// A cloud that already holds this phone's device folder is its own
    /// vault: after a peer rotation it is `JoinWithPhrase`, entries or not.
    /// Needs first launch or an unlocked vault.
    pub fn drive_connect_complete(
        &self,
        session_id: String,
        callback_url: String,
    ) -> Result<ConnectOutcomeFfi, MemloreError> {
        let callback_url = Zeroizing::new(callback_url);
        // The guard and the lock check come first: a refusal leaves the
        // one-time sign-in usable for a retry.
        let guard = SyncInProgressGuard::try_acquire()
            .ok_or_else(|| io(SYNC_IN_PROGRESS_ERR.to_string()))?;
        let local_mode = self.connect_mode()?;
        let oauth = self.take_oauth(&session_id)?;
        let code = Zeroizing::new(
            parse_redirect_callback(&callback_url, &oauth.state, &oauth.redirect_uri)
                .map_err(io)?,
        );
        let tokens = runtime()?
            .block_on(exchange_code(
                &oauth.client_id,
                None,
                &code,
                &oauth.verifier,
                &oauth.redirect_uri,
                &GoogleOAuthConfig::default(),
            ))
            .map_err(io)?;
        let refresh_token = Zeroizing::new(
            tokens
                .refresh_token
                .ok_or_else(|| io("Google did not return a refresh token".to_string()))?,
        );
        let session = GDriveSession {
            access_token: tokens.access_token,
            expires_at: Instant::now() + Duration::from_secs(tokens.expires_in.saturating_sub(60)),
            refresh_token,
            client_id: oauth.client_id,
            client_secret: None,
        };
        let provider = || {
            GDriveProvider::new_live(session.clone())
                .map(CloudProvider::GDrive)
                .map_err(io)
        };
        self.finish_connect(
            guard,
            &session_id,
            local_mode,
            provider()?,
            provider()?,
            PendingCloudSession::GDrive {
                session: session.clone(),
                root_folder_id: String::new(),
            },
        )
    }

    /// Drop an unfinished sign-in, and the onboarding session a connect
    /// left for it.
    pub fn drive_connect_cancel(&self, session_id: String) {
        {
            let mut slot = self.oauth.lock().unwrap_or_else(|p| p.into_inner());
            if slot.as_ref().is_some_and(|p| p.session_id == session_id) {
                *slot = None;
            }
        }
        drop(pending_session::consume(&session_id));
    }

    /// Disconnect Drive on this phone only: end a running sync, clear the
    /// Drive settings, the Keychain token and the live session. Nothing is
    /// revoked at Google (decision 3).
    pub fn drive_disconnect(&self) -> Result<(), MemloreError> {
        if !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        drop(self.cancel_sync_and_wait());
        apply_disconnect_clear(&self.state, &self.connect_hooks()).map_err(io)
    }

    /// The Drive connection as Settings > Sync shows it.
    pub fn drive_status(&self) -> Result<DriveStatusFfi, MemloreError> {
        let status = self.with_unlocked(|conn| read_drive_status(conn).map_err(io))?;
        if !status.connected {
            // Like the desktop status read: no live session outlives the
            // connection.
            *self.live_session.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        Ok(status)
    }
}

impl MemloreEngine {
    pub(crate) fn connect_hooks(&self) -> MobileConnectHooks<'_> {
        MobileConnectHooks {
            credentials: &self.credentials,
            live_session: &self.live_session,
        }
    }

    /// The sign-in `session_id` started, if it is the current one and has
    /// not expired. Taken either way once it matches: a code is used once.
    fn take_oauth(&self, session_id: &str) -> Result<PendingOAuth, MemloreError> {
        let mut slot = self.oauth.lock().unwrap_or_else(|p| p.into_inner());
        match slot.take() {
            Some(oauth) if oauth.session_id == session_id => {
                if oauth.created_at.elapsed() < OAUTH_TTL {
                    Ok(oauth)
                } else {
                    Err(io("The Google sign-in expired; start it again".to_string()))
                }
            }
            other => {
                *slot = other;
                Err(io("Unknown or expired sign-in session".to_string()))
            }
        }
    }

    /// The local mode a connect starts from: a first launch (no vault yet)
    /// or an unlocked vault, else `Locked`.
    pub(crate) fn connect_mode(&self) -> Result<db::EncryptionMode, MemloreError> {
        let first_launch = *self.startup_mode.lock().unwrap_or_else(|p| p.into_inner())
            == StartupMode::FirstLaunch;
        if !first_launch && !self.key_state.is_initialized().map_err(io)? {
            return Err(MemloreError::Locked);
        }
        let conn = self.state.lock().map_err(io)?;
        db::get_encryption_mode(&conn).map_err(|e| io(e.to_string()))
    }

    /// The provider-agnostic tail of a connect, holding the single-sync
    /// guard: engine `complete_cloud_connect` with the mobile hooks, then
    /// the mobile mapping of its outcome. `local_mode` comes from
    /// [`Self::connect_mode`]. `cleanup` is a second handle on the same
    /// cloud, only for this phone's device folder: whether it was there
    /// before the connect (the proof of a reconnect to the phone's own
    /// vault), and its removal after a refusal.
    pub(crate) fn finish_connect(
        &self,
        _guard: SyncInProgressGuard,
        session_id: &str,
        local_mode: db::EncryptionMode,
        provider: CloudProvider,
        cleanup: CloudProvider,
        pending: PendingCloudSession,
    ) -> Result<ConnectOutcomeFfi, MemloreError> {
        let unlocked = self.key_state.is_initialized().map_err(io)?;
        let runtime = runtime()?;
        // `cleanup` only ever touches this phone's device folder: the probe
        // here and the delete in `undo_foreign_connect`.
        let folder = self.device_folder(runtime, &cleanup);
        let hooks = self.connect_hooks();
        let outcome = runtime
            .block_on(complete_cloud_connect(
                &hooks,
                &self.state,
                &self.key_state,
                session_id,
                provider,
                pending,
                local_mode,
                unlocked,
                "",
            ))
            .map_err(io)?;
        match outcome {
            GdriveConnectOutcome::Ready => Ok(ConnectOutcomeFfi::Connected),
            GdriveConnectOutcome::NeedsOnboarding => Ok(ConnectOutcomeFfi::NeedsOnboarding {
                session_id: session_id.to_string(),
            }),
            GdriveConnectOutcome::NeedsFirstTimeSetup => {
                // The phone has no "set up, then attach this Drive" step:
                // drop the session the engine kept (it holds the token).
                drop(pending_session::consume(session_id));
                Ok(ConnectOutcomeFfi::NoCloudVault)
            }
            GdriveConnectOutcome::NeedsForceRePair { .. } => {
                // Fails closed: a count that cannot be read refuses.
                let holds_entries = {
                    let conn = self.state.lock().map_err(io)?;
                    db::count_all_entries(&conn).map_or(true, |n| n > 0)
                };
                // The phone's own vault, rotated by a peer: the connection
                // and the re-pair flag stay as the engine left them, and
                // `re_pair` keeps the entries.
                if !holds_entries || folder == DeviceFolder::Existed {
                    return Ok(ConnectOutcomeFfi::JoinWithPhrase);
                }
                let absent_in = match folder {
                    DeviceFolder::Absent { generation } => Some(generation),
                    DeviceFolder::Existed | DeviceFolder::Unknown => None,
                };
                self.undo_foreign_connect(runtime, &hooks, &cleanup, absent_in)?;
                Err(MemloreError::VaultNotEmpty)
            }
            GdriveConnectOutcome::V1WipedReconnectRequired => Err(io(
                "Google Drive held an old vault format that was cleared; connect again".to_string(),
            )),
        }
    }

    /// Whether this phone's device folder is in the cloud of `cleanup`
    /// before the connect: a reconnect to the phone's own vault (which the
    /// engine also answers with a force re-pair after a peer rotation)
    /// finds the folder holding its synced payload. Looked for in the
    /// cloud's current generation, the one the connect writes into. No
    /// device id yet means no folder. `Unknown` when it cannot tell.
    fn device_folder(&self, runtime: &Runtime, cleanup: &CloudProvider) -> DeviceFolder {
        let Some(generation) = stamp_cloud_generation(runtime, cleanup) else {
            return DeviceFolder::Unknown;
        };
        let device_id = self
            .state
            .lock()
            .and_then(|conn| db::get_setting(&conn, db::DEVICE_ID_KEY).map_err(|e| e.to_string()));
        let device_id = match device_id {
            Ok(Some(id)) => id,
            Ok(None) => return DeviceFolder::Absent { generation },
            Err(e) => {
                log::warn!("connect: no device id to probe the cloud for ({e})");
                return DeviceFolder::Unknown;
            }
        };
        match runtime.block_on(SyncProvider::list_devices(cleanup)) {
            Ok(devices) if devices.contains(&device_id) => DeviceFolder::Existed,
            Ok(_) => DeviceFolder::Absent { generation },
            Err(e) => {
                log::warn!("connect: the cloud's device folders were not listed ({e})");
                DeviceFolder::Unknown
            }
        }
    }

    /// Undo a connect to a different cloud vault: the disconnect clear
    /// (settings, Keychain token, live session), the force re-pair flag the
    /// connect set (the disconnect clear keeps it; cleared only after the
    /// disconnect clear succeeded), then, when `absent_in` (the generation
    /// the folder was proven absent in before the connect) is still the
    /// cloud's current one, a best-effort removal of the device folder the
    /// connect created there. Otherwise the folder stays: it may hold this
    /// phone's synced payload (fail closed).
    fn undo_foreign_connect(
        &self,
        runtime: &Runtime,
        hooks: &MobileConnectHooks<'_>,
        cleanup: &CloudProvider,
        absent_in: Option<u64>,
    ) -> Result<(), MemloreError> {
        // The flag goes only once the clear went through: a failed clear can
        // leave the Drive settings behind, and the flag then keeps the
        // phone from pushing into that vault (fail closed).
        let cleared = apply_disconnect_clear(&self.state, hooks).and_then(|()| {
            self.state
                .lock()
                .and_then(|conn| clear_force_re_pair(&conn))
        });
        let Some(absent_in) = absent_in else {
            log::info!("connect refusal: this phone's folder may predate the connect and stays");
            return cleared.map_err(io);
        };
        let device_id = self
            .state
            .lock()
            .and_then(|conn| db::get_or_create_device_id(&conn).map_err(|e| e.to_string()));
        match device_id {
            Ok(device_id) => {
                // Stamped again from a fresh read: the folder the connect
                // created is in the cloud's current generation, and the
                // stamp drops the folder ids the probe cached before it.
                match stamp_cloud_generation(runtime, cleanup) {
                    Some(generation) if generation == absent_in => {
                        if let Err(e) = runtime
                            .block_on(cleanup.best_effort_delete_device_namespace(&device_id))
                        {
                            log::warn!(
                                "connect refusal: this phone's folder stayed in the cloud ({e})"
                            );
                        }
                    }
                    _ => log::warn!(
                        "connect refusal: the cloud's generation is unknown or moved; this phone's folder stays"
                    ),
                }
            }
            Err(e) => log::warn!("connect refusal: no device id to clean up ({e})"),
        }
        cleared.map_err(io)
    }
}

/// Stamp `cleanup` with the recovery generation in the cloud's control, so
/// its device-folder lookups and deletes land in that generation's folder
/// (`generations/g-N`). `None` (nothing stamped) when the control cannot be
/// read or is missing.
fn stamp_cloud_generation(runtime: &Runtime, cleanup: &CloudProvider) -> Option<u64> {
    match runtime.block_on(read_sync_control(cleanup)) {
        Ok(Some(control)) => {
            cleanup.configure_recovery_fence_authority(control.recovery_generation, None);
            Some(control.recovery_generation)
        }
        Ok(None) => {
            log::warn!("connect: the cloud has no recovery control");
            None
        }
        Err(e) => {
            log::warn!("connect: the cloud's recovery control was not read ({e})");
            None
        }
    }
}

/// This phone's device folder in a cloud, probed before a connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceFolder {
    /// The folder is there: the cloud is this phone's own vault.
    Existed,
    /// No folder (or no device id yet) in the cloud's `generation`: any
    /// folder there is the connect's.
    Absent { generation: u64 },
    /// The probe failed: never delete.
    Unknown,
}

/// The settings behind [`DriveStatusFfi`], like the desktop
/// `gdrive_get_status`: account and last sync only while connected.
fn read_drive_status(conn: &rusqlite::Connection) -> Result<DriveStatusFfi, String> {
    let connected = db::get_setting(conn, "sync_connected")
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some("true");
    let (email, last_sync_ms) = if connected {
        (
            db::get_setting(conn, "gdrive_email").map_err(|e| e.to_string())?,
            db::get_last_sync_at(conn)
                .map_err(|e| e.to_string())?
                .map(|secs| secs.saturating_mul(1000)),
        )
    } else {
        (None, None)
    };
    Ok(DriveStatusFfi {
        connected,
        email,
        last_sync_ms,
        pending_count: db::count_pending_entries(conn).map_err(|e| e.to_string())?,
        blocked: get_force_re_pair_status_inner(conn)?,
    })
}

/// `com.googleusercontent.apps.<id>:/oauth2redirect` for an iOS client id
/// `<id>.apps.googleusercontent.com`; `None` for anything else.
fn ios_redirect_uri(client_id: &str) -> Option<String> {
    let id = client_id.strip_suffix(CLIENT_ID_SUFFIX)?;
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    valid.then(|| format!("com.googleusercontent.apps.{id}:/oauth2redirect"))
}

/// 128 random bits as hex, from the same `OsRng` source as content keys.
fn random_hex() -> String {
    hex::encode(&memlore_core::encryption::generate_content_key()[..16])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use memlore_engine::db;
    use memlore_engine::sync::connect::persist_connect_settings;
    use memlore_engine::sync::force_re_pair::get_force_re_pair_status_inner;
    use memlore_engine::sync::gdrive_provider::GDriveSession;
    use memlore_engine::sync::pending_session::{self, PendingCloudSession, PendingDriveSession};
    use memlore_engine::sync::platform::CloudConnectHooks;
    use memlore_engine::sync::LocalSyncProvider;
    use memlore_engine::vault::VaultPlatformHooks;
    use zeroize::Zeroizing;

    use super::*;
    use crate::keystore::{DriveTokenStore, MemoryTokenStore};
    use crate::sync::test_support::{
        cloud_tree, connect_folder, engine_at, serial, sync_and_wait, DesktopRole,
    };
    use crate::sync::SyncPhaseFfi;
    use crate::MemloreError;

    fn setting(engine: &MemloreEngine, key: &str) -> Option<String> {
        db::get_setting(&engine.state.lock().unwrap(), key).unwrap()
    }

    /// Decision 4 on the connect side: a phone vault that holds an entry
    /// (a trashed one counts) never joins a different cloud vault. The
    /// connect is undone: no blocked (force re-pair) state, no Drive
    /// settings, and no phone folder left in the other vault, also when that
    /// vault is past recovery generation 0 (the connect wrote into its
    /// `generations/g-1`). An empty phone vault is offered the join with the
    /// phrase instead, and the re-pair then joins it.
    #[test]
    fn connect_refuses_a_foreign_vault_for_a_phone_with_entries() {
        let _serial = serial();
        for wiped in [false, true] {
            refuses_a_foreign_vault(wiped);
        }
    }

    fn refuses_a_foreign_vault(wiped: bool) {
        let cloud = tempfile::TempDir::new().unwrap();
        let generation = prepare_cloud(cloud.path(), wiped);
        let desktop_dir = tempfile::TempDir::new().unwrap();
        let desktop = engine_at(desktop_dir.path(), Arc::default());
        let phrase = desktop.create_vault("password12".to_string()).unwrap();
        assert_eq!(
            connect_folder(&desktop, cloud.path(), "foreign-desktop").unwrap(),
            ConnectOutcomeFfi::Connected
        );
        assert_eq!(cloud_generation(cloud.path()), generation);

        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        phone.create_vault("password34".to_string()).unwrap();
        let entry = phone.create_entry("Mine".to_string(), None).unwrap();
        phone.trash_entry(entry).unwrap();
        let device_id = db::get_or_create_device_id(&phone.state.lock().unwrap()).unwrap();
        let before = cloud_tree(cloud.path());

        match connect_folder(&phone, cloud.path(), "foreign-phone") {
            Err(MemloreError::VaultNotEmpty) => {}
            other => panic!("generation {generation}: expected VaultNotEmpty, got {other:?}"),
        }
        assert_eq!(
            get_force_re_pair_status_inner(&phone.state.lock().unwrap()).unwrap(),
            None,
            "generation {generation}: the refusal left the phone blocked"
        );
        for key in ["sync_connected", "sync_provider", "sync_config_json"] {
            assert_eq!(
                setting(&phone, key),
                None,
                "generation {generation}: the refusal kept {key}"
            );
        }
        let after = cloud_tree(cloud.path());
        assert!(
            !after.keys().any(|path| path.contains(&device_id)),
            "generation {generation}: the phone's folder stayed in the other vault"
        );
        assert_eq!(
            after, before,
            "generation {generation}: the refusal changed the other vault"
        );
        assert_eq!(
            db::count_all_entries(&phone.state.lock().unwrap()).unwrap(),
            1
        );

        let empty_dir = tempfile::TempDir::new().unwrap();
        let empty = engine_at(empty_dir.path(), Arc::default());
        empty.create_vault("password56".to_string()).unwrap();
        assert_eq!(
            connect_folder(&empty, cloud.path(), "foreign-empty").unwrap(),
            ConnectOutcomeFfi::JoinWithPhrase
        );
        assert!(empty.drive_status().unwrap().blocked.is_some());
        empty.re_pair(phrase, "password78".to_string()).unwrap();
        assert_eq!(empty.drive_status().unwrap().blocked, None);
    }

    /// The engine also answers a reconnect to the phone's own vault with a
    /// force re-pair when a peer rotated it. The phone's device folder is
    /// already there (its synced payload, in the cloud's current
    /// `generations/g-N`, also past generation 0), so the vault is the
    /// phone's own: a phone holding entries is offered the re-pair, not
    /// refused, and its folder is left byte for byte. `re_pair` with the
    /// phrase then resumes sync, and the phone's entries, one written after
    /// the rotation included, reach the cloud.
    #[test]
    fn reconnect_after_a_peer_rotation_re_pairs_a_phone_with_entries() {
        let _serial = serial();
        for wiped in [false, true] {
            re_pairs_after_a_peer_rotation(wiped);
        }
    }

    fn re_pairs_after_a_peer_rotation(wiped: bool) {
        let cloud = tempfile::TempDir::new().unwrap();
        let generation = prepare_cloud(cloud.path(), wiped);
        let phone_dir = tempfile::TempDir::new().unwrap();
        let phone = engine_at(phone_dir.path(), Arc::default());
        let phrase = phone.create_vault("password12".to_string()).unwrap();
        let entry = phone.create_entry("Mine".to_string(), None).unwrap();
        crate::tests::write_text(&phone, &entry, "phone body");
        assert_eq!(
            connect_folder(&phone, cloud.path(), "rotated-phone").unwrap(),
            ConnectOutcomeFfi::Connected
        );
        assert_eq!(sync_and_wait(&phone).phase, SyncPhaseFfi::Synced);

        let desktop = DesktopRole::join(cloud.path(), &phrase, "password34");
        desktop.rotate("password34", &phrase);
        phone.drive_disconnect().unwrap();
        let later = phone.create_entry("After".to_string(), None).unwrap();
        crate::tests::write_text(&phone, &later, "written after the rotation");
        let device_id = db::get_or_create_device_id(&phone.state.lock().unwrap()).unwrap();
        let phone_folder = || {
            cloud_tree(cloud.path())
                .into_iter()
                .filter(|(path, _)| path.split('/').any(|part| part == device_id))
                .collect::<BTreeMap<_, _>>()
        };
        let before = phone_folder();
        let synced_prefix = format!("generations/g-{generation}/{device_id}/");
        assert_eq!(cloud_generation(cloud.path()), generation);
        assert!(
            before
                .keys()
                .any(|path| path.starts_with(&synced_prefix) && !path.ends_with('/')),
            "the phone synced no file into {synced_prefix}"
        );

        match connect_folder(&phone, cloud.path(), "rotated-reconnect") {
            Ok(ConnectOutcomeFfi::JoinWithPhrase) => {}
            other => panic!("generation {generation}: expected JoinWithPhrase, got {other:?}"),
        }
        assert!(
            get_force_re_pair_status_inner(&phone.state.lock().unwrap())
                .unwrap()
                .is_some(),
            "generation {generation}: the reconnect did not ask for the re-pair"
        );
        assert_eq!(setting(&phone, "sync_connected").as_deref(), Some("true"));
        let after = phone_folder();
        assert!(
            after == before,
            "generation {generation}: the reconnect changed the phone's synced folder: {:?}",
            after.keys().collect::<Vec<_>>()
        );

        phone.re_pair(phrase, "password56".to_string()).unwrap();
        let status = sync_and_wait(&phone);
        assert_eq!(status.phase, SyncPhaseFfi::Synced, "{status:?}");
        assert_eq!(status.blocked, None);
        assert_eq!(status.pending_count, 0, "the phone pushed nothing");
        assert_eq!(
            db::count_all_entries(&phone.state.lock().unwrap()).unwrap(),
            2
        );
        let summary = desktop.sync();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        let phone_conn = phone.state.lock().unwrap();
        let desktop_conn = desktop.state.lock().unwrap();
        for id in [&entry, &later] {
            let pulled = db::get_entry(&desktop_conn, id)
                .unwrap()
                .unwrap_or_else(|| panic!("the phone's entry {id} did not reach the cloud"));
            let mine = db::get_entry(&phone_conn, id).unwrap().unwrap();
            assert_eq!(
                (pulled.title, pulled.content_text),
                (mine.title, mine.content_text)
            );
        }
    }

    /// The folder cloud at `root` before a test vault connects: untouched
    /// (recovery generation 0), or, when `wiped`, a desktop vault connected
    /// and its payload deleted under the cleanup lease (generation 1). The
    /// generation.
    fn prepare_cloud(root: &Path, wiped: bool) -> u64 {
        if !wiped {
            return 0;
        }
        let wiper = DesktopRole::first_launch();
        wiper.create_vault("password90");
        wiper.connect(root, "wiped-cloud");
        let generation = wiper.wipe_cloud();
        assert_eq!(generation, 1);
        generation
    }

    /// The recovery generation in the control of the folder cloud at `root`.
    fn cloud_generation(root: &Path) -> u64 {
        let cloud = LocalSyncProvider::new(root);
        runtime()
            .unwrap()
            .block_on(read_sync_control(&cloud))
            .unwrap()
            .expect("the cloud has no control")
            .recovery_generation
    }

    /// The refresh token lives in the token store (the Keychain), never in
    /// the DB setting the desktop keeps it in: not after onboarding consumes
    /// a pending Drive session, not after a connect persists one, and after
    /// a disconnect it is gone from the store too.
    #[test]
    fn refresh_token_never_lands_in_db_settings() {
        let dir = tempfile::TempDir::new().unwrap();
        let tokens = Arc::new(MemoryTokenStore::default());
        let engine = engine_at(dir.path(), tokens.clone());
        engine.create_vault("password12".to_string()).unwrap();
        let drive_session = |tag: &str| PendingCloudSession::GDrive {
            session: GDriveSession {
                access_token: "access-fixture".to_string(),
                expires_at: Instant::now() + Duration::from_secs(3600),
                refresh_token: Zeroizing::new(format!("refresh-{tag}")),
                client_id: "client-fixture".to_string(),
                client_secret: None,
            },
            root_folder_id: "root-fixture".to_string(),
        };
        let stored = || tokens.load().unwrap().map(|t| t.to_string());

        let session_id = "token-never-in-db-onboard";
        pending_session::stash(
            session_id.to_string(),
            PendingDriveSession {
                session: drive_session("onboard"),
                created_at: Instant::now(),
            },
        );
        {
            let conn = engine.state.lock().unwrap();
            assert!(matches!(
                engine
                    .hooks
                    .consume_pending_cloud_session(&conn, session_id),
                Some(Ok(()))
            ));
        }
        assert_eq!(setting(&engine, "gdrive_refresh_token"), None);
        assert_eq!(stored().as_deref(), Some("refresh-onboard"));
        assert_eq!(setting(&engine, "sync_provider").as_deref(), Some("gdrive"));

        {
            let conn = engine.state.lock().unwrap();
            persist_connect_settings(
                &conn,
                &drive_session("connect"),
                engine.connect_hooks().credentials(),
            )
            .unwrap();
        }
        assert_eq!(setting(&engine, "gdrive_refresh_token"), None);
        assert_eq!(stored().as_deref(), Some("refresh-connect"));

        engine.drive_disconnect().unwrap();
        assert_eq!(setting(&engine, "gdrive_refresh_token"), None);
        assert_eq!(stored(), None, "disconnect kept the token");
        assert_eq!(setting(&engine, "sync_connected"), None);
    }
}
