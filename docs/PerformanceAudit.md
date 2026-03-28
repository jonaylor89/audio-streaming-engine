# Performance Audit: Audio Streaming Engine

## Architecture Overview

```
HTTP Request
  → CorsLayer → TraceLayer → track_metrics
    → cache_middleware (response cache: Redis / filesystem)
      → auth_middleware (Argon2 hash verify)
        → streamingpath_handler / meta_handler
          → Storage.get (source) OR fetch_audio_buffer (remote HTTP)
          → Processor (Semaphore → spawn_blocking → FFmpeg pipeline)
          → Storage.put (result hash)
          → Response
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
`Processor` gates FFmpeg work through a `tokio::sync::Semaphore` sized to `num_cpus`. This prevents oversubscription — critical since FFmpeg decode/encode is CPU-bound. `spawn_blocking` is correctly used so the tokio runtime isn't starved.

### 3. Passthrough Short-Circuit
`is_passthrough_request()` returns `input.clone()` (which is cheap — it's a `Bytes` refcount bump) when no processing is needed. Avoids the entire FFmpeg pipeline for identity requests.

### 4. Multi-Tier Caching
- **Response cache** (Redis/filesystem) in `cache_middleware` — avoids re-processing entirely on repeated requests.
- **Source cache** (`CachedStorage`) — avoids re-downloading from S3/GCS on repeated fetches of the same source file.
- **Result storage** (`storage.put(params_hash)`) — pre-computed results are stored and checked first in `streamingpath_handler`.

### 5. Lean Docker Image
Multi-stage build with `cargo-chef` for layer caching, static FFmpeg with only required filters/codecs. Runtime image is minimal `bookworm-slim`.

### 6. Robust Observability & Structured Logging
The project uses `tracing` with a Bunyan formatter, providing structured JSON logs and `#[instrument]` spans across the hot path. It also integrates a Prometheus metrics recorder (`metrics-exporter-prometheus`) for real-time monitoring of request latency and throughput.

### 7. Zero-Copy Data Passing
By leveraging the `bytes` crate, the engine passes audio data through the request lifecycle (fetch -> process -> respond) using atomic reference counting. Clones of `AudioBuffer` (and the underlying `Bytes` struct) are O(1) pointer bumps rather than O(N) memory copies, significantly reducing pressure on the CPU's memory controller during high-throughput scenarios.

### 8. Clean Storage & Provider Abstractions
The `AudioStorage` trait allows the engine to swap between Local Filesystem, S3, and GCS with zero changes to the core routing logic. This decoupling is a hallmark of good systems design, allowing for easy transitions from single-server dev to cloud-scale production.

---

## What It Does Poorly ❌

### 6. **CRITICAL: Catastrophic Memory Overhead (No Streaming Pipeline)**
`fetch_audio_buffer` downloads the entire audio payload into a single `Bytes` struct. `AudioProcessor::process` reads the full byte array and transcodes into *another* full `Vec<u8>` in memory. Finally, Axum serves the entire response at once (`Body::from(blob.into_bytes())`). This yields an $O(N)$ memory footprint with respect to file size. Processing a 1-hour podcast (e.g., 50MB MP3 -> 600MB WAV -> 50MB OGG) will consume nearly 1GB of RAM per request. Just a handful of concurrent requests will OOM the server.

### 8. **HIGH: Pathological Disk I/O in Cache Eviction (`FileSystemCache`)**
In `src/cache/fs.rs`, every single time a cache entry is written (`FileSystemCache::set`), it calls `evict_notify.notify_one()`. This immediately wakes up `run_eviction`, which performs a full `tokio_fs::read_dir` scan and invokes `metadata()` on *every single file* in the cache directory to calculate total size. As the cache grows to thousands of files, this $O(N)$ operation runs constantly, eventually pegging disk I/O at 100%.

### 9. **MEDIUM: Extraneous Allocations in Request Parsing**
Hot path components like `Params::to_string()` and `suffix_result_storage_hasher` allocate heavily (`format!`, `to_string()`, `flat_map`, `collect::<Vec<_>>().join("&")`). This introduces unnecessary GC pressure on the system allocator during high throughput spikes.

### 10. **LOW: `Params` Parsing Double-Computes Hash**
`cache_middleware` and `streamingpath_handler` both call `suffix_result_storage_hasher(&params)`, which serializes `Params` to string and SHA1-hashes it — computed twice on every cache miss.

---

## Recommended Improvements (Prioritized)

| Priority | Fix | Effort | Impact | Status |
|----------|-----|--------|--------|--------|
| **P0** | **Implement Chunked Streaming** for `fetch_audio_buffer` and Axum responses. Refactor the FFmpeg FFI to yield `AsyncRead`/`AsyncWrite` streams rather than operating on full `Vec<u8>` | Large | Eliminates OOM | |
| **P1** | **Refactor `FileSystemCache` eviction logic** to use a running `AtomicU64` size counter or an in-memory LRU tracking system (`moka`) instead of `read_dir` scans on every write | Medium | Fixes Disk I/O | |
| **P2** | **Optimize hot path strings** using `std::fmt::Write` over pre-allocated buffers in `Params::to_string()` and `hasher.rs` to eliminate transient `Vec` allocations | Small | Lowers GC pauses | |
| **P3** | Thread the computed `params_hash` through the request extensions so it's computed once | Small | Minor CPU save | |
