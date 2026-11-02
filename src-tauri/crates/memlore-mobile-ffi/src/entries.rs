//! Entry calls of [`MemloreEngine`]: the list, one entry and its fields, and
//! Trash. Every call runs behind the lock gate and the per-entry and
//! per-journal gates of `lib.rs`. Lists and Trash take the view and the open
//! invisible vault of the lock session: Hidden leaves locked entries out,
//! Covered lists them redacted by the engine (no title, preview or other
//! content), Revealed shows them.

use memlore_engine::db::{self, EntrySort, EntryTimeRange, LockFilter};
use memlore_engine::ops::entries::{
    create_entry_impl, empty_trash_impl, list_trashed_entries_impl, move_entry_to_journal_impl,
    not_in_trash, purge_entry_impl, restore_entry_impl, save_entry_content_impl,
    soft_delete_entry_impl, toggle_favorite_impl, update_entry_date_impl,
    update_entry_emotion_impl, update_entry_impl,
};
use memlore_engine::ops::guards::validate_emotion;

use crate::organize::TagFfi;
use crate::{
    format_err, io, list_page, require_listed_entry, require_listed_journal, MemloreEngine,
    MemloreError,
};

/// One row of the entries list.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EntryRowFfi {
    pub id: String,
    pub title: Option<String>,
    pub preview_text: Option<String>,
    /// Unix seconds.
    pub entry_date: i64,
    pub emotion: Option<String>,
    pub is_favorite: bool,
    pub journal_id: String,
    /// Locked by its own flag or its journal's (effective, like the gates).
    pub is_locked: bool,
    /// Invisible by its own flag or its journal's: listed only while its
    /// vault is open.
    pub is_invisible: bool,
}

impl From<db::Entry> for EntryRowFfi {
    /// The listing queries project the effective flags (entry OR journal),
    /// the rule `db::is_entry_visible_for_active_vault` applies.
    fn from(e: db::Entry) -> Self {
        Self {
            id: e.id,
            title: e.title,
            preview_text: e.preview_text,
            entry_date: e.entry_date,
            emotion: e.emotion,
            is_favorite: e.is_favorite,
            journal_id: e.journal_id,
            is_locked: e.is_locked,
            is_invisible: e.is_invisible,
        }
    }
}

/// One page of the entries list, newest first.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EntryPageFfi {
    pub items: Vec<EntryRowFfi>,
    /// Rows across every page.
    pub total: u64,
    /// Whether a later page has rows.
    pub has_more: bool,
}

/// One entry: the list row plus its plain text and its tags.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EntryDetailFfi {
    pub id: String,
    pub title: Option<String>,
    pub preview_text: Option<String>,
    /// Unix seconds.
    pub entry_date: i64,
    pub emotion: Option<String>,
    pub is_favorite: bool,
    pub journal_id: String,
    /// Effective, as on [`EntryRowFfi`]; true only while revealed.
    pub is_locked: bool,
    /// Effective, as on [`EntryRowFfi`]; true only while its vault is open.
    pub is_invisible: bool,
    pub content_text: Option<String>,
    pub tags: Vec<TagFfi>,
}

/// One row of Trash.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TrashRowFfi {
    pub id: String,
    pub title: Option<String>,
    pub preview_text: Option<String>,
    /// Unix seconds.
    pub trashed_at: i64,
    /// Effective, as on [`EntryRowFfi`].
    pub is_locked: bool,
    /// Effective, as on [`EntryRowFfi`].
    pub is_invisible: bool,
}

