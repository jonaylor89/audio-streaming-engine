use axum::{Extension, Json, extract::State};
use bytes::Bytes;
use color_eyre::eyre::eyre;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, instrument};
use utoipa::ToSchema;

use crate::{
    middleware::CacheMissContext,
    remote::fetch_audio_buffer,
    state::AppStateDyn,
    streamingpath::params::Params,
    utils::{AppError, e404, e500},
};

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct AudioMetadata {
    pub format: String,
    pub duration: Option<f64>,
    pub bit_rate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub channels: Option<i64>,
    pub codec: Option<String>,
    pub size: Option<i64>,
    pub tags: HashMap<String, String>,
}

#[instrument(skip(state, cache_miss))]
pub async fn meta_handler(
    State(state): State<AppStateDyn>,
    cache_miss: Option<Extension<CacheMissContext>>,
    params: Params,
) -> Result<Json<AudioMetadata>, AppError> {
    info!("meta: {:?}", params);

    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

    let processed_blob = state.processor.process(&blob, &params).await.map_err(|e| {
        tracing::error!("Failed to process audio with params {:?}: {}", params, e);
        e500(eyre!("Failed to process audio: {}", e))
    })?;

    let audio_data = processed_blob.as_ref().to_vec();
    let file_meta = tokio::task::spawn_blocking(move || ffmpeg::extract_metadata(&audio_data))
        .await
        .map_err(|e| {
            tracing::error!("Metadata extraction task panicked: {}", e);
            e500(eyre!("Metadata extraction task failed: {}", e))
        })?
        .map_err(|e| {
            tracing::error!("Failed to extract metadata: {}", e);
            e500(eyre!("Failed to extract metadata: {}", e))
        })?;

    let metadata = AudioMetadata {
        format: file_meta.format,
        duration: file_meta.duration,
        bit_rate: file_meta.bit_rate,
        sample_rate: file_meta.sample_rate.map(|v| v as i64),
        channels: file_meta.channels.map(|v| v as i64),
        codec: file_meta.codec,
        size: file_meta.size,
        tags: file_meta.tags,
    };

    // Populate response cache in background
    if let Some(Extension(ctx)) = cache_miss
        && let Ok(json_bytes) = serde_json::to_vec(&metadata)
    {
        ctx.populate(Bytes::from(json_bytes));
    }

    Ok(Json(metadata))
}
