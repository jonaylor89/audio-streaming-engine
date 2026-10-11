//! Internet Archive (`archive.org/details/<item>`) support.
//!
//! An item page lists many files (originals plus derived copies in other
//! formats). The public metadata API tells us which ones are audio, so we can
//! pick one and download it from `archive.org/download/<item>/<file>`.

use reqwest::Client;
use serde::Deserialize;
use url::Url;

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav"];

pub struct ArchiveResolver {
    client: Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct ItemMetadata {
    #[serde(default)]
    files: Vec<ItemFile>,
}

#[derive(Debug, Deserialize)]
struct ItemFile {
    name: String,
    #[serde(default)]
    source: String,
    /// The API returns sizes as strings.
    size: Option<String>,
}

/// What an archive.org URL points at.
enum ArchiveTarget {
    /// A specific file; downloadable without asking the API.
    File { item: String, file: String },
    /// A whole item; the API decides which file to fetch.
    Item(String),
}

impl ArchiveResolver {
    pub fn new(client: Client, base_url: String) -> Self {
        Self { client, base_url }
    }

    /// Resolve an archive.org item or file URL to a direct download URL.
    /// Returns `Ok(None)` for URLs that aren't archive.org item/file links.
    pub async fn resolve(
        &self,
        source: &str,
        max_source_size: usize,
    ) -> Result<Option<String>, String> {
        let Some(target) = parse(source) else {
            return Ok(None);
        };
        let base = self.base_url.trim_end_matches('/');

        let (item, file) = match target {
            ArchiveTarget::File { item, file } => (item, file),
            ArchiveTarget::Item(item) => {
                let file = self.pick_audio_file(&item, max_source_size).await?;
                (item, file)
            }
        };

        let file_path = file
            .split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        Ok(Some(format!("{base}/download/{item}/{file_path}")))
    }

    /// Choose the file to fetch for an item: original uploads beat derived
    /// copies, and files over `max_source_size` are skipped so that a huge
    /// original falls back to its smaller derived MP3.
    async fn pick_audio_file(&self, item: &str, max_source_size: usize) -> Result<String, String> {
        let metadata_url = format!("{}/metadata/{}", self.base_url.trim_end_matches('/'), item);
        let response = self
            .client
            .get(&metadata_url)
            .send()
            .await
            .map_err(|e| format!("Internet Archive metadata request failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Internet Archive metadata request failed with status {}",
                response.status()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("Internet Archive metadata read failed: {e}"))?;
        let metadata: ItemMetadata = serde_json::from_str(&body)
            .map_err(|e| format!("Internet Archive metadata decode failed: {e}"))?;

        let mut candidates: Vec<&ItemFile> = metadata
            .files
            .iter()
            .filter(|f| is_audio_file(&f.name))
            .filter(|f| {
                f.size
                    .as_deref()
                    .and_then(|s| s.parse::<usize>().ok())
                    .is_none_or(|size| size <= max_source_size)
            })
            .collect();
        candidates.sort_by(|a, b| {
            (a.source != "original", &a.name).cmp(&(b.source != "original", &b.name))
        });

        candidates
            .first()
            .map(|f| f.name.clone())
            .ok_or_else(|| {
                format!(
                    "Internet Archive item '{item}' has no audio files within the {max_source_size}-byte size limit"
                )
            })
    }
}

fn parse(source: &str) -> Option<ArchiveTarget> {
    let url = Url::parse(source).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("archive.org" | "www.archive.org"))
    {
        return None;
    }

    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["details", item] => Some(ArchiveTarget::Item(item.to_string())),
        // `/download/<item>` with no file behaves like the item page; with a
        // file it's already a direct link and is fetched as-is.
        ["download", item] => Some(ArchiveTarget::Item(item.to_string())),
        ["details", item, file @ ..] => Some(ArchiveTarget::File {
            item: item.to_string(),
            file: file
                .iter()
                .map(|s| urlencoding::decode(s).map(|d| d.into_owned()))
                .collect::<Result<Vec<_>, _>>()
                .ok()?
                .join("/"),
        }),
        _ => None,
    }
}

fn is_audio_file(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn resolver_with_files(files: serde_json::Value) -> (MockServer, ArchiveResolver) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/metadata/some-item"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "files": files })),
            )
            .mount(&server)
            .await;
        let resolver = ArchiveResolver::new(Client::new(), server.uri());
        (server, resolver)
    }

    #[tokio::test]
    async fn item_prefers_original_audio() {
        let (server, resolver) = resolver_with_files(serde_json::json!([
            { "name": "cover.jpg", "source": "original", "size": "1000" },
            { "name": "02 b.mp3", "source": "derivative", "size": "100" },
            { "name": "01 a.flac", "source": "original", "size": "5000" },
        ]))
        .await;

        let url = resolver
            .resolve("https://archive.org/details/some-item", 1_000_000)
            .await
            .unwrap();
        assert_eq!(
            url,
            Some(format!("{}/download/some-item/01%20a.flac", server.uri()))
        );
    }

    #[tokio::test]
    async fn oversized_original_falls_back_to_derivative() {
        let (server, resolver) = resolver_with_files(serde_json::json!([
            { "name": "track.flac", "source": "original", "size": "900000000" },
            { "name": "track.mp3", "source": "derivative", "size": "4000000" },
        ]))
        .await;

        let url = resolver
            .resolve("https://archive.org/details/some-item", 100 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            url,
            Some(format!("{}/download/some-item/track.mp3", server.uri()))
        );
    }

    #[tokio::test]
    async fn item_without_audio_is_an_error() {
        let (_server, resolver) = resolver_with_files(serde_json::json!([
            { "name": "book.pdf", "source": "original", "size": "1000" },
        ]))
        .await;

        let err = resolver
            .resolve("https://archive.org/details/some-item", 1_000_000)
            .await
            .unwrap_err();
        assert!(err.contains("no audio files"), "{err}");
    }

    #[tokio::test]
    async fn file_link_needs_no_api_call() {
        // No mocks mounted: any API call would fail the resolve.
        let server = MockServer::start().await;
        let resolver = ArchiveResolver::new(Client::new(), server.uri());

        let url = resolver
            .resolve(
                "https://archive.org/details/some-item/disc%201/01%20intro.mp3",
                1_000_000,
            )
            .await
            .unwrap();
        assert_eq!(
            url,
            Some(format!(
                "{}/download/some-item/disc%201/01%20intro.mp3",
                server.uri()
            ))
        );
    }

    #[tokio::test]
    async fn non_archive_urls_are_ignored() {
        let resolver = ArchiveResolver::new(Client::new(), "http://unused".into());
        for url in [
            "https://example.com/details/x",
            "https://archive.org/download/item/file.mp3",
            "https://archive.org/search?query=x",
        ] {
            assert_eq!(resolver.resolve(url, 1).await.unwrap(), None, "{url}");
        }
    }
}
