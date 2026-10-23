use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use tt_domain::errors::DomainError;
use tt_ports::bundled_resources::BundledResources;
use tt_ports::bundled_template::BundledTemplateStore;

/// Read-only bundled resources from a directory containing `default/content` and `default/skills`.
/// No alternate directory or embedded source is used when a read fails.
pub struct DirectoryBundledResources {
    resource_root: PathBuf,
}

impl DirectoryBundledResources {
    pub fn new(resource_root: PathBuf) -> Self {
        Self { resource_root }
    }
}

impl BundledResources for DirectoryBundledResources {
    fn read_bytes(&self, relative_path: &str) -> Result<Vec<u8>, DomainError> {
        let normalized = normalize_resource_relative_path(relative_path)?;
        std::fs::read(self.resource_root.join(&normalized)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                DomainError::NotFound(format!("Resource not found: {}", normalized))
            } else {
                DomainError::InternalError(format!(
                    "Failed to read resource '{}': {}",
                    normalized, error
                ))
            }
        })
    }

    fn list_default_files_under(
        &self,
        root: &str,
        prefix: &str,
    ) -> Result<Vec<String>, DomainError> {
        if !matches!(root, "content" | "skills") {
            return Err(DomainError::InvalidData(format!(
                "Unknown default resource root: {root}"
            )));
        }
        let prefix = prefix.trim_matches('/').replace('\\', "/");
        let query = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", normalize_resource_relative_path(&prefix)?)
        };
        let content_root = self.resource_root.join("default").join(root);
        let mut files = Vec::new();
        collect_default_files(&content_root, &content_root, &mut files)?;
        files.retain(|path| path.starts_with(&query));
        files.sort();
        Ok(files)
    }
}

impl BundledTemplateStore for DirectoryBundledResources {
    fn read_text(&self, relative_path: &str) -> Result<String, DomainError> {
        decode_resource_text(relative_path, self.read_bytes(relative_path)?)
    }
}

fn collect_default_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<String>,
) -> Result<(), DomainError> {
    let entries = std::fs::read_dir(current).map_err(|error| {
        DomainError::InternalError(format!(
            "Failed to list default content directory '{}': {}",
            current.display(),
            error
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            DomainError::InternalError(format!(
                "Failed to read default content entry in '{}': {}",
                current.display(),
                error
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            DomainError::InternalError(format!(
                "Failed to inspect default content entry '{}': {}",
                path.display(),
                error
            ))
        })?;
        if file_type.is_dir() {
            collect_default_files(root, &path, files)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).map_err(|error| {
                DomainError::InternalError(format!(
                    "Invalid default content entry '{}': {}",
                    path.display(),
                    error
                ))
            })?;
            let relative = relative.to_str().ok_or_else(|| {
                DomainError::InvalidData(format!(
                    "Default content path is not valid UTF-8: {}",
                    path.display()
                ))
            })?;
            files.push(relative.replace('\\', "/"));
        }
    }
    Ok(())
}

/// Keep path interpretation identical for disk and packaged resources.
pub fn normalize_resource_relative_path(relative_path: &str) -> Result<String, DomainError> {
    let normalized = relative_path.trim().replace('\\', "/");
    let normalized = normalized.trim_start_matches('/').to_string();

    if normalized.is_empty() || normalized.contains("..") {
        return Err(DomainError::InvalidData(format!(
            "Invalid resource path: {}",
            relative_path
        )));
    }

    Ok(normalized)
}

pub fn decode_resource_text(relative_path: &str, bytes: Vec<u8>) -> Result<String, DomainError> {
    String::from_utf8(bytes).map_err(|error| {
        tracing::error!(
            "Failed to decode resource text {:?}: {}",
            relative_path,
            error
        );
        DomainError::InvalidData(format!(
            "Resource '{}' is not valid UTF-8: {}",
            relative_path, error
        ))
    })
}

pub async fn read_resource_bytes(
    resources: Arc<dyn BundledResources>,
    relative_path: &str,
) -> Result<Vec<u8>, DomainError> {
    let relative_path = relative_path.to_string();
    tokio::task::spawn_blocking(move || resources.read_bytes(&relative_path))
        .await
        .map_err(|error| {
            DomainError::InternalError(format!("Bundled resource read task failed: {}", error))
        })?
}

pub async fn read_resource_json<T: DeserializeOwned>(
    resources: Arc<dyn BundledResources>,
    relative_path: &str,
) -> Result<T, DomainError> {
    let bytes = read_resource_bytes(resources, relative_path).await?;
    let text = decode_resource_text(relative_path, bytes)?;
    serde_json::from_str(&text).map_err(|error| {
        tracing::error!(
            "Failed to parse JSON resource {:?}: {}",
            relative_path,
            error
        );
        DomainError::InvalidData(format!(
            "Invalid JSON resource '{}': {}",
            relative_path, error
        ))
    })
}
