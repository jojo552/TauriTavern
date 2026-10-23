use std::sync::Arc;

use crate::{HostDependencies, RuntimeConfig};
use tt_adapter_extension::FileExtensionRepository;
use tt_adapter_media::{
    FileAvatarRepository, FileBackgroundRepository, FileImageMetadataRepository,
};
use tt_adapter_provider_http::{
    HttpEmbeddingRepository, HttpProviderMetadataRepository, HttpSearxngSearchRepository,
    HttpStableDiffusionRepository, HttpTranslateRepository, HttpTtsRepository,
};
use tt_adapter_storage_core::{
    DataDirectory, FileAssetRepository, FileChatRepository, FileContentRepository,
    FileExtensionStoreRepository, FileGroupRepository, FileLlmConnectionRepository,
    FileMcpServerRepository, FilePresetRepository, FilePromptCacheRepository,
    FileQuickReplyRepository, FileSecretRepository, FileSettingsRepository, FileThemeRepository,
    FileUserDirectoryRepository, FileUserEndpointGrantRepository, FileUserRepository,
    chat_directory_identity::new_shared_chat_alias_store_for_user_dir,
};
use tt_adapter_storage_userdata::FileAgentProfileRepository;
use tt_adapter_storage_userdata::FileAgentRepository;
use tt_adapter_storage_userdata::FileCharacterRepository;
use tt_adapter_storage_userdata::FileWorldInfoRepository;
use tt_adapter_storage_userdata::{FileSkillRepository, FileSpriteRepository};
use tt_adapter_tokenization::MiktikTokenizerRepository;
use tt_adapter_vector::{CandleLocalEmbeddingRepository, RedbVectorRepository};
use tt_domain::errors::DomainError;
use tt_domain::models::settings::UserSettings;
use tt_ports::repositories::agent_invocation_repository::AgentInvocationRepository;
use tt_ports::repositories::agent_profile_repository::AgentProfileRepository;
use tt_ports::repositories::agent_profile_storage_health_repository::AgentProfileStorageHealthRepository;
use tt_ports::repositories::agent_run_repository::AgentRunRepository;
use tt_ports::repositories::agent_session_repository::AgentSessionRepository;
use tt_ports::repositories::agent_workspace_lifecycle_repository::AgentWorkspaceLifecycleRepository;
use tt_ports::repositories::asset_repository::AssetRepository;
use tt_ports::repositories::avatar_repository::AvatarRepository;
use tt_ports::repositories::background_repository::BackgroundRepository;
use tt_ports::repositories::character_repository::CharacterRepository;
use tt_ports::repositories::chat_commit_repository::ChatCommitRepository;
use tt_ports::repositories::chat_completion_repository::ChatCompletionRepository;
use tt_ports::repositories::chat_repository::ChatRepository;
use tt_ports::repositories::content_repository::ContentRepository;
use tt_ports::repositories::extension_repository::ExtensionRepository;
use tt_ports::repositories::extension_store_repository::ExtensionStoreRepository;
use tt_ports::repositories::group_chat_repository::GroupChatRepository;
use tt_ports::repositories::group_repository::GroupRepository;
use tt_ports::repositories::image_metadata_repository::ImageMetadataRepository;
use tt_ports::repositories::llm_connection_repository::LlmConnectionRepository;
use tt_ports::repositories::mcp_server_repository::McpServerRepository;
use tt_ports::repositories::preset_repository::PresetRepository;
use tt_ports::repositories::prompt_cache_repository::PromptCacheRepository;
use tt_ports::repositories::provider_metadata_repository::ProviderMetadataRepository;
use tt_ports::repositories::quick_reply_repository::QuickReplyRepository;
use tt_ports::repositories::searxng_search_repository::SearxngSearchRepository;
use tt_ports::repositories::secret_repository::SecretRepository;
use tt_ports::repositories::settings_repository::SettingsRepository;
use tt_ports::repositories::skill_repository::SkillRepository;
use tt_ports::repositories::sprite_repository::SpriteRepository;
use tt_ports::repositories::stable_diffusion_repository::StableDiffusionRepository;
use tt_ports::repositories::theme_repository::ThemeRepository;
use tt_ports::repositories::tokenizer_repository::TokenizerRepository;
use tt_ports::repositories::translate_repository::TranslateRepository;
use tt_ports::repositories::tts_repository::TtsRepository;
use tt_ports::repositories::user_directory_repository::UserDirectoryRepository;
use tt_ports::repositories::user_endpoint_grant_repository::UserEndpointGrantRepository;
use tt_ports::repositories::user_repository::UserRepository;
use tt_ports::repositories::vector_repository::{
    LocalEmbeddingRepository, RemoteEmbeddingRepository, VectorRepository,
};
use tt_ports::repositories::workspace_repository::WorkspaceRepository;
use tt_ports::repositories::world_info_repository::WorldInfoRepository;
use tt_ports::settings::ChatBackupRuntime;

