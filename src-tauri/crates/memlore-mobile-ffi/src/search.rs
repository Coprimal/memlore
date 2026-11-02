//! Keyword search of [`MemloreEngine`]: the desktop Search modal's query
//! (FTS5, accent-insensitive) with its basic filters. Like the lists, it uses
//! `LockedView::Hidden` and no invisible vault, so locked, invisible and
//! trashed entries are never returned.

use memlore_engine::db::{self, EmotionKey, LockedView, SearchFilters, TimeRangeFilter};

use crate::{format_err, io, MemloreEngine, MemloreError};

/// The search filters; an empty list means "no restriction".
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct SearchFiltersFfi {
    /// Unix seconds, inclusive. Applied only together with `to_exclusive`.
    pub from: Option<i64>,
    /// Unix seconds, exclusive. Applied only together with `from`.
    pub to_exclusive: Option<i64>,
    pub journal_ids: Vec<String>,
    pub tag_ids: Vec<String>,
    /// `good`, `neutral` or `bad`; anything else is a `Format` error.
    pub emotions: Vec<String>,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchResultFfi {
    pub id: String,
    pub journal_id: String,
    pub title: Option<String>,
    pub preview_text: Option<String>,
    /// Unix seconds.
    pub entry_date: i64,
}

impl From<db::SearchResult> for SearchResultFfi {
    fn from(r: db::SearchResult) -> Self {
        Self {
            id: r.id,
            journal_id: r.journal_id,
            title: r.title,
            preview_text: r.preview_text,
            entry_date: r.entry_date,
        }
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// At most 50 listed entries, like the desktop Search modal: a query
    /// ranks FTS matches; a blank query with a filter browses the newest
    /// matching entries; a blank query without a filter returns nothing.
    pub fn search_entries(
        &self,
        query: String,
        filters: SearchFiltersFfi,
    ) -> Result<Vec<SearchResultFfi>, MemloreError> {
        self.with_unlocked(|conn| {
            let filters = search_filters(filters)?;
            let query = query.trim();
            let rows = if !query.is_empty() {
                db::search_entries_with_locked_view(
                    conn,
                    query,
                    Some(&filters),
                    LockedView::Hidden,
                    None,
                    false,
                )
            } else if !filters.is_empty() {
                db::list_entries_with_filters_and_locked_view(
                    conn,
                    &filters,
                    LockedView::Hidden,
                    None,
                )
            } else {
                return Ok(Vec::new());
            };
            Ok(rows
                .map_err(|e| io(e.to_string()))?
                .into_iter()
                .map(SearchResultFfi::from)
                .collect())
        })
    }
}

/// The engine's filters; an unknown emotion is a `Format` error.
fn search_filters(f: SearchFiltersFfi) -> Result<SearchFilters, MemloreError> {
    let emotions = f
        .emotions
        .iter()
        .map(|e| match e.as_str() {
            "bad" => Ok(EmotionKey::Bad),
            "neutral" => Ok(EmotionKey::Neutral),
            "good" => Ok(EmotionKey::Good),
            _ => Err(format_err("emotion must be good, neutral or bad")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let time_range = match (f.from, f.to_exclusive) {
        (Some(from), Some(to_exclusive)) => Some(TimeRangeFilter { from, to_exclusive }),
        _ => None,
    };
    Ok(SearchFilters {
        time_range,
        journal_ids: Some(f.journal_ids),
        tag_ids: Some(f.tag_ids),
        emotions: Some(emotions),
        has_media: None,
    })
}
