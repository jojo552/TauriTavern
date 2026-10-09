use super::{args::Args, json, unsupported};
use crate::error::CommandError;
use serde_json::Value;
use tt_application::dto::{
    chat_dto::{PinnedCharacterChatDto, PinnedGroupChatDto},
    group_dto::GroupDto,
};
use tt_application::errors::ApplicationError;
use tt_ports::repositories::chat_types::{ChatPayloadCursor, ChatPayloadTail};
use tt_runtime::CoreServices;

pub(super) fn handles(command: &str) -> bool {
    matches!(
        command,
        "get_all_chats"
            | "get_chat"
            | "get_character_chats"
            | "create_chat"
            | "add_message"
            | "rename_chat"
            | "delete_chat"
            | "search_chats"
            | "list_chat_summaries"
            | "list_recent_chat_summaries"
            | "clear_chat_cache"
            | "chat_history_generation_started"
            | "chat_history_generation_finished"
            | "get_all_groups"
            | "get_group"
            | "create_group"
            | "update_group"
            | "delete_group"
            | "clear_group_cache"
            | "list_group_chat_summaries"
            | "list_recent_group_chat_summaries"
            | "search_group_chats"
            | "delete_group_chat"
            | "rename_group_chat"
            | "get_chat_payload_tail"
            | "get_chat_payload_before"
            | "get_chat_payload_before_pages"
            | "get_group_chat_payload_tail"
            | "get_group_chat_payload_before"
            | "get_group_chat_payload_before_pages"
            | "get_character_chat_summary"
            | "get_character_chat_integrity"
            | "get_character_chat_metadata"
            | "get_character_chat_store_json"
            | "set_character_chat_store_json"
            | "update_character_chat_store_json"
            | "rename_character_chat_store_key"
            | "delete_character_chat_store_json"
            | "list_character_chat_store_keys"
            | "find_last_character_chat_message"
            | "search_character_chat_messages"
            | "get_group_chat_summary"
            | "get_group_chat_metadata"
            | "get_group_chat_store_json"
            | "set_group_chat_store_json"
            | "update_group_chat_store_json"
            | "rename_group_chat_store_key"
            | "delete_group_chat_store_json"
            | "list_group_chat_store_keys"
            | "find_last_group_chat_message"
            | "search_group_chat_messages"
    )
}

