use super::{args::Args, json, unsupported};
use crate::error::CommandError;
use serde_json::Value;
use tt_application::dto::character_dto::{AdoptCharacterLorebookDto, UpdateCharacterCardDataDto};
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
            | "adopt_character_lorebook"
            | "resolve_character_lorebook_adoption"
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
        "adopt_character_lorebook" => {
            let dto: AdoptCharacterLorebookDto = a.required("dto")?;
            json(s.character_service.adopt_character_lorebook(&dto.name).await?)
        }
        "resolve_character_lorebook_adoption" => json(
            s.character_service
                .resolve_lorebook_adoption(a.required("dto")?)
                .await?,
        ),
        "delete_character" => json(
            s.character_service
                .delete_character(a.required("dto")?, &s.skill_service)
                .await?,
        ),
        "rename_character" => json(
            s.character_service
                .rename_character(a.required("dto")?, &s.skill_service)
                .await?,
        ),
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
