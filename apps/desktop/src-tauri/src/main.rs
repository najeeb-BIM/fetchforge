// Tauri shell for FetchForge. The download logic lives in fetchforge-engine;
// this file only wires Tauri commands to it. The bridge binary in
// crates/bridge shares the same engine and is the path browser extensions use.
//
// NOTE: This Tauri crate is built on Windows (see scripts/build-windows.ps1
// and .github/workflows/windows-build.yml). It is NOT compiled on the Linux
// dev VM, which has no WebView2 / Windows toolchain; see docs/TEST_REPORT.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fetchforge_engine::{
    category_for_download, category_for_filename, delete_download_files, download, download_dash,
    download_dash_audio_only, download_gltf_bundle, download_hls_audio_only_opts,
    download_hls_pair_opts, download_hls_variant_audio_only_opts, download_hls_variant_opts,
    download_media, ensure_ffmpeg, ensure_image_extension, extract_audio_as,
    fetch_gltf_plan_with_proxy, find_ffmpeg, find_ytdlp, generate_download_id, is_gltf_url,
    is_image_content_type, list_formats_with_referer_and_proxy, list_playlist_with_proxy,
    next_lower_format, plan_media_download_or_drive_fallback_with_options, probe,
    probe_video_quality, remove_partial_files, sanitize_filename, sniff_stream_kind_with_proxy,
    stop_reason, unique_path, validate_proxy_url, CapturedManifest, DashDownloadOptions,
    DownloadOptions, DownloadRecord, GltfDownloadOptions, MediaDownloadOptions, MediaInfo,
    PlanOutcome, PlaylistInfo, SizeKind, Status, Store, StreamDownloadOptions,
};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

struct Job {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

struct State {
    store: Mutex<Store>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    download_dir: PathBuf,
    data_dir: PathBuf,
    /// F08: bridge child spawned on app launch, killed on app exit.
    /// The bridge never keeps running after the app closes.
    bridge_child: Mutex<Option<std::process::Child>>,
    /// Shutdown-when-finished guard (round 11 extension, feature
    /// 7): once the shutdown command has fired, it must never
    /// fire again in this process — two workers finishing at the
    /// same moment must not both run `shutdown`. Mirrors the
    /// bridge's guard; the two processes each guard their own
    /// workers (the bridge owns extension downloads, the app
    /// owns its own).
    shutdown_triggered: AtomicBool,
}

/// F08 (NOT VERIFIED on Windows): spawn the bridge in serve mode next
/// to the app executable, so browser extensions can talk to it without
/// the user running anything manually. If port 18443 is already taken,
/// the bridge exits with its clear "could not bind" error (written to
/// bridge.log in the data dir) and there is NO silent port fallback.
/// If the bridge binary is missing, direct downloads in the app still
/// work; only the extension bridge path is unavailable.
fn spawn_bridge(data_dir: &PathBuf) -> Option<std::process::Child> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()));
    let mut candidates = Vec::new();
    if let Some(d) = exe_dir {
        candidates.push(d.join("fetchforge-bridge.exe"));
        candidates.push(d.join("fetchforge-bridge"));
    }
    let bridge_path = candidates.into_iter().find(|c| c.exists());
    let bridge_path = match bridge_path {
        Some(p) => p,
        None => {
            eprintln!("FetchForge: bridge binary not found next to the app. Browser extension bridge path will not work; direct downloads in the app still work.");
            return None;
        }
    };
    let log_path = data_dir.join("bridge.log");
    let log_file = std::fs::File::create(&log_path).ok();
    let mut cmd = std::process::Command::new(&bridge_path);
    cmd.arg("serve")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("18443");
    // F08b: on Windows, start the bridge with no console window. The
    // bridge used to open a blank black console next to the app, and
    // closing it killed the bridge (and Pair with it). Windows-only;
    // other platforms are unaffected.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000): no console window is created.
        cmd.creation_flags(0x08000000);
    }
    // The bridge logs to bridge.log (stderr); its stdout is not used in
    // serve mode, so discard it instead of inheriting the app's.
    cmd.stdout(std::process::Stdio::null());
    if let Some(f) = log_file {
        cmd.stderr(f);
    }
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            eprintln!("FetchForge: could not start the bridge ({e}). Browser extension bridge path will not work; direct downloads in the app still work.");
            None
        }
    }
}

/// One media item the extension detected on a page, read from the
/// bridge's detected.json in the data dir (Phase 9 Media Detector
/// panel). Local struct with serde derive in this file — the
/// DetectedItem type lives in the bridge crate, which this crate does
/// not depend on. Field-for-field the same shape as the bridge's, so
/// the same JSON parses on both sides; every field except `url` is
/// optional and omitted when unknown.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DetectedItem {
    url: String,
    // Mirrors the bridge's struct (detected.json is shared): the
    // extension sends pageUrl/audioUrl in camelCase.
    #[serde(default, alias = "pageUrl", skip_serializing_if = "Option::is_none")]
    page_url: Option<String>,
    #[serde(default, alias = "audioUrl", skip_serializing_if = "Option::is_none")]
    audio_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// The file's own name as the extension saw it (video file
    /// name, PDF file name): the detector panel shows this when
    /// there is no page title. Mirrors the bridge's field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    /// A poster/thumbnail URL for a video: the detector panel
    /// shows it like an image thumbnail. Mirrors the bridge's
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    poster: Option<String>,
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

/// List media the extension detected, for the Media Detector panel.
/// A missing file means no detections yet (empty list, not an error);
/// a corrupt file is logged and also yields an empty list, mirroring
/// the bridge's own detected.json handling.
#[tauri::command]
fn list_detected_media(state: tauri::State<Arc<State>>) -> Vec<DetectedItem> {
    let path = state.data_dir.join("detected.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<DetectedItem>>(&text) {
        Ok(items) => items,
        Err(e) => {
            eprintln!(
                "FetchForge WARNING: detected.json is not valid JSON ({e}); detected media list started empty"
            );
            Vec::new()
        }
    }
}

#[tauri::command]
fn bridge_status(state: tauri::State<Arc<State>>) -> String {
    let mut guard = state.bridge_child.lock().unwrap();
    match guard.as_mut() {
        None => "Bridge not found next to the app. Browser extension bridge path will not work; direct downloads in the app still work.".to_string(),
        Some(child) => match child.try_wait() {
            Ok(None) => "running".to_string(),
            Ok(Some(status)) => format!(
                "Bridge stopped (exit {status}). Port 18443 may already be in use, or the bridge failed to start. See bridge.log in the FetchForge data folder. The bridge does not fall back to another port."
            ),
            Err(e) => format!("Bridge status unknown ({e})."),
        },
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Captured-manifest sidecars (round 6): the bridge saves playlist
/// text the extension captured from the player's own response to
/// `data_dir/manifests/<id>.m3u8` at add time, so a signed manifest
/// link is never fetched twice. The desktop worker reads the same
/// sidecar when it plans or runs a record the bridge added (a
/// resume runs here, not in the bridge), and deletes it when the
/// download reaches a terminal state. Saving lives in the bridge
/// only — the desktop never receives manifest text. Mirrors the
/// bridge's helpers; the file layout is shared.
fn manifest_sidecar_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    data_dir.join("manifests").join(format!("{id}.m3u8"))
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

fn data_dir() -> PathBuf {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local).join("FetchForge")
    } else {
        PathBuf::from(".fetchforge")
    }
}

fn download_dir() -> PathBuf {
    if let Ok(home) = std::env::var("USERPROFILE") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("Downloads")
    }
}

/// App settings (round 11 extension, features 3/7/10): mirrors
/// the bridge's Settings field-for-field — both processes read
/// the SAME settings.json in the data dir, so the file is the
/// single source of truth and a save in the UI takes effect on
/// the next download in either process without a restart. Each
/// worker loads once at its start; a change mid-download applies
/// to the NEXT download, never to a running one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Settings {
    download_dir: String,
    max_simultaneous: usize,
    max_segments: usize,
    max_retries: u32,
    bandwidth_kbps: Option<u64>,
    organise_by_category: bool,
    /// User-configured proxy (feature 10): `http(s)://` or
    /// `socks5://`, optionally with credentials. Validated at
    /// save time; NEVER logged anywhere.
    #[serde(default)]
    proxy_url: Option<String>,
    /// Shut the computer down when all downloads finish (feature
    /// 7): explicit opt-in, default OFF.
    #[serde(default)]
    shutdown_when_finished: bool,
    /// Desktop notifications on download completion (feature 8):
    /// default ON.
    #[serde(default = "default_true")]
    notifications_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: download_dir().to_string_lossy().to_string(),
            max_simultaneous: 3,
            max_segments: 4,
            max_retries: 3,
            bandwidth_kbps: None,
            organise_by_category: true,
            proxy_url: None,
            shutdown_when_finished: false,
            notifications_enabled: true,
        }
    }
}

