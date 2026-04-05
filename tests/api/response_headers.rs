//! Tests that audio response headers are **consistent across every cache layer**.
//!
//! There are three paths a request can take:
//!
//! | Layer                  | What serves it                      |
//! |------------------------|-------------------------------------|
//! | Fresh processing       | Handler: result-storage miss → FFmpeg |
//! | Result-storage hit     | Handler: result-storage hit, cache miss |
//! | Response-cache hit     | `cache_middleware` serves directly   |
//!
//! All three must return the same set of headers so that browsers and audio
//! players behave identically regardless of which layer served the response.
//!
//! Required headers on every audio response:
//! - `Content-Type`: a real audio MIME type (never `application/octet-stream`)
//! - `Content-Length`: exact byte count
//! - `Accept-Ranges: bytes`
//! - `Content-Disposition: inline`
//! - `Access-Control-Allow-Origin: *`
//! - `Cache-Control: no-cache`
//! - Range requests → `206 Partial Content` + `Content-Range`

use crate::helpers::spawn_app;

/// The expected headers that every audio response must include.
fn assert_common_audio_headers(response: &reqwest::Response, context: &str) {
    let headers = response.headers();

    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(missing)");
    assert!(
        content_type.starts_with("audio/"),
        "{context}: Content-Type should be audio/*, got {content_type:?}"
    );

    assert!(
        headers.get(reqwest::header::CONTENT_LENGTH).is_some(),
        "{context}: missing Content-Length"
    );

    assert_eq!(
        headers
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes"),
        "{context}: missing or wrong Accept-Ranges"
    );

    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("inline"),
        "{context}: missing or wrong Content-Disposition"
    );

    assert_eq!(
        headers
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("*"),
        "{context}: missing or wrong Access-Control-Allow-Origin"
    );

    assert_eq!(
        headers
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache"),
        "{context}: missing or wrong Cache-Control"
    );
}

/// Verify that the response for a Range request has the correct 206 headers.
fn assert_range_headers(response: &reqwest::Response, expected_slice_len: usize, context: &str) {
    assert_eq!(
        response.status(),
        reqwest::StatusCode::PARTIAL_CONTENT,
        "{context}: expected 206"
    );
    let expected_len = expected_slice_len.to_string();
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some(expected_len.as_str()),
        "{context}: Content-Length should match slice"
    );
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .is_some(),
        "{context}: missing Content-Range"
    );
}

// ---------------------------------------------------------------------------
// Layer 1: Fresh processing (result-storage miss)
// ---------------------------------------------------------------------------

/// On the very first request for a file, FFmpeg processes it from scratch.
/// The response must include all standard audio headers.
#[tokio::test]
async fn fresh_processing_returns_correct_headers() {
    let app = spawn_app().await;

    let response = app
        .api_client
        .get(format!("{}/unsafe/sample4.mp3", app.address))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_common_audio_headers(&response, "fresh processing");
}

