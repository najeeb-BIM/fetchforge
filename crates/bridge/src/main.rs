//! FetchForge localhost bridge.
//!
//! * Binds ONLY to 127.0.0.1 (loopback). Never 0.0.0.0.
//! * Extension API requires `X-FetchForge-Token`, a per-install secret
//!   stored in the user's data dir and shown in the desktop app's Settings.
//!   The token is never logged and never leaves the machine.
//! * Rejects non-extension / non-loopback `Origin` headers.
//! * `native-host` speaks the Chrome/Firefox native-messaging framing
//!   (4-byte little-endian length + JSON) on stdin/stdout. Only the
//!   extension ID registered in the host manifest can invoke it.
//!   Chrome launches the host with NO arguments and a pipe on stdin,
//!   so a piped stdin with no mode argument also enters native-host
//!   mode (see main).
//!
//! Usage:
//!   fetchforge-bridge serve [--port 18443] [--data-dir DIR]
//!   fetchforge-bridge native-host [--data-dir DIR]
//!   fetchforge-bridge print-token [--data-dir DIR]

use fetchforge_engine::{
    category_for_filename, download, download_dash, download_gltf_bundle, download_hls_pair,
    download_hls_variant, download_media, ensure_ffmpeg, ensure_image_extension, fetch_gltf_plan,
    find_ytdlp, generate_download_id, generate_secure_token, is_gltf_url, is_image_content_type,
    list_formats, plan_media_download_or_drive_fallback, probe, sanitize_filename, stop_reason,
    CapturedManifest, DashDownloadOptions, DownloadOptions, DownloadRecord, GltfDownloadOptions,
    MediaDownloadOptions, PlanOutcome, Status, Store, MAX_CAPTURED_MANIFEST_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_PORT: u16 = 18443;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    download_dir: String,
    max_simultaneous: usize,
    max_segments: usize,
    max_retries: u32,
    bandwidth_kbps: Option<u64>,
    organise_by_category: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir().to_string_lossy().to_string(),
            max_simultaneous: 3,
            max_segments: 4,
            max_retries: 3,
            bandwidth_kbps: None,
            organise_by_category: true,
        }
    }
}

fn default_data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FETCHFORGE_DATA_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("FetchForge");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/share/fetchforge");
    }
    PathBuf::from(".fetchforge")
}

fn default_download_dir() -> PathBuf {
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        return PathBuf::from(home).join("Downloads");
    }
    PathBuf::from("Downloads")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// P05 (F11): per-field settings fallback. A settings file with one bad
/// field used to discard ALL settings silently (whole-file `from_str`
/// failure -> `Default`). Now each known field is read on its own: a
/// valid field keeps its value, a missing or wrongly-typed field falls
/// back to its default, and the bad field names are returned as a
/// warning so the user can see what was ignored. A missing file is
/// normal on first run (no warning); a file that is not JSON, or not a
/// JSON object, still falls back to all defaults, but with a warning.
fn load_settings_with_warning(data_dir: &std::path::Path) -> (Settings, Option<String>) {
    let defaults = Settings::default();
    let path = data_dir.join("settings.json");
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (defaults, None),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return (
                defaults,
                Some(
                    "settings.json is not valid JSON; all settings fell back to defaults"
                        .to_string(),
                ),
            )
        }
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            return (
                defaults,
                Some(
                    "settings.json is not a JSON object; all settings fell back to defaults"
                        .to_string(),
                ),
            )
        }
    };
    let mut bad: Vec<&str> = Vec::new();
    let mut s = defaults.clone();
    // download_dir: non-empty string, else default.
    match obj.get("download_dir") {
        None => {}
        Some(v) => match v.as_str() {
            Some(d) if !d.is_empty() => s.download_dir = d.to_string(),
            _ => bad.push("download_dir"),
        },
    }
    // Numeric fields: JSON number, else default.
    match obj.get("max_simultaneous") {
        None => {}
        Some(v) => match v.as_u64() {
            Some(n) => s.max_simultaneous = n as usize,
            None => bad.push("max_simultaneous"),
        },
    }
    match obj.get("max_segments") {
        None => {}
        Some(v) => match v.as_u64() {
            Some(n) => s.max_segments = n as usize,
            None => bad.push("max_segments"),
        },
    }
    match obj.get("max_retries") {
        None => {}
        Some(v) => match v.as_u64() {
            Some(n) => s.max_retries = n as u32,
            None => bad.push("max_retries"),
        },
    }
    // bandwidth_kbps: null = unlimited, number = limit, anything else = default.
    match obj.get("bandwidth_kbps") {
        None => {}
        Some(serde_json::Value::Null) => s.bandwidth_kbps = None,
        Some(v) => match v.as_u64() {
            Some(n) => s.bandwidth_kbps = Some(n),
            None => bad.push("bandwidth_kbps"),
        },
    }
    match obj.get("organise_by_category") {
        None => {}
        Some(v) => match v.as_bool() {
            Some(b) => s.organise_by_category = b,
            None => bad.push("organise_by_category"),
        },
    }
    let warning = if bad.is_empty() {
        None
    } else {
        Some(format!(
            "settings.json field(s) ignored (using defaults): {}",
            bad.join(", ")
        ))
    };
    (s, warning)
}

fn load_or_create_token(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("bridge-token.txt");
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let token = generate_token();
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&path, &token);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    token
}

