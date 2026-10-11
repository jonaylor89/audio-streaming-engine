//! Rewrites for share links that serve an HTML page instead of the file
//! itself. Each rewrite turns the link people copy from the browser into the
//! provider's direct-download URL. None of them need a network round trip.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use url::Url;

/// Rewrite a known share link to its direct-download URL. Returns `None` when
/// the URL isn't a recognised share link (it is then fetched as-is).
pub fn rewrite_share_link(source: &str) -> Option<String> {
    let url = Url::parse(source).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();

    match host.as_str() {
        "dropbox.com" | "www.dropbox.com" => dropbox(url),
        "drive.google.com" => google_drive(&url),
        "1drv.ms" | "onedrive.live.com" => onedrive_personal(&url),
        h if h.ends_with(".sharepoint.com") => sharepoint(url),
        "github.com" | "www.github.com" => github_blob(&url),
        _ => None,
    }
}

/// `dropbox.com/s/...?dl=0` or `dropbox.com/scl/fi/...?rlkey=...&dl=0`:
/// force `dl=1`, keeping every other query parameter (`rlkey` is required).
fn dropbox(mut url: Url) -> Option<String> {
    let path = url.path();
    if !(path.starts_with("/s/") || path.starts_with("/scl/fi/")) {
        return None;
    }
    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "dl" && k != "raw")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(query)
        .append_pair("dl", "1");
    Some(url.into())
}

/// `drive.google.com/file/d/<id>/view`, `/open?id=<id>` or `/uc?id=<id>`.
///
/// `confirm=t` skips the "can't scan this file for viruses" page that Drive
/// shows instead of the file for anything over ~100 MB.
fn google_drive(url: &Url) -> Option<String> {
    let mut segments = url.path_segments()?;
    let id = match segments.next()? {
        "file" if segments.next() == Some("d") => segments.next().map(str::to_owned),
        "open" | "uc" => url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned()),
        _ => None,
    }?;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(format!(
        "https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"
    ))
}

/// Personal OneDrive share links (`1drv.ms/...`, `onedrive.live.com/...`) go
/// through the OneDrive shares API, which takes the share URL itself encoded
/// as `u!<base64url>` and redirects to the file.
fn onedrive_personal(url: &Url) -> Option<String> {
    // A bare onedrive.live.com page isn't a share link.
    if url.host_str() == Some("onedrive.live.com") && url.query().is_none() {
        return None;
    }
    let encoded = URL_SAFE_NO_PAD.encode(url.as_str());
    Some(format!(
        "https://api.onedrive.com/v1.0/shares/u!{encoded}/root/content"
    ))
}

/// OneDrive for Business / SharePoint share links (`<tenant>.sharepoint.com/:u:/...`)
/// download directly with `download=1`.
fn sharepoint(mut url: Url) -> Option<String> {
    if !url.path().starts_with("/:") {
        return None;
    }
    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "download")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(query)
        .append_pair("download", "1");
    Some(url.into())
}

/// `github.com/<owner>/<repo>/blob/<ref>/<path>` → `raw.githubusercontent.com/<owner>/<repo>/<ref>/<path>`.
fn github_blob(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    match segments.as_slice() {
        [owner, repo, "blob", rest @ ..] if rest.len() >= 2 => Some(format!(
            "https://raw.githubusercontent.com/{owner}/{repo}/{}",
            rest.join("/")
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropbox_legacy_link_forces_download() {
        assert_eq!(
            rewrite_share_link("https://www.dropbox.com/s/abc123/song.mp3?dl=0").as_deref(),
            Some("https://www.dropbox.com/s/abc123/song.mp3?dl=1")
        );
    }

    #[test]
    fn dropbox_scl_link_keeps_rlkey() {
        assert_eq!(
            rewrite_share_link("https://www.dropbox.com/scl/fi/xyz/song.mp3?rlkey=k3y&dl=0")
                .as_deref(),
            Some("https://www.dropbox.com/scl/fi/xyz/song.mp3?rlkey=k3y&dl=1")
        );
    }

    #[test]
    fn dropbox_non_share_page_is_untouched() {
        assert_eq!(rewrite_share_link("https://www.dropbox.com/home"), None);
    }

    #[test]
    fn google_drive_view_link() {
        assert_eq!(
            rewrite_share_link("https://drive.google.com/file/d/1AbC-d_E/view?usp=sharing")
                .as_deref(),
            Some(
                "https://drive.usercontent.google.com/download?id=1AbC-d_E&export=download&confirm=t"
            )
        );
    }

    #[test]
    fn google_drive_open_and_uc_links() {
        let expected =
            Some("https://drive.usercontent.google.com/download?id=1AbC&export=download&confirm=t");
        assert_eq!(
            rewrite_share_link("https://drive.google.com/open?id=1AbC").as_deref(),
            expected
        );
        assert_eq!(
            rewrite_share_link("https://drive.google.com/uc?id=1AbC&export=download").as_deref(),
            expected
        );
    }

    #[test]
    fn google_drive_rejects_odd_ids() {
        assert_eq!(
            rewrite_share_link("https://drive.google.com/open?id=a%26b%3Dc"),
            None
        );
    }

    #[test]
    fn onedrive_short_link_uses_shares_api() {
        let rewritten = rewrite_share_link("https://1drv.ms/u/s!AbCdEf").unwrap();
        let encoded = URL_SAFE_NO_PAD.encode("https://1drv.ms/u/s!AbCdEf");
        assert_eq!(
            rewritten,
            format!("https://api.onedrive.com/v1.0/shares/u!{encoded}/root/content")
        );
    }

    #[test]
    fn sharepoint_link_gets_download_flag() {
        assert_eq!(
            rewrite_share_link("https://contoso.sharepoint.com/:u:/g/personal/x/EAbC?e=Xyz")
                .as_deref(),
            Some("https://contoso.sharepoint.com/:u:/g/personal/x/EAbC?e=Xyz&download=1")
        );
    }

    #[test]
    fn github_blob_to_raw() {
        assert_eq!(
            rewrite_share_link("https://github.com/owner/repo/blob/main/audio/clip.wav").as_deref(),
            Some("https://raw.githubusercontent.com/owner/repo/main/audio/clip.wav")
        );
    }

    #[test]
    fn github_non_blob_is_untouched() {
        assert_eq!(rewrite_share_link("https://github.com/owner/repo"), None);
    }

    #[test]
    fn other_urls_are_untouched() {
        assert_eq!(rewrite_share_link("https://example.com/song.mp3"), None);
        assert_eq!(rewrite_share_link("not a url"), None);
    }
}
