# Request Coalescing / Deduplication — Architecture Design

**Status:** Draft  
**Addresses:** [PerformanceAudit §7 — No Request Deduplication (Thundering Herd)](./PerformanceAudit.md)  
**References:** [fasterthanli.me — Request coalescing in async Rust](https://fasterthanli.me/articles/request-coalescing-in-async-rust)

---

## Problem

When N concurrent requests arrive for the same uncached audio + params combination, **all N independently**:

1. Check cache → miss
2. Check result storage → miss
3. Fetch source audio from storage / HTTP
4. Acquire a semaphore permit (queue behind `num_cpus` slots)
5. Run FFmpeg
6. Write to result storage + response cache

The semaphore bounds CPU parallelism, but N−1 requests still do redundant I/O, memory allocation, and queueing. Under bursty traffic (e.g. a podcast RSS feed update, a shared link going viral), this wastes significant resources.

---

## Design Goals

1. **Coalesce identical in-flight work** — the first request for a given `(params_hash)` does the real work; subsequent requests subscribe to the result.
2. **Scoped to buffered endpoint only** — `streamingpath_handler` only. The streaming endpoint (`stream_handler`) is excluded from coalescing for now to avoid the complexity of fan-out streaming or forcing waiters into a different response mode.
3. **Cancellation-safe** — if the "leader" request is dropped (client disconnect, timeout, panic), waiters must not hang forever; they should either retry or receive an error.
4. **Failure-aware** — repeated failures for the same key must not cause infinite retry loops; failed keys are temporarily blacklisted.
5. **No global lock contention** — use a concurrent map, not a `Mutex<HashMap>`.
6. **Minimal footprint** — clean up map entries as soon as the in-flight work completes.

---

## Key Insight from fasterthanli.me

The blog post walks through several iterations:

| Approach | Pros | Cons |
|----------|------|------|
| **`Mutex<HashMap<K, broadcast::Sender>>`** | Simple | Mutex contention; `broadcast` requires `Clone + Send + Sync` on the value; need manual cleanup |
| **`Weak<broadcast::Sender>`** | Auto-cleanup when leader dies (panic/drop) — waiters see `RecvError` and can retry | Still needs mutex for the map |
| **`DashMap` + `tokio::sync::broadcast`** | Lock-free reads; sharded writes | `broadcast` clones the value per receiver (fine for small values, expensive for large audio buffers) |
| **`DashMap` + `Shared<JoinHandle>`** | Waiters `.await` a shared future; value produced once | Requires `futures::future::Shared`; the future must be `Clone`-able |

### Best fit for this project

Audio buffers are large (megabytes). We should **not** use `broadcast` because it clones the payload per subscriber. Instead:

**`DashMap<String, Arc<Notify>>` + result storage as the "mailbox"**

The leader processes and writes to result storage. Waiters are notified and read from result storage. The payload is never cloned through the coalescing layer — it flows through the existing storage path.

---

## Proposed Architecture

### New component: `InflightTracker`

```
┌─────────────────────────────────────────────────────────┐
│                    InflightTracker                      │
│                                                         │
│   inflight: DashMap<String, Arc<InflightEntry>>         │
│                                                         │
│   InflightEntry {                                       │
│       notify: tokio::sync::Notify,                      │
│       result: Mutex<Option<Result<(), CoalesceError>>>, │
│   }                                                     │
└─────────────────────────────────────────────────────────┘
```

### Flow

```
Request arrives with params_hash = "abc123"
        │
        ▼
┌─────────────────────┐
│ cache_middleware     │──▶ cache HIT → return cached response (unchanged)
│ checks resp cache   │
└────────┬────────────┘
         │ cache MISS
         ▼
┌─────────────────────┐
│ Handler checks      │──▶ result storage HIT → return (unchanged)
│ result_storage      │
└────────┬────────────┘
         │ result storage MISS
         ▼
┌──────────────────────────────────────────┐
│ inflight.entry("abc123")                 │
│                                          │
│   Vacant? ──▶ I am the LEADER            │
│     • Insert Arc<InflightEntry>          │
│     • Fetch source                       │
│     • Run FFmpeg                         │
│     • Write to result_storage            │
│     • Set result = Ok(())                │
│     • notify.notify_waiters()            │
│     • Remove entry from map              │
│     • Return response                    │
│                                          │
│   Occupied? ──▶ I am a WAITER            │
│     • Clone the Arc<InflightEntry>       │
│     • Drop the DashMap ref (no lock)     │
│     • entry.notify.notified().await      │
│     • Check entry.result                 │
│     • Read from result_storage           │
│     • Return response                    │
└──────────────────────────────────────────┘
```

### Cancellation Safety (Weak Arc pattern from fasterthanli.me)

If the leader panics or is cancelled:
- The `Arc<InflightEntry>` held by the leader is dropped.
- But waiters also hold `Arc` clones, so the entry isn't freed yet.
- The `result` field remains `None` (never set to `Ok`).
- **Solution:** Use `tokio::sync::Notify` with `notify_waiters()` in a `Drop` guard, and have waiters check the `result`. If `result` is `None` after being notified (or after a timeout), the waiter **promotes itself to leader** and retries the work.

```rust
struct InflightGuard {
    entry: Arc<InflightEntry>,
    key: String,
    map: Arc<DashMap<String, Arc<InflightEntry>>>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Always notify waiters — even on panic/cancel
        // so they don't hang forever.
        self.entry.notify.notify_waiters();
        self.map.remove(&self.key);
    }
}
```

Waiters see a `None` result → one of them re-enters the `entry()` call and becomes the new leader. This is the async equivalent of the `Weak<Sender>` pattern from the blog post, adapted for our "storage as mailbox" approach.

---

## Scope: Buffered Endpoint Only

Coalescing applies **only to `streamingpath_handler`** (`/{hash}/{key}/{params...}`). The streaming endpoint (`stream_handler`) is out of scope for now.

**Why not the streaming endpoint?**
- The streaming handler returns `Transfer-Encoding: chunked` — there's no single buffer to share. Coalescing would require either fan-out via `broadcast` (complex, slow-receiver issues) or forcing waiters into a buffered response (changes the endpoint's contract).
- The streaming endpoint already has lower traffic in practice (it's experimental).
- If coalescing is needed there later, the `InflightTracker` can be reused — waiters would simply read the completed result from `result_storage` as a full-body response.

---

## Failure Cache (Circuit Breaker)

### Problem

If the leader fails because the source is corrupt, unreachable, or produces an FFmpeg error, waiters inherit the error. On leader cancellation, a waiter promotes to leader and retries — hitting the same failure. With N waiters, this creates N sequential failures for the same broken input.

### Solution: Negative result caching

The `InflightTracker` maintains a second map of recently-failed keys:

```rust
pub struct InflightTracker {
    inflight: DashMap<String, Arc<InflightEntry>>,
    failed: DashMap<String, FailedEntry>,
}

struct FailedEntry {
    error: String,
    failed_at: Instant,
}

const FAILURE_TTL: Duration = Duration::from_secs(30);
```

**Flow with failure cache:**

1. Before checking `inflight`, check `failed`. If the key is present and within TTL → return the cached error immediately. No work attempted.
2. If the leader fails, it writes to `failed` with the error message and current timestamp.
3. Waiters that were already waiting get the error via `InflightEntry::result`. New arrivals hit the `failed` cache.
4. After TTL expires, the entry is lazily evicted on next access. The next request retries normally.

**Why 30s TTL?**
- Long enough to absorb a burst of identical requests for a broken resource.
- Short enough that transient failures (network blip, upstream 503) self-heal without operator intervention.
- Configurable via `ProcessorSettings` if needed.

**Cleanup:** A background `tokio::spawn` task runs every 60s and sweeps expired entries from `failed`, or entries are lazily evicted on read. The lazy approach is simpler and sufficient given the map is small (only failed keys).

---

## Where to Integrate

### Placement: Inside the handler, not the middleware

The `cache_middleware` already handles cache hits. Coalescing should happen **after** the cache miss and result-storage miss, right before the "fetch + process" work begins.

Integration point: **`streamingpath_handler`** (line ~47, after result storage miss).

### Integration sketch (buffered handler)

```rust
pub async fn streamingpath_handler(
    State(state): State<AppStateDyn>,
    cache_miss: Option<Extension<CacheMissContext>>,
    params: Params,
) -> Result<impl IntoResponse, AppError> {
    let params_hash = suffix_result_storage_hasher(&params);

    // 1. Check result storage (unchanged)
    if let Ok(blob) = state.result_storage.get(&params_hash).await { ... }

    // 2. NEW: coalescing check
    match state.inflight.try_join(&params_hash).await {
        CoalesceResult::Leader(guard) => {
            // I do the work (fetch, process, store)
            match do_processing(&state, &params).await {
                Ok(processed) => {
                    // IMPORTANT: write to result_storage BEFORE notifying waiters,
                    // so waiters' get() never races with the put().
                    store_result(&state, &params_hash, &processed).await;
                    guard.complete(Ok(()));  // notifies waiters, removes entry
                    return build_response(processed);
                }
                Err(e) => {
                    guard.fail(e.to_string());  // writes to failed cache, notifies waiters
                    return Err(e);
                }
            }
        }
        CoalesceResult::Waiter(result) => {
            // Leader finished; result is now in result_storage
            result?;  // propagate leader errors
            let blob = state.result_storage.get(&params_hash).await?;
            return build_response(blob);
        }
        CoalesceResult::Failed(err) => {
            // Recent failure for this key — fast-fail without doing any work
            return Err(e500(eyre!("request failed (cached): {}", err)));
        }
    }
}
```

---

## Data Structure Details

### New dependency

```toml
dashmap = "6"  # already a transitive dep via tower_governor
```

No new dependencies required beyond what's already in the lockfile.

### `InflightTracker` API

```rust
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const FAILURE_TTL: Duration = Duration::from_secs(30);

pub struct InflightTracker {
    inflight: Arc<DashMap<String, Arc<InflightEntry>>>,
    failed: Arc<DashMap<String, FailedEntry>>,
}

struct InflightEntry {
    notify: Notify,
    result: std::sync::Mutex<Option<Result<(), CoalesceError>>>,
}

struct FailedEntry {
    error: String,
    failed_at: Instant,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("coalesced request failed: {inner}")]
pub struct CoalesceError {
    inner: String,
}

pub enum CoalesceResult {
    /// You are the leader — do the work, then call guard.complete()
    Leader(InflightGuard),
    /// Another request did the work — result is in storage
    Waiter(Result<(), CoalesceError>),
    /// A recent leader failed for this key — fast-fail with cached error
    Failed(CoalesceError),
}
```

### Where to put it

New file: `src/inflight.rs`, added to `AppStateDyn`:

```rust
pub struct AppStateDyn {
    // ... existing fields ...
    pub inflight: InflightTracker,
}
```

---

## Metrics & Observability

Add counters to track coalescing effectiveness:

```rust
metrics::counter!("request.coalesced.leader").increment(1);
metrics::counter!("request.coalesced.waiter").increment(1);
metrics::counter!("request.coalesced.waiter_retry").increment(1);  // leader died, waiter promoted
```

These will show up in the existing Prometheus exporter with zero additional setup.

---

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| Leader panics in `spawn_blocking` (FFmpeg crash) | `InflightGuard::drop` fires → notifies waiters with `None` result → one waiter retries. If retry also fails, error is written to `failed` cache and subsequent requests fast-fail for 30s. |
| Leader's client disconnects (Axum drops the future) | Same as panic — guard is dropped, waiters retry. This is a cancellation, not a processing failure, so nothing is written to `failed`. |
| All waiters disconnect before leader finishes | Leader continues (it doesn't know about waiters). Result is stored normally. Entry cleaned up on completion. No wasted work since the result is cached for future requests. |
| Corrupt source / permanent FFmpeg error | Leader fails → error written to `failed` with 30s TTL. Waiters get error immediately. New requests within 30s fast-fail via `CoalesceResult::Failed`. After TTL, next request retries normally. |
| Leader succeeds but result_storage write fails | Leader sets `result = Err(...)` → waiters get the error. Written to `failed` cache since the processing output was lost. |
| Map entry leak (leader neither completes nor drops) | Not possible — Rust's ownership guarantees `InflightGuard::drop` always runs, even on panic. Tokio task cancellation also drops the future. |

---

## Estimated Impact

- **CPU:** Under thundering-herd conditions (N identical requests), reduces FFmpeg invocations from N to 1. With `num_cpus=4` and 100 concurrent identical requests, this eliminates 99 queued FFmpeg jobs.
- **Memory:** Eliminates N−1 redundant source audio fetches (each can be multi-MB).
- **Latency for waiters:** Adds ~0-200ms (time to check result storage after notification) vs. waiting in the semaphore queue anyway. Net improvement since the semaphore queue drains faster.
- **Complexity:** ~150 lines of new code (`src/inflight.rs`) + ~20 lines per handler.

---

## Implementation Plan

1. Add `dashmap` as a direct dependency
2. Create `src/inflight.rs` with `InflightTracker`, `InflightEntry`, `InflightGuard`, `FailedEntry`, `CoalesceResult`
3. Add `inflight: InflightTracker` to `AppStateDyn` and initialize in `startup.rs`
4. Integrate into `streamingpath_handler` (buffered path only)
5. Add metrics counters
6. Add integration test: spawn N concurrent requests for the same params, assert only 1 FFmpeg invocation
7. Add integration test: verify failure cache prevents retry storms (corrupt source → fast-fail for TTL)
8. Load test with `oha` to validate under realistic conditions
