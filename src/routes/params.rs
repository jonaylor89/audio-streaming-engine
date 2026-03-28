use axum::Json;
use tracing::info;

use crate::streamingpath::params::Params;
use crate::utils::AppError;

#[tracing::instrument]
pub async fn params(params: Params) -> Result<Json<Params>, AppError> {
    info!("params: {:?}", params);

    Ok(Json(params))
}
