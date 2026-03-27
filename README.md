# Streaming Engine

On-the-fly audio processing server. Think [Thumbor](https://github.com/thumbor/thumbor) / [Imagor](https://github.com/cshum/imagor), but for audio.

Process audio files through URL parameters — apply effects, convert formats, slice, reverse, and stream the result back in real time.

## Quick Start

```sh
# Default build with filesystem storage
cargo run

# Process audio via URL
curl "http://localhost:8080/unsafe/sample.mp3?reverse=true&fade_in=1&speed=0.8"

# Get audio metadata
curl "http://localhost:8080/meta/unsafe/sample.mp3"
```

### MCP Integration (Connect to LLMs)

See [MCP README](./mcp-server/README.md)

## How It Works

```
GET /unsafe/AUDIO_URL?effect1=value&effect2=value
```

The server fetches the audio, builds an FFmpeg filter chain from the query parameters, processes it, caches the result, and streams it back.

```
/HASH|unsafe/AUDIO?param1=value1&param2=value2&...
```

- `HASH` — URL signature hash, or `unsafe` for development
- `AUDIO` — audio URI (local file or remote URL)

## Supported Parameters

### Format & Encoding
- `format` — Output format (mp3, wav, etc.)
- `codec` — Audio codec
- `sample_rate` — Sample rate in Hz
- `channels` — Number of audio channels
- `bit_rate` — Bit rate in kbps
- `bit_depth` — Bit depth
- `quality` — Encoding quality (0.0–1.0)
- `compression_level` — Compression level

### Time Operations
- `start_time` — Start time in seconds
- `duration` — Duration in seconds
- `speed` — Playback speed multiplier
- `reverse` — Reverse audio (true/false)

### Volume Operations
- `volume` — Volume adjustment multiplier
- `normalize` — Normalize audio levels (true/false)
- `normalize_level` — Target normalization level in dB

### Audio Effects
- `lowpass` / `highpass` / `bandpass` — Filter cutoff frequencies
- `bass` / `treble` — Boost/cut levels
- `echo` / `chorus` / `flanger` / `phaser` / `tremolo` — Effect parameters
- `compressor` — Compressor parameters
- `noise_reduction` — Noise reduction parameters

### Fades
- `fade_in` / `fade_out` — Duration in seconds
- `cross_fade` — Cross-fade duration in seconds

### Advanced
- `custom_filters` — Custom FFmpeg filter parameters
- `custom_options` — Custom FFmpeg options
- `tags` — Metadata tags (as `tag_NAME=VALUE`)

## Endpoints

### Preview Parameters — `/params`

```sh
curl "http://localhost:8080/params/unsafe/sample.mp3?reverse=true&fade_in=1"
```

### Audio Metadata — `/meta`

```sh
curl "http://localhost:8080/meta/unsafe/sample.mp3"
```

## Storage Backends

- **Local File System** (`filesystem` feature — default)
- **AWS S3 / MinIO** (`s3` feature)
- **Google Cloud Storage** (`gcs` feature)

```sh
cargo build --features filesystem   # default
cargo build --features gcs
cargo build --features s3
cargo build --features "filesystem,gcs"
```

## Configuration

Uses YAML config files + environment variable overrides.

- `config/base.yml` — Base configuration
- `config/local.yml` — Development
- `config/production.yml` — Production

Environment variables: `APP_SECTION__KEY=value` (e.g., `APP_APPLICATION__PORT=8080`)

See [full configuration docs](./docs/) for details.

## Development

```sh
just dev          # Run with auto-reload
just test         # Run tests
just bench        # Run benchmarks
just lint         # Clippy
just fmt          # Format
just check        # Full check: fmt, lint, build, test
```