/// Per-field settings fallback (mirrors the bridge's loader): a
/// settings file with one bad field discards only that field,
/// never all settings. A missing file is normal on first run
/// (no warning); a bad proxy value is ignored, never logged.
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
    match obj.get("download_dir") {
        None => {}
        Some(v) => match v.as_str() {
            Some(d) if !d.is_empty() => s.download_dir = d.to_string(),
            _ => bad.push("download_dir"),
        },
    }
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
    match obj.get("proxy_url") {
        None => {}
        Some(serde_json::Value::Null) => s.proxy_url = None,
        Some(v) => match v.as_str() {
            Some(p) if p.trim().is_empty() => s.proxy_url = None,
            Some(p) => match validate_proxy_url(p) {
                Ok(clean) => s.proxy_url = Some(clean),
                Err(_) => bad.push("proxy_url"),
            },
            None => bad.push("proxy_url"),
        },
    }
    match obj.get("shutdown_when_finished") {
        None => {}
        Some(v) => match v.as_bool() {
            Some(b) => s.shutdown_when_finished = b,
            None => bad.push("shutdown_when_finished"),
        },
    }
    match obj.get("notifications_enabled") {
        None => {}
        Some(v) => match v.as_bool() {
            Some(b) => s.notifications_enabled = b,
            None => bad.push("notifications_enabled"),
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

/// Current settings, loaded fresh from settings.json (see the
/// Settings docs): the file on disk is the single source of
/// truth shared with the bridge.
fn current_settings(data_dir: &std::path::Path) -> Settings {
    load_settings_with_warning(data_dir).0
}

/// Persist settings: write to a temp file and atomically rename
/// over settings.json, so a crash mid-save never leaves a
/// corrupt settings file. Mirrors the bridge's save_settings.
fn save_settings(data_dir: &std::path::Path, settings: &Settings) -> Result<(), String> {
    let path = data_dir.join("settings.json");
    let tmp = data_dir.join("settings.json.tmp");
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, &text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Update settings from an update_settings body (mirrors the
/// bridge's method): merge the known fields the body carries
/// into the current settings, validate, and persist. Unknown
/// fields are ignored; a wrongly-typed or invalid field fails
/// the whole save with the field NAME only — the proxy value is
/// never echoed back. Numeric fields are clamped to sane ranges.
fn update_settings_value(
    data_dir: &std::path::Path,
    body: &serde_json::Value,
) -> Result<Settings, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "Settings must be a JSON object".to_string())?;
    let mut s = current_settings(data_dir);
    if let Some(v) = obj.get("download_dir") {
        match v.as_str() {
            Some(d) if !d.trim().is_empty() => s.download_dir = d.trim().to_string(),
            _ => return Err("download_dir must be a non-empty string".to_string()),
        }
    }
    if let Some(v) = obj.get("max_simultaneous") {
        match v.as_u64() {
            Some(n) => s.max_simultaneous = (n as usize).clamp(1, 10),
            None => return Err("max_simultaneous must be a number".to_string()),
        }
    }
    if let Some(v) = obj.get("max_segments") {
        match v.as_u64() {
            Some(n) => s.max_segments = (n as usize).clamp(1, 16),
            None => return Err("max_segments must be a number".to_string()),
        }
    }
    if let Some(v) = obj.get("max_retries") {
        match v.as_u64() {
            Some(n) => s.max_retries = (n as u32).min(10),
            None => return Err("max_retries must be a number".to_string()),
        }
    }
    if let Some(v) = obj.get("bandwidth_kbps") {
        match v {
            serde_json::Value::Null => s.bandwidth_kbps = None,
            _ => match v.as_u64() {
                Some(0) => s.bandwidth_kbps = None,
                Some(n) => s.bandwidth_kbps = Some(n),
                None => return Err("bandwidth_kbps must be a number or null".to_string()),
            },
        }
    }
    if let Some(v) = obj.get("organise_by_category") {
        match v.as_bool() {
            Some(b) => s.organise_by_category = b,
            None => return Err("organise_by_category must be true or false".to_string()),
        }
    }
    if let Some(v) = obj.get("proxy_url") {
        match v {
            serde_json::Value::Null => s.proxy_url = None,
            _ => {
                match v.as_str() {
                    Some(p) if p.trim().is_empty() => s.proxy_url = None,
                    Some(p) => match validate_proxy_url(p) {
                        Ok(clean) => s.proxy_url = Some(clean),
                        Err(_) => return Err(
                            "proxy_url must be an http://, https:// or socks5:// URL with a host"
                                .to_string(),
                        ),
                    },
                    None => return Err("proxy_url must be a string or null".to_string()),
                }
            }
        }
    }
    if let Some(v) = obj.get("shutdown_when_finished") {
        match v.as_bool() {
            Some(b) => s.shutdown_when_finished = b,
            None => return Err("shutdown_when_finished must be true or false".to_string()),
        }
    }
    if let Some(v) = obj.get("notifications_enabled") {
        match v.as_bool() {
            Some(b) => s.notifications_enabled = b,
            None => return Err("notifications_enabled must be true or false".to_string()),
        }
    }
    save_settings(data_dir, &s)?;
    Ok(s)
}

/// The ids this process's workers are currently running
/// (round 10, item 6): `reload_from_disk` protects exactly these
/// from being overwritten by disk state — every other record
/// refreshes from disk, so bridge-driven downloads show their
/// real progress instead of the stale values this process last
/// saw. A worker's job leaves the map when it ends (see
/// `remove_job_if_current`), so the set is exactly the running
/// workers, never stale entries.
fn active_job_ids(state: &State) -> HashSet<String> {
    state.jobs.lock().unwrap().keys().cloned().collect()
}

/// Remove a job from the map when its worker ends — but only if
/// the map still holds THIS job: a resume spawns a fresh job
/// under the same id, and the old worker must not remove the new
/// job. Mirrors the bridge's method of the same name.
fn remove_job_if_current(state: &Arc<State>, id: &str, job: &Arc<Job>) {
    let mut jobs = state.jobs.lock().unwrap();
    if jobs.get(id).is_some_and(|j| Arc::ptr_eq(j, job)) {
        jobs.remove(id);
    }
}

#[tauri::command]
fn list_downloads(state: tauri::State<Arc<State>>) -> Vec<DownloadRecord> {
    let mut store = state.store.lock().unwrap();
    // Pick up downloads added by the bridge (browser extension) since
    // launch; without this the list never shows them until restart.
    // Round 10, item 6: only this process's running workers are
    // protected — every other record refreshes from disk, so the
    // list shows the bridge's real progress.
    store.reload_from_disk(&active_job_ids(&state));
    store.all().into_iter().cloned().collect()
}

// Async so the blocking probe runs on Tauri's async runtime instead of
// the IPC thread: a slow probe used to freeze the whole UI.
// `format_id` is the quality the user picked in the format picker;
// None means the recorded default (the paste-link box also accepts a
// video page directly, without opening the picker first).
// rename_all = "snake_case" is LOAD-BEARING, not style: Tauri's
// #[tauri::command] macro camelCases argument names by default, so
// `format_id` would be looked up as `formatId` in the invoke payload
// — but the frontend sends `format_id` (snake_case, shared with the
// bridge HTTP API). Without this attribute the picker selection was
// silently dropped and every download used the default format
// (found 2026-11-22: direct invoke with format_id "278" recorded
// the default 234; filename, a single word, was unaffected).
#[tauri::command(rename_all = "snake_case")]
async fn add_download(
    state: tauri::State<'_, Arc<State>>,
    url: String,
    filename: Option<String>,
    format_id: Option<String>,
    referer: Option<String>,
    audio_url: Option<String>,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
) -> Result<DownloadRecord, String> {
    add_download_inner(
        &state,
        url,
        filename,
        format_id,
        referer,
        audio_url,
        audio_lang,
        subtitle_langs,
        audio_format,
        None,
    )
}

/// The add logic shared by the `add_download` command, the batch
/// command and the playlist command (round 11 extension, features
/// 5/11): classify the URL (manifest / gltf / page / direct file),
/// probe, and queue. `audio_lang` / `subtitle_langs` /
/// `audio_format` are the picker choices (features 2/4/13);
/// `playlist` tags the record with its playlist title + index
/// (feature 11) and is None for a plain add.
#[allow(clippy::too_many_arguments)]
fn add_download_inner(
    state: &Arc<State>,
    url: String,
    filename: Option<String>,
    format_id: Option<String>,
    referer: Option<String>,
    audio_url: Option<String>,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
    playlist: Option<(String, u32)>,
) -> Result<DownloadRecord, String> {
    let settings = current_settings(&state.data_dir);
    let clean = fetchforge_engine::validate_url(&url)?;
    // HLS (.m3u8) / DASH (.mpd) manifests go straight to the media
    // path (manifest parsing + segment assembly in media.rs): a
    // manifest must never reach the direct path, which would save
    // the manifest text itself as a file.
    if fetchforge_engine::is_manifest_url(&clean) {
        return add_media_download(
            state,
            &clean,
            filename,
            format_id,
            referer,
            audio_url,
            audio_lang,
            subtitle_langs,
            audio_format,
            playlist,
        );
    }
    // glTF models (.gltf) carry dependencies (buffers, textures)
    // that must be downloaded with the model and kept in the same
    // folder structure (spec §6). They go to the 3D engine, never
    // the direct path, which would save the .gltf alone and leave
    // a broken model. Single-file models (GLB, OBJ, ...) take the
    // direct path below with category "3D".
    if is_gltf_url(&clean) {
        return add_gltf_download(state, &clean, filename);
    }
    let probe_opts = DownloadOptions {
        url: clean.clone(),
        referer: referer.clone(),
        proxy_url: settings.proxy_url.clone(),
        ..Default::default()
    };
    let probe_result = probe(&probe_opts);
    // A web page is not a downloadable file — but since v0.2.0 it is no
    // longer refused outright: a page whose probe returns text/html
    // (YouTube, Vimeo, a public Google Drive file, any video page) goes
    // through media extraction instead. Same flow as the bridge's
    // add_download; the two must not drift apart.
    let is_page = probe_result.as_ref().is_ok_and(|p| {
        p.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/html"))
    });
    if is_page {
        return add_media_download(
            state,
            &clean,
            filename,
            format_id,
            referer,
            audio_url,
            audio_lang,
            subtitle_langs,
            audio_format,
            playlist,
        );
    }
    match probe_result {
        Ok(p) => {
            // Extension-less manifests: the probe's content type
            // says HLS/DASH, so this is a stream, not a file.
            if p.content_type
                .as_deref()
                .is_some_and(|ct| fetchforge_engine::stream_kind_of_content_type(ct).is_some())
            {
                return add_media_download(
                    state,
                    &clean,
                    filename,
                    format_id,
                    referer,
                    audio_url,
                    audio_lang,
                    subtitle_langs,
                    audio_format,
                    playlist,
                );
            }
            // Round 10, item 4: some playlist endpoints serve a
            // generic content type, so the check above misses them
            // (MoviesJoy/NOXX/123Movies saved ~940 B of playlist
            // text as a file). Sniff the first bytes before
            // treating the URL as a direct file.
            if sniff_stream_kind_with_proxy(&clean, &referer, settings.proxy_url.as_deref())
                .is_some()
            {
                return add_media_download(
                    state,
                    &clean,
                    filename,
                    format_id,
                    referer,
                    audio_url,
                    audio_lang,
                    subtitle_langs,
                    audio_format,
                    playlist,
                );
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
            if p.content_type.as_deref().is_some_and(is_image_content_type) {
                filename = ensure_image_extension(&filename, p.content_type.as_deref());
            }
            queue_direct_download(
                state,
                clean,
                filename,
                total,
                resumable,
                referer,
                audio_format,
            )
        }
        Err(mut e) => {
            // Probe failed: try media extraction as a fallback, but
            // ONLY if yt-dlp is already installed (see the bridge for
            // why a dead direct link must never trigger a first-use
            // yt-dlp download). When extraction is tried and fails,
            // its error diagnoses the problem (yt-dlp actually read
            // the page) — the probe error is replaced by it, not
            // recorded. Mirrors the bridge's probe-failure branch.
            if find_ytdlp(&state.data_dir).is_ok() {
                match try_media_download(
                    state,
                    &clean,
                    filename.clone(),
                    format_id.clone(),
                    referer.clone(),
                    audio_url.clone(),
                    audio_lang.clone(),
                    subtitle_langs.clone(),
                    audio_format.clone(),
                    playlist.clone(),
                ) {
                    Ok(rec) => return Ok(rec),
                    Err(extract_err) => e = extract_err,
                }
            }
            let rec = failed_record(
                state,
                clean,
                filename,
                e,
                audio_lang,
                subtitle_langs,
                audio_format,
                playlist,
            );
            Ok(rec)
        }
    }
}

/// A Failed record, saved so the user sees the reason.
fn failed_record(
    state: &Arc<State>,
    url: String,
    filename: Option<String>,
    error: String,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
    playlist: Option<(String, u32)>,
) -> DownloadRecord {
    let (playlist_title, playlist_index) = match playlist {
        Some((title, index)) => (Some(title), Some(index)),
        None => (None, None),
    };
    let rec = DownloadRecord {
        id: generate_download_id(),
        url,
        filename: filename.unwrap_or_else(|| "download".to_string()),
        destination: state.download_dir.to_string_lossy().to_string(),
        category: "Other".to_string(),
        status: Status::Failed,
        total_bytes: None,
        downloaded_bytes: 0,
        resumable: false,
        error: Some(error),
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
        audio_only: false,
        picked_height: None,
        picked_vbr_kbps: None,
        delivered_height: None,
        delivered_bitrate_kbps: None,
        quality_note: None,
        speed_bps: None,
        size_kind: None,
        audio_lang,
        subtitle_langs,
        audio_format,
        format_kind: None,
        playlist: playlist_title,
        playlist_index,
        downgraded_from: None,
        downgrade_attempted: false,
    };
    state.store.lock().unwrap().upsert(rec.clone());
    let _ = state.store.lock().unwrap().save();
    rec
}

/// Media path for the desktop app (mirrors the bridge): queue the
/// download; extraction runs in the worker (see
/// [`plan_pending_download`]). A queueing failure records a Failed
/// row; the Google Drive direct-download fallback lives in the
/// worker's planning step.
fn add_media_download(
    state: &Arc<State>,
    clean_url: &str,
    filename: Option<String>,
    format_id: Option<String>,
    referer: Option<String>,
    audio_url: Option<String>,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
    playlist: Option<(String, u32)>,
) -> Result<DownloadRecord, String> {
    match try_media_download(
        state,
        clean_url,
        filename.clone(),
        format_id,
        referer.clone(),
        audio_url,
        audio_lang.clone(),
        subtitle_langs.clone(),
        audio_format.clone(),
        playlist.clone(),
    ) {
        Ok(rec) => Ok(rec),
        Err(e) => {
            // Extraction now runs in the worker (see
            // try_media_download), so this arm is only reached for
            // queueing failures.
            Ok(failed_record(
                state,
                clean_url.to_string(),
                filename,
                e,
                audio_lang,
                subtitle_langs,
                audio_format,
                playlist,
            ))
        }
    }
}

/// glTF path for the desktop app (mirrors the bridge): plan the
/// model + dependencies at add time, queue the bundle download. A
/// plan failure records a Failed row with the reason.
fn add_gltf_download(
    state: &Arc<State>,
    clean_url: &str,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
    match try_gltf_download(state, clean_url, filename.clone()) {
        Ok(rec) => Ok(rec),
        Err(e) => Ok(failed_record(
            state,
            clean_url.to_string(),
            filename,
            e,
            None,
            Vec::new(),
            None,
            None,
        )),
    }
}

fn try_gltf_download(
    state: &Arc<State>,
    clean_url: &str,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
    let settings = current_settings(&state.data_dir);
    let plan = fetch_gltf_plan_with_proxy(clean_url, settings.proxy_url.as_deref())?;
    let record_filename = filename
        .as_deref()
        .map(sanitize_filename)
        .unwrap_or_else(|| plan.filename.clone());
    // Same idempotent-add rule as the other paths (see
    // Store::find_duplicate): an already-active or already-completed
    // download for the same URL + filename is returned, never
    // queued twice.
    {
        let mut store = state.store.lock().unwrap();
        // Fresh disk state before the duplicate check: the bridge
        // may have cleared this record since this process last
        // loaded — a stale in-memory match would return a download
        // the user already removed instead of queueing the new one
        // (2026-11-22 incident).
        store.reload_from_disk(&active_job_ids(state));
        if let Some(mut existing) = store.find_duplicate(
            clean_url,
            &record_filename,
            &state.download_dir.to_string_lossy(),
            false,
        ) {
            // The caller must know nothing new was queued: flag
            // the returned clone (never the stored record) so the
            // extension reports "Already downloaded" instead of
            // claiming a fresh download was sent.
            existing.duplicate = true;
            return Ok(existing);
        }
    }
    let id = generate_download_id();
    let rec = DownloadRecord {
        id: id.clone(),
        url: clean_url.to_string(),
        filename: record_filename,
        destination: state.download_dir.to_string_lossy().to_string(),
        category: "3D".to_string(),
        status: Status::Queued,
        total_bytes: plan.total_bytes,
        downloaded_bytes: 0,
        // The bundle downloader skips files already present at their
        // target, so an interrupted bundle can be resumed by re-adding it.
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
        audio_only: false,
        picked_height: None,
        picked_vbr_kbps: None,
        delivered_height: None,
        delivered_bitrate_kbps: None,
        quality_note: None,
        speed_bps: None,
        size_kind: None,
        audio_lang: None,
        subtitle_langs: Vec::new(),
        audio_format: None,
        format_kind: None,
        playlist: None,
        playlist_index: None,
        downgraded_from: None,
        downgrade_attempted: false,
    };
    {
        let mut store = state.store.lock().unwrap();
        store.upsert(rec.clone());
        let _ = store.save();
    }
    spawn_download_worker(state, id);
    Ok(rec)
}

/// Queue a media download WITHOUT planning it first: the record is
/// inserted immediately (stream_kind "pending") and the worker
/// plans it (see [`plan_pending_download`]). Same as the bridge:
/// planning used to run synchronously in the add call, and a slow
/// extraction left the UI hanging; the record now appears at once
/// and a planning failure surfaces as a Failed row with the
/// reason. The requested format id is stored on the record as the
/// planning input (the worker overwrites it with the chosen
/// format); the filename is the caller's, or the provisional
/// "download" the worker replaces with the planned name.
fn try_media_download(
    state: &Arc<State>,
    clean_url: &str,
    filename: Option<String>,
    format_id: Option<String>,
    referer: Option<String>,
    audio_url: Option<String>,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
    playlist: Option<(String, u32)>,
) -> Result<DownloadRecord, String> {
    let provisional = filename
        .as_deref()
        .map(sanitize_filename)
        .unwrap_or_else(|| "download".to_string());
    // Same idempotent-add rule as the bridge (see
    // Store::find_duplicate): an already-active or
    // already-completed download for the same URL + filename is
    // returned, never queued twice. The worker re-checks after
    // planning, when the resolved URL and planned filename are
    // known.
    {
        let mut store = state.store.lock().unwrap();
        // Fresh disk state before the duplicate check (see the
        // glTF add above): a record the bridge cleared must not
        // match from stale memory.
        store.reload_from_disk(&active_job_ids(state));
        if let Some(mut existing) = store.find_duplicate(
            clean_url,
            &provisional,
            &state.download_dir.to_string_lossy(),
            false,
        ) {
            // The caller must know nothing new was queued: flag
            // the returned clone (never the stored record) so the
            // extension reports "Already downloaded" instead of
            // claiming a fresh download was sent.
            existing.duplicate = true;
            return Ok(existing);
        }
    }
    let id = generate_download_id();
    let (playlist_title, playlist_index) = match playlist {
        Some((title, index)) => (Some(title), Some(index)),
        None => (None, None),
    };
    let rec = DownloadRecord {
        id: id.clone(),
        url: clean_url.to_string(),
        filename: provisional,
        destination: state.download_dir.to_string_lossy().to_string(),
        category: "Other".to_string(),
        status: Status::Queued,
        total_bytes: None,
        downloaded_bytes: 0,
        // yt-dlp resumes from its own .part files.
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
        audio_only: false,
        picked_height: None,
        picked_vbr_kbps: None,
        delivered_height: None,
        delivered_bitrate_kbps: None,
        quality_note: None,
        speed_bps: None,
        size_kind: None,
        audio_lang,
        subtitle_langs,
        audio_format,
        format_kind: None,
        playlist: playlist_title,
        playlist_index,
        downgraded_from: None,
        downgrade_attempted: false,
    };
    {
        let mut store = state.store.lock().unwrap();
        store.upsert(rec.clone());
        let _ = store.save();
    }
    spawn_download_worker(state, id);
    Ok(rec)
}

/// Plan a "pending" record in the worker (mirrors the bridge):
/// run the plan (or the Google Drive direct-download fallback),
/// apply the paired HLS override, re-check duplicates against the
/// resolved URL, and update the record in place. Returns the
/// updated record to dispatch on; Ok(None) means a duplicate was
/// found after planning and this record was removed (the existing
/// download covers it); Err means planning failed and the record
/// was marked Failed with the reason. A record stuck in "pending"
/// (app closed mid-plan) is planned the same way by any worker
/// that picks it up, including a resume.
fn plan_pending_download(
    state: &Arc<State>,
    rec: &DownloadRecord,
    job: &Job,
) -> Result<Option<DownloadRecord>, String> {
    let id = rec.id.clone();
    let settings = current_settings(&state.data_dir);
    // The provisional filename ("download") means no name was
    // requested: let the plan derive it from the title. Any other
    // stored filename was the caller's and is kept as the plan's
    // stem.
    let filename_arg = if rec.filename == "download" {
        None
    } else {
        Some(rec.filename.as_str())
    };
    // Captured playlist text (extension sends only, saved by the
    // bridge at add time): the plan fetch is the FIRST fetch of
    // the manifest, so it must use the capture — a single-use
    // signed link consumed here would fail again at run time.
    let captured = read_manifest_sidecar(&state.data_dir, &id, &rec.url);
    let outcome = plan_media_download_or_drive_fallback_with_options(
        &state.data_dir,
        &rec.url,
        rec.format_id.as_deref(),
        filename_arg,
        &rec.referer,
        captured.as_ref(),
        rec.audio_lang.as_deref(),
        settings.proxy_url.as_deref(),
    );
    if job.cancelled.load(Ordering::Relaxed) {
        set_status(state, &id, Status::Cancelled, None);
        return Err("download cancelled".to_string());
    }
    match outcome {
        Ok(PlanOutcome::Media(mut plan)) => {
            // Paired HLS variants (extension observed a video
            // variant AND its audio sibling, no master): the plan
            // from the video variant alone would be a silent .ts
            // with no merge. The worker downloads both tracks and
            // merges them, so the plan must say so: needs_merge,
            // and an .mp4 filename (the merge output). Only for
            // HLS: DASH keeps its audio inside the manifest, so an
            // audio URL with a DASH plan is ignored.
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
            // Subtitles on a muxed HLS variant (round 11
            // extension, feature 2): the variant downloads as
            // one .ts file, so the subtitle track can only join
            // it through an ffmpeg merge — mark the plan for a
            // merge so the worker resolves ffmpeg upfront. Same
            // as the bridge: the plan keeps the .ts name (the
            // subtitle may not exist in the master), and the
            // completion step reconciles the filename after the
            // engine re-containerises to .mp4 at merge time.
            if !rec.subtitle_langs.is_empty()
                && plan.stream_kind == Some(fetchforge_engine::StreamKind::Hls)
                && !plan.needs_merge
            {
                plan.needs_merge = true;
            }
            // The HTML fallback may have resolved the page to an
            // embedded stream/file URL: the record and the worker
            // both use the resolved URL, not the page URL.
            let record_url = plan
                .effective_url
                .clone()
                .unwrap_or_else(|| rec.url.clone());
            let mut store = state.store.lock().unwrap();
            // Fresh disk state before the plan-time duplicate
            // check: the removal below must only fire when the
            // matched record is CONFIRMED still on disk (same
            // 2026-11-22 incident as the bridge: a new download
            // vanished when the plan matched a cleared clip in
            // stale memory).
            store.reload_from_disk(&active_job_ids(state));
            if store
                .find_duplicate_except(
                    &record_url,
                    &plan.filename,
                    &state.download_dir.to_string_lossy(),
                    &id,
                    rec.audio_only,
                )
                .is_some()
            {
                store.remove(&id);
                let _ = store.save();
                drop(store);
                // The record is gone, so its sidecar is orphaned
                // — delete it (set_status never runs for a removed
                // record).
                delete_manifest_sidecar(&state.data_dir, &id);
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
            // Round 11, item 10: how the plan total was
            // determined — the UI shows the marker.
            updated.size_kind = plan.size_kind;
            updated.format_id = Some(plan.format_id);
            updated.needs_merge = plan.needs_merge;
            updated.format_selector = Some(plan.selector);
            updated.stream_kind = plan.stream_kind.map(|k| k.as_str().to_string());
            // Round 11 extension, feature 13: what kind of
            // format was planned — the worker passes
            // --extract-audio --audio-format to yt-dlp only
            // when the chosen format is audio-only.
            updated.format_kind = plan.format_kind;
            updated.audio_url = paired_audio;
            // HLS/DASH only: the manifest's segment count, so the
            // UI can show "0 / N pieces" before the first segment
            // lands. yt-dlp plans carry None.
            updated.segments_total = plan.segments_total;
            // Round 10, item 8: what the user picked, so the
            // delivered file can be probed against it.
            updated.picked_height = plan.picked_height;
            updated.picked_vbr_kbps = plan.picked_vbr_kbps;
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
            // record to a direct download and let it dispatch down
            // the direct path.
            let mut store = state.store.lock().unwrap();
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
            set_status(state, &id, Status::Failed, Some(e));
            Err("download failed".to_string())
        }
    }
}

/// Queue a direct file download (the pre-0.2.0 path, unchanged except
/// for the new record fields).
fn queue_direct_download(
    state: &Arc<State>,
    clean: String,
    filename: String,
    total: Option<u64>,
    resumable: bool,
    referer: Option<String>,
    audio_format: Option<String>,
) -> Result<DownloadRecord, String> {
    // Same idempotent-add rule as the bridge (see
    // Store::find_duplicate): an already-active or
    // already-completed download for the same URL + filename is
    // returned, never queued twice (two workers would share one
    // .part path and corrupt each other). This check runs before
    // `clean` is moved into the new record below, so the same value
    // is never used after a move (E0382 fix).
    {
        let mut store = state.store.lock().unwrap();
        // Fresh disk state before the duplicate check (see the
        // glTF add above): a record the bridge cleared must not
        // match from stale memory.
        store.reload_from_disk(&active_job_ids(state));
        if let Some(mut existing) = store.find_duplicate(
            &clean,
            &filename,
            &state.download_dir.to_string_lossy(),
            false,
        ) {
            // The caller must know nothing new was queued: flag
            // the returned clone (never the stored record) so the
            // extension reports "Already downloaded" instead of
            // claiming a fresh download was sent.
            existing.duplicate = true;
            return Ok(existing);
        }
    }
    let id = generate_download_id();
    let rec = DownloadRecord {
        id: id.clone(),
        url: clean,
        filename: filename.clone(),
        destination: state.download_dir.to_string_lossy().to_string(),
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
        audio_only: false,
        picked_height: None,
        picked_vbr_kbps: None,
        delivered_height: None,
        delivered_bitrate_kbps: None,
        quality_note: None,
        speed_bps: None,
        // Round 11, item 10: a probed Content-Length is the
        // exact total.
        size_kind: total.map(|_| SizeKind::Exact),
        audio_lang: None,
        subtitle_langs: Vec::new(),
        audio_format,
        format_kind: None,
        playlist: None,
        playlist_index: None,
        downgraded_from: None,
        downgrade_attempted: false,
    };
    {
        let mut store = state.store.lock().unwrap();
        store.upsert(rec.clone());
        let _ = store.save();
    }
    spawn_download_worker(state, id);
    Ok(rec)
}

fn spawn_download_worker(state: &Arc<State>, id: String) {
    let state2 = Arc::clone(state);
    let job = Arc::new(Job {
        paused: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    state
        .jobs
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::clone(&job));
    std::thread::spawn(move || {
        run_download(&state2, &id, &job);
        // Round 10, item 6: the job leaves the map when its worker
        // ends (paused workers too — resume spawns a fresh job),
        // so `active_job_ids` stays exactly the running workers.
        remove_job_if_current(&state2, &id, &job);
        // Round 11 extension, feature 7: shutdown when finished
        // — the helper only fires after a COMPLETED download with
        // nothing else remaining.
        maybe_shutdown_after_completion(&state2, &id);
    });
}

/// List the qualities available for a video page (format picker).
/// Async so the yt-dlp extraction (up to 90 s on a slow site) does not
/// block the IPC thread. The optional referer is the page the
/// stream came from (round 11, item 5): the engine forwards it as
/// the Referer header on the manifest fetch — vidrift-class hosts
/// 403 without it. The desktop's own listing calls pass None
/// (a pasted link has no page context); the bridge endpoint is
/// the one the extension's popup uses.
#[tauri::command]
async fn media_formats(
    state: tauri::State<'_, Arc<State>>,
    url: String,
    referer: Option<String>,
) -> Result<MediaInfo, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    let settings = current_settings(&state.data_dir);
    list_formats_with_referer_and_proxy(
        &state.data_dir,
        &clean,
        referer,
        settings.proxy_url.as_deref(),
    )
}

/// Best-effort delivered-quality probe (round 10, item 8): probe
/// the finished file with ffmpeg (when one is already available —
/// never downloaded for this) and record its real resolution /
/// bitrate, plus a mismatch note when the site mislabelled the
/// stream. A missing ffmpeg or a failed probe leaves the record
/// unchanged: the download already completed, and a probe failure
/// must never fail it. Only media downloads carry picked values,
/// so direct/gltf records skip the probe entirely. Mirrors the
/// bridge's method of the same name.
fn probe_delivered_quality(
    state: &Arc<State>,
    r: &mut DownloadRecord,
    final_path: &std::path::Path,
) {
    if r.picked_height.is_none() && r.picked_vbr_kbps.is_none() {
        return;
    }
    if let Ok(ffmpeg) = find_ffmpeg(&state.data_dir) {
        if let Some(q) = probe_video_quality(&ffmpeg, final_path) {
            r.apply_delivered_quality(q.height, q.bitrate_kbps);
        }
    }
}

fn set_status(state: &Arc<State>, id: &str, status: Status, error: Option<String>) {
    // Terminal state (Completed/Failed/Cancelled): the captured
    // manifest sidecar is no longer needed — a resume re-plans
    // from the stored format, and a finished download never
    // fetches its manifest again. Paused keeps the sidecar: a
    // resume re-fetches the manifest, and a single-use signed
    // link still needs the capture. Mirrors the bridge's
    // set_status. (Status is not Copy, so the check runs before
    // the move into the record.)
    let terminal = matches!(
        status,
        Status::Completed | Status::Failed | Status::Cancelled
    );
    let mut store = state.store.lock().unwrap();
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
        delete_manifest_sidecar(&state.data_dir, id);
    }
}

/// "Audio only" post-step (mirrors the bridge's): turn a
/// downloaded video file into its audio track, in the format the
/// user chose (round 11 extension, feature 13: "mp3", "opus" or
/// "m4a"). A file already audio in the CHOSEN format passes
/// through unchanged; a file in a different audio format is
/// converted; a video file is extracted to `<stem>.<format>` and
/// the video is deleted. On extraction failure the video file is
/// KEPT and the error is returned, so the caller can mark the
/// record Failed with a clear reason instead of completing a
/// download whose audio file does not exist. Needed here because
/// a bridge-added audio-only download may be RESUMED in this
/// process — the desktop workers previously had no audio-only
/// handling at all, so a resumed audio download completed as a
/// video.
fn audio_only_post_step(
    state: &Arc<State>,
    video_path: &std::path::Path,
    ffmpeg: Option<&std::path::Path>,
    format: &str,
) -> Result<PathBuf, String> {
    let ext = video_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == format {
        return Ok(video_path.to_path_buf());
    }
    let ffmpeg_path;
    let ffmpeg = match ffmpeg {
        Some(f) => f,
        None => {
            ffmpeg_path = ensure_ffmpeg(&state.data_dir)?;
            &ffmpeg_path
        }
    };
    let stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let dir = video_path.parent().unwrap_or(std::path::Path::new("."));
    let audio_path = unique_path(dir, &format!("{stem}.{format}"));
    extract_audio_as(ffmpeg, video_path, &audio_path, format)?;
    // Extraction succeeded: the video file is no longer needed.
    let _ = std::fs::remove_file(video_path);
    Ok(audio_path)
}

/// Retry at the next lower quality after a failure (round 11
/// extension, feature 9; mirrors the bridge's try_downgrade).
/// Returns true when a downgrade was queued: the record is reset
/// to "pending" with the lower format id and a fresh worker is
/// spawned to re-plan and download it. One downgrade only, only
/// for a download that had a picked quality, never for direct or
/// gltf downloads, never on pause/cancel (the caller checks
/// `stop_reason` first), never on a planning failure. For yt-dlp
/// downloads the old .part files are deleted first, or yt-dlp
/// would resume the HIGHER quality's fragments under the same
/// filename.
fn try_downgrade(state: &Arc<State>, rec: &DownloadRecord) -> bool {
    let Some(current) = state.store.lock().unwrap().get(&rec.id).cloned() else {
        return false;
    };
    if current.downgrade_attempted || current.format_id.is_none() {
        return false;
    }
    let Some(height) = current.picked_height else {
        return false;
    };
    if current.stream_kind.as_deref() == Some("gltf") {
        return false;
    }
    let settings = current_settings(&state.data_dir);
    let info = match list_formats_with_referer_and_proxy(
        &state.data_dir,
        &current.url,
        current.referer.clone(),
        settings.proxy_url.as_deref(),
    ) {
        Ok(i) => i,
        Err(_) => return false,
    };
    let Some(lower) = next_lower_format(&info.formats, height) else {
        return false;
    };
    if current.stream_kind.is_none() {
        let mut dir = state.download_dir.clone();
        if settings.organise_by_category {
            dir = dir.join(category_for_filename(&current.filename));
        }
        let (stem, ext) = match current.filename.rsplit_once('.') {
            Some((s, e)) => (s, e),
            None => (current.filename.as_str(), ""),
        };
        remove_partial_files(&dir, stem, ext);
    }
    let old_label = format!("{height}p");
    let new_format_id = lower.format_id.clone();
    let new_label = lower.label.clone();
    {
        let mut store = state.store.lock().unwrap();
        if let Some(mut r) = store.get(&current.id).cloned() {
            r.reset_for_downgrade(new_format_id, old_label);
            store.upsert(r);
            let _ = store.save();
        } else {
            return false;
        }
    }
    eprintln!(
        "FetchForge: download {} failed at {height}p; retrying at lower quality ({new_label})",
        current.id
    );
    spawn_download_worker(state, current.id.clone());
    true
}

/// Shut the computer down when all downloads are finished (round
/// 11 extension, feature 7; mirrors the bridge's
/// maybe_shutdown_after_completion). Called after a worker ends.
/// Fires ONLY when ALL of these hold: the setting is on; the
/// just-ended download COMPLETED (a pause or cancel never
/// triggers it); no Queued, Downloading or Paused record remains
/// (fresh disk state — the bridge may have queued work this
/// process has not seen); no other worker is running in this
/// process (the jobs map is empty — the just-ended worker's job
/// was already removed); and the shutdown has not already fired
/// in this process. Windows runs `shutdown /s /t 60` (a
/// 60-second abort window); anywhere else logs a warning.
fn maybe_shutdown_after_completion(state: &Arc<State>, id: &str) {
    if state.shutdown_triggered.load(Ordering::Relaxed) {
        return;
    }
    if !current_settings(&state.data_dir).shutdown_when_finished {
        return;
    }
    let completed = {
        let store = state.store.lock().unwrap();
        store.get(id).is_some_and(|r| r.status == Status::Completed)
    };
    if !completed {
        return;
    }
    let remaining = {
        let mut store = state.store.lock().unwrap();
        store.reload_from_disk(&active_job_ids(state));
        store.all().iter().any(|r| {
            matches!(
                r.status,
                Status::Queued | Status::Downloading | Status::Paused
            )
        })
    };
    if remaining {
        return;
    }
    if !state.jobs.lock().unwrap().is_empty() {
        return;
    }
    if state
        .shutdown_triggered
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    #[cfg(windows)]
    {
        eprintln!(
            "FetchForge: all downloads finished; shutting down in 60 seconds (shutdown /s /t 60)."
        );
        let _ = std::process::Command::new("shutdown")
            .args(["/s", "/t", "60"])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        eprintln!("FetchForge WARNING: shutdown_when_finished is on and all downloads finished, but shutdown is only implemented on Windows; not shutting down.");
    }
}

fn run_download(state: &Arc<State>, id: &str, job: &Job) {
    let mut rec = match state.store.lock().unwrap().get(id) {
        Some(r) => r.clone(),
        None => return,
    };
    // Deferred planning (see try_media_download): a "pending"
    // record has not been extracted yet. Plan it here, in the
    // worker, then dispatch on the planned record. A duplicate
    // found after planning removes this record (Ok(None)); a
    // planning failure already marked it Failed.
    if rec.stream_kind.as_deref() == Some("pending") {
        match plan_pending_download(state, &rec, job) {
            Ok(Some(planned)) => rec = planned,
            Ok(None) => return,
            Err(_) => return,
        }
    }
    // Stream downloads (HLS/DASH) run through media.rs; yt-dlp
    // media downloads (a format was chosen) run through yt-dlp;
    // everything else takes the direct path, exactly as before.
    // An unknown stream kind fails honestly rather than being
    // misrouted to the wrong engine.
    match rec.stream_kind.as_deref() {
        Some("hls") => {
            run_stream_hls_download(state, &rec, job);
            return;
        }
        Some("dash") => {
            run_stream_dash_download(state, &rec, job);
            return;
        }
        // SCOPE NOTE (Phase 7): same as the bridge — a `.gltf` bundle
        // cannot be downloaded at add time without losing progress,
        // pause, and resume, and it cannot run through the direct
        // path, which downloads one file. This arm mirrors the
        // hls/dash arms; the work lives in the engine (threed.rs).
        Some("gltf") => {
            run_gltf_download(state, &rec, job);
            return;
        }
        Some(other) => {
            set_status(
                state,
                id,
                Status::Failed,
                Some(format!(
                    "Stream downloads of this kind ({other}) are not supported yet."
                )),
            );
            return;
        }
        None => {}
    }
    if rec.format_id.is_some() {
        run_media_download(state, &rec, job);
        return;
    }
    set_status(state, id, Status::Downloading, None);
    let settings = current_settings(&state.data_dir);
    // "Audio only": resolve ffmpeg UPFRONT, so a missing ffmpeg
    // fails fast instead of after a full video download (same
    // fail-fast pattern as the bridge). The desktop previously
    // had no audio-only handling here at all: a bridge-added
    // audio download resumed in this process completed as a
    // video.
    let ffmpeg = if rec.audio_only {
        match ensure_ffmpeg(&state.data_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                set_status(state, id, Status::Failed, Some(e));
                return;
            }
        }
    } else {
        None
    };
    let opts = DownloadOptions {
        url: rec.url.clone(),
        destination_dir: state.download_dir.clone(),
        filename: Some(rec.filename.clone()),
        max_segments: settings.max_segments,
        max_retries: settings.max_retries,
        bandwidth_kbps: settings.bandwidth_kbps,
        referer: rec.referer.clone(),
        proxy_url: settings.proxy_url.clone(),
        user_agent: None,
        organise_by_category: settings.organise_by_category,
    };
    let id2 = id.to_string();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
            // Round 10, items 2/6: monotonic progress — a later
            // report never moves a counter backwards (the Plex
            // audio phase restarted its segment counter at 0),
            // and a report that carries no segment value never
            // wipes the plan-time count.
            r.apply_progress(&p);
            store.upsert(r);
            let _ = store.save();
        }
    };
    match download(&opts, &progress, &job.paused, &job.cancelled) {
        Ok(final_path) => {
            // "Audio only": extract the audio track BEFORE the
            // record is marked Completed, so the list points at
            // the audio file. A failed extraction keeps the video
            // file and fails the record with the reason.
            let final_path = if rec.audio_only {
                match audio_only_post_step(
                    state,
                    &final_path,
                    ffmpeg.as_deref(),
                    rec.audio_extension(),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        set_status(state, id, Status::Failed, Some(e));
                        return;
                    }
                }
            } else {
                final_path
            };
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(id).cloned() {
                // Round 10, item 2: completion reconciles against
                // the real file size — the file on disk is the
                // truth (novahd: 121 MB shown for a 3.37 GB file).
                r.complete_with_file(&final_path);
                store.upsert(r);
                let _ = store.save();
            }
        }
        Err(e) => {
            if let Some(reason) = stop_reason(&e) {
                match reason {
                    fetchforge_engine::download::StopReason::Paused => {
                        set_status(state, id, Status::Paused, None)
                    }
                    fetchforge_engine::download::StopReason::Cancelled => {
                        set_status(state, id, Status::Cancelled, None)
                    }
                }
            } else {
                set_status(state, id, Status::Failed, Some(e));
            }
        }
    }
}

