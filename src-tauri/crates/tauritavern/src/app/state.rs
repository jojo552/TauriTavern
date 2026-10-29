use std::ops::Deref;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tt_application::services::data_archive_service::DataArchiveService;
use tt_application::services::lan_sync_service::LanSyncService;
use tt_application::services::sync_automation_service::SyncAutomationService;
use tt_application::services::tt_sync_service::TtSyncService;
use tt_application::services::update_service::UpdateService;
use tt_domain::errors::DomainError;
use tt_domain::ios_policy::IosPolicyActivationReport;
use tt_runtime::CoreServices;

use super::{HostEnvironment, StartupProfile, composition};
use crate::infrastructure::paths::RuntimePaths;

pub struct AppState {
    pub(crate) services: AppServices,
    pub(crate) lifecycle: AppLifecycle,
    pub(crate) ios_policy: IosPolicyActivationReport,
}

pub(crate) struct AppLifecycle {
    pub(crate) sync_automation_cancel: CancellationToken,
    pub(crate) agent_run_retention_automation_cancel: CancellationToken,
    pub(crate) chat_history_cancel: CancellationToken,
}

impl AppLifecycle {
    fn new() -> Self {
        Self {
            sync_automation_cancel: CancellationToken::new(),
            agent_run_retention_automation_cancel: CancellationToken::new(),
            chat_history_cancel: CancellationToken::new(),
        }
    }
}

/// Desktop/mobile capabilities augment the same business graph used by tt-server.
pub(crate) struct AppServices {
    pub(crate) core: CoreServices,
    pub(crate) lan_sync_service: Arc<LanSyncService>,
    pub(crate) tt_sync_service: Arc<TtSyncService>,
    pub(crate) sync_automation_service: Arc<SyncAutomationService>,
    pub(crate) data_archive_service: Arc<DataArchiveService>,
    pub(crate) update_service: Arc<UpdateService>,
}

impl Deref for AppServices {
    type Target = CoreServices;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl AppState {
    pub(crate) async fn new(
        environment: HostEnvironment,
        runtime_paths: RuntimePaths,
        startup_profile: StartupProfile,
    ) -> Result<Self, DomainError> {
        tracing::info!(
            "Initializing application in {:?} mode with data root: {:?}",
            runtime_paths.mode,
            runtime_paths.data_root
        );

        let data_directory =
            composition::initialize_data_directory(&runtime_paths.data_root).await?;
        let services = composition::build_services(
            &environment,
            &runtime_paths.app_root,
            &data_directory,
            &startup_profile,
        )
        .await?;

        tracing::info!("Application initialized successfully");

        Ok(Self {
            services,
            lifecycle: AppLifecycle::new(),
            ios_policy: startup_profile.ios_policy,
        })
    }
}
