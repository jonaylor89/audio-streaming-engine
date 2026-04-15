# Architecture

On-the-fly audio processing server — like Thumbor/Imagor but for audio. Processes audio via URL parameters, caches results, streams back.

## Endpoints

| Route | Handler | Middleware | Description |
|---|---|---|---|
| `GET /{*streamingpath}` | `streamingpath_handler` | cache → auth | **Primary.** Buffered processing; returns `Content-Length` + `Accept-Ranges`. Request coalescing via `InflightTracker`. |
| `GET /stream/{*streamingpath}` | `stream_handler` | auth only | Streaming via `Transfer-Encoding: chunked`. Lower TTFB on cache miss. No coalescing. |
| `GET /thumbnail/{*streamingpath}` | `thumbnail_handler` | cache → auth | Auto-selects the most representative segment (chroma → SSM → fitness). Returns `X-Thumbnail-*` headers. |
| `GET /meta/{*streamingpath}` | `meta_handler` | cache → auth | Returns JSON metadata for the audio file. |
| `GET /params/{*streamingpath}` | `params` | — | Preview parsed parameters as JSON without processing. |
| `GET /health` | `health_check` | — | Health check. |
| `GET /metrics` | — | — | Prometheus metrics. |
| `GET /openapi.json` | — | — | OpenAPI spec. |
| `GET /api-schema` | — | — | Simplified schema for LLM consumption. |
| `GET /list` | `list_handler` | — | Web UI file listing (when `web_ui: true`). |

URL format: `/{HASH|unsafe}/AUDIO_KEY?param1=value1&param2=value2`

- `unsafe` — skip HMAC verification (development)
- `HASH` — HMAC-SHA256 signature of the path

## Request Flow

### Buffered (`/{*streamingpath}`)

```
Request → CorsLayer → TraceLayer → track_metrics
  → cache_middleware (parses Params, checks response cache)
    HIT → 200/206 with Content-Length + Accept-Ranges
    MISS → injects CacheMissContext + Params into extensions
      → auth_middleware (HMAC-SHA256 verify or /unsafe/ passthrough)
        → streamingpath_handler
            1. result_storage.get(params_hash) → HIT? serve immediately
            2. InflightTracker.try_join(params_hash)
               Leader: fetch → process → result_storage.put → guard.complete() → notify waiters
               Waiter: await Notify → read result_storage
               Failed: return cached error (30s TTL)
            3. ctx.populate(body) → cache.set (background)
            → Body::from(bytes) with Content-Length
```

### Streaming (`/stream/{*streamingpath}`)

```
Request → auth_middleware → stream_handler
  1. result_storage.get(params_hash) → HIT? serve with Content-Length
  2. Passthrough? (no processing params + known format + local storage)
     YES → storage.get_stream() → tee to HTTP + background storage collector
  3. Otherwise: fetch source → Processor.process_streaming
     → SyncSender<Bytes> → bridge → tee to HTTP + storage collector
     → Body::from_stream (chunked, no Content-Length)
```

### Thumbnail (`/thumbnail/{*streamingpath}`)

```
Request → cache/auth → thumbnail_handler
  1. result_storage.get(thumb_hash) → HIT? serve
  2. Fetch source audio
  3. spawn_blocking: decode to PCM
  4. spawn_blocking: chroma → self-similarity matrix → fitness measure
  5. Process with computed start_time + duration
  6. Background: result_storage.put + cache.populate
  → Response with X-Thumbnail-Confidence, X-Thumbnail-Start, X-Thumbnail-Duration, Link: rel=canonical
```

## Processing Pipeline

All CPU-heavy work runs in `spawn_blocking` behind a `tokio::sync::Semaphore` (sized to `num_cpus`).

**Buffered:** FFmpeg processes via native FFI bindings (not CLI subprocesses). Custom `AVIOContext` callbacks for in-memory I/O. Output collected into `Vec<u8>` pre-allocated at `input_size / 2`.

**Streaming:** `StreamingOutputContext` sends 32 KB chunks through `SyncSender`, bridged to async Tokio via `std::mpsc` → `tokio::mpsc`. Peak memory ~256 KB in flight regardless of file size.

**Passthrough short-circuit:** When no processing params are set, returns `input.clone()` (O(1) `Bytes` refcount bump). Streaming passthrough uses `storage.get_stream()` for zero-buffer file I/O.

## Request Coalescing (`src/inflight.rs`)

Prevents thundering herd on the buffered endpoint. Uses `DashMap::entry` for lock-free leader election.

- **Leader** does the work, writes to result storage *before* notifying waiters
- **Waiters** sleep on `tokio::sync::Notify`, then read from result storage (payload never cloned through coalescing layer)
- **Cancellation-safe:** `InflightGuard::drop` always calls `notify_waiters()` — waiters detect `None` result and retry leader election
- **Failure cache:** Failed keys cached for 30s via `FailedEntry` to prevent retry storms

Metrics: `request.coalesced.leader`, `request.coalesced.waiter`, `request.coalesced.waiter_retry`, `request.coalesced.failed_cache_hit`

## Storage & Caching

### Source Storage (`AudioStorage` trait)
- **FileStorage** — local filesystem, streaming via `ReaderStream`
- **S3Storage** — AWS S3 / MinIO, streaming via `ByteStream::into_async_read`
- **GCloudStorage** — Google Cloud Storage
- **CachedStorage** — wraps S3/GCS with local disk LRU cache (`DiskEvictor` with `AtomicU64` running size counter)

### Result Storage
Separate storage instance for processed output. Configured via `result_storage` in YAML. Falls back to source storage if not configured.

### Response Cache
- **RedisCache** — `MultiplexedConnection` with TTL
- **FileSystemCache** — local filesystem with `.meta` expiry files, `DiskEvictor` for size-based eviction

Cache middleware checks response cache on every buffered request. On miss, injects `CacheMissContext` — handlers call `ctx.populate(body.clone())` to write in background (no middleware body buffering).

## Encoded Parameters

Complex parameter sets can be packed into a single `encoded` query parameter (JSON → URL-safe base64, no padding).

```
Traditional:  /track.mp3?format=wav&volume=0.8&reverse=true&lowpass=5000&sample_rate=48000
Encoded:      /track.mp3?encoded=eyJmb3JtYXQiOiJ3YXYi...
Mixed:        /track.mp3?encoded=eyJmb3JtYXQiOiJ3YXYi...&format=flac  (explicit wins)
```

Precedence: path key > explicit query params > encoded params. Tags are merged.

## Configuration

YAML files in `config/` + environment variable overrides (`APP_SECTION__KEY=value`).

| File | Purpose |
|---|---|
| `config/base.yml` | Defaults |
| `config/local.yml` | Development |
| `config/production.yml` | Production |

Key settings:
- `port` / `PORT` — listen port (default: 8080)
- `application.hmac_secret` — HMAC signing key
- `application.web_ui` — enable web UI at `/`
- `processor.concurrency` — FFmpeg semaphore permits (default: `num_cpus`)
- `processor.max_source_size` — max source file bytes (default: 100 MB)
- `storage.client` — `S3(...)` or `GCS(...)` or omit for filesystem
- `storage.client.*.local_cache` — local disk cache for remote backends
- `cache` — `Redis { uri }` or `Filesystem { base_dir, max_size_mb }`

Storage feature flags: `--features filesystem` (default), `--features s3`, `--features gcs`.

## MCP Integration

The `mcp-server/` directory contains a Node.js MCP server that exposes audio processing to LLM clients. See [`mcp-server/README.md`](../mcp-server/README.md) for setup instructions.

Tools: `process_audio`, `preview_audio_params`, `get_server_health`.