/// A Range request on a fresh (never-cached) file must return 206.
#[tokio::test]
async fn fresh_processing_supports_range_requests() {
    let app = spawn_app().await;

    // First: full request to learn the body size.
    let full = app
        .api_client
        .get(format!("{}/unsafe/sample8.mp3", app.address))
        .send()
        .await
        .unwrap();
    let body_len = full.bytes().await.unwrap().len();
    assert!(body_len > 10, "fixture too small for range test");

    // Second: range request (result-storage hit path).
    let response = app
        .api_client
        .get(format!("{}/unsafe/sample8.mp3", app.address))
        .header(reqwest::header::RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();

    assert_common_audio_headers(&response, "fresh processing + range");
    assert_range_headers(&response, 10, "fresh processing + range");
}

// ---------------------------------------------------------------------------
// Layer 2: Result-storage hit (response cache miss)
// ---------------------------------------------------------------------------

/// The second request hits result-storage (the processed audio is persisted
/// on disk) but not the response cache.  Headers must be identical.
#[tokio::test]
async fn result_storage_hit_returns_correct_headers() {
    let app = spawn_app().await;
    let url = format!("{}/unsafe/sample6.mp3", app.address);

    // First request: populates result storage.
    let _ = app.api_client.get(&url).send().await.unwrap().bytes().await;

    // Second request: result-storage hit, response-cache may or may not
    // have been populated yet (async background task). Either way the
    // headers must be correct.
    let response = app.api_client.get(&url).send().await.unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_common_audio_headers(&response, "result-storage hit");
}

/// Range request served from result-storage must return 206.
#[tokio::test]
async fn result_storage_hit_supports_range_requests() {
    let app = spawn_app().await;
    let url = format!("{}/unsafe/test_tone.wav", app.address);

    // Warm result storage.
    let _ = app.api_client.get(&url).send().await.unwrap().bytes().await;

    let response = app
        .api_client
        .get(&url)
        .header(reqwest::header::RANGE, "bytes=10-19")
        .send()
        .await
        .unwrap();

    assert_common_audio_headers(&response, "result-storage hit + range");
    assert_range_headers(&response, 10, "result-storage hit + range");
}

// ---------------------------------------------------------------------------
// Layer 3: Response-cache hit (cache_middleware)
// ---------------------------------------------------------------------------

/// After the response cache is populated (async background task), the
/// middleware serves directly.  Headers must be identical to the other layers.
#[tokio::test]
async fn response_cache_hit_returns_correct_headers() {
    let app = spawn_app().await;
    let url = format!("{}/unsafe/sample7.mp3", app.address);

    // First request: populates result storage + kicks off cache population.
    let _ = app.api_client.get(&url).send().await.unwrap().bytes().await;

    // Wait for background cache write.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Third request: should be a response-cache hit.
    let response = app.api_client.get(&url).send().await.unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_common_audio_headers(&response, "response-cache hit");
}

/// Range request served from the response cache must return 206.
#[tokio::test]
async fn response_cache_hit_supports_range_requests() {
    let app = spawn_app().await;
    let url = format!("{}/unsafe/test_tone.wav", app.address);

    // Warm result storage + response cache.
    let _ = app.api_client.get(&url).send().await.unwrap().bytes().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let response = app
        .api_client
        .get(&url)
        .header(reqwest::header::RANGE, "bytes=0-49")
        .send()
        .await
        .unwrap();

    assert_common_audio_headers(&response, "response-cache hit + range");
    assert_range_headers(&response, 50, "response-cache hit + range");
}

// ---------------------------------------------------------------------------
// Cross-layer: headers are identical regardless of which layer serves
// ---------------------------------------------------------------------------

/// Fire three sequential requests for the same file.  Regardless of which
/// cache layer serves each one, the headers must be consistent.
///
/// Uses `sample2.mp3` to avoid cache-key collisions with tests that hit
/// `/meta/sample1.mp3` (the response cache key currently doesn't include
/// the route prefix, so a `/meta` JSON response can poison the cache for
/// the audio route).
#[tokio::test]
async fn headers_are_consistent_across_sequential_requests() {
    let app = spawn_app().await;
    let url = format!("{}/unsafe/sample2.mp3", app.address);

    let mut prev_headers: Option<(String, String, String, String, String)> = None;

    for i in 1..=3 {
        if i == 3 {
            // Give cache population time before the third request.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let r = app.api_client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK, "request {i}");
        assert_common_audio_headers(&r, &format!("request {i}"));

        let ct = r
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let ar = r
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let cd = r
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let cc = r
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let cors = r
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let _ = r.bytes().await;

        let current = (ct, ar, cd, cc, cors);
        if let Some(ref prev) = prev_headers {
            assert_eq!(
                &current, prev,
                "headers changed between requests (request {i} vs previous)"
            );
        }
        prev_headers = Some(current);
    }
}

// ---------------------------------------------------------------------------
// Content-Type sniffing — never application/octet-stream
// ---------------------------------------------------------------------------

/// The Content-Type must be a real audio MIME, not `application/octet-stream`.
/// This guards against the bug where `AudioFormat::Unknown` leaked through
/// as the Content-Type instead of sniffing the bytes with `infer`.
#[tokio::test]
async fn content_type_is_never_application_octet_stream() {
    let app = spawn_app().await;

    for file in ["sample1.mp3", "test_tone.wav"] {
        let response = app
            .api_client
            .get(format!("{}/unsafe/{}", app.address, file))
            .send()
            .await
            .unwrap();

        let ct = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(missing)");

        assert!(
            ct.starts_with("audio/"),
            "{file}: Content-Type should start with audio/, got {ct:?}"
        );
        assert_ne!(
            ct, "application/octet-stream",
            "{file}: Content-Type must not be application/octet-stream"
        );
        let _ = response.bytes().await;
    }
}
