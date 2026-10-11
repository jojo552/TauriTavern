//! Native browser host: no Tauri, WebView, display server or parallel business backend.
mod ai;
mod backend_errors;
mod commands;
mod downloads;
mod error;
mod events;
mod files;
mod host;
mod logging;
mod resources;
mod security;
mod startup_paths;

use clap::Parser;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use tt_adapter_http::HttpClientPool;
use tt_adapter_media::{FilesystemHostResourceStore, FilesystemUserMediaStore};
use tt_adapter_observability::devtools::BackendLogStore;
use tt_adapter_observability::llm_api_logs::{LlmApiLogStore, LoggingChatCompletionRepository};
use tt_adapter_provider_http::HttpChatCompletionRepository;
use tt_adapter_storage_core::{
    bundled_resources::DirectoryBundledResources, commit_stage::PageGeneration,
    load_tauritavern_settings_blocking,
};
use tt_adapter_workspace_shell::WorkspaceShellEngine;
use tt_application::services::{
    bundled_template_service::BundledTemplateService, host_resource_service::HostResourceService,
    user_media_service::UserMediaService,
};
use tt_domain::{
    ios_policy::{IosPolicyScope, resolve_ios_policy_activation_report},
    models::settings::TauriTavernSettings,
};
use tt_runtime::{HostDependencies, RuntimeConfig};

