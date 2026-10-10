mod archive;
mod sync;

use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tt_adapter_extension::tools::AgentExtensionTools;
use tt_adapter_http::HttpClientPool;
use tt_adapter_provider_http::HttpChatCompletionRepository;
use tt_adapter_storage_core::{DataDirectory, commit_stage::PageGeneration};
use tt_adapter_workspace_shell::WorkspaceShellEngine;
use tt_application::services::update_service::UpdateService;
use tt_domain::errors::DomainError;
use tt_runtime::{HostDependencies, RuntimeConfig};

use crate::app::{AppServices, StartupProfile};
use crate::infrastructure::bundled_resources::BundledResourceStore;
use crate::infrastructure::paths::RuntimePaths;
use tt_adapter_http::GitHubUpdateRepository;
use tt_adapter_observability::llm_api_logs::{LlmApiLogStore, LoggingChatCompletionRepository};

pub(super) async fn build(
    app_handle: &AppHandle,
    data_directory: &DataDirectory,
    startup_profile: &StartupProfile,
) -> Result<AppServices, DomainError> {
    let http_client_pool = app_handle.state::<Arc<HttpClientPool>>().inner().clone();
    let runtime_paths = app_handle.state::<RuntimePaths>();
    let config = RuntimeConfig {
        app_root: runtime_paths.app_root.clone(),
        tauritavern_settings: startup_profile.tauritavern_settings.clone(),
        initial_tauritavern_settings: crate::app::startup_profile::initial_tauritavern_settings(),
        ios_policy: startup_profile.ios_policy.clone(),
        max_commit_frame_bytes: crate::platform::ipc::COMMIT_FRAME_BYTES,
    };
    let dependencies = HostDependencies {
        http_client_pool: http_client_pool.clone(),
        page_generation: app_handle.state::<Arc<PageGeneration>>().inner().clone(),
        bundled_resources: Arc::new(BundledResourceStore::new(app_handle.clone())),
        chat_completion_repository: Arc::new(LoggingChatCompletionRepository::new(
            Arc::new(HttpChatCompletionRepository::new(http_client_pool.clone())),
            app_handle.state::<Arc<LlmApiLogStore>>().inner().clone(),
        )),
        generation_background_runtime: crate::platform::generation_background::runtime(app_handle),
        workspace_shell: Arc::new(WorkspaceShellEngine),
        extension_tools: app_handle
            .state::<Arc<AgentExtensionTools>>()
            .inner()
            .clone(),
    };
    let core = tt_runtime::build(data_directory, &config, dependencies).await?;

    // Native services share the core's mutation gate, database and reconciler.
    // Their window/dialog/event capabilities stay outside the shared runtime.
    let data_archive_service = archive::build(
        app_handle,
        core.data_change_reconciler.clone(),
        core.database_service.clone(),
    );
    let sync_services = sync::build(
        app_handle,
        data_directory,
        core.data_change_reconciler.clone(),
        &startup_profile.ios_policy,
        core.local_mutation_gate.clone(),
        core.database_service.clone(),
    );
    let update_service = Arc::new(UpdateService::new(
        Arc::new(GitHubUpdateRepository::new(http_client_pool)),
        crate::product::VERSION,
        crate::product::optional_build_value(crate::product::GIT_REVISION),
    ));

    Ok(AppServices {
        core,
        lan_sync_service: sync_services.lan_sync_service,
        tt_sync_service: sync_services.tt_sync_service,
        sync_automation_service: sync_services.sync_automation_service,
        data_archive_service,
        update_service,
    })
}