/// Run a media download (yt-dlp). Mirrors the bridge's run_media:
/// the filename stem comes from the record so a resume re-runs yt-dlp
/// with the same output template and picks up its own .part files,
/// and the record's filename is corrected to the real file name on
/// completion (a merge may pick a different container than planned).
fn run_media_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let settings = current_settings(&state.data_dir);
    // "Audio only" with a VIDEO format (round 11 extension,
    // feature 13): yt-dlp downloads the video and the post-step
    // extracts the audio — resolve ffmpeg UPFRONT so a missing
    // ffmpeg fails fast. An audio FORMAT needs no post-step:
    // yt-dlp extracts to the chosen format itself.
    let needs_audio_post_step = rec.audio_only && rec.format_kind.as_deref() != Some("audio");
    let ffmpeg = if needs_audio_post_step {
        match ensure_ffmpeg(&state.data_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                set_status(state, &id, Status::Failed, Some(e));
                return;
            }
        }
    } else {
        None
    };
    let stem = std::path::Path::new(&rec.filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "video".to_string());
    let ext = std::path::Path::new(&rec.filename)
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".to_string());
    // The selector was computed at plan time and stored on the
    // record; rebuild a plain one only for records written before
    // the field existed.
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
        destination_dir: state.download_dir.clone(),
        filename_stem: stem,
        expected_ext: ext,
        expected_total: rec.total_bytes,
        organise_by_category: settings.organise_by_category,
        referer: rec.referer.clone(),
        subtitle_langs: rec.subtitle_langs.clone(),
        audio_format: if rec.format_kind.as_deref() == Some("audio") {
            Some(
                rec.audio_format
                    .clone()
                    .unwrap_or_else(|| "m4a".to_string()),
            )
        } else {
            None
        },
        proxy_url: settings.proxy_url.clone(),
        bandwidth_kbps: settings.bandwidth_kbps,
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
            // Round 10, items 2/6: monotonic progress — a later
            // report never moves a counter backwards (the Plex
            // audio phase restarted its segment counter at 0),
            // and a report that carries no segment value never
            // wipes the plan-time count.
            r.apply_progress(&p);
            store.upsert(r);
            let _ = store.save();
        }
    };
    match download_media(
        &state.data_dir,
        &opts,
        &progress,
        &job.paused,
        &job.cancelled,
    ) {
        Ok(final_path) => {
            // "Audio only" with a video format: extract the audio
            // track BEFORE the record is marked Completed, so the
            // list points at the audio file (see run_download).
            let final_path = if needs_audio_post_step {
                match audio_only_post_step(
                    state,
                    &final_path,
                    ffmpeg.as_deref(),
                    rec.audio_extension(),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        set_status(state, &id, Status::Failed, Some(e));
                        return;
                    }
                }
            } else {
                final_path
            };
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                // Round 10, item 2: completion reconciles against
                // the real file size — the file on disk is the
                // truth (novahd: 121 MB shown for a 3.37 GB file).
                r.complete_with_file(&final_path);
                probe_delivered_quality(state, &mut r, &final_path);
                store.upsert(r);
                let _ = store.save();
            }
        }
        Err(e) => {
            if let Some(reason) = stop_reason(&e) {
                match reason {
                    fetchforge_engine::download::StopReason::Paused => {
                        set_status(state, &id, Status::Paused, None)
                    }
                    fetchforge_engine::download::StopReason::Cancelled => {
                        set_status(state, &id, Status::Cancelled, None)
                    }
                }
            } else if try_downgrade(state, rec) {
                // A lower quality was queued (round 11 extension,
                // feature 9): the record is back in the queue
                // with a fresh worker, so this worker ends
                // cleanly instead of failing it.
            } else {
                set_status(state, &id, Status::Failed, Some(e));
            }
        }
    }
}

