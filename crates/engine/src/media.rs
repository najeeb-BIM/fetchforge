//! Stream downloads: HLS (.m3u8) and DASH (.mpd), plus the variant /
//! representation parsing behind the quality picker (StreamAnalyzer,
//! HLSParser, DASHParser in the spec §11 module map).
//!
//! Supported ONLY when the manifest and segments are openly
//! accessible and unencrypted. Anything with DRM, an EXT-X-KEY,
//! ContentProtection, expiring signed segments the caller cannot
//! legally fetch, or authentication the user has not supplied is
//! refused with a plain explanation. No DRM is bypassed, ever.
//!
//! Live (unbounded) streams are not recorded: an HLS playlist is
//! fetched once and only the segments listed at that moment are
//! downloaded (a finite snapshot); a DASH manifest with
//! type="dynamic" is refused outright.

use crate::download::Progress;
use crate::extract::DOWNLOAD_STALL_TIMEOUT;
use crate::validate::validate_url;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which streaming protocol a URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Hls,
    Dash,
}

impl StreamKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StreamKind::Hls => "hls",
            StreamKind::Dash => "dash",
        }
    }
}

/// The protocol of a URL, by its path extension (query strings and
/// fragments ignored, case-insensitive). Extension-less manifests
/// are detected by content type instead (see
/// [`stream_kind_of_content_type`]) or fall through to yt-dlp,
/// which handles HLS/DASH itself.
pub fn stream_kind_of_url(url: &str) -> Option<StreamKind> {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|_| {
            url.split(['?', '#'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        });
    if path.ends_with(".m3u8") {
        Some(StreamKind::Hls)
    } else if path.ends_with(".mpd") {
        Some(StreamKind::Dash)
    } else if proxy_inner_manifest_url(url).is_some() {
        // Proxy-wrapped HLS manifest (see
        // [`proxy_inner_manifest_url`]): routed to the HLS engine,
        // which fetches through the wrapper and resolves segments
        // against the inner URL.
        Some(StreamKind::Hls)
    } else {
        None
    }
}

pub fn is_manifest_url(url: &str) -> bool {
    stream_kind_of_url(url).is_some()
}

/// The protocol of a probed response, by content type. Note
/// `audio/x-mpegurl` is deliberately NOT here: that is an M3U audio
/// playlist, not HLS.
pub fn stream_kind_of_content_type(content_type: &str) -> Option<StreamKind> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => Some(StreamKind::Hls),
        "application/dash+xml" => Some(StreamKind::Dash),
        _ => None,
    }
}

/// One quality option of a stream, in protocol-neutral form. The
/// extraction module converts these to [`crate::extract::MediaFormat`]
/// for the picker, so HLS/DASH qualities look exactly like yt-dlp
/// qualities in the UI.
#[derive(Debug, Clone)]
pub struct StreamFormat {
    /// HLS: the variant playlist URL. DASH: the representation id.
    pub id: String,
    pub ext: String,
    pub is_video: bool,
    pub is_audio: bool,
    pub height: Option<u32>,
    pub width: Option<u32>,
    /// Audio bitrate in kbps, when the manifest reports it.
    pub abr_kbps: Option<f64>,
    /// Video bitrate in kbps, when the manifest reports it.
    pub vbr_kbps: Option<f64>,
    pub fps: Option<f64>,
    pub filesize: Option<u64>,
    /// True when the download must merge this stream with a
    /// separate audio stream (needs ffmpeg).
    pub needs_merge: bool,
}

/// Everything the picker needs for one stream URL.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub kind: StreamKind,
    pub title: String,
    pub duration_secs: Option<u64>,
    pub manifest_url: String,
    pub formats: Vec<StreamFormat>,
    /// How many segments the (default) media playlist lists.
    /// Recorded at plan time so the UI can show "0 / N pieces"
    /// before the first segment lands. None when the count could
    /// not be determined (DASH template manifests without a
    /// timeline, or a variant playlist that failed to fetch).
    pub segment_count: Option<u64>,
}

/// List the qualities of an HLS or DASH manifest URL (routed by
/// extension). The caller must have checked [`is_manifest_url`].
/// `referer` is the page the stream came from (sent on the
/// manifest request — signed CDNs refuse requests without it);
/// `captured` is playlist text the extension captured from the
/// player's own response, used instead of re-fetching a signed
/// link that may only work once (see [`CapturedManifest`]).
pub fn list_stream_formats(
    url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    match stream_kind_of_url(url) {
        Some(StreamKind::Hls) => list_hls_formats(url, referer, captured),
        Some(StreamKind::Dash) => list_dash_formats(url, referer, captured),
        None => Err("Not an HLS (.m3u8) or DASH (.mpd) URL.".to_string()),
    }
}

/// A human title for a manifest URL: the file stem, unless it is a
/// generic word like "master" or "index", in which case the parent
/// folder name is more informative (e.g. `.../bbb/master.m3u8` →
/// "bbb").
fn title_from_manifest_url(url: &str) -> String {
    // A proxy-wrapped manifest has no useful path of its own
    // (`/`); title it from the inner manifest URL instead.
    let url = proxy_inner_manifest_url(url).unwrap_or_else(|| url.to_string());
    let url = url.as_str();
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string());
    let stem = Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    const GENERIC: [&str; 8] = [
        "master", "index", "playlist", "manifest", "chunklist", "hls", "dash", "video",
    ];
    if !stem.is_empty() && !GENERIC.contains(&stem.to_ascii_lowercase().as_str()) {
        return stem;
    }
    let parent = Path::new(&path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if parent.is_empty() {
        "stream".to_string()
    } else {
        parent
    }
}

/// Format a reqwest error WITH its underlying cause. reqwest's
/// Display for a body-read failure is just "error decoding
/// response body": `Response::bytes()` maps EVERY body error to
/// Kind::Decode — a timeout mid-body, a connection reset, a
/// truncated chunked body — and this build has no decoder
/// features at all (Cargo.toml: blocking + rustls-tls +
/// system-proxy only), so decompression is never even attempted.
/// The bare message hid the real cause twice on 2026-11-22 (an
/// Atlantic segment failed 3 times with it) and, worse, it hides
/// timeouts from `looks_like_timeout`, so the 0-byte fail-fast
/// never fired and the download sat silent for 3 x 120 s. Walk
/// the source chain so the recorded error says what actually
/// happened ("error decoding response body: operation timed
/// out"), which the retry rule can then recognise.
fn reqwest_error(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// How many HLS/DASH segments download at once (bounded worker
/// pool in [`download_segments`]). Four is the conservative end of
/// the 4–8 range every mature downloader in the study uses (XDM/AB
/// Download Manager default 8; the yt-dlp GUIs 4–8): enough to
/// keep a movie flowing over one warm connection pool, few enough
/// not to trip per-host connection limits on strict CDNs.
pub const HLS_SEGMENT_WORKERS: usize = 4;

/// Largest playlist text the extension may pass to the engine with
/// a download (see [`CapturedManifest`]): a media playlist for a
/// feature film is tens of KB; 256 KB leaves ample headroom while
/// keeping the native message (1 MB cap) and the manifest sidecar
/// small. Larger captures are ignored and the engine fetches the
/// playlist normally.
pub const MAX_CAPTURED_MANIFEST_BYTES: usize = 256 * 1024;

/// The User-Agent sent on manifest and segment requests: a real
/// current Chrome string, not `FetchForge/0.2`. Several CDNs
/// (totallyacdn.cc among them, seen 2026-11-22) treat unknown
/// user-agents as bots and stall or refuse their segment requests,
/// while the same requests from a browser identity succeed — every
/// working downloader in the study replays browser headers. This
/// is only an identity string: no cookies, no auth, no session is
/// ever sent with it.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

/// Total timeout for one manifest or segment request (unchanged
/// from the sequential engine): a request that has not finished in
/// this long is dead, and the error surfaces as a timeout (see
/// [`reqwest_error`]).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// One shared HTTP client for a whole stream download (study §A1):
/// manifest + every segment reuse the same connection pool, so
/// segments ride the warm connection that fetched the manifest
/// instead of paying a fresh TCP+TLS handshake per segment — the
/// old code built a new client per segment, the same anti-pattern
/// res-downloader and ODM exhibit, and a prime suspect for the
/// Atlantic segment-1 timeout on a CDN that rate-limits new
/// connections. HTTP/1.1 only (study §A3: XDM and ABDM both force
/// it; some CDNs reject Range with 400 over HTTP/2).
fn hls_client() -> Result<reqwest::blocking::Client, String> {
    hls_client_with_timeout(REQUEST_TIMEOUT)
}

/// [`hls_client`] with an overridden total timeout, for tests (a
/// stalling stub server must fail in seconds, not after 120 s).
fn hls_client_with_timeout(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(BROWSER_USER_AGENT)
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(60))
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Benign browser headers for manifest + segment requests (study
/// §A2): the headers the player's own fetch/XHR sent when it
/// played the stream. Only the benign subset is ever replayed —
/// never Cookie or Authorization (spec §10 forbids credential use
/// outright; see the study §12). `Accept-Encoding: identity`
/// stays: segments are already-compressed media and this build has
/// no decoder features, so a compressed body only adds failure
/// modes (see [`reqwest_error`]).
fn browser_headers(
    req: reqwest::blocking::RequestBuilder,
    referer: &Option<String>,
) -> reqwest::blocking::RequestBuilder {
    let mut req = req
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept-Encoding", "identity");
    if let Some(r) = referer {
        req = req.header("Referer", r.clone());
        // Origin: the player fetches cross-origin manifests via
        // fetch/XHR, which send Origin; include the page's origin
        // so the request looks like the player's own.
        if let Ok(u) = url::Url::parse(r) {
            if let (Some(host), Some(port)) = (
                u.host_str(),
                u.port_or_known_default(),
            ) {
                let default_port = match u.scheme() {
                    "http" => 80,
                    "https" => 443,
                    _ => 0,
                };
                let origin = if port == default_port {
                    format!("{}://{}", u.scheme(), host)
                } else {
                    format!("{}://{}:{}", u.scheme(), host, port)
                };
                req = req.header("Origin", origin);
            }
        }
    }
    req
}

/// A playlist the browser extension captured from the player's own
/// response (see the extension's MAIN-world fetch/XHR hook): the
/// exact text the player received, so the engine never re-fetches
/// a signed link that may only work once (study §A8 — some CDNs
/// sign a playlist link for a single request, so a re-fetch gets
/// 403). The text is used ONLY when its URL exactly matches the
/// URL being fetched; anything else falls through to a normal
/// fetch. Captured text is validated to look like a playlist
/// before use (see [`manifest_text`]).
#[derive(Debug, Clone)]
pub struct CapturedManifest {
    pub url: String,
    pub text: String,
}

/// Fetch a manifest's text, preferring a captured copy when it
/// matches the URL exactly. A captured text that does not look
/// like a playlist (no `#EXTM3U` for .m3u8, no `<MPD` for .mpd)
/// is ignored and the URL is fetched normally — a stale or
/// mismatched capture must never poison a download.
fn manifest_text(
    client: &reqwest::blocking::Client,
    url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<String, String> {
    if let Some(cap) = captured {
        if cap.url == url {
            let lower = url.to_ascii_lowercase();
            let looks_like_playlist = if lower.contains(".m3u8") {
                cap.text.contains("#EXTM3U")
            } else if lower.contains(".mpd") {
                cap.text.contains("<MPD")
            } else {
                false
            };
            if looks_like_playlist {
                return Ok(cap.text.clone());
            }
        }
    }
    fetch_text(client, url, referer)
}

fn fetch_text(
    client: &reqwest::blocking::Client,
    url: &str,
    referer: &Option<String>,
) -> Result<String, String> {
    let req = browser_headers(client.get(url), referer);
    let resp = req.send().map_err(|e| reqwest_error(&e))?;
    if !resp.status().is_success() {
        return Err(format!("Manifest request failed: HTTP {}", resp.status()));
    }
    resp.text().map_err(|e| reqwest_error(&e))
}

/// A segment-fetch failure, with the HTTP status and any
/// `Retry-After` the server sent. The `message` is exactly what
/// `fetch_bytes_range` has always returned (callers match on its
/// text); `status` and `retry_after` are the structured facts the
/// rate-limit handling needs — a 429 must back off and slow down,
/// not just retry (see [`RateLimiter`]).
struct SegmentFetchError {
    message: String,
    status: Option<u16>,
    retry_after: Option<Duration>,
}

/// Parse a `Retry-After` header value: delay-seconds, or an
/// HTTP-date (IMF-fixdate). Anything unparseable is None (the
/// caller falls back to exponential backoff). Clamped to 60 s: a
/// server that asks for an hour gets a minute — the run stays
/// bounded by the stall watchdog, and a further 429 re-backs-off.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(60)));
    }
    parse_http_date(value).map(|when| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        when.saturating_sub(now).min(Duration::from_secs(60))
    })
}

/// Parse an IMF-fixdate ("Wed, 21 Oct 2015 07:28:00 GMT") into a
/// duration since the Unix epoch. Only the IMF-fixdate form is
/// supported (RFC 7231's preferred form, and what CDNs send);
/// the obsolete RFC 850 / asctime forms return None.
fn parse_http_date(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (_, rest) = value.split_once(", ")?;
    let mut parts = rest.splitn(4, ' ');
    let day: u64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts.next()?.parse().ok()?;
    if year < 1970 {
        return None;
    }
    let time = parts.next()?;
    let time = time.strip_suffix(" GMT").unwrap_or(time);
    let mut t = time.split(':');
    let hour: u64 = t.next()?.parse().ok()?;
    let min: u64 = t.next()?.parse().ok()?;
    let sec: u64 = t.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Days since the epoch (Howard Hinnant's civil-from-days
    // algorithm, inverted): valid for all realistic HTTP dates.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    Some(Duration::from_secs(secs))
}

/// Fetch bytes, optionally a single inclusive byte range
/// (DASH SegmentList `@mediaRange`). A 206 response is used as-is;
/// a 200 response (server ignored the Range header) is sliced to
/// the requested range, so the assembly stays correct. The caller
/// passes the download's shared client (see [`hls_client`]); a
/// fresh client per segment is exactly the anti-pattern that
/// starved Atlantic's CDN of warm connections.
///
/// On an HTTP error the returned [`SegmentFetchError`] carries
/// the status and any `Retry-After` header (429s only in
/// practice); the message is the same text `fetch_bytes_range`
/// has always produced.
fn fetch_segment(
    client: &reqwest::blocking::Client,
    url: &str,
    range: Option<(u64, u64)>,
    referer: &Option<String>,
) -> Result<Vec<u8>, SegmentFetchError> {
    let mut req = browser_headers(client.get(url), referer);
    if let Some((start, end)) = range {
        req = req.header("Range", format!("bytes={start}-{end}"));
    }
    let resp = req.send().map_err(|e| SegmentFetchError {
        message: reqwest_error(&e),
        status: None,
        retry_after: None,
    })?;
    let status = resp.status();
    if !status.is_success() {
        let retry_after = if status.as_u16() == 429 {
            resp.headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after)
        } else {
            None
        };
        return Err(SegmentFetchError {
            message: format!("Segment request failed: HTTP {status}"),
            status: Some(status.as_u16()),
            retry_after,
        });
    }
    let bytes = resp
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| SegmentFetchError {
            message: reqwest_error(&e),
            status: Some(status.as_u16()),
            retry_after: None,
        })?;
    match range {
        None => Ok(bytes),
        Some((start, end)) => {
            if status.as_u16() == 206 {
                Ok(bytes)
            } else {
                // Server ignored Range: slice the full body.
                let start = start as usize;
                let end = end as usize;
                if bytes.len() <= end {
                    return Err(SegmentFetchError {
                        message: "Segment range request returned a short body; the server may not support Range requests."
                            .to_string(),
                        status: Some(status.as_u16()),
                        retry_after: None,
                    });
                }
                Ok(bytes[start..=end].to_vec())
            }
        }
    }
}

/// [`fetch_segment`] with the error reduced to its message, for
/// callers that do not handle rate limits (the legacy sequential
/// path, the mp4 probe). The message text is unchanged.
fn fetch_bytes_range(
    client: &reqwest::blocking::Client,
    url: &str,
    range: Option<(u64, u64)>,
    referer: &Option<String>,
) -> Result<Vec<u8>, String> {
    fetch_segment(client, url, range, referer).map_err(|e| e.message)
}

fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return inherit_query(base, rel);
    }
    match url::Url::parse(base).and_then(|b| b.join(rel)) {
        Ok(u) => inherit_query(base, &u.to_string()),
        Err(_) => rel.to_string(),
    }
}

/// Signed playlists carry their auth in the query string
/// (`?sign=...`, `?token=...`), and the CDN requires the same
/// query on every segment request: a segment fetched without it
/// is refused (HTTP 400 on the CDNs these sites use). URL joining
/// drops the base's query for relative references, and absolute
/// segment URLs never had it — so a signed playlist's segments
/// all failed. Inherit the base's query onto a resolved URL when
/// the resolved URL has no query of its own AND shares the base's
/// origin (scheme + host + port): the query is the playlist's
/// credential for its own CDN, never something to leak to another
/// origin. A resolved URL that already has a query keeps it.
fn inherit_query(base: &str, resolved: &str) -> String {
    let Ok(base_url) = url::Url::parse(base) else {
        return resolved.to_string();
    };
    let Some(query) = base_url.query() else {
        return resolved.to_string();
    };
    if query.is_empty() {
        return resolved.to_string();
    }
    let Ok(mut out) = url::Url::parse(resolved) else {
        return resolved.to_string();
    };
    if out.query().is_some() {
        return resolved.to_string();
    }
    if out.scheme() != base_url.scheme()
        || out.host_str() != base_url.host_str()
        || out.port_or_known_default() != base_url.port_or_known_default()
    {
        return resolved.to_string();
    }
    out.set_query(Some(query));
    out.to_string()
}

/// A proxy-wrapped manifest URL: the player fetches its manifest
/// through a proxy worker whose query carries the real manifest
/// URL (`https://proxy.example/?origin=...&url=https://cdn.../index.m3u8`).
/// The wrapper's path is `/`, so extension routing would not
/// recognise it as a manifest at all — and yt-dlp, handed the
/// wrapper, resolves the manifest's relative segments against
/// the PROXY origin, where they 404, retrying each fragment until
/// the download sits at 0% with no error (seen 2026-11-22 on a
/// workers.dev proxy). Returns the inner manifest URL when the
/// query's `url` (or `u`) parameter is itself an HLS manifest
/// URL; DASH wrappers are not unwrapped (the DASH engine has no
/// base override, so they keep their current yt-dlp routing).
pub fn proxy_inner_manifest_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    for (k, v) in parsed.query_pairs() {
        if (k == "url" || k == "u")
            && stream_kind_of_url(&v) == Some(StreamKind::Hls)
        {
            return Some(v.into_owned());
        }
    }
    None
}

/// The base URL segment/variant references resolve against: the
/// inner manifest URL for a proxy wrapper (segments live on the
/// real CDN, not the proxy), else the manifest URL itself. The
/// manifest is still FETCHED from the wrapper URL — only the
/// resolution base changes.
fn manifest_base_url(url: &str) -> String {
    proxy_inner_manifest_url(url).unwrap_or_else(|| url.to_string())
}

// ---------------------------------------------------------------------------
// HLS parsing
// ---------------------------------------------------------------------------

/// One variant of an HLS master playlist.
#[derive(Debug, Clone)]
pub struct HlsVariant {
    /// Absolute URL of the variant (media) playlist.
    pub uri: String,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub fps: Option<f64>,
    /// The AUDIO group this variant's sound lives in, if it is
    /// separate from the video (see [`HlsMaster::audio`]).
    pub audio_group: Option<String>,
}

/// An `#EXT-X-MEDIA` audio rendition of a master playlist.
#[derive(Debug, Clone)]
pub struct HlsAudioGroup {
    pub group_id: String,
    /// Absolute URL of the audio (media) playlist.
    pub uri: String,
    pub name: Option<String>,
}

/// A parsed HLS master playlist. A media playlist parses to zero
/// variants (its segments come from [`parse_hls_segments`]).
#[derive(Debug, Clone, Default)]
pub struct HlsMaster {
    pub variants: Vec<HlsVariant>,
    pub audio: Vec<HlsAudioGroup>,
}

/// Split an `#EXT-X-STREAM-INF` / `#EXT-X-MEDIA` attribute list on
/// commas that are NOT inside double quotes (`CODECS` values contain
/// commas). Returns (KEY, VALUE) pairs with quotes stripped.
fn split_attrs(s: &str) -> Vec<(&str, &str)> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                items.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&s[start..]);
    items
        .into_iter()
        .filter_map(|item| {
            let (k, v) = item.split_once('=')?;
            Some((k.trim(), v.trim().trim_matches('"')))
        })
        .collect()
}

fn parse_resolution(v: &str) -> (Option<u32>, Option<u32>) {
    match v.split_once('x') {
        Some((w, h)) => (w.parse().ok(), h.parse().ok()),
        None => (None, None),
    }
}

/// Parse an HLS master playlist. A media playlist yields an empty
/// master (no variants). Relative URIs resolve against `base_url`.
pub fn parse_hls_master(text: &str, base_url: &str) -> HlsMaster {
    let mut master = HlsMaster::default();
    let mut lines = text.lines().map(str::trim).peekable();
    while let Some(line) = lines.next() {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
            // The variant URI is the next non-empty, non-comment line.
            let mut uri = None;
            while let Some(next) = lines.next() {
                if !next.is_empty() && !next.starts_with('#') {
                    uri = Some(resolve(base_url, next));
                    break;
                }
            }
            let Some(uri) = uri else { continue };
            let (width, height) = map
                .get("RESOLUTION")
                .map(|r| parse_resolution(r))
                .unwrap_or((None, None));
            master.variants.push(HlsVariant {
                uri,
                bandwidth: map.get("BANDWIDTH").and_then(|v| v.parse().ok()),
                width,
                height,
                codecs: map.get("CODECS").map(|s| s.to_string()),
                fps: map.get("FRAME-RATE").and_then(|v| v.parse().ok()),
                audio_group: map.get("AUDIO").map(|s| s.to_string()),
            });
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
            let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
            if map.get("TYPE") == Some(&"AUDIO") {
                if let (Some(group_id), Some(uri)) = (map.get("GROUP-ID"), map.get("URI")) {
                    master.audio.push(HlsAudioGroup {
                        group_id: group_id.to_string(),
                        uri: resolve(base_url, uri),
                        name: map.get("NAME").map(|s| s.to_string()),
                    });
                }
            }
        }
    }
    master
}

