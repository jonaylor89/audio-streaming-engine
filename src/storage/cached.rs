use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use color_eyre::Result;
use tokio::fs;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::blob::AudioBuffer;
use crate::config::LocalCacheSettings;
use crate::storage::backend::ByteStream;
use crate::storage::AudioStorage;

#[derive(Clone)]
pub struct CachedStorage<S> {
    inner: S,
    cache_dir: PathBuf,
    evict_notify: Arc<Notify>,
}

impl<S: AudioStorage + Clone + Send + Sync + 'static> CachedStorage<S> {
    pub fn new(inner: S, settings: &LocalCacheSettings) -> Self {
        let cache_dir = PathBuf::from(&settings.base_dir);
        let max_size_bytes = settings.max_size_mb * 1024 * 1024;
        let evict_notify = Arc::new(Notify::new());

        // Spawn background eviction task
        let bg_dir = cache_dir.clone();
        let bg_max = max_size_bytes;
        let bg_notify = evict_notify.clone();
        tokio::spawn(async move {
            loop {
                bg_notify.notified().await;
                if let Err(e) = run_eviction(&bg_dir, bg_max).await {
                    warn!(error = %e, "background eviction failed");
                }
            }
        });

        Self {
            inner,
            cache_dir,
            evict_notify,
        }
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        let safe_key = key.replace(['/', '\\'], "_");
        self.cache_dir.join(safe_key)
    }

    async fn read_from_cache(&self, key: &str) -> Option<AudioBuffer> {
        let path = self.cache_path(key);
        match fs::read(&path).await {
            Ok(data) => {
                debug!(key, "local cache hit");
                Some(AudioBuffer::from_bytes(data))
            }
            Err(_) => None,
        }
    }

    async fn write_to_cache(&self, key: &str, blob: &AudioBuffer) {
        let path = self.cache_path(key);
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent).await
        {
            warn!(key, error = %e, "failed to create local cache directory");
            return;
        }

        if let Err(e) = fs::write(&path, blob.as_ref()).await {
            warn!(key, error = %e, "failed to write to local cache");
        } else {
            debug!(key, "cached source blob locally");
        }

        // Signal background eviction (non-blocking)
        self.evict_notify.notify_one();
    }
}

async fn run_eviction(cache_dir: &PathBuf, max_size_bytes: u64) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }

    let mut entries = Vec::new();
    let mut total_size: u64 = 0;
    let mut dir = fs::read_dir(cache_dir).await?;
    while let Some(entry) = dir.next_entry().await? {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            let size = meta.len();
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            total_size += size;
            entries.push((entry.path(), size, modified));
        }
    }

    if total_size <= max_size_bytes {
        return Ok(());
    }

    // Evict oldest first
    entries.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in &entries {
        if total_size <= max_size_bytes {
            break;
        }
        debug!(path = %path.display(), "evicting cached source blob");
        if let Err(e) = fs::remove_file(path).await {
            warn!(path = %path.display(), error = %e, "failed to evict cached file");
        } else {
            total_size -= size;
        }
    }

    Ok(())
}

#[async_trait]
impl<S: AudioStorage + Clone + Send + Sync + 'static> AudioStorage for CachedStorage<S> {
    async fn get(&self, key: &str) -> Result<AudioBuffer> {
        if let Some(blob) = self.read_from_cache(key).await {
            return Ok(blob);
        }

        let blob = self.inner.get(key).await?;
        self.write_to_cache(key, &blob).await;
        Ok(blob)
    }

    async fn get_stream(&self, key: &str) -> Result<ByteStream> {
        // If the file is already in the local cache, stream from disk
        let cache_path = self.cache_path(key);
        if cache_path.exists() {
            debug!(key, "streaming from local cache");
            let file = tokio::fs::File::open(cache_path).await?;
            let stream = tokio_util::io::ReaderStream::new(file);
            return Ok(Box::pin(futures::stream::StreamExt::map(stream, |r| {
                r.map_err(|e| e.into())
            })));
        }

        // Cache miss: stream from inner and tee to cache in background
        let inner_stream = self.inner.get_stream(key).await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes>>(8);
        let bg_cache_path = cache_path;
        let bg_notify = self.evict_notify.clone();

        // Tee task: forward chunks to the HTTP consumer channel and collect for caching
        tokio::spawn(async move {
            use futures::StreamExt;

            futures::pin_mut!(inner_stream);
            let mut cache_buf = bytes::BytesMut::new();
            while let Some(chunk) = inner_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        cache_buf.extend_from_slice(&bytes);
                        if tx.send(Ok(bytes)).await.is_err() {
                            return; // consumer disconnected
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }

            // Write collected bytes to local cache
            if !cache_buf.is_empty() {
                if let Some(parent) = bg_cache_path.parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
                if let Err(e) = fs::write(&bg_cache_path, &cache_buf).await {
                    warn!(error = %e, "failed to write stream to local cache");
                } else {
                    debug!("cached streamed source blob locally");
                    bg_notify.notify_one();
                }
            }
        });

        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })))
    }

    async fn put(&self, key: &str, blob: &AudioBuffer) -> Result<()> {
        self.inner.put(key, blob).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // Remove from local cache too
        let cache_path = self.cache_path(key);
        let _ = fs::remove_file(cache_path).await;
        self.inner.delete(key).await
    }

    async fn list(&self) -> Result<Vec<String>> {
        self.inner.list().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::AudioFormat;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::tempdir;

    #[derive(Clone)]
    struct MockStorage {
        fetch_count: std::sync::Arc<AtomicU32>,
    }

    #[async_trait]
    impl AudioStorage for MockStorage {
        async fn get(&self, _key: &str) -> Result<AudioBuffer> {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            Ok(AudioBuffer::from_bytes_with_format(
                vec![0xFF, 0xFB, 0x90, 0x00],
                AudioFormat::Mp3,
            ))
        }
        async fn put(&self, _key: &str, _blob: &AudioBuffer) -> Result<()> {
            Ok(())
        }
        async fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_cache_hit_avoids_inner_fetch() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 100,
        };
        let cached = CachedStorage::new(mock.clone(), &settings);

        // First fetch should hit inner
        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);

        // Second fetch should come from local cache
        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_eviction_respects_max_size() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        // 1 byte max — forces eviction on every write
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 0,
        };
        let cached = CachedStorage::new(mock, &settings);

        let _ = cached.get("a.mp3").await.unwrap();
        let _ = cached.get("b.mp3").await.unwrap();

        // Yield to let the background eviction task run after the notify
        tokio::task::yield_now().await;

        // Cache dir should have at most 1 file (the latest) since eviction runs after writes
        let mut count = 0;
        let mut dir = fs::read_dir(temp.path()).await.unwrap();
        while dir.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert!(count <= 1);
    }

    #[tokio::test]
    async fn test_delete_removes_from_cache() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 100,
        };
        let cached = CachedStorage::new(mock.clone(), &settings);

        // Populate cache
        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);

        // Delete should remove from cache
        let _ = cached.delete("test.mp3").await;

        // Next get should hit inner again
        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 2);
    }
}
