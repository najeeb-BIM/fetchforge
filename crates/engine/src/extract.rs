//! Media extraction via yt-dlp: video pages (YouTube, Vimeo, ...),
//! public Google Drive files, and generic video pages.
//!
//! The direct engine (download.rs) only handles direct file links. A
//! web page (probe returns `text/html`) used to be refused outright.
//! This module turns such a page into a real download: yt-dlp lists
//! the available formats, the user picks one (or the extension path
//! picks a sensible default), and yt-dlp downloads it, merging
//! separate video+audio streams with ffmpeg when needed.
//!
//! NO DRM, EVER. Formats flagged `has_drm` are dropped before they
//! can be listed; a page whose formats are all DRM is refused with a
//! plain explanation, and yt-dlp itself refuses DRM content. Live
//! streams are refused too (recording an unbounded live stream is
//! not a download). See docs/SECURITY.md and media.rs for the rule.
//!
//! TOOL PROVISIONING (decided 2026-11-21, numbers verified on the
//! dev VM by downloading the real release assets):
//!
//! * yt-dlp is BUNDLED with the installer as `yt-dlp.exe` next to
//!   the app/bridge binaries (23,575,744 bytes / 22.5 MiB for
//!   2026.10.26; NSIS LZMA compresses it to roughly half, so the
//!   installer grows by ~10-12 MB). It is the core of this feature
//!   and must work offline after install; a first-run download would
//!   make the very first YouTube paste fail offline. The build script
//!   downloads the pinned release and verifies its SHA-256 before
//!   staging it (see scripts/build-windows.ps1). If the bundled
//!   binary is missing anyway (portable build, dev run), the engine
//!   downloads the same pinned release into `<data-dir>/tools/`
//!   with SHA-256 verification, so the feature self-heals instead of
//!   dying. The version is pinned on purpose: a pinned yt-dlp is a
//!   known-good, reproducible build. yt-dlp updates ship with app
//!   updates (bump YTDLP_VERSION + the hashes below).
//! * ffmpeg is DOWNLOADED ON FIRST USE, only when a merge is
//!   actually needed — see ffmpeg.rs (FFmpegProcessor), which owns
//!   ffmpeg provisioning and A/V merging.
//!
//! yt-dlp lookup order:
//!   1. `FETCHFORGE_YTDLP_PATH` env override (dev and tests; an
//!      override that does not exist is an error, never silently
//!      ignored).
//!   2. Next to the current executable (the bundled install layout).
//!   3. `<data-dir>/tools/` (first-use download location).
//!   4. PATH (a system install).
//!   5. (`ensure_ytdlp` only) Download the pinned release, verify
//!      SHA-256.

use crate::download::Progress;
use crate::filename::{sanitize_filename, unique_path};
use crate::validate::validate_url;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Pinned yt-dlp release. Hashes verified 2026-11-21 by downloading
/// the release assets and comparing against the published SHA2-256SUMS:
///   yt-dlp.exe  23,575,744 bytes  f5389d36...
///   yt-dlp       3,073,072 bytes  e51e69e8...
pub const YTDLP_VERSION: &str = "2026.10.26";
pub const YTDLP_SHA256_WINDOWS: &str =
    "f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d";
pub const YTDLP_SHA256_UNIX: &str =
    "e51e69e89489f199530f13d32a4342da3ddb2cf34842f364603a066db05afbf3";

/// How long `yt-dlp -J` may run before the extraction is abandoned.
/// Extraction is usually a few seconds; 90 s covers slow sites and a
/// cold first run without hanging the UI forever.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a yt-dlp download may go without ANY output (no progress
/// line, no status line) before it is declared stalled and killed.
/// A healthy download prints progress lines constantly; a merge
/// prints its own lines. Five silent minutes means yt-dlp is stuck
/// retrying a dead fragment or a dead server — seen 2026-11-22 as
/// a download sitting at 0% with no error. The watchdog turns that
/// into a clear failure instead of an endless hang.
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(300);

fn ytdlp_exe_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}


/// Lookup only: no download. Used where a tool must already be
/// present (for example, the probe-failure fallback in add_download,
/// which must not trigger a 23 MB download for a dead direct link).
pub fn find_ytdlp(data_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("FETCHFORGE_YTDLP_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "FETCHFORGE_YTDLP_PATH is set to {} but that file does not exist.",
            p.display()
        ));
    }
    // Bundled: next to the current executable (app or bridge).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(ytdlp_exe_name());
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    // First-use download location.
    let tools = data_dir.join("tools").join(ytdlp_exe_name());
    if tools.is_file() {
        return Ok(tools);
    }
    if let Some(p) = crate::ffmpeg::find_on_path(ytdlp_exe_name()) {
        return Ok(p);
    }
    Err(
        "yt-dlp was not found next to the app, in the FetchForge tools folder, or on PATH."
            .to_string(),
    )
}

