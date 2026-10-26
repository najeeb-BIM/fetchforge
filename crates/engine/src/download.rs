use crate::filename::{
    filename_from_content_disposition, filename_from_url, sanitize_filename, unique_path,
};
use crate::validate::validate_url;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, RANGE};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Probe {
    pub final_url: String,
    pub content_length: Option<u64>,
    pub accept_ranges: bool,
    pub filename: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    pub destination_dir: PathBuf,
    /// Explicit filename; when None it is derived from headers, then URL.
    pub filename: Option<String>,
    pub max_segments: usize,
    pub max_retries: u32,
    /// Kilobits per second, None = unlimited. Enforced per download.
    pub bandwidth_kbps: Option<u64>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    /// If true, place the file in `<dir>/<Category>/` (see category.rs).
    pub organise_by_category: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            destination_dir: PathBuf::from("."),
            filename: None,
            max_segments: 4,
            max_retries: 3,
            bandwidth_kbps: None,
            referer: None,
            user_agent: None,
            organise_by_category: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes per second, smoothed over the current run.
    pub bytes_per_sec: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Paused,
    Cancelled,
}

fn client(opts: &DownloadOptions) -> Result<Client, String> {
    Client::builder()
        .user_agent(
            opts.user_agent
                .clone()
                .unwrap_or_else(|| "FetchForge/0.1".to_string()),
        )
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// HEAD probe (falls back to a ranged GET when HEAD is refused).
/// Never claims resumability: `accept_ranges` reflects the server header.
pub fn probe(opts: &DownloadOptions) -> Result<Probe, String> {
    let url = validate_url(&opts.url)?;
    let c = client(opts)?;
    let mut req = c.head(&url);
    if let Some(r) = &opts.referer {
        req = req.header("Referer", r.clone());
    }
    match req.send() {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() || status.as_u16() == 206 {
                return Ok(probe_from_response(resp));
            }
            // Some servers reject HEAD; fall through to a ranged GET probe.
            if !(status.as_u16() == 405 || status.as_u16() == 403) {
                return Err(http_error(status.as_u16()));
            }
        }
        Err(_) => { /* fall through to ranged GET */ }
    }
    let mut req = c.get(&url).header(RANGE, "bytes=0-0");
    if let Some(r) = &opts.referer {
        req = req.header("Referer", r.clone());
    }
    let resp = req.send().map_err(|e| friendly_net_error(&e))?;
    if !resp.status().is_success() {
        return Err(http_error(resp.status().as_u16()));
    }
    Ok(probe_from_response(resp))
}

fn probe_from_response(resp: reqwest::blocking::Response) -> Probe {
    let final_url = resp.url().to_string();
    let length = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let ranges = resp
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);
    let disp = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok());
    let filename =
        filename_from_content_disposition(disp).or_else(|| filename_from_url(&final_url));
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Probe {
        final_url,
        content_length: length,
        accept_ranges: ranges,
        filename,
        content_type,
    }
}

fn http_error(code: u16) -> String {
    match code {
        400 => "Server rejected the request (400)".to_string(),
        401 | 403 => "Access denied by the server (401/403). The link may need sign-in or may have expired.".to_string(),
        404 => "File not found on the server (404)".to_string(),
        416 => "Requested range not satisfiable (416); the file may have changed. Retry from the start.".to_string(),
        429 => "Server rate-limited the request (429). Retry later.".to_string(),
        500..=599 => format!("Server error ({code}). Retry later."),
        _ => format!("HTTP error {code}"),
    }
}

fn friendly_net_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Connection timed out".to_string()
    } else if e.is_connect() {
        "Could not connect to the server. Check the link and your connection.".to_string()
    } else {
        format!("Network error: {e}")
    }
}

/// Throttle helper: sleep so average throughput stays under `kbps`.
struct Limiter {
    kbps: Option<u64>,
    window_start: Instant,
    window_bytes: u64,
}
impl Limiter {
    fn new(kbps: Option<u64>) -> Self {
        Self {
            kbps,
            window_start: Instant::now(),
            window_bytes: 0,
        }
    }
    fn account(&mut self, n: usize) {
        let Some(kbps) = self.kbps else { return };
        if kbps == 0 {
            return;
        }
        self.window_bytes += n as u64;
        let budget = kbps * 1024 / 8; // bytes per second
        let elapsed = self.window_start.elapsed().as_secs_f64();
        let expected = budget as f64 * elapsed;
        if self.window_bytes as f64 > expected {
            let over = (self.window_bytes as f64 - expected) / budget as f64;
            if over > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(over.min(1.0)));
            }
        }
        if elapsed >= 1.0 {
            self.window_start = Instant::now();
            self.window_bytes = 0;
        }
    }
}

fn check_stop(paused: &AtomicBool, cancelled: &AtomicBool) -> Result<(), StopReason> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(StopReason::Cancelled);
    }
    if paused.load(Ordering::Relaxed) {
        return Err(StopReason::Paused);
    }
    Ok(())
}

