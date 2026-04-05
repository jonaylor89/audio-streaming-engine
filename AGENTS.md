# Streaming Engine Development Guide

## What This Project Is

Streaming Engine is an **audio processing server** — it processes audio on the fly via URL parameters (like Thumbor/Imagor for images, but for audio). It is **not** an AI product. The MCP server integration exists to let LLMs use the audio processing API as a tool, but the core project is a deterministic audio processing pipeline built on FFmpeg.

## Build & Test Commands
- `just` — List available recipes
- `just dev` — Run with auto-reload
- `just build` — Build the project
- `just test` — Run all tests
- `just test-name <name>` — Run a specific test
- `just bench` — Run benchmarks
- `just lint` — Run linter (clippy)
- `just fmt` — Format code
- `just check` — Full check: format, lint, build, test

## Project Structure
- `src/` — Core streaming engine (Rust)
- `crates/ffmpeg` — Safe FFmpeg wrapper
- `crates/ffmpeg-sys` — Raw FFI bindings to FFmpeg
- `mcp-server/` — MCP integration for LLM tool use (Node.js)
- `config/` — YAML configuration files
- `benches/` — Performance benchmarks
- `scripts/` — Deployment and CI scripts

## Code Style
- **Imports**: Group std, external crates, then local modules
- **Error Handling**: Use `color_eyre::Result`, `thiserror` for custom errors
- **Logging**: Use `tracing` with structured logging and `#[instrument]` for functions
- **Types**: Prefer explicit types, use `Uuid` for IDs, `DateTime<Utc>` for timestamps
- **Naming**: snake_case for functions/variables, PascalCase for types, modules in snake_case
- **Async**: Use `tokio::main` and async/await throughout
- **API**: Use Axum with `State` extraction and `Json` responses

## Testing Conventions
- **Location**: Integration tests go in `tests/api/` and are registered in `tests/api/main.rs`. Keep `#[cfg(test)] mod tests` in `src/` only for trivial unit tests that need access to private internals.
- **Fixtures**: Use existing audio files in `uploads/` (`sample1.mp3`, `test_tone.wav`, etc.). Do NOT generate temporary files — it leaks artifacts and adds cleanup complexity.
- **Documentation**: Every test file gets a module-level `//!` doc comment explaining what it covers. Every `#[test]`/`#[tokio::test]` gets a doc comment explaining *what bug or behavior it guards against*, not just what it does mechanically.
- **Readability**: Test bodies should read like plain English. Prefer direct `match` arms with descriptive panic messages over helper enums or classifier functions. Use macros like `assert_leader!` only when they genuinely reduce noise.
- **Naming**: Test names should describe the expected behavior: `cached_response_honors_byte_ranges`, not `test_range_request_1`. No `test_` prefix.
- **Concurrency tests**: Use `Arc` + `AtomicUsize` counters to verify how many tasks took each path. Use `tokio::sync::Barrier` to maximize contention. Use `#[tokio::test(flavor = "multi_thread")]` when testing real thread races.
