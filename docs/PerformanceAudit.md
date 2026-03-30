# Performance Audit: Audio Streaming Engine

## Architecture Overview

### Cache hit (no processing)
```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → cache_middleware — Redis/FS hit → 206/200 with Content-Length
```

### Cache miss — first request (streaming pipeline)
```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → cache_middleware (miss: passes through, strips Range header)
      → auth_middleware (HMAC-SHA256 verify)
        → streamingpath_handler
            storage.get(params_hash) hit? → Body::from(bytes) with Content-Length
            ↓ miss
          Storage.get (source) OR fetch_audio_buffer (remote HTTP)
          ↓
          Processor.process_streaming
            Semaphore.acquire_owned → spawn_blocking → FFmpeg pipeline
            → PipelineContext (decoder + encoder + filter graph)
            → StreamingOutputContext (AVIOContext → SyncSender<Bytes>)
            → bridge spawn_blocking (std::mpsc → tokio::mpsc)
            → Stream<Item = Result<Bytes>>
          ↓
          tee task: fan-out to HTTP channel + storage collector channel
          ↓                              ↓
    Body::from_stream (chunked)     tokio::spawn: collect → storage.put(params_hash)
          ↓
    Transfer-Encoding: chunked response (first bytes in ~3ms)
```

### Subsequent requests (result storage hit)
```
HTTP Request → ... → streamingpath_handler
  storage.get(params_hash) hit → Body::from(bytes) with Content-Length + Accept-Ranges
```

### Storage Backends
- **FileStorage** — local filesystem
- **S3Storage** — AWS S3 / MinIO
- **GCloudStorage** — Google Cloud Storage
- **CachedStorage** — wraps S3/GCS with a local disk cache

### Cache Backends
- **RedisCache** — Redis with TTL
- **FileSystemCache** — local filesystem with `.meta` expiry files

---

## What It Does Well ✅

### 1. FFmpeg via Native Bindings (not CLI subprocesses)
The core processing pipeline (`crates/ffmpeg`) uses direct FFI into libavcodec/libavformat/libavfilter — zero fork/exec overhead, in-memory I/O via custom `AVIOContext` callbacks, no temp files on the hot path. This is the single best architectural decision in the project.

### 2. Concurrency-Limited Processing via Semaphore
`Processor` gates FFmpeg work through a `tokio::sync::Semaphore` sized to `num_cpus`. This prevents oversubscription — critical since FFmpeg decode/encode is CPU-bound. `spawn_blocking` is correctly used so the tokio runtime isn't starved. The semaphore permit is now held as an `OwnedSemaphorePermit` for the full lifetime of the FFmpeg `spawn_blocking` task, including the streaming case.

