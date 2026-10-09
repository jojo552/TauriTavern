use super::{args::Args, json, unsupported};
use crate::error::CommandError;
use serde_json::{Value, json as value};
use tt_application::dto::{
    background_dto::*, preset_dto::*, secret_dto::*, theme_dto::*, world_info_dto::*,
};
use tt_domain::models::preset::PresetType;
use tt_runtime::CoreServices;

pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "normalize_world_info_name"
            | "delete_world_info"
            | "save_preset"
            | "delete_preset"
            | "restore_preset"
            | "save_openai_preset"
            | "delete_openai_preset"
            | "list_presets"
            | "preset_exists"
            | "get_preset"
            | "save_theme"
            | "delete_theme"
            | "save_quick_reply_set"
            | "delete_quick_reply_set"
            | "get_extensions"
            | "write_secret"
            | "read_secret_state"
            | "read_secret_settings"
            | "view_secrets"
            | "find_secret"
            | "delete_secret"
            | "rotate_secret"
            | "rename_secret"
            | "get_personas"
            | "get_avatars"
            | "delete_avatar"
            | "get_all_backgrounds"
            | "get_all_background_metadata"
            | "delete_background"
            | "rename_background"
            | "upload_background"
            | "list_sprites"
            | "delete_sprite"
            | "initialize_default_content"
            | "is_default_content_initialized"
            | "get_assets_library"
            | "delete_asset"
            | "get_character_assets"
    )
}

