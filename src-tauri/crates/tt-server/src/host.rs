//! HTTP surface of the browser host: authentication, sessions, invoke, events and resources.
//!
//! Every route is behind HTTP Basic authentication. Invoke and event routes additionally require
//! a page session bound to the browser cookie, and every state-changing request needs the
//! session CSRF token. Command semantics come from the shared services; this module only maps
//! the browser transport onto them.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_http::CompressionLevel;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::services::ServeDir;
use tt_application::dto::agent_dto::{AgentRunLiveUpdateDto, AgentSubscribeRunLiveProjectionDto};
use tt_application::dto::bootstrap_dto::BootstrapMetadataDto;
use tt_application::dto::group_dto::GroupDto;
use tt_application::dto::settings_dto::UpdateTauriTavernSettingsDto;
use tt_application::services::agent_runtime_service::AgentRunLivePresenter;
use tt_application::services::bundled_template_service::BundledTemplateService;
use tt_application::services::host_resource_service::{
    HostResourceDeliveryCapabilities, HostResourceService,
};
use tt_application::services::user_media_service::{
    ListUserImagesInput, UploadUserImageInput, UserMediaService,
};
use tt_contracts::extension_tools::{
    ExtensionToolDefinition, ExtensionToolEvent, ResolveExtensionToolCallDto,
    SetExtensionToolEnabledDto,
};
use tt_domain::ios_policy::IosPolicyActivationReport;
use tt_ports::extension_tools::ExtensionToolEventSink;
use tt_runtime::CoreServices;
use tt_runtime::extension_tools::AgentExtensionTools;

use crate::ai::AiSession;
use crate::error::CommandError;
use crate::events::{PageBus, PageChannel};
use crate::files::HostFiles;
use crate::resources::{ResourceReply, SessionResources};
use crate::security::{Security, Session};

/// Raw invoke bodies are bounded by the largest native frame (4 MiB) plus headroom.
const MAX_INVOKE_BODY_BYTES: usize = 64 * 1024 * 1024;
const SILLYTAVERN_COMPAT_VERSION: &str = "1.19.0";
/// The page loads once; the browser must ask again after a deployment rather than mix builds.
const INDEX_CACHE_CONTROL: &str = "no-store";

pub struct Host {
    pub services: Arc<CoreServices>,
    pub security: Arc<Security>,
    pub ios_policy: IosPolicyActivationReport,
    pub templates: Arc<BundledTemplateService>,
    pub media: Arc<UserMediaService>,
    pub host_resources: Arc<HostResourceService>,
    pub static_files: ServeDir,
    pub files: HostFiles,
    pub index_html: Bytes,
    pub extension_tools: Arc<AgentExtensionTools>,
    pub updates: Arc<tt_application::services::update_service::UpdateService>,
    pub downloads: crate::downloads::Downloads,
    pub archives: Arc<tt_application::services::data_archive_service::DataArchiveService>,
}

/// A page channel is how the browser host delivers extension tool events to a page.
impl ExtensionToolEventSink for PageChannel {
    fn send(&self, event: ExtensionToolEvent) -> Result<(), String> {
        PageChannel::send(self, event).map_err(|error| error.to_string())
    }
}

/// Server-side page state. The page's events stream doubles as its liveness signal.
#[derive(Default)]
pub struct PageState {
    pub resources: SessionResources,
    pub ai: AiSession,
    pub bus: Arc<PageBus>,
}

pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/__tt/session", get(create_session))
        .route("/__tt/events", get(events))
        .route("/__tt/invoke", post(invoke))
        .fallback(static_or_resource)
        .layer(compression())
        // Archives are already compressed and may be large; keep them out of the compression layer.
        .route("/__tt/download/{token}", get(download))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_INVOKE_BODY_BYTES))
        .layer(middleware::from_fn_with_state(host.clone(), authenticate))
        .with_state(host)
}

