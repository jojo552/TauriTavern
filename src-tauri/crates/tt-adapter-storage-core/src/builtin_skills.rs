//! Materialize the host's immutable Skill resources for the shared workspace view.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tt_domain::errors::DomainError;
use tt_ports::bundled_resources::BundledResources;
use uuid::Uuid;

pub async fn materialize_builtin_skills(
    resources: Arc<dyn BundledResources>,
    data_root: &Path,
) -> Result<PathBuf, DomainError> {
    let cache_root = data_root.join("_cache/builtin-skills");
    tokio::task::spawn_blocking(move || materialize(resources.as_ref(), &cache_root))
        .await
        .map_err(|error| DomainError::InternalError(format!("Builtin Skills task failed: {error}")))?
}

fn materialize(resources: &dyn BundledResources, cache_root: &Path) -> Result<PathBuf, DomainError> {
    let files = resources.list_default_files_under("skills", "")?;
    if files.is_empty() {
        return Err(DomainError::NotFound(
            "Bundled builtin Skills are missing".into(),
        ));
    }
    let mut digest = Sha256::new();
    let mut content = Vec::with_capacity(files.len());
    for relative in files {
        let bytes = resources.read_bytes(&format!("default/skills/{relative}"))?;
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(&bytes);
        content.push((relative, bytes));
    }
    let digest = digest.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let target = cache_root.join(&digest);
    if target.try_exists().map_err(|error| {
        DomainError::file_io("stat builtin Skills", target.display().to_string(), error)
    })? {
        return Ok(target);
    }
    let staging = cache_root.join(format!(".{digest}.tmp-{}", Uuid::new_v4().simple()));
    for (relative, bytes) in content {
        let path = staging.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                DomainError::file_io(
                    "create builtin Skills staging",
                    parent.display().to_string(),
                    error,
                )
            })?;
        }
        std::fs::write(&path, bytes).map_err(|error| {
            DomainError::file_io("write builtin Skill", path.display().to_string(), error)
        })?;
    }
    // Publish only a complete package, never a partially copied workspace view.
    std::fs::rename(&staging, &target).map_err(|error| {
        DomainError::file_io("publish builtin Skills", target.display().to_string(), error)
    })?;
    if let Err(error) = remove_other_entries(cache_root, &target) {
        tracing::warn!(
            "Failed to remove old builtin Skills in {}: {error}",
            cache_root.display()
        );
    }
    Ok(target)
}

fn remove_other_entries(cache_root: &Path, keep: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(cache_root)? {
        let entry = entry?;
        if entry.path() == keep {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
