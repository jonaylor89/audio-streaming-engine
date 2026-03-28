use super::AudioCache;
use async_trait::async_trait;
use color_eyre::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::fs as tokio_fs;
use tokio::sync::Notify;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct FileSystemCache {
    base_path: PathBuf,
    evict_notify: Arc<Notify>,
}

impl FileSystemCache {
    pub fn new<P: AsRef<Path>>(base_path: P, max_size_mb: u64) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base_path)?;
        let max_size_bytes = max_size_mb * 1024 * 1024;
        let evict_notify = Arc::new(Notify::new());

        // Spawn background eviction task
        let bg_path = base_path.clone();
        let bg_max = max_size_bytes;
        let bg_notify = evict_notify.clone();
        tokio::spawn(async move {
            loop {
                bg_notify.notified().await;
                if let Err(e) = run_eviction(&bg_path, bg_max).await {
                    warn!(error = %e, "background cache eviction failed");
                }
            }
        });

        Ok(FileSystemCache {
            base_path,
            evict_notify,
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
}

async fn run_eviction(base_path: &Path, max_size_bytes: u64) -> Result<()> {
    if max_size_bytes == 0 {
        return Ok(());
    }

    let mut entries = Vec::new();
    let mut total_size: u64 = 0;
    let mut dir = tokio_fs::read_dir(base_path).await?;
    while let Some(entry) = dir.next_entry().await? {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                continue;
            }
            let size = meta.len();
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total_size += size;
            entries.push((path, size, modified));
        }
    }

    if total_size <= max_size_bytes {
        return Ok(());
    }

    entries.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in &entries {
        if total_size <= max_size_bytes {
            break;
        }
        debug!(path = %path.display(), "evicting cached entry");
        if let Err(e) = tokio_fs::remove_file(path).await {
            warn!(path = %path.display(), error = %e, "failed to evict cached file");
        } else {
            total_size -= size;
            let mut meta_name = path.file_name().unwrap_or_default().to_os_string();
            meta_name.push(".meta");
            let meta_path = path.with_file_name(meta_name);
            let _ = tokio_fs::remove_file(meta_path).await;
        }
    }

    Ok(())
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

        tokio_fs::write(&file_path, value).await?;

        if let Some(duration) = ttl {
            let meta_path = self.get_meta_path(key);
            let expiry = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs()
                + duration.as_secs();

            tokio_fs::write(&meta_path, expiry.to_string()).await?;
        }

        // Signal background eviction (non-blocking)
        self.evict_notify.notify_one();

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
