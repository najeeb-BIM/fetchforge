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
//!   mode (see main). When a download starts while the desktop app
//!   is off, the native host spawns a detached bridge AND wakes the
//!   app (round 12, item 12 — see `wake_desktop_app`).
//!
//! Usage:
//!   fetchforge-bridge serve [--port 18443] [--data-dir DIR]
//!   fetchforge-bridge native-host [--data-dir DIR]
//!   fetchforge-bridge print-token [--data-dir DIR]

use fetchforge_engine::{
    category_for_download, category_for_filename, download, download_dash,
    download_dash_audio_only, download_gltf_bundle, download_hls_audio_only_opts,
    download_hls_pair_opts, download_hls_variant_audio_only_opts, download_hls_variant_opts,
    download_media, ensure_ffmpeg, ensure_image_extension, extract_audio_as,
    fetch_gltf_plan_with_proxy, find_ytdlp, generate_download_id, generate_secure_token,
    is_gltf_url, is_image_content_type, list_formats_with_referer_and_proxy,
    list_playlist_with_proxy, next_lower_format,
    plan_media_download_or_drive_fallback_with_options, probe, remove_partial_files,
    sanitize_filename, sniff_stream_kind_with_proxy, stop_reason, unique_path, validate_proxy_url,
    CapturedManifest, DashDownloadOptions, DownloadOptions, DownloadRecord, GltfDownloadOptions,
    MediaDownloadOptions, PlanOutcome, SizeKind, Status, Store, StreamDownloadOptions,
    MAX_CAPTURED_MANIFEST_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
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
    /// User-configured proxy (round 11 extension, feature 10):
    /// `http(s)://` or `socks5://`, optionally with credentials.
    /// Validated at save time; NEVER logged anywhere.
    #[serde(default)]
    proxy_url: Option<String>,
    /// Shut the computer down when all downloads finish (round
    /// 11 extension, feature 7): explicit opt-in, default OFF.
    #[serde(default)]
    shutdown_when_finished: bool,
    /// Desktop notifications on download completion (round 11
    /// extension, feature 8): default ON.
    #[serde(default = "default_true")]
    notifications_enabled: bool,
}

