//! Template calls of [`MemloreEngine`], the desktop template commands with
//! the same `db::*` calls. User templates are name and description only (no
//! content), as on desktop; predefined ones are read-only.

use memlore_engine::db;

use crate::{format_err, io, MemloreEngine, MemloreError};

/// One template. Predefined ones carry a slug as `name` (`blank`,
/// `daily-reflection`, `morning-pages`); their text comes from i18n.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TemplateFfi {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// HTML synced from another device, `None` when absent or not UTF-8.
    pub content_html: Option<String>,
    pub is_predefined: bool,
    pub sort_order: i64,
}

impl From<db::Template> for TemplateFfi {
    fn from(t: db::Template) -> Self {
        Self {
            id: t.id,
            name: t.name,
            description: t.description,
            content_html: t.content.and_then(|bytes| String::from_utf8(bytes).ok()),
            is_predefined: t.is_predefined,
            sort_order: t.sort_order,
        }
    }
}

#[uniffi::export]
impl MemloreEngine {
    /// Predefined templates first (in their order), then user templates.
    pub fn list_templates(&self) -> Result<Vec<TemplateFfi>, MemloreError> {
        self.with_unlocked(|conn| {
            Ok(db::list_templates(conn)
                .map_err(|e| io(e.to_string()))?
                .into_iter()
                .map(TemplateFfi::from)
                .collect())
        })
    }

    /// Create a user template named `name`, trimmed (empty is a `Format`
    /// error), with no content.
    pub fn create_template(
        &self,
        name: String,
        description: Option<String>,
    ) -> Result<TemplateFfi, MemloreError> {
        self.with_unlocked(|conn| {
            let name = template_name(&name)?;
            db::create_template(conn, name, description.as_deref(), None)
                .map(TemplateFfi::from)
                .map_err(template_err)
        })
    }

    /// Rename and redescribe a user template. Desktop parity: its content is
    /// cleared. A predefined or unknown template is `NotFound`.
    pub fn update_template(
        &self,
        id: String,
        name: String,
        description: Option<String>,
    ) -> Result<TemplateFfi, MemloreError> {
        self.with_unlocked(|conn| {
            let name = template_name(&name)?;
            require_user_template(conn, &id)?;
            db::update_template(conn, &id, name, description.as_deref(), None)
                .map(TemplateFfi::from)
                .map_err(template_err)
        })
    }

    /// Delete a user template. A predefined or unknown template is
    /// `NotFound`.
    pub fn delete_template(&self, id: String) -> Result<(), MemloreError> {
        self.with_unlocked(|conn| {
            require_user_template(conn, &id)?;
            db::delete_template(conn, &id).map_err(template_err)
        })
    }
}

/// A template name, trimmed; empty is a `Format` error.
fn template_name(name: &str) -> Result<&str, MemloreError> {
    let name = name.trim();
    if name.is_empty() {
        Err(format_err("template name must not be empty"))
    } else {
        Ok(name)
    }
}

/// A live user template; a predefined, deleted or unknown id is `NotFound`.
fn require_user_template(conn: &rusqlite::Connection, id: &str) -> Result<(), MemloreError> {
    match db::get_template(conn, id).map_err(|e| io(e.to_string()))? {
        Some(t) if !t.is_predefined => Ok(()),
        _ => Err(MemloreError::NotFound),
    }
}

/// The size caps raise `InvalidParameterName`: that is the caller's input.
fn template_err(e: rusqlite::Error) -> MemloreError {
    match e {
        rusqlite::Error::InvalidParameterName(msg) => format_err(msg),
        rusqlite::Error::QueryReturnedNoRows => MemloreError::NotFound,
        other => io(other.to_string()),
    }
}
