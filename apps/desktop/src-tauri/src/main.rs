// Tauri shell for FetchForge. The download logic lives in fetchforge-engine;
// this file only wires Tauri commands to it. The bridge binary in
// crates/bridge shares the same engine and is the path browser extensions use.
//
// NOTE: This Tauri crate is built on Windows (see scripts/build-windows.ps1
// and .github/workflows/windows-build.yml). It is NOT compiled on the Linux
// dev VM, which has no WebView2 / Windows toolchain; see docs/TEST_REPORT.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fetchforge_engine::{
    category_for_filename, download, probe, sanitize_filename, stop_reason, DownloadOptions,
    DownloadRecord, Status, Store,
};
use std::collections::HashMap;
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
                id: format!("dl-{}", now_unix()),
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
    let id = format!("dl-{}-{}", now_unix(), std::process::id());
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

fn main() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut store = Store::load(dir.join("downloads.json"));
    store.recover_interrupted();
    let _ = store.save();
    let state = Arc::new(State {
        store: Mutex::new(store),
        jobs: Mutex::new(HashMap::new()),
        download_dir: download_dir(),
    });
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            add_download,
            pause_download,
            resume_download,
            cancel_download
        ])
        .run(tauri::generate_context!())
        .expect("error while running FetchForge");
}