struct Job {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

struct App {
    settings: Settings,
    data_dir: PathBuf,
    token: Mutex<String>,
    pairing: Mutex<fetchforge_engine::pairing::PairingState>,
    store: Mutex<Store>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    active: Mutex<usize>,
    /// Media the extension detected on pages (POST /media/detected),
    /// oldest first. Persisted to detected.json; a corrupt file
    /// starts empty with a warning, never a crash.
    detected: Mutex<Vec<DetectedItem>>,
}

impl App {
    fn new(data_dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&data_dir);
        let (settings, settings_warning) = load_settings_with_warning(&data_dir);
        if let Some(w) = &settings_warning {
            eprintln!("FetchForge WARNING: {w}");
        }
        let token = load_or_create_token(&data_dir);
        let store = Store::load(data_dir.join("downloads.json"));
        if let Some(w) = &store.load_warning {
            eprintln!("FetchForge WARNING: {w}");
        }
        // NOTE: App::new deliberately does NOT recover interrupted
        // downloads (see recover_interrupted): a short-lived
        // native-host probe constructs an App on every extension
        // button click, and recovery here would stomp every
        // in-progress download to Paused on disk.
        let detected = match std::fs::read_to_string(data_dir.join("detected.json")) {
            Ok(text) => match serde_json::from_str::<Vec<DetectedItem>>(&text) {
                Ok(items) => items.into_iter().take(MAX_DETECTED_ITEMS).collect(),
                Err(_) => {
                    eprintln!(
                        "FetchForge WARNING: detected.json is not valid JSON; detected media list started empty"
                    );
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Arc::new(Self {
            settings,
            data_dir,
            token: Mutex::new(token),
            pairing: Mutex::new(fetchforge_engine::pairing::PairingState::new()),
            store: Mutex::new(store),
            jobs: Mutex::new(HashMap::new()),
            active: Mutex::new(0),
            detected: Mutex::new(detected),
        })
    }

    /// Mark records left Downloading by a previous run as Paused
    /// ("Interrupted by app close or crash") and persist. Called
    /// ONLY by the process that actually becomes the server, after
    /// its TCP bind succeeds (see main). A short-lived native-host
    /// probe must never do this: every extension button click
    /// launches one, and before round 7 each click stomped every
    /// in-progress download to Paused on disk (proven live
    /// 2026-11-23: a nime download flipped Downloading→Paused
    /// with no process restart, while the download itself kept
    /// progressing).
    fn recover_interrupted(&self) {
        let mut store = self.store.lock().unwrap();
        store.recover_interrupted();
        let _ = store.save();
    }

    /// Store detected media items: dedupe by URL (a re-detection
    /// replaces the old entry with fresh metadata and moves it to
    /// the end), drop the oldest past the cap, persist best-effort.
    /// Returns how many items were accepted (empty URLs are skipped).
    fn store_detected(&self, items: Vec<DetectedItem>) -> usize {
        let now = now_unix();
        let mut accepted = 0usize;
        let mut list = self.detected.lock().unwrap();
        for mut item in items {
            if item.url.trim().is_empty() {
                continue;
            }
            if item.detected_unix.is_none() {
                item.detected_unix = Some(now);
            }
            if let Some(pos) = list.iter().position(|d| d.url == item.url) {
                list.remove(pos);
            }
            list.push(item);
            accepted += 1;
        }
        while list.len() > MAX_DETECTED_ITEMS {
            // Per-kind fairness: the detector floods with images,
            // which used to crowd every video/PDF out of the list
            // (the panel showed images only). Trim the oldest
            // IMAGE first; only drop a non-image when the list
            // holds no images at all.
            let pos = list
                .iter()
                .position(|d| d.kind.as_deref() == Some("image"))
                .unwrap_or(0);
            list.remove(pos);
        }
        drop(list);
        self.save_detected();
        accepted
    }

    /// Best-effort persistence of the detected list (tmp file +
    /// rename, like the store). A failed save is logged, never fatal:
    /// detections are a cache the extension rebuilds.
    fn save_detected(&self) {
        let list = self.detected.lock().unwrap();
        let text = match serde_json::to_string(&*list) {
            Ok(t) => t,
            Err(_) => return,
        };
        drop(list);
        let path = self.data_dir.join("detected.json");
        let tmp = self.data_dir.join("detected.json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Rotate the bridge token ("Unpair all browsers"): generate a new
    /// token, write it to the token file, replace the in-memory token,
    /// and clear any active pairing code. Old tokens stop working
    /// immediately; paired browsers must re-pair with a new code.
    /// The new token is NOT returned here — the app reads it from the
    /// token file, and extensions must re-pair, never receive it
    /// automatically.
    fn rotate_token(&self) -> Result<(), String> {
        let new_token = generate_token();
        let path = self.data_dir.join("bridge-token.txt");
        std::fs::write(&path, &new_token).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        *self.token.lock().unwrap() = new_token;
        *self.pairing.lock().unwrap() = fetchforge_engine::pairing::PairingState::new();
        Ok(())
    }

    fn add_download(
        self: &Arc<Self>,
        url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
    ) -> Result<DownloadRecord, String> {
        let clean_url = fetchforge_engine::validate_url(url)?;
        // HLS (.m3u8) / DASH (.mpd) manifests go straight to the
        // media path (manifest parsing + segment assembly in
        // media.rs): a manifest must never reach the direct path,
        // which would save the manifest text itself as a file.
        if fetchforge_engine::is_manifest_url(&clean_url) {
            return self.add_media_download(
                &clean_url,
                filename,
                format_id,
                referer,
                audio_url,
                manifest_text,
            );
        }
        // glTF models (.gltf) carry dependencies (buffers, textures)
        // that must be downloaded with the model and kept in the same
        // folder structure (spec §6). They go to the 3D engine, never
        // the direct path, which would save the .gltf alone and leave
        // a broken model. Single-file models (GLB, OBJ, ...) need no
        // special routing: they take the direct path below with
        // category "3D".
        if is_gltf_url(&clean_url) {
            return self.add_gltf_download(&clean_url, filename);
        }
        // Probe first so we can record a real filename / size / resumability
        // and fail fast on 404 / denied links instead of queueing a dead job.
        // The probe carries the page Referer when one was sent: some
        // servers refuse a bare probe but answer a referred one.
        let probe_opts = DownloadOptions {
            url: clean_url.clone(),
            referer: referer.clone(),
            ..Default::default()
        };
        let probe_result = probe(&probe_opts);
        // A web page is not a downloadable file — but since v0.2.0 it is
        // no longer refused outright: a page whose probe returns text/html
        // (YouTube, Vimeo, a public Google Drive file, any video page) goes
        // through media extraction instead, and the user picks a quality
        // (or the extension path takes the recorded default). Direct file
        // links behave exactly as before.
        let is_page = probe_result
            .as_ref()
            .is_ok_and(|p| {
                p.content_type
                    .as_deref()
                    .is_some_and(|ct| ct.starts_with("text/html"))
            });
        if is_page {
            return self.add_media_download(&clean_url, filename, format_id, referer, audio_url, manifest_text);
        }
        match probe_result {
            Ok(p) => {
                // Extension-less manifests: the probe's content type
                // says HLS/DASH, so this is a stream, not a file.
                if p.content_type.as_deref().is_some_and(|ct| {
                    fetchforge_engine::stream_kind_of_content_type(ct).is_some()
                }) {
                    return self.add_media_download(&clean_url, filename, format_id, referer, audio_url, manifest_text);
                }
                let (mut filename, total, resumable) = (
                    filename
                        .as_deref()
                        .map(sanitize_filename)
                        .or(p.filename.as_deref().map(sanitize_filename))
                        .unwrap_or_else(|| "download".to_string()),
                    p.content_length,
                    p.accept_ranges,
                );
                // Extension-less image URLs (e.g. a photo served as
                // /photo?id=123) get their extension from the probed
                // content type, so the file opens and categorises as
                // an image instead of landing in "Other" with no
                // extension. A name that already has an extension is
                // never rewritten.
                if p.content_type
                    .as_deref()
                    .is_some_and(is_image_content_type)
                {
                    filename = ensure_image_extension(&filename, p.content_type.as_deref());
                }
                self.add_direct_record(&clean_url, filename, total, resumable, referer)
            }
            Err(mut e) => {
                // Probe failed. Some video pages refuse HEAD/GET probes
                // (403 to non-browser user agents) while yt-dlp reads
                // them fine, so try media extraction as a fallback —
                // but ONLY if yt-dlp is already installed. A failed
                // probe on a dead direct link must never trigger a
                // 23 MB first-use yt-dlp download.
                //
                // When extraction is tried and fails, its error is the
                // one that diagnoses the problem: yt-dlp actually read
                // the page (e.g. YouTube's "Sign in to confirm you're
                // not a bot"), while the probe only knows the server
                // refused it. Record the extraction error, not the
                // probe error.
                if find_ytdlp(&self.data_dir).is_ok() {
                    match self.try_media_download(
                        &clean_url,
                        filename.clone(),
                        format_id.clone(),
                        referer.clone(),
                        audio_url.clone(),
                        manifest_text,
                    ) {
                        Ok(rec) => return Ok(rec),
                        Err(extract_err) => e = extract_err,
                    }
                }
                // Still record the failure so the user sees it, with the reason.
                let id = generate_download_id();
                let rec = DownloadRecord {
                    id: id.clone(),
                    url: clean_url.clone(),
                    filename: filename.unwrap_or_else(|| "download".to_string()),
                    destination: self.settings.download_dir.clone(),
                    category: "Other".to_string(),
                    status: Status::Failed,
                    total_bytes: None,
                    downloaded_bytes: 0,
                    resumable: false,
                    error: Some(e),
                    created_unix: now_unix(),
                    updated_unix: now_unix(),
                    format_id: None,
                    needs_merge: false,
                    format_selector: None,
                    stream_kind: None,
                    referer: None,
                    audio_url: None,
                    duplicate: false,
                    segments_total: None,
                    segments_done: 0,
                    estimated_total_bytes: None,
                };
                self.store.lock().unwrap().upsert(rec.clone());
                let _ = self.store.lock().unwrap().save();
                Ok(rec)
            }
        }
    }

    /// Media path: extract the page, pick a format, queue the download.
    /// `format_id` is the user's picker choice; None means the recorded
    /// default (extension path). On extraction failure, a Google Drive
    /// link gets one direct-download fallback (see below); anything else
    /// records a Failed row with the extraction error.
    fn add_media_download(
        self: &Arc<Self>,
        clean_url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
    ) -> Result<DownloadRecord, String> {
        match self.try_media_download(
            clean_url,
            filename.clone(),
            format_id,
            referer.clone(),
            audio_url,
            manifest_text,
        ) {
            Ok(rec) => Ok(rec),
            Err(e) => {
                // Extraction now runs in the worker (see
                // try_media_download), so this arm is only reached
                // for queueing failures; the Google Drive fallback
                // lives in the worker's planning step
                // (plan_media_download_or_drive_fallback).
                let id = generate_download_id();
                let rec = DownloadRecord {
                    id: id.clone(),
                    url: clean_url.to_string(),
                    filename: filename.unwrap_or_else(|| "download".to_string()),
                    destination: self.settings.download_dir.clone(),
                    category: "Other".to_string(),
                    status: Status::Failed,
                    total_bytes: None,
                    downloaded_bytes: 0,
                    resumable: false,
                    error: Some(e),
                    created_unix: now_unix(),
                    updated_unix: now_unix(),
                    format_id: None,
                    needs_merge: false,
                    format_selector: None,
                    stream_kind: None,
                    referer: None,
                    audio_url: None,
                    duplicate: false,
                    segments_total: None,
                    segments_done: 0,
                    estimated_total_bytes: None,
                };
                self.store.lock().unwrap().upsert(rec.clone());
                let _ = self.store.lock().unwrap().save();
                Ok(rec)
            }
        }
    }

    /// glTF path: plan the model + dependencies at add time (fetch
    /// the JSON, resolve and probe every dependency), then queue the
    /// bundle download. A plan failure records a Failed row with the
    /// reason instead of queueing a job that would fail halfway
    /// through. The worker re-plans from the URL at run time, so the
    /// record itself needs no dependency list stored on it.
    fn add_gltf_download(
        self: &Arc<Self>,
        clean_url: &str,
        filename: Option<String>,
    ) -> Result<DownloadRecord, String> {
        match fetch_gltf_plan(clean_url) {
            Ok(plan) => {
                let record_filename = filename
                    .as_deref()
                    .map(sanitize_filename)
                    .unwrap_or_else(|| plan.filename.clone());
                // Same idempotent-add rule as the other paths.
                {
                    let mut store = self.store.lock().unwrap();
                    // Fresh disk state before the duplicate check:
                    // the other process may have cleared this record
                    // (Clear finished) since this process last
                    // loaded — a stale in-memory match would return
                    // a download the user already removed instead of
                    // queueing the new one (2026-11-22 incident).
                    store.reload_from_disk();
                    if let Some(mut existing) = store.find_duplicate(
                        clean_url,
                        &record_filename,
                        &self.settings.download_dir,
                    ) {
                        // The caller must know nothing new was
                        // queued: flag the returned clone (never
                        // the stored record) so the extension
                        // reports "Already downloaded" instead of
                        // claiming a fresh download was sent.
                        existing.duplicate = true;
                        return Ok(existing);
                    }
                    let id = generate_download_id();
                    let rec = DownloadRecord {
                        id: id.clone(),
                        url: clean_url.to_string(),
                        filename: record_filename,
                        destination: self.settings.download_dir.clone(),
                        category: "3D".to_string(),
                        status: Status::Queued,
                        total_bytes: plan.total_bytes,
                        downloaded_bytes: 0,
                        // The bundle downloader skips files already
                        // present at their target, so an interrupted
                        // bundle can be resumed by re-adding it.
                        resumable: true,
                        error: None,
                        created_unix: now_unix(),
                        updated_unix: now_unix(),
                        format_id: None,
                        needs_merge: false,
                        format_selector: None,
                        stream_kind: Some("gltf".to_string()),
                        referer: None,
                        audio_url: None,
                        duplicate: false,
                        segments_total: None,
                        segments_done: 0,
                        estimated_total_bytes: None,
                    };
                    store.upsert(rec.clone());
                    let _ = store.save();
                    drop(store);
                    self.spawn_worker(id);
                    Ok(rec)
                }
            }
            Err(e) => {
                let id = generate_download_id();
                let rec = DownloadRecord {
                    id: id.clone(),
                    url: clean_url.to_string(),
                    filename: filename.unwrap_or_else(|| "download".to_string()),
                    destination: self.settings.download_dir.clone(),
                    category: "Other".to_string(),
                    status: Status::Failed,
                    total_bytes: None,
                    downloaded_bytes: 0,
                    resumable: false,
                    error: Some(e),
                    created_unix: now_unix(),
                    updated_unix: now_unix(),
                    format_id: None,
                    needs_merge: false,
                    format_selector: None,
                    stream_kind: None,
                    referer: None,
                    audio_url: None,
                    duplicate: false,
                    segments_total: None,
                    segments_done: 0,
                    estimated_total_bytes: None,
                };
                self.store.lock().unwrap().upsert(rec.clone());
                let _ = self.store.lock().unwrap().save();
                Ok(rec)
            }
        }
    }

    /// Queue a media download WITHOUT planning it first: the record
    /// is inserted immediately (stream_kind "pending") and the
    /// worker plans it (see [`Self::plan_pending_download`]).
    /// Planning used to run here, synchronously in the add request,
    /// and a slow extraction (a manifest fetch on a streaming CDN
    /// took 16 s on 2026-11-22) left the extension button stuck on
    /// "Sending…" until the response came back. The button now
    /// confirms "Sent" in milliseconds; a planning failure surfaces
    /// in the app list as a Failed row with the reason, exactly as
    /// before — just asynchronously. The requested format id is
    /// stored on the record as the planning input (the worker
    /// overwrites it with the chosen format); the filename is the
    /// caller's, or the provisional "download" the worker replaces
    /// with the planned name.
    fn try_media_download(
        self: &Arc<Self>,
        clean_url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
    ) -> Result<DownloadRecord, String> {
        let provisional = filename
            .as_deref()
            .map(sanitize_filename)
            .unwrap_or_else(|| "download".to_string());
        // Idempotent add (see Store::find_duplicate): an
        // already-active or already-completed download for the same
        // URL + filename is returned, never queued twice. The
        // worker re-checks after planning, when the resolved URL
        // and planned filename are known.
        {
            let mut store = self.store.lock().unwrap();
            // Fresh disk state before the duplicate check (see
            // add_gltf_download): a record the other process
            // cleared must not match from stale memory.
            store.reload_from_disk();
            if let Some(mut existing) = store.find_duplicate(
                clean_url,
                &provisional,
                &self.settings.download_dir,
            ) {
                // The caller must know nothing new was queued:
                // flag the returned clone (never the stored
                // record) so the extension reports "Already
                // downloaded" instead of claiming a fresh
                // download was sent.
                existing.duplicate = true;
                return Ok(existing);
            }
            let id = generate_download_id();
            let rec = DownloadRecord {
                id: id.clone(),
                url: clean_url.to_string(),
                filename: provisional,
                destination: self.settings.download_dir.clone(),
                category: "Other".to_string(),
                status: Status::Queued,
                total_bytes: None,
                downloaded_bytes: 0,
                // yt-dlp resumes from its own .part files, so a media
                // download interrupted by app close can be resumed.
                resumable: true,
                error: None,
                created_unix: now_unix(),
                updated_unix: now_unix(),
                format_id,
                needs_merge: false,
                format_selector: None,
                stream_kind: Some("pending".to_string()),
                referer,
                audio_url,
                duplicate: false,
                segments_total: None,
                segments_done: 0,
                estimated_total_bytes: None,
            };
            store.upsert(rec.clone());
            let _ = store.save();
            drop(store);
            // Captured playlist text (extension sends only): save
            // it to the record's sidecar before the worker plans,
            // so a signed manifest link is never fetched twice.
            // A duplicate add above never reaches here, so no
            // sidecar is written for a download that was not
            // queued.
            if let Some(text) = &manifest_text {
                save_manifest_sidecar(&self.data_dir, &id, text);
            }
            self.spawn_worker(id);
            Ok(rec)
        }
    }

    /// Plan a "pending" record in the worker: run the plan (or the
    /// Google Drive direct-download fallback), apply the paired
    /// HLS override, re-check duplicates against the resolved URL,
    /// and update the record in place. Returns the updated record
    /// to dispatch on; Ok(None) means a duplicate was found after
    /// planning and this record was removed (the existing download
    /// covers it); Err means planning failed and the record was
    /// marked Failed with the reason. A record stuck in "pending"
    /// (app closed mid-plan) is planned the same way by any worker
    /// that picks it up, including a resume.
    fn plan_pending_download(
        &self,
        rec: &DownloadRecord,
        job: &Job,
    ) -> Result<Option<DownloadRecord>, String> {
        let id = rec.id.clone();
        // The provisional filename ("download") means no name was
        // requested: let the plan derive it from the title. Any
        // other stored filename was the caller's and is kept as
        // the plan's stem.
        let filename_arg = if rec.filename == "download" {
            None
        } else {
            Some(rec.filename.as_str())
        };
        // Captured playlist text (extension sends only): the plan
        // fetch is the FIRST fetch of the manifest, so it must use
        // the capture — a single-use signed link consumed here
        // would fail again at run time.
        let captured = read_manifest_sidecar(&self.data_dir, &id, &rec.url);
        let outcome = plan_media_download_or_drive_fallback(
            &self.data_dir,
            &rec.url,
            rec.format_id.as_deref(),
            filename_arg,
            &rec.referer,
            captured.as_ref(),
        );
        if job.cancelled.load(Ordering::Relaxed) {
            self.set_status(&id, Status::Cancelled, None);
            return Err("download cancelled".to_string());
        }
        match outcome {
            Ok(PlanOutcome::Media(mut plan)) => {
                // Paired HLS variants (extension observed a video
                // variant AND its audio sibling, no master): the
                // plan from the video variant alone would be a
                // silent .ts with no merge. The worker downloads
                // both tracks and merges them, so the plan must say
                // so: needs_merge, and an .mp4 filename (the merge
                // output). Only for HLS: DASH keeps its audio
                // inside the manifest, so an audio URL with a DASH
                // plan is ignored.
                let paired_audio = if rec.audio_url.is_some()
                    && plan.stream_kind == Some(fetchforge_engine::StreamKind::Hls)
                {
                    plan.needs_merge = true;
                    if let Some((stem, _)) = plan.filename.rsplit_once('.') {
                        plan.filename = format!("{stem}.mp4");
                    }
                    rec.audio_url.clone()
                } else {
                    None
                };
                // The HTML fallback may have resolved the page to
                // an embedded stream/file URL: the record and the
                // worker both use the resolved URL, not the page
                // URL.
                let record_url = plan
                    .effective_url
                    .clone()
                    .unwrap_or_else(|| rec.url.clone());
                let mut store = self.store.lock().unwrap();
                // Fresh disk state before the plan-time duplicate
                // check: the removal below must only fire when the
                // matched record is CONFIRMED still on disk. The
                // bridge's memory can hold a record the app's Clear
                // finished already removed (2026-11-22: a new
                // novahd download vanished this way — the plan
                // matched the cleared clip in stale memory and
                // removed the new record).
                store.reload_from_disk();
                if store
                    .find_duplicate_except(
                        &record_url,
                        &plan.filename,
                        &self.settings.download_dir,
                        &id,
                    )
                    .is_some()
                {
                    store.remove(&id);
                    let _ = store.save();
                    drop(store);
                    // The record is gone, so its sidecar is
                    // orphaned — delete it (set_status never runs
                    // for a removed record).
                    delete_manifest_sidecar(&self.data_dir, &id);
                    return Ok(None);
                }
                let mut updated = match store.get(&id) {
                    Some(r) => r.clone(),
                    None => return Err("download removed".to_string()),
                };
                updated.url = record_url;
                updated.filename = plan.filename.clone();
                updated.category = category_for_filename(&plan.filename).to_string();
                updated.total_bytes = plan.total_bytes;
                updated.format_id = Some(plan.format_id);
                updated.needs_merge = plan.needs_merge;
                updated.format_selector = Some(plan.selector);
                updated.stream_kind = plan.stream_kind.map(|k| k.as_str().to_string());
                updated.audio_url = paired_audio;
                // HLS/DASH only: the manifest's segment count, so
                // the UI can show "0 / N pieces" before the first
                // segment lands. yt-dlp plans carry None.
                updated.segments_total = plan.segments_total;
                updated.updated_unix = now_unix();
                store.upsert(updated.clone());
                let _ = store.save();
                Ok(Some(updated))
            }
            Ok(PlanOutcome::Direct {
                url,
                filename,
                total_bytes,
                resumable,
            }) => {
                // Google Drive fallback: extraction failed, but the
                // file itself is directly downloadable. Convert the
                // record to a direct download and let it dispatch
                // down the direct path.
                let mut store = self.store.lock().unwrap();
                let mut updated = match store.get(&id) {
                    Some(r) => r.clone(),
                    None => return Err("download removed".to_string()),
                };
                updated.url = url;
                updated.filename = filename.clone();
                updated.category = category_for_filename(&filename).to_string();
                updated.total_bytes = total_bytes;
                updated.resumable = resumable;
                updated.format_id = None;
                updated.needs_merge = false;
                updated.format_selector = None;
                updated.stream_kind = None;
                updated.audio_url = None;
                updated.updated_unix = now_unix();
                store.upsert(updated.clone());
                let _ = store.save();
                Ok(Some(updated))
            }
            Err(e) => {
                self.set_status(&id, Status::Failed, Some(e));
                Err("download failed".to_string())
            }
        }
    }

    /// Queue a direct file download (the pre-0.2.0 path, unchanged
    /// except for the new record fields).
    fn add_direct_record(
        self: &Arc<Self>,
        clean_url: &str,
        filename: String,
        total: Option<u64>,
        resumable: bool,
        referer: Option<String>,
    ) -> Result<DownloadRecord, String> {
        // Idempotent add (safe to run twice): if the same URL +
        // filename is already Queued, Downloading, or Completed,
        // return that record instead of starting a second download
        // (see Store::find_duplicate). Without this, two workers
        // derive the same .part path and corrupt each other (found
        // by the duplicate test on 2026-10-26: one record reported
        // "got 0 bytes" while progress said complete) — and a
        // repeated click re-downloads a file that already finished.
        // The check + insert happen under one store lock so two
        // simultaneous POSTs cannot both slip through.
        {
            let mut store = self.store.lock().unwrap();
            // Fresh disk state before the duplicate check (see
            // add_gltf_download): a record the other process
            // cleared must not match from stale memory.
            store.reload_from_disk();
            if let Some(mut existing) =
                store.find_duplicate(clean_url, &filename, &self.settings.download_dir)
            {
                // The caller must know nothing new was queued:
                // flag the returned clone (never the stored
                // record) so the extension reports "Already
                // downloaded" instead of claiming a fresh
                // download was sent.
                existing.duplicate = true;
                return Ok(existing);
            }
            let id = generate_download_id();
            let rec = DownloadRecord {
                id: id.clone(),
                url: clean_url.to_string(),
                filename: filename.clone(),
                destination: self.settings.download_dir.clone(),
                category: category_for_filename(&filename).to_string(),
                status: Status::Queued,
                total_bytes: total,
                downloaded_bytes: 0,
                resumable,
                error: None,
                created_unix: now_unix(),
                updated_unix: now_unix(),
                format_id: None,
                needs_merge: false,
                format_selector: None,
                stream_kind: None,
                referer,
                audio_url: None,
                duplicate: false,
                segments_total: None,
                segments_done: 0,
                estimated_total_bytes: None,
            };
            store.upsert(rec.clone());
            let _ = store.save();
            drop(store);
            self.spawn_worker(id);
            Ok(rec)
        }
    }

    fn spawn_worker(self: &Arc<Self>, id: String) {
        let app = Arc::clone(self);
        let job = Arc::new(Job {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        app.jobs
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&job));
        std::thread::spawn(move || {
            {
                let mut active = app.active.lock().unwrap();
                // Simple queue gate: if at capacity, wait (poll) until a slot frees.
                while *active >= app.settings.max_simultaneous.max(1) {
                    drop(active);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if job.cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    active = app.active.lock().unwrap();
                }
                *active += 1;
            }
            let result = app.run_one(&id, &job);
            {
                let mut active = app.active.lock().unwrap();
                *active = active.saturating_sub(1);
            }
            let _ = result;
        });
    }

    fn run_one(&self, id: &str, job: &Job) -> Result<(), String> {
        let mut rec = match self.store.lock().unwrap().get(id) {
            Some(r) => r.clone(),
            None => return Err("unknown download".to_string()),
        };
        // Deferred planning (see try_media_download): a "pending"
        // record has not been extracted yet. Plan it here, in the
        // worker, then dispatch on the planned record. A duplicate
        // found after planning removes this record (Ok(None)); a
        // planning failure already marked it Failed.
        if rec.stream_kind.as_deref() == Some("pending") {
            match self.plan_pending_download(&rec, job) {
                Ok(Some(planned)) => rec = planned,
                Ok(None) => return Ok(()),
                Err(_) => return Err("download failed".to_string()),
            }
        }
        // Stream downloads (HLS/DASH) run through media.rs; yt-dlp
        // media downloads (a format was chosen) run through yt-dlp;
        // everything else takes the direct path, exactly as before.
        // An unknown stream kind fails honestly rather than being
        // misrouted to the wrong engine.
        match rec.stream_kind.as_deref() {
            Some("hls") => return self.run_stream_hls(&rec, job),
            Some("dash") => return self.run_stream_dash(&rec, job),
            // SCOPE NOTE (Phase 7): the brief scoped bridge/tauri
            // changes to add-path classification only, but a `.gltf`
            // bundle cannot be downloaded at add time without losing
            // progress, pause, and resume — and it cannot run through
            // the direct path, which downloads one file. This arm is
            // the same shape as the hls/dash arms (plan at add,
            // execute in the worker, dispatch on stream_kind), and the
            // bulk of the work lives in the engine (threed.rs).
            Some("gltf") => return self.run_gltf(&rec, job),
            Some(other) => {
                self.set_status(
                    id,
                    Status::Failed,
                    Some(format!(
                        "Stream downloads of this kind ({other}) are not supported yet."
                    )),
                );
                return Err("download failed".to_string());
            }
            None => {}
        }
        if rec.format_id.is_some() {
            return self.run_media(&rec, job);
        }
        self.set_status(id, Status::Downloading, None);
        let opts = DownloadOptions {
            url: rec.url.clone(),
            destination_dir: PathBuf::from(&rec.destination),
            filename: Some(rec.filename.clone()),
            max_segments: self.settings.max_segments,
            max_retries: self.settings.max_retries,
            bandwidth_kbps: self.settings.bandwidth_kbps,
            referer: rec.referer.clone(),
            user_agent: None,
            organise_by_category: self.settings.organise_by_category,
        };
        let app_id = id.to_string();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(r) = store.get(&app_id).cloned() {
                let mut r = r;
                r.downloaded_bytes = p.downloaded;
                r.total_bytes = p.total.or(r.total_bytes);
                // Segment progress (HLS/DASH only): only overwrite when
                // the report carries a value — yt-dlp and direct
                // downloads report None here, and must never wipe
                // the plan-time segment count.
                if let Some(d) = p.segments_done {
                    r.segments_done = d;
                }
                if let Some(t) = p.segments_total {
                    r.segments_total = Some(t);
                }
                if let Some(e) = p.estimated_total {
                    r.estimated_total_bytes = Some(e);
                }
                r.updated_unix = now_unix();
                store.upsert(r);
                let _ = store.save();
            }
        };
        match download(&opts, &progress, &job.paused, &job.cancelled) {
            Ok(final_path) => {
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(id).cloned() {
                    r.status = Status::Completed;
                    r.destination = final_path.to_string_lossy().to_string();
                    r.downloaded_bytes = r.total_bytes.unwrap_or(r.downloaded_bytes);
                    r.error = None;
                    r.updated_unix = now_unix();
                    store.upsert(r);
                    let _ = store.save();
                }
                Ok(())
            }
            Err(e) => {
                if let Some(reason) = stop_reason(&e) {
                    match reason {
                        fetchforge_engine::download::StopReason::Paused => {
                            self.set_status(id, Status::Paused, None);
                        }
                        fetchforge_engine::download::StopReason::Cancelled => {
                            self.set_status(id, Status::Cancelled, None);
                        }
                    }
                    Ok(())
                } else {
                    self.set_status(id, Status::Failed, Some(e));
                    Err("download failed".to_string())
                }
            }
        }
    }

    /// Run a media download (yt-dlp). The filename stem comes from the
    /// record, so a resume re-runs yt-dlp with the same output template
    /// and picks up its own .part files. On success the record's
    /// filename is corrected to the real file name: a merge may pick a
    /// different container than the planned extension.
    fn run_media(&self, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let stem = std::path::Path::new(&rec.filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "video".to_string());
        let ext = std::path::Path::new(&rec.filename)
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "mp4".to_string());
        // The selector was computed at plan time and stored on the
        // record; rebuild a plain one only for records written
        // before the field existed.
        let format_id = rec.format_id.clone().unwrap_or_default();
        let selector = rec.format_selector.clone().unwrap_or_else(|| {
            if rec.needs_merge {
                format!("{format_id}+bestaudio/{format_id}")
            } else {
                format_id.clone()
            }
        });
        let opts = MediaDownloadOptions {
            url: rec.url.clone(),
            selector,
            needs_merge: rec.needs_merge,
            destination_dir: PathBuf::from(&self.settings.download_dir),
            filename_stem: stem,
            expected_ext: ext,
            expected_total: rec.total_bytes,
            organise_by_category: self.settings.organise_by_category,
            referer: rec.referer.clone(),
        };
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(r) = store.get(&app_id).cloned() {
                let mut r = r;
                r.downloaded_bytes = p.downloaded;
                r.total_bytes = p.total.or(r.total_bytes);
                // Segment progress (HLS/DASH only): only overwrite when
                // the report carries a value — yt-dlp and direct
                // downloads report None here, and must never wipe
                // the plan-time segment count.
                if let Some(d) = p.segments_done {
                    r.segments_done = d;
                }
                if let Some(t) = p.segments_total {
                    r.segments_total = Some(t);
                }
                if let Some(e) = p.estimated_total {
                    r.estimated_total_bytes = Some(e);
                }
                r.updated_unix = now_unix();
                store.upsert(r);
                let _ = store.save();
            }
        };
        match download_media(
            &self.data_dir,
            &opts,
            &progress,
            &job.paused,
            &job.cancelled,
        ) {
            Ok(final_path) => {
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    r.status = Status::Completed;
                    r.destination = final_path.to_string_lossy().to_string();
                    if let Some(name) = final_path.file_name() {
                        r.filename = name.to_string_lossy().to_string();
                    }
                    r.downloaded_bytes = r.total_bytes.unwrap_or(r.downloaded_bytes);
                    r.error = None;
                    r.updated_unix = now_unix();
                    store.upsert(r);
                    let _ = store.save();
                }
                Ok(())
            }
            Err(e) => {
                if let Some(reason) = stop_reason(&e) {
                    match reason {
                        fetchforge_engine::download::StopReason::Paused => {
                            self.set_status(&id, Status::Paused, None);
                        }
                        fetchforge_engine::download::StopReason::Cancelled => {
                            self.set_status(&id, Status::Cancelled, None);
                        }
                    }
                    Ok(())
                } else {
                    self.set_status(&id, Status::Failed, Some(e));
                    Err("download failed".to_string())
                }
            }
        }
    }