pub(super) async fn dispatch(
    s: &CoreServices,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    let character = || a.required::<String>("character_name");
    let file = || a.required::<String>("file_name");
    let chat = || a.required::<String>("chat_id");
    let namespace = || a.required::<String>("namespace");
    let key = || a.required::<String>("key");
    match command {
        "get_all_chats" => json(s.chat_service.get_all_chats().await?),
        "get_chat" => json(s.chat_service.get_chat(&character()?, &file()?).await?),
        "get_character_chats" => json(s.chat_service.get_character_chats(&character()?).await?),
        "create_chat" => json(s.chat_service.create_chat(a.required("dto")?).await?),
        "add_message" => json(s.chat_service.add_message(a.required("dto")?).await?),
        "rename_chat" => json(s.chat_service.rename_chat(a.required("dto")?).await?),
        "delete_chat" => json(s.chat_service.delete_chat(&character()?, &file()?).await?),
        "search_chats" => json(
            s.chat_service
                .search_chats(
                    &a.required::<String>("query")?,
                    a.optional::<String>("character_filter")?.as_deref(),
                )
                .await?,
        ),
        "list_chat_summaries" => json(
            s.chat_service
                .list_chat_summaries(
                    a.optional::<String>("character_filter")?.as_deref(),
                    a.default("include_metadata")?,
                )
                .await?,
        ),
        "list_recent_chat_summaries" => {
            let pinned: Vec<PinnedCharacterChatDto> = a.default("pinned")?;
            let pinned = pinned.into_iter().map(Into::into).collect::<Vec<_>>();
            json(
                s.chat_service
                    .list_recent_chat_summaries(
                        a.optional::<String>("character_filter")?.as_deref(),
                        a.default("include_metadata")?,
                        a.optional("max_entries")?.unwrap_or(usize::MAX),
                        &pinned,
                    )
                    .await?,
            )
        }
        "clear_chat_cache" => json(s.chat_service.clear_cache().await?),
        "chat_history_generation_started" => json(
            s.chat_history_coordinator
                .generation_started(a.required("locator")?)
                .await?,
        ),
        "chat_history_generation_finished" => json(
            s.chat_history_coordinator
                .generation_finished(a.required("locator")?)
                .await?,
        ),
        "get_all_groups" => json(
            s.group_service
                .get_all_groups()
                .await?
                .into_iter()
                .map(GroupDto::from)
                .collect::<Vec<_>>(),
        ),
        "get_group" => json(
            s.group_service
                .get_group(&a.required::<String>("id")?)
                .await?
                .map(GroupDto::from),
        ),
        "create_group" => json(GroupDto::from(
            s.group_service.create_group(a.required("dto")?).await?,
        )),
        "update_group" => json(GroupDto::from(
            s.group_service.update_group(a.required("dto")?).await?,
        )),
        "delete_group" => json(s.group_service.delete_group(a.required("dto")?).await?),
        "clear_group_cache" => json(s.group_service.clear_cache().await?),
        "list_group_chat_summaries" => json(
            s.group_chat_service
                .list_group_chat_summaries(
                    a.optional::<Vec<String>>("chat_ids")?.as_deref(),
                    a.default("include_metadata")?,
                )
                .await?,
        ),
        "list_recent_group_chat_summaries" => {
            let pinned: Vec<PinnedGroupChatDto> = a.default("pinned")?;
            let pinned = pinned.into_iter().map(Into::into).collect::<Vec<_>>();
            json(
                s.group_chat_service
                    .list_recent_group_chat_summaries(
                        a.optional::<Vec<String>>("chat_ids")?.as_deref(),
                        a.default("include_metadata")?,
                        a.optional("max_entries")?.unwrap_or(usize::MAX),
                        &pinned,
                    )
                    .await?,
            )
        }
        "search_group_chats" => json(
            s.group_chat_service
                .search_group_chats(
                    &a.required::<String>("query")?,
                    a.optional::<Vec<String>>("chat_ids")?.as_deref(),
                )
                .await?,
        ),
        "delete_group_chat" => json(
            s.group_chat_service
                .delete_group_chat(a.required("dto")?)
                .await?,
        ),
        "rename_group_chat" => json(
            s.group_chat_service
                .rename_group_chat(a.required("dto")?)
                .await?,
        ),
        "get_chat_payload_tail" => tail(
            s.chat_service
                .get_chat_payload_tail_lines(&character()?, &file()?, a.required("max_lines")?)
                .await,
            a.default("allow_not_found")?,
        ),
        "get_chat_payload_before" => json(
            s.chat_service
                .get_chat_payload_before_lines(
                    &character()?,
                    &file()?,
                    a.required("cursor")?,
                    a.required("max_lines")?,
                )
                .await?,
        ),
        "get_chat_payload_before_pages" => json(
            s.chat_service
                .get_chat_payload_before_pages_lines(
                    &character()?,
                    &file()?,
                    a.required("cursor")?,
                    a.required("max_lines")?,
                    a.required("max_pages")?,
                )
                .await?,
        ),
        "get_group_chat_payload_tail" => tail(
            s.group_chat_service
                .get_group_chat_payload_tail_lines(
                    &a.required::<String>("id")?,
                    a.required("max_lines")?,
                )
                .await,
            a.default("allow_not_found")?,
        ),
        "get_group_chat_payload_before" => json(
            s.group_chat_service
                .get_group_chat_payload_before_lines(
                    &a.required::<String>("id")?,
                    a.required("cursor")?,
                    a.required("max_lines")?,
                )
                .await?,
        ),
        "get_group_chat_payload_before_pages" => json(
            s.group_chat_service
                .get_group_chat_payload_before_pages_lines(
                    &a.required::<String>("id")?,
                    a.required("cursor")?,
                    a.required("max_lines")?,
                    a.required("max_pages")?,
                )
                .await?,
        ),
        "get_character_chat_summary" => json(
            s.chat_service
                .get_character_chat_summary(&character()?, &file()?, a.default("include_metadata")?)
                .await?,
        ),
        "get_character_chat_integrity" => json(
            s.chat_service
                .get_character_chat_integrity(&character()?, &file()?)
                .await?,
        ),
        "get_character_chat_metadata" => json(
            s.chat_service
                .get_character_chat_metadata(&character()?, &file()?)
                .await?,
        ),
        "get_character_chat_store_json" => json(
            s.chat_service
                .get_character_chat_store_json(&character()?, &file()?, &namespace()?, &key()?)
                .await?,
        ),
        "set_character_chat_store_json" => json(
            s.chat_service
                .set_character_chat_store_json(
                    &character()?,
                    &file()?,
                    &namespace()?,
                    &key()?,
                    a.required("value")?,
                )
                .await?,
        ),
        "update_character_chat_store_json" => json(
            s.chat_service
                .update_character_chat_store_json(
                    &character()?,
                    &file()?,
                    &namespace()?,
                    &key()?,
                    a.required("value")?,
                )
                .await?,
        ),
        "rename_character_chat_store_key" => json(
            s.chat_service
                .rename_character_chat_store_key(
                    &character()?,
                    &file()?,
                    &namespace()?,
                    &key()?,
                    &a.required::<String>("new_key")?,
                )
                .await?,
        ),
        "delete_character_chat_store_json" => json(
            s.chat_service
                .delete_character_chat_store_json(&character()?, &file()?, &namespace()?, &key()?)
                .await?,
        ),
        "list_character_chat_store_keys" => json(
            s.chat_service
                .list_character_chat_store_keys(&character()?, &file()?, &namespace()?)
                .await?,
        ),
        "find_last_character_chat_message" => json(
            s.chat_service
                .find_last_character_chat_message(&character()?, &file()?, a.required("query")?)
                .await?,
        ),
        "search_character_chat_messages" => json(
            s.chat_service
                .search_character_chat_messages(&character()?, &file()?, a.required("query")?)
                .await?,
        ),
        "get_group_chat_summary" => json(
            s.group_chat_service
                .get_group_chat_summary(&chat()?, a.default("include_metadata")?)
                .await?,
        ),
        "get_group_chat_metadata" => json(
            s.group_chat_service
                .get_group_chat_metadata(&chat()?)
                .await?,
        ),
        "get_group_chat_store_json" => json(
            s.group_chat_service
                .get_group_chat_store_json(&chat()?, &namespace()?, &key()?)
                .await?,
        ),
        "set_group_chat_store_json" => json(
            s.group_chat_service
                .set_group_chat_store_json(&chat()?, &namespace()?, &key()?, a.required("value")?)
                .await?,
        ),
        "update_group_chat_store_json" => json(
            s.group_chat_service
                .update_group_chat_store_json(
                    &chat()?,
                    &namespace()?,
                    &key()?,
                    a.required("value")?,
                )
                .await?,
        ),
        "rename_group_chat_store_key" => json(
            s.group_chat_service
                .rename_group_chat_store_key(
                    &chat()?,
                    &namespace()?,
                    &key()?,
                    &a.required::<String>("new_key")?,
                )
                .await?,
        ),
        "delete_group_chat_store_json" => json(
            s.group_chat_service
                .delete_group_chat_store_json(&chat()?, &namespace()?, &key()?)
                .await?,
        ),
        "list_group_chat_store_keys" => json(
            s.group_chat_service
                .list_group_chat_store_keys(&chat()?, &namespace()?)
                .await?,
        ),
        "find_last_group_chat_message" => json(
            s.group_chat_service
                .find_last_group_chat_message(&chat()?, a.required("query")?)
                .await?,
        ),
        "search_group_chat_messages" => json(
            s.group_chat_service
                .search_group_chat_messages(&chat()?, a.required("query")?)
                .await?,
        ),
        _ => Err(unsupported(command)),
    }
}

fn tail(
    result: Result<ChatPayloadTail, ApplicationError>,
    allow_not_found: bool,
) -> Result<Value, CommandError> {
    match result {
        Ok(tail) => json(tail),
        Err(ApplicationError::NotFound(_)) if allow_not_found => json(ChatPayloadTail {
            header: String::new(),
            lines: Vec::new(),
            cursor: ChatPayloadCursor {
                offset: 0,
                size: 0,
                modified_millis: 0,
            },
            has_more_before: false,
        }),
        Err(error) => Err(error.into()),
    }
}
