use std::sync::Arc;

use tauri::{AppHandle, Manager};

use crate::infrastructure::paths::RuntimePaths;
use tt_adapter_archive::{FileDataArchiveExecutor, FileDataArchiveGateway};
use tt_adapter_storage_core::DataDirectoryDataRootInitializer;
use tt_application::services::data_archive_service::{DataArchiveJobRegistry, DataArchiveService};
use tt_application::services::database_service::DatabaseService;
use tt_ports::sync::DataChangeReconciler;

pub(super) fn build(
    app_handle: &AppHandle,
    data_change_reconciler: Arc<dyn DataChangeReconciler>,
    database_service: Arc<DatabaseService>,
) -> Arc<DataArchiveService> {
    // Runtime paths are resolved once at setup and never replaced; a data-root
    // change is staged in the runtime config and applied only after restart.
    let runtime_paths = app_handle.state::<RuntimePaths>();
    let file_gateway = FileDataArchiveGateway::new(
        runtime_paths.data_root.clone(),
        runtime_paths.archive_imports_root.clone(),
        runtime_paths.archive_exports_root.clone(),
    );
    Arc::new(DataArchiveService::new(
        database_service,
        Arc::new(DataArchiveJobRegistry::new()),
        tauri::async_runtime::handle().inner().clone(),
        Arc::new(FileDataArchiveExecutor {
            prepare_personas: tt_adapter_media::persona_cards::migrate_personas,
            read_personas: tt_adapter_media::persona_cards::read_personas,
        }),
        Arc::new(file_gateway),
        Arc::new(DataDirectoryDataRootInitializer),
        data_change_reconciler,
    ))
}