/// Download to completion (or pause/cancel). Writes `<final>.part` and
/// renames atomically on success. Resume: if a `.part` file exists and the
/// server supports ranges, downloading continues from its size; otherwise
/// the partial file is discarded and the download restarts, and the caller
/// is never told a non-resumable download was resumed.
///
/// Returns the final path on success, or `Err((message, StopReason))` on
/// pause/cancel so the caller can persist state instead of marking failure.
pub fn download(
    opts: &DownloadOptions,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let probe = probe(opts)?;
    let filename = opts
        .filename
        .clone()
        .map(|f| sanitize_filename(&f))
        .or_else(|| probe.filename.clone().map(|f| sanitize_filename(&f)))
        .unwrap_or_else(|| "download".to_string());
    let mut dir = opts.destination_dir.clone();
    if opts.organise_by_category {
        dir = dir.join(crate::category_for_filename(&filename));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create download folder: {e}"))?;
    let final_path = unique_path(&dir, &filename);
    // If a previous run already produced this exact final name, unique_path
    // moved us on; the .part file belongs to that new name.
    let part_path = final_path.with_extension(format!(
        "{}.part",
        final_path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let part_path = if final_path.extension().is_some() {
        part_path
    } else {
        final_path.with_file_name(format!(
            "{}.part",
            final_path.file_name().unwrap().to_string_lossy()
        ))
    };

    let total = probe.content_length;
    let existing = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
    let resumable = probe.accept_ranges && total.is_some_and(|t| existing > 0 && existing < t);

    // Segmented path: only for a FRESH download, when the server really
    // supports ranges and the size is large enough to be worth splitting.
    // A partial file is never resumed through the segmented path, because
    // segment threads write out of order and a pre-allocated part file
    // cannot be trusted as a contiguous prefix. Partial single-stream
    // downloads resume via Range (below); partial segmented downloads are
    // discarded and restarted cleanly, never resumed into a corrupt file.
    let use_segments = probe.accept_ranges
        && total.is_some_and(|t| t >= 1_048_576)
        && opts.max_segments > 1
        && existing == 0;
    if use_segments {
        let total = total.unwrap();
        return download_segmented(
            opts,
            &probe,
            &final_path,
            &part_path,
            total,
            progress,
            paused,
            cancelled,
        );
    }

    if existing > 0 && !resumable {
        // Stale partial from a non-resumable server: restart honestly.
        let _ = std::fs::remove_file(&part_path);
    }
    let start_from = if resumable { existing } else { 0 };
    download_single(
        opts,
        &probe,
        &final_path,
        &part_path,
        start_from,
        progress,
        paused,
        cancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn download_single(
    opts: &DownloadOptions,
    probe: &Probe,
    final_path: &Path,
    part_path: &Path,
    start_from: u64,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let mut last_err = String::new();
    for attempt in 0..=opts.max_retries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1 << attempt.min(4)));
        }
        match download_single_attempt(
            opts, probe, final_path, part_path, start_from, progress, paused, cancelled,
        ) {
            Ok(p) => return Ok(p),
            Err(StopOrError::Stop(reason)) => {
                return Err(match reason {
                    StopReason::Paused => "__PAUSED__".to_string(),
                    StopReason::Cancelled => "__CANCELLED__".to_string(),
                });
            }
            Err(StopOrError::Error(e)) => last_err = e,
        }
    }
    Err(last_err)
}

enum StopOrError {
    Stop(StopReason),
    Error(String),
}

#[allow(clippy::too_many_arguments)]
fn download_single_attempt(
    opts: &DownloadOptions,
    probe: &Probe,
    final_path: &Path,
    part_path: &Path,
    mut downloaded: u64,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, StopOrError> {
    let c = client(opts).map_err(StopOrError::Error)?;
    let mut req = c.get(&probe.final_url);
    if downloaded > 0 {
        req = req.header(RANGE, format!("bytes={downloaded}-"));
    }
    if let Some(r) = &opts.referer {
        req = req.header("Referer", r.clone());
    }
    let mut resp = req
        .send()
        .map_err(|e| StopOrError::Error(friendly_net_error(&e)))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(StopOrError::Error(http_error(status)));
    }
    if downloaded > 0 && status != 206 {
        // Server ignored Range: restart from zero rather than corrupting.
        let _ = std::fs::remove_file(part_path);
        return download_single_attempt(
            opts, probe, final_path, part_path, 0, progress, paused, cancelled,
        );
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(downloaded == 0)
        .open(part_path)
        .map_err(|e| StopOrError::Error(format!("Cannot write download file: {e}")))?;
    if downloaded > 0 {
        file.seek(SeekFrom::Start(downloaded))
            .map_err(|e| StopOrError::Error(e.to_string()))?;
    } else {
        // Fresh start: truncate any stale partial.
        file.set_len(0).ok();
    }
    let total = probe.content_length;
    let started = Instant::now();
    let mut limiter = Limiter::new(opts.bandwidth_kbps);
    let mut buf = [0u8; 64 * 1024];
    loop {
        check_stop(paused, cancelled).map_err(StopOrError::Stop)?;
        // Blocking Response implements Read; there is no chunk() on it.
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                limiter.account(n);
                file.write_all(&buf[..n])
                    .map_err(|e| StopOrError::Error(format!("Write failed: {e}")))?;
                downloaded += n as u64;
                let secs = started.elapsed().as_secs_f64().max(0.001);
                progress(Progress {
                    downloaded,
                    total,
                    bytes_per_sec: downloaded as f64 / secs,
                });
            }
            Err(e) => return Err(StopOrError::Error(format!("Read failed: {e}"))),
        }
    }
    drop(file);
    if let Some(t) = total {
        let actual = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
        if actual != t {
            return Err(StopOrError::Error(format!(
                "Incomplete download: got {actual} bytes, expected {t}"
            )));
        }
    }
    std::fs::rename(part_path, final_path)
        .map_err(|e| StopOrError::Error(format!("Could not finish download: {e}")))?;
    Ok(final_path.to_path_buf())
}

