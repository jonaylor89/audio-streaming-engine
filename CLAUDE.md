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