#[derive(Parser)]
#[command(version, about = "TauriTavern native Rust HTTP/browser host")]
struct Options {
    /// Bind only on loopback; put an HTTPS reverse proxy in front for remote access.
    #[arg(long, default_value = "127.0.0.1:8002")]
    listen: SocketAddr,
    /// The isolated TT data directory (not a running Luker data directory).
    #[arg(long)]
    data_root: PathBuf,
    /// Writable TT application storage for runtime-owned state.
    #[arg(long)]
    app_root: PathBuf,
    /// Immutable packaged resources: default/content and frontend templates.
    #[arg(long)]
    resource_root: PathBuf,
    /// Immutable built frontend; never point this at a data directory.
    #[arg(long)]
    web_root: PathBuf,
    /// Exact externally visible origin, e.g. https://tt.example.org.
    #[arg(long)]
    public_origin: url::Url,
    #[arg(long, default_value = "tt")]
    username: String,
    /// Private UTF-8 file containing a high-entropy HTTP Basic password, not a CLI secret.
    #[arg(long)]
    password_file: PathBuf,
    /// Trusted operator approval of user-configured upstream endpoints. Never exposed to JS.
    #[arg(long = "allow-endpoint")]
    allowed_endpoints: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The same buffer the native shell keeps: the backend log panel reads it, the journal still
    // receives everything through the fmt layer.
    let backend_logs = Arc::new(BackendLogStore::new());
    let backend_errors = Arc::new(backend_errors::BackendErrorHub::default());
    {
        let errors = backend_errors.clone();
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .with(backend_logs.layer())
            .with(
                tt_adapter_observability::user_errors::BackendUserErrorLayer::new(Arc::new(
                    move |message| errors.publish(message),
                )),
            )
            .init();
    }
    let options = Options::parse();
    if !matches!(options.listen.ip(), IpAddr::V4(v) if v.is_loopback())
        && !matches!(options.listen.ip(), IpAddr::V6(v) if v.is_loopback())
    {
        return Err("listen must be a loopback address; configure an HTTPS reverse proxy".into());
    }
    let startup_paths::StartupPaths {
        app_root,
        data_root,
        web_root,
        resource_root,
        archive_root,
        log_root,
    } = startup_paths::StartupPaths::resolve(
        &options.app_root,
        &options.data_root,
        &options.web_root,
        &options.resource_root,
    )
    .await?;
    let data_directory = tt_runtime::initialize_data_directory(&data_root).await?;
    let password = tokio::fs::read_to_string(&options.password_file).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if tokio::fs::metadata(&options.password_file)
            .await?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(
                "password file must not be accessible by group/other users (use mode 0600)".into(),
            );
        }
    }
    let security = Arc::new(security::Security::new(
        &options.username,
        password.trim_end_matches(['\r', '\n']),
        &options.public_origin,
    )?);
    drop(password);
    let initial_settings = TauriTavernSettings::default();
    let startup_settings =
        load_tauritavern_settings_blocking(&data_root.join("default-user"), &initial_settings)?;
    let ios_policy = resolve_ios_policy_activation_report(IosPolicyScope::Ignored, None, None)?;
    let config = RuntimeConfig {
        app_root: app_root.clone(),
        tauritavern_settings: startup_settings.clone(),
        initial_tauritavern_settings: initial_settings,
        ios_policy: ios_policy.clone(),
        max_commit_frame_bytes: 4 * 1024 * 1024,
    };
    let llm_api_logs = Arc::new(LlmApiLogStore::new(
        tokio::runtime::Handle::current(),
        log_root,
    ));
    llm_api_logs.apply_settings(startup_settings.dev.effective_llm_api_keep());
    let bundled_resources = Arc::new(DirectoryBundledResources::new(resource_root));
    let extension_tools = Arc::new(tt_adapter_extension::tools::AgentExtensionTools::default());
    let http_clients = Arc::new(HttpClientPool::new(concat!(
        "TauriTavern/",
        env!("CARGO_PKG_VERSION"),
        " (native-web)"
    )));
    let services = Arc::new(
        tt_runtime::build(
            &data_directory,
            &config,
            HostDependencies {
                chat_completion_repository: Arc::new(LoggingChatCompletionRepository::new(
                    Arc::new(HttpChatCompletionRepository::new(http_clients.clone())),
                    llm_api_logs.clone(),
                )),
                http_client_pool: http_clients.clone(),
                page_generation: Arc::new(PageGeneration::default()),
                bundled_resources: bundled_resources.clone(),
                generation_background_runtime: None,
                workspace_shell: Arc::new(WorkspaceShellEngine),
                extension_tools: extension_tools.clone(),
            },
        )
        .await?,
    );
    for endpoint in options.allowed_endpoints {
        // Operator approval replaces the native confirmation dialog; JS can never add grants.
        let endpoint = tt_domain::models::endpoint_url::parse_user_http_endpoint(&endpoint)?;
        services
            .user_endpoint_access_service
            .grant(endpoint.to_string())
            .await;
    }
    services
        .content_service
        .initialize_default_content("default-user")
        .await?;
    services
        .settings_service
        .schedule_chat_backup_reconciliation();
    let cancel = CancellationToken::new();
    let history_worker = tokio::spawn(
        services
            .chat_history_coordinator
            .clone()
            .run(cancel.clone()),
    );
    let retention_worker = tokio::spawn(
        services
            .agent_run_retention_automation_service
            .clone()
            .run(cancel.clone()),
    );
    let files = files::HostFiles::new(&app_root).await?;
    let archives = Arc::new(
        tt_application::services::data_archive_service::DataArchiveService::new(
            services.database_service.clone(),
            Arc::new(tt_application::services::data_archive_service::DataArchiveJobRegistry::new()),
            tokio::runtime::Handle::current(),
            Arc::new(tt_adapter_archive::FileDataArchiveExecutor {
                prepare_personas: tt_adapter_media::persona_cards::migrate_personas,
                read_personas: tt_adapter_media::persona_cards::read_personas,
            }),
            Arc::new(tt_adapter_archive::FileDataArchiveGateway::new(
                data_root.clone(),
                archive_root.join("imports"),
                archive_root.join("exports"),
            )),
            Arc::new(tt_adapter_storage_core::DataDirectoryDataRootInitializer),
            services.data_change_reconciler.clone(),
        ),
    );
    let host = Arc::new(host::Host {
        updates: Arc::new(
            tt_application::services::update_service::UpdateService::new(
                Arc::new(tt_adapter_http::GitHubUpdateRepository::new(http_clients)),
                env!("CARGO_PKG_VERSION"),
                None::<String>,
            ),
        ),
        downloads: Default::default(),
        archives,
        backend_errors,
        backend_logs,
        llm_api_logs,
        services,
        files,
        extension_tools,
        security,
        ios_policy,
        templates: Arc::new(BundledTemplateService::new(bundled_resources)),
        media: Arc::new(UserMediaService::new(Arc::new(
            FilesystemUserMediaStore::from_data_root(&data_root),
        ))),
        host_resources: Arc::new(HostResourceService::new(
            startup_settings.avatar_persona_original_images_enabled,
            Arc::new(FilesystemHostResourceStore::from_data_root(&data_root)),
        )),
        index_html: host::inject_host_identity(
            &tokio::fs::read_to_string(web_root.join("index.html")).await?,
        )?
        .into(),
        static_files: tower_http::services::ServeDir::new(web_root)
            .append_index_html_on_directories(false),
    });
    let error_hub = host.backend_errors.clone();
    let error_cancel = cancel.clone();
    let error_worker = tokio::spawn(async move { error_hub.run(error_cancel).await });
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    tracing::info!(listen = %options.listen, origin = %host.security.public_origin, "Native TT browser host ready");
    let expiry_cancel = cancel.clone();
    let expiry_host = host.clone();
    let expiry_worker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = expiry_cancel.cancelled() => break,
                _ = interval.tick() => {
                    for session in expiry_host.security.expired_sessions() {
                        host::close_session(&expiry_host, session).await;
                    }
                    expiry_host.downloads.expire().await;
                }
            }
        }
    });
    let result = axum::serve(listener, host::router(host))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    cancel.cancel();
    history_worker.await?;
    retention_worker.await?;
    expiry_worker.await?;
    error_worker.await?;
    result?;
    Ok(())
}
