use super::AudioCache;
use crate::disk_evictor::DiskEvictor;
use async_trait::async_trait;
use bytes::Bytes;
use color_eyre::Result;
use std::fs;
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

    async fn is_expired(&self, key: &str) -> Result<bool> {
        let meta_path = self.get_meta_path(key);

        if !meta_path.exists() {
            return Ok(false);
        }

        let content = tokio_fs::read_to_string(&meta_path).await?;
        if let Ok(expiry) = content.parse::<u64>() {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs();
            Ok(now > expiry)
        } else {
            Ok(false)
        }
    }
}

#[async_trait]
impl AudioCache for FileSystemCache {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let file_path = self.get_file_path(key);

        if !file_path.exists() {
            return Ok(None);
        }

        if self.is_expired(key).await? {
            // Proactively clean up expired entries — only track after successful delete
            let size = tokio_fs::metadata(&file_path).await.map(|m| m.len()).ok();
            if tokio_fs::remove_file(&file_path).await.is_ok()
                && let Some(s) = size
            {
                self.evictor.track_delete(s);
            }
            let _ = tokio_fs::remove_file(self.get_meta_path(key)).await;
            return Ok(None);
        }

        let contents = tokio_fs::read(&file_path).await?;
        Ok(Some(Bytes::from(contents)))
    }

    async fn set(&self, key: &str, value: Bytes, ttl: Option<Duration>) -> Result<()> {
        let file_path = self.get_file_path(key);

        // Check existing size before overwriting
        let old_size = tokio_fs::metadata(&file_path).await.map(|m| m.len()).ok();

        tokio_fs::write(&file_path, &value).await?;

        if let Some(duration) = ttl {
            let meta_path = self.get_meta_path(key);
            let expiry = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
                + duration.as_secs();

            if let Err(e) = tokio_fs::write(&meta_path, expiry.to_string()).await {
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

        if file_path.exists() {
            let size = tokio_fs::metadata(&file_path).await.map(|m| m.len()).ok();
            tokio_fs::remove_file(&file_path).await?;
            if let Some(s) = size {
                self.evictor.track_delete(s);
            }
        }
        if meta_path.exists() {
            tokio_fs::remove_file(&meta_path).await?;
        }

        debug!(key, "deleted cache entry");
        Ok(())
    }
}
