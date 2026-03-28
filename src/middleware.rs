use crate::state::AppStateDyn;
use crate::streamingpath::hasher::{suffix_result_storage_hasher, verify_hash};
use crate::streamingpath::params::Params;
use crate::utils::{AppError, e400, e416, e500};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    middleware::Next,
    response::IntoResponse,
};
use color_eyre::eyre::eyre;
use std::time::Duration;
use tracing::debug;

const CACHE_KEY_PREFIX: &str = "req_cache:";
const META_CACHE_KEY_PREFIX: &str = "meta_cache:";
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour
/// Maximum response body size we will buffer for caching (256 MB).
const MAX_CACHEABLE_BODY_SIZE: usize = 256 * 1024 * 1024;

#[tracing::instrument(skip(state, req, next))]
pub async fn cache_middleware(
    State(state): State<AppStateDyn>,
    params: Params,
    mut req: Request,
    next: Next,
) -> Result<impl IntoResponse, AppError> {
    let params_hash = suffix_result_storage_hasher(&params);
    let request_headers = req.headers().clone();

    let uri_path = req.uri().path();
    let cache_key_prefix = if uri_path.starts_with("/meta") {
        META_CACHE_KEY_PREFIX
    } else {
        CACHE_KEY_PREFIX
    };

    let cache_key = format!("{}:{}:{}", cache_key_prefix, req.method(), params_hash);

    debug!("Cache key: {}", cache_key);
    let cache_response = state
        .cache
        .get(&cache_key)
        .await
        .map_err(|e| e500(eyre!("Failed to get cache: {}", e)))?;
    if let Some(buf) = cache_response {
        let content_type = infer::get(&buf)
            .map(|mime| mime.to_string())
            .unwrap_or("audio/mpeg".to_string());
        debug!("Cache hit key={}", cache_key);
        return build_audio_response(&request_headers, &content_type, Bytes::from(buf));
    }

    // Cache the full response body, then apply range handling locally so first
    // requests and cache hits behave the same way.
    req.headers_mut().remove(header::RANGE);
    let response = next.run(req).await;
    if response.status() != StatusCode::OK {
        return Ok(response);
    }

    // Buffer response body so we can cache it and handle range requests
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, MAX_CACHEABLE_BODY_SIZE)
        .await
        .map_err(|e| e500(eyre!("Failed to read response body: {}", e)))?;

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| infer::get(bytes.as_ref()).map(|mime| mime.to_string()))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // Write to cache in background — don't block the response
    let bg_cache = state.cache.clone();
    let bg_bytes = bytes.clone();
    tokio::spawn(async move {
        let _ = bg_cache
            .set(&cache_key, bg_bytes.as_ref(), Some(CACHE_TTL))
            .await;
    });

    build_audio_response(&request_headers, &content_type, bytes)
}

pub async fn auth_middleware(
    State(state): State<AppStateDyn>,
    params: Params,
    req: Request,
    next: Next,
) -> Result<impl IntoResponse, AppError> {
    let path = params.to_string();

    let hash = req
        .uri()
        .path()
        .trim_start_matches("/meta")
        .strip_prefix("/")
        .and_then(|s| s.split("/").next())
        .ok_or_else(|| e400(eyre!("Failed to parse URI hash")))?;

    if hash != "unsafe" {
        verify_hash(
            hash.to_owned().into(),
            path.to_owned().into(),
            &state.hmac_secret,
        )
        .map_err(|e| e400(eyre!("Failed to verify hash: {}", e)))?;
    }

    Ok(next.run(req).await)
}

fn build_audio_response(
    request_headers: &HeaderMap,
    content_type: &str,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    match parse_range(request_headers, body.len())? {
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
                .header(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_static("inline"),
                )
                .body(Body::from(content))
                .map_err(e500)
        }
        None => Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, body.len().to_string())
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("inline"),
            )
            .body(Body::from(body))
            .map_err(e500),
    }
}

fn parse_range(
    request_headers: &HeaderMap,
    total_size: usize,
) -> Result<Option<(usize, usize)>, AppError> {
    let Some(range) = request_headers.get(header::RANGE) else {
        return Ok(None);
    };

    let range = range
        .to_str()
        .map_err(|_| e400(eyre!("Range header must be valid ASCII")))?;

    let range = range
        .strip_prefix("bytes=")
        .ok_or_else(|| e416(eyre!("Only byte ranges are supported")))?;

    if total_size == 0 || range.contains(',') {
        return Err(e416(eyre!("Requested range cannot be satisfied")));
    }

    let Some((start, end)) = range.split_once('-') else {
        return Err(e416(eyre!("Invalid Range header")));
    };

    if start.is_empty() {
        let suffix_len = end
            .parse::<usize>()
            .map_err(|_| e416(eyre!("Invalid suffix byte range")))?;

        if suffix_len == 0 {
            return Err(e416(eyre!("Requested range cannot be satisfied")));
        }

        let start = total_size.saturating_sub(suffix_len);
        return Ok(Some((start, total_size - 1)));
    }

    let start = start
        .parse::<usize>()
        .map_err(|_| e416(eyre!("Invalid byte range start")))?;

    if start >= total_size {
        return Err(e416(eyre!("Requested range cannot be satisfied")));
    }

    let end = if end.is_empty() {
        total_size - 1
    } else {
        end.parse::<usize>()
            .map_err(|_| e416(eyre!("Invalid byte range end")))?
    };

    if end < start {
        return Err(e416(eyre!("Requested range cannot be satisfied")));
    }

    Ok(Some((start, end.min(total_size - 1))))
}
