//! Failure-case tests: the engine must fail with a clear message, never
//! crash, hang silently, or claim success on an error.
//! Covers: 401 / 403 / 429 / 500 / 404, no connection (closed port),
//! missing destination (destination path is a file), empty URL,
//! zero segments, zero bandwidth (treated as unlimited), and a
//! download to a read-only-ish location via a file-as-directory.
use fetchforge_engine::{download, probe, DownloadOptions};
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
    for _ in 0..50 {
        if reqwest::blocking::get(format!("http://127.0.0.1:{port}/playlist.m3u8")).is_ok() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn opts(url: &str, dir: &std::path::Path) -> DownloadOptions {
    DownloadOptions {
        url: url.to_string(),
        destination_dir: dir.to_path_buf(),
        filename: None,
        max_segments: 1,
        max_retries: 0,
        bandwidth_kbps: None,
        referer: None,
        user_agent: None,
        organise_by_category: false,
    }
}

#[test]
fn http_errors_are_clear_not_crashes() {
    let root = std::env::temp_dir().join("fetchforge-fail-root");
    let _ = std::fs::create_dir_all(&root);
    let mut server = match start_server(&root, 8769) {
        Some(c) => c,
        None => {
            eprintln!("skipping: test server unavailable");
            return;
        }
    };
    let dir = std::env::temp_dir().join("fetchforge-fail-out");
    let _ = std::fs::create_dir_all(&dir);
    for (path, needle) in [
        ("/unauthorized", "401"),
        ("/forbidden", "401/403"),
        ("/rate-limited", "429"),
        ("/server-error", "500"),
        ("/does-not-exist.bin", "404"),
    ] {
        let o = opts(&format!("http://127.0.0.1:8769{path}"), &dir);
        let err = probe(&o).expect_err("probe should fail");
        assert!(
            err.contains(needle),
            "for {path}: expected '{needle}' in '{err}'"
        );
    }
    let _ = server.kill();
}

#[test]
fn no_connection_fails_fast_with_clear_message() {
    // Port 1 on loopback is (practically) always closed: this simulates
    // no internet / server down without waiting for a long timeout.
    let dir = std::env::temp_dir().join("fetchforge-fail-out");
    let o = opts("http://127.0.0.1:1/file.bin", &dir);
    let err = probe(&o).expect_err("closed-port probe should fail");
    assert!(
        err.to_lowercase().contains("connect") || err.contains("Network"),
        "unexpected message: {err}"
    );
}

#[test]
fn empty_url_fails_clearly() {
    let dir = std::env::temp_dir().join("fetchforge-fail-out");
    let o = opts("", &dir);
    assert!(probe(&o).is_err());
}

#[test]
fn file_as_destination_fails_clearly() {
    // Destination is an existing file, not a directory. This test needs a
    // LIVE server (unlike the first version, which ran while the other
    // test's server was down and so failed at probe time instead).
    let root = std::env::temp_dir().join("fetchforge-fail-root");
    let _ = std::fs::create_dir_all(&root);
    std::fs::write(root.join("tiny2.bin"), b"hi").unwrap();
    let mut server = match start_server(&root, 8771) {
        Some(c) => c,
        None => {
            eprintln!("skipping: test server unavailable");
            return;
        }
    };
    let file_dest = std::env::temp_dir().join("fetchforge-not-a-dir");
    std::fs::write(&file_dest, b"x").unwrap();
    let paused = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let o2 = opts("http://127.0.0.1:8771/tiny2.bin", &file_dest);
    let err = download(&o2, &|_| {}, &paused, &cancelled).expect_err("should fail");
    let _ = server.kill();
    assert!(
        err.contains("Cannot create download folder") || err.contains("Cannot write"),
        "unexpected message: {err}"
    );
    let _ = std::fs::remove_file(&file_dest);
}

#[test]
fn zero_segments_and_zero_bandwidth_do_not_crash() {
    // Zero is not a valid segment count and zero kbps is not a valid
    // limit; both must degrade safely (single-stream, unlimited) rather
    // than divide by zero or hang. Verified against a real server file.
    let root = std::env::temp_dir().join("fetchforge-fail-root");
    let _ = std::fs::create_dir_all(&root);
    std::fs::write(root.join("tiny.bin"), b"hello").unwrap();
    let mut server = match start_server(&root, 8770) {
        Some(c) => c,
        None => {
            eprintln!("skipping: test server unavailable");
            return;
        }
    };
    let dir = std::env::temp_dir().join("fetchforge-zero-out");
    let _ = std::fs::create_dir_all(&dir);
    let paused = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let mut o = opts("http://127.0.0.1:8770/tiny.bin", &dir);
    o.max_segments = 0;
    o.bandwidth_kbps = Some(0);
    o.filename = Some("tiny-zero.bin".to_string());
    let path =
        download(&o, &|_| {}, &paused, &cancelled).expect("zero settings should still download");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    let _ = std::fs::remove_file(&path);
    let _ = server.kill();
}
