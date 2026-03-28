use axum::{Json, extract::State};
use color_eyre::eyre::eyre;
use serde::Serialize;
use tracing::instrument;

use crate::state::AppStateDyn;
use crate::utils::{AppError, e500};

#[derive(Serialize)]
pub struct ListResponse {
    pub keys: Vec<String>,
}

#[instrument(skip(state))]
pub async fn list_handler(
    State(state): State<AppStateDyn>,
) -> Result<Json<ListResponse>, AppError> {
    let keys = state.storage.list().await.map_err(|e| {
        tracing::error!("Failed to list audio files: {}", e);
        e500(eyre!("Failed to list audio files: {}", e))
    })?;

    Ok(Json(ListResponse { keys }))
}
