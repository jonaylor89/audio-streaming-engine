use axum::{
    body::Body,
    extract::State,
    http::{Response, header},
    response::IntoResponse,
};
use color_eyre::eyre::eyre;
use tracing::{info, instrument, warn};

use crate::{
    remote::fetch_audio_buffer,
    state::AppStateDyn,
    streamingpath::{hasher::suffix_result_storage_hasher, params::Params},
    utils::{AppError, e404, e500},
};

#[instrument(skip(state))]
pub async fn streamingpath_handler(
    State(state): State<AppStateDyn>,
    params: Params,
) -> Result<impl IntoResponse, AppError> {
    let params_hash = suffix_result_storage_hasher(&params);
    let result = state
        .result_storage
        .get(&params_hash)
        .await
        .inspect_err(|_| {
            info!("no audio in results storage: {}", &params);
        });

    // Result storage HIT: serve with Content-Length + range support
    if let Ok(blob) = result {
        return Response::builder()
            .header(header::CONTENT_TYPE, blob.mime_type())
            .header(header::CONTENT_LENGTH, blob.len().to_string())
            .body(Body::from(blob.into_bytes()))
            .map_err(|e| e500(eyre!("Failed to build response: {}", e)));
    }

    // Result storage MISS: fetch source audio
    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

    // Process audio (buffered)
    let processed = state.processor.process(&blob, &params).await.map_err(|e| {
        tracing::error!("Failed to process audio: {}", e);
        e500(eyre!("Failed to process audio: {}", e))
    })?;

    // Store result in background so we don't block the response
    let bg_storage = state.result_storage.clone();
    let bg_hash = params_hash;
    let bg_processed = processed.clone();
    tokio::spawn(async move {
        if let Err(e) = bg_storage.put(&bg_hash, &bg_processed).await {
            warn!("Failed to save result audio [{}]: {}", &bg_hash, e);
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, processed.mime_type())
        .header(header::CONTENT_LENGTH, processed.len().to_string())
        .body(Body::from(processed.into_bytes()))
        .map_err(|e| e500(eyre!("Failed to build response: {}", e)))
}
