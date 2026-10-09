use std::path::Path;

use async_trait::async_trait;
use tt_domain::errors::DomainError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMediaEntry {
    pub name: String,
    pub mime_type: Option<String>,
    pub modified_ms: Option<i128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMediaStoreError {
    NotFound(String),
    Internal(String),
}

impl UserMediaStoreError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl From<UserMediaStoreError> for DomainError {
    fn from(error: UserMediaStoreError) -> Self {
        match error {
            UserMediaStoreError::NotFound(message) => DomainError::NotFound(message),
            UserMediaStoreError::Internal(message) => DomainError::InternalError(message),
        }
    }
}

#[async_trait]
pub trait UserMediaStore: Send + Sync {
    /// Publish attachment bytes without damaging an existing file on failure.
    async fn write_user_file(
        &self,
        relative_path: &Path,
        bytes: Vec<u8>,
    ) -> Result<(), DomainError>;

    async fn delete_user_file(&self, relative_path: &Path) -> Result<(), DomainError>;

    /// InvalidData denotes a reference rejected by filesystem confinement.
    async fn user_file_exists(&self, relative_path: &Path) -> Result<bool, DomainError>;

    async fn write_file(
        &self,
        relative_path: &Path,
        bytes: Vec<u8>,
    ) -> Result<(), UserMediaStoreError>;

    async fn ensure_folder(&self, relative_folder: &Path) -> Result<(), UserMediaStoreError>;

    async fn list_files(
        &self,
        relative_folder: &Path,
    ) -> Result<Vec<UserMediaEntry>, UserMediaStoreError>;

    async fn list_folders(&self) -> Result<Vec<String>, UserMediaStoreError>;

    async fn delete_file(&self, relative_path: &Path) -> Result<(), UserMediaStoreError>;
}
