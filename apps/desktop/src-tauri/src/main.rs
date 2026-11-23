// Tauri shell for FetchForge. The download logic lives in fetchforge-engine;
// this file only wires Tauri commands to it. The bridge binary in
// crates/bridge shares the same engine and is the path browser extensions use.
//
// NOTE: This Tauri crate is built on Windows (see scripts/build-windows.ps1
// and .github/workflows/windows-build.yml). It is NOT compiled on the Linux
// dev VM, which has no WebView2 / Windows toolchain; see docs/TEST_REPORT.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fetchforge_engine::{
    category_for_filename, download, download_dash, download_gltf_bundle, download_hls_pair,
    download_hls_variant, download_media, ensure_ffmpeg, ensure_image_extension, fetch_gltf_plan,
    find_ytdlp, generate_download_id, is_gltf_url, is_image_content_type, list_formats,
    plan_media_download_or_drive_fallback, probe, sanitize_filename, stop_reason, CapturedManifest,
    DashDownloadOptions, DownloadOptions, DownloadRecord, GltfDownloadOptions,
    MediaDownloadOptions, MediaInfo, PlanOutcome, Status, Store,
};
use std::collections::HashMap;
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

#[tauri::command]
fn list_downloads(state: tauri::State<Arc<State>>) -> Vec<DownloadRecord> {
    let mut store = state.store.lock().unwrap();
    // Pick up downloads added by the bridge (browser extension) since
    // launch; without this the list never shows them until restart.
    store.reload_from_disk();
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
) -> Result<DownloadRecord, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    // HLS (.m3u8) / DASH (.mpd) manifests go straight to the media
    // path (manifest parsing + segment assembly in media.rs): a
    // manifest must never reach the direct path, which would save
    // the manifest text itself as a file.
    if fetchforge_engine::is_manifest_url(&clean) {
        return add_media_download(&state, &clean, filename, format_id, referer, audio_url);
    }
    // glTF models (.gltf) carry dependencies (buffers, textures)
    // that must be downloaded with the model and kept in the same
    // folder structure (spec §6). They go to the 3D engine, never
    // the direct path, which would save the .gltf alone and leave
    // a broken model. Single-file models (GLB, OBJ, ...) take the
    // direct path below with category "3D".
    if is_gltf_url(&clean) {
        return add_gltf_download(&state, &clean, filename);
    }
    let probe_opts = DownloadOptions {
        url: clean.clone(),
        referer: referer.clone(),
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
        return add_media_download(&state, &clean, filename, format_id, referer, audio_url);
    }
    match probe_result {
        Ok(p) => {
            // Extension-less manifests: the probe's content type
            // says HLS/DASH, so this is a stream, not a file.
            if p.content_type.as_deref().is_some_and(|ct| {
                fetchforge_engine::stream_kind_of_content_type(ct).is_some()
            }) {
                return add_media_download(&state, &clean, filename, format_id, referer, audio_url);
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
            queue_direct_download(&state, clean, filename, total, resumable, referer)
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
                    &state,
                    &clean,
                    filename.clone(),
                    format_id.clone(),
                    referer.clone(),
                    audio_url.clone(),
                ) {
                    Ok(rec) => return Ok(rec),
                    Err(extract_err) => e = extract_err,
                }
            }
            let rec = failed_record(&state, clean, filename, e);
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
) -> DownloadRecord {
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
) -> Result<DownloadRecord, String> {
    match try_media_download(state, clean_url, filename.clone(), format_id, referer.clone(), audio_url) {
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
        )),
    }
}

