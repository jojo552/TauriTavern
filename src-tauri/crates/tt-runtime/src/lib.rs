//! Shared, Tauri-free composition for native and HTTP hosts.
//!
//! This outer composition crate owns no business rules: it selects the existing
//! adapters and constructs the same application services for every host.

mod agent;
mod cache_reconciler;
pub mod extension_tools;
mod repositories;
mod services;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tt_adapter_http::HttpClientPool;
use tt_adapter_storage_core::{DataDirectory, commit_stage::PageGeneration};
use tt_domain::models::settings::TauriTavernSettings;
use tt_ports::bundled_resources::BundledResources;
use tt_ports::extension_tools::ExtensionTools;
use tt_ports::generation_background::GenerationBackgroundRuntime;
use tt_ports::repositories::chat_completion_repository::ChatCompletionRepository;
use tt_ports::workspace_shell::WorkspaceShell;

use tt_application::services::agent_profile_diagnostic_service::AgentProfileDiagnosticService;
use tt_application::services::agent_profile_service::AgentProfileService;
use tt_application::services::agent_run_history_service::AgentRunHistoryService;
use tt_application::services::agent_run_retention_automation_service::AgentRunRetentionAutomationService;
use tt_application::services::agent_runtime_service::AgentRuntimeService;
use tt_application::services::asset_service::AssetService;
use tt_application::services::avatar_service::AvatarService;
use tt_application::services::background_service::BackgroundService;
use tt_application::services::character_service::CharacterService;
use tt_application::services::chat_commit_service::ChatCommitService;
use tt_application::services::chat_completion_service::ChatCompletionService;
use tt_application::services::chat_history_coordinator::ChatHistoryCoordinator;
use tt_application::services::chat_service::ChatService;
use tt_application::services::content_service::ContentService;
use tt_application::services::database_service::DatabaseService;
use tt_application::services::extension_service::ExtensionService;
use tt_application::services::extension_store_service::ExtensionStoreService;
use tt_application::services::group_chat_service::GroupChatService;
use tt_application::services::group_service::GroupService;
use tt_application::services::image_metadata_service::ImageMetadataService;
use tt_application::services::llm_connection_service::LlmConnectionService;
use tt_application::services::mcp_service::McpService;
use tt_application::services::preset_service::PresetService;
use tt_application::services::prompt_assembly_service::PromptAssemblyService;
use tt_application::services::provider_metadata_service::ProviderMetadataService;
use tt_application::services::quick_reply_service::QuickReplyService;
use tt_application::services::searxng_search_service::SearxngSearchService;
use tt_application::services::secret_service::SecretService;
use tt_application::services::settings_service::SettingsService;
use tt_application::services::skill_service::SkillService;
use tt_application::services::sprite_service::SpriteService;
use tt_application::services::stable_diffusion_service::StableDiffusionService;
use tt_application::services::theme_service::ThemeService;
use tt_application::services::tokenization_service::TokenizationService;
use tt_application::services::translate_service::TranslateService;
use tt_application::services::tts_service::TtsService;
use tt_application::services::user_directory_service::UserDirectoryService;
use tt_application::services::user_endpoint_access_service::UserEndpointAccessService;
use tt_application::services::user_service::UserService;
use tt_application::services::vector_service::VectorService;
use tt_application::services::world_info_service::WorldInfoService;
use tt_domain::errors::DomainError;
use tt_domain::ios_policy::IosPolicyActivationReport;

/// The startup snapshot is resolved once by the host, then shared by all services.
pub struct RuntimeConfig {
    pub app_root: PathBuf,
    pub tauritavern_settings: TauriTavernSettings,
    pub initial_tauritavern_settings: TauriTavernSettings,
    pub ios_policy: IosPolicyActivationReport,
    pub max_commit_frame_bytes: u64,
}

