//! JSON invoke boundary. Business behavior stays in the shared application services.
//!
//! Modules here only map browser arguments onto those services. Anything that needs host-global
//! state (staging paths, template resources, media libraries) receives `Host`; anything that is
//! purely business state receives `CoreServices`.

use http::HeaderMap;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tt_runtime::CoreServices;

use crate::error::CommandError;
use crate::host::Host;

mod agents;
mod ai_support;
mod archives;
mod args;
mod characters;
mod chats;
mod configuration;
mod documents;
mod extensions;
mod media;
use args::Args;

pub async fn dispatch(
    host: &Host,
    services: &CoreServices,
    command: &str,
    args: Value,
) -> Result<Value, CommandError> {
    let a = Args::new(args)?;
    match command {
        "get_tauritavern_settings" => {
            json(services.settings_service.get_tauritavern_settings().await?)
        }
        "get_chat_backup_storage_stats" => json(
            services
                .settings_service
                .get_chat_backup_storage_stats()
                .await?,
        ),
        "create_settings_snapshot" => json(services.settings_service.create_snapshot().await?),
        "get_settings_snapshots" => json(services.settings_service.get_snapshots().await?),
        "load_settings_snapshot" => json(
            services
                .settings_service
                .load_snapshot(&a.required::<String>("name")?)
                .await?,
        ),
        "restore_settings_snapshot" => json(
            services
                .settings_service
                .restore_snapshot(&a.required::<String>("name")?)
                .await?,
        ),
        "get_all_users" => json(services.user_service.get_all_users().await?),
        "get_user" => json(
            services
                .user_service
                .get_user(&a.required::<String>("id")?)
                .await?,
        ),
        "get_user_by_username" => json(
            services
                .user_service
                .get_user_by_username(&a.required::<String>("username")?)
                .await?,
        ),
        "create_user" => json(
            services
                .user_service
                .create_user(a.required("dto")?)
                .await?,
        ),
        "update_user" => json(
            services
                .user_service
                .update_user(a.required("dto")?)
                .await?,
        ),
        "delete_user" => json(
            services
                .user_service
                .delete_user(&a.required::<String>("id")?)
                .await?,
        ),
        "get_user_directory" => json(
            services
                .user_directory_service
                .get_user_directory(&a.required::<String>("handle")?)
                .await?,
        ),
        "ensure_user_directories_exist" => json(
            services
                .user_directory_service
                .ensure_user_directories_exist(&a.required::<String>("handle")?)
                .await?,
        ),
        "ensure_default_user_directories_exist" => json(
            services
                .user_directory_service
                .ensure_default_user_directories_exist()
                .await?,
        ),
        _ if characters::handles(command) => characters::dispatch(services, command, &a).await,
        _ if chats::handles(command) => chats::dispatch(services, command, &a).await,
        _ if configuration::handles(command) => {
            configuration::dispatch(services, command, &a).await
        }
        _ if media::handles(command) => media::dispatch(host, services, command, &a).await,
        _ if documents::handles(command) => documents::dispatch(host, services, command, &a).await,
        _ if ai_support::handles(command) => {
            ai_support::dispatch(host, services, command, &a).await
        }
        _ if agents::handles(command) => agents::dispatch(host, services, command, &a).await,
        _ if extensions::handles(command) => {
            extensions::dispatch(host, services, command, &a).await
        }
        _ => Err(unsupported(command)),
    }
}

/// Archive transfers are bound to the requesting browser, which only the page dispatcher knows.
pub async fn archives(
    host: &Host,
    browser: &str,
    command: &str,
    args: Value,
) -> Result<Option<Value>, CommandError> {
    if !archives::handles(command) {
        return Ok(None);
    }
    archives::dispatch(host, browser, command, &Args::new(args)?)
        .await
        .map(Some)
}

/// Commands whose payload is raw bytes rather than JSON, keyed by their transport headers.
pub async fn raw(
    host: &Host,
    services: &CoreServices,
    command: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<Value>, CommandError> {
    if documents::handles_raw(command) {
        return documents::raw(host, services, command, headers, body)
            .await
            .map(Some);
    }
    Ok(None)
}

pub(crate) fn json(value: impl Serialize) -> Result<Value, CommandError> {
    serde_json::to_value(value).map_err(|error| {
        CommandError::InternalServerError(format!("Failed to serialize command response: {error}"))
    })
}

pub(crate) fn unsupported(command: &str) -> CommandError {
    CommandError::BadRequest(format!("Unsupported HTTP invoke command: {command}"))
}

pub(crate) fn decode<T: DeserializeOwned>(value: Value, name: &str) -> Result<T, CommandError> {
    args::decode(value)
        .map_err(|error| CommandError::BadRequest(format!("Invalid argument {name}: {error}")))
}