/// Run an HLS download (media.rs). Mirrors the bridge's
/// run_stream_hls: the record's format_id is the chosen variant
/// playlist URL (or the manifest URL itself for a plain media
/// playlist), and ffmpeg is resolved only when the variant needs a
/// merge (separate audio track).
fn run_stream_hls_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let settings = current_settings(&state.data_dir);
    let mut dir = state.download_dir.clone();
    if settings.organise_by_category {
        // Round 13 follow-up, item 6: the folder is decided by
        // what the download IS — an audio-only record goes to
        // Audio/ even when it still carries the video format's
        // filename (the NOXX case: a combined format picked
        // audio-only). Mirrors the bridge's run_stream_hls.
        dir = dir.join(category_for_download(&rec.filename, rec.audio_only));
    }
    let variant_uri = rec.format_id.clone().unwrap_or_else(|| rec.url.clone());
    let ffmpeg = if rec.needs_merge || rec.audio_only {
        match ensure_ffmpeg(&state.data_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                set_status(state, &id, Status::Failed, Some(e));
                return;
            }
        }
    } else {
        None
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
            // Round 10, items 2/6: monotonic progress — a later
            // report never moves a counter backwards (the Plex
            // audio phase restarted its segment counter at 0),
            // and a report that carries no segment value never
            // wipes the plan-time count.
            r.apply_progress(&p);
            store.upsert(r);
            let _ = store.save();
        }
    };
    // Paired variants (the extension saw a video variant and its
    // audio sibling, no master): download both tracks and merge
    // them. Otherwise the normal variant path, with the page
    // Referer on every manifest/segment request. Mirrors the
    // bridge's run_stream_hls. The captured manifest (extension
    // sends only) is used for whichever fetch its URL matches, so
    // a single-use signed link is never re-fetched here.
    //
    // "Audio only" never touches the video track: a paired audio
    // sibling downloads alone, and a master variant resolves its
    // audio group (or falls back to the muxed variant, whose
    // audio the post-step extracts). The desktop previously had
    // NO audio-only branch here — a bridge-added audio download
    // resumed in this process downloaded the video.
    //
    // Round 11 extension, features 2/3/4/10/12: the popup's
    // language and subtitle choices, the global speed limit, the
    // proxy, and the title metadata all travel in one options
    // struct. Subtitles are skipped for audio-only downloads.
    let stream_opts = StreamDownloadOptions {
        audio_language: rec.audio_lang.clone(),
        subtitle_language: if rec.audio_only {
            None
        } else {
            rec.subtitle_langs.first().cloned()
        },
        proxy_url: settings.proxy_url.clone(),
        bandwidth_kbps: settings.bandwidth_kbps,
        title: Some(
            std::path::Path::new(&rec.filename)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| rec.filename.clone()),
        ),
    };
    let captured = read_manifest_sidecar(&state.data_dir, &id, &rec.url);
    let result = if rec.audio_only {
        match &rec.audio_url {
            // Round 13 follow-up, item 6: the paired audio
            // sibling downloads alone through the audio-only
            // variant entry point, so it lands in Audio/ even
            // when the caller computed Video/ — the plain
            // variant entry point cannot tell this playlist
            // from a video-only one. Mirrors the bridge.
            Some(audio_uri) => download_hls_variant_audio_only_opts(
                audio_uri,
                audio_uri,
                &dir,
                &rec.filename,
                &rec.referer,
                None,
                &progress,
                &job.paused,
                &job.cancelled,
                None,
                &stream_opts,
            ),
            None => download_hls_audio_only_opts(
                &rec.url,
                &variant_uri,
                &dir,
                &rec.filename,
                &rec.referer,
                &progress,
                &job.paused,
                &job.cancelled,
                captured.as_ref(),
                &stream_opts,
            ),
        }
    } else {
        match &rec.audio_url {
            Some(audio_uri) => {
                let ffmpeg = match &ffmpeg {
                    Some(f) => f.clone(),
                    None => {
                        set_status(
                            state,
                            &id,
                            Status::Failed,
                            Some("This quality keeps its audio on a separate track, which needs ffmpeg to merge.".to_string()),
                        );
                        return;
                    }
                };
                download_hls_pair_opts(
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
                    &stream_opts,
                )
            }
            None => download_hls_variant_opts(
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
                &stream_opts,
            ),
        }
    };
    match result {
        Ok(final_path) => {
            // "Audio only": extract the audio track BEFORE the
            // record is marked Completed (see run_download).
            let final_path = if rec.audio_only {
                match audio_only_post_step(
                    state,
                    &final_path,
                    ffmpeg.as_deref(),
                    rec.audio_extension(),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        set_status(state, &id, Status::Failed, Some(e));
                        return;
                    }
                }
            } else {
                final_path
            };
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                // Round 10, item 2: completion reconciles against
                // the real file size — the file on disk is the
                // truth (novahd: 121 MB shown for a 3.37 GB file).
                r.complete_with_file(&final_path);
                probe_delivered_quality(state, &mut r, &final_path);
                store.upsert(r);
                let _ = store.save();
            }
        }
        Err(e) => {
            if let Some(reason) = stop_reason(&e) {
                match reason {
                    fetchforge_engine::download::StopReason::Paused => {
                        set_status(state, &id, Status::Paused, None)
                    }
                    fetchforge_engine::download::StopReason::Cancelled => {
                        set_status(state, &id, Status::Cancelled, None)
                    }
                }
            } else if try_downgrade(state, rec) {
                // A lower quality was queued (round 11 extension,
                // feature 9): the record is back in the queue
                // with a fresh worker, so this worker ends
                // cleanly instead of failing it.
            } else {
                set_status(state, &id, Status::Failed, Some(e));
            }
        }
    }
}

