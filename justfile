# Streaming Engine Development Commands

# List available recipes
default:
    @just --list

# Install cargo-watch if not present
install-tools:
    cargo install cargo-watch

# Initialize Redis (Docker)
init-redis:
    #!/usr/bin/env bash
    ./scripts/init_redis.sh

# Run streaming engine with auto-reload
dev:
    cargo watch -x 'run' -w src -w Cargo.toml -w config

# Build the project
build:
    cargo build

# Build with release optimizations
build-release:
    cargo build --release

# Run all tests
test:
    cargo test

# Run specific test by name
test-name name:
    cargo test {{name}}

# Run benchmarks
bench:
    cargo bench

# Run linter
lint:
    cargo clippy

# Format code
fmt:
    cargo fmt

# Check formatting without changing files
fmt-check:
    cargo fmt --check

# Clean build artifacts
clean:
    cargo clean

# Full check: format, lint, build, test
check:
    just fmt-check
    just lint
    just build
    just test

# Setup development environment
setup:
    just install-tools
    just build

# Run without auto-reload
run:
    cargo run

# Show project structure
tree:
    tree -I target

# Stop Redis container
stop-redis:
    #!/usr/bin/env bash
    echo "🛑 Stopping Redis..."
    docker ps -a --filter 'name=redis' -q | xargs -r docker rm -f
    echo "✅ Redis stopped"
