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
    → cache_middleware (miss: injects CacheMissContext + parsed Params into extensions)
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

### 18. Params Cached in Request Extensions
`cache_middleware` parses `Params` once and inserts it into request extensions (`req.extensions_mut().insert(params)`). The `FromRequestParts` impl for `Params` checks `parts.extensions.remove::<Params>()` first, so downstream handlers (e.g., `streamingpath_handler`) reuse the already-parsed `Params` without re-parsing the URI. This eliminates redundant query string parsing on every buffered request.

### 19. Pre-Sized File Read Buffers
`FileStorage::get` calls `file.metadata().await?` to obtain the file size, then allocates `Vec::with_capacity(meta.len() as usize)` before reading. This eliminates the repeated `Vec` reallocations and memcpys that would otherwise occur for large files (a 50 MB file would trigger 3-4 reallocs without pre-sizing).

### 20. Cache Trait Uses `Bytes` Throughout
The `AudioCache` trait's `get` returns `Option<Bytes>` and `set` takes `Bytes`, enabling zero-copy ownership transfer from cache to response. This avoids unnecessary `Vec<u8>` ↔ `Bytes` conversions and means cache population can hand off the buffer without pinning it in the caller.

---

## What It Does Poorly ❌

### 1. **HIGH: Thumbnail SSM is O(N²) Memory and Compute**
`build_ssm` allocates a full `num_frames × num_frames` matrix. For a 10-minute track at 2 frames/sec, that's 1200 frames → 1.44M f32 entries (~5.5 MB). For a 60-minute track: 7200 frames → 51.8M entries (~200 MB). The `find_best_segment` search then does O(N² × L) work over this matrix with only a coarse stride to limit iterations. There is **no upper bound** on `num_frames` — a malicious or very long input (e.g., a 4-hour podcast) will allocate ~1.7 GB for the SSM alone and likely OOM the process or starve other requests.

### 2. **MEDIUM: `meta_handler` Processes Audio Before Extracting Metadata**
`meta_handler` (meta.rs:46) calls `state.processor.process(&blob, &params)` — running the full FFmpeg encode pipeline — before extracting metadata. For a metadata-only request with no filter params, this re-encodes the audio unnecessarily. The passthrough short-circuit inside `process_audio` helps when no params are set, but if *any* param (even `format`) is present, the full pipeline runs just to extract metadata from the output.

### 3. **MEDIUM: Thumbnail Pipeline Has No Request Coalescing**
The `/thumbnail/` endpoint does not participate in the `InflightTracker` coalescing that protects the main `streamingpath_handler`. If N concurrent requests arrive for the same thumbnail, all N will independently decode PCM, run the SSM analysis, and process the audio. Only the result storage check at the top provides protection after the first request completes.

### 4. **MEDIUM: `AudioProcessor` is Recreated Per Request**
In `processor/ffmpeg.rs:168-169`, every call to `process_audio` creates a new `ffmpeg::AudioProcessor::new()` inside the `spawn_blocking` closure. While `AudioProcessor::new()` is cheap today (just calls `crate::init()` which is a no-op after the first call), the comment in `pipeline.rs:98-99` explicitly notes the struct "allows future extension (e.g., thread pool, reusable contexts)." More importantly, the per-request allocation pattern means the decoder, encoder, and filter graph contexts are built from scratch for every request — even when consecutive requests use the same codec pair. Pooling `PipelineContext` or at minimum caching FFmpeg codec lookups (`find_encoder_by_name`, `find_decoder`) would eliminate repeated FFmpeg internal hash table walks.

### ~~5. **MEDIUM: `metadata::extract_metadata` Copies Input Data**~~ ✅ Already resolved
`extract_metadata` already accepts `Bytes` directly, and `meta_handler` calls `processed_blob.bytes()` which is a zero-copy `Arc` bump. No double-copy exists.

### ~~6. **MEDIUM: Streaming Bridge Uses Two `spawn_blocking` Tasks**~~ ✅ Resolved
The bridge task in `process_audio_streaming` has been changed from `spawn_blocking` to `tokio::spawn` with a `try_recv` + `yield_now()` polling loop. Under high concurrency, this halves the blocking thread demand (only the FFmpeg task uses `spawn_blocking`).

### 7. **LOW: Remote Audio Fetch Has No Streaming Support**
`fetch_audio_buffer` (remote.rs:33) calls `response.bytes().await`, which buffers the entire remote response into memory. For large remote files (up to the 256 MB limit), this is a significant memory spike. The streaming pipeline via `/stream/` currently falls back to this full-buffer fetch for remote URLs, negating the TTFB benefit.

