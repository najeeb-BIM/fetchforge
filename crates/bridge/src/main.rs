//! FetchForge localhost bridge.
//!
//! * Binds ONLY to 127.0.0.1 (loopback). Never 0.0.0.0.
//! * Extension API requires `X-FetchForge-Token`, a per-install secret
//!   stored in the user's data dir and shown in the desktop app's Settings.
//!   The token is never logged and never leaves the machine.
//! * Rejects non-extension / non-loopback `Origin` headers.
//! * `--native-host` speaks the Chrome/Firefox native-messaging framing
//!   (4-byte little-endian length + JSON) on stdin/stdout. Only the
//!   extension ID registered in the host manifest can invoke it.
//!
//! Usage:
//!   fetchforge-bridge serve [--port 18443] [--data-dir DIR]
//!   fetchforge-bridge native-host [--data-dir DIR]
//!   fetchforge-bridge print-token [--data-dir DIR]

use fetchforge_engine::{
    category_for_filename, download, generate_download_id, generate_secure_token, probe,
    sanitize_filename, stop_reason, DownloadOptions, DownloadRecord, Status, Store,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
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
}

impl App {
    fn new(data_dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&data_dir);
        let (settings, settings_warning) = load_settings_with_warning(&data_dir);
        if let Some(w) = &settings_warning {
            eprintln!("FetchForge WARNING: {w}");
        }
        let token = load_or_create_token(&data_dir);
        let mut store = Store::load(data_dir.join("downloads.json"));
        if let Some(w) = &store.load_warning {
            eprintln!("FetchForge WARNING: {w}");
        }
        store.recover_interrupted();
        let _ = store.save();
        Arc::new(Self {
            settings,
            data_dir,
            token: Mutex::new(token),
            pairing: Mutex::new(fetchforge_engine::pairing::PairingState::new()),
            store: Mutex::new(store),
            jobs: Mutex::new(HashMap::new()),
            active: Mutex::new(0),
        })
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
    ) -> Result<DownloadRecord, String> {
        let clean_url = fetchforge_engine::validate_url(url)?;
        // Probe first so we can record a real filename / size / resumability
        // and fail fast on 404 / denied links instead of queueing a dead job.
        let probe_opts = DownloadOptions {
            url: clean_url.clone(),
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
                };
                self.store.lock().unwrap().upsert(rec.clone());
                let _ = self.store.lock().unwrap().save();
                return Ok(rec);
            }
        };
        // Idempotent add (safe to run twice): if the same URL + filename
        // is already Queued or Downloading, return that job instead of
        // starting a second worker. Without this, two workers derive the
        // same .part path and corrupt each other (found by the duplicate
        // test on 2026-10-26: one record reported "got 0 bytes" while
        // progress said complete). The check + insert happen under one
        // store lock so two simultaneous POSTs cannot both slip through.
        {
            let mut store = self.store.lock().unwrap();
            for existing in store.all() {
                if existing.url == clean_url
                    && existing.filename == filename
                    && existing.destination == self.settings.download_dir
                    && (existing.status == Status::Queued || existing.status == Status::Downloading)
                {
                    return Ok(existing.clone());
                }
            }
            let id = generate_download_id();
            let rec = DownloadRecord {
                id: id.clone(),
                url: clean_url.clone(),
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
        let rec = match self.store.lock().unwrap().get(id) {
            Some(r) => r.clone(),
            None => return Err("unknown download".to_string()),
        };
        self.set_status(id, Status::Downloading, None);
        let opts = DownloadOptions {
            url: rec.url.clone(),
            destination_dir: PathBuf::from(&rec.destination),
            filename: Some(rec.filename.clone()),
            max_segments: self.settings.max_segments,
            max_retries: self.settings.max_retries,
            bandwidth_kbps: self.settings.bandwidth_kbps,
            referer: None,
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

    fn set_status(&self, id: &str, status: Status, error: Option<String>) {
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
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
    filename: Option<String>,
}

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
    if method == "GET" && path == "/downloads" {
        let list: Vec<DownloadRecord> = app
            .store
            .lock()
            .unwrap()
            .all()
            .into_iter()
            .cloned()
            .collect();
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
            Ok(r) => match app.add_download(&r.url, r.filename) {
                Ok(rec) => {
                    let b = serde_json::to_string(&rec).unwrap();
                    let _ = stream.write_all(http_response(201, &b).as_bytes());
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
                _ => {}
            }
        }
    }
    let _ = stream.write_all(http_response(404, r#"{"error":"Not found"}"#).as_bytes());
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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
                match app.add_download(url, filename) {
                    Ok(rec) => serde_json::json!({"type": "added", "download": rec}),
                    Err(e) => serde_json::json!({"type": "error", "error": e}),
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
    if args.iter().any(|a| a == "native-host") {
        native_host(&app);
        return;
    }
    if args.iter().any(|a| a == "print-token") {
        // Explicit user action only: this command prints the token to
        // stdout so the app can read it. Serve mode never prints it.
        println!("{}", app.token.lock().unwrap());
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