fn try_gltf_download(
    state: &Arc<State>,
    clean_url: &str,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
    let plan = fetch_gltf_plan(clean_url)?;
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
        store.reload_from_disk();
        if let Some(mut existing) =
            store.find_duplicate(clean_url, &record_filename, &state.download_dir.to_string_lossy(), false)
        {
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
        store.reload_from_disk();
        if let Some(mut existing) =
            store.find_duplicate(clean_url, &provisional, &state.download_dir.to_string_lossy(), false)
        {
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
    let outcome = plan_media_download_or_drive_fallback(
        &state.data_dir,
        &rec.url,
        rec.format_id.as_deref(),
        filename_arg,
        &rec.referer,
        captured.as_ref(),
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
            store.reload_from_disk();
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
            updated.format_id = Some(plan.format_id);
            updated.needs_merge = plan.needs_merge;
            updated.format_selector = Some(plan.selector);
            updated.stream_kind = plan.stream_kind.map(|k| k.as_str().to_string());
            updated.audio_url = paired_audio;
            // HLS/DASH only: the manifest's segment count, so the
            // UI can show "0 / N pieces" before the first segment
            // lands. yt-dlp plans carry None.
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
        store.reload_from_disk();
        if let Some(mut existing) =
            store.find_duplicate(&clean, &filename, &state.download_dir.to_string_lossy(), false)
        {
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
    std::thread::spawn(move || run_download(&state2, &id, &job));
}

/// List the qualities available for a video page (format picker).
/// Async so the yt-dlp extraction (up to 90 s on a slow site) does not
/// block the IPC thread.
#[tauri::command]
async fn media_formats(
    state: tauri::State<'_, Arc<State>>,
    url: String,
) -> Result<MediaInfo, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    list_formats(&state.data_dir, &clean)
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
    let opts = DownloadOptions {
        url: rec.url.clone(),
        destination_dir: state.download_dir.clone(),
        filename: Some(rec.filename.clone()),
        max_segments: 4,
        max_retries: 3,
        bandwidth_kbps: None,
        referer: rec.referer.clone(),
        user_agent: None,
        organise_by_category: true,
    };
    let id2 = id.to_string();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
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
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(id).cloned() {
                r.status = Status::Completed;
                r.destination = final_path.to_string_lossy().to_string();
                r.error = None;
                r.updated_unix = now_unix();
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
        organise_by_category: true,
        referer: rec.referer.clone(),
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
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
        &state.data_dir,
        &opts,
        &progress,
        &job.paused,
        &job.cancelled,
    ) {
        Ok(final_path) => {
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                r.status = Status::Completed;
                r.destination = final_path.to_string_lossy().to_string();
                if let Some(name) = final_path.file_name() {
                    r.filename = name.to_string_lossy().to_string();
                }
                r.error = None;
                r.updated_unix = now_unix();
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

/// Run an HLS download (media.rs). Mirrors the bridge's
/// run_stream_hls: the record's format_id is the chosen variant
/// playlist URL (or the manifest URL itself for a plain media
/// playlist), and ffmpeg is resolved only when the variant needs a
/// merge (separate audio track).
fn run_stream_hls_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let dir = state
        .download_dir
        .join(category_for_filename(&rec.filename));
    let variant_uri = rec.format_id.clone().unwrap_or_else(|| rec.url.clone());
    let ffmpeg = if rec.needs_merge {
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
    // Paired variants (the extension saw a video variant and its
    // audio sibling, no master): download both tracks and merge
    // them. Otherwise the normal variant path, with the page
    // Referer on every manifest/segment request. Mirrors the
    // bridge's run_stream_hls. The captured manifest (extension
    // sends only) is used for whichever fetch its URL matches, so
    // a single-use signed link is never re-fetched here.
    let captured = read_manifest_sidecar(&state.data_dir, &id, &rec.url);
    let result = match &rec.audio_url {
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
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                r.status = Status::Completed;
                r.destination = final_path.to_string_lossy().to_string();
                if let Some(name) = final_path.file_name() {
                    r.filename = name.to_string_lossy().to_string();
                }
                r.error = None;
                r.updated_unix = now_unix();
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

/// Run a DASH download (media.rs). Mirrors the bridge's
/// run_stream_dash: the record's format_id is the chosen
/// representation id (None means the default), and ffmpeg is
/// resolved inside the engine only when the manifest keeps audio
/// separate from the chosen video representation.
fn run_stream_dash_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let dir = state
        .download_dir
        .join(category_for_filename(&rec.filename));
    let opts = DashDownloadOptions {
        manifest_url: rec.url.clone(),
        representation_id: rec.format_id.clone(),
        destination_dir: dir,
        filename: rec.filename.clone(),
        referer: rec.referer.clone(),
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
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
        &state.data_dir,
        &opts,
        &progress,
        &job.paused,
        &job.cancelled,
        read_manifest_sidecar(&state.data_dir, &id, &rec.url).as_ref(),
    ) {
        Ok(final_path) => {
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                r.status = Status::Completed;
                r.destination = final_path.to_string_lossy().to_string();
                if let Some(name) = final_path.file_name() {
                    r.filename = name.to_string_lossy().to_string();
                }
                r.error = None;
                r.updated_unix = now_unix();
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

/// Run a `.gltf` bundle download (threed.rs). Mirrors the bridge's
/// run_gltf: the plan is rebuilt from the URL at run time, so a
/// resume always works from the current file.
fn run_gltf_download(state: &Arc<State>, rec: &DownloadRecord, job: &Job) {
    let id = rec.id.clone();
    set_status(state, &id, Status::Downloading, None);
    let plan = match fetch_gltf_plan(&rec.url) {
        Ok(p) => p,
        Err(e) => {
            set_status(state, &id, Status::Failed, Some(e));
            return;
        }
    };
    let opts = GltfDownloadOptions {
        destination_dir: state.download_dir.clone(),
        max_segments: 4,
        max_retries: 3,
        bandwidth_kbps: None,
    };
    let id2 = id.clone();
    let state2 = Arc::clone(state);
    let progress = move |p: fetchforge_engine::Progress| {
        let mut store = state2.store.lock().unwrap();
        if let Some(mut r) = store.get(&id2).cloned() {
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
            let mut store = state.store.lock().unwrap();
            if let Some(mut r) = store.get(&id).cloned() {
                r.status = Status::Completed;
                r.destination = final_path.to_string_lossy().to_string();
                if let Some(name) = final_path.file_name() {
                    r.filename = name.to_string_lossy().to_string();
                }
                r.error = None;
                r.updated_unix = now_unix();
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
fn pause_download(state: tauri::State<Arc<State>>, id: String) {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.paused.store(true, Ordering::Relaxed);
    }
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
    std::thread::spawn(move || run_download(&state2, &id, &job));
}

#[tauri::command]
fn cancel_download(state: tauri::State<Arc<State>>, id: String) {
    if let Some(job) = state.jobs.lock().unwrap().get(&id).cloned() {
        job.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Clear finished downloads from the list: removes Completed/Failed/
/// Cancelled records only. Downloaded files are never touched, and
/// active downloads are never removed. Returns how many records were
/// removed.
#[tauri::command]
fn clear_finished_downloads(state: tauri::State<Arc<State>>) -> usize {
    let mut store = state.store.lock().unwrap();
    store.reload_from_disk();
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
    let token = std::fs::read_to_string(data_dir.join("bridge-token.txt"))
        .map_err(|_| "Bridge token not found. Wait for the bridge to start, then try again.".to_string())?;
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
    });
    let state_for_exit = Arc::clone(&state);
    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            add_download,
            media_formats,
            list_detected_media,
            clear_detected_media,
            clear_cache,
            pause_download,
            resume_download,
            cancel_download,
            clear_finished_downloads,
            bridge_status,
            pair_browser,
            pair_status,
            unpair_all_browsers
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
