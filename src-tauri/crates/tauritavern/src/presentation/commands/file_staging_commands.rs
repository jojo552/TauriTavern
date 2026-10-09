use std::sync::Arc;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tt_adapter_storage_core::staged_file;

use crate::app::AppState;
use crate::infrastructure::staging::{create_staged_file, discard_file, validate_staged_path};
use crate::platform::ipc::{SMALL_ASSET_UPLOAD_CHUNK_BYTES, UPLOAD_CHUNK_BYTES};
use crate::presentation::commands::chunk_body::chunk_bytes_from_request;
use crate::presentation::commands::helpers::{ensure_ios_policy_allows, log_command};
use crate::presentation::errors::CommandError;

const DEFAULT_KIND: &str = "generic";
const HEADER_FILE_PATH: &str = "file-path";
const HEADER_OFFSET: &str = "offset";

#[derive(Debug, Deserialize)]
pub struct StageFileBeginDto {
    pub kind: Option<String>,
    pub preferred_extension: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct StageFileBeginResult {
    pub file_path: String,
    pub chunk_size: u64,
}

#[derive(Debug, Serialize)]
pub struct StageFileFinishResult {
    pub file_path: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct StageFileFromUrlResult {
    pub file_path: String,
    pub mime_type: Option<String>,
}

fn required_header(request: &tauri::ipc::Request<'_>, name: &str) -> Result<String, CommandError> {
    request
        .headers()
        .get(name)
        .ok_or_else(|| CommandError::BadRequest(format!("Missing file staging header: {name}")))?
        .to_str()
        .map(str::to_string)
        .map_err(|_| CommandError::BadRequest(format!("Invalid file staging header: {name}")))
}

fn chunk_file_path_from_request(request: &tauri::ipc::Request<'_>) -> Result<String, CommandError> {
    let encoded = required_header(request, HEADER_FILE_PATH)?;
    percent_decode_str(&encoded)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| {
            CommandError::BadRequest("File staging file path header is invalid".to_string())
        })
}

fn chunk_offset_from_request(request: &tauri::ipc::Request<'_>) -> Result<u64, CommandError> {
    let offset = required_header(request, HEADER_OFFSET)?;
    offset
        .parse::<u64>()
        .map_err(|_| CommandError::BadRequest("File staging offset header is invalid".to_string()))
}

#[tauri::command]
pub async fn stage_file_begin(
    app: AppHandle,
    dto: StageFileBeginDto,
) -> Result<StageFileBeginResult, CommandError> {
    let kind = dto
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .unwrap_or(DEFAULT_KIND);
    let extension = dto
        .preferred_extension
        .as_deref()
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .unwrap_or("bin");
    log_command(format!(
        "stage_file_begin kind={} size={}",
        kind,
        dto.size.unwrap_or(0)
    ));

    let file_path = create_staged_file(&app, kind, extension).await?;

    Ok(StageFileBeginResult {
        file_path: file_path.to_string_lossy().to_string(),
        chunk_size: match kind {
            "avatar" | "user-avatar" | "worldinfo-import" => SMALL_ASSET_UPLOAD_CHUNK_BYTES,
            _ => UPLOAD_CHUNK_BYTES,
        },
    })
}

#[tauri::command]
pub async fn stage_file_chunk(
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<u64, CommandError> {
    let file_path = chunk_file_path_from_request(&request)?;
    let offset = chunk_offset_from_request(&request)?;
    let data = chunk_bytes_from_request(&request)?;
    let path = validate_staged_path(&app, &file_path).await?;
    staged_file::append(&path, offset, &data).await.map_err(CommandError::from)
}

#[tauri::command]
pub async fn stage_file_finish(
    app: AppHandle,
    file_path: String,
    expected_size: u64,
) -> Result<StageFileFinishResult, CommandError> {
    let path = validate_staged_path(&app, &file_path).await?;
    let size = staged_file::finish(&path, expected_size).await?;

    Ok(StageFileFinishResult {
        file_path: path.to_string_lossy().to_string(),
        size,
    })
}

/// Stages a remote file the page cannot read; the frontend then delivers it like any staged file.
#[tauri::command]
pub async fn stage_file_from_url(
    app: AppHandle,
    url: String,
    app_state: State<'_, Arc<AppState>>,
) -> Result<StageFileFromUrlResult, CommandError> {
    log_command("stage_file_from_url");
    ensure_ios_policy_allows(
        &app_state.ios_policy,
        app_state.ios_policy.capabilities.content.external_import,
        "content.external_import",
    )?;
    let path = create_staged_file(&app, "remote", "bin").await?;
    match app_state
        .services
        .content_service
        .download_external_file(&url, &path)
        .await
    {
        Ok(file) => Ok(StageFileFromUrlResult {
            file_path: path.to_string_lossy().into_owned(),
            mime_type: file.content_type,
        }),
        Err(error) => {
            if let Err(cleanup_error) = discard_file(&path).await {
                tracing::warn!(%cleanup_error, "Failed to discard incomplete remote download");
            }
            Err(error.into())
        }
    }
}

#[tauri::command]
pub async fn stage_file_discard(app: AppHandle, file_path: String) -> Result<(), CommandError> {
    let path = validate_staged_path(&app, &file_path).await?;
    discard_file(&path).await.map_err(CommandError::from)
}
