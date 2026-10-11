use super::AudioCache;
use crate::disk_evictor::DiskEvictor;
use crate::utils::write_atomic;
use async_trait::async_trait;
use bytes::Bytes;
use color_eyre::Result;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::fs as tokio_fs;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct FileSystemCache {
    base_path: PathBuf,
    evictor: DiskEvictor,
}

impl FileSystemCache {
    pub fn new<P: AsRef<Path>>(base_path: P, max_size_mb: u64) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base_path)?;
        let max_size_bytes = max_size_mb * 1024 * 1024;
        let evictor = DiskEvictor::new(base_path.clone(), max_size_bytes, Some("meta"));

        Ok(FileSystemCache { base_path, evictor })
    }

    fn get_file_path(&self, key: &str) -> PathBuf {
        self.base_path.join(key)
    }

    fn get_meta_path(&self, key: &str) -> PathBuf {
        self.base_path.join(format!("{}.meta", key))
    }
}

/// Outcome of reading a cache entry from disk.
enum DiskRead {
    Missing,
    Expired { size: u64 },
    Hit(Vec<u8>),
}

/// Read a cache entry and its optional TTL sidecar in one go. Meant to run on
/// the blocking pool, so the whole lookup costs a single hop.
fn read_entry(file_path: &Path, meta_path: &Path) -> std::io::Result<DiskRead> {
    let data = match fs::read(file_path) {
        Ok(data) => data,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(DiskRead::Missing),
        Err(e) => return Err(e),
    };

    // No sidecar means no TTL; that's a single failed open().
    let expired = match fs::read_to_string(meta_path) {
        Ok(content) => content.trim().parse::<u64>().is_ok_and(|expiry| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|now| now.as_secs() > expiry)
                .unwrap_or(false)
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };

    if expired {
        let size = data.len() as u64;
        fs::remove_file(file_path)?;
        let _ = fs::remove_file(meta_path);
        return Ok(DiskRead::Expired { size });
    }
    Ok(DiskRead::Hit(data))
}

/// Remove a file, treating "already gone" as success.
async fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match tokio_fs::remove_file(path).await {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[async_trait]
impl AudioCache for FileSystemCache {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let file_path = self.get_file_path(key);
        let meta_path = self.get_meta_path(key);

        let read =
            tokio::task::spawn_blocking(move || read_entry(&file_path, &meta_path)).await??;
        match read {
            DiskRead::Missing => Ok(None),
            DiskRead::Expired { size } => {
                // Expired entries are removed proactively by `read_entry`.
                self.evictor.track_delete(size);
                Ok(None)
            }
            DiskRead::Hit(data) => Ok(Some(Bytes::from(data))),
        }
    }

    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<()> {
        let file_path = self.get_file_path(key);

        // Check existing size before overwriting
        let old_size = tokio_fs::metadata(&file_path).await.map(|m| m.len()).ok();

        write_atomic(file_path, value.clone()).await?;

        if let Some(duration) = ttl {
            let meta_path = self.get_meta_path(key);
            let expiry = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
                + duration.as_secs();

            if let Err(e) = write_atomic(meta_path, expiry.to_string().into()).await {
                // Data file was written successfully — still track it even
                // though the sidecar failed, so the counter stays accurate.
                if let Some(old) = old_size {
                    self.evictor.track_delete(old);
                }
                self.evictor.track_write(value.len() as u64);
                return Err(e.into());
            }
        }

        // Only adjust the counter after all I/O succeeded
        if let Some(old) = old_size {
            self.evictor.track_delete(old);
        }
        self.evictor.track_write(value.len() as u64);

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let file_path = self.get_file_path(key);
        let meta_path = self.get_meta_path(key);

        if let Ok(meta) = tokio_fs::metadata(&file_path).await {
            remove_if_exists(&file_path).await?;
            self.evictor.track_delete(meta.len());
        }
        remove_if_exists(&meta_path).await?;

        debug!(key, "deleted cache entry");
        Ok(())
    }
}
