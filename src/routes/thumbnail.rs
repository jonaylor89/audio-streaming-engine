use axum::{
    body::Body,
    extract::{Query, State},
    http::{Response, header},
    response::IntoResponse,
};
use color_eyre::eyre::eyre;
use serde::Deserialize;
use tracing::{info, instrument};

use crate::{
    remote::fetch_audio_buffer,
    state::AppStateDyn,
    streamingpath::{hasher::suffix_result_storage_hasher, params::Params},
    thumbnail::{ThumbnailConfig, ThumbnailResult, analyze},
    utils::{AppError, e404, e500},
};

#[derive(Debug, Deserialize)]
pub struct ThumbnailQuery {
    pub thumbnail_duration: Option<f64>,
    pub thumbnail_min_duration: Option<f64>,
    pub thumbnail_max_duration: Option<f64>,
}

#[instrument(skip(state))]
pub async fn thumbnail_handler(
    State(state): State<AppStateDyn>,
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

    if let Ok(blob) = result {
        return Response::builder()
            .header(header::CONTENT_TYPE, blob.mime_type())
            .header(header::CONTENT_LENGTH, blob.len().to_string())
            .body(Body::from(blob.into_bytes()))
            .map_err(|e| e500(eyre!("Failed to build response: {}", e)));
    }

    // Fetch source audio
    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

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

    let canonical = format!(
        "/unsafe/{}?start_time={:.1}&duration={:.1}",
        thumb_params.key, thumbnail_result.start_time, thumbnail_result.duration
    );

    Response::builder()
        .header(header::CONTENT_TYPE, processed.mime_type())
        .header(header::CONTENT_LENGTH, processed.len().to_string())
        .header(
            "X-Thumbnail-Confidence",
            format!("{:.2}", thumbnail_result.confidence),
        )
        .header(
            "X-Thumbnail-Start",
            format!("{:.1}", thumbnail_result.start_time),
        )
        .header(
            "X-Thumbnail-Duration",
            format!("{:.1}", thumbnail_result.duration),
        )
        .header("Link", format!("<{}>; rel=canonical", canonical))
        .body(Body::from(processed.into_bytes()))
        .map_err(|e| e500(eyre!("Failed to build response: {}", e)))
}