/// Capabilities whose identity or delivery depends on the concrete host.
///
/// The HTTP pool and repository decorator must refer to the same outbound pool;
/// endpoint-grant and request-proxy changes are applied to that shared instance.
pub struct HostDependencies {
    pub http_client_pool: Arc<HttpClientPool>,
    pub page_generation: Arc<PageGeneration>,
    pub bundled_resources: Arc<dyn BundledResources>,
    pub chat_completion_repository: Arc<dyn ChatCompletionRepository>,
    pub generation_background_runtime: Option<Arc<dyn GenerationBackgroundRuntime>>,
    pub workspace_shell: Arc<dyn WorkspaceShell>,
    pub extension_tools: Arc<dyn ExtensionTools>,
}

pub struct CoreServices {
    pub character_service: Arc<CharacterService>,
    pub chat_service: Arc<ChatService>,
    pub group_chat_service: Arc<GroupChatService>,
    pub chat_history_coordinator: Arc<ChatHistoryCoordinator>,
    pub chat_commit_service: Arc<ChatCommitService>,
    pub user_service: Arc<UserService>,
    pub settings_service: Arc<SettingsService>,
    pub user_directory_service: Arc<UserDirectoryService>,
    pub secret_service: Arc<SecretService>,
    pub skill_service: Arc<SkillService>,
    pub sprite_service: Arc<SpriteService>,
    pub content_service: Arc<ContentService>,
    pub asset_service: Arc<AssetService>,
    pub extension_service: Arc<ExtensionService>,
    pub extension_store_service: Arc<ExtensionStoreService>,
    pub avatar_service: Arc<AvatarService>,
    pub group_service: Arc<GroupService>,
    pub background_service: Arc<BackgroundService>,
    pub image_metadata_service: Arc<ImageMetadataService>,
    pub theme_service: Arc<ThemeService>,
    pub preset_service: Arc<PresetService>,
    pub quick_reply_service: Arc<QuickReplyService>,
    pub agent_profile_service: Arc<AgentProfileService>,
    pub agent_profile_diagnostic_service: Arc<AgentProfileDiagnosticService>,
    pub prompt_assembly_service: Arc<PromptAssemblyService>,
    pub agent_run_history_service: Arc<AgentRunHistoryService>,
    pub agent_run_retention_automation_service: Arc<AgentRunRetentionAutomationService>,
    pub agent_runtime_service: Arc<AgentRuntimeService>,
    pub chat_completion_service: Arc<ChatCompletionService>,
    pub llm_connection_service: Arc<LlmConnectionService>,
    pub user_endpoint_access_service: Arc<UserEndpointAccessService>,
    pub mcp_service: Arc<McpService>,
    pub provider_metadata_service: Arc<ProviderMetadataService>,
    pub searxng_search_service: Arc<SearxngSearchService>,
    pub vector_service: Arc<VectorService>,
    pub tokenization_service: Arc<TokenizationService>,
    pub stable_diffusion_service: Arc<StableDiffusionService>,
    pub translate_service: Arc<TranslateService>,
    pub tts_service: Arc<TtsService>,
    pub world_info_service: Arc<WorldInfoService>,
    pub database_service: Arc<DatabaseService>,
    pub local_mutation_gate: Arc<tokio::sync::Semaphore>,
    pub data_change_reconciler: Arc<dyn tt_ports::sync::DataChangeReconciler>,
}

/// Initialize TT's existing filesystem layout; this may create directories.
pub async fn initialize_data_directory(data_root: &Path) -> Result<DataDirectory, DomainError> {
    let data_directory = DataDirectory::new(data_root.to_path_buf());
    data_directory.initialize().await?;
    Ok(data_directory)
}

/// Build the complete shared business graph without starting host background tasks.
///
/// Hosts initialize default content before announcing readiness and own shutdown
/// cancellation for retention/history workers. Each host separately composes
/// archive/update delivery and any supported sync runtime; this graph does not
/// silently simulate capabilities its host cannot provide.
pub async fn build(
    data_directory: &DataDirectory,
    config: &RuntimeConfig,
    dependencies: HostDependencies,
) -> Result<CoreServices, DomainError> {
    config
        .tauritavern_settings
        .chat_backups
        .validate()
        .map_err(|error| DomainError::InvalidData(error.message()))?;
    services::build(data_directory, config, dependencies).await
}
