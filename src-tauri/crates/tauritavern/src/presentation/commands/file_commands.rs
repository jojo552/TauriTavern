use std::collections::HashMap;
use std::sync::Arc;

use tauri::State;
use tt_application::services::user_media_service::{UserFileUploadResult, UserMediaService};
use tt_domain::models::filename::sanitize_filename as sanitize_filename_contract;

use crate::presentation::commands::helpers::log_command;
use crate::presentation::errors::CommandError;

#[tauri::command]
pub async fn sanitize_filename(file_name: String) -> Result<String, CommandError> {
    log_command(format!("sanitize_filename {}", file_name));

    if file_name.is_empty() {
        return Err(CommandError::BadRequest(
            "No fileName specified".to_string(),
        ));
    }

    Ok(sanitize_filename_contract(&file_name))
}

#[tauri::command]
pub async fn upload_user_file(
    name: String,
    data_base64: String,
    user_media: State<'_, Arc<UserMediaService>>,
) -> Result<UserFileUploadResult, CommandError> {
    log_command(format!("upload_user_file {}", name));
    Ok(user_media.upload_user_file(&name, &data_base64).await?)
}

#[tauri::command]
pub async fn delete_user_file(
    path: String,
    user_media: State<'_, Arc<UserMediaService>>,
) -> Result<(), CommandError> {
    log_command(format!("delete_user_file {}", path));
    Ok(user_media.delete_user_file(&path).await?)
}

#[tauri::command]
pub async fn verify_user_files(
    urls: Vec<String>,
    user_media: State<'_, Arc<UserMediaService>>,
) -> Result<HashMap<String, bool>, CommandError> {
    log_command(format!("verify_user_files {}", urls.len()));
    Ok(user_media.verify_user_files(urls).await?)
}
