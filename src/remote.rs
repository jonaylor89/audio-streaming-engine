mod archive;
mod guard;
mod links;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use color_eyre::eyre::eyre;
use dashmap::DashMap;
use reqwest::Client;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tracing::{debug, warn};

use crate::blob::AudioBuffer;
use crate::cache::AudioCache;
use crate::utils::{AppError, e404, e413, e500};
use archive::ArchiveResolver;

const AUDIOUS_API_BASE_URL: &str = "https://api.audius.co/v1";
const ARCHIVE_BASE_URL: &str = "https://archive.org";

/// Cache key prefix for downloaded remote sources in the response cache.
const SOURCE_CACHE_KEY_PREFIX: &str = "remote_src:";

/// Audius permalinks map to a fixed track ID, so this can be long-lived.
const AUDIUS_TRACK_ID_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Upper bound on cached Audius track IDs, so unique URLs can't grow the map
/// without limit.
const AUDIUS_TRACK_ID_CACHE_MAX: usize = 10_000;

/// Fetches audio from remote URLs, caching the downloaded bytes in the
/// response cache so repeated requests for the same source with different
/// processing params don't download it again.
///
/// Besides plain file URLs it understands Audius track pages, Internet Archive
/// items, and share links from Dropbox, Google Drive, OneDrive/SharePoint and
/// GitHub (see [`links`]).
///
/// Unless `allow_private_addresses` is set, it refuses to connect to
/// loopback, private, link-local and other non-public addresses, including
/// after redirects (see [`guard`]).
pub struct RemoteFetcher {
    client: Client,
    cache: Arc<dyn AudioCache>,
    /// `None` disables the source cache.
    source_ttl: Option<Duration>,
    allow_private_addresses: bool,
    audius: AudiusResolver,
    archive: ArchiveResolver,
}

impl RemoteFetcher {
    pub fn new(
        cache: Arc<dyn AudioCache>,
        source_ttl: Option<Duration>,
        allow_private_addresses: bool,
    ) -> color_eyre::Result<Self> {
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10));
        if !allow_private_addresses {
            builder = builder
                .dns_resolver(Arc::new(guard::GuardedResolver))
                .redirect(guard::redirect_policy());
        }
        let client = builder.build()?;

        Ok(Self {
            audius: AudiusResolver::new(client.clone(), AUDIOUS_API_BASE_URL.to_string()),
            archive: ArchiveResolver::new(client.clone(), ARCHIVE_BASE_URL.to_string()),
            client,
            cache,
            source_ttl,
            allow_private_addresses,
        })
    }

    pub async fn fetch(
        &self,
        source: &str,
        max_source_size: usize,
    ) -> Result<AudioBuffer, AppError> {
        let cache_key = source_cache_key(source);

        if self.source_ttl.is_some() {
            match self.cache.get(&cache_key).await {
                Ok(Some(bytes)) if bytes.len() <= max_source_size => {
                    debug!(source, "remote source cache hit");
                    metrics::counter!("remote.source_cache.hit").increment(1);
                    return Ok(AudioBuffer::from_bytes(bytes));
                }
                Ok(_) => {}
                Err(e) => warn!(source, error = %e, "remote source cache read failed"),
            }
            metrics::counter!("remote.source_cache.miss").increment(1);
        }

        let resolved = self.resolve(source, max_source_size).await?;

        if !self.allow_private_addresses {
            let url = reqwest::Url::parse(&resolved).map_err(|e| {
                AppError::new(eyre!("Invalid source URL: {}", e), StatusCode::BAD_REQUEST)
            })?;
            guard::check_url(&url).map_err(|e| {
                warn!(source, "blocked remote fetch: {}", e);
                AppError::new(eyre!("{}", e), StatusCode::FORBIDDEN)
            })?;
        }

        let blob = download(&self.client, &resolved, max_source_size).await?;

        if let Some(ttl) = self.source_ttl {
            let cache = self.cache.clone();
            let data = blob.bytes();
            tokio::spawn(async move {
                if let Err(e) = cache.set(&cache_key, data, Some(ttl)).await {
                    warn!(key = cache_key, error = %e, "remote source cache write failed");
                }
            });
        }

        Ok(blob)
    }
}

