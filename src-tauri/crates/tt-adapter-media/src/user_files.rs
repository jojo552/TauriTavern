//! Attachment IO. Descendants of the configured data root may not be symlinks.
use std::path::{Component, Path, PathBuf};

use tokio::fs;
use tokio::io::AsyncWriteExt;
use tt_adapter_storage_core::file_system::{persist_file, unique_temp_path};
use tt_domain::errors::DomainError;
use tt_domain::models::user_directory::UserDirectory;

#[derive(Debug, Clone)]
pub(crate) struct FilesystemUserFiles {
    data_root: PathBuf,
}

impl FilesystemUserFiles {
    pub(crate) fn new(data_root: &Path) -> Self {
        Self {
            data_root: data_root.to_owned(),
        }
    }

    async fn root(&self) -> Result<PathBuf, DomainError> {
        let mut root = fs::canonicalize(&self.data_root).await.map_err(|error| {
            DomainError::file_io(
                "Resolve user files data root",
                self.data_root.display().to_string(),
                error,
            )
        })?;
        // Walk before creating: create_dir_all would follow an existing directory symlink.
        // The configured data root is trusted; every descendant belongs to confinement.
        let files_directory = UserDirectory::default_user(&root).files;
        let relative_root = files_directory.strip_prefix(&root).map_err(|_| invalid_path())?;
        for segment in relative_root.components() {
            root.push(segment);
            match fs::create_dir(&root).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(DomainError::file_io(
                        "Ensure user files directory",
                        root.display().to_string(),
                        error,
                    ));
                }
            }
            let metadata = fs::symlink_metadata(&root).await.map_err(|error| {
                DomainError::file_io(
                    "Inspect user files directory",
                    root.display().to_string(),
                    error,
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(invalid_path());
            }
            if !metadata.is_dir() {
                return Err(DomainError::InternalError(format!(
                    "User files directory is not a directory: {}",
                    root.display(),
                )));
            }
        }
        Ok(root)
    }

    async fn resolve(&self, relative: &Path) -> Result<PathBuf, DomainError> {
        if relative.as_os_str().is_empty()
            || relative.components().any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid_path());
        }
        let mut target = self.root().await?;
        for segment in relative.components() {
            target.push(segment);
            match fs::symlink_metadata(&target).await {
                Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid_path()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(error) => {
                    return Err(DomainError::file_io(
                        "Inspect user file",
                        target.display().to_string(),
                        error,
                    ));
                }
            }
        }
        Ok(target)
    }

    pub(crate) async fn write(&self, relative: &Path, bytes: &[u8]) -> Result<(), DomainError> {
        let target = self.resolve(relative).await?;
        let temp = unique_temp_path(&target);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|error| {
                DomainError::file_io("Stage user file", temp.display().to_string(), error)
            })?;
        let result = async {
            let written = async {
                file.write_all(bytes).await?;
                file.flush().await
            }
            .await;
            if let Err(error) = written {
                drop(file);
                return Err(DomainError::file_io(
                    "Write user file",
                    temp.display().to_string(),
                    error,
                ));
            }
            persist_file(file, &temp, &target).await
        }
        .await;
        if result.is_err() {
            if let Err(error) = fs::remove_file(&temp).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("Failed to remove user-file staging {}: {error}", temp.display());
                }
            }
        }
        result
    }

    pub(crate) async fn exists(&self, relative: &Path) -> Result<bool, DomainError> {
        let target = self.resolve(relative).await?;
        is_file(&target).await
    }

    pub(crate) async fn delete(&self, relative: &Path) -> Result<(), DomainError> {
        let target = self.resolve(relative).await?;
        if !is_file(&target).await? {
            return Err(DomainError::NotFound("File not found".into()));
        }
        fs::remove_file(&target).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                DomainError::NotFound("File not found".into())
            } else {
                DomainError::file_io("Delete user file", target.display().to_string(), error)
            }
        })
    }
}

