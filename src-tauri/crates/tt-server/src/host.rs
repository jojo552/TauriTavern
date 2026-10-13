//! HTTP surface of the browser host: authentication, sessions, invoke, events and resources.
//!
//! Private routes require Cookie authentication. Invoke and event routes additionally require
//! a page session bound to that authentication and browser identity; state-changing page requests
//! need the session CSRF token. Command semantics come from the shared services; this module only maps
//! the browser transport onto them.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
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
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::services::ServeDir;
use tt_adapter_extension::tools::AgentExtensionTools;
use tt_adapter_observability::devtools::{BACKEND_LOG_EVENT, BackendLogStore};
use tt_adapter_observability::llm_api_logs::{LLM_API_LOG_EVENT, LlmApiLogStore};
use tt_application::dto::agent_dto::{AgentRunLiveUpdateDto, AgentSubscribeRunLiveProjectionDto};
use tt_application::dto::settings_dto::UpdateTauriTavernSettingsDto;
use tt_application::services::agent_runtime_service::AgentRunLivePresenter;
use tt_application::services::bootstrap_metadata::read_bootstrap_metadata;
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

use crate::ai::AiSession;
use crate::ai_jobs::BrowserAiJobs;
use crate::auth::{LoginError, random_token};
use crate::error::CommandError;
use crate::events::{PageBus, PageChannel, PageStream, ResumeError};
use crate::files::HostFiles;
use crate::logging::PageLogStreams;
use crate::resources::{ResourceReply, SessionResources};
use crate::security::{Security, Session, StreamClaim, StreamGuard};

/// Raw invoke bodies are bounded by the largest native frame (4 MiB) plus headroom. The limit
/// applies to decoded bytes, so a gzip request body cannot expand past it.
const MAX_INVOKE_BODY_BYTES: usize = 64 * 1024 * 1024;
const SILLYTAVERN_COMPAT_VERSION: &str = "1.19.0";
/// The page loads once; the browser must ask again after a deployment rather than mix builds.
const INDEX_CACHE_CONTROL: &str = "no-store";

pub struct Host {
    pub services: Arc<CoreServices>,
    pub ai_jobs: BrowserAiJobs,
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
    pub backend_errors: Arc<crate::backend_errors::BackendErrorHub>,
    pub backend_logs: Arc<BackendLogStore>,
    pub llm_api_logs: Arc<LlmApiLogStore>,
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
    /// Live log delivery this page asked for; collection itself is host-wide.
    pub logs: PageLogStreams,
}

pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/login", get(login_page))
        .route("/logout", get(logout_page))
        .route("/__tt/auth/status", get(auth_status))
        .route(
            "/__tt/auth/login",
            post(login).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/__tt/auth/logout",
            post(logout).layer(DefaultBodyLimit::max(4 * 1024)),
        )
        .route("/__tt/session", get(create_session))
        .route("/__tt/session/close", post(close_page))
        .route("/__tt/events", get(events))
        .route("/__tt/invoke", post(invoke).layer(invoke_decompression()))
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

/// Text commit frames cross the same network in the other direction. The browser gzips them
/// (`CompressionStream`) and this layer decodes before the command reads its body, so offsets and
/// sizes stay in decoded bytes. Other encodings are refused with 415 rather than passed through.
fn invoke_decompression() -> RequestDecompressionLayer {
    RequestDecompressionLayer::new()
        .no_br()
        .no_deflate()
        .no_zstd()
}

fn public_auth_route(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (&Method::GET, "/login" | "/logout" | "/__tt/auth/status")
            | (&Method::POST, "/__tt/auth/login" | "/__tt/auth/logout")
    )
}

fn document_navigation(request: &Request) -> bool {
    if request.method() != Method::GET {
        return false;
    }
    let path = request.uri().path();
    if matches!(path, "/" | "/index.html") {
        return true;
    }
    // API/download/static resource requests must remain machine-readable, even with HTML Accept.
    // Additional top-level document entries may explicitly identify navigation; not asset paths.
    !path.starts_with("/__tt/")
        && !path.starts_with("/api/")
        && !path.contains('.')
        && !path.trim_matches('/').contains('/')
        && request
            .headers()
            .get("sec-fetch-dest")
            .is_some_and(|v| v == "document")
        && request
            .headers()
            .get("sec-fetch-mode")
            .is_some_and(|v| v == "navigate")
}

fn auth_error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(json!({ "error": message }))).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert("x-tt-login-required", HeaderValue::from_static("1"));
    }
    response
}