#[uniffi::export]
impl MemloreEngine {
    /// Page `page` of the entries list (1-based, 0 reads as 1), newest
    /// first: every listed journal, or only `journal_id`, which must be listed.
    pub fn list_entries(
        &self,
        journal_id: Option<String>,
        page: u32,
    ) -> Result<EntryPageFfi, MemloreError> {
        self.with_scope(|conn, scope| {
            let paged = match journal_id.as_deref() {
                Some(journal_id) => {
                    require_listed_journal(conn, scope, journal_id)?;
                    db::list_entries_paged_with_locked_view(
                        conn,
                        journal_id,
                        EntrySort::Newest,
                        EntryTimeRange::All,
                        None,
                        1,
                        page,
                        scope.view,
                        scope.vault.as_deref(),
                        LockFilter::All,
                    )
                    .map_err(|e| io(e.to_string()))?
                }
                None => list_page(conn, scope, page)?,
            };
            let total = u64::from(paged.total);
            Ok(EntryPageFfi {
                items: paged.items.into_iter().map(EntryRowFfi::from).collect(),
                total,
                has_more: u64::from(page.max(1)) * u64::from(db::PAGE_SIZE) < total,
            })
        })
    }

    /// One listed entry with its plain text and tags; anything the list does
    /// not show is `NotFound`, a redacted (Covered) one `SecondLockRequired`.
    pub fn get_entry(&self, id: String) -> Result<EntryDetailFfi, MemloreError> {
        self.with_scope(|conn, scope| {
            let e = require_listed_entry(conn, scope, &id)?;
            let tags = db::get_tags_for_entry(conn, &id).map_err(|e| io(e.to_string()))?;
            Ok(EntryDetailFfi {
                id: e.id,
                title: e.title,
                preview_text: e.preview_text,
                entry_date: e.entry_date,
                emotion: e.emotion,
                is_favorite: e.is_favorite,
                journal_id: e.journal_id,
                is_locked: e.is_locked,
                is_invisible: e.is_invisible,
                content_text: e.content_text,
                tags: tags.into_iter().map(TagFfi::from).collect(),
            })
        })
    }

    /// Create an entry with `title` and an empty body in `journal_id` (which
    /// must be listed), or else in the first visible, unlocked journal.
    /// Returns its id.
    pub fn create_entry(
        &self,
        title: String,
        journal_id: Option<String>,
    ) -> Result<String, MemloreError> {
        self.with_scope(|conn, scope| {
            let journal_id = match journal_id {
                Some(id) => require_listed_journal(conn, scope, &id)?.id,
                // `None`, not `scope.vault`, and unlocked in every view: an
                // entry never lands unasked in a vault's or a locked journal.
                None => db::list_journals(conn, None)
                    .map_err(|e| io(e.to_string()))?
                    .into_iter()
                    .find(|j| !j.is_locked)
                    .map(|j| j.id)
                    .ok_or_else(|| io("No journal to create the entry in".to_string()))?,
            };
            let title = Some(title.as_str()).filter(|t| !t.trim().is_empty());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let entry = create_entry_impl(conn, &journal_id, title, None, None, now).map_err(io)?;
            let (yjs, content_text, preview_text) =
                memlore_engine::yjs_doc::build_entry_yjs(&[""], &[]);
            save_entry_content_impl(conn, &entry.id, &yjs, &content_text, &preview_text)
                .map_err(io)?;
            Ok(entry.id)
        })
    }