pub(crate) struct Repositories {
    pub(crate) character_repository: Arc<dyn CharacterRepository>,
    pub(crate) chat_repository: Arc<dyn ChatRepository>,
    pub(crate) group_chat_repository: Arc<dyn GroupChatRepository>,
    pub(crate) chat_commit_repository: Arc<dyn ChatCommitRepository>,
    pub(crate) chat_backup_runtime: Arc<dyn ChatBackupRuntime>,
    pub(crate) user_repository: Arc<dyn UserRepository>,
    pub(crate) settings_repository: Arc<dyn SettingsRepository>,
    pub(crate) prompt_cache_repository: Arc<dyn PromptCacheRepository>,
    pub(crate) user_directory_repository: Arc<dyn UserDirectoryRepository>,
    pub(crate) secret_repository: Arc<dyn SecretRepository>,
    pub(crate) skill_repository: Arc<dyn SkillRepository>,
    pub(crate) sprite_repository: Arc<dyn SpriteRepository>,
    pub(crate) content_repository: Arc<dyn ContentRepository>,
    pub(crate) asset_repository: Arc<dyn AssetRepository>,
    pub(crate) extension_repository: Arc<dyn ExtensionRepository>,
    pub(crate) extension_store_repository: Arc<dyn ExtensionStoreRepository>,
    pub(crate) avatar_repository: Arc<dyn AvatarRepository>,
    pub(crate) group_repository: Arc<dyn GroupRepository>,
    pub(crate) background_repository: Arc<dyn BackgroundRepository>,
    pub(crate) image_metadata_repository: Arc<dyn ImageMetadataRepository>,
    pub(crate) theme_repository: Arc<dyn ThemeRepository>,
    pub(crate) preset_repository: Arc<dyn PresetRepository>,
    pub(crate) quick_reply_repository: Arc<dyn QuickReplyRepository>,
    pub(crate) agent_profile_repository: Arc<dyn AgentProfileRepository>,
    pub(crate) agent_profile_storage_health_repository:
        Arc<dyn AgentProfileStorageHealthRepository>,
    pub(crate) agent_run_repository: Arc<dyn AgentRunRepository>,
    pub(crate) agent_session_repository: Arc<dyn AgentSessionRepository>,
    pub(crate) agent_invocation_repository: Arc<dyn AgentInvocationRepository>,
    pub(crate) agent_workspace_lifecycle_repository: Arc<dyn AgentWorkspaceLifecycleRepository>,
    pub(crate) llm_connection_repository: Arc<dyn LlmConnectionRepository>,
    pub(crate) user_endpoint_grant_repository: Arc<dyn UserEndpointGrantRepository>,
    pub(crate) mcp_server_repository: Arc<dyn McpServerRepository>,
    pub(crate) workspace_repository: Arc<dyn WorkspaceRepository>,
    pub(crate) chat_completion_repository: Arc<dyn ChatCompletionRepository>,
    pub(crate) provider_metadata_repository: Arc<dyn ProviderMetadataRepository>,
    pub(crate) searxng_search_repository: Arc<dyn SearxngSearchRepository>,
    pub(crate) tokenizer_repository: Arc<dyn TokenizerRepository>,
    pub(crate) stable_diffusion_repository: Arc<dyn StableDiffusionRepository>,
    pub(crate) translate_repository: Arc<dyn TranslateRepository>,
    pub(crate) tts_repository: Arc<dyn TtsRepository>,
    pub(crate) world_info_repository: Arc<dyn WorldInfoRepository>,
    pub(crate) vector_repository: Arc<dyn VectorRepository>,
    pub(crate) remote_embedding_repository: Arc<dyn RemoteEmbeddingRepository>,
    pub(crate) local_embedding_repository: Arc<dyn LocalEmbeddingRepository>,
}

