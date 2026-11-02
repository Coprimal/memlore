//! Version history calls of [`MemloreEngine`]. Only entries the list shows
//! readably have a history on mobile: every call goes through
//! `require_listed_entry`, so a trashed entry, or an invisible one outside
//! its open vault, is `NotFound`, and a locked one needs the reveal
//! (`SecondLockRequired` under Covered, `NotFound` under Hidden).

use memlore_core::outbox::MAX_YJS_DOC_BYTES;
use memlore_engine::db;
use memlore_engine::ops::entries::snapshot_entry_version_impl;

use crate::{format_err, io, require_listed_entry, MemloreEngine, MemloreError};

/// One version of an entry, without its document.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct VersionFfi {
    pub id: String,
    /// Unix seconds.
    pub created_at: i64,
    pub preview_text: String,
}

impl From<db::VersionMeta> for VersionFfi {
    fn from(v: db::VersionMeta) -> Self {
        Self {
            id: v.id,
            created_at: v.created_at,
            preview_text: v.preview_text,
        }
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// The versions of a listed entry, newest first.
    pub fn list_entry_versions(&self, entry_id: String) -> Result<Vec<VersionFfi>, MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &entry_id)?;
            Ok(db::list_entry_versions(conn, &entry_id)
                .map_err(|e| io(e.to_string()))?
                .into_iter()
                .map(VersionFfi::from)
                .collect())
        })
    }

    /// One version's Yjs document (full state). A version of another entry
    /// is `NotFound`.
    pub fn get_entry_version_content(
        &self,
        entry_id: String,
        version_id: String,
    ) -> Result<Vec<u8>, MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &entry_id)?;
            db::get_entry_version_blob_for_entry(conn, &version_id, &entry_id).map_err(
                |e| match e {
                    rusqlite::Error::QueryReturnedNoRows => MemloreError::NotFound,
                    other => io(other.to_string()),
                },
            )
        })
    }

    /// Keep the entry's STORED document as a new version (the desktop session
    /// baseline and pre-restore snapshot), then prune old versions as the
    /// desktop does. Returns the version id, or `None` when the stored text
    /// is blank (the birth doc is non-empty bytes but has no text).
    pub fn snapshot_entry_version(&self, entry_id: String) -> Result<Option<String>, MemloreError> {
        self.with_scope(|conn, scope| {
            let entry = require_listed_entry(conn, scope, &entry_id)?;
            let preview = entry.preview_text.unwrap_or_default();
            let yjs = db::get_entry_content(conn, &entry_id)
                .map_err(|e| io(e.to_string()))?
                .unwrap_or_default();
            if preview.trim().is_empty() || yjs.is_empty() {
                return Ok(None);
            }
            if yjs.len() > MAX_YJS_DOC_BYTES {
                return Err(format_err(
                    "Yjs document exceeds maximum allowed size (10 MiB)",
                ));
            }
            snapshot_entry_version_impl(conn, &entry_id, &yjs, &preview)
                .map(Some)
                .map_err(io)
        })
    }
}