fn default_true() -> bool {
    true
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
            proxy_url: None,
            shutdown_when_finished: false,
            notifications_enabled: true,
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
    // proxy_url: null/empty = no proxy; a non-empty string must
    // validate (http/https/socks5 + host), else it is ignored —
    // a bad proxy must never break every download. The value is
    // NEVER logged (it may contain credentials).
    match obj.get("proxy_url") {
        None => {}
        Some(serde_json::Value::Null) => s.proxy_url = None,
        Some(v) => match v.as_str() {
            Some(p) if p.trim().is_empty() => s.proxy_url = None,
            Some(p) => match fetchforge_engine::validate_proxy_url(p) {
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

/// Persist settings (round 11 extension, feature 3/7/10):
/// write to a temp file and atomically rename over
/// settings.json, so a crash mid-save never leaves a corrupt
/// settings file. The proxy value is written as-is (it lives
/// in the user's own data dir, like the bridge token) and is
/// never logged.
fn save_settings(data_dir: &std::path::Path, settings: &Settings) -> Result<(), String> {
    let path = data_dir.join("settings.json");
    let tmp = data_dir.join("settings.json.tmp");
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, &text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
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

/// One paired browser (round 10, item 15): the extension's stable
/// instance id (generated once per browser profile, stored in the
/// extension's storage.local) plus a display name ("Chrome",
/// "Firefox", "Edge"). Recorded at pair/verify time, persisted to
/// paired.json, deduped by instance id — re-pairing the same
/// browser updates its timestamp, never adds a second row. The
/// count is real: browsers paired by pasting a token manually
/// (no verify call) are NOT counted, and the UI shows nothing
/// rather than a guessed number when the list cannot be read.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairedBrowser {
    instance_id: String,
    browser_name: String,
    paired_unix: u64,
}

struct App {
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
    /// Browsers paired by code (POST /pair/verify). Persisted to
    /// paired.json; cleared by rotate_token ("Unpair all").
    paired: Mutex<Vec<PairedBrowser>>,
    /// Shutdown-when-finished guard (round 11 extension, feature
    /// 7): once the shutdown command has fired, it must never
    /// fire again in this process — two workers finishing at the
    /// same moment must not both run `shutdown`.
    shutdown_triggered: AtomicBool,
}

impl App {
    fn new(data_dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&data_dir);
        // The settings themselves are NOT kept in memory: every
        // read goes to disk (see current_settings), so a POST
        // /settings takes effect on the next download without a
        // restart. Only the load warning is needed here.
        let (_settings, settings_warning) = load_settings_with_warning(&data_dir);
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
        let paired = match std::fs::read_to_string(data_dir.join("paired.json")) {
            Ok(text) => match serde_json::from_str::<Vec<PairedBrowser>>(&text) {
                Ok(items) => items,
                Err(_) => {
                    eprintln!(
                        "FetchForge WARNING: paired.json is not valid JSON; paired-browsers list started empty"
                    );
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        Arc::new(Self {
            data_dir,
            token: Mutex::new(token),
            pairing: Mutex::new(fetchforge_engine::pairing::PairingState::new()),
            store: Mutex::new(store),
            jobs: Mutex::new(HashMap::new()),
            active: Mutex::new(0),
            detected: Mutex::new(detected),
            paired: Mutex::new(paired),
            shutdown_triggered: AtomicBool::new(false),
        })
    }

    /// Current settings, loaded fresh from settings.json (round
    /// 11 extension): the file on disk is the single source of
    /// truth, so a POST /settings takes effect on the next
    /// download without restarting the bridge. Each worker loads
    /// once at its start — a settings change mid-download
    /// applies to the NEXT download, never to a running one.
    fn current_settings(&self) -> Settings {
        load_settings_with_warning(&self.data_dir).0
    }

    /// Update settings from a POST /settings body (round 11
    /// extension, features 3/7/10): merge the known fields the
    /// body carries into the current settings, validate, and
    /// persist. Unknown fields are ignored; a wrongly-typed or
    /// invalid field fails the whole save with the field NAME
    /// only — the proxy value is never echoed back (it may
    /// contain credentials). Numeric fields are clamped to sane
    /// ranges instead of failing: a UI slider cannot wedge the
    /// queue gate with max_simultaneous 0.
    fn update_settings(&self, body: &serde_json::Value) -> Result<Settings, String> {
        let obj = body
            .as_object()
            .ok_or_else(|| "Settings must be a JSON object".to_string())?;
        let mut s = self.current_settings();
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
                _ => match v.as_str() {
                    Some(p) if p.trim().is_empty() => s.proxy_url = None,
                    Some(p) => match validate_proxy_url(p) {
                        Ok(clean) => s.proxy_url = Some(clean),
                        Err(_) => return Err(
                            "proxy_url must be an http://, https:// or socks5:// URL with a host"
                                .to_string(),
                        ),
                    },
                    None => return Err("proxy_url must be a string or null".to_string()),
                },
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
        save_settings(&self.data_dir, &s)?;
        Ok(s)
    }

    /// Record a pairing (round 10, item 15): dedupe by instance id
    /// (a re-pair refreshes the timestamp), persist best-effort.
    /// An empty instance id is not recorded — the count must stay
    /// real, and an anonymous pairing is not a countable browser.
    fn record_paired_browser(&self, instance_id: &str, browser_name: &str) {
        let id = instance_id.trim();
        if id.is_empty() {
            return;
        }
        let name = browser_name.trim();
        let name = if name.is_empty() { "Browser" } else { name };
        let now = now_unix();
        let mut list = self.paired.lock().unwrap();
        if let Some(existing) = list.iter_mut().find(|b| b.instance_id == id) {
            existing.browser_name = name.to_string();
            existing.paired_unix = now;
        } else {
            list.push(PairedBrowser {
                instance_id: id.to_string(),
                browser_name: name.to_string(),
                paired_unix: now,
            });
        }
        drop(list);
        self.save_paired();
    }

    /// Best-effort persistence of the paired list (tmp file +
    /// rename, like the store). A failed save is logged, never fatal.
    fn save_paired(&self) {
        let list = self.paired.lock().unwrap();
        let text = match serde_json::to_string(&*list) {
            Ok(t) => t,
            Err(_) => return,
        };
        drop(list);
        let path = self.data_dir.join("paired.json");
        let tmp = self.data_dir.join("paired.json.tmp");
        if std::fs::write(&tmp, text).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Ids of downloads this process is currently running (its live
    /// job map), for `reload_from_disk`: only those records are
    /// protected from disk (round 10, item 6). A finished worker
    /// removes its job (see `spawn_worker`), so the set is exactly
    /// the running workers — never stale entries for downloads
    /// that already finished.
    fn active_job_ids(&self) -> std::collections::HashSet<String> {
        self.jobs.lock().unwrap().keys().cloned().collect()
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
    /// automatically. The paired-browsers list is cleared too
    /// (round 10, item 15): an unpaired browser is not a paired one.
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
        {
            let mut paired = self.paired.lock().unwrap();
            paired.clear();
        }
        self.save_paired();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_download(
        self: &Arc<Self>,
        url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
        audio_only: bool,
        audio_lang: Option<String>,
        subtitle_langs: Vec<String>,
        audio_format: Option<String>,
        playlist: Option<(String, u32)>,
    ) -> Result<DownloadRecord, String> {
        let settings = self.current_settings();
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
                audio_only,
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
            proxy_url: settings.proxy_url.clone(),
            ..Default::default()
        };
        let probe_result = probe(&probe_opts);
        // A web page is not a downloadable file — but since v0.2.0 it is
        // no longer refused outright: a page whose probe returns text/html
        // (YouTube, Vimeo, a public Google Drive file, any video page) goes
        // through media extraction instead, and the user picks a quality
        // (or the extension path takes the recorded default). Direct file
        // links behave exactly as before.
        let is_page = probe_result.as_ref().is_ok_and(|p| {
            p.content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("text/html"))
        });
        if is_page {
            return self.add_media_download(
                &clean_url,
                filename,
                format_id,
                referer,
                audio_url,
                manifest_text,
                audio_only,
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
                    return self.add_media_download(
                        &clean_url,
                        filename,
                        format_id,
                        referer,
                        audio_url,
                        manifest_text,
                        audio_only,
                        audio_lang,
                        subtitle_langs,
                        audio_format,
                        playlist,
                    );
                }
                // Round 10, item 4: some playlist endpoints serve a
                // generic content type, so the check above misses
                // them (MoviesJoy/NOXX/123Movies served HLS master
                // text as a ~940 B "file"). Sniff the first bytes:
                // playlist text re-routes to the media path instead
                // of being saved as a junk file. A failed sniff
                // fails open (the normal path below).
                if sniff_stream_kind_with_proxy(&clean_url, &referer, settings.proxy_url.as_deref())
                    .is_some()
                {
                    return self.add_media_download(
                        &clean_url,
                        filename,
                        format_id,
                        referer,
                        audio_url,
                        manifest_text,
                        audio_only,
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
                self.add_direct_record(
                    &clean_url,
                    filename,
                    total,
                    resumable,
                    referer,
                    audio_only,
                    audio_format,
                )
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
                        audio_only,
                        audio_lang.clone(),
                        subtitle_langs.clone(),
                        audio_format.clone(),
                        playlist.clone(),
                    ) {
                        Ok(rec) => return Ok(rec),
                        Err(extract_err) => e = extract_err,
                    }
                }
                // Still record the failure so the user sees it, with the reason.
                let id = generate_download_id();
                let (playlist_title, playlist_index) = match playlist {
                    Some((title, index)) => (Some(title), Some(index)),
                    None => (None, None),
                };
                let rec = DownloadRecord {
                    id: id.clone(),
                    url: clean_url.clone(),
                    filename: filename.unwrap_or_else(|| "download".to_string()),
                    destination: settings.download_dir.clone(),
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
    #[allow(clippy::too_many_arguments)]
    fn add_media_download(
        self: &Arc<Self>,
        clean_url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
        audio_only: bool,
        audio_lang: Option<String>,
        subtitle_langs: Vec<String>,
        audio_format: Option<String>,
        playlist: Option<(String, u32)>,
    ) -> Result<DownloadRecord, String> {
        let settings = self.current_settings();
        match self.try_media_download(
            clean_url,
            filename.clone(),
            format_id,
            referer.clone(),
            audio_url,
            manifest_text,
            audio_only,
            audio_lang.clone(),
            subtitle_langs.clone(),
            audio_format.clone(),
            playlist.clone(),
        ) {
            Ok(rec) => Ok(rec),
            Err(e) => {
                // Extraction now runs in the worker (see
                // try_media_download), so this arm is only reached
                // for queueing failures; the Google Drive fallback
                // lives in the worker's planning step
                // (plan_media_download_or_drive_fallback).
                let id = generate_download_id();
                let (playlist_title, playlist_index) = match playlist {
                    Some((title, index)) => (Some(title), Some(index)),
                    None => (None, None),
                };
                let rec = DownloadRecord {
                    id: id.clone(),
                    url: clean_url.to_string(),
                    filename: filename.unwrap_or_else(|| "download".to_string()),
                    destination: settings.download_dir.clone(),
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
        let settings = self.current_settings();
        match fetch_gltf_plan_with_proxy(clean_url, settings.proxy_url.as_deref()) {
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
                    store.reload_from_disk(&self.active_job_ids());
                    if let Some(mut existing) = store.find_duplicate(
                        clean_url,
                        &record_filename,
                        &settings.download_dir,
                        false,
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
                        destination: settings.download_dir.clone(),
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
                    destination: settings.download_dir.clone(),
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
    #[allow(clippy::too_many_arguments)]
    fn try_media_download(
        self: &Arc<Self>,
        clean_url: &str,
        filename: Option<String>,
        format_id: Option<String>,
        referer: Option<String>,
        audio_url: Option<String>,
        manifest_text: Option<String>,
        audio_only: bool,
        audio_lang: Option<String>,
        subtitle_langs: Vec<String>,
        audio_format: Option<String>,
        playlist: Option<(String, u32)>,
    ) -> Result<DownloadRecord, String> {
        let settings = self.current_settings();
        let provisional = filename
            .as_deref()
            .map(sanitize_filename)
            .unwrap_or_else(|| "download".to_string());
        // Idempotent add (see Store::find_duplicate): an
        // already-active or already-completed download for the same
        // URL + filename is returned, never queued twice. The
        // worker re-checks after planning, when the resolved URL
        // and planned filename are known. Audio-only requests are
        // checked twice: once under the video filename (a queued
        // audio request still carries it) and once under the audio
        // filename (a completed audio download's record was renamed
        // to the chosen audio format by the post-step) — a
        // completed VIDEO never matches either, because the flag
        // differs.
        {
            let mut store = self.store.lock().unwrap();
            // Fresh disk state before the duplicate check (see
            // add_gltf_download): a record the other process
            // cleared must not match from stale memory.
            store.reload_from_disk(&self.active_job_ids());
            let audio_filename = audio_only.then(|| {
                let stem = provisional
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(&provisional);
                let ext = audio_format.as_deref().unwrap_or("m4a");
                format!("{stem}.{ext}")
            });
            let existing = store
                .find_duplicate(clean_url, &provisional, &settings.download_dir, audio_only)
                .or_else(|| {
                    audio_filename.as_ref().and_then(|f| {
                        store.find_duplicate(clean_url, f, &settings.download_dir, audio_only)
                    })
                });
            if let Some(mut existing) = existing {
                // The caller must know nothing new was queued:
                // flag the returned clone (never the stored
                // record) so the extension reports "Already
                // downloaded" instead of claiming a fresh
                // download was sent.
                existing.duplicate = true;
                return Ok(existing);
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
                destination: settings.download_dir.clone(),
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
                audio_only,
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
        let settings = self.current_settings();
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
        let outcome = plan_media_download_or_drive_fallback_with_options(
            &self.data_dir,
            &rec.url,
            rec.format_id.as_deref(),
            filename_arg,
            &rec.referer,
            captured.as_ref(),
            rec.audio_lang.as_deref(),
            settings.proxy_url.as_deref(),
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
                // Subtitles on a muxed HLS variant (round 11
                // extension, feature 2): the variant downloads as
                // one .ts file, so the subtitle track can only
                // join it through an ffmpeg merge — mark the plan
                // for a merge so the worker resolves ffmpeg
                // upfront. The engine re-containerises to .mp4 at
                // merge time and the completion step reconciles
                // the record's filename; the plan keeps the .ts
                // name until then, because the subtitle may not
                // exist in the master at all (in which case the
                // download is a plain .ts, exactly as before).
                if !rec.subtitle_langs.is_empty()
                    && plan.stream_kind == Some(fetchforge_engine::StreamKind::Hls)
                    && !plan.needs_merge
                {
                    plan.needs_merge = true;
                }
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
                store.reload_from_disk(&self.active_job_ids());
                if store
                    .find_duplicate_except(
                        &record_url,
                        &plan.filename,
                        &settings.download_dir,
                        &id,
                        rec.audio_only,
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
                // Round 13 follow-up, item 6: the record's
                // category matches the folder the file lands in —
                // an audio-only record is Audio even when the
                // plan still carries the video format's filename.
                updated.category =
                    category_for_download(&plan.filename, rec.audio_only).to_string();
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
                // Round 10, item 8: the picked quality as the site
                // declared it — the completion probe compares the
                // delivered file against these.
                updated.picked_height = plan.picked_height;
                updated.picked_vbr_kbps = plan.picked_vbr_kbps;
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
        audio_only: bool,
        audio_format: Option<String>,
    ) -> Result<DownloadRecord, String> {
        let settings = self.current_settings();
        // Idempotent add (safe to run twice): if the same URL +
        // filename is already Queued, Downloading, or Completed,
        // return that record instead of starting a second download
        // (see Store::find_duplicate). Without this, two workers
        // derive the same .part path and corrupt each other (found
        // by the duplicate test on 2026-10-26: one record reported
        // "got 0 bytes" while progress said complete) — and a
        // repeated click re-downloads a file that already finished.
        // The check + insert happen under one store lock so two
        // simultaneous POSTs cannot both slip through. Audio-only
        // requests are checked under the video filename AND the
        // .m4a filename (see try_media_download): a completed
        // video must not swallow an audio request, and a completed
        // audio download must swallow a repeat audio request.
        {
            let mut store = self.store.lock().unwrap();
            // Fresh disk state before the duplicate check (see
            // add_gltf_download): a record the other process
            // cleared must not match from stale memory.
            store.reload_from_disk(&self.active_job_ids());
            let audio_filename = audio_only.then(|| {
                let stem = filename
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(&filename);
                let ext = audio_format.as_deref().unwrap_or("m4a");
                format!("{stem}.{ext}")
            });
            let existing = store
                .find_duplicate(clean_url, &filename, &settings.download_dir, audio_only)
                .or_else(|| {
                    audio_filename.as_ref().and_then(|f| {
                        store.find_duplicate(clean_url, f, &settings.download_dir, audio_only)
                    })
                });
            if let Some(mut existing) = existing {
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
                destination: settings.download_dir.clone(),
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
                audio_only,
                picked_height: None,
                picked_vbr_kbps: None,
                delivered_height: None,
                delivered_bitrate_kbps: None,
                quality_note: None,
                speed_bps: None,
                // Round 11, item 10: a probed Content-Length is
                // the exact total.
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
            store.upsert(rec.clone());
            let _ = store.save();
            drop(store);
            self.spawn_worker(id);
            Ok(rec)
        }
    }

    /// Playlist download (round 11 extension, feature 11): list
    /// a playlist with yt-dlp and queue every entry as its own
    /// record, tagged with the playlist title + its index, so
    /// the list shows where each item came from. Entries that
    /// fail to queue (a dead entry URL) are reported, never
    /// fatal to the rest. Returns (playlist title, queued
    /// records, per-entry errors, truncated?). The language,
    /// subtitle and audio-format choices apply to every entry;
    /// each entry plans at its own default quality (a yt-dlp
    /// format id belongs to one video, so no single format can
    /// be forced across a playlist).
    fn add_playlist(
        self: &Arc<Self>,
        url: &str,
        audio_lang: Option<String>,
        subtitle_langs: Vec<String>,
        audio_format: Option<String>,
    ) -> Result<(String, Vec<DownloadRecord>, Vec<String>, bool), String> {
        let settings = self.current_settings();
        let info = list_playlist_with_proxy(&self.data_dir, url, settings.proxy_url.as_deref())?;
        let title = info.title.clone();
        let total = info.entries.len();
        let truncated = total > MAX_PLAYLIST_ENTRIES;
        let mut added = Vec::new();
        let mut errors = Vec::new();
        for entry in info.entries.into_iter().take(MAX_PLAYLIST_ENTRIES) {
            match self.add_download(
                &entry.url,
                Some(entry.title.clone()),
                None,
                None,
                None,
                None,
                false,
                audio_lang.clone(),
                subtitle_langs.clone(),
                audio_format.clone(),
                Some((title.clone(), entry.index)),
            ) {
                Ok(rec) => added.push(rec),
                Err(e) => errors.push(format!("{}: {e}", entry.title)),
            }
        }
        Ok((title, added, errors, truncated))
    }

    /// Remove a job from the map when its worker ends — but only
    /// if the map still holds THIS job: a resume spawns a fresh
    /// job under the same id, and the old worker must not remove
    /// the new job.
    fn remove_job_if_current(&self, id: &str, job: &Arc<Job>) {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.get(id).is_some_and(|j| Arc::ptr_eq(j, job)) {
            jobs.remove(id);
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
                while *active >= app.current_settings().max_simultaneous.max(1) {
                    drop(active);
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if job.cancelled.load(Ordering::Relaxed) {
                        // Cancelled while waiting for a slot: the
                        // job leaves the map, or its id stays
                        // "protected" from disk reloads forever.
                        app.remove_job_if_current(&id, &job);
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
            // Round 10: the job leaves the map when its worker
            // ends, so `active_job_ids` is exactly the running
            // workers. A paused worker's job is removed too: its
            // record is Paused (run_one set the status before
            // returning), and a resume spawns a fresh job — the
            // stale entry must not keep the record "protected"
            // from disk forever.
            app.remove_job_if_current(&id, &job);
            // Round 11 extension, feature 7: shutdown when
            // finished — only ever after a COMPLETED download
            // (the helper checks the record's status), and only
            // when nothing else remains.
            if result.is_ok() {
                app.maybe_shutdown_after_completion(&id);
            }
            let _ = result;
        });
    }

    /// Auto-start queued downloads (round 11 extension, feature
    /// 1): spawn a worker for every Queued record, oldest first.
    /// Called once, by the process that becomes the server, right
    /// after `recover_interrupted` — so downloads queued before
    /// a restart start on their own instead of sitting until the
    /// user resumes each by hand. Paused records stay paused
    /// (the user paused them on purpose). The queue gate in
    /// `spawn_worker` caps the running workers at
    /// max_simultaneous; the rest wait for a slot. Mid-run
    /// auto-start already works through that same gate: a worker
    /// that ends frees a slot for the next waiting worker.
    /// NOTE: only the bridge auto-starts. The desktop app must
    /// NOT duplicate this — two workers on one record would
    /// derive the same .part path and corrupt each other.
    fn auto_start_queued(self: &Arc<Self>) {
        let mut queued: Vec<DownloadRecord> = {
            let store = self.store.lock().unwrap();
            store
                .all()
                .into_iter()
                .filter(|r| r.status == Status::Queued)
                .cloned()
                .collect()
        };
        queued.sort_by_key(|r| r.created_unix);
        for rec in queued {
            self.spawn_worker(rec.id);
        }
    }

    /// Retry at the next lower quality after a failure (round 11
    /// extension, feature 9). Returns true when a downgrade was
    /// queued: the record is reset to "pending" with the lower
    /// format id and a fresh worker is spawned to re-plan and
    /// download it. Returns false when no downgrade is possible
    /// — the caller then fails the record normally with the
    /// original error.
    ///
    /// Rules: one downgrade only (`downgrade_attempted`); only
    /// for a download that had a picked quality (no ladder to
    /// step down otherwise); never for direct or gltf downloads
    /// (no quality ladder); never on pause/cancel (the caller
    /// checks `stop_reason` first); never on a planning failure
    /// (the plan step does not call this). For yt-dlp downloads
    /// the old .part files are deleted first, or yt-dlp would
    /// resume the HIGHER quality's fragments under the same
    /// filename. A failed re-listing (e.g. a signed manifest
    /// link that only worked once) means no downgrade.
    fn try_downgrade(self: &Arc<Self>, rec: &DownloadRecord) -> bool {
        let Some(current) = self.store.lock().unwrap().get(&rec.id).cloned() else {
            return false;
        };
        if current.downgrade_attempted || current.format_id.is_none() {
            return false;
        }
        let Some(height) = current.picked_height else {
            return false;
        };
        // gltf bundles have no quality ladder.
        if current.stream_kind.as_deref() == Some("gltf") {
            return false;
        }
        let settings = self.current_settings();
        // Re-list the formats fresh: the ladder may have changed
        // since the plan (a live manifest rotates variants). The
        // one listing call covers all three paths — manifest
        // URLs are listed through the stream path and converted
        // to MediaFormat entries (variant URL / rep id as the
        // format id), page URLs through yt-dlp.
        let info = match list_formats_with_referer_and_proxy(
            &self.data_dir,
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
        // yt-dlp path only (no stream kind): delete the old
        // quality's .part files, or the retry resumes them.
        // Stream downloads write their segments under a sidecar
        // keyed by the variant URL, so a new variant starts
        // fresh on its own.
        if current.stream_kind.is_none() {
            let mut dir = PathBuf::from(&settings.download_dir);
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
            let mut store = self.store.lock().unwrap();
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
        self.spawn_worker(current.id.clone());
        true
    }

    /// Shut the computer down when all downloads are finished
    /// (round 11 extension, feature 7). Called after a worker
    /// ends. Fires ONLY when ALL of these hold: the setting is
    /// on; the just-ended download COMPLETED (a pause or cancel
    /// never triggers it); no Queued, Downloading or Paused
    /// record remains (fresh disk state — the desktop app may
    /// have queued work this process has not seen); no other
    /// worker is running in this process; and the shutdown has
    /// not already fired in this process. Windows runs
    /// `shutdown /s /t 60` (a 60-second abort window — the user
    /// can run `shutdown /a` to stop it); anywhere else logs a
    /// warning, since the deployment is Windows-only.
    fn maybe_shutdown_after_completion(&self, id: &str) {
        if self.shutdown_triggered.load(Ordering::Relaxed) {
            return;
        }
        if !self.current_settings().shutdown_when_finished {
            return;
        }
        // The just-ended download must have COMPLETED: a worker
        // also returns Ok(()) for a pause or a cancel, and
        // neither may shut the machine down.
        let completed = {
            let store = self.store.lock().unwrap();
            store.get(id).is_some_and(|r| r.status == Status::Completed)
        };
        if !completed {
            return;
        }
        // Fresh disk state: the other process may have queued
        // work since this process last loaded.
        let remaining = {
            let mut store = self.store.lock().unwrap();
            store.reload_from_disk(&self.active_job_ids());
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
        if *self.active.lock().unwrap() > 0 {
            return;
        }
        // Claim the one-shot guard BEFORE running the command, so
        // two workers finishing together cannot both fire it.
        if self
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
            // NEVER run a real shutdown from a test binary: the
            // shutdown-guard test calls this function with the
            // setting on and nothing remaining, and on a CI runner
            // (or a developer machine) it would schedule a real
            // system shutdown 60 seconds later, killing the runner
            // mid-build. The one-shot guard above still fires, so
            // the test's assertions are unaffected. (Round 11 CI
            // failure, 2026-11-25: ten consecutive Windows builds
            // died this way; the release binary is compiled
            // without cfg(test), so production is unaffected.)
            if !cfg!(test) {
                let _ = std::process::Command::new("shutdown")
                    .args(["/s", "/t", "60"])
                    .spawn();
            }
        }
        #[cfg(not(windows))]
        {
            eprintln!("FetchForge WARNING: shutdown_when_finished is on and all downloads finished, but shutdown is only implemented on Windows; not shutting down.");
        }
    }

    /// Best-effort delivered-quality probe (round 10, item 8):
    /// probe the finished file with ffmpeg (when one is already
    /// available — never downloaded for this) and record its real
    /// resolution/bitrate, plus a mismatch note when the site
    /// mislabelled the stream. A missing ffmpeg or a failed probe
    /// leaves the record unchanged: the download already
    /// completed, and a probe failure must never fail it. Only
    /// media downloads carry picked values, so direct/gltf
    /// records skip the probe entirely.
    fn probe_delivered_quality(&self, r: &mut DownloadRecord, final_path: &Path) {
        if r.picked_height.is_none() && r.picked_vbr_kbps.is_none() {
            return;
        }
        if let Ok(ffmpeg) = fetchforge_engine::find_ffmpeg(&self.data_dir) {
            if let Some(q) = fetchforge_engine::probe_video_quality(&ffmpeg, final_path) {
                r.apply_delivered_quality(q.height, q.bitrate_kbps);
            }
        }
    }

    /// "Audio only" post-step (extension popup, round 8): turn a
    /// downloaded video file into its audio track, in the format
    /// the user chose (round 11 extension, feature 13: "mp3",
    /// "opus" or "m4a"). Returns the path the record should point
    /// at. A file that is already audio in the CHOSEN format (a
    /// DASH audio representation the engine named `.m4a`, when
    /// m4a was chosen) passes through unchanged — no ffmpeg
    /// needed. A file in a DIFFERENT audio format is converted
    /// (stream copy when the container allows it, re-encode
    /// otherwise — see [`extract_audio_as`]). A video file is
    /// extracted to `<stem>.<format>` next to the video, and the
    /// video file is deleted. On extraction failure the video
    /// file is KEPT and the error is returned, so the caller can
    /// mark the record Failed with a clear reason instead of
    /// completing a download whose audio file does not exist.
    /// `ffmpeg` is the worker's already-resolved binary when one
    /// exists; otherwise it is resolved here (which may download
    /// ffmpeg on first use — the workers resolve it upfront for
    /// audio-only downloads so a missing ffmpeg fails fast
    /// instead of after a full download).
    fn audio_only_post_step(
        &self,
        video_path: &Path,
        ffmpeg: Option<&Path>,
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
                ffmpeg_path = ensure_ffmpeg(&self.data_dir)?;
                &ffmpeg_path
            }
        };
        let stem = video_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        let dir = video_path.parent().unwrap_or(Path::new("."));
        let audio_path = unique_path(dir, &format!("{stem}.{format}"));
        extract_audio_as(ffmpeg, video_path, &audio_path, format)?;
        // Extraction succeeded: the video file is no longer needed.
        let _ = std::fs::remove_file(video_path);
        Ok(audio_path)
    }

    fn run_one(self: &Arc<Self>, id: &str, job: &Job) -> Result<(), String> {
        let settings = self.current_settings();
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
        // "Audio only": resolve ffmpeg UPFRONT, so a missing ffmpeg
        // fails fast instead of after a full video download (same
        // fail-fast pattern as the HLS merge path).
        let ffmpeg = if rec.audio_only {
            match ensure_ffmpeg(&self.data_dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.set_status(id, Status::Failed, Some(e));
                    return Err("download failed".to_string());
                }
            }
        } else {
            None
        };
        let opts = DownloadOptions {
            url: rec.url.clone(),
            destination_dir: PathBuf::from(&rec.destination),
            filename: Some(rec.filename.clone()),
            max_segments: settings.max_segments,
            max_retries: settings.max_retries,
            bandwidth_kbps: settings.bandwidth_kbps,
            referer: rec.referer.clone(),
            proxy_url: settings.proxy_url.clone(),
            user_agent: None,
            organise_by_category: settings.organise_by_category,
        };
        let app_id = id.to_string();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(mut r) = store.get(&app_id).cloned() {
                // Monotonic (round 10, items 2/6): progress never
                // moves backwards — a phase change or re-plan must
                // not lower a counter the UI already showed.
                r.apply_progress(&p);
                store.upsert(r);
                let _ = store.save();
            }
        };
        match download(&opts, &progress, &job.paused, &job.cancelled) {
            Ok(final_path) => {
                // "Audio only": extract the audio track BEFORE the
                // record is marked Completed, so the list points at
                // the audio file. A failed extraction keeps the
                // video file and fails the record with the reason.
                let final_path = if rec.audio_only {
                    match self.audio_only_post_step(
                        &final_path,
                        ffmpeg.as_deref(),
                        rec.audio_extension(),
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            self.set_status(id, Status::Failed, Some(e));
                            return Err("download failed".to_string());
                        }
                    }
                } else {
                    final_path
                };
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(id).cloned() {
                    // Reconcile against the real file (round 10,
                    // item 2): the plan total can be badly wrong.
                    r.complete_with_file(&final_path);
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
    fn run_media(self: &Arc<Self>, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let settings = self.current_settings();
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        // "Audio only" with a VIDEO format (round 11 extension,
        // feature 13): yt-dlp downloads the video and the
        // post-step extracts the audio — resolve ffmpeg UPFRONT
        // so a missing ffmpeg fails fast instead of after a full
        // video download (same fail-fast pattern as the HLS merge
        // path). An audio FORMAT needs no post-step: yt-dlp
        // extracts to the chosen format itself (--extract-audio).
        let needs_audio_post_step = rec.audio_only && rec.format_kind.as_deref() != Some("audio");
        let ffmpeg = if needs_audio_post_step {
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
            destination_dir: PathBuf::from(&settings.download_dir),
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
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(mut r) = store.get(&app_id).cloned() {
                // Monotonic (round 10, items 2/6): progress never
                // moves backwards — a phase change or re-plan must
                // not lower a counter the UI already showed.
                r.apply_progress(&p);
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
                // "Audio only" with a video format: extract the
                // audio track BEFORE the record is marked
                // Completed, so the list points at the audio file
                // (see run_one). A failed extraction keeps the
                // video file and fails the record with the reason.
                let final_path = if needs_audio_post_step {
                    match self.audio_only_post_step(
                        &final_path,
                        ffmpeg.as_deref(),
                        rec.audio_extension(),
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            self.set_status(&id, Status::Failed, Some(e));
                            return Err("download failed".to_string());
                        }
                    }
                } else {
                    final_path
                };
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    // Reconcile against the real file (round 10,
                    // item 2): the plan total can be badly wrong.
                    r.complete_with_file(&final_path);
                    self.probe_delivered_quality(&mut r, &final_path);
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
                } else if self.try_downgrade(rec) {
                    // A lower quality was queued: the record is
                    // back in the queue with a fresh worker, so
                    // this worker ends cleanly instead of failing
                    // the record.
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
    /// chosen variant needs a merge (separate audio track) or the
    /// user picked "Audio only" (the audio track is extracted after
    /// the download); a resume re-runs the same variant and picks
    /// up the segment sidecar left by the paused run.
    fn run_stream_hls(self: &Arc<Self>, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let settings = self.current_settings();
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let mut dir = PathBuf::from(&settings.download_dir);
        if settings.organise_by_category {
            // Round 13 follow-up, item 6: the folder is decided
            // by what the download IS — an audio-only record
            // goes to Audio/ even when it still carries the
            // video format's filename (the NOXX case: a combined
            // format picked audio-only). category_for_download
            // never guesses from the dir name, so a download
            // root literally named Video is safe with organising
            // off (the dir is then the root, unchanged).
            dir = dir.join(category_for_download(&rec.filename, rec.audio_only));
        }
        let variant_uri = rec.format_id.clone().unwrap_or_else(|| rec.url.clone());
        let ffmpeg = if rec.needs_merge || rec.audio_only {
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
            if let Some(mut r) = store.get(&app_id).cloned() {
                // Monotonic (round 10, items 2/6): progress never
                // moves backwards — a phase change or re-plan must
                // not lower a counter the UI already showed.
                r.apply_progress(&p);
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
        //
        // "Audio only" never touches the video track: a paired
        // audio sibling downloads alone, and a master variant
        // resolves its audio group (or falls back to the muxed
        // variant, whose audio the post-step extracts).
        //
        // Round 11 extension, features 2/3/4/10/12: the popup's
        // language and subtitle choices, the global speed limit,
        // the proxy, and the title metadata all travel in one
        // options struct. Subtitles are skipped for audio-only
        // downloads (an audio file carries no subtitle track).
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
        let captured = read_manifest_sidecar(&self.data_dir, &id, &rec.url);
        let result = if rec.audio_only {
            match &rec.audio_url {
                // Round 13 follow-up, item 6: the paired audio
                // sibling downloads alone through the audio-only
                // variant entry point, so it lands in Audio/
                // even when the caller computed Video/ — the
                // plain variant entry point cannot tell this
                // playlist from a video-only one.
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
                            self.set_status(
                                &id,
                                Status::Failed,
                                Some("This quality keeps its audio on a separate track, which needs ffmpeg to merge.".to_string()),
                            );
                            return Err("download failed".to_string());
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
                // record is marked Completed (see run_one).
                let final_path = if rec.audio_only {
                    match self.audio_only_post_step(
                        &final_path,
                        ffmpeg.as_deref(),
                        rec.audio_extension(),
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            self.set_status(&id, Status::Failed, Some(e));
                            return Err("download failed".to_string());
                        }
                    }
                } else {
                    final_path
                };
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    // Reconcile against the real file (round 10,
                    // item 2): the plan total can be badly wrong.
                    r.complete_with_file(&final_path);
                    self.probe_delivered_quality(&mut r, &final_path);
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
                } else if self.try_downgrade(rec) {
                    // A lower quality was queued (round 11
                    // extension, feature 9): the record is back
                    // in the queue with a fresh worker, so this
                    // worker ends cleanly instead of failing it.
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
    /// separate from the chosen video representation — except for
    /// "Audio only" downloads, where the worker resolves it upfront
    /// (fail fast) for the post-step extraction.
    fn run_stream_dash(self: &Arc<Self>, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let settings = self.current_settings();
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let mut dir = PathBuf::from(&settings.download_dir);
        if settings.organise_by_category {
            // Round 13 follow-up, item 6: see run_stream_hls —
            // an audio-only record goes to Audio/ even when it
            // still carries the video format's filename.
            dir = dir.join(category_for_download(&rec.filename, rec.audio_only));
        }
        // "Audio only": resolve ffmpeg UPFRONT, so a missing ffmpeg
        // fails fast instead of after a full video download (same
        // fail-fast pattern as the HLS merge path).
        let ffmpeg = if rec.audio_only {
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
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(mut r) = store.get(&app_id).cloned() {
                // Monotonic (round 10, items 2/6): progress never
                // moves backwards — a phase change or re-plan must
                // not lower a counter the UI already showed.
                r.apply_progress(&p);
                store.upsert(r);
                let _ = store.save();
            }
        };
        match if rec.audio_only {
            // Round 13 follow-up, item 6: an audio-only DASH
            // record whose chosen representation is video needs
            // the audio-only entry point — the engine cannot
            // tell it from a video download on its own. (When
            // the chosen representation itself is audio, the
            // plain entry point already re-routes.)
            download_dash_audio_only(
                &self.data_dir,
                &opts,
                &progress,
                &job.paused,
                &job.cancelled,
                read_manifest_sidecar(&self.data_dir, &id, &rec.url).as_ref(),
            )
        } else {
            download_dash(
                &self.data_dir,
                &opts,
                &progress,
                &job.paused,
                &job.cancelled,
                read_manifest_sidecar(&self.data_dir, &id, &rec.url).as_ref(),
            )
        } {
            Ok(final_path) => {
                // "Audio only": extract the audio track BEFORE the
                // record is marked Completed (see run_one). A DASH
                // audio representation already lands as .m4a and
                // passes the post-step unchanged.
                let final_path = if rec.audio_only {
                    match self.audio_only_post_step(
                        &final_path,
                        ffmpeg.as_deref(),
                        rec.audio_extension(),
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            self.set_status(&id, Status::Failed, Some(e));
                            return Err("download failed".to_string());
                        }
                    }
                } else {
                    final_path
                };
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    // Reconcile against the real file (round 10,
                    // item 2): the plan total can be badly wrong.
                    r.complete_with_file(&final_path);
                    self.probe_delivered_quality(&mut r, &final_path);
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
                } else if self.try_downgrade(rec) {
                    // A lower quality was queued (round 11
                    // extension, feature 9): the record is back
                    // in the queue with a fresh worker, so this
                    // worker ends cleanly instead of failing it.
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
    fn run_gltf(self: &Arc<Self>, rec: &DownloadRecord, job: &Job) -> Result<(), String> {
        let settings = self.current_settings();
        let id = rec.id.clone();
        self.set_status(&id, Status::Downloading, None);
        let plan = match fetch_gltf_plan_with_proxy(&rec.url, settings.proxy_url.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                self.set_status(&id, Status::Failed, Some(e));
                return Err("download failed".to_string());
            }
        };
        let opts = GltfDownloadOptions {
            destination_dir: PathBuf::from(&settings.download_dir),
            max_segments: settings.max_segments,
            max_retries: settings.max_retries,
            bandwidth_kbps: settings.bandwidth_kbps,
            proxy_url: settings.proxy_url.clone(),
        };
        let app_id = id.clone();
        let store_clone = &self.store;
        let progress = |p: fetchforge_engine::Progress| {
            let mut store = store_clone.lock().unwrap();
            if let Some(mut r) = store.get(&app_id).cloned() {
                // Monotonic (round 10, items 2/6): progress never
                // moves backwards — a phase change or re-plan must
                // not lower a counter the UI already showed.
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
                let mut store = self.store.lock().unwrap();
                if let Some(mut r) = store.get(&id).cloned() {
                    // Reconcile against the real file (round 10,
                    // item 2): the plan total can be badly wrong.
                    r.complete_with_file(&final_path);
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

    /// Delete a download (round 10, item 13): stop it first when
    /// it is running (cancel the job, then wait up to ~5 s for the
    /// worker to acknowledge by leaving Downloading), remove the
    /// record, and — when the user chose "delete the files too" —
    /// delete its files. Returns how many files were removed (0
    /// when only the list entry was removed). A missing record is
    /// an error. A worker that never acknowledges (parked in one
    /// long network call) still has its record removed — its later
    /// status update is a no-op on a missing record — and its
    /// files are still deleted on the user's instruction; the
    /// worst case is an orphaned .part the next Clear cache
    /// removes.
    fn delete_download(&self, id: &str, delete_files: bool) -> Result<usize, String> {
        let settings = self.current_settings();
        if let Some(job) = self.jobs.lock().unwrap().get(id).cloned() {
            job.cancelled.store(true, Ordering::Relaxed);
            for _ in 0..50 {
                let still_running = {
                    let store = self.store.lock().unwrap();
                    store
                        .get(id)
                        .is_some_and(|r| r.status == Status::Downloading)
                };
                if !still_running {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        let record = {
            let mut store = self.store.lock().unwrap();
            store.reload_from_disk(&self.active_job_ids());
            let rec = match store.get(id) {
                Some(r) => r.clone(),
                None => return Err("Download not found.".to_string()),
            };
            store.remove(id);
            if let Err(e) = store.save() {
                return Err(e);
            }
            rec
        };
        if !delete_files {
            return Ok(0);
        }
        Ok(fetchforge_engine::delete_download_files(
            &record,
            std::path::Path::new(&settings.download_dir),
            &self.data_dir,
        ))
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
        let settings = self.current_settings();
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
            store.reload_from_disk(&self.active_job_ids());
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
        let mut dirs_to_scan = vec![PathBuf::from(&settings.download_dir)];
        if let Ok(entries) = std::fs::read_dir(&settings.download_dir) {
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
    /// True when the user picked "Audio only" in the extension
    /// popup (round 8): the worker delivers an audio file instead
    /// of the video. Ignored for yt-dlp downloads, where the
    /// format choice already governs.
    #[serde(default)]
    audio_only: bool,
    /// Audio language the user picked (round 11 extension,
    /// feature 4): a language tag ("en", "hi", "es") matched
    /// against the manifest's audio renditions or the yt-dlp
    /// format's language tag. Absent means the default track.
    #[serde(default)]
    audio_lang: Option<String>,
    /// Subtitle languages to embed (round 11 extension, feature
    /// 2): yt-dlp embeds all of them; HLS/DASH embed the first
    /// one that the manifest carries. Empty means none.
    #[serde(default)]
    subtitle_langs: Vec<String>,
    /// Audio format for audio-only downloads (round 11 extension,
    /// feature 13): "mp3", "opus" or "m4a". Absent means "m4a".
    /// Ignored for video downloads.
    #[serde(default)]
    audio_format: Option<String>,
}

/// Batch add (round 11 extension, feature 5): several items
/// queued from one action (a series page, the detector's
/// multi-select). Each item is a full AddRequest; the endpoint
/// answers per item, so one bad URL never blocks the rest.
#[derive(Deserialize)]
struct BatchRequest {
    items: Vec<AddRequest>,
}

/// Playlist add (round 11 extension, feature 11): the URL of a
/// playlist page (yt-dlp lists its entries); each entry becomes
/// its own record. Language/subtitle/audio-format choices apply
/// to every entry. There is deliberately no `format_id`: a
/// yt-dlp format id belongs to ONE video, so each entry plans
/// at its own default quality.
#[derive(Deserialize)]
struct PlaylistAddRequest {
    url: String,
    #[serde(default)]
    audio_lang: Option<String>,
    #[serde(default)]
    subtitle_langs: Vec<String>,
    #[serde(default)]
    audio_format: Option<String>,
}

#[derive(Deserialize)]
struct FormatsRequest {
    url: String,
    /// The page the stream came from (round 11, item 5): the
    /// extension sends the verdict's referer, and the engine
    /// forwards it as the Referer header on the manifest fetch —
    /// vidrift-class hosts 403 without it. Absent for old
    /// extension versions and the desktop's own listing calls.
    #[serde(default)]
    referer: Option<String>,
}

/// Playlist listing request (round 11 extension, feature 11):
/// just the playlist page URL.
#[derive(Deserialize)]
struct PlaylistListRequest {
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

/// Maximum items in one batch add (round 11 extension, feature
/// 5): a series page rarely tops 50 episodes, and a runaway
/// selection must not bury the list in one click.
const MAX_BATCH_ITEMS: usize = 50;

/// Maximum entries queued from one playlist add (round 11
/// extension, feature 11): a 1,000-video playlist queued by one
/// click would bury the list. Past the cap the endpoint reports
/// `truncated: true` with the total, so the user knows entries
/// were left out.
const MAX_PLAYLIST_ENTRIES: usize = 200;

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
        let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
        let code = parsed
            .as_ref()
            .and_then(|v| v.get("code").and_then(|n| n.as_str()))
            .unwrap_or("")
            .to_string();
        // Round 10, item 15: the extension sends its stable
        // instance id + browser name so the app can count paired
        // browsers. Both are optional (older extensions send
        // neither) — an anonymous pairing still succeeds, it just
        // is not counted.
        let browser_id = parsed
            .as_ref()
            .and_then(|v| v.get("browser_id").and_then(|n| n.as_str()))
            .unwrap_or("")
            .to_string();
        let browser_name = parsed
            .as_ref()
            .and_then(|v| v.get("browser_name").and_then(|n| n.as_str()))
            .unwrap_or("")
            .to_string();
        let result = app.pairing.lock().unwrap().verify(&code);
        match result {
            Ok(()) => {
                app.record_paired_browser(&browser_id, &browser_name);
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
    // Paired browsers (app only, token required; round 10, item
    // 15): the real count of browsers paired by code, plus their
    // names and pairing times. Browsers paired by pasting a token
    // manually are not in the list — the count is what was
    // recorded, never a guess.
    if method == "GET" && path == "/pair/browsers" {
        let browsers: Vec<PairedBrowser> = app.paired.lock().unwrap().clone();
        let b = serde_json::json!({
            "count": browsers.len(),
            "browsers": browsers
        })
        .to_string();
        let _ = stream.write_all(http_response(200, &b).as_bytes());
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
            store.reload_from_disk(&app.active_job_ids());
            store.all().into_iter().cloned().collect()
        };
        let body = serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string());
        let _ = stream.write_all(http_response(200, &body).as_bytes());
        return;
    }
    if method == "GET" && path == "/settings" {
        let body =
            serde_json::to_string(&app.current_settings()).unwrap_or_else(|_| "{}".to_string());
        let _ = stream.write_all(http_response(200, &body).as_bytes());
        return;
    }
    // Update settings (round 11 extension, features 3/7/10):
    // merges the fields the body carries into the current
    // settings, validates (an invalid proxy fails the save with
    // the field name only — the value is never echoed), and
    // persists. Takes effect on the next download; a running
    // download keeps the settings it started with.
    if method == "POST" && path == "/settings" {
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => match app.update_settings(&v) {
                Ok(saved) => {
                    let b = serde_json::to_string(&saved).unwrap_or_else(|_| "{}".to_string());
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
                r.audio_only,
                r.audio_lang,
                r.subtitle_langs,
                r.audio_format,
                None,
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
            Ok(r) => match list_formats_with_referer_and_proxy(
                &app.data_dir,
                &r.url,
                r.referer,
                app.current_settings().proxy_url.as_deref(),
            ) {
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
    // Playlist listing (round 11 extension, feature 11; token
    // required): list a playlist's entries (titles + URLs, flat
    // — no per-video extraction) so the UI can show "Download
    // all N videos". Errors plainly when the URL is not a
    // playlist.
    if method == "POST" && path == "/media/playlist" {
        let req: Result<PlaylistListRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => match list_playlist_with_proxy(
                &app.data_dir,
                &r.url,
                app.current_settings().proxy_url.as_deref(),
            ) {
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
            store.reload_from_disk(&app.active_job_ids());
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
    // Batch add (round 11 extension, feature 5; token
    // required): queue several items from one action (a series
    // page, the detector's multi-select). Each item is answered
    // individually, so one bad URL never blocks the rest.
    // Must sit before the /downloads/<id>/... catch-all below.
    if method == "POST" && path == "/downloads/batch" {
        let req: Result<BatchRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => {
                let mut added = Vec::new();
                let mut errors = Vec::new();
                for item in r.items.into_iter().take(MAX_BATCH_ITEMS) {
                    match app.add_download(
                        &item.url,
                        item.filename,
                        item.format_id,
                        item.referer,
                        item.audio_url,
                        item.manifest_text,
                        item.audio_only,
                        item.audio_lang,
                        item.subtitle_langs,
                        item.audio_format,
                        None,
                    ) {
                        Ok(rec) => added.push(rec),
                        Err(e) => errors.push(serde_json::json!({
                            "url": item.url,
                            "error": e,
                        })),
                    }
                }
                let b = serde_json::json!({"added": added, "errors": errors}).to_string();
                let _ = stream.write_all(http_response(202, &b).as_bytes());
            }
            Err(_) => {
                let _ = stream
                    .write_all(http_response(400, r#"{"error":"Invalid JSON body"}"#).as_bytes());
            }
        }
        return;
    }
    // Playlist add (round 11 extension, feature 11; token
    // required): queue every entry of a playlist as its own
    // record. Must sit before the /downloads/<id>/... catch-all
    // below.
    if method == "POST" && path == "/downloads/playlist" {
        let req: Result<PlaylistAddRequest, _> = serde_json::from_str(&body);
        match req {
            Ok(r) => match app.add_playlist(&r.url, r.audio_lang, r.subtitle_langs, r.audio_format)
            {
                Ok((title, added, errors, truncated)) => {
                    let b = serde_json::json!({
                        "playlist": title,
                        "added": added,
                        "errors": errors,
                        "truncated": truncated,
                    })
                    .to_string();
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
                // Round 10, item 13: delete a download (any state).
                // Body {"delete_files": true} also deletes the
                // files from disk; false (or absent) removes only
                // the list entry and keeps the files.
                "delete" => {
                    let delete_files = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("delete_files").and_then(|b| b.as_bool()))
                        .unwrap_or(false);
                    match app.delete_download(id, delete_files) {
                        Ok(files) => {
                            let b = serde_json::json!({
                                "deleted": true,
                                "files_deleted": files
                            })
                            .to_string();
                            let _ = stream.write_all(http_response(200, &b).as_bytes());
                        }
                        Err(e) => {
                            let b = serde_json::json!({"error": e}).to_string();
                            let _ = stream.write_all(http_response(404, &b).as_bytes());
                        }
                    }
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
    audio_only: bool,
    audio_lang: Option<&str>,
    subtitle_langs: &[String],
    audio_format: Option<&str>,
) -> Result<DownloadRecord, String> {
    let token = std::fs::read_to_string(data_dir.join("bridge-token.txt"))
        .map_err(|_| "Bridge token file not found.".to_string())?;
    let token = token.trim();
    if token.is_empty() {
        return Err("Bridge token file is empty.".to_string());
    }
    let body = serde_json::json!({"url": url, "filename": filename, "referer": referer, "audio_url": audio_url, "format_id": format_id, "manifest_text": manifest_text, "audio_only": audio_only, "audio_lang": audio_lang, "subtitle_langs": subtitle_langs, "audio_format": audio_format}).to_string();
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

/// Where the desktop app (FetchForge.exe) lives, for the
/// auto-wake (round 12, item 12): `FETCHFORGE_APP_PATH` wins
/// when it names an existing file (dev/test override, mirroring
/// `FETCHFORGE_FFMPEG_PATH`), else the app binary next to this
/// one — the installer puts FetchForge.exe and
/// fetchforge-bridge.exe in the same folder. None when neither
/// exists (a bridge-only install, or a dev layout): the wake is
/// best-effort and the download still proceeds.
fn desktop_app_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FETCHFORGE_APP_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))?;
    ["FetchForge.exe", "FetchForge"]
        .iter()
        .map(|n| exe_dir.join(n))
        .find(|c| c.is_file())
}

/// Open the desktop app (round 12, item 12 — user order
/// 2026-11-25): when a download is started while the app is off,
/// the extension/native host must trigger the app to open
/// automatically instead of falling back to a typical browser
/// download. The native host already spawns a detached bridge so
/// the download itself survives; this opens the UI on top of it,
/// so the user sees the download in the app instead of wondering
/// where it went. Detached, no wait, no console: the host must
/// never block on the app. Returns true when the app was
/// launched; false (never an error) when no app binary was found
/// or the spawn failed — the download is unaffected either way.
///
/// NEVER spawns from a test binary (`cfg!(test)` guard, the
/// round-11 shutdown-test lesson): a test that woke a real app
/// would open windows on the CI runner. The release binary is
/// compiled without cfg(test), so production is unaffected.
///
/// LIMITATION, stated honestly: this fires only when the bridge
/// itself had to be started (the signal the app is off — the app
/// always spawns a bridge on launch, so a running bridge means
/// the app is open). It does not wake the app when a bridge was
/// already running without the app (possible after a detached
/// bridge outlives an app close), and there is no single-instance
/// plugin in the Tauri app, so a wake in a stale state could open
/// a second window — the laptop retest must check the app-off
/// case end to end.
fn wake_desktop_app() -> bool {
    // Never run a real app launch from a test binary (see above).
    if cfg!(test) {
        return false;
    }
    let Some(path) = desktop_app_path() else {
        eprintln!("FetchForge: desktop app not found next to the bridge; not waking it.");
        return false;
    };
    let mut cmd = std::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000) + DETACHED_PROCESS
        // (0x00000008): no console flash, and the app outlives
        // this short-lived host process.
        cmd.creation_flags(0x08000000 | 0x00000008);
    }
    match cmd.spawn() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("FetchForge: could not open the desktop app ({e}).");
            false
        }
    }
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
                // The popup's "Audio only" choice (extension sends
                // only): deliver an audio file instead of the video.
                let audio_only = value
                    .get("audio_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // The popup's option choices (round 11 extension,
                // features 2/4/13): audio language, subtitle
                // languages, and the audio format for audio-only
                // downloads. Absent for old extension versions.
                let audio_lang = value
                    .get("audio_lang")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let subtitle_langs: Vec<String> = value
                    .get("subtitle_langs")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let audio_format = value
                    .get("audio_format")
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
                    audio_only,
                    audio_lang.as_deref(),
                    &subtitle_langs,
                    audio_format.as_deref(),
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
                                audio_only,
                                audio_lang.as_deref(),
                                &subtitle_langs,
                                audio_format.as_deref(),
                            )
                        }) {
                            Ok(rec) => {
                                // Round 12, item 12 (auto-wake):
                                // the bridge was not running, so the
                                // app is off — open it, so the user
                                // sees this download in the app
                                // instead of a silent background
                                // job. Best-effort: a failed wake
                                // never fails the download.
                                let app_woken = wake_desktop_app();
                                serde_json::json!({"type": "added", "download": rec, "via": "bridge", "bridge_started": true, "app_woken": app_woken})
                            }
                            Err(_second_err) => match app.add_download(
                                url,
                                filename,
                                format_id,
                                referer,
                                audio_url,
                                manifest_text,
                                audio_only,
                                audio_lang,
                                subtitle_langs,
                                audio_format,
                                None,
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
    // Round 11 extension, feature 1: auto-start whatever was
    // Queued when the bridge last stopped (oldest first). Paused
    // records stay paused.
    app.auto_start_queued();
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
            false,
            None,
            &[],
            None,
        )
        .unwrap();
        assert_eq!(rec.id, "dl-1");
        assert_eq!(rec.filename, "x.zip");
        assert_eq!(
            rec.format_id, None,
            "old record JSON must load with defaults"
        );
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
            false,
            None,
            &[],
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
            false,
            None,
            &[],
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
        let r: AddRequest = serde_json::from_str(r#"{"url":"https://example.com/x.zip"}"#).unwrap();
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
            .try_media_download(
                &url,
                Some("clip.mp4".to_string()),
                None,
                None,
                None,
                None,
                false,
                None,
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        assert!(!first.duplicate, "first add queues a new download");
        let second = app
            .try_media_download(
                &url,
                Some("clip.mp4".to_string()),
                None,
                None,
                None,
                None,
                false,
                None,
                Vec::new(),
                None,
                None,
            )
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
    fn audio_only_post_step_skips_audio_files_and_extracts_video() {
        // Needs a real ffmpeg; skip (do not fail) where none exists,
        // so the suite stays green on machines without ffmpeg.
        let dir = test_data_dir("fetchforge-test-audio-post-step");
        let app = App::new(dir.clone());
        let ffmpeg = match fetchforge_engine::find_ffmpeg(&dir) {
            Ok(p) => p,
            Err(_) => return,
        };
        // Case 1: an already-audio file in the CHOSEN format passes
        // through unchanged — no ffmpeg call at all (even garbage
        // bytes pass, since the extension decides, not a probe).
        let audio = dir.join("song.m4a");
        std::fs::write(&audio, b"fake audio bytes").unwrap();
        let out = app.audio_only_post_step(&audio, None, "m4a").unwrap();
        assert_eq!(out, audio);
        assert!(audio.is_file());
        // Case 2: a real muxed mp4 extracts to .m4a and the video
        // file is deleted.
        let video = dir.join("clip.mp4");
        let mk = std::process::Command::new(&ffmpeg)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:d=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                video.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(mk, "fixture build failed");
        let out = app
            .audio_only_post_step(&video, Some(&ffmpeg), "m4a")
            .unwrap();
        assert_eq!(out.file_name().unwrap(), "clip.m4a");
        assert!(out.is_file());
        assert!(
            !video.exists(),
            "the video file must be deleted after extraction"
        );
        // Case 3: garbage in fails, and the video file is KEPT
        // (the caller marks the record Failed; the user still has
        // the download).
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"not a video").unwrap();
        assert!(app
            .audio_only_post_step(&bad, Some(&ffmpeg), "m4a")
            .is_err());
        assert!(
            bad.is_file(),
            "a failed extraction must keep the video file"
        );
        // Case 4 (round 11 extension, feature 13): an audio file
        // in the WRONG format is converted, not passed through —
        // the user asked for mp3, so an .m4a must become one.
        let m4a = dir.join("track.m4a");
        let mk = std::process::Command::new(&ffmpeg)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:d=1",
                "-c:a",
                "aac",
                m4a.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(mk, "m4a fixture build failed");
        let out = app
            .audio_only_post_step(&m4a, Some(&ffmpeg), "mp3")
            .unwrap();
        assert_eq!(out.file_name().unwrap(), "track.mp3");
        assert!(out.is_file());
        assert!(
            !m4a.exists(),
            "the source file must be deleted after conversion"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_only_add_does_not_match_completed_video() {
        // Round 8: "Audio only" for a URL whose video already
        // downloaded must queue a new download, not report
        // "Already downloaded" — while a video re-add still does.
        // (The spawned workers fail in the background — no network
        // for example.com here — which is harmless: the assertions
        // are on the returned records.)
        let dir = test_data_dir("fetchforge-test-audio-dedupe");
        let app = App::new(dir.clone());
        let download_dir = app.current_settings().download_dir.clone();
        let mut video = test_record("video-1", "clip.mp4", Status::Completed);
        video.url = "https://example.com/clip.mp4".to_string();
        video.destination = download_dir.clone();
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(video);
            let _ = store.save();
        }
        let audio = app
            .try_media_download(
                "https://example.com/clip.mp4",
                Some("clip.mp4".to_string()),
                None,
                None,
                None,
                None,
                true,
                None,
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        assert!(!audio.duplicate, "audio must not match the completed video");
        assert!(audio.audio_only, "the flag must be stored on the record");
        assert_ne!(audio.id, "video-1", "audio must queue a new record");
        let again = app
            .try_media_download(
                "https://example.com/clip.mp4",
                Some("clip.mp4".to_string()),
                None,
                None,
                None,
                None,
                false,
                None,
                Vec::new(),
                None,
                None,
            )
            .unwrap();
        assert!(
            again.duplicate,
            "a video re-add must still match the completed video"
        );
        assert_eq!(again.id, "video-1");
        let _ = std::fs::remove_dir_all(&dir);
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
        let download_dir = app.current_settings().download_dir.clone();
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
                false,
                None,
                Vec::new(),
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
        let download_dir = app.current_settings().download_dir.clone();
        let url = format!("http://127.0.0.1:{port}/v.m3u8");
        // Learn the planned filename first: the cleared record must
        // match it exactly, or the test would pass trivially.
        let planned_filename = match plan_media_download_or_drive_fallback_with_options(
            &dir,
            &url,
            None,
            Some("clip"),
            &None,
            None,
            None,
            None,
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

    #[test]
    fn update_settings_merges_validates_and_persists() {
        // Round 11 extension, features 3/7/10: a partial body
        // merges into the current settings and persists to disk;
        // an invalid proxy fails the whole save and the error
        // never contains the proxy value (it may hold
        // credentials); numerics clamp instead of failing.
        let dir = test_data_dir("fetchforge-test-update-settings");
        let app = App::new(dir.clone());
        let saved = app
            .update_settings(&serde_json::json!({
                "bandwidth_kbps": 500,
                "proxy_url": "http://user:secret@proxy.example.com:8080",
                "shutdown_when_finished": true,
                "notifications_enabled": false,
                "max_simultaneous": 0,
            }))
            .unwrap();
        assert_eq!(saved.bandwidth_kbps, Some(500));
        // validate_proxy_url normalises the URL (the url crate
        // adds the empty path "/"); the stored form is the
        // normalised one.
        assert_eq!(
            saved.proxy_url.as_deref(),
            Some("http://user:secret@proxy.example.com:8080/")
        );
        assert!(saved.shutdown_when_finished);
        assert!(!saved.notifications_enabled);
        assert_eq!(saved.max_simultaneous, 1, "0 clamps to 1");
        // Persisted: a fresh load reads the same values.
        let reloaded = app.current_settings();
        assert_eq!(reloaded.bandwidth_kbps, Some(500));
        assert!(reloaded.shutdown_when_finished);
        // An invalid proxy fails, names only the field, and
        // leaves the saved settings untouched.
        let err = app
            .update_settings(&serde_json::json!({"proxy_url": "ftp://bad.example.com"}))
            .unwrap_err();
        assert!(err.contains("proxy_url"), "error: {err}");
        assert!(
            !err.contains("ftp://"),
            "error must not echo the value: {err}"
        );
        assert_eq!(
            app.current_settings().proxy_url.as_deref(),
            Some("http://user:secret@proxy.example.com:8080/")
        );
        // bandwidth 0 means unlimited.
        let saved = app
            .update_settings(&serde_json::json!({"bandwidth_kbps": 0}))
            .unwrap();
        assert_eq!(saved.bandwidth_kbps, None);
    }

    #[test]
    fn try_downgrade_refuses_without_a_ladder() {
        // Round 11 extension, feature 9: no picked height, no
        // format id, or an already-attempted downgrade means no
        // retry — without ever touching the network.
        let dir = test_data_dir("fetchforge-test-downgrade-guard");
        let app = App::new(dir);
        let mut rec = test_record("no-height", "clip.mp4", Status::Failed);
        rec.format_id = Some("1080".to_string());
        rec.picked_height = None;
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(rec.clone());
        }
        assert!(!app.try_downgrade(&rec));
        let mut rec2 = test_record("already-tried", "clip.mp4", Status::Failed);
        rec2.format_id = Some("1080".to_string());
        rec2.picked_height = Some(1080);
        rec2.downgrade_attempted = true;
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(rec2.clone());
        }
        assert!(!app.try_downgrade(&rec2));
        let mut rec3 = test_record("gltf", "model.gltf", Status::Failed);
        rec3.format_id = Some("x".to_string());
        rec3.picked_height = Some(1080);
        rec3.stream_kind = Some("gltf".to_string());
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(rec3.clone());
        }
        assert!(!app.try_downgrade(&rec3));
    }

    #[test]
    fn auto_start_queued_spawns_queued_only() {
        // Round 11 extension, feature 1: Queued records get a
        // worker (visible in the job map immediately — the
        // worker thread itself fails fast on the fake URL, which
        // is harmless here); Paused records are left alone.
        let dir = test_data_dir("fetchforge-test-auto-start");
        let app = App::new(dir);
        let queued = test_record("q-1", "a.zip", Status::Queued);
        let paused = test_record("p-1", "b.zip", Status::Paused);
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(queued);
            store.upsert(paused);
            let _ = store.save();
        }
        app.auto_start_queued();
        assert!(
            app.jobs.lock().unwrap().contains_key("q-1"),
            "queued record must get a worker"
        );
        assert!(
            !app.jobs.lock().unwrap().contains_key("p-1"),
            "paused record must not get a worker"
        );
        // Tidy: cancel the spawned worker so it stops promptly.
        let job = app.jobs.lock().unwrap().get("q-1").cloned();
        if let Some(job) = job {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn auto_wake_finds_app_by_override_and_never_spawns_from_tests() {
        // Round 12, item 12: the finder prefers
        // FETCHFORGE_APP_PATH when it names a real file, and
        // yields None for a missing override when no app sits
        // next to the test binary (there is none in target/).
        // The wake itself NEVER spawns from a test binary — even
        // with the override pointing at a real executable — so
        // this test asserts the guard, not a launched app.
        let dir = test_data_dir("fetchforge-test-auto-wake");
        let fake_app = dir.join("FetchForge.exe");
        std::fs::write(&fake_app, b"fake").unwrap();
        std::env::set_var("FETCHFORGE_APP_PATH", &fake_app);
        assert_eq!(desktop_app_path(), Some(fake_app.clone()));
        assert!(
            !wake_desktop_app(),
            "a test binary must never launch the app"
        );
        std::env::set_var("FETCHFORGE_APP_PATH", dir.join("does-not-exist"));
        assert_eq!(
            desktop_app_path(),
            None,
            "a missing override falls through to no sibling app"
        );
        assert!(!wake_desktop_app());
        std::env::remove_var("FETCHFORGE_APP_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_guard_fires_once_and_only_when_idle() {
        // Round 11 extension, feature 7: with the setting on and
        // nothing remaining, the guard fires (on non-Windows the
        // shutdown itself is only a logged warning, so the test
        // asserts the one-shot guard instead of a real shutdown).
        // A remaining Queued record blocks it.
        let dir = test_data_dir("fetchforge-test-shutdown-guard");
        let app = App::new(dir);
        app.update_settings(&serde_json::json!({"shutdown_when_finished": true}))
            .unwrap();
        let done = test_record("done-1", "a.zip", Status::Completed);
        let waiting = test_record("wait-1", "b.zip", Status::Queued);
        {
            let mut store = app.store.lock().unwrap();
            store.upsert(done);
            store.upsert(waiting);
            let _ = store.save();
        }
        app.maybe_shutdown_after_completion("done-1");
        assert!(
            !app.shutdown_triggered.load(Ordering::Relaxed),
            "a queued record must block the shutdown"
        );
        {
            let mut store = app.store.lock().unwrap();
            store.remove("wait-1");
            let _ = store.save();
        }
        app.maybe_shutdown_after_completion("done-1");
        assert!(
            app.shutdown_triggered.load(Ordering::Relaxed),
            "with nothing remaining, the guard must fire"
        );
        // A second call is a no-op (the guard is already set).
        app.maybe_shutdown_after_completion("done-1");
        assert!(app.shutdown_triggered.load(Ordering::Relaxed));
    }
}
