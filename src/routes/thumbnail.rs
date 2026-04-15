use axum::{
    Extension,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::IntoResponse,
};
use color_eyre::eyre::eyre;
use serde::Deserialize;
use tracing::{info, instrument};

use crate::{
    middleware::{CacheMissContext, build_audio_response},
    remote::fetch_audio_buffer,
    state::AppStateDyn,
    streamingpath::{hasher::suffix_result_storage_hasher, params::Params},
    thumbnail::{ThumbnailConfig, ThumbnailResult, analyze},
    utils::{AppError, e404, e413, e500},
};

#[derive(Debug, Deserialize)]
pub struct ThumbnailQuery {
    pub thumbnail_duration: Option<f64>,
    pub thumbnail_min_duration: Option<f64>,
    pub thumbnail_max_duration: Option<f64>,
}

/// Detect MIME type from the raw audio bytes using magic-byte sniffing,
/// matching the same approach used by `cache_middleware` on cache hits.
fn sniff_content_type(buf: &[u8]) -> String {
    infer::get(buf)
        .map(|t| t.to_string())
        .unwrap_or_else(|| "audio/mpeg".to_string())
}

#[instrument(skip(state, headers, cache_miss))]
pub async fn thumbnail_handler(
    State(state): State<AppStateDyn>,
    headers: HeaderMap,
    cache_miss: Option<Extension<CacheMissContext>>,
    Query(query): Query<ThumbnailQuery>,
    params: Params,
) -> Result<impl IntoResponse, AppError> {
    let thumbnail_config = ThumbnailConfig {
        target_duration: query.thumbnail_duration.unwrap_or(30.0),
        min_duration: query.thumbnail_min_duration.unwrap_or(15.0),
        max_duration: query.thumbnail_max_duration.unwrap_or(45.0),
    };

    // Check result storage first (thumbnail-specific hash)
    let thumb_hash = format!("{}_thumb", suffix_result_storage_hasher(&params));
    let result = state
        .result_storage
        .get(&thumb_hash)
        .await
        .inspect_err(|_| {
            info!("no thumbnail in results storage: {}", &params);
        });

    let cache_miss_ctx = cache_miss.map(|Extension(ctx)| ctx);

    if let Ok(blob) = result {
        let body = blob.into_bytes();
        if let Some(ctx) = cache_miss_ctx {
            ctx.populate(body.clone());
        }
        return build_audio_response(&headers, &sniff_content_type(&body), body);
    }

    // Fetch source audio
    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key, state.max_source_size).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

    if blob.len() > state.max_source_size {
        return Err(e413(eyre!(
            "Source audio too large: {} bytes (max {})",
            blob.len(),
            state.max_source_size
        )));
    }

    // Decode to PCM for analysis (spawn_blocking — CPU-heavy)
    let pcm_bytes = blob.clone().into_bytes();
    let pcm_data = tokio::task::spawn_blocking(move || ffmpeg::decode_to_pcm(pcm_bytes))
        .await
        .map_err(|e| {
            tracing::error!("PCM decode task panicked: {}", e);
            e500(eyre!("PCM decode failed: {}", e))
        })?
        .map_err(|e| {
            tracing::error!("Failed to decode audio to PCM: {}", e);
            e500(eyre!("Failed to decode audio: {}", e))
        })?;

    // Run thumbnail analysis (spawn_blocking — CPU-heavy)
    let config = thumbnail_config;
    let thumbnail_result: ThumbnailResult = tokio::task::spawn_blocking(move || {
        analyze(&pcm_data.samples, pcm_data.sample_rate, &config)
    })
    .await
    .map_err(|e| {
        tracing::error!("Thumbnail analysis task panicked: {}", e);
        e500(eyre!("Thumbnail analysis failed: {}", e))
    })?
    .map_err(|e| {
        tracing::error!("Thumbnail analysis failed: {}", e);
        e500(eyre!("Thumbnail analysis failed: {}", e))
    })?;

    info!(
        start_time = thumbnail_result.start_time,
        duration = thumbnail_result.duration,
        confidence = thumbnail_result.confidence,
        "Thumbnail segment identified"
    );

    // Build modified params with the computed start_time and duration
    let mut thumb_params = params.clone();
    thumb_params.start_time = Some(thumbnail_result.start_time);
    thumb_params.duration = Some(thumbnail_result.duration);

    // Process audio using existing pipeline
    let processed = state
        .processor
        .process(&blob, &thumb_params)
        .await
        .map_err(|e| {
            tracing::error!("Failed to process thumbnail audio: {}", e);
            e500(eyre!("Failed to process audio: {}", e))
        })?;

    // Store result in background
    let bg_storage = state.result_storage.clone();
    let bg_hash = thumb_hash;
    let bg_processed = processed.clone();
    tokio::spawn(async move {
        if let Err(e) = bg_storage.put(&bg_hash, &bg_processed).await {
            tracing::warn!("Failed to save thumbnail result [{}]: {}", &bg_hash, e);
        }
    });

    // Populate response cache in background
    if let Some(ctx) = cache_miss_ctx {
        ctx.populate(processed.clone().into_bytes());
    }

    let canonical = format!(
        "/unsafe/{}?start_time={:.1}&duration={:.1}",
        thumb_params.key, thumbnail_result.start_time, thumbnail_result.duration
    );

    // Build range-aware response, then append thumbnail-specific headers.
    let body = processed.into_bytes();
    let content_type = sniff_content_type(&body);
    let mut response = build_audio_response(&headers, &content_type, body)?;
    let h = response.headers_mut();
    h.insert(
        "X-Thumbnail-Confidence",
        format!("{:.2}", thumbnail_result.confidence)
            .parse()
            .unwrap(),
    );
    h.insert(
        "X-Thumbnail-Start",
        format!("{:.1}", thumbnail_result.start_time)
            .parse()
            .unwrap(),
    );
    h.insert(
        "X-Thumbnail-Duration",
        format!("{:.1}", thumbnail_result.duration).parse().unwrap(),
    );
    h.insert(
        header::LINK,
        format!("<{}>; rel=canonical", canonical).parse().unwrap(),
    );
    Ok(response)
}
