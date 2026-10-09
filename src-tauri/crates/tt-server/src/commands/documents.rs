//! Document-shaped operations: character/card import and export, chat and world-info files,
//! chat backup catalogue, archive export, and the staged-file transport those need.
//!
//! The page cannot name server paths. Every `file_path` argument must be a path the page
//! received from `stage_file_begin` / `stage_file_from_url`, and is resolved through `HostFiles`.

use http::HeaderMap;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tt_adapter_storage_core::staged_file;
use tt_application::dto::character_dto::{ImportCharacterDto, ReplaceCharacterDto};
use tt_application::dto::chat_dto::{
    ImportCharacterChatsDto, ImportGroupChatDto, RestoreCharacterChatBackupDto,
    RestoreGroupChatBackupDto,
};
use tt_application::dto::world_info_dto::{ImportWorldInfoDto, ImportWorldInfoResponseDto};
use tt_runtime::CoreServices;

use super::args::Args;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::files::{LARGE_UPLOAD_CHUNK_BYTES, SMALL_ASSET_UPLOAD_CHUNK_BYTES};
use crate::host::Host;

const DEFAULT_STAGING_KIND: &str = "generic";
const DEFAULT_STAGING_EXTENSION: &str = "bin";
const HEADER_FILE_PATH: &str = "file-path";
const HEADER_OFFSET: &str = "offset";
const HEADER_CHUNK_ENCODING: &str = "chunk-encoding";

#[derive(Deserialize)]
struct StageFileBeginDto {
    kind: Option<String>,
    preferred_extension: Option<String>,
    size: Option<u64>,
}

#[derive(Serialize)]
struct StageFileBeginResult {
    file_path: String,
    chunk_size: u64,
}

#[derive(Serialize)]
struct StageFileFinishResult {
    file_path: String,
    size: u64,
}

#[derive(Serialize)]
struct StageFileFromUrlResult {
    file_path: String,
    mime_type: Option<String>,
}

pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "stage_file_begin"
            | "stage_file_finish"
            | "stage_file_discard"
            | "stage_file_from_url"
            | "import_character"
            | "replace_character"
            | "import_character_chats"
            | "list_chat_backups"
            | "list_chat_backup_catalog"
            | "delete_chat_backup"
            | "restore_character_chat_backup"
            | "import_group_chat_payload"
            | "restore_group_chat_backup"
            | "import_world_info"
            | "download_external_import_url"
    )
}

