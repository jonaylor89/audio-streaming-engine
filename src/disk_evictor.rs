use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use color_eyre::Result;
use tokio::fs;
use tokio::sync::Notify;
use tracing::{debug, warn};

/// Shared state between the `DiskEvictor` handle(s) and the background task.
#[derive(Debug)]
struct EvictorInner {
    current_bytes: AtomicU64,
    evict_notify: Notify,
    shutdown: AtomicBool,
    /// `true` when a background task was spawned and needs shutdown signalling.
    has_background_task: bool,
}

/// A shared disk-based eviction tracker that maintains a running size counter
/// instead of scanning the directory on every write.
///
/// The counter is kept up-to-date via `track_write` / `track_delete`. A full
/// directory scan only happens during `scan()` (startup) and `evict()`.
///
/// # Modes
///
/// * **Background** (`DiskEvictor::new`) — spawns a tokio task that scans on
///   startup and runs eviction automatically when the counter exceeds the
///   limit.  The task shuts down when the last clone is dropped.
///
/// * **Manual** (`DiskEvictor::manual`) — no background task.  The caller is
///   responsible for calling `scan()` and `evict()` explicitly.  This is the
///   preferred mode for tests because it is fully deterministic.
#[derive(Debug, Clone)]
pub struct DiskEvictor {
    dir: PathBuf,
    max_bytes: u64,
    skip_ext: Option<&'static str>,
    inner: Arc<EvictorInner>,
}

impl Drop for DiskEvictor {
    fn drop(&mut self) {
        if !self.inner.has_background_task {
            return;
        }
        // strong_count == 2 means *this* clone + background task; after this
        // drop the task's Arc is the only one left — signal shutdown.
        if Arc::strong_count(&self.inner) == 2 {
            self.inner.shutdown.store(true, Ordering::Release);
            self.inner.evict_notify.notify_one();
        }
    }
}

impl DiskEvictor {
    /// Create a new evictor **with** a background eviction task.
    ///
    /// The background task scans the directory once to initialise the size
    /// counter, then loops waiting for `track_write` to signal that the
    /// counter has exceeded `max_bytes`.
    ///
    /// `skip_ext` — if `Some("meta")`, files ending in `.meta` are excluded
    /// from size accounting and will only be deleted as companions of their
    /// parent cache entry.
    pub fn new(dir: PathBuf, max_bytes: u64, skip_ext: Option<&'static str>) -> Self {
        let inner = Arc::new(EvictorInner {
            current_bytes: AtomicU64::new(0),
            evict_notify: Notify::new(),
            shutdown: AtomicBool::new(false),
            has_background_task: true,
        });

        let bg_dir = dir.clone();
        let bg_inner = inner.clone();
        let bg_skip = skip_ext;
        let bg_max = max_bytes;
        tokio::spawn(async move {
            // Initialise size counter
            match scan_total_size(&bg_dir, bg_skip).await {
                Ok(size) => {
                    bg_inner.current_bytes.store(size, Ordering::Release);
                    if size > bg_max {
                        bg_inner.evict_notify.notify_one();
                    }
                }
                Err(e) => {
                    warn!(error = %e, "initial cache size scan failed; counter starts at 0");
                }
            }

            // Eviction loop
            loop {
                bg_inner.evict_notify.notified().await;

                if bg_inner.shutdown.load(Ordering::Acquire) {
                    debug!("disk evictor background task shutting down");
                    return;
                }

                if bg_inner.current_bytes.load(Ordering::Acquire) <= bg_max {
                    continue;
                }
                match run_eviction(&bg_dir, bg_max, bg_skip).await {
                    Ok(freed) => {
                        saturating_sub(&bg_inner.current_bytes, freed);
                    }
                    Err(e) => {
                        warn!(error = %e, "background cache eviction failed");
                    }
                }
                // If still over limit after eviction, retry on next iteration
                if bg_inner.current_bytes.load(Ordering::Acquire) > bg_max {
                    bg_inner.evict_notify.notify_one();
                }
            }
        });

        Self {
            dir,
            max_bytes,
            skip_ext,
            inner,
        }
    }

