//! Stream downloads: HLS (.m3u8) and DASH (.mpd), plus the variant /
//! representation parsing behind the quality picker (StreamAnalyzer,
//! HLSParser, DASHParser in the spec §11 module map).
//!
//! Supported ONLY when the manifest and segments are openly
//! accessible. Anything with DRM, a non-AES-128 EXT-X-KEY method
//! (SAMPLE-AES / FairPlay), ContentProtection, expiring signed
//! segments the caller cannot legally fetch, or authentication the
//! user has not supplied is refused with a plain explanation. No
//! DRM is bypassed, ever. Standard HLS AES-128 (round 12, item
//! 5) is the one exception: its key is served openly at the URI
//! the playlist itself declares, ffmpeg handles it natively, and
//! the native path decrypts it the same way — that is not a DRM
//! bypass, it is the documented HLS encryption.
//!
//! Live (unbounded) streams are not recorded: an HLS playlist is
//! fetched once and only the segments listed at that moment are
//! downloaded (a finite snapshot); a DASH manifest with
//! type="dynamic" is refused outright.

use crate::download::{Progress, SharedLimiter};
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

/// How a total size was determined (round 11, item 10 — the
/// study's XDM-style estimate ladder). The UI shows the marker,
/// so the user can tell an exact total from an estimate at a
/// glance: exact plain, average `≈`, peak `~`, measured `~`.
/// Serialized lowercase ("exact"/"average"/"peak"/"measured") on
/// records and format listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeKind {
    /// Byte-range sum, a server Content-Length, or an
    /// extractor-provided length: the true total.
    Exact,
    /// `AVERAGE-BANDWIDTH × duration`: the mean bitrate the
    /// manifest declares, usually within ~1%.
    Average,
    /// Peak `BANDWIDTH × duration` (or yt-dlp's
    /// `filesize_approx` from tbr): a rough upper bound, often
    /// 2–4× high for VBR content.
    Peak,
    /// Measured during the download (average segment size so
    /// far × segment count): converges on the true total as
    /// segments land.
    Measured,
}

impl SizeKind {
    /// The marker the UI prefixes the size with ("" for exact).
    pub fn marker(self) -> &'static str {
        match self {
            SizeKind::Exact => "",
            SizeKind::Average => "≈",
            SizeKind::Peak | SizeKind::Measured => "~",
        }
    }

    /// The weaker (less precise) of two kinds — for a merged
    /// total, the estimate dominates: an exact video plus an
    /// estimated audio is an estimated total.
    pub fn weaker(self, other: SizeKind) -> SizeKind {
        let rank = |k: SizeKind| match k {
            SizeKind::Exact => 0,
            SizeKind::Average => 1,
            SizeKind::Peak => 2,
            SizeKind::Measured => 3,
        };
        if rank(self) >= rank(other) {
            self
        } else {
            other
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

/// Bytes fetched by [`sniff_stream_kind`]: enough to see the
/// `#EXTM3U` header or the `<MPD` root element of a DASH manifest.
const SNIFF_PREFIX_BYTES: usize = 2048;

/// Classify the first bytes of a response as a stream manifest
/// (round 10, item 4): HLS playlists start with `#EXTM3U`
/// (allowing a UTF-8 BOM and leading whitespace); DASH manifests
/// are XML whose first 2 KB contain `<MPD`. Anything else is not
/// a playlist — binary media, HTML, JSON all return None. Pure
/// function, unit-tested; [`sniff_stream_kind`] fetches the bytes.
pub fn classify_stream_prefix(prefix: &[u8]) -> Option<StreamKind> {
    let text = String::from_utf8_lossy(prefix);
    let trimmed = text.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace());
    if trimmed.starts_with("#EXTM3U") {
        return Some(StreamKind::Hls);
    }
    if trimmed.starts_with("<?xml") && trimmed.contains("<MPD") {
        return Some(StreamKind::Dash);
    }
    if trimmed.starts_with("<MPD") {
        return Some(StreamKind::Dash);
    }
    None
}

/// Sniff an extensionless URL's first bytes to tell a playlist
/// from a file (round 10, item 4). Some playlist endpoints serve
/// a generic content type, so content-type routing alone misses
/// them (MoviesJoy/NOXX/123Movies served HLS master text as a
/// ~940 B "file"). One ranged GET (first 2 KB, 30 s timeout);
/// content type first, then the prefix classifier. None on any
/// failure — the caller proceeds down its normal path (fail open).
/// Never sends cookies or credentials (the client has none).
pub fn sniff_stream_kind(url: &str, referer: &Option<String>) -> Option<StreamKind> {
    sniff_stream_kind_with_proxy(url, referer, None)
}

/// [`sniff_stream_kind`] through the user-configured proxy
/// (round 11 extension, feature 10): without it, a proxy user
/// could not detect an extensionless stream at all.
pub fn sniff_stream_kind_with_proxy(
    url: &str,
    referer: &Option<String>,
    proxy: Option<&str>,
) -> Option<StreamKind> {
    use std::io::Read;
    let client = hls_client_with_timeout_and_proxy(Duration::from_secs(30), proxy).ok()?;
    let req = browser_headers(
        client
            .get(url)
            .header("Range", format!("bytes=0-{}", SNIFF_PREFIX_BYTES - 1)),
        referer,
    );
    let resp = req.send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if let Some(ct) = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(kind) = stream_kind_of_content_type(ct) {
            return Some(kind);
        }
    }
    let mut prefix = Vec::new();
    resp.take(SNIFF_PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)
        .ok()?;
    classify_stream_prefix(&prefix)
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
    /// How `filesize` was determined (round 11, item 10 — the
    /// estimate ladder). None when there is no size at all.
    pub size_kind: Option<SizeKind>,
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
    /// Audio languages the stream offers (round 11 extension,
    /// feature 4): the distinct `LANGUAGE` values of the HLS
    /// audio renditions / DASH audio AdaptationSets (falling
    /// back to the rendition NAME when no LANGUAGE is set).
    /// Empty when the stream has no separate audio tracks.
    pub audio_languages: Vec<String>,
    /// Subtitle languages the stream offers (round 11
    /// extension, feature 2): the distinct `LANGUAGE` values
    /// of the HLS SUBTITLES renditions / DASH WebVTT text
    /// AdaptationSets. TTML text tracks are NOT listed — the
    /// engine cannot assemble TTML segments (see the notes).
    pub subtitle_languages: Vec<String>,
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
    list_stream_formats_with_proxy(url, referer, captured, None)
}

/// [`list_stream_formats`] through the user-configured proxy
/// (round 11 extension, feature 10): the manifest fetch uses
/// it, so a proxy user can list a stream's qualities at all.
pub fn list_stream_formats_with_proxy(
    url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
    proxy: Option<&str>,
) -> Result<StreamInfo, String> {
    match stream_kind_of_url(url) {
        Some(kind) => list_stream_formats_kind_with_proxy(url, kind, referer, captured, proxy),
        None => Err("Not an HLS (.m3u8) or DASH (.mpd) URL.".to_string()),
    }
}

/// [`list_stream_formats`] with the protocol given (round 10,
/// item 4): for extensionless playlist URLs the caller already
/// sniffed the kind from the first bytes, so the URL extension
/// must not be consulted.
pub fn list_stream_formats_kind(
    url: &str,
    kind: StreamKind,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    list_stream_formats_kind_with_proxy(url, kind, referer, captured, None)
}

/// [`list_stream_formats_kind`] through the user-configured
/// proxy (round 11 extension, feature 10).
pub fn list_stream_formats_kind_with_proxy(
    url: &str,
    kind: StreamKind,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
    proxy: Option<&str>,
) -> Result<StreamInfo, String> {
    match kind {
        StreamKind::Hls => list_hls_formats_with_proxy(url, referer, captured, proxy),
        StreamKind::Dash => list_dash_formats_with_proxy(url, referer, captured, proxy),
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
        "master",
        "index",
        "playlist",
        "manifest",
        "chunklist",
        "hls",
        "dash",
        "video",
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
    hls_client_with_timeout_and_proxy(timeout, None)
}

/// [`hls_client_with_timeout`] through the user-configured proxy
/// (round 11 extension, feature 10). The proxy value is never
/// logged and never appears in the error message — it may
/// contain credentials.
fn hls_client_with_timeout_and_proxy(
    timeout: Duration,
    proxy: Option<&str>,
) -> Result<reqwest::blocking::Client, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(BROWSER_USER_AGENT)
        .http1_only()
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(60))
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(p) = proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(p)
                .map_err(|_| "The configured proxy could not be used.".to_string())?,
        );
    }
    builder.build().map_err(|e| e.to_string())
}

/// The extra knobs a stream download takes beyond the positional
/// arguments of the `download_hls_*` / `download_dash` functions
/// (round 11 extension): grouped into one struct so the existing
/// 10-argument signatures do not grow a further five parameters.
/// The `_opts` download functions take one of these; the old
/// functions pass `StreamDownloadOptions::default()` unchanged.
#[derive(Debug, Clone, Default)]
pub struct StreamDownloadOptions {
    /// Audio language chosen in the popup (feature 4): the
    /// `LANGUAGE` (or NAME) of the HLS audio rendition / DASH
    /// audio AdaptationSet to use. None means the default
    /// (first rendition / highest-bandwidth audio).
    pub audio_language: Option<String>,
    /// Subtitle language chosen in the popup (feature 2): the
    /// `LANGUAGE` (or NAME) of the subtitle rendition to mux
    /// into the output. None means no subtitles.
    pub subtitle_language: Option<String>,
    /// User-configured proxy (feature 10), validated at
    /// settings-save time. Never logged.
    pub proxy_url: Option<String>,
    /// Global speed limit in KB/s (feature 3), enforced across
    /// the parallel segment workers by one shared limiter.
    pub bandwidth_kbps: Option<u64>,
    /// Title metadata for the merged output (feature 12).
    pub title: Option<String>,
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
            if let (Some(host), Some(port)) = (u.host_str(), u.port_or_known_default()) {
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
        if (k == "url" || k == "u") && stream_kind_of_url(&v) == Some(StreamKind::Hls) {
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
    /// Mean bitrate of the whole variant, when the master
    /// declares `AVERAGE-BANDWIDTH` (round 11, item 10): the
    /// estimate ladder's second rung — peak `BANDWIDTH` can be
    /// 4× the average on VBR content, so the average is the
    /// estimate to use when present.
    pub average_bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub fps: Option<f64>,
    /// The AUDIO group this variant's sound lives in, if it is
    /// separate from the video (see [`HlsMaster::audio`]).
    pub audio_group: Option<String>,
    /// The SUBTITLES group this variant's captions live in, if
    /// any (round 11 extension, feature 2; see
    /// [`HlsMaster::subtitles`]).
    pub subtitles_group: Option<String>,
}

/// An `#EXT-X-MEDIA` audio rendition of a master playlist.
#[derive(Debug, Clone)]
pub struct HlsAudioGroup {
    pub group_id: String,
    /// Absolute URL of the audio (media) playlist.
    pub uri: String,
    pub name: Option<String>,
    /// The rendition's `LANGUAGE` attribute (an ISO 639 code such
    /// as "en"), when the master declares one. A group can hold
    /// several renditions with different languages (novahd
    /// carries 7); the user picks one (round 11 extension,
    /// feature 4) and the download uses that rendition's playlist
    /// instead of the first one in the group.
    pub language: Option<String>,
}

/// An `#EXT-X-MEDIA` subtitle rendition of a master playlist
/// (round 11 extension, feature 2): a WebVTT media playlist the
/// user can ask to have muxed into the output file.
#[derive(Debug, Clone)]
pub struct HlsSubtitleGroup {
    pub group_id: String,
    /// Absolute URL of the subtitle (media) playlist.
    pub uri: String,
    pub name: Option<String>,
    /// The rendition's `LANGUAGE` attribute (an ISO 639 code such
    /// as "en"), when the master declares one.
    pub language: Option<String>,
}

/// A parsed HLS master playlist. A media playlist parses to zero
/// variants (its segments come from [`parse_hls_segments`]).
#[derive(Debug, Clone, Default)]
pub struct HlsMaster {
    pub variants: Vec<HlsVariant>,
    pub audio: Vec<HlsAudioGroup>,
    /// Subtitle renditions (TYPE=SUBTITLES), one entry per
    /// `#EXT-X-MEDIA` line — a group can hold several languages.
    pub subtitles: Vec<HlsSubtitleGroup>,
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
                average_bandwidth: map.get("AVERAGE-BANDWIDTH").and_then(|v| v.parse().ok()),
                width,
                height,
                codecs: map.get("CODECS").map(|s| s.to_string()),
                fps: map.get("FRAME-RATE").and_then(|v| v.parse().ok()),
                audio_group: map.get("AUDIO").map(|s| s.to_string()),
                subtitles_group: map.get("SUBTITLES").map(|s| s.to_string()),
            });
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
            let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
            // TYPE=AUDIO and TYPE=SUBTITLES share the shape
            // (GROUP-ID, URI, NAME, LANGUAGE); only the list
            // they land in differs. Every rendition is kept —
            // a group can hold several languages (novahd has 7
            // audio tracks in one group), and keeping only the
            // first would silently drop the user's choice.
            let media_type = map.get("TYPE").copied().unwrap_or("");
            if media_type == "AUDIO" || media_type == "SUBTITLES" {
                if let (Some(group_id), Some(uri)) = (map.get("GROUP-ID"), map.get("URI")) {
                    let name = map.get("NAME").map(|s| s.to_string());
                    let language = map.get("LANGUAGE").map(|s| s.to_string());
                    if media_type == "AUDIO" {
                        master.audio.push(HlsAudioGroup {
                            group_id: group_id.to_string(),
                            uri: resolve(base_url, uri),
                            name,
                            language,
                        });
                    } else {
                        master.subtitles.push(HlsSubtitleGroup {
                            group_id: group_id.to_string(),
                            uri: resolve(base_url, uri),
                            name,
                            language,
                        });
                    }
                }
            }
        }
    }
    master
}

/// One `#EXT-X-KEY` declaration (round 12, item 5): the key's URI
/// (resolved against the playlist URL) and its IV, when the
/// playlist declares one. Only METHOD=AES-128 is ever stored —
/// any other method is refused at parse time (see
/// [`parse_hls_segments`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsKey {
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

/// A resolved decryption key for one downloaded segment: the
/// 16 key bytes (fetched once per distinct URI) and the IV that
/// segment actually decrypts with — the playlist's IV, or the
/// segment's sequence number as a 128-bit big-endian integer
/// when the playlist declares none (RFC 8216 §4.3.2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentKey {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

/// One HLS media segment: a URL plus the byte range of it the
/// playlist assigns (`#EXT-X-BYTERANGE`), if any. A range means
/// the segment is a SLICE of the URL's file, not the whole file:
/// totallyacdn (atlantic, 2026-11-23) serves a film as byte
/// ranges of ONE 12.68 GB MP4, and a fetch without the Range
/// header streams that whole file until the read times out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSegment {
    pub url: String,
    /// Inclusive byte range `(start, end)`, or None for a
    /// whole-file segment.
    pub range: Option<(u64, u64)>,
    /// The `#EXT-X-KEY` in force for this segment (round 12,
    /// item 5): None for a plain segment, Some for an AES-128
    /// segment. A later KEY line replaces it; METHOD=NONE clears
    /// it (a playlist may mix plain and encrypted segments).
    pub key: Option<HlsKey>,
    /// The segment's sequence number: `#EXT-X-MEDIA-SEQUENCE`
    /// plus its index in the playlist (0 when the playlist
    /// declares no sequence). It is the default IV when the KEY
    /// declares none.
    pub sequence: u64,
}

/// A parsed HLS media playlist: its segments plus the optional
/// init segment (`#EXT-X-MAP` — the fMP4 header a byterange
/// playlist's segments do not carry themselves; the player
/// fetches it once and prepends it, and so must we, or the
/// assembled file has no header and will not play).
#[derive(Debug, Clone, Default)]
pub struct HlsMediaPlaylist {
    pub init: Option<HlsSegment>,
    pub segments: Vec<HlsSegment>,
}

impl HlsMediaPlaylist {
    /// How many pieces the worker downloads: the media segments
    /// plus the init segment when present (same convention as
    /// DASH — the init counts as a piece).
    pub fn piece_count(&self) -> usize {
        self.segments.len() + usize::from(self.init.is_some())
    }