impl RemoteFetcher {
    /// Turn a user-supplied source URL into the URL to download.
    async fn resolve(&self, source: &str, max_source_size: usize) -> Result<String, AppError> {
        if let Some(direct) = links::rewrite_share_link(source) {
            debug!(source, direct, "rewrote share link");
            return Ok(direct);
        }

        if is_audius_url(source) {
            return self.audius.resolve_stream_url(source).await.map_err(|e| {
                tracing::error!("Failed to resolve Audius URL {}: {}", source, e);
                AppError::new(eyre!("{}", e), StatusCode::BAD_GATEWAY)
            });
        }

        match self.archive.resolve(source, max_source_size).await {
            Ok(Some(direct)) => Ok(direct),
            Ok(None) => Ok(source.to_string()),
            Err(e) => {
                tracing::error!("Failed to resolve Internet Archive URL {}: {}", source, e);
                Err(AppError::new(eyre!("{}", e), StatusCode::BAD_GATEWAY))
            }
        }
    }
}

fn source_cache_key(source: &str) -> String {
    format!(
        "{}{}",
        SOURCE_CACHE_KEY_PREFIX,
        hex::encode(Sha1::digest(source.as_bytes()))
    )
}

async fn download(
    client: &Client,
    resolved: &str,
    max_source_size: usize,
) -> Result<AudioBuffer, AppError> {
    let response = client.get(resolved).send().await.map_err(|e| {
        if let Some(refused) = guard::find_refusal(&e) {
            warn!(url = resolved, "blocked remote fetch: {}", refused);
            return AppError::new(eyre!("{}", refused), StatusCode::FORBIDDEN);
        }
        tracing::error!("Failed to fetch audio from URL {}: {:?}", resolved, e);
        e404(eyre!("Failed to fetch audio: {}", e))
    })?;

    let status = response.status();
    if !status.is_success() {
        let code = if status == StatusCode::NOT_FOUND {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        return Err(AppError::new(
            eyre!("Remote source returned HTTP {}", status),
            code,
        ));
    }

    // Share links that aren't public (or have changed format) serve a login or
    // preview page instead of the file.
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().starts_with("text/html"));
    if is_html {
        return Err(AppError::new(
            eyre!("Remote source returned a web page instead of audio (is the link public?)"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ));
    }

    if let Some(content_length) = response.content_length()
        && content_length as usize > max_source_size
    {
        return Err(e413(eyre!(
            "Remote audio too large: {} bytes (max {})",
            content_length,
            max_source_size
        )));
    }

    let raw_bytes = response.bytes().await.map_err(|e| {
        tracing::error!("Failed to read bytes from URL {}: {}", resolved, e);
        e500(eyre!("Failed to fetch audio: {}", e))
    })?;

    if raw_bytes.len() > max_source_size {
        return Err(e413(eyre!(
            "Remote audio too large: {} bytes (max {})",
            raw_bytes.len(),
            max_source_size
        )));
    }

    Ok(AudioBuffer::from_bytes(raw_bytes))
}

fn is_audius_url(source: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(source) else {
        return false;
    };

    matches!(url.scheme(), "http" | "https")
        && url
            .host_str()
            .is_some_and(|host| host == "audius.co" || host.ends_with(".audius.co"))
}

struct AudiusResolver {
    client: Client,
    base_url: String,
    /// Permalink → (track ID, resolved at).
    track_ids: DashMap<String, (String, Instant)>,
}

#[derive(Debug, Deserialize)]
struct AudiusDataResponse<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct AudiusTrack {
    id: String,
}

impl AudiusResolver {
    fn new(client: Client, base_url: String) -> Self {
        Self {
            client,
            base_url,
            track_ids: DashMap::new(),
        }
    }

    async fn resolve_stream_url(&self, source: &str) -> Result<String, String> {
        let track_id = self.cached_track_id(source).await?;
        // The stream URL points at a specific content node and may be signed,
        // so it is looked up fresh each time.
        self.fetch_track_stream_url(&track_id).await
    }

    async fn cached_track_id(&self, source: &str) -> Result<String, String> {
        if let Some(entry) = self.track_ids.get(source)
            && entry.1.elapsed() < AUDIUS_TRACK_ID_TTL
        {
            return Ok(entry.0.clone());
        }

        let track_id = self.resolve_track_id(source).await?;

        if self.track_ids.len() >= AUDIUS_TRACK_ID_CACHE_MAX {
            self.track_ids
                .retain(|_, (_, at)| at.elapsed() < AUDIUS_TRACK_ID_TTL);
            if self.track_ids.len() >= AUDIUS_TRACK_ID_CACHE_MAX {
                self.track_ids.clear();
            }
        }
        self.track_ids
            .insert(source.to_string(), (track_id.clone(), Instant::now()));
        Ok(track_id)
    }

    async fn resolve_track_id(&self, source: &str) -> Result<String, String> {
        let resolve_url = format!("{}/resolve", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&resolve_url)
            .query(&[("url", source)])
            .send()
            .await
            .map_err(|e| format!("Audius resolve request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Audius resolve failed with status {}",
                response.status()
            ));
        }

        let final_url = response.url().clone();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Audius resolve response read failed: {}", e))
            .and_then(|body| {
                serde_json::from_str::<AudiusDataResponse<AudiusTrack>>(&body)
                    .map_err(|e| format!("Audius resolve response decode failed: {}", e))
            });

        match body {
            Ok(track) => Ok(track.data.id),
            Err(err) => {
                if let Some(track_id) = track_id_from_resolved_url(&final_url) {
                    warn!(
                        "Fell back to resolved Audius URL parsing after JSON decode failed: {}",
                        err
                    );
                    Ok(track_id)
                } else {
                    Err(format!("Audius resolve response was not a track: {}", err))
                }
            }
        }
    }

    async fn fetch_track_stream_url(&self, track_id: &str) -> Result<String, String> {
        let stream_url = format!(
            "{}/tracks/{}/stream",
            self.base_url.trim_end_matches('/'),
            track_id
        );

        let response = self
            .client
            .get(&stream_url)
            .query(&[("no_redirect", "true")])
            .send()
            .await
            .map_err(|e| format!("Audius stream lookup failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!(
                "Audius stream lookup failed with status {}",
                response.status()
            ));
        }

        let body = response
            .text()
            .await
            .map_err(|e| format!("Audius stream response read failed: {}", e))?;
        let stream = serde_json::from_str::<AudiusDataResponse<String>>(&body)
            .map_err(|e| format!("Audius stream response decode failed: {}", e))?;

        Ok(stream.data)
    }
}