    /// Create a new evictor **without** a background task.
    ///
    /// The caller must explicitly call [`scan`](Self::scan) and
    /// [`evict`](Self::evict) as needed.  This mode is deterministic and
    /// intended for tests.
    pub fn manual(dir: PathBuf, max_bytes: u64, skip_ext: Option<&'static str>) -> Self {
        let inner = Arc::new(EvictorInner {
            current_bytes: AtomicU64::new(0),
            evict_notify: Notify::new(),
            shutdown: AtomicBool::new(false),
            has_background_task: false,
        });

        Self {
            dir,
            max_bytes,
            skip_ext,
            inner,
        }
    }

    // ── public operations ────────────────────────────────────────────────

    /// Scan the cache directory and **set** the size counter to the actual
    /// total on disk.  Safe to call more than once (resets the counter).
    pub async fn scan(&self) -> Result<()> {
        let size = scan_total_size(&self.dir, self.skip_ext).await?;
        self.inner.current_bytes.store(size, Ordering::Release);
        Ok(())
    }

    /// Run one eviction pass: delete oldest files until the total is at or
    /// below `max_bytes`.  Returns the number of bytes freed.
    ///
    /// This is the same logic the background task runs, exposed so that
    /// callers (and tests) can trigger it synchronously.
    pub async fn evict(&self) -> Result<u64> {
        let freed = run_eviction(&self.dir, self.max_bytes, self.skip_ext).await?;
        saturating_sub(&self.inner.current_bytes, freed);
        Ok(freed)
    }

    /// Record a write of `size` bytes to the cache.
    /// In background mode this signals eviction if over limit.
    pub fn track_write(&self, size: u64) {
        let new = self.inner.current_bytes.fetch_add(size, Ordering::AcqRel) + size;
        if new > self.max_bytes && self.inner.has_background_task {
            self.inner.evict_notify.notify_one();
        }
    }

    /// Record deletion of `size` bytes from the cache (saturating).
    pub fn track_delete(&self, size: u64) {
        saturating_sub(&self.inner.current_bytes, size);
    }

    /// Returns the cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the current tracked size in bytes.
    pub fn current_bytes(&self) -> u64 {
        self.inner.current_bytes.load(Ordering::Acquire)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Atomically subtract `n` from `counter`, saturating at 0.
fn saturating_sub(counter: &AtomicU64, n: u64) {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(current.saturating_sub(n))
        })
        .ok();
}

/// Scan the directory once to compute total size of data files.
async fn scan_total_size(dir: &Path, skip_ext: Option<&str>) -> Result<u64> {
    let mut total: u64 = 0;
    if !dir.exists() {
        return Ok(0);
    }
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            if should_skip(&entry.path(), skip_ext) {
                continue;
            }
            total += meta.len();
        }
    }
    Ok(total)
}

/// Evict oldest files until total size is within `max_bytes`.
/// Returns the number of **bytes freed** (not the new total).
async fn run_eviction(dir: &Path, max_bytes: u64, skip_ext: Option<&str>) -> Result<u64> {
    let mut entries = Vec::new();
    let mut rd = fs::read_dir(dir).await?;
    let mut total: u64 = 0;
    while let Some(entry) = rd.next_entry().await? {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            if should_skip(&entry.path(), skip_ext) {
                continue;
            }
            let size = meta.len();
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total += size;
            entries.push((entry.path(), size, modified));
        }
    }

    if total <= max_bytes {
        return Ok(0);
    }

    let mut freed: u64 = 0;
    entries.sort_by_key(|(_, _, modified)| *modified);
    for (path, size, _) in &entries {
        if total <= max_bytes {
            break;
        }
        debug!(path = %path.display(), "evicting cached entry");
        if let Err(e) = fs::remove_file(path).await {
            warn!(path = %path.display(), error = %e, "failed to evict cached file");
        } else {
            total -= size;
            freed += size;
            // Also remove companion sidecar if present
            if skip_ext.is_some() {
                let mut meta_name = path.file_name().unwrap_or_default().to_os_string();
                meta_name.push(".meta");
                let meta_path = path.with_file_name(meta_name);
                let _ = fs::remove_file(meta_path).await;
            }
        }
    }

    Ok(freed)
}

fn should_skip(path: &Path, skip_ext: Option<&str>) -> bool {
    if let Some(ext) = skip_ext {
        path.extension().and_then(|e| e.to_str()) == Some(ext)
    } else {
        false
    }
}
