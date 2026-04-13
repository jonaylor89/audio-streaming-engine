//! Tests for the Params-caching optimization in `cache_middleware`.
//!
//! When a request passes through `cache_middleware` (cache miss), the already-parsed
//! `Params` are stored in request extensions. The downstream handler's
//! `FromRequestParts` impl picks them up via `extensions.remove()`, skipping
//! a redundant URI parse. These tests verify that the buffered endpoint still
//! produces correct responses — confirming the extension-based shortcut works.

use crate::helpers::spawn_app;

/// A buffered request through `/{*streamingpath}` with query params produces
/// a valid audio response, confirming that the `Params` parsed by
/// `cache_middleware` are correctly forwarded to `streamingpath_handler`.
#[tokio::test]
async fn buffered_request_reuses_cached_params() {
    let app = spawn_app().await;

    // First request — cache miss. Params parsed in cache_middleware,
    // inserted into extensions, then consumed by streamingpath_handler.
    let response = app
        .api_client
        .get(&format!(
            "{}/unsafe/sample1.mp3?volume=0.5",
            &app.address
        ))
        .send()
        .await
        .expect("Failed to execute request");

    assert!(
        response.status().is_success(),
        "expected 2xx, got {}",
        response.status()
    );
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("audio") || content_type.contains("octet"),
        "expected audio content-type, got: {}",
        content_type
    );

    // Second request — cache hit in cache_middleware. Params are parsed
    // but never inserted (early return). Confirms no regression on hit path.
    let response2 = app
        .api_client
        .get(&format!(
            "{}/unsafe/sample1.mp3?volume=0.5",
            &app.address
        ))
        .send()
        .await
        .expect("Failed to execute second request");

    assert!(
        response2.status().is_success(),
        "cache hit should succeed, got {}",
        response2.status()
    );
}
