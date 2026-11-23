//! Round 7 integration tests: bridge mode detection (Fix 1) and
//! interrupted-download recovery (Fix 2). These spawn the real
//! binary, because the behaviour under test IS process startup:
//! which mode main() picks, and whether a short-lived probe writes
//! downloads.json.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fetchforge-bridge"))
}

fn temp_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fetchforge-test-bridge-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bridge-token.txt"), "test-token-1234567890abcdef").unwrap();
    dir
}

/// One download record, serialised the way Store::save writes it
/// (a JSON object keyed by id).
fn write_downloads_json(dir: &PathBuf, status: &str) {
    let record = serde_json::json!({
        "id": "dl-1",
        "url": "https://example.com/movie.m3u8",
        "filename": "movie.ts",
        "destination": dir.join("movie.ts").to_string_lossy(),
        "category": "Video",
        "status": status,
        "total_bytes": null,
        "downloaded_bytes": 1234567,
        "resumable": true,
        "error": null,
        "created_unix": 1700000000,
        "updated_unix": 1700000000,
    });
    let mut map = serde_json::Map::new();
    map.insert("dl-1".to_string(), record);
    std::fs::write(
        dir.join("downloads.json"),
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap(),
    )
    .unwrap();
}

fn read_status(dir: &PathBuf) -> String {
    let text = std::fs::read_to_string(dir.join("downloads.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["dl-1"]["status"].as_str().unwrap().to_string()
}

/// One native-messaging frame: 4-byte little-endian length + JSON.
fn frame(value: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap();
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn read_frame<R: Read>(reader: &mut R) -> serde_json::Value {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).unwrap();
    let len = u32::from_le_bytes(len_buf) as usize;
    assert!(len < 1024 * 1024, "frame too large: {len}");
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Spawn the binary the way Chrome launches the native host: no
/// mode argument (unless `extra_args` adds one), stdin/stdout
/// piped.
fn spawn_native(data_dir: &PathBuf, extra_args: &[&str]) -> Child {
    Command::new(bin_path())
        .args(extra_args)
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Send a framed ping, read the framed reply, then close stdin —
/// which ends the host loop (Chrome closes the pipe when the
/// extension goes away).
fn ping_pong(child: &mut Child) -> serde_json::Value {
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin
        .write_all(&frame(&serde_json::json!({"type": "ping"})))
        .unwrap();
    stdin.flush().unwrap();
    let reply = read_frame(&mut stdout);
    drop(stdin);
    drop(stdout);
    reply
}

fn wait_exit(child: &mut Child) {
    let start = Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert!(status.success(), "bridge exited with {status}");
                return;
            }
            None => {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "bridge did not exit after stdin closed"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_health(port: u16) {
    let start = Instant::now();
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = s.write_all(
                b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            );
            let mut buf = String::new();
            if s.read_to_string(&mut buf).is_ok() && buf.contains("200") {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "bridge never answered /health on port {port}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn native_host_enters_on_piped_stdin_with_no_args() {
    // Fix 1: Chrome launches the host with NO arguments and a pipe
    // on stdin. Before round 7 this fell through to serve mode,
    // failed to bind, and exited without replying — the
    // extension's native path never worked.
    let dir = temp_data_dir("native-noargs");
    let mut child = spawn_native(&dir, &[]);
    let reply = ping_pong(&mut child);
    assert_eq!(reply["type"], "pong");
    assert_eq!(reply["app"], "FetchForge");
    wait_exit(&mut child);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_host_explicit_arg_still_works() {
    let dir = temp_data_dir("native-arg");
    let mut child = spawn_native(&dir, &["native-host"]);
    let reply = ping_pong(&mut child);
    assert_eq!(reply["type"], "pong");
    wait_exit(&mut child);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn serve_arg_wins_over_piped_stdin_and_answers_health() {
    // The desktop app and spawn_detached_bridge spawn with
    // `serve` and a NULL stdin (not a terminal): the explicit arg
    // must win over the stdin test, or the bridge would enter
    // native-host mode and never serve.
    let dir = temp_data_dir("serve-piped");
    let port = free_port();
    let mut child = Command::new(bin_path())
        .arg("serve")
        .arg("--data-dir")
        .arg(&dir)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_health(port);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_probe_never_stomps_downloads_but_serve_recovers() {
    // Fix 2: a native-host probe must NEVER write downloads.json
    // (every extension click launches one); only the process that
    // becomes the server recovers interrupted downloads.
    let dir = temp_data_dir("no-stomp");
    write_downloads_json(&dir, "downloading");
    let mut child = spawn_native(&dir, &[]);
    let reply = ping_pong(&mut child);
    assert_eq!(reply["type"], "pong");
    wait_exit(&mut child);
    assert_eq!(
        read_status(&dir),
        "downloading",
        "a native-host probe must not touch the on-disk status"
    );
    // Now the real server starts over the same data dir: it binds,
    // recovers the interrupted record, and answers /health.
    let port = free_port();
    let mut server = Command::new(bin_path())
        .arg("serve")
        .arg("--data-dir")
        .arg(&dir)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_health(port);
    let _ = server.kill();
    let _ = server.wait();
    assert_eq!(
        read_status(&dir),
        "paused",
        "the serving process must recover the interrupted record"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
