//! Second-lock and invisible-vault calls of [`MemloreEngine`]. They call the
//! engine's `ops::second_lock` and `ops::invisible_lock` (the desktop
//! commands' impls) and keep the reveal and the open vault in the Rust-held
//! lock session, which JS never sees. Engine errors are mapped by equality
//! with their contract strings. Invisible-vault refusals stay the generic
//! "Unable to update" (deniability): only a short password is told apart.

use memlore_engine::db::{self, LockedView};
use memlore_engine::ops::invisible_lock::{
    change_invisible_vault_password_impl, invisible_lock_password_too_short_message,
    open_or_create_invisible_vault_impl, remove_empty_invisible_vaults_impl,
    set_entry_invisible_impl, set_journal_invisible_impl, validate_invisible_lock_password,
    UNABLE_TO_UPDATE,
};
use memlore_engine::ops::second_lock::{
    change_second_lock_password_impl, disable_second_lock_impl, second_lock_status_impl,
    set_entry_locked_impl, set_journal_locked_impl, set_second_lock_password_impl,
    validate_second_lock_password, verify_second_lock_password_impl, INVALID_SECOND_LOCK_PASSWORD,
};
use zeroize::Zeroizing;

use crate::{
    format_err, io, require_listed_journal, require_visible_entry, MemloreEngine, MemloreError,
    SHOW_EXISTENCE_SETTING,
};

/// Minutes of inactivity before the reveal ends (synced, desktop encoding).
const SECOND_LOCK_MINUTES_SETTING: &str = "second_lock_auto_lock_minutes";
/// Minutes of inactivity before the invisible vault closes (synced).
const INVISIBLE_MINUTES_SETTING: &str = "invisible_lock_auto_lock_minutes";
/// 0 means never.
const AUTO_RELOCK_CHOICES: [u32; 5] = [0, 1, 5, 15, 30];
/// The default, and what any missing or unexpected stored value reads as.
const AUTO_RELOCK_DEFAULT: u32 = 5;

/// The lock session and its settings. Never carries the open vault's id.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LockStateFfi {
    /// A second-lock password is set.
    pub enabled: bool,
    /// Locked content is readable now: the session is revealed AND the
    /// second lock is enabled.
    pub revealed: bool,
    /// Locked rows are listed redacted (Covered) instead of left out.
    pub show_existence: bool,
    /// One of 0 (never), 1, 5, 15 or 30.
    pub auto_lock_minutes: u32,
    /// One of 0 (never), 1, 5, 15 or 30.
    pub invisible_auto_lock_minutes: u32,
    /// An invisible vault is open.
    pub vault_open: bool,
}

#[uniffi::export]
impl MemloreEngine {
    /// The lock session and its settings. The minutes fail closed: a
    /// missing, unparsable or unexpected stored value reads as 5. A disabled
    /// second lock ends the session's reveal (as [`Self::with_scope`] does).
    pub fn lock_state(&self) -> Result<LockStateFfi, MemloreError> {
        self.with_unlocked(|conn| {
            let enabled = second_lock_status_impl(conn).map_err(io)?;
            let show_existence = db::get_setting(conn, SHOW_EXISTENCE_SETTING)
                .map_err(|e| io(e.to_string()))?
                .is_some_and(|v| v == "true");
            let auto_lock_minutes = relock_minutes(conn, SECOND_LOCK_MINUTES_SETTING)?;
            let invisible_auto_lock_minutes = relock_minutes(conn, INVISIBLE_MINUTES_SETTING)?;
            let (revealed, vault_open) = match self.lock_session.lock() {
                Ok(mut s) => {
                    if !enabled {
                        s.revealed = false;
                    }
                    (s.revealed, s.vault_id.is_some())
                }
                Err(_) => (false, false),
            };
            Ok(LockStateFfi {
                enabled,
                revealed: revealed && enabled,
                show_existence,
                auto_lock_minutes,
                invisible_auto_lock_minutes,
                vault_open,
            })
        })
    }

    /// Set the first second-lock password (`Format` when too short) and
    /// reveal, as the desktop does after setup.
    pub fn set_second_lock_password(&self, password: String) -> Result<(), MemloreError> {
        let password = Zeroizing::new(password);
        self.with_unlocked(|conn| {
            validate_second_lock_password(&password).map_err(format_err)?;
            set_second_lock_password_impl(conn, &password).map_err(second_lock_err)?;
            self.update_lock_session(|s| s.revealed = true);
            Ok(())
        })
    }

