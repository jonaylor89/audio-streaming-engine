# Benchmarks

Three focused suites using [Divan](https://docs.rs/divan/latest/divan/).

## Suites

| File | What it measures | FFmpeg? |
|------|-----------------|---------|
| `processing.rs` | Passthrough, transcode, filter chains, streaming-vs-buffered, thumbnail pipeline | Yes — uses real fixtures from `uploads/` |
| `hashing.rs` | Storage hashing, params hashing, HMAC, params parsing & serialization, path normalization | No — pure CPU |
| `io.rs` | `FileStorage` and `FileSystemCache` put/get at 10KB–1MB | No — filesystem I/O |

## Running

```bash
# All benchmarks
cargo bench

# Single suite
cargo bench --bench processing
cargo bench --bench hashing
cargo bench --bench io

# Single function
cargo bench --bench processing -- passthrough
```

## Fixtures

Benchmarks that hit FFmpeg use real audio files from `uploads/`:
- `sample1.mp3` — 32s stereo MP3 @ 48kHz (~776KB)
- `test_tone.wav` — 5s mono PCM @ 44.1kHz (~432KB)

## Notes

- Processing benchmarks are annotated with `ignore = cfg!(codspeed)` so they run
  locally but are skipped in CI CodSpeed mode (they're too slow for wall-clock instrumentation).
- FFmpeg log level is set to `AV_LOG_ERROR` at init to suppress noisy warnings
  (e.g. `mp3float` timestamp messages).
