//! Resolve operator-configured roots before assembling services or publishing files.

use std::io;
use std::path::{Path, PathBuf};

use tokio::fs;

pub struct StartupPaths {
    pub app_root: PathBuf,
    pub data_root: PathBuf,
    pub web_root: PathBuf,
    pub resource_root: PathBuf,
    pub archive_root: PathBuf,
}

impl StartupPaths {
    pub async fn resolve(
        app_root: &Path,
        data_root: &Path,
        web_root: &Path,
        resource_root: &Path,
    ) -> io::Result<Self> {
        let web_root = fs::canonicalize(web_root).await?;
        let resource_root = fs::canonicalize(resource_root).await?;
        let app_root = create_canonical_directory(app_root).await?;
        let data_root = create_canonical_directory(data_root).await?;
        for public_root in [&web_root, &resource_root] {
            reject_overlap(public_root, &data_root)?;
        }

        // Check the actual private directories, not the entire app root: app=/state and
        // data=/state/data is the native layout and remains valid. Resolve pre-existing
        // symlinks too, so a private directory cannot redirect into served/archived data.
        let mut archive_root = app_root.join(".data-archive");
        for name in ["security", "staging", ".data-archive"] {
            let private_path = app_root.join(name);
            for exposed_root in [&web_root, &resource_root, &data_root] {
                reject_overlap(&private_path, exposed_root)?;
            }
            let private_root = create_canonical_directory(&private_path).await?;
            for exposed_root in [&web_root, &resource_root, &data_root] {
                reject_overlap(&private_root, exposed_root)?;
            }
            if name == ".data-archive" {
                archive_root = private_root;
            }
        }

        Ok(Self {
            app_root,
            data_root,
            web_root,
            resource_root,
            archive_root,
        })
    }
}

async fn create_canonical_directory(path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(path).await?;
    fs::canonicalize(path).await
}

fn reject_overlap(left: &Path, right: &Path) -> io::Result<()> {
    if left.starts_with(right) || right.starts_with(left) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Private storage and served/archived roots must not overlap: {} and {}",
                left.display(),
                right.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        async fn new() -> Self {
            Self::new_in(&std::env::temp_dir()).await
        }

        async fn new_in(parent: &Path) -> Self {
            let root = parent.join(format!("tt-startup-paths-{}", uuid::Uuid::new_v4().simple()));
            for name in ["web", "resources", "state"] {
                fs::create_dir_all(root.join(name)).await.unwrap();
            }
            Self(root)
        }

        async fn resolve(&self, app: &Path, data: &Path) -> io::Result<StartupPaths> {
            StartupPaths::resolve(app, data, &self.0.join("web"), &self.0.join("resources")).await
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[tokio::test]
    async fn relative_app_root_preserves_native_layout_and_absolute_staging_paths() {
        // A relative fixture avoids process-wide chdir, which would race other tests.
        let fixture = Fixture::new_in(Path::new(".")).await;
        let app = fixture.0.join("state");
        let paths = fixture.resolve(&app, &app.join("data")).await.unwrap();
        assert_eq!(paths.app_root, fs::canonicalize(&app).await.unwrap());
        let files = crate::files::HostFiles::new(&paths.app_root).await.unwrap();
        let staged = files
            .create_staged_file("avatar", "png")
            .await
            .unwrap();
        assert!(staged.is_absolute());
        assert_eq!(
            files
                .validate_staged_path(staged.to_str().unwrap())
                .await
                .unwrap(),
            fs::canonicalize(staged).await.unwrap()
        );
    }

    #[tokio::test]
    async fn private_directories_cannot_be_served_or_archived() {
        let fixture = Fixture::new().await;
        let separate_data = fixture.0.join("data");
        for public_name in ["web", "resources"] {
            let public_root = fixture.0.join(public_name);
            let error = fixture
                .resolve(&public_root, &separate_data)
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            let error = fixture
                .resolve(&fixture.0.join("state"), &public_root.join("data"))
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let app = fixture.0.join("state");
        for private_name in ["security", "staging", ".data-archive"] {
            let private_root = app.join(private_name);
            let error = fixture.resolve(&app, &private_root).await.err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            // A served root inside private storage is unsafe too, not just its ancestor.
            let public_child = private_root.join("public");
            fs::create_dir_all(&public_child).await.unwrap();
            let error = StartupPaths::resolve(
                &app,
                &separate_data,
                &public_child,
                &fixture.0.join("resources"),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let error = fixture
            .resolve(&separate_data.join("app"), &separate_data)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_symlinks_cannot_redirect_into_served_or_archived_roots() {
        let fixture = Fixture::new().await;
        let data = fixture.0.join("data");
        fs::create_dir_all(&data).await.unwrap();
        let app = fixture.0.join("state");
        for name in ["security", "staging", ".data-archive"] {
            for target in [
                fixture.0.join("web"),
                fixture.0.join("resources"),
                data.clone(),
            ] {
                let link = app.join(name);
                std::os::unix::fs::symlink(target, &link).unwrap();
                let error = fixture.resolve(&app, &data).await.err().unwrap();
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
                fs::remove_file(&link).await.unwrap();
            }
            fs::create_dir_all(app.join(name)).await.unwrap();
        }
    }
}