async fn is_file(path: &Path) -> Result<bool, DomainError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_path()),
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => Ok(false),
        Err(error) => Err(DomainError::file_io("Stat user file", path.display().to_string(), error)),
    }
}

fn invalid_path() -> DomainError {
    DomainError::InvalidData("Invalid path".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("tauritavern-user-files-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).expect("create data root");
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn attachments_replace_bytes_and_failed_publish_keeps_existing_state() {
        let root = TempRoot::new();
        let files = FilesystemUserFiles::new(&root.0);
        let relative = Path::new("notes.txt");
        files.write(relative, b"old").await.expect("first upload");
        files.write(relative, b"new").await.expect("replace upload");
        assert_eq!(
            fs::read(root.0.join("default-user/user/files/notes.txt")).await.expect("read"),
            b"new",
        );
        assert!(files.exists(relative).await.expect("exists"));
        assert!(!files.exists(Path::new("missing.txt")).await.expect("missing"));
        assert!(matches!(
            files.write(Path::new("../outside.txt"), b"bad").await,
            Err(DomainError::InvalidData(_)),
        ));

        // An existing directory forces publication to fail after bytes are staged.
        // Its contents must survive, and the failed staging file must be removed.
        let blocked = root.0.join("default-user/user/files/blocked.txt");
        fs::create_dir(&blocked).await.expect("blocking directory");
        fs::write(blocked.join("keep"), b"keep").await.expect("existing contents");
        assert!(files.write(Path::new("blocked.txt"), b"bad").await.is_err());
        assert_eq!(fs::read(blocked.join("keep")).await.expect("retained contents"), b"keep");
        let mut entries = fs::read_dir(blocked.parent().expect("parent")).await.expect("list");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            assert!(!entry.file_name().to_string_lossy().ends_with(".tmp"));
        }
        files.delete(relative).await.expect("delete");
        assert!(!files.exists(relative).await.expect("deleted"));
        assert!(matches!(files.delete(relative).await, Err(DomainError::NotFound(_))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_files_directories_and_user_roots_cannot_escape_confinement() {
        let root = TempRoot::new();
        let outside = TempRoot::new();
        fs::write(outside.0.join("notes.txt"), b"outside").await.expect("outside file");
        let files = FilesystemUserFiles::new(&root.0);
        files.root().await.expect("root");
        let files_root = root.0.join("default-user/user/files");
        std::os::unix::fs::symlink(outside.0.join("notes.txt"), files_root.join("notes.txt"))
            .expect("file link");
        std::os::unix::fs::symlink(&outside.0, files_root.join("linked")).expect("directory link");
        for relative in [Path::new("notes.txt"), Path::new("linked/notes.txt")] {
            assert!(matches!(files.write(relative, b"bad").await, Err(DomainError::InvalidData(_))));
            assert!(matches!(files.exists(relative).await, Err(DomainError::InvalidData(_))));
            assert!(matches!(files.delete(relative).await, Err(DomainError::InvalidData(_))));
        }
        assert_eq!(fs::read(outside.0.join("notes.txt")).await.expect("outside preserved"), b"outside");
        std::os::unix::fs::symlink(outside.0.join("missing.txt"), files_root.join("dangling.txt"))
            .expect("dangling link");
        assert!(matches!(files.exists(Path::new("dangling.txt")).await, Err(DomainError::InvalidData(_))));

        for linked_root in [
            Path::new("default-user"),
            Path::new("default-user/user"),
            Path::new("default-user/user/files"),
        ] {
            let data_root = TempRoot::new();
            let link = data_root.0.join(linked_root);
            fs::create_dir_all(link.parent().expect("parent")).await.expect("parent directory");
            std::os::unix::fs::symlink(&outside.0, link).expect("root link");
            let store = FilesystemUserFiles::new(&data_root.0);
            assert!(matches!(store.write(Path::new("notes.txt"), b"bad").await, Err(DomainError::InvalidData(_))));
        }
    }
}