    /// Reveal locked content with the second-lock password. `false` (wrong
    /// password, or no second lock) changes nothing.
    pub fn unlock_second_lock(&self, password: String) -> Result<bool, MemloreError> {
        let password = Zeroizing::new(password);
        self.with_unlocked(|conn| {
            let ok = verify_second_lock_password_impl(conn, &password).map_err(io)?;
            if ok {
                self.update_lock_session(|s| s.revealed = true);
            }
            Ok(ok)
        })
    }

    /// End the reveal. No gate: it only hides.
    pub fn lock_second_lock(&self) {
        self.update_lock_session(|s| s.revealed = false);
    }

    /// Change the second-lock password: a wrong old one is `WrongPassword`,
    /// a short new one `Format`.
    pub fn change_second_lock_password(
        &self,
        old_password: String,
        new_password: String,
    ) -> Result<(), MemloreError> {
        let old_password = Zeroizing::new(old_password);
        let new_password = Zeroizing::new(new_password);
        self.with_unlocked(|conn| {
            validate_second_lock_password(&new_password).map_err(format_err)?;
            change_second_lock_password_impl(conn, &old_password, &new_password)
                .map_err(second_lock_err)
        })
    }

    /// Remove the second-lock password and every lock flag (desktop
    /// semantics), then end the whole lock session (the reveal and the open
    /// vault; README re-lock rules). A wrong password is `WrongPassword`.
    pub fn disable_second_lock(&self, password: String) -> Result<(), MemloreError> {
        let password = Zeroizing::new(password);
        self.with_unlocked(|conn| {
            disable_second_lock_impl(conn, &password).map_err(second_lock_err)?;
            self.end_lock_session();
            Ok(())
        })
    }

    /// Show locked rows redacted (Covered) or leave them out (Hidden).
    /// Turning it on would show that locked rows exist, so with the second
    /// lock enabled it needs the reveal (`SecondLockRequired`); turning it
    /// off only hides and is always allowed.
    pub fn set_second_lock_show_existence(&self, show: bool) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            if show && scope.enabled && scope.view != LockedView::Revealed {
                return Err(MemloreError::SecondLockRequired);
            }
            db::set_setting(
                conn,
                SHOW_EXISTENCE_SETTING,
                if show { "true" } else { "false" },
            )
            .map_err(|e| io(e.to_string()))
        })
    }

    /// Minutes of inactivity before the reveal ends: 0 (never), 1, 5, 15 or
    /// 30, else `Format`. With the second lock enabled and not revealed it
    /// may only shorten (`SecondLockRequired` otherwise), so nobody can make
    /// a later reveal last for ever.
    pub fn set_second_lock_auto_lock_minutes(&self, minutes: u32) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            let free = !scope.enabled || scope.view == LockedView::Revealed;
            set_relock_minutes(conn, SECOND_LOCK_MINUTES_SETTING, minutes, free)
                .map_err(|e| e.unwrap_or(MemloreError::SecondLockRequired))
        })
    }

    /// Minutes of inactivity before the invisible vault closes: 0 (never),
    /// 1, 5, 15 or 30, else `Format`. With no vault open it may only shorten
    /// (the generic "Unable to update" otherwise).
    pub fn set_invisible_auto_lock_minutes(&self, minutes: u32) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            let free = scope.vault.is_some();
            set_relock_minutes(conn, INVISIBLE_MINUTES_SETTING, minutes, free)
                .map_err(|e| e.unwrap_or_else(unable_to_update))
        })
    }

    /// Lock or unlock an entry the session can address. Unlocking a locked
    /// one needs the reveal; engine refusals (second lock not enabled, entry
    /// in Trash) keep their text.
    pub fn set_entry_locked(&self, entry_id: String, locked: bool) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_visible_entry(conn, scope, &entry_id)?;
            set_entry_locked_impl(conn, &entry_id, locked).map_err(io)
        })
    }

    /// Lock or unlock a listed journal, with the rules of
    /// [`Self::set_entry_locked`].
    pub fn set_journal_locked(&self, journal_id: String, locked: bool) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_journal(conn, scope, &journal_id)?;
            set_journal_locked_impl(conn, &journal_id, locked).map_err(io)
        })
    }

    /// Open the vault `password` opens, or create an empty one: the same
    /// result either way (deniability). A short password is `Format`, any
    /// other refusal the generic "Unable to update".
    pub fn open_invisible_vault(&self, password: String) -> Result<(), MemloreError> {
        let password = Zeroizing::new(password);
        self.with_unlocked(|conn| {
            validate_invisible_lock_password(&password).map_err(format_err)?;
            let id = open_or_create_invisible_vault_impl(conn, &password).map_err(invisible_err)?;
            self.update_lock_session(|s| s.vault_id = Some(id));
            Ok(())
        })
    }

    /// Close the open invisible vault. No gate: it only hides.
    pub fn close_invisible_vault(&self) {
        self.update_lock_session(|s| s.vault_id = None);
    }

    /// Change the open vault's password. When the new one opens another
    /// vault, this one is merged into it and the session follows. A short new
    /// password is `Format`; every other refusal (no vault open, wrong
    /// password) the generic "Unable to update".
    pub fn change_invisible_vault_password(
        &self,
        old_password: String,
        new_password: String,
    ) -> Result<(), MemloreError> {
        let old_password = Zeroizing::new(old_password);
        let new_password = Zeroizing::new(new_password);
        self.with_scope(|conn, scope| {
            validate_invisible_lock_password(&new_password).map_err(format_err)?;
            let vault_id = scope.vault.as_deref().ok_or_else(unable_to_update)?;
            let id =
                change_invisible_vault_password_impl(conn, vault_id, &old_password, &new_password)
                    .map_err(invisible_err)?;
            self.update_lock_session(|s| s.vault_id = Some(id));
            Ok(())
        })
    }

    /// Delete every invisible vault that holds nothing, and close the open
    /// one if it was among them.
    pub fn remove_empty_invisible_vaults(&self) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            remove_empty_invisible_vaults_impl(conn).map_err(invisible_err)?;
            if let Some(vault_id) = scope.vault.as_deref() {
                let kept = db::get_invisible_vault(conn, vault_id)
                    .map_err(|_| unable_to_update())?
                    .is_some();
                if !kept {
                    self.update_lock_session(|s| s.vault_id = None);
                }
            }
            Ok(())
        })
    }

    /// Move an entry the session can address into the open vault, or out of
    /// it. Making it invisible with no vault open is the generic "Unable to
    /// update"; the engine clears its lock flag.
    pub fn set_entry_invisible(
        &self,
        entry_id: String,
        invisible: bool,
    ) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_visible_entry(conn, scope, &entry_id)?;
            set_entry_invisible_impl(conn, &entry_id, invisible, scope.vault.as_deref()).map_err(io)
        })
    }

    /// Move a listed journal (and its entries) into the open vault, or out
    /// of it, with the rules of [`Self::set_entry_invisible`].
    pub fn set_journal_invisible(
        &self,
        journal_id: String,
        invisible: bool,
    ) -> Result<(), MemloreError> {
        self.with_scope(|conn, scope| {
            require_listed_journal(conn, scope, &journal_id)?;
            set_journal_invisible_impl(conn, &journal_id, invisible, scope.vault.as_deref())
                .map_err(io)
        })
    }
}

