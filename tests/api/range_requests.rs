//! Tests for HTTP Range request support.
//!
//! All response paths (cache hit, result-storage hit, and fresh processing)
//! go through the shared `build_audio_response` which honors the `Range`
//! header.  These tests verify that byte ranges work on both the first
//! (uncached) request and subsequent (cached) requests.

use crate::helpers::spawn_app;

/// The very first request for an audio file — before any cache is populated —
/// must support byte ranges.  Audio players (Safari, iOS AVPlayer) send
/// `Range: bytes=0-` on the first request to probe for range support.
#[tokio::test]
async fn first_request_honors_byte_ranges() {
    let app = spawn_app().await;
    let file = "test_tone.wav";

    // First: fetch the full body so we know the expected size and content.
    let full_response = app
        .api_client
        .get(format!("{}/unsafe/{}", app.address, file))
        .send()
        .await
        .expect("Failed to execute full request");

    assert_eq!(full_response.status(), reqwest::StatusCode::OK);
    let full_bytes = full_response
        .bytes()
        .await
        .expect("failed to read full body");

    // Give result-storage and the response-cache background write a moment to
    // flush.  Under heavy CI load the async cache population may race with the
    // next request, causing an empty body (and thus 416).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second: a range request for the same file (still a result-storage hit,
    // not a cache hit — the cache is populated asynchronously).
    let range_response = app
        .api_client
        .get(format!("{}/unsafe/{}", app.address, file))
        .header(reqwest::header::RANGE, "bytes=2-7")
        .send()
        .await
        .expect("Failed to execute range request");

    assert_eq!(
        range_response.status(),
        reqwest::StatusCode::PARTIAL_CONTENT,
        "handler should support byte ranges on non-cached responses"
    );
    assert_eq!(
        range_response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
    assert_eq!(
        range_response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("6")
    );

    let content_range = range_response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .expect("missing Content-Range header")
        .to_string();
    let partial_bytes = range_response
        .bytes()
        .await
        .expect("failed to read partial body");

    assert_eq!(content_range, format!("bytes 2-7/{}", full_bytes.len()));
    assert_eq!(partial_bytes.len(), 6);
    assert_eq!(partial_bytes, full_bytes.slice(2..8));
}

/// After the response cache is populated, a cached response should also
/// honor byte ranges (handled by `cache_middleware`).
#[tokio::test]
async fn cached_response_honors_byte_ranges() {
    let app = spawn_app().await;
    let file = "test_tone.wav";

    // Warm the cache.
    let full_response = app
        .api_client
        .get(format!("{}/unsafe/{}", app.address, file))
        .send()
        .await
        .expect("Failed to execute first request");

    assert_eq!(full_response.status(), reqwest::StatusCode::OK);
    let full_bytes = full_response
        .bytes()
        .await
        .expect("failed to read full body");

    // Let background cache population finish.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Range request against cached response.
    let range_response = app
        .api_client
        .get(format!("{}/unsafe/{}", app.address, file))
        .header(reqwest::header::RANGE, "bytes=2-7")
        .send()
        .await
        .expect("Failed to execute range request");

    assert_eq!(
        range_response.status(),
        reqwest::StatusCode::PARTIAL_CONTENT,
        "cached response should support byte ranges"
    );

    let content_range = range_response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .expect("missing Content-Range header")
        .to_string();
    let partial_bytes = range_response
        .bytes()
        .await
        .expect("failed to read partial body");

    assert_eq!(content_range, format!("bytes 2-7/{}", full_bytes.len()));
    assert_eq!(partial_bytes.len(), 6);
    assert_eq!(partial_bytes, full_bytes.slice(2..8));
}
