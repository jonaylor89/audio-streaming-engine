use axum::{Extension, extract::State, http::HeaderMap, response::IntoResponse};
use color_eyre::eyre::eyre;
use tracing::{info, instrument, warn};

use crate::{
    blob::AudioBuffer,
    inflight::CoalesceResult,
    middleware::{CacheMissContext, build_audio_response},
    remote::fetch_audio_buffer,
    state::AppStateDyn,
    streamingpath::{hasher::suffix_result_storage_hasher, params::Params},
    utils::{AppError, e404, e500},
};

#[instrument(skip(state, headers, cache_miss))]
pub async fn streamingpath_handler(
    State(state): State<AppStateDyn>,
    headers: HeaderMap,
    cache_miss: Option<Extension<CacheMissContext>>,
    params: Params,
) -> Result<impl IntoResponse, AppError> {
    let cache_miss_ctx = cache_miss.map(|Extension(ctx)| ctx);
    let params_hash = suffix_result_storage_hasher(&params);
    let result = state
        .result_storage
        .get(&params_hash)
        .await
        .inspect_err(|_| {
            info!("no audio in results storage: {}", &params);
        });

    // Result storage HIT
    if let Ok(blob) = result {
        let body = blob.into_bytes();
        if let Some(ctx) = cache_miss_ctx {
            ctx.populate(body.clone());
        }
        return build_audio_response(&headers, &sniff_content_type(&body), body);
    }

    // Result storage MISS: attempt to coalesce with any in-flight request
    match state.inflight.try_join(&params_hash).await {
        CoalesceResult::Leader(guard) => {
            match fetch_and_process(&state, &params).await {
                Ok(processed) => {
                    // Write to result_storage BEFORE notifying waiters
                    let bg_storage = state.result_storage.clone();
                    let bg_hash = params_hash;
                    let bg_processed = processed.clone();
                    if let Err(e) = bg_storage.put(&bg_hash, &bg_processed).await {
                        warn!("Failed to save result audio [{}]: {}", &bg_hash, e);
                        guard.fail(format!("result storage write failed: {}", e));
                        return Err(e500(eyre!("Failed to save result audio: {}", e)));
                    }

                    guard.complete();

                    let body = processed.into_bytes();
                    if let Some(ctx) = cache_miss_ctx {
                        ctx.populate(body.clone());
                    }
                    build_audio_response(&headers, &sniff_content_type(&body), body)
                }
                Err(e) => {
                    guard.fail(format!("{:?}", e));
                    Err(e)
                }
            }
        }
        CoalesceResult::Waiter(result) => {
            result.map_err(|e| e500(eyre!("coalesced request failed: {}", e)))?;
            let blob =
                state.result_storage.get(&params_hash).await.map_err(|e| {
                    e500(eyre!("failed to read coalesced result from storage: {}", e))
                })?;
            let body = blob.into_bytes();
            if let Some(ctx) = cache_miss_ctx {
                ctx.populate(body.clone());
            }
            build_audio_response(&headers, &sniff_content_type(&body), body)
        }
        CoalesceResult::Failed(err) => Err(e500(eyre!("request failed (cached): {}", err))),
    }
}

/// Detect MIME type from the raw audio bytes using magic-byte sniffing,
/// matching the same approach used by `cache_middleware` on cache hits.
fn sniff_content_type(buf: &[u8]) -> String {
    infer::get(buf)
        .map(|t| t.to_string())
        .unwrap_or_else(|| "audio/mpeg".to_string())
}

async fn fetch_and_process(state: &AppStateDyn, params: &Params) -> Result<AudioBuffer, AppError> {
    let blob = if params.key.starts_with("https://") || params.key.starts_with("http://") {
        fetch_audio_buffer(&state.http_client, &params.key).await?
    } else {
        state.storage.get(&params.key).await.map_err(|e| {
            tracing::error!("Failed to fetch audio from storage {}: {}", params.key, e);
            e404(eyre!("Failed to fetch audio: {}", e))
        })?
    };

    state.processor.process(&blob, params).await.map_err(|e| {
        tracing::error!("Failed to process audio: {}", e);
        e500(eyre!("Failed to process audio: {}", e))
    })
}