    /// Set the title as typed, like the desktop `update_entry` (an empty
    /// string clears it), then the same post-save hooks.
    pub fn set_entry_title(&self, id: String, title: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            update_entry_impl(conn, &id, Some(&title), None, None)
                .map(drop)
                .map_err(io)
        })?;
        self.after_entry_save(&id)
    }

    /// Set the emotion (`good`, `neutral` or `bad`), or clear it with `None`.
    /// Any other value is a `Format` error.
    pub fn set_entry_emotion(
        &self,
        id: String,
        emotion: Option<String>,
    ) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            validate_emotion(emotion.as_deref()).map_err(format_err)?;
            update_entry_emotion_impl(conn, &id, emotion.as_deref())
                .map(drop)
                .map_err(io)
        })
    }

    /// Flip the favorite flag. Returns the new value.
    pub fn toggle_entry_favorite(&self, id: String) -> Result<bool, MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            toggle_favorite_impl(conn, &id).map_err(io)
        })
    }

    /// Set the entry date (unix seconds) and mark it user-edited, as the
    /// desktop date picker does with its two commands.
    pub fn set_entry_date(&self, id: String, entry_date: i64) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            update_entry_date_impl(conn, &id, entry_date).map_err(io)?;
            db::mark_entry_date_user_edited(conn, &id).map_err(|e| io(e.to_string()))
        })
    }

    /// Move a listed entry into another listed journal.
    pub fn move_entry(&self, id: String, journal_id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            require_listed_journal(conn, scope, &journal_id)?;
            move_entry_to_journal_impl(conn, &id, &journal_id).map_err(io)
        })
    }

    /// Move a listed entry to Trash. Its media stay until it is purged.
    pub fn trash_entry(&self, id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_entry(conn, scope, &id)?;
            soft_delete_entry_impl(conn, &id).map_err(io)
        })
    }

    /// Trash, most recently trashed first, for the session's view and open
    /// vault: Hidden leaves locked entries out, Covered lists them redacted.
    pub fn list_trash(&self) -> Result<Vec<TrashRowFfi>, MemloreError> {
        self.with_scope(|conn, scope| {
            let rows =
                list_trashed_entries_impl(conn, scope.view, scope.vault.as_deref()).map_err(io)?;
            Ok(rows
                .into_iter()
                .map(|e| TrashRowFfi {
                    id: e.id,
                    title: e.title,
                    preview_text: e.preview_text,
                    // The Trash query only returns rows with `trashed_at`.
                    trashed_at: e.trashed_at.unwrap_or_default(),
                    is_locked: e.is_locked,
                    is_invisible: e.is_invisible,
                })
                .collect())
        })
    }

    /// Bring an entry of [`Self::list_trash`] back; anything else is
    /// `NotFound`. A redacted (Covered) row can be restored: nothing is lost.
    pub fn restore_entry(&self, id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            restore_entry_impl(conn, &id, scope.view, scope.vault.as_deref()).map_err(trash_err)
        })
    }

    /// Delete an entry of [`Self::list_trash`] for good; anything else is
    /// `NotFound`. A locked entry only when revealed: the engine purges a
    /// Covered view as Hidden (`purge_view`). The impl unlinks its media
    /// files once the purge commits.
    // TODO(later): M2 must re-arm reset_session_own_cloud_reconciled after a purge (docs/LATER.md "Mobile M3a notes")
    pub fn purge_entry(&self, id: String) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            purge_entry_impl(conn, &id, scope.view, scope.vault.as_deref())
                .map(drop)
                .map_err(trash_err)
        })
    }

    /// Delete for good the confirmed `ids` that [`Self::list_trash`] still
    /// shows readably; any other id is skipped (a Covered locked row too, as
    /// for [`Self::purge_entry`]). Returns how many were deleted. The impl
    /// unlinks their media files once the purge commits.
    // TODO(later): M2 must re-arm reset_session_own_cloud_reconciled after a purge (docs/LATER.md "Mobile M3a notes")
    pub fn empty_trash(&self, ids: Vec<String>) -> Result<u32, MemloreError> {
        self.with_scope(|conn, scope| {
            let purged =
                empty_trash_impl(conn, &ids, scope.view, scope.vault.as_deref()).map_err(io)?;
            Ok(u32::try_from(purged).unwrap_or(u32::MAX))
        })
    }
}

/// The trash impls refuse an id this Trash view does not list (missing,
/// live, locked or invisible) with one message; that is `NotFound`.
// TODO(later): match a typed engine error instead of the message text
// (docs/LATER.md "Mobile M3a notes").
fn trash_err(e: String) -> MemloreError {
    if e == not_in_trash(rusqlite::Error::QueryReturnedNoRows) {
        MemloreError::NotFound
    } else {
        io(e)
    }
}
