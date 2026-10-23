//! Browser-session resource ownership: the HTTP equivalent of a WebView resource table.
//!
//! Readers, cold swipe sources and commit sessions are created by shared application services;
//! this table only decides which browser session may continue them. IDs are session-local and
//! never index another session's table, and commit IDs are accepted only after this session began
//! them. Closing a session aborts its unfinished commits; published data is never touched here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tt_application::dto::chat_history_dto::{ChatHistoryLocator, CurrentCommitReason};
use tt_application::dto::settings_dto::UserSettingsRevisionDto;
use tt_application::dto::world_info_dto::GetWorldInfoDto;
use tt_contracts::byte_commit::CommitBegin;
use tt_contracts::extension_store::{EntryKind, WriteOperation};
use tt_ports::byte_reader::ByteReader;
use tt_ports::repositories::chat_commit_repository::{
    ChatCommitOperation, ChatSwipeSource, ColdSwipeCommitSource,
};
use tt_runtime::CoreServices;

use crate::error::CommandError;

const CHAT_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const BACKUP_CHUNK_BYTES: usize = 512 * 1024;
const EXTENSION_STORE_CHUNK_BYTES: usize = 512 * 1024;
/// A page holds a handful of readers at once; the cap bounds abandoned-reader leaks per tab.
const MAX_OPEN_RESOURCES: usize = 64;
const MAX_OPEN_COMMITS: usize = 16;

pub enum ResourceReply {
    Json(Value),
    Bytes(Vec<u8>),
}

struct Reader {
    reader: AsyncMutex<Box<dyn ByteReader>>,
    chunk_bytes: usize,
}

