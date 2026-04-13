# Performance Audit: Audio Streaming Engine

*Last updated: 2026-04-13*

## Architecture Overview

### Cache hit (no processing)
```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → cache_middleware — Redis/FS hit → 206/200 with Content-Length + Accept-Ranges
```

### Cache miss — first request (buffered pipeline via `/{*streamingpath}`)
```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → cache_middleware (miss: injects CacheMissContext extension)
      → auth_middleware (HMAC-SHA256 verify)
        → streamingpath_handler
            result_storage.get(params_hash) hit? → Body::from(bytes) with Content-Length
            ↓ miss
          InflightTracker.try_join(params_hash)
            ↓ Leader                              ↓ Waiter
          Storage.get OR fetch_audio_buffer     awaits Notify → reads result_storage
          ↓
          Processor.process
            Semaphore.acquire → spawn_blocking → FFmpeg pipeline
            → PipelineContext (decoder + encoder + filter graph)
            → OutputContext (in-memory WriteBuffer → Vec<u8>)
            → AudioBuffer
          ↓
          result_storage.put (inline, before notifying waiters)
          guard.complete() → notify_waiters()
          ↓
          handler calls ctx.populate(body) → cache.set (bg tokio::spawn)
          ↓
    Body::from(bytes) with Content-Length
```

### Cache miss — first request (streaming pipeline via `/stream/{*streamingpath}`)
```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → auth_middleware (HMAC-SHA256 verify)
      → stream_handler
          result_storage.get(params_hash) hit? → Body::from(bytes) with Content-Length
          ↓ miss
        is_passthrough_for_format?
          ↓ yes (zero-buffer path)            ↓ no
        storage.get_stream()                Storage.get OR fetch_audio_buffer
        ↓                                   ↓
        tee task: fan-out to HTTP +         Processor.process_streaming
          storage collector                   Semaphore.acquire_owned → spawn_blocking
        ↓                                     → PipelineContext → StreamingOutputContext
        Body::from_stream (chunked)             → SyncSender<Bytes> → bridge → Stream
                                              ↓
                                            tee task: fan-out to HTTP + storage collector
                                              ↓                         ↓
                                        Body::from_stream         tokio::spawn: collect → storage.put
                                          (chunked)
```

### Thumbnail pipeline via `/thumbnail/{*streamingpath}`
```
HTTP Request
  → ... → thumbnail_handler
      result_storage.get(thumb_hash) hit? → response
      ↓ miss
    Storage.get OR fetch_audio_buffer (source)
    ↓
    spawn_blocking → ffmpeg::decode_to_pcm (CPU-heavy)
    ↓
    spawn_blocking → thumbnail::analyze (CPU-heavy: chroma → SSM → fitness)
    ↓
    Processor.process (with computed start_time + duration)
    ↓
    result_storage.put (bg) + cache.populate (bg)
    ↓
    Response with X-Thumbnail-* headers + Link: rel=canonical
```

### Storage Backends
- **FileStorage** — local filesystem, native streaming via `ReaderStream`
- **S3Storage** — AWS S3 / MinIO, streaming via `ByteStream::into_async_read`
- **GCloudStorage** — Google Cloud Storage
- **CachedStorage** — wraps S3/GCS with a local disk LRU cache and streaming tee-on-miss

### Cache Backends
- **RedisCache** — Redis with TTL via `MultiplexedConnection`
- **FileSystemCache** — local filesystem with `.meta` expiry files

---

## What It Does Well ✅

### 1. FFmpeg via Native Bindings (not CLI subprocesses)
The core processing pipeline (`crates/ffmpeg`) uses direct FFI into libavcodec/libavformat/libavfilter — zero fork/exec overhead, in-memory I/O via custom `AVIOContext` callbacks, no temp files on the hot path. This is the single best architectural decision in the project.

