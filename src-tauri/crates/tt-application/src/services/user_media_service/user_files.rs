//! User-file attachment policy shared by the native and browser command boundaries.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use tt_domain::errors::DomainError;
use url::Url;

use super::{BASE64_STANDARD, UserMediaService};

const UNSAFE_EXTENSIONS: &[&str] = &[
    ".php",
    ".exe",
    ".com",
    ".dll",
    ".pif",
    ".application",
    ".gadget",
    ".msi",
    ".jar",
    ".cmd",
    ".bat",
    ".reg",
    ".sh",
    ".py",
    ".js",
    ".jse",
    ".jsp",
    ".pdf",
    ".html",
    ".htm",
    ".hta",
    ".vb",
    ".vbs",
    ".vbe",
    ".cpl",
    ".msc",
    ".scr",
    ".sql",
    ".iso",
    ".img",
    ".dmg",
    ".ps1",
    ".ps1xml",
    ".ps2",
    ".ps2xml",
    ".psc1",
    ".psc2",
    ".msh",
    ".msh1",
    ".msh2",
    ".mshxml",
    ".msh1xml",
    ".msh2xml",
    ".scf",
    ".lnk",
    ".inf",
    ".doc",
    ".docm",
    ".docx",
    ".dot",
    ".dotm",
    ".dotx",
    ".xls",
    ".xlsm",
    ".xlsx",
    ".xlt",
    ".xltm",
    ".xltx",
    ".xlam",
    ".ppt",
    ".pptm",
    ".pptx",
    ".pot",
    ".potm",
    ".potx",
    ".ppam",
    ".ppsx",
    ".ppsm",
    ".pps",
    ".sldx",
    ".sldm",
    ".ws",
];

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

// Preserve native reference semantics: absolute URLs use their path, backslashes
// are separators, and percent escapes are not decoded into filesystem components.
fn normalize_relative_path(raw: &str) -> Result<PathBuf, DomainError> {
    let normalized = String::from(raw)
        .replace('\\', "/")
        .trim()
        .trim_start_matches('/')
        .to_string();

    if normalized.is_empty() {
        return Err(DomainError::InvalidData(
            "File path cannot be empty".to_string(),
        ));
    }

    let mut path = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(segment) => path.push(segment),
            _ => {
                return Err(DomainError::InvalidData("Invalid file path".to_string()));
            }
        }
    }

    if path.as_os_str().is_empty() {
        return Err(DomainError::InvalidData(
            "File path cannot be empty".to_string(),
        ));
    }

    Ok(path)
}

fn validate_upload_name(raw: &str) -> Result<String, DomainError> {
    let name = String::from(raw).trim().to_string();
    if name.is_empty() {
        return Err(DomainError::InvalidData(
            "No upload name specified".to_string(),
        ));
    }

    if name.starts_with('.') {
        return Err(DomainError::InvalidData(
            "Filename cannot start with '.'".to_string(),
        ));
    }

    if name.contains('/') || name.contains('\\') {
        return Err(DomainError::InvalidData(
            "Illegal character in filename".to_string(),
        ));
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
    {
        return Err(DomainError::InvalidData(
            "Illegal character in filename; only alphanumeric, '_', '-' are accepted.".to_string(),
        ));
    }

    let extension = Path::new(&name)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();

    if UNSAFE_EXTENSIONS.contains(&extension.as_str()) {
        return Err(DomainError::InvalidData(
            "Forbidden file extension.".to_string(),
        ));
    }

    Ok(name)
}

fn normalize_user_file_reference(raw: &str) -> Result<PathBuf, DomainError> {
    let mut value = String::from(raw).trim().to_string();
    if value.is_empty() {
        return Err(DomainError::InvalidData("No path specified".to_string()));
    }

    if let Ok(parsed_url) = Url::parse(&value) {
        value = parsed_url.path().to_string();
    }

    let normalized = value.replace('\\', "/");
    let without_leading = normalized.trim_start_matches('/');
    let relative = without_leading
        .strip_prefix("user/files/")
        .ok_or_else(|| DomainError::InvalidData("Invalid path".to_string()))?;

    normalize_relative_path(relative)
}
