use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::debug;

const FAILURE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{inner}")]
pub struct CoalesceError {
    inner: String,
}

pub enum CoalesceResult {
    /// You are the leader - do the work, then call `guard.complete()` or `guard.fail()`.
    Leader(InflightGuard),
    /// Another request did the work - check result storage.
    Waiter(Result<(), CoalesceError>),
    /// A recent leader failed for this key - fast-fail with cached error.
    Failed(CoalesceError),
}

impl std::fmt::Debug for CoalesceResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Leader(_) => write!(f, "Leader(...)"),
            Self::Waiter(Ok(())) => write!(f, "Waiter(Ok)"),
            Self::Waiter(Err(e)) => write!(f, "Waiter(Err({e}))"),
            Self::Failed(e) => write!(f, "Failed({e})"),
        }
    }
}

struct InflightEntry {
    notify: Notify,
    result: std::sync::Mutex<Option<Result<(), CoalesceError>>>,
}

struct FailedEntry {
    error: String,
    failed_at: Instant,
}

pub struct InflightTracker {
    inflight: Arc<DashMap<String, Arc<InflightEntry>>>,
    failed: Arc<DashMap<String, FailedEntry>>,
}

impl Default for InflightTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl InflightTracker {
    pub fn new() -> Self {
        Self {
            inflight: Arc::new(DashMap::new()),
            failed: Arc::new(DashMap::new()),
        }
    }

    /// Attempt to join an in-flight request for `key`.
    ///
    /// Returns `Leader` if no one is currently processing this key (you do the work),
    /// `Waiter` if another request already finished while we waited,
    /// or `Failed` if a recent attempt for this key failed and is within the TTL.
    pub async fn try_join(&self, key: &str) -> CoalesceResult {
        loop {
            // Check failure cache first (lazy eviction of expired entries).
            if let Some(entry) = self.failed.get(key) {
                if entry.failed_at.elapsed() < FAILURE_TTL {
                    metrics::counter!("request.coalesced.failed_cache_hit").increment(1);
                    return CoalesceResult::Failed(CoalesceError {
                        inner: entry.error.clone(),
                    });
                }
                let failed_at = entry.failed_at;
                drop(entry);
                self.failed.remove_if(key, |_, v| v.failed_at == failed_at);
            }

            // Atomic leader election via DashMap::entry.
            match self.inflight.entry(key.to_owned()) {
                Entry::Vacant(v) => {
                    let new_entry = Arc::new(InflightEntry {
                        notify: Notify::new(),
                        result: std::sync::Mutex::new(None),
                    });
                    v.insert(Arc::clone(&new_entry));
                    metrics::counter!("request.coalesced.leader").increment(1);

                    return CoalesceResult::Leader(InflightGuard {
                        entry: new_entry,
                        key: key.to_owned(),
                        inflight: Arc::clone(&self.inflight),
                        failed: Arc::clone(&self.failed),
                    });
                }
                Entry::Occupied(o) => {
                    let entry = Arc::clone(o.get());
                    drop(o); // release shard lock before awaiting

                    metrics::counter!("request.coalesced.waiter").increment(1);
                    debug!(key, "coalescing: waiting on in-flight request");

                    // Create the Notified future BEFORE checking result, so that a
                    // concurrent notify_waiters() between our check and await is
                    // not lost.
                    let notified = entry.notify.notified();
                    tokio::pin!(notified);

                    // If the leader already finished, the result is set — return
                    // immediately without awaiting.
                    if let Some(r) = entry.result.lock().unwrap().clone() {
                        return CoalesceResult::Waiter(r);
                    }

                    notified.await;

                    match entry.result.lock().unwrap().clone() {
                        Some(r) => return CoalesceResult::Waiter(r),
                        None => {
                            // Leader was cancelled/panicked without setting a result.
                            // Loop back to retry atomic leader election.
                            metrics::counter!("request.coalesced.waiter_retry").increment(1);
                            debug!(key, "coalescing: leader died, promoting to leader");
                            continue;
                        }
                    }
                }
            }
        }
    }
}

pub struct InflightGuard {
    entry: Arc<InflightEntry>,
    key: String,
    inflight: Arc<DashMap<String, Arc<InflightEntry>>>,
    failed: Arc<DashMap<String, FailedEntry>>,
}

impl InflightGuard {
    /// Mark this key's processing as successful and notify all waiters.
    ///
    /// Call this **after** writing to result_storage so waiters can read the result.
    pub fn complete(self) {
        *self.entry.result.lock().unwrap() = Some(Ok(()));
        // Drop runs next: notifies waiters and removes the inflight entry.
    }

    /// Mark this key's processing as failed, write to the failure cache, and notify waiters.
    pub fn fail(self, error: String) {
        let err = CoalesceError {
            inner: error.clone(),
        };
        *self.entry.result.lock().unwrap() = Some(Err(err));
        self.failed.insert(
            self.key.clone(),
            FailedEntry {
                error,
                failed_at: Instant::now(),
            },
        );
        // Drop runs next: notifies waiters and removes the inflight entry.
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Always notify waiters so they never hang — even on panic/cancel.
        self.entry.notify.notify_waiters();
        // Only remove our own entry — a successor leader may have already
        // inserted a new entry for the same key.
        self.inflight
            .remove_if(&self.key, |_, current| Arc::ptr_eq(current, &self.entry));
    }
}