/// Run a DASH download (media.rs). Mirrors the bridge's
/// run_stream_dash: the record's format_id is the chosen
/// representation id (None means the default), and ffmpeg is
/// resolved inside the engine only when the manifest keeps audio
/// separate from the chosen video representation.
fn run_stream_dash_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let settings = current_settings(&state.data_dir);
    let mut dir = state.download_dir.clone();
    if settings.organise_by_category {
        // Round 13 follow-up, item 6: see
        // run_stream_hls_download — an audio-only record goes
        // to Audio/ even when it still carries the video
        // format's filename. Mirrors the bridge's run_stream_dash.
        dir = dir.join(category_for_download(&rec.filename, rec.audio_only));
    }
    // "Audio only": resolve ffmpeg UPFRONT, so a missing ffmpeg
    // fails fast instead of after a full video download (same
    // fail-fast pattern as the HLS merge path). The desktop
    // previously had no audio-only handling here at all.
    let ffmpeg = if rec.audio_only {
        match ensure_ffmpeg(&state.data_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                set_status(state, &id, Status::Failed, Some(e));
                return;
            }
        }
    } else {
        None
    };
    let opts = DashDownloadOptions {
        manifest_url: rec.url.clone(),
        representation_id: rec.format_id.clone(),
        destination_dir: dir,
        filename: rec.filename.clone(),
        referer: rec.referer.clone(),
        audio_language: rec.audio_lang.clone(),
        subtitle_language: rec.subtitle_langs.first().cloned(),
        proxy_url: settings.proxy_url.clone(),
        bandwidth_kbps: settings.bandwidth_kbps,
        title: Some(
            std::path::Path::new(&rec.filename)
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| rec.filename.clone()),
        ),
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
            // Round 10, items 2/6: monotonic progress — a later
            // report never moves a counter backwards (the Plex
            // audio phase restarted its segment counter at 0),
            // and a report that carries no segment value never
            // wipes the plan-time count.
            r.apply_progress(&p);
            store.upsert(r);
            let _ = store.save();
        }
    };
    match if rec.audio_only {
        // Round 13 follow-up, item 6: an audio-only DASH record
        // whose chosen representation is video needs the
        // audio-only entry point — the engine cannot tell it
        // from a video download on its own. (When the chosen
        // representation itself is audio, the plain entry point
        // already re-routes.) Mirrors the bridge.
        download_dash_audio_only(
            &state.data_dir,
            &opts,
            &progress,
            &job.paused,
            &job.cancelled,
            read_manifest_sidecar(&state.data_dir, &id, &rec.url).as_ref(),
        )
    } else {
        download_dash(
            &state.data_dir,
            &opts,
            &progress,
            &job.paused,
            &job.cancelled,
            read_manifest_sidecar(&state.data_dir, &id, &rec.url).as_ref(),
        )
    } {
        Ok(final_path) => {
            // "Audio only": extract the audio track BEFORE the
            // record is marked Completed (see run_download). A
            // DASH audio representation already lands as .m4a and
            // passes the post-step unchanged when m4a was chosen;
            // another chosen format converts it.
            let final_path = if rec.audio_only {
                match audio_only_post_step(
                    state,
                    &final_path,
                    ffmpeg.as_deref(),
                    rec.audio_extension(),
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        set_status(state, &id, Status::Failed, Some(e));
                        return;
                    }
                }
            } else {
                final_path
            };
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                // Round 10, item 2: completion reconciles against
                // the real file size — the file on disk is the
                // truth (novahd: 121 MB shown for a 3.37 GB file).
                r.complete_with_file(&final_path);
                probe_delivered_quality(state, &mut r, &final_path);
                store.upsert(r);
                let _ = store.save();
            }
        }
        Err(e) => {
            if let Some(reason) = stop_reason(&e) {
                match reason {
                    fetchforge_engine::download::StopReason::Paused => {
                        set_status(state, &id, Status::Paused, None)
                    }
                    fetchforge_engine::download::StopReason::Cancelled => {
                        set_status(state, &id, Status::Cancelled, None)
                    }
                }
            } else if try_downgrade(state, rec) {
                // A lower quality was queued (round 11 extension,
                // feature 9): the record is back in the queue
                // with a fresh worker, so this worker ends
                // cleanly instead of failing it.
            } else {
                set_status(state, &id, Status::Failed, Some(e));
            }
        }
    }
}