/// A stored re-lock interval, or the default for anything unexpected.
fn relock_minutes(conn: &rusqlite::Connection, key: &str) -> Result<u32, MemloreError> {
    Ok(db::get_setting(conn, key)
        .map_err(|e| io(e.to_string()))?
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|m| AUTO_RELOCK_CHOICES.contains(m))
        .unwrap_or(AUTO_RELOCK_DEFAULT))
}

/// Store a re-lock interval. Unless `free`, only one no longer than the
/// current (effective) value is stored, 0 (never) counting as the longest;
/// a longer one is `Err(None)`, for the caller's own refusal.
fn set_relock_minutes(
    conn: &rusqlite::Connection,
    key: &str,
    minutes: u32,
    free: bool,
) -> Result<(), Option<MemloreError>> {
    if !AUTO_RELOCK_CHOICES.contains(&minutes) {
        return Err(Some(format_err(format!(
            "re-lock must be 0, 1, 5, 15 or 30 minutes, not {minutes}"
        ))));
    }
    if !free {
        let current = relock_minutes(conn, key)?;
        if minutes == 0 || (current != 0 && minutes > current) {
            return Err(None);
        }
    }
    db::set_setting(conn, key, &minutes.to_string()).map_err(|e| Some(io(e.to_string())))
}

/// The wrong-password contract text is `WrongPassword`; any other engine
/// text is kept.
fn second_lock_err(e: String) -> MemloreError {
    if e == INVALID_SECOND_LOCK_PASSWORD {
        MemloreError::WrongPassword
    } else {
        io(e)
    }
}

/// A short password is `Format`; everything else collapses to the generic
/// error so nothing tells whether a vault exists.
fn invisible_err(e: String) -> MemloreError {
    if e == invisible_lock_password_too_short_message() {
        format_err(e)
    } else {
        unable_to_update()
    }
}

fn unable_to_update() -> MemloreError {
    io(UNABLE_TO_UPDATE.to_string())
}