fn track_id_from_resolved_url(url: &reqwest::Url) -> Option<String> {
    let mut segments = url.path_segments()?;
    while let Some(segment) = segments.next() {
        if segment == "tracks" {
            return segments.next().map(|id| id.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    #[tokio::test]
    async fn resolves_regular_audius_url_to_stream_url() {
        let server = MockServer::start().await;
        let resolver = AudiusResolver::new(Client::new(), format!("{}/v1", server.uri()));

        Mock::given(method("GET"))
            .and(path("/v1/resolve"))
            .and(query_param(
                "url",
                "https://audius.co/Ookay/ookay-veronica-bravo-dontwakemeup-ft-veronica-bravo",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "id": "track-123" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/tracks/track-123/stream"))
            .and(query_param("no_redirect", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": "https://content.audius.co/audio/track-123.mp3"
            })))
            .mount(&server)
            .await;

        let resolved = resolver
            .resolve_stream_url(
                "https://audius.co/Ookay/ookay-veronica-bravo-dontwakemeup-ft-veronica-bravo",
            )
            .await
            .unwrap();

        assert_eq!(resolved, "https://content.audius.co/audio/track-123.mp3");
    }

    fn fs_cache(dir: &std::path::Path) -> Arc<dyn AudioCache> {
        Arc::new(crate::cache::fs::FileSystemCache::new(dir, 100).unwrap())
    }

    /// A second fetch of the same URL is served from the source cache.
    #[tokio::test]
    async fn remote_source_is_downloaded_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/song.mp3"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xFB, 0x90, 0x00]))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        // Mock servers listen on 127.0.0.1, so allow private addresses.
        let fetcher =
            RemoteFetcher::new(fs_cache(dir.path()), Some(Duration::from_secs(60)), true).unwrap();
        let url = format!("{}/song.mp3", server.uri());

        let first = fetcher.fetch(&url, 1024).await.unwrap();
        // The cache write happens in the background; wait for it to land.
        for _ in 0..100 {
            if fetcher
                .cache
                .get(&source_cache_key(&url))
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second = fetcher.fetch(&url, 1024).await.unwrap();

        assert_eq!(first.as_ref(), second.as_ref());
        // `expect(1)` is verified when `server` drops.
    }

    /// With the source cache disabled, every fetch downloads.
    #[tokio::test]
    async fn remote_source_cache_can_be_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/song.mp3"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xFB, 0x90, 0x00]))
            .expect(2)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let fetcher = RemoteFetcher::new(fs_cache(dir.path()), None, true).unwrap();
        let url = format!("{}/song.mp3", server.uri());

        fetcher.fetch(&url, 1024).await.unwrap();
        fetcher.fetch(&url, 1024).await.unwrap();
    }

    /// The permalink → track ID lookup happens once; the stream URL is looked
    /// up every time.
    #[tokio::test]
    async fn audius_track_id_is_cached() {
        let server = MockServer::start().await;
        let resolver = AudiusResolver::new(Client::new(), format!("{}/v1", server.uri()));

        Mock::given(method("GET"))
            .and(path("/v1/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "id": "track-123" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/tracks/track-123/stream"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": "https://content.audius.co/audio/track-123.mp3"
            })))
            .expect(2)
            .mount(&server)
            .await;

        for _ in 0..2 {
            let url = resolver
                .resolve_stream_url("https://audius.co/artist/some-track")
                .await
                .unwrap();
            assert_eq!(url, "https://content.audius.co/audio/track-123.mp3");
        }
    }

    async fn mp3_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/song.mp3"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xFB, 0x90, 0x00]))
            .mount(&server)
            .await;
        server
    }

    /// By default, fetches from private addresses are refused with 403,
    /// whether the URL names an IP or a hostname that resolves to one.
    #[tokio::test]
    async fn private_addresses_are_refused_by_default() {
        let server = mp3_server().await;
        let port = server.address().port();
        let dir = tempfile::tempdir().unwrap();
        let fetcher = RemoteFetcher::new(fs_cache(dir.path()), None, false).unwrap();

        for url in [
            format!("http://127.0.0.1:{port}/song.mp3"),
            format!("http://localhost:{port}/song.mp3"),
        ] {
            let err = fetcher.fetch(&url, 1024).await.unwrap_err();
            assert_eq!(err.status(), StatusCode::FORBIDDEN, "{url}");
        }
    }

    /// A redirect from a public URL to a private address is refused too.
    #[tokio::test]
    async fn redirect_to_private_address_is_refused() {
        let client = Client::builder()
            .redirect(guard::redirect_policy())
            .build()
            .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "http://169.254.169.254/latest/meta-data"),
            )
            .mount(&server)
            .await;

        let err = download(&client, &format!("{}/start", server.uri()), 1024)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn upstream_404_is_not_processed_as_audio() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let err = download(
            &Client::new(),
            &format!("{}/missing.mp3", server.uri()),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn html_page_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html>Sign in</html>", "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let err = download(&Client::new(), &format!("{}/share", server.uri()), 1024)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
