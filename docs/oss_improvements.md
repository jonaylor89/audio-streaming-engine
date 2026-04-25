# Open Source Readiness Audit

A review of the audio-streaming-engine codebase for open source readiness — what's already strong, what's missing, and what would make this project thrive as a community-driven OSS project. Findings are grouped by impact tier.

---

## What's Already Strong

Before diving into improvements, it's worth calling out what this project gets right — some of these are things many OSS projects never achieve:

- **Clear, compelling README pitch.** The Thumbor/Imagor analogy immediately communicates what this does and who it's for. The quick-start section lets someone go from clone to working request in under a minute.
- **Excellent architecture documentation.** `docs/ARCHITECTURE.md` is detailed, accurate, and reads like a real engineering doc, not marketing. The request flow diagrams, memory profiles, and blocking thread budgets in `docs/PerformanceAudit.md` are the kind of thing most OSS projects never produce.
- **Serious CI pipeline.** Separate workflows for build/test/lint, benchmarks (CodSpeed), and MCP integration. Benchmarks run on every PR — this is table-stakes for performance-sensitive infrastructure but rarely done.
- **Well-factored storage abstraction.** The `AudioStorage` trait with filesystem/S3/GCS backends behind feature flags is clean and extensible. New storage backends can be added without touching core routing.
- **Lean Docker image.** Multi-stage build with `cargo-chef`, static FFmpeg compiled with only the needed filters, minimal runtime base. This is production-grade.
- **Native FFmpeg FFI.** Direct bindings instead of CLI subprocesses — the right architectural call for a performance-critical audio server.

---

## Tier 1 — High Impact (Will determine adoption or abandonment)

### 1.1 License: GPL-3 is a dealbreaker for most startups

The project is licensed under **GPL-3.0**, which requires any software that links to this code to also be released under GPL-3. For a server library that startups want to embed or deploy as part of a larger proprietary system, this is a hard blocker. Most companies' legal teams will reject GPL-3 outright.

**What to do:** Re-license to **MIT**, **Apache-2.0**, or **dual MIT/Apache-2.0**. This is the de facto standard for infrastructure OSS that wants broad adoption.