    /// Run an HLS download (media.rs). The record's format_id is the
    /// chosen variant playlist URL (or the manifest URL itself for a
    /// plain media playlist). ffmpeg is resolved only when the
    /// chosen variant needs a merge (separate audio track); a
    /// resume re-runs the same variant and picks up the segment
    /// sidecar left by the paused run.
    fn run_stream_hls(&self, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let mut dir = PathBuf::from(&self.settings.download_dir);
        if self.settings.organise_by_category {
            dir = dir.join(category_for_filename(&rec.filename));
        }
        let variant_uri = rec.format_id.clone().unwrap_or_else(|| rec.url.clone());
        let ffmpeg = if rec.needs_merge {
            match ensure_ffmpeg(&self.data_dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.set_status(&id, Status::Failed, Some(e));
                    return Err("download failed".to_string());
                }
            }
        } else {
            None
        };
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(r) = store.get(&app_id).cloned() {
                let mut r = r;
                r.downloaded_bytes = p.downloaded;
                r.total_bytes = p.total.or(r.total_bytes);
                // Segment progress (HLS/DASH only): only overwrite when
                // the report carries a value — yt-dlp and direct
                // downloads report None here, and must never wipe
                // the plan-time segment count.
                if let Some(d) = p.segments_done {
                    r.segments_done = d;
                }
                if let Some(t) = p.segments_total {
                    r.segments_total = Some(t);
                }
                if let Some(e) = p.estimated_total {
                    r.estimated_total_bytes = Some(e);
                }
                r.updated_unix = now_unix();
                store.upsert(r);
                let _ = store.save();
            }
        };
        // Paired variants (the extension saw a video variant and
        // its audio sibling, no master): download both tracks and
        // merge them. Otherwise the normal variant path, with the
        // page Referer on every manifest/segment request. The
        // captured manifest (extension sends only) is used for
        // whichever fetch its URL matches — the plan fetch already
        // consumed nothing, but a single-use signed link must
        // still not be re-fetched here.
        let captured = read_manifest_sidecar(&self.data_dir, &id, &rec.url);
        let result = match &rec.audio_url {
            Some(audio_uri) => {
                let ffmpeg = match &ffmpeg {
                    Some(f) => f.clone(),
                    None => {
                        self.set_status(
                            &id,
                            Status::Failed,
                            Some("This quality keeps its audio on a separate track, which needs ffmpeg to merge.".to_string()),
                        );
                        return Err("download failed".to_string());
                    }
                };
                download_hls_pair(
                    &variant_uri,
                    audio_uri,
                    &dir,
                    &rec.filename,
                    &rec.referer,
                    &ffmpeg,
                    &progress,
                    &job.paused,
                    &job.cancelled,
                    captured.as_ref(),
                )
            }
            None => download_hls_variant(
                &rec.url,
                &variant_uri,
                &dir,
                &rec.filename,
                &rec.referer,
                ffmpeg.as_deref(),
                &progress,
                &job.paused,
                &job.cancelled,
                captured.as_ref(),
            ),
        };
        match result {
            Ok(final_path) => {
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    r.status = Status::Completed;
                    r.destination = final_path.to_string_lossy().to_string();
                    if let Some(name) = final_path.file_name() {
                        r.filename = name.to_string_lossy().to_string();
                    }
                    r.downloaded_bytes = r.total_bytes.unwrap_or(r.downloaded_bytes);
                    r.error = None;
                    r.updated_unix = now_unix();
                    store.upsert(r);
                    let _ = store.save();
                }
                Ok(())
            }
            Err(e) => {
                if let Some(reason) = stop_reason(&e) {
                    match reason {
                        fetchforge_engine::download::StopReason::Paused => {
                            self.set_status(&id, Status::Paused, None);
                        }
                        fetchforge_engine::download::StopReason::Cancelled => {
                            self.set_status(&id, Status::Cancelled, None);
                        }
                    }
                    Ok(())
                } else {
                    self.set_status(&id, Status::Failed, Some(e));
                    Err("download failed".to_string())
                }
            }
        }
    }

    /// Run a DASH download (media.rs). The record's format_id is the
    /// chosen representation id (None means the default). ffmpeg is
    /// resolved inside the engine only when the manifest keeps audio
    /// separate from the chosen video representation.
    fn run_stream_dash(&self, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let mut dir = PathBuf::from(&self.settings.download_dir);
        if self.settings.organise_by_category {
            dir = dir.join(category_for_filename(&rec.filename));
        }
        let opts = DashDownloadOptions {
            manifest_url: rec.url.clone(),
            representation_id: rec.format_id.clone(),
            destination_dir: dir,
            filename: rec.filename.clone(),
            referer: rec.referer.clone(),
        };
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(r) = store.get(&app_id).cloned() {
                let mut r = r;
                r.downloaded_bytes = p.downloaded;
                r.total_bytes = p.total.or(r.total_bytes);
                // Segment progress (HLS/DASH only): only overwrite when
                // the report carries a value — yt-dlp and direct
                // downloads report None here, and must never wipe
                // the plan-time segment count.
                if let Some(d) = p.segments_done {
                    r.segments_done = d;
                }
                if let Some(t) = p.segments_total {
                    r.segments_total = Some(t);
                }
                if let Some(e) = p.estimated_total {
                    r.estimated_total_bytes = Some(e);
                }
                r.updated_unix = now_unix();
                store.upsert(r);
                let _ = store.save();
            }
        };
        match download_dash(
            &self.data_dir,
            &opts,
            &progress,
            &job.paused,
            &job.cancelled,
            read_manifest_sidecar(&self.data_dir, &id, &rec.url).as_ref(),
        ) {
            Ok(final_path) => {
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    r.status = Status::Completed;
                    r.destination = final_path.to_string_lossy().to_string();
                    if let Some(name) = final_path.file_name() {
                        r.filename = name.to_string_lossy().to_string();
                    }
                    r.downloaded_bytes = r.total_bytes.unwrap_or(r.downloaded_bytes);
                    r.error = None;
                    r.updated_unix = now_unix();
                    store.upsert(r);
                    let _ = store.save();
                }
                Ok(())
            }
            Err(e) => {
                if let Some(reason) = stop_reason(&e) {
                    match reason {
                        fetchforge_engine::download::StopReason::Paused => {
                            self.set_status(&id, Status::Paused, None);
                        }
                        fetchforge_engine::download::StopReason::Cancelled => {
                            self.set_status(&id, Status::Cancelled, None);
                        }
                    }
                    Ok(())
                } else {
                    self.set_status(&id, Status::Failed, Some(e));
                    Err("download failed".to_string())
                }
            }
        }
    }

    /// Run a `.gltf` bundle download (threed.rs). The plan is rebuilt
    /// from the URL at run time, so a resume always works from the
    /// current file, and files already complete at their target are
    /// skipped by the bundle downloader.
    fn run_gltf(&self, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let plan = match fetch_gltf_plan(&rec.url) {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&id, Status::Failed, Some(e));
                return Err("download failed".to_string());
            }
        };
        let opts = GltfDownloadOptions {
            destination_dir: PathBuf::from(&self.settings.download_dir),
            max_segments: self.settings.max_segments,
            max_retries: self.settings.max_retries,
            bandwidth_kbps: self.settings.bandwidth_kbps,
        };
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(r) = store.get(&app_id).cloned() {
                let mut r = r;
                r.downloaded_bytes = p.downloaded;
                r.total_bytes = p.total.or(r.total_bytes);
                // Segment progress (HLS/DASH only): only overwrite when
                // the report carries a value — yt-dlp and direct
                // downloads report None here, and must never wipe
                // the plan-time segment count.
                if let Some(d) = p.segments_done {
                    r.segments_done = d;
                }
                if let Some(t) = p.segments_total {
                    r.segments_total = Some(t);
                }
                if let Some(e) = p.estimated_total {
                    r.estimated_total_bytes = Some(e);
                }
                r.updated_unix = now_unix();
                store.upsert(r);
                let _ = store.save();
            }
        };
        match download_gltf_bundle(
            &plan,
            &rec.filename,
            &opts,
            &progress,
            &job.paused,
            &job.cancelled,
        ) {
            Ok(final_path) => {
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    r.status = Status::Completed;
                    r.destination = final_path.to_string_lossy().to_string();
                    if let Some(name) = final_path.file_name() {
                        r.filename = name.to_string_lossy().to_string();
                    }
                    r.downloaded_bytes = r.total_bytes.unwrap_or(r.downloaded_bytes);
                    r.error = None;
                    r.updated_unix = now_unix();
                    store.upsert(r);
                    let _ = store.save();
                }
                Ok(())
            }
            Err(e) => {
                if let Some(reason) = stop_reason(&e) {
                    match reason {
                        fetchforge_engine::download::StopReason::Paused => {
                            self.set_status(&id, Status::Paused, None);
                        }
                        fetchforge_engine::download::StopReason::Cancelled => {
                            self.set_status(&id, Status::Cancelled, None);
                        }
                    }
                    Ok(())
                } else {
                    self.set_status(&id, Status::Failed, Some(e));
                    Err("download failed".to_string())
                }
            }
        }
    }

    fn set_status(&self, id: &str, status: Status, error: Option<String>) {
        // Terminal state (Completed/Failed/Cancelled): the captured
        // manifest sidecar is no longer needed — a resume re-plans
        // from the stored format, and a finished download never
        // fetches its manifest again. Paused keeps the sidecar: a
        // resume re-fetches the manifest, and a single-use signed
        // link still needs the capture. (Status is not Copy, so
        // the check runs before the move into the record.)
        let terminal = matches!(
            status,
            Status::Completed | Status::Failed | Status::Cancelled
        );
        let mut store = self.store.lock().unwrap();
        if let Some(mut r) = store.get(id).cloned() {
            r.status = status;
            if let Some(e) = error {
                r.error = Some(e);
            }
            r.updated_unix = now_unix();
            store.upsert(r);
            let _ = store.save();
        }
        drop(store);
        if terminal {
            delete_manifest_sidecar(&self.data_dir, id);
        }
    }

    /// Empty the detector list (memory + detected.json). Detector
    /// items only — downloads and history are never touched.
    /// Returns how many items were removed.
    fn clear_detected(&self) -> usize {
        let mut list = self.detected.lock().unwrap();
        let n = list.len();
        list.clear();
        drop(list);
        let _ = std::fs::remove_file(self.data_dir.join("detected.json"));
        n
    }

    /// Clear cached files (the app's "Clear cache" action). Removes:
    /// (a) the detector store (see clear_detected); (b) per-download
    /// segment temp dirs (`<name>.part.seg/`) and legacy sidecars
    /// (`<name>.part.segments`) for downloads that are terminal or
    /// have no record — NEVER for active or paused downloads, whose
    /// segments are their resume state; (c) manifest sidecars for
    /// terminal or missing records; (d) stale `*.tmp` files in the
    /// data dir. NEVER deletes downloaded files, the download list,
    /// or history. Returns (files removed, bytes freed).
    fn clear_cache(&self) -> (usize, u64) {
        let mut removed_files = 0usize;
        let mut freed_bytes = 0u64;
        // (a) Detector store: count the file itself as one removed
        // file (sized before clearing, since clearing deletes it).
        let detected_path = self.data_dir.join("detected.json");
        let detected_size = std::fs::metadata(&detected_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let detected_existed = detected_path.is_file();
        self.clear_detected();
        if detected_existed {
            removed_files += 1;
            freed_bytes += detected_size;
        }
        // Live records protect their temp files: active (queued /
        // downloading) and paused downloads keep every segment.
        let (live_ids, live_filenames): (Vec<String>, Vec<String>) = {
            let mut store = self.store.lock().unwrap();
            store.reload_from_disk();
            let mut ids = Vec::new();
            let mut names = Vec::new();
            for rec in store.all() {
                if matches!(
                    rec.status,
                    Status::Queued | Status::Downloading | Status::Paused
                ) {
                    ids.push(rec.id.clone());
                    names.push(rec.filename.clone());
                }
            }
            (ids, names)
        };
        // (b) Segment temp dirs + legacy sidecars in the download
        // tree (top level and one level of category subdirs — the
        // only depths the worker writes to).
        let mut dirs_to_scan = vec![PathBuf::from(&self.settings.download_dir)];
        if let Ok(entries) = std::fs::read_dir(&self.settings.download_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    dirs_to_scan.push(p);
                }
            }
        }
        for dir in &dirs_to_scan {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let stem = if name.ends_with(".part.seg") {
                    Some(name.trim_end_matches(".part.seg"))
                } else if name.ends_with(".part.segments") {
                    Some(name.trim_end_matches(".part.segments"))
                } else {
                    None
                };
                let Some(stem) = stem else { continue };
                // A live download with a matching filename keeps
                // its temp files (fail-safe: keep on any doubt).
                if live_filenames
                    .iter()
                    .any(|f| filename_matches_unique(stem, f))
                {
                    continue;
                }
                if path.is_dir() {
                    // Sum the segment files, then remove the dir.
                    let mut dir_bytes = 0u64;
                    let mut dir_files = 0usize;
                    if let Ok(files) = std::fs::read_dir(&path) {
                        for f in files.flatten() {
                            if let Ok(m) = f.metadata() {
                                if m.is_file() {
                                    dir_files += 1;
                                    dir_bytes += m.len();
                                }
                            }
                        }
                    }
                    if std::fs::remove_dir_all(&path).is_ok() {
                        removed_files += dir_files;
                        freed_bytes += dir_bytes;
                    }
                } else if path.is_file() {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if std::fs::remove_file(&path).is_ok() {
                        removed_files += 1;
                        freed_bytes += size;
                    }
                }
            }
        }
        // (c) Manifest sidecars for terminal or missing records.
        let manifests_dir = self.data_dir.join("manifests");
        if let Ok(entries) = std::fs::read_dir(&manifests_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".m3u8") else {
                    continue;
                };
                if live_ids.iter().any(|live| live == id) {
                    continue;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(&path).is_ok() {
                    removed_files += 1;
                    freed_bytes += size;
                }
            }
        }
        // (d) Stale *.tmp files in the data dir (detected.json.tmp,
        // downloads.json.tmp — both writers are best-effort tmp +
        // rename, so a tmp removed mid-write only loses one save,
        // which the next save rewrites).
        if let Ok(entries) = std::fs::read_dir(&self.data_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().is_some_and(|e| e == "tmp") {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if std::fs::remove_file(&path).is_ok() {
                        removed_files += 1;
                        freed_bytes += size;
                    }
                }
            }
        }
        (removed_files, freed_bytes)
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
    filename: Option<String>,
    /// Media downloads only: the yt-dlp format id picked in the format
    /// picker. Absent means "pick the default" (extension path).
    format_id: Option<String>,
    /// The page the download came from (extension sends only):
    /// sent as the Referer header on later requests.
    #[serde(default)]
    referer: Option<String>,
    /// HLS paired-variant downloads only (extension sends only):
    /// the sibling audio variant playlist URL observed next to the
    /// video variant. When set, the worker downloads both tracks
    /// and merges them.
    #[serde(default)]
    audio_url: Option<String>,
    /// Playlist text the extension captured from the player's own
    /// response (extension sends only): the worker uses it instead
    /// of re-fetching a signed manifest link that may only work
    /// once. Saved to a sidecar file at add time, never stored in
    /// the record (see save_manifest_sidecar). Over the native
    /// limit it is ignored with a warning.
    #[serde(default)]
    manifest_text: Option<String>,
}