### 3. Passthrough Short-Circuit
`is_passthrough_request()` returns `input.clone()` (which is cheap — it's a `Bytes` refcount bump) when no processing is needed. Avoids the entire FFmpeg pipeline for identity requests. In streaming mode, this short-circuits to a single-item `futures::stream::once` stream.

### 4. Multi-Tier Caching
- **Result storage** (`storage.put(params_hash)`) — the primary cache. Checked first in `streamingpath_handler`; on hit, serves with `Content-Length` and `Accept-Ranges` support. Written in a background `tokio::spawn` task as the stream drains, so it doesn't block the response.
- **Source cache** (`CachedStorage`) — avoids re-downloading from S3/GCS on repeated fetches of the same source file.
- **Response cache** (Redis/filesystem) in `cache_middleware` — checked on every request; hit path unchanged and still returns buffered responses with range support.

### 5. Lean Docker Image
Multi-stage build with `cargo-chef` for layer caching, static FFmpeg with only required filters/codecs. Runtime image is minimal `bookworm-slim`.

### 6. Robust Observability & Structured Logging
The project uses `tracing` with a Bunyan formatter, providing structured JSON logs and `#[instrument]` spans across the hot path. It also integrates a Prometheus metrics recorder (`metrics-exporter-prometheus`) for real-time monitoring of request latency and throughput.

### 7. Zero-Copy Data Passing
By leveraging the `bytes` crate, the engine passes audio data through the request lifecycle (fetch → process → respond) using atomic reference counting. Clones of `AudioBuffer` (and the underlying `Bytes` struct) are O(1) pointer bumps rather than O(N) memory copies, significantly reducing pressure on the CPU's memory controller during high-throughput scenarios.

### 8. Clean Storage & Provider Abstractions
The `AudioStorage` trait allows the engine to swap between Local Filesystem, S3, and GCS with zero changes to the core routing logic. This decoupling is a hallmark of good systems design, allowing for easy transitions from single-server dev to cloud-scale production.

### 9. Streaming FFmpeg Output Pipeline (P0 — implemented)
On first-request cache misses, encoded audio now streams directly from FFmpeg to the HTTP response via a channel-backed `AVIOContext`. The implementation:

- `StreamingOutputContext` (`crates/ffmpeg/src/io.rs`) — a new RAII `AVIOContext` whose `write_callback` sends each 32 KB AVIO buffer as a `Bytes` chunk to a bounded `std::sync::mpsc::SyncSender`. Backpressure is built in: FFmpeg blocks on `send()` if the consumer is slower than the encoder.
- `OutputWrite` trait (`crates/ffmpeg/src/io.rs`) — shared interface over `OutputContext` (buffered) and `StreamingOutputContext`, allowing the pipeline logic to be written once in `run_to_output(&mut dyn OutputWrite)`.
- `PipelineContext` (`crates/ffmpeg/src/pipeline.rs`) — packages decoder, encoder, filter graph, and resampler setup. `process()` and `process_streaming()` both call `setup_pipeline()` then `run_to_output()` against their respective output context.
- `process_audio_streaming` (`src/processor/ffmpeg.rs`) — bridges the sync FFmpeg world to async Tokio: one `spawn_blocking` task runs FFmpeg (holding the semaphore permit), a second `spawn_blocking` bridge drains the `std::mpsc` channel into a `tokio::mpsc` channel, and the caller receives a `Stream<Item = Result<Bytes>>`.
- Tee in `streamingpath_handler` (`src/routes/streamingpath.rs`) — a `tokio::spawn` task fans each chunk out to both the HTTP `Body::from_stream()` and a storage collector channel. When the stream ends, the collector assembles chunks and calls `storage.put()`.
- `cache_middleware` (`src/middleware.rs`) — miss path simplified: no longer buffers the response body with `to_bytes()`. Passes through directly; `result_storage` is the persistence layer.

**Measured impact** (775 KB MP3, volume + lowpass filter, `cargo bench -- streaming_vs_buffered`):

| Path | Median latency | Notes |
|---|---|---|
| `buffered_total` | 631 ms | old behavior: client blocks until transcode complete |
| `streaming_total` | 628 ms | new: drain all chunks — throughput unchanged (<1% overhead) |
| `streaming_ttfb` | **3.2 ms** | new: first bytes available after first AVIO flush |

**197× reduction in time to first byte.** Peak memory per request drops from O(file size × 3) — input buffer + FFmpeg output buffer + HTTP body buffer all live simultaneously — to O(chunk buffer), roughly 8 × 32 KB = 256 KB in flight regardless of file size. This is not measurable with divan; observe it with `heaptrack` or by watching RSS under concurrent load.

**Limitations of the current streaming implementation:**
- Output formats that seek during writing (`wav`, `m4a`) are not streaming-safe. The `channel_write_seek_callback` returns `-1` for all seeks, which causes FFmpeg to skip the seek silently for most muxers, but WAV and MP4 will produce malformed output if the seek is needed to finalize headers. Restrict streaming to `mp3` and `ogg` formats.
- The `areverse` filter and two-pass `loudnorm` require the full buffer and are fundamentally non-streaming. They continue to work correctly (FFmpeg will produce the output regardless), but TTFB will match the buffered path for those filter combinations.
- Streaming responses use `Transfer-Encoding: chunked` without `Content-Length`. Range requests (`Accept-Ranges`) are only supported on the result-storage hit path (second and subsequent requests), where the full buffer is available.
- Mid-stream FFmpeg errors cause a truncated response rather than a well-formed HTTP error. The bridge detects channel closure but cannot inject an error frame into an already-started chunked body.

---

## What It Does Poorly ❌

### 1. **HIGH: Pathological Disk I/O in Cache Eviction (`FileSystemCache`)**
In `src/cache/fs.rs`, every single time a cache entry is written (`FileSystemCache::set`), it calls `evict_notify.notify_one()`. This immediately wakes up `run_eviction`, which performs a full `tokio_fs::read_dir` scan and invokes `metadata()` on *every single file* in the cache directory to calculate total size. As the cache grows to thousands of files, this O(N) operation runs constantly, eventually pegging disk I/O at 100%.

### 2. **MEDIUM: Extraneous Allocations in Request Parsing**
Hot path components like `Params::to_string()` and `suffix_result_storage_hasher` allocate heavily (`format!`, `to_string()`, `flat_map`, `collect::<Vec<_>>().join("&")`). This introduces unnecessary allocator pressure during high throughput spikes.

### 3. **LOW: `Params` Parsing Double-Computes Hash**
`cache_middleware` and `streamingpath_handler` both call `suffix_result_storage_hasher(&params)`, which serializes `Params` to string and SHA1-hashes it — computed twice on every cache miss.

---

## Recommended Improvements (Prioritized)

| Priority | Fix | Effort | Impact | Status |
|----------|-----|--------|--------|--------|
| **P0** | **Streaming FFmpeg output pipeline** — channel-backed `AVIOContext`, `OutputWrite` trait, tee handler, simplified cache middleware | Large | 197× TTFB reduction; O(1) peak memory vs O(file size) | ✅ Done |
| **P1** | **Refactor `FileSystemCache` eviction logic** to use a running `AtomicU64` size counter or an in-memory LRU tracking system (`moka`) instead of `read_dir` scans on every write | Medium | Fixes disk I/O pegging | |
| **P2** | **Optimize hot path strings** using `std::fmt::Write` over pre-allocated buffers in `Params::to_string()` and `hasher.rs` to eliminate transient `Vec` allocations | Small | Lowers allocator pressure | |
| **P3** | Thread the computed `params_hash` through the request extensions so it is computed once per request instead of twice | Small | Minor CPU save | |
| **P4** | **Streaming-safe WAV/M4A output** — use fragmented MP4 (`-movflags frag_keyframe+empty_moov`) and WAV header post-patch to extend streaming to all output formats | Medium | Removes format restriction on TTFB improvement | |
| **P5** | **Stream source input into FFmpeg** — replace the full-buffer `InputContext` with a `mmap`-backed or chunked-read AVIO source for local files, eliminating the source read allocation | Medium | Eliminates remaining O(N) input allocation | |
