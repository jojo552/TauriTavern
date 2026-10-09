//! Provider and model plumbing around generation: token counting, connection profiles,
//! provider metadata, and the auxiliary AI services (translate, TTS, images, vectors, search).
//!
//! Each arm calls the same shared service as the native command of the same name.

use serde_json::Value;
use tt_application::dto::image_metadata_dto::{
    CreateImageMetadataFolderDto, DeleteImageMetadataFolderDto, ImageMetadataFolderAssignmentDto,
    SetImageMetadataFolderThumbnailsDto, UpdateImageMetadataFolderDto,
};
use tt_application::dto::llm_connection_dto::{
    ListLlmConnectionsResultDto, LlmConnectionIdDto, LoadLlmConnectionResultDto,
    SaveLlmConnectionDto,
};
use tt_application::dto::searxng_search_dto::SearxngSearchRequestDto;
use tt_runtime::CoreServices;

use super::args::Args;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

const COMMANDS: &[&str] = &[
    "count_openai_tokens_batch",
    "count_openai_token_prefixes",
    "encode_tokens",
    "decode_tokens",
    "build_openai_logit_bias",
    "list_llm_connections",
    "load_llm_connection",
    "save_llm_connection",
    "delete_llm_connection",
    "get_openrouter_model_providers",
    "get_openrouter_credits",
    "get_nanogpt_model_providers",
    "get_nanogpt_credits",
    "get_siliconflow_embedding_models",
    "get_workers_ai_embedding_models",
    "get_workers_ai_multimodal_models",
    "translate_text",
    "tts_handle",
    "vector_handle",
    "sd_handle",
    "cancel_sd_request",
    "search_searxng",
    "database_handle",
    "get_background_folders",
    "create_image_metadata_folder",
    "update_image_metadata_folder",
    "delete_image_metadata_folder",
    "set_image_metadata_folder_thumbnails",
    "assign_images_to_metadata_folder",
    "unassign_images_from_metadata_folder",
];

pub(super) fn handles(command: &str) -> bool {
    COMMANDS.contains(&command)
}

fn request_id(a: &Args) -> Result<String, CommandError> {
    let id = a.required::<String>("request_id")?.trim().to_string();
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CommandError::BadRequest("Invalid request id".into()));
    }
    Ok(id)
}

pub(super) async fn dispatch(
    _host: &Host,
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        "count_openai_tokens_batch" => json(
            s.tokenization_service
                .count_openai_tokens_batch(a.required("dto")?)
                .await?,
        ),
        "count_openai_token_prefixes" => json(
            s.tokenization_service
                .count_openai_token_prefixes(a.required("dto")?)
                .await?,
        ),
        "encode_tokens" => json(
            s.tokenization_service
                .encode_tokens(a.required("dto")?)
                .await?,
        ),
        "decode_tokens" => json(
            s.tokenization_service
                .decode_tokens(a.required("dto")?)
                .await?,
        ),
        "build_openai_logit_bias" => json(
            s.tokenization_service
                .build_openai_logit_bias(a.required("dto")?)
                .await?,
        ),

        "list_llm_connections" => json(ListLlmConnectionsResultDto {
            connections: s.llm_connection_service.list_connections().await?,
        }),
        "load_llm_connection" => {
            let dto: LlmConnectionIdDto = a.required("dto")?;
            json(LoadLlmConnectionResultDto {
                connection: s
                    .llm_connection_service
                    .load_connection(&dto.connection_id)
                    .await?,
            })
        }
        "save_llm_connection" => {
            let dto: SaveLlmConnectionDto = a.required("dto")?;
            json(
                s.llm_connection_service
                    .save_connection(dto.connection)
                    .await?,
            )
        }
        "delete_llm_connection" => {
            let dto: LlmConnectionIdDto = a.required("dto")?;
            json(
                s.llm_connection_service
                    .delete_connection(&dto.connection_id)
                    .await?,
            )
        }

        "get_openrouter_model_providers" => json(
            s.provider_metadata_service
                .openrouter_model_providers(a.required("dto")?)
                .await?,
        ),
        "get_openrouter_credits" => json(s.provider_metadata_service.openrouter_credits().await?),
        "get_nanogpt_model_providers" => json(
            s.provider_metadata_service
                .nanogpt_model_providers(a.required("dto")?)
                .await?,
        ),
        "get_nanogpt_credits" => json(s.provider_metadata_service.nanogpt_credits().await?),
        "get_siliconflow_embedding_models" => json(
            s.provider_metadata_service
                .siliconflow_embedding_models(a.required("dto")?)
                .await?,
        ),
        "get_workers_ai_embedding_models" => json(
            s.provider_metadata_service
                .workers_ai_embedding_models(a.required("dto")?)
                .await?,
        ),
        "get_workers_ai_multimodal_models" => json(
            s.provider_metadata_service
                .workers_ai_multimodal_models(a.required("dto")?)
                .await?,
        ),

        "translate_text" => json(
            s.translate_service
                .translate(&a.required::<String>("provider")?, a.required("body")?)
                .await?,
        ),
        "tts_handle" => json(
            s.tts_service
                .handle_request(a.required("path")?, a.required("body")?)
                .await?,
        ),
        "vector_handle" => json(
            s.vector_service
                .handle_request(&a.required::<String>("path")?, a.required("request")?)
                .await?,
        ),
        "sd_handle" => json(
            s.stable_diffusion_service
                .handle_request(&request_id(a)?, a.required("path")?, a.required("body")?)
                .await?,
        ),
        "cancel_sd_request" => {
            s.stable_diffusion_service
                .cancel_request(&request_id(a)?)
                .await;
            json(())
        }
        "search_searxng" => {
            let dto: SearxngSearchRequestDto = a.required("dto")?;
            let request = s.searxng_search_service.prepare_request(dto)?;
            crate::ai::ensure_endpoint_granted(s, Some(request.base_url.to_string())).await?;
            json(s.searxng_search_service.search(request).await?)
        }
        "database_handle" => json(s.database_service.execute(a.required("request")?).await?),

        "get_background_folders" => json(s.image_metadata_service.get_background_folders().await?),
        "create_image_metadata_folder" => {
            let dto: CreateImageMetadataFolderDto = a.required("dto")?;
            json(s.image_metadata_service.create_folder(dto).await?)
        }
        "update_image_metadata_folder" => {
            let dto: UpdateImageMetadataFolderDto = a.required("dto")?;
            json(s.image_metadata_service.update_folder(dto).await?)
        }
        "delete_image_metadata_folder" => {
            let dto: DeleteImageMetadataFolderDto = a.required("dto")?;
            json(s.image_metadata_service.delete_folder(dto).await?)
        }
        "set_image_metadata_folder_thumbnails" => {
            let dto: SetImageMetadataFolderThumbnailsDto = a.required("dto")?;
            json(s.image_metadata_service.set_folder_thumbnails(dto).await?)
        }
        "assign_images_to_metadata_folder" => {
            let dto: ImageMetadataFolderAssignmentDto = a.required("dto")?;
            json(
                s.image_metadata_service
                    .assign_images_to_folder(dto)
                    .await?,
            )
        }
        "unassign_images_from_metadata_folder" => {
            let dto: ImageMetadataFolderAssignmentDto = a.required("dto")?;
            json(
                s.image_metadata_service
                    .unassign_images_from_folder(dto)
                    .await?,
            )
        }
        _ => Err(unsupported(command)),
    }
}
