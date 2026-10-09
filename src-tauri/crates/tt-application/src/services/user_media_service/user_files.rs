//! User-file attachment policy shared by the native and browser command boundaries.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use tt_domain::errors::DomainError;
use url::Url;

use super::{BASE64_STANDARD, UserMediaService};
use crate::client_asset_paths::{UserDataAssetKind, parse_user_data_asset_request_path};
use crate::services::asset_service::validate_asset_file_name;

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct UserFileUploadResult {
    pub path: String,
}

impl UserMediaService {
    pub async fn upload_user_file(
        &self,
        name: &str,
        data_base64: &str,
    ) -> Result<UserFileUploadResult, DomainError> {
        let name = validate_upload_name(name)?;
        let bytes = BASE64_STANDARD
            .decode(data_base64.as_bytes())
            .map_err(|error| {
                DomainError::InvalidData(format!("No upload data specified: {error}"))
            })?;
        self.store.write_user_file(Path::new(&name), bytes).await?;
        Ok(UserFileUploadResult {
            path: format!("/user/files/{name}"),
        })
    }

    pub async fn delete_user_file(&self, path_or_url: &str) -> Result<(), DomainError> {
        let relative = normalize_user_file_reference(path_or_url)?;
        self.store.delete_user_file(&relative).await
    }

    pub async fn verify_user_files(
        &self,
        urls: Vec<String>,
    ) -> Result<HashMap<String, bool>, DomainError> {
        let mut result = HashMap::with_capacity(urls.len());
        for original_url in urls {
            let Ok(relative) = normalize_user_file_reference(&original_url) else {
                continue;
            };
            match self.store.user_file_exists(&relative).await {
                Ok(exists) => {
                    result.insert(original_url, exists);
                }
                // A symlink or other confinement violation is an invalid reference, not a missing file.
                Err(DomainError::InvalidData(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(result)
    }
}

/// Attachment URLs name the same file that the browser resource route serves. Decode and
/// validate one segment at a time via that route's parser, never decode a whole filesystem path.
fn normalize_user_file_reference(raw: &str) -> Result<PathBuf, DomainError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(DomainError::InvalidData("No path specified".into()));
    }
    let path = Url::parse(value).map_or_else(|_| value.to_owned(), |url| url.path().to_owned());
    let path = format!("/{}", path.replace('\\', "/").trim_start_matches('/'));
    match parse_user_data_asset_request_path(&path) {
        Ok(Some(parsed)) if parsed.kind == UserDataAssetKind::UserFile => Ok(parsed.relative_path),
        _ => Err(DomainError::InvalidData("Invalid path".into())),
    }
}

fn validate_upload_name(raw: &str) -> Result<String, DomainError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(DomainError::InvalidData("No upload name specified".into()));
    }
    validate_asset_file_name(name)
}
