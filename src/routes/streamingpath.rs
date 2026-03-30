use axum::{
    body::Body,
    extract::State,
    http::{Response, header},
    response::IntoResponse,
};
use bytes::Bytes;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use futures::StreamExt;
use tracing::{info, instrument, warn};

use crate::{
    blob::AudioBuffer,
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

    // Cache HIT: serve the stored result directly (includes Content-Length + range support)
    if let Ok(blob) = result {
        return Response::builder()
            .header(header::CONTENT_TYPE, blob.mime_type())
            .header(header::CONTENT_LENGTH, blob.len().to_string())
            .body(Body::from(blob.into_bytes()))
            .map_err(|e| e500(eyre!("Failed to build response: {}", e)));
    }

    // Cache MISS: fetch source audio
    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

    // Determine output MIME type before starting the stream
    let output_format = params.format.unwrap_or(blob.format());
    let mime = output_format.mime_type();

    // Start streaming FFmpeg pipeline
    let stream = state
        .processor
        .process_streaming(&blob, &params)
        .await
        .map_err(|e| {
            tracing::error!("Failed to start audio processing: {}", e);
            e500(eyre!("Failed to process audio: {}", e))
        })?;

    // Tee: forward each chunk to the HTTP response AND collect for storage.
    // Two channels; a tee task pulls from the FFmpeg stream and fans out.
    let (http_tx, http_rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(8);
    let (store_tx, store_rx) = tokio::sync::mpsc::channel::<Bytes>(8);

    // Tee task: drives the FFmpeg stream and fans out to HTTP + storage channels
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    // If storage collector disconnected, just continue streaming to HTTP
                    let _ = store_tx.send(bytes.clone()).await;
                    if http_tx.send(Ok(bytes)).await.is_err() {
                        // HTTP client disconnected — stop processing
                        break;
                    }
                }
                Err(e) => {
                    // Propagate error to HTTP consumer; abandon storage write
                    let _ = http_tx.send(Err(e)).await;
                    break;
                }
            }
        }
        // Dropping store_tx signals the storage collector that all chunks are sent
    });

    // Storage collector: accumulates all chunks and calls storage.put when done
    let bg_storage = state.storage.clone();
    let bg_hash = params_hash;
    let bg_format = output_format;
    tokio::spawn(async move {
        let mut chunks: Vec<Bytes> = Vec::new();
        let mut rx = store_rx;
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        // All chunks received (store_tx was dropped) — assemble and persist
        if !chunks.is_empty() {
            let mut total = bytes::BytesMut::new();
            for chunk in chunks {
                total.extend_from_slice(&chunk);
            }
            let audio_buf = AudioBuffer::from_bytes_with_format(total.freeze(), bg_format);
            if let Err(e) = bg_storage.put(&bg_hash, &audio_buf).await {
                warn!("Failed to save result audio [{}]: {}", &bg_hash, e);
            }
        }
    });

    // Build chunked HTTP response from the tee'd HTTP channel
    let http_stream = futures::stream::unfold(http_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (item.map_err(|e| std::io::Error::other(e.to_string())), rx))
    });

    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from_stream(http_stream))
        .map_err(|e| e500(eyre!("Failed to build response: {}", e)))
}