#[derive(Deserialize)]
struct FormatsRequest {
    url: String,
}

/// One media item the extension detected on a page (Phase 4/8).
/// Only confirmed metadata is stored: every field except `url` is
/// optional and omitted when unknown, so a partial detection never
/// invents a title, type or size. The bridge stores these for the
/// future detected-media panel (Phase 9); storing an item never
/// downloads anything on its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DetectedItem {
    url: String,
    // The extension sends pageUrl (camelCase); accept it so the
    // page a detection came from is actually stored (it used to be
    // dropped on ingest, so detector downloads had no Referer).
    #[serde(default, alias = "pageUrl", skip_serializing_if = "Option::is_none")]
    page_url: Option<String>,
    /// A sibling audio variant observed with a video variant (the
    /// extension sends audioUrl); the detector download passes it
    /// through so the pair is downloaded and merged.
    #[serde(default, alias = "audioUrl", skip_serializing_if = "Option::is_none")]
    audio_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// The file's own name as the extension saw it (video file
    /// name, PDF file name): the detector panel shows this when
    /// there is no page title, so a video is listed BY NAME
    /// instead of by its URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    /// A poster/thumbnail URL for a video (its poster attribute):
    /// the detector panel shows it like an image thumbnail, so
    /// the user can tell what the video is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    poster: Option<String>,
    /// The extension's word for the kind ("video", "audio", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detected_unix: Option<u64>,
}