fn login_required(request: &Request) -> Response {
    if document_navigation(request) {
        let mut response = StatusCode::SEE_OTHER.into_response();
        response
            .headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/login"));
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    } else {
        auth_error(StatusCode::UNAUTHORIZED, "请先登录后再继续操作。")
    }
}

fn cleanup_pages(host: &Arc<Host>, pages: Vec<Arc<Session>>) {
    if pages.is_empty() {
        return;
    }
    let host = host.clone();
    tokio::spawn(async move {
        for session in pages {
            close_session(&host, session).await;
        }
    });
}

async fn authenticate(State(host): State<Arc<Host>>, request: Request, next: Next) -> Response {
    // The exact deployment authority is checked even for login/status/logout and wrong methods.
    if let Err(status) = host.security.check_host(request.headers()) {
        return auth_error(status, "请求的服务器地址不正确。");
    }
    cleanup_pages(&host, host.security.expired_sessions());
    let public = public_auth_route(request.method(), request.uri().path());
    let authentication = if public {
        None
    } else {
        match host.security.authenticate(request.headers()) {
            Ok(authentication) => Some(authentication),
            Err(_) => return login_required(&request),
        }
    };
    let mut response = next.run(request).await;
    // Admission is checked again inside Security. This final check also prevents a raced logout
    // from returning an authorized private body after its authentication lease was revoked.
    if authentication.is_some_and(|lease| !lease.active_at(std::time::Instant::now())) {
        response = auth_error(StatusCode::UNAUTHORIZED, "登录已失效，请重新登录。");
    }
    let headers = response.headers_mut();
    if public {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    response
}

fn auth_page(mode: &str) -> Response {
    let nonce = random_token();
    let html = include_str!("login.html")
        .replace("__MODE__", mode)
        .replace("__NONCE__", &nonce);
    let mut response = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response();
    let policy = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    );
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_str(&policy).expect("controlled CSP"),
    );
    response
}

async fn login_page() -> Response {
    auth_page("login")
}

async fn logout_page() -> Response {
    auth_page("logout")
}