| Project | License | Why |
|---|---|---|
| [Imagor](https://github.com/cshum/imagor) | Apache-2.0 | Direct analogue to this project |
| [Thumbor](https://github.com/thumbor/thumbor) | MIT | The project you cite as inspiration |
| [FFmpeg](https://ffmpeg.org/legal.html) | LGPL-2.1 / GPL (optional) | Note: FFmpeg itself is LGPL by default; GPL only when `--enable-gpl` codecs are used |
| [Axum](https://github.com/tokio-rs/axum) | MIT | Framework this project uses |
| [MinIO](https://github.com/minio/minio) | AGPL-3.0 | Chose AGPL deliberately to force commercial licensing — a revenue strategy, not a community-growth strategy |

> **Note:** Since this project links FFmpeg with `--enable-gpl` and `--enable-nonfree` (libfdk-aac), the Dockerfile build is already GPL-encumbered. If you want to re-license to MIT/Apache-2.0, you'll need to either drop `libfdk-aac` (replace with the built-in AAC encoder) or make the nonfree codecs optional. This is a solvable problem — Imagor handles it by making features optional.

### 1.2 MCP server lists MIT, root project lists GPL-3

`mcp-server/package.json` says `"license": "MIT"` while the root project is GPL-3. This is a legal contradiction — the MCP server lives in the same repo and presumably shares code or is considered part of the same work. Pick one license and apply it consistently.

### 1.3 Missing `CONTRIBUTING.md`

There is no contributing guide. A developer who wants to submit a PR has no idea:
- How to set up the development environment (FFmpeg dependencies, Redis, etc.)
- What the PR review process looks like
- Whether there's a code style guide beyond `cargo fmt`
- How to run tests locally (the full FFmpeg dev library install isn't obvious)

**Exemplars:**
- [Axum CONTRIBUTING.md](https://github.com/tokio-rs/axum/blob/main/CONTRIBUTING.md) — concise, covers setup, testing, and PR expectations
- [Imagor CONTRIBUTING.md](https://github.com/cshum/imagor/blob/master/CONTRIBUTING.md) — short but covers the essentials
- [Ruff CONTRIBUTING.md](https://github.com/astral-sh/ruff/blob/main/CONTRIBUTING.md) — excellent for Rust projects, covers architecture, testing, and development workflow

### 1.4 No `docker-compose.yml` for local development

The project requires Redis (optional), FFmpeg dev libraries, and Rust nightly. There's an `init_redis.sh` script, but no unified way to spin up a development environment. A `docker-compose.yml` that provides the server + Redis + a sample audio file volume would dramatically lower the barrier to first contribution.

**Exemplars:**
- [Supabase docker-compose](https://github.com/supabase/supabase/blob/master/docker/docker-compose.yml) — full local dev stack
- [Plausible docker-compose](https://github.com/plausible/analytics/blob/master/docker-compose.yml) — simple, self-contained

### 1.5 Hardcoded HMAC secret in base config and code

`config/base.yml` ships with `hmac_secret: "this-is-a-secret"` and the Rust `Default` impl in `config.rs` also hardcodes this value. While this is fine for development (the `unsafe` path bypasses HMAC anyway), it creates a risk that someone deploys to production without setting a real secret and all HMAC signatures are trivially forgeable.

**What to do:**
- Remove the hardcoded default from the `Default` impl — require it to be explicitly set
- Add a startup warning/error when the default secret is detected in production mode
- Document this in a `SECURITY.md` or in the README's deployment section

---

## Tier 2 — Medium Impact (Will determine contributor experience and project maturity)

### 2.1 No `CHANGELOG.md`

There's no changelog. For a `v0.1.0` project this is forgivable, but as soon as you cut a release, adopters need to know what changed, what broke, and what's new.

**Exemplars:**
- [Keep a Changelog](https://keepachangelog.com/) — the standard format
- [git-cliff](https://github.com/orhun/git-cliff) — auto-generates changelogs from conventional commits; widely used in Rust projects

### 2.2 No `SECURITY.md` or vulnerability reporting process

No `SECURITY.md`, no security policy, no responsible disclosure process. For an audio processing server that handles untrusted input (remote URLs, FFmpeg filter chains, `custom_filters` parameter), this is a gap. The `custom_filters` parameter in particular allows arbitrary FFmpeg filter strings — this needs to be documented as a security consideration.

**What to do:** Add a `SECURITY.md` with:
- How to report vulnerabilities (email, not public issues)
- Scope of security considerations (FFmpeg filter injection, SSRF via remote URL fetching, HMAC bypass)
- Supported versions

**Exemplar:** [Rust's SECURITY.md](https://github.com/rust-lang/rust/blob/master/SECURITY.md)

### 2.3 No GitHub issue/PR templates

No issue templates, no PR template. This means every bug report will be a wall of unstructured text, and every PR will lack context on what it changes and how to test it.

**What to do:** Add:
- `.github/ISSUE_TEMPLATE/bug_report.md` — with sections for reproduction steps, expected vs. actual, environment (OS, FFmpeg version, storage backend)
- `.github/ISSUE_TEMPLATE/feature_request.md`
- `.github/PULL_REQUEST_TEMPLATE.md` — checklist for tests, docs, breaking changes

**Exemplar:** [Deno's issue templates](https://github.com/denoland/deno/tree/main/.github/ISSUE_TEMPLATE)

### 2.4 README lacks prerequisites section

The README jumps straight to `cargo run` without mentioning that you need:
- FFmpeg development libraries (`libavcodec-dev`, `libavformat-dev`, etc.)
- Rust (with specific edition — the project uses `edition = "2024"`)
- Optionally: Redis, Docker

A new contributor on a fresh machine will hit a wall of compiler errors about missing `libav*` headers. The CI workflow (`rust.yml`) lists the required packages but the README doesn't.

**Exemplars:**
- [SurrealDB README](https://github.com/surrealdb/surrealdb#readme) — clear prerequisites section
- [Sonic README](https://github.com/valeriansaliou/sonic#readme) — lists system dependencies prominently

### 2.5 `TODO.md` exposes internal product roadmap

`TODO.md` reads like an internal product planning document with references to specific commercial APIs (Cyanite, Udio), internal feature ideas ("MusicGen MCP Tool"), and detailed ML pipeline plans. This is fine for internal use but in an OSS context it:
- Sets expectations that may never be met
- Mixes aspirational ideas with actionable work
- Doesn't use GitHub Issues/Projects, which is where OSS contributors look for work

**What to do:** Move actionable items to GitHub Issues with `good first issue` / `help wanted` labels. Remove or archive the speculative items. Use GitHub Projects or a roadmap discussion for longer-term vision.

---

## Tier 3 — Nice-to-Have (Polish and community growth)

### 3.1 No `docker-compose.yml` for the full stack

For users who just want to try the server without installing Rust and FFmpeg dev libraries, a `docker-compose.yml` that builds and runs the server with a mounted `uploads/` directory and optional Redis would be the fastest path to "I processed my first audio file."

```yaml
# Example structure
services:
  streaming-engine:
    build: .
    ports:
      - "8080:8080"
    volumes:
      - ./uploads:/app/uploads
    environment:
      - APP_ENVIRONMENT=local
  redis:
    image: redis:7-alpine
    ports:
      - "6379:6379"
```

### 3.2 Missing crate-level documentation (`//!` doc comments)

`src/lib.rs` is a bare list of `pub mod` declarations with no crate-level doc comment. For a Rust library that exposes `streaming_engine` as a crate (it has a `[lib]` target), adding `//!` documentation at the top of `lib.rs` would make `cargo doc` output useful.

**Exemplar:** [Axum's lib.rs](https://github.com/tokio-rs/axum/blob/main/axum/src/lib.rs) — extensive crate-level documentation with examples

### 3.3 No release automation

There are no GitHub Actions for:
- Creating GitHub Releases with changelogs
- Publishing the MCP server to npm
- Publishing Docker images to GHCR or Docker Hub

Right now, releases are manual. As the project grows, this becomes a bottleneck and a source of inconsistency.

**Exemplars:**
- [release-plz](https://github.com/MarcoIeni/release-plz) — automated release workflow for Rust projects
- [Changesets](https://github.com/changesets/changesets) — for the npm MCP server

### 3.4 Add a `Makefile` or document `just` as a dependency

The project uses `just` (a command runner), but `just` isn't a standard tool that most developers have installed. The README references `just dev`, `just test`, etc. without mentioning that `just` needs to be installed. Either:
- Document `just` installation in prerequisites
- Add a `Makefile` as a fallback (many developers already have `make`)
- Or add the raw `cargo` commands alongside the `just` recipes in the README

### 3.5 The `image` crate dependency looks unused for audio processing

`Cargo.toml` includes `image = "0.25.4"` which is an image processing library. For an audio processing server, this is surprising. If it's used for waveform visualization or spectrograms, that should be documented. If it's a leftover, removing it reduces compile time and dependency surface.

### 3.6 GitHub Sponsors is configured but not promoted

`.github/FUNDING.yml` exists with `github: [jonaylor89]`, which is great. But the README has no "Sponsors" badge or section. If you want community funding, make it visible.

### 3.7 No architecture diagram

The `ARCHITECTURE.md` uses ASCII request flow diagrams which are clear but a Mermaid diagram in the README showing the high-level data flow (request → auth → cache → process → respond) would make the project more approachable at a glance. GitHub renders Mermaid natively in markdown.

### 3.8 `pretty_assertions` is listed as a runtime dependency

`pretty_assertions = "1.4.1"` is in `[dependencies]` instead of `[dev-dependencies]`. It's a testing utility that provides colored diffs — it should only be included in test builds. This adds unnecessary compile time and binary size for production builds.

---

## Summary Checklist

| # | Item | Tier | Effort |
|---|---|---|---|
| 1.1 | Re-license from GPL-3 to MIT/Apache-2.0 | 🔴 T1 | Small (legal decision) |
| 1.2 | Reconcile MIT vs GPL-3 license conflict | 🔴 T1 | Trivial |
| 1.3 | Add `CONTRIBUTING.md` | 🔴 T1 | Medium |
| 1.4 | Add `docker-compose.yml` for dev | 🔴 T1 | Small |
| 1.5 | Fix hardcoded HMAC secret defaults | 🔴 T1 | Small |
| 2.1 | Add `CHANGELOG.md` | 🟡 T2 | Small |
| 2.2 | Add `SECURITY.md` | 🟡 T2 | Small |
| 2.3 | Add issue/PR templates | 🟡 T2 | Small |
| 2.4 | Add prerequisites to README | 🟡 T2 | Small |
| 2.5 | Move `TODO.md` items to GitHub Issues | 🟡 T2 | Medium |
| 3.1 | Add `docker-compose.yml` for users | 🟢 T3 | Small |
| 3.2 | Add crate-level `//!` doc comments | 🟢 T3 | Small |
| 3.3 | Add release automation workflows | 🟢 T3 | Medium |
| 3.4 | Document `just` or add `Makefile` fallback | 🟢 T3 | Small |
| 3.5 | Audit `image` crate dependency | 🟢 T3 | Trivial |
| 3.6 | Add sponsors badge to README | 🟢 T3 | Trivial |
| 3.7 | Add Mermaid architecture diagram to README | 🟢 T3 | Small |
| 3.8 | Move `pretty_assertions` to dev-dependencies | 🟢 T3 | Trivial |
