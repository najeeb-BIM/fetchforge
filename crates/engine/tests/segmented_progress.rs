//! Segmented aggregate progress test: while a 4-segment download runs
//! against the slow loopback server, the progress callback must report
//! values strictly between 0 and total — not 0 until the very end.
//! Previously the segmented path only called progress once, at completion.
use fetchforge_engine::{download, DownloadOptions};
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn start_server(root: &std::path::Path, port: u16) -> Option<Child> {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/server.py");
    if !script.exists() {
        return None;
    }
    let child = Command::new("python3")
        .arg(script)
        .arg(root)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    for _ in 0..50 {
        if reqwest::blocking::get(format!("http://127.0.0.1:{port}/playlist.m3u8")).is_ok() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn segmented_progress_rises_while_downloading() {
    let root = std::env::temp_dir().join("fetchforge-segprog-root");
    let _ = std::fs::create_dir_all(&root);
    // 2 MiB deterministic file; the server streams /slow.bin in 16 KiB
    // chunks with a 0.15 s delay, so the download lasts several seconds.
    let mut data = Vec::with_capacity(2 * 1024 * 1024);
    for i in 0..(2 * 1024 * 1024) {
        data.push(((i * 13) % 256) as u8);
    }
    std::fs::write(root.join("slow.bin"), &data).unwrap();
    let mut server = match start_server(&root, 8772) {
        Some(c) => c,
        None => {
            eprintln!("skipping: test server unavailable");
            return;
        }
    };
    let dir = std::env::temp_dir().join("fetchforge-segprog-out");
    let _ = std::fs::create_dir_all(&dir);
    let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let progress = move |p: fetchforge_engine::Progress| {
        seen_c.lock().unwrap().push(p.downloaded);
    };
    let paused = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let opts = DownloadOptions {
        url: "http://127.0.0.1:8772/slow.bin".to_string(),
        destination_dir: dir.clone(),
        filename: Some("slow-seg.bin".to_string()),
        max_segments: 4,
        max_retries: 0,
        bandwidth_kbps: None,
        referer: None,
        user_agent: None,
        organise_by_category: false,
    };
    let path =
        download(&opts, &progress, &paused, &cancelled).expect("segmented download should succeed");
    let _ = server.kill();
    let values = seen.lock().unwrap().clone();
    let total = 2 * 1024 * 1024u64;
    assert!(
        values.iter().any(|&v| v > 0 && v < total),
        "expected intermediate progress (0 < v < {total}), got: {values:?}"
    );
    assert!(
        values.last() == Some(&total),
        "final progress should be total {total}, got: {values:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), data);
    let _ = std::fs::remove_file(&path);
}
