use std::sync::Arc;

use tauri::State;

use crate::app::backend_errors::BackendErrorHub;
use crate::app::{AppState, BackendReadiness};
use crate::presentation::commands::helpers::log_command;
use crate::presentation::errors::CommandError;
use tt_application::dto::bootstrap_dto::BootstrapMetadataDto;
use tt_application::services::bootstrap_metadata::read_bootstrap_metadata;

#[tauri::command]
pub async fn get_bootstrap_metadata(
    app_state: State<'_, Arc<AppState>>,
) -> Result<BootstrapMetadataDto, CommandError> {
    log_command("get_bootstrap_metadata");

    let services = &app_state.services;
    read_bootstrap_metadata(
        &services.character_service,
        &services.group_service,
        &services.avatar_service,
        &services.secret_service,
        &app_state.ios_policy,
    )
    .await
    .map_err(CommandError::from)
}

#[tauri::command]
pub async fn backend_error_bridge_ready(
    backend_errors: State<'_, Arc<BackendErrorHub>>,
) -> Result<Vec<String>, CommandError> {
    log_command("backend_error_bridge_ready");
    Ok(backend_errors.mark_bridge_ready_and_drain())
}

#[tauri::command]
pub async fn wait_for_backend_ready(
    backend_readiness: State<'_, Arc<BackendReadiness>>,
) -> Result<(), CommandError> {
    log_command("wait_for_backend_ready");
    backend_readiness
        .wait_ready()
        .await
        .map_err(|error| CommandError::InternalServerError(error.to_string()))
}
