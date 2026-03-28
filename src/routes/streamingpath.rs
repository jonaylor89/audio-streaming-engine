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
    let result = state.storage.get(&params_hash).await.inspect_err(|_| {
        info!("no audio in results storage: {}", &params);
    });
    if let Ok(blob) = result {
        return Response::builder()
            .header(header::CONTENT_TYPE, blob.mime_type())
            .body(Body::from(blob.into_bytes()))
            .map_err(|e| e500(eyre!("Failed to build response: {}", e)));
    }

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

    let mime = processed_blob.mime_type().to_owned();
    let result_bytes = processed_blob.into_bytes();

    // Store result in background — don't block the response
    let bg_storage = state.storage.clone();
    let bg_hash = params_hash;
    let bg_bytes = result_bytes.clone();
    let bg_format = crate::blob::AudioBuffer::from_bytes(bg_bytes);
    tokio::spawn(async move {
        if let Err(e) = bg_storage.put(&bg_hash, &bg_format).await {
            warn!("Failed to save result audio [{}]: {}", &bg_hash, e);
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(result_bytes))
        .map_err(|e| e500(eyre!("Failed to build response: {}", e)))
}