/// Run a `.gltf` bundle download (threed.rs). Mirrors the bridge's
/// run_gltf: the plan is rebuilt from the URL at run time, so a
/// resume always works from the current file.
fn run_gltf_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let settings = current_settings(&state.data_dir);
    let plan = match fetch_gltf_plan_with_proxy(&rec.url, settings.proxy_url.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            set_status(state, &id, Status::Failed, Some(e));
            return;
        }
    };
    let opts = GltfDownloadOptions {
        destination_dir: state.download_dir.clone(),
        max_segments: settings.max_segments,
        max_retries: settings.max_retries,
        bandwidth_kbps: settings.bandwidth_kbps,
        proxy_url: settings.proxy_url.clone(),
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
            // Round 10, items 2/6: monotonic progress — a later
            // report never moves a counter backwards (the Plex
            // audio phase restarted its segment counter at 0),
            // and a report that carries no segment value never
            // wipes the plan-time count.
            r.apply_progress(&p);
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
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                // Round 10, item 2: completion reconciles against
                // the real file size — the file on disk is the
                // truth (novahd: 121 MB shown for a 3.37 GB file).
                r.complete_with_file(&final_path);
                store.upsert(r);
                let _ = store.save();
            }
        }
        Err(e) => {
            if let Some(reason) = stop_reason(&e) {
                match reason {
                    fetchforge_engine::download::StopReason::Paused => {
                        set_status(state, &id, Status::Paused, None)
                    }
                    fetchforge_engine::download::StopReason::Cancelled => {
                        set_status(state, &id, Status::Cancelled, None)
                    }
                }
            } else {
                set_status(state, &id, Status::Failed, Some(e));
            }
        }
    }
}