pub(super) async fn dispatch(
    host: &Host,
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    match command {
        "stage_file_begin" => {
            let dto: StageFileBeginDto = a.required("dto")?;
            let kind = non_empty(dto.kind.as_deref()).unwrap_or(DEFAULT_STAGING_KIND);
            let extension =
                non_empty(dto.preferred_extension.as_deref()).unwrap_or(DEFAULT_STAGING_EXTENSION);
            tracing::debug!(kind, size = dto.size.unwrap_or(0), "stage_file_begin");
            let path = host.files.create_staged_file(kind, extension).await?;
            json(StageFileBeginResult {
                file_path: path.to_string_lossy().into_owned(),
                chunk_size: match kind {
                    "avatar" | "user-avatar" | "worldinfo-import" => SMALL_ASSET_UPLOAD_CHUNK_BYTES,
                    _ => LARGE_UPLOAD_CHUNK_BYTES,
                },
            })
        }
        "stage_file_finish" => {
            let path = host
                .files
                .validate_staged_path(&a.required::<String>("file_path")?)
                .await?;
            let expected_size: u64 = a.required("expected_size")?;
            let size = staged_file::finish(&path, expected_size).await?;
            json(StageFileFinishResult {
                file_path: path.to_string_lossy().into_owned(),
                size,
            })
        }
        "stage_file_discard" => {
            let path = host
                .files
                .validate_staged_path(&a.required::<String>("file_path")?)
                .await?;
            json(host.files.discard_file(&path).await?)
        }
        "stage_file_from_url" => {
            let url: String = a.required("url")?;
            let path = host
                .files
                .create_staged_file("remote", DEFAULT_STAGING_EXTENSION)
                .await?;
            match s.content_service.download_external_file(&url, &path).await {
                Ok(file) => json(StageFileFromUrlResult {
                    file_path: path.to_string_lossy().into_owned(),
                    mime_type: file.content_type,
                }),
                Err(error) => {
                    if let Err(cleanup_error) = host.files.discard_file(&path).await {
                        tracing::warn!(%cleanup_error, "Failed to discard incomplete remote download");
                    }
                    Err(error.into())
                }
            }
        }
        "import_character" => {
            let mut dto: ImportCharacterDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.character_service.import_character(dto).await?)
        }
        "replace_character" => {
            let mut dto: ReplaceCharacterDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.character_service.replace_character(dto).await?)
        }
        "import_character_chats" => {
            let mut dto: ImportCharacterChatsDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.chat_service.import_character_chats(dto).await?)
        }
        "list_chat_backups" => json(s.chat_service.list_chat_backups().await?),
        "list_chat_backup_catalog" => json(s.chat_service.list_chat_backup_catalog().await?),
        "delete_chat_backup" => json(
            s.chat_service
                .delete_chat_backup(&a.required::<String>("name")?)
                .await?,
        ),
        "restore_character_chat_backup" => {
            let dto: RestoreCharacterChatBackupDto = a.required("dto")?;
            json(s.chat_service.restore_character_chat_backup(dto).await?)
        }
        "import_group_chat_payload" => {
            let mut dto: ImportGroupChatDto = a.required("dto")?;
            dto.file_path = staged_path_string(host, &dto.file_path).await?;
            json(s.group_chat_service.import_group_chat(dto).await?)
        }
        "restore_group_chat_backup" => {
            let dto: RestoreGroupChatBackupDto = a.required("dto")?;
            json(s.group_chat_service.restore_group_chat_backup(dto).await?)
        }
        "import_world_info" => {
            let dto: ImportWorldInfoDto = a.required("dto")?;
            let file_path = staged_path_string(host, &dto.file_path).await?;
            let name = s
                .world_info_service
                .import_world_info(&file_path, &dto.original_filename, dto.converted_data)
                .await?;
            json(ImportWorldInfoResponseDto { name })
        }
        "download_external_import_url" => json(
            s.content_service
                .download_external_import_url(&a.required::<String>("url")?)
                .await?,
        ),
        _ => Err(unsupported(command)),
    }
}

pub(super) fn handles_raw(command: &str) -> bool {
    command == "stage_file_chunk"
}

pub(super) async fn raw(
    host: &Host,
    _services: &CoreServices,
    command: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value, CommandError> {
    if command != "stage_file_chunk" {
        return Err(unsupported(command));
    }
    if headers.contains_key(HEADER_CHUNK_ENCODING) {
        return Err(CommandError::BadRequest(
            "Browser file staging frames must be raw bytes".into(),
        ));
    }
    let file_path = percent_decode_str(header(headers, HEADER_FILE_PATH)?)
        .decode_utf8()
        .map_err(|_| CommandError::BadRequest("File staging file path header is invalid".into()))?;
    let offset = header(headers, HEADER_OFFSET)?
        .parse::<u64>()
        .map_err(|_| CommandError::BadRequest("File staging offset header is invalid".into()))?;
    let path = host.files.validate_staged_path(&file_path).await?;
    Ok(Value::from(staged_file::append(&path, offset, body).await?))
}

/// Resolves a page-supplied staged path; services then receive only paths inside the staging root.
pub(super) async fn staged_path_string(host: &Host, raw: &str) -> Result<String, CommandError> {
    let path = host.files.validate_staged_path(raw).await?;
    path.into_os_string().into_string().map_err(|_| {
        CommandError::InternalServerError("Staged file path is not valid UTF-8".into())
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, CommandError> {
    headers
        .get(name)
        .ok_or_else(|| CommandError::BadRequest(format!("Missing file staging header: {name}")))?
        .to_str()
        .map_err(|_| CommandError::BadRequest(format!("Invalid file staging header: {name}")))
}
