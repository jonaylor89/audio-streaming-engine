use axum::{
    body::Body,
    extract::State,
    http::{Response, header},
    response::IntoResponse,
};
use bytes::Bytes;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use futures::{Stream, StreamExt};
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::{
    blob::{AudioBuffer, AudioFormat},
    processor::ffmpeg::is_passthrough_for_format,
    state::AppStateDyn,
    storage::AudioStorage,
    streamingpath::{hasher::suffix_result_storage_hasher, params::Params},
    utils::{AppError, e404, e413, e500},
};

/// Experimental streaming endpoint: processes audio via FFmpeg and streams chunks
/// directly to the client via Transfer-Encoding: chunked.
///
/// Limitations:
/// - WAV and M4A output formats may produce malformed output (seek-during-write).
/// - `areverse` filter and two-pass `loudnorm` will not reduce TTFB.
/// - No `Content-Length` or range support on first request.
/// - Mid-stream FFmpeg errors produce a truncated response.
#[instrument(skip(state))]
pub async fn stream_handler(
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

    // Result storage MISS: detect format from key extension for passthrough check
    let is_remote = params.key.starts_with("https://") || params.key.starts_with("http://");
    let source_format = params
        .key
        .rsplit('.')
        .next()
        .map(AudioFormat::from_extension)
        .unwrap_or(AudioFormat::Unknown);

    // Streaming passthrough: stream directly from storage to HTTP response
    // when no processing is needed and we can determine the format from the key.
    if !is_remote
        && source_format != AudioFormat::Unknown
        && is_passthrough_for_format(source_format, &params)
    {
        info!(key = %params.key, "streaming passthrough — zero-buffer path");
        let mime = source_format.mime_type();

        let storage_stream = state.storage.get_stream(&params.key).await.map_err(|e| {
            tracing::error!("Failed to stream audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?;

        let http_stream = tee_to_result_storage(
            storage_stream,
            state.result_storage.clone(),
            params_hash,
            source_format,
        );

        return Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from_stream(http_stream))
            .map_err(|e| e500(eyre!("Failed to build response: {}", e)));
    }

    // Non-passthrough: fetch entire source into memory (FFmpeg needs seekable input)
    let blob = if is_remote {
        state
            .remote
            .fetch(&params.key, state.max_source_size)
            .await?
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

    let http_stream = tee_to_result_storage(
        stream,
        state.result_storage.clone(),
        params_hash,
        output_format,
    );

    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from_stream(http_stream))
        .map_err(|e| e500(eyre!("Failed to build response: {}", e)))
}

/// Forward `source` to the HTTP response while collecting it, and write the
/// collected bytes to result storage once the source ends cleanly.
///
/// Nothing is stored if the source yields an error (e.g. FFmpeg failed
/// mid-stream) or the client disconnects, so a truncated body never becomes
/// the permanent result for this hash.
fn tee_to_result_storage(
    source: impl Stream<Item = Result<Bytes>> + Send + 'static,
    storage: Arc<dyn AudioStorage>,
    hash: String,
    format: AudioFormat,
) -> impl Stream<Item = std::io::Result<Bytes>> {
    let (http_tx, http_rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(8);

    tokio::spawn(async move {
        futures::pin_mut!(source);
        let mut collected = bytes::BytesMut::new();
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(bytes) => {
                    collected.extend_from_slice(&bytes);
                    if http_tx.send(Ok(bytes)).await.is_err() {
                        // Client disconnected; dropping `source` stops FFmpeg.
                        return;
                    }
                }
                Err(e) => {
                    let _ = http_tx.send(Err(e)).await;
                    return;
                }
            }
        }
        drop(http_tx);

        if !collected.is_empty() {
            let audio_buf = AudioBuffer::from_bytes_with_format(collected.freeze(), format);
            if let Err(e) = storage.put(&hash, &audio_buf).await {
                warn!("Failed to save result audio [{}]: {}", &hash, e);
            }
        }
    });

    futures::stream::unfold(http_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (item.map_err(|e| std::io::Error::other(e.to_string())), rx))
    })
}
