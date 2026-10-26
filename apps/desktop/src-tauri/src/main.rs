// Tauri shell for FetchForge. The download logic lives in fetchforge-engine;
// this file only wires Tauri commands to it. The bridge binary in
// crates/bridge shares the same engine and is the path browser extensions use.
//
// NOTE: This Tauri crate is built on Windows (see scripts/build-windows.ps1
// and .github/workflows/windows-build.yml). It is NOT compiled on the Linux
// dev VM, which has no WebView2 / Windows toolchain; see docs/TEST_REPORT.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fetchforge_engine::{
    category_for_filename, download, generate_download_id, probe, sanitize_filename, stop_reason, DownloadOptions,
    DownloadRecord, Status, Store,
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
    state
        .store
        .lock()
        .unwrap()
        .all()
        .into_iter()
        .cloned()
        .collect()
}

#[tauri::command]
fn add_download(
    state: tauri::State<Arc<State>>,
    url: String,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
    let clean = fetchforge_engine::validate_url(&url)?;
    let probe_opts = DownloadOptions {
        url: clean.clone(),
        ..Default::default()
    };
    let (filename, total, resumable) = match probe(&probe_opts) {
        Ok(p) => (
            filename
                .as_deref()
                .map(sanitize_filename)
                .or(p.filename.as_deref().map(sanitize_filename))
                .unwrap_or_else(|| "download".to_string()),
            p.content_length,
            p.accept_ranges,
        ),
        Err(e) => {
            let rec = DownloadRecord {
                id: generate_download_id(),
                url: clean,
                filename: filename.unwrap_or_else(|| "download".to_string()),
                destination: state.download_dir.to_string_lossy().to_string(),
                category: "Other".to_string(),
                status: Status::Failed,
                total_bytes: None,
                downloaded_bytes: 0,
                resumable: false,
                error: Some(e),
                created_unix: now_unix(),
                updated_unix: now_unix(),
            };
            state.store.lock().unwrap().upsert(rec.clone());
            let _ = state.store.lock().unwrap().save();
            return Ok(rec);
        }
    };
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
    };
    // Same idempotent-add rule as the bridge: an already-active job for
    // the same URL + filename is returned, never duplicated (two workers
    // would share one .part path and corrupt each other).
    {
        let mut store = state.store.lock().unwrap();
        for existing in store.all() {
            if existing.url == clean
                && existing.filename == filename
                && existing.destination == state.download_dir.to_string_lossy().to_string()
                && (existing.status == Status::Queued
                    || existing.status == Status::Downloading)
            {
                return Ok(existing.clone());
            }
        }
        store.upsert(rec.clone());
        let _ = store.save();
    }
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
    Ok(rec)
}

fn set_status(state: &Arc<State>, id: &str, status: Status, error: Option<String>) {
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
}

fn run_download(state: &Arc<State>, id: &str, job: &Job) {
    let rec = match state.store.lock().unwrap().get(id) {
        Some(r) => r.clone(),
        None => return,
    };
    set_status(state, id, Status::Downloading, None);
    let opts = DownloadOptions {
        url: rec.url.clone(),
        destination_dir: state.download_dir.clone(),
        filename: Some(rec.filename.clone()),
        max_segments: 4,
        max_retries: 3,
        bandwidth_kbps: None,
        referer: None,
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

/// Feat 1 (NOT VERIFIED on Windows): talk to the local bridge over
/// loopback with plain TcpStream, so no new dependency is needed.
/// The token is read from the app's own data dir, never shown in the
/// UI and never sent anywhere except 127.0.0.1:18443.
fn bridge_post(data_dir: &PathBuf, token: &str, path: &str) -> Result<String, String> {
    use std::net::TcpStream;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
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

fn read_bridge_token(data_dir: &PathBuf) -> Result<String, String> {
    let token = std::fs::read_to_string(data_dir.join("bridge-token.txt"))
        .map_err(|_| "Bridge token not found. Wait for the bridge to start, then try again.".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("Bridge token is empty. Restart FetchForge, then try again.".to_string());
    }
    Ok(token)
}

#[tauri::command]
fn pair_browser(state: tauri::State<Arc<State>>) -> Result<String, String> {
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
fn unpair_all_browsers(state: tauri::State<Arc<State>>) -> Result<(), String> {
    let token = read_bridge_token(&state.data_dir)?;
    let _ = bridge_post(&state.data_dir, &token, "/unpair-all")?;
    Ok(())
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
            pause_download,
            resume_download,
            cancel_download,
            bridge_status,
            pair_browser,
            unpair_all_browsers
        ])
        .build(tauri::generate_context!())
        .expect("error while building FetchForge");
    // F08: kill the bridge on app exit, so it never outlives the app.
    app.run(|_app, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(mut child) = state_for_exit.bridge_child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
}