### 8. **LOW: Streaming `/stream/` Endpoint Has No Request Coalescing**
Unlike the buffered `/{*streamingpath}` endpoint which uses `InflightTracker`, the streaming `/stream/` endpoint has no coalescing. Concurrent requests for the same uncached params each independently fetch the source and run the FFmpeg pipeline. The streaming nature makes coalescing harder (you can't share a stream between consumers), but the work duplication is still wasteful.

### 9. **LOW: `chroma::extract_chroma` Allocates a New `FftPlanner` Per Call**
In `thumbnail/chroma.rs:15`, each call to `extract_chroma` creates a fresh `FftPlanner::<f32>::new()`. The planner internally caches computed FFT plans, but that cache is per-instance. Since the thumbnail pipeline creates a new planner for every request, the planner cache provides no benefit. For a 4096-point FFT this is a minor cost (microseconds), but it's a missed optimization.

### 10. **LOW: `sniff_content_type` Is Duplicated Across Handlers**
The `sniff_content_type` function is copy-pasted in `routes/streamingpath.rs` and `routes/thumbnail.rs`. While not a runtime performance issue, it indicates the absence of a centralized `ContentType` detection path that could be optimized once (e.g., cached per params hash).

### 11. **LOW: Chroma Window Recomputed Per Call**
`chroma::hann_window(fft_size)` allocates a new `Vec<f32>` of 4096 floats on every `extract_chroma` call. Since `fft_size` is always 4096, this window could be computed once and reused (e.g., via a `LazyLock` or passed in from the caller).

### 12. **LOW: SSM Cosine Similarity Not SIMD-Optimized**
The `build_ssm` inner loop computes dot products over 12-element f32 vectors using scalar iterator chains. While 12 is a small dimension, the loop runs O(N²/2) times. Unrolling the 12-element dot product or using SIMD intrinsics (or simply letting the compiler auto-vectorize by ensuring the loop body is a simple accumulator over a fixed-size array) could provide a 2-4× speedup on the SSM construction.

### 13. **LOW: Tee Storage Collectors Reassemble in `BytesMut`**
Both the passthrough and FFmpeg streaming paths in `stream.rs` collect chunks in a `Vec<Bytes>` then manually reassemble via `BytesMut::extend_from_slice`. This performs a full copy of every chunk. Using `bytes::buf::BufMut` with a `BytesMut` directly, or accumulating into a single growing `BytesMut` from the start instead of a `Vec<Bytes>`, would avoid one full copy of the output.

### 14. **INFO: No Connection Pooling Configuration for Redis**
`RedisCache::new` creates a single `MultiplexedConnection`. The `redis` crate's `MultiplexedConnection` is a single TCP connection multiplexed across tasks, which is efficient. However, there's no reconnection logic or connection pool — if the Redis connection drops, all cache operations fail until the server is restarted. This isn't a throughput issue (multiplexed connections handle high concurrency well) but affects resilience.

### ~~15. **INFO: No Request Size Limits on Buffered Endpoint**~~ ✅ Resolved
A configurable `max_source_size` (default 100 MB) is now enforced in all handlers — buffered, streaming, metadata, and thumbnail — before processing begins. The same limit is used for remote fetches (replacing the hardcoded `MAX_REMOTE_BODY_SIZE`). Oversized requests receive 413 Payload Too Large. Peak memory for the processing pipeline is now bounded to `num_cpus × 3 × max_source_size`. The limit is set via `processor.max_source_size` in config or the `APP_PROCESSOR__MAX_SOURCE_SIZE` env var.

### 16. **INFO: `S3Storage::list` Does Not Paginate**
`S3Storage::list` calls `list_objects_v2` once without handling the `continuation_token`, so it returns at most 1000 keys. This is fine for the current `/list` web UI but would silently truncate results for larger buckets.

---

## Recommended Improvements (Prioritized)

| Priority | Fix | Effort | Impact |
|----------|-----|--------|--------|
| **P1** | **Cap SSM size for thumbnails** — downsample chroma frames when `num_frames > threshold` (e.g., 2000), or use a band-diagonal SSM of width W to bound memory at O(N×W) instead of O(N²). Add an explicit `num_frames` ceiling to reject or truncate absurdly long inputs. | Medium | Prevents OOM on long tracks; closes a denial-of-service vector |
| **P2** | **Add coalescing to thumbnail endpoint** — reuse `InflightTracker` (with the `_thumb` suffixed hash) so concurrent identical thumbnail requests share a single PCM decode + analysis + process pipeline | Small | Eliminates redundant CPU-heavy work for popular tracks |
| **P3** | **Optimize `meta_handler`** — skip `processor.process()` when params contain no filters/transforms and extract metadata directly from the source blob | Small | Avoids a full FFmpeg encode for metadata-only requests |
| ~~P4~~ | ~~Eliminate double-copy in metadata extraction~~ | ✅ | Already resolved — `extract_metadata` accepts `Bytes` directly |
| ~~P5~~ | ~~Reduce blocking thread pressure for streaming~~ | ✅ | Bridge task changed from `spawn_blocking` to `tokio::spawn` with `try_recv` + yield |
| **P6** | **Stream remote audio fetches** — use `response.bytes_stream()` to pipe remote content into the streaming pipeline instead of buffering the entire response in memory | Medium | Reduces memory spike for large remote files |
| **P7** | **Streaming-safe WAV/M4A output** — use fragmented MP4 (`-movflags frag_keyframe+empty_moov`) and WAV header post-patch to extend streaming to all output formats | Medium | Removes format restriction on TTFB improvement |
| ~~P8~~ | ~~Add per-request file-size limits~~ | ✅ | `max_source_size` config (default 100 MB) enforced in all handlers; returns 413 early |

### Previously Recommended — Now Resolved ✅

| Fix | Status |
|-----|--------|
| Pre-size `FileStorage::get` buffer via `file.metadata().len()` | ✅ Fixed: `file.rs:31-32` reads metadata and pre-allocates `Vec::with_capacity` |
| Change `AudioCache` trait to use `Bytes` instead of `Vec<u8>` | ✅ Fixed: `backend.rs:33-34` — `get` returns `Option<Bytes>`, `set` takes `Bytes` |
| Cache `Params` in request extensions to avoid double-parse | ✅ Fixed: `middleware.rs:87` inserts parsed `Params`; `params.rs:35-37` reuses from extensions |
| Eliminate double-copy in `extract_metadata` | ✅ Already resolved: `extract_metadata` accepts `Bytes` directly; `meta_handler` passes `processed_blob.bytes()` (Arc bump) |
| Reduce blocking thread pressure for streaming bridge | ✅ Fixed: bridge changed from `spawn_blocking` to `tokio::spawn` with `try_recv` + `yield_now()` |
| Add per-request file-size limits | ✅ Fixed: `max_source_size` config (default 100 MB) enforced in all handlers with 413 response; replaces hardcoded `MAX_REMOTE_BODY_SIZE` |

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
| Cache hit (FS) | ~size of cached file | File read into `Vec<u8>` via pre-sized buffer |
| Buffered miss (small file) | ~3× file size | source `Bytes` + FFmpeg working set + output `Vec<u8>` |
| Buffered miss (large file) | ~3× file size | Same, but problematic at 100 MB+; **no enforced limit** |
| Streaming miss (local) | ~256 KB + source `Bytes` | Channel backpressure limits in-flight chunks |
| Streaming passthrough | ~256 KB | Never loads full file; `ReaderStream` chunks |
| Thumbnail (10 min track) | ~source + 5.5 MB SSM + PCM | SSM dominates for long tracks |
| Thumbnail (60 min track) | ~source + 200 MB SSM + PCM | **OOM risk — no cap** |
| Remote fetch | ~256 MB max | `response.bytes()` buffers entire body |
| Streaming (2 `spawn_blocking`) | ~source `Bytes` + 2 blocking threads | Bridge thread + FFmpeg thread per request |

## Appendix: Blocking Thread Budget

Under the streaming `/stream/` endpoint, each concurrent request consumes **two** threads from Tokio's blocking pool: one for FFmpeg processing and one for the sync-to-async bridge. With the semaphore defaulting to `num_cpus` (e.g., 8), a fully loaded server needs 16 blocking threads just for streaming processing. If the blocking pool is at its default size (512), this isn't a problem, but if combined with many concurrent thumbnail requests (each using 2× `spawn_blocking` for PCM decode + analysis, plus 1× for FFmpeg processing), thread starvation becomes possible.

| Operation | `spawn_blocking` threads per request |
|---|---|
| Buffered processing | 1 (FFmpeg) |
| Streaming processing | 2 (FFmpeg + bridge) |
| Thumbnail | 3 (PCM decode + analysis + FFmpeg process) |
| Metadata extraction | 2 (FFmpeg process + metadata extraction) |
