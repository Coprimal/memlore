//! Journal and tag calls of [`MemloreEngine`], for the lock session's view
//! and open invisible vault. Locked journals are left out under Hidden and
//! listed by name with `is_locked` under Covered and Revealed. Tags used only
//! on locked entries are listed only when revealed (Covered reads as Hidden,
//! stricter than desktop). Invisible journals and tags used only on
//! invisible entries are listed only while their vault is open.

use memlore_engine::db::{self, LockedView};
use memlore_engine::ops::journals::{delete_journal_impl, update_journal_impl, JournalImplError};
use memlore_engine::ops::media::remove_media_files_best_effort;
use memlore_engine::ops::tags::{
    add_tag_to_entry_impl, delete_tag_impl, remove_tag_from_entry_impl, update_tag_impl,
    UpdateTagError,
};

use crate::{
    format_err, io, require_listed_entry, require_listed_journal, MemloreEngine, MemloreError,
    Scope,
};

/// One journal.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct JournalFfi {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub is_locked: bool,
    /// Listed only while its vault is open.
    pub is_invisible: bool,
}

impl From<db::Journal> for JournalFfi {
    fn from(j: db::Journal) -> Self {
        Self {
            id: j.id,
            name: j.name,
            color: j.color,
            is_locked: j.is_locked,
            is_invisible: j.is_invisible,
        }
    }
}

/// One tag.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TagFfi {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
}

impl From<db::Tag> for TagFfi {
    fn from(t: db::Tag) -> Self {
        Self {
            id: t.id,
            name: t.name,
            color: t.color,
        }
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// Journals in the desktop sidebar order: visible ones and those of the
    /// open vault. A locked journal is left out under Hidden and listed (name
    /// and `is_locked`) under Covered and Revealed.
    pub fn list_journals(&self) -> Result<Vec<JournalFfi>, MemloreError> {
        self.with_scope(|conn, scope| {
            Ok(db::list_journals(conn, scope.vault.as_deref())
                .map_err(|e| io(e.to_string()))?
                .into_iter()
                .filter(|j| scope.view != LockedView::Hidden || !j.is_locked)
                .map(JournalFfi::from)
                .collect())
        })
    }

    /// Create a journal named `name`, trimmed; an empty name is a `Format`
    /// error.
    pub fn create_journal(&self, name: String) -> Result<JournalFfi, MemloreError> {
        self.with_unlocked(|conn| {
            let name = journal_name(&name)?;
            db::create_journal(conn, name, None)
                .map(JournalFfi::from)
                .map_err(|e| io(e.to_string()))
        })
    }

    /// Rename a listed journal to `name`, trimmed (empty is a `Format`
    /// error). Its colour and auto-tags are kept.
    pub fn rename_journal(&self, id: String, name: String) -> Result<JournalFfi, MemloreError> {
        self.with_scope(|conn, scope| {
            let name = journal_name(&name)?;
            let journal = require_listed_journal(conn, scope, &id)?;
            update_journal_impl(conn, &id, name, journal.color.as_deref(), None)
                .map(JournalFfi::from)
                .map_err(|e| io(e.to_string()))
        })
    }

    /// The delete confirmation's count for a listed journal (desktop's): live
    /// entries, locked ones included. Trashed and invisible entries also go
    /// with the journal but are not counted.
    pub fn journal_entry_count(&self, id: String) -> Result<u64, MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_journal(conn, scope, &id)?;
            let count = db::count_entries_in_journal(conn, &id).map_err(|e| io(e.to_string()))?;
            Ok(u64::try_from(count).unwrap_or_default())
        })
    }

    /// Delete a listed journal and every entry in it, locked ones included
    /// (desktop parity, irreversible). Refused with `LastJournal` when no
    /// other unlocked, visible journal would remain, in every view (a user is
    /// never left with only journals that need a reveal or a vault). Media
    /// files are unlinked once the delete has committed.
    // TODO(later): M2 must re-arm reset_session_own_cloud_reconciled after a journal delete (docs/LATER.md "Mobile M3a notes")
    pub fn delete_journal(&self, id: String) -> Result<(), MemloreError> {
        let media_paths = self.with_scope(|conn, scope| {
            require_listed_journal(conn, scope, &id)?;
            // `None`, not `scope.vault`, on purpose: an open vault's journals
            // do not count, nor do locked ones, whatever the view.
            let others = db::list_journals(conn, None)
                .map_err(|e| io(e.to_string()))?
                .into_iter()
                .filter(|j| !j.is_locked && j.id != id)
                .count();
            if others == 0 {
                return Err(MemloreError::LastJournal);
            }
            delete_journal_impl(conn, &id).map_err(journal_err)
        })?;
        for (storage_path, thumbnail_path) in &media_paths {
            remove_media_files_best_effort(storage_path, thumbnail_path.as_deref());
        }
        Ok(())
    }

    /// Tags sorted by name, minus those used only on entries the session
    /// cannot read: locked ones unless revealed (Covered too), invisible ones
    /// outside the open vault. A tag used only on deleted entries is still
    /// listed, as on desktop.
    pub fn list_tags(&self) -> Result<Vec<TagFfi>, MemloreError> {
        self.with_scope(|conn, scope| {
            Ok(
                db::list_tags_with_locked_view(conn, tag_view(scope), scope.vault.as_deref())
                    .map_err(|e| io(e.to_string()))?
                    .into_iter()
                    .map(TagFfi::from)
                    .collect(),
            )
        })
    }

    /// The tag named `name` (trimmed), created when it does not exist yet;
    /// an empty name is a `Format` error.
    // TODO(later): get-or-create returns an existing tag even when it is used
    // only on locked entries and so hidden from `list_tags` (desktop parity);
    // revisit with M3c (docs/LATER.md "Mobile M3a notes").
    pub fn create_tag(&self, name: String) -> Result<TagFfi, MemloreError> {
        self.with_unlocked(|conn| {
            db::create_tag(conn, &name, None)
                .map(TagFfi::from)
                .map_err(|e| match e {
                    // The only one `create_tag` raises without a colour:
                    // the empty name.
                    rusqlite::Error::InvalidParameterName(msg) => format_err(msg),
                    other => io(other.to_string()),
                })
        })
    }

    /// Rename a listed tag to `name`, trimmed (empty is a `Format` error);
    /// its colour is kept. A name another tag holds, a deleted one included,
    /// is `NameTaken`.
    pub fn rename_tag(&self, id: String, name: String) -> Result<TagFfi, MemloreError> {
        self.with_scope(|conn, scope| {
            let name = name.trim();
            if name.is_empty() {
                return Err(format_err("tag name must not be empty"));
            }
            require_listed_tag(conn, scope, &id)?;
            update_tag_impl(conn, &id, Some(name), None)
                .map(TagFfi::from)
                .map_err(|e| match e {
                    UpdateTagError::NameTaken => MemloreError::NameTaken,
                    UpdateTagError::Db(rusqlite::Error::QueryReturnedNoRows) => {
                        MemloreError::NotFound
                    }
                    UpdateTagError::Db(other) => io(other.to_string()),
                })
        })
    }

    /// Delete a listed tag. Desktop parity: it also comes off the locked
    /// entries that carry it.
    pub fn delete_tag(&self, id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_tag(conn, scope, &id)?;
            delete_tag_impl(conn, &id).map_err(io)
        })
    }

    /// Put a tag on a listed entry. An unknown or deleted tag is `NotFound`.
    pub fn add_tag_to_entry(&self, entry_id: String, tag_id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &entry_id)?;
            let live = db::get_tag_for_import(conn, &tag_id)
                .map_err(|e| io(e.to_string()))?
                .is_some_and(|t| !t.is_deleted);
            if !live {
                return Err(MemloreError::NotFound);
            }
            add_tag_to_entry_impl(conn, &entry_id, &tag_id).map_err(io)
        })
    }

    /// Take a tag off a listed entry.
    pub fn remove_tag_from_entry(
        &self,
        entry_id: String,
        tag_id: String,
    ) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &entry_id)?;
            remove_tag_from_entry_impl(conn, &entry_id, &tag_id).map_err(io)
        })
    }
}

