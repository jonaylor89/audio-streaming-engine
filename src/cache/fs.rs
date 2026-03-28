use super::AudioCache;
use async_trait::async_trait;
use color_eyre::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::fs as tokio_fs;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct FileSystemCache {
    base_path: PathBuf,
    max_size_bytes: u64,
}

impl FileSystemCache {
    pub fn new<P: AsRef<Path>>(base_path: P, max_size_mb: u64) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        // Create directory if it doesn't exist
        fs::create_dir_all(&base_path)?;
        Ok(FileSystemCache {
            base_path,
            max_size_bytes: max_size_mb * 1024 * 1024,
        })
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

    async fn maybe_evict(&self) -> Result<()> {
        if self.max_size_bytes == 0 {
            return Ok(());
        }

        let mut entries = Vec::new();
        let mut total_size: u64 = 0;
        let mut dir = tokio_fs::read_dir(&self.base_path).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Ok(meta) = entry.metadata().await {
                if meta.is_file() {
                    let path = entry.path();
                    // Skip .meta files from size accounting
                    if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                        continue;
                    }
                    let size = meta.len();
                    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    total_size += size;
                    entries.push((path, size, modified));
                }
            }
        }

        if total_size <= self.max_size_bytes {
            return Ok(());
        }

        // Sort oldest first (LRU eviction)
        entries.sort_by_key(|(_, _, modified)| *modified);
        for (path, size, _) in &entries {
            if total_size <= self.max_size_bytes {
                break;
            }
            debug!(path = %path.display(), "evicting cached entry");
            if let Err(e) = tokio_fs::remove_file(path).await {
                warn!(path = %path.display(), error = %e, "failed to evict cached file");
            } else {
                total_size -= size;
                // Also remove the corresponding .meta file
                let mut meta_name = path.file_name().unwrap_or_default().to_os_string();
                meta_name.push(".meta");
                let meta_path = path.with_file_name(meta_name);
                let _ = tokio_fs::remove_file(meta_path).await;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl AudioCache for FileSystemCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let file_path = self.get_file_path(key);

        if !file_path.exists() {
            return Ok(None);
        }

        if self.is_expired(key).await? {
            // Proactively clean up expired entries
            let _ = tokio_fs::remove_file(&file_path).await;
            let _ = tokio_fs::remove_file(self.get_meta_path(key)).await;
            return Ok(None);
        }

        let contents = tokio_fs::read(&file_path).await?;
        Ok(Some(contents))
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<()> {
        let file_path = self.get_file_path(key);

        // Write the actual data
        tokio_fs::write(&file_path, value).await?;

        // If TTL is specified, write the expiration time
        if let Some(duration) = ttl {
            let meta_path = self.get_meta_path(key);
            let expiry = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
                + duration.as_secs();

            tokio_fs::write(&meta_path, expiry.to_string()).await?;
        }

        // Evict oldest entries if over size limit
        if let Err(e) = self.maybe_evict().await {
            warn!(error = %e, "failed to run cache eviction");
        }

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let file_path = self.get_file_path(key);
        let meta_path = self.get_meta_path(key);

        // Delete both the data file and meta file if they exist
        if file_path.exists() {
            tokio_fs::remove_file(&file_path).await?;
        }
        if meta_path.exists() {
            tokio_fs::remove_file(&meta_path).await?;
        }

        Ok(())
    }
}