async fn auth_status(State(host): State<Arc<Host>>, headers: HeaderMap) -> Response {
    if let Err(error) = host.security.auth_origin(&headers, false) {
        return error.into_response();
    }
    let mut response =
        Json(json!({ "authenticated": host.security.authenticate(&headers).is_ok() }))
            .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginCredentials {
    username: String,
    password: String,
}

async fn login(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    body: Result<Json<LoginCredentials>, JsonRejection>,
) -> Response {
    if let Err(error) = host.security.auth_origin(&headers, true) {
        return error.into_response();
    }
    let Json(credentials) = match body {
        Ok(body) => body,
        Err(error) => return auth_error(error.status(), "登录请求格式不正确或内容过大。"),
    };
    match host
        .security
        .login(&headers, &credentials.username, &credentials.password)
    {
        Ok(result) => {
            cleanup_pages(&host, result.closed_pages);
            let mut response = Json(json!({ "ok": true })).into_response();
            for cookie in result.cookies {
                response.headers_mut().append(
                    header::SET_COOKIE,
                    HeaderValue::from_str(&cookie).expect("controlled cookie"),
                );
            }
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(LoginError::Credentials) => {
            auth_error(StatusCode::UNAUTHORIZED, "用户名或密码不正确。")
        }
        Err(LoginError::RateLimited) => {
            let mut response = auth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "登录尝试过于频繁，请一分钟后再试。",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            response
        }
        Err(LoginError::Capacity) => auth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "登录数量已达上限，请先退出其他浏览器。",
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogoutRequest {}

async fn logout(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    body: Result<Json<LogoutRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = host.security.auth_origin(&headers, true) {
        return error.into_response();
    }
    if let Err(error) = body {
        return auth_error(error.status(), "退出请求格式不正确或内容过大。");
    }
    cleanup_pages(&host, host.security.logout(&headers));
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&host.security.clear_auth_cookie()).expect("controlled cookie"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
        Ok(session) => {
            host.backend_errors
                .register_page(&session.id, &session.page.bus);
            let mut response =
                Json(json!({ "sessionId": session.id, "csrfToken": session.csrf })).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClosePage {
    session_id: String,
}

/// Explicit navigation/close is not an accidental network loss. Release registrations before
/// the replacement page starts; retain the grace period only for disconnected live tabs.
async fn close_page(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Json(page): Json<ClosePage>,
) -> Result<StatusCode, CommandError> {
    let session = host.security.session(&headers, &page.session_id, true)?;
    terminate_page(host, session);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsQuery {
    session_id: String,
    /// Cursor for an explicitly reopened EventSource; native reconnect uses Last-Event-ID.
    after: Option<u64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum ResetReason {
    Expired,
    Gap,
}

fn session_reset(reason: ResetReason) -> Event {
    // Control frames must not advance the browser's data cursor.
    Event::default().data(json!({ "type": "session-reset", "reason": reason }).to_string())
}

fn events_cursor(headers: &HeaderMap, after: Option<u64>) -> Result<Option<u64>, CommandError> {
    let last_seen = headers
        .get("last-event-id")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| CommandError::BadRequest("Invalid Last-Event-ID".into()))
        })
        .transpose()?;
    if let (Some(header), Some(query)) = (last_seen, after)
        && header != query
    {
        return Err(CommandError::BadRequest("Conflicting event cursors".into()));
    }
    Ok(last_seen.or(after))
}

pub async fn close_session(host: &Host, session: Arc<Session>) {
    host.security.remove_session(&session.id);
    host.backend_errors.remove_page(&session.id);
    session.page.bus.close();
    host.ai_jobs.detach_page(&session.id);
    host.extension_tools.clear_page(&session.id);
    session.page.ai.close_all(&host.services).await;
    if let Err(error) = session.page.resources.close_all(&host.services).await {
        tracing::error!(%error, "Browser page closed with commits that could not be aborted");
    }
}

/// Stop admitting work before starting asynchronous cleanup, even if the response is dropped.
fn terminate_page(host: Arc<Host>, session: Arc<Session>) {
    host.security.remove_session(&session.id);
    host.backend_errors.remove_page(&session.id);
    session.page.bus.close();
    host.ai_jobs.detach_page(&session.id);
    host.extension_tools.clear_page(&session.id);
    tokio::spawn(async move { close_session(&host, session).await });
}

fn reset_response(reason: ResetReason) -> Response {
    Sse::new(futures_util::stream::once(async move {
        Ok::<_, Infallible>(session_reset(reason))
    }))
    .into_response()
}

async fn events(
    State(host): State<Arc<Host>>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Response, CommandError> {
    let last_seen = events_cursor(&headers, query.after)?;
    let guard = match host.security.claim_stream(&headers, &query.session_id)? {
        StreamClaim::Attached(guard) => guard,
        StreamClaim::Expired(session) => {
            if let Some(session) = session {
                terminate_page(host, session);
            }
            return Ok(reset_response(ResetReason::Expired));
        }
    };
    let stream = match guard.session().page.bus.attach(last_seen) {
        Ok(stream) => stream,
        Err(ResumeError::Gap) => {
            terminate_page(host, guard.session().clone());
            return Ok(reset_response(ResetReason::Gap));
        }
        Err(ResumeError::Closed) => {
            return Err(CommandError::Unauthorized("Browser session closed".into()));
        }
    };
    let ready = Event::default()
        .data(json!({ "type": "ready", "sessionId": guard.session().id }).to_string());
    Ok(Sse::new(page_stream(host, ready, stream, guard)).into_response())
}

fn page_stream(
    host: Arc<Host>,
    ready: Event,
    stream: PageStream,
    guard: StreamGuard,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let session = guard.session().clone();
    let period = Duration::from_secs(20);
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let frames = futures_util::stream::unfold(
        Some((stream, heartbeat, Some(ready))),
        move |state| {
            let host = host.clone();
            let session = session.clone();
            async move {
                let (mut stream, mut heartbeat, ready) = state?;
                if !session.active_at(std::time::Instant::now()) {
                    cleanup_pages(&host, host.security.expired_sessions());
                    return None;
                }
                let event = if let Some(ready) = ready {
                    ready
                } else {
                    let closed = session.page.bus.closed();
                    tokio::select! {
                        biased;
                        _ = session.authentication.revoked.cancelled() => return None,
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(session.authentication.expires_at)) => {
                            cleanup_pages(&host, host.security.expired_sessions());
                            return None;
                        }
                        _ = closed.cancelled() => return None,
                        // PageStream::next advances only when it returns a frame.
                        frame = stream.next() => match frame {
                            Ok(Some((id, frame))) => Event::default().id(id.to_string()).data(frame),
                            Err(ResumeError::Gap) => {
                                let authentication = session.authentication.clone();
                                terminate_page(host.clone(), session.clone());
                                return authentication.active_at(std::time::Instant::now())
                                    .then(|| (Ok(session_reset(ResetReason::Gap)), None));
                            }
                            Ok(None) | Err(ResumeError::Closed) => return None,
                        },
                        _ = heartbeat.tick() => Event::default().data(json!({
                            "type": "heartbeat", "sessionId": session.id,
                        }).to_string()),
                    }
                };
                // Recheck immediately before yielding ready, page data or heartbeat. A cancellation
                // can race a ready data branch, and authentication expiry is independent of page grace.
                if !session.active_at(std::time::Instant::now()) {
                    cleanup_pages(&host, host.security.expired_sessions());
                    return None;
                }
                Some((Ok(event), Some((stream, heartbeat, None))))
            }
        },
    );
    // The claim's guard lives exactly as long as the SSE body, including blocked/cancelled reads.
    frames.map(move |event| {
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
    let mut command_name = None;
    let result = async {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if content_type.starts_with("application/octet-stream") {
            let command = required(&headers, "x-tt-command")?;
            command_name = Some(command.to_owned());
            let session_id = required(&headers, "x-tt-session")?;
            let session = host.security.session(&headers, session_id, true)?;
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
        command_name = Some(request.command.clone());
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
        Err(error) => {
            // All branches share one diagnostic boundary, including resources/commit/session
            // errors that previously returned before the command-dispatch warning.
            tracing::warn!(command = command_name.as_deref().unwrap_or("<invalid>"), error = %error, "HTTP invoke failed");
            error.into_response()
        }
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
        Err(error) => return Err(error),
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
    if let Some(value) = host
        .ai_jobs
        .dispatch(services, session, command, &args)
        .await?
    {
        return Ok(ResourceReply::Json(value));
    }
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
    Ok(ResourceReply::Json(
        crate::commands::dispatch(host, services, command, args).await?,
    ))
}

fn arg<T: serde::de::DeserializeOwned>(args: &Value, name: &str) -> Result<T, CommandError> {
    serde_json::from_value(args.get(name).cloned().unwrap_or(Value::Null))
        .map_err(|error| CommandError::BadRequest(format!("Invalid argument {name}: {error}")))
}

/// A missing entry was evicted by retention; anything else is a storage fault with its context.
fn llm_log_error(id: u64, error: std::io::Error) -> CommandError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return CommandError::NotFound(format!("LLM API log {id} is no longer retained"));
    }
    CommandError::InternalServerError(format!("Failed to read LLM API log {id}: {error}"))
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
        "backend_error_bridge_ready" => {
            host.backend_errors.ready(owner)?;
            // Browser pending errors use the page SSE replay protocol, not an HTTP backlog.
            to_json(Vec::<String>::new())
        }
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
        // Same commands as the native host. Enabling is per page and idempotent; entries go to
        // this page's event stream only and stop with the page.
        "devlog_set_backend_log_stream_enabled" => {
            let logs = host.backend_logs.clone();
            session
                .page
                .logs
                .backend
                .set(
                    arg(args, "enabled")?,
                    move || logs.subscribe(),
                    session.page.bus.clone(),
                    BACKEND_LOG_EVENT,
                )
                .await;
            to_json(())
        }
        "devlog_set_llm_api_log_stream_enabled" => {
            let logs = host.llm_api_logs.clone();
            session
                .page
                .logs
                .llm
                .set(
                    arg(args, "enabled")?,
                    move || logs.subscribe(),
                    session.page.bus.clone(),
                    LLM_API_LOG_EVENT,
                )
                .await;
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
        // The server is ready before it accepts connections.
        "wait_for_backend_ready" => to_json(()),
        // Page console output is mirrored into the server log, as the native host mirrors it into
        // its backend log; this is the only place a browser page's errors become operator-visible.
        "devlog_get_backend_log_tail" => {
            let limit: Option<u32> = arg(args, "limit")?;
            to_json(host.backend_logs.tail(limit.unwrap_or(800) as usize))
        }
        "devlog_get_llm_api_log_index" => {
            let limit: Option<u32> = arg(args, "limit")?;
            to_json(
                host.llm_api_logs
                    .tail_index(limit.unwrap_or(50).max(1) as usize),
            )
        }
        "devlog_get_llm_api_log_preview" => {
            let id: u64 = arg(args, "id")?;
            let preview = host
                .llm_api_logs
                .get_preview(id)
                .await
                .map_err(|error| llm_log_error(id, error))?;
            to_json(preview)
        }
        "devlog_get_llm_api_log_raw" => {
            let id: u64 = arg(args, "id")?;
            let raw = host
                .llm_api_logs
                .get_raw(id)
                .await
                .map_err(|error| llm_log_error(id, error))?;
            to_json(raw)
        }
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
        "get_bootstrap_metadata" => to_json(
            read_bootstrap_metadata(
                &services.character_service,
                &services.group_service,
                &services.avatar_service,
                &services.secret_service,
                &host.ios_policy,
            )
            .await
            .map_err(CommandError::from)?,
        ),
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
            host.llm_api_logs.apply_settings(settings.dev.llm_api_keep);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gzip_invoke_bodies_are_decoded_and_bounded_by_decoded_size() {
        use std::io::Write;
        let app = Router::new()
            .route(
                "/",
                post(|body: Bytes| async move { body.len().to_string() })
                    .layer(invoke_decompression()),
            )
            .layer(axum::extract::DefaultBodyLimit::max(1024));
        let gzip = |bytes: &[u8]| {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        };
        let send = |encoding: &'static str, body: Vec<u8>| {
            app.clone().oneshot(
                Request::post("/")
                    .header(header::CONTENT_ENCODING, encoding)
                    .body(Body::from(body))
                    .unwrap(),
            )
        };

        let response = send("gzip", gzip(&[b'a'; 1000])).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], b"1000");
        // A small compressed body that expands past the limit fails instead of being buffered.
        let response = send("gzip", gzip(&[b'a'; 1025])).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = send("br", vec![0]).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn native_and_explicit_event_reconnect_share_one_cursor_and_reject_conflicts() {
        let mut headers = HeaderMap::new();
        assert_eq!(events_cursor(&headers, None).unwrap(), None);
        assert_eq!(events_cursor(&headers, Some(0)).unwrap(), Some(0));
        headers.insert("last-event-id", "0".parse().unwrap());
        assert_eq!(events_cursor(&headers, None).unwrap(), Some(0));
        assert_eq!(events_cursor(&headers, Some(0)).unwrap(), Some(0));
        assert!(matches!(
            events_cursor(&headers, Some(1)),
            Err(CommandError::BadRequest(_))
        ));
        headers.insert("last-event-id", "invalid".parse().unwrap());
        assert!(matches!(
            events_cursor(&headers, Some(0)),
            Err(CommandError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn anonymous_documents_redirect_but_resources_and_apis_return_login_json() {
        for path in ["/", "/index.html"] {
            let request = Request::builder().uri(path).body(Body::empty()).unwrap();
            let response = login_required(&request);
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(response.headers()[header::LOCATION], "/login");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
        }
        for path in [
            "/__tt/events",
            "/__tt/invoke",
            "/__tt/download/token",
            "/web/transport.js",
            "/images/avatar.png",
        ] {
            let request = Request::builder()
                .uri(path)
                .header(header::ACCEPT, "text/html")
                .header("sec-fetch-dest", "document")
                .header("sec-fetch-mode", "navigate")
                .body(Body::empty())
                .unwrap();
            let response = login_required(&request);
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()["x-tt-login-required"], "1");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
            assert!(!response.headers().contains_key(header::LOCATION));
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert!(
                value["error"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty())
            );
        }
    }

    #[tokio::test]
    async fn login_and_logout_documents_use_fresh_shared_nonce_policies_without_cookies() {
        let mut nonces = Vec::new();
        for mode in ["login", "logout"] {
            let response = auth_page(mode);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            let policy = response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .to_owned();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            let nonce = html
                .split("nonce=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert_eq!(html.matches(&format!("nonce=\"{nonce}\"")).count(), 2);
            assert!(policy.contains(&format!("script-src 'nonce-{nonce}'")));
            assert!(policy.contains(&format!("style-src 'nonce-{nonce}'")));
            assert!(html.contains(&format!("data-mode=\"{mode}\"")));
            nonces.push(nonce.to_owned());
        }
        assert_ne!(nonces[0], nonces[1]);
    }

    #[tokio::test]
    async fn session_reset_is_a_terminal_default_sse_message_without_a_data_cursor() {
        for (reason, expected) in [(ResetReason::Expired, "expired"), (ResetReason::Gap, "gap")] {
            let response = reset_response(reason);
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/event-stream"
            );
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body = std::str::from_utf8(&body).unwrap();
            // Reading the complete body deterministically verifies the reset closes the stream;
            // the only SSE field is data (no id or named event).
            let mut lines = body.lines().filter(|line| !line.is_empty());
            let data = lines
                .next()
                .unwrap()
                .strip_prefix("data:")
                .unwrap()
                .trim_start();
            assert!(lines.next().is_none());
            let control: Value = serde_json::from_str(data).unwrap();
            assert_eq!(
                control,
                json!({ "type": "session-reset", "reason": expected })
            );
        }
    }
}