/// A journal name, trimmed; empty is a `Format` error.
fn journal_name(name: &str) -> Result<&str, MemloreError> {
    let name = name.trim();
    if name.is_empty() {
        Err(format_err("journal name must not be empty"))
    } else {
        Ok(name)
    }
}

/// The per-tag gate of the tag writes: a tag [`MemloreEngine::list_tags`]
/// shows; anything else (deleted, or used only on locked or invisible
/// entries) is [`MemloreError::NotFound`]. Covered reads as Hidden: a tag
/// used only on locked entries is addressable only when revealed.
fn require_listed_tag(
    conn: &rusqlite::Connection,
    scope: &Scope,
    id: &str,
) -> Result<(), MemloreError> {
    let listed = db::list_tags_with_locked_view(conn, tag_view(scope), scope.vault.as_deref())
        .map_err(|e| io(e.to_string()))?
        .iter()
        .any(|t| t.id == id);
    if listed {
        Ok(())
    } else {
        Err(MemloreError::NotFound)
    }
}

/// The view tags are listed under: Covered reads as Hidden, so a tag used
/// only on locked entries is never listed or addressable outside Revealed
/// (stricter than desktop, which lists it under Covered).
fn tag_view(scope: &Scope) -> LockedView {
    match scope.view {
        LockedView::Covered => LockedView::Hidden,
        view => view,
    }
}

/// Typed: the last-journal refusal is its own error code.
fn journal_err(e: JournalImplError) -> MemloreError {
    match e {
        JournalImplError::LastJournal => MemloreError::LastJournal,
        other => io(other.to_string()),
    }
}
