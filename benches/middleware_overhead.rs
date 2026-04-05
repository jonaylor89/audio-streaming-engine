//! Benchmark quantifying the overhead removed by the P0 cache_middleware change.
//!
//! The old miss path did:
//!   1. `axum::body::to_bytes(response.into_body(), usize::MAX)` — poll + collect
//!   2. `body_bytes.clone()` — for the background cache write
//!   3. `build_audio_response(…, body_bytes)` — rebuild Response with headers
//!
//! The new miss path does: nothing — response passes through untouched.
//! This benchmark measures the cost of (1) + (2) + (3) at various body sizes.

fn main() {
    divan::main();
}

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use bytes::Bytes;
use divan::Bencher;

fn make_body(size: usize) -> Bytes {
    Bytes::from(vec![0xFFu8; size])
}

const SIZES: &[usize] = &[
    1024,            // 1 KB
    100 * 1024,      // 100 KB
    1024 * 1024,     // 1 MB
    10 * 1024 * 1024, // 10 MB
];

/// Simulates the OLD cache_middleware miss path:
/// to_bytes → clone for cache → rebuild response with headers.
fn old_miss_path(body: Bytes, request_headers: &HeaderMap) -> Response<Body> {
    // Step 1: to_bytes already done (we pass Bytes directly to simulate the result)
    // In reality to_bytes on Body::from(Bytes) extracts the single chunk.
    // We simulate the clone that was needed for the background cache write.
    let _cache_copy = body.clone(); // Step 2: clone for bg cache write

    // Step 3: rebuild response with headers (same as build_audio_response)
    let content_type = "audio/mpeg";
    match parse_range(request_headers, body.len()) {
        Some((start, end)) => {
            let content = body.slice(start..=end);
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, content.len().to_string())
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, body.len()),
                )
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"))
                .body(Body::from(content))
                .unwrap()
        }
        None => Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, body.len().to_string())
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"))
            .body(Body::from(body))
            .unwrap(),
    }
}

fn parse_range(headers: &HeaderMap, total: usize) -> Option<(usize, usize)> {
    let range = headers.get(header::RANGE)?;
    let s = range.to_str().ok()?.strip_prefix("bytes=")?;
    let (start, end) = s.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end = if end.is_empty() { total - 1 } else { end.parse().ok()? };
    Some((start, end.min(total - 1)))
}

/// OLD path: to_bytes + clone + rebuild response.
/// This is what was removed.
mod old_middleware_miss {
    use super::*;

    #[divan::bench(args = SIZES)]
    fn to_bytes_clone_rebuild(bencher: Bencher<'_, '_>, size: usize) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let headers = HeaderMap::new();

        bencher
            .with_inputs(|| {
                // Simulate handler returning Body::from(bytes)
                let bytes = make_body(size);
                let response = Response::builder()
                    .header(header::CONTENT_TYPE, "audio/mpeg")
                    .header(header::CONTENT_LENGTH, bytes.len().to_string())
                    .body(Body::from(bytes))
                    .unwrap();
                response
            })
            .bench_values(|response| {
                rt.block_on(async {
                    // Step 1: to_bytes — this is the expensive call
                    let body_bytes =
                        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

                    // Step 2 + 3: clone + rebuild
                    divan::black_box(old_miss_path(body_bytes, &headers));
                })
            });
    }
}

/// NEW path: pass through (zero cost in the middleware).
mod new_middleware_miss {
    use super::*;

    #[divan::bench(args = SIZES)]
    fn passthrough(bencher: Bencher<'_, '_>, size: usize) {
        bencher
            .with_inputs(|| {
                let bytes = make_body(size);
                let response = Response::builder()
                    .header(header::CONTENT_TYPE, "audio/mpeg")
                    .header(header::CONTENT_LENGTH, bytes.len().to_string())
                    .body(Body::from(bytes))
                    .unwrap();
                response
            })
            .bench_values(|response| {
                // New path: middleware just returns the response.
                divan::black_box(response);
            });
    }
}