/// The browser reaches this host over a network, unlike the native WebView's in-process IPC:
/// settings aggregates, chat bytes and scripts are text and must not cross it uncompressed.
/// Level 3 is measured on the 1-CPU host: the ~110 MiB preset aggregate shrinks to ~11 MiB with
/// zstd (vs ~21 MiB at level 1) at no extra CPU. Brotli's default (quality 11) is too slow here.
fn compression() -> CompressionLayer<impl Predicate> {
    let predicate = DefaultPredicate::new()
        .and(NotForContentType::const_new("audio/"))
        .and(NotForContentType::const_new("video/"))
        .and(NotForContentType::const_new("font/woff"))
        .and(NotForContentType::const_new("application/zip"))
        .and(NotForContentType::const_new("application/gzip"));
    CompressionLayer::new()
        .no_br()
        .no_deflate()
        .quality(CompressionLevel::Precise(3))
        .compress_when(predicate)
}

async fn authenticate(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
    match host.security.authenticate(request.headers()) {
        Ok(()) => {
            let mut response = next.run(request).await;
            let headers = response.headers_mut();
            headers.insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
            headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
            response
        }
        Err(StatusCode::UNAUTHORIZED) => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"TauriTavern\", charset=\"UTF-8\"",
            )],
            "Authentication required",
        )
            .into_response(),
        Err(status) => status.into_response(),
    }
}