    /// Segments for the downloader: the init segment first (when
    /// present — same shape as DASH, where the init is piece 0),
    /// then the media segments in playlist order. Keys are left
    /// unresolved here (None): the listing path uses this for
    /// counts and byte-range totals only, and never downloads.
    /// Downloaders use [`HlsMediaPlaylist::resolve_keys`] so the
    /// segments they fetch carry their decryption keys.
    fn into_segments(self) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::with_capacity(self.piece_count());
        if let Some(init) = self.init {
            out.push(Segment {
                url: init.url,
                range: init.range,
                key: None,
            });
        }
        out.extend(self.segments.into_iter().map(|s| Segment {
            url: s.url,
            range: s.range,
            key: None,
        }));
        out
    }

    /// Segments for the downloader with their AES-128 keys
    /// resolved (round 12, item 5): each distinct key URI is
    /// fetched ONCE from the playlist's declared URI (the key is
    /// served openly — this is standard HLS, not a DRM bypass)
    /// and must be exactly 16 bytes. A segment whose KEY declares
    /// no IV decrypts with its sequence number as the IV
    /// (RFC 8216 §4.3.2.4). Plain segments keep `key: None`.
    /// The init segment is never encrypted (an `#EXT-X-MAP` has
    /// no KEY), so it stays keyless like [`into_segments`].
    fn resolve_keys(
        &self,
        client: &reqwest::blocking::Client,
        referer: &Option<String>,
    ) -> Result<Vec<Segment>, String> {
        let mut keys: HashMap<String, [u8; 16]> = HashMap::new();
        for seg in &self.segments {
            if let Some(k) = &seg.key {
                if !keys.contains_key(&k.uri) {
                    let bytes = fetch_key(client, &k.uri, referer)?;
                    keys.insert(k.uri.clone(), bytes);
                }
            }
        }
        let mut out: Vec<Segment> = Vec::with_capacity(self.piece_count());
        if let Some(init) = &self.init {
            out.push(Segment {
                url: init.url.clone(),
                range: init.range,
                key: None,
            });
        }
        for seg in &self.segments {
            let key = seg.key.as_ref().map(|k| SegmentKey {
                key: keys[&k.uri],
                iv: k.iv.unwrap_or_else(|| sequence_iv(seg.sequence)),
            });
            out.push(Segment {
                url: seg.url.clone(),
                range: seg.range,
                key,
            });
        }
        Ok(out)
    }
}

/// Fetch one HLS AES-128 key (round 12, item 5): a plain GET of
/// the playlist's declared URI, with the same client (and
/// referer) as the segments — a signed playlist's key needs the
/// same headers its segments do. The key must be exactly 16
/// bytes; anything else is an error, never a truncated key.
fn fetch_key(
    client: &reqwest::blocking::Client,
    uri: &str,
    referer: &Option<String>,
) -> Result<[u8; 16], String> {
    let req = browser_headers(client.get(uri), referer);
    let resp = req.send().map_err(|e| {
        format!(
            "The stream's decryption key could not be fetched ({})",
            reqwest_error(&e)
        )
    })?;
    if !resp.status().is_success() {
        return Err(format!(
            "The stream's decryption key could not be fetched (HTTP {}).",
            resp.status()
        ));
    }
    let bytes = resp.bytes().map_err(|e| reqwest_error(&e))?;
    let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| {
        "The stream's decryption key is not 16 bytes long, so the stream cannot be decrypted."
            .to_string()
    })?;
    Ok(key)
}

/// The default IV for an AES-128 segment with no declared IV:
/// its sequence number as a 128-bit big-endian integer
/// (RFC 8216 §4.3.2.4).
fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..16].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// Parse one `#EXT-X-KEY` IV value (`0x...`, up to 32 hex digits)
/// into 16 bytes, left-padded with zeros when shorter. None for
/// anything malformed — a bad IV is an error at parse time, never
/// a silently wrong decryption.
fn parse_key_iv(value: &str) -> Option<[u8; 16]> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Left-pad an odd-length value with one zero nibble, then
    // decode two hex digits per byte.
    let padded;
    let hex = if hex.len() % 2 == 1 {
        padded = format!("0{hex}");
        padded.as_str()
    } else {
        hex
    };
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(s, 16).ok()
        })
        .collect::<Option<Vec<u8>>>()?;
    let mut out = [0u8; 16];
    let start = 16 - bytes.len();
    out[start..].copy_from_slice(&bytes);
    Some(out)
}

/// Decrypt AES-128-CBC ciphertext (round 12, item 5) with the
/// `aes` crate's block cipher and a hand-rolled CBC layer (no
/// separate `cbc` crate needed). The ciphertext must be a whole
/// number of 16-byte blocks — HLS AES-128 segments are
/// PKCS#7-padded to a block boundary. Valid PKCS#7 padding is
/// stripped; a last byte that is not valid padding leaves the
/// plaintext untouched (some packagers emit unpadded TS, whose
/// length is a block multiple by construction, and stripping a
/// genuine trailing byte would corrupt the stream).
fn decrypt_aes128_cbc(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>, String> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    use aes::Aes128;
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err(
            "An encrypted segment is not a whole number of AES blocks, so it cannot be decrypted."
                .to_string(),
        );
    }
    let cipher = Aes128::new_from_slice(key).map_err(|e| e.to_string())?;
    let mut out = ciphertext.to_vec();
    let mut prev = *iv;
    for block in out.chunks_exact_mut(16) {
        let ciphertext_block: [u8; 16] = block.try_into().unwrap();
        let mut block_arr = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
        cipher.decrypt_block(&mut block_arr);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        prev = ciphertext_block;
    }
    if let Some(&pad) = out.last() {
        let pad = pad as usize;
        if pad >= 1
            && pad <= 16
            && out.len() >= pad
            && out[out.len() - pad..].iter().all(|&b| b as usize == pad)
        {
            out.truncate(out.len() - pad);
        }
    }
    Ok(out)
}

/// Refuse an HLS playlist whose `#EXT-X-KEY` method is not
/// standard AES-128 (round 12, item 5): METHOD=NONE (plain) and
/// METHOD=AES-128 pass; anything else — SAMPLE-AES, SAMPLE-AES-CTR
/// (FairPlay DRM) or an unknown method — is refused with the DRM
/// explanation. Used for master playlists and anywhere else the
/// text is checked before parsing, so a DRM stream is refused
/// before any segment is fetched.
fn check_hls_encryption_method(text: &str) -> Result<(), String> {
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
            match map.get("METHOD").map(|m| m.to_ascii_uppercase()) {
                Some(m) if m == "AES-128" || m == "NONE" => {}
                Some(m) => {
                    return Err(format!(
                        "This stream is encrypted with {m}, which FetchForge does not bypass. Only standard HLS AES-128 is supported."
                    ));
                }
                None => {
                    return Err(
                        "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                            .to_string(),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Parse one `#EXT-X-BYTERANGE` value (`n[@offset]`) into an
/// inclusive byte range. An omitted offset continues from
/// `default_offset` (the end of the previous sub-range, or 0 for
/// the first). A malformed value is an error: silently ignoring
/// it would fetch a whole multi-GB file instead of one slice.
fn parse_byterange(value: &str, default_offset: u64) -> Result<(u64, u64), String> {
    let (len_s, offset) = match value.split_once('@') {
        Some((n, o)) => (
            n,
            o.parse::<u64>()
                .map_err(|_| format!("Bad #EXT-X-BYTERANGE offset in '{value}'"))?,
        ),
        None => (value, default_offset),
    };
    let len: u64 = len_s
        .parse()
        .map_err(|_| format!("Bad #EXT-X-BYTERANGE length in '{value}'"))?;
    if len == 0 {
        return Err(format!(
            "Bad #EXT-X-BYTERANGE in '{value}': a zero-length range is not a segment"
        ));
    }
    let start = offset;
    let end = start
        .checked_add(len - 1)
        .ok_or_else(|| format!("Bad #EXT-X-BYTERANGE in '{value}': the range overflows"))?;
    Ok((start, end))
}

/// Parse an HLS media playlist into its segments (resolved
/// against `base_url`), with byte ranges (`#EXT-X-BYTERANGE`)
/// and the init segment (`#EXT-X-MAP`) when present. Standard
/// AES-128 encryption (round 12, item 5) is PARSED, not refused:
/// each segment records the `#EXT-X-KEY` in force for it (its
/// URI, resolved, and its IV when declared), so the downloader
/// can fetch the openly served key and decrypt. Any other KEY
/// method (SAMPLE-AES and friends — DRM) is refused, as is a
/// KEY line with no METHOD or (for AES-128) no URI. A playlist
/// with no segments is an error. A `#EXT-X-BYTERANGE` applies to
/// the NEXT segment URI; an omitted offset continues from the
/// previous segment's end (or 0 for the first ranged segment).
/// Only the first `#EXT-X-MAP` is kept: a mid-playlist map change
/// (ad insertion) cannot be represented in one assembled file.
pub fn parse_hls_segments(text: &str, base_url: &str) -> Result<HlsMediaPlaylist, String> {
    let mut playlist = HlsMediaPlaylist::default();
    let mut pending_range: Option<(u64, u64)> = None;
    // End of the last sub-range consumed (for omitted offsets);
    // only ranged segments move it — a whole-file segment in
    // between does not define a sub-range to continue from.
    let mut next_offset: u64 = 0;
    // The KEY in force for the next segment, and the sequence
    // number the next segment carries.
    let mut current_key: Option<HlsKey> = None;
    let mut media_sequence: u64 = 0;
    let mut index: u64 = 0;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
            match map.get("METHOD").map(|m| m.to_ascii_uppercase()) {
                Some(m) if m == "NONE" => current_key = None,
                Some(m) if m == "AES-128" => {
                    let uri = map.get("URI").ok_or_else(|| {
                        "This stream's EXT-X-KEY declares AES-128 but no key URI, so it cannot be decrypted."
                            .to_string()
                    })?;
                    let iv = match map.get("IV") {
                        Some(v) => Some(parse_key_iv(v).ok_or_else(|| {
                            format!("This stream's EXT-X-KEY has a malformed IV ('{v}').")
                        })?),
                        None => None,
                    };
                    current_key = Some(HlsKey {
                        uri: resolve(base_url, uri),
                        iv,
                    });
                }
                Some(m) => {
                    return Err(format!(
                        "This stream is encrypted with {m}, which FetchForge does not bypass. Only standard HLS AES-128 is supported."
                    ));
                }
                None => {
                    return Err(
                        "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                            .to_string(),
                    );
                }
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value
                .trim()
                .parse()
                .map_err(|_| format!("Bad #EXT-X-MEDIA-SEQUENCE value in '{value}'"))?;
            continue;
        }
        if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            if playlist.init.is_none() {
                let map: HashMap<&str, &str> = split_attrs(attrs).into_iter().collect();
                if let Some(uri) = map.get("URI") {
                    // A MAP byterange without an offset starts at
                    // 0: the map has no previous sub-range.
                    let range = match map.get("BYTERANGE") {
                        Some(v) => Some(parse_byterange(v, 0)?),
                        None => None,
                    };
                    playlist.init = Some(HlsSegment {
                        url: resolve(base_url, uri),
                        range,
                        key: None,
                        sequence: media_sequence,
                    });
                }
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_byterange(value.trim(), next_offset)?);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let url = resolve(base_url, line);
        let range = pending_range.take();
        if let Some((_, end)) = range {
            next_offset = end.saturating_add(1);
        }
        playlist.segments.push(HlsSegment {
            url,
            range,
            key: current_key.clone(),
            sequence: media_sequence + index,
        });
        index += 1;
    }
    if playlist.segments.is_empty() {
        return Err("Playlist contains no media segments".to_string());
    }
    Ok(playlist)
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

// ---------------------------------------------------------------------------
// Subtitle (WebVTT) merging — round 11 extension, feature 2
// ---------------------------------------------------------------------------

/// One subtitle segment of an HLS subtitle playlist: its URL and
/// its `#EXTINF` duration. The duration is the fallback cue
/// offset when a segment carries no `X-TIMESTAMP-MAP` (see
/// [`merge_vtt_segments`]).
#[derive(Debug, Clone)]
struct SubtitleSegment {
    url: String,
    duration_secs: f64,
}

/// Parse an HLS subtitle (WebVTT) media playlist into its
/// segments, resolved against `base_url`. Only `#EXTINF`
/// durations and segment URIs are read — byte ranges and init
/// segments do not occur in subtitle playlists, and an encrypted
/// subtitle playlist is refused like any other (spec §10).
fn parse_hls_subtitle_playlist(text: &str, base_url: &str) -> Result<Vec<SubtitleSegment>, String> {
    if text.contains("#EXT-X-KEY") {
        return Err(
            "This stream's subtitles are encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let mut out = Vec::new();
    let mut pending_duration: Option<f64> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest.split(',').next().and_then(|s| s.parse().ok());
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push(SubtitleSegment {
            url: resolve(base_url, line),
            // A segment without a preceding #EXTINF keeps the
            // previous offset (duration 0) rather than failing
            // the whole subtitle track.
            duration_secs: pending_duration.take().unwrap_or(0.0),
        });
    }
    if out.is_empty() {
        return Err("The subtitle playlist lists no segments.".to_string());
    }
    Ok(out)
}

/// Parse one WebVTT cue timestamp (`HH:MM:SS.mmm` or
/// `MM:SS.mmm`) into seconds. Returns None for anything else —
/// the merge keeps an unparsable timestamp line untouched rather
/// than dropping the cue.
fn parse_vtt_timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    let (main, frac) = match s.split_once('.') {
        Some((m, f)) => (m, f),
        None => (s, "0"),
    };
    let millis: f64 = if frac.is_empty() {
        0.0
    } else {
        // "5" means 500 ms, "05" means 50 ms: pad to 3 digits.
        let padded = format!("{frac:0<3}");
        padded.parse::<f64>().ok()? / 1000.0
    };
    let parts: Vec<&str> = main.split(':').collect();
    let secs = match parts.as_slice() {
        [h, m, s] => {
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + s.parse::<f64>().ok()?
        }
        [m, s] => m.parse::<f64>().ok()? * 60.0 + s.parse::<f64>().ok()?,
        _ => return None,
    };
    Some(secs + millis)
}

/// Format seconds as a WebVTT timestamp (`HH:MM:SS.mmm`).
/// Negative values clamp to zero — a cue can never start before
/// the file does.
fn format_vtt_timestamp(secs: f64) -> String {
    let total_millis = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_millis % 1000;
    let total_secs = total_millis / 1000;
    let s = total_secs % 60;
    let total_mins = total_secs / 60;
    let m = total_mins % 60;
    let h = total_mins / 60;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Parse an `X-TIMESTAMP-MAP` header line
/// (`X-TIMESTAMP-MAP=MPEGTS:123456,LOCAL:00:00:00.000`) into its
/// MPEG-TS clock value (90 kHz ticks) and its local cue time in
/// seconds. Returns None when the line is absent or malformed.
fn parse_timestamp_map(line: &str) -> Option<(u64, f64)> {
    let rest = line.strip_prefix("X-TIMESTAMP-MAP=")?;
    let mut mpegts = None;
    let mut local = None;
    for part in rest.split(',') {
        if let Some(v) = part.strip_prefix("MPEGTS:") {
            mpegts = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("LOCAL:") {
            local = parse_vtt_timestamp(v);
        }
    }
    match (mpegts, local) {
        (Some(m), Some(l)) => Some((m, l)),
        _ => None,
    }
}

/// Merge one subtitle track's WebVTT segments into a single
/// WebVTT document. Each segment is `(its #EXTINF duration, its
/// text)`.
fn merge_vtt_segments(segments: &[(f64, String)]) -> String {
    merge_vtt_segments_impl(segments, false)
}

/// [`merge_vtt_segments`] for DASH, where each segment is `(its
/// start time in seconds, its text)`: the offset is the segment's
/// start time itself (DASH VTT cues are relative to their
/// segment's start), not the cumulative durations.
fn merge_vtt_segments_with_start_times(segments: &[(f64, String)]) -> String {
    merge_vtt_segments_impl(segments, true)
}

fn merge_vtt_segments_impl(segments: &[(f64, String)], starts: bool) -> String {
    // Cue timestamps in a segment are LOCAL to that segment, so
    // they must be shifted onto the track's timeline before the
    // segments are concatenated — otherwise every segment's cues
    // would pile up at 00:00. Two offset sources, in order:
    //
    // 1. `X-TIMESTAMP-MAP` (the accurate one, present on most
    //    real-world HLS subtitle segments): the segment's MPEG-TS
    //    clock value anchors it absolutely —
    //    `offset = (mpegts - first_mpegts) / 90000 - local`.
    //    The FIRST segment's MPEGTS is the baseline, so a track
    //    whose clock starts at a non-zero value still begins at 0.
    // 2. The segment's start time (DASH) or the cumulative
    //    `#EXTINF` durations of the preceding segments (HLS),
    //    used when a segment carries no map.
    //
    // The `WEBVTT` header and any `X-TIMESTAMP-MAP` / `NOTE`
    // lines are dropped from the segments (the merged document
    // carries a single header); cue identifiers, cue text and
    // cue settings (e.g. `line:90%`) pass through unchanged.
    let mut out = String::from("WEBVTT\n\n");
    let mut cumulative = 0.0f64;
    let mut first_mpegts: Option<u64> = None;
    for (value, text) in segments {
        let mut map: Option<(u64, f64)> = None;
        let mut body: Vec<&str> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("WEBVTT") {
                continue;
            }
            if let Some(parsed) = parse_timestamp_map(trimmed) {
                map = Some(parsed);
                continue;
            }
            if trimmed.starts_with("NOTE") {
                continue;
            }
            body.push(line);
        }
        // Drop leading blank lines (the header separator of the
        // segment itself) so the merged cues stay separated by
        // exactly one blank line each.
        let mut body_iter = body.into_iter().peekable();
        while body_iter.peek().is_some_and(|l| l.trim().is_empty()) {
            body_iter.next();
        }
        let offset = match map {
            Some((mpegts, local)) => {
                let first = *first_mpegts.get_or_insert(mpegts);
                mpegts.saturating_sub(first) as f64 / 90_000.0 - local
            }
            // DASH: the segment's start time. HLS: the cumulative
            // #EXTINF durations of the preceding segments.
            None => {
                if starts {
                    *value
                } else {
                    cumulative
                }
            }
        };
        for line in body_iter {
            if let Some((start, rest)) = line.split_once("-->") {
                let start_s = start.trim();
                let rest = rest.trim_start();
                // The end timestamp is the first token; anything
                // after it is cue settings, kept verbatim.
                let (end_s, settings) = match rest.split_once(char::is_whitespace) {
                    Some((e, s)) => (e, s.trim_start()),
                    None => (rest, ""),
                };
                if let (Some(start_t), Some(end_t)) =
                    (parse_vtt_timestamp(start_s), parse_vtt_timestamp(end_s))
                {
                    let shifted_start = format_vtt_timestamp(start_t + offset);
                    let shifted_end = format_vtt_timestamp(end_t + offset);
                    if settings.is_empty() {
                        out.push_str(&format!("{shifted_start} --> {shifted_end}\n"));
                    } else {
                        out.push_str(&format!("{shifted_start} --> {shifted_end} {settings}\n"));
                    }
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        cumulative += value;
    }
    out
}

/// Download one HLS subtitle track and merge its segments into a
/// single WebVTT file at `dest` (round 11 extension, feature
/// 2). Segments are fetched sequentially — subtitle segments
/// are kilobytes, so parallelism would buy nothing — with the
/// same client (and proxy) as the video. Returns the path of
/// the merged file. A subtitle failure is the CALLER's decision
/// (the video workers warn and continue without subtitles);
/// this function reports it as an error.
fn download_hls_subtitles(
    client: &reqwest::blocking::Client,
    playlist_url: &str,
    referer: &Option<String>,
    dest: &Path,
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let text = fetch_text(client, playlist_url, referer)?;
    let segments = parse_hls_subtitle_playlist(&text, playlist_url)?;
    let mut merged: Vec<(f64, String)> = Vec::with_capacity(segments.len());
    for seg in &segments {
        if cancelled.load(Ordering::Relaxed) {
            return Err("__CANCELLED__".to_string());
        }
        if paused.load(Ordering::Relaxed) {
            return Err("__PAUSED__".to_string());
        }
        let seg_text = fetch_text(client, &seg.url, referer)
            .map_err(|e| format!("A subtitle segment could not be downloaded ({e})."))?;
        merged.push((seg.duration_secs, seg_text));
    }
    let doc = merge_vtt_segments(&merged);
    std::fs::write(dest, doc).map_err(|e| e.to_string())?;
    Ok(dest.to_path_buf())
}

/// Download one DASH WebVTT subtitle track and merge its
/// segments into a single WebVTT file at `dest` (round 11
/// extension, feature 2). Segments are fetched sequentially
/// (subtitle segments are kilobytes) and offset by their start
/// times — DASH VTT cues are relative to their segment's start.
/// The init segment of a text representation, when one exists,
/// is skipped: it carries no cues. A subtitle failure is the
/// CALLER's decision (the video workers warn and continue
/// without subtitles); this function reports it as an error.
fn download_dash_subtitles(
    client: &reqwest::blocking::Client,
    rep: &DashRepresentation,
    manifest_url: &str,
    duration_secs: Option<f64>,
    referer: &Option<String>,
    dest: &Path,
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let (_init, timed) = dash_segments_with_times(rep, manifest_url, duration_secs)?;
    let mut merged: Vec<(f64, String)> = Vec::with_capacity(timed.len());
    for (seg, start) in &timed {
        if cancelled.load(Ordering::Relaxed) {
            return Err("__CANCELLED__".to_string());
        }
        if paused.load(Ordering::Relaxed) {
            return Err("__PAUSED__".to_string());
        }
        let seg_text = fetch_text(client, &seg.url, referer)
            .map_err(|e| format!("A subtitle segment could not be downloaded ({e})."))?;
        merged.push((*start, seg_text));
    }
    let doc = merge_vtt_segments_with_start_times(&merged);
    std::fs::write(dest, doc).map_err(|e| e.to_string())?;
    Ok(dest.to_path_buf())
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

/// The identifier the popup offers (and the engine matches) for
/// an HLS audio or subtitle rendition: its `LANGUAGE` when the
/// master declares one, else its NAME, else its GROUP-ID. The
/// three are tried in that order by the matcher too, so a
/// rendition with no LANGUAGE tag is still selectable by name.
fn rendition_language(language: &Option<String>, name: &Option<String>, group_id: &str) -> String {
    if let Some(l) = language {
        if !l.is_empty() {
            return l.clone();
        }
    }
    if let Some(n) = name {
        if !n.is_empty() {
            return n.clone();
        }
    }
    group_id.to_string()
}

/// The distinct audio languages of a parsed master, in playlist
/// order (round 11 extension, feature 4).
fn master_audio_languages(master: &HlsMaster) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in &master.audio {
        let label = rendition_language(&a.language, &a.name, &a.group_id);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// The distinct subtitle languages of a parsed master, in
/// playlist order (round 11 extension, feature 2).
fn master_subtitle_languages(master: &HlsMaster) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in &master.subtitles {
        let label = rendition_language(&s.language, &s.name, &s.group_id);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// Pick the audio rendition of `group_id` matching `language`
/// (round 11 extension, feature 4): the rendition whose
/// LANGUAGE, NAME or GROUP-ID equals the choice (case-insensitive
/// on LANGUAGE). None means the group's first rendition — the
/// pre-feature-4 behaviour. When a choice matches nothing (the
/// manifest changed between listing and download) the first
/// rendition is returned instead: a movie in the wrong language
/// beats a failed download, and the popup only ever offers
/// languages the listing found.
fn pick_audio_rendition<'a>(
    audio: &'a [HlsAudioGroup],
    group_id: &str,
    language: Option<&str>,
) -> Option<&'a HlsAudioGroup> {
    let group: Vec<&HlsAudioGroup> = audio.iter().filter(|a| a.group_id == group_id).collect();
    if group.is_empty() {
        return None;
    }
    if let Some(want) = language {
        for a in &group {
            let matches = a
                .language
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(want))
                || a.name.as_deref() == Some(want)
                || a.group_id == want;
            if matches {
                return Some(a);
            }
        }
    }
    Some(group[0])
}

/// Pick the subtitle rendition of `group_id` matching `language`
/// (round 11 extension, feature 2): same matching as
/// [`pick_audio_rendition`], but a non-match returns None — a
/// subtitle in the wrong language is worse than none, and the
/// download continues without subtitles. `group_id` is None when
/// the variant names no SUBTITLES group; then any subtitle
/// rendition matches (the common single-group case).
fn pick_subtitle_rendition<'a>(
    subtitles: &'a [HlsSubtitleGroup],
    group_id: Option<&str>,
    language: Option<&str>,
) -> Option<&'a HlsSubtitleGroup> {
    let want = language?;
    subtitles.iter().find(|s| {
        let group_ok = group_id.map_or(true, |g| s.group_id == g);
        group_ok
            && (s
                .language
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(want))
                || s.name.as_deref() == Some(want)
                || s.group_id == want)
    })
}

/// List the qualities of an HLS manifest: one format per variant
/// (or one format for a plain media playlist). The duration is
/// read from the default variant's playlist when it can be
/// fetched; a failure there leaves the duration unknown rather
/// than failing the whole listing.
///
/// Sizes (round 11, item 10 — the estimate ladder): each format
/// carries the best size the manifest supports, with its kind —
/// the exact byte-range sum when the (default) variant's playlist
/// declares every segment's range, else `AVERAGE-BANDWIDTH ×
/// duration` (≈), else peak `BANDWIDTH × duration` (~). No
/// size at all when the manifest declares neither bandwidth nor
/// ranges (the download's measured estimate takes over then).
pub fn list_hls_formats(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    list_hls_formats_with_proxy(manifest_url, referer, captured, None)
}

/// [`list_hls_formats`] through the user-configured proxy
/// (round 11 extension, feature 10).
pub fn list_hls_formats_with_proxy(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
    proxy: Option<&str>,
) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, proxy)?;
    let text = manifest_text(&client, &manifest_url, referer, captured)?;
    check_hls_encryption_method(&text)?;
    let master = parse_hls_master(&text, &manifest_base_url(&manifest_url));
    let title = title_from_manifest_url(&manifest_url);
    if master.variants.is_empty() {
        // Media playlist: validate it has segments, then one format.
        // A byte-ranged playlist (Atlantic-style) yields the exact
        // total from its ranges — the only size a media playlist
        // can offer, since it declares no bandwidth.
        let playlist = parse_hls_segments(&text, &manifest_base_url(&manifest_url))?;
        let (filesize, size_kind) = match exact_total_bytes(&playlist.clone().into_segments()) {
            Some(total) => (Some(total), Some(SizeKind::Exact)),
            None => (None, None),
        };
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
                filesize,
                size_kind,
                needs_merge: false,
            }],
            segment_count: Some(playlist.piece_count() as u64),
            // A media playlist carries no renditions at all.
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
        });
    }
    // Duration + segment count from the default variant's
    // playlist (best effort): one fetch serves both, so the plan
    // knows the piece count before the download starts — and the
    // formats below need the duration for the size ladder, so
    // this fetch happens BEFORE the formats are built.
    let default_uri = pick_default_variant(&master.variants).map(|v| v.uri.clone());
    let default_text = default_uri
        .as_deref()
        .and_then(|uri| manifest_text(&client, uri, referer, captured).ok());
    let duration_secs = default_text.as_deref().and_then(hls_duration_secs);
    let default_playlist = match (&default_uri, &default_text) {
        (Some(uri), Some(text)) => parse_hls_segments(text, &manifest_base_url(uri)).ok(),
        _ => None,
    };
    let segment_count = default_playlist.as_ref().map(|p| p.piece_count() as u64);
    // The default variant's exact total, when its playlist is
    // byte-ranged (Atlantic-style): the first ladder rung, and
    // the only one that is exact. Other variants keep the
    // bandwidth estimate — fetching every variant's playlist to
    // total its ranges would multiply manifest requests for a
    // listing the user may not even pick from.
    let default_exact = default_playlist
        .as_ref()
        .and_then(|p| exact_total_bytes(&p.clone().into_segments()));
    let formats = master
        .variants
        .iter()
        .map(|v| {
            let separate_audio = v.audio_group.is_some();
            // The estimate ladder (round 11, item 10): exact
            // byte-range sum for the default variant, else
            // AVERAGE-BANDWIDTH × duration, else peak BANDWIDTH ×
            // duration. Separate audio adds nothing to a bitrate
            // estimate: BANDWIDTH covers the whole combination
            // including separate audio (RFC 8216 §4.3.4.2).
            let (filesize, size_kind) = if Some(&v.uri) == default_uri.as_ref() {
                if let Some(total) = default_exact {
                    (Some(total), Some(SizeKind::Exact))
                } else {
                    variant_size_estimate(v, duration_secs)
                }
            } else {
                variant_size_estimate(v, duration_secs)
            };
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
                filesize,
                size_kind,
                needs_merge: separate_audio,
            }
        })
        .collect();
    Ok(StreamInfo {
        kind: StreamKind::Hls,
        title,
        duration_secs,
        manifest_url,
        formats,
        segment_count,
        audio_languages: master_audio_languages(&master),
        subtitle_languages: master_subtitle_languages(&master),
    })
}

