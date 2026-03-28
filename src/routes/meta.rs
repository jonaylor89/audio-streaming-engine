use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, instrument};
use utoipa::ToSchema;

use crate::{remote::fetch_audio_buffer, state::AppStateDyn, streamingpath::params::Params};

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

#[instrument(skip(state))]
pub async fn meta_handler(
    State(state): State<AppStateDyn>,
    params: Params,
) -> Result<Json<AudioMetadata>, (StatusCode, String)> {
    info!("meta: {:?}", params);

    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            (
                StatusCode::NOT_FOUND,
                format!("Failed to fetch audio: {}", e),
            )
        })?
    };

    let processed_blob = state.processor.process(&blob, &params).await.map_err(|e| {
        tracing::error!("Failed to process audio with params {:?}: {}", params, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to process audio: {}", e),
        )
    })?;

    let audio_data = processed_blob.as_ref().to_vec();
    let file_meta = tokio::task::spawn_blocking(move || ffmpeg::extract_metadata(&audio_data))
        .await
        .map_err(|e| {
            tracing::error!("Metadata extraction task panicked: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Metadata extraction task failed: {}", e),
            )
        })?
        .map_err(|e| {
            tracing::error!("Failed to extract metadata: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to extract metadata: {}", e),
            )
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

    Ok(Json(metadata))
}