### 2. Concurrency-Limited Processing via Semaphore
`Processor` gates FFmpeg work through a `tokio::sync::Semaphore` sized to `num_cpus`. This prevents oversubscription — critical since FFmpeg decode/encode is CPU-bound. `spawn_blocking` is correctly used so the tokio runtime isn't starved. The semaphore permit is held as an `OwnedSemaphorePermit` for the full lifetime of the FFmpeg `spawn_blocking` task, including the streaming case.

### 3. Passthrough Short-Circuit
`is_passthrough_request()` returns `input.clone()` (which is cheap — it's a `Bytes` refcount bump) when no processing is needed. Avoids the entire FFmpeg pipeline for identity requests. The streaming endpoint extends this to a zero-buffer file streaming passthrough via `storage.get_stream()`, avoiding any in-memory buffering of the source file.

### 4. Multi-Tier Caching
- **Result storage** (`storage.put(params_hash)`) — the primary cache. Checked first in `streamingpath_handler` and `stream_handler`; on hit, serves with `Content-Length`. Written inline (buffered path) or in a background collector task (streaming path).
- **Source cache** (`CachedStorage`) — avoids re-downloading from S3/GCS on repeated fetches of the same source file. Includes streaming tee-on-miss for the `get_stream` path.
- **Response cache** (Redis/filesystem) in `cache_middleware` — checked on every buffered request; hit path returns with range support. Populated by the handler via `CacheMissContext` — no body buffering in the middleware.

### 5. Request Coalescing (Thundering Herd Prevention)
`InflightTracker` uses `DashMap::entry` for lock-free leader election. When N concurrent requests arrive for the same uncached params, exactly one becomes the leader (does the work), and the rest become waiters that sleep on a `Notify`. On completion, the leader writes to `result_storage` *before* calling `guard.complete()`, so waiters can immediately read the result. Failed requests are cached with a 30-second TTL via `FailedEntry` to fast-fail subsequent attempts. The `Drop` impl on `InflightGuard` ensures waiters are always notified, even on panic or cancellation — preventing hangs.

### 6. Zero-Copy Response Cache Population
The old architecture buffered the entire HTTP response body in `cache_middleware` via `axum::body::to_bytes(response, usize::MAX)`. This has been replaced with `CacheMissContext` — a lightweight extension injected into the request on a cache miss. The downstream handler calls `ctx.populate(body.clone())` directly, which spawns a background `tokio::spawn` to write to cache. This eliminates the O(N) body buffering in middleware and lets the response flow to the client immediately.

### 7. Lean Docker Image
Multi-stage build with `cargo-chef` for layer caching, static FFmpeg compiled with only required filters/codecs (23 explicit `--enable-filter` flags, everything else disabled). Runtime image is minimal `bookworm-slim` with only `openssl`, `ca-certificates`, and `curl`.

### 8. Robust Observability & Structured Logging
The project uses `tracing` with Bunyan formatter for structured JSON logs and `#[instrument]` spans across the hot path. Prometheus metrics recorder exposes `http_requests_total` and `http_requests_duration_seconds` histogram with exponential buckets. `track_metrics` middleware captures method/path/status labels. Coalescing metrics (`request.coalesced.leader`, `request.coalesced.waiter`, `request.coalesced.failed_cache_hit`, `request.coalesced.waiter_retry`) provide visibility into deduplication behavior.

### 9. Zero-Copy Data Passing
By leveraging the `bytes` crate, the engine passes audio data through the request lifecycle (fetch → process → respond) using atomic reference counting. Clones of `AudioBuffer` (and the underlying `Bytes` struct) are O(1) pointer bumps rather than O(N) memory copies.

### 10. Clean Storage & Provider Abstractions
The `AudioStorage` trait with `get_stream` default method allows the engine to swap between Local Filesystem, S3, and GCS with zero changes to the core routing logic, and enables progressive adoption of streaming I/O.

### 11. Streaming FFmpeg Output Pipeline
On first-request cache misses via `/stream/`, encoded audio streams directly from FFmpeg to the HTTP response via a channel-backed `AVIOContext`. Key properties:
- `StreamingOutputContext` sends each 32 KB AVIO buffer as a `Bytes` chunk through `SyncSender` with built-in backpressure.
- `process_audio_streaming` bridges sync FFmpeg to async Tokio via `std::mpsc` → `tokio::mpsc`.
- The tee task fans each chunk to both the HTTP body and a storage collector.
- Peak memory per request drops from O(file size × 3) to O(chunk buffer), roughly 8 × 32 KB = 256 KB in flight regardless of file size.

### 12. Streaming Passthrough with Direct File I/O
The `/stream/` endpoint detects passthrough cases by format extension and uses `storage.get_stream()` to pipe file chunks directly to the HTTP response without ever loading the entire file into memory. `FileStorage` uses `tokio_util::io::ReaderStream` and S3Storage uses `ByteStream::into_async_read()`, both zero-buffer streaming paths. The passthrough tee also populates result storage in the background.

### 13. Correct `spawn_blocking` Placement for CPU-Intensive Work
All CPU-heavy operations — FFmpeg processing, PCM decode for thumbnails, thumbnail analysis, metadata extraction — are correctly dispatched via `tokio::task::spawn_blocking`, keeping the async executor unblocked.

### 14. Allocation-Free Params Hashing
`hash_params` uses a `Sha1Writer` adapter that implements `fmt::Write` to feed `Params::write_hash_input` directly into the SHA-1 state machine. This avoids allocating an intermediate `String` representation of the params — the hash is computed in a single streaming pass. Additionally, `write_hash_input` uses `itoa` and `ryu` for zero-allocation integer/float formatting.

### 15. Output Buffer Pre-Allocation
`OutputContext::open_with_capacity` pre-allocates the `WriteBuffer`'s backing `Vec<u8>` with a size hint of `input_size / 2`, avoiding repeated reallocations during encoding. This is a small but meaningful optimization for large files.

### 16. DiskEvictor with Running Size Counter
The `DiskEvictor` maintains an `AtomicU64` running byte counter updated via `track_write` / `track_delete`. This avoids scanning the directory on every cache write to check whether eviction is needed. Full directory scans only happen at startup and during actual eviction passes. The background task sleeps on a `Notify` and only wakes when the counter exceeds the limit — zero polling overhead.

### 17. Correct Waiter-Promotion in Coalescing
When a leader panics or is cancelled without setting a result, waiters do not hang — the `Drop` impl on `InflightGuard` calls `notify_waiters()`, and the waiter loop detects `None` in the result mutex and retries leader election. The `remove_if` with `Arc::ptr_eq` prevents a successor from clobbering a new entry for the same key.

---

## What It Does Poorly ❌

### 1. **MEDIUM: Thumbnail SSM is O(N²) Memory and Compute**
`build_ssm` allocates a full `num_frames × num_frames` matrix. For a 10-minute track at 2 frames/sec, that's 1200 frames → 1.44M f32 entries (~5.5 MB). For a 60-minute track: 7200 frames → 51.8M entries (~200 MB). The `find_best_segment` search then does O(N² × L) work over this matrix with only a coarse stride to limit iterations.

### 2. **MEDIUM: `meta_handler` Processes Audio Before Extracting Metadata**
`meta_handler` (meta.rs:46) calls `state.processor.process(&blob, &params)` — running the full FFmpeg encode pipeline — before extracting metadata. For a metadata-only request with no filter params, this re-encodes the audio unnecessarily. The passthrough short-circuit inside `process_audio` helps when no params are set, but if *any* param (even `format`) is present, the full pipeline runs just to extract metadata from the output.

### 3. **MEDIUM: Thumbnail Pipeline Has No Request Coalescing**
The `/thumbnail/` endpoint does not participate in the `InflightTracker` coalescing that protects the main `streamingpath_handler`. If N concurrent requests arrive for the same thumbnail, all N will independently decode PCM, run the SSM analysis, and process the audio. Only the result storage check at the top provides protection after the first request completes.

### 4. **LOW: Remote Audio Fetch Has No Streaming Support**
`fetch_audio_buffer` (remote.rs:33) calls `response.bytes().await`, which buffers the entire remote response into memory. For large remote files (up to the 256 MB limit), this is a significant memory spike. The streaming pipeline via `/stream/` currently falls back to this full-buffer fetch for remote URLs, negating the TTFB benefit.

### 5. **LOW: `AudioCache::get` Returns `Vec<u8>`, Not `Bytes`**
The `AudioCache` trait's `get` method returns `Option<Vec<u8>>` (cache/backend.rs:32). In `cache_middleware`, this `Vec<u8>` is immediately wrapped in `Bytes::from(buf)` (middleware.rs:76), which is zero-copy (takes ownership of the `Vec` heap). However, the `FileSystemCache::get` (fs.rs:75) reads the file into a `Vec<u8>` via `tokio_fs::read`, which is fine — but the `RedisCache::get` deserializes into `Vec<u8>`, then the middleware clones into `Bytes`. The real issue is that `AudioCache::set` takes `&[u8]`, forcing the caller to hold the data in memory for the duration of the write. Changing the trait to accept `Bytes` would let cache population hand off ownership and avoid pinning large buffers in the caller.

### 6. **LOW: `FileStorage::get` Reads Entire File into `Vec<u8>`**
`FileStorage::get` (file.rs:30-33) uses `file.read_to_end(&mut buffer)`, allocating a `Vec<u8>` that grows dynamically. It doesn't pre-size the buffer using file metadata (`File::metadata().await?.len()`), so a large file triggers multiple reallocations as the `Vec` grows. For a 50 MB file this could mean 3-4 reallocs with memcpy of the growing buffer.

### 7. **LOW: Streaming `/stream/` Endpoint Has No Request Coalescing**
Unlike the buffered `/{*streamingpath}` endpoint which uses `InflightTracker`, the streaming `/stream/` endpoint has no coalescing. Concurrent requests for the same uncached params each independently fetch the source and run the FFmpeg pipeline. The streaming nature makes coalescing harder (you can't share a stream between consumers), but the work duplication is still wasteful.

### 8. **LOW: `chroma::extract_chroma` Allocates a New `FftPlanner` Per Call**
In `thumbnail/chroma.rs:15`, each call to `extract_chroma` creates a fresh `FftPlanner::<f32>::new()`. The planner internally caches computed FFT plans, but that cache is per-instance. Since the thumbnail pipeline creates a new planner for every request, the planner cache provides no benefit. For a 4096-point FFT this is a minor cost (microseconds), but it's a missed optimization.

### 9. **LOW: `sniff_content_type` Is Duplicated Across Handlers**
The `sniff_content_type` function is copy-pasted in `routes/streamingpath.rs` and `routes/thumbnail.rs`. While not a runtime performance issue, it indicates the absence of a centralized `ContentType` detection path that could be optimized once (e.g., cached per params hash).

### 10. **INFO: No Connection Pooling Configuration for Redis**
`RedisCache::new` creates a single `MultiplexedConnection`. The `redis` crate's `MultiplexedConnection` is a single TCP connection multiplexed across tasks, which is efficient. However, there's no reconnection logic or connection pool — if the Redis connection drops, all cache operations fail until the server is restarted. This isn't a throughput issue (multiplexed connections handle high concurrency well) but affects resilience.

### 11. **INFO: `Params` Is Parsed Twice on Buffered Requests**
On the buffered path (`/{*streamingpath}`), `Params` is extracted via `FromRequestParts` in both `cache_middleware` and `streamingpath_handler`. The Axum extractor parses the URI and query params each time. Since `Params` implements `Clone`, the first parse could be stored in request extensions and reused. This is ~microseconds per request and unlikely to matter, but it's an unnecessary repeated parse.

---

## Recommended Improvements (Prioritized)

| Priority | Fix | Effort | Impact |
|----------|-----|--------|--------|
| **P1** | **Add coalescing to thumbnail endpoint** — reuse `InflightTracker` (with the `_thumb` suffixed hash) so concurrent identical thumbnail requests share a single PCM decode + analysis + process pipeline | Small | Eliminates redundant CPU-heavy work for popular tracks |
| **P2** | **Optimize `meta_handler`** — skip `processor.process()` when params contain no filters/transforms and extract metadata directly from the source blob | Small | Avoids a full FFmpeg encode for metadata-only requests |
| **P3** | **Cap SSM size for thumbnails** — downsample chroma frames or use a band-diagonal SSM when `num_frames > threshold` (e.g., 2000) to bound memory at O(N×W) instead of O(N²) | Medium | Prevents OOM on long tracks |
| **P4** | **Pre-size `FileStorage::get` buffer** — call `file.metadata().await?.len()` and `Vec::with_capacity` to avoid reallocs on large files | Small | Eliminates 3-4 memcpys per large file read |
| **P5** | **Stream remote audio fetches** — use `response.bytes_stream()` to pipe remote content into the streaming pipeline instead of buffering the entire response in memory | Medium | Reduces memory spike for large remote files |
| **P6** | **Change `AudioCache` trait to use `Bytes`** — `get` returns `Option<Bytes>`, `set` takes `Bytes` — avoids unnecessary `Vec<u8>` ↔ `Bytes` conversions and lets cache population transfer ownership | Small | Cleaner data flow, avoids pinning buffers |
| **P7** | **Cache `Params` in request extensions** — parse once in `cache_middleware`, store in extensions, reuse in handler | Small | Eliminates redundant URI parsing on buffered path |
| **P8** | **Streaming-safe WAV/M4A output** — use fragmented MP4 (`-movflags frag_keyframe+empty_moov`) and WAV header post-patch to extend streaming to all output formats | Medium | Removes format restriction on TTFB improvement |

---

## Appendix: Streaming Pipeline Limitations

Documented limitations of the current `/stream/` streaming implementation:

1. **Format restrictions** — Output formats that seek during writing (`wav`, `m4a`) are not streaming-safe. The `channel_write_seek_callback` returns `-1` for all seeks, which causes FFmpeg to skip the seek silently for most muxers, but WAV and MP4 will produce malformed output. Restrict streaming to `mp3`, `ogg`, and `opus` formats.

2. **Non-streaming filters** — `areverse` and two-pass `loudnorm` require the full buffer and are fundamentally non-streaming. They work correctly, but TTFB will match the buffered path for those filter combinations.

3. **No `Content-Length` on first request** — Streaming responses use `Transfer-Encoding: chunked` without `Content-Length`. Range requests (`Accept-Ranges`) are only supported on the result-storage hit path (second and subsequent requests).

4. **Truncated error responses** — Mid-stream FFmpeg errors cause a truncated response rather than a well-formed HTTP error. The bridge detects channel closure but cannot inject an error frame into an already-started chunked body.

5. **No streaming for remote sources** — Remote URLs are always fully buffered via `fetch_audio_buffer` before processing, even on the `/stream/` endpoint. The streaming TTFB benefit only applies to local/S3/GCS storage sources.

## Appendix: Memory Profile by Request Type

| Request Type | Peak Memory per Request | Notes |
|---|---|---|
| Cache hit (Redis) | ~size of cached response | Redis returns `Vec<u8>`, converted to `Bytes` |
| Cache hit (FS) | ~size of cached file | File read into `Vec<u8>` |
| Buffered miss (small file) | ~3× file size | source `Bytes` + FFmpeg working set + output `Vec<u8>` |
| Buffered miss (large file) | ~3× file size | Same, but problematic at 100 MB+ |
| Streaming miss (local) | ~256 KB + source `Bytes` | Channel backpressure limits in-flight chunks |
| Streaming passthrough | ~256 KB | Never loads full file; `ReaderStream` chunks |
| Thumbnail (10 min track) | ~source + 5.5 MB SSM + PCM | SSM dominates for long tracks |
| Thumbnail (60 min track) | ~source + 200 MB SSM + PCM | **OOM risk** |
| Remote fetch | ~256 MB max | `response.bytes()` buffers entire body |
