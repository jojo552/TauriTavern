//! Data archive import/export and per-user backups for the browser host.
//!
//! The archive service and executor are the shared ones. Only file transport differs from the
//! native host: imports start from a file the page staged, and finished archives are handed to the
//! browser as a one-shot download instead of a native save dialog.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tt_application::services::data_archive_service::DataArchiveService;

use super::args::Args;
use super::{json, unsupported};
use crate::error::CommandError;
use crate::host::Host;

const COMMANDS: &[&str] = &[
    "import_data_archive",
    "start_export_data_archive",
    "get_data_archive_job_status",
    "cancel_data_archive_job",
    "save_export_data_archive",
    "export_user_backup_archive",
];

pub(super) fn handles(command: &str) -> bool {
    COMMANDS.contains(&command)
}

async fn blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, tt_domain::errors::DomainError> + Send + 'static,
) -> Result<T, CommandError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| {
            CommandError::InternalServerError(format!("Archive task failed: {error}"))
        })?
        .map_err(CommandError::from)
}

pub(super) async fn dispatch(
    host: &Host,
    browser: &str,
    command: &str,
    a: &Args,
) -> Result<Value, CommandError> {
    let service: Arc<DataArchiveService> = host.archives.clone();
    match command {
        "import_data_archive" => {
            // The staged copy becomes the job's own archive (temporary = true moves it into the
            // job workspace), so it is never read back from a page-supplied location.
            let staged = host
                .files
                .validate_staged_path(&a.required::<String>("staged_path")?)
                .await?;
            let file_name: String = a.default("file_name")?;
            let job_id = blocking({
                let staged = staged.clone();
                move || service.start_import(&staged, true)
            })
            .await;
            if job_id.is_err() {
                let _ = host.files.discard_file(&staged).await;
            }
            json(json!({ "cancelled": false, "job_id": job_id?, "file_name": file_name }))
        }
        "start_export_data_archive" => json(blocking(move || service.start_export()).await?),
        "get_data_archive_job_status" => {
            let job_id: String = a.required("job_id")?;
            json(blocking(move || service.get_status(&job_id)).await?)
        }
        "cancel_data_archive_job" => {
            let job_id: String = a.required("job_id")?;
            json(blocking(move || service.cancel(&job_id)).await?)
        }
        "save_export_data_archive" => {
            let job_id: String = a.required("job_id")?;
            // Claiming disposes the artifact on the service side once handed over, as natively.
            let artifact = service.claim_export_artifact(&job_id)?;
            let url = host
                .downloads
                .register(artifact.archive_path, artifact.file_name.clone(), browser)
                .await;
            service.finish_export_delivery(&job_id)?;
            json(json!({ "url": url?, "file_name": artifact.file_name }))
        }
        "export_user_backup_archive" => {
            let handle: String = a.required("handle")?;
            let include_secrets: bool = a.default("include_secrets")?;
            let archive = service.export_user_backup(handle, include_secrets).await?;
            let url = host
                .downloads
                .register(
                    PathBuf::from(archive.archive_path),
                    archive.file_name.clone(),
                    browser,
                )
                .await?;
            json(json!({ "url": url, "file_name": archive.file_name }))
        }
        _ => Err(unsupported(command)),
    }
}
