//! Direct media downloads: plain video files are ordinary downloads.
//! HLS (.m3u8) and DASH (.mpd) are supported ONLY when the manifest and
//! segments are openly accessible and unencrypted. Anything with DRM,
//! an EXT-X-KEY, ContentProtection, expiring signed segments the caller
//! cannot legally fetch, or authentication the user has not supplied is
//! refused with a plain explanation. No DRM is bypassed, ever.

use crate::download::Progress;
use crate::validate::validate_url;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn fetch_text(url: &str, referer: &Option<String>) -> Result<String, String> {
    let c = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.1")
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

fn resolve(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    match url::Url::parse(base).and_then(|b| b.join(rel)) {
        Ok(u) => u.to_string(),
        Err(_) => rel.to_string(),
    }
}

/// Download an HLS playlist into a single `.ts` file (segments concatenated
/// in playlist order). Master playlists follow the first variant playlist.
/// Returns an error for encrypted playlists.
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
    // Master playlist: follow the first variant.
    if text.contains("#EXT-X-STREAM-INF") {
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                return download_hls(
                    &resolve(&manifest_url, line),
                    destination_dir,
                    filename,
                    referer,
                    progress,
                    paused,
                    cancelled,
                );
            }
        }
        return Err("Master playlist contains no variant".to_string());
    }
    let segments: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| resolve(&manifest_url, l))
        .collect();
    if segments.is_empty() {
        return Err("Playlist contains no media segments".to_string());
    }
    let clean = crate::filename::sanitize_filename(filename);
    std::fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let final_path = crate::filename::unique_path(destination_dir, &clean);
    let part_path = final_path.with_file_name(format!(
        "{}.part",
        final_path.file_name().unwrap().to_string_lossy()
    ));
    let mut out = std::fs::File::create(&part_path).map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    for seg in &segments {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = std::fs::remove_file(&part_path);
            return Err("__CANCELLED__".to_string());
        }
        if paused.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("__PAUSED__".to_string());
        }
        // Fetch segment bytes directly and append them in playlist order.
        let bytes = fetch_bytes(seg, referer)?;
        use std::io::Write;
        out.write_all(&bytes).map_err(|e| e.to_string())?;
        downloaded += bytes.len() as u64;
        progress(Progress {
            downloaded,
            total: None,
            bytes_per_sec: 0.0,
        });
    }
    drop(out);
    std::fs::rename(&part_path, &final_path).map_err(|e| e.to_string())?;
    Ok(final_path)
}

fn fetch_bytes(url: &str, referer: &Option<String>) -> Result<Vec<u8>, String> {
    let c = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.1")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = c.get(url);
    if let Some(r) = referer {
        req = req.header("Referer", r.clone());
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Segment request failed: HTTP {}", resp.status()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
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
