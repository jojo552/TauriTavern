use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::TimeZone;
use serde::Serialize;
use tauri::{AppHandle, Emitter, async_runtime::JoinHandle};
use tokio::sync::broadcast;
use tt_adapter_observability::devtools::{BACKEND_LOG_EVENT, BackendLogEntry, BackendLogStore};
use tt_adapter_observability::llm_api_logs::{
    LLM_API_LOG_EVENT, LlmApiLogEntryPreview, LlmApiLogEntryRaw, LlmApiLogIndexEntry,
    LlmApiLogStore, LlmApiRawKind,
};

use crate::infrastructure::logging::dev_bundle::{DevLogBundleInput, export_dev_log_bundle};
use crate::infrastructure::paths::RuntimePaths;
use crate::infrastructure::staging;
use tt_application::dto::dev_observability_dto::{
    BackendLogEntryDto, DevBundleVersionDto, FrontendLogEntrySnapshotDto, LlmApiLogIndexEntryDto,
    LlmApiLogPreviewDto, LlmApiLogRawDto, LlmApiRawKindDto,
};
use tt_domain::errors::DomainError;

pub struct DevObservabilityHub {
    app_handle: AppHandle,
    runtime_paths: RuntimePaths,
    backend_logs: Arc<BackendLogStore>,
    llm_api_logs: Arc<LlmApiLogStore>,
    backend_stream: WindowEventForward,
    llm_api_stream: WindowEventForward,
}

/// Forwards one store broadcast to the window as a Tauri event while the frontend keeps the
/// stream enabled. The frontend reference-counts its subscribers and toggles this once.
#[derive(Default)]
struct WindowEventForward(Mutex<Option<JoinHandle<()>>>);

impl WindowEventForward {
    fn set<T>(
        &self,
        enabled: bool,
        app_handle: &AppHandle,
        event: &'static str,
        subscribe: impl FnOnce() -> broadcast::Receiver<T>,
    ) where
        T: Clone + Serialize + Send + 'static,
    {
        let mut forward = self.0.lock().expect("window event forward poisoned");
        if enabled == forward.is_some() {
            return;
        }
        if let Some(task) = forward.take() {
            task.abort();
            return;
        }

        let mut receiver = subscribe();
        let app_handle = app_handle.clone();
        *forward = Some(tauri::async_runtime::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(entry) => {
                        let _ = app_handle.emit(event, entry);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Visible in the panel itself; tail/index still hold the skipped entries.
                        tracing::warn!(
                            "{event} stream skipped {skipped} entries; reopen the panel to resync"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }));
    }
}

impl DevObservabilityHub {
    pub fn new(
        app_handle: AppHandle,
        runtime_paths: RuntimePaths,
        backend_logs: Arc<BackendLogStore>,
        llm_api_logs: Arc<LlmApiLogStore>,
    ) -> Self {
        Self {
            app_handle,
            runtime_paths,
            backend_logs,
            llm_api_logs,
            backend_stream: WindowEventForward::default(),
            llm_api_stream: WindowEventForward::default(),
        }
    }

    pub fn set_backend_stream_enabled(&self, enabled: bool) {
        let logs = &self.backend_logs;
        self.backend_stream.set(enabled, &self.app_handle, BACKEND_LOG_EVENT, || logs.subscribe());
    }

    pub fn tail_backend_logs(&self, limit: usize) -> Vec<BackendLogEntryDto> {
        self.backend_logs
            .tail(limit)
            .into_iter()
            .map(backend_log_entry_dto)
            .collect()
    }

    pub fn set_llm_api_stream_enabled(&self, enabled: bool) {
        let logs = &self.llm_api_logs;
        self.llm_api_stream.set(enabled, &self.app_handle, LLM_API_LOG_EVENT, || logs.subscribe());
    }

    pub fn apply_llm_api_log_retention(&self, keep: u32) {
        self.llm_api_logs.apply_settings(keep);
    }

    pub fn tail_llm_api_index(&self, limit: usize) -> Vec<LlmApiLogIndexEntryDto> {
        self.llm_api_logs
            .tail_index(limit)
            .into_iter()
            .map(llm_api_log_index_entry_dto)
            .collect()
    }

    pub async fn get_llm_api_preview(&self, id: u64) -> Result<LlmApiLogPreviewDto, DomainError> {
        let entry = self.llm_api_logs.get_preview(id).await.map_err(|error| {
            DomainError::InternalError(format!("Failed to read LLM API log preview: {error}"))
        })?;

        Ok(llm_api_log_preview_dto(entry))
    }

    pub async fn get_llm_api_raw(&self, id: u64) -> Result<LlmApiLogRawDto, DomainError> {
        let entry = self.llm_api_logs.get_raw(id).await.map_err(|error| {
            DomainError::InternalError(format!("Failed to read LLM API log raw: {error}"))
        })?;

        Ok(llm_api_log_raw_dto(entry))
    }