#[tauri::command]
async fn pause_download(state: tauri::State<'_, Arc<State>>, id: String) -> Result<(), String> {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.paused.store(true, Ordering::Relaxed);
    }
    // Round 10, item 13: the download may be running in the bridge
    // (extension path), not in this process — forward the pause
    // there too, best-effort. A dead bridge must not break the
    // local pause. Async so the loopback call runs off the IPC
    // thread (see pair_browser). Returns Result because Tauri
    // requires async commands taking references to return one
    // (CI run 45314495234 failed on the () return).
    if let Ok(token) = read_bridge_token(&state.data_dir) {
        let _ = bridge_post(&state.data_dir, &token, &format!("/downloads/{id}/pause"));
    }
    Ok(())
}

#[tauri::command]
fn resume_download(state: tauri::State<Arc<State>>, id: String) {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.paused.store(false, Ordering::Relaxed);
    }
    set_status(&state, &id, Status::Queued, None);
    let state2 = Arc::clone(&state);
    let job = Arc::new(Job {
        paused: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    state
        .jobs
        .lock()
        .unwrap()
        .insert(id.clone(), Arc::clone(&job));
    std::thread::spawn(move || {
        run_download(&state2, &id, &job);
        // Round 10, item 6: the job leaves the map when its worker
        // ends (paused workers too — resume spawns a fresh job),
        // so `active_job_ids` stays exactly the running workers.
        remove_job_if_current(&state2, &id, &job);
        // Round 11 extension, feature 7: shutdown when finished
        // (see spawn_download_worker).
        maybe_shutdown_after_completion(&state2, &id);
    });
}

#[tauri::command]
async fn cancel_download(state: tauri::State<'_, Arc<State>>, id: String) -> Result<(), String> {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.cancelled.store(true, Ordering::Relaxed);
    }
    // Round 10, item 13: same cross-process forward as pause —
    // the download may be running in the bridge, not here.
    // Returns Result for the same Tauri rule as pause_download.
    if let Ok(token) = read_bridge_token(&state.data_dir) {
        let _ = bridge_post(&state.data_dir, &token, &format!("/downloads/{id}/cancel"));
    }
    Ok(())
}

/// Delete a download from the list (round 10, item 13), on ANY
/// row state: a running download is stopped first (cancelled in
/// both processes, then up to ~5 s for the worker to acknowledge
/// by leaving Downloading), the record is removed, and — when the
/// user chose "delete the files too" — its files are deleted from
/// disk. Returns how many files were removed (0 when only the
/// list entry was removed). Async so the poll runs off the IPC
/// thread. The frontend gates this behind the two-option
/// confirmation; nothing is deleted until the user picks one.
#[tauri::command(rename_all = "snake_case")]
async fn delete_download(
    state: tauri::State<'_, Arc<State>>,
    id: String,
    delete_files: bool,
) -> Result<usize, String> {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.cancelled.store(true, Ordering::Relaxed);
    }
    if let Ok(token) = read_bridge_token(&state.data_dir) {
        let _ = bridge_post(&state.data_dir, &token, &format!("/downloads/{id}/cancel"));
    }
    for _ in 0..50 {
        let still_running = {
            let mut store = state.store.lock().unwrap();
            store.reload_from_disk(&active_job_ids(&state));
            store
                .get(&id)
                .is_some_and(|r| r.status == Status::Downloading)
        };
        if !still_running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let record = {
        let mut store = state.store.lock().unwrap();
        store.reload_from_disk(&active_job_ids(&state));
        let rec = match store.get(&id) {
            Some(r) => r.clone(),
            None => return Err("Download not found.".to_string()),
        };
        store.remove(&id);
        if let Err(e) = store.save() {
            return Err(e);
        }
        rec
    };
    // The manifest sidecar is orphaned once the record is gone —
    // set_status never runs for a removed record.
    delete_manifest_sidecar(&state.data_dir, &id);
    if !delete_files {
        return Ok(0);
    }
    Ok(delete_download_files(
        &record,
        &state.download_dir,
        &state.data_dir,
    ))
}

/// Clear finished downloads from the list: removes Completed/Failed/
/// Cancelled records only. Downloaded files are never touched, and
/// active downloads are never removed. Returns how many records were
/// removed.
#[tauri::command]
fn clear_finished_downloads(state: tauri::State<Arc<State>>) -> usize {
    let mut store = state.store.lock().unwrap();
    store.reload_from_disk(&active_job_ids(&state));
    let removed = store.remove_finished();
    if let Err(e) = store.save() {
        eprintln!("FetchForge WARNING: could not save after clearing finished downloads: {e}");
    }
    removed
}

/// Pairing-code status for the app's pairing pill, mirrored from the
/// bridge's GET /pair/status JSON. Local struct with serde derive in
/// this file, like DetectedItem.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairStatus {
    active: bool,
    used: bool,
    seconds_left: u64,
}

/// Paired browsers for the Browser Integration section (round 10,
/// item 15), mirrored from the bridge's GET /pair/browsers JSON:
/// the real count of browsers paired by code, plus their names and
/// pairing times. Browsers paired by pasting a token manually are
/// not in the list — the count is what was recorded, never a guess.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairedBrowser {
    instance_id: String,
    browser_name: String,
    paired_unix: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PairedBrowsers {
    count: usize,
    browsers: Vec<PairedBrowser>,
}

/// Feat 1 (NOT VERIFIED on Windows): talk to the local bridge over
/// loopback with plain TcpStream, so no new dependency is needed.
/// The token is read from the app's own data dir, never shown in the
/// UI and never sent anywhere except 127.0.0.1:18443.
// bridge_post's data_dir is intentionally unused: callers already read
// the token from data_dir via read_bridge_token, and the port is fixed
// at 127.0.0.1:18443 (the same port spawn_bridge uses), so there is no
// port or token left to look up here. Renamed to _data_dir to silence
// the warning, not a logic bug. Callers still pass &state.data_dir.
fn bridge_post(_data_dir: &PathBuf, token: &str, path: &str) -> Result<String, String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
    // Never block forever: if the bridge accepts but never answers,
    // fail visibly instead of hanging the UI.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:18443\r\nX-FetchForge-Token: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Bridge request failed ({e})"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("Bridge response failed ({e})"))?;
    let (head, body) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => ("", raw.as_str()),
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!(
            "Bridge returned HTTP {status}: {}",
            body.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(body.to_string())
}

/// POST twin of bridge_post with a JSON body (round 11
/// extension, features 3/7/10): POST /settings carries a body;
/// the existing bridge_post sends Content-Length: 0.
fn bridge_post_body(
    _data_dir: &PathBuf,
    token: &str,
    path: &str,
    body: &str,
) -> Result<String, String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:18443\r\nX-FetchForge-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Bridge request failed ({e})"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("Bridge response failed ({e})"))?;
    let (head, resp_body) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => ("", raw.as_str()),
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!(
            "Bridge returned HTTP {status}: {}",
            resp_body.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(resp_body.to_string())
}

// GET twin of bridge_post: same loopback call, same token header,
// same timeouts, for the read-only /pair/status endpoint.
fn bridge_get(_data_dir: &PathBuf, token: &str, path: &str) -> Result<String, String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
    // Never block forever: if the bridge accepts but never answers,
    // fail visibly instead of hanging the UI.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:18443\r\nX-FetchForge-Token: {token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("Bridge request failed ({e})"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("Bridge response failed ({e})"))?;
    let (head, body) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => ("", raw.as_str()),
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!(
            "Bridge returned HTTP {status}: {}",
            body.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(body.to_string())
}

fn read_bridge_token(data_dir: &PathBuf) -> Result<String, String> {
    let token = std::fs::read_to_string(data_dir.join("bridge-token.txt")).map_err(|_| {
        "Bridge token not found. Wait for the bridge to start, then try again.".to_string()
    })?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("Bridge token is empty. Restart FetchForge, then try again.".to_string());
    }
    Ok(token)
}

// Async so the blocking loopback call runs off the IPC thread (see
// add_download).
#[tauri::command]
async fn pair_browser(state: tauri::State<'_, Arc<State>>) -> Result<String, String> {
    let token = read_bridge_token(&state.data_dir)?;
    let body = bridge_post(&state.data_dir, &token, "/pair/generate")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Bridge returned bad JSON ({e})"))?;
    v.get("code")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Bridge did not return a pairing code.".to_string())
}

#[tauri::command]
async fn unpair_all_browsers(state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
    let token = read_bridge_token(&state.data_dir)?;
    let _ = bridge_post(&state.data_dir, &token, "/unpair-all")?;
    Ok(())
}

// Async so the blocking loopback call runs off the IPC thread (see
// pair_browser). Polled once a second while a pairing code is shown,
// so the pill can transition to "code used" when the extension
// claims the code instead of the countdown just running out.
#[tauri::command]
async fn pair_status(state: tauri::State<'_, Arc<State>>) -> Result<PairStatus, String> {
    let token = read_bridge_token(&state.data_dir)?;
    let body = bridge_get(&state.data_dir, &token, "/pair/status")?;
    serde_json::from_str(&body).map_err(|e| format!("Bridge returned bad JSON ({e})"))
}

