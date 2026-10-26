//! F01: segmented downloads must verify each segment.
//! A server that returns a short segment, or a segment whose
//! Content-Range does not match the requested range, must NOT produce
//! a "completed" file. The download must fail, the partial data must
//! be removed, and no final file must exist. Before the fix these
//! tests fail: the download returns Ok with a corrupt file.

use fetchforge_engine::{download, DownloadOptions};
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn start_audit_server(port: u16) -> Option<Child> {
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/audit/server_audit.py");
    if !script.exists() {
        return None;
    }
    let root = std::env::temp_dir().join("fetchforge-segver-root");
    let _ = std::fs::create_dir_all(&root);
    let child = Command::new("python3")
        .arg(script)
        .arg(root)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    for _ in 0..50 {
        if reqwest::blocking::get(format!("http://127.0.0.1:{port}/zero")).is_ok() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn try_download(
    url: &str,
    filename: &str,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let paused = AtomicBool::new(false);
    let cancelled = AtomicBool::new(false);
    let progress = |_p: fetchforge_engine::Progress| {};
    let opts = DownloadOptions {
        url: url.to_string(),
        destination_dir: dir.to_path_buf(),
        filename: Some(filename.to_string()),
        max_segments: 4,
        max_retries: 0,
        bandwidth_kbps: None,
        referer: None,
        user_agent: None,
        organise_by_category: false,
    };
    download(&opts, &progress, &paused, &cancelled)
}

#[test]
fn short_segment_is_not_completed() {
    let mut server = match start_audit_server(8773) {
        Some(c) => c,
        None => {
            eprintln!("skipping: audit server unavailable");
            return;
        }
    };
    let dir = std::env::temp_dir().join("fetchforge-segver-short");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let res = try_download("http://127.0.0.1:8773/short-segment", "short.bin", &dir);
    let _ = server.kill();
    assert!(res.is_err(), "short segment must fail, got Ok: {res:?}");
    let err = res.unwrap_err();
    assert!(
        err.contains("part") || err.contains("segment"),
        "error should mention the bad segment, got: {err}"
    );
    // No final file and no partial file must remain (part lives in
    // the category subfolder, e.g. Other/short.bin.part).
    assert!(
        !dir.join("Other").join("short.bin").exists(),
        "final file must not exist"
    );
    assert!(
        !dir.join("Other").join("short.bin.part").exists(),
        "partial data must be removed on failure"
    );
}

#[test]
fn mismatched_content_range_is_not_completed() {
    let mut server = match start_audit_server(8774) {
        Some(c) => c,
        None => {
            eprintln!("skipping: audit server unavailable");
            return;
        }
    };
    let dir = std::env::temp_dir().join("fetchforge-segver-mismatch");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let res = try_download("http://127.0.0.1:8774/mismatch-range", "mismatch.bin", &dir);
    let _ = server.kill();
    assert!(
        res.is_err(),
        "mismatched Content-Range must fail, got Ok: {res:?}"
    );
    assert!(
        !dir.join("Other").join("mismatch.bin").exists(),
        "final file must not exist"
    );
}
