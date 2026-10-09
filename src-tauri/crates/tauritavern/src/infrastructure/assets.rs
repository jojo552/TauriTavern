use std::sync::OnceLock;
use tauri::AppHandle;
#[cfg(not(embedded_resources))]
use tauri::Manager;
#[cfg(not(embedded_resources))]
use tauri::path::BaseDirectory;
#[cfg(not(embedded_resources))]
use tauri_plugin_fs::FsExt;
use tt_adapter_storage_core::bundled_resources::{
    decode_resource_text, normalize_resource_relative_path,
};
use tt_domain::errors::DomainError;

// The generated manifest is immutable; keep a parse failure as text so every caller sees it.
static DEFAULT_CONTENT_MANIFEST: OnceLock<Result<Vec<String>, String>> = OnceLock::new();

#[cfg(any(embedded_resources, feature = "portable"))]
mod embedded_resources {
    include!(concat!(env!("OUT_DIR"), "/embedded_resources.rs"));
}

pub fn read_resource_text(
    app_handle: &AppHandle,
    relative_path: &str,
) -> Result<String, DomainError> {
    let bytes = read_resource_bytes(app_handle, relative_path)?;
    decode_resource_text(relative_path, bytes)
}

pub fn read_resource_bytes(
    app_handle: &AppHandle,
    relative_path: &str,
) -> Result<Vec<u8>, DomainError> {
    let normalized = normalize_resource_relative_path(relative_path)?;

    #[cfg(embedded_resources)]
    {
        let _ = app_handle;
        if let Some(bytes) = embedded_resources::get_embedded_resource(&normalized) {
            return Ok(bytes.to_vec());
        }

        Err(DomainError::NotFound(format!(
            "Embedded resource not found: {}",
            normalized
        )))
    }

    #[cfg(not(embedded_resources))]
    {
        let path = resolve_resource_path(app_handle, &normalized)?;
        match app_handle.fs().read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) => {
                #[cfg(feature = "portable")]
                {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        if let Some(bytes) = embedded_resources::get_embedded_resource(&normalized)
                        {
                            tracing::debug!(
                                "Resource {:?} not found on disk at {:?}, using embedded fallback",
                                normalized,
                                path
                            );
                            return Ok(bytes.to_vec());
                        }
                    }
                }

                tracing::error!(
                    "Failed to read resource bytes {:?} (resolved to {:?}): {}",
                    normalized,
                    path,
                    error
                );
                Err(map_resource_error(&normalized, error))
            }
        }
    }
}

pub fn list_default_content_files_under(prefix: &str) -> Result<Vec<String>, DomainError> {
    let normalized_prefix = prefix.trim_matches('/').replace('\\', "/");
    let query = if normalized_prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", normalized_prefix)
    };

    let files = DEFAULT_CONTENT_MANIFEST
        .get_or_init(load_default_content_manifest)
        .as_ref()
        .map_err(|message| DomainError::InvalidData(message.clone()))?;
    if query.is_empty() {
        return Ok(files.to_vec());
    }

    Ok(files
        .iter()
        .filter(|path| path.starts_with(&query))
        .cloned()
        .collect())
}

fn load_default_content_manifest() -> Result<Vec<String>, String> {
    let raw = include_str!(concat!(env!("OUT_DIR"), "/default_content_manifest.json"));
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(mut entries) => {
            entries.sort();
            Ok(entries)
        }
        Err(error) => {
            tracing::error!(
                target: crate::observability_targets::USER_VISIBLE_ERROR,
                "Failed to load generated default content manifest: {}",
                error
            );
            Err(format!(
                "Invalid generated default content manifest: {}",
                error
            ))
        }
    }
}

#[cfg(not(embedded_resources))]
fn resolve_resource_path(
    app_handle: &AppHandle,
    relative_path: &str,
) -> Result<std::path::PathBuf, DomainError> {
    app_handle
        .path()
        .resolve(relative_path, BaseDirectory::Resource)
        .map_err(|e| {
            tracing::error!("Failed to resolve resource path {:?}: {}", relative_path, e);
            DomainError::InternalError(format!("Failed to resolve resource path: {}", e))
        })
}

#[cfg(not(embedded_resources))]
fn map_resource_error(relative_path: &str, error: std::io::Error) -> DomainError {
    if error.kind() == std::io::ErrorKind::NotFound {
        DomainError::NotFound(format!("Resource not found: {}", relative_path))
    } else {
        DomainError::InternalError(format!(
            "Failed to read resource '{}': {}",
            relative_path, error
        ))
    }
}