// Async so the blocking loopback call runs off the IPC thread (see
// pair_browser). Mirrors pair_status; the frontend hides the
// paired-browsers indicator when this fails rather than showing a
// fake number.
#[tauri::command]
async fn paired_browsers(state: tauri::State<'_, Arc<State>>) -> Result<PairedBrowsers, String> {
    let token = read_bridge_token(&state.data_dir)?;
    let body = bridge_get(&state.data_dir, &token, "/pair/browsers")?;
    serde_json::from_str(&body).map_err(|e| format!("Bridge returned bad JSON ({e})"))
}

/// Site directory (round 10, item 16): the working sweep sites
/// only. A link is opened in the user's default browser, never
/// inside the app's webview. The URL must be https and its host
/// must be on this allowlist — an arbitrary URL from the frontend
/// must never reach the OS shell. This crate has no shell/opener
/// plugin and no `url` dependency, so the check is a plain string
/// parse: scheme, host, and no shell metacharacters anywhere.
const SITE_DIRECTORY_HOSTS: [&str; 14] = [
    "novahd.cc",
    "nime.pro",
    "atlantic.bz",
    "lookmovie2.uk",
    "primewire.mov",
    "flixtortv.im",
    "soap2dayhd.autos",
    "fmovies.co",
    "watch.plex.tv",
    "therokuchannel.roku.com",
    "archive.org",
    "www.youtube.com",
    "123movieseasy.com",
    "fmovie.buzz",
];

#[tauri::command]
fn open_external_link(url: String) -> Result<(), String> {
    if url.contains(['&', '|', '<', '>', '^', '%', '"', '\'', '`', ';', ' ', '\\']) {
        return Err("That link is not a valid site-directory link.".to_string());
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "Only secure (https) links can be opened.".to_string())?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains(':') {
        return Err("That link is not a valid site-directory link.".to_string());
    }
    if !SITE_DIRECTORY_HOSTS.contains(&host) {
        return Err("That site is not in the FetchForge site directory.".to_string());
    }
    // Open via the OS default browser (`cmd /C start` on Windows,
    // `open`/`xdg-open` elsewhere). The URL was validated above,
    // so no shell metacharacter can reach the command.
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| format!("Could not open the link ({e})."))?;
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        std::process::Command::new(opener)
            .arg(&url)
            .spawn()
            .map_err(|e| format!("Could not open the link ({e})."))?;
    }
    Ok(())
}

/// What the bridge's POST /cache/clear returns: how many cached
/// files were removed and how many bytes that freed. Serialized
/// back to the frontend as {"removed_files": N, "freed_bytes": N}.
#[derive(serde::Serialize, serde::Deserialize)]
struct ClearCacheResult {
    removed_files: usize,
    freed_bytes: u64,
}

// Async so the blocking loopback call runs off the IPC thread (see
// pair_browser). Forwards to the bridge, which owns the detector
// list (memory + detected.json). Detector items only — downloads
// and history are never touched. Returns how many items were
// removed.
#[tauri::command]
async fn clear_detected_media(state: tauri::State<'_, Arc<State>>) -> Result<usize, String> {
    let token = read_bridge_token(&state.data_dir)?;
    let body = bridge_post(&state.data_dir, &token, "/media/detected/clear")?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Bridge returned bad JSON ({e})"))?;
    v.get("removed")
        .and_then(|n| n.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| "Bridge did not return a removed count.".to_string())
}

// Async so the blocking loopback call runs off the IPC thread (see
// pair_browser). Forwards to the bridge, which clears temp/segment
// cache files and the detector store. Downloaded files, the
// download list and history are never touched (see the bridge's
// clear_cache).
#[tauri::command]
async fn clear_cache(state: tauri::State<'_, Arc<State>>) -> Result<ClearCacheResult, String> {
    let token = read_bridge_token(&state.data_dir)?;
    let body = bridge_post(&state.data_dir, &token, "/cache/clear")?;
    serde_json::from_str(&body).map_err(|e| format!("Bridge returned bad JSON ({e})"))
}

/// Read the current settings (round 11 extension, features
/// 3/7/10): the bridge's live settings when it answers, else
/// the settings file on disk (the bridge may be down when its
/// binary is missing — the app still works).
#[tauri::command]
async fn get_settings(state: tauri::State<'_, Arc<State>>) -> Result<Settings, String> {
    if let Ok(token) = read_bridge_token(&state.data_dir) {
        if let Ok(body) = bridge_get(&state.data_dir, &token, "/settings") {
            if let Ok(s) = serde_json::from_str::<Settings>(&body) {
                return Ok(s);
            }
        }
    }
    Ok(current_settings(&state.data_dir))
}

/// Save settings (round 11 extension, features 3/7/10): POSTs
/// the settings to the bridge when it answers (the bridge
/// validates + persists), else saves to the file directly with
/// the same validation. The body is a partial settings object
/// — only the fields it carries change. An invalid proxy fails
/// the save with the field name only, never the value.
#[tauri::command(rename_all = "snake_case")]
async fn update_settings(
    state: tauri::State<'_, Arc<State>>,
    settings: serde_json::Value,
) -> Result<Settings, String> {
    if let Ok(token) = read_bridge_token(&state.data_dir) {
        let body = serde_json::to_string(&settings).map_err(|e| e.to_string())?;
        if let Ok(resp) = bridge_post_body(&state.data_dir, &token, "/settings", &body) {
            if let Ok(s) = serde_json::from_str::<Settings>(&resp) {
                return Ok(s);
            }
        }
    }
    update_settings_value(&state.data_dir, &settings)
}

/// One item of a batch add (round 11 extension, feature 5):
/// mirrors the add_download command's arguments, so the
/// detector's multi-select queues the same way as a single add.
/// All fields except `url` are optional.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchAddItem {
    url: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    format_id: Option<String>,
    #[serde(default)]
    referer: Option<String>,
    #[serde(default)]
    audio_url: Option<String>,
    #[serde(default)]
    audio_lang: Option<String>,
    #[serde(default)]
    subtitle_langs: Vec<String>,
    #[serde(default)]
    audio_format: Option<String>,
}

/// One failed batch item: which URL, and why.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchAddError {
    url: String,
    error: String,
}

/// Batch add result: what queued, what failed (per item, so one
/// bad URL never blocks the rest).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BatchAddResult {
    added: Vec<DownloadRecord>,
    errors: Vec<BatchAddError>,
}

/// Playlist add result (round 11 extension, feature 11).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PlaylistAddResult {
    playlist: String,
    added: Vec<DownloadRecord>,
    errors: Vec<String>,
    truncated: bool,
}

/// Batch add (round 11 extension, feature 5): queue several
/// items from one action (a series page, the detector's
/// multi-select). Each item is answered individually, so one
/// bad URL never blocks the rest. Capped at 50 items.
#[tauri::command(rename_all = "snake_case")]
async fn add_download_batch(
    state: tauri::State<'_, Arc<State>>,
    items: Vec<BatchAddItem>,
) -> Result<BatchAddResult, String> {
    let mut added = Vec::new();
    let mut errors = Vec::new();
    for item in items.into_iter().take(50) {
        match add_download_inner(
            &state,
            item.url.clone(),
            item.filename,
            item.format_id,
            item.referer,
            item.audio_url,
            item.audio_lang,
            item.subtitle_langs,
            item.audio_format,
            None,
        ) {
            Ok(rec) => added.push(rec),
            Err(e) => errors.push(BatchAddError {
                url: item.url,
                error: e,
            }),
        }
    }
    Ok(BatchAddResult { added, errors })
}

/// List a playlist's entries (round 11 extension, feature 11):
/// titles + URLs, flat — no per-video extraction — so the UI
/// can show "Download all N videos". Errors plainly when the
/// URL is not a playlist.
#[tauri::command]
async fn media_playlist(
    state: tauri::State<'_, Arc<State>>,
    url: String,
) -> Result<PlaylistInfo, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    let settings = current_settings(&state.data_dir);
    list_playlist_with_proxy(&state.data_dir, &clean, settings.proxy_url.as_deref())
}

/// Queue every entry of a playlist as its own record (round 11
/// extension, feature 11), tagged with the playlist title +
/// index. Entries that fail to queue are reported, never fatal
/// to the rest. Capped at 200 entries. Each entry plans at its
/// own default quality (a yt-dlp format id belongs to one
/// video, so no single format can be forced across a playlist).
#[tauri::command(rename_all = "snake_case")]
async fn add_playlist_download(
    state: tauri::State<'_, Arc<State>>,
    url: String,
    audio_lang: Option<String>,
    subtitle_langs: Vec<String>,
    audio_format: Option<String>,
) -> Result<PlaylistAddResult, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    let settings = current_settings(&state.data_dir);
    let info = list_playlist_with_proxy(&state.data_dir, &clean, settings.proxy_url.as_deref())?;
    let title = info.title.clone();
    let total = info.entries.len();
    let truncated = total > 200;
    let mut added = Vec::new();
    let mut errors = Vec::new();
    for entry in info.entries.into_iter().take(200) {
        match add_download_inner(
            &state,
            entry.url.clone(),
            Some(entry.title.clone()),
            None,
            None,
            None,
            audio_lang.clone(),
            subtitle_langs.clone(),
            audio_format.clone(),
            Some((title.clone(), entry.index)),
        ) {
            Ok(rec) => added.push(rec),
            Err(e) => errors.push(format!("{}: {e}", entry.title)),
        }
    }
    Ok(PlaylistAddResult {
        playlist: title,
        added,
        errors,
        truncated,
    })
}

fn main() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut store = Store::load(dir.join("downloads.json"));
    if let Some(w) = &store.load_warning {
        eprintln!("FetchForge WARNING: {w}");
    }
    store.recover_interrupted();
    let _ = store.save();
    let bridge_child = spawn_bridge(&dir);
    let state = Arc::new(State {
        store: Mutex::new(store),
        jobs: Mutex::new(HashMap::new()),
        download_dir: download_dir(),
        data_dir: dir,
        bridge_child: Mutex::new(bridge_child),
        shutdown_triggered: AtomicBool::new(false),
    });
    let state_for_exit = Arc::clone(&state);
    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            add_download,
            add_download_batch,
            media_formats,
            media_playlist,
            add_playlist_download,
            get_settings,
            update_settings,
            list_detected_media,
            clear_detected_media,
            clear_cache,
            pause_download,
            resume_download,
            cancel_download,
            delete_download,
            clear_finished_downloads,
            bridge_status,
            pair_browser,
            pair_status,
            paired_browsers,
            unpair_all_browsers,
            open_external_link
        ])
        .build(tauri::generate_context!())
        .expect("error while building FetchForge");
    // F08: kill the bridge on app exit, so it never outlives the app.
    app.run(move |_app, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(mut child) = state_for_exit.bridge_child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
}