    pub async fn export_bundle(
        &self,
        frontend_entries: Vec<FrontendLogEntrySnapshotDto>,
        version: DevBundleVersionDto,
    ) -> Result<PathBuf, DomainError> {
        let backend_tail = self.tail_backend_logs(800);
        let meta = DevLogBundleMeta {
            exported_at: chrono::Utc::now().to_rfc3339(),
            os: crate::platform::identity::HOST_IDENTITY
                .platform
                .as_str()
                .to_string(),
            arch: std::env::consts::ARCH.to_string(),
            runtime_paths: DevLogBundleRuntimePaths {
                data_root: self.runtime_paths.data_root.to_string_lossy().to_string(),
                log_root: self.runtime_paths.log_root.to_string_lossy().to_string(),
            },
            version,
        };

        let meta_json = serde_json::to_string_pretty(&meta).map_err(|error| {
            DomainError::InternalError(format!("Failed to serialize dev bundle metadata: {error}"))
        })?;

        let frontend_logs_jsonl = format_frontend_jsonl(&frontend_entries)?;
        let backend_logs_tail_text = format_backend_tail(&backend_tail);
        let readme_text = bundle_readme();

        let output_path =
            staging::create_staged_file(&self.app_handle, "dev-bundle", "zip").await?;
        let staged_path = output_path.clone();
        let runtime_paths = self.runtime_paths.clone();

        let result = tauri::async_runtime::spawn_blocking(move || {
            export_dev_log_bundle(
                output_path,
                &runtime_paths,
                DevLogBundleInput {
                    meta_json,
                    readme_text,
                    frontend_logs_jsonl,
                    backend_logs_tail_text,
                },
            )
        })
        .await
        .map_err(|error| {
            DomainError::InternalError(format!("Export bundle task join error: {error}"))
        })?;
        if result.is_err()
            && let Err(error) = staging::discard_file(&staged_path).await
        {
            tracing::warn!(%error, "Failed to discard incomplete dev bundle");
        }
        result
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevLogBundleMeta {
    exported_at: String,
    os: String,
    arch: String,
    runtime_paths: DevLogBundleRuntimePaths,
    version: DevBundleVersionDto,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DevLogBundleRuntimePaths {
    data_root: String,
    log_root: String,
}

fn backend_log_entry_dto(entry: BackendLogEntry) -> BackendLogEntryDto {
    BackendLogEntryDto {
        id: entry.id,
        timestamp_ms: entry.timestamp_ms,
        level: entry.level,
        target: entry.target,
        message: entry.message,
    }
}

fn llm_api_raw_kind_dto(kind: LlmApiRawKind) -> LlmApiRawKindDto {
    match kind {
        LlmApiRawKind::Json => LlmApiRawKindDto::Json,
        LlmApiRawKind::Sse => LlmApiRawKindDto::Sse,
    }
}

fn llm_api_log_index_entry_dto(entry: LlmApiLogIndexEntry) -> LlmApiLogIndexEntryDto {
    LlmApiLogIndexEntryDto {
        id: entry.id,
        timestamp_ms: entry.timestamp_ms,
        level: entry.level,
        ok: entry.ok,
        source: entry.source,
        model: entry.model,
        endpoint: entry.endpoint,
        duration_ms: entry.duration_ms,
        stream: entry.stream,
    }
}

fn llm_api_log_preview_dto(entry: LlmApiLogEntryPreview) -> LlmApiLogPreviewDto {
    LlmApiLogPreviewDto {
        id: entry.id,
        timestamp_ms: entry.timestamp_ms,
        level: entry.level,
        ok: entry.ok,
        source: entry.source,
        model: entry.model,
        endpoint: entry.endpoint,
        duration_ms: entry.duration_ms,
        stream: entry.stream,
        error_message: entry.error_message,
        request_readable: entry.request_readable,
        response_readable: entry.response_readable,
        response_raw_kind: entry.response_raw_kind.map(llm_api_raw_kind_dto),
    }
}

fn llm_api_log_raw_dto(entry: LlmApiLogEntryRaw) -> LlmApiLogRawDto {
    LlmApiLogRawDto {
        id: entry.id,
        request_raw: entry.request_raw,
        response_raw: entry.response_raw,
        response_raw_kind: entry.response_raw_kind.map(llm_api_raw_kind_dto),
    }
}

fn format_log_timestamp(ms: i64) -> String {
    let Some(ts) = chrono::Utc.timestamp_millis_opt(ms).single() else {
        return format!("Invalid({ms})");
    };

    ts.to_rfc3339()
}

fn format_backend_tail(entries: &[BackendLogEntryDto]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!(
            "[{}] [{}] [{}] {}\n",
            format_log_timestamp(entry.timestamp_ms),
            entry.level,
            entry.target,
            entry.message
        ));
    }

    out
}

fn format_frontend_jsonl(entries: &[FrontendLogEntrySnapshotDto]) -> Result<String, DomainError> {
    let mut out = String::new();

    for entry in entries {
        let line = serde_json::to_string(entry).map_err(|error| {
            DomainError::InternalError(format!("Failed to serialize frontend log entry: {error}"))
        })?;
        out.push_str(&line);
        out.push('\n');
    }

    Ok(out)
}

fn bundle_readme() -> String {
    [
        "TauriTavern dev bundle (for bug reports)",
        "",
        "- frontend/logs.jsonl: preview only (truncated/summarized).",
        "- backend/*.log: full backend file logs (may include forwarded frontend logs).",
        "- llm-api/*: LLM API request/response raw logs (may contain prompts/responses).",
        "- settings/*: app settings snapshot (secrets are not included).",
        "",
        "Review files before sharing.",
        "",
    ]
    .join("\n")
}
