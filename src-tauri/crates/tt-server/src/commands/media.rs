//! Media libraries and user uploads: backgrounds, avatars, user images and files, assets.
//!
//! Every file path these commands accept is a page-staged upload resolved through `HostFiles`;
//! the page can never name an arbitrary server path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Serialize;
use serde_json::Value;
use tokio::fs;
use tt_application::dto::character_dto::{CreateWithAvatarDto, UpdateAvatarDto};
use tt_application::dto::sprite_dto::{UploadSpriteDto, UploadSpritePackDto};
use tt_application::services::asset_service::validate_asset_file_name;
use tt_contracts::client_asset_paths::{UserDataAssetKind, parse_user_data_asset_request_path};
use tt_domain::models::avatar::CropInfo;
use tt_domain::models::filename::sanitize_filename;
use tt_runtime::CoreServices;
use url::Url;

use super::args::Args;
use super::documents::staged_path_string;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

/// Same wire shape as the native `upload_user_file` result.
#[derive(Serialize)]
struct UserFileUploadResult {
    path: String,
}

pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "upload_avatar"
            | "upload_background_from_path"
            | "create_character_with_avatar"
            | "update_avatar"
            | "upload_sprite"
            | "upload_sprite_pack"
            | "delete_user_file"
            | "sanitize_filename"
            | "upload_user_file"
            | "verify_user_files"
            | "download_asset"
    )
}

pub(super) async fn dispatch(
    host: &Host,
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        "upload_avatar" => {
            let path = host
                .files
                .validate_staged_path(&a.required::<String>("file_path")?)
                .await?;
            let crop = avatar_crop(a.optional("crop")?);
            json(
                s.avatar_service
                    .upload_avatar(&path, a.optional("overwrite_name")?, crop)
                    .await?,
            )
        }
        "upload_background_from_path" => {
            let path = host
                .files
                .validate_staged_path(&a.required::<String>("file_path")?)
                .await?;
            json(
                s.background_service
                    .upload_background_from_path(&a.required::<String>("filename")?, &path)
                    .await?,
            )
        }
        "create_character_with_avatar" => {
            let mut dto: CreateWithAvatarDto = a.required("dto")?;
            if let Some(raw) = &dto.avatar_path {
                dto.avatar_path = Some(staged_path_string(host, raw).await?);
            }
            json(s.character_service.create_with_avatar(dto).await?)
        }
        "update_avatar" => {
            let mut dto: UpdateAvatarDto = a.required("dto")?;
            dto.avatar_path = staged_path_string(host, &dto.avatar_path).await?;
            json(s.character_service.update_avatar(dto).await?)
        }
        "upload_sprite" => {
            let mut dto: UploadSpriteDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.sprite_service.upload(dto).await?)
        }
        "upload_sprite_pack" => {
            let mut dto: UploadSpritePackDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.sprite_service.upload_pack(dto).await?)
        }
        "sanitize_filename" => {
            let file_name: String = a.required("file_name")?;
            if file_name.is_empty() {
                return Err(CommandError::BadRequest("No fileName specified".into()));
            }
            json(sanitize_filename(&file_name))
        }
        "upload_user_file" => {
            let name = a.required::<String>("name")?.trim().to_owned();
            if name.is_empty() {
                return Err(CommandError::BadRequest("No upload name specified".into()));
            }
            // SillyTavern validates user-file names with its asset file-name contract.
            let name = validate_asset_file_name(&name)?;
            let bytes = BASE64_STANDARD
                .decode(a.required::<String>("data_base64")?.as_bytes())
                .map_err(|error| {
                    CommandError::BadRequest(format!("No upload data specified: {error}"))
                })?;
            let target = user_files_directory(s).await?.join(&name);
            fs::write(&target, &bytes).await.map_err(|error| {
                CommandError::InternalServerError(format!("Failed to save file: {error}"))
            })?;
            json(UserFileUploadResult {
                path: format!("/user/files/{name}"),
            })
        }
        "delete_user_file" => {
            let raw: String = a.required("path")?;
            if raw.trim().is_empty() {
                return Err(CommandError::BadRequest("No path specified".into()));
            }
            let relative = user_file_reference(&raw)
                .ok_or_else(|| CommandError::BadRequest("Invalid path".into()))?;
            let target = user_files_directory(s).await?.join(relative);
            if !is_file(&target).await {
                return Err(CommandError::NotFound("File not found".into()));
            }
            fs::remove_file(&target).await.map_err(|error| {
                CommandError::InternalServerError(format!("Failed to delete file: {error}"))
            })?;
            json(())
        }
        "verify_user_files" => {
            let urls: Vec<String> = a.required("urls")?;
            let files_dir = user_files_directory(s).await?;
            let mut result = HashMap::with_capacity(urls.len());
            for url in urls {
                // References outside /user/files/ are omitted from the answer, as natively.
                let Some(relative) = user_file_reference(&url) else {
                    continue;
                };
                let exists = is_file(&files_dir.join(relative)).await;
                result.insert(url, exists);
            }
            json(result)
        }
        "download_asset" => json(
            s.asset_service
                .download_asset(
                    &a.required::<String>("url")?,
                    &a.required::<String>("category")?,
                    &a.required::<String>("filename")?,
                )
                .await?,
        ),
        _ => Err(unsupported(command)),
    }
}

/// As the native command: an invalid crop is reported to the user and the image is uploaded uncropped.
fn avatar_crop(raw: Option<String>) -> Option<CropInfo> {
    serde_json::from_str(&raw?)
        .map_err(|error| {
            tracing::error!(
                target: tt_contracts::observability::USER_VISIBLE_ERROR,
                "Invalid avatar crop information: {error}"
            )
        })
        .ok()
}

async fn user_files_directory(s: &CoreServices) -> Result<PathBuf, CommandError> {
    let files_dir = PathBuf::from(
        s.user_directory_service
            .get_default_user_directory()
            .await?
            .files,
    );
    fs::create_dir_all(&files_dir).await.map_err(|error| {
        CommandError::InternalServerError(format!("Failed to ensure files directory: {error}"))
    })?;
    Ok(files_dir)
}

/// Maps a `/user/files/...` path or URL to its validated relative path, using the same parser
/// that serves those files, so a reference names exactly the file the page can load.
fn user_file_reference(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    let path = Url::parse(raw).map_or_else(|_| raw.to_owned(), |url| url.path().to_owned());
    let path = format!("/{}", path.replace('\\', "/").trim_start_matches('/'));
    match parse_user_data_asset_request_path(&path) {
        Ok(Some(parsed)) if parsed.kind == UserDataAssetKind::UserFile => {
            Some(parsed.relative_path)
        }
        _ => None,
    }
}

async fn is_file(path: &Path) -> bool {
    fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}