#[derive(Deserialize)]
struct DetectedRequest {
    items: Vec<DetectedItem>,
}

/// Maximum detected items kept in memory and on disk; older items
/// are dropped first. The list is a recent-detections cache, not an
/// archive — 200 is far more than a panel ever shows at once.
const MAX_DETECTED_ITEMS: usize = 200;

fn generate_token() -> String {
    // F04: OS CSPRNG only, on every platform. No time/pid fallback.
    // Refusing to start is safer than minting a predictable token.
    match generate_secure_token() {
        Ok(tok) => tok,
        Err(e) => {
            eprintln!("FetchForge bridge: cannot generate a secure token: {e}");
            eprintln!("Refusing to start with a weak token.");
            std::process::exit(1);
        }
    }
}

/// Captured-manifest sidecars (round 6): the extension captures a
/// playlist's text from the player's own response — where the page
/// origin and cookies apply — and sends it with the download, so a
/// signed link is never fetched twice (the second fetch fails).
/// The text is too large to live in downloads.json, so it is kept
/// in a sidecar file per record, `data_dir/manifests/<id>.m3u8`,
/// written at add time, read by the worker at plan and run time,
/// and deleted when the download reaches a terminal state (see
/// set_status) or is removed as a post-plan duplicate. A missing
/// or unreadable sidecar simply means "fetch normally".
fn manifest_sidecar_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    data_dir.join("manifests").join(format!("{id}.m3u8"))
}

fn save_manifest_sidecar(data_dir: &std::path::Path, id: &str, text: &str) {
    if text.len() > MAX_CAPTURED_MANIFEST_BYTES {
        eprintln!(
            "FetchForge WARNING: captured manifest for download {id} is {} bytes (limit {}); ignoring it, the manifest will be fetched normally",
            text.len(),
            MAX_CAPTURED_MANIFEST_BYTES
        );
        return;
    }
    let dir = data_dir.join("manifests");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if std::fs::write(manifest_sidecar_path(data_dir, id), text).is_err() {
        eprintln!("FetchForge WARNING: could not save the captured manifest for download {id}");
    }
}

fn read_manifest_sidecar(
    data_dir: &std::path::Path,
    id: &str,
    url: &str,
) -> Option<CapturedManifest> {
    let text = std::fs::read_to_string(manifest_sidecar_path(data_dir, id)).ok()?;
    Some(CapturedManifest {
        url: url.to_string(),
        text,
    })
}

fn delete_manifest_sidecar(data_dir: &std::path::Path, id: &str) {
    let _ = std::fs::remove_file(manifest_sidecar_path(data_dir, id));
}