enum Resource {
    Reader(Arc<Reader>),
    SwipeSource(Arc<dyn ChatSwipeSource>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommitKind {
    Chat,
    Settings,
    WorldInfo,
    ExtensionStore,
}

#[derive(Default)]
struct Table {
    next_id: u32,
    resources: HashMap<u32, Resource>,
    commits: HashMap<String, CommitKind>,
    closed: bool,
}

#[derive(Default)]
pub struct SessionResources {
    table: Mutex<Table>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ChatCommitOperationDto {
    Payload {
        force: bool,
        #[serde(rename = "coldSourceId")]
        cold_source_id: Option<u32>,
    },
    Metadata,
    MetadataExtension {
        namespace: String,
    },
}

/// A finish request that decoded completely; only then does the page give up its commit.
struct Finish {
    id: String,
    expected_size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenColdChatResult {
    source_id: u32,
    reader_id: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenEntryResponse {
    reader_id: u32,
    mime_type: String,
}

impl SessionResources {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Table>, CommandError> {
        let table = self.table.lock().expect("session resource table poisoned");
        if table.closed {
            return Err(CommandError::Unauthorized(
                "Browser session closed; reload the page".into(),
            ));
        }
        Ok(table)
    }

    fn add(&self, resource: Resource) -> Result<u32, CommandError> {
        let mut table = self.lock()?;
        if table.resources.len() >= MAX_OPEN_RESOURCES {
            return Err(CommandError::TooManyRequests(
                "Too many open byte readers in this page; close unused readers".into(),
            ));
        }
        table.next_id = table.next_id.checked_add(1).ok_or_else(|| {
            CommandError::TooManyRequests("Resource IDs exhausted; reload the page".into())
        })?;
        let id = table.next_id;
        table.resources.insert(id, resource);
        Ok(id)
    }

    fn add_reader(
        &self,
        reader: Box<dyn ByteReader>,
        chunk_bytes: usize,
    ) -> Result<u32, CommandError> {
        self.add(Resource::Reader(Arc::new(Reader {
            reader: AsyncMutex::new(reader),
            chunk_bytes,
        })))
    }

    fn reader(&self, rid: u32) -> Result<Arc<Reader>, CommandError> {
        match self.lock()?.resources.get(&rid) {
            Some(Resource::Reader(reader)) => Ok(reader.clone()),
            _ => Err(CommandError::BadRequest(format!(
                "Byte reader {rid} is not open in this page"
            ))),
        }
    }

    fn swipe_source(&self, rid: u32) -> Result<Arc<dyn ChatSwipeSource>, CommandError> {
        match self.lock()?.resources.get(&rid) {
            Some(Resource::SwipeSource(source)) => Ok(source.clone()),
            _ => Err(CommandError::BadRequest(
                "Cold swipe source expired; reopen the chat".into(),
            )),
        }
    }

    fn register_commit(&self, kind: CommitKind, begin: &CommitBegin) -> Result<(), CommandError> {
        let mut table = self.lock()?;
        if table.commits.len() >= MAX_OPEN_COMMITS {
            return Err(CommandError::TooManyRequests(
                "Too many unfinished commits in this page".into(),
            ));
        }
        table.commits.insert(begin.session_id.clone(), kind);
        Ok(())
    }

    fn owned_commit(&self, kind: CommitKind, id: &str) -> Result<(), CommandError> {
        match self.lock()?.commits.get(id) {
            Some(owned) if *owned == kind => Ok(()),
            _ => Err(CommandError::BadRequest(
                "Commit session is not open in this page".into(),
            )),
        }
    }

    /// Decodes a finish request, then claims its commit. A request that does not decode is not a
    /// finish attempt: the commit stays owned, so the page can still abort it and closing the page
    /// reclaims it. Claiming before decoding would orphan the commit's shared receiving slot.
    fn claim_finish(&self, kind: CommitKind, a: &Args) -> Result<Finish, CommandError> {
        let finish = Finish {
            id: a.get("sessionId")?,
            expected_size: a.get("expectedSize")?,
        };
        self.take_commit(kind, &finish.id)?;
        Ok(finish)
    }

    /// Finish and abort consume the commit whatever the service outcome, matching byte-commit.js.
    fn take_commit(&self, kind: CommitKind, id: &str) -> Result<(), CommandError> {
        let mut table = self.lock()?;
        match table.commits.get(id) {
            Some(owned) if *owned == kind => {
                table.commits.remove(id);
                Ok(())
            }
            _ => Err(CommandError::BadRequest(
                "Commit session is not open in this page".into(),
            )),
        }
    }

    /// Begins a commit and records ownership; a commit the table cannot own is aborted at once.
    async fn begin<F>(
        &self,
        services: &CoreServices,
        kind: CommitKind,
        begin: F,
    ) -> Result<ResourceReply, CommandError>
    where
        F: std::future::Future<
                Output = Result<CommitBegin, tt_application::errors::ApplicationError>,
            >,
    {
        drop(self.lock()?);
        let started = begin.await?;
        if let Err(error) = self.register_commit(kind, &started) {
            if let Err(abort) = abort(services, kind, &started.session_id).await {
                tracing::error!(%abort, "Failed to abort an unowned commit session");
            }
            return Err(error);
        }
        json(started)
    }

    pub async fn dispatch(
        &self,
        services: &CoreServices,
        command: &str,
        args: Value,
    ) -> Result<Option<ResourceReply>, CommandError> {
        let a = Args(args);
        let reply = match command {
            "read_bytes" => {
                let reader = self.reader(a.get("rid")?)?;
                let mut buffer = vec![0; reader.chunk_bytes];
                let n = reader.reader.lock().await.read(&mut buffer).await?;
                buffer.truncate(n);
                ResourceReply::Bytes(buffer)
            }
            "close_resource" => {
                let rid: u32 = a.get("rid")?;
                if self.lock()?.resources.remove(&rid).is_none() {
                    return Err(CommandError::BadRequest(format!(
                        "Resource {rid} is not open in this page"
                    )));
                }
                json(())?
            }
            "open_chat_reader" => {
                let reader = services
                    .chat_commit_service
                    .open_payload_jsonl(a.get("target")?, a.get("allowNotFound")?)
                    .await?;
                json(
                    reader
                        .map(|reader| self.add_reader(reader, CHAT_CHUNK_BYTES))
                        .transpose()?,
                )?
            }
            "open_chat_payload_json" => {
                let reader = services
                    .chat_commit_service
                    .open_payload_json(a.get("target")?, a.get("allowNotFound")?)
                    .await?;
                json(
                    reader
                        .map(|reader| self.add_reader(reader, CHAT_CHUNK_BYTES))
                        .transpose()?,
                )?
            }
            "open_cold_chat" => {
                let target: ChatHistoryLocator = a.get("target")?;
                let Some(source) = services
                    .chat_commit_service
                    .open_swipe_source(target, a.get("allowNotFound")?)
                    .await?
                else {
                    return Ok(Some(json(Option::<OpenColdChatResult>::None)?));
                };
                let source_id = self.add(Resource::SwipeSource(source.clone()))?;
                let reader_id =
                    match self.add_reader(source.projection(source_id), CHAT_CHUNK_BYTES) {
                        Ok(id) => id,
                        Err(error) => {
                            self.lock()?.resources.remove(&source_id);
                            return Err(error);
                        }
                    };
                json(Some(OpenColdChatResult {
                    source_id,
                    reader_id,
                }))?
            }
            "open_cold_swipe_record" => {
                let source = self.swipe_source(a.get("sourceId")?)?;
                let reader = source.record(a.get("record")?).await?;
                json(self.add_reader(reader, CHAT_CHUNK_BYTES)?)?
            }
            "open_chat_backup_download" => {
                let name: String = a.get("name")?;
                let reader = services
                    .chat_service
                    .open_chat_backup_download(&name)
                    .await?;
                json(self.add_reader(reader, BACKUP_CHUNK_BYTES)?)?
            }
            "open_extension_store_entry" => {
                let namespace: String = a.get("namespace")?;
                let key: String = a.get("key")?;
                let table: Option<String> = a.get("table")?;
                let kind: EntryKind = a.get("kind")?;
                let Some(reader) = services
                    .extension_store_service
                    .open_entry(&namespace, table.as_deref(), &key, kind)
                    .await?
                else {
                    return Ok(Some(json(Option::<OpenEntryResponse>::None)?));
                };
                let mime_type = match kind {
                    EntryKind::Json => "application/json".to_string(),
                    EntryKind::Blob => mime_guess::from_path(&key)
                        .first_or_octet_stream()
                        .essence_str()
                        .to_string(),
                };
                json(Some(OpenEntryResponse {
                    reader_id: self.add_reader(reader, EXTENSION_STORE_CHUNK_BYTES)?,
                    mime_type,
                }))?
            }
            "read_agent_workspace_bytes" => ResourceReply::Bytes(
                services
                    .agent_runtime_service
                    .read_workspace_bytes(a.get("dto")?)
                    .await?,
            ),
            "get_world_info" => {
                let dto: GetWorldInfoDto = a.get("dto")?;
                ResourceReply::Bytes(
                    services
                        .world_info_service
                        .get_world_info(&dto.name)
                        .await?
                        .bytes,
                )
            }
            "export_character_content" => {
                let dto: tt_application::dto::character_dto::ExportCharacterContentDto =
                    a.get("dto")?;
                ResourceReply::Bytes(
                    services
                        .character_service
                        .export_character_content(dto)
                        .await?
                        .data,
                )
            }
            "get_sillytavern_settings" => ResourceReply::Bytes(
                services
                    .settings_service
                    .get_sillytavern_settings()
                    .await?
                    .bytes
                    .to_vec(),
            ),
            "begin_chat_commit" => {
                let target: ChatHistoryLocator = a.get("target")?;
                let operation = match a.get::<ChatCommitOperationDto>("operation")? {
                    ChatCommitOperationDto::Payload {
                        force,
                        cold_source_id,
                    } => ChatCommitOperation::Payload {
                        force,
                        cold_source: cold_source_id
                            .map(|id| {
                                self.swipe_source(id)
                                    .map(|source| ColdSwipeCommitSource { id, source })
                            })
                            .transpose()?,
                    },
                    ChatCommitOperationDto::Metadata => ChatCommitOperation::Metadata,
                    ChatCommitOperationDto::MetadataExtension { namespace } => {
                        ChatCommitOperation::MetadataExtension { namespace }
                    }
                };
                self.begin(
                    services,
                    CommitKind::Chat,
                    services.chat_commit_service.begin(target, operation),
                )
                .await?
            }
            "finish_chat_commit" => {
                // Part of the request, so it decodes before the commit is claimed.
                let reason: CurrentCommitReason = a.get("commitReason")?;
                let finish = self.claim_finish(CommitKind::Chat, &a)?;
                json(
                    services
                        .chat_commit_service
                        .finish(&finish.id, finish.expected_size, reason)
                        .await?,
                )?
            }
            "abort_chat_commit" => {
                let id: String = a.get("sessionId")?;
                self.take_commit(CommitKind::Chat, &id)?;
                json(services.chat_commit_service.abort(&id).await?)?
            }
            "begin_settings_commit" => {
                let expected: Option<UserSettingsRevisionDto> = a.get("expectedRevision")?;
                self.begin(
                    services,
                    CommitKind::Settings,
                    services.settings_service.begin_commit(expected),
                )
                .await?
            }
            "finish_settings_commit" => {
                let finish = self.claim_finish(CommitKind::Settings, &a)?;
                json(
                    services
                        .settings_service
                        .finish_commit(&finish.id, finish.expected_size)
                        .await?,
                )?
            }
            "abort_settings_commit" => {
                let id: String = a.get("sessionId")?;
                self.take_commit(CommitKind::Settings, &id)?;
                json(services.settings_service.abort_commit(&id).await?)?
            }
            "begin_world_info_commit" => {
                self.begin(
                    services,
                    CommitKind::WorldInfo,
                    services.world_info_service.begin_commit(),
                )
                .await?
            }
            "finish_world_info_commit" => {
                let finish = self.claim_finish(CommitKind::WorldInfo, &a)?;
                json(
                    services
                        .world_info_service
                        .finish_commit(&finish.id, finish.expected_size)
                        .await?,
                )?
            }
            "abort_world_info_commit" => {
                let id: String = a.get("sessionId")?;
                self.take_commit(CommitKind::WorldInfo, &id)?;
                json(services.world_info_service.abort_commit(&id).await?)?
            }
            "begin_extension_store_commit" => {
                let namespace: String = a.get("namespace")?;
                let key: String = a.get("key")?;
                let table: Option<String> = a.get("table")?;
                let operation: WriteOperation = a.get("operation")?;
                self.begin(
                    services,
                    CommitKind::ExtensionStore,
                    services.extension_store_service.begin_commit(
                        &namespace,
                        table.as_deref(),
                        &key,
                        operation,
                    ),
                )
                .await?
            }
            "finish_extension_store_commit" => {
                let finish = self.claim_finish(CommitKind::ExtensionStore, &a)?;
                json(
                    services
                        .extension_store_service
                        .finish_commit(&finish.id, finish.expected_size)
                        .await?,
                )?
            }
            "abort_extension_store_commit" => {
                let id: String = a.get("sessionId")?;
                self.take_commit(CommitKind::ExtensionStore, &id)?;
                json(services.extension_store_service.abort_commit(&id).await?)?
            }
            _ => return Ok(None),
        };
        Ok(Some(reply))
    }

    /// Raw-body commit frames. Headers are the native `session-id` and `offset`.
    pub async fn append(
        &self,
        services: &CoreServices,
        command: &str,
        headers: &HeaderMap,
        bytes: &[u8],
    ) -> Result<Option<Value>, CommandError> {
        let kind = match command {
            "append_chat_commit_chunk" => CommitKind::Chat,
            "append_settings_commit_chunk" => CommitKind::Settings,
            "append_world_info_commit_chunk" => CommitKind::WorldInfo,
            "append_extension_store_commit_chunk" => CommitKind::ExtensionStore,
            _ => return Ok(None),
        };
        if headers.contains_key("chunk-encoding") {
            return Err(CommandError::BadRequest(
                "Browser commit frames must be raw bytes".into(),
            ));
        }
        let id = header(headers, "session-id")?;
        let offset = header(headers, "offset")?
            .parse::<u64>()
            .map_err(|_| CommandError::BadRequest("Commit offset is invalid".into()))?;
        self.owned_commit(kind, id)?;
        let accepted = match kind {
            CommitKind::Chat => {
                services
                    .chat_commit_service
                    .append(id, offset, bytes)
                    .await?
            }
            CommitKind::Settings => {
                services
                    .settings_service
                    .append_commit(id, offset, bytes)
                    .await?
            }
            CommitKind::WorldInfo => {
                services
                    .world_info_service
                    .append_commit(id, offset, bytes)
                    .await?
            }
            CommitKind::ExtensionStore => {
                services
                    .extension_store_service
                    .append_commit(id, offset, bytes)
                    .await?
            }
        };
        Ok(Some(Value::from(accepted)))
    }

    /// Ends the page: later calls fail, readers drop, and unfinished commits are aborted.
    pub async fn close_all(&self, services: &CoreServices) -> Result<(), CommandError> {
        let commits = {
            let mut table = self.table.lock().expect("session resource table poisoned");
            table.closed = true;
            table.resources.clear();
            std::mem::take(&mut table.commits)
        };
        let mut failure = None;
        for (id, kind) in commits {
            if let Err(error) = abort(services, kind, &id).await {
                tracing::error!(%error, ?kind, "Failed to abort a commit of a closed browser session");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

async fn abort(services: &CoreServices, kind: CommitKind, id: &str) -> Result<(), CommandError> {
    match kind {
        CommitKind::Chat => services.chat_commit_service.abort(id).await?,
        CommitKind::Settings => services.settings_service.abort_commit(id).await?,
        CommitKind::WorldInfo => services.world_info_service.abort_commit(id).await?,
        CommitKind::ExtensionStore => services.extension_store_service.abort_commit(id).await?,
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, CommandError> {
    headers
        .get(name)
        .ok_or_else(|| CommandError::BadRequest(format!("Missing commit header: {name}")))?
        .to_str()
        .map_err(|_| CommandError::BadRequest(format!("Invalid commit header: {name}")))
}

fn json(value: impl Serialize) -> Result<ResourceReply, CommandError> {
    serde_json::to_value(value)
        .map(ResourceReply::Json)
        .map_err(|error| {
            CommandError::InternalServerError(format!("Failed to serialize response: {error}"))
        })
}

/// Native command arguments are camelCase on the wire; snake_case is accepted for the same field.
struct Args(Value);

impl Args {
    fn get<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<T, CommandError> {
        let snake: String = name
            .chars()
            .flat_map(|c| {
                if c.is_ascii_uppercase() {
                    vec!['_', c.to_ascii_lowercase()]
                } else {
                    vec![c]
                }
            })
            .collect();
        let value = self
            .0
            .get(name)
            .or_else(|| self.0.get(&snake))
            .cloned()
            .unwrap_or(Value::Null);
        serde_json::from_value(value)
            .map_err(|error| CommandError::BadRequest(format!("Invalid argument {name}: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(id: &str) -> CommitBegin {
        CommitBegin {
            session_id: id.into(),
            max_frame_bytes: 8,
        }
    }

    #[test]
    fn commits_are_owned_by_kind_and_consumed_once() {
        let resources = SessionResources::default();
        resources
            .register_commit(CommitKind::Chat, &begin("a"))
            .unwrap();
        assert!(resources.owned_commit(CommitKind::Settings, "a").is_err());
        assert!(resources.owned_commit(CommitKind::Chat, "guessed").is_err());
        resources.take_commit(CommitKind::Chat, "a").unwrap();
        assert!(resources.take_commit(CommitKind::Chat, "a").is_err());
    }

    #[test]
    fn a_finish_that_does_not_decode_leaves_the_commit_owned_for_abort() {
        let resources = SessionResources::default();
        resources
            .register_commit(CommitKind::Settings, &begin("a"))
            .unwrap();
        // Missing expectedSize: rejected without consuming the page's ownership.
        let malformed = Args(serde_json::json!({ "sessionId": "a" }));
        assert!(matches!(
            resources.claim_finish(CommitKind::Settings, &malformed),
            Err(CommandError::BadRequest(_))
        ));
        assert!(resources.owned_commit(CommitKind::Settings, "a").is_ok());

        let finish = Args(serde_json::json!({ "sessionId": "a", "expectedSize": 2 }));
        let claimed = resources
            .claim_finish(CommitKind::Settings, &finish)
            .unwrap();
        assert_eq!((claimed.id.as_str(), claimed.expected_size), ("a", 2));
        assert!(resources.owned_commit(CommitKind::Settings, "a").is_err());
    }

    #[test]
    fn closed_sessions_reject_new_resources() {
        let resources = SessionResources::default();
        resources.table.lock().unwrap().closed = true;
        assert!(
            resources
                .register_commit(CommitKind::Chat, &begin("a"))
                .is_err()
        );
    }
}