/// Lookup, downloading the pinned release into `<data-dir>/tools/`
/// (with SHA-256 verification) when the binary is missing.
pub fn ensure_ytdlp(data_dir: &Path) -> Result<PathBuf, String> {
    match find_ytdlp(data_dir) {
        Ok(p) => Ok(p),
        Err(_) => {
            let (url, sha) = if cfg!(windows) {
                (
                    format!(
                        "https://github.com/yt-dlp/yt-dlp/releases/download/{YTDLP_VERSION}/yt-dlp.exe"
                    ),
                    YTDLP_SHA256_WINDOWS,
                )
            } else {
                (
                    format!(
                        "https://github.com/yt-dlp/yt-dlp/releases/download/{YTDLP_VERSION}/yt-dlp"
                    ),
                    YTDLP_SHA256_UNIX,
                )
            };
            let tools_dir = data_dir.join("tools");
            std::fs::create_dir_all(&tools_dir).map_err(|e| e.to_string())?;
            let dest = tools_dir.join(ytdlp_exe_name());
            crate::ffmpeg::download_verified(
                &url,
                sha,
                &dest,
                "yt-dlp (needed to download video pages)",
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
            }
            Ok(dest)
        }
    }
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

/// What a format contains. `VideoOnly` formats are downloaded as
/// `<id>+bestaudio` and merged with ffmpeg; the other kinds need no merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatKind {
    Combined,
    VideoOnly,
    AudioOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFormat {
    pub format_id: String,
    /// Human-readable label for the picker, e.g.
    /// "1080p · mp4 · video + audio · ~12.3 MB".
    pub label: String,
    pub ext: String,
    pub kind: FormatKind,
    pub height: Option<u32>,
    pub width: Option<u32>,
    /// Audio bitrate in kbps (audio formats).
    pub abr: Option<f64>,
    /// Video bitrate in kbps (video formats).
    pub vbr: Option<f64>,
    /// Frames per second, when yt-dlp reports it.
    pub fps: Option<f64>,
    /// File size in bytes: `filesize`, falling back to
    /// `filesize_approx` (then `filesize_approx` is true).
    pub filesize: Option<u64>,
    pub filesize_approx: bool,
    /// True for VideoOnly formats: the download merges this video
    /// stream with the best audio stream (needs ffmpeg).
    pub needs_merge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub title: String,
    pub extractor: String,
    pub duration_secs: Option<u64>,
    pub webpage_url: String,
    /// Sorted best-first: combined, then video-only, then audio-only.
    pub formats: Vec<MediaFormat>,
    /// The format the extension path (no UI) downloads. The UI
    /// pre-selects it in the picker.
    pub default_format_id: Option<String>,
    /// Set only when yt-dlp could not read the page ("Unsupported
    /// URL") and the HTML fallback found the real stream/file URL
    /// embedded in the page instead: the worker must download THIS
    /// URL, not the page URL (a manifest goes to the stream engine,
    /// a direct file to yt-dlp). None for normal extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_url: Option<String>,
}

fn opt_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 || i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn format_label(
    format_id: &str,
    kind: FormatKind,
    ext: &str,
    height: Option<u32>,
    width: Option<u32>,
    abr: Option<f64>,
    fps: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: bool,
    needs_merge: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if format_id == "source" {
        parts.push("Original file".to_string());
    } else {
        match kind {
            FormatKind::Combined | FormatKind::VideoOnly => {
                if let Some(h) = height {
                    let mut res = format!("{h}p");
                    if let Some(f) = fps {
                        if f > 30.5 {
                            res.push_str(&format!(" {f:.0}fps"));
                        }
                    }
                    parts.push(res);
                } else if let (Some(w), Some(h)) = (width, height) {
                    parts.push(format!("{w}x{h}"));
                } else {
                    parts.push("Video".to_string());
                }
            }
            FormatKind::AudioOnly => {
                parts.push("Audio only".to_string());
            }
        }
    }
    parts.push(ext.to_string());
    match kind {
        FormatKind::Combined => {
            if format_id != "source" {
                parts.push("video + audio".to_string());
            }
        }
        FormatKind::VideoOnly => {
            // Only promise a merge when one actually happens: a DASH
            // video representation in a manifest with no audio track
            // downloads as-is (video only, no merge).
            if needs_merge {
                parts.push("video only (merged with best audio)".to_string())
            } else {
                parts.push("video only".to_string())
            }
        }
        FormatKind::AudioOnly => {
            if let Some(a) = abr {
                parts.push(format!("{a:.0} kbps"));
            }
        }
    }
    if let Some(s) = filesize {
        if filesize_approx {
            parts.push(format!("~{}", fmt_size(s)));
        } else {
            parts.push(fmt_size(s));
        }
    }
    parts.join(" · ")
}

/// Parse the JSON from `yt-dlp -J` into a MediaInfo.
///
/// Filtering rules (see the module docs for the DRM rule):
/// * formats with `has_drm: true` are dropped;
/// * storyboard formats (both codecs explicitly `"none"`, e.g. the
///   `mhtml` sb0-sb3 entries) are dropped;
/// * a format with neither codec reported at all (e.g. Google Drive's
///   `source` format, the original file) is kept as Combined.
pub fn parse_media_info(value: &serde_json::Value, page_url: &str) -> Result<MediaInfo, String> {
    if value.is_null() {
        return Err(
            "Media extraction returned no information for this page. The video may have been removed or made private."
                .to_string(),
        );
    }
    if value.get("is_live").and_then(|v| v.as_bool()) == Some(true) {
        return Err(
            "This is a live stream. FetchForge downloads finished videos; it does not record live streams."
                .to_string(),
        );
    }
    let title = opt_str(value, "title")
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("Untitled video")
        .to_string();
    let extractor = opt_str(value, "extractor").unwrap_or("unknown").to_string();
    let duration_secs = value.get("duration").and_then(|v| v.as_f64()).map(|d| d as u64);
    let webpage_url = opt_str(value, "webpage_url")
        .unwrap_or(page_url)
        .to_string();
    let mut formats: Vec<MediaFormat> = Vec::new();
    let mut saw_drm_only = false;
    if let Some(list) = value.get("formats").and_then(|v| v.as_array()) {
        let mut any_drm = false;
        for f in list {
            let Some(format_id) = opt_str(f, "format_id") else {
                continue;
            };
            if f.get("has_drm").and_then(|v| v.as_bool()) == Some(true) {
                any_drm = true;
                continue;
            }
            let vcodec_raw = opt_str(f, "vcodec");
            let acodec_raw = opt_str(f, "acodec");
            // Both codecs explicitly "none" = storyboard/thumbnail, not media.
            if vcodec_raw == Some("none") && acodec_raw == Some("none") {
                continue;
            }
            let ext = opt_str(f, "ext")
                .or_else(|| opt_str(f, "video_ext"))
                .or_else(|| opt_str(f, "audio_ext"))
                .unwrap_or("mp4")
                .to_string();
            if ext.eq_ignore_ascii_case("mhtml") {
                continue;
            }
            let vcodec = vcodec_raw.filter(|c| *c != "none");
            let acodec = acodec_raw.filter(|c| *c != "none");
            let kind = match (vcodec.is_some(), acodec.is_some()) {
                (true, true) => FormatKind::Combined,
                (true, false) => FormatKind::VideoOnly,
                (false, true) => FormatKind::AudioOnly,
                // Neither codec usable. Two cases, and they must not
                // be conflated:
                // - Both fields ABSENT (e.g. Google Drive's `source`,
                //   the original file): treat as combined; it needs
                //   no merge.
                // - Video EXPLICITLY "none" with no acodec field
                //   (YouTube's HLS audio-only entries 233/234 report
                //   exactly this, with protocol m3u8_native and
                //   resolution "audio only"): there is no video
                //   stream, so this is audio-only. Treating it as
                //   combined made the picker offer an audio-only
                //   file as the recommended video: on 2026-11-22 the
                //   Windows e2e downloaded format 234 and got an
                //   AAC audio file in an .mp4, marked completed.
                (false, false) => {
                    if vcodec_raw == Some("none") {
                        FormatKind::AudioOnly
                    } else {
                        FormatKind::Combined
                    }
                }
            };
            let height = f.get("height").and_then(|v| v.as_u64()).map(|h| h as u32);
            let width = f.get("width").and_then(|v| v.as_u64()).map(|w| w as u32);
            let abr = f.get("abr").and_then(|v| v.as_f64());
            let vbr = f.get("vbr").and_then(|v| v.as_f64());
            let fps = f.get("fps").and_then(|v| v.as_f64());
            let (filesize, filesize_approx) =
                match f.get("filesize").and_then(|v| v.as_u64()) {
                    Some(s) => (Some(s), false),
                    None => (
                        f.get("filesize_approx").and_then(|v| v.as_u64()),
                        true,
                    ),
                };
            // Plausibility: some extractors report nonsense sizes
            // (Google Drive's transcodes report ~2.9 KB for a whole
            // video). A real stream carries far more than 1 KB/s, so
            // a size below duration * 1 KB/s is dropped and shown as
            // unknown rather than misleading the picker.
            let filesize = match (filesize, duration_secs) {
                (Some(s), Some(d)) if d > 0 && s < d.saturating_mul(1000) => None,
                (s, _) => s,
            };
            let needs_merge = kind == FormatKind::VideoOnly;
            let label = format_label(
                format_id,
                kind,
                &ext,
                height,
                width,
                abr,
                fps,
                filesize,
                filesize_approx,
                needs_merge,
            );
            formats.push(MediaFormat {
                format_id: format_id.to_string(),
                label,
                ext,
                kind,
                height,
                width,
                abr,
                vbr,
                fps,
                filesize,
                filesize_approx,
                needs_merge,
            });
        }
        saw_drm_only = any_drm && formats.is_empty();
    }
    if formats.is_empty() {
        // Some extractors return no formats array but a single direct
        // file (top-level url + ext). Synthesise one combined format
        // for it instead of failing.
        if value.get("url").and_then(|v| v.as_str()).is_some() {
            let ext = opt_str(value, "ext").unwrap_or("mp4").to_string();
            let filesize = value.get("filesize").and_then(|v| v.as_u64());
            formats.push(MediaFormat {
                format_id: opt_str(value, "format_id").unwrap_or("0").to_string(),
                label: format_label("0", FormatKind::Combined, &ext, None, None, None, None, filesize, false, false),
                ext,
                kind: FormatKind::Combined,
                height: None,
                width: None,
                abr: None,
                vbr: None,
                fps: None,
                filesize,
                filesize_approx: false,
                needs_merge: false,
            });
        } else if saw_drm_only {
            return Err(
                "This video is DRM-protected. FetchForge does not bypass DRM or encryption."
                    .to_string(),
            );
        } else {
            return Err(
                "No downloadable formats were found on this page. The video may be protected, \
                 members-only, or in a format FetchForge cannot download."
                    .to_string(),
            );
        }
    }
    // Best-first: combined, then video-only, then audio-only; within a
    // kind, higher resolution first, then higher bitrate, then larger file.
    formats.sort_by(|a, b| {
        let rank = |k: FormatKind| match k {
            FormatKind::Combined => 0,
            FormatKind::VideoOnly => 1,
            FormatKind::AudioOnly => 2,
        };
        rank(a.kind)
            .cmp(&rank(b.kind))
            .then(b.height.unwrap_or(0).cmp(&a.height.unwrap_or(0)))
            .then(
                b.abr
                    .unwrap_or(0.0)
                    .partial_cmp(&a.abr.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.filesize.unwrap_or(0).cmp(&a.filesize.unwrap_or(0)))
    });
    let default_format_id = pick_default_format(&formats, &extractor);
    Ok(MediaInfo {
        title,
        extractor,
        duration_secs,
        webpage_url,
        formats,
        default_format_id,
        resolved_url: None,
    })
}

/// The default format for the no-UI (extension) path, and the
/// pre-selected format in the picker:
///   1. Google Drive's `source` format (the original file, no transcode).
///   2. Best combined (single-file, no merge needed) at or below 1080p.
///   3. Best video-only at or below 1080p (merged with best audio).
///   4. Best combined at any resolution.
///   5. Best video-only at any resolution.
///   6. Best audio-only.
/// The 1080p cap keeps the automatic choice a reasonable file size;
/// the picker still offers 4K/8K when the user wants it. Within a
/// step, mp4 wins ties (widest player support), then larger file.
fn pick_default_format(formats: &[MediaFormat], extractor: &str) -> Option<String> {
    if extractor.eq_ignore_ascii_case("googledrive") {
        if let Some(f) = formats.iter().find(|f| f.format_id == "source") {
            return Some(f.format_id.clone());
        }
    }
    let best = |kind: FormatKind, max_height: Option<u32>| -> Option<&MediaFormat> {
        formats
            .iter()
            .filter(|f| f.kind == kind)
            .filter(|f| max_height.is_none_or(|m| f.height.unwrap_or(0) <= m))
            .max_by(|a, b| {
                a.height
                    .unwrap_or(0)
                    .cmp(&b.height.unwrap_or(0))
                    .then((a.ext == "mp4").cmp(&(b.ext == "mp4")))
                    .then(a.filesize.unwrap_or(0).cmp(&b.filesize.unwrap_or(0)))
            })
    };
    if let Some(f) = best(FormatKind::Combined, Some(1080)) {
        return Some(f.format_id.clone());
    }
    if let Some(f) = best(FormatKind::VideoOnly, Some(1080)) {
        return Some(f.format_id.clone());
    }
    if let Some(f) = best(FormatKind::Combined, None) {
        return Some(f.format_id.clone());
    }
    if let Some(f) = best(FormatKind::VideoOnly, None) {
        return Some(f.format_id.clone());
    }
    formats
        .iter()
        .filter(|f| f.kind == FormatKind::AudioOnly)
        .max_by(|a, b| {
            a.abr
                .unwrap_or(0.0)
                .partial_cmp(&b.abr.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|f| f.format_id.clone())
}

/// The yt-dlp `-f` selector for a chosen format. A video-only format
/// is paired with the best audio stream in the SAME container when
/// one exists (m4a for mp4 video, webm for webm video) so the merge
/// stays in the video's container: plain `bestaudio` on YouTube is
/// usually opus (webm), which forces an mp4 video to merge into an
/// .mkv file — surprising on Windows, where mp4 plays everywhere.
/// The chain falls back to any audio, then (`/<id>`) to the video
/// alone if the page has no audio stream at all.
pub fn format_selector(format: &MediaFormat) -> String {
    match format.kind {
        FormatKind::VideoOnly => {
            let compat = match format.ext.as_str() {
                "mp4" => "m4a",
                "webm" => "webm",
                _ => "",
            };
            if compat.is_empty() {
                format!("{}+bestaudio/{}", format.format_id, format.format_id)
            } else {
                format!(
                    "{}+bestaudio[ext={}]/{}+bestaudio/{}",
                    format.format_id, compat, format.format_id, format.format_id
                )
            }
        }
        _ => format.format_id.clone(),
    }
}

// ---------------------------------------------------------------------------
// Listing formats
// ---------------------------------------------------------------------------

fn no_console_window(cmd: &mut Command) {
    // On Windows, yt-dlp.exe is a console program: without
    // CREATE_NO_WINDOW it flashes a black console window next to the
    // app for every extraction and download. (Same flag the desktop
    // app uses for the bridge child.)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Join a reader thread, but only briefly: a killed child's
/// grandchildren (ffmpeg, or a `sleep` in a test stub) inherit the
/// pipe and can hold it open long after the child died, so an
/// unbounded join can hang for the grandchild's whole lifetime.
/// After the grace period the thread is detached (dropping the
/// handle); it exits on its own when the pipe finally closes, or
/// when the process ends.
fn join_reader<T: Send>(handle: std::thread::JoinHandle<T>, grace: Duration) -> Option<T> {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() > grace {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    handle.join().ok()
}
fn run_with_timeout(mut child: Child, timeout: Duration) -> Result<(bool, String, String), String> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Media extraction timed out after {} seconds. The site may be slow; try again.",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = join_reader(out_thread, Duration::from_secs(5)).ok_or_else(|| {
        "Media extraction produced no output. Try again.".to_string()
    })?;
    let stderr = join_reader(err_thread, Duration::from_secs(2)).unwrap_or_default();
    Ok((success, stdout, stderr))
}

/// Map yt-dlp's stderr to a plain-English explanation. yt-dlp's raw
/// errors name extractors and HTTP details the user cannot act on;
/// these mappings keep the reason while dropping the jargon. An
/// unmapped error falls back to yt-dlp's own last line, capped.
fn friendly_media_error(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    let has = |s: &str| lower.contains(s);
    if has("unsupported url") {
        return "This page is not a supported video or media page. FetchForge downloads video pages such as YouTube, Vimeo and public Google Drive files, plus direct file links.".to_string();
    }
    if has("private video") || has("login required") || has("sign in") || has("log in") {
        return "This video needs sign-in or is private. FetchForge does not use accounts or cookies, so it cannot download it.".to_string();
    }
    if has("video unavailable") {
        return "This video is unavailable. It may have been removed, made private, or blocked in your region.".to_string();
    }
    if has("drm") || has("encrypted") {
        return "This video is DRM-protected. FetchForge does not bypass DRM or encryption.".to_string();
    }
    if has("http error 403") || has("http error 401") {
        return "Access denied by the site (403). The link may need sign-in or may have expired.".to_string();
    }
    if has("http error 404") {
        return "This page was not found (404). The link may have expired or been removed.".to_string();
    }
    if has("timed out") || has("timeout") {
        return "The site took too long to respond. Try again.".to_string();
    }
    if has("unable to download webpage") || has("name or service not known") || has("connection") {
        return "Could not reach the page. Check the link and your connection, then try again.".to_string();
    }
    if has("no video formats found") || has("no formats found") {
        return "No downloadable video was found on this page. The video may be protected or members-only.".to_string();
    }
    // Fallback: yt-dlp's own last error line, without the "ERROR: " prefix.
    let last = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .unwrap_or("")
        .trim_start_matches("ERROR: ")
        .trim_start_matches("ERROR:");
    if last.is_empty() {
        "Media extraction failed for this page. The video may be protected or the site may have changed.".to_string()
    } else {
        let capped: String = last.chars().take(300).collect();
        format!("Media extraction failed: {capped}")
    }
}

/// List formats for a page, using an explicit yt-dlp binary (the
/// testable core; production callers use [`list_formats`]).
pub fn list_formats_with(ytdlp: &Path, url: &str) -> Result<MediaInfo, String> {
    list_formats_with_fallback(ytdlp, url, true)
}

fn list_formats_with_fallback(
    ytdlp: &Path,
    url: &str,
    allow_fallback: bool,
) -> Result<MediaInfo, String> {
    let clean = validate_url(url)?;
    let mut cmd = Command::new(ytdlp);
    cmd.args([
        "-J",
        "--no-playlist",
        "--no-warnings",
        "--socket-timeout",
        "30",
        &clean,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let child = cmd.spawn().map_err(|e| {
        format!("Could not run yt-dlp ({e}). Reinstall FetchForge, or set FETCHFORGE_YTDLP_PATH to a working yt-dlp binary.")
    })?;
    let output = run_with_timeout(child, EXTRACT_TIMEOUT)?;
    let (_success, stdout, stderr) = output;
    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(v) if !v.is_null() => parse_media_info(&v, &clean),
        _ => {
            // yt-dlp has no extractor for this page ("Unsupported
            // URL") — but the page may still embed its stream in
            // HTML/JS that the generic extractor cannot see (a JS
            // player that loads its manifest at runtime). Try the
            // embedded URLs before failing. Any other error
            // (private, DRM, bot-check, ...) goes straight to the
            // friendly message: a plain HTML fetch has no cookies
            // or session, so it cannot unlock what yt-dlp was
            // refused, and re-fetching would only waste a request.
            if allow_fallback && stderr.to_lowercase().contains("unsupported url") {
                if let Some(result) = try_html_embedded_fallback(ytdlp, &clean) {
                    return result;
                }
            }
            Err(friendly_media_error(&stderr))
        }
    }
}

/// List formats for a page, resolving (and if needed downloading)
/// yt-dlp first. HLS (.m3u8) and DASH (.mpd) URLs never reach
/// yt-dlp: their manifests are parsed directly by media.rs, so a
/// stream lists its qualities even with no yt-dlp installed.
pub fn list_formats(data_dir: &Path, url: &str) -> Result<MediaInfo, String> {
    if crate::media::is_manifest_url(url) {
        let info = crate::media::list_stream_formats(url)?;
        return Ok(stream_info_to_media_info(&info));
    }
    let ytdlp = ensure_ytdlp(data_dir)?;
    list_formats_with(&ytdlp, url)
}

// ---------------------------------------------------------------------------
// HTML fallback for pages yt-dlp cannot read ("Unsupported URL")
// ---------------------------------------------------------------------------

/// Maximum page HTML the fallback reads: a page's markup is small,
/// and a huge body is not a page worth scanning.
const MAX_FALLBACK_HTML_BYTES: u64 = 4 * 1024 * 1024;

/// Media extensions the HTML fallback looks for: manifests first
/// (the real stream), then direct files.
const FALLBACK_MANIFEST_EXTS: [&str; 2] = [".m3u8", ".mpd"];
const FALLBACK_FILE_EXTS: [&str; 4] = [".mp4", ".webm", ".mov", ".mkv"];

/// Fetch a page's HTML for the embedded-stream fallback: a plain GET
/// with the engine's user agent, no cookies (this reqwest client has
/// no cookie store), no auth headers, 60 s timeout, at most 10
/// redirects — the same discipline as the manifest fetches in
/// media.rs. A non-HTML response is still scanned (some players
/// serve the player page as JSON), but the body is capped.
fn fetch_page_html(url: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.2")
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Page request failed: HTTP {}", resp.status()));
    }
    let mut buf = Vec::new();
    resp.take(MAX_FALLBACK_HTML_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// A byte that can appear inside a URL in HTML/JS text. Quotes,
/// angle brackets, whitespace, braces and backslashes end a URL.
/// (`=` is allowed here — it belongs in query strings — but the
/// left-expansion below stops at it, so `src=/path/file.mp4` does
/// not swallow the attribute name.)
fn is_url_byte(b: u8) -> bool {
    b.is_ascii()
        && !b.is_ascii_whitespace()
        && !matches!(
            b,
            b'"' | b'\''
                | b'<'
                | b'>'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b'`'
                | b'\\'
                | b'|'
                | b','
                | b';'
        )
}

/// Resolve an embedded URL candidate against the page URL: absolute
/// URLs pass through, protocol-relative URLs take the page's scheme,
/// relative URLs join against the page.
fn resolve_embedded_url(raw: &str, page_url: &str) -> Option<String> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_string());
    }
    if let Some(rest) = raw.strip_prefix("//") {
        let scheme = page_url.split("://").next().unwrap_or("https");
        return Some(format!("{scheme}://{rest}"));
    }
    url::Url::parse(page_url)
        .and_then(|base| base.join(raw))
        .map(|u| u.to_string())
        .ok()
}

/// Extract the URL around one extension occurrence: expand left to
/// the URL start and right to the URL end (including a query
/// string), unescape JSON/HTML escaping, resolve against the page,
/// and validate. Returns None when the text around the extension
/// is not a usable http(s) URL.
fn extract_url_around(
    html: &str,
    ext_start: usize,
    ext_end: usize,
    page_url: &str,
) -> Option<String> {
    let bytes = html.as_bytes();
    let mut start = ext_start;
    // Left: stop at `=` as well as at the URL terminators, so an
    // unquoted attribute (`src=/videos/clip.mp4`) does not swallow
    // the attribute name into the URL.
    while start > 0 && bytes[start - 1] != b'=' && is_url_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = ext_end;
    while end < bytes.len() && is_url_byte(bytes[end]) {
        end += 1;
    }
    // Byte-walking can land inside a multi-byte char (a non-ASCII
    // byte stops the walk mid-character); snap to boundaries.
    while start < html.len() && !html.is_char_boundary(start) {
        start += 1;
    }
    while end > 0 && !html.is_char_boundary(end) {
        end -= 1;
    }
    if start >= end {
        return None;
    }
    // A trailing full stop is sentence punctuation, not part of
    // the URL (a URL at the end of a sentence).
    let raw = html[start..end].trim_end_matches('.');
    if raw.is_empty() {
        return None;
    }
    if let Some(absolute) = resolve_embedded_url(raw, page_url) {
        if let Ok(valid) = validate_url(&absolute) {
            return Some(valid);
        }
    }
    // The left-expansion may have swallowed a prefix
    // (`url:https://cdn/...`): retry from the last http(s)://
    // inside the candidate.
    let pos = raw.rfind("https://").or_else(|| raw.rfind("http://"))?;
    if pos == 0 {
        return None;
    }
    validate_url(&raw[pos..]).ok()
}

/// Scan page HTML for embedded media URLs: absolute http(s) URLs and
/// relative URLs ending in a media extension, wherever they appear —
/// `src` attributes, JSON blobs (`"file":"..."`), `data-*`
/// attributes, or bare text. Returns absolute URLs in priority
/// order (manifests first, then direct files), document order
/// within each class, deduplicated. Anything that fails URL
/// validation (wrong scheme, embedded credentials) is dropped.
pub fn find_embedded_media_urls(html: &str, page_url: &str) -> Vec<String> {
    // JSON/HTML escaping hides URLs from the scanner
    // (`https:\/\/cdn...` has no contiguous `https://`, and `&amp;`
    // breaks query strings): unescape the whole page first, then
    // scan the clean text throughout.
    let clean = html
        .replace("\\/", "/")
        .replace("\\u0026", "&")
        .replace("&amp;", "&");
    let html = clean.as_str();
    let lower = html.to_ascii_lowercase();
    // Every extension occurrence, in document order.
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for ext in FALLBACK_MANIFEST_EXTS
        .iter()
        .chain(FALLBACK_FILE_EXTS.iter())
    {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(ext) {
            let at = from + rel;
            hits.push((at, ext.len()));
            from = at + ext.len();
        }
    }
    hits.sort_unstable();
    let mut found: Vec<String> = Vec::new();
    for (at, ext_len) in hits {
        let after = at + ext_len;
        // The extension must end the path part: the next char starts
        // a query/fragment or ends the URL. A `/` or a letter after
        // it means the extension is mid-path (`/video.mp4/thumb.jpg`)
        // or a longer word (`video.mp4x`).
        let next_ok = match lower.as_bytes().get(after) {
            None => true,
            Some(b) => {
                matches!(
                    b,
                    b'?' | b'#' | b'&' | b'"' | b'\'' | b'<' | b'>' | b')' | b',' | b';'
                ) || b.is_ascii_whitespace()
            }
        };
        if !next_ok {
            continue;
        }
        if let Some(url) = extract_url_around(html, at, after, page_url) {
            if !found.contains(&url) {
                found.push(url);
            }
        }
    }
    // Manifests first (the real stream), then direct files; stable
    // within each class, so document order survives.
    let (mut manifests, mut files): (Vec<String>, Vec<String>) = found
        .into_iter()
        .partition(|u| crate::media::is_manifest_url(u));
    manifests.append(&mut files);
    manifests
}

/// HTML fallback for a page yt-dlp cannot read: fetch the page, scan
/// for embedded stream/file URLs, and extract the first one that
/// lists formats. Returns None when the page has no embedded media
/// (the caller then reports the original friendly error), or
/// Some(result) when a candidate was found — Ok on success, Err
/// with the candidate's own error when every candidate failed. A
/// manifest candidate lists through media.rs (no yt-dlp); a
/// direct-file candidate lists through yt-dlp's generic extractor
/// with no second HTML fallback (the file URL either lists or it
/// doesn't, and re-fetching it as HTML would only download media
/// bytes). The returned MediaInfo carries `resolved_url` so the
/// planner routes the worker to the found URL.
fn try_html_embedded_fallback(ytdlp: &Path, page_url: &str) -> Option<Result<MediaInfo, String>> {
    let html = fetch_page_html(page_url).ok()?;
    let candidates = find_embedded_media_urls(&html, page_url);
    if candidates.is_empty() {
        return None;
    }
    let mut last_err: Option<String> = None;
    for candidate in &candidates {
        let attempt: Result<MediaInfo, String> = if crate::media::is_manifest_url(candidate) {
            crate::media::list_stream_formats(candidate)
                .map(|info| stream_info_to_media_info(&info))
        } else {
            list_formats_with_fallback(ytdlp, candidate, false)
        };
        match attempt {
            Ok(mut info) => {
                info.resolved_url = Some(candidate.clone());
                return Some(Ok(info));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Some(Err(last_err.unwrap_or_else(|| {
        "No downloadable video was found on this page.".to_string()
    })))
}

/// Convert a parsed HLS/DASH stream into the same MediaInfo shape
/// yt-dlp extraction produces, so the picker and the default-choice
/// rule work identically for streams and video pages.
fn stream_info_to_media_info(info: &crate::media::StreamInfo) -> MediaInfo {
    let mut formats: Vec<MediaFormat> = info
        .formats
        .iter()
        .map(|f| {
            let kind = match (f.is_video, f.is_audio) {
                (true, true) => FormatKind::Combined,
                (true, false) => FormatKind::VideoOnly,
                (false, true) => FormatKind::AudioOnly,
                // A stream that is neither is not listable; treat
                // it as audio-only so it cannot be mistaken for a
                // watchable video (unreachable in practice: HLS and
                // DASH formats always carry at least one track).
                (false, false) => FormatKind::AudioOnly,
            };
            MediaFormat {
                format_id: f.id.clone(),
                label: format_label(
                    &f.id,
                    kind,
                    &f.ext,
                    f.height,
                    f.width,
                    f.abr_kbps,
                    f.fps,
                    f.filesize,
                    false,
                    f.needs_merge,
                ),
                ext: f.ext.clone(),
                kind,
                height: f.height,
                width: f.width,
                abr: f.abr_kbps,
                vbr: f.vbr_kbps,
                fps: f.fps,
                filesize: f.filesize,
                filesize_approx: false,
                needs_merge: f.needs_merge,
            }
        })
        .collect();
    // Same best-first order as yt-dlp results: combined, then
    // video-only, then audio-only; highest resolution first.
    formats.sort_by(|a, b| {
        let rank = |k: FormatKind| match k {
            FormatKind::Combined => 0,
            FormatKind::VideoOnly => 1,
            FormatKind::AudioOnly => 2,
        };
        rank(a.kind)
            .cmp(&rank(b.kind))
            .then(b.height.unwrap_or(0).cmp(&a.height.unwrap_or(0)))
            .then(
                b.vbr
                    .unwrap_or(0.0)
                    .partial_cmp(&a.vbr.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    let extractor = match info.kind {
        crate::media::StreamKind::Hls => "HLS",
        crate::media::StreamKind::Dash => "DASH",
    };
    let default_format_id = pick_default_format(&formats, extractor);
    MediaInfo {
        title: info.title.clone(),
        extractor: extractor.to_string(),
        duration_secs: info.duration_secs,
        webpage_url: info.manifest_url.clone(),
        formats,
        default_format_id,
        resolved_url: None,
    }
}

// ---------------------------------------------------------------------------
// Planning a media download (shared by the bridge and the desktop app)
// ---------------------------------------------------------------------------

/// Everything add_download needs to create the record for a media
/// download, decided once here so the bridge and the desktop app
/// cannot drift apart.
#[derive(Debug, Clone)]
pub struct MediaPlan {
    pub title: String,
    /// Filename for the record: sanitised stem + expected extension.
    /// Corrected to the real file name when the download completes
    /// (a merge may pick a different container than expected).
    pub filename: String,
    pub total_bytes: Option<u64>,
    pub format_id: String,
    pub format_label: String,
    pub needs_merge: bool,
    /// Full yt-dlp `-f` selector for the chosen format, from
    /// [`format_selector`] — container-aware, so a merge stays in
    /// the video's container when possible. Empty for stream
    /// downloads (HLS/DASH), which the worker dispatches on
    /// `stream_kind` instead of a selector.
    pub selector: String,
    /// HLS/DASH downloads only: which stream protocol this plan
    /// is. None means a yt-dlp download (or a direct download,
    /// which never produces a plan).
    pub stream_kind: Option<crate::media::StreamKind>,
    /// The URL the worker must actually download. None means the
    /// requested page URL (normal extraction). Set when the HTML
    /// fallback resolved the page to an embedded stream/file URL:
    /// the record URL and the worker input both use this value.
    pub effective_url: Option<String>,
}

fn stem_for_title(title: &str) -> String {
    let clean = sanitize_filename(title);
    let stem = Path::new(&clean)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(clean);
    let stem = if stem.chars().count() > 100 {
        stem.chars().take(100).collect::<String>()
    } else {
        stem
    };
    let stem = stem.trim_end_matches(['.', ' ']).to_string();
    if stem.is_empty() {
        "video".to_string()
    } else {
        stem
    }
}

/// Extract a page, pick a format (the requested one, or the default),
/// and plan the download. `filename` overrides the title-derived
/// name; its extension is replaced by the format's extension, since
/// yt-dlp decides the real container. HLS/DASH manifest URLs plan
/// from the parsed manifest instead (no yt-dlp involved).
pub fn plan_media_download(
    data_dir: &Path,
    url: &str,
    format_id: Option<&str>,
    filename: Option<&str>,
) -> Result<MediaPlan, String> {
    if crate::media::is_manifest_url(url) {
        return plan_stream_download(url, format_id, filename);
    }
    let info = list_formats(data_dir, url)?;
    plan_from_info(&info, format_id, filename)
}

/// Plan an HLS/DASH download from the parsed manifest: same
/// MediaPlan shape as yt-dlp plans, but tagged with the stream
/// protocol so the worker dispatches to media.rs instead of yt-dlp.
fn plan_stream_download(
    url: &str,
    format_id: Option<&str>,
    filename: Option<&str>,
) -> Result<MediaPlan, String> {
    let info = crate::media::list_stream_formats(url)?;
    let media = stream_info_to_media_info(&info);
    let mut plan = plan_from_info(&media, format_id, filename)?;
    plan.stream_kind = Some(info.kind);
    plan.selector = String::new();
    Ok(plan)
}

fn plan_from_info(
    info: &MediaInfo,
    format_id: Option<&str>,
    filename: Option<&str>,
) -> Result<MediaPlan, String> {
    let chosen = match format_id {
        Some(id) => info
            .formats
            .iter()
            .find(|f| f.format_id == id)
            .ok_or_else(|| {
                format!(
                    "The selected quality ({id}) is no longer available for this video. List the formats again and pick another."
                )
            })?,
        None => {
            let id = info.default_format_id.as_deref().ok_or_else(|| {
                "No downloadable format was found for this video.".to_string()
            })?;
            info.formats
                .iter()
                .find(|f| f.format_id == id)
                .ok_or_else(|| "No downloadable format was found for this video.".to_string())?
        }
    };
    let stem = match filename {
        Some(f) => stem_for_title(f),
        None => stem_for_title(&info.title),
    };
    // For a merge the container is yt-dlp's choice (usually mp4);
    // the video format's extension is the best guess until then.
    let expected_ext = chosen.ext.as_str();
    let filename = format!("{stem}.{expected_ext}");
    // Total: the chosen format's size, plus the best audio stream's
    // size when a merge is planned (yt-dlp downloads both).
    let mut total = chosen.filesize;
    if chosen.needs_merge {
        if let Some(audio) = info
            .formats
            .iter()
            .filter(|f| f.kind == FormatKind::AudioOnly)
            .max_by(|a, b| {
                a.abr
                    .unwrap_or(0.0)
                    .partial_cmp(&b.abr.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .and_then(|f| f.filesize)
        {
            total = Some(total.unwrap_or(0) + audio);
        }
    }
    let mut plan = MediaPlan {
        title: info.title.clone(),
        filename,
        total_bytes: total,
        format_id: chosen.format_id.clone(),
        format_label: chosen.label.clone(),
        needs_merge: chosen.needs_merge,
        selector: format_selector(chosen),
        stream_kind: None,
        // HTML-fallback plans: extraction resolved the page to an
        // embedded stream/file URL, and the worker downloads the
        // resolved URL, not the page. Normal extraction leaves
        // this None.
        effective_url: info.resolved_url.clone(),
    };
    // A resolved manifest is routed to the stream engine (empty
    // yt-dlp selector, stream_kind set) exactly as if the user had
    // pasted the manifest URL directly; a resolved direct file keeps
    // its yt-dlp selector.
    if let Some(resolved) = &info.resolved_url {
        if let Some(kind) = crate::media::stream_kind_of_url(resolved) {
            plan.stream_kind = Some(kind);
            plan.selector = String::new();
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Google Drive direct-download fallback
// ---------------------------------------------------------------------------

/// Extract the file ID from a Google Drive file link, if it is one:
/// `/file/d/<id>/view`, `open?id=<id>`, `uc?id=<id>`, `uc?export=download&id=<id>`.
pub fn drive_file_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str().unwrap_or("");
    if host != "drive.google.com" && !host.ends_with(".drive.google.com") {
        return None;
    }
    if let Some(rest) = parsed.path().split("/file/d/").nth(1) {
        let id = rest.split('/').next().unwrap_or("");
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    for (k, v) in parsed.query_pairs() {
        if k == "id" && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Direct download URL for a public Drive file. Small files download
/// straight from here; large files get Google's virus-scan warning
/// page (HTML) instead, which the caller must detect via the probe
/// content-type and NOT treat as a file.
pub fn drive_direct_url(file_id: &str) -> String {
    format!("https://drive.google.com/uc?export=download&id={file_id}")
}

// ---------------------------------------------------------------------------
// Downloading via yt-dlp
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MediaDownloadOptions {
    pub url: String,
    /// Full yt-dlp `-f` selector, computed at plan time by
    /// [`format_selector`] and stored on the record — the download
    /// path must not rebuild it from the bare format id, or the
    /// container-aware audio choice is lost.
    pub selector: String,
    pub needs_merge: bool,
    pub destination_dir: PathBuf,
    /// Filename stem (no extension); the container comes from yt-dlp.
    pub filename_stem: String,
    /// Expected extension, used only to pick a non-colliding stem.
    pub expected_ext: String,
    pub expected_total: Option<u64>,
    pub organise_by_category: bool,
    /// Referer header for the download (the page the user clicked
    /// Download on, sent by the extension). Some CDNs and proxies
    /// refuse requests without the Referer they expect; yt-dlp gets
    /// it as `--referer`. None for pasted links (no page context).
    pub referer: Option<String>,
}

/// Download a media page with yt-dlp, resolving (and if needed
/// downloading) yt-dlp and ffmpeg first. Progress, pause and cancel
/// behave like [`crate::download::download`]: pause/cancel return the
/// `__PAUSED__` / `__CANCELLED__` sentinels via [`stop_reason`].
/// A paused media download resumes from yt-dlp's own `.part` files
/// when the same options are run again.
pub fn download_media(
    data_dir: &Path,
    opts: &MediaDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let ytdlp = ensure_ytdlp(data_dir)?;
    let ffmpeg = if opts.needs_merge {
        Some(crate::ffmpeg::ensure_ffmpeg(data_dir)?)
    } else {
        None
    };
    download_media_with(&ytdlp, ffmpeg.as_deref(), opts, progress, paused, cancelled)
}

/// Escape `%` for a yt-dlp `-o` template (Python `%`-formatting:
/// a literal `%` in a title or folder name must be `%%`, or yt-dlp
/// reads it as a field reference and fails).
fn escape_template(s: &str) -> String {
    s.replace('%', "%%")
}

/// Parse a size token such as `939.19KiB`, `4.12MiB`, `1.34MiB/s`
/// (the `/s` suffix is stripped by the caller) into bytes.
fn parse_size_token(token: &str) -> Option<u64> {
    let token = token.trim();
    if token.eq_ignore_ascii_case("unknown") {
        return None;
    }
    let split = token
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(token.len());
    let (num, unit) = token.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "B" => 1.0,
        "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult) as u64)
}

/// Parse one yt-dlp `--progress --newline` line, e.g.
/// `[download]  54.4% of  939.19KiB at    1.34MiB/s ETA 00:00`
/// or `[download] 100% of  939.19KiB in 00:00:02 at 386.04KiB/s`.
/// Returns (percent 0-100, total bytes of this phase, speed bytes/s).
/// Any other line returns None.
fn parse_progress_line(line: &str) -> Option<(f64, Option<u64>, f64)> {
    let rest = line.trim().strip_prefix("[download]")?.trim();
    let mut tokens = rest.split_whitespace();
    let pct_token = tokens.next()?;
    let pct: f64 = pct_token.strip_suffix('%')?.parse().ok()?;
    if tokens.next() != Some("of") {
        return None;
    }
    let total = parse_size_token(tokens.next()?);
    let mut speed = 0.0;
    let mut prev = "";
    for tok in tokens {
        if prev == "at" {
            if let Some(v) = tok.strip_suffix("/s") {
                speed = parse_size_token(v).unwrap_or(0) as f64;
            }
            break;
        }
        prev = tok;
    }
    Some((pct, total, speed))
}

/// Tracks multi-phase yt-dlp progress (a merge downloads video, then
/// audio, each 0-100%). A percentage reset means a new phase started:
/// the previous phase's total joins the completed bytes.
struct ProgressTracker {
    completed: u64,
    phase_total: Option<u64>,
    last_pct: f64,
    expected_total: Option<u64>,
}

impl ProgressTracker {
    fn new(expected_total: Option<u64>) -> Self {
        Self {
            completed: 0,
            phase_total: None,
            last_pct: 0.0,
            expected_total,
        }
    }

    fn feed(&mut self, pct: f64, phase_bytes: Option<u64>, speed: f64) -> Progress {
        if let Some(t) = phase_bytes {
            if pct + 1.0 < self.last_pct {
                // New phase: the previous phase finished.
                if let Some(prev) = self.phase_total {
                    self.completed += prev;
                }
            }
            self.phase_total = Some(t);
        }
        self.last_pct = pct;
        let in_phase = match self.phase_total {
            Some(t) => (pct / 100.0 * t as f64) as u64,
            None => 0,
        };
        let downloaded = self.completed + in_phase;
        let total = self
            .expected_total
            .or_else(|| self.phase_total.map(|t| self.completed + t));
        Progress {
            downloaded,
            total,
            bytes_per_sec: speed,
        }
    }
}

/// The testable core of [`download_media`]: yt-dlp and ffmpeg paths
/// are explicit, so tests can substitute a stub yt-dlp.
#[allow(clippy::too_many_arguments)]
pub fn download_media_with(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    opts: &MediaDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    download_media_with_stall(
        ytdlp,
        ffmpeg,
        opts,
        progress,
        paused,
        cancelled,
        DOWNLOAD_STALL_TIMEOUT,
    )
}

/// [`download_media_with`] with an explicit stall timeout, so tests
/// can use a short one instead of waiting out five real minutes.
#[allow(clippy::too_many_arguments)]
pub fn download_media_with_stall(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    opts: &MediaDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
    stall_timeout: Duration,
) -> Result<PathBuf, String> {
    let clean_url = validate_url(&opts.url)?;
    if opts.needs_merge && ffmpeg.is_none() {
        return Err(
            "This quality needs ffmpeg to merge the separate video and audio streams, \
             and ffmpeg could not be found or downloaded. Pick a combined (video + audio) \
             quality instead, or install ffmpeg and try again."
                .to_string(),
        );
    }
    let stem = sanitize_filename(&opts.filename_stem);
    let mut dir = opts.destination_dir.clone();
    if opts.organise_by_category {
        let probe_name = format!("{stem}.{}", opts.expected_ext);
        dir = dir.join(crate::category_for_filename(&probe_name));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create download folder: {e}"))?;
    // Non-colliding stem: unique_path on the expected full name, then
    // take its stem back (yt-dlp appends the real extension).
    let unique = unique_path(&dir, &format!("{stem}.{}", opts.expected_ext));
    let unique_stem = unique
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(stem.clone());
    let out_template = format!(
        "{}.%(ext)s",
        escape_template(&dir.join(&unique_stem).to_string_lossy())
    );
    let selector = opts.selector.clone();
    let mut cmd = Command::new(ytdlp);
    cmd.args(["--no-playlist", "--no-warnings", "--progress", "--newline"])
        .arg("--socket-timeout")
        .arg("30")
        // Bound the retry loops: yt-dlp's defaults (10 fragment
        // retries) let a dead fragment or a dead proxy stall a
        // download for many minutes at 0% with no error. Three
        // retries still ride out transient blips.
        .arg("--retries")
        .arg("3")
        .arg("--fragment-retries")
        .arg("3")
        .arg("-f")
        .arg(&selector)
        .arg("-o")
        .arg(&out_template)
        .arg("--print")
        .arg("after_move:filepath");
    if let Some(r) = &opts.referer {
        // The page the download came from: some CDNs and proxies
        // refuse requests without the Referer they expect.
        cmd.arg("--referer").arg(r);
    }
    if let Some(ff) = ffmpeg {
        cmd.arg("--ffmpeg-location").arg(ff);
    }
    cmd.arg(&clean_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        format!("Could not run yt-dlp ({e}). Reinstall FetchForge, or set FETCHFORGE_YTDLP_PATH to a working yt-dlp binary.")
    })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Reader threads: stdout lines go to a channel (parsed live for
    // progress), stderr is collected whole (read at the end, only
    // used when yt-dlp fails).
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        buf
    });
    let mut tracker = ProgressTracker::new(opts.expected_total);
    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stop: Option<crate::download::StopReason> = None;
    // Stall watchdog: the last time yt-dlp printed ANYTHING. A
    // download that goes silent for the whole timeout is killed
    // with a clear error instead of sitting at 0% forever.
    let mut last_output = Instant::now();
    let status = loop {
        // Drain available lines without blocking the stop checks.
        match line_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                last_output = Instant::now();
                if let Some((pct, total, speed)) = parse_progress_line(&line) {
                    progress(tracker.feed(pct, total, speed));
                }
                stdout_lines.push(line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_output.elapsed() > stall_timeout
                    && child.try_wait().map(|s| s.is_none()).unwrap_or(false)
                {
                    // Still running but silent for the whole
                    // timeout: stuck retrying a dead fragment or a
                    // dead server. Kill it and fail plainly.
                    let _ = child.kill();
                    let _ = child.wait();
                    let span = if stall_timeout.as_secs() >= 60 {
                        format!("{} minutes", stall_timeout.as_secs() / 60)
                    } else {
                        format!("{} seconds", stall_timeout.as_secs())
                    };
                    return Err(format!(
                        "The download stalled: no progress or output for {span}. \
                         The server may be too slow, the link may have expired, or the \
                         stream may need a different quality. Try again, or pick another quality."
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Stdout closed: the child is exiting; wait for it.
                match child.try_wait() {
                    Ok(Some(s)) => break s,
                    _ => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            stop = Some(crate::download::StopReason::Cancelled);
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        if paused.load(Ordering::Relaxed) {
            stop = Some(crate::download::StopReason::Paused);
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        if let Ok(Some(s)) = child.try_wait() {
            // Drain any lines the reader thread already sent.
            while let Ok(line) = line_rx.try_recv() {
                if let Some((pct, total, speed)) = parse_progress_line(&line) {
                    progress(tracker.feed(pct, total, speed));
                }
                stdout_lines.push(line);
            }
            break s;
        }
    };
    // Bounded joins: a killed yt-dlp can leave an ffmpeg grandchild
    // holding the pipes open (see join_reader).
    let _ = join_reader(stdout_thread, Duration::from_secs(2));
    // Drain lines the reader forwarded after the child was reaped: a
    // fast child can exit before its last lines (the 100% progress
    // line, the final filepath) reach the channel, so draining only
    // inside the loop above loses them intermittently.
    while let Ok(line) = line_rx.try_recv() {
        if let Some((pct, total, speed)) = parse_progress_line(&line) {
            progress(tracker.feed(pct, total, speed));
        }
        stdout_lines.push(line);
    }
    let stderr_text = join_reader(stderr_thread, Duration::from_secs(2)).unwrap_or_default();
    if let Some(reason) = stop {
        match reason {
            crate::download::StopReason::Cancelled => {
                // Cancelled: remove yt-dlp's partial files so no
                // orphan .part is left behind.
                cleanup_media_partials(&dir, &unique_stem);
                return Err("__CANCELLED__".to_string());
            }
            crate::download::StopReason::Paused => {
                // Paused: KEEP the .part files. Re-running the same
                // command resumes from them (yt-dlp native resume).
                return Err("__PAUSED__".to_string());
            }
        }
    }
    if !status.success() {
        return Err(friendly_media_error(&stderr_text));
    }
    // The final path is the `--print after_move:filepath` line: the
    // last stdout line that names an existing file.
    for line in stdout_lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        let p = PathBuf::from(line);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Fallback: the newest non-partial file with our stem.
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(&unique_stem) {
                continue;
            }
            if name.ends_with(".part") || name.ends_with(".ytdl") || name.ends_with(".temp") {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    if let Ok(mtime) = meta.modified() {
                        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                            best = Some((mtime, e.path()));
                        }
                    }
                }
            }
        }
    }
    match best {
        Some((_, p)) => Ok(p),
        None => Err(
            "yt-dlp finished but the downloaded file was not found. Check the download folder; the video may have been removed."
                .to_string(),
        ),
    }
}

/// Remove yt-dlp partial files for a cancelled media download:
/// `<stem>.*.part`, `<stem>.*.ytdl`, `<stem>.*.temp` (merge parts are
/// named `<stem>.f<id>.mp4.part`). Finished files are never touched.
fn cleanup_media_partials(dir: &Path, stem: &str) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(stem)
                && (name.ends_with(".part") || name.ends_with(".ytdl") || name.ends_with(".temp"))
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    fn youtube_json() -> serde_json::Value {
        serde_json::json!({
            "title": "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film",
            "extractor": "youtube",
            "duration": 635,
            "webpage_url": "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "formats": [
                // Storyboard: must be dropped.
                {"format_id": "sb0", "ext": "mhtml", "vcodec": "none", "acodec": "none"},
                // DRM: must be dropped.
                {"format_id": "drm1", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a", "height": 1080, "has_drm": true},
                // Combined 720p.
                {"format_id": "22", "ext": "mp4", "vcodec": "avc1.64001F", "acodec": "mp4a.40.2",
                 "height": 720, "width": 1280, "filesize": 100000000},
                // Combined 360p.
                {"format_id": "18", "ext": "mp4", "vcodec": "avc1.42001E", "acodec": "mp4a.40.2",
                 "height": 360, "width": 640, "filesize": 30000000},
                // Video-only 1080p + 4K.
                {"format_id": "137", "ext": "mp4", "vcodec": "avc1.640028", "acodec": "none",
                 "height": 1080, "width": 1920, "filesize": 200000000, "vbr": 4500.0},
                {"format_id": "313", "ext": "webm", "vcodec": "vp09", "acodec": "none",
                 "height": 2160, "width": 3840, "filesize_approx": 900000000},
                // Audio-only.
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2",
                 "abr": 128.0, "filesize": 10000000},
                {"format_id": "251", "ext": "webm", "vcodec": "none", "acodec": "opus",
                 "abr": 160.0, "filesize": 12000000},
                // YouTube HLS audio-only entries: vcodec explicitly
                // "none", NO acodec field at all (real yt-dlp output
                // for Big Buck Bunny, verified 2026-11-22). Must parse
                // as audio-only, never as combined (regression: they
                // used to be the recommended "video" default).
                {"format_id": "233", "ext": "mp4", "vcodec": "none",
                 "protocol": "m3u8_native", "format_note": "Default, low"},
                {"format_id": "234", "ext": "mp4", "vcodec": "none",
                 "protocol": "m3u8_native", "format_note": "Default, high"}
            ]
        })
    }

    fn drive_json() -> serde_json::Value {
        serde_json::json!({
            "title": "SampleVideo_1280x720_1mb.mp4",
            "extractor": "GoogleDrive",
            "ext": "mp4",
            "webpage_url": "https://drive.google.com/file/d/1FFVqpzcgA-T0db2l6Gz4XxxWBnDNYNg3/view",
            "formats": [
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 128.0, "filesize": 88033},
                {"format_id": "18", "ext": "mp4", "vcodec": "avc1.4D001F", "acodec": "mp4a.40.2", "height": 360, "width": 640},
                {"format_id": "22", "ext": "mp4", "vcodec": "avc1.640029", "acodec": "mp4a.40.2", "height": 720, "width": 1280},
                {"format_id": "source", "ext": "mp4"}
            ]
        })
    }

    #[test]
    fn parses_youtube_info_filters_storyboard_and_drm() {
        let info = parse_media_info(&youtube_json(), "https://www.youtube.com/watch?v=aqz-KE-bpKQ")
            .unwrap();
        assert_eq!(info.title, "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film");
        assert_eq!(info.extractor, "youtube");
        assert_eq!(info.duration_secs, Some(635));
        let ids: Vec<&str> = info.formats.iter().map(|f| f.format_id.as_str()).collect();
        assert!(!ids.contains(&"sb0"), "storyboard must be dropped");
        assert!(!ids.contains(&"drm1"), "DRM format must be dropped");
        assert_eq!(ids.len(), 8);
        // Combined first, best resolution first.
        assert_eq!(ids[0], "22");
        assert_eq!(ids[1], "18");
        // Video-only after combined.
        assert_eq!(info.formats[2].kind, FormatKind::VideoOnly);
        assert!(info.formats[2].needs_merge);
        // Audio-only last.
        assert_eq!(info.formats[5].kind, FormatKind::AudioOnly);
        // Regression (2026-11-22 e2e): YouTube's HLS audio-only
        // entries 233/234 (vcodec "none", no acodec field) must
        // parse as audio-only, never as combined video.
        for hls in ["233", "234"] {
            let f = info.formats.iter().find(|f| f.format_id == hls).unwrap();
            assert_eq!(f.kind, FormatKind::AudioOnly, "format {hls} kind");
            assert!(!f.needs_merge, "format {hls} must not need a merge");
            assert!(f.label.contains("Audio only"), "label: {}", f.label);
        }
        // Default: best combined at or below 1080p = 22 (720p),
        // never the HLS audio entry 234.
        assert_eq!(info.default_format_id.as_deref(), Some("22"));
        // Labels carry resolution + kind + size.
        let f22 = info.formats.iter().find(|f| f.format_id == "22").unwrap();
        assert!(f22.label.contains("720p"), "label: {}", f22.label);
        assert!(f22.label.contains("video + audio"), "label: {}", f22.label);
        let f137 = info.formats.iter().find(|f| f.format_id == "137").unwrap();
        assert!(f137.label.contains("1080p"), "label: {}", f137.label);
        assert!(f137.label.contains("merged with best audio"), "label: {}", f137.label);
    }

    #[test]
    fn default_prefers_video_only_1080_when_no_combined() {
        let mut v = youtube_json();
        // Remove combined formats 22 and 18.
        let formats = v.get_mut("formats").unwrap().as_array_mut().unwrap();
        formats.retain(|f| !matches!(f["format_id"].as_str(), Some("22" | "18")));
        let info = parse_media_info(&v, "https://www.youtube.com/watch?v=aqz-KE-bpKQ").unwrap();
        // Best video-only <= 1080p is 137 (1080p), not 313 (4K).
        assert_eq!(info.default_format_id.as_deref(), Some("137"));
    }

    #[test]
    fn drive_source_format_is_default() {
        let info = parse_media_info(
            &drive_json(),
            "https://drive.google.com/file/d/1FFVqpzcgA-T0db2l6Gz4XxxWBnDNYNg3/view",
        )
        .unwrap();
        assert_eq!(info.formats.len(), 4);
        let source = info.formats.iter().find(|f| f.format_id == "source").unwrap();
        assert_eq!(source.kind, FormatKind::Combined);
        assert!(!source.needs_merge);
        assert!(source.label.contains("Original file"), "label: {}", source.label);
        assert_eq!(info.default_format_id.as_deref(), Some("source"));
    }

    #[test]
    fn implausible_filesizes_are_dropped() {
        // Google Drive reports ~2.9 KB for whole-video transcodes.
        // With a 5 s duration, that is below the 1 KB/s floor and
        // must be shown as unknown, not as "2.8 KB".
        let v = serde_json::json!({
            "title": "Sample",
            "extractor": "GoogleDrive",
            "duration": 5,
            "formats": [
                {"format_id": "22", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a",
                 "height": 720, "filesize": 2899},
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a",
                 "abr": 128.0, "filesize": 88033}
            ]
        });
        let info = parse_media_info(&v, "https://example.com").unwrap();
        let f22 = info.formats.iter().find(|f| f.format_id == "22").unwrap();
        assert_eq!(f22.filesize, None);
        assert!(!f22.label.contains("KB"), "label: {}", f22.label);
        // The plausible audio size survives (88,033 > 5,000).
        let f140 = info.formats.iter().find(|f| f.format_id == "140").unwrap();
        assert_eq!(f140.filesize, Some(88033));
    }

    #[test]
    fn all_drm_formats_are_refused_plainly() {
        let v = serde_json::json!({
            "title": "Protected",
            "extractor": "youtube",
            "formats": [
                {"format_id": "a", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a", "has_drm": true}
            ]
        });
        let err = parse_media_info(&v, "https://example.com/v").unwrap_err();
        assert!(err.contains("DRM"), "error: {err}");
    }

    #[test]
    fn live_streams_are_refused() {
        let v = serde_json::json!({
            "title": "Live",
            "extractor": "youtube",
            "is_live": true,
            "formats": [
                {"format_id": "a", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a"}
            ]
        });
        let err = parse_media_info(&v, "https://example.com/live").unwrap_err();
        assert!(err.contains("live stream"), "error: {err}");
    }

    #[test]
    fn format_selector_pairs_video_only_with_best_audio() {
        let info = parse_media_info(&youtube_json(), "https://example.com").unwrap();
        let video = info.formats.iter().find(|f| f.format_id == "137").unwrap();
        // mp4 video prefers m4a audio so the merge stays mp4 (plain
        // bestaudio would be opus and force an .mkv), then any
        // audio, then the video alone.
        assert_eq!(
            format_selector(video),
            "137+bestaudio[ext=m4a]/137+bestaudio/137"
        );
        let webm = info.formats.iter().find(|f| f.format_id == "313").unwrap();
        assert_eq!(
            format_selector(webm),
            "313+bestaudio[ext=webm]/313+bestaudio/313"
        );
        let combined = info.formats.iter().find(|f| f.format_id == "22").unwrap();
        assert_eq!(format_selector(combined), "22");
        let audio = info.formats.iter().find(|f| f.format_id == "140").unwrap();
        assert_eq!(format_selector(audio), "140");
    }

    #[test]
    fn plan_from_info_computes_filename_and_merge_total() {
        let info = parse_media_info(&youtube_json(), "https://example.com").unwrap();
        // Explicit video-only choice: filename uses the video ext,
        // total = video size + best audio size (251, 12,000,000).
        let plan = plan_from_info(&info, Some("137"), None).unwrap();
        assert_eq!(plan.format_id, "137");
        assert!(plan.needs_merge);
        // The plan carries the full container-aware selector, so the
        // download path cannot lose it (a plain 137+bestaudio would
        // merge to .mkv via opus audio).
        assert_eq!(plan.selector, "137+bestaudio[ext=m4a]/137+bestaudio/137");
        assert_eq!(plan.total_bytes, Some(200_000_000 + 12_000_000));
        assert!(plan.filename.ends_with(".mp4"), "filename: {}", plan.filename);
        assert!(plan.filename.starts_with("Big Buck Bunny"), "filename: {}", plan.filename);
        // Default choice (no format id): 22, no merge.
        let plan = plan_from_info(&info, None, None).unwrap();
        assert_eq!(plan.format_id, "22");
        assert!(!plan.needs_merge);
        assert_eq!(plan.total_bytes, Some(100_000_000));
        // Filename override replaces the title stem.
        let plan = plan_from_info(&info, Some("22"), Some("my holiday video")).unwrap();
        assert_eq!(plan.filename, "my holiday video.mp4");
        // Unknown format id is a clear error, not a panic.
        assert!(plan_from_info(&info, Some("999"), None).is_err());
    }

    #[test]
    fn parses_progress_lines_and_sizes() {
        let (pct, total, speed) =
            parse_progress_line("[download]  54.4% of  939.19KiB at    1.34MiB/s ETA 00:00").unwrap();
        assert!((pct - 54.4).abs() < 0.01);
        assert_eq!(total, Some((939.19 * 1024.0) as u64));
        assert_eq!(speed, (1.34 * 1024.0 * 1024.0) as u64 as f64);
        let (pct, total, _) =
            parse_progress_line("[download] 100% of  939.19KiB in 00:00:02 at 386.04KiB/s").unwrap();
        assert_eq!(pct, 100.0);
        assert!(total.is_some());
        assert!(parse_progress_line("[download] Destination: /tmp/x.mp4").is_none());
        assert!(parse_progress_line("[Merger] Merging formats into \"x.mp4\"").is_none());
        assert!(parse_progress_line("some other line").is_none());
        assert_eq!(parse_size_token("4.12MiB"), Some((4.12 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size_token("1.5GiB"), Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size_token("512B"), Some(512));
        assert_eq!(parse_size_token("Unknown"), None);
    }

    #[test]
    fn progress_tracker_accumulates_phases() {
        let mut t = ProgressTracker::new(None);
        // Phase 1: 4 MiB video.
        let p = t.feed(50.0, Some(4 * 1024 * 1024), 0.0);
        assert_eq!(p.downloaded, 2 * 1024 * 1024);
        let p = t.feed(100.0, Some(4 * 1024 * 1024), 0.0);
        assert_eq!(p.downloaded, 4 * 1024 * 1024);
        // Phase 2 starts (audio, 10 MiB): percentage resets.
        let p = t.feed(10.0, Some(10 * 1024 * 1024), 0.0);
        assert_eq!(p.downloaded, 4 * 1024 * 1024 + 1024 * 1024);
        assert_eq!(p.total, Some(14 * 1024 * 1024));
    }

    #[test]
    fn drive_file_id_and_direct_url() {
        assert_eq!(
            drive_file_id("https://drive.google.com/file/d/1FFVqpzcgA-T0db2l6Gz4XxxWBnDNYNg3/view").as_deref(),
            Some("1FFVqpzcgA-T0db2l6Gz4XxxWBnDNYNg3")
        );
        assert_eq!(
            drive_file_id("https://drive.google.com/open?id=ABC123").as_deref(),
            Some("ABC123")
        );
        assert_eq!(
            drive_file_id("https://drive.google.com/uc?export=download&id=XYZ").as_deref(),
            Some("XYZ")
        );
        assert_eq!(drive_file_id("https://drive.google.com/drive/folders/ABC"), None);
        assert_eq!(drive_file_id("https://example.com/file/d/ABC/view"), None);
        assert_eq!(
            drive_direct_url("ABC123"),
            "https://drive.google.com/uc?export=download&id=ABC123"
        );
    }

    #[test]
    fn friendly_errors_map_ytdlp_stderr() {
        assert!(friendly_media_error("ERROR: Unsupported URL: https://example.com")
            .contains("not a supported video"));
        assert!(friendly_media_error("ERROR: [youtube] abc: Private video")
            .contains("private"));
        assert!(friendly_media_error("ERROR: [youtube] abc: Video unavailable")
            .contains("unavailable"));
        assert!(friendly_media_error("ERROR: This video is DRM protected")
            .contains("DRM"));
        assert!(friendly_media_error("ERROR: HTTP Error 403: Forbidden").contains("403"));
        assert!(friendly_media_error("").contains("failed"));
    }

    // -- Tool resolution. Env vars are process-global, so all
    // env-mutating checks live in this ONE test, run sequentially,
    // and every other test uses explicit paths (the _with variants).
    #[test]
    fn tool_resolution_env_override_data_dir_and_missing() {
        let dir = std::env::temp_dir().join("fetchforge-test-tools");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        // A fake yt-dlp in the data-dir tools folder is found.
        let tools_ytdlp = dir.join("tools").join(ytdlp_exe_name());
        std::fs::write(&tools_ytdlp, b"fake").unwrap();
        // Env override wins over the tools folder.
        let override_path = dir.join("custom-yt-dlp");
        std::fs::write(&override_path, b"fake").unwrap();
        std::env::set_var("FETCHFORGE_YTDLP_PATH", &override_path);
        assert_eq!(find_ytdlp(&dir).unwrap(), override_path);
        // An override pointing at nothing is an error, not a silent skip.
        std::env::set_var("FETCHFORGE_YTDLP_PATH", dir.join("does-not-exist"));
        assert!(find_ytdlp(&dir).is_err());
        std::env::remove_var("FETCHFORGE_YTDLP_PATH");
        // Without the override, the tools-folder binary is found.
        assert_eq!(find_ytdlp(&dir).unwrap(), tools_ytdlp);
        // Missing tools in an empty data dir: clear error (unless a
        // system ffmpeg exists on PATH, which is also a valid find).
        let empty = std::env::temp_dir().join("fetchforge-test-tools-empty");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        match find_ytdlp(&empty) {
            Ok(p) => assert!(
                p.to_string_lossy().contains("yt-dlp"),
                "unexpected yt-dlp at {}",
                p.display()
            ),
            Err(e) => assert!(e.contains("yt-dlp was not found"), "error: {e}"),
        }
    }

    /// Write a stub yt-dlp the production code can spawn, on either OS.
    ///
    /// Windows cannot execute a `#!/bin/sh` script: `CreateProcess` has
    /// no shebang support, so the extensionless shell stub fails to spawn
    /// and every test using it fails on the Windows runner (the v0.2.0
    /// build's "Run tests" step failed exactly this way). On Windows the
    /// stub is therefore a `.bat` wrapper — `Command::new` runs `.bat`
    /// files via `cmd.exe` (std documents this) and std's batch escaping
    /// passes `%` in arguments through untouched, so the `%(ext)s`
    /// output template survives — that forwards its arguments to an
    /// adjacent PowerShell script holding the real stub logic.
    /// PowerShell 5.1 ships with Windows, and `-ExecutionPolicy Bypass`
    /// sidesteps any machine policy.
    fn write_stub_ytdlp(dir: &Path, name: &str, sh_script: &str, ps_script: &str) -> PathBuf {
        if cfg!(windows) {
            let bat_path = dir.join(format!("{name}.bat"));
            let ps_path = dir.join(format!("{name}.ps1"));
            std::fs::write(&ps_path, ps_script).unwrap();
            std::fs::write(
                &bat_path,
                format!(
                    "@echo off\r\npowershell -NoProfile -ExecutionPolicy Bypass -File \"%~dp0{name}.ps1\" %*\r\nexit /b %ERRORLEVEL%\r\n"
                ),
            )
            .unwrap();
            bat_path
        } else {
            let path = dir.join(name);
            // Write + sync + close before the stub is ever executed: on
            // some filesystems an exec immediately after a write can hit
            // ETXTBSY ("Text file busy") if the write is still in flight.
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(sh_script.as_bytes()).unwrap();
            f.sync_all().unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    /// Per-process unique temp dir: the cancel test orphans a `sleep`
    /// grandchild, so a fixed dir name could collide with a leftover
    /// from an earlier test run.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fetchforge-test-{}-{}",
            tag,
            std::process::id()
        ))
    }

    #[test]
    fn list_formats_with_uses_stub_ytdlp() {
        let dir = unique_temp_dir("stub-list");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("info.json");
        std::fs::write(&json_path, serde_json::to_string(&youtube_json()).unwrap()).unwrap();
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            &format!(
                "#!/bin/sh\ncat \"{}\"\n",
                json_path.to_string_lossy().replace('\\', "\\\\")
            ),
            &format!(
                "[Console]::Out.Write([IO.File]::ReadAllText('{}'))\n",
                json_path.to_string_lossy().replace('\'', "''")
            ),
        );
        let info = list_formats_with(&stub, "https://www.youtube.com/watch?v=aqz-KE-bpKQ").unwrap();
        assert_eq!(info.formats.len(), 8);
        assert_eq!(info.default_format_id.as_deref(), Some("22"));
    }

    #[test]
    fn list_formats_with_maps_stub_error() {
        let dir = unique_temp_dir("stub-err");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            "#!/bin/sh\necho 'ERROR: [youtube] abc: Private video' >&2\nexit 1\n",
            "[Console]::Error.WriteLine('ERROR: [youtube] abc: Private video')\nexit 1\n",
        );
        let err = list_formats_with(&stub, "https://www.youtube.com/watch?v=abc").unwrap_err();
        assert!(err.contains("private"), "error: {err}");
    }

    #[test]
    fn find_embedded_media_urls_finds_manifests_first() {
        // A JS-player page: the manifest lives in a JS var, a direct
        // file in a <source>, plus traps (a mid-path extension, a
        // duplicate, an unquoted attribute, JSON escaping).
        let html = r##"<html><head><title>Movie Night</title></head><body>
<script>var player = { "file": "https:\/\/cdn.example.com\/v\/master.m3u8?token=abc&amp;x=1", "poster": "/img/poster.jpg" };</script>
<video controls><source src="/videos/trailer.mp4" type="video/mp4"></video>
<video src=/videos/preview.mp4></video>
<a href="https://cdn.example.com/v/master.m3u8?token=abc&x=1">again</a>
<img src="/thumbs/video.mp4/frame.jpg">
</body></html>"##;
        let urls = find_embedded_media_urls(html, "https://watch.example.com/watch/m/1");
        // Manifest first, then direct files in document order; the
        // duplicate manifest and the mid-path .mp4 are dropped.
        assert_eq!(
            urls,
            vec![
                "https://cdn.example.com/v/master.m3u8?token=abc&x=1".to_string(),
                "https://watch.example.com/videos/trailer.mp4".to_string(),
                "https://watch.example.com/videos/preview.mp4".to_string(),
            ],
            "urls: {urls:?}"
        );
    }

    #[test]
    fn find_embedded_media_urls_empty_and_relative() {
        // A page with no media at all finds nothing.
        let bare = "<html><body><p>No video here.</p></body></html>";
        assert!(find_embedded_media_urls(bare, "https://example.com/p").is_empty());
        // Relative and protocol-relative URLs resolve against the page.
        let html = r##"<script>var s = "play/list.m3u8"; var t = "//cdn.example.com/x/manifest.mpd";</script>"##;
        let urls = find_embedded_media_urls(html, "https://watch.example.com/a/b/page");
        assert_eq!(
            urls,
            vec![
                "https://watch.example.com/a/b/play/list.m3u8".to_string(),
                "https://cdn.example.com/x/manifest.mpd".to_string(),
            ],
            "urls: {urls:?}"
        );
    }

    #[test]
    fn find_embedded_media_urls_rejects_bad_schemes_and_credentials() {
        let html = r##"<a href="ftp://example.com/v/movie.mp4">ftp</a>
<a href="https://user:secret@example.com/v/movie.mp4">creds</a>
<a href="https://example.com/v/ok.mp4">ok</a>"##;
        let urls = find_embedded_media_urls(html, "https://example.com/p");
        assert_eq!(urls, vec!["https://example.com/v/ok.mp4".to_string()]);
    }

    #[test]
    fn list_formats_with_falls_back_to_embedded_manifest() {
        // yt-dlp has no extractor for the page ("Unsupported URL"),
        // but the page HTML embeds its HLS manifest: the fallback
        // must list the stream instead of failing.
        let dir = unique_temp_dir("stub-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            "#!/bin/sh\necho 'ERROR: Unsupported URL: https://novahd.cc/watch/m/1' >&2\nexit 1\n",
            "[Console]::Error.WriteLine('ERROR: Unsupported URL: https://novahd.cc/watch/m/1')\nexit 1\n",
        );
        let mut files = std::collections::HashMap::new();
        files.insert(
            "/watch".to_string(),
            b"<html><body><script>var p={\"file\":\"/stream/master.m3u8\"};</script></body></html>"
                .to_vec(),
        );
        files.insert(
            "/stream/master.m3u8".to_string(),
            TEST_MASTER.as_bytes().to_vec(),
        );
        files.insert(
            "/stream/low.m3u8".to_string(),
            TEST_MEDIA.as_bytes().to_vec(),
        );
        files.insert(
            "/stream/mid.m3u8".to_string(),
            TEST_MEDIA.as_bytes().to_vec(),
        );
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_files(files, stop.clone());
        let page = format!("http://127.0.0.1:{port}/watch");
        let info = list_formats_with(&stub, &page).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(info.extractor, "HLS");
        assert_eq!(info.formats.len(), 2);
        assert_eq!(
            info.resolved_url.as_deref(),
            Some(format!("http://127.0.0.1:{port}/stream/master.m3u8").as_str()),
            "resolved_url: {:?}",
            info.resolved_url
        );
    }

    #[test]
    fn list_formats_with_no_embedded_media_keeps_friendly_error() {
        // Same "Unsupported URL", but the page embeds nothing: the
        // original friendly "not supported" error is returned, not
        // a fallback error.
        let dir = unique_temp_dir("stub-fallback-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            "#!/bin/sh\necho 'ERROR: Unsupported URL: https://example.com/x' >&2\nexit 1\n",
            "[Console]::Error.WriteLine('ERROR: Unsupported URL: https://example.com/x')\nexit 1\n",
        );
        let mut files = std::collections::HashMap::new();
        files.insert(
            "/watch".to_string(),
            b"<html><body><p>Just text, no player.</p></body></html>".to_vec(),
        );
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_files(files, stop.clone());
        let page = format!("http://127.0.0.1:{port}/watch");
        let err = list_formats_with(&stub, &page).unwrap_err();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert!(err.contains("not a supported video"), "error: {err}");
    }

    #[test]
    fn plan_from_info_routes_resolved_manifest_to_stream_engine() {
        // A plan from fallback-extracted HLS info: the worker must
        // download the manifest (effective_url), via the stream
        // engine (stream_kind set, empty yt-dlp selector).
        use crate::media::{StreamFormat, StreamInfo, StreamKind};
        let stream = StreamInfo {
            kind: StreamKind::Hls,
            title: "clip".to_string(),
            duration_secs: Some(120),
            manifest_url: "https://cdn.example.com/clip/master.m3u8".to_string(),
            formats: vec![StreamFormat {
                id: "https://cdn.example.com/clip/mid.m3u8".to_string(),
                ext: "ts".to_string(),
                is_video: true,
                is_audio: true,
                height: Some(720),
                width: Some(1280),
                abr_kbps: None,
                vbr_kbps: Some(2800.0),
                fps: Some(30.0),
                filesize: None,
                needs_merge: false,
            }],
        };
        let mut info = stream_info_to_media_info(&stream);
        info.resolved_url = Some("https://cdn.example.com/clip/master.m3u8".to_string());
        let plan = plan_from_info(&info, None, None).unwrap();
        assert_eq!(
            plan.effective_url.as_deref(),
            Some("https://cdn.example.com/clip/master.m3u8")
        );
        assert_eq!(plan.stream_kind, Some(StreamKind::Hls));
        assert_eq!(plan.selector, "");
        assert_eq!(plan.format_id, "https://cdn.example.com/clip/mid.m3u8");
        // A resolved direct file keeps its yt-dlp selector and no
        // stream kind.
        let mut yt = parse_media_info(&youtube_json(), "https://example.com").unwrap();
        yt.resolved_url = Some("https://cdn.example.com/v/movie.mp4".to_string());
        let plan = plan_from_info(&yt, Some("22"), None).unwrap();
        assert_eq!(
            plan.effective_url.as_deref(),
            Some("https://cdn.example.com/v/movie.mp4")
        );
        assert_eq!(plan.stream_kind, None);
        assert_eq!(plan.selector, "22");
        // Normal extraction: no effective URL.
        let plain = parse_media_info(&youtube_json(), "https://example.com").unwrap();
        let plan = plan_from_info(&plain, Some("22"), None).unwrap();
        assert_eq!(plan.effective_url, None);
        assert_eq!(plan.stream_kind, None);
    }

    #[test]
    fn download_media_with_stub_reports_progress_and_final_path() {
        let dir = unique_temp_dir("stub-dl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            r#"#!/bin/sh
prev=""
template=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then template="$a"; fi
  prev="$a"
done
out=$(printf '%s' "$template" | sed 's/%(ext)s/mp4/')
echo "[download]  50.0% of  10.00KiB at  100.00KiB/s ETA 00:00"
echo "[download] 100.0% of  10.00KiB at  100.00KiB/s ETA 00:00"
echo "[download] 100% of  10.00KiB in 00:00:01 at 100.00KiB/s"
printf 'FAKE-VIDEO-DATA' > "$out"
echo "$out"
exit 0
"#,
            r#"$template = $null
$prev = $null
foreach ($a in $args) {
  if ($prev -eq '-o') { $template = $a }
  $prev = $a
}
$out = $template.Replace('%(ext)s', 'mp4')
Write-Output '[download]  50.0% of  10.00KiB at  100.00KiB/s ETA 00:00'
Write-Output '[download] 100.0% of  10.00KiB at  100.00KiB/s ETA 00:00'
Write-Output '[download] 100% of  10.00KiB in 00:00:01 at 100.00KiB/s'
[IO.File]::WriteAllText($out, 'FAKE-VIDEO-DATA')
Write-Output $out
exit 0
"#,
        );
        let opts = MediaDownloadOptions {
            url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_string(),
            selector: "22".to_string(),
            needs_merge: false,
            destination_dir: dir.clone(),
            filename_stem: "Big Buck Bunny".to_string(),
            expected_ext: "mp4".to_string(),
            expected_total: Some(10 * 1024),
            organise_by_category: true,
            referer: None,
        };
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: Progress| {
            seen.lock().unwrap().push(p.downloaded);
        };
        let final_path =
            download_media_with(&stub, None, &opts, &progress, &paused, &cancelled).unwrap();
        assert!(final_path.is_file(), "final path missing: {}", final_path.display());
        assert_eq!(
            std::fs::read_to_string(&final_path).unwrap(),
            "FAKE-VIDEO-DATA"
        );
        // Category folder: Video.
        assert!(
            final_path.to_string_lossy().contains("Video"),
            "path: {}",
            final_path.display()
        );
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "no progress reported");
        assert_eq!(*seen.last().unwrap(), 10 * 1024);
    }

    #[test]
    fn download_media_with_cancel_kills_stub() {
        let dir = unique_temp_dir("stub-cancel");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Stub that would sleep 60 s if not killed.
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            "#!/bin/sh\nsleep 60\nexit 0\n",
            "Start-Sleep -Seconds 60\nexit 0\n",
        );
        let opts = MediaDownloadOptions {
            url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_string(),
            selector: "22".to_string(),
            needs_merge: false,
            destination_dir: dir.clone(),
            filename_stem: "Slow Video".to_string(),
            expected_ext: "mp4".to_string(),
            expected_total: None,
            organise_by_category: false,
            referer: None,
        };
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let cancelled_flag = &cancelled;
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                cancelled_flag.store(true, Ordering::Relaxed);
            });
            let progress = |_p: Progress| {};
            let start = Instant::now();
            let err = download_media_with(&stub, None, &opts, &progress, &paused, &cancelled)
                .unwrap_err();
            assert_eq!(err, "__CANCELLED__");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "cancel took {:?}; the stub was not killed",
                start.elapsed()
            );
        });
    }

    #[test]
    fn download_media_with_stall_kills_silent_stub() {
        // Regression (2026-11-22, nime.pro): a yt-dlp download
        // that prints nothing (stuck retrying a dead fragment)
        // used to sit at 0% with no error indefinitely. The stall
        // watchdog kills it with a clear error instead.
        let dir = unique_temp_dir("stub-stall");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Stub that would sleep 60 s silently if not killed.
        let stub = write_stub_ytdlp(
            &dir,
            "fake-yt-dlp",
            "#!/bin/sh\nsleep 60\nexit 0\n",
            "Start-Sleep -Seconds 60\nexit 0\n",
        );
        let opts = MediaDownloadOptions {
            url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_string(),
            selector: "22".to_string(),
            needs_merge: false,
            destination_dir: dir.clone(),
            filename_stem: "Stalled Video".to_string(),
            expected_ext: "mp4".to_string(),
            expected_total: None,
            organise_by_category: false,
            referer: None,
        };
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let progress = |_p: Progress| {};
        let start = Instant::now();
        let err = download_media_with_stall(
            &stub,
            None,
            &opts,
            &progress,
            &paused,
            &cancelled,
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(err.contains("stalled"), "error: {err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "stall watchdog took {:?}; the stub was not killed",
            start.elapsed()
        );
    }

    #[test]
    fn merge_without_ffmpeg_is_a_clear_error() {
        let dir = std::env::temp_dir().join("fetchforge-test-no-ffmpeg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stub = write_stub_ytdlp(&dir, "fake-yt-dlp", "#!/bin/sh\nexit 0\n", "exit 0\n");
        let opts = MediaDownloadOptions {
            url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_string(),
            selector: "137".to_string(),
            needs_merge: true,
            destination_dir: dir.clone(),
            filename_stem: "Video".to_string(),
            expected_ext: "mp4".to_string(),
            expected_total: None,
            organise_by_category: false,
            referer: None,
        };
        let paused = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let progress = |_p: Progress| {};
        let err = download_media_with(&stub, None, &opts, &progress, &paused, &cancelled)
            .unwrap_err();
        assert!(err.contains("ffmpeg"), "error: {err}");
    }

    #[test]
    fn stream_info_converts_to_media_info_with_default() {
        use crate::media::{StreamFormat, StreamInfo, StreamKind};
        let info = StreamInfo {
            kind: StreamKind::Hls,
            title: "clip".to_string(),
            duration_secs: Some(120),
            manifest_url: "https://example.com/clip/master.m3u8".to_string(),
            formats: vec![
                StreamFormat {
                    id: "https://example.com/clip/low.m3u8".to_string(),
                    ext: "ts".to_string(),
                    is_video: true,
                    is_audio: true,
                    height: Some(360),
                    width: Some(640),
                    abr_kbps: None,
                    vbr_kbps: Some(800.0),
                    fps: None,
                    filesize: None,
                    needs_merge: false,
                },
                StreamFormat {
                    id: "https://example.com/clip/high.m3u8".to_string(),
                    ext: "mp4".to_string(),
                    is_video: true,
                    is_audio: false,
                    height: Some(1080),
                    width: Some(1920),
                    abr_kbps: None,
                    vbr_kbps: Some(6000.0),
                    fps: Some(60.0),
                    filesize: None,
                    needs_merge: true,
                },
                StreamFormat {
                    id: "https://example.com/clip/mid.m3u8".to_string(),
                    ext: "ts".to_string(),
                    is_video: true,
                    is_audio: true,
                    height: Some(720),
                    width: Some(1280),
                    abr_kbps: None,
                    vbr_kbps: Some(2800.0),
                    fps: Some(30.0),
                    filesize: None,
                    needs_merge: false,
                },
            ],
        };
        let media = stream_info_to_media_info(&info);
        assert_eq!(media.extractor, "HLS");
        assert_eq!(media.duration_secs, Some(120));
        // Best-first: combined formats before the video-only one.
        assert_eq!(media.formats[0].height, Some(720));
        assert_eq!(media.formats[0].kind, FormatKind::Combined);
        assert_eq!(media.formats[2].kind, FormatKind::VideoOnly);
        assert!(media.formats[2].needs_merge);
        // Default: best combined at or below 1080p (720p), not the
        // 1080p video-only format (a merge is never the default
        // when a combined format exists).
        assert_eq!(
            media.default_format_id.as_deref(),
            Some("https://example.com/clip/mid.m3u8")
        );
        assert!(media.formats[0].label.contains("720p"), "label: {}", media.formats[0].label);
    }

    /// Loopback server for stream-routing tests: serves `files` by
    /// path until `stop` is set. Returns the port and the thread.
    fn serve_files(
        files: std::collections::HashMap<String, Vec<u8>>,
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

    const TEST_MASTER: &str = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
        low.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\n\
        mid.m3u8\n";

    const TEST_MEDIA: &str = "#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn list_formats_routes_manifests_without_ytdlp() {
        // The data dir is empty (no yt-dlp anywhere): if the
        // manifest routed to yt-dlp, this would try a 3 MB GitHub
        // download and fail. Routing to media.rs succeeds.
        let dir = std::env::temp_dir().join("fetchforge-test-hls-noytdlp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = std::collections::HashMap::new();
        files.insert("/master.m3u8".to_string(), TEST_MASTER.as_bytes().to_vec());
        files.insert("/low.m3u8".to_string(), TEST_MEDIA.as_bytes().to_vec());
        files.insert("/mid.m3u8".to_string(), TEST_MEDIA.as_bytes().to_vec());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_files(files, stop.clone());
        let info = list_formats(&dir, &format!("http://127.0.0.1:{port}/master.m3u8")).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        assert_eq!(info.extractor, "HLS");
        assert_eq!(info.formats.len(), 2);
        assert_eq!(info.duration_secs, Some(10));
    }

    #[test]
    fn plan_stream_download_picks_default_and_explicit_format() {
        let mut files = std::collections::HashMap::new();
        files.insert("/master.m3u8".to_string(), TEST_MASTER.as_bytes().to_vec());
        files.insert("/low.m3u8".to_string(), TEST_MEDIA.as_bytes().to_vec());
        files.insert("/mid.m3u8".to_string(), TEST_MEDIA.as_bytes().to_vec());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_files(files, stop.clone());
        let url = format!("http://127.0.0.1:{port}/master.m3u8");
        let dir = std::env::temp_dir().join("fetchforge-test-hls-plan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Default: best combined <= 1080p (720p mid), .ts, no merge,
        // tagged as an HLS plan with an empty yt-dlp selector.
        let plan = plan_media_download(&dir, &url, None, None).unwrap();
        assert_eq!(plan.format_id, format!("http://127.0.0.1:{port}/mid.m3u8"));
        assert!(plan.filename.ends_with(".ts"), "filename: {}", plan.filename);
        assert!(!plan.needs_merge);
        assert_eq!(plan.stream_kind, Some(crate::media::StreamKind::Hls));
        assert_eq!(plan.selector, "");
        // Explicit choice: the 360p variant.
        let plan = plan_media_download(
            &dir,
            &url,
            Some(&format!("http://127.0.0.1:{port}/low.m3u8")),
            None,
        )
        .unwrap();
        assert_eq!(plan.format_id, format!("http://127.0.0.1:{port}/low.m3u8"));
        // Unknown format id is a clear error.
        assert!(plan_media_download(&dir, &url, Some("nope"), None).is_err());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
    }

    #[test]
    fn plan_stream_download_dash_picks_default_video_with_merge() {
        // DASH manifest with separate audio: default is the best
        // video at or below 1080p (720p), needs_merge, .mp4.
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT8S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate duration="120" timescale="30" media="$RepresentationID$/$RepresentationID$_$Number$.m4v" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4v"/>
   <Representation id="v360" codecs="avc1.64001e" bandwidth="1000000" width="640" height="360" frameRate="30"/>
   <Representation id="v720" codecs="avc1.64001f" bandwidth="4000000" width="1280" height="720" frameRate="30"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <SegmentTemplate duration="192512" timescale="48000" media="$RepresentationID$/$RepresentationID$_$Number$.m4a" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4a"/>
   <Representation id="a64k" codecs="mp4a.40.5" bandwidth="67071"/>
  </AdaptationSet>
 </Period></MPD>"#;
        let mut files = std::collections::HashMap::new();
        files.insert("/bbb.mpd".to_string(), mpd.as_bytes().to_vec());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (port, server) = serve_files(files, stop.clone());
        let url = format!("http://127.0.0.1:{port}/bbb.mpd");
        let dir = std::env::temp_dir().join("fetchforge-test-dash-plan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = list_formats(&dir, &url).unwrap();
        assert_eq!(info.extractor, "DASH");
        assert_eq!(info.formats.len(), 3);
        let plan = plan_media_download(&dir, &url, None, None).unwrap();
        assert_eq!(plan.format_id, "v720");
        assert!(plan.filename.ends_with(".mp4"), "filename: {}", plan.filename);
        assert!(plan.needs_merge);
        assert_eq!(plan.stream_kind, Some(crate::media::StreamKind::Dash));
        assert_eq!(plan.selector, "");
        // Audio-only choice: .m4a, no merge.
        let plan = plan_media_download(&dir, &url, Some("a64k"), None).unwrap();
        assert_eq!(plan.format_id, "a64k");
        assert!(plan.filename.ends_with(".m4a"), "filename: {}", plan.filename);
        assert!(!plan.needs_merge);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
    }
}