#[allow(clippy::too_many_arguments)]
fn download_segmented(
    opts: &DownloadOptions,
    probe: &Probe,
    final_path: &Path,
    part_path: &Path,
    total: u64,
    progress: &dyn Fn(Progress),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    // Pre-allocate the part file so segment threads can seek safely.
    {
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(part_path)
            .map_err(|e| format!("Cannot write download file: {e}"))?;
        f.set_len(total).map_err(|e| e.to_string())?;
    }
    let segments = opts.max_segments.clamp(2, 8);
    let chunk_size = total.div_ceil(segments as u64);
    let downloaded = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    // Scoped threads: &AtomicBool is Send because AtomicBool is Sync, so
    // pause/cancel flags set by the caller are visible to every segment.
    let mut first_error: Option<String> = None;
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for i in 0..segments {
            let start = i as u64 * chunk_size;
            let end = ((i as u64 + 1) * chunk_size - 1).min(total - 1);
            if start > end {
                continue;
            }
            let opts_c = opts.clone();
            let url = probe.final_url.clone();
            let part = part_path.to_path_buf();
            let dl = Arc::clone(&downloaded);
            handles.push(scope.spawn(move || {
                segment_worker(&opts_c, &url, &part, start, end, &dl, paused, cancelled)
            }));
        }
        for h in handles {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some("A download segment thread failed".to_string());
                    }
                }
            }
        }
    });
    if cancelled.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(part_path);
        return Err("__CANCELLED__".to_string());
    }
    if paused.load(Ordering::Relaxed) {
        return Err("__PAUSED__".to_string());
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    let actual = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
    if actual != total {
        return Err(format!(
            "Incomplete download: got {actual} bytes, expected {total}"
        ));
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    progress(Progress {
        downloaded: total,
        total: Some(total),
        bytes_per_sec: total as f64 / secs,
    });
    std::fs::rename(part_path, final_path)
        .map_err(|e| format!("Could not finish download: {e}"))?;
    Ok(final_path.to_path_buf())
}

#[allow(clippy::too_many_arguments)]
fn segment_worker(
    opts: &DownloadOptions,
    url: &str,
    part_path: &Path,
    start: u64,
    end: u64,
    downloaded: &AtomicU64,
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let c = client(opts)?;
    let mut req = c.get(url).header(RANGE, format!("bytes={start}-{end}"));
    if let Some(r) = &opts.referer {
        req = req.header("Referer", r.clone());
    }
    let mut resp = req.send().map_err(|e| friendly_net_error(&e))?;
    if resp.status().as_u16() != 206 {
        return Err(format!(
            "Server did not honour range request (HTTP {})",
            resp.status()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .open(part_path)
        .map_err(|e| format!("Cannot write segment: {e}"))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut limiter = Limiter::new(opts.bandwidth_kbps.map(|k| k / 4).filter(|k| *k > 0));
    let mut buf = [0u8; 64 * 1024];
    loop {
        check_stop(paused, cancelled).map_err(|r| match r {
            StopReason::Paused => "__PAUSED__".to_string(),
            StopReason::Cancelled => "__CANCELLED__".to_string(),
        })?;
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                limiter.account(n);
                file.write_all(&buf[..n])
                    .map_err(|e| format!("Write failed: {e}"))?;
                downloaded.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) => return Err(format!("Read failed: {e}")),
        }
    }
    Ok(())
}

/// Decode a `__PAUSED__` / `__CANCELLED__` sentinel into a StopReason.
pub fn stop_reason(err: &str) -> Option<StopReason> {
    match err {
        "__PAUSED__" => Some(StopReason::Paused),
        "__CANCELLED__" => Some(StopReason::Cancelled),
        _ => None,
    }
}
