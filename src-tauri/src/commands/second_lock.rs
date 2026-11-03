use tauri::State;

use crate::ops::second_lock::{
    change_second_lock_password_impl, disable_second_lock_impl, second_lock_status_impl,
    set_entry_locked_impl, set_journal_locked_impl, set_second_lock_password_impl,
    verify_second_lock_password_impl,
};
use crate::AppState;

#[tauri::command]
pub fn second_lock_status(state: State<'_, AppState>) -> Result<bool, String> {
    let conn = state.lock()?;
    second_lock_status_impl(&conn)
}

#[tauri::command]
pub fn set_second_lock_password(
    state: State<'_, AppState>,
    password: String,
) -> Result<(), String> {
    let conn = state.lock()?;
    set_second_lock_password_impl(&conn, &password)
}

#[tauri::command]
pub fn change_second_lock_password(
    state: State<'_, AppState>,
    old_password: String,
    new_password: String,
) -> Result<(), String> {
    let conn = state.lock()?;
    change_second_lock_password_impl(&conn, &old_password, &new_password)
}

#[tauri::command]
pub fn verify_second_lock_password(
    state: State<'_, AppState>,
    password: String,
) -> Result<bool, String> {
    let conn = state.lock()?;
    verify_second_lock_password_impl(&conn, &password)
}

#[tauri::command]
pub fn disable_second_lock(state: State<'_, AppState>, password: String) -> Result<(), String> {
    let conn = state.lock()?;
    disable_second_lock_impl(&conn, &password)
}

#[tauri::command]
pub fn set_entry_locked(
    state: State<'_, AppState>,
    entry_id: String,
    locked: bool,
) -> Result<(), String> {
    let conn = state.lock()?;
    set_entry_locked_impl(&conn, &entry_id, locked)
}

#[tauri::command]
pub fn set_journal_locked(
    state: State<'_, AppState>,
    journal_id: String,
    locked: bool,
) -> Result<(), String> {
    let conn = state.lock()?;
    set_journal_locked_impl(&conn, &journal_id, locked)
}
