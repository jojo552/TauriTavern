use super::{args::Args, json, unsupported};
use crate::error::CommandError;
use serde_json::Value;
use tt_application::dto::character_dto::{
    DeleteCharacterDto, RenameCharacterDto, UpdateCharacterCardDataDto,
};
use tt_domain::models::skill::{SkillScope, SkillScopeRetargetRequest};
use tt_runtime::CoreServices;

pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "get_all_characters"
            | "get_character"
            | "create_character"
            | "update_character"
            | "update_character_card_data"
            | "merge_character_card_data"
            | "bulk_merge_character_card_data"
            | "check_character_lorebook_conflict"
            | "resolve_character_lorebook_conflict"
            | "delete_character"
            | "rename_character"
            | "duplicate_character"
            | "get_character_chats_by_id"
            | "clear_character_cache"
    )
}

pub(super) async fn dispatch(
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        "get_all_characters" => json(
            s.character_service
                .get_all_characters(a.default("shallow")?)
                .await?,
        ),
        "get_character" => json(
            s.character_service
                .get_character(&a.required::<String>("name")?)
                .await?,
        ),
        "create_character" => json(
            s.character_service
                .create_character(a.required("dto")?)
                .await?,
        ),
        "update_character" => json(
            s.character_service
                .update_character(&a.required::<String>("name")?, a.required("dto")?)
                .await?,
        ),
        "update_character_card_data" => {
            let dto: UpdateCharacterCardDataDto = a.required("dto")?;
            // Paths belong to the host's authenticated staging-token boundary, not JSON invoke.
            if dto.avatar_path.is_some() {
                return Err(unsupported("update_character_card_data with avatar_path"));
            }
            json(
                s.character_service
                    .update_character_card_data(&a.required::<String>("name")?, dto)
                    .await?,
            )
        }
        "merge_character_card_data" => json(
            s.character_service
                .merge_character_card_data(&a.required::<String>("name")?, a.required("dto")?)
                .await?,
        ),
        "bulk_merge_character_card_data" => json(
            s.character_service
                .bulk_merge_character_card_data(a.required("dto")?)
                .await?,
        ),
        "check_character_lorebook_conflict" => json(
            s.character_service
                .check_lorebook_conflict(a.required("dto")?)
                .await?,
        ),
        "resolve_character_lorebook_conflict" => json(
            s.character_service
                .resolve_lorebook_conflict(a.required("dto")?)
                .await?,
        ),
        "delete_character" => {
            let dto: DeleteCharacterDto = a.required("dto")?;
            let name = dto.name.clone();
            s.character_service.delete_character(dto).await?;
            // Preserve the existing command's best-effort linked-skill cleanup.
            if let Err(error) = s
                .skill_service
                .delete_skills_for_source(
                    "character",
                    &SkillScope::Character { character_id: name }.label(),
                )
                .await
            {
                tracing::error!(%error, "Deleted character but could not delete its linked Agent Skills");
            }
            json(())
        }
        "rename_character" => {
            let dto: RenameCharacterDto = a.required("dto")?;
            let old_id = dto.old_name.clone();
            let renamed = s.character_service.rename_character(dto).await?;
            if let Some(new_id) = renamed
                .avatar
                .strip_suffix(".png")
                .filter(|id| !id.is_empty())
            {
                if old_id != new_id
                    && let Err(error) = s
                        .skill_service
                        .retarget_scope(SkillScopeRetargetRequest {
                            from_scope: SkillScope::Character {
                                character_id: old_id,
                            },
                            to_scope: SkillScope::Character {
                                character_id: new_id.to_owned(),
                            },
                        })
                        .await
                {
                    tracing::error!(%error, "Renamed character but could not retarget its linked Agent Skills");
                }
            } else {
                tracing::error!("Renamed character but could not retarget its linked Agent Skills");
            }
            json(renamed)
        }
        "duplicate_character" => json(
            s.character_service
                .duplicate_character(a.required("dto")?)
                .await?,
        ),
        "get_character_chats_by_id" => json(
            s.character_service
                .get_character_chats(a.required("dto")?)
                .await?,
        ),
        "clear_character_cache" => json(s.character_service.clear_cache().await?),
        _ => Err(unsupported(command)),
    }
}
