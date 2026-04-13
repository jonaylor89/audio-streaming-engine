use crate::cache::AudioCache;
use crate::state::AppStateDyn;
use crate::streamingpath::hasher::{suffix_result_storage_hasher, verify_hash};
use crate::streamingpath::params::Params;
use crate::streamingpath::strip_route_prefix;
use crate::utils::{AppError, e400, e416, e500};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    middleware::Next,
    response::IntoResponse,
};
use color_eyre::eyre::eyre;
use std::sync::Arc;
use tracing::{debug, warn};

const CACHE_KEY_PREFIX: &str = "req_cache:";
const META_CACHE_KEY_PREFIX: &str = "meta_cache:";
const THUMB_CACHE_KEY_PREFIX: &str = "thumb_cache:";

/// Inserted into request extensions on a cache miss so downstream handlers can
/// populate the response cache without the middleware needing to buffer the body.
#[derive(Clone)]
pub struct CacheMissContext {
    pub cache: Arc<dyn AudioCache>,
    pub cache_key: String,
}

impl CacheMissContext {
    /// Write bytes to the response cache in a background task.
    pub fn populate(self, data: Bytes) {
        tokio::spawn(async move {
            if let Err(e) = self.cache.set(&self.cache_key, data, None).await {
                warn!(
                    "Failed to write response to cache [{}]: {}",
                    &self.cache_key, e
                );
            }
        });
    }
}

#[tracing::instrument(skip(state, req, next))]
pub async fn cache_middleware(
    State(state): State<AppStateDyn>,
    params: Params,
    mut req: Request,
    next: Next,
) -> Result<impl IntoResponse, AppError> {
    let params_hash = suffix_result_storage_hasher(&params);

    let uri_path = req.uri().path();
    let cache_key_prefix = if uri_path.starts_with("/meta") {
        META_CACHE_KEY_PREFIX
    } else if uri_path.starts_with("/thumbnail") {
        THUMB_CACHE_KEY_PREFIX
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
        let request_headers = req.headers().clone();
        let content_type = infer::get(&buf)
            .map(|mime| mime.to_string())
            .unwrap_or("audio/mpeg".to_string());
        debug!("Cache hit key={}", cache_key);
        return build_audio_response(&request_headers, &content_type, buf);
    }

    // Cache MISS: pass through to the handler. The handler will populate the
    // cache via CacheMissContext — no body buffering needed in the middleware.
    req.extensions_mut().insert(CacheMissContext {
        cache: state.cache.clone(),
        cache_key,
    });

    Ok(next.run(req).await)
}

pub async fn auth_middleware(
    State(state): State<AppStateDyn>,
    params: Params,
    req: Request,
    next: Next,
) -> Result<impl IntoResponse, AppError> {
    let path = params.to_string();

    let hash = strip_route_prefix(req.uri().path())
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

pub(crate) fn build_audio_response(
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
