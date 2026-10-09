use std::sync::Arc;

use async_trait::async_trait;

use tt_application::services::character_service::CharacterService;
use tt_application::services::chat_history_coordinator::ChatHistoryCoordinator;
use tt_application::services::chat_service::ChatService;
use tt_application::services::group_chat_service::GroupChatService;
use tt_application::services::group_service::GroupService;
use tt_application::services::mcp_service::McpService;
use tt_application::services::secret_service::SecretService;
use tt_application::services::settings_service::SettingsService;
use tt_domain::errors::DomainError;
use tt_ports::sync::DataChangeReconciler;

pub(crate) struct ServiceCacheReconciler {
    pub(crate) character_service: Arc<CharacterService>,
    pub(crate) chat_service: Arc<ChatService>,
    pub(crate) group_chat_service: Arc<GroupChatService>,
    pub(crate) group_service: Arc<GroupService>,
    pub(crate) secret_service: Arc<SecretService>,
    pub(crate) settings_service: Arc<SettingsService>,
    pub(crate) mcp_service: Arc<McpService>,
    pub(crate) chat_history_coordinator: Arc<ChatHistoryCoordinator>,
}

#[async_trait]
impl DataChangeReconciler for ServiceCacheReconciler {
    async fn reconcile(&self, reason: &str) -> Result<(), DomainError> {
        tracing::info!(
            reason = reason,
            "Refreshing runtime caches after external data change"
        );

        self.chat_history_coordinator.invalidate_all_pending().await;
        self.mcp_service.clear_catalog_memory();

        self.character_service.clear_cache().await?;
        self.chat_service.clear_cache().await?;
        self.group_chat_service.clear_cache().await?;
        self.group_service.clear_cache().await?;
        self.secret_service.clear_cache().await?;
        self.settings_service.reload().await?;

        Ok(())
    }
}