/// Parse an HLS media playlist into its segment URLs (resolved
/// against `base_url`). Refuses encrypted playlists (EXT-X-KEY)
/// and playlists with no segments.
pub fn parse_hls_segments(text: &str, base_url: &str) -> Result<Vec<String>, String> {
    if text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let segments: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| resolve(base_url, l))
        .collect();
    if segments.is_empty() {
        return Err("Playlist contains no media segments".to_string());
    }
    Ok(segments)
}

/// Sum the `#EXTINF:<seconds>,` durations of a media playlist.
fn hls_duration_secs(text: &str) -> Option<u64> {
    let mut total = 0.0f64;
    let mut any = false;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("#EXTINF:") {
            if let Some(secs) = rest.split(',').next().and_then(|s| s.parse::<f64>().ok()) {
                total += secs;
                any = true;
            }
        }
    }
    if any {
        Some(total as u64)
    } else {
        None
    }
}

/// The default HLS variant for the no-UI path (and the picker's
/// pre-selection): highest resolution at or below 1080p, else the
/// highest resolution; without resolution info, the highest
/// bandwidth.
fn pick_default_variant<'a>(variants: &'a [HlsVariant]) -> Option<&'a HlsVariant> {
    let best = |max_height: Option<u32>| {
        variants
            .iter()
            .filter(|v| max_height.is_none_or(|m| v.height.unwrap_or(0) <= m))
            .max_by(|a, b| {
                a.height
                    .unwrap_or(0)
                    .cmp(&b.height.unwrap_or(0))
                    .then(a.bandwidth.unwrap_or(0).cmp(&b.bandwidth.unwrap_or(0)))
            })
    };
    best(Some(1080)).or_else(|| best(None))
}

/// List the qualities of an HLS manifest: one format per variant
/// (or one format for a plain media playlist). The duration is
/// read from the default variant's playlist when it can be
/// fetched; a failure there leaves the duration unknown rather
/// than failing the whole listing.
pub fn list_hls_formats(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let client = hls_client()?;
    let text = manifest_text(&client, &manifest_url, referer, captured)?;
    if text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let master = parse_hls_master(&text, &manifest_base_url(&manifest_url));
    let title = title_from_manifest_url(&manifest_url);
    if master.variants.is_empty() {
        // Media playlist: validate it has segments, then one format.
        let segments = parse_hls_segments(&text, &manifest_base_url(&manifest_url))?;
        return Ok(StreamInfo {
            kind: StreamKind::Hls,
            title,
            duration_secs: hls_duration_secs(&text),
            manifest_url: manifest_url.clone(),
            formats: vec![StreamFormat {
                id: manifest_url,
                ext: "ts".to_string(),
                is_video: true,
                is_audio: true,
                height: None,
                width: None,
                abr_kbps: None,
                vbr_kbps: None,
                fps: None,
                filesize: None,
                needs_merge: false,
            }],
            segment_count: Some(segments.len() as u64),
        });
    }
    let formats = master
        .variants
        .iter()
        .map(|v| {
            let separate_audio = v.audio_group.is_some();
            StreamFormat {
                id: v.uri.clone(),
                // A variant merged with separate audio lands as mp4;
                // a muxed variant stays in its native .ts container.
                ext: if separate_audio { "mp4" } else { "ts" }.to_string(),
                is_video: true,
                is_audio: !separate_audio,
                height: v.height,
                width: v.width,
                abr_kbps: None,
                vbr_kbps: v.bandwidth.map(|b| b as f64 / 1000.0),
                fps: v.fps,
                filesize: None,
                needs_merge: separate_audio,
            }
        })
        .collect();
    // Duration + segment count from the default variant's
    // playlist (best effort): one fetch serves both, so the plan
    // knows the piece count before the download starts.
    let default_uri = pick_default_variant(&master.variants).map(|v| v.uri.clone());
    let default_text = default_uri
        .as_deref()
        .and_then(|uri| manifest_text(&client, uri, referer, captured).ok());
    let duration_secs = default_text.as_deref().and_then(hls_duration_secs);
    let segment_count = match (&default_uri, &default_text) {
        (Some(uri), Some(text)) => parse_hls_segments(text, &manifest_base_url(uri))
            .ok()
            .map(|s| s.len() as u64),
        _ => None,
    };
    Ok(StreamInfo {
        kind: StreamKind::Hls,
        title,
        duration_secs,
        manifest_url,
        formats,
        segment_count,
    })
}

// ---------------------------------------------------------------------------
// Segment downloading (shared by HLS and DASH)
// ---------------------------------------------------------------------------

/// One media segment: a URL, optionally a single byte range of it
/// (DASH SegmentList `@mediaRange`).
#[derive(Debug, Clone)]
struct Segment {
    url: String,
    range: Option<(u64, u64)>,
}

/// Download `segments` in order, appending them to `part_path`.
/// Retries each segment up to 3 times; honours pause/cancel
/// (returning the `__PAUSED__` / `__CANCELLED__` sentinels the
/// workers understand). Resume: a sidecar file next to the `.part`
/// records how many segments completed and the URL of the next one;
/// on a later run the download continues from there instead of
/// starting over — but only if the manifest still lists that same
/// segment at that position, so a changed manifest restarts cleanly
/// instead of assembling a corrupt file. Returns the total bytes in
/// the part file when done. `progress_base` offsets the reported
/// bytes (a DASH audio phase reports after the video bytes).
#[allow(clippy::too_many_arguments)]
/// Does this segment-fetch error look like a timeout (the server
/// never answered) rather than a fast refusal (HTTP error,
/// connection refused)? Timeout errors are the silent ones: each
/// attempt burns the whole fetch timeout, so at 0 bytes they fail
/// fast instead of retrying into minutes of silence (see
/// [`should_retry_segment`]).
fn looks_like_timeout(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("timed out") || e.contains("timeout") || e.contains("deadline exceeded")
}

/// Retry decision for a failed segment attempt (`attempt` is
/// 0-based; 3 attempts total normally) — except a timeout before
/// ANY byte has downloaded fails immediately. At 0 bytes a timeout
/// means the stream endpoint is not answering at all (wrong URL,
/// dead CDN); retrying just burns minutes of silence, and the user
/// can resume. Mid-download timeouts keep all 3 attempts: a
/// transient blip must not kill a movie that is already flowing.
fn should_retry_segment(attempt: usize, downloaded_so_far: u64, err: &str) -> bool {
    if downloaded_so_far == 0 && looks_like_timeout(err) {
        return false;
    }
    attempt < 2
}

/// Is this segment-fetch failure an HTTP 429 (rate limit)? The
/// server is telling us to slow down, not that the segment is
/// gone — it gets backoff + throttling, never the fail-fast or
/// the plain retry.
fn is_rate_limit_fetch(err: &SegmentFetchError) -> bool {
    err.status == Some(429)
}

/// Is this download error a 429 segment failure (the form
/// `download_segments` returns)? The fallback gate in
/// [`download_segments`]: a 429 gets the throttled rescue pass,
/// a timeout gets the plain sequential pass, anything else fails
/// at once.
fn is_rate_limit_error(err: &str, paused: &AtomicBool, cancelled: &AtomicBool) -> bool {
    if paused.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
        return false;
    }
    if err == "__PAUSED__" || err == "__CANCELLED__" {
        return false;
    }
    err.starts_with("Segment ") && err.contains("HTTP 429")
}

/// Backoff before retrying a 429'd segment (`attempt` is 0-based,
/// per segment — a shared backoff step would let one burst of 4
/// simultaneous 429s escalate the wait 4×): the server's
/// `Retry-After` when it sent one, else exponential 5s → 10s →
/// 20s → 40s → 60s (capped). A 429 is a rate limit, not a dead
/// server — the wait is what makes the retry succeed.
fn rate_limit_backoff(attempt: usize, retry_after: Option<Duration>) -> Duration {
    if let Some(d) = retry_after {
        return d.min(Duration::from_secs(60));
    }
    Duration::from_secs(5u64.saturating_mul(2u64.saturating_pow(attempt.min(4) as u32)))
        .min(Duration::from_secs(60))
}

/// Sleep that a pause/cancel can interrupt (checks every 100 ms):
/// a 60 s backoff must not ignore the user's Cancel for a minute.
/// Returns false when interrupted.
fn sleep_interruptible(dur: Duration, paused: &AtomicBool, cancelled: &AtomicBool) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if paused.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

/// How many consecutive 429s (no successful segment between) end
/// a pass: the server is clamped down, not merely busy. Five is
/// the bound the 2026-11-23 nime evidence supports — the audio
/// phase 429'd after ~120 segments per run, so a pass that cannot
/// land one segment in five tries is not going to recover by
/// hammering. A resume starts a fresh budget.
const RATE_LIMIT_MAX_CONSECUTIVE_429: u32 = 5;

/// First inter-request delay in the throttled pass (250 ms ≈ 4
/// requests/sec), and its cap (2 s ≈ 0.5 requests/sec). The delay
/// doubles on each 429 in the throttled pass until it reaches the
/// cap — it adapts to whatever the server's limit is, instead of
/// assuming one magic rate.
const RATE_LIMIT_INITIAL_DELAY: Duration = Duration::from_millis(250);
const RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(2);

/// Rate-limit state shared by one `download_segments` call (one
/// phase: one HLS variant, or one DASH video/audio phase) across
/// its parallel pass and its throttled rescue pass. The video and
/// audio phases of a DASH download each get their own limiter —
/// the phase that 429s throttles from its first 429, which is
/// what the nime evidence needs (the video phase never 429'd).
///
/// The limiter is deliberately small: pacing (a shared
/// next-request slot), the last `Retry-After` (the rescue pass's
/// initial backoff), and the consecutive-429 count (the failure
/// bound). Backoff itself is per-segment-attempt (see
/// [`rate_limit_backoff`]) — a shared backoff step would let one
/// burst of 4 simultaneous 429s escalate the wait 4×.
struct RateLimiter {
    state: Mutex<RateLimiterState>,
}

struct RateLimiterState {
    /// Pacing on? Only in the throttled rescue pass; the parallel
    /// pass runs unpaced (a single 429 blip must not slow a whole
    /// download — the in-loop backoff handles blips).
    pacing: bool,
    /// Current inter-request delay (zero while unpaced).
    delay: Duration,
    /// Earliest start of the next request (pacing slot).
    next_allowed: Instant,
    /// Last `Retry-After` any 429 carried (the rescue pass's
    /// initial backoff); None means the 5 s default.
    last_retry_after: Option<Duration>,
    /// 429s since the last successful segment.
    consecutive_429s: u32,
}

impl RateLimiter {
    fn new() -> Self {
        RateLimiter {
            state: Mutex::new(RateLimiterState {
                pacing: false,
                delay: Duration::ZERO,
                next_allowed: Instant::now(),
                last_retry_after: None,
                consecutive_429s: 0,
            }),
        }
    }

    /// Is this limiter in the throttled rescue pass?
    fn is_throttled(&self) -> bool {
        self.state.lock().unwrap().pacing
    }

    /// Record a 429: remember its `Retry-After` and count it.
    /// Returns the new consecutive count (the caller fails the
    /// pass at [`RATE_LIMIT_MAX_CONSECUTIVE_429`]).
    fn note_429(&self, retry_after: Option<Duration>) -> u32 {
        let mut s = self.state.lock().unwrap();
        if retry_after.is_some() {
            s.last_retry_after = retry_after;
        }
        s.consecutive_429s += 1;
        s.consecutive_429s
    }

    /// Record a successful segment: the 429 streak is over.
    /// (The delay is NOT reset — once throttled, a download stays
    /// throttled for the rest of the pass.)
    fn note_success(&self) {
        self.state.lock().unwrap().consecutive_429s = 0;
    }

    /// How long the rescue pass waits before its first request:
    /// the last `Retry-After` the parallel pass saw, else 5 s.
    fn initial_backoff(&self) -> Duration {
        self.state
            .lock()
            .unwrap()
            .last_retry_after
            .unwrap_or(Duration::from_secs(5))
            .min(Duration::from_secs(60))
    }

    /// Switch to the throttled rescue pass: pacing on at the
    /// initial delay, with a FRESH 429 budget — the mode switch
    /// (1 worker + pacing + initial backoff) is itself the
    /// response to the parallel pass's 429s, so they must not
    /// count against the rescue.
    fn begin_throttled_pass(&self) {
        let mut s = self.state.lock().unwrap();
        s.pacing = true;
        s.delay = RATE_LIMIT_INITIAL_DELAY;
        s.next_allowed = Instant::now();
        s.consecutive_429s = 0;
    }

    /// In the throttled pass, a 429 proves the current delay is
    /// still too short: double it (capped). No-op while unpaced.
    fn escalate_delay(&self) {
        let mut s = self.state.lock().unwrap();
        if s.pacing {
            s.delay = (s.delay * 2).min(RATE_LIMIT_MAX_DELAY);
        }
    }

    /// Pacing: wait until this request's slot, then book the next
    /// slot one delay later. With 1 worker this is just "one
    /// request per delay"; the shared slot is what would serialize
    /// several workers. No-op while unpaced. Returns false when
    /// pause/cancel interrupted the wait.
    fn wait_before_request(&self, paused: &AtomicBool, cancelled: &AtomicBool) -> bool {
        let slot = {
            let mut s = self.state.lock().unwrap();
            if !s.pacing || s.delay.is_zero() {
                return true;
            }
            let now = Instant::now();
            let slot = s.next_allowed.max(now);
            s.next_allowed = slot + s.delay;
            slot
        };
        let now = Instant::now();
        if slot > now {
            sleep_interruptible(slot - now, paused, cancelled)
        } else {
            true
        }
    }
}

/// Stall watchdog for segment downloads: no completed segment for
/// `timeout` means the stream is dead — seen 2026-11-22 as a
/// paired HLS download sitting at 0% for 5+ minutes with no
/// error, because the yt-dlp watchdog never covered this path.
/// Fails with a clear reason instead of hanging.
fn check_stall(last_progress: Instant, timeout: Duration) -> Result<(), String> {
    if last_progress.elapsed() > timeout {
        let span = if timeout.as_secs() >= 60 {
            format!("{} minutes", timeout.as_secs() / 60)
        } else {
            format!("{} seconds", timeout.as_secs())
        };
        return Err(format!(
            "The download stalled: no progress for {span}. The server may be too slow, the link may have expired, or the stream may need a different quality. Try again, or pick another quality."
        ));
    }
    Ok(())
}

/// Estimated total size of an HLS/DASH download, from the
/// measured average segment size so far times the segment count:
/// `downloaded_bytes × total_segments / completed_segments`.
/// None until at least one segment has completed (no average to
/// extrapolate from). An estimate, never exact — segment sizes
/// vary (keyframes, silence), so the UI shows it with "~".
/// Sanity check against the nime download (2026-11-22): 3,321
/// pieces, ~1.5 GB total, ~450 KB per piece.
pub fn estimate_total_bytes(
    downloaded: u64,
    done: u64,
    total: u64,
) -> Option<u64> {
    if done == 0 || total == 0 {
        return None;
    }
    // u128: downloaded (≤ ~10 GB) × total (≤ ~100k segments)
    // fits u64 in practice, but saturate rather than overflow.
    let est = (downloaded as u128 * total as u128) / done as u128;
    Some(u64::try_from(est).unwrap_or(u64::MAX))
}

/// Directory holding one segment file per downloaded segment for
/// the parallel path: `<part>.seg/` next to the `.part` file.
/// Distinct from the legacy sidecar FILE (`<part>.segments`) so a
/// download started by an older build keeps its own resume state.
fn seg_dir_for(part_path: &Path) -> PathBuf {
    part_path.with_file_name(format!(
        "{}.seg",
        part_path.file_name().unwrap().to_string_lossy()
    ))
}

fn seg_file_name(index: usize) -> String {
    format!("{index:06}.seg")
}

