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
        return rel.to_string();
    }
    match url::Url::parse(base).and_then(|b| b.join(rel)) {
        Ok(u) => u.to_string(),
        Err(_) => rel.to_string(),
    }
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
    let master = parse_hls_master(&text, &manifest_url);
    let title = title_from_manifest_url(&manifest_url);
    if master.variants.is_empty() {
        // Media playlist: validate it has segments, then one format.
        parse_hls_segments(&text, &manifest_url)?;
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
fn download_segments(
    segments: &[Segment],
    part_path: &Path,
    referer: &Option<String>,
    progress_base: u64,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
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
        // Retry transient failures (a dropped segment must not fail
        // a whole stream); a persistent failure keeps the .part and
        // sidecar so a later resume continues from here.
        let mut bytes = None;
        let mut last_err = String::new();
        for attempt in 0..3 {
            match fetch_bytes_range(&seg.url, seg.range, referer) {
                Ok(b) => {
                    bytes = Some(b);
                    break;
                }
                Err(e) => {
                    last_err = e;
                    if attempt < 2 {
                        std::thread::sleep(Duration::from_millis(500 * (attempt + 1) as u64));
                    }
                }
            }
        }
        let bytes = match bytes {
            Some(b) => b,
            None => {
                return Err(format!(
                    "Segment {} of {} could not be downloaded after 3 tries. ({last_err})",
                    i + 1,
                    segments.len()
                ));
            }
        };
        use std::io::Write;
        out.write_all(&bytes).map_err(|e| e.to_string())?;
        downloaded += bytes.len() as u64;
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
    let master = parse_hls_master(&text, &manifest_url);
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
        let master = parse_hls_master(&master_text, &manifest_url);
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
    let video_segments = parse_hls_segments(&variant_text, &variant_uri)?
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
    )?;
    let ffmpeg = ffmpeg.expect("audio merge checked ffmpeg above");
    let merged = crate::ffmpeg::merge_av(ffmpeg, &video_part, &audio_part, &final_path);
    let _ = std::fs::remove_file(&video_part);
    let _ = std::fs::remove_file(&audio_part);
    merged?;
    Ok(final_path)
}

// ---------------------------------------------------------------------------
// DASH
// ---------------------------------------------------------------------------

/// List the qualities of a DASH manifest. Implemented with the DASH
/// segment assembly work (same phase); until then a DASH URL fails
/// here with a clear message instead of being saved as a file.
pub fn list_dash_formats(_manifest_url: &str) -> Result<StreamInfo, String> {
    Err(
        "DASH segment assembly is not yet implemented for this manifest shape. \
         Direct video files and HLS are supported; see docs/FEATURE_MATRIX.md."
            .to_string(),
    )
}

/// Minimal DASH support: only clear, single-period manifests with a
/// SegmentTemplate or SegmentList. Anything with ContentProtection,
/// multiple periods, or an unsupported template is refused honestly
/// instead of producing a broken file.
pub fn download_dash(
    manifest_url: &str,
    _destination_dir: &Path,
    _filename: &str,
    referer: &Option<String>,
) -> Result<PathBuf, String> {
    let manifest_url = validate_url(manifest_url)?;
    let text = fetch_text(&manifest_url, referer)?;
    if text.contains("ContentProtection") || text.contains("cenc:") {
        return Err(
            "This DASH stream is DRM-protected (ContentProtection). FetchForge does not bypass DRM."
                .to_string(),
        );
    }
    if text.matches("<Period").count() > 1 {
        return Err("Multi-period DASH manifests are not supported in this release".to_string());
    }
    // Full DASH segment assembly (initialisation + media segments, template
    // substitution, per-representation choice) is implemented in the desktop
    // app's media module in a later milestone; this engine entry point
    // deliberately refuses rather than emitting a corrupt file.
    Err(
        "DASH segment assembly is not yet implemented for this manifest shape. \
         Direct video files and HLS are supported; see docs/FEATURE_MATRIX.md."
            .to_string(),
    )
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
}
