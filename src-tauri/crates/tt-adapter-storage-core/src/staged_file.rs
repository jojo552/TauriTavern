//! File IO for already-validated host staging paths. Both hosts use this receiver, so checking an
//! offset and acknowledging an append have one serialization boundary, shared with finish/discard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;
use tt_domain::errors::DomainError;

static FILE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> = OnceLock::new();

async fn file_lock(path: &Path) -> Result<Arc<AsyncMutex<()>>, DomainError> {
    // A discarded file is missing, but its canonical parent still identifies its receiver.
    let key = match fs::canonicalize(path).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| DomainError::InvalidData("Staged path has no parent".into()))?;
            let name = path.file_name().ok_or_else(|| DomainError::InvalidData("Staged path has no filename".into()))?;
            fs::canonicalize(parent).await.map_err(|error| DomainError::file_io("Resolve staging parent", parent.display().to_string(), error))?.join(name)
        }
        Err(error) => return Err(DomainError::file_io("Resolve staged file", path.display().to_string(), error)),
    };
    let mut locks = FILE_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().expect("staged file locks poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    let lock = locks.entry(key).or_default();
    if let Some(lock) = lock.upgrade() {
        return Ok(lock);
    }
    let receiver = Arc::new(AsyncMutex::new(()));
    *lock = Arc::downgrade(&receiver);
    Ok(receiver)
}

pub async fn append(path: &Path, offset: u64, data: &[u8]) -> Result<u64, DomainError> {
    let receiver = file_lock(path).await?;
    let _guard = receiver.lock().await;
    let mut file = fs::OpenOptions::new().append(true).open(path).await
        .map_err(|error| DomainError::file_io("Open staged file", path.display().to_string(), error))?;
    let size = file.metadata().await
        .map_err(|error| DomainError::file_io("Stat staged file", path.display().to_string(), error))?.len();
    if size != offset {
        return Err(DomainError::InvalidData(format!("File staging offset mismatch: expected {size}, got {offset}")));
    }
    file.write_all(data).await
        .map_err(|error| DomainError::file_io("Append staged file", path.display().to_string(), error))?;
    // Tokio writes can be outstanding on the blocking pool: flush before another request can
    // observe the next offset, while still holding the same receiver lock.
    file.flush().await
        .map_err(|error| DomainError::file_io("Flush staged file", path.display().to_string(), error))?;
    Ok(offset + data.len() as u64)
}

pub async fn finish(path: &Path, expected_size: u64) -> Result<u64, DomainError> {
    let receiver = file_lock(path).await?;
    let _guard = receiver.lock().await;
    let size = fs::metadata(path).await
        .map_err(|error| DomainError::file_io("Stat staged file", path.display().to_string(), error))?.len();
    if size != expected_size {
        return Err(DomainError::InvalidData(format!("File staging size mismatch: expected {expected_size}, got {size}")));
    }
    Ok(size)
}

pub async fn discard(path: &Path) -> Result<(), DomainError> {
    let receiver = file_lock(path).await?;
    let _guard = receiver.lock().await;
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DomainError::file_io("Remove staged file", path.display().to_string(), error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receiving_frames_rejects_replays_and_acks_observable_bytes() {
        let path = std::env::temp_dir().join(format!("tt-staging-{}.bin", uuid::Uuid::new_v4()));
        fs::File::create(&path).await.unwrap();
        assert!(matches!(append(&path, 1, b"early").await, Err(DomainError::InvalidData(_))));
        assert_eq!(fs::metadata(&path).await.unwrap().len(), 0);
        // Two clients proposing the same offset cannot both succeed, even if they arrive together.
        let (a, b) = tokio::join!(append(&path, 0, b"abc"), append(&path, 0, b"abc"));
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(fs::read(&path).await.unwrap(), b"abc");
        let data: Vec<_> = (0..1_094_711).map(|index| (index % 251) as u8).collect();
        assert_eq!(append(&path, 3, &data).await.unwrap(), 3 + data.len() as u64);
        let bytes = fs::read(&path).await.unwrap();
        assert_eq!(&bytes[3..], data);
        assert!(finish(&path, 2).await.is_err());
        assert_eq!(finish(&path, bytes.len() as u64).await.unwrap(), bytes.len() as u64);
        discard(&path).await.unwrap();
        discard(&path).await.unwrap();
        assert!(!path.exists());
    }
}
