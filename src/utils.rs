use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use color_eyre::eyre;

#[derive(Debug)]
pub struct AppError {
    error: eyre::Error,
    status: StatusCode,
}

impl AppError {
    pub fn new(error: eyre::Error, status: StatusCode) -> Self {
        Self { error, status }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("{:?}", self.error);
        let body = serde_json::json!({
            "error": self.error.to_string(),
        });
        (self.status, Json(body)).into_response()
    }
}

pub fn e500<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::INTERNAL_SERVER_ERROR)
}

pub fn e400<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::BAD_REQUEST)
}

pub fn e401<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::UNAUTHORIZED)
}

pub fn e404<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::NOT_FOUND)
}

pub fn e413<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::PAYLOAD_TOO_LARGE)
}

pub fn e416<T>(e: T) -> AppError
where
    T: Into<eyre::Error>,
{
    AppError::new(e.into(), StatusCode::RANGE_NOT_SATISFIABLE)
}
