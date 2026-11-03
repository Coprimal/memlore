use tauri::State;

use crate::ops::invisible_lock::{
    change_invisible_vault_password_impl, open_or_create_invisible_vault_impl,
    remove_empty_invisible_vaults_impl, set_entry_invisible_impl, set_journal_invisible_impl,
};
use crate::AppState;

#[tauri::command]
pub fn open_or_create_invisible_vault(
    state: State<'_, AppState>,
    password: String,
) -> Result<String, String> {
    let conn = state.lock()?;
    open_or_create_invisible_vault_impl(&conn, &password)
}

#[tauri::command]
pub fn change_invisible_vault_password(
    state: State<'_, AppState>,
    vault_id: String,
    current: String,
    new: String,
) -> Result<String, String> {
    let conn = state.lock()?;
    change_invisible_vault_password_impl(&conn, &vault_id, &current, &new)
}

#[tauri::command]
pub fn remove_empty_invisible_vaults(state: State<'_, AppState>) -> Result<(), String> {
    let conn = state.lock()?;
    remove_empty_invisible_vaults_impl(&conn)
}

#[tauri::command]
pub fn set_entry_invisible(
    state: State<'_, AppState>,
    entry_id: String,
    invisible: bool,
    active_vault_id: Option<String>,
) -> Result<(), String> {
    let conn = state.lock()?;
    set_entry_invisible_impl(&conn, &entry_id, invisible, active_vault_id.as_deref())
}

#[tauri::command]
pub fn set_journal_invisible(
    state: State<'_, AppState>,
    journal_id: String,
    invisible: bool,
    active_vault_id: Option<String>,
) -> Result<(), String> {
    let conn = state.lock()?;
    set_journal_invisible_impl(&conn, &journal_id, invisible, active_vault_id.as_deref())
}
