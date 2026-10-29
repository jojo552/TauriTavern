mod archive;
mod sync;

use std::path::Path;
use std::sync::Arc;

use tt_adapter_provider_http::HttpChatCompletionRepository;
use tt_adapter_storage_core::DataDirectory;
use tt_adapter_workspace_shell::WorkspaceShellEngine;
use tt_application::services::update_service::UpdateService;
use tt_domain::errors::DomainError;
use tt_runtime::{HostDependencies, RuntimeConfig};

use super::HostEnvironment;
use crate::app::{AppServices, StartupProfile};
use tt_adapter_http::GitHubUpdateRepository;
use tt_adapter_observability::llm_api_logs::LoggingChatCompletionRepository;

pub(super) async fn build(
    environment: &HostEnvironment,
    app_root: &Path,
    data_directory: &DataDirectory,
    startup_profile: &StartupProfile,
) -> Result<AppServices, DomainError> {
    let http_client_pool = environment.http_client_pool.clone();
    let config = RuntimeConfig {
        app_root: app_root.to_path_buf(),
        tauritavern_settings: startup_profile.tauritavern_settings.clone(),
        initial_tauritavern_settings: crate::app::startup_profile::initial_tauritavern_settings(),
        ios_policy: startup_profile.ios_policy.clone(),
        max_commit_frame_bytes: crate::platform::ipc::COMMIT_FRAME_BYTES,
    };
    let dependencies = HostDependencies {
        http_client_pool: http_client_pool.clone(),
        page_generation: environment.page_generation.clone(),
        bundled_resources: environment.bundled_resources.clone(),
        chat_completion_repository: Arc::new(LoggingChatCompletionRepository::new(
            Arc::new(HttpChatCompletionRepository::new(http_client_pool.clone())),
            environment.llm_api_log_store.clone(),
        )),
        generation_background_runtime: environment.generation_background_runtime.clone(),
        workspace_shell: Arc::new(WorkspaceShellEngine),
        extension_tools: environment.agent_extension_tools.clone(),
    };
    let core = tt_runtime::build(data_directory, &config, dependencies).await?;

    // Native services share the core's mutation gate, database and reconciler.
    // Their window/dialog/event capabilities arrive through the host environment.
    let data_archive_service = archive::build(
        environment,
        core.data_change_reconciler.clone(),
        core.database_service.clone(),
    );
    let sync_services = sync::build(
        environment,
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