/// Serves a one-shot host-built file to the browser that requested it.
async fn download(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Response {
    if let Err(error) = host.security.same_origin(&headers) {
        return error.into_response();
    }
    let browser = crate::security::browser_cookie(&headers);
    host.downloads.take(&token, browser.as_deref()).await
}

async fn create_session(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    match host.security.create_session(&headers) {
        Ok((session, cookie)) => {
            let mut response =
                Json(json!({ "sessionId": session.id, "csrfToken": session.csrf })).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Some(cookie) = cookie {
                response.headers_mut().insert(
                    header::SET_COOKIE,
                    HeaderValue::from_str(&cookie).expect("controlled cookie"),
                );
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsQuery {
    session_id: String,
}

/// Closes the page's resources when the browser's events stream ends (navigation, reload, close).
struct PageGuard {
    host: Arc<Host>,
    session: Arc<Session>,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        let host = self.host.clone();
        let session = self.session.clone();
        tokio::spawn(async move { close_session(&host, session).await });
    }
}

pub async fn close_session(host: &Host, session: Arc<Session>) {
    host.security.remove_session(&session.id);
    session.page.bus.close();
    host.extension_tools.clear_page(&session.id);
    session.page.ai.close_all(&host.services).await;
    if let Err(error) = session.page.resources.close_all(&host.services).await {
        tracing::error!(%error, "Browser page closed with commits that could not be aborted");
    }
}

async fn events(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, CommandError> {
    let session = host.security.session(&headers, &query.session_id, false)?;
    if session.event_connected.swap(true, Ordering::SeqCst) {
        return Err(CommandError::Conflict(
            "Page event stream already connected; reload the page".into(),
        ));
    }
    let receiver = session.page.bus.take_receiver().ok_or_else(|| {
        CommandError::Conflict("Page event stream already connected; reload the page".into())
    })?;
    let guard = PageGuard {
        host: host.clone(),
        session: session.clone(),
    };
    let ready =
        Event::default().data(json!({ "type": "ready", "sessionId": session.id }).to_string());
    let stream = page_stream(ready, receiver, guard);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))))
}

fn page_stream(
    ready: Event,
    receiver: tokio::sync::mpsc::Receiver<String>,
    guard: PageGuard,
) -> impl Stream<Item = Result<Event, Infallible>> {
    // The guard lives as long as the browser keeps the stream open; dropping it closes the page.
    let frames = tokio_stream::wrappers::ReceiverStream::new(receiver)
        .map(|frame| Ok(Event::default().data(frame)));
    futures_util::stream::once(async move { Ok(ready) })
        .chain(frames)
        .map(move |event| {
            let _ = &guard;
            event
        })
}

use futures_util::StreamExt;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonInvoke {
    session_id: String,
    command: String,
    #[serde(default)]
    args: Value,
}

async fn invoke(State(host): State<Arc<Host>>, headers: HeaderMap, body: Bytes) -> Response {
    let result = async {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if content_type.starts_with("application/octet-stream") {
            let session_id = required(&headers, "x-tt-session")?;
            let session = host.security.session(&headers, session_id, true)?;
            let command = required(&headers, "x-tt-command")?;
            return raw_invoke(&host, &session, command, &headers, &body).await;
        }
        if !content_type.starts_with("application/json") {
            return Err(CommandError::BadRequest(
                "Invoke requires JSON or octet-stream".into(),
            ));
        }
        let request: JsonInvoke = serde_json::from_slice(&body).map_err(|error| {
            CommandError::BadRequest(format!("Invalid invoke request: {error}"))
        })?;
        let session = host.security.session(&headers, &request.session_id, true)?;
        dispatch(&host, &session, &request.command, request.args).await
    }
    .await;
    match result {
        Ok(ResourceReply::Json(value)) => Json(value).into_response(),
        Ok(ResourceReply::Bytes(bytes)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

fn required<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, CommandError> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| CommandError::BadRequest(format!("Missing header: {name}")))
}

async fn raw_invoke(
    host: &Host,
    session: &Session,
    command: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<ResourceReply, CommandError> {
    if let Some(value) = session
        .page
        .resources
        .append(&host.services, command, headers, body)
        .await?
    {
        return Ok(ResourceReply::Json(value));
    }
    match crate::commands::raw(host, &host.services, command, headers, body).await {
        Ok(Some(value)) => return Ok(ResourceReply::Json(value)),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(command, error = %error, "HTTP raw invoke failed");
            return Err(error);
        }
    }
    Err(CommandError::BadRequest(format!(
        "Command {command} does not accept a raw body in the browser host"
    )))
}

async fn dispatch(
    host: &Host,
    session: &Session,
    command: &str,
    args: Value,
) -> Result<ResourceReply, CommandError> {
    let services = &host.services;
    if let Some(reply) = session
        .page
        .resources
        .dispatch(services, command, args.clone())
        .await?
    {
        return Ok(reply);
    }
    if let Some(value) = session.page.ai.dispatch(services, command, &args).await? {
        return Ok(ResourceReply::Json(value));
    }
    if let Some(value) = page_command(host, session, command, &args).await? {
        return Ok(ResourceReply::Json(value));
    }
    if let Some(value) = host_command(host, command, &args).await? {
        return Ok(ResourceReply::Json(value));
    }
    // A page that calls an unavailable or misspelled command is only visible here: the response
    // carries the error, and the server log must name the command to make it diagnosable.
    match crate::commands::dispatch(host, services, command, args).await {
        Ok(value) => Ok(ResourceReply::Json(value)),
        Err(error) => {
            tracing::warn!(command, error = %error, "HTTP invoke failed");
            Err(error)
        }
    }
}

fn arg<T: serde::de::DeserializeOwned>(args: &Value, name: &str) -> Result<T, CommandError> {
    serde_json::from_value(args.get(name).cloned().unwrap_or(Value::Null))
        .map_err(|error| CommandError::BadRequest(format!("Invalid argument {name}: {error}")))
}

fn to_json(value: impl serde::Serialize) -> Result<Option<Value>, CommandError> {
    serde_json::to_value(value)
        .map(Some)
        .map_err(|error| CommandError::InternalServerError(error.to_string()))
}

/// Commands bound to the calling page: they push to its event stream or act for its tools.
async fn page_command(
    host: &Host,
    session: &Session,
    command: &str,
    args: &Value,
) -> Result<Option<Value>, CommandError> {
    let owner = session.id.as_str();
    if let Some(value) =
        crate::commands::archives(host, &session.browser, command, args.clone()).await?
    {
        return Ok(Some(value));
    }
    match command {
        "register_agent_extension_tool" => {
            let definition: ExtensionToolDefinition = arg(args, "definition")?;
            let channel = session.page.bus.channel(args, "channel")?;
            host.extension_tools
                .register(owner, definition, Arc::new(channel))?;
            to_json(())
        }
        "set_agent_extension_tool_enabled" => {
            let dto: SetExtensionToolEnabledDto = arg(args, "dto")?;
            host.extension_tools
                .set_enabled(owner, &dto.tool_id, dto.enabled)?;
            to_json(())
        }
        "resolve_agent_extension_tool_call" => {
            let dto: ResolveExtensionToolCallDto = arg(args, "dto")?;
            to_json(
                host.extension_tools
                    .resolve(owner, &dto.request_id, dto.result),
            )
        }
        "subscribe_agent_run_live_projection" => {
            let dto: AgentSubscribeRunLiveProjectionDto = arg(args, "dto")?;
            let channel = session.page.bus.channel(args, "channel")?;
            stream_live_projection(host, &dto.run_id, &channel).await?;
            to_json(())
        }
        _ => Ok(None),
    }
}

/// Mirrors the native command: the invoke stays pending until the run's projection ends, the page
/// goes away, or delivery fails; the channel ends when this returns.
async fn stream_live_projection(
    host: &Host,
    run_id: &str,
    channel: &PageChannel,
) -> Result<(), CommandError> {
    let Some(mut receiver) = host
        .services
        .agent_runtime_service
        .subscribe_live_projection(run_id)
        .await?
    else {
        let _ = channel.send(AgentRunLiveUpdateDto::Snapshot {
            calls: Vec::new(),
            responses: Vec::new(),
        });
        return Ok(());
    };
    let mut presenter = AgentRunLivePresenter::default();
    let snapshot = presenter.snapshot(&receiver.borrow_and_update());
    if channel.send(snapshot).is_err() {
        return Ok(());
    }
    let closed = channel.closed();
    loop {
        tokio::select! {
            changed = receiver.changed() => if changed.is_err() { return Ok(()) },
            _ = closed.cancelled() => return Ok(()),
        }
        let updates = presenter.updates(&receiver.borrow_and_update())?;
        for update in updates {
            if channel.send(update).is_err() {
                return Ok(());
            }
        }
    }
}

/// Commands whose answer is a fact about this host rather than shared application state.
async fn host_command(
    host: &Host,
    command: &str,
    args: &Value,
) -> Result<Option<Value>, CommandError> {
    let services = &host.services;
    match command {
        "is_ready" => to_json(true),
        // The server is ready before it accepts connections; backend errors stay in its log.
        "wait_for_backend_ready" => to_json(()),
        "backend_error_bridge_ready" => to_json(Vec::<String>::new()),
        // Page console output is mirrored into the server log, as the native host mirrors it into
        // its backend log; this is the only place a browser page's errors become operator-visible.
        "devlog_append_frontend_logs" => {
            let entries: Vec<tt_application::dto::dev_observability_dto::FrontendLogEntryDto> =
                arg(args, "entries")?;
            for entry in entries {
                let message = match entry.target.as_deref() {
                    Some(target) => format!("[{target}] {}", entry.message),
                    None => entry.message,
                };
                match entry.level.trim().to_ascii_lowercase().as_str() {
                    "debug" => tracing::debug!(target: "frontend", "{message}"),
                    "warn" | "warning" => tracing::warn!(target: "frontend", "{message}"),
                    "error" => tracing::error!(target: "frontend", "{message}"),
                    _ => tracing::info!(target: "frontend", "{message}"),
                }
            }
            to_json(())
        }
        // Same releases query as the native host; the browser host only reports, never installs.
        "check_for_update" => to_json(host.updates.check_for_update(arg(args, "channel")?).await?),
        "get_client_version" => to_json(json!({
            "agent": format!("SillyTavern:{SILLYTAVERN_COMPAT_VERSION}:TauriTavern"),
            "pkgVersion": SILLYTAVERN_COMPAT_VERSION,
            "tauriVersion": env!("CARGO_PKG_VERSION"),
            "gitRevision": option_env!("TAURITAVERN_GIT_REVISION").filter(|v| !v.is_empty()),
            "gitBranch": option_env!("TAURITAVERN_GIT_BRANCH").filter(|v| !v.is_empty()),
            "defaultUpdateChannel": "stable",
        })),
        "get_bootstrap_metadata" => {
            let (characters, groups, avatars, secret_state) = tokio::join!(
                services.character_service.get_all_characters(true),
                services.group_service.get_all_groups(),
                services.avatar_service.get_avatars(),
                services.secret_service.read_secret_state(),
            );
            // Same policy as the native command: characters are startup-critical, the rest degrade
            // to locally unavailable after their user-visible error.
            let groups = groups.map(|groups| groups.into_iter().map(GroupDto::from).collect()).unwrap_or_else(|error| {
                tracing::error!(target: tt_contracts::observability::USER_VISIBLE_ERROR, "Failed to load bootstrap groups snapshot: {error}");
                Vec::new()
            });
            let avatars = avatars.unwrap_or_else(|error| {
                tracing::error!(target: tt_contracts::observability::USER_VISIBLE_ERROR, "Failed to load bootstrap avatars snapshot: {error}");
                Vec::new()
            });
            let secret_state = secret_state.unwrap_or_else(|error| {
                tracing::error!(target: tt_contracts::observability::USER_VISIBLE_ERROR, "Failed to load bootstrap secret state snapshot: {error}");
                Default::default()
            });
            to_json(BootstrapMetadataDto {
                characters: characters?,
                groups,
                avatars,
                secret_state,
                ios_policy: host.ios_policy.clone(),
            })
        }
        "update_tauritavern_settings" => {
            let dto: UpdateTauriTavernSettingsDto = arg(args, "dto")?;
            let retention_updated = dto
                .agent
                .as_ref()
                .and_then(|agent| agent.retention.as_ref())
                .is_some();
            let settings = services
                .settings_service
                .update_tauritavern_settings(dto)
                .await?;
            host.host_resources
                .set_avatar_persona_original_images_enabled(
                    settings.avatar_persona_original_images_enabled,
                );
            if retention_updated {
                services
                    .agent_run_retention_automation_service
                    .notify_settings_changed();
            }
            to_json(settings)
        }
        "read_frontend_template" => to_json(host.templates.read_frontend_template(&arg::<
            String,
        >(
            args, "name",
        )?)?),
        "read_frontend_extension_template" => {
            to_json(host.templates.read_frontend_extension_template(
                &arg::<String>(args, "extension")?,
                &arg::<String>(args, "name")?,
            )?)
        }
        "upload_user_image" => to_json(
            host.media
                .upload_user_image(UploadUserImageInput {
                    image_base64: arg(args, "imageBase64")?,
                    format: arg(args, "format")?,
                    filename: arg(args, "filename")?,
                    ch_name: arg(args, "chName")?,
                })
                .await?,
        ),
        "list_user_images" => to_json(
            host.media
                .list_user_images(ListUserImagesInput {
                    folder: arg(args, "folder")?,
                    sort_field: arg(args, "sortField")?,
                    sort_order: arg(args, "sortOrder")?,
                    media_type: arg(args, "mediaType")?,
                })
                .await?,
        ),
        "list_user_image_folders" => to_json(host.media.list_user_image_folders().await?),
        "delete_user_image" => to_json(
            host.media
                .delete_user_image(&arg::<String>(args, "path")?)
                .await?,
        ),
        _ => Ok(None),
    }
}

/// Host resources (user data, thumbnails, extensions, user CSS) first, then the immutable frontend.
async fn static_or_resource(State(host): State<Arc<Host>>, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = request.uri().path();
    if path == "/" || path == "/index.html" {
        return (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, INDEX_CACHE_CONTROL),
            ],
            host.index_html.clone(),
        )
            .into_response();
    }
    let (parts, _) = request.into_parts();
    let mut resource_request = http::Request::builder()
        .method(parts.method.clone())
        .uri(parts.uri.clone());
    for (name, value) in &parts.headers {
        resource_request = resource_request.header(name, value);
    }
    let resource_request = resource_request.body(Vec::new()).expect("copied request");
    let resources = host.host_resources.clone();
    let delivery = HostResourceDeliveryCapabilities::new(true, false);
    let served =
        tokio::task::spawn_blocking(move || resources.try_serve(&resource_request, delivery)).await;
    match served {
        Ok(Some(response)) => {
            let (parts, body) = response.into_parts();
            Response::from_parts(parts, Body::from(body))
        }
        Ok(None) => {
            let request = Request::from_parts(parts, Body::empty());
            match host.static_files.clone().oneshot(request).await {
                Ok(response) => response.map(Body::new),
                Err(error) => match error {},
            }
        }
        Err(error) => CommandError::InternalServerError(format!("Resource task failed: {error}"))
            .into_response(),
    }
}

/// The browser host identity is a fact the server states before any module runs.
pub fn inject_host_identity(index_html: &str) -> Result<String, String> {
    const MARKER: &str = "<head>";
    let script = "<head>\n    <script>Object.defineProperty(window, '__TAURITAVERN_HOST__', { value: Object.freeze({ platform: 'browser', kind: 'web' }) });</script>";
    if index_html.matches(MARKER).count() != 1 {
        return Err("index.html must contain exactly one <head> element".into());
    }
    Ok(index_html.replacen(MARKER, script, 1))
}
