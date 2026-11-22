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
use std::sync::atomic::AtomicBool;
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
}

/// List the qualities of an HLS or DASH manifest URL (routed by
/// extension). The caller must have checked [`is_manifest_url`].
pub fn list_stream_formats(url: &str) -> Result<StreamInfo, String> {
    match stream_kind_of_url(url) {
        Some(StreamKind::Hls) => list_hls_formats(url),
        Some(StreamKind::Dash) => list_dash_formats(url),
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

fn fetch_text(url: &str, referer: &Option<String>) -> Result<String, String> {
    let c = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.2")
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = c.get(url);
    if let Some(r) = referer {
        req = req.header("Referer", r.clone());
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Manifest request failed: HTTP {}", resp.status()));
    }
    resp.text().map_err(|e| e.to_string())
}

/// Fetch bytes, optionally a single inclusive byte range
/// (DASH SegmentList `@mediaRange`). A 206 response is used as-is;
/// a 200 response (server ignored the Range header) is sliced to
/// the requested range, so the assembly stays correct.
fn fetch_bytes_range(
    url: &str,
    range: Option<(u64, u64)>,
    referer: &Option<String>,
) -> Result<Vec<u8>, String> {
    let c = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.2")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = c.get(url);
    if let Some(r) = referer {
        req = req.header("Referer", r.clone());
    }
    if let Some((start, end)) = range {
        req = req.header("Range", format!("bytes={start}-{end}"));
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("Segment request failed: HTTP {status}"));
    }
    let bytes = resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())?;
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
                    return Err(
                        "Segment range request returned a short body; the server may not support Range requests."
                            .to_string(),
                    );
                }
                Ok(bytes[start..=end].to_vec())
            }
        }
    }
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
pub fn list_hls_formats(manifest_url: &str) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let text = fetch_text(&manifest_url, &None)?;
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
        parse_hls_segments(&text, &manifest_base_url(&manifest_url))?;
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
    // Duration from the default variant's playlist (best effort).
    let duration_secs = pick_default_variant(&master.variants)
        .and_then(|v| fetch_text(&v.uri, &None).ok())
        .and_then(|t| hls_duration_secs(&t));
    Ok(StreamInfo {
        kind: StreamKind::Hls,
        title,
        duration_secs,
        manifest_url,
        formats,
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

fn download_segments(
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    progress: &dyn Fn(Progress),
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
    for (i, seg) in segments.iter().enumerate().skip(start) {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(part_path);
            let _ = std::fs::remove_file(&sidecar);
            return Err("__CANCELLED__".to_string());
        }
        if paused.load(std::sync::atomic::Ordering::Relaxed) {
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
            match fetch_bytes_range(&seg.url, seg.range, referer) {
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
        progress(Progress {
            downloaded: progress_base + downloaded,
            total: None,
            bytes_per_sec: downloaded as f64 / elapsed,
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
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let text = fetch_text(&manifest_url, referer)?;
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
    )
}

/// Download one chosen variant of an HLS master playlist (or a
/// plain media playlist, when `variant_uri` is the manifest URL
/// itself). A variant with a separate audio track downloads both
/// tracks and merges them with `ffmpeg` (`.mp4` output); without
/// ffmpeg that variant is refused with a clear error instead of
/// producing a silent video. Muxed variants concatenate straight
/// to `.ts`.
#[allow(clippy::too_many_arguments)]
pub fn download_hls_variant(
    manifest_url: &str,
    variant_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    ffmpeg: Option<&Path>,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let variant_uri = validate_url(variant_uri)?;
    // The master text tells us whether this variant's audio is a
    // separate track (skip the fetch when the "variant" is the
    // manifest itself — a plain media playlist).
    let audio_uri = if variant_uri != manifest_url {
        let master_text = fetch_text(&manifest_url, referer)?;
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
    let variant_text = fetch_text(&variant_uri, referer)?;
    // Segments resolve against the variant playlist's own URL —
    // unwrapped when the variant came through a proxy worker, so
    // they are fetched from the real CDN, not the proxy origin.
    let video_segments = parse_hls_segments(&variant_text, &manifest_base_url(&variant_uri))?
        .into_iter()
        .map(|url| Segment { url, range: None })
        .collect::<Vec<_>>();
    let audio_segments = match &audio_uri {
        Some(uri) => {
            let audio_text = fetch_text(uri, referer)?;
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
        return Ok(final_path);
    };
    // Separate audio: download it, then merge (stream copy, no
    // re-encode). The merge itself is seconds; pause/cancel are
    // honoured before and after it by the caller re-checking the
    // flags on the next worker run.
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(&video_part);
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
    Ok(final_path)
}

/// Download a PAIRED video + audio variant: two media playlists
/// observed together (no master playlist was available — the
/// extension saw only the variants, e.g. `v0.m3u8` + `audio_1.m3u8`
/// on a streaming site). Both tracks download in full and merge
/// with ffmpeg into one `.mp4`; a video-only variant must never
/// again be delivered as a silent `.ts` when its audio sibling was
/// right there. ffmpeg is required (the caller resolves it when
/// the plan says the download needs a merge).
#[allow(clippy::too_many_arguments)]
pub fn download_hls_pair(
    video_uri: &str,
    audio_uri: &str,
    destination_dir: &Path,
    filename: &str,
    referer: &Option<String>,
    ffmpeg: &Path,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let video_uri = validate_url(video_uri)?;
    let audio_uri = validate_url(audio_uri)?;
    let video_text = fetch_text(&video_uri, referer)?;
    if video_text.contains("#EXT-X-KEY") {
        return Err(
            "This stream is encrypted (EXT-X-KEY). FetchForge does not bypass DRM or encryption."
                .to_string(),
        );
    }
    let audio_text = fetch_text(&audio_uri, referer)?;
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
pub fn list_dash_formats(manifest_url: &str) -> Result<StreamInfo, String> {
    let manifest_url = validate_url(manifest_url)?;
    let text = fetch_text(&manifest_url, &None)?;
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
    Ok(StreamInfo {
        kind: StreamKind::Dash,
        title: title_from_manifest_url(&manifest_url),
        duration_secs: manifest.duration_secs.map(|d| d as u64),
        manifest_url,
        formats,
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
pub fn download_dash(
    data_dir: &Path,
    opts: &DashDownloadOptions,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    // One manifest fetch to decide whether a merge is needed; the
    // assembly fetch is a second cheap GET of the same small file.
    let manifest_url = validate_url(&opts.manifest_url)?;
    let text = fetch_text(&manifest_url, &opts.referer)?;
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
    download_dash_with(ffmpeg.as_deref(), opts, progress, paused, cancelled)
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
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(&opts.manifest_url)?;
    let text = fetch_text(&manifest_url, &opts.referer)?;
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
        return Ok(final_path);
    };
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = std::fs::remove_file(&video_part);
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
            list_hls_formats(&format!("http://127.0.0.1:{port}/master.m3u8")).unwrap();
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
        let info = list_hls_formats(&format!("http://127.0.0.1:{port}/play.m3u8")).unwrap();
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
        let err = download_hls(&url, &dir, "x.ts", &None, &|_| {}, &paused, &cancelled)
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
        let info = list_hls_formats(&proxy).unwrap();
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
        let err = download_segments(
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
        let err = download_segments(
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
        assert_eq!(hits.lock().unwrap().len(), 3, "hits: {:?}", hits.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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
        let info = list_dash_formats(&format!("http://127.0.0.1:{port}/bbb.mpd")).unwrap();
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
        let out = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled).unwrap();
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
        let out = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled).unwrap();
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
            download_dash_with(Some(&ffmpeg), &opts, &|_| {}, &paused, &cancelled).unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        // No ffmpeg -> the same manifest is refused, not silently
        // downloaded as a silent video.
        let err = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled).unwrap_err();
        assert!(err.contains("ffmpeg"), "error: {err}");
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
        let err = download_dash_with(None, &opts, &|_| {}, &paused, &cancelled).unwrap_err();
        assert!(err.contains("DRM"), "error: {err}");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
