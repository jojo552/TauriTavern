//! Media libraries and user uploads: backgrounds, avatars, user images and files, assets.
//!
//! Local upload paths are resolved through `HostFiles`; attachment references are confined
//! by the shared user media service and store. Pages cannot name arbitrary server paths.

use serde_json::Value;
use tt_application::dto::character_dto::{CreateWithAvatarDto, UpdateAvatarDto};
use tt_application::dto::sprite_dto::{UploadSpriteDto, UploadSpritePackDto};
use tt_domain::models::avatar::CropInfo;
use tt_domain::models::filename::sanitize_filename;
use tt_runtime::CoreServices;

use super::args::Args;
use super::documents::staged_path_string;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

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
        "upload_user_file" => json(
            host.media
                .upload_user_file(
                    &a.required::<String>("name")?,
                    &a.required::<String>("data_base64")?,
                )
                .await?,
        ),
        "delete_user_file" => json(
            host.media
                .delete_user_file(&a.required::<String>("path")?)
                .await?,
        ),
        "verify_user_files" => json(host.media.verify_user_files(a.required("urls")?).await?),
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
