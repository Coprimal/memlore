//! The configured cloud provider and the credential seam it reads through.
//!
//! [`CloudCredentialStore`] is where each platform keeps the Google Drive
//! refresh token and resolves the OAuth client: the desktop keeps the token
//! in a DB setting, the mobile app will keep it in the Keychain. The
//! provider builders take it as a parameter so the engine never reaches for
//! a platform store itself.

use std::sync::Arc;

use serde::Serialize;
use zeroize::Zeroizing;

use super::cloud_provider::CloudProvider;
use super::gdrive_provider::{GDriveProvider, GDriveSession};
use super::local_provider::LocalSyncProvider;
use super::provider::SyncProvider;
use crate::db;

/// OAuth client the Drive session refreshes its token with. The desktop
/// (installed-app flow) always has a secret; an iOS client has none.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct GDriveClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Platform store for the Drive refresh token and OAuth client.
///
/// Every method takes the caller's connection (or transaction) so the
/// desktop, which keeps the token in a settings row, stays atomic with the
/// rest of the caller's writes. A store that keeps the token elsewhere may
/// ignore `conn`.
#[doc(hidden)]
pub trait CloudCredentialStore: Send + Sync {
    /// Resolve the OAuth client id and secret.
    fn client_config(&self, conn: &rusqlite::Connection) -> Result<GDriveClientConfig, String>;
    /// The stored refresh token, or `None` when Drive is not connected.
    fn load_refresh_token(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<Option<Zeroizing<String>>, String>;
    /// Persist the refresh token.
    fn store_refresh_token(&self, conn: &rusqlite::Connection, token: &str) -> Result<(), String>;
    /// Forget the refresh token.
    fn clear_refresh_token(&self, conn: &rusqlite::Connection) -> Result<(), String>;
}

/// JSON shape persisted in `settings.sync_config_json` for the `local`
/// provider. Today it's just a root path; Chunk 4's provider will have a
/// richer shape and we'll discriminate on `settings.sync_provider`.
#[doc(hidden)]
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LocalConfig {
    pub root_path: String,
}

/// Build the configured provider. Returns `None` when sync is disabled or
/// unconfigured — the caller should surface a clear error instead of
/// silently succeeding with a no-op.
///
/// The supported providers carry their config in different places:
/// - `local` and `icloud` store a root path in the `sync_config_json` setting row.
/// - `gdrive` keeps its refresh token in the platform's
///   [`CloudCredentialStore`] (the desktop: `gdrive_refresh_token`) plus
///   companion rows (`gdrive_root_folder_id`, `gdrive_email`). It does
///   not use `sync_config_json` — requiring that row for every provider
///   was the bug that left "Sync now" reporting "Sync is disabled or not
///   configured" even after a successful Google Drive connect.
#[doc(hidden)]
pub fn configured_cloud_provider(
    conn: &rusqlite::Connection,
    creds: &dyn CloudCredentialStore,
) -> Result<Option<CloudProvider>, String> {
    let enabled = db::get_sync_enabled(conn).map_err(|e| e.to_string())?;
    if !enabled {
        return Ok(None);
    }
    let provider = match db::get_sync_provider(conn).map_err(|e| e.to_string())? {
        Some(p) => p,
        None => return Ok(None),
    };
    match provider.as_str() {
        "local" | "icloud" => {
            let json = match db::get_sync_config_json(conn).map_err(|e| e.to_string())? {
                Some(j) => j,
                None => return Ok(None),
            };
            let cfg: LocalConfig = serde_json::from_str(&json).map_err(|e| e.to_string())?;
            let generation = db::get_sync_recovery_generation(conn).map_err(|e| e.to_string())?;
            Ok(Some(CloudProvider::Folder(
                LocalSyncProvider::new(cfg.root_path).with_recovery_fence(generation, None),
            )))
        }
        "gdrive" => {
            // Rebuild the session on every sync — the in-memory
            // `GDriveSessionState` may be empty after an app restart and
            // `GDriveProvider::ensure_fresh_token` will refresh the token
            // itself on first use (the reconstructed session is stamped
            // as expired on purpose).
            let session = match reconstruct_session_from_conn(conn, creds)? {
                Some(s) => s,
                None => return Ok(None),
            };
            let generation = db::get_sync_recovery_generation(conn).map_err(|e| e.to_string())?;
            let gdrive = GDriveProvider::new_live(session)?.with_recovery_fence(generation, None);
            Ok(Some(CloudProvider::GDrive(gdrive)))
        }
        other => Err(format!("Unknown sync provider: {other}")),
    }
}

/// Thin wrapper around [`configured_cloud_provider`] for callers that only
/// need the [`SyncProvider`] trait object.
#[doc(hidden)]
pub fn make_configured_provider(
    conn: &rusqlite::Connection,
    creds: &dyn CloudCredentialStore,
) -> Result<Option<Arc<dyn SyncProvider>>, String> {
    Ok(configured_cloud_provider(conn, creds)?
        .map(|cloud| Arc::new(cloud) as Arc<dyn SyncProvider>))
}

/// Build a Drive session from the stored refresh token on a borrowed
/// connection, without re-locking the `AppState` mutex. The caller of
/// `make_configured_provider` already holds the lock, so locking again
/// would deadlock.
#[doc(hidden)]
pub fn reconstruct_session_from_conn(
    conn: &rusqlite::Connection,
    creds: &dyn CloudCredentialStore,
) -> Result<Option<GDriveSession>, String> {
    let refresh_token = match creds.load_refresh_token(conn)? {
        Some(h) if !h.is_empty() => h,
        _ => return Ok(None),
    };

    let GDriveClientConfig {
        client_id,
        client_secret,
    } = creds.client_config(conn)?;

    Ok(Some(GDriveSession {
        access_token: String::new(),
        // Past expiry → `ensure_fresh_token` refreshes on first API call.
        expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
        refresh_token,
        client_id,
        client_secret,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;
    use crate::AppState;
    use crate::EncryptionKeyState;
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
        ) -> Result<Option<Zeroizing<String>>, String> {
            Ok(db::get_setting(conn, "gdrive_refresh_token")
                .map_err(|e| e.to_string())?
                .filter(|t| !t.is_empty())
                .map(Zeroizing::new))
        }

        fn store_refresh_token(&self, conn: &Connection, token: &str) -> Result<(), String> {
            db::set_setting(conn, "gdrive_refresh_token", token).map_err(|e| e.to_string())
        }

        fn clear_refresh_token(&self, conn: &Connection) -> Result<(), String> {
            db::delete_setting(conn, "gdrive_refresh_token").map_err(|e| e.to_string())
        }
    }

    fn make_state() -> AppState {
        let conn = Connection::open_in_memory().expect("in-memory db");
        migrate(&conn).expect("migrate");
        AppState::new(conn)
    }

    fn make_configured_provider(
        conn: &Connection,
    ) -> Result<Option<Arc<dyn SyncProvider>>, String> {
        super::make_configured_provider(conn, &DbSettingCredentialStore)
    }

    fn make_key_state() -> EncryptionKeyState {
        let ks = EncryptionKeyState::new();
        let key = zeroize::Zeroizing::new([42u8; 32]);
        ks.set_key(key).unwrap();
        ks
    }

    #[test]
    fn make_configured_provider_returns_none_when_sync_disabled() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "local").unwrap();
            db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/xj\"}").unwrap();
            // sync_enabled stays false — disabled overrides everything else.
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(provider.is_none());
    }

    #[test]
    fn make_configured_provider_returns_none_when_no_provider() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(provider.is_none());
    }

    #[test]
    fn make_configured_provider_builds_local_from_config_json() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "local").unwrap();
            db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/xj\"}").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(
            provider.is_some(),
            "local provider should build from sync_config_json"
        );
    }

    #[test]
    fn make_configured_provider_builds_icloud_from_config_json() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "icloud").unwrap();
            db::set_sync_config_json(&conn, "{\"root_path\":\"/tmp/xj-icloud\"}").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(
            provider.is_some(),
            "icloud provider should build from sync_config_json"
        );
    }

    #[test]
    fn make_configured_provider_returns_none_when_local_missing_config_json() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "local").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
            // No sync_config_json.
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(provider.is_none());
    }

    #[test]
    fn make_configured_provider_builds_gdrive_from_refresh_token() {
        let state = make_state();
        let _key_state = make_key_state();
        let refresh_token = "1//fake-refresh-token";
        {
            let conn = state.lock().unwrap();
            db::set_setting(&conn, "gdrive_refresh_token", refresh_token).unwrap();
            db::set_sync_provider(&conn, "gdrive").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(
            provider.is_some(),
            "gdrive provider should build without sync_config_json, using refresh token instead"
        );
    }

    #[test]
    fn make_configured_provider_returns_none_when_gdrive_missing_refresh_token() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "gdrive").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
            // No gdrive_refresh_token.
        }
        let conn = state.lock().unwrap();
        let provider = make_configured_provider(&conn).unwrap();
        assert!(provider.is_none());
    }

    #[test]
    fn make_configured_provider_errors_on_unknown_provider() {
        let state = make_state();
        let _key_state = make_key_state();
        {
            let conn = state.lock().unwrap();
            db::set_sync_provider(&conn, "dropbox").unwrap();
            db::set_sync_enabled(&conn, true).unwrap();
        }
        let conn = state.lock().unwrap();
        let result = make_configured_provider(&conn);
        match result {
            Err(e) => assert!(e.contains("Unknown sync provider"), "got: {e}"),
            Ok(_) => panic!("expected unknown-provider error"),
        }
    }
}
