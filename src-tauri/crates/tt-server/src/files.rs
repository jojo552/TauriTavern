//! Host-owned file staging for the browser host.
//!
//! A browser page has no filesystem paths, so every import passes through a server-side staging
//! area: the page uploads bytes under an opaque name, and later commands may only read paths that
//! resolved inside this root. Paths outside it are refused, never resolved.

use std::path::{Path, PathBuf};

use tokio::fs;
use tt_domain::errors::DomainError;

/// Large media chunks; the page sends these and expects the same limit back.
pub const LARGE_UPLOAD_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
/// Small assets (avatars, world-info imports) stay in one frame for speed.
pub const SMALL_ASSET_UPLOAD_CHUNK_BYTES: u64 = 512 * 1024;

pub struct HostFiles {
    staging_root: PathBuf,
}

impl HostFiles {
    pub async fn new(app_root: &Path) -> Result<Self, DomainError> {
        let staging_root = app_root.join("staging");
        fs::create_dir_all(&staging_root).await.map_err(|error| {
            DomainError::InternalError(format!(
                "Failed to create the staging directory {}: {error}",
                staging_root.display()
            ))
        })?;
        Ok(Self { staging_root })
    }

    /// Creates an empty staged file. `kind` and `extension` are the page's own hints and are
    /// validated as path components so they can never escape the staging root.
    pub async fn create_staged_file(
        &self,
        kind: &str,
        extension: &str,
    ) -> Result<PathBuf, DomainError> {
        let kind = validated_component(kind, "file staging kind", 48)?;
        let extension = validated_component(
            extension.trim_start_matches('.'),
            "file staging extension",
            12,
        )?;
        let directory = self.staging_root.join(kind);
        fs::create_dir_all(&directory).await.map_err(|error| {
            DomainError::InternalError(format!("Failed to create staging kind directory: {error}"))
        })?;
        let path = directory.join(format!("{}.{}", uuid::Uuid::new_v4().simple(), extension));
        fs::File::create(&path).await.map_err(|error| {
            DomainError::InternalError(format!("Failed to create staged file: {error}"))
        })?;
        Ok(path)
    }

    /// Resolves an existing staged path against the canonical root; anything else is refused.
    pub async fn validate_staged_path(&self, raw: &str) -> Result<PathBuf, DomainError> {
        let requested = PathBuf::from(raw);
        if !requested.is_absolute() {
            return Err(DomainError::InvalidData(
                "File staging path must be absolute".into(),
            ));
        }
        let canonical_root = fs::canonicalize(&self.staging_root)
            .await
            .map_err(|error| {
                DomainError::InternalError(format!("Failed to resolve the staging root: {error}"))
            })?;
        match fs::canonicalize(&requested).await {
            Ok(canonical) if canonical.starts_with(&canonical_root) => Ok(canonical),
            Ok(_) => Err(DomainError::InvalidData(
                "File is outside the staging directory".into(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A file created moments ago cannot resolve on every filesystem; keep the
                // lexical check so an unseen path is still refused unless it lies under the root.
                let parent = requested.parent().unwrap_or(&requested);
                let canonical_parent = fs::canonicalize(parent).await.map_err(|_| {
                    DomainError::InvalidData("File staging path does not exist".into())
                })?;
                canonical_parent
                    .starts_with(&canonical_root)
                    .then_some(requested)
                    .ok_or_else(|| {
                        DomainError::InvalidData("File is outside the staging directory".into())
                    })
            }
            Err(error) => Err(DomainError::InternalError(format!(
                "Failed to resolve the staged file: {error}"
            ))),
        }
    }

    pub async fn discard_file(&self, path: &Path) -> Result<(), DomainError> {
        tt_adapter_storage_core::staged_file::discard(path).await
    }
}

fn validated_component(value: &str, label: &str, max: usize) -> Result<String, DomainError> {
    let value = value.trim();
    let single = !value.is_empty()
        && value.len() <= max
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && value.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
        });
    if !single {
        return Err(DomainError::InvalidData(format!("Invalid {label}")));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staging_paths_outside_the_root_are_refused() {
        let root =
            std::env::temp_dir().join(format!("tt-host-files-{}", uuid::Uuid::new_v4().simple()));
        let files = HostFiles::new(&root).await.unwrap();
        let inside = files.create_staged_file("avatar", "png").await.unwrap();
        assert!(
            files
                .validate_staged_path(inside.to_str().unwrap())
                .await
                .is_ok()
        );
        assert!(files.discard_file(&inside).await.is_ok());

        let outside = root.parent().unwrap().join("elsewhere.txt");
        assert!(
            files
                .validate_staged_path(outside.to_str().unwrap())
                .await
                .is_err()
        );
        assert!(files.validate_staged_path("relative/path").await.is_err());
        assert!(
            files
                .validate_staged_path(root.join("../escape").to_str().unwrap())
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn staging_hints_cannot_traverse() {
        let root =
            std::env::temp_dir().join(format!("tt-host-files-{}", uuid::Uuid::new_v4().simple()));
        let files = HostFiles::new(&root).await.unwrap();
        assert!(files.create_staged_file("../escape", "png").await.is_err());
        assert!(files.create_staged_file("avatar", "p/ng").await.is_err());
        assert!(files.create_staged_file("", "png").await.is_err());
        let _ = fs::remove_dir_all(&root).await;
    }
}
