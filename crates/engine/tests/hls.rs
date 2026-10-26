//! HLS integration test against the loopback test server.
//! Only runs when python3 is available; skips otherwise.
use fetchforge_engine::download_hls;
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
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
    // Wait for the server to answer.
    for _ in 0..50 {
        if reqwest::blocking::get(format!("http://127.0.0.1:{port}/playlist.m3u8")).is_ok() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn hls_clear_playlist_concatenates_segments() {
    let root = std::env::temp_dir().join("fetchforge-hls-test");
    let _ = std::fs::create_dir_all(&root);
    std::fs::write(root.join("seg1.ts"), b"SEGMENT-ONE-").unwrap();
    std::fs::write(root.join("seg2.ts"), b"SEGMENT-TWO-").unwrap();
    let out_dir = std::env::temp_dir().join("fetchforge-hls-out");
    let _ = std::fs::create_dir_all(&out_dir);
    let mut server = match start_server(&root, 8768) {
        Some(c) => c,
        None => {
            eprintln!("skipping: test server unavailable");
            return;
        }
    };
    let paused = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let result = download_hls(
        "http://127.0.0.1:8768/playlist.m3u8",
        &out_dir,
        "clip.ts",
        &None,
        &|_| {},
        &paused,
        &cancelled,
    );
    let _ = server.kill();
    let path = result.expect("HLS download should succeed");
    let content = std::fs::read(&path).expect("output should exist");
    assert_eq!(content, b"SEGMENT-ONE-SEGMENT-TWO-");
    let _ = std::fs::remove_file(&path);
}
