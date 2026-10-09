use std::sync::Arc;

use tauri::State;

use crate::app::backend_errors::BackendErrorHub;
use crate::app::{AppState, BackendReadiness};
use crate::presentation::commands::helpers::log_command;
use crate::presentation::errors::CommandError;
use tt_application::dto::bootstrap_dto::BootstrapMetadataDto;
use tt_application::dto::group_dto::GroupDto;

#[tauri::command]
pub async fn get_bootstrap_metadata(
    app_state: State<'_, Arc<AppState>>,
) -> Result<BootstrapMetadataDto, CommandError> {
    log_command("get_bootstrap_metadata");

    let characters_fut = async {
        app_state
            .services
            .character_service
            .get_all_characters(true)
            .await
            .map_err(|error| {
                CommandError::InternalServerError(format!(
                    "Failed to load bootstrap characters snapshot: {error}"
                ))
            })
    };

    let groups_fut = async {
        app_state
            .services
            .group_service
            .get_all_groups()
            .await
            .map(|groups| groups.into_iter().map(GroupDto::from).collect())
            .map_err(|error| {
                CommandError::InternalServerError(format!(
                    "Failed to load bootstrap groups snapshot: {error}"
                ))
            })
    };

    let avatars_fut = async {
        app_state
            .services
            .avatar_service
            .get_avatars()
            .await
            .map_err(|error| {
                CommandError::InternalServerError(format!(
                    "Failed to load bootstrap avatars snapshot: {error}"
                ))
            })
    };

    let secret_state_fut = async {
        app_state
            .services
            .secret_service
            .read_secret_state()
            .await
            .map_err(|error| {
                CommandError::InternalServerError(format!(
                    "Failed to load bootstrap secret state snapshot: {error}"
                ))
            })
    };

    let (characters, groups, avatars, secret_state) =
        tokio::join!(characters_fut, groups_fut, avatars_fut, secret_state_fut);

    // This is a complete startup snapshot: a failed read is not an empty domain.
    // The caller owns failure presentation; do not also emit a backend-error toast.
    let characters = characters?;
    let groups = groups?;
    let avatars = avatars?;
    let secret_state = secret_state?;

    Ok(BootstrapMetadataDto {
        ios_policy: app_state.ios_policy.clone(),
        characters,
        groups,
        avatars,
        secret_state,
    })
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
