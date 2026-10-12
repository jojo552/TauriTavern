//! Chat-completion request ownership for one browser session.
//!
//! The shared service keys streams and cancellations by a process-wide ID. Browser IDs are
//! mapped to server-generated IDs, so one page can never read, close or cancel another page's
//! generation, and two pages using the same client ID cannot collide.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tt_application::dto::chat_completion_dto::{
    ChatCompletionGenerateRequestDto, ChatCompletionStatusRequestDto,
};
use tt_runtime::CoreServices;

use crate::error::CommandError;

const MAX_ACTIVE: usize = 16;

#[derive(Default)]
pub struct AiSession {
    streams: Mutex<HashMap<String, String>>,
    generations: Mutex<HashMap<String, String>>,
}

pub(crate) fn client_id(args: &Value, name: &str) -> Result<String, CommandError> {
    let id = args
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CommandError::BadRequest(format!("Invalid {name}")));
    }
    Ok(id.to_string())
}

pub(crate) fn arg<T: serde::de::DeserializeOwned>(args: &Value, name: &str) -> Result<T, CommandError> {
    serde_json::from_value(args.get(name).cloned().unwrap_or(Value::Null))
        .map_err(|error| CommandError::BadRequest(format!("Invalid argument {name}: {error}")))
}

/// The browser has no trusted confirmation dialog. Endpoints are approved by the operator.
pub async fn ensure_endpoint_granted(
    services: &CoreServices,
    endpoint: Option<String>,
) -> Result<(), CommandError> {
    let Some(endpoint) = endpoint else {
        return Ok(());
    };
    if services
        .user_endpoint_access_service
        .is_granted(&endpoint)
        .await
    {
        return Ok(());
    }
    Err(CommandError::Unauthorized(format!(
        "Endpoint {endpoint} is not approved for this server. The operator must start tt-server with --allow-endpoint {endpoint}"
    )))
}

fn insert(map: &Mutex<HashMap<String, String>>, client: String) -> Result<String, CommandError> {
    let mut map = map.lock().expect("AI session table poisoned");
    if map.contains_key(&client) {
        return Err(CommandError::Conflict(format!(
            "Request already active: {client}"
        )));
    }
    if map.len() >= MAX_ACTIVE {
        return Err(CommandError::TooManyRequests(
            "Too many active AI requests in this page".into(),
        ));
    }
    let server = format!("web-{}", uuid::Uuid::new_v4().simple());
    map.insert(client, server.clone());
    Ok(server)
}

fn lookup(map: &Mutex<HashMap<String, String>>, client: &str) -> Result<String, CommandError> {
    map.lock()
        .expect("AI session table poisoned")
        .get(client)
        .cloned()
        .ok_or_else(|| {
            CommandError::NotFound(format!("Chat completion stream not found: {client}"))
        })
}

impl AiSession {
    pub async fn dispatch(
        &self,
        services: &CoreServices,
        command: &str,
        args: &Value,
    ) -> Result<Option<Value>, CommandError> {
        let service = &services.chat_completion_service;
        let value = match command {
            "get_chat_completions_status" => {
                let dto: ChatCompletionStatusRequestDto = arg(args, "dto")?;
                ensure_endpoint_granted(services, service.resolve_status_user_endpoint(&dto)?)
                    .await?;
                service.get_status(dto).await?
            }
            "generate_chat_completion" => {
                let client = client_id(args, "requestId")?;
                let dto: ChatCompletionGenerateRequestDto = arg(args, "dto")?;
                ensure_endpoint_granted(services, service.resolve_generate_user_endpoint(&dto)?)
                    .await?;
                let server = insert(&self.generations, client.clone())?;
                let result = service.generate_request(&server, dto).await;
                self.generations
                    .lock()
                    .expect("AI session table poisoned")
                    .remove(&client);
                result?
            }
            "cancel_chat_completion_generation" => {
                let client = client_id(args, "requestId")?;
                let server = self
                    .generations
                    .lock()
                    .expect("AI session table poisoned")
                    .get(&client)
                    .cloned();
                if let Some(server) = server {
                    service.cancel_generation(&server).await;
                }
                Value::Null
            }
            "start_chat_completion_stream" => {
                let client = client_id(args, "streamId")?;
                let dto: ChatCompletionGenerateRequestDto = arg(args, "dto")?;
                ensure_endpoint_granted(services, service.resolve_generate_user_endpoint(&dto)?)
                    .await?;
                let server = insert(&self.streams, client.clone())?;
                if let Err(error) = service.clone().start_stream(server, dto).await {
                    self.streams
                        .lock()
                        .expect("AI session table poisoned")
                        .remove(&client);
                    return Err(error.into());
                }
                Value::Null
            }
            "read_chat_completion_stream" => {
                let server = lookup(&self.streams, &client_id(args, "streamId")?)?;
                serde_json::to_value(service.read_stream(&server, arg(args, "afterSeq")?).await?)
                    .map_err(|error| CommandError::InternalServerError(error.to_string()))?
            }
            "close_chat_completion_stream" => {
                let client = client_id(args, "streamId")?;
                let server = self
                    .streams
                    .lock()
                    .expect("AI session table poisoned")
                    .remove(&client);
                if let Some(server) = server {
                    service.close_stream(&server).await;
                }
                Value::Null
            }
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// A closed page cannot read its streams; stop the upstream requests it started.
    pub async fn close_all(&self, services: &CoreServices) {
        let streams: Vec<_> = self
            .streams
            .lock()
            .expect("AI session table poisoned")
            .drain()
            .map(|(_, id)| id)
            .collect();
        let generations: Vec<_> = self
            .generations
            .lock()
            .expect("AI session table poisoned")
            .drain()
            .map(|(_, id)| id)
            .collect();
        for id in streams {
            services.chat_completion_service.close_stream(&id).await;
        }
        for id in generations {
            services
                .chat_completion_service
                .cancel_generation(&id)
                .await;
        }
    }
}