/// Meta file inside the segment dir, validating a resume: the
/// manifest must still list the same number of segments with the
/// same first segment, or the saved pieces belong to a different
/// playlist and the download restarts cleanly instead of
/// assembling a corrupt file.
fn read_seg_meta(seg_dir: &Path, segments: &[Segment]) -> bool {
    let Ok(saved) = std::fs::read_to_string(seg_dir.join("meta")) else {
        return false;
    };
    let mut count: Option<usize> = None;
    let mut first: Option<String> = None;
    for line in saved.lines() {
        if let Some(v) = line.strip_prefix("count=") {
            count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("first=") {
            first = Some(v.to_string());
        }
    }
    match (count, first) {
        (Some(n), Some(f)) => {
            n == segments.len() && segments.first().is_some_and(|s| s.url == f)
        }
        _ => false,
    }
}

fn write_seg_meta(seg_dir: &Path, segments: &[Segment]) {
    let first = segments.first().map(|s| s.url.as_str()).unwrap_or("");
    let _ = std::fs::write(
        seg_dir.join("meta"),
        format!("count={}\nfirst={}\n", segments.len(), first),
    );
}

/// Download `segments` into `part_path`, in parallel over the
/// download's shared client (see [`hls_client`]).
///
/// Parallelism (study §A5): `workers` workers pull
/// segment indices from a shared counter, each fetching its
/// segment with the same 3-attempt retry rule as the old
/// sequential engine (5 attempts for HTTP 429, with backoff —
/// see [`RateLimiter`]), writing it to its own file in the segment
/// dir (`<part>.seg/NNNNNN.seg`, via a `.tmp` + atomic rename, so
/// a file's existence means the segment is complete). When every
/// segment is on disk the files are concatenated in playlist
/// order into the `.part` file — the output is byte-identical to
/// the sequential engine's, verified by test. Progress reports
/// carry the segment counts and the running size estimate (see
/// [`estimate_total_bytes`]). With `workers == 1` this is the
/// round-7 sequential fallback: same code, same segment dir, one
/// connection at a time; the 429 rescue pass additionally paces
/// its requests through the limiter (see [`download_segments`]).
///
/// Resume: a segment dir whose meta still matches the manifest
/// (same count, same first segment) keeps its completed pieces;
/// only missing segments are fetched. A stale dir is wiped and
/// the download restarts.
///
/// Legacy resume: a download started by an older build (a
/// `<part>.segments` sidecar FILE next to an existing `.part`
/// file — one concatenated prefix, not per-segment files) cannot
/// be split back into pieces, so it finishes on the sequential
/// path ([`download_segments_sequential`]) instead of restarting
/// from zero. That path is also what a paused parallel download
/// never needs: parallel downloads resume through the segment
/// dir, never the sidecar.
///
/// Failure rules (kept from the sequential engine): a segment
/// that exhausts its retries fails the run with its error — the
/// file cannot be assembled without it. A timeout before ANY byte
/// has downloaded fails fast (no retry — see
/// [`should_retry_segment`]). The stall watchdog (no completed
/// segment for `stall_timeout`) still bounds the whole run.
/// Pause/cancel return the `__PAUSED__` / `__CANCELLED__`
/// sentinels the workers understand; cancel deletes the part
/// file, the segment dir and the sidecar, pause keeps everything
/// for resume.
///
/// Two rescue passes: a parallel run that fails a segment on a
/// TIMEOUT gets one plain sequential retry (Fix 8 — a CDN
/// throttling concurrency); a run that fails on HTTP 429 gets one
/// THROTTLED sequential retry (1 worker, paced requests, adaptive
/// delay — see [`RateLimiter`]). A 429 is a rate limit, not a
/// concurrency limit: sequential alone, at ~20 requests/sec on
/// tiny segments, would 429 again. Both rescues reuse the same
/// segment dir, so landed pieces are never re-fetched, and if a
/// rescue also fails the ORIGINAL error stands.
#[allow(clippy::too_many_arguments)]
fn download_segments(
    client: &reqwest::blocking::Client,
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
) -> Result<u64, String> {
    if segments.is_empty() {
        std::fs::File::create(part_path).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    // Legacy resume (see the doc comment above): a sidecar FILE
    // plus an existing .part means an older build's concatenated
    // prefix — finish it sequentially.
    let sidecar = part_path.with_file_name(format!(
        "{}.segments",
        part_path.file_name().unwrap().to_string_lossy()
    ));
    if sidecar.is_file() && part_path.is_file() {
        return download_segments_sequential(
            client,
            segments,
            part_path,
            referer,
            progress_base,
            progress,
            paused,
            cancelled,
            stall_timeout,
        );
    }
    let limiter = RateLimiter::new();
    match download_segments_parallel(
        client,
        segments,
        part_path,
        referer,
        progress_base,
        progress,
        paused,
        cancelled,
        stall_timeout,
        HLS_SEGMENT_WORKERS,
        &limiter,
    ) {
        Ok(n) => Ok(n),
        Err(e) if should_fallback_sequential(&e, paused, cancelled) => {
            // Round 7: a parallel run that fails a segment on a
            // TIMEOUT gets one sequential retry. totallyacdn
            // (atlantic, 2026-11-23) timed out segment 2 with 4
            // parallel workers + browser headers + HTTP/1.1 — the
            // CDN appears to throttle concurrent connections from
            // one client. The retry reuses the same segment dir,
            // so pieces the parallel pass already landed are not
            // re-fetched, and it is bounded by the same fail-fast
            // and stall rules as any other pass. If the retry also
            // fails, the ORIGINAL error stands: the sequential pass
            // was a rescue attempt, and it must never rewrite the
            // failure's diagnosis (e.g. a server that died between
            // passes would otherwise turn a timeout into a
            // "connection refused").
            match download_segments_parallel(
                client,
                segments,
                part_path,
                referer,
                progress_base,
                progress,
                paused,
                cancelled,
                stall_timeout,
                1,
                &limiter,
            ) {
                Ok(n) => Ok(n),
                Err(_) => Err(e),
            }
        }
        Err(e) if is_rate_limit_error(&e, paused, cancelled) => {
            // Round 7 follow-up: the parallel pass failed on HTTP
            // 429 — the server is rate-limiting REQUESTS, not
            // connections (nime DASH audio, 2026-11-23: 2,116 tiny
            // audio segments at 4 parallel workers made ~40-80
            // requests/sec and 429'd after ~120, while the video
            // phase's big segments never did). One more worker
            // would not help; slowing down does. The rescue pass
            // runs 1 worker, paced (250 ms between requests,
            // doubling on each further 429 to a 2 s cap), after an
            // initial backoff from the last Retry-After (or 5 s).
            // It reuses the same segment dir, so pieces the
            // parallel pass landed are not re-fetched. If the
            // rescue also fails, the ORIGINAL 429 error stands.
            limiter.begin_throttled_pass();
            match download_segments_parallel(
                client,
                segments,
                part_path,
                referer,
                progress_base,
                progress,
                paused,
                cancelled,
                stall_timeout,
                1,
                &limiter,
            ) {
                Ok(n) => Ok(n),
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Retry a failed parallel segment run sequentially? Only for a
/// SEGMENT timeout: the server stopped answering mid-run, which a
/// single connection may survive (a CDN throttling concurrency).
/// Never for pause/cancel (the user asked to stop), a stall (the
/// run already burned the whole stall timeout — a second pass
/// would just burn it again), or a fast refusal (HTTP 4xx/5xx, DNS
/// — sequential will fail the same way, so fail at once).
fn should_fallback_sequential(err: &str, paused: &AtomicBool, cancelled: &AtomicBool) -> bool {
    if paused.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
        return false;
    }
    if err == "__PAUSED__" || err == "__CANCELLED__" {
        return false;
    }
    // Segment failures are phrased "Segment N of M could not be
    // downloaded..."; anything else (stall, assembly, IO) is not a
    // segment timeout and gets no retry.
    err.starts_with("Segment ") && looks_like_timeout(err)
}

#[allow(clippy::too_many_arguments)]
fn download_segments_parallel(
    client: &reqwest::blocking::Client,
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
    workers: usize,
    limiter: &RateLimiter,
) -> Result<u64, String> {
    let total = segments.len() as u64;
    let seg_dir = seg_dir_for(part_path);
    // Resume validation: a stale dir (different manifest) is
    // wiped; a matching one keeps its completed pieces.
    if seg_dir.exists() && !read_seg_meta(&seg_dir, segments) {
        let _ = std::fs::remove_dir_all(&seg_dir);
    }
    if seg_dir.is_file() {
        let _ = std::fs::remove_file(&seg_dir);
    }
    std::fs::create_dir_all(&seg_dir).map_err(|e| e.to_string())?;
    write_seg_meta(&seg_dir, segments);
    // Count resumed pieces (file presence = complete, by the
    // atomic-rename rule) and their bytes.
    let mut resumed_done: u64 = 0;
    let mut resumed_bytes: u64 = 0;
    for i in 0..segments.len() {
        if let Ok(meta) = std::fs::metadata(seg_dir.join(seg_file_name(i))) {
            resumed_done += 1;
            resumed_bytes += meta.len();
        }
    }
    // Throttled rescue pass (see download_segments): wait out the
    // last Retry-After (or 5 s) before the first request — the
    // server just told us to slow down, and the rescue's first
    // request must not land in the same rate-limit window. Before
    // any clock starts, so the wait never trips the stall
    // watchdog.
    if limiter.is_throttled() && !sleep_interruptible(limiter.initial_backoff(), paused, cancelled)
    {
        if cancelled.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(part_path);
            let _ = std::fs::remove_dir_all(&seg_dir);
            return Err("__CANCELLED__".to_string());
        }
        return Err("__PAUSED__".to_string());
    }
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let downloaded = AtomicU64::new(resumed_bytes);
    let done = AtomicUsize::new(resumed_done as usize);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let last_progress = Mutex::new(Instant::now());
    let report = |progress: &(dyn Fn(Progress) + Send + Sync)| {
        let dl = downloaded.load(Ordering::Relaxed);
        let dn = done.load(Ordering::Relaxed) as u64;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        progress(Progress {
            downloaded: progress_base + dl,
            total: None,
            bytes_per_sec: dl as f64 / elapsed,
            segments_done: Some(dn),
            segments_total: Some(total),
            // The estimate is COMBINED: completed phases contribute
            // their exact bytes (progress_base) and the current
            // phase its measured estimate. A per-phase estimate
            // here made the UI show 1324% on a nime DASH download
            // (2.2 GB cumulative bytes against a ~166 MB
            // audio-phase estimate, 2026-11-23).
            estimated_total: estimate_total_bytes(dl, dn, total)
                .map(|e| progress_base.saturating_add(e)),
        });
    };
    // Report the resumed state at once, so a resumed download
    // does not sit at 0 pieces until the next segment lands.
    report(progress);
    // Workers run as `move` closures (each owns its clone of the
    // shared client); the shared counters are borrowed through
    // these references, so every worker sees the same state.
    let next = &next;
    let downloaded = &downloaded;
    let done = &done;
    let stop = &stop;
    let first_error = &first_error;
    let last_progress = &last_progress;
    let seg_dir = &seg_dir;
    let report = &report;
    std::thread::scope(|scope| {
        // `workers` is HLS_SEGMENT_WORKERS normally; the round-7
        // sequential fallback re-runs this same function with 1
        // worker over the same segment dir (completed pieces are
        // reused, never re-fetched).
        for _ in 0..workers.max(1) {
            let client = client.clone();
            scope.spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if cancelled.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                // Stall watchdog (see check_stall): checked before
                // every segment, so a dead run fails plainly
                // instead of sitting at 0% forever.
                {
                    let last = *last_progress.lock().unwrap();
                    if let Err(e) = check_stall(last, stall_timeout) {
                        let mut slot = first_error.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= segments.len() {
                    return;
                }
                // Already on disk from an earlier run: counted in
                // the resume scan, nothing to fetch.
                if seg_dir.join(seg_file_name(i)).is_file() {
                    continue;
                }
                let seg = &segments[i];
                // Retry transient failures; a timeout at 0 bytes
                // fails fast (see should_retry_segment) — the
                // shared counter is what "0 bytes" means here.
                // 429s get 5 attempts (the rate-limit budget —
                // see RateLimiter); everything else keeps 3 (the
                // retry gate below breaks earlier for non-429s).
                let mut bytes = None;
                let mut last_err = String::new();
                let mut attempts = 0;
                for attempt in 0..5 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    // Stall watchdog, inside the retry loop (same
                    // placement as the sequential path): a server
                    // that answers slowly but never delivers a
                    // segment must be caught between attempts, not
                    // after all three retries have burned the
                    // timeout several times over. last_progress is
                    // shared: any worker completing a segment
                    // resets the clock for all of them.
                    if let Err(e) = check_stall(
                        *last_progress.lock().unwrap(),
                        stall_timeout,
                    ) {
                        let mut slot = first_error.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                    // Pacing (throttled rescue pass only): one
                    // request per delay, across all workers.
                    if !limiter.wait_before_request(paused, cancelled) {
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                    attempts = attempt + 1;
                    match fetch_segment(&client, &seg.url, seg.range, referer) {
                        Ok(b) => {
                            limiter.note_success();
                            bytes = Some(b);
                            break;
                        }
                        Err(e) => {
                            last_err = e.message.clone();
                            if is_rate_limit_fetch(&e) {
                                // HTTP 429: back off (the server's
                                // Retry-After, else exponential)
                                // and — in the throttled pass —
                                // slow the whole download down.
                                // Five consecutive 429s with no
                                // success between end the pass:
                                // the server is clamped, and
                                // hammering only extends the ban.
                                let consecutive =
                                    limiter.note_429(e.retry_after);
                                limiter.escalate_delay();
                                if consecutive >= RATE_LIMIT_MAX_CONSECUTIVE_429
                                    || attempt >= 4
                                {
                                    break;
                                }
                                if !sleep_interruptible(
                                    rate_limit_backoff(attempt, e.retry_after),
                                    paused,
                                    cancelled,
                                ) {
                                    stop.store(true, Ordering::Relaxed);
                                    return;
                                }
                            } else {
                                if !should_retry_segment(
                                    attempt,
                                    downloaded.load(Ordering::Relaxed),
                                    &last_err,
                                ) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(
                                    500 * (attempt + 1) as u64,
                                ));
                            }
                        }
                    }
                }
                let Some(bytes) = bytes else {
                    // This segment is persistently failing. The
                    // file cannot be assembled without it, so any
                    // persistent failure ends the run — the same
                    // rule as the sequential path. At 0 bytes this
                    // is the fail-fast: the download ends on the
                    // first persistent failure instead of waiting
                    // out the stall timeout. The saved pieces stay
                    // in the segment dir for resume.
                    let tries = if attempts == 1 { "try" } else { "tries" };
                    let mut slot = first_error.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(format!(
                            "Segment {} of {} could not be downloaded after {attempts} {tries}. ({last_err})",
                            i + 1,
                            segments.len()
                        ));
                    }
                    stop.store(true, Ordering::Relaxed);
                    continue;
                };
                // Atomic completion marker: write temp, then
                // rename, so a file's existence means the segment
                // is whole (a crash mid-write leaves only a .tmp).
                let tmp_path = seg_dir.join(format!("{}.tmp", seg_file_name(i)));
                let final_path = seg_dir.join(seg_file_name(i));
                if let Err(e) = (|| -> Result<(), String> {
                    use std::io::Write;
                    let mut f = std::fs::File::create(&tmp_path)
                        .map_err(|e| e.to_string())?;
                    f.write_all(&bytes).map_err(|e| e.to_string())?;
                    drop(f);
                    std::fs::rename(&tmp_path, &final_path).map_err(|e| e.to_string())?;
                    Ok(())
                })() {
                    let mut slot = first_error.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                done.fetch_add(1, Ordering::Relaxed);
                *last_progress.lock().unwrap() = Instant::now();
                report(progress);
            });
        }
    });
    // A fetch error beats pause/cancel here (the run genuinely
    // failed); cancel beats pause (the user asked for the files to
    // go). The segment dir is kept on failure and pause, so a
    // later resume continues from the completed pieces.
    if let Some(err) = first_error.lock().unwrap().take() {
        return Err(err);
    }
    if cancelled.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(part_path);
        let _ = std::fs::remove_dir_all(&seg_dir);
        return Err("__CANCELLED__".to_string());
    }
    if paused.load(Ordering::Relaxed) {
        return Err("__PAUSED__".to_string());
    }
    // Assembly: stream-concatenate the segment files in playlist
    // order (io::copy per file — no whole-file reads into memory).
    // Every index must be present: a missing file here means a
    // worker exited without recording its failure, which must
    // never silently produce a short file.
    {
        use std::io::Write;
        let mut out = std::fs::File::create(part_path).map_err(|e| e.to_string())?;
        for i in 0..segments.len() {
            let path = seg_dir.join(seg_file_name(i));
            let mut f = std::fs::File::open(&path).map_err(|_| {
                format!(
                    "Segment {} of {} is missing after download; the download will retry from the saved pieces.",
                    i + 1,
                    segments.len()
                )
            })?;
            std::io::copy(&mut f, &mut out).map_err(|e| e.to_string())?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    let total_bytes = downloaded.load(Ordering::Relaxed);
    // The segment dir is KEPT after a successful assembly: a
    // multi-phase download (HLS pair, DASH video+audio) can still
    // fail in a LATER phase, and its resume must not re-fetch this
    // completed phase (nime DASH, 2026-11-23: the video phase was
    // re-downloaded in full after the audio phase failed with
    // HTTP 429, because the old code deleted the pieces here).
    // The caller removes the dir when the final file is produced
    // (see remove_seg_dir); "Clear cache" sweeps orphaned dirs.
    Ok(total_bytes)
}

/// Best-effort removal of one phase's segment dir, called when
/// the final file is produced (rename or merge): the pieces are
/// no longer needed, and leaving thousands of files behind per
/// download would litter the destination. Called only on
/// SUCCESS — if the merge fails, the pieces stay so a resume
/// re-assembles locally instead of re-downloading.
fn remove_seg_dir(part_path: &Path) {
    let _ = std::fs::remove_dir_all(seg_dir_for(part_path));
}

/// The pre-round-6 sequential engine, kept for one purpose:
/// finishing downloads an older build started (a `<part>.segments`
/// sidecar FILE next to an existing `.part` file — one
/// concatenated prefix, not per-segment files). New downloads
/// always take the parallel path. Same retry, stall, pause/cancel
/// and progress rules as before, now over the shared client and
/// reporting segment counts + the size estimate like the parallel
/// path, so a resumed legacy download shows its pieces too.
#[allow(clippy::too_many_arguments)]
fn download_segments_sequential(
    client: &reqwest::blocking::Client,
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
) -> Result<u64, String> {
    let sidecar = part_path.with_file_name(format!(
        "{}.segments",
        part_path.file_name().unwrap().to_string_lossy()
    ));
    // Resume check: sidecar says N done and names the next segment;
    // trust it only if the manifest still agrees.
    let mut start = 0usize;
    let mut downloaded: u64 = 0;
    if let Ok(saved) = std::fs::read_to_string(&sidecar) {
        let mut done: Option<usize> = None;
        let mut next_url: Option<String> = None;
        let mut next_range: Option<(u64, u64)> = None;
        for line in saved.lines() {
            if let Some(v) = line.strip_prefix("done=") {
                done = v.parse().ok();
            } else if let Some(v) = line.strip_prefix("next=") {
                next_url = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("range=") {
                if let Some((s, e)) = v.split_once('-') {
                    if let (Ok(s), Ok(e)) = (s.parse(), e.parse()) {
                        next_range = Some((s, e));
                    }
                }
            }
        }
        if let (Some(n), Some(next)) = (done, next_url) {
            if n < segments.len()
                && segments[n].url == next
                && segments[n].range == next_range
                && part_path.is_file()
            {
                start = n;
                downloaded = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    let mut out = if start > 0 {
        std::fs::OpenOptions::new()
            .append(true)
            .open(part_path)
            .map_err(|e| e.to_string())?
    } else {
        std::fs::File::create(part_path).map_err(|e| e.to_string())?
    };
    let started = Instant::now();
    let mut last_progress = Instant::now();
    let total = segments.len() as u64;
    for (i, seg) in segments.iter().enumerate().skip(start) {
        if cancelled.load(Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(part_path);
            let _ = std::fs::remove_file(&sidecar);
            return Err("__CANCELLED__".to_string());
        }
        if paused.load(Ordering::Relaxed) {
            return Err("__PAUSED__".to_string());
        }
        // Stall watchdog: no completed segment for the whole
        // timeout means the stream is dead — fail plainly instead
        // of sitting at 0% forever (see check_stall).
        check_stall(last_progress, stall_timeout)?;
        // Retry transient failures (a dropped segment must not fail
        // a whole stream); a persistent failure keeps the .part and
        // sidecar so a later resume continues from here. A timeout
        // at 0 bytes fails fast (see should_retry_segment).
        let mut bytes = None;
        let mut last_err = String::new();
        let mut attempts = 0;
        for attempt in 0..3 {
            check_stall(last_progress, stall_timeout)?;
            attempts = attempt + 1;
            match fetch_bytes_range(client, &seg.url, seg.range, referer) {
                Ok(b) => {
                    bytes = Some(b);
                    break;
                }
                Err(e) => {
                    last_err = e;
                    if !should_retry_segment(attempt, downloaded, &last_err) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500 * (attempt + 1) as u64));
                }
            }
        }
        let bytes = match bytes {
            Some(b) => b,
            None => {
                let tries = if attempts == 1 { "try" } else { "tries" };
                return Err(format!(
                    "Segment {} of {} could not be downloaded after {attempts} {tries}. ({last_err})",
                    i + 1,
                    segments.len()
                ));
            }
        };
        use std::io::Write;
        out.write_all(&bytes).map_err(|e| e.to_string())?;
        downloaded += bytes.len() as u64;
        last_progress = Instant::now();
        // Record progress before reporting it, so a crash or pause
        // right after this segment still resumes correctly.
        let mut sidecar_text = format!("done={}\n", i + 1);
        if let Some(next) = segments.get(i + 1) {
            sidecar_text.push_str(&format!("next={}\n", next.url));
            if let Some((s, e)) = next.range {
                sidecar_text.push_str(&format!("range={s}-{e}\n"));
            }
        }
        let _ = std::fs::write(&sidecar, sidecar_text);
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let done_now = (i + 1) as u64;
        progress(Progress {
            downloaded: progress_base + downloaded,
            total: None,
            bytes_per_sec: downloaded as f64 / elapsed,
            segments_done: Some(done_now),
            segments_total: Some(total),
            // Combined estimate, as in the parallel path: the
            // completed phases' exact bytes plus this phase's
            // measured estimate, so the percent never exceeds 100%
            // on a multi-phase download.
            estimated_total: estimate_total_bytes(downloaded, done_now, total)
                .map(|e| progress_base.saturating_add(e)),
        });
    }
    drop(out);
    let _ = std::fs::remove_file(&sidecar);
    Ok(downloaded)
}

fn part_path_for(final_path: &Path) -> PathBuf {
    final_path.with_file_name(format!(
        "{}.part",
        final_path.file_name().unwrap().to_string_lossy()
    ))
}

// ---------------------------------------------------------------------------
// HLS download
// ---------------------------------------------------------------------------

/// Download an HLS playlist into a single file (segments
/// concatenated in playlist order). A master playlist follows the
/// DEFAULT variant (best at or below 1080p, else the best); use
/// [`download_hls_variant`] for an explicitly chosen variant.
/// Returns an error for encrypted playlists. A variant whose audio
/// lives on a separate track needs ffmpeg to merge, which this
/// entry point cannot provide — it fails with a clear error; the
/// workers use `download_hls_variant` with a resolved ffmpeg.
pub fn download_hls(
    manifest_url: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let client = hls_client()?;
    let text = manifest_text(&client, &manifest_url, referer, captured)?;
    if text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let master = parse_hls_master(&text, &manifest_base_url(&manifest_url));
    let variant_uri = match pick_default_variant(&master.variants) {
        Some(v) => v.uri.clone(),
        None => manifest_url.clone(),
    };
    download_hls_variant(
        &manifest_url,
        &variant_uri,
        destination_dir,
        filename,
        referer,
        None,
        progress,
        paused,
        cancelled,
        captured,
    )
}

/// Download one chosen variant of an HLS master playlist (or a
/// plain media playlist, when `variant_uri` is the manifest URL
/// itself). A variant with a separate audio track downloads both
/// tracks and merges them with `ffmpeg` (`.mp4` output); without
/// ffmpeg that variant is refused with a clear error instead of
/// producing a silent video. Muxed variants concatenate straight
/// to `.ts`. `captured` is playlist text the extension captured
/// from the player's own response (see [`CapturedManifest`]):
/// used for the master and the variant playlist when its URL
/// matches, so a signed link is never re-fetched.
#[allow(clippy::too_many_arguments)]
pub fn download_hls_variant(
    manifest_url: &str,
    variant_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    ffmpeg: Option<&Path>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let variant_uri = validate_url(variant_uri)?;
    let client = hls_client()?;
    // The master text tells us whether this variant's audio is a
    // separate track (skip the fetch when the "variant" is the
    // manifest itself — a plain media playlist).
    let audio_uri = if variant_uri != manifest_url {
        let master_text = manifest_text(&client, &manifest_url, referer, captured)?;
        if master_text.contains("#EXT-X-KEY") {
            return Err(
                "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                    .to_string(),
            );
        }
        let master = parse_hls_master(&master_text, &manifest_base_url(&manifest_url));
        master
            .variants
            .iter()
            .find(|v| v.uri == variant_uri)
            .and_then(|v| v.audio_group.clone())
            .and_then(|group| {
                master
                    .audio
                    .iter()
                    .find(|a| a.group_id == group)
                    .map(|a| a.uri.clone())
            })
    } else {
        None
    };
    if audio_uri.is_some() && ffmpeg.is_none() {
        return Err(
            "This quality keeps its audio on a separate track, which needs ffmpeg to merge. \
             Install ffmpeg (or pick a combined quality) and try again."
                .to_string(),
        );
    }
    let variant_text = manifest_text(&client, &variant_uri, referer, captured)?;
    // Segments resolve against the variant playlist's own URL —
    // unwrapped when the variant came through a proxy worker, so
    // they are fetched from the real CDN, not the proxy origin.
    let video_segments = parse_hls_segments(&variant_text, &manifest_base_url(&variant_uri))?
        .into_iter()
        .map(|url| Segment { url, range: None })
        .collect::<Vec<_>>();
    let audio_segments = match &audio_uri {
        Some(uri) => {
            let audio_text = fetch_text(&client, uri, referer)?;
            Some(
                parse_hls_segments(&audio_text, uri)?
                    .into_iter()
                    .map(|url| Segment { url, range: None })
                    .collect::<Vec<_>>(),
            )
        }
        None => None,
    };
    let clean = crate::filename::sanitize_filename(filename);
    std::fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let video_bytes = download_segments(
        &client,
        &video_segments,
        &video_part,
        referer,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    let Some(audio_segments) = audio_segments else {
        std::fs::rename(&video_part, &final_path).map_err(|e| e.to_string())?;
        remove_seg_dir(&video_part);
        return Ok(final_path);
    };
    // Separate audio: download it, then merge (stream copy, no
    // re-encode). The merge itself is seconds; pause/cancel are
    // honoured before and after it by the caller re-checking the
    // flags on the next worker run.
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(&video_part);
        remove_seg_dir(&video_part);
        return Err("__CANCELLED__".to_string());
    }
    if paused.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("__PAUSED__".to_string());
    }
    let audio_part = final_path.with_file_name(format!(
        "{}.audio.part",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    download_segments(
        &client,
        &audio_segments,
        &audio_part,
        referer,
        video_bytes,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    let ffmpeg = ffmpeg.expect("audio merge checked ffmpeg above");
    let merged = crate::ffmpeg::merge_av(ffmpeg, &video_part, &audio_part, &final_path);
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    merged?;
    remove_seg_dir(&video_part);
    remove_seg_dir(&audio_part);
    Ok(final_path)
}

/// Download a PAIRED video + audio variant: two media playlists
/// observed together (no master playlist was available — the
/// extension saw only the variants, e.g. `v0.m3u8` + `audio_1.m3u8`
/// on a streaming site). Both tracks download in full and merge
/// with ffmpeg into one `.mp4`; a video-only variant must never
/// again be delivered as a silent `.ts` when its audio sibling was
/// right there. ffmpeg is required (the caller resolves it when
/// the plan says the download needs a merge). `captured` is
/// playlist text the extension captured for the VIDEO playlist
/// (see [`CapturedManifest`]); the audio playlist is always
/// fetched normally.
#[allow(clippy::too_many_arguments)]
pub fn download_hls_pair(
    video_uri: &str,
    audio_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    ffmpeg: &Path,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    let video_uri = validate_url(video_uri)?;
    let audio_uri = validate_url(audio_uri)?;
    let client = hls_client()?;
    let video_text = manifest_text(&client, &video_uri, referer, captured)?;
    if video_text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let audio_text = fetch_text(&client, &audio_uri, referer)?;
    if audio_text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let video_segments = parse_hls_segments(&video_text, &manifest_base_url(&video_uri))?
        .into_iter()
        .map(|url| Segment { url, range: None })
        .collect::<Vec<_>>();
    let audio_segments = parse_hls_segments(&audio_text, &manifest_base_url(&audio_uri))?
        .into_iter()
        .map(|url| Segment { url, range: None })
        .collect::<Vec<_>>();
    let clean = crate::filename::sanitize_filename(filename);
    std::fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let video_bytes = download_segments(
        &client,
        &video_segments,
        &video_part,
        referer,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(&video_part);
        remove_seg_dir(&video_part);
        return Err("__CANCELLED__".to_string());
    }
    if paused.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("__PAUSED__".to_string());
    }
    let audio_part = final_path.with_file_name(format!(
        "{}.audio.part",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    download_segments(
        &client,
        &audio_segments,
        &audio_part,
        referer,
        video_bytes,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    let merged = crate::ffmpeg::merge_av(ffmpeg, &video_part, &audio_part, &final_path);
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    merged?;
    remove_seg_dir(&video_part);
    remove_seg_dir(&audio_part);
    Ok(final_path)
}


// ---------------------------------------------------------------------------
// DASH parsing (MPD is XML; parsed with quick-xml, not string scans:
// attribute order, self-closing tags and namespace prefixes vary)
// ---------------------------------------------------------------------------

/// One representation (quality) of a DASH manifest.
#[derive(Debug, Clone)]
pub struct DashRepresentation {
    pub id: String,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub codecs: Option<String>,
    /// Effective MIME type (representation, else AdaptationSet).
    pub mime_type: String,
    /// Effective content type ("video" / "audio", else empty).
    pub content_type: String,
    /// Most specific BaseURL (representation > set > period > MPD),
    /// still relative — resolved against the manifest URL at use.
    pub base_url: Option<String>,
    /// SegmentTemplate, representation-level if present, else the
    /// AdaptationSet's. Mutually exclusive with `segment_list`.
    pub template: Option<SegmentTemplate>,
    pub segment_list: Option<DashSegmentList>,
}

impl DashRepresentation {
    fn is_video(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("video")
            || self.mime_type.to_ascii_lowercase().starts_with("video/")
            || (!self.is_audio() && (self.width.is_some() || self.height.is_some()))
    }

    fn is_audio(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("audio")
            || self.mime_type.to_ascii_lowercase().starts_with("audio/")
    }
}

/// A parsed DASH manifest (clear, single-period only).
#[derive(Debug, Clone)]
pub struct DashManifest {
    /// Presentation duration in seconds (MPD mediaPresentationDuration,
    /// else the Period duration), when the manifest states one.
    pub duration_secs: Option<f64>,
    pub representations: Vec<DashRepresentation>,
}

/// A SegmentTemplate element (at AdaptationSet or Representation
/// level), with its SegmentTimeline when present.
#[derive(Debug, Clone, Default)]
pub struct SegmentTemplate {
    pub media: Option<String>,
    pub initialization: Option<String>,
    pub timescale: u64,
    /// Segment duration in `timescale` units (Number-based).
    pub duration: Option<u64>,
    pub start_number: u64,
    pub timeline: Vec<TimelineEntry>,
}

/// One `<S>` element of a SegmentTimeline: `t` is the start time
/// (timescale units; omitted means "previous segment's end"),
/// `d` the duration, `r` how many times it repeats (default 0).
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    pub start: Option<u64>,
    pub duration: u64,
    pub repeat: u32,
}

/// A SegmentList element.
#[derive(Debug, Clone, Default)]
pub struct DashSegmentList {
    pub initialization: Option<ListInit>,
    pub segments: Vec<ListSegment>,
}

/// SegmentList Initialization: sourceURL and/or a byte range of the
/// BaseURL file.
#[derive(Debug, Clone)]
pub struct ListInit {
    pub url: Option<String>,
    pub range: Option<(u64, u64)>,
}

/// One SegmentURL: a media URL, or a byte range of the BaseURL file.
#[derive(Debug, Clone)]
pub struct ListSegment {
    pub media: Option<String>,
    pub range: Option<(u64, u64)>,
}

/// Parse an ISO 8601 duration such as PT634.566S, PT10M30.5S,
/// PT2H3M or P1DT2H into seconds. Years/months/weeks are not
/// supported (they never occur in real manifests) and return None.
fn parse_iso8601_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let rest = s.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    let mut secs = 0.0f64;
    let parse_part = |part: &str, units: &[char]| -> Option<f64> {
        let mut total = 0.0f64;
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else if units.contains(&c) {
                let v: f64 = num.parse().ok()?;
                let mult = match c {
                    'D' => 86_400.0,
                    'H' => 3_600.0,
                    'M' => 60.0,
                    'S' => 1.0,
                    _ => return None,
                };
                total += v * mult;
                num.clear();
            } else {
                return None;
            }
        }
        if !num.is_empty() {
            return None;
        }
        Some(total)
    };
    // Date part: only days are supported (Y/M/W rejected).
    if !date.is_empty() {
        secs += parse_part(date, &['D'])?;
    }
    if !time.is_empty() {
        secs += parse_part(time, &['H', 'M', 'S'])?;
    }
    Some(secs)
}

/// Parse a byte range "start-end" (inclusive) from a mediaRange
/// attribute.
fn parse_byte_range(s: &str) -> Option<(u64, u64)> {
    let (a, b) = s.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: u64 = b.parse().ok()?;
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Substitute a SegmentTemplate URL: $RepresentationID$,
/// $Bandwidth$, $Number$, $Time$, each optionally with a %0Nd
/// zero-padding format, and $$ for a literal $. `number`/`time`
/// are None where the template must not use them (initialization).
fn substitute_template(
    tpl: &str,
    rep_id: &str,
    bandwidth: Option<u64>,
    number: Option<u64>,
    time: Option<u64>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        rest = &rest[dollar..];
        if let Some(after) = rest.strip_prefix("$$") {
            out.push('$');
            rest = after;
            continue;
        }
        let Some(end) = rest[1..].find('$').map(|i| i + 1) else {
            return Err(format!(
                "Bad SegmentTemplate: unterminated $ in {tpl:?}"
            ));
        };
        let token = &rest[1..end];
        rest = &rest[end + 1..];
        let (name, fmt) = match token.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (token, None),
        };
        let value: String = match name {
            "RepresentationID" => rep_id.to_string(),
            "Bandwidth" => bandwidth
                .map(|b| b.to_string())
                .ok_or_else(|| {
                    "SegmentTemplate uses $Bandwidth$ but the Representation has no bandwidth"
                        .to_string()
                })?,
            "Number" => number
                .map(|n| n.to_string())
                .ok_or_else(|| {
                    "SegmentTemplate uses $Number$ but no segment number is available"
                        .to_string()
                })?,
            "Time" => time
                .map(|t| t.to_string())
                .ok_or_else(|| {
                    "SegmentTemplate uses $Time$ but no segment time is available".to_string()
                })?,
            other => {
                return Err(format!(
                    "Unsupported SegmentTemplate variable ${other}$"
                ));
            }
        };
        match fmt {
            None => out.push_str(&value),
            Some(f) => {
                // %0Nd: zero-pad a numeric value to width N.
                let width = f
                    .strip_suffix('d')
                    .and_then(|w| w.parse::<usize>().ok())
                    .unwrap_or(0);
                if width > 0 && value.chars().all(|c| c.is_ascii_digit()) {
                    for _ in 0..width.saturating_sub(value.len()) {
                        out.push('0');
                    }
                }
                out.push_str(&value);
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Parse an MPD document. Refuses DRM (ContentProtection),
/// dynamic (live) manifests, and multi-period manifests with plain
/// explanations instead of producing a broken download.
pub fn parse_dash_manifest(text: &str) -> Result<DashManifest, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut manifest = DashManifest {
        duration_secs: None,
        representations: Vec::new(),
    };
    let mut mpd_base_url: Option<String> = None;
    let mut period_base_url: Option<String> = None;
    let mut period_count = 0usize;
    let mut period_duration: Option<f64> = None;

    // AdaptationSet-level state, inherited by its Representations.
    let mut set_mime = String::new();
    let mut set_content = String::new();
    let mut set_template: Option<SegmentTemplate> = None;
    let mut set_list: Option<DashSegmentList> = None;
    let mut set_base_url: Option<String> = None;

    // Representation being built (None outside one).
    let mut current_rep: Option<RepBuilder> = None;

    // A SegmentTemplate/SegmentList whose children are still being
    // read (Start, not Empty): (belongs to set or rep, value).
    let mut pending_template: Option<(bool, SegmentTemplate)> = None; // bool: true = rep-level
    let mut pending_list: Option<(bool, DashSegmentList)> = None;

    // BaseURL text: which level the open <BaseURL> belongs to.
    let mut base_url_owner: Option<&'static str> = None;
    let mut stack: Vec<String> = Vec::new();

    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Start and Empty are matched separately: the distinction
        // (self-closing vs paired element) decides whether a
        // Representation/SegmentTemplate is finished immediately or
        // on its End event.
        let (e, is_empty) = match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::Text(e)) => {
                if let Some(owner) = base_url_owner.take() {
                    if stack.last().map(String::as_str) == Some("BaseURL") {
                        if let Ok(value) =
                            quick_xml::escape::unescape(&String::from_utf8_lossy(e.as_ref()))
                        {
                            let value = value.trim().to_string();
                            if !value.is_empty() {
                                match owner {
                                    "mpd" => mpd_base_url = Some(value),
                                    "period" => period_base_url = Some(value),
                                    "set" => set_base_url = Some(value),
                                    "rep" => {
                                        if let Some(r) = current_rep.as_mut() {
                                            r.own_base_url = Some(value);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                continue;
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match name.as_str() {
                    "SegmentTemplate" => {
                        if let Some((in_rep, tpl)) = pending_template.take() {
                            if in_rep {
                                if let Some(r) = current_rep.as_mut() {
                                    r.own_template = Some(tpl);
                                }
                            } else {
                                set_template = Some(tpl);
                            }
                        }
                    }
                    "SegmentList" => {
                        if let Some((in_rep, list)) = pending_list.take() {
                            if in_rep {
                                if let Some(r) = current_rep.as_mut() {
                                    r.own_list = Some(list);
                                }
                            } else {
                                set_list = Some(list);
                            }
                        }
                    }
                    "Representation" => {
                        if let Some(rep) = current_rep.take() {
                            manifest.representations.push(finish_rep(
                                rep,
                                &set_mime,
                                &set_content,
                                &set_template,
                                &set_list,
                                &set_base_url,
                                &period_base_url,
                                &mpd_base_url,
                            ));
                        }
                    }
                    "BaseURL" => {
                        base_url_owner = None;
                    }
                    _ => {}
                }
                stack.pop();
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                return Err(format!("Could not parse this DASH manifest as XML ({e})."));
            }
        };
        {
            let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let mut attrs: HashMap<String, String> = HashMap::new();
                for a in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(a.key.local_name().as_ref()).to_string();
                    let value = a
                        .unescape_value()
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    attrs.insert(key, value);
                }
                match name.as_str() {
                    "MPD" => {
                        if attrs
                            .get("type")
                            .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"))
                        {
                            return Err(
                                "This DASH stream is live (type=\"dynamic\"). FetchForge does not record live streams."
                                    .to_string(),
                            );
                        }
                        if let Some(d) = attrs.get("mediaPresentationDuration") {
                            manifest.duration_secs = parse_iso8601_duration(d);
                        }
                    }
                    "Period" => {
                        period_count += 1;
                        if period_count > 1 {
                            return Err(
                                "Multi-period DASH manifests are not supported in this release"
                                    .to_string(),
                            );
                        }
                        if let Some(d) = attrs.get("duration") {
                            period_duration = parse_iso8601_duration(d);
                        }
                    }
                    "BaseURL" => {
                        // Owner = the enclosing element.
                        base_url_owner = match stack.last().map(String::as_str) {
                            Some("MPD") => Some("mpd"),
                            Some("Period") => Some("period"),
                            Some("AdaptationSet") => Some("set"),
                            Some("Representation") => Some("rep"),
                            _ => None,
                        };
                    }
                    "AdaptationSet" => {
                        set_mime = attrs.get("mimeType").cloned().unwrap_or_default();
                        set_content = attrs.get("contentType").cloned().unwrap_or_default();
                        set_template = None;
                        set_list = None;
                        set_base_url = None;
                    }
                    "SegmentTemplate" => {
                        let in_rep = current_rep.is_some();
                        let tpl = SegmentTemplate {
                            media: attrs.get("media").cloned(),
                            initialization: attrs.get("initialization").cloned(),
                            timescale: attrs
                                .get("timescale")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(1),
                            duration: attrs.get("duration").and_then(|v| v.parse().ok()),
                            start_number: attrs
                                .get("startNumber")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(1),
                            timeline: Vec::new(),
                        };
                        if is_empty {
                            if in_rep {
                                if let Some(r) = current_rep.as_mut() {
                                    r.own_template = Some(tpl);
                                }
                            } else {
                                set_template = Some(tpl);
                            }
                        } else {
                            pending_template = Some((in_rep, tpl));
                        }
                    }
                    "S" => {
                        if let Some((_, tpl)) = pending_template.as_mut() {
                            if let Some(d) = attrs.get("d").and_then(|v| v.parse::<u64>().ok()) {
                                tpl.timeline.push(TimelineEntry {
                                    start: attrs.get("t").and_then(|v| v.parse().ok()),
                                    duration: d,
                                    repeat: attrs
                                        .get("r")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0),
                                });
                            }
                        }
                    }
                    "SegmentList" => {
                        let in_rep = current_rep.is_some();
                        let list = DashSegmentList {
                            initialization: None,
                            segments: Vec::new(),
                        };
                        if is_empty {
                            if in_rep {
                                if let Some(r) = current_rep.as_mut() {
                                    r.own_list = Some(list);
                                }
                            } else {
                                set_list = Some(list);
                            }
                        } else {
                            pending_list = Some((in_rep, list));
                        }
                    }
                    "Initialization" => {
                        if let Some((_, list)) = pending_list.as_mut() {
                            list.initialization = Some(ListInit {
                                url: attrs.get("sourceURL").cloned(),
                                range: attrs
                                    .get("range")
                                    .and_then(|v| parse_byte_range(v)),
                            });
                        }
                    }
                    "SegmentURL" => {
                        if let Some((_, list)) = pending_list.as_mut() {
                            list.segments.push(ListSegment {
                                media: attrs.get("media").cloned(),
                                range: attrs
                                    .get("mediaRange")
                                    .and_then(|v| parse_byte_range(v)),
                            });
                        }
                    }
                    "Representation" => {
                        let rep = RepBuilder {
                            id: attrs.get("id").cloned().unwrap_or_default(),
                            bandwidth: attrs.get("bandwidth").and_then(|v| v.parse().ok()),
                            width: attrs.get("width").and_then(|v| v.parse().ok()),
                            height: attrs.get("height").and_then(|v| v.parse().ok()),
                            frame_rate: attrs.get("frameRate").and_then(|v| {
                                // frameRate may be a ratio ("30000/1001").
                                if let Some((n, d)) = v.split_once('/') {
                                    let n: f64 = n.parse().ok()?;
                                    let d: f64 = d.parse().ok()?;
                                    if d == 0.0 {
                                        None
                                    } else {
                                        Some(n / d)
                                    }
                                } else {
                                    v.parse().ok()
                                }
                            }),
                            codecs: attrs.get("codecs").cloned(),
                            mime_type: attrs.get("mimeType").cloned(),
                            content_type: attrs.get("contentType").cloned(),
                            own_base_url: None,
                            own_template: None,
                            own_list: None,
                        };
                        if is_empty {
                            manifest.representations.push(finish_rep(
                                rep,
                                &set_mime,
                                &set_content,
                                &set_template,
                                &set_list,
                                &set_base_url,
                                &period_base_url,
                                &mpd_base_url,
                            ));
                        } else {
                            current_rep = Some(rep);
                        }
                    }
                    "ContentProtection" => {
                        return Err(
                            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                                .to_string(),
                        );
                    }
                    _ => {}
                }
                if !is_empty {
                    stack.push(name);
                }
        }
    }
    if period_count == 0 {
        return Err("This DASH manifest contains no Period element.".to_string());
    }
    if manifest.representations.is_empty() {
        return Err("This DASH manifest contains no Representations.".to_string());
    }
    if manifest.duration_secs.is_none() {
        manifest.duration_secs = period_duration;
    }
    Ok(manifest)
}

/// A Representation being built while its element is open.
struct RepBuilder {
    id: String,
    bandwidth: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f64>,
    codecs: Option<String>,
    mime_type: Option<String>,
    content_type: Option<String>,
    own_base_url: Option<String>,
    own_template: Option<SegmentTemplate>,
    own_list: Option<DashSegmentList>,
}

/// Apply AdaptationSet/Period/MPD inheritance to a finished
/// Representation builder.
#[allow(clippy::too_many_arguments)]
fn finish_rep(
    rep: RepBuilder,
    set_mime: &str,
    set_content: &str,
    set_template: &Option<SegmentTemplate>,
    set_list: &Option<DashSegmentList>,
    set_base_url: &Option<String>,
    period_base_url: &Option<String>,
    mpd_base_url: &Option<String>,
) -> DashRepresentation {
    DashRepresentation {
        id: rep.id,
        bandwidth: rep.bandwidth,
        width: rep.width,
        height: rep.height,
        frame_rate: rep.frame_rate,
        codecs: rep.codecs,
        mime_type: rep.mime_type.unwrap_or_else(|| set_mime.to_string()),
        content_type: rep.content_type.unwrap_or_else(|| set_content.to_string()),
        base_url: rep
            .own_base_url
            .or_else(|| set_base_url.clone())
            .or_else(|| period_base_url.clone())
            .or_else(|| mpd_base_url.clone()),
        template: rep.own_template.or_else(|| set_template.clone()),
        segment_list: rep.own_list.or_else(|| set_list.clone()),
    }
}

/// Compute a representation's segments: (initialisation segment,
/// media segments). The initialisation segment downloads first and
/// is part of the assembled file.
fn dash_segments(
    rep: &DashRepresentation,
    manifest_url: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<Segment>), String> {
    let base = match &rep.base_url {
        Some(b) => resolve(manifest_url, b),
        None => manifest_url.to_string(),
    };
    if let Some(t) = &rep.template {
        return template_segments(rep, t, &base, duration_secs);
    }
    if let Some(list) = &rep.segment_list {
        return list_segments(list, &base);
    }
    Err(format!(
        "Representation {} has neither a SegmentTemplate nor a SegmentList, so its segments cannot be found.",
        rep.id
    ))
}

fn template_segments(
    rep: &DashRepresentation,
    t: &SegmentTemplate,
    base: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<Segment>), String> {
    let init = match &t.initialization {
        Some(init_tpl) => {
            let url = substitute_template(init_tpl, &rep.id, rep.bandwidth, None, None)?;
            Some(Segment {
                url: resolve(base, &url),
                range: None,
            })
        }
        None => None,
    };
    let media_tpl = t.media.as_ref().ok_or_else(|| {
        "This SegmentTemplate has no media attribute, so its segment URLs cannot be built."
            .to_string()
    })?;
    let uses_time = media_tpl.contains("$Time");
    let mut segments = Vec::new();
    if !t.timeline.is_empty() {
        // SegmentTimeline: expand S entries (r repeats each). Times
        // default to the previous segment's end; the first defaults
        // to 0. Numbers run from startNumber regardless of whether
        // the template uses $Number$ or $Time$.
        let mut number = t.start_number;
        let mut time = 0u64;
        for s in &t.timeline {
            if let Some(st) = s.start {
                time = st;
            }
            for _ in 0..=s.repeat {
                let url = substitute_template(
                    media_tpl,
                    &rep.id,
                    rep.bandwidth,
                    Some(number),
                    Some(time),
                )?;
                segments.push(Segment {
                    url: resolve(base, &url),
                    range: None,
                });
                number += 1;
                time += s.duration;
            }
        }
    } else if uses_time {
        return Err(
            "This SegmentTemplate uses $Time$ but the manifest has no SegmentTimeline."
                .to_string(),
        );
    } else if let Some(d) = t.duration {
        // Number-based: ceil(manifest duration / segment duration).
        // The last segment is the partial remainder; it is a real
        // segment on the server, not an overshoot.
        let seg_secs = d as f64 / t.timescale.max(1) as f64;
        let total = duration_secs.ok_or_else(|| {
            "Cannot determine how many segments this stream has: no SegmentTimeline and no manifest duration."
                .to_string()
        })?;
        let count = (total / seg_secs).ceil() as u64;
        if count == 0 {
            return Err("This manifest's duration is zero.".to_string());
        }
        for n in 0..count {
            let url = substitute_template(
                media_tpl,
                &rep.id,
                rep.bandwidth,
                Some(t.start_number + n),
                None,
            )?;
            segments.push(Segment {
                url: resolve(base, &url),
                range: None,
            });
        }
    } else {
        return Err(
            "This SegmentTemplate has neither a duration nor a SegmentTimeline, so its segments cannot be counted."
                .to_string(),
        );
    }
    Ok((init, segments))
}

fn list_segments(
    list: &DashSegmentList,
    base: &str,
) -> Result<(Option<Segment>, Vec<Segment>), String> {
    let init = list.initialization.as_ref().map(|i| Segment {
        url: match &i.url {
            Some(u) => resolve(base, u),
            None => base.to_string(),
        },
        range: i.range,
    });
    let mut segments = Vec::new();
    for s in &list.segments {
        match (&s.media, s.range) {
            (Some(m), _) => segments.push(Segment {
                url: resolve(base, m),
                range: None,
            }),
            (None, Some(r)) => segments.push(Segment {
                url: base.to_string(),
                range: Some(r),
            }),
            (None, None) => {
                return Err(
                    "A SegmentURL in this manifest has neither a media URL nor a byte range."
                        .to_string(),
                );
            }
        }
    }
    if segments.is_empty() {
        return Err("This SegmentList contains no segments.".to_string());
    }
    Ok((init, segments))
}

/// The default representation for the no-UI path: best video at or
/// below 1080p, else the best video; an audio-only manifest falls
/// back to the best audio representation.
fn pick_default_representation<'a>(
    reps: &'a [DashRepresentation],
) -> Option<&'a DashRepresentation> {
    let best_video = |max_height: Option<u32>| {
        reps.iter()
            .filter(|r| r.is_video())
            .filter(|r| max_height.is_none_or(|m| r.height.unwrap_or(0) <= m))
            .max_by(|a, b| {
                a.height
                    .unwrap_or(0)
                    .cmp(&b.height.unwrap_or(0))
                    .then(a.bandwidth.unwrap_or(0).cmp(&b.bandwidth.unwrap_or(0)))
            })
    };
    best_video(Some(1080))
        .or_else(|| best_video(None))
        .or_else(|| {
            reps.iter()
                .filter(|r| r.is_audio())
                .max_by_key(|r| r.bandwidth.unwrap_or(0))
        })
}

/// Choose a representation by id (or the default), plus the audio
/// representation it must merge with when the chosen one is video
/// and the manifest keeps audio separate. Returns
/// (chosen, audio pair or None).
fn choose_dash_representation<'a>(
    manifest: &'a DashManifest,
    representation_id: Option<&str>,
) -> Result<(&'a DashRepresentation, Option<&'a DashRepresentation>), String> {
    let chosen = match representation_id {
        Some(id) => manifest
            .representations
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| {
                format!(
                    "The selected quality ({id}) is no longer available for this stream. List the formats again and pick another."
                )
            })?,
        None => pick_default_representation(&manifest.representations)
            .ok_or_else(|| "No downloadable representation was found in this manifest.".to_string())?,
    };
    let audio = if chosen.is_video() {
        manifest
            .representations
            .iter()
            .filter(|r| r.is_audio())
            .max_by_key(|r| r.bandwidth.unwrap_or(0))
    } else {
        None
    };
    Ok((chosen, audio))
}

/// List the qualities of a DASH manifest: one format per
/// representation. Video representations in a manifest with a
/// separate audio track are video-only (merged with the best audio
/// on download); audio representations are audio-only.
pub fn list_dash_formats(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let client = hls_client()?;
    let text = manifest_text(&client, &manifest_url, referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let has_audio = manifest
        .representations
        .iter()
        .any(|r| r.is_audio());
    let formats = manifest
        .representations
        .iter()
        .map(|r| {
            let is_video = r.is_video();
            let is_audio = r.is_audio();
            StreamFormat {
                id: r.id.clone(),
                ext: if is_audio && !is_video {
                    "m4a"
                } else {
                    "mp4"
                }
                .to_string(),
                is_video,
                is_audio,
                height: r.height,
                width: r.width,
                abr_kbps: if is_audio {
                    r.bandwidth.map(|b| b as f64 / 1000.0)
                } else {
                    None
                },
                vbr_kbps: if is_video {
                    r.bandwidth.map(|b| b as f64 / 1000.0)
                } else {
                    None
                },
                fps: r.frame_rate,
                filesize: None,
                needs_merge: is_video && has_audio,
            }
        })
        .collect();
    // Segment count from the default representation (best
    // effort): the template/timeline expands to a concrete list,
    // so the plan can show the piece count up front like HLS.
    // The init segment counts too — it is a piece the worker
    // downloads.
    let segment_count = choose_dash_representation(&manifest, None)
        .ok()
        .and_then(|(chosen, _)| {
            dash_segments(chosen, &manifest_url, manifest.duration_secs).ok()
        })
        .map(|(init, segments)| segments.len() as u64 + init.map_or(0, |_| 1));
    Ok(StreamInfo {
        kind: StreamKind::Dash,
        title: title_from_manifest_url(&manifest_url),
        duration_secs: manifest.duration_secs.map(|d| d as u64),
        manifest_url,
        formats,
        segment_count,
    })
}

/// Options for a DASH download.
#[derive(Debug, Clone)]
pub struct DashDownloadOptions {
    pub manifest_url: String,
    /// Representation id to download; None means the default
    /// (best video at or below 1080p).
    pub representation_id: Option<String>,
    /// Final directory (the caller applies category organisation).
    pub destination_dir: PathBuf,
    /// Full filename (stem + extension from the plan).
    pub filename: String,
    pub referer: Option<String>,
}

/// Download a DASH manifest: resolve ffmpeg when the manifest
/// needs a merge, then assemble via [`download_dash_with`].
/// `captured` is manifest text the extension captured from the
/// player's own response (see [`CapturedManifest`]).
pub fn download_dash(
    data_dir: &Path,
    opts: &DashDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    // One manifest fetch to decide whether a merge is needed; the
    // assembly fetch is a second cheap GET of the same small file.
    let manifest_url = validate_url(&opts.manifest_url)?;
    let client = hls_client()?;
    let text = manifest_text(&client, &manifest_url, &opts.referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let (_, audio) =
        choose_dash_representation(&manifest, opts.representation_id.as_deref())?;
    let ffmpeg = if audio.is_some() {
        Some(crate::ffmpeg::ensure_ffmpeg(data_dir)?)
    } else {
        None
    };
    download_dash_with(ffmpeg.as_deref(), opts, progress, paused, cancelled, captured)
}

/// The testable core of [`download_dash`]: ffmpeg is explicit, so
/// tests can substitute a stub or the system ffmpeg. Assembles the
/// chosen representation (initialisation + media segments, in
/// order) into one file; when the manifest keeps audio separate,
/// downloads the best audio representation too and merges both
/// with ffmpeg (stream copy, no re-encode). Without ffmpeg a merge
/// is refused with a clear error instead of producing a silent
/// video.
pub fn download_dash_with(
    ffmpeg: Option<&Path>,
    opts: &DashDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(&opts.manifest_url)?;
    let client = hls_client()?;
    let text = manifest_text(&client, &manifest_url, &opts.referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let (chosen, audio) =
        choose_dash_representation(&manifest, opts.representation_id.as_deref())?;
    if audio.is_some() && ffmpeg.is_none() {
        return Err(
            "This quality keeps its audio on a separate track, which needs ffmpeg to merge. \
             Install ffmpeg (or pick a combined quality) and try again."
                .to_string(),
        );
    }
    let clean = crate::filename::sanitize_filename(&opts.filename);
    std::fs::create_dir_all(&opts.destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(&opts.destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let (init, mut segments) =
        dash_segments(chosen, &manifest_url, manifest.duration_secs)?;
    if let Some(init) = init {
        segments.insert(0, init);
    }
    let video_bytes = download_segments(
        &client,
        &segments,
        &video_part,
        &opts.referer,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    let Some(audio_rep) = audio else {
        std::fs::rename(&video_part, &final_path).map_err(|e| e.to_string())?;
        remove_seg_dir(&video_part);
        return Ok(final_path);
    };
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(&video_part);
        remove_seg_dir(&video_part);
        return Err("__CANCELLED__".to_string());
    }
    if paused.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("__PAUSED__".to_string());
    }
    let audio_part = final_path.with_file_name(format!(
        "{}.audio.part",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    let (audio_init, mut audio_segments) =
        dash_segments(audio_rep, &manifest_url, manifest.duration_secs)?;
    if let Some(init) = audio_init {
        audio_segments.insert(0, init);
    }
    download_segments(
        &client,
        &audio_segments,
        &audio_part,
        &opts.referer,
        video_bytes,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )?;
    let ffmpeg = ffmpeg.expect("audio merge checked ffmpeg above");
    let merged = crate::ffmpeg::merge_av(ffmpeg, &video_part, &audio_part, &final_path);
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    merged?;
    remove_seg_dir(&video_part);
    remove_seg_dir(&audio_part);
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-VERSION:3\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",URI=\"audio.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.42001e,mp4a.40.2\"\n\
        low.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",FRAME-RATE=30\n\
        mid.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028\",FRAME-RATE=60,AUDIO=\"aac\"\n\
        high.m3u8\n";

    const MEDIA_PLAYLIST: &str = "#EXTM3U\n\
        #EXT-X-VERSION:3\n\
        #EXT-X-TARGETDURATION:10\n\
        #EXT-X-MEDIA-SEQUENCE:0\n\
        #EXTINF:10.0,\n\
        seg0.ts\n\
        #EXTINF:10.0,\n\
        seg1.ts\n\
        #EXTINF:5.5,\n\
        sub/seg2.ts\n\
        #EXT-X-ENDLIST\n";

    #[test]
    fn stream_kind_of_url_and_content_type() {
        assert_eq!(
            stream_kind_of_url("https://example.com/v/master.m3u8"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind_of_url("https://example.com/v/manifest.mpd?token=abc#frag"),
            Some(StreamKind::Dash)
        );
        assert_eq!(
            stream_kind_of_url("https://example.com/v/MASTER.M3U8"),
            Some(StreamKind::Hls)
        );
        assert_eq!(stream_kind_of_url("https://example.com/v/movie.mp4"), None);
        assert_eq!(stream_kind_of_url("https://example.com/v/page"), None);
        assert_eq!(
            stream_kind_of_content_type("application/vnd.apple.mpegurl"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind_of_content_type("application/x-mpegURL; charset=utf-8"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind_of_content_type("application/dash+xml"),
            Some(StreamKind::Dash)
        );
        // An M3U audio playlist is not HLS.
        assert_eq!(stream_kind_of_content_type("audio/x-mpegurl"), None);
        assert_eq!(stream_kind_of_content_type("text/html"), None);
    }

    #[test]
    fn parse_hls_master_reads_variants_and_audio_groups() {
        let m = parse_hls_master(MASTER, "https://example.com/v/master.m3u8");
        assert_eq!(m.variants.len(), 3);
        // CODECS contains a comma inside quotes: the split must not
        // break on it (RESOLUTION/FRAME-RATE still parse).
        let mid = &m.variants[1];
        assert_eq!(mid.uri, "https://example.com/v/mid.m3u8");
        assert_eq!(mid.bandwidth, Some(2_800_000));
        assert_eq!((mid.width, mid.height), (Some(1280), Some(720)));
        assert_eq!(mid.fps, Some(30.0));
        assert_eq!(mid.audio_group, None);
        let high = &m.variants[2];
        assert_eq!(high.audio_group.as_deref(), Some("aac"));
        assert_eq!(high.fps, Some(60.0));
        assert_eq!(m.audio.len(), 1);
        assert_eq!(m.audio[0].group_id, "aac");
        assert_eq!(m.audio[0].uri, "https://example.com/v/audio.m3u8");
    }

    #[test]
    fn parse_hls_master_empty_for_media_playlist() {
        let m = parse_hls_master(MEDIA_PLAYLIST, "https://example.com/v/play.m3u8");
        assert!(m.variants.is_empty());
        assert!(m.audio.is_empty());
    }

    #[test]
    fn parse_hls_segments_resolves_relative_urls_and_refuses_key() {
        let segs = parse_hls_segments(MEDIA_PLAYLIST, "https://example.com/v/play.m3u8").unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], "https://example.com/v/seg0.ts");
        assert_eq!(segs[2], "https://example.com/v/sub/seg2.ts");
        let enc = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:10,\nseg0.ts\n";
        let err = parse_hls_segments(enc, "https://example.com/v/play.m3u8").unwrap_err();
        assert!(err.contains("encrypted"), "error: {err}");
        assert!(parse_hls_segments("#EXTM3U\n#EXT-X-ENDLIST\n", "https://example.com/x").is_err());
    }

    #[test]
    fn hls_duration_sums_extinf() {
        assert_eq!(hls_duration_secs(MEDIA_PLAYLIST), Some(25));
        assert_eq!(hls_duration_secs("#EXTM3U\n"), None);
    }

    #[test]
    fn pick_default_variant_prefers_1080() {
        let m = parse_hls_master(MASTER, "https://example.com/v/master.m3u8");
        // 1080p exists, so it is the default despite 720p also
        // being <= 1080p (highest wins within the cap).
        assert_eq!(pick_default_variant(&m.variants).unwrap().height, Some(1080));
        let no_res = vec![
            HlsVariant {
                uri: "a".into(),
                bandwidth: Some(100),
                width: None,
                height: None,
                codecs: None,
                fps: None,
                audio_group: None,
            },
            HlsVariant {
                uri: "b".into(),
                bandwidth: Some(500),
                width: None,
                height: None,
                codecs: None,
                fps: None,
                audio_group: None,
            },
        ];
        assert_eq!(pick_default_variant(&no_res).unwrap().uri, "b");
    }

    #[test]
    fn title_from_manifest_url_uses_parent_for_generic_stems() {
        assert_eq!(
            title_from_manifest_url("https://example.com/bbb/master.m3u8"),
            "bbb"
        );
        assert_eq!(
            title_from_manifest_url("https://example.com/v/my-show.m3u8"),
            "my-show"
        );
        assert_eq!(
            title_from_manifest_url("https://example.com/index.m3u8"),
            "stream"
        );
    }

    /// Loopback server for stream tests: serves `files` by path
    /// until `stop` is set, recording every requested path in
    /// `hits`. Returns the port and the server thread.
    fn serve_map(
        files: HashMap<String, Vec<u8>>,
        hits: std::sync::Arc<Mutex<Vec<String>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        serve_map_bound(listener, files, hits, stop)
    }

    /// `serve_map` on a listener the caller already bound (the port
    /// is known before serving starts, so served paths can contain
    /// it — needed for the proxy-wrapper test, where the manifest
    /// URL's query embeds the inner URL with its port).
    fn serve_map_bound(
        listener: std::net::TcpListener,
        files: HashMap<String, Vec<u8>>,
        hits: std::sync::Arc<Mutex<Vec<String>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    hits.lock().unwrap().push(path.clone());
                    match files.get(&path) {
                        Some(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        None => {
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    }
                    let _ = stream.flush();
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("fetchforge-test-{tag}-{pid}-{nanos}"))
    }

    #[test]
    fn list_hls_formats_lists_variants() {
        let mut files = HashMap::new();
        files.insert("/master.m3u8".to_string(), MASTER.as_bytes().to_vec());
        files.insert("/low.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        files.insert("/mid.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        files.insert("/high.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        files.insert("/audio.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let info =
            list_hls_formats(&format!("http://127.0.0.1:{port}/master.m3u8"), &None, None).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(info.kind, StreamKind::Hls);
        assert_eq!(info.formats.len(), 3);
        // Muxed variants are combined .ts; the separate-audio
        // variant is video-only .mp4 (merged).
        let low = &info.formats[0];
        assert_eq!(low.height, Some(360));
        assert!(low.is_audio && low.is_video);
        assert_eq!(low.ext, "ts");
        assert!(!low.needs_merge);
        let high = &info.formats[2];
        assert_eq!(high.height, Some(1080));
        assert!(high.is_video && !high.is_audio);
        assert_eq!(high.ext, "mp4");
        assert!(high.needs_merge);
        // Duration comes from the default (1080p) variant's playlist.
        assert_eq!(info.duration_secs, Some(25));
    }

    #[test]
    fn list_hls_formats_media_playlist_is_one_format() {
        let mut files = HashMap::new();
        files.insert("/play.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let info =
            list_hls_formats(&format!("http://127.0.0.1:{port}/play.m3u8"), &None, None).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(info.formats.len(), 1);
        assert_eq!(info.formats[0].id, format!("http://127.0.0.1:{port}/play.m3u8"));
        assert_eq!(info.duration_secs, Some(25));
    }

    fn hls_segment_files() -> HashMap<String, Vec<u8>> {
        let mut files = HashMap::new();
        files.insert("/play.m3u8".to_string(), MEDIA_PLAYLIST.as_bytes().to_vec());
        files.insert("/seg0.ts".to_string(), b"SEGMENT-ZERO-".to_vec());
        files.insert("/seg1.ts".to_string(), b"SEGMENT-ONE-".to_vec());
        files.insert("/sub/seg2.ts".to_string(), b"SEGMENT-TWO".to_vec());
        files
    }

    #[test]
    fn download_hls_variant_concatenates_segments() {
        let dir = unique_temp_dir("hls-dl");
        std::fs::create_dir_all(&dir).unwrap();
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(hls_segment_files(), hits, stop.clone());
        let url = format!("http://127.0.0.1:{port}/play.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "clip.ts",
            &None,
            None,
            &|p| seen.lock().unwrap().push(p.downloaded),
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO");
        assert_eq!(*seen.lock().unwrap().last().unwrap(), 36);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_variant_resumes_from_sidecar() {
        let dir = unique_temp_dir("hls-resume");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate a paused download: the .part holds segment 0 and
        // the sidecar says segment 1 is next.
        std::fs::write(dir.join("clip.ts.part"), b"SEGMENT-ZERO-").unwrap();
        let seg1 = format!("http://127.0.0.1:PORT/seg1.ts");
        std::fs::write(
            dir.join("clip.ts.part.segments"),
            format!("done=1\nnext={seg1}\n"),
        )
        .unwrap();
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(hls_segment_files(), hits.clone(), stop.clone());
        // Fix the sidecar's port now that the server port is known.
        std::fs::write(
            dir.join("clip.ts.part.segments"),
            format!("done=1\nnext=http://127.0.0.1:{port}/seg1.ts\n"),
        )
        .unwrap();
        let url = format!("http://127.0.0.1:{port}/play.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "clip.ts",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO");
        // Segment 0 was NOT re-downloaded; the sidecar is gone.
        let hits = hits.lock().unwrap();
        assert!(!hits.contains(&"/seg0.ts".to_string()), "hits: {hits:?}");
        assert!(hits.contains(&"/seg1.ts".to_string()));
        assert!(!dir.join("clip.ts.part.segments").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_variant_restarts_when_manifest_changed() {
        let dir = unique_temp_dir("hls-restart");
        std::fs::create_dir_all(&dir).unwrap();
        // Stale sidecar: the "next" segment is not in this manifest.
        std::fs::write(dir.join("clip.ts.part"), b"STALE-BYTES").unwrap();
        std::fs::write(
            dir.join("clip.ts.part.segments"),
            "done=1\nnext=http://127.0.0.1:1/gone.ts\n",
        )
        .unwrap();
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(hls_segment_files(), hits.clone(), stop.clone());
        let url = format!("http://127.0.0.1:{port}/play.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "clip.ts",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        // Restarted from segment 0: no stale bytes in the output.
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO");
        assert!(hits.lock().unwrap().contains(&"/seg0.ts".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_refuses_encrypted_playlist() {
        let mut files = HashMap::new();
        files.insert(
            "/enc.m3u8".to_string(),
            b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXTINF:10,\ns.ts\n".to_vec(),
        );
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("hls-enc");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/enc.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let err = download_hls(
            &url,
            &dir,
            "x.ts",
            &None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap_err();
        assert!(err.contains("encrypted"), "error: {err}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_variant_merges_separate_audio() {
        // Needs a real ffmpeg; skip where none exists.
        let ffmpeg = match crate::ffmpeg::find_on_path("ffmpeg") {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("hls-merge");
        std::fs::create_dir_all(&dir).unwrap();
        // 1 s of black video + 1 s of tone as .ts, each split into
        // two byte chunks served as segments (concatenation
        // restores the original files, so the merge is real).
        let mk = |args: &[&str]| {
            use std::process::{Command, Stdio};
            Command::new(&ffmpeg)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let vts = dir.join("v.ts");
        let ats = dir.join("a.ts");
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-f", "mpegts",
            vts.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "sine=frequency=440:d=1",
            "-c:a", "aac", "-f", "mpegts", ats.to_str().unwrap()
        ]));
        let vbytes = std::fs::read(&vts).unwrap();
        let abytes = std::fs::read(&ats).unwrap();
        let (v0, v1) = vbytes.split_at(vbytes.len() / 2);
        let (a0, a1) = abytes.split_at(abytes.len() / 2);
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",URI=\"audio.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=64x64,AUDIO=\"aac\"\n\
            video.m3u8\n";
        let vplay = "#EXTM3U\n#EXTINF:1,\nv0.ts\n#EXTINF:1,\nv1.ts\n#EXT-X-ENDLIST\n";
        let aplay = "#EXTM3U\n#EXTINF:1,\na0.ts\n#EXTINF:1,\na1.ts\n#EXT-X-ENDLIST\n";
        let mut files = HashMap::new();
        files.insert("/master.m3u8".to_string(), master.as_bytes().to_vec());
        files.insert("/video.m3u8".to_string(), vplay.as_bytes().to_vec());
        files.insert("/audio.m3u8".to_string(), aplay.as_bytes().to_vec());
        files.insert("/v0.ts".to_string(), v0.to_vec());
        files.insert("/v1.ts".to_string(), v1.to_vec());
        files.insert("/a0.ts".to_string(), a0.to_vec());
        files.insert("/a1.ts".to_string(), a1.to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let master_url = format!("http://127.0.0.1:{port}/master.m3u8");
        let variant_url = format!("http://127.0.0.1:{port}/video.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_variant(
            &master_url,
            &variant_url,
            &dir,
            "merged.mp4",
            &None,
            Some(&ffmpeg),
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        // No ffmpeg -> the same variant is refused, not silently
        // downloaded as a silent video. (The server must stay up
        // for this second call: the variant's audio group is read
        // from the master playlist before ffmpeg is checked.)
        let err = download_hls_variant(
            &master_url,
            &variant_url,
            &dir,
            "merged2.mp4",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap_err();
        assert!(err.contains("ffmpeg"), "error: {err}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signed_playlist_query_inherits_to_segments() {
        // Regression (2026-11-22, atlantic.bz): a signed playlist
        // (`audio_1.m3u8?sign=...`) whose segments were fetched
        // WITHOUT the query got HTTP 400 on every segment. Relative
        // and absolute same-origin segments inherit the playlist's
        // query; cross-origin segments and segments with their own
        // query are left alone.
        let base = "https://totallyacdn.cc/aws/hls/024781/audio_1.m3u8?sign=abc123&exp=999";
        let text = "#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nhttps://totallyacdn.cc/aws/hls/024781/seg1.ts\n#EXTINF:10,\nhttps://othercdn.example.com/x/seg2.ts\n#EXTINF:10,\nseg3.ts?token=zzz\n";
        let segs = parse_hls_segments(text, base).unwrap();
        assert_eq!(segs.len(), 4);
        assert_eq!(
            segs[0],
            "https://totallyacdn.cc/aws/hls/024781/seg0.ts?sign=abc123&exp=999",
            "relative segment must carry the playlist query"
        );
        assert_eq!(
            segs[1],
            "https://totallyacdn.cc/aws/hls/024781/seg1.ts?sign=abc123&exp=999",
            "absolute same-origin segment must carry the playlist query"
        );
        assert_eq!(
            segs[2], "https://othercdn.example.com/x/seg2.ts",
            "cross-origin segment must NOT inherit the query"
        );
        assert_eq!(
            segs[3],
            "https://totallyacdn.cc/aws/hls/024781/seg3.ts?token=zzz",
            "a segment with its own query keeps it"
        );
        // An unsigned playlist resolves plainly (no query invented).
        let plain = parse_hls_segments("#EXTM3U\n#EXTINF:10,\nseg0.ts\n", "https://cdn.example.com/p/play.m3u8").unwrap();
        assert_eq!(plain, vec!["https://cdn.example.com/p/seg0.ts".to_string()]);
    }

    #[test]
    fn download_hls_variant_signed_playlist_downloads() {
        // End-to-end version of the 400 regression: the test server
        // serves segments ONLY under their signed paths (an unsigned
        // request 404s, as the real CDN 400s it). Before the fix the
        // download failed on segment 1; now it completes.
        let mut files = HashMap::new();
        files.insert(
            "/audio_1.m3u8?sign=s3cret".to_string(),
            b"#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n#EXT-X-ENDLIST\n".to_vec(),
        );
        files.insert("/seg0.ts?sign=s3cret".to_string(), b"SEGMENT-ZERO-".to_vec());
        files.insert("/seg1.ts?sign=s3cret".to_string(), b"SEGMENT-ONE".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-signed");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/audio_1.m3u8?sign=s3cret");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "audio.ts",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE");
        let hits = hits.lock().unwrap();
        assert!(hits.contains(&"/seg0.ts?sign=s3cret".to_string()), "hits: {hits:?}");
        assert!(hits.contains(&"/seg1.ts?sign=s3cret".to_string()), "hits: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn proxy_inner_manifest_url_detection() {
        // A workers.dev-style wrapper: the real manifest is the
        // `url` query parameter.
        assert_eq!(
            proxy_inner_manifest_url(
                "https://frappe.hybrows.workers.dev/?origin=https%3A%2F%2Fvidly.pro&url=https%3A%2F%2Fimg.rousav.tech%2Fmedia%2Fabc%2Findex.m3u8"
            )
            .as_deref(),
            Some("https://img.rousav.tech/media/abc/index.m3u8")
        );
        assert_eq!(
            proxy_inner_manifest_url("https://proxy.example/?u=https://cdn.example.com/v/play.m3u8").as_deref(),
            Some("https://cdn.example.com/v/play.m3u8")
        );
        // Not a manifest wrapper: no url param, or the param is not
        // an HLS manifest (DASH wrappers keep yt-dlp routing).
        assert_eq!(proxy_inner_manifest_url("https://proxy.example/?q=1"), None);
        assert_eq!(
            proxy_inner_manifest_url("https://proxy.example/?url=https://cdn.example.com/v/movie.mp4"),
            None
        );
        assert_eq!(
            proxy_inner_manifest_url("https://proxy.example/?url=https://cdn.example.com/v/manifest.mpd"),
            None
        );
        // And the wrapper itself routes to the HLS engine.
        assert_eq!(
            stream_kind_of_url("https://proxy.example/?url=https://cdn.example.com/v/play.m3u8"),
            Some(StreamKind::Hls)
        );
        assert_eq!(stream_kind_of_url("https://proxy.example/?url=https://cdn.example.com/v/manifest.mpd"), None);
    }

    #[test]
    fn proxy_wrapped_manifest_downloads_from_inner_cdn() {
        // Regression (2026-11-22, nime.pro): a proxy-wrapped m3u8
        // went to yt-dlp, which resolved the manifest's relative
        // segments against the PROXY origin, 404ing and retrying
        // until the download sat at 0% with no error. The HLS
        // engine now fetches the manifest through the wrapper but
        // resolves segments against the inner URL. The test server
        // serves the manifest only under the wrapper path and the
        // segments only under the inner paths.
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let inner = format!("http://127.0.0.1:{port}/media/index.m3u8");
        let proxy = format!(
            "http://127.0.0.1:{port}/?origin=https%3A%2F%2Fvidly.pro&url={}",
            url::form_urlencoded::byte_serialize(inner.as_bytes()).collect::<String>()
        );
        let proxy_path = proxy
            .split_once(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .1
            .to_string();
        let mut files = HashMap::new();
        files.insert(
            proxy_path,
            b"#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n#EXT-X-ENDLIST\n".to_vec(),
        );
        files.insert("/media/seg0.ts".to_string(), b"SEGMENT-ZERO-".to_vec());
        files.insert("/media/seg1.ts".to_string(), b"SEGMENT-ONE".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (_port, server) = serve_map_bound(listener, files, hits.clone(), stop.clone());
        // Listing works through the wrapper (title from the inner URL).
        let info = list_hls_formats(&proxy, &None, None).unwrap();
        assert_eq!(info.formats.len(), 1);
        assert_eq!(info.title, "media");
        let dir = unique_temp_dir("hls-proxy");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_variant(
            &proxy,
            &proxy,
            &dir,
            "movie.ts",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE");
        // Segments came from the inner paths, never the proxy origin.
        let hits = hits.lock().unwrap();
        assert!(hits.contains(&"/media/seg0.ts".to_string()), "hits: {hits:?}");
        assert!(hits.contains(&"/media/seg1.ts".to_string()), "hits: {hits:?}");
        assert!(
            !hits.iter().any(|h| h == "/seg0.ts" || h == "/seg1.ts"),
            "segments must not resolve against the proxy origin: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_pair_merges_video_and_audio_variants() {
        // Paired variants (no master): v0.m3u8 + audio_1.m3u8 merge
        // into one mp4 with video and audio. Needs a real ffmpeg;
        // skip where none exists.
        let ffmpeg = match crate::ffmpeg::find_on_path("ffmpeg") {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("hls-pair");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |args: &[&str]| {
            use std::process::{Command, Stdio};
            Command::new(&ffmpeg)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let vts = dir.join("v.ts");
        let ats = dir.join("a.ts");
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-f", "mpegts",
            vts.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "sine=frequency=440:d=1",
            "-c:a", "aac", "-f", "mpegts", ats.to_str().unwrap()
        ]));
        let vbytes = std::fs::read(&vts).unwrap();
        let abytes = std::fs::read(&ats).unwrap();
        let (v0, v1) = vbytes.split_at(vbytes.len() / 2);
        let (a0, a1) = abytes.split_at(abytes.len() / 2);
        let vplay = "#EXTM3U\n#EXTINF:1,\nv0.ts\n#EXTINF:1,\nv1.ts\n#EXT-X-ENDLIST\n";
        let aplay = "#EXTM3U\n#EXTINF:1,\na0.ts\n#EXTINF:1,\na1.ts\n#EXT-X-ENDLIST\n";
        let mut files = HashMap::new();
        files.insert("/v0.m3u8".to_string(), vplay.as_bytes().to_vec());
        files.insert("/audio_1.m3u8".to_string(), aplay.as_bytes().to_vec());
        files.insert("/v0.ts".to_string(), v0.to_vec());
        files.insert("/v1.ts".to_string(), v1.to_vec());
        files.insert("/a0.ts".to_string(), a0.to_vec());
        files.insert("/a1.ts".to_string(), a1.to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let video_url = format!("http://127.0.0.1:{port}/v0.m3u8");
        let audio_url = format!("http://127.0.0.1:{port}/audio_1.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_pair(
            &video_url,
            &audio_url,
            &dir,
            "movie.mp4",
            &None,
            &ffmpeg,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        assert_eq!(out.extension().and_then(|e| e.to_str()), Some("mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_retry_and_stall_helpers() {
        // Timeout-looking errors…
        assert!(looks_like_timeout("error sending request: operation timed out"));
        assert!(looks_like_timeout("request timeout"));
        assert!(looks_like_timeout("deadline exceeded"));
        assert!(!looks_like_timeout("Segment request failed: HTTP 404"));
        assert!(!looks_like_timeout("connection refused"));
        // …fail fast at 0 bytes (no retry), but keep retrying
        // mid-download and for fast refusals.
        assert!(!should_retry_segment(0, 0, "operation timed out"));
        assert!(should_retry_segment(0, 1024, "operation timed out"));
        assert!(should_retry_segment(0, 0, "Segment request failed: HTTP 500"));
        assert!(should_retry_segment(1, 0, "Segment request failed: HTTP 500"));
        assert!(!should_retry_segment(2, 0, "Segment request failed: HTTP 500"));
        // Stall check: fresh is fine, expired fails plainly.
        assert!(check_stall(Instant::now(), Duration::from_secs(300)).is_ok());
        let old = Instant::now() - Duration::from_secs(301);
        let err = check_stall(old, Duration::from_secs(300)).unwrap_err();
        assert!(err.contains("stalled"), "error: {err}");
        assert!(err.contains("5 minutes"), "error: {err}");
    }

    /// Loopback server that waits `delay` then answers 500 to every
    /// request: models a slow, failing CDN for the stall-watchdog
    /// test.
    fn serve_delayed_500(
        delay: Duration,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    std::thread::sleep(delay);
                    let _ = stream.write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    let _ = stream.flush();
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn download_segments_stall_watchdog_fires() {
        // A paired HLS download sat at 0% for 5+ minutes with no
        // error because no watchdog covered the segment path. With
        // a short watchdog the same shape fails fast and plainly.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_delayed_500(Duration::from_secs(3), stop.clone());
        let dir = unique_temp_dir("seg-stall");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/seg0.ts"),
            range: None,
        }];
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let start = Instant::now();
        let client = hls_client().unwrap();
        let err = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            Duration::from_secs(2),
        )
        .unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(err.contains("stalled"), "error: {err}");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "watchdog took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_segments_retries_fast_failures_three_times() {
        // Fast refusals (404) still get all 3 attempts: the
        // watchdog must not turn a quick, clear failure into a
        // stall error.
        let files = HashMap::new();
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("seg-retry");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/missing.ts"),
            range: None,
        }];
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let client = hls_client().unwrap();
        let err = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(
            err.contains("after 3 tries"),
            "error: {err}"
        );
        // Bind the guard once: locking twice in one assert deadlocks
        // (the first guard lives until the end of the statement).
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 3, "hits: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One-shot stub: sends response headers promising `len` body
    /// bytes, then stalls `stall` before closing, so a client with
    /// a shorter timeout fails while reading the body. Records the
    /// raw request (for the Accept-Encoding assertion).
    fn serve_stalled_body(
        len: usize,
        stall: Duration,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                seen.lock().unwrap().push(
                    String::from_utf8_lossy(&buf[..n]).to_string(),
                );
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
                std::thread::sleep(stall);
            }
        });
        (port, handle)
    }

    /// One-shot stub: sends headers promising `len` body bytes,
    /// then `sent` bytes, then closes — a truncated body, the
    /// shape a CDN reset produces. Records the raw request.
    fn serve_truncated_body(
        len: usize,
        sent: &[u8],
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = sent.to_vec();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                seen.lock().unwrap().push(
                    String::from_utf8_lossy(&buf[..n]).to_string(),
                );
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(&sent);
                let _ = stream.flush();
                // Drop the socket: the body is short, hyper reports
                // an incomplete message.
            }
        });
        (port, handle)
    }

    #[test]
    fn segment_body_timeout_reports_timed_out_and_fails_fast() {
        // Atlantic 2026-11-22: segment 1 failed 3 times over ~6
        // minutes with "error decoding response body". reqwest
        // maps EVERY body-read error (including a timeout
        // mid-body) to Kind::Decode, whose Display never says
        // "timed out" — so the 0-byte fail-fast never fired and
        // all 3 attempts burned their full 120 s. The formatted
        // error must carry the underlying cause, and the retry
        // rule must recognise it as a timeout.
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (port, server) = serve_stalled_body(1_000_000, Duration::from_secs(1), seen);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://127.0.0.1:{port}/seg0.ts"))
            .send()
            .and_then(|r| r.bytes())
            .unwrap_err();
        server.join().unwrap();
        assert!(err.is_timeout(), "expected a timeout error: {err:?}");
        let msg = reqwest_error(&err);
        assert!(
            msg.to_lowercase().contains("timed out") || msg.to_lowercase().contains("timeout"),
            "formatted error must name the timeout: {msg}"
        );
        assert!(
            looks_like_timeout(&msg),
            "retry rule must recognise it: {msg}"
        );
        assert!(
            !should_retry_segment(0, 0, &msg),
            "a 0-byte timeout must fail fast, not retry: {msg}"
        );
    }

    #[test]
    fn segment_body_failure_reports_underlying_cause_and_identity_encoding() {
        // A truncated body (CDN reset) must not collapse to the
        // bare "error decoding response body": the recorded error
        // must say what actually happened. The request must also
        // ask for identity encoding (segments are already
        // compressed; a compressed body only adds failure modes).
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (port, server) =
            serve_truncated_body(100, b"0123456789", seen.clone());
        let client = hls_client().unwrap();
        let err = fetch_bytes_range(
            &client,
            &format!("http://127.0.0.1:{port}/seg0.ts"),
            None,
            &None,
        )
        .unwrap_err();
        server.join().unwrap();
        assert_ne!(
            err, "error decoding response body",
            "the bare kind message hides the cause"
        );
        assert!(
            err.starts_with("error decoding response body: "),
            "the cause must be appended: {err}"
        );
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1, "one request expected: {reqs:?}");
        assert!(
            reqs[0].to_ascii_lowercase().contains("accept-encoding: identity"),
            "segment fetch must request identity encoding: {:?}",
            reqs[0]
        );
    }

    #[test]
    fn parse_iso8601_duration_values() {
        assert_eq!(parse_iso8601_duration("PT634.566S"), Some(634.566));
        assert_eq!(parse_iso8601_duration("PT10M30.5S"), Some(630.5));
        assert_eq!(parse_iso8601_duration("PT2H3M"), Some(7380.0));
        assert_eq!(parse_iso8601_duration("P1DT2H"), Some(93600.0));
        assert_eq!(parse_iso8601_duration("PT0S"), Some(0.0));
        // Years/months/weeks are not supported.
        assert_eq!(parse_iso8601_duration("P1M"), None);
        assert_eq!(parse_iso8601_duration("P1Y"), None);
        assert_eq!(parse_iso8601_duration("abc"), None);
        assert_eq!(parse_iso8601_duration("PT"), Some(0.0));
    }

    #[test]
    fn substitute_template_handles_variables_padding_and_escapes() {
        let sub = |t: &str, n: Option<u64>, time: Option<u64>| {
            substitute_template(t, "v720", Some(4000000), n, time)
        };
        assert_eq!(
            sub("$RepresentationID$/$RepresentationID$_$Number$.m4v", Some(3), None).unwrap(),
            "v720/v720_3.m4v"
        );
        assert_eq!(
            sub("seg_$Time$.m4v", None, Some(20000)).unwrap(),
            "seg_20000.m4v"
        );
        assert_eq!(
            sub("seg_$Number%03d$.m4v", Some(7), None).unwrap(),
            "seg_007.m4v"
        );
        assert_eq!(sub("100$$-off.m4v", None, None).unwrap(), "100$-off.m4v");
        assert_eq!(
            sub("$Bandwidth$.m4v", None, None).unwrap(),
            "4000000.m4v"
        );
        assert!(sub("$Nope$.m4v", None, None).is_err());
        assert!(sub("$Number.m4v", Some(1), None).is_err());
        assert!(sub("$Number$.m4v", None, None).is_err());
    }

    /// Trimmed from the real Big Buck Bunny test manifest
    /// (dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd): same
    /// shapes — BaseURL, Number-based SegmentTemplate, separate
    /// audio AdaptationSet — with 2 video + 1 audio representations
    /// and an 8 s duration to keep segment counts small.
    const BBB_MPD: &str = r#"<MPD mediaPresentationDuration="PT8S" minBufferTime="PT2.00S" type="static" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <BaseURL>./</BaseURL>
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate duration="120" timescale="30" media="$RepresentationID$/$RepresentationID$_$Number$.m4v" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4v"/>
   <Representation id="v360" codecs="avc1.64001e" bandwidth="1000000" width="640" height="360" frameRate="30"/>
   <Representation id="v720" codecs="avc1.64001f" bandwidth="4000000" width="1280" height="720" frameRate="30000/1001"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <SegmentTemplate duration="192512" timescale="48000" media="$RepresentationID$/$RepresentationID$_$Number$.m4a" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4a"/>
   <Representation id="a64k" codecs="mp4a.40.5" bandwidth="67071" audioSamplingRate="48000">
    <AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;

    #[test]
    fn parse_dash_manifest_template_number() {
        let m = parse_dash_manifest(BBB_MPD).unwrap();
        assert_eq!(m.duration_secs, Some(8.0));
        assert_eq!(m.representations.len(), 3);
        let v720 = m
            .representations
            .iter()
            .find(|r| r.id == "v720")
            .unwrap();
        assert_eq!((v720.width, v720.height), (Some(1280), Some(720)));
        assert_eq!(v720.bandwidth, Some(4_000_000));
        // frameRate may be a ratio: 30000/1001 ≈ 29.97.
        assert!((v720.frame_rate.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(v720.mime_type, "video/mp4");
        assert!(v720.is_video());
        assert!(!v720.is_audio());
        assert_eq!(v720.base_url.as_deref(), Some("./"));
        let tpl = v720.template.as_ref().unwrap();
        assert_eq!(tpl.timescale, 30);
        assert_eq!(tpl.duration, Some(120));
        assert_eq!(tpl.start_number, 1);
        assert!(tpl.timeline.is_empty());
        let a = m.representations.iter().find(|r| r.id == "a64k").unwrap();
        assert!(a.is_audio());
        // Default: best video at or below 1080p (720p, not 360p).
        assert_eq!(pick_default_representation(&m.representations).unwrap().id, "v720");
        // Segments for 8 s at 4 s each: init + 2 media segments.
        let (init, segs) =
            dash_segments(v720, "https://example.com/bbb/bbb.mpd", m.duration_secs).unwrap();
        assert_eq!(
            init.unwrap().url,
            "https://example.com/bbb/v720/v720_0.m4v"
        );
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].url, "https://example.com/bbb/v720/v720_1.m4v");
        assert_eq!(segs[1].url, "https://example.com/bbb/v720/v720_2.m4v");
    }

    const TIMELINE_MPD: &str = r#"<MPD type="static" mediaPresentationDuration="PT30S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate timescale="1000" media="seg_$Time$.m4v" initialization="init.m4v">
    <SegmentTimeline>
     <S t="0" d="10000" r="1"/>
     <S d="10000"/>
    </SegmentTimeline>
   </SegmentTemplate>
   <Representation id="v1" bandwidth="1000000" width="640" height="360"/>
  </AdaptationSet>
 </Period>
</MPD>"#;

    #[test]
    fn parse_dash_manifest_timeline_and_time_template() {
        let m = parse_dash_manifest(TIMELINE_MPD).unwrap();
        let rep = &m.representations[0];
        let tpl = rep.template.as_ref().unwrap();
        assert_eq!(tpl.timeline.len(), 2);
        assert_eq!(tpl.timeline[0].repeat, 1);
        // S(t=0,d=10000,r=1) -> t=0,10000; S(d=10000) -> t=20000.
        let (init, segs) =
            dash_segments(rep, "https://example.com/x.mpd", m.duration_secs).unwrap();
        assert_eq!(init.unwrap().url, "https://example.com/init.m4v");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].url, "https://example.com/seg_0.m4v");
        assert_eq!(segs[1].url, "https://example.com/seg_10000.m4v");
        assert_eq!(segs[2].url, "https://example.com/seg_20000.m4v");
    }

    const LIST_MPD: &str = r#"<MPD type="static" mediaPresentationDuration="PT10S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentList timescale="1" duration="5">
    <Initialization sourceURL="init.mp4"/>
    <SegmentURL media="seg1.m4v"/>
    <SegmentURL mediaRange="100-199"/>
   </SegmentList>
   <Representation id="v1" bandwidth="1000000" width="640" height="360">
    <BaseURL>file.mp4</BaseURL>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;

    #[test]
    fn parse_dash_manifest_segment_list() {
        let m = parse_dash_manifest(LIST_MPD).unwrap();
        let rep = &m.representations[0];
        assert_eq!(rep.base_url.as_deref(), Some("file.mp4"));
        let (init, segs) =
            dash_segments(rep, "https://example.com/list.mpd", m.duration_secs).unwrap();
        assert_eq!(init.unwrap().url, "https://example.com/init.mp4");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].url, "https://example.com/seg1.m4v");
        assert_eq!(segs[0].range, None);
        // A mediaRange segment is a byte range of the BaseURL file.
        assert_eq!(segs[1].url, "https://example.com/file.mp4");
        assert_eq!(segs[1].range, Some((100, 199)));
    }

    #[test]
    fn parse_dash_manifest_refusals() {
        let drm = "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"><Period><AdaptationSet><ContentProtection/></AdaptationSet></Period></MPD>";
        assert!(parse_dash_manifest(drm).unwrap_err().contains("DRM"));
        let live = BBB_MPD.replace("type=\"static\"", "type=\"dynamic\"");
        assert!(parse_dash_manifest(&live).unwrap_err().contains("live"));
        let multi = BBB_MPD.replace("</Period>", "</Period><Period></Period>");
        assert!(parse_dash_manifest(&multi).unwrap_err().contains("Multi-period"));
        let no_period = "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"></MPD>";
        assert!(parse_dash_manifest(no_period).is_err());
        let no_reps = "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"><Period></Period></MPD>";
        assert!(parse_dash_manifest(no_reps).is_err());
        // $Time$ without a timeline: the template still uses $Time$
        // but the timeline is commented out. Parsing succeeds;
        // segment computation must fail clearly.
        let no_timeline = TIMELINE_MPD
            .replace("<SegmentTimeline>", "<!--")
            .replace("</SegmentTimeline>", "-->");
        let m = parse_dash_manifest(&no_timeline).unwrap();
        let err = dash_segments(&m.representations[0], "https://example.com/x.mpd", m.duration_secs)
            .unwrap_err();
        assert!(err.contains("$Time$"), "error: {err}");
    }

    #[test]
    fn list_dash_formats_lists_representations() {
        let mut files = HashMap::new();
        files.insert("/bbb.mpd".to_string(), BBB_MPD.as_bytes().to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let info = list_dash_formats(&format!("http://127.0.0.1:{port}/bbb.mpd"), &None, None).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(info.kind, StreamKind::Dash);
        assert_eq!(info.duration_secs, Some(8));
        assert_eq!(info.formats.len(), 3);
        let v720 = info.formats.iter().find(|f| f.id == "v720").unwrap();
        assert_eq!(v720.height, Some(720));
        assert!(v720.is_video && !v720.is_audio);
        assert!(v720.needs_merge); // separate audio track exists
        assert_eq!(v720.ext, "mp4");
        let a = info.formats.iter().find(|f| f.id == "a64k").unwrap();
        assert!(a.is_audio && !a.is_video);
        assert_eq!(a.ext, "m4a");
        assert!(!a.needs_merge);
    }

    #[test]
    fn download_dash_with_assembles_template_segments() {
        let mut files = HashMap::new();
        // Video-only manifest (no audio set): no merge, plain concat.
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period><AdaptationSet mimeType="video/mp4" contentType="video">
  <SegmentTemplate duration="120" timescale="30" media="v_$Number$.m4v" startNumber="1" initialization="v_init.m4v"/>
  <Representation id="v1" bandwidth="1000000" width="640" height="360"/>
 </AdaptationSet></Period></MPD>"#;
        files.insert("/v.mpd".to_string(), mpd.as_bytes().to_vec());
        files.insert("/v_init.m4v".to_string(), b"INIT-".to_vec());
        files.insert("/v_1.m4v".to_string(), b"SEG-ONE-".to_vec());
        files.insert("/v_2.m4v".to_string(), b"SEG-TWO".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("dash-dl");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let opts = DashDownloadOptions {
            manifest_url: format!("http://127.0.0.1:{port}/v.mpd"),
            representation_id: Some("v1".to_string()),
            destination_dir: dir.clone(),
            filename: "clip.mp4".to_string(),
            referer: None,
        };
        let out = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled, None).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"INIT-SEG-ONE-SEG-TWO");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_dash_with_segment_list_ranges() {
        let mut files = HashMap::new();
        files.insert("/list.mpd".to_string(), LIST_MPD.as_bytes().to_vec());
        files.insert("/init.mp4".to_string(), b"INIT!".to_vec());
        files.insert("/seg1.m4v".to_string(), b"SEG1".to_vec());
        let mut file_bytes = vec![b'A'; 100];
        file_bytes.extend(vec![b'B'; 100]);
        file_bytes.extend(vec![b'C'; 100]);
        files.insert("/file.mp4".to_string(), file_bytes);
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("dash-list");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let opts = DashDownloadOptions {
            manifest_url: format!("http://127.0.0.1:{port}/list.mpd"),
            representation_id: Some("v1".to_string()),
            destination_dir: dir.clone(),
            filename: "clip.mp4".to_string(),
            referer: None,
        };
        let out = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled, None).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        // init + seg1 + bytes 100..199 of file.mp4 (100 B's). The
        // test server ignores Range and returns the whole file, so
        // this also proves the 200-response slicing path.
        let mut expected = b"INIT!SEG1".to_vec();
        expected.extend(vec![b'B'; 100]);
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_dash_with_merges_separate_audio() {
        // Needs a real ffmpeg; skip where none exists.
        let ffmpeg = match crate::ffmpeg::find_on_path("ffmpeg") {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("dash-merge");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |args: &[&str]| {
            use std::process::{Command, Stdio};
            Command::new(&ffmpeg)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let vmp4 = dir.join("v.mp4");
        let am4a = dir.join("a.m4a");
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", vmp4.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "sine=frequency=440:d=1",
            "-c:a", "aac", am4a.to_str().unwrap()
        ]));
        let vbytes = std::fs::read(&vmp4).unwrap();
        let abytes = std::fs::read(&am4a).unwrap();
        let (v0, v1) = vbytes.split_at(vbytes.len() / 2);
        let (a0, a1) = abytes.split_at(abytes.len() / 2);
        // 1 s manifest, 1 s segments: init + 1 segment per track.
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT1S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate duration="1" timescale="1" media="v_$Number$.m4v" startNumber="1" initialization="v_init.m4v"/>
   <Representation id="v1" bandwidth="1000000" width="64" height="64"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <SegmentTemplate duration="1" timescale="1" media="a_$Number$.m4a" startNumber="1" initialization="a_init.m4a"/>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period></MPD>"#;
        let mut files = HashMap::new();
        files.insert("/v.mpd".to_string(), mpd.as_bytes().to_vec());
        files.insert("/v_init.m4v".to_string(), v0.to_vec());
        files.insert("/v_1.m4v".to_string(), v1.to_vec());
        files.insert("/a_init.m4a".to_string(), a0.to_vec());
        files.insert("/a_1.m4a".to_string(), a1.to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let opts = DashDownloadOptions {
            manifest_url: format!("http://127.0.0.1:{port}/v.mpd"),
            representation_id: Some("v1".to_string()),
            destination_dir: dir.clone(),
            filename: "merged.mp4".to_string(),
            referer: None,
        };
        let out =
            download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled, None).unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        // No ffmpeg -> the same manifest is refused, not silently
        // downloaded as a silent video.
        let err = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled, None).unwrap_err();
        assert!(err.contains("ffmpeg"), "error: {err}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `serve_map` with a file map the test can mutate between
    /// runs (same port, so segment URLs — and the resume meta —
    /// stay identical): models a CDN that 404s a segment on the
    /// first run and serves it on the retry.
    fn serve_map_shared(
        files: std::sync::Arc<std::sync::RwLock<HashMap<String, Vec<u8>>>>,
        hits: std::sync::Arc<Mutex<Vec<String>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    hits.lock().unwrap().push(path.clone());
                    let body = files.read().unwrap().get(&path).cloned();
                    match body {
                        Some(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&body);
                        }
                        None => {
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    }
                    let _ = stream.flush();
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn dash_resume_skips_completed_video_phase() {
        // Round 7, Finding A, end to end: the nime DASH download
        // failed in the AUDIO phase (HTTP 429 at audio segment
        // 121) after the video phase had fully completed — and the
        // resume re-downloaded all 1,768 video segments from
        // scratch. A resume must reuse the completed video phase
        // (and the partial audio phase) and fetch only the missing
        // pieces. Needs a real ffmpeg; skip where none exists.
        let ffmpeg = match crate::ffmpeg::find_on_path("ffmpeg") {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("dash-resume");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |args: &[&str]| {
            use std::process::{Command, Stdio};
            Command::new(&ffmpeg)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let vmp4 = dir.join("v.mp4");
        let am4a = dir.join("a.m4a");
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", vmp4.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "sine=frequency=440:d=1",
            "-c:a", "aac", am4a.to_str().unwrap()
        ]));
        let vbytes = std::fs::read(&vmp4).unwrap();
        let abytes = std::fs::read(&am4a).unwrap();
        // 2 s manifest, 1 s segments: init + 2 media segments per
        // track. Split each real file into 3 parts so the
        // concatenation is the valid original.
        let (v_init, v_rest) = vbytes.split_at(vbytes.len() / 3);
        let (v_1, v_2) = v_rest.split_at(v_rest.len() / 2);
        let (a_init, a_rest) = abytes.split_at(abytes.len() / 3);
        let (a_1, a_2) = a_rest.split_at(a_rest.len() / 2);
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT2S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate duration="1" timescale="1" media="v_$Number$.m4v" startNumber="1" initialization="v_init.m4v"/>
   <Representation id="v1" bandwidth="1000000" width="64" height="64"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <SegmentTemplate duration="1" timescale="1" media="a_$Number$.m4a" startNumber="1" initialization="a_init.m4a"/>
   <Representation id="a1" bandwidth="64000"/>
  </AdaptationSet>
 </Period></MPD>"#;
        let files = std::sync::Arc::new(std::sync::RwLock::new(HashMap::new()));
        {
            let mut m = files.write().unwrap();
            m.insert("/v.mpd".to_string(), mpd.as_bytes().to_vec());
            m.insert("/v_init.m4v".to_string(), v_init.to_vec());
            m.insert("/v_1.m4v".to_string(), v_1.to_vec());
            m.insert("/v_2.m4v".to_string(), v_2.to_vec());
            m.insert("/a_init.m4a".to_string(), a_init.to_vec());
            m.insert("/a_1.m4a".to_string(), a_1.to_vec());
            // NOTE: no /a_2.m4a yet — the audio phase fails on it.
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map_shared(files.clone(), hits.clone(), stop.clone());
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let opts = DashDownloadOptions {
            manifest_url: format!("http://127.0.0.1:{port}/v.mpd"),
            representation_id: Some("v1".to_string()),
            destination_dir: dir.clone(),
            filename: "merged.mp4".to_string(),
            referer: None,
        };
        // Run 1: video completes, audio fails on its last segment.
        let err =
            download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled, None)
                .unwrap_err();
        assert!(err.contains("Segment 3 of 3"), "error: {err}");
        let video_part = dir.join("merged.mp4.part");
        let video_seg_dir = dir.join("merged.mp4.part.seg");
        let audio_seg_dir = dir.join("merged.mp4.audio.part.seg");
        assert_eq!(std::fs::read(&video_part).unwrap(), vbytes);
        assert!(
            video_seg_dir.is_dir(),
            "a completed phase keeps its pieces for resume"
        );
        assert_eq!(
            std::fs::read_dir(&audio_seg_dir).unwrap().count(),
            3, // meta + 2 downloaded audio pieces
            "the partial audio phase keeps its pieces"
        );
        // The CDN recovers: the missing segment is servable now.
        files
            .write()
            .unwrap()
            .insert("/a_2.m4a".to_string(), a_2.to_vec());
        hits.lock().unwrap().clear();
        // Run 2 (resume): must fetch ONLY the missing audio
        // segment — no video piece, no already-downloaded audio
        // piece — then merge and clean up.
        let out =
            download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled, None)
                .expect("resume must complete from the saved pieces");
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        let hits = hits.lock().unwrap().clone();
        assert!(
            !hits.iter().any(|h| h.starts_with("/v_")),
            "resume re-fetched video segments: {hits:?}"
        );
        assert!(
            !hits.iter().any(|h| h == "/a_init.m4a" || h == "/a_1.m4a"),
            "resume re-fetched completed audio pieces: {hits:?}"
        );
        assert_eq!(
            hits.iter().filter(|h| h.as_str() == "/a_2.m4a").count(),
            1,
            "only the missing audio segment is fetched: {hits:?}"
        );
        assert!(
            !video_seg_dir.exists() && !audio_seg_dir.exists(),
            "segment dirs are cleaned up once the final file exists"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_dash_with_refuses_drm() {
        let mpd = "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"><Period><AdaptationSet><ContentProtection schemeIdUri=\"urn:uuid:edef8ba9\"/></AdaptationSet></Period></MPD>";
        let mut files = HashMap::new();
        files.insert("/drm.mpd".to_string(), mpd.as_bytes().to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("dash-drm");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let opts = DashDownloadOptions {
            manifest_url: format!("http://127.0.0.1:{port}/drm.mpd"),
            representation_id: None,
            destination_dir: dir.clone(),
            filename: "x.mp4".to_string(),
            referer: None,
        };
        let err = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled, None).unwrap_err();
        assert!(err.contains("DRM"), "error: {err}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loopback server for the trickle test: serves one body to
    /// every connection, in `chunk`-byte writes `delay` apart —
    /// a slow-but-alive server, the shape a throttled CDN takes.
    fn serve_trickle(
        body: Vec<u8>,
        chunk: usize,
        delay: Duration,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    for part in body.chunks(chunk) {
                        let _ = stream.write_all(part);
                        let _ = stream.flush();
                        std::thread::sleep(delay);
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    /// `serve_map`, but records the full raw request text of every
    /// request (for the browser-headers assertions).
    fn serve_map_recording(
        files: HashMap<String, Vec<u8>>,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    seen.lock().unwrap().push(req);
                    match files.get(&path) {
                        Some(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        None => {
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    }
                    let _ = stream.flush();
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn parallel_segments_match_sequential_byte_for_byte() {
        // The parallel engine must produce exactly the output the
        // sequential engine produced for the same playlist —
        // segment order in the file is playlist order, never
        // arrival order — and its progress reports must carry the
        // segment counts and the running size estimate.
        let bodies: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("SEGMENT-{i}-").repeat(i + 1).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let segments: Vec<Segment> = (0..8)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let dir_p = unique_temp_dir("seg-parallel");
        std::fs::create_dir_all(&dir_p).unwrap();
        let part_p = dir_p.join("movie.ts.part");
        let reports: std::sync::Arc<Mutex<Vec<Progress>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let reports_c = reports.clone();
        let n_p = download_segments(
            &client,
            &segments,
            &part_p,
            &None,
            0,
            &move |p| reports_c.lock().unwrap().push(p),
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        assert_eq!(n_p as usize, expected.len());
        assert_eq!(std::fs::read(&part_p).unwrap(), expected);

        let dir_s = unique_temp_dir("seg-sequential");
        std::fs::create_dir_all(&dir_s).unwrap();
        let part_s = dir_s.join("movie.ts.part");
        let n_s = download_segments_sequential(
            &client,
            &segments,
            &part_s,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        assert_eq!(n_s as usize, expected.len());
        assert_eq!(std::fs::read(&part_s).unwrap(), expected);

        // Progress: the last report shows all 8 pieces done and
        // the estimate converged on the real total.
        let reports = reports.lock().unwrap();
        let last = reports.last().expect("at least one progress report");
        assert_eq!(last.segments_done, Some(8));
        assert_eq!(last.segments_total, Some(8));
        assert_eq!(last.estimated_total, Some(expected.len() as u64));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir_p);
        let _ = std::fs::remove_dir_all(&dir_s);
    }

    #[test]
    fn slow_trickle_segment_succeeds() {
        // A slow-but-alive server (10 bytes every 100 ms) must
        // still deliver the whole segment: the client timeout is
        // a total budget, not a per-read deadline, and the stall
        // watchdog only fires when NO segment completes.
        let body = b"TRICKLE-SEGMENT-BODY-0123456789".to_vec();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) =
            serve_trickle(body.clone(), 10, Duration::from_millis(100), stop.clone());
        let dir = unique_temp_dir("seg-trickle");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/seg0.ts"),
            range: None,
        }];
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        assert_eq!(n as usize, body.len());
        assert_eq!(std::fs::read(&part).unwrap(), body);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stalling_segment_fails_fast_with_timeout_reason() {
        // Atlantic 2026-11-22, round 6: the first segment's body
        // never arrives. With a short client timeout the run must
        // fail after ONE try, quickly, saying "timed out" — not
        // burn three long attempts behind a bare "error decoding
        // response body".
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (port, server) = serve_stalled_body(1_000_000, Duration::from_secs(3), seen);
        let dir = unique_temp_dir("seg-stall-fast");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/seg0.ts"),
            range: None,
        }];
        let client = hls_client_with_timeout(Duration::from_secs(1)).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let start = Instant::now();
        let err = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(err.contains("timed out"), "error: {err}");
        assert!(err.contains("after 1 try"), "error: {err}");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "fail-fast took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stub CDN that stalls the first `stall_first` requests (each
    /// handled on its own thread, so concurrent requests stall in
    /// parallel like a real throttling CDN) and serves the rest
    /// instantly. Models totallyacdn (atlantic, 2026-11-23): the
    /// parallel pass times out, the sequential retry succeeds.
    fn serve_stall_first_n(
        files: HashMap<String, Vec<u8>>,
        stall_first: usize,
        stall: Duration,
        hits: std::sync::Arc<Mutex<Vec<String>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let files = std::sync::Arc::new(files);
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let files = files.clone();
                    let hits = hits.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 8192];
                        let len = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..len]).to_string();
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        hits.lock().unwrap().push(path.clone());
                        if n < stall_first {
                            // Stalled past the client's timeout: the
                            // client gives up; the write below fails
                            // silently on the closed socket.
                            std::thread::sleep(stall);
                        }
                        if let Some(body) = files.get(&path) {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                            let _ = stream.flush();
                        }
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn parallel_timeout_falls_back_to_sequential_and_succeeds() {
        // Round 7: the parallel pass times out (a throttling CDN),
        // the sequential retry over the same segment dir completes
        // the download. Without the fallback this fails outright.
        let bodies: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("SEGMENT-{i}-").repeat(i + 1).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        // The first 4 requests (the parallel pass, 4 workers) stall
        // past the 300 ms client timeout; later requests serve.
        let (port, server) = serve_stall_first_n(
            files,
            4,
            Duration::from_secs(2),
            hits.clone(),
            stop.clone(),
        );
        let segments: Vec<Segment> = (0..8)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client_with_timeout(Duration::from_millis(300)).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-fallback-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let start = Instant::now();
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .expect("the sequential fallback must rescue the download");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        // 4 timed-out parallel attempts + 8 sequential fetches: the
        // retry really ran (a bare parallel success would be 8).
        // Bind the guard once: locking twice in one assert deadlocks
        // (the first guard lives until the end of the statement).
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 12, "hits: {hits:?}");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "fallback took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_timeout_fallback_still_fails_with_timeout_reason() {
        // Both passes time out: the record fails with the ORIGINAL
        // timeout error (the rescue attempt never rewrites the
        // diagnosis), quickly, and the hit count proves the
        // sequential attempt really ran.
        let mut files = HashMap::new();
        files.insert("/seg0.ts".to_string(), b"SEGMENT".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_stall_first_n(
            files,
            usize::MAX,
            Duration::from_secs(2),
            hits.clone(),
            stop.clone(),
        );
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/seg0.ts"),
            range: None,
        }];
        let client = hls_client_with_timeout(Duration::from_millis(300)).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-fallback-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let start = Instant::now();
        let err = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(err.contains("timed out"), "error: {err}");
        assert!(err.contains("after 1 try"), "error: {err}");
        // 1 parallel attempt + 1 sequential attempt: the fallback
        // ran exactly once. (With a single segment only one worker
        // fetches per pass — the other workers claim indices past
        // the end and return without a request.) Bind the guard
        // once: locking twice in one assert deadlocks.
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 2, "hits: {hits:?}");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "bounded failure took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_fallback_sequential_gates_on_segment_timeouts() {
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        assert!(should_fallback_sequential(
            "Segment 2 of 1730 could not be downloaded after 1 try. (error decoding response body: operation timed out)",
            &paused,
            &cancelled
        ));
        assert!(!should_fallback_sequential(
            "Segment 1 of 8 could not be downloaded after 3 tries. (HTTP 404)",
            &paused,
            &cancelled
        ));
        assert!(!should_fallback_sequential(
            "The download stalled: no progress for 5 minutes.",
            &paused,
            &cancelled
        ));
        assert!(!should_fallback_sequential("__PAUSED__", &paused, &cancelled));
        assert!(!should_fallback_sequential("__CANCELLED__", &paused, &cancelled));
        paused.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!should_fallback_sequential(
            "Segment 2 of 1730 could not be downloaded after 1 try. (operation timed out)",
            &paused,
            &cancelled
        ));
    }

    #[test]
    fn is_rate_limit_error_gates_on_429_segment_failures() {
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        assert!(is_rate_limit_error(
            "Segment 121 of 2117 could not be downloaded after 3 tries. (Segment request failed: HTTP 429 Too Many Requests)",
            &paused,
            &cancelled
        ));
        assert!(!is_rate_limit_error(
            "Segment 1 of 8 could not be downloaded after 3 tries. (Segment request failed: HTTP 404 Not Found)",
            &paused,
            &cancelled
        ));
        assert!(!is_rate_limit_error(
            "Segment 2 of 1730 could not be downloaded after 1 try. (error decoding response body: operation timed out)",
            &paused,
            &cancelled
        ));
        assert!(!is_rate_limit_error("__PAUSED__", &paused, &cancelled));
        paused.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(!is_rate_limit_error(
            "Segment 1 of 8 could not be downloaded after 3 tries. (HTTP 429)",
            &paused,
            &cancelled
        ));
    }

    #[test]
    fn parse_retry_after_seconds_and_dates() {
        assert_eq!(parse_retry_after("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        // Clamped: a server that asks for an hour gets a minute.
        assert_eq!(parse_retry_after("3600"), Some(Duration::from_secs(60)));
        assert_eq!(parse_retry_after("garbage"), None);
        assert_eq!(parse_retry_after(""), None);
        // HTTP-date in the past: no wait.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        // HTTP-date far in the future: the delta, clamped to 60 s.
        assert_eq!(
            parse_retry_after("Wed, 18 May 2033 03:33:20 GMT"),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn parse_http_date_known_values() {
        // 2015-10-21 07:28:00 UTC (verified with date(1)).
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::from_secs(1_445_412_480))
        );
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("Wed, 32 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_http_date("Wed, 21 Foo 2015 07:28:00 GMT"), None);
        assert_eq!(parse_http_date("Wed, 21 Oct 1969 07:28:00 GMT"), None);
    }

    #[test]
    fn rate_limit_backoff_is_exponential_and_capped() {
        assert_eq!(rate_limit_backoff(0, None), Duration::from_secs(5));
        assert_eq!(rate_limit_backoff(1, None), Duration::from_secs(10));
        assert_eq!(rate_limit_backoff(2, None), Duration::from_secs(20));
        assert_eq!(rate_limit_backoff(3, None), Duration::from_secs(40));
        assert_eq!(rate_limit_backoff(4, None), Duration::from_secs(60));
        assert_eq!(rate_limit_backoff(9, None), Duration::from_secs(60));
        assert_eq!(
            rate_limit_backoff(0, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        assert_eq!(
            rate_limit_backoff(0, Some(Duration::from_secs(3600))),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn rate_limiter_delay_doubles_to_cap_and_only_when_throttled() {
        let limiter = RateLimiter::new();
        // Unpaced: escalations are no-ops (the parallel pass never
        // paces, however many 429s it sees).
        limiter.escalate_delay();
        assert!(limiter.state.lock().unwrap().delay.is_zero());
        limiter.begin_throttled_pass();
        assert_eq!(
            limiter.state.lock().unwrap().delay,
            Duration::from_millis(250)
        );
        limiter.escalate_delay();
        assert_eq!(
            limiter.state.lock().unwrap().delay,
            Duration::from_millis(500)
        );
        limiter.escalate_delay();
        assert_eq!(limiter.state.lock().unwrap().delay, Duration::from_secs(1));
        limiter.escalate_delay();
        assert_eq!(limiter.state.lock().unwrap().delay, Duration::from_secs(2));
        limiter.escalate_delay();
        assert_eq!(
            limiter.state.lock().unwrap().delay,
            Duration::from_secs(2),
            "the delay caps at 2 s"
        );
    }

    #[test]
    fn rate_limiter_counts_consecutive_429s_and_resets() {
        let limiter = RateLimiter::new();
        assert_eq!(limiter.initial_backoff(), Duration::from_secs(5));
        assert_eq!(limiter.note_429(None), 1);
        assert_eq!(limiter.note_429(Some(Duration::from_secs(2))), 2);
        assert_eq!(limiter.initial_backoff(), Duration::from_secs(2));
        // A success resets the streak (but keeps the last
        // Retry-After — the rescue's initial backoff still uses
        // the freshest value the server sent).
        limiter.note_success();
        assert_eq!(limiter.note_429(None), 1);
        assert_eq!(limiter.initial_backoff(), Duration::from_secs(2));
        // The rescue pass starts with a fresh budget.
        limiter.note_429(None);
        limiter.begin_throttled_pass();
        assert_eq!(limiter.note_429(None), 1);
    }

    /// Loopback server for rate-limit tests: answers 429 to the
    /// first `n429` requests (with `retry_after` as the
    /// `Retry-After` header value when Some), then serves `files`
    /// (404 for unknown paths). Records (path, arrival time,
    /// status) per request in `hits`, so tests can assert on
    /// backoff waits and pacing gaps.
    fn serve_429_first_n(
        files: HashMap<String, Vec<u8>>,
        n429: usize,
        retry_after: Option<String>,
        hits: std::sync::Arc<Mutex<Vec<(String, Instant, u16)>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let files = std::sync::Arc::new(files);
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let files = files.clone();
                    let hits = hits.clone();
                    let retry_after = retry_after.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 8192];
                        let len = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..len]).to_string();
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        let at = Instant::now();
                        if n < n429 {
                            hits.lock().unwrap().push((path, at, 429));
                            let ra = retry_after
                                .map(|v| format!("Retry-After: {v}\r\n"))
                                .unwrap_or_default();
                            let _ = stream.write_all(
                                format!(
                                    "HTTP/1.1 429 Too Many Requests\r\n{ra}Content-Length: 0\r\nConnection: close\r\n\r\n"
                                )
                                .as_bytes(),
                            );
                        } else if let Some(body) = files.get(&path) {
                            hits.lock().unwrap().push((path, at, 200));
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                        } else {
                            hits.lock().unwrap().push((path, at, 404));
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                        let _ = stream.flush();
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn rate_limit_429_then_serve_completes_after_backoff() {
        // Round 7 follow-up: a 429 blip (no Retry-After) is
        // retried after the default 5 s backoff, and the download
        // completes — the 429 must not fail a download that the
        // server will serve a moment later.
        let bodies: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("SEGMENT-{i}-").repeat(2).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_429_first_n(files, 4, None, hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..8)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-429-backoff");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let start = Instant::now();
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .expect("a 429 blip must not fail the download");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        // Bind the guard once: locking twice in one assert deadlocks.
        let hits = hits.lock().unwrap();
        let n429 = hits.iter().filter(|h| h.2 == 429).count();
        let n200 = hits.iter().filter(|h| h.2 == 200).count();
        assert_eq!(n429, 4, "hits: {hits:?}");
        assert_eq!(n200, 8, "hits: {hits:?}");
        // The retries waited out the 5 s default backoff: the last
        // request lands ~5 s after the first (4.5 s floor allows
        // scheduling jitter; a missing backoff would finish in ms).
        let first = hits.iter().map(|h| h.1).min().unwrap();
        let last = hits.iter().map(|h| h.1).max().unwrap();
        assert!(
            last - first >= Duration::from_millis(4500),
            "no 5 s backoff visible: span {:?}",
            last - first
        );
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "429 recovery took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_retry_after_header_is_honoured() {
        // The server's Retry-After (1 s) replaces the default 5 s
        // backoff: the whole download finishes in ~1 s, not ~5 s.
        let bodies: Vec<Vec<u8>> = (0..4)
            .map(|i| format!("SEGMENT-{i}-").repeat(2).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) =
            serve_429_first_n(files, 2, Some("1".to_string()), hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..4)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-429-retry-after");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .expect("a Retry-After 429 must not fail the download");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        // Bind the guard once: locking twice in one assert deadlocks.
        let hits = hits.lock().unwrap();
        let n429 = hits.iter().filter(|h| h.2 == 429).count();
        let n200 = hits.iter().filter(|h| h.2 == 200).count();
        assert_eq!(n429, 2, "hits: {hits:?}");
        assert_eq!(n200, 4, "hits: {hits:?}");
        // The retries waited ~1 s (the Retry-After), not ~5 s (the
        // default): the span from the first to the last request is
        // at least 0.8 s (a wait happened) and under 4 s (the
        // default backoff would put it at ~5 s).
        let first = hits.iter().map(|h| h.1).min().unwrap();
        let last = hits.iter().map(|h| h.1).max().unwrap();
        let span = last - first;
        assert!(
            span >= Duration::from_millis(800) && span < Duration::from_secs(4),
            "Retry-After: 1 was not honoured: span {span:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loopback server for the rescue test: answers 429 to every
    /// request until `until`, then serves `files`. The time window
    /// (not a request count) is what makes the parallel pass fail
    /// deterministically — a count-based stub lets a retry serve
    /// and reset the consecutive-429 counter before the budget
    /// trips, so the parallel pass can succeed via in-loop
    /// retries and the rescue never runs.
    fn serve_429_until(
        files: HashMap<String, Vec<u8>>,
        until: Instant,
        hits: std::sync::Arc<Mutex<Vec<(String, Instant, u16)>>>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = std::sync::Arc::new(files);
        let handle = std::thread::spawn(move || loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let files = files.clone();
                    let hits = hits.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 8192];
                        let len = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..len]).to_string();
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/")
                            .to_string();
                        let at = Instant::now();
                        if at < until {
                            hits.lock().unwrap().push((path, at, 429));
                            let _ = stream.write_all(
                                b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        } else if let Some(body) = files.get(&path) {
                            hits.lock().unwrap().push((path, at, 200));
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                        } else {
                            hits.lock().unwrap().push((path, at, 404));
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                        let _ = stream.flush();
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        });
        (port, handle)
    }

    #[test]
    fn rate_limit_sustained_429_falls_back_to_throttled_pass() {
        // Sustained 429s exhaust the parallel pass (the 5-streak
        // budget), and the throttled rescue (1 worker, paced)
        // completes the download once the server starts serving.
        // The serves in the rescue pass must be spaced by the
        // pacing delay (250 ms), not fired in a burst.
        let bodies: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("SEGMENT-{i}-").repeat(2).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        // 429 for the first 8 s: the parallel pass's retries (at
        // ~5 s) still 429, so the budget trips and the pass fails
        // deterministically. The rescue's 5 s initial backoff ends
        // after the window, so its requests serve.
        let until = Instant::now() + Duration::from_secs(8);
        let (port, server) = serve_429_until(files, until, hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..8)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-429-rescue");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let start = Instant::now();
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .expect("the throttled rescue must complete the download");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        // Bind the guard once: locking twice in one assert deadlocks.
        let hits = hits.lock().unwrap();
        let n429 = hits.iter().filter(|h| h.2 == 429).count();
        let n200 = hits.iter().filter(|h| h.2 == 200).count();
        assert!(n429 >= 5, "the parallel pass never hit 429s: {hits:?}");
        assert_eq!(n200, 8, "each segment served exactly once: {hits:?}");
        // Pacing proof: the serves (all in the rescue pass — the
        // parallel pass got only 429s) are spaced by the 250 ms
        // delay (200 ms floor allows scheduling jitter). A burst
        // would show sub-millisecond gaps.
        let mut serve_times: Vec<Instant> =
            hits.iter().filter(|h| h.2 == 200).map(|h| h.1).collect();
        serve_times.sort();
        for pair in serve_times.windows(2) {
            assert!(
                pair[1] - pair[0] >= Duration::from_millis(200),
                "rescue serves not paced: gaps {:?}",
                serve_times
                    .windows(2)
                    .map(|p| p[1] - p[0])
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "throttled rescue took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_429_forever_fails_bounded_with_429_error() {
        // A server that 429s every request (with Retry-After: 0,
        // so the backoffs stay short in the test) must fail the
        // record with the real 429 error after the bounded budget
        // — not loop forever, and not collapse into a stall error.
        let mut files = HashMap::new();
        files.insert("/seg0.ts".to_string(), b"SEGMENT".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_429_first_n(
            files,
            usize::MAX,
            Some("0".to_string()),
            hits.clone(),
            stop.clone(),
        );
        let segments = vec![Segment {
            url: format!("http://127.0.0.1:{port}/seg0.ts"),
            range: None,
        }];
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-429-forever");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let start = Instant::now();
        let err = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(err.contains("HTTP 429"), "error: {err}");
        assert!(err.contains("after 5 tries"), "error: {err}");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "bounded failure took {:?}",
            start.elapsed()
        );
        // 5 attempts in the parallel pass + 5 in the rescue pass:
        // the budget bounds the request count (no unbounded loop).
        // Bind the guard once: locking twice in one assert deadlocks.
        let hits = hits.lock().unwrap();
        assert_eq!(hits.len(), 10, "hits: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_resume_skips_completed_segments() {
        // A segment dir with a matching meta and two of four
        // pieces already on disk: only the two missing pieces are
        // fetched, and the assembled file is still the whole
        // playlist in order.
        let bodies: Vec<Vec<u8>> = (0..4)
            .map(|i| format!("PIECE-{i}-").repeat(3).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..4)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let dir = unique_temp_dir("seg-resume");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        // Pre-seed pieces 0 and 2 as a crashed run left them.
        let seg_dir = seg_dir_for(&part);
        std::fs::create_dir_all(&seg_dir).unwrap();
        write_seg_meta(&seg_dir, &segments);
        std::fs::write(seg_dir.join(seg_file_name(0)), &bodies[0]).unwrap();
        std::fs::write(seg_dir.join(seg_file_name(2)), &bodies[2]).unwrap();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let n = download_segments(
            &client,
            &segments,
            &part,
            &None,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        let expected: Vec<u8> = bodies.concat();
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        let mut got = hits.lock().unwrap().clone();
        got.sort();
        assert_eq!(got, vec!["/seg1.ts".to_string(), "/seg3.ts".to_string()]);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hls_requests_send_browser_headers() {
        // Round 6: the engine must announce itself as a current
        // Chrome, not as FetchForge, and carry the page as
        // Referer/Origin on manifest and segment fetches alike —
        // CDNs that gate on browser-like requests (Atlantic's
        // totallyacdn.cc) reject anything else.
        let variant = "#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n#EXT-X-ENDLIST\n";
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\nvar.m3u8\n";
        let mut files = HashMap::new();
        files.insert("/master.m3u8".to_string(), master.as_bytes().to_vec());
        files.insert("/var.m3u8".to_string(), variant.as_bytes().to_vec());
        files.insert("/seg0.ts".to_string(), b"SEGMENT-ZERO-".to_vec());
        files.insert("/seg1.ts".to_string(), b"SEGMENT-ONE".to_vec());
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map_recording(files, seen.clone(), stop.clone());
        let dir = unique_temp_dir("hls-headers");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let referer = Some("https://novahd.cc/watch/m/1368337".to_string());
        let master_url = format!("http://127.0.0.1:{port}/master.m3u8");
        let variant_url = format!("http://127.0.0.1:{port}/var.m3u8");
        let out = download_hls_variant(
            &master_url,
            &variant_url,
            &dir,
            "movie.ts",
            &referer,
            None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let seen = seen.lock().unwrap();
        // Master, variant, and both segments: four requests, all
        // browser-like.
        assert_eq!(seen.len(), 4, "requests: {seen:?}");
        for req in seen.iter() {
            let lower = req.to_lowercase();
            assert!(
                req.contains("Chrome/142.0.0.0"),
                "no Chrome UA in request: {req}"
            );
            assert!(
                !lower.contains("fetchforge"),
                "FetchForge UA leaked into request: {req}"
            );
            assert!(
                lower.contains("accept-encoding: identity"),
                "no identity encoding in request: {req}"
            );
            assert!(
                lower.contains("referer: https://novahd.cc/watch/m/1368337"),
                "no Referer in request: {req}"
            );
            assert!(
                lower.contains("origin: https://novahd.cc"),
                "no Origin in request: {req}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn estimate_total_bytes_matches_nime_sanity_check() {
        // The nime download (2026-11-22): 3,321 pieces, ~1.5 GB.
        // At 690 MB with 1,240 pieces done the estimate must land
        // in the right neighbourhood, and it must stay silent
        // until at least one piece has landed.
        assert_eq!(estimate_total_bytes(0, 0, 3321), None);
        assert_eq!(estimate_total_bytes(100, 5, 0), None);
        assert_eq!(estimate_total_bytes(100, 2, 4), Some(200));
        let est = estimate_total_bytes(690_000_000, 1240, 3321)
            .expect("estimate once pieces have landed");
        assert!(
            (1_800_000_000..2_000_000_000).contains(&est),
            "estimate {est} is not ~1.8 GB"
        );
    }

    #[test]
    fn download_segments_reuses_completed_phase_without_refetch() {
        // Round 7, Finding A: a phase that assembled successfully
        // keeps its segment dir, so a later run over the same part
        // path (a resume after a LATER phase failed) fetches
        // nothing — it re-assembles locally. The old code deleted
        // the dir on assembly, so a DASH resume re-downloaded the
        // whole video phase (1,768 segments) after the audio phase
        // failed with HTTP 429.
        let bodies: Vec<Vec<u8>> = (0..4)
            .map(|i| format!("PIECE-{i}-").repeat(3).into_bytes())
            .collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..4)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let expected: Vec<u8> = bodies.concat();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-reuse");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("movie.ts.part");
        let run = |hits: &std::sync::Arc<Mutex<Vec<String>>>| {
            hits.lock().unwrap().clear();
            download_segments(
                &client,
                &segments,
                &part,
                &None,
                0,
                &|_| {},
                &paused,
                &cancelled,
                DOWNLOAD_STALL_TIMEOUT,
            )
            .unwrap()
        };
        let n = run(&hits);
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        assert_eq!(hits.lock().unwrap().len(), 4, "first run fetches all");
        assert!(
            seg_dir_for(&part).is_dir(),
            "the segment dir must survive a successful assembly"
        );
        // Second run: same part path, same segments — zero HTTP
        // requests, same bytes, same assembled file.
        let n = run(&hits);
        assert_eq!(n as usize, expected.len());
        assert_eq!(std::fs::read(&part).unwrap(), expected);
        // Bind the guard once: locking twice in one assert deadlocks
        // (the first guard lives until the end of the statement).
        let hits = hits.lock().unwrap();
        assert!(
            hits.is_empty(),
            "resume re-fetched segments: {hits:?}"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_estimate_includes_completed_phases() {
        // Round 7, Finding B: during the second phase of a
        // multi-phase download, downloaded_bytes is cumulative
        // (progress_base + this phase) but the estimate used to be
        // per-phase only — the UI showed 1324% on a nime DASH
        // download (2.2 GB against a ~166 MB audio estimate). The
        // reported estimate must be the COMBINED total.
        let bodies: Vec<Vec<u8>> = (0..4).map(|_| vec![b'x'; 100]).collect();
        let mut files = HashMap::new();
        for (i, b) in bodies.iter().enumerate() {
            files.insert(format!("/seg{i}.ts"), b.clone());
        }
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let segments: Vec<Segment> = (0..4)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
            })
            .collect();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let dir = unique_temp_dir("seg-est-base");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("audio.ts.part");
        let reports = std::sync::Arc::new(Mutex::new(Vec::new()));
        let reports_clone = reports.clone();
        // progress_base = 1,000,000 (a completed video phase).
        download_segments(
            &client,
            &segments,
            &part,
            &None,
            1_000_000,
            &move |p| reports_clone.lock().unwrap().push((p.downloaded, p.estimated_total)),
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let reports = reports.lock().unwrap();
        let (dl, est) = reports.last().copied().expect("at least one report");
        assert_eq!(dl, 1_000_400, "cumulative bytes");
        assert_eq!(
            est,
            Some(1_000_400),
            "combined estimate: completed phase + this phase"
        );
        for (_, e) in reports.iter() {
            if let Some(e) = e {
                assert!(
                    *e >= 1_000_000,
                    "no estimate may fall below the completed phases: {e}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captured_manifest_used_instead_of_fetch() {
        // A signed playlist link may only work once: the extension
        // captures the text from the player's own response, and the
        // engine must use it instead of re-fetching the URL. Here
        // the master URL 404s — the download still succeeds, and
        // the server never sees a request for it.
        let variant = "#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n#EXT-X-ENDLIST\n";
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\nvar.m3u8\n";
        let mut files = HashMap::new();
        // NOTE: no /master.m3u8 — fetching it would 404.
        files.insert("/var.m3u8".to_string(), variant.as_bytes().to_vec());
        files.insert("/seg0.ts".to_string(), b"SEGMENT-ZERO-".to_vec());
        files.insert("/seg1.ts".to_string(), b"SEGMENT-ONE".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-captured");
        std::fs::create_dir_all(&dir).unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let master_url = format!("http://127.0.0.1:{port}/master.m3u8");
        let variant_url = format!("http://127.0.0.1:{port}/var.m3u8");
        let captured = CapturedManifest {
            url: master_url.clone(),
            text: master.to_string(),
        };
        let out = download_hls_variant(
            &master_url,
            &variant_url,
            &dir,
            "movie.ts",
            &None,
            None,
            &|_| {},
            &paused,
            &cancelled,
            Some(&captured),
        )
        .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZERO-SEGMENT-ONE");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let hits = hits.lock().unwrap();
        assert!(
            !hits.iter().any(|h| h == "/master.m3u8"),
            "master was re-fetched: {hits:?}"
        );
        assert!(hits.iter().any(|h| h == "/var.m3u8"), "hits: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