/// Bitrate-based size estimate for one HLS variant (round 11,
/// item 10): `AVERAGE-BANDWIDTH × duration` when the master
/// declares the average (usually within ~1%), else peak
/// `BANDWIDTH × duration` (a rough upper bound, often 2–4× high
/// for VBR content). None when the duration is unknown or the
/// variant declares no bandwidth at all.
fn variant_size_estimate(
    v: &HlsVariant,
    duration_secs: Option<u64>,
) -> (Option<u64>, Option<SizeKind>) {
    let Some(d) = duration_secs else {
        return (None, None);
    };
    if d == 0 {
        return (None, None);
    }
    if let Some(avg) = v.average_bandwidth {
        let bytes = (avg as u128 * d as u128) / 8;
        return (
            Some(u64::try_from(bytes).unwrap_or(u64::MAX)),
            Some(SizeKind::Average),
        );
    }
    if let Some(peak) = v.bandwidth {
        let bytes = (peak as u128 * d as u128) / 8;
        return (
            Some(u64::try_from(bytes).unwrap_or(u64::MAX)),
            Some(SizeKind::Peak),
        );
    }
    (None, None)
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
    /// The resolved AES-128 key for this segment (round 12,
    /// item 5): None for a plain segment (and for every DASH
    /// segment — DASH encryption is still refused outright).
    /// The fetcher decrypts with it before writing the piece,
    /// so resumed pieces on disk are always plaintext.
    key: Option<SegmentKey>,
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
/// `segments_base` does the same for the segment counts (round 10,
/// items 2/6): without it the audio phase restarted its counter at
/// 0 and the Plex record went 645/820 back to 353/821 while bytes
/// kept growing. Counts are cumulative across phases, like bytes.
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
pub fn estimate_total_bytes(downloaded: u64, done: u64, total: u64) -> Option<u64> {
    if done == 0 || total == 0 {
        return None;
    }
    // u128: downloaded (≤ ~10 GB) × total (≤ ~100k segments)
    // fits u64 in practice, but saturate rather than overflow.
    let est = (downloaded as u128 * total as u128) / done as u128;
    Some(u64::try_from(est).unwrap_or(u64::MAX))
}

/// Exact total size of a download whose EVERY segment carries
/// a byte range (HLS `#EXT-X-BYTERANGE`, DASH SegmentList
/// `@mediaRange`): the sum of the range lengths. None when any
/// segment is a whole-file fetch — then the measured estimate
/// (see [`estimate_total_bytes`]) is all there is. With ranges
/// the UI can show the true size from the first report instead
/// of "~" extrapolated from a one-segment average.
fn exact_total_bytes(segments: &[Segment]) -> Option<u64> {
    let mut total: u128 = 0;
    for seg in segments {
        let (s, e) = seg.range?;
        total += (e as u128).saturating_sub(s as u128) + 1;
    }
    Some(u64::try_from(total).unwrap_or(u64::MAX))
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
/// assembling a corrupt file. A ranged first segment also pins
/// its range: the same URL re-sliced by the CDN (same sign, new
/// byte ranges) must not resume against the old pieces.
fn read_seg_meta(seg_dir: &Path, segments: &[Segment]) -> bool {
    let Ok(saved) = std::fs::read_to_string(seg_dir.join("meta")) else {
        return false;
    };
    let mut count: Option<usize> = None;
    let mut first: Option<String> = None;
    let mut first_range: Option<(u64, u64)> = None;
    for line in saved.lines() {
        if let Some(v) = line.strip_prefix("count=") {
            count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("first=") {
            first = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("first_range=") {
            if let Some((s, e)) = v.split_once('-') {
                if let (Ok(s), Ok(e)) = (s.parse(), e.parse()) {
                    first_range = Some((s, e));
                }
            }
        }
    }
    match (count, first) {
        (Some(n), Some(f)) => {
            n == segments.len()
                && segments.first().is_some_and(|s| s.url == f)
                && first_range == segments.first().and_then(|s| s.range)
        }
        _ => false,
    }
}

fn write_seg_meta(seg_dir: &Path, segments: &[Segment]) {
    let first = segments.first().map(|s| s.url.as_str()).unwrap_or("");
    let mut meta = format!("count={}\nfirst={}\n", segments.len(), first);
    if let Some((s, e)) = segments.first().and_then(|s| s.range) {
        meta.push_str(&format!("first_range={s}-{e}\n"));
    }
    let _ = std::fs::write(seg_dir.join("meta"), meta);
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
    segments_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
) -> Result<u64, String> {
    // No speed limit: a no-op limiter (the production paths call
    // download_segments_with_bandwidth instead).
    let no_limit = SharedLimiter::new(None);
    download_segments_with_bandwidth(
        client,
        segments,
        part_path,
        referer,
        progress_base,
        segments_base,
        progress,
        paused,
        cancelled,
        stall_timeout,
        &no_limit,
    )
}

/// [`download_segments`] with the shared bandwidth limiter
/// (round 11 extension, feature 3): every segment's bytes are
/// accounted against the global speed limit, so the parallel
/// workers together stay under it instead of each taking a
/// quarter (which the old per-worker split could never enforce
/// across phases).
#[allow(clippy::too_many_arguments)]
fn download_segments_with_bandwidth(
    client: &reqwest::blocking::Client,
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    segments_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
    bandwidth: &SharedLimiter,
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
            segments_base,
            progress,
            paused,
            cancelled,
            stall_timeout,
            bandwidth,
        );
    }
    let limiter = RateLimiter::new();
    match download_segments_parallel(
        client,
        segments,
        part_path,
        referer,
        progress_base,
        segments_base,
        progress,
        paused,
        cancelled,
        stall_timeout,
        HLS_SEGMENT_WORKERS,
        &limiter,
        bandwidth,
    ) {
        Ok(n) => Ok(n),
        Err(e) if should_fallback_sequential(&e, paused, cancelled) => {
            // Round 7: a parallel run that fails a segment on a
            // TIMEOUT gets one sequential retry (a CDN throttling
            // concurrent connections from one client may still
            // serve a single connection). The retry reuses the
            // same segment dir, so pieces the parallel pass
            // already landed are not re-fetched, and it is
            // bounded by the same fail-fast and stall rules as
            // any other pass. If the retry also fails, the
            // ORIGINAL error stands: the sequential pass was a
            // rescue attempt, and it must never rewrite the
            // failure's diagnosis (e.g. a server that died
            // between passes would otherwise turn a timeout into
            // a "connection refused").
            // NOTE (round 8): the Atlantic case this fallback was
            // built for turned out NOT to be concurrency at all —
            // totallyacdn serves byteranged HLS and the engine
            // was fetching without Range (see parse_hls_segments).
            // The fallback stays: it is the right rescue for a
            // genuine concurrency throttle.
            match download_segments_parallel(
                client,
                segments,
                part_path,
                referer,
                progress_base,
                segments_base,
                progress,
                paused,
                cancelled,
                stall_timeout,
                1,
                &limiter,
                bandwidth,
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
                segments_base,
                progress,
                paused,
                cancelled,
                stall_timeout,
                1,
                &limiter,
                bandwidth,
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
    segments_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
    workers: usize,
    limiter: &RateLimiter,
    bandwidth: &SharedLimiter,
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
    // Exact total when every segment carries a byte range (HLS
    // byteranges, DASH SegmentList); the measured extrapolation
    // otherwise (see the report closure below).
    let known_total = exact_total_bytes(segments);
    let report = |progress: &(dyn Fn(Progress) + Send + Sync)| {
        let dl = downloaded.load(Ordering::Relaxed);
        let dn = done.load(Ordering::Relaxed) as u64;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        progress(Progress {
            downloaded: progress_base + dl,
            total: None,
            bytes_per_sec: dl as f64 / elapsed,
            segments_done: Some(segments_base + dn),
            segments_total: Some(segments_base + total),
            // The estimate is COMBINED: completed phases contribute
            // their exact bytes (progress_base) and the current
            // phase its measured estimate. A per-phase estimate
            // here made the UI show 1324% on a nime DASH download
            // (2.2 GB cumulative bytes against a ~166 MB
            // audio-phase estimate, 2026-11-23).
            estimated_total: known_total
                .map(|e| progress_base.saturating_add(e))
                .or_else(|| {
                    estimate_total_bytes(dl, dn, total).map(|e| progress_base.saturating_add(e))
                }),
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
                            // AES-128 (round 12, item 5): decrypt
                            // the fetched bytes with the segment's
                            // resolved key before writing the piece
                            // — resumed pieces on disk are always
                            // plaintext. A decryption failure is
                            // fatal, not retryable: re-fetching the
                            // same ciphertext with the same key
                            // fails the same way.
                            let b = match &seg.key {
                                Some(k) => match decrypt_aes128_cbc(&b, &k.key, &k.iv) {
                                    Ok(d) => d,
                                    Err(e) => {
                                        let mut slot = first_error.lock().unwrap();
                                        if slot.is_none() {
                                            *slot = Some(format!(
                                                "Segment {} of {} could not be decrypted. ({e})",
                                                i + 1,
                                                segments.len()
                                            ));
                                        }
                                        stop.store(true, Ordering::Relaxed);
                                        return;
                                    }
                                },
                                None => b,
                            };
                            limiter.note_success();
                            // Speed limit (feature 3): account
                            // this segment's bytes against the
                            // shared window — the sleep keeps the
                            // parallel workers under the limit
                            // together.
                            bandwidth.account(b.len());
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
    segments_base: u64,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
    bandwidth: &SharedLimiter,
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
    // Exact total when every segment carries a byte range (see
    // the parallel path); the measured extrapolation otherwise.
    let known_total = exact_total_bytes(segments);
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
                    // AES-128 (round 12, item 5): decrypt before
                    // appending — see the parallel path. A
                    // decryption failure aborts the run outright.
                    let b = match &seg.key {
                        Some(k) => match decrypt_aes128_cbc(&b, &k.key, &k.iv) {
                            Ok(d) => d,
                            Err(e) => {
                                return Err(format!(
                                    "Segment {} of {} could not be decrypted. ({e})",
                                    i + 1,
                                    segments.len()
                                ));
                            }
                        },
                        None => b,
                    };
                    // Speed limit (feature 3): same accounting
                    // as the parallel path.
                    bandwidth.account(b.len());
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
            segments_done: Some(segments_base + done_now),
            segments_total: Some(segments_base + total),
            // Combined estimate, as in the parallel path: the
            // completed phases' exact bytes plus this phase's
            // measured estimate, so the percent never exceeds 100%
            // on a multi-phase download.
            estimated_total: known_total
                .map(|e| progress_base.saturating_add(e))
                .or_else(|| {
                    estimate_total_bytes(downloaded, done_now, total)
                        .map(|e| progress_base.saturating_add(e))
                }),
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
    check_hls_encryption_method(&text)?;
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
///
/// No language / subtitle / proxy / speed-limit options: see
/// [`download_hls_variant_opts`] for those (round 11 extension).
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
    download_hls_variant_opts(
        manifest_url,
        variant_uri,
        destination_dir,
        filename,
        referer,
        ffmpeg,
        progress,
        paused,
        cancelled,
        captured,
        &StreamDownloadOptions::default(),
    )
}

/// [`download_hls_variant`] with the round 11 extension options
/// ([`StreamDownloadOptions`]): the audio language (feature 4),
/// the subtitle language to mux in (feature 2), the proxy
/// (feature 10), the speed limit (feature 3) and the title
/// metadata (feature 12).
///
/// A subtitle merge re-containerises a muxed `.ts` variant to
/// `.mp4` (mov_text subtitles are not valid in MPEG-TS), so the
/// returned path may differ from `filename` in extension — the
/// caller reconciles the record against the returned path.
/// A subtitle track that fails to download does NOT fail the
/// video: the download continues without subtitles and the
/// failure is logged to stderr (a 404 on one subtitle segment
/// must not cost the user a two-hour film).
#[allow(clippy::too_many_arguments)]
pub fn download_hls_variant_opts(
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
    opts: &StreamDownloadOptions,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let variant_uri = validate_url(variant_uri)?;
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, opts.proxy_url.as_deref())?;
    let bandwidth = SharedLimiter::new(opts.bandwidth_kbps);
    // The master text tells us whether this variant's audio is a
    // separate track (skip the fetch when the "variant" is the
    // manifest itself — a plain media playlist).
    let (audio_uri, subtitle_uri) = if variant_uri != manifest_url {
        let master_text = manifest_text(&client, &manifest_url, referer, captured)?;
        check_hls_encryption_method(&master_text)?;
        let master = parse_hls_master(&master_text, &manifest_base_url(&manifest_url));
        let variant = master.variants.iter().find(|v| v.uri == variant_uri);
        let audio_uri = variant
            .and_then(|v| v.audio_group.as_deref())
            .and_then(|group| {
                pick_audio_rendition(&master.audio, group, opts.audio_language.as_deref())
                    .map(|a| a.uri.clone())
            });
        let subtitle_uri = variant
            .and_then(|v| v.subtitles_group.as_deref())
            .or_else(|| {
                // A variant with no SUBTITLES attribute still
                // gets its subtitles when the master has only one
                // subtitle group (the common case).
                if master.subtitles.len() == 1 {
                    Some(master.subtitles[0].group_id.as_str())
                } else {
                    None
                }
            })
            .and_then(|group| {
                pick_subtitle_rendition(
                    &master.subtitles,
                    Some(group),
                    opts.subtitle_language.as_deref(),
                )
                .map(|s| s.uri.clone())
            })
            .or_else(|| {
                // No group at all (variant names none, master has
                // several): match by language across all groups.
                pick_subtitle_rendition(&master.subtitles, None, opts.subtitle_language.as_deref())
                    .map(|s| s.uri.clone())
            });
        (audio_uri, subtitle_uri)
    } else {
        (None, None)
    };
    if (audio_uri.is_some() || subtitle_uri.is_some()) && ffmpeg.is_none() {
        return Err(
            "This quality keeps its audio or subtitles on a separate track, which needs ffmpeg to merge. \
             Install ffmpeg (or pick a combined quality) and try again."
                .to_string(),
        );
    }
    let variant_text = manifest_text(&client, &variant_uri, referer, captured)?;
    // Segments resolve against the variant playlist's own URL —
    // unwrapped when the variant came through a proxy worker, so
    // they are fetched from the real CDN, not the proxy origin.
    // Byte ranges ride along (see parse_hls_segments): a
    // byterange playlist's segments are slices of one file, and
    // fetching them without Range would stream the whole file.
    let video_segments = parse_hls_segments(&variant_text, &manifest_base_url(&variant_uri))?
        .resolve_keys(&client, referer)?;
    let audio_segments = match &audio_uri {
        Some(uri) => {
            let audio_text = fetch_text(&client, uri, referer)?;
            Some(parse_hls_segments(&audio_text, uri)?.resolve_keys(&client, referer)?)
        }
        None => None,
    };
    let clean = crate::filename::sanitize_filename(filename);
    std::fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let video_bytes = download_segments_with_bandwidth(
        &client,
        &video_segments,
        &video_part,
        referer,
        0,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
    )?;
    // Subtitles download BEFORE the merge decision: a subtitle
    // failure must not cost the video, so it is a warning, not an
    // error — but the merge decision below needs to know whether
    // a subtitle file actually exists.
    let subtitle_file: Option<PathBuf> = match &subtitle_uri {
        Some(uri) => {
            let subs_path = final_path.with_file_name(format!(
                "{}.subs.vtt",
                final_path.file_name().unwrap().to_string_lossy()
            ));
            match download_hls_subtitles(&client, uri, referer, &subs_path, paused, cancelled) {
                Ok(path) => Some(path),
                Err(e) if e == "__PAUSED__" || e == "__CANCELLED__" => {
                    let _ = std::fs::remove_file(&subs_path);
                    return Err(e);
                }
                Err(e) => {
                    eprintln!("FetchForge: subtitles could not be downloaded ({e}); continuing without them.");
                    let _ = std::fs::remove_file(&subs_path);
                    None
                }
            }
        }
        None => None,
    };
    let Some(audio_segments) = audio_segments else {
        if let Some(subs) = &subtitle_file {
            // Muxed variant + subtitles: the merge re-containers
            // the .ts to .mp4 (mov_text is not valid in MPEG-TS).
            // The returned path's extension changes — the caller
            // reconciles the record against it.
            let ffmpeg = ffmpeg.expect("subtitle merge checked ffmpeg above");
            let mp4_path = if final_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ts"))
            {
                let mp4_name = format!("{}.mp4", final_path.file_stem().unwrap().to_string_lossy());
                crate::filename::unique_path(destination_dir, &mp4_name)
            } else {
                final_path.clone()
            };
            let lang = opts.subtitle_language.clone().unwrap_or_default();
            let merged = crate::ffmpeg::merge_tracks(
                ffmpeg,
                &video_part,
                None,
                Some((subs.as_path(), lang.as_str())),
                opts.title.as_deref(),
                &mp4_path,
            );
            let _ = std::fs::remove_file(&video_part);
            let _ = std::fs::remove_file(subs);
            merged?;
            remove_seg_dir(&video_part);
            return Ok(mp4_path);
        }
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
    download_segments_with_bandwidth(
        &client,
        &audio_segments,
        &audio_part,
        referer,
        video_bytes,
        video_segments.len() as u64,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
    )?;
    let ffmpeg = ffmpeg.expect("audio merge checked ffmpeg above");
    let subtitle_arg = subtitle_file.as_ref().map(|p| {
        (
            p.as_path(),
            opts.subtitle_language.clone().unwrap_or_default(),
        )
    });
    let merged = crate::ffmpeg::merge_tracks(
        ffmpeg,
        &video_part,
        Some(&audio_part),
        subtitle_arg.as_ref().map(|(p, lang)| (*p, lang.as_str())),
        opts.title.as_deref(),
        &final_path,
    );
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    if let Some(subs) = &subtitle_file {
        let _ = std::fs::remove_file(subs);
    }
    merged?;
    remove_seg_dir(&video_part);
    remove_seg_dir(&audio_part);
    Ok(final_path)
}

/// Download only the audio track of an HLS variant (the popup's
/// "Audio only" choice, round 8). When the variant's audio lives
/// on a separate playlist (master audio group), download THAT
/// playlist alone — never the video. When the audio is muxed
/// into the variant, download the full variant and let the caller
/// extract the audio (see [`crate::ffmpeg::extract_audio`]).
/// Returns the downloaded file (an audio-only `.ts`, or the full
/// video when muxed); the caller extracts/remuxes it to `.m4a`.
///
/// No language / proxy / speed-limit options: see
/// [`download_hls_audio_only_opts`] for those (round 11
/// extension).
#[allow(clippy::too_many_arguments)]
pub fn download_hls_audio_only(
    manifest_url: &str,
    variant_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
) -> Result<PathBuf, String> {
    download_hls_audio_only_opts(
        manifest_url,
        variant_uri,
        destination_dir,
        filename,
        referer,
        progress,
        paused,
        cancelled,
        captured,
        &StreamDownloadOptions::default(),
    )
}

/// [`download_hls_audio_only`] with the round 11 extension
/// options: the audio language picks WHICH separate audio
/// rendition to download (feature 4 — the pre-extension code
/// always took the first rendition of the group), the proxy
/// (feature 10) and the speed limit (feature 3) apply to the
/// segment fetches.
#[allow(clippy::too_many_arguments)]
pub fn download_hls_audio_only_opts(
    manifest_url: &str,
    variant_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    captured: Option<&CapturedManifest>,
    opts: &StreamDownloadOptions,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let variant_uri = validate_url(variant_uri)?;
    // Resolve the variant's audio group from the master (skip the
    // fetch when the "variant" is the manifest itself — a plain
    // media playlist, whose audio is muxed by definition).
    let audio_uri = if variant_uri != manifest_url {
        let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, opts.proxy_url.as_deref())?;
        let master_text = manifest_text(&client, &manifest_url, referer, captured)?;
        check_hls_encryption_method(&master_text)?;
        let master = parse_hls_master(&master_text, &manifest_base_url(&manifest_url));
        master
            .variants
            .iter()
            .find(|v| v.uri == variant_uri)
            .and_then(|v| v.audio_group.as_deref())
            .and_then(|group| {
                pick_audio_rendition(&master.audio, group, opts.audio_language.as_deref())
                    .map(|a| a.uri.clone())
            })
    } else {
        None
    };
    match audio_uri {
        // Separate audio: the audio playlist alone, as a plain
        // media playlist (no merge — there is no video). The
        // captured text belongs to the video playlist, so it is
        // not passed: the audio playlist is fetched normally.
        // The language is already baked into the URI, so the
        // options pass through for proxy + speed limit only.
        Some(uri) => download_hls_variant_opts(
            &uri,
            &uri,
            destination_dir,
            filename,
            referer,
            None,
            progress,
            paused,
            cancelled,
            None,
            opts,
        ),
        // Muxed: the full variant; the caller extracts the audio.
        None => download_hls_variant_opts(
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
            opts,
        ),
    }
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
///
/// No proxy / speed-limit / title options: see
/// [`download_hls_pair_opts`] for those (round 11 extension).
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
    download_hls_pair_opts(
        video_uri,
        audio_uri,
        destination_dir,
        filename,
        referer,
        ffmpeg,
        progress,
        paused,
        cancelled,
        captured,
        &StreamDownloadOptions::default(),
    )
}

/// [`download_hls_pair`] with the round 11 extension options:
/// the proxy (feature 10), the speed limit (feature 3) and the
/// title metadata (feature 12). There is no master playlist in
/// the paired case, so there are no language choices to make —
/// the two URIs ARE the choice.
#[allow(clippy::too_many_arguments)]
pub fn download_hls_pair_opts(
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
    opts: &StreamDownloadOptions,
) -> Result<PathBuf, String> {
    let video_uri = validate_url(video_uri)?;
    let audio_uri = validate_url(audio_uri)?;
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, opts.proxy_url.as_deref())?;
    let bandwidth = SharedLimiter::new(opts.bandwidth_kbps);
    let video_text = manifest_text(&client, &video_uri, referer, captured)?;
    check_hls_encryption_method(&video_text)?;
    let audio_text = fetch_text(&client, &audio_uri, referer)?;
    check_hls_encryption_method(&audio_text)?;
    let video_segments = parse_hls_segments(&video_text, &manifest_base_url(&video_uri))?
        .resolve_keys(&client, referer)?;
    let audio_segments = parse_hls_segments(&audio_text, &manifest_base_url(&audio_uri))?
        .resolve_keys(&client, referer)?;
    let clean = crate::filename::sanitize_filename(filename);
    std::fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let video_bytes = download_segments_with_bandwidth(
        &client,
        &video_segments,
        &video_part,
        referer,
        0,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
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
    download_segments_with_bandwidth(
        &client,
        &audio_segments,
        &audio_part,
        referer,
        video_bytes,
        video_segments.len() as u64,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
    )?;
    let merged = crate::ffmpeg::merge_tracks(
        ffmpeg,
        &video_part,
        Some(&audio_part),
        None,
        opts.title.as_deref(),
        &final_path,
    );
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
    /// The `lang` attribute (Representation, else AdaptationSet):
    /// the audio/subtitle language (an ISO 639 code such as "en").
    /// Round 11 extension, features 2 + 4.
    pub lang: Option<String>,
}

impl DashRepresentation {
    fn is_video(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("video")
            || self.mime_type.to_ascii_lowercase().starts_with("video/")
            || (!self.is_audio()
                && !self.is_text()
                && (self.width.is_some() || self.height.is_some()))
    }

    fn is_audio(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("audio")
            || self.mime_type.to_ascii_lowercase().starts_with("audio/")
    }

    /// A text (subtitle) representation: contentType "text" or a
    /// text/* MIME type. Text reps are NOT formats — they never
    /// appear in the quality list (round 11 extension, feature
    /// 2); they are offered as subtitle languages instead.
    fn is_text(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("text")
            || self.mime_type.to_ascii_lowercase().starts_with("text/")
            || self.mime_type.to_ascii_lowercase().contains("ttml")
    }

    /// A WebVTT text representation — the only subtitle kind the
    /// engine can assemble (feature 2). TTML reps (`application/
    /// ttml+xml`, codecs `stpp`) are NOT WebVTT: their segments
    /// are XML fragments, not complete subtitle documents, and
    /// assembling them is out of scope (see the notes).
    fn is_vtt(&self) -> bool {
        if !self.is_text() {
            return false;
        }
        let mime = self.mime_type.to_ascii_lowercase();
        let codecs = self.codecs.as_deref().unwrap_or("").to_ascii_lowercase();
        mime.contains("vtt") || codecs.contains("wvtt") || mime == "text/vtt"
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
            return Err(format!("Bad SegmentTemplate: unterminated $ in {tpl:?}"));
        };
        let token = &rest[1..end];
        rest = &rest[end + 1..];
        let (name, fmt) = match token.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (token, None),
        };
        let value: String = match name {
            "RepresentationID" => rep_id.to_string(),
            "Bandwidth" => bandwidth.map(|b| b.to_string()).ok_or_else(|| {
                "SegmentTemplate uses $Bandwidth$ but the Representation has no bandwidth"
                    .to_string()
            })?,
            "Number" => number.map(|n| n.to_string()).ok_or_else(|| {
                "SegmentTemplate uses $Number$ but no segment number is available".to_string()
            })?,
            "Time" => time.map(|t| t.to_string()).ok_or_else(|| {
                "SegmentTemplate uses $Time$ but no segment time is available".to_string()
            })?,
            other => {
                return Err(format!("Unsupported SegmentTemplate variable ${other}$"));
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
    let mut set_lang: Option<String> = None;
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
                                &set_lang,
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
                    // `lang` covers both lang="en" and
                    // xml:lang="en" (quick-xml's local_name
                    // strips the prefix).
                    set_lang = attrs.get("lang").cloned();
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
                                repeat: attrs.get("r").and_then(|v| v.parse().ok()).unwrap_or(0),
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
                            range: attrs.get("range").and_then(|v| parse_byte_range(v)),
                        });
                    }
                }
                "SegmentURL" => {
                    if let Some((_, list)) = pending_list.as_mut() {
                        list.segments.push(ListSegment {
                            media: attrs.get("media").cloned(),
                            range: attrs.get("mediaRange").and_then(|v| parse_byte_range(v)),
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
                        lang: attrs.get("lang").cloned(),
                        own_base_url: None,
                        own_template: None,
                        own_list: None,
                    };
                    if is_empty {
                        manifest.representations.push(finish_rep(
                            rep,
                            &set_mime,
                            &set_content,
                            &set_lang,
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
    lang: Option<String>,
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
    set_lang: &Option<String>,
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
        lang: rep.lang.or_else(|| set_lang.clone()),
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

/// [`dash_segments`] with each segment's start time in seconds
/// (round 11 extension, feature 2 — subtitle cues are relative
/// to their segment's start). A SegmentList carries no durations,
/// so its starts are the equal division of the presentation
/// duration across the listed segments — an estimate, but
/// SegmentList subtitle tracks are rare, and a slightly-off
/// subtitle beats none at all.
fn dash_segments_with_times(
    rep: &DashRepresentation,
    manifest_url: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<(Segment, f64)>), String> {
    let base = match &rep.base_url {
        Some(b) => resolve(manifest_url, b),
        None => manifest_url.to_string(),
    };
    if let Some(t) = &rep.template {
        return template_segments_with_times(rep, t, &base, duration_secs);
    }
    if let Some(list) = &rep.segment_list {
        return list_segments_with_times(list, &base, duration_secs);
    }
    Err(format!(
        "Representation {} has neither a SegmentTemplate nor a SegmentList, so its segments cannot be found.",
        rep.id
    ))
}

/// [`list_segments`] with start times: the equal division of the
/// presentation duration across the listed segments (a
/// SegmentList carries no durations of its own). Without a
/// duration the starts are all 0 — the cues stay relative to
/// their segment, which is wrong, but the alternative is no
/// subtitles at all.
fn list_segments_with_times(
    list: &DashSegmentList,
    base: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<(Segment, f64)>), String> {
    let (init, segments) = list_segments(list, base)?;
    let step = match duration_secs {
        Some(d) if d > 0.0 && !segments.is_empty() => d / segments.len() as f64,
        _ => 0.0,
    };
    let timed = segments
        .into_iter()
        .enumerate()
        .map(|(i, seg)| (seg, i as f64 * step))
        .collect();
    Ok((init, timed))
}

fn template_segments(
    rep: &DashRepresentation,
    t: &SegmentTemplate,
    base: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<Segment>), String> {
    let (init, timed) = template_segments_with_times(rep, t, base, duration_secs)?;
    Ok((init, timed.into_iter().map(|(seg, _)| seg).collect()))
}

/// [`template_segments`] with each segment's start time in
/// seconds (round 11 extension, feature 2): subtitle cues are
/// relative to their segment's start, so the merge needs the
/// starts, not just the URLs. Timeline segments carry their
/// start directly; number-based segments start at
/// `n × segment duration`.
fn template_segments_with_times(
    rep: &DashRepresentation,
    t: &SegmentTemplate,
    base: &str,
    duration_secs: Option<f64>,
) -> Result<(Option<Segment>, Vec<(Segment, f64)>), String> {
    let init = match &t.initialization {
        Some(init_tpl) => {
            let url = substitute_template(init_tpl, &rep.id, rep.bandwidth, None, None)?;
            Some(Segment {
                url: resolve(base, &url),
                range: None,
                key: None,
            })
        }
        None => None,
    };
    let media_tpl = t.media.as_ref().ok_or_else(|| {
        "This SegmentTemplate has no media attribute, so its segment URLs cannot be built."
            .to_string()
    })?;
    let uses_time = media_tpl.contains("$Time");
    let timescale = t.timescale.max(1) as f64;
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
                segments.push((
                    Segment {
                        url: resolve(base, &url),
                        range: None,
                        key: None,
                    },
                    time as f64 / timescale,
                ));
                number += 1;
                time += s.duration;
            }
        }
    } else if uses_time {
        return Err(
            "This SegmentTemplate uses $Time$ but the manifest has no SegmentTimeline.".to_string(),
        );
    } else if let Some(d) = t.duration {
        // Number-based: ceil(manifest duration / segment duration).
        // The last segment is the partial remainder; it is a real
        // segment on the server, not an overshoot.
        let seg_secs = d as f64 / timescale;
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
            segments.push((
                Segment {
                    url: resolve(base, &url),
                    range: None,
                    key: None,
                },
                n as f64 * seg_secs,
            ));
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
        key: None,
    });
    let mut segments = Vec::new();
    for s in &list.segments {
        match (&s.media, s.range) {
            (Some(m), _) => segments.push(Segment {
                url: resolve(base, m),
                range: None,
                key: None,
            }),
            (None, Some(r)) => segments.push(Segment {
                url: base.to_string(),
                range: Some(r),
                key: None,
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
/// (chosen, audio pair or None). `audio_language` (round 11
/// extension, feature 4) picks the audio representation whose
/// `lang` matches; when no audio rep matches (the manifest
/// changed between listing and download) the highest-bandwidth
/// audio is used instead — a movie in the wrong language beats a
/// failed download.
fn choose_dash_representation<'a>(
    manifest: &'a DashManifest,
    representation_id: Option<&str>,
    audio_language: Option<&str>,
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
        let audio_reps: Vec<&DashRepresentation> = manifest
            .representations
            .iter()
            .filter(|r| r.is_audio())
            .collect();
        if let Some(want) = audio_language {
            audio_reps
                .iter()
                .find(|r| {
                    r.lang
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(want))
                        || r.id == want
                })
                .copied()
                .or_else(|| {
                    audio_reps
                        .iter()
                        .max_by_key(|r| r.bandwidth.unwrap_or(0))
                        .copied()
                })
        } else {
            audio_reps
                .iter()
                .max_by_key(|r| r.bandwidth.unwrap_or(0))
                .copied()
        }
    } else {
        None
    };
    Ok((chosen, audio))
}

/// Choose the WebVTT subtitle representation matching `language`
/// (round 11 extension, feature 2): the text rep whose `lang`
/// (or id) equals the choice. None when no language was
/// requested, or no VTT rep matches — a subtitle in the wrong
/// language is worse than none, and the download continues
/// without subtitles. TTML reps never match (see
/// [`DashRepresentation::is_vtt`]).
fn choose_dash_subtitle<'a>(
    manifest: &'a DashManifest,
    language: Option<&str>,
) -> Option<&'a DashRepresentation> {
    let want = language?;
    manifest.representations.iter().find(|r| {
        r.is_vtt()
            && (r
                .lang
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(want))
                || r.id == want)
    })
}

/// List the qualities of a DASH manifest: one format per
/// representation. Video representations in a manifest with a
/// separate audio track are video-only (merged with the best audio
/// on download); audio representations are audio-only.
///
/// Sizes (round 11, item 10 — the estimate ladder): each format
/// carries `BANDWIDTH × duration` (~, peak bitrate) when the
/// manifest declares both; the default representation carries the
/// exact byte-range sum when its segments are all ranged
/// (SegmentList with `@mediaRange`, the Atlantic pattern). A
/// video representation's estimate covers the video only — the
/// merged audio's size is added by the plan (see
/// [`crate::extract::plan_from_info`]), which combines kinds
/// with worst-kind-wins.
pub fn list_dash_formats(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
) -> Result<StreamInfo, String> {
    list_dash_formats_with_proxy(manifest_url, referer, captured, None)
}

/// [`list_dash_formats`] through the user-configured proxy
/// (round 11 extension, feature 10).
pub fn list_dash_formats_with_proxy(
    manifest_url: &str,
    referer: &Option<String>,
    captured: Option<&CapturedManifest>,
    proxy: Option<&str>,
) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, proxy)?;
    let text = manifest_text(&client, &manifest_url, referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let has_audio = manifest.representations.iter().any(|r| r.is_audio());
    // Segment count + exact total from the default representation
    // (best effort): the template/timeline expands to a concrete
    // list, so the plan can show the piece count up front like
    // HLS — and a fully ranged list yields the exact total. The
    // init segment counts too — it is a piece the worker
    // downloads.
    let default_segments = choose_dash_representation(&manifest, None, None)
        .ok()
        .and_then(|(chosen, _)| dash_segments(chosen, &manifest_url, manifest.duration_secs).ok());
    let segment_count = default_segments
        .as_ref()
        .map(|(init, segments)| segments.len() as u64 + init.is_some() as u64);
    let default_exact = default_segments.as_ref().and_then(|(init, segments)| {
        let mut all: Vec<Segment> = Vec::with_capacity(segments.len() + 1);
        if let Some(init) = init {
            all.push(init.clone());
        }
        all.extend(segments.iter().cloned());
        exact_total_bytes(&all)
    });
    let default_id = pick_default_representation(&manifest.representations).map(|r| r.id.clone());
    let duration_secs = manifest.duration_secs.map(|d| d as u64);
    let formats = manifest
        .representations
        .iter()
        // Text (subtitle) representations are not formats: they
        // carry no video or audio, and listing them as qualities
        // would offer the user a "quality" that downloads
        // subtitles as a video file. They are offered as subtitle
        // languages instead (round 11 extension, feature 2).
        .filter(|r| !r.is_text())
        .map(|r| {
            let is_video = r.is_video();
            let is_audio = r.is_audio();
            // The estimate ladder (round 11, item 10): the
            // default representation's exact byte-range sum when
            // available, else peak BANDWIDTH × duration. DASH
            // declares no average bandwidth, so there is no
            // middle rung.
            let (filesize, size_kind) = if Some(&r.id) == default_id.as_ref() {
                if let Some(total) = default_exact {
                    (Some(total), Some(SizeKind::Exact))
                } else {
                    dash_size_estimate(r, duration_secs)
                }
            } else {
                dash_size_estimate(r, duration_secs)
            };
            StreamFormat {
                id: r.id.clone(),
                ext: if is_audio && !is_video { "m4a" } else { "mp4" }.to_string(),
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
                filesize,
                size_kind,
                needs_merge: is_video && has_audio,
            }
        })
        .collect();
    // Audio languages: the distinct `lang` of the audio
    // representations (falling back to the representation id
    // when a rep declares no language — the chooser matches on
    // either). Subtitle languages: the distinct `lang` of the
    // WebVTT text representations only (TTML is not offered;
    // see DashRepresentation::is_vtt).
    let mut audio_languages: Vec<String> = Vec::new();
    let mut subtitle_languages: Vec<String> = Vec::new();
    for r in &manifest.representations {
        let label = r.lang.clone().unwrap_or_else(|| r.id.clone());
        if r.is_audio() && !audio_languages.contains(&label) {
            audio_languages.push(label.clone());
        }
        if r.is_vtt() && !subtitle_languages.contains(&label) {
            subtitle_languages.push(label);
        }
    }
    Ok(StreamInfo {
        kind: StreamKind::Dash,
        title: title_from_manifest_url(&manifest_url),
        duration_secs,
        manifest_url,
        formats,
        segment_count,
        audio_languages,
        subtitle_languages,
    })
}

/// Peak-`BANDWIDTH × duration` size estimate for one DASH
/// representation (round 11, item 10): a rough upper bound, since
/// DASH declares no average bandwidth. None when the duration is
/// unknown or the representation declares no bandwidth.
fn dash_size_estimate(
    r: &DashRepresentation,
    duration_secs: Option<u64>,
) -> (Option<u64>, Option<SizeKind>) {
    let (Some(d), Some(bw)) = (duration_secs, r.bandwidth) else {
        return (None, None);
    };
    if d == 0 {
        return (None, None);
    }
    let bytes = (bw as u128 * d as u128) / 8;
    (
        Some(u64::try_from(bytes).unwrap_or(u64::MAX)),
        Some(SizeKind::Peak),
    )
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
    /// Audio language chosen in the popup (round 11 extension,
    /// feature 4): the `lang` of the audio representation to
    /// merge with. None means the highest-bandwidth audio.
    pub audio_language: Option<String>,
    /// Subtitle language chosen in the popup (round 11
    /// extension, feature 2): the `lang` of the WebVTT text
    /// representation to mux in. None means no subtitles.
    pub subtitle_language: Option<String>,
    /// User-configured proxy (round 11 extension, feature 10),
    /// validated at settings-save time. Never logged.
    pub proxy_url: Option<String>,
    /// Global speed limit in KB/s (round 11 extension, feature
    /// 3), enforced across the segment workers.
    pub bandwidth_kbps: Option<u64>,
    /// Title metadata for the merged output (round 11 extension,
    /// feature 12).
    pub title: Option<String>,
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
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, opts.proxy_url.as_deref())?;
    let text = manifest_text(&client, &manifest_url, &opts.referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let (chosen, audio) = choose_dash_representation(
        &manifest,
        opts.representation_id.as_deref(),
        opts.audio_language.as_deref(),
    )?;
    // Subtitles need a merge too (muxing), but only for a video
    // representation — an audio-only download has no video track
    // to mux subtitles into.
    let subtitle = if chosen.is_video() {
        choose_dash_subtitle(&manifest, opts.subtitle_language.as_deref())
    } else {
        None
    };
    let ffmpeg = if audio.is_some() || subtitle.is_some() {
        Some(crate::ffmpeg::ensure_ffmpeg(data_dir)?)
    } else {
        None
    };
    download_dash_with(
        ffmpeg.as_deref(),
        opts,
        progress,
        paused,
        cancelled,
        captured,
    )
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
    let client = hls_client_with_timeout_and_proxy(REQUEST_TIMEOUT, opts.proxy_url.as_deref())?;
    let bandwidth = SharedLimiter::new(opts.bandwidth_kbps);
    let text = manifest_text(&client, &manifest_url, &opts.referer, captured)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    let manifest = parse_dash_manifest(&text)?;
    let (chosen, audio) = choose_dash_representation(
        &manifest,
        opts.representation_id.as_deref(),
        opts.audio_language.as_deref(),
    )?;
    // Subtitles need ffmpeg too (muxing), but only for a video
    // representation — an audio-only download has no video track
    // to mux subtitles into.
    let subtitle_rep = if chosen.is_video() {
        choose_dash_subtitle(&manifest, opts.subtitle_language.as_deref())
    } else {
        None
    };
    if (audio.is_some() || subtitle_rep.is_some()) && ffmpeg.is_none() {
        return Err(
            "This quality keeps its audio or subtitles on a separate track, which needs ffmpeg to merge. \
             Install ffmpeg (or pick a combined quality) and try again."
                .to_string(),
        );
    }
    let clean = crate::filename::sanitize_filename(&opts.filename);
    std::fs::create_dir_all(&opts.destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(&opts.destination_dir, &clean);
    let video_part = part_path_for(&final_path);
    let (init, mut segments) = dash_segments(chosen, &manifest_url, manifest.duration_secs)?;
    if let Some(init) = init {
        segments.insert(0, init);
    }
    let video_bytes = download_segments_with_bandwidth(
        &client,
        &segments,
        &video_part,
        &opts.referer,
        0,
        0,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
    )?;
    // Subtitles download BEFORE the merge decision: a subtitle
    // failure must not cost the video, so it is a warning, not an
    // error — but the merge decision below needs to know whether
    // a subtitle file actually exists.
    let subtitle_file: Option<PathBuf> = match subtitle_rep {
        Some(rep) => {
            let subs_path = final_path.with_file_name(format!(
                "{}.subs.vtt",
                final_path.file_name().unwrap().to_string_lossy()
            ));
            match download_dash_subtitles(
                &client,
                rep,
                &manifest_url,
                manifest.duration_secs,
                &opts.referer,
                &subs_path,
                paused,
                cancelled,
            ) {
                Ok(path) => Some(path),
                Err(e) if e == "__PAUSED__" || e == "__CANCELLED__" => {
                    let _ = std::fs::remove_file(&subs_path);
                    return Err(e);
                }
                Err(e) => {
                    eprintln!("FetchForge: subtitles could not be downloaded ({e}); continuing without them.");
                    let _ = std::fs::remove_file(&subs_path);
                    None
                }
            }
        }
        None => None,
    };
    let Some(audio_rep) = audio else {
        if let Some(subs) = &subtitle_file {
            // Video-only representation + subtitles: mux the
            // subtitles in (the output is already .mp4, so no
            // re-containerising is needed).
            let ffmpeg = ffmpeg.expect("subtitle merge checked ffmpeg above");
            let lang = opts.subtitle_language.clone().unwrap_or_default();
            let merged = crate::ffmpeg::merge_tracks(
                ffmpeg,
                &video_part,
                None,
                Some((subs.as_path(), lang.as_str())),
                opts.title.as_deref(),
                &final_path,
            );
            let _ = std::fs::remove_file(&video_part);
            let _ = std::fs::remove_file(subs);
            merged?;
            remove_seg_dir(&video_part);
            return Ok(final_path);
        }
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
    download_segments_with_bandwidth(
        &client,
        &audio_segments,
        &audio_part,
        &opts.referer,
        video_bytes,
        segments.len() as u64,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
        &bandwidth,
    )?;
    let ffmpeg = ffmpeg.expect("audio merge checked ffmpeg above");
    let subtitle_arg = subtitle_file.as_ref().map(|p| {
        (
            p.as_path(),
            opts.subtitle_language.clone().unwrap_or_default(),
        )
    });
    let merged = crate::ffmpeg::merge_tracks(
        ffmpeg,
        &video_part,
        Some(&audio_part),
        subtitle_arg.as_ref().map(|(p, lang)| (*p, lang.as_str())),
        opts.title.as_deref(),
        &final_path,
    );
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    if let Some(subs) = &subtitle_file {
        let _ = std::fs::remove_file(subs);
    }
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
    fn parse_hls_segments_resolves_relative_urls_and_parses_aes128_key() {
        let pl = parse_hls_segments(MEDIA_PLAYLIST, "https://example.com/v/play.m3u8").unwrap();
        assert!(pl.init.is_none(), "a plain playlist has no init segment");
        assert_eq!(pl.segments.len(), 3);
        assert_eq!(pl.segments[0].url, "https://example.com/v/seg0.ts");
        assert_eq!(pl.segments[0].range, None, "a plain segment has no range");
        assert_eq!(pl.segments[2].url, "https://example.com/v/sub/seg2.ts");
        assert!(pl.segments[0].key.is_none(), "a plain segment has no key");
        // Round 12, item 5: AES-128 is PARSED, not refused. The
        // key rides on the segments in force when each appears:
        // seg0/seg1 share the first KEY (explicit IV), seg2
        // follows METHOD=NONE (plain again), and the sequence
        // numbers honour #EXT-X-MEDIA-SEQUENCE.
        let enc = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000001\n\
             #EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n\
             #EXT-X-KEY:METHOD=NONE\n#EXTINF:10,\nseg2.ts\n";
        let pl = parse_hls_segments(enc, "https://example.com/v/play.m3u8").unwrap();
        assert_eq!(pl.segments.len(), 3);
        let k0 = pl.segments[0].key.as_ref().expect("seg0 carries the KEY");
        assert_eq!(k0.uri, "https://example.com/v/key.bin");
        assert_eq!(
            k0.iv,
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(pl.segments[0].sequence, 7);
        assert_eq!(pl.segments[1].sequence, 8);
        assert_eq!(pl.segments[1].key, pl.segments[0].key, "KEY stays in force");
        assert!(pl.segments[2].key.is_none(), "METHOD=NONE clears the key");
        assert_eq!(pl.segments[2].sequence, 9);
        // A KEY with no URI, a malformed IV, and any non-AES-128
        // method (SAMPLE-AES is DRM) are all refused.
        let no_uri = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:10,\nseg0.ts\n";
        assert!(parse_hls_segments(no_uri, "https://example.com/v/play.m3u8").is_err());
        let bad_iv = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xZZ\n#EXTINF:10,\nseg0.ts\n";
        assert!(parse_hls_segments(bad_iv, "https://example.com/v/play.m3u8").is_err());
        let sample_aes =
            "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:10,\nseg0.ts\n";
        let err = parse_hls_segments(sample_aes, "https://example.com/v/play.m3u8").unwrap_err();
        assert!(err.contains("SAMPLE-AES"), "error: {err}");
        assert!(parse_hls_segments("#EXTM3U\n#EXT-X-ENDLIST\n", "https://example.com/x").is_err());
    }

    /// AES-128-CBC encrypt with PKCS#7 padding (test fixture
    /// only): the mirror of [`decrypt_aes128_cbc`], so the
    /// download test serves genuinely encrypted segments.
    fn aes128_cbc_encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        use aes::cipher::{BlockEncrypt, KeyInit};
        use aes::Aes128;
        let mut padded = plain.to_vec();
        let pad = 16 - (plain.len() % 16);
        padded.extend(std::iter::repeat(pad as u8).take(pad));
        let cipher = Aes128::new_from_slice(key).unwrap();
        let mut prev = *iv;
        let mut out = Vec::with_capacity(padded.len());
        for block in padded.chunks_exact(16) {
            let mut b = [0u8; 16];
            for i in 0..16 {
                b[i] = block[i] ^ prev[i];
            }
            let mut ga = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut b);
            cipher.encrypt_block(&mut ga);
            out.extend_from_slice(&b);
            prev = b;
        }
        out
    }

    #[test]
    fn decrypt_aes128_cbc_round_trips_and_strips_pkcs7() {
        // Round 12, item 5: encrypt with the same crate the
        // engine decrypts with, so the test proves the CBC layer
        // (not just a constant). PKCS#7 padding is stripped;
        // unpadded block-aligned plaintext passes through.
        use aes::cipher::{BlockEncrypt, KeyInit};
        use aes::Aes128;
        let key: [u8; 16] = *b"0123456789abcdef";
        let iv: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let plain = b"SEGMENT-ZERO-SEGMENT-ONE";
        let ct = aes128_cbc_encrypt(plain, &key, &iv);
        assert_eq!(decrypt_aes128_cbc(&ct, &key, &iv).unwrap(), plain);
        // Block-aligned plaintext with no padding survives intact:
        // encrypt without padding and decrypt must return it whole.
        let cipher = Aes128::new_from_slice(&key).unwrap();
        let mut raw = b"0123456789abcdef0123456789abcdef".to_vec();
        let mut prev = iv;
        for block in raw.chunks_exact_mut(16) {
            let mut b = [0u8; 16];
            for i in 0..16 {
                b[i] = block[i] ^ prev[i];
            }
            let mut ga = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut b);
            cipher.encrypt_block(&mut ga);
            block.copy_from_slice(&b);
            prev = b;
        }
        assert_eq!(
            decrypt_aes128_cbc(&raw, &key, &iv).unwrap(),
            b"0123456789abcdef0123456789abcdef"
        );
        // A ciphertext that is not a whole number of blocks, and
        // a wrong-length key, are errors, never silent garbage.
        assert!(decrypt_aes128_cbc(&ct[..ct.len() - 1], &key, &iv).is_err());
        assert!(decrypt_aes128_cbc(&[], &key, &iv).is_err());
        // IV parsing: full, short (left-padded) and malformed.
        assert_eq!(
            parse_key_iv("0x00000000000000000000000000000001"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(
            parse_key_iv("0x01"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
        assert_eq!(parse_key_iv("0xZZ"), None);
        assert_eq!(parse_key_iv("01"), None, "the 0x prefix is required");
        assert_eq!(sequence_iv(7)[15], 7);
        assert_eq!(sequence_iv(7)[..8], [0; 8]);
    }

    #[test]
    fn parse_hls_segments_reads_byteranges_with_explicit_offsets() {
        // totallyacdn (atlantic, 2026-11-23): every segment is a
        // slice of one giant MP4, addressed by explicit offsets.
        let text = "#EXTM3U\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@0\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:200@100\nmovie.mp4\n#EXT-X-ENDLIST\n";
        let pl = parse_hls_segments(text, "https://cdn.example.com/v/play.m3u8").unwrap();
        assert!(pl.init.is_none());
        assert_eq!(pl.segments.len(), 2);
        assert_eq!(pl.segments[0].url, "https://cdn.example.com/v/movie.mp4");
        assert_eq!(
            pl.segments[0].range,
            Some((0, 99)),
            "100 bytes at 0 => 0-99"
        );
        assert_eq!(pl.segments[1].range, Some((100, 299)));
        assert_eq!(pl.piece_count(), 2);
    }

    #[test]
    fn parse_hls_segments_continues_omitted_offsets() {
        // An omitted offset continues from the previous segment's
        // end; the FIRST ranged segment with an omitted offset
        // starts at 0.
        let text = "#EXTM3U\n#EXTINF:10,\n#EXT-X-BYTERANGE:50\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:60\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:70@500\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:80\nmovie.mp4\n#EXT-X-ENDLIST\n";
        let pl = parse_hls_segments(text, "https://cdn.example.com/v/play.m3u8").unwrap();
        assert_eq!(pl.segments[0].range, Some((0, 49)));
        assert_eq!(pl.segments[1].range, Some((50, 109)), "continues from 50");
        assert_eq!(
            pl.segments[2].range,
            Some((500, 569)),
            "explicit offset resets the chain"
        );
        assert_eq!(pl.segments[3].range, Some((570, 649)), "continues from 570");
    }

    #[test]
    fn parse_hls_segments_whole_file_segment_does_not_break_continuation() {
        // A whole-file segment between ranged ones carries no
        // sub-range, so the next omitted offset still continues
        // from the last RANGED segment's end.
        let text = "#EXTM3U\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@0\nmovie.mp4\n#EXTINF:10,\nwhole.ts\n#EXTINF:10,\n#EXT-X-BYTERANGE:100\nmovie.mp4\n#EXT-X-ENDLIST\n";
        let pl = parse_hls_segments(text, "https://cdn.example.com/v/play.m3u8").unwrap();
        assert_eq!(pl.segments[0].range, Some((0, 99)));
        assert_eq!(pl.segments[1].range, None);
        assert_eq!(pl.segments[2].range, Some((100, 199)));
    }

    #[test]
    fn parse_hls_segments_reads_map_init_segment() {
        // fMP4 byterange playlists carry their header in EXT-X-MAP;
        // without it the assembled file has no ftyp/moov and will
        // not play. A MAP byterange without an offset starts at 0.
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"movie.mp4\",BYTERANGE=\"40@0\"\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@40\nmovie.mp4\n#EXT-X-ENDLIST\n";
        let pl = parse_hls_segments(text, "https://cdn.example.com/v/play.m3u8").unwrap();
        let init = pl.init.as_ref().expect("MAP init must be parsed");
        assert_eq!(init.url, "https://cdn.example.com/v/movie.mp4");
        assert_eq!(init.range, Some((0, 39)));
        assert_eq!(pl.piece_count(), 2, "init counts as a piece");
        // A MAP without BYTERANGE is a whole-file init.
        let text2 = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\nseg0.m4s\n#EXT-X-ENDLIST\n";
        let pl2 = parse_hls_segments(text2, "https://cdn.example.com/v/play.m3u8").unwrap();
        assert_eq!(
            pl2.init.as_ref().unwrap().url,
            "https://cdn.example.com/v/init.mp4"
        );
        assert_eq!(pl2.init.as_ref().unwrap().range, None);
        // Only the first MAP is kept (a mid-playlist map change
        // cannot be represented in one assembled file).
        let text3 = "#EXTM3U\n#EXT-X-MAP:URI=\"init1.mp4\"\n#EXTINF:10,\nseg0.m4s\n#EXT-X-MAP:URI=\"init2.mp4\"\n#EXTINF:10,\nseg1.m4s\n#EXT-X-ENDLIST\n";
        let pl3 = parse_hls_segments(text3, "https://cdn.example.com/v/play.m3u8").unwrap();
        assert_eq!(
            pl3.init.as_ref().unwrap().url,
            "https://cdn.example.com/v/init1.mp4"
        );
    }

    #[test]
    fn parse_hls_segments_rejects_malformed_byterange() {
        // A malformed BYTERANGE must fail the parse loudly: silently
        // ignoring it would fetch a whole multi-GB file instead of
        // one slice (the exact atlantic failure).
        for bad in [
            "#EXTM3U\n#EXT-X-BYTERANGE:abc@0\nseg.ts\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:100@xyz\nseg.ts\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:0@0\nseg.ts\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i.mp4\",BYTERANGE=\"nope\"\n#EXTINF:10,\nseg.ts\n",
        ] {
            let err = parse_hls_segments(bad, "https://cdn.example.com/v/play.m3u8").unwrap_err();
            assert!(err.contains("BYTERANGE"), "error must name the tag: {err}");
        }
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
        assert_eq!(
            pick_default_variant(&m.variants).unwrap().height,
            Some(1080)
        );
        let no_res = vec![
            HlsVariant {
                uri: "a".into(),
                bandwidth: Some(100),
                average_bandwidth: None,
                width: None,
                height: None,
                codecs: None,
                fps: None,
                audio_group: None,
                subtitles_group: None,
            },
            HlsVariant {
                uri: "b".into(),
                bandwidth: Some(500),
                average_bandwidth: None,
                width: None,
                height: None,
                codecs: None,
                fps: None,
                audio_group: None,
                subtitles_group: None,
            },
        ];
        assert_eq!(pick_default_variant(&no_res).unwrap().uri, "b");
    }

    #[test]
    fn parse_hls_master_reads_average_bandwidth() {
        // Round 11, item 10: AVERAGE-BANDWIDTH is the estimate
        // ladder's second rung — peak BANDWIDTH can be 4x the
        // average on VBR content (Shaka's 768x576 variant: peak
        // 8.06 Mbps vs average 1.82 Mbps).
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=8060000,AVERAGE-BANDWIDTH=1820000,RESOLUTION=768x576\n\
            v.m3u8\n";
        let m = parse_hls_master(text, "https://example.com/v/master.m3u8");
        assert_eq!(m.variants.len(), 1);
        assert_eq!(m.variants[0].bandwidth, Some(8_060_000));
        assert_eq!(m.variants[0].average_bandwidth, Some(1_820_000));
    }

    #[test]
    fn variant_size_estimate_prefers_average_over_peak() {
        let v = HlsVariant {
            uri: "v.m3u8".into(),
            bandwidth: Some(8_060_000),
            average_bandwidth: Some(1_820_000),
            width: Some(768),
            height: Some(576),
            codecs: None,
            fps: None,
            audio_group: None,
            subtitles_group: None,
        };
        // 1,820,000 bps / 8 * 600 s = 136,500,000 bytes.
        assert_eq!(
            variant_size_estimate(&v, Some(600)),
            (Some(136_500_000), Some(SizeKind::Average))
        );
        let peak_only = HlsVariant {
            average_bandwidth: None,
            ..v.clone()
        };
        // 8,060,000 / 8 * 600 = 604,500,000 — the rough peak bound.
        assert_eq!(
            variant_size_estimate(&peak_only, Some(600)),
            (Some(604_500_000), Some(SizeKind::Peak))
        );
        assert_eq!(variant_size_estimate(&v, None), (None, None));
        let no_bw = HlsVariant {
            bandwidth: None,
            average_bandwidth: None,
            ..v
        };
        assert_eq!(variant_size_estimate(&no_bw, Some(600)), (None, None));
    }

    #[test]
    fn dash_size_estimate_uses_peak_bandwidth() {
        // Round 11, item 10: DASH declares no average bandwidth,
        // so the ladder's only bitrate rung is peak.
        let r = DashRepresentation {
            id: "0".into(),
            bandwidth: Some(1_780_000),
            width: Some(1920),
            height: Some(1080),
            frame_rate: Some(23.98),
            codecs: Some("hev1".into()),
            mime_type: "video/mp4".into(),
            content_type: "video".into(),
            base_url: None,
            template: None,
            segment_list: None,
            lang: None,
        };
        // 1,780,000 / 8 * 100 = 22,250,000 bytes, peak kind.
        assert_eq!(
            dash_size_estimate(&r, Some(100)),
            (Some(22_250_000), Some(SizeKind::Peak))
        );
        assert_eq!(dash_size_estimate(&r, None), (None, None));
    }

    #[test]
    fn size_kind_markers_and_weaker() {
        assert_eq!(SizeKind::Exact.marker(), "");
        assert_eq!(SizeKind::Average.marker(), "≈");
        assert_eq!(SizeKind::Peak.marker(), "~");
        assert_eq!(SizeKind::Measured.marker(), "~");
        assert_eq!(SizeKind::Exact.weaker(SizeKind::Peak), SizeKind::Peak);
        assert_eq!(SizeKind::Average.weaker(SizeKind::Exact), SizeKind::Average);
        assert_eq!(
            SizeKind::Measured.weaker(SizeKind::Average),
            SizeKind::Measured
        );
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

    /// Loopback server for byterange tests: serves `files` by path,
    /// honouring a single `Range: bytes=a-b` header with 206 + the
    /// slice, and serving the WHOLE file with 200 when no Range
    /// header is sent — exactly what totallyacdn does. That makes
    /// the test fail loudly if the engine ignores byteranges (the
    /// assembled file comes out N times too long). Records every
    /// request as `(path, range-header-or-"")` in `hits`, in
    /// arrival order.
    fn serve_range_map(
        files: HashMap<String, Vec<u8>>,
        hits: std::sync::Arc<Mutex<Vec<(String, String)>>>,
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
                    let range_hdr = req
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("range") {
                                Some(v.trim().to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    hits.lock().unwrap().push((path.clone(), range_hdr.clone()));
                    let Some(body) = files.get(&path) else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.flush();
                        continue;
                    };
                    // Single range only, as the engine sends it.
                    let range = range_hdr.strip_prefix("bytes=").and_then(|r| {
                        let (a, b) = r.split_once('-')?;
                        let a: usize = a.parse().ok()?;
                        let b: usize = b.parse().ok()?;
                        if a > b || a >= body.len() {
                            return None;
                        }
                        Some((a, b.min(body.len() - 1)))
                    });
                    match range {
                        Some((a, b)) => {
                            let slice = &body[a..=b];
                            let head = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nConnection: close\r\n\r\n",
                                slice.len(),
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(slice);
                        }
                        None => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
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
        files.insert(
            "/audio.m3u8".to_string(),
            MEDIA_PLAYLIST.as_bytes().to_vec(),
        );
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
        assert_eq!(
            info.formats[0].id,
            format!("http://127.0.0.1:{port}/play.m3u8")
        );
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
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO"
        );
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
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO"
        );
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
        assert_eq!(
            std::fs::read(&out).unwrap(),
            b"SEGMENT-ZERO-SEGMENT-ONE-SEGMENT-TWO"
        );
        assert!(hits.lock().unwrap().contains(&"/seg0.ts".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_decrypts_aes128_playlist() {
        // Round 12, item 5: the fixture serves a keyed playlist
        // the way existing HLS tests stub their servers
        // (serve_map): the key at k.bin, two encrypted segments.
        // seg0 uses an explicit IV; seg1's KEY declares no IV,
        // so it decrypts with its sequence number (1) as the IV.
        // The assembled file must be the PLAINTEXT segments, and
        // the key must have been fetched exactly once (two
        // segments, one distinct URI).
        let key: [u8; 16] = *b"0123456789abcdef";
        let iv0: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let iv1 = sequence_iv(1);
        let mut files = HashMap::new();
        files.insert(
            "/enc.m3u8".to_string(),
            b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x00000000000000000000000000000001\n#EXTINF:10,\ns0.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXTINF:10,\ns1.ts\n".to_vec(),
        );
        files.insert("/k.bin".to_string(), key.to_vec());
        files.insert(
            "/s0.ts".to_string(),
            aes128_cbc_encrypt(b"SEGMENT-ZERO", &key, &iv0),
        );
        files.insert(
            "/s1.ts".to_string(),
            aes128_cbc_encrypt(b"SEGMENT-ONE!", &key, &iv1),
        );
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-aes128");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/enc.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls(
            &url,
            &dir,
            "x.ts",
            &None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SEGMENT-ZEROSEGMENT-ONE!");
        let hits = hits.lock().unwrap();
        assert_eq!(
            hits.iter().filter(|h| *h == "/k.bin").count(),
            1,
            "one distinct key URI is fetched once: {hits:?}"
        );
        assert!(hits.contains(&"/s0.ts".to_string()));
        assert!(hits.contains(&"/s1.ts".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_still_refuses_sample_aes_playlist() {
        // Round 12, item 5: SAMPLE-AES (FairPlay DRM) is still
        // refused — only standard AES-128 was opened up. The
        // Widevine/DRM refusal tests elsewhere are untouched.
        let mut files = HashMap::new();
        files.insert(
            "/drm.m3u8".to_string(),
            b"#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://example\"\n#EXTINF:10,\ns.ts\n"
                .to_vec(),
        );
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("hls-sample-aes");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/drm.m3u8");
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
        assert!(
            err.contains("SAMPLE-AES") || err.contains("encrypted"),
            "error: {err}"
        );
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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mpegts",
            vts.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:d=1",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
            ats.to_str().unwrap()
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
        let segs = parse_hls_segments(text, base).unwrap().segments;
        assert_eq!(segs.len(), 4);
        assert_eq!(
            segs[0].url, "https://totallyacdn.cc/aws/hls/024781/seg0.ts?sign=abc123&exp=999",
            "relative segment must carry the playlist query"
        );
        assert_eq!(
            segs[1].url, "https://totallyacdn.cc/aws/hls/024781/seg1.ts?sign=abc123&exp=999",
            "absolute same-origin segment must carry the playlist query"
        );
        assert_eq!(
            segs[2].url, "https://othercdn.example.com/x/seg2.ts",
            "cross-origin segment must NOT inherit the query"
        );
        assert_eq!(
            segs[3].url, "https://totallyacdn.cc/aws/hls/024781/seg3.ts?token=zzz",
            "a segment with its own query keeps it"
        );
        // An unsigned playlist resolves plainly (no query invented).
        let plain = parse_hls_segments(
            "#EXTM3U\n#EXTINF:10,\nseg0.ts\n",
            "https://cdn.example.com/p/play.m3u8",
        )
        .unwrap();
        assert_eq!(plain.segments.len(), 1);
        assert_eq!(plain.segments[0].url, "https://cdn.example.com/p/seg0.ts");
        assert_eq!(plain.segments[0].range, None);
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
        files.insert(
            "/seg0.ts?sign=s3cret".to_string(),
            b"SEGMENT-ZERO-".to_vec(),
        );
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
        assert!(
            hits.contains(&"/seg0.ts?sign=s3cret".to_string()),
            "hits: {hits:?}"
        );
        assert!(
            hits.contains(&"/seg1.ts?sign=s3cret".to_string()),
            "hits: {hits:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_variant_downloads_byteranges() {
        // End-to-end version of the atlantic root cause (round 8):
        // totallyacdn serves a film as byte ranges of ONE file.
        // The test server honours Range with 206 and serves the
        // WHOLE file on a request without one — so an engine that
        // ignored byteranges would assemble 3 copies of the whole
        // file (900 bytes) instead of the 3 slices (300 bytes).
        // The third segment uses an omitted offset (continuation).
        let mut file = Vec::new();
        file.extend(std::iter::repeat_n(b'A', 100));
        file.extend(std::iter::repeat_n(b'B', 100));
        file.extend(std::iter::repeat_n(b'C', 100));
        let playlist = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@0\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@100\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:100\nmovie.mp4\n#EXT-X-ENDLIST\n".to_vec();
        let mut files = HashMap::new();
        files.insert("/play.m3u8".to_string(), playlist);
        files.insert("/movie.mp4".to_string(), file);
        let hits: std::sync::Arc<Mutex<Vec<(String, String)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_range_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-byterange");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/play.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        // Collect the size estimates: with ranges the total is
        // EXACT (300), known from the first report.
        let estimates: std::sync::Arc<Mutex<Vec<Option<u64>>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let estimates_clone = estimates.clone();
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "out.ts",
            &None,
            None,
            &move |p| estimates_clone.lock().unwrap().push(p.estimated_total),
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let mut expected = Vec::new();
        expected.extend(std::iter::repeat_n(b'A', 100));
        expected.extend(std::iter::repeat_n(b'B', 100));
        expected.extend(std::iter::repeat_n(b'C', 100));
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        let hits = hits.lock().unwrap();
        let seg_hits: Vec<&(String, String)> =
            hits.iter().filter(|(p, _)| p == "/movie.mp4").collect();
        assert_eq!(seg_hits.len(), 3, "three ranged fetches: {hits:?}");
        for (_, range) in &seg_hits {
            assert!(
                range.starts_with("bytes="),
                "every segment fetch must carry a Range header: {hits:?}"
            );
        }
        assert!(
            seg_hits.iter().any(|(_, r)| r == "bytes=0-99"),
            "first slice: {hits:?}"
        );
        assert!(
            seg_hits.iter().any(|(_, r)| r == "bytes=200-299"),
            "omitted-offset continuation: {hits:?}"
        );
        let estimates = estimates.lock().unwrap();
        assert!(
            estimates.iter().any(|e| *e == Some(300)),
            "exact total (300) must be reported: {estimates:?}"
        );
        assert!(
            estimates.iter().all(|e| e.is_none() || *e == Some(300)),
            "no other estimate may appear: {estimates:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_variant_prepends_map_init() {
        // The EXT-X-MAP init segment is fetched once (with its own
        // byterange) and prepended to the assembled file — without
        // it the file has no ftyp/moov header and will not play.
        let mut file = Vec::new();
        file.extend(std::iter::repeat_n(b'I', 40));
        file.extend(std::iter::repeat_n(b'A', 100));
        file.extend(std::iter::repeat_n(b'B', 100));
        let playlist = b"#EXTM3U\n#EXT-X-MAP:URI=\"movie.mp4\",BYTERANGE=\"40@0\"\n#EXTINF:10,\n#EXT-X-BYTERANGE:100@40\nmovie.mp4\n#EXTINF:10,\n#EXT-X-BYTERANGE:100\nmovie.mp4\n#EXT-X-ENDLIST\n".to_vec();
        let mut files = HashMap::new();
        files.insert("/play.m3u8".to_string(), playlist);
        files.insert("/movie.mp4".to_string(), file);
        let hits: std::sync::Arc<Mutex<Vec<(String, String)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_range_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-map");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("http://127.0.0.1:{port}/play.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let totals: std::sync::Arc<Mutex<Vec<Option<u64>>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let totals_clone = totals.clone();
        let out = download_hls_variant(
            &url,
            &url,
            &dir,
            "out.mp4",
            &None,
            None,
            &move |p| totals_clone.lock().unwrap().push(p.segments_total),
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let mut expected = Vec::new();
        expected.extend(std::iter::repeat_n(b'I', 40));
        expected.extend(std::iter::repeat_n(b'A', 100));
        expected.extend(std::iter::repeat_n(b'B', 100));
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        let hits = hits.lock().unwrap();
        assert!(
            hits.iter()
                .any(|(p, r)| p == "/movie.mp4" && r == "bytes=0-39"),
            "the init slice must be fetched: {hits:?}"
        );
        let totals = totals.lock().unwrap();
        assert!(
            totals.iter().any(|t| *t == Some(3)),
            "init + 2 segments = 3 pieces: {totals:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_audio_only_downloads_only_the_audio_playlist() {
        // The popup's "Audio only" choice for a master variant
        // with a separate audio track: the audio playlist alone is
        // downloaded — the video playlist is never even fetched.
        let master = b"#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,AUDIO=\"aac\"\nvideo.m3u8\n#EXT-X-ENDLIST\n".to_vec();
        let video_pl = b"#EXTM3U\n#EXTINF:10,\nv0.ts\n#EXT-X-ENDLIST\n".to_vec();
        let audio_pl =
            b"#EXTM3U\n#EXTINF:10,\na0.ts\n#EXTINF:10,\na1.ts\n#EXT-X-ENDLIST\n".to_vec();
        let mut files = HashMap::new();
        files.insert("/master.m3u8".to_string(), master);
        files.insert("/video.m3u8".to_string(), video_pl);
        files.insert("/audio.m3u8".to_string(), audio_pl);
        files.insert("/v0.ts".to_string(), b"VIDEO-BYTES".to_vec());
        files.insert("/a0.ts".to_string(), b"AUDIO-ZERO-".to_vec());
        files.insert("/a1.ts".to_string(), b"AUDIO-ONE".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits.clone(), stop.clone());
        let dir = unique_temp_dir("hls-audio-only");
        std::fs::create_dir_all(&dir).unwrap();
        let master_url = format!("http://127.0.0.1:{port}/master.m3u8");
        let video_url = format!("http://127.0.0.1:{port}/video.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_audio_only(
            &master_url,
            &video_url,
            &dir,
            "audio.ts",
            &None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"AUDIO-ZERO-AUDIO-ONE");
        let hits = hits.lock().unwrap();
        assert!(
            !hits
                .iter()
                .any(|h: &String| h.contains("video.m3u8") || h.contains("v0.ts")),
            "the video track must never be fetched: {hits:?}"
        );
        assert!(hits.contains(&"/audio.m3u8".to_string()), "hits: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_hls_audio_only_falls_back_to_the_muxed_variant() {
        // No separate audio track: the full (muxed) variant is
        // downloaded and the caller extracts the audio from it.
        let master = b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\nvideo.m3u8\n#EXT-X-ENDLIST\n".to_vec();
        let video_pl = b"#EXTM3U\n#EXTINF:10,\nv0.ts\n#EXT-X-ENDLIST\n".to_vec();
        let mut files = HashMap::new();
        files.insert("/master.m3u8".to_string(), master);
        files.insert("/video.m3u8".to_string(), video_pl);
        files.insert("/v0.ts".to_string(), b"MUXED-BYTES".to_vec());
        let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_map(files, hits, stop.clone());
        let dir = unique_temp_dir("hls-audio-muxed");
        std::fs::create_dir_all(&dir).unwrap();
        let master_url = format!("http://127.0.0.1:{port}/master.m3u8");
        let video_url = format!("http://127.0.0.1:{port}/video.m3u8");
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let out = download_hls_audio_only(
            &master_url,
            &video_url,
            &dir,
            "audio.ts",
            &None,
            &|_| {},
            &paused,
            &cancelled,
            None,
        )
        .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"MUXED-BYTES");
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
            proxy_inner_manifest_url(
                "https://proxy.example/?u=https://cdn.example.com/v/play.m3u8"
            )
            .as_deref(),
            Some("https://cdn.example.com/v/play.m3u8")
        );
        // Not a manifest wrapper: no url param, or the param is not
        // an HLS manifest (DASH wrappers keep yt-dlp routing).
        assert_eq!(proxy_inner_manifest_url("https://proxy.example/?q=1"), None);
        assert_eq!(
            proxy_inner_manifest_url(
                "https://proxy.example/?url=https://cdn.example.com/v/movie.mp4"
            ),
            None
        );
        assert_eq!(
            proxy_inner_manifest_url(
                "https://proxy.example/?url=https://cdn.example.com/v/manifest.mpd"
            ),
            None
        );
        // And the wrapper itself routes to the HLS engine.
        assert_eq!(
            stream_kind_of_url("https://proxy.example/?url=https://cdn.example.com/v/play.m3u8"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            stream_kind_of_url("https://proxy.example/?url=https://cdn.example.com/v/manifest.mpd"),
            None
        );
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
        assert!(
            hits.contains(&"/media/seg0.ts".to_string()),
            "hits: {hits:?}"
        );
        assert!(
            hits.contains(&"/media/seg1.ts".to_string()),
            "hits: {hits:?}"
        );
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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mpegts",
            vts.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:d=1",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
            ats.to_str().unwrap()
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
        assert!(looks_like_timeout(
            "error sending request: operation timed out"
        ));
        assert!(looks_like_timeout("request timeout"));
        assert!(looks_like_timeout("deadline exceeded"));
        assert!(!looks_like_timeout("Segment request failed: HTTP 404"));
        assert!(!looks_like_timeout("connection refused"));
        // …fail fast at 0 bytes (no retry), but keep retrying
        // mid-download and for fast refusals.
        assert!(!should_retry_segment(0, 0, "operation timed out"));
        assert!(should_retry_segment(0, 1024, "operation timed out"));
        assert!(should_retry_segment(
            0,
            0,
            "Segment request failed: HTTP 500"
        ));
        assert!(should_retry_segment(
            1,
            0,
            "Segment request failed: HTTP 500"
        ));
        assert!(!should_retry_segment(
            2,
            0,
            "Segment request failed: HTTP 500"
        ));
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
            key: None,
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
            key: None,
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
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(err.contains("after 3 tries"), "error: {err}");
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
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
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
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
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
        let (port, server) = serve_truncated_body(100, b"0123456789", seen.clone());
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
            reqs[0]
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"),
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
            sub(
                "$RepresentationID$/$RepresentationID$_$Number$.m4v",
                Some(3),
                None
            )
            .unwrap(),
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
        assert_eq!(sub("$Bandwidth$.m4v", None, None).unwrap(), "4000000.m4v");
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
        let v720 = m.representations.iter().find(|r| r.id == "v720").unwrap();
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
        assert_eq!(
            pick_default_representation(&m.representations).unwrap().id,
            "v720"
        );
        // Segments for 8 s at 4 s each: init + 2 media segments.
        let (init, segs) =
            dash_segments(v720, "https://example.com/bbb/bbb.mpd", m.duration_secs).unwrap();
        assert_eq!(init.unwrap().url, "https://example.com/bbb/v720/v720_0.m4v");
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
        assert!(parse_dash_manifest(&multi)
            .unwrap_err()
            .contains("Multi-period"));
        let no_period = "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"></MPD>";
        assert!(parse_dash_manifest(no_period).is_err());
        let no_reps =
            "<MPD type=\"static\" xmlns=\"urn:mpeg:dash:schema:mpd:2011\"><Period></Period></MPD>";
        assert!(parse_dash_manifest(no_reps).is_err());
        // $Time$ without a timeline: the template still uses $Time$
        // but the timeline is commented out. Parsing succeeds;
        // segment computation must fail clearly.
        let no_timeline = TIMELINE_MPD
            .replace("<SegmentTimeline>", "<!--")
            .replace("</SegmentTimeline>", "-->");
        let m = parse_dash_manifest(&no_timeline).unwrap();
        let err = dash_segments(
            &m.representations[0],
            "https://example.com/x.mpd",
            m.duration_secs,
        )
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
        let info =
            list_dash_formats(&format!("http://127.0.0.1:{port}/bbb.mpd"), &None, None).unwrap();
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
            audio_language: None,
            subtitle_language: None,
            proxy_url: None,
            bandwidth_kbps: None,
            title: None,
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
            audio_language: None,
            subtitle_language: None,
            proxy_url: None,
            bandwidth_kbps: None,
            title: None,
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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            vmp4.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:d=1",
            "-c:a",
            "aac",
            am4a.to_str().unwrap()
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
            audio_language: None,
            subtitle_language: None,
            proxy_url: None,
            bandwidth_kbps: None,
            title: None,
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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            vmp4.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:d=1",
            "-c:a",
            "aac",
            am4a.to_str().unwrap()
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
            audio_language: None,
            subtitle_language: None,
            proxy_url: None,
            bandwidth_kbps: None,
            title: None,
        };
        // Run 1: video completes, audio fails on its last segment.
        let err = download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled, None)
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
        let out = download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled, None)
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
            audio_language: None,
            subtitle_language: None,
            proxy_url: None,
            bandwidth_kbps: None,
            title: None,
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
                key: None,
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
        let no_limit = SharedLimiter::new(None);
        let n_s = download_segments_sequential(
            &client,
            &segments,
            &part_s,
            &None,
            0,
            0,
            &|_| {},
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
            &no_limit,
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
    fn classify_stream_prefix_spots_playlists_not_files() {
        // Round 10, item 4: the first bytes decide playlist vs file.
        assert_eq!(
            classify_stream_prefix(b"#EXTM3U\n#EXT-X-VERSION:3\n"),
            Some(StreamKind::Hls)
        );
        // BOM + leading whitespace tolerated.
        assert_eq!(
            classify_stream_prefix(b"\xef\xbb\xbf\n#EXTM3U\n"),
            Some(StreamKind::Hls)
        );
        assert_eq!(
            classify_stream_prefix(
                b"<?xml version=\"1.0\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\">"
            ),
            Some(StreamKind::Dash)
        );
        assert_eq!(
            classify_stream_prefix(b"<MPD type=\"static\">"),
            Some(StreamKind::Dash)
        );
        // XML that is not a DASH manifest, HTML, JSON, binary media.
        assert_eq!(
            classify_stream_prefix(b"<?xml version=\"1.0\"?>\n<root/>"),
            None
        );
        assert_eq!(classify_stream_prefix(b"<html><body>hi"), None);
        assert_eq!(classify_stream_prefix(b"{\"status\":\"ok\"}"), None);
        assert_eq!(
            classify_stream_prefix(&[0, 0, 0, 24, b'f', b't', b'y', b'p']),
            None
        );
        assert_eq!(classify_stream_prefix(b""), None);
    }

    #[test]
    fn segment_counts_are_cumulative_across_phases() {
        // Round 10, items 2/6: the Plex record went 645/820 back to
        // 353/821 when the audio phase started — its counter was
        // phase-local. A segments_base offsets the counts the same
        // way progress_base offsets the bytes, on both the parallel
        // and the sequential path. Max-based assertions: parallel
        // reports can land out of order, but no report may exceed
        // the true total and the peak must reach it.
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
                key: None,
            })
            .collect();
        let client = hls_client().unwrap();
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        // Parallel path with base 100 (a completed 100-segment
        // video phase): counts run 100..108 of 108, never 0..8.
        let dir_p = unique_temp_dir("seg-cumulative-parallel");
        std::fs::create_dir_all(&dir_p).unwrap();
        let part_p = dir_p.join("audio.ts.part");
        let reports: std::sync::Arc<Mutex<Vec<Progress>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let reports_c = reports.clone();
        download_segments(
            &client,
            &segments,
            &part_p,
            &None,
            0,
            100,
            &move |p| reports_c.lock().unwrap().push(p),
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
        )
        .unwrap();
        let reports = reports.lock().unwrap();
        assert!(!reports.is_empty());
        assert!(reports.iter().all(|p| p.segments_total == Some(108)));
        // No report may fall below the base (the Plex bug: the
        // audio phase restarted at 0); the peak must reach 108.
        assert!(reports.iter().all(|p| p.segments_done.unwrap() >= 100));
        assert_eq!(
            reports.iter().map(|p| p.segments_done.unwrap()).max(),
            Some(108)
        );
        drop(reports);

        // Sequential path with base 50: counts run 51..58 of 58.
        let dir_s = unique_temp_dir("seg-cumulative-sequential");
        std::fs::create_dir_all(&dir_s).unwrap();
        let part_s = dir_s.join("audio.ts.part");
        let reports2: std::sync::Arc<Mutex<Vec<Progress>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let reports2_c = reports2.clone();
        let no_limit = SharedLimiter::new(None);
        download_segments_sequential(
            &client,
            &segments,
            &part_s,
            &None,
            0,
            50,
            &move |p| reports2_c.lock().unwrap().push(p),
            &paused,
            &cancelled,
            DOWNLOAD_STALL_TIMEOUT,
            &no_limit,
        )
        .unwrap();
        let reports2 = reports2.lock().unwrap();
        assert!(!reports2.is_empty());
        assert!(reports2.iter().all(|p| p.segments_total == Some(58)));
        assert_eq!(
            reports2.iter().map(|p| p.segments_done.unwrap()).max(),
            Some(58)
        );
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
            key: None,
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
            key: None,
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
        let (port, server) =
            serve_stall_first_n(files, 4, Duration::from_secs(2), hits.clone(), stop.clone());
        let segments: Vec<Segment> = (0..8)
            .map(|i| Segment {
                url: format!("http://127.0.0.1:{port}/seg{i}.ts"),
                range: None,
                key: None,
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
            key: None,
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
        assert!(!should_fallback_sequential(
            "__PAUSED__",
            &paused,
            &cancelled
        ));
        assert!(!should_fallback_sequential(
            "__CANCELLED__",
            &paused,
            &cancelled
        ));
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
                key: None,
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
                key: None,
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
                key: None,
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
            key: None,
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
                key: None,
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
                key: None,
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
        assert!(hits.is_empty(), "resume re-fetched segments: {hits:?}");
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
                key: None,
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
            0,
            &move |p| {
                reports_clone
                    .lock()
                    .unwrap()
                    .push((p.downloaded, p.estimated_total))
            },
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

    // --- Round 11 extension: languages, subtitles, DASH lang ---

    #[test]
    fn parse_hls_master_keeps_all_audio_renditions_and_subtitles() {
        // novahd carries 7 audio tracks in one group; the parser
        // must keep every rendition (language + name + uri), not
        // just the first, and must read SUBTITLES renditions too.
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",LANGUAGE=\"en\",NAME=\"English\",URI=\"en.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",LANGUAGE=\"es\",NAME=\"Spanish\",URI=\"es.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",LANGUAGE=\"fr\",NAME=\"French\",URI=\"fr.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",NAME=\"English\",URI=\"en.vtt.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"de\",NAME=\"German\",URI=\"de.vtt.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,AUDIO=\"aac\",SUBTITLES=\"subs\"\n\
            v.m3u8\n";
        let m = parse_hls_master(text, "https://example.com/v/master.m3u8");
        assert_eq!(m.audio.len(), 3);
        assert_eq!(m.audio[1].language.as_deref(), Some("es"));
        assert_eq!(m.audio[1].uri, "https://example.com/v/es.m3u8");
        assert_eq!(m.subtitles.len(), 2);
        assert_eq!(m.subtitles[1].language.as_deref(), Some("de"));
        assert_eq!(m.variants[0].audio_group.as_deref(), Some("aac"));
        assert_eq!(m.variants[0].subtitles_group.as_deref(), Some("subs"));
        assert_eq!(
            master_audio_languages(&m),
            vec!["en".to_string(), "es".to_string(), "fr".to_string()]
        );
        assert_eq!(
            master_subtitle_languages(&m),
            vec!["en".to_string(), "de".to_string()]
        );
        // The picker matches by language, by name, and falls back
        // to the first rendition for an unknown language.
        assert_eq!(
            pick_audio_rendition(&m.audio, "aac", Some("es"))
                .unwrap()
                .uri,
            "https://example.com/v/es.m3u8"
        );
        assert_eq!(
            pick_audio_rendition(&m.audio, "aac", Some("French"))
                .unwrap()
                .uri,
            "https://example.com/v/fr.m3u8"
        );
        assert_eq!(
            pick_audio_rendition(&m.audio, "aac", Some("xx"))
                .unwrap()
                .uri,
            "https://example.com/v/en.m3u8"
        );
        assert_eq!(
            pick_subtitle_rendition(&m.subtitles, Some("subs"), Some("de"))
                .unwrap()
                .uri,
            "https://example.com/v/de.vtt.m3u8"
        );
        // A subtitle in the wrong language is worse than none.
        assert!(pick_subtitle_rendition(&m.subtitles, Some("subs"), Some("xx")).is_none());
        assert!(pick_subtitle_rendition(&m.subtitles, Some("subs"), None).is_none());
    }

    #[test]
    fn merge_vtt_segments_shifts_cues_by_extinf_and_timestamp_map() {
        // EXTINF offsets: segment 2's cues shift by segment 1's
        // 10 s; cue settings and identifiers survive.
        let segs = vec![
            (
                10.0,
                "WEBVTT\n\n00:00:01.000 --> 00:00:03.000 line:90%\nHello\n".to_string(),
            ),
            (
                10.0,
                "WEBVTT\n\ncue-2\n00:00:02.500 --> 00:00:04.000\nWorld\n".to_string(),
            ),
        ];
        let merged = merge_vtt_segments(&segs);
        assert!(merged.starts_with("WEBVTT\n\n"), "merged: {merged}");
        assert!(
            merged.contains("00:00:01.000 --> 00:00:03.000 line:90%"),
            "merged: {merged}"
        );
        assert!(merged.contains("cue-2"), "merged: {merged}");
        assert!(
            merged.contains("00:00:12.500 --> 00:00:14.000"),
            "merged: {merged}"
        );
        // X-TIMESTAMP-MAP: the MPEG-TS clock anchors the cues
        // absolutely (90000 ticks = 1 s), independent of EXTINF.
        let mapped = vec![
            (
                10.0,
                "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000\nOne\n"
                    .to_string(),
            ),
            (
                10.0,
                "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:1800000,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000\nTwo\n"
                    .to_string(),
            ),
        ];
        let merged = merge_vtt_segments(&mapped);
        assert!(
            merged.contains("00:00:01.000 --> 00:00:02.000\nOne"),
            "merged: {merged}"
        );
        // Second segment's clock is 10 s ahead of the first's.
        assert!(
            merged.contains("00:00:11.000 --> 00:00:12.000\nTwo"),
            "merged: {merged}"
        );
        // DASH start times: the offset is the segment start.
        let dash = vec![
            (
                0.0,
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nA\n".to_string(),
            ),
            (
                6.0,
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nB\n".to_string(),
            ),
        ];
        let merged = merge_vtt_segments_with_start_times(&dash);
        assert!(
            merged.contains("00:00:07.000 --> 00:00:08.000\nB"),
            "merged: {merged}"
        );
    }

    #[test]
    fn parse_hls_subtitle_playlist_reads_durations_and_urls() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:6.0,\nsub0.vtt\n#EXTINF:6.0,\nsub1.vtt\n#EXT-X-ENDLIST\n";
        let segs = parse_hls_subtitle_playlist(text, "https://example.com/s/play.m3u8").unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].url, "https://example.com/s/sub0.vtt");
        assert_eq!(segs[0].duration_secs, 6.0);
        let enc = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:6,\nsub0.vtt\n";
        assert!(parse_hls_subtitle_playlist(enc, "https://example.com/s/").is_err());
    }

    #[test]
    fn parse_dash_manifest_reads_lang_and_vtt_text_reps() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT10S" xmlns="urn:mpeg:dash:schema:mpd:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v1" bandwidth="1000000" width="1280" height="720">
        <BaseURL>video/</BaseURL>
        <SegmentTemplate media="$RepresentationID$/$Number$.m4s" initialization="$RepresentationID$/init.mp4" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="en">
      <Representation id="a1" bandwidth="128000">
        <BaseURL>audio-en/</BaseURL>
        <SegmentTemplate media="$RepresentationID$/$Number$.m4s" initialization="$RepresentationID$/init.mp4" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="es">
      <Representation id="a2" bandwidth="128000">
        <BaseURL>audio-es/</BaseURL>
        <SegmentTemplate media="$RepresentationID$/$Number$.m4s" initialization="$RepresentationID$/init.mp4" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="text/vtt" contentType="text" lang="en">
      <Representation id="s1" bandwidth="1000">
        <BaseURL>subs-en/</BaseURL>
        <SegmentTemplate media="$RepresentationID$/$Number$.vtt" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="application/ttml+xml" contentType="text" lang="fr">
      <Representation id="s2" bandwidth="1000">
        <BaseURL>subs-fr/</BaseURL>
        <SegmentTemplate media="$RepresentationID$/$Number$.ttml" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let m = parse_dash_manifest(mpd).unwrap();
        assert_eq!(m.representations.len(), 5);
        let a1 = m.representations.iter().find(|r| r.id == "a1").unwrap();
        assert!(a1.is_audio());
        assert_eq!(a1.lang.as_deref(), Some("en"));
        let s1 = m.representations.iter().find(|r| r.id == "s1").unwrap();
        assert!(s1.is_text());
        assert!(s1.is_vtt());
        assert!(!s1.is_video());
        let s2 = m.representations.iter().find(|r| r.id == "s2").unwrap();
        assert!(s2.is_text());
        assert!(!s2.is_vtt(), "TTML is not WebVTT");
        // Language-aware audio choice + subtitle choice.
        let (_, audio) = choose_dash_representation(&m, Some("v1"), Some("es")).unwrap();
        assert_eq!(audio.unwrap().id, "a2");
        let (_, audio) = choose_dash_representation(&m, Some("v1"), None).unwrap();
        assert!(audio.is_some(), "no language = best audio");
        assert_eq!(choose_dash_subtitle(&m, Some("en")).unwrap().id, "s1");
        assert!(
            choose_dash_subtitle(&m, Some("fr")).is_none(),
            "TTML never matches"
        );
        assert!(choose_dash_subtitle(&m, None).is_none());
        // Timed segments for the subtitle rep: 5 segments at 2 s
        // starts (10 s / 2 s), no init.
        let (init, timed) =
            dash_segments_with_times(s1, "https://example.com/m.mpd", m.duration_secs).unwrap();
        assert!(init.is_none());
        assert_eq!(timed.len(), 5);
        assert_eq!(timed[2].1, 4.0);
        assert_eq!(timed[2].0.url, "https://example.com/subs-en/s1/3.vtt");
    }
}
