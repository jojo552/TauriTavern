mod adapters;
mod services;

use std::path::Path;

use tauri::AppHandle;

use tt_adapter_storage_core::file_system::DataDirectory;
use tt_domain::errors::DomainError;

use super::{AppServices, StartupProfile};

pub(super) async fn initialize_data_directory(
    data_root: &Path,
) -> Result<DataDirectory, DomainError> {
    tt_runtime::initialize_data_directory(data_root).await
}

pub(super) async fn build_services(
    app_handle: &AppHandle,
    data_directory: &DataDirectory,
    startup_profile: &StartupProfile,
) -> Result<AppServices, DomainError> {
    if let Err(error) = crate::infrastructure::staging::clear_staging_root(app_handle).await {
        tracing::warn!(%error, "Failed to clear files left by the previous session");
    }
    services::build(app_handle, data_directory, startup_profile).await
}
