//! One-shot downloads of host-built files (data archives, user backups).
//!
//! The native host hands such a file to a save dialog and deletes it afterwards. A browser can
//! only save what it fetches, so the host registers the file under an unguessable token bound to
//! the requesting browser, serves it exactly once, and deletes it when that response ends. Tokens
//! that are never fetched expire and their files are deleted.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tokio_util::io::ReaderStream;

use crate::error::CommandError;

const DOWNLOAD_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_DOWNLOADS: usize = 32;
pub const DOWNLOAD_ROUTE: &str = "/__tt/download/";

struct PendingDownload {
    path: PathBuf,
    file_name: String,
    browser: String,
    created: Instant,
}

#[derive(Default)]
pub struct Downloads {
    pending: Mutex<HashMap<String, PendingDownload>>,
}

/// Deletes the served file when the response body is dropped (sent, failed or aborted).
struct DeleteOnDrop(PathBuf);

impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.0);
        tokio::spawn(async move { remove(&path).await });
    }
}

async fn remove(path: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %path.display(), "Failed to delete a served download");
    }
}

impl Downloads {
    /// Registers a host-owned file and returns its same-origin URL.
    pub async fn register(
        &self,
        path: PathBuf,
        file_name: String,
        browser: &str,
    ) -> Result<String, CommandError> {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let rejected = {
            let mut pending = self.pending.lock().expect("download registry poisoned");
            if pending.len() >= MAX_PENDING_DOWNLOADS {
                Some(path)
            } else {
                pending.insert(
                    token.clone(),
                    PendingDownload {
                        path,
                        file_name,
                        browser: browser.to_owned(),
                        created: Instant::now(),
                    },
                );
                None
            }
        };
        if let Some(path) = rejected {
            remove(&path).await;
            return Err(CommandError::TooManyRequests(
                "Too many pending downloads; finish or wait for earlier ones".into(),
            ));
        }
        Ok(format!("{DOWNLOAD_ROUTE}{token}"))
    }

    /// Serves and consumes a download. Only the browser that requested it may fetch it.
    pub async fn take(&self, token: &str, browser: Option<&str>) -> Response {
        let entry = {
            let mut pending = self.pending.lock().expect("download registry poisoned");
            match pending.get(token) {
                Some(entry)
                    if Some(entry.browser.as_str()) == browser
                        && entry.created.elapsed() <= DOWNLOAD_LIFETIME =>
                {
                    pending.remove(token)
                }
                _ => None,
            }
        };
        let Some(entry) = entry else {
            return (StatusCode::NOT_FOUND, "Download expired or already used").into_response();
        };
        let file = match tokio::fs::File::open(&entry.path).await {
            Ok(file) => file,
            Err(error) => {
                tracing::error!(%error, path = %entry.path.display(), "Registered download is missing");
                return (StatusCode::GONE, "Download is no longer available").into_response();
            }
        };
        let length = file.metadata().await.map(|m| m.len()).ok();
        let guard = DeleteOnDrop(entry.path);
        let stream = ReaderStream::new(file);
        let body = Body::from_stream(futures_util::StreamExt::map(stream, move |chunk| {
            let _ = &guard;
            chunk
        }));
        let mut response = Response::new(body);
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let encoded = utf8_percent_encode(&entry.file_name, NON_ALPHANUMERIC).to_string();
        if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{encoded}"))
        {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        if let Some(length) = length
            && let Ok(value) = HeaderValue::from_str(&length.to_string())
        {
            headers.insert(header::CONTENT_LENGTH, value);
        }
        response
    }

    /// Deletes files whose download was never fetched.
    pub async fn expire(&self) {
        let expired: Vec<PathBuf> = {
            let mut pending = self.pending.lock().expect("download registry poisoned");
            let tokens: Vec<String> = pending
                .iter()
                .filter(|(_, entry)| entry.created.elapsed() > DOWNLOAD_LIFETIME)
                .map(|(token, _)| token.clone())
                .collect();
            tokens
                .into_iter()
                .filter_map(|token| pending.remove(&token))
                .map(|entry| entry.path)
                .collect()
        };
        for path in expired {
            remove(&path).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_download_is_served_once_to_its_browser_and_then_deleted() {
        let dir =
            std::env::temp_dir().join(format!("tt-downloads-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("archive.zip");
        tokio::fs::write(&path, b"archive-bytes").await.unwrap();
        let downloads = Downloads::default();
        let url = downloads
            .register(path.clone(), "备份.zip".into(), "browser-a")
            .await
            .unwrap();
        let token = url.strip_prefix(DOWNLOAD_ROUTE).unwrap();

        assert_eq!(
            downloads.take(token, Some("browser-b")).await.status(),
            StatusCode::NOT_FOUND
        );
        let response = downloads.take(token, Some("browser-a")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("UTF-8''")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"archive-bytes");
        assert_eq!(
            downloads.take(token, Some("browser-a")).await.status(),
            StatusCode::NOT_FOUND
        );

        for _ in 0..50 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!path.exists(), "a served download must be deleted");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
