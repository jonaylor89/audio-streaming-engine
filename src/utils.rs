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

    pub fn status(&self) -> StatusCode {
        self.status
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

/// Detect MIME type from raw audio bytes using magic-byte sniffing.
pub fn sniff_content_type(buf: &[u8]) -> String {
    infer::get(buf)
        .map(|t| t.to_string())
        .unwrap_or_else(|| "audio/mpeg".to_string())
}

/// Write `data` to `path` so that concurrent readers see either the old file
/// or the complete new one, never a partially written file: the bytes go to a
/// temporary sibling first, which is then renamed over `path`. Missing parent
/// directories are created.
///
/// Runs as a single blocking task rather than one per filesystem call.
pub async fn write_atomic(path: std::path::PathBuf, data: bytes::Bytes) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(format!(".tmp.{:016x}", rand::random::<u64>()));
        let tmp_path = path.with_file_name(tmp_name);

        let result =
            std::fs::write(&tmp_path, &data).and_then(|()| std::fs::rename(&tmp_path, &path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        result
    })
    .await
    .map_err(std::io::Error::other)?
}