pub(super) async fn dispatch(
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        "normalize_world_info_name" => {
            let dto: NormalizeWorldInfoNameDto = a.required("dto")?;
            json(NormalizeWorldInfoNameResponseDto {
                name: s
                    .world_info_service
                    .normalize_world_info_name(&dto.name, dto.import_filename)?,
            })
        }
        "delete_world_info" => {
            let dto: DeleteWorldInfoDto = a.required("dto")?;
            json(s.world_info_service.delete_world_info(&dto.name).await?)
        }
        "save_preset" => {
            let dto: SavePresetDto = a.required("dto")?;
            save_preset(s, dto.name, &dto.api_id, dto.preset).await
        }
        "save_openai_preset" => {
            let dto: SaveOpenAIPresetDto = a.required("dto")?;
            save_preset(s, a.required("name")?, "openai", dto.preset).await
        }
        "delete_preset" => {
            let dto: DeletePresetDto = a.required("dto")?;
            delete_preset(s, &dto.name, &preset_type(&dto.api_id)?).await?;
            json(())
        }
        "delete_openai_preset" => {
            let dto: DeleteOpenAIPresetDto = a.required("dto")?;
            nonempty_name(&dto.name)?;
            // This existing specialized route deliberately represents deletion failure as {error:true}.
            let response = match delete_preset(s, &dto.name, &PresetType::OpenAI).await {
                Ok(()) => DeleteOpenAIPresetResponseDto::success(),
                Err(error) => {
                    tracing::error!(%error, "Failed to delete OpenAI preset or linked Agent Skills");
                    DeleteOpenAIPresetResponseDto::error()
                }
            };
            json(response)
        }
        "restore_preset" => {
            let dto: RestorePresetDto = a.required("dto")?;
            nonempty_name(&dto.name)?;
            json(
                match s
                    .preset_service
                    .restore_default_preset(&dto.name, &preset_type(&dto.api_id)?)
                    .await?
                {
                    Some(preset) => RestorePresetResponseDto::new(true, preset.data),
                    None => RestorePresetResponseDto::not_found(),
                },
            )
        }
        "list_presets" => json(
            s.preset_service
                .list_presets(&preset_type(&a.required::<String>("api_id")?)?)
                .await?,
        ),
        "preset_exists" => json(
            s.preset_service
                .preset_exists(
                    &a.required::<String>("name")?,
                    &preset_type(&a.required::<String>("api_id")?)?,
                )
                .await?,
        ),
        "get_preset" => json(
            s.preset_service
                .get_preset(
                    &a.required::<String>("name")?,
                    &preset_type(&a.required::<String>("api_id")?)?,
                )
                .await?
                .map(|preset| preset.data_with_name()),
        ),
        "save_theme" => {
            let dto: SaveThemeDto = a.required("dto")?;
            json(s.theme_service.save_theme(&dto.name, dto.data).await?)
        }
        "delete_theme" => {
            let dto: DeleteThemeDto = a.required("dto")?;
            json(s.theme_service.delete_theme(&dto.name).await?)
        }
        "save_quick_reply_set" => json(
            s.quick_reply_service
                .save_quick_reply_set(a.required("payload")?)
                .await?,
        ),
        "delete_quick_reply_set" => json(
            s.quick_reply_service
                .delete_quick_reply_set(a.required("payload")?)
                .await?,
        ),
        "get_extensions" => json(s.extension_service.get_extensions().await?),
        "write_secret" => {
            let dto: WriteSecretDto = a.required("dto")?;
            json(
                s.secret_service
                    .write_secret(&dto.key, &dto.value, dto.label.as_deref())
                    .await?,
            )
        }
        "read_secret_state" => json(s.secret_service.read_secret_state().await?),
        "read_secret_settings" => json(s.secret_service.read_settings()),
        "view_secrets" => json(s.secret_service.view_secrets().await?),
        "find_secret" => {
            let dto: FindSecretDto = a.required("dto")?;
            json(
                s.secret_service
                    .find_secret(&dto.key, dto.id.as_deref())
                    .await?,
            )
        }
        "delete_secret" => {
            let dto: DeleteSecretDto = a.required("dto")?;
            json(
                s.secret_service
                    .delete_secret(&dto.key, dto.id.as_deref())
                    .await?,
            )
        }
        "rotate_secret" => {
            let dto: RotateSecretDto = a.required("dto")?;
            json(s.secret_service.rotate_secret(&dto.key, &dto.id).await?)
        }
        "rename_secret" => {
            let dto: RenameSecretDto = a.required("dto")?;
            json(
                s.secret_service
                    .rename_secret(&dto.key, &dto.id, &dto.label)
                    .await?,
            )
        }
        "get_personas" => json(s.avatar_service.get_personas().await?),
        "get_avatars" => json(s.avatar_service.get_avatars().await?),
        "delete_avatar" => json(
            s.avatar_service
                .delete_avatar(&a.required::<String>("avatar")?)
                .await?,
        ),
        "get_all_backgrounds" => json(
            s.image_metadata_service
                .get_background_list_entries()
                .await?,
        ),
        "get_all_background_metadata" => json(
            s.image_metadata_service
                .get_all_background_metadata(a.optional::<String>("prefix")?.as_deref())
                .await?,
        ),
        "delete_background" => {
            let dto: DeleteBackgroundDto = a.required("dto")?;
            json(s.background_service.delete_background(&dto.bg).await?)
        }
        "rename_background" => {
            let dto: RenameBackgroundDto = a.required("dto")?;
            json(
                s.background_service
                    .rename_background(&dto.old_bg, &dto.new_bg)
                    .await?,
            )
        }
        "upload_background" => json(
            s.background_service
                .upload_background(
                    &a.required::<String>("filename")?,
                    &a.required::<Vec<u8>>("data")?,
                )
                .await?,
        ),
        "list_sprites" => json(s.sprite_service.list(a.required("dto")?).await?),
        "delete_sprite" => json(s.sprite_service.delete(a.required("dto")?).await?),
        "initialize_default_content" => json(
            s.content_service
                .initialize_default_content("default-user")
                .await?,
        ),
        "is_default_content_initialized" => json(
            s.content_service
                .is_default_content_initialized("default-user")
                .await?,
        ),
        "get_assets_library" => json(s.asset_service.list_assets().await?),
        "delete_asset" => json(
            s.asset_service
                .delete_asset_file(
                    &a.required::<String>("category")?,
                    &a.required::<String>("filename")?,
                )
                .await?,
        ),
        "get_character_assets" => json(
            s.asset_service
                .list_character_assets(
                    &a.required::<String>("name")?,
                    &a.required::<String>("category")?,
                )
                .await?,
        ),
        _ => Err(unsupported(command)),
    }
}

fn preset_type(api_id: &str) -> Result<PresetType, CommandError> {
    PresetType::from_api_id(api_id)
        .ok_or_else(|| CommandError::BadRequest(format!("Unknown API ID: {api_id}")))
}
fn nonempty_name(name: &str) -> Result<(), CommandError> {
    if name.trim().is_empty() {
        Err(CommandError::BadRequest(
            "Preset name cannot be empty".into(),
        ))
    } else {
        Ok(())
    }
}
async fn save_preset(
    s: &CoreServices,
    name: String,
    api_id: &str,
    data: Value,
) -> Result<Value, CommandError> {
    nonempty_name(&name)?;
    if data.is_null() {
        return Err(CommandError::BadRequest(
            "Preset data cannot be null".into(),
        ));
    }
    let preset = s.preset_service.create_preset(name, api_id, data)?;
    s.preset_service.save_preset(&preset).await?;
    Ok(value!({"name": preset.name}))
}
async fn delete_preset(
    s: &CoreServices,
    name: &str,
    kind: &PresetType,
) -> Result<(), CommandError> {
    nonempty_name(name)?;
    s.preset_service.delete_preset(name, kind).await?;
    s.skill_service
        .delete_skills_for_source(
            "preset",
            &format!("preset:{}:{}", kind.to_api_id().trim(), name.trim()),
        )
        .await?;
    Ok(())
}
