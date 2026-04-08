use std::path::PathBuf;

use async_trait::async_trait;
use color_eyre::Result;
use tokio::fs;
use tracing::{debug, warn};

use crate::blob::AudioBuffer;
use crate::config::LocalCacheSettings;
use crate::disk_evictor::DiskEvictor;
use crate::storage::AudioStorage;
use crate::storage::backend::ByteStream;

#[derive(Clone)]
pub struct CachedStorage<S> {
    inner: S,
    cache_dir: PathBuf,
    evictor: DiskEvictor,
}

impl<S: AudioStorage + Clone + Send + Sync + 'static> CachedStorage<S> {
    pub fn new(inner: S, settings: &LocalCacheSettings) -> Self {
        let cache_dir = PathBuf::from(&settings.base_dir);
        let max_size_bytes = settings.max_size_mb * 1024 * 1024;
        let evictor = DiskEvictor::new(cache_dir.clone(), max_size_bytes, None);

        Self {
            inner,
            cache_dir,
            evictor,
        }
    }

    /// Build a `CachedStorage` with a **manual** evictor (no background task).
    /// Tests can call `evictor().scan()` / `evictor().evict()` explicitly.
    #[cfg(test)]
    pub fn new_manual(inner: S, settings: &LocalCacheSettings) -> Self {
        let cache_dir = PathBuf::from(&settings.base_dir);
        let max_size_bytes = settings.max_size_mb * 1024 * 1024;
        let evictor = DiskEvictor::manual(cache_dir.clone(), max_size_bytes, None);

        Self {
            inner,
            cache_dir,
            evictor,
        }
    }

    /// Returns a reference to the inner evictor (for test assertions).
    #[cfg(test)]
    pub fn evictor(&self) -> &DiskEvictor {
        &self.evictor
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

        let data = blob.as_ref();
        if let Err(e) = fs::write(&path, data).await {
            warn!(key, error = %e, "failed to write to local cache");
        } else {
            debug!(key, "cached source blob locally");
            self.evictor.track_write(data.len() as u64);
        }
    }
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
        let bg_evictor = self.evictor.clone();

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
                let len = cache_buf.len() as u64;
                if let Err(e) = fs::write(&bg_cache_path, &cache_buf).await {
                    warn!(error = %e, "failed to write stream to local cache");
                } else {
                    debug!("cached streamed source blob locally");
                    bg_evictor.track_write(len);
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
        let cache_path = self.cache_path(key);
        let size = fs::metadata(&cache_path).await.map(|m| m.len()).ok();
        if fs::remove_file(&cache_path).await.is_ok()
            && let Some(s) = size
        {
            self.evictor.track_delete(s);
        }
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

    /// Second fetch for the same key returns cached data without hitting inner.
    #[tokio::test]
    async fn cache_hit_avoids_inner_fetch() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 100,
        };
        let cached = CachedStorage::new_manual(mock.clone(), &settings);

        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);

        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);
    }

    /// Manual eviction with max_size=0 removes all cached files.
    #[tokio::test]
    async fn eviction_respects_max_size() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 0,
        };
        let cached = CachedStorage::new_manual(mock, &settings);

        let _ = cached.get("a.mp3").await.unwrap();
        let _ = cached.get("b.mp3").await.unwrap();

        // Deterministic: scan + evict, no polling needed.
        cached.evictor().evict().await.unwrap();

        let count = std::fs::read_dir(temp.path()).unwrap().count();
        assert!(
            count <= 1,
            "expected at most 1 file after eviction, found {count}"
        );
    }

    /// Deleting a key removes it from the local cache so the next get
    /// hits inner again.
    #[tokio::test]
    async fn delete_removes_from_cache() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 100,
        };
        let cached = CachedStorage::new_manual(mock.clone(), &settings);

        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 1);

        let _ = cached.delete("test.mp3").await;

        let _ = cached.get("test.mp3").await.unwrap();
        assert_eq!(mock.fetch_count.load(Ordering::SeqCst), 2);
    }

    /// The evictor counter stays in sync with what CachedStorage writes and
    /// deletes, without any background task.
    #[tokio::test]
    async fn evictor_counter_tracks_writes_and_deletes() {
        let temp = tempdir().unwrap();
        let mock = MockStorage {
            fetch_count: std::sync::Arc::new(AtomicU32::new(0)),
        };
        let settings = LocalCacheSettings {
            base_dir: temp.path().to_str().unwrap().to_string(),
            max_size_mb: 100,
        };
        let cached = CachedStorage::new_manual(mock, &settings);

        let _ = cached.get("x.mp3").await.unwrap();
        assert_eq!(cached.evictor().current_bytes(), 4); // 4-byte mock payload

        let _ = cached.delete("x.mp3").await;
        assert_eq!(cached.evictor().current_bytes(), 0);
    }
}