/// Does a segment-temp stem belong to a record's filename? The
/// worker writes `<unique-filename>.part.seg` / `.segments`, where
/// the unique filename is the record filename, or the record
/// filename with a " (N)" collision suffix before the extension
/// (see unique_path). Cache clearing must never delete a live
/// download's pieces, so the match is deliberately generous: any
/// plausible suffix keeps the files.
fn filename_matches_unique(stem: &str, filename: &str) -> bool {
    if stem == filename {
        return true;
    }
    let Some((stem_base, stem_ext)) = stem.rsplit_once('.') else {
        return false;
    };
    let Some((file_base, file_ext)) = filename.rsplit_once('.') else {
        return false;
    };
    if stem_ext != file_ext {
        return false;
    }
    let Some(rest) = stem_base.strip_prefix(file_base) else {
        return false;
    };
    // Rest must be exactly " (N)" with N a positive integer.
    let rest = rest.strip_prefix(" (").and_then(|r| r.strip_suffix(')'));
    matches!(rest, Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Host check (DNS-rebinding defence): the bridge only answers when the
/// HTTP Host is loopback. A browser that was tricked by DNS rebinding
/// into contacting 127.0.0.1 via an attacker domain would send that
/// domain as Host, and must be rejected even though the TCP destination
/// is loopback. A missing Host is also rejected (HTTP/1.1 requires it;
/// curl, fetch, and the desktop UI always send it).
fn host_allowed(headers: &str) -> bool {
    for line in headers.lines() {
        if let Some(v) = line
            .strip_prefix("Host:")
            .or_else(|| line.strip_prefix("host:"))
        {
            let v = v.trim().to_ascii_lowercase();
            let host = if let Some(rest) = v.strip_prefix('[') {
                // IPv6: "[::1]:18443" -> "::1"
                rest.split(']').next().unwrap_or("")
            } else {
                v.split(':').next().unwrap_or("")
            };
            return matches!(host, "127.0.0.1" | "localhost" | "::1");
        }
    }
    false
}

fn origin_allowed(headers: &str) -> bool {
    // No Origin (curl, native tools) is allowed on loopback; a browser
    // Origin must be an extension origin or another loopback page.
    for line in headers.lines() {
        if let Some(v) = line
            .strip_prefix("Origin:")
            .or_else(|| line.strip_prefix("origin:"))
        {
            let v = v.trim();
            return v.starts_with("chrome-extension://")
                || v.starts_with("moz-extension://")
                || v.starts_with("http://127.0.0.1")
                || v.starts_with("http://localhost");
        }
    }
    true
}

fn http_response(status: u16, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn handle_connection(app: &Arc<App>, mut stream: std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // Read headers.
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 64 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let content_length: usize = headers
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let body = if content_length > 0 && buf.len() >= header_end + content_length {
        String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string()
    } else {
        String::new()
    };
    if !host_allowed(&headers) {
        let _ = stream.write_all(http_response(403, r#"{"error":"Host not allowed"}"#).as_bytes());
        return;
    }
    if !origin_allowed(&headers) {
        let _ =
            stream.write_all(http_response(403, r#"{"error":"Origin not allowed"}"#).as_bytes());
        return;
    }
    // Preflight (extension fetch CORS).
    if method == "OPTIONS" {
        let _ = stream.write_all(http_response(200, "{}").as_bytes());
        return;
    }
    if method == "GET" && path == "/health" {
        let body = serde_json::json!({"app": "FetchForge", "version": env!("CARGO_PKG_VERSION"), "bridge": true});
        let _ = stream.write_all(http_response(200, &body.to_string()).as_bytes());
        return;
    }
    // Challenge-response (no token required, no token returned): the
    // extension sends a nonce, the bridge answers with HMAC(token,
    // nonce). The extension verifies that HMAC with its stored token
    // BEFORE sending the token to this server. A fake server that does
    // not know the token cannot produce a matching HMAC.
    if method == "POST" && path == "/challenge" {
        let nonce = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("nonce")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let valid_nonce = !nonce.is_empty()
            && nonce.len() <= 256
            && nonce
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_nonce {
            let _ = stream.write_all(http_response(400, r#"{"error":"Invalid nonce"}"#).as_bytes());
            return;
        }
        let token = app.token.lock().unwrap().clone();
        let hmac = fetchforge_engine::pairing::challenge_hmac(&token, &nonce);
        let b = serde_json::json!({"hmac": hmac}).to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    // Pairing verification (no token required): the extension sends the
    // one-time code it was given by the user. Only on a correct code is
    // the bridge token returned, to the extension background script only.
    if method == "POST" && path == "/pair/verify" {
        let code = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("code")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let result = app.pairing.lock().unwrap().verify(&code);
        match result {
            Ok(()) => {
                let token = app.token.lock().unwrap().clone();
                let b = serde_json::json!({"paired": true, "token": token}).to_string();
                let _ = stream.write_all(http_response(200, &b).as_bytes());
            }
            Err(e) => {
                let b = serde_json::json!({"error": e.to_string()}).to_string();
                let _ = stream.write_all(http_response(403, &b).as_bytes());
            }
        }
        return;
    }
    // Everything else needs the token, compared via SHA-256 hashes in
    // constant time (see pairing::constant_time_eq) so a wrong token
    // does not leak how many characters matched through timing.
    let token_ok = headers.lines().any(|l| {
        l.to_ascii_lowercase()
            .strip_prefix("x-fetchforge-token:")
            .map(|v| {
                let provided = fetchforge_engine::pairing::hash_code(v.trim());
                let stored = fetchforge_engine::pairing::hash_code(&app.token.lock().unwrap());
                fetchforge_engine::pairing::constant_time_eq(&provided, &stored)
            })
            .unwrap_or(false)
    });
    if !token_ok {
        let _ = stream.write_all(
            http_response(401, r#"{"error":"Missing or invalid bridge token"}"#).as_bytes(),
        );
        return;
    }
    // Pairing code generation (app only, token required): returns the
    // display form XXXX-XXXX. Only its hash is stored in the bridge.
    if method == "POST" && path == "/pair/generate" {
        match app.pairing.lock().unwrap().generate() {
            Ok(code) => {
                let b = serde_json::json!({
                    "code": fetchforge_engine::pairing::display_code(&code),
                    "expires_in_seconds": 60
                })
                .to_string();
                let _ = stream.write_all(http_response(200, &b).as_bytes());
            }
            Err(e) => {
                let b = serde_json::json!({"error": e}).to_string();
                let _ = stream.write_all(http_response(500, &b).as_bytes());
            }
        }
        return;
    }
    // Unpair all browsers (app only, token required): rotates the token
    // and clears any active pairing code. Old tokens stop working
    // immediately. The new token is NOT returned here.
    if method == "POST" && path == "/unpair-all" {
        match app.rotate_token() {
            Ok(()) => {
                let _ = stream.write_all(http_response(200, r#"{"rotated":true}"#).as_bytes());
            }
            Err(e) => {
                let b = serde_json::json!({"error": e}).to_string();
                let _ = stream.write_all(http_response(500, &b).as_bytes());
            }
        }
        return;
    }
    // Pairing code status (app only, token required): lets the app
    // show a "code used" transition when the extension claims the
    // code, instead of the countdown just running out. `used` stays
    // true until a new code is generated.
    if method == "GET" && path == "/pair/status" {
        let (active, used, seconds_left) = app.pairing.lock().unwrap().status();
        let b = serde_json::json!({
            "active": active,
            "used": used,
            "seconds_left": seconds_left
        })
        .to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    if method == "GET" && path == "/downloads" {
        let list: Vec<DownloadRecord> = {
            let mut store = app.store.lock().unwrap();
            // Pick up downloads added by the desktop app since launch.
            store.reload_from_disk();
            store.all().into_iter().cloned().collect()
        };
        let body = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        let _ = stream.write_all(http_response(200, &body).as_bytes());
        return;
    }
    if method == "GET" && path == "/settings" {
        let body = serde_json::to_string(&app.settings).unwrap_or_else(|_| "{}".to_string());
        let _ = stream.write_all(http_response(200, &body).as_bytes());
        return;
    }
    if method == "POST" && path == "/downloads" {
        let req: Result<AddRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => match app.add_download(
                &r.url,
                r.filename,
                r.format_id,
                r.referer,
                r.audio_url,
                r.manifest_text,
            ) {
                Ok(rec) => {
                    // 202: the download is queued; extraction and
                    // the download itself run asynchronously in the
                    // worker, so the response is immediate.
                    let b = serde_json::to_string(&rec).unwrap();
                    let _ = stream.write_all(http_response(202, &b).as_bytes());
                }
                Err(e) => {
                    let b = serde_json::json!({"error": e}).to_string();
                    let _ = stream.write_all(http_response(400, &b).as_bytes());
                }
            },
            Err(_) => {
                let _ = stream
                    .write_all(http_response(400, r#"{"error":"Invalid JSON body"}"#).as_bytes());
            }
        }
        return;
    }
    // Media formats (token required): list the qualities available for
    // a video page so the UI can show a picker. The chosen format id is
    // then passed to POST /downloads as `format_id`.
    if method == "POST" && path == "/media/formats" {
        let req: Result<FormatsRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => match list_formats(&app.data_dir, &r.url) {
                Ok(info) => {
                    let b = serde_json::to_string(&info).unwrap();
                    let _ = stream.write_all(http_response(200, &b).as_bytes());
                }
                Err(e) => {
                    let b = serde_json::json!({"error": e}).to_string();
                    let _ = stream.write_all(http_response(400, &b).as_bytes());
                }
            },
            Err(_) => {
                let _ = stream
                    .write_all(http_response(400, r#"{"error":"Invalid JSON body"}"#).as_bytes());
            }
        }
        return;
    }
    // Detected media (token required): the extension reports media
    // it found on a page; the bridge stores it for the future
    // detected-media panel (Phase 9). Storing never downloads.
    if method == "POST" && path == "/media/detected" {
        let req: Result<DetectedRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => {
                let stored = app.store_detected(r.items);
                let b = serde_json::json!({"stored": stored}).to_string();
                let _ = stream.write_all(http_response(200, &b).as_bytes());
            }
            Err(_) => {
                let _ = stream
                    .write_all(http_response(400, r#"{"error":"Invalid JSON body"}"#).as_bytes());
            }
        }
        return;
    }
    if method == "GET" && path == "/media/detected" {
        let items: Vec<DetectedItem> = app.detected.lock().unwrap().clone();
        let b = serde_json::json!({"items": items}).to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    // Clear the detector list (token required): empties detected
    // media only — downloads and history are never touched.
    if method == "POST" && path == "/media/detected/clear" {
        let removed = app.clear_detected();
        let b = serde_json::json!({"removed": removed}).to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    // Clear cache (token required): temp/segment files and the
    // detector store only — downloaded files, the download list
    // and history are never touched (see clear_cache).
    if method == "POST" && path == "/cache/clear" {
        let (removed_files, freed_bytes) = app.clear_cache();
        let b = serde_json::json!({"removed_files": removed_files, "freed_bytes": freed_bytes})
            .to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    // Clear finished downloads from the list (token required): removes
    // Completed/Failed/Cancelled records only. Downloaded files are
    // never touched, and active downloads are never removed. Must sit
    // before the /downloads/<id>/... catch-all below, or the catch-all
    // swallows it (its segments would be ["downloads",
    // "clear-finished"], which falls through to 404).
    if method == "POST" && path == "/downloads/clear-finished" {
        let removed = {
            let mut store = app.store.lock().unwrap();
            store.reload_from_disk();
            let n = store.remove_finished();
            if let Err(e) = store.save() {
                let b = serde_json::json!({"error": e}).to_string();
                let _ = stream.write_all(http_response(500, &b).as_bytes());
                return;
            }
            n
        };
        let b = serde_json::json!({"removed": removed}).to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
        return;
    }
    // /downloads/<id>/pause | resume | cancel
    if method == "POST" && path.starts_with("/downloads/") {
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        if segs.len() == 3 {
            let id = segs[1];
            let action = segs[2];
            match action {
                "pause" => {
                    if let Some(job) = app.jobs.lock().unwrap().get(id).cloned() {
                        job.paused.store(true, Ordering::Relaxed);
                    }
                    let _ = stream.write_all(http_response(200, "{}").as_bytes());
                    return;
                }
                "resume" => {
                    // Clear pause and spawn a fresh worker; the engine
                    // resumes from the .part file when the server allows it.
                    if let Some(job) = app.jobs.lock().unwrap().get(id).cloned() {
                        job.paused.store(false, Ordering::Relaxed);
                    }
                    app.set_status(id, Status::Queued, None);
                    app.spawn_worker(id.to_string());
                    let _ = stream.write_all(http_response(200, "{}").as_bytes());
                    return;
                }
                "cancel" => {
                    if let Some(job) = app.jobs.lock().unwrap().get(id).cloned() {
                        job.cancelled.store(true, Ordering::Relaxed);
                    }
                    let _ = stream.write_all(http_response(200, "{}").as_bytes());
                    return;
                }
                _ => {}
            }
        }
    }
    let _ = stream.write_all(http_response(404, r#"{"error":"Not found"}"#).as_bytes());
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Minimal loopback HTTP client (plain TcpStream, no new dependency —
/// the same approach as the desktop app's bridge_post). Returns
/// (status, body). Used by the native host to talk to the bridge.
fn loopback_http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Result<(u16, String), String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("Bridge not reachable ({e})"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("X-FetchForge-Token: {t}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Bridge request failed ({e})"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("Bridge response failed ({e})"))?;
    let (head, resp_body) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b.to_string()),
        None => ("", raw),
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, resp_body))
}

/// True when something answering like the FetchForge bridge is
/// listening on `port` (GET /health, no token needed). A foreign
/// process squatting on the port does not count: the body must name
/// FetchForge.
fn bridge_health(port: u16) -> bool {
    match loopback_http(port, "GET", "/health", None, None) {
        Ok((200, body)) => body.contains("\"app\"") && body.contains("FetchForge"),
        _ => false,
    }
}

/// Forward an addDownload request to the long-running bridge over
/// loopback HTTP. The token is re-read from the token file at call
/// time (it is the shared secret between this host and the bridge;
/// re-reading keeps a rotated token working).
fn forward_add_to_bridge(
    data_dir: &std::path::Path,
    port: u16,
    url: &str,
    filename: Option<&str>,
    referer: Option<&str>,
    audio_url: Option<&str>,
    format_id: Option<&str>,
    manifest_text: Option<&str>,
) -> Result<DownloadRecord, String> {
    let token = std::fs::read_to_string(data_dir.join("bridge-token.txt"))
        .map_err(|_| "Bridge token file not found.".to_string())?;
    let token = token.trim();
    if token.is_empty() {
        return Err("Bridge token file is empty.".to_string());
    }
    let body = serde_json::json!({"url": url, "filename": filename, "referer": referer, "audio_url": audio_url, "format_id": format_id, "manifest_text": manifest_text}).to_string();
    let (status, resp) = loopback_http(port, "POST", "/downloads", Some(token), Some(&body))?;
    // POST /downloads answers 202 (queued; extraction runs in the
    // worker). 201 is still accepted so a newer CLI keeps working
    // against an older bridge during an upgrade.
    if status != 201 && status != 202 {
        let detail: String = resp.chars().take(200).collect();
        return Err(format!("Bridge returned HTTP {status}: {detail}"));
    }
    serde_json::from_str(&resp).map_err(|e| format!("Bridge returned bad JSON ({e})"))
}

/// Start the bridge (this same binary, `serve` mode) as a detached
/// background process and wait until it answers /health (up to ~5 s).
/// Detached so it survives this host process exiting: on Windows the
/// child gets CREATE_NO_WINDOW (no console flash) + DETACHED_PROCESS;
/// on Unix a spawned child is not killed when its parent exits.
/// Stderr goes to bridge.log in the data dir, like the app-spawned
/// bridge. Only used when no bridge is already listening.
fn spawn_detached_bridge(data_dir: &std::path::Path, port: u16) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find own executable ({e})"))?;
    let log_path = data_dir.join("bridge.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000) + DETACHED_PROCESS (0x00000008).
        cmd.creation_flags(0x08000000 | 0x00000008);
    }
    if let Some(f) = log_file {
        cmd.stderr(f);
    } else {
        cmd.stderr(std::process::Stdio::null());
    }
    cmd.spawn()
        .map_err(|e| format!("Could not start the bridge ({e})"))?;
    for _ in 0..10 {
        if bridge_health(port) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err("The bridge was started but did not answer within 5 seconds. See bridge.log in the FetchForge data folder.".to_string())
}

fn native_host(app: &Arc<App>) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut input = stdin.lock();
    loop {
        let mut len_buf = [0u8; 4];
        if input.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > 1024 * 1024 {
            break;
        }
        let mut msg = vec![0u8; len];
        if input.read_exact(&mut msg).is_err() {
            break;
        }
        let value: serde_json::Value = match serde_json::from_slice(&msg) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let reply = match kind {
            "ping" => {
                serde_json::json!({"type": "pong", "app": "FetchForge", "version": env!("CARGO_PKG_VERSION")})
            }
            "addDownload" => {
                let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let filename = value
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let referer = value
                    .get("referer")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let audio_url = value
                    .get("audio_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // The picker popup's choice (extension sends only):
                // a format id, and playlist text captured from the
                // player's own response (see save_manifest_sidecar).
                let format_id = value
                    .get("format_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let manifest_text = value
                    .get("manifest_text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // A download started from the extension must survive
                // this host process exiting: Chrome/Firefox terminate
                // the native host after its sendNativeMessage reply,
                // so an in-process download would be killed
                // mid-download. Forward to the long-running bridge
                // instead; start one if none is listening; only as a
                // last resort run in-process, with a warning the
                // extension can show.
                //   1. Bridge already running (normal case: the app is
                //      open) -> forward, reply carries "via": "bridge".
                //   2. No bridge -> spawn a detached bridge, wait for
                //      /health, then forward ("bridge_started": true).
                //      The bridge keeps running after the browser
                //      closes, so the download completes; when the app
                //      later starts, its own bridge child fails to bind
                //      (port taken) and logs it, while the existing
                //      bridge keeps serving the same data dir.
                //   3. Bridge cannot be started (port squatted by a
                //      foreign process, spawn failure) -> in-process
                //      add with a warning. The download is recorded
                //      and may complete if the host stays alive; if it
                //      is killed, the record is recovered as Paused on
                //      next app start (media downloads are resumable)
                //      and the user can resume it. Refusing outright
                //      would be worse: the extension would then hand
                //      the URL to the browser's own downloader, which
                //      saves a YouTube page as a useless .html file.
                match forward_add_to_bridge(
                    &app.data_dir,
                    DEFAULT_PORT,
                    url,
                    filename.as_deref(),
                    referer.as_deref(),
                    audio_url.as_deref(),
                    format_id.as_deref(),
                    manifest_text.as_deref(),
                ) {
                    Ok(rec) => {
                        serde_json::json!({"type": "added", "download": rec, "via": "bridge"})
                    }
                    Err(_first_err) => {
                        match spawn_detached_bridge(&app.data_dir, DEFAULT_PORT).and_then(|()| {
                            forward_add_to_bridge(
                                &app.data_dir,
                                DEFAULT_PORT,
                                url,
                                filename.as_deref(),
                                referer.as_deref(),
                                audio_url.as_deref(),
                                format_id.as_deref(),
                                manifest_text.as_deref(),
                            )
                        }) {
                            Ok(rec) => {
                                serde_json::json!({"type": "added", "download": rec, "via": "bridge", "bridge_started": true})
                            }
                            Err(_second_err) => match app.add_download(
                                url,
                                filename,
                                format_id,
                                referer,
                                audio_url,
                                manifest_text,
                            ) {
                                Ok(rec) => {
                                    serde_json::json!({"type": "added", "download": rec, "via": "in-process", "warning": "The FetchForge bridge is not running, so this download started inside the browser connection and may stop if the browser closes it. Open the FetchForge app to keep downloads running."})
                                }
                                Err(e) => serde_json::json!({"type": "error", "error": e}),
                            },
                        }
                    }
                }
            }
            _ => serde_json::json!({"type": "error", "error": "Unknown message type"}),
        };
        let bytes = reply.to_string().into_bytes();
        let len_bytes = (bytes.len() as u32).to_le_bytes();
        if stdout.write_all(&len_bytes).is_err()
            || stdout.write_all(&bytes).is_err()
            || stdout.flush().is_err()
        {
            break;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = default_data_dir();
    let mut port = DEFAULT_PORT;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" if i + 1 < args.len() => {
                data_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                port = args[i + 1].parse().unwrap_or(DEFAULT_PORT);
                i += 2;
            }
            _ => i += 1,
        }
    }
    let app = App::new(data_dir.clone());
    // Mode selection, in precedence order:
    //
    // 1. `print-token` — explicit user action (the app reads the
    //    token from stdout). Checked first: it must work even
    //    when stdin is piped.
    if args.iter().any(|a| a == "print-token") {
        // Explicit user action only: this command prints the token to
        // stdout so the app can read it. Serve mode never prints it.
        println!("{}", app.token.lock().unwrap());
        return;
    }
    // 2. `native-host` — explicit native-messaging mode.
    // 3. `serve` — explicit serve mode. Checked BEFORE the stdin
    //    test: both the desktop app and spawn_detached_bridge
    //    spawn the bridge with `serve` and a null stdin, and null
    //    stdin is not a terminal — without this check those
    //    spawns would wrongly enter native-host mode.
    // 4. Piped stdin (not a terminal), no mode argument — Chrome
    //    launches the native host with NO arguments and a pipe on
    //    stdin, so this IS a native-host launch. Before round 7
    //    it fell through to serve mode, failed to bind (the
    //    bridge was already running), and exited without
    //    replying — so the extension's native path never worked.
    // 5. Anything else (a terminal, a plain double-click): serve.
    let native_arg = args.iter().any(|a| a == "native-host");
    let serve_arg = args.iter().any(|a| a == "serve");
    if native_arg || (!serve_arg && !std::io::stdin().is_terminal()) {
        native_host(&app);
        return;
    }
    // Default: serve.
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("FetchForge bridge could not bind 127.0.0.1:{port}: {e}");
            eprintln!("Is another copy already running? Only one bridge may run at a time.");
            std::process::exit(1);
        }
    };
    // Only the process that actually becomes the server recovers
    // interrupted downloads — after the bind succeeds, so a second
    // process that fails to bind never touches downloads.json.
    app.recover_interrupted();
    eprintln!("FetchForge bridge listening on 127.0.0.1:{port} (loopback only)");
    eprintln!("Data dir: {}", data_dir.display());
    eprintln!(
        "Token file: {}",
        data_dir.join("bridge-token.txt").display()
    );
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let app = Arc::clone(&app);
                std::thread::spawn(move || handle_connection(&app, s));
            }
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Duration;

    fn test_data_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bridge-token.txt"), "test-token-123").unwrap();
        dir
    }

    /// A one-shot stub bridge: accepts one connection, checks the
    /// request, answers with `response`, and reports what it saw.
    fn stub_server(response: String) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = vec![0u8; 8192];
            let mut request = Vec::new();
            // Read until the full body arrives (Content-Length) or EOF.
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&request).to_string();
                        if let Some(head) = text.split("\r\n\r\n").next() {
                            let len: usize = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                })
                                .unwrap_or(0);
                            if text.len() >= head.len() + 4 + len {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            String::from_utf8_lossy(&request).to_string()
        });
        (port, handle)
    }

    fn record_json() -> String {
        // Deliberately omits format_id / needs_merge: old records
        // must still deserialize (serde defaults).
        r#"{"id":"dl-1","url":"https://example.com/x.zip","filename":"x.zip","destination":"/tmp","category":"Archives","status":"queued","total_bytes":10,"downloaded_bytes":0,"resumable":true,"error":null,"created_unix":1700000000,"updated_unix":1700000000}"#.to_string()
    }

    #[test]
    fn forward_add_posts_token_and_url_to_bridge() {
        let dir = test_data_dir("fetchforge-test-forward");
        let response = format!(
            "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            record_json().len(),
            record_json()
        );
        let (port, handle) = stub_server(response);
        let rec = forward_add_to_bridge(
            &dir,
            port,
            "https://example.com/x.zip",
            Some("x.zip"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(rec.id, "dl-1");
        assert_eq!(rec.filename, "x.zip");
        assert_eq!(rec.format_id, None, "old record JSON must load with defaults");
        let request = handle.join().unwrap();
        assert!(
            request.contains("POST /downloads HTTP/1.1"),
            "request: {request}"
        );
        assert!(
            request.contains("X-FetchForge-Token: test-token-123"),
            "token header missing: {request}"
        );
        assert!(
            request.contains("https://example.com/x.zip"),
            "url missing: {request}"
        );
    }

    #[test]
    fn forward_add_fails_fast_when_bridge_is_down() {
        let dir = test_data_dir("fetchforge-test-forward-down");
        // A port with (almost certainly) nothing listening: bind, drop.
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let start = std::time::Instant::now();
        let err = forward_add_to_bridge(
            &dir,
            port,
            "https://example.com/x.zip",
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("not reachable"), "error: {err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn forward_add_rejects_wrong_status() {
        let dir = test_data_dir("fetchforge-test-forward-401");
        let body = r#"{"error":"Missing or invalid bridge token"}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (port, _handle) = stub_server(response);
        let err = forward_add_to_bridge(
            &dir,
            port,
            "https://example.com/x.zip",
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("401"), "error: {err}");
    }

    #[test]
    fn bridge_health_checks_body_not_just_status() {
        let good_body = r#"{"app":"FetchForge","version":"0.2.0","bridge":true}"#;
        let good = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            good_body.len(),
            good_body
        );
        let (port, _h) = stub_server(good);
        assert!(bridge_health(port));
        // A foreign process answering 200 with other content is NOT a bridge.
        let bad_body = r#"{"hello":"world"}"#;
        let bad = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            bad_body.len(),
            bad_body
        );
        let (port, _h) = stub_server(bad);
        assert!(!bridge_health(port));
    }

    #[test]
    fn add_request_accepts_optional_format_id() {
        let r: AddRequest =
            serde_json::from_str(r#"{"url":"https://example.com/x.zip"}"#).unwrap();
        assert_eq!(r.format_id, None);
        let r: AddRequest = serde_json::from_str(
            r#"{"url":"https://www.youtube.com/watch?v=x","filename":"v.mp4","format_id":"22"}"#,
        )
        .unwrap();
        assert_eq!(r.format_id.as_deref(), Some("22"));
        assert_eq!(r.filename.as_deref(), Some("v.mp4"));
    }

    fn detected(url: &str) -> DetectedItem {
        DetectedItem {
            url: url.to_string(),
            page_url: None,
            audio_url: None,
            title: None,
            filename: None,
            poster: None,
            kind: None,
            mime_type: None,
            size_bytes: None,
            duration_secs: None,
            detected_unix: None,
        }
    }

    #[test]
    fn detected_store_dedupes_by_url_and_skips_empty() {
        let dir = test_data_dir("fetchforge-test-detected");
        let app = App::new(dir.clone());
        let mut first = detected("https://example.com/v.mp4");
        first.title = Some("Old title".to_string());
        assert_eq!(app.store_detected(vec![first, detected("")]), 1);
        let mut second = detected("https://example.com/v.mp4");
        second.title = Some("New title".to_string());
        assert_eq!(app.store_detected(vec![second]), 1);
        let list = app.detected.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title.as_deref(), Some("New title"));
        assert!(list[0].detected_unix.is_some(), "timestamp stamped");
        // Persisted: a fresh App over the same data dir reloads it.
        drop(list);
        let app2 = App::new(dir);
        let list = app2.detected.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].url, "https://example.com/v.mp4");
    }

    #[test]
    fn detected_store_caps_at_200_dropping_oldest() {
        let dir = test_data_dir("fetchforge-test-detected-cap");
        let app = App::new(dir);
        let items: Vec<DetectedItem> = (0..205)
            .map(|i| detected(&format!("https://example.com/{i}.mp4")))
            .collect();
        assert_eq!(app.store_detected(items), 205);
        let list = app.detected.lock().unwrap();
        assert_eq!(list.len(), MAX_DETECTED_ITEMS);
        assert_eq!(list[0].url, "https://example.com/5.mp4");
        assert_eq!(list[199].url, "https://example.com/204.mp4");
    }

    #[test]
    fn detected_item_omits_unknown_fields() {
        // Only confirmed metadata is serialized; unknown fields are
        // absent, never null-invented.
        let item = detected("https://example.com/v.mp4");
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(json, r#"{"url":"https://example.com/v.mp4"}"#);
        // And a corrupt detected.json starts empty instead of
        // crashing the bridge.
        let dir = test_data_dir("fetchforge-test-detected-corrupt");
        std::fs::write(dir.join("detected.json"), "{not json").unwrap();
        let app = App::new(dir);
        assert!(app.detected.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_add_returns_existing_record_flagged_duplicate() {
        // A re-sent URL (2026-11-22: a clip the user clicked twice)
        // must not queue a second download — and the caller must be
        // TOLD nothing new was queued, or the extension claims
        // "Sent to FetchForge" for a download that never existed.
        // The manifest stub answers slowly (2 s) so the worker's
        // planning cannot finish (and rename the record) before
        // the second add runs.
        use std::io::Write;
        use std::sync::atomic::AtomicBool;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
            loop {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        std::thread::sleep(Duration::from_secs(2));
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                playlist.len(),
                                playlist
                            )
                            .as_bytes(),
                        );
                        let _ = stream.flush();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let dir = test_data_dir("fetchforge-test-duplicate-flag");
        let app = App::new(dir);
        let url = format!("http://127.0.0.1:{port}/v.m3u8");
        let first = app
            .try_media_download(&url, Some("clip.mp4".to_string()), None, None, None, None)
            .unwrap();
        assert!(!first.duplicate, "first add queues a new download");
        let second = app
            .try_media_download(&url, Some("clip.mp4".to_string()), None, None, None, None)
            .unwrap();
        assert!(second.duplicate, "second add must be flagged duplicate");
        assert_eq!(second.id, first.id, "same record returned");
        // The stored record is untouched: the flag lives only on
        // the returned clone, never in downloads.json.
        let stored = app.store.lock().unwrap().get(&first.id).unwrap().clone();
        assert!(!stored.duplicate, "stored record must not carry the flag");
        let json = serde_json::to_string(&stored).unwrap();
        assert!(!json.contains("duplicate"), "flag must not persist: {json}");
        // ...but the flagged clone serializes it, so the extension
        // sees it in the add response.
        let json = serde_json::to_string(&second).unwrap();
        assert!(
            json.contains("\"duplicate\":true"),
            "flag must serialize on the duplicate return: {json}"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
    }

    #[test]
    fn cleared_record_does_not_block_readd() {
        // 2026-11-22 incident: the user cleared a finished clip
        // (Clear finished in the app), then re-added the same URL;
        // the bridge's stale in-memory store still held the cleared
        // record, so the add-time duplicate check returned it as
        // "Already downloaded" and nothing was queued. The add must
        // reload from disk first, so a cleared record never blocks
        // a re-add in the same process.
        let dir = test_data_dir("fetchforge-test-cleared-readd");
        let app = App::new(dir.clone());
        let download_dir = app.settings.download_dir.clone();
        let mut old = test_record("clip-old", "clip.mp4", Status::Completed);
        old.url = "https://example.com/clip.mp4".to_string();
        old.destination = download_dir.clone();
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(old.clone());
            let _ = store.save();
            // Age the in-memory copy: a real cleared record was
            // finished hours ago, so the reload's grace period
            // (which protects just-created records) must not
            // protect it. No save here — the disk copy is deleted
            // by the other process below either way.
            old.created_unix -= 3600;
            old.updated_unix -= 3600;
            store.upsert(old);
        }
        // The other process (the app) clears it from disk.
        let mut other = Store::load(dir.join("downloads.json"));
        assert!(other.remove("clip-old"));
        let _ = other.save();
        // Re-add the same URL + filename without restarting: the
        // bridge's memory still holds the cleared record, but the
        // add-time check must reload and queue a NEW download.
        // (The spawned worker fails in the background — no yt-dlp
        // or network for example.com here — which is harmless: the
        // assertion is on the returned record.)
        let second = app
            .try_media_download(
                "https://example.com/clip.mp4",
                Some("clip.mp4".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(
            !second.duplicate,
            "a cleared record must not flag the re-add as a duplicate"
        );
        assert_ne!(second.id, "clip-old", "the re-add must queue a new record");
        assert_eq!(second.status, Status::Queued);
    }

    #[test]
    fn plan_time_duplicate_check_uses_fresh_disk_state() {
        // The 2026-11-22 incident itself: the add queued a pending
        // record, the worker planned it, the HTML fallback resolved
        // the page to a clip URL, and the PLAN-time duplicate check
        // matched the cleared clip in stale memory and removed the
        // new record — downloads.json was written with no record
        // at all. The plan-time check must reload from disk first,
        // so only a record CONFIRMED still on disk can kill a new
        // download.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::AtomicBool;
        // Stub server: one media playlist. The plan fetches it
        // twice (once to learn the planned filename, once inside
        // plan_pending_download), so the server loops until told
        // to stop.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let playlist =
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
            loop {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                playlist.len(),
                                playlist
                            )
                            .as_bytes(),
                        );
                        let _ = stream.flush();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let dir = test_data_dir("fetchforge-test-plan-duplicate-fresh");
        let app = App::new(dir.clone());
        let download_dir = app.settings.download_dir.clone();
        let url = format!("http://127.0.0.1:{port}/v.m3u8");
        // Learn the planned filename first: the cleared record must
        // match it exactly, or the test would pass trivially.
        let planned_filename = match plan_media_download_or_drive_fallback(
            &dir, &url, None, Some("clip"), &None, None,
        )
        .unwrap()
        {
            PlanOutcome::Media(plan) => plan.filename,
            PlanOutcome::Direct { .. } => panic!("manifest URL must plan as media"),
        };
        // The cleared clip: same URL + planned filename, Completed,
        // aged so the reload's grace period does not protect it.
        let mut cleared = test_record("clip-cleared", &planned_filename, Status::Completed);
        cleared.url = url.clone();
        cleared.destination = download_dir.clone();
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(cleared.clone());
            let _ = store.save();
            cleared.created_unix -= 3600;
            cleared.updated_unix -= 3600;
            store.upsert(cleared);
        }
        // The pending record the worker is about to plan.
        let mut pending = test_record("clip-pending", "clip", Status::Queued);
        pending.url = url.clone();
        pending.destination = download_dir.clone();
        pending.stream_kind = Some("pending".to_string());
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(pending.clone());
            let _ = store.save();
        }
        // The other process clears the clip from disk.
        let mut other = Store::load(dir.join("downloads.json"));
        assert!(other.remove("clip-cleared"));
        let _ = other.save();
        // Plan the pending record directly (no worker thread): the
        // plan-time check must reload, find the clip gone, and
        // keep the new record.
        let job = Job {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let outcome = app.plan_pending_download(&pending, &job).unwrap();
        assert!(
            outcome.is_some(),
            "the new record must survive planning once its duplicate is cleared from disk"
        );
        let updated = outcome.unwrap();
        assert_eq!(updated.id, "clip-pending");
        assert_eq!(updated.filename, planned_filename);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
    }

    fn detected_item(url: &str, kind: &str) -> DetectedItem {
        DetectedItem {
            url: url.to_string(),
            page_url: None,
            audio_url: None,
            title: None,
            filename: None,
            poster: None,
            kind: Some(kind.to_string()),
            mime_type: None,
            size_bytes: None,
            duration_secs: None,
            detected_unix: None,
        }
    }

    #[test]
    fn store_detected_keeps_videos_when_images_flood() {
        // The detector used to show images only: 200 image
        // detections crowded every video out of the capped list.
        // Trimming must drop the oldest IMAGE first, so a video
        // (or PDF) survives the flood.
        let dir = test_data_dir("fetchforge-test-detected-fairness");
        let app = App::new(dir);
        let mut items: Vec<DetectedItem> = (0..200)
            .map(|i| detected_item(&format!("https://example.com/img{i}.jpg"), "image"))
            .collect();
        items.push(detected_item("https://example.com/movie.m3u8", "video"));
        items.push(detected_item("https://example.com/manual.pdf", "document"));
        let stored = app.store_detected(items);
        assert_eq!(stored, 202);
        let list = app.detected.lock().unwrap();
        assert_eq!(list.len(), MAX_DETECTED_ITEMS);
        assert!(
            list.iter()
                .any(|d| d.url == "https://example.com/movie.m3u8"),
            "video crowded out by images"
        );
        assert!(
            list.iter()
                .any(|d| d.url == "https://example.com/manual.pdf"),
            "PDF crowded out by images"
        );
        // Two oldest images gave way, not the newcomers.
        assert!(!list.iter().any(|d| d.url == "https://example.com/img0.jpg"));
        assert!(!list.iter().any(|d| d.url == "https://example.com/img1.jpg"));
    }

    #[test]
    fn clear_detected_empties_list_and_file() {
        let dir = test_data_dir("fetchforge-test-detected-clear");
        let app = App::new(dir.clone());
        app.store_detected(vec![
            detected_item("https://example.com/a.jpg", "image"),
            detected_item("https://example.com/b.mp4", "video"),
        ]);
        assert!(dir.join("detected.json").is_file());
        let removed = app.clear_detected();
        assert_eq!(removed, 2);
        assert!(app.detected.lock().unwrap().is_empty());
        assert!(!dir.join("detected.json").is_file());
        // Clearing an empty list is a no-op, not an error.
        assert_eq!(app.clear_detected(), 0);
    }

    fn test_record(id: &str, filename: &str, status: Status) -> DownloadRecord {
        // Current timestamps: the store's anti-resurrection rule
        // drops records absent from disk whose updated_unix is
        // older than the removal grace period, so an ancient
        // timestamp would make save()/reload() discard the record.
        let now = now_unix();
        DownloadRecord {
            id: id.to_string(),
            url: format!("https://example.com/{filename}"),
            filename: filename.to_string(),
            destination: String::new(),
            category: "Video".to_string(),
            status,
            total_bytes: None,
            downloaded_bytes: 0,
            resumable: true,
            error: None,
            created_unix: now,
            updated_unix: now,
            format_id: None,
            needs_merge: false,
            format_selector: None,
            stream_kind: None,
            referer: None,
            audio_url: None,
            duplicate: false,
            segments_total: None,
            segments_done: 0,
            estimated_total_bytes: None,
        }
    }

    #[test]
    fn clear_cache_keeps_live_download_pieces() {
        // Cache clear must remove temp files for terminal/orphaned
        // downloads only. A queued download's segment dir, a paused
        // download's segment dir, and a live download's manifest
        // sidecar must all survive; a completed download's pieces,
        // an orphan's pieces, a stale tmp file, and the detector
        // store must go. Downloaded files are never touched.
        let dir = test_data_dir("fetchforge-test-cache-clear");
        let downloads = std::env::temp_dir().join("fetchforge-test-cache-clear-dl");
        let _ = std::fs::remove_dir_all(&downloads);
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            serde_json::json!({"download_dir": downloads.to_string_lossy()}).to_string(),
        )
        .unwrap();
        let app = App::new(dir.clone());
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(test_record("live-id", "live-movie.mp4", Status::Queued));
            store.upsert(test_record("paused-id", "paused-movie.mp4", Status::Paused));
            store.upsert(test_record("done-id", "done-movie.mp4", Status::Completed));
            let _ = store.save();
        }
        // Segment dirs: live, paused, done, orphan.
        for name in [
            "live-movie.mp4.part.seg",
            "paused-movie.mp4.part.seg",
            "done-movie.mp4.part.seg",
            "orphan.mp4.part.seg",
        ] {
            let d = downloads.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("000000.seg"), b"SEGMENT").unwrap();
        }
        // Legacy sidecar for the done download.
        std::fs::write(downloads.join("done-movie.mp4.part.segments"), b"done=1\n").unwrap();
        // A real downloaded file: never touched.
        std::fs::write(downloads.join("done-movie.mp4"), b"MOVIE").unwrap();
        // Manifest sidecars: live (keep), done + missing (remove).
        save_manifest_sidecar(&dir, "live-id", "#EXTM3U\n");
        save_manifest_sidecar(&dir, "done-id", "#EXTM3U\n");
        save_manifest_sidecar(&dir, "missing-id", "#EXTM3U\n");
        // Stale tmp + a detector item.
        std::fs::write(dir.join("stale.tmp"), b"tmp").unwrap();
        app.store_detected(vec![detected_item("https://example.com/a.jpg", "image")]);

        let (removed_files, freed_bytes) = app.clear_cache();
        assert!(removed_files > 0, "nothing was cleared");
        assert!(freed_bytes > 0, "nothing was freed");
        // Live + paused pieces survive.
        assert!(downloads.join("live-movie.mp4.part.seg").is_dir());
        assert!(downloads.join("paused-movie.mp4.part.seg").is_dir());
        assert!(manifest_sidecar_path(&dir, "live-id").is_file());
        // Terminal + orphan temp files are gone.
        assert!(!downloads.join("done-movie.mp4.part.seg").exists());
        assert!(!downloads.join("orphan.mp4.part.seg").exists());
        assert!(!downloads.join("done-movie.mp4.part.segments").exists());
        assert!(!manifest_sidecar_path(&dir, "done-id").exists());
        assert!(!manifest_sidecar_path(&dir, "missing-id").exists());
        assert!(!dir.join("stale.tmp").exists());
        assert!(app.detected.lock().unwrap().is_empty());
        // The downloaded file itself is untouched.
        assert_eq!(
            std::fs::read(downloads.join("done-movie.mp4")).unwrap(),
            b"MOVIE"
        );
        let _ = std::fs::remove_dir_all(&downloads);
    }

    #[test]
    fn manifest_sidecar_round_trip_and_oversize_ignored() {
        let dir = test_data_dir("fetchforge-test-manifest-sidecar");
        save_manifest_sidecar(&dir, "rec-1", "#EXTM3U\n#EXTINF:10,\nseg0.ts\n");
        let got = read_manifest_sidecar(&dir, "rec-1", "https://cdn.example.com/v.m3u8")
            .expect("sidecar readable");
        assert_eq!(got.url, "https://cdn.example.com/v.m3u8");
        assert!(got.text.contains("#EXTM3U"));
        delete_manifest_sidecar(&dir, "rec-1");
        assert!(read_manifest_sidecar(&dir, "rec-1", "https://cdn.example.com/v.m3u8").is_none());
        // Oversize captures are ignored (with a warning), never
        // written — the worker fetches the manifest normally.
        let big = "x".repeat(MAX_CAPTURED_MANIFEST_BYTES + 1);
        save_manifest_sidecar(&dir, "rec-2", &big);
        assert!(!manifest_sidecar_path(&dir, "rec-2").is_file());
    }

    #[test]
    fn filename_matches_unique_handles_collision_suffixes() {
        assert!(filename_matches_unique("movie.mp4", "movie.mp4"));
        assert!(filename_matches_unique("movie (1).mp4", "movie.mp4"));
        assert!(filename_matches_unique("movie (12).mp4", "movie.mp4"));
        assert!(!filename_matches_unique("movie (x).mp4", "movie.mp4"));
        assert!(!filename_matches_unique("movie (1).mkv", "movie.mp4"));
        assert!(!filename_matches_unique("other.mp4", "movie.mp4"));
        assert!(!filename_matches_unique("movie.mp4", "movie (1).mp4"));
    }
}