pub(crate) async fn build(
    data_directory: &DataDirectory,
    config: &RuntimeConfig,
    dependencies: &HostDependencies,
) -> Result<Repositories, DomainError> {
    let http_client_pool = dependencies.http_client_pool.clone();
    let page_generation = dependencies.page_generation.clone();
    let chat_backup_settings = config.tauritavern_settings.chat_backups;
    let data_root = data_directory.root().to_path_buf();
    let default_user_dir = data_directory.default_user().to_path_buf();
    let chat_aliases = new_shared_chat_alias_store_for_user_dir(data_directory.default_user());

    let file_chat_repository = Arc::new(FileChatRepository::with_chat_aliases_and_backup_settings(
        data_directory.characters().to_path_buf(),
        data_directory.chats().to_path_buf(),
        data_directory.group_chats().to_path_buf(),
        data_directory.backups().to_path_buf(),
        chat_aliases.clone(),
        chat_backup_settings,
        page_generation.clone(),
        config.max_commit_frame_bytes,
    ));
    file_chat_repository
        .cleanup_orphaned_chat_commit_staging()
        .await;
    let character_repository: Arc<dyn CharacterRepository> =
        Arc::new(FileCharacterRepository::with_chat_repository(
            data_directory.characters().to_path_buf(),
            data_directory.chats().to_path_buf(),
            data_directory.default_avatar().to_path_buf(),
            chat_aliases,
            file_chat_repository.clone(),
        ));
    let chat_repository: Arc<dyn ChatRepository> = file_chat_repository.clone();
    let chat_commit_repository: Arc<dyn ChatCommitRepository> = file_chat_repository.clone();
    let chat_backup_runtime: Arc<dyn ChatBackupRuntime> = file_chat_repository.clone();
    let group_chat_repository: Arc<dyn GroupChatRepository> = file_chat_repository;

    let user_repository: Arc<dyn UserRepository> = Arc::new(FileUserRepository::new(
        data_directory.user_data().to_path_buf(),
    ));

    let default_user_settings: UserSettings =
        tt_adapter_storage_core::bundled_resources::read_resource_json(
            dependencies.bundled_resources.clone(),
            "default/content/settings.json",
        )
        .await?;
    let avatar_repository: Arc<dyn AvatarRepository> = Arc::new(FileAvatarRepository::new(
        default_user_dir.join("User Avatars"),
    ));

    let settings_repository = Arc::new(FileSettingsRepository::new(
        data_directory.settings().to_path_buf(),
        default_user_settings,
        config.initial_tauritavern_settings.clone(),
        page_generation.clone(),
        config.max_commit_frame_bytes,
    ));
    settings_repository.cleanup_orphaned_commit_staging().await;
    // Materialize settings sections before background sync can scan the data directory.
    settings_repository.load_user_settings().await?;
    let persona_root = default_user_dir.clone();
    tokio::task::spawn_blocking(move || {
        tt_adapter_media::persona_cards::migrate_personas(&persona_root, &persona_root)
    })
    .await
    .map_err(|error| DomainError::InternalError(error.to_string()))??;

    let prompt_cache_repository: Arc<dyn PromptCacheRepository> = Arc::new(
        FilePromptCacheRepository::new(data_root.join("_tauritavern").join("prompt-cache")),
    );

    let user_directory_repository: Arc<dyn UserDirectoryRepository> =
        Arc::new(FileUserDirectoryRepository::new(data_root.clone()));

    let secret_repository: Arc<dyn SecretRepository> = Arc::new(FileSecretRepository::new(
        default_user_dir.join("secrets.json"),
    ));
    let skill_repository: Arc<dyn SkillRepository> = Arc::new(FileSkillRepository::new(
        data_root.join("_tauritavern").join("skills"),
        tt_adapter_storage_core::builtin_skills::materialize_builtin_skills(
            dependencies.bundled_resources.clone(),
            &data_root,
        )
        .await?,
    ));
    let sprite_repository: Arc<dyn SpriteRepository> = Arc::new(FileSpriteRepository::new(
        data_directory.characters().to_path_buf(),
    ));

    let content_repository: Arc<dyn ContentRepository> = Arc::new(FileContentRepository::new(
        dependencies.bundled_resources.clone(),
        data_root.clone(),
        default_user_dir.clone(),
    ));

    let asset_repository: Arc<dyn AssetRepository> = Arc::new(FileAssetRepository::new(
        default_user_dir.clone(),
        default_user_dir.join("assets"),
        default_user_dir.join("characters"),
    ));

    let extension_repository: Arc<dyn ExtensionRepository> =
        Arc::new(FileExtensionRepository::new(
            default_user_dir.join("extensions"),
            data_directory.global_extensions().to_path_buf(),
            data_directory.extension_sources().to_path_buf(),
            http_client_pool.clone(),
        ));

    let extension_store_repository = Arc::new(FileExtensionStoreRepository::new(
        data_root.join("_tauritavern").join("extension-store"),
        data_root.join(".staging").join("extension-store"),
        page_generation.clone(),
        config.max_commit_frame_bytes,
    ));
    extension_store_repository
        .cleanup_orphaned_commit_staging()
        .await;

    let group_repository: Arc<dyn GroupRepository> = Arc::new(FileGroupRepository::new(
        data_directory.groups().to_path_buf(),
        data_directory.group_chats().to_path_buf(),
    ));

    let background_repository: Arc<dyn BackgroundRepository> = Arc::new(
        FileBackgroundRepository::new(data_directory.default_user().join("backgrounds")),
    );
    let image_metadata_repository: Arc<dyn ImageMetadataRepository> =
        Arc::new(FileImageMetadataRepository::new(
            default_user_dir.clone(),
            data_directory.default_user().join("backgrounds"),
        ));

    let theme_repository: Arc<dyn ThemeRepository> =
        Arc::new(FileThemeRepository::new(default_user_dir.join("themes")));

    let preset_repository: Arc<dyn PresetRepository> = Arc::new(FilePresetRepository::new(
        dependencies.bundled_resources.clone(),
        default_user_dir.clone(),
        content_repository.clone(),
    ));
    let quick_reply_repository: Arc<dyn QuickReplyRepository> = Arc::new(
        FileQuickReplyRepository::new(data_directory.default_user().join("QuickReplies")),
    );

    let agent_profile_file_repository = Arc::new(FileAgentProfileRepository::new(
        data_root.join("_tauritavern").join("agent-profiles"),
    ));
    let agent_profile_repository: Arc<dyn AgentProfileRepository> =
        agent_profile_file_repository.clone();
    let agent_profile_storage_health_repository: Arc<dyn AgentProfileStorageHealthRepository> =
        agent_profile_file_repository;

    let llm_connection_repository: Arc<dyn LlmConnectionRepository> = Arc::new(
        FileLlmConnectionRepository::new(data_root.join("_tauritavern").join("llm-connections")),
    );
    let user_endpoint_grant_repository: Arc<dyn UserEndpointGrantRepository> =
        Arc::new(FileUserEndpointGrantRepository::new(
            config
                .app_root
                .join("security")
                .join("user-endpoint-grants.json"),
        ));
    let mcp_server_repository: Arc<dyn McpServerRepository> = Arc::new(
        FileMcpServerRepository::new(data_root.join("_tauritavern").join("mcp")),
    );

    let file_agent_repository = Arc::new(FileAgentRepository::new(
        data_root.join("_tauritavern").join("agent-workspaces"),
    ));
    let agent_run_repository: Arc<dyn AgentRunRepository> = file_agent_repository.clone();
    let agent_session_repository: Arc<dyn AgentSessionRepository> = file_agent_repository.clone();
    let agent_invocation_repository: Arc<dyn AgentInvocationRepository> =
        file_agent_repository.clone();
    let workspace_repository: Arc<dyn WorkspaceRepository> = file_agent_repository.clone();
    let agent_workspace_lifecycle_repository: Arc<dyn AgentWorkspaceLifecycleRepository> =
        file_agent_repository;

    let chat_completion_repository = dependencies.chat_completion_repository.clone();
    let provider_metadata_repository: Arc<dyn ProviderMetadataRepository> = Arc::new(
        HttpProviderMetadataRepository::new(http_client_pool.clone()),
    );
    let searxng_search_repository: Arc<dyn SearxngSearchRepository> =
        Arc::new(HttpSearxngSearchRepository::new(http_client_pool.clone()));

    let tokenizer_cache_dir = data_root.join("_cache").join("tokenizers");
    let tokenizer_repository: Arc<dyn TokenizerRepository> = Arc::new(
        MiktikTokenizerRepository::new(tokenizer_cache_dir, http_client_pool.clone()),
    );

    let stable_diffusion_repository: Arc<dyn StableDiffusionRepository> =
        Arc::new(HttpStableDiffusionRepository::new(
            http_client_pool.clone(),
            default_user_dir.join("user").join("workflows"),
        ));

    let translate_repository: Arc<dyn TranslateRepository> =
        Arc::new(HttpTranslateRepository::new(http_client_pool.clone()));
    let tts_repository: Arc<dyn TtsRepository> =
        Arc::new(HttpTtsRepository::new(http_client_pool.clone()));

    let world_info_repository = Arc::new(FileWorldInfoRepository::new(
        data_directory.default_user().join("worlds"),
        page_generation,
        config.max_commit_frame_bytes,
    ));
    world_info_repository
        .cleanup_orphaned_commit_staging()
        .await;

    let vector_root = default_user_dir.join("vectors");
    let vector_repository: Arc<dyn VectorRepository> = Arc::new(RedbVectorRepository::new(
        vector_root.join("tauritavern-v1.redb"),
    ));
    let remote_embedding_repository: Arc<dyn RemoteEmbeddingRepository> =
        Arc::new(HttpEmbeddingRepository::new(http_client_pool));
    let local_embedding_repository: Arc<dyn LocalEmbeddingRepository> = Arc::new(
        CandleLocalEmbeddingRepository::new(data_root.join("_cache").join("embedding-models")),
    );

    Ok(Repositories {
        character_repository,
        chat_repository,
        group_chat_repository,
        chat_commit_repository,
        chat_backup_runtime,
        user_repository,
        settings_repository,
        prompt_cache_repository,
        user_directory_repository,
        secret_repository,
        skill_repository,
        sprite_repository,
        content_repository,
        asset_repository,
        extension_repository,
        extension_store_repository,
        avatar_repository,
        group_repository,
        background_repository,
        image_metadata_repository,
        theme_repository,
        preset_repository,
        quick_reply_repository,
        agent_profile_repository,
        agent_profile_storage_health_repository,
        agent_run_repository,
        agent_session_repository,
        agent_invocation_repository,
        agent_workspace_lifecycle_repository,
        llm_connection_repository,
        user_endpoint_grant_repository,
        mcp_server_repository,
        workspace_repository,
        chat_completion_repository,
        provider_metadata_repository,
        searxng_search_repository,
        tokenizer_repository,
        stable_diffusion_repository,
        translate_repository,
        tts_repository,
        world_info_repository,
        vector_repository,
        remote_embedding_repository,
        local_embedding_repository,
    })
}
