//! FFmpegProcessor (spec §11): locate ffmpeg, provision it on first
//! use, and merge separate audio/video tracks without re-encoding.
//!
//! FFMPEG PROVISIONING (decided 2026-11-21, numbers verified on the
//! dev VM by downloading the real release assets):
//!
//! * ffmpeg is DOWNLOADED ON FIRST USE, only when a merge is
//!   actually needed. The Windows build (gyan.dev essentials,
//!   ffmpeg 9.1) is a 119,467,219-byte zip whose ffmpeg.exe alone is
//!   110,590,976 bytes (~105 MiB): bundling it would more than
//!   double the installer for a tool many downloads never need
//!   (combined single-file formats need no merge). A system ffmpeg
//!   on PATH is used first when present, so users who already have
//!   it pay nothing; otherwise the pinned zip is downloaded once,
//!   SHA-256-verified, and only `bin/ffmpeg.exe` is extracted into
//!   `<data-dir>/tools/ffmpeg/`. On non-Windows there is no pinned
//!   binary: a system ffmpeg is required and the error says how to
//!   install one (the product targets Windows; Linux/macOS users
//!   install ffmpeg from their package manager).
//!
//! Lookup order:
//!   1. `FETCHFORGE_FFMPEG_PATH` env override (dev and tests; an
//!      override that does not exist is an error, never silently
//!      ignored).
//!   2. PATH (a system install).
//!   3. `<data-dir>/tools/ffmpeg/` (first-use download location).
//!   4. (`ensure_ffmpeg` only) Download the pinned release, verify
//!      SHA-256, extract `bin/ffmpeg.exe`.
//!
//! This module also owns `download_verified`, the shared
//! download-a-tool-release-with-hash-verification helper used by the
//! yt-dlp provisioning in extract.rs.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Pinned ffmpeg release (Windows only, gyan.dev essentials build).
/// Hash verified 2026-11-21 by downloading the 119,467,219-byte zip
/// and comparing against the published .sha256 file. The URL is the
/// versioned package URL, not the moving `ffmpeg-release-essentials`
/// alias, so the pinned hash cannot go stale on a new ffmpeg release.
pub const FFMPEG_VERSION: &str = "9.1";
pub const FFMPEG_ZIP_URL_WINDOWS: &str =
    "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.1-essentials_build.zip";
pub const FFMPEG_ZIP_SHA256_WINDOWS: &str =
    "b9bbbb3acece000cbeec994f856649354e52c773807841e78908374b24d658fd";

fn ffmpeg_exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Test-only: download_verified hashes incrementally while
/// streaming, so production code never hashes a whole file at once.
#[cfg(test)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(to_hex(&h.finalize()))
}

pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Lookup only: no download.
pub fn find_ffmpeg(data_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("FETCHFORGE_FFMPEG_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "FETCHFORGE_FFMPEG_PATH is set to {} but that file does not exist.",
            p.display()
        ));
    }
    if let Some(p) = find_on_path(ffmpeg_exe_name()) {
        return Ok(p);
    }
    let tools = data_dir
        .join("tools")
        .join("ffmpeg")
        .join(ffmpeg_exe_name());
    if tools.is_file() {
        return Ok(tools);
    }
    Err("ffmpeg was not found on PATH or in the FetchForge tools folder.".to_string())
}

/// Lookup, downloading the pinned ffmpeg build (Windows only) when
/// no ffmpeg is available. On non-Windows there is no pinned binary,
/// so the error tells the user how to install one.
pub fn ensure_ffmpeg(data_dir: &Path) -> Result<PathBuf, String> {
    match find_ffmpeg(data_dir) {
        Ok(p) => Ok(p),
        Err(_) => {
            if !cfg!(windows) {
                return Err(
                    "This quality needs ffmpeg to merge the separate video and audio streams, \
                     and ffmpeg is not installed. Install it with your package manager \
                     (for example: sudo apt install ffmpeg), or set FETCHFORGE_FFMPEG_PATH \
                     to an ffmpeg binary, then try again."
                        .to_string(),
                );
            }
            let tools_dir = data_dir.join("tools").join("ffmpeg");
            std::fs::create_dir_all(&tools_dir).map_err(|e| e.to_string())?;
            let zip_path = tools_dir.join(format!("ffmpeg-{FFMPEG_VERSION}-essentials_build.zip"));
            // One-time ~114 MB download; only the ffmpeg.exe inside
            // (~105 MB) is kept, the zip is deleted afterwards.
            download_verified(
                FFMPEG_ZIP_URL_WINDOWS,
                FFMPEG_ZIP_SHA256_WINDOWS,
                &zip_path,
                "ffmpeg (needed to merge video and audio)",
            )?;
            let dest = tools_dir.join("ffmpeg.exe");
            match extract_ffmpeg_exe(&zip_path, &dest) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&zip_path);
                    Ok(dest)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&zip_path);
                    Err(e)
                }
            }
        }
    }
}

/// Download `url` to `dest` (via a `.tmp` file + atomic rename),
/// verifying the SHA-256 against `expected_sha256` before the rename.
/// A hash mismatch deletes the download and is always an error: a
/// mismatched binary is never executed.
pub(crate) fn download_verified(
    url: &str,
    expected_sha256: &str,
    dest: &Path,
    what: &str,
) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.2")
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| {
        format!("Could not download {what}. Check your connection and try again. ({e})")
    })?;
    if !resp.status().is_success() {
        return Err(format!(
            "Could not download {what}: the server answered HTTP {}. Try again later.",
            resp.status()
        ));
    }
    let tmp = dest.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "Download of {what} failed halfway. Check your connection and try again. ({e})"
                ));
            }
        }
    }
    drop(file);
    let actual = to_hex(&hasher.finalize());
    if actual != expected_sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "Downloaded {what} did not match its expected checksum, so it was discarded and not used. \
             Try again later; if this keeps happening, the release may have changed."
        ));
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// Extract only `*/bin/ffmpeg.exe` from a (hash-verified) gyan.dev
/// essentials zip into `dest`. No other entry is read, and the
/// destination path is fixed, so a hostile zip entry name cannot
/// escape the tools folder (no ZipSlip: entry names are never used
/// as output paths).
fn extract_ffmpeg_exe(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| {
        format!("The downloaded ffmpeg archive could not be opened ({e}). Try again.")
    })?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if name.to_ascii_lowercase().ends_with("/bin/ffmpeg.exe") {
            let tmp = dest.with_extension("tmp");
            let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            drop(out);
            // Sanity: a real ffmpeg.exe is ~105 MB. Anything tiny is
            // not ffmpeg, whatever the zip claimed.
            let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            if size < 1_000_000 {
                let _ = std::fs::remove_file(&tmp);
                return Err(
                    "The downloaded ffmpeg archive did not contain a usable ffmpeg.exe."
                        .to_string(),
                );
            }
            std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("The downloaded ffmpeg archive did not contain bin/ffmpeg.exe.".to_string())
}

fn no_console_window(cmd: &mut Command) {
    // On Windows, ffmpeg.exe is a console program: without
    // CREATE_NO_WINDOW it flashes a black console window next to the
    // app for every merge. (Same flag the desktop app uses for the
    // bridge child and yt-dlp spawns.)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Merge a video-only file and an audio-only file into one container
/// with ffmpeg, stream-copied (`-c copy`: no re-encode, no quality
/// loss). The output extension decides the container (`.mp4` for
/// DASH). Used by DASH downloads whose manifest keeps audio and
/// video in separate AdaptationSets. A failed merge removes any
/// partial output and returns ffmpeg's error tail. Runs on the
/// caller's worker thread; pause/cancel are checked by the caller
/// before and after (a stream copy is seconds, not minutes).
pub fn merge_av(
    ffmpeg: &Path,
    video: &Path,
    audio: &Path,
    output: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-i"])
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("Could not run ffmpeg to merge the video and audio tracks ({e})."))?;
    if out.status.success() && output.is_file() {
        return Ok(());
    }
    let _ = std::fs::remove_file(output);
    let tail: String = String::from_utf8_lossy(&out.stderr)
        .chars()
        .rev()
        .take(300)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Err(format!(
        "ffmpeg could not merge the video and audio tracks. {tail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("fetchforge-test-{tag}-{pid}-{nanos}"))
    }

    #[test]
    fn sha256_of_known_content() {
        let dir = unique_temp_dir("sha");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One-shot loopback HTTP server for download tests: serves `body`
    /// once (plain HTTP, no TLS), then exits. Returns the URL.
    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}/tool.bin"), handle)
    }

    #[test]
    fn download_verified_accepts_matching_hash_rejects_mismatch() {
        let dir = unique_temp_dir("dl-verified");
        std::fs::create_dir_all(&dir).unwrap();
        let body = b"fake-tool-binary-content".to_vec();
        let mut h = Sha256::new();
        h.update(&body);
        let good_hash = to_hex(&h.finalize());
        // Matching hash: file lands, temp file gone.
        let (url, handle) = serve_once(body.clone());
        let dest = dir.join("tool.bin");
        download_verified(&url, &good_hash, &dest, "test tool").unwrap();
        handle.join().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!dir.join("tool.tmp").exists());
        // Mismatching hash: error, nothing left behind (no dest, no tmp).
        let (url, handle) = serve_once(body);
        let dest2 = dir.join("tool2.bin");
        let err = download_verified(&url, &"0".repeat(64), &dest2, "test tool").unwrap_err();
        handle.join().unwrap();
        assert!(err.contains("did not match"), "error: {err}");
        assert!(!dest2.exists());
        assert!(!dir.join("tool2.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_zip_extraction_pulls_only_ffmpeg_exe() {
        let dir = unique_temp_dir("ffzip");
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("ffmpeg.zip");
        // Build a zip with a fake 2 MB ffmpeg.exe plus decoys.
        let f = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("ffmpeg-9.1-essentials_build/bin/ffmpeg.exe", opts)
            .unwrap();
        zip.write_all(&vec![0x4d; 2_000_000]).unwrap();
        zip.start_file("ffmpeg-9.1-essentials_build/bin/ffprobe.exe", opts)
            .unwrap();
        zip.write_all(b"decoy").unwrap();
        zip.start_file("ffmpeg-9.1-essentials_build/doc/readme.txt", opts)
            .unwrap();
        zip.write_all(b"decoy").unwrap();
        zip.finish().unwrap();
        let dest = dir.join("ffmpeg.exe");
        extract_ffmpeg_exe(&zip_path, &dest).unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 2_000_000);
        // A zip without ffmpeg.exe is an error, not a silent skip.
        let zip2 = dir.join("no-ffmpeg.zip");
        let f = std::fs::File::create(&zip2).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        zip.start_file("other.txt", opts).unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        assert!(extract_ffmpeg_exe(&zip2, &dir.join("out.exe")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffmpeg_resolution_env_override_and_tools_folder() {
        let dir = unique_temp_dir("ff-resolve");
        std::fs::create_dir_all(dir.join("tools").join("ffmpeg")).unwrap();
        let tools_ff = dir
            .join("tools")
            .join("ffmpeg")
            .join(ffmpeg_exe_name());
        std::fs::write(&tools_ff, b"fake").unwrap();
        // Env override wins (PATH may contain a real ffmpeg on this
        // machine; the override test is order-independent either way).
        std::env::set_var("FETCHFORGE_FFMPEG_PATH", &tools_ff);
        assert_eq!(find_ffmpeg(&dir).unwrap(), tools_ff);
        // An override pointing at nothing is an error, not a skip.
        std::env::set_var("FETCHFORGE_FFMPEG_PATH", dir.join("does-not-exist"));
        assert!(find_ffmpeg(&dir).is_err());
        std::env::remove_var("FETCHFORGE_FFMPEG_PATH");
        // Without the override, the tools folder or PATH is found.
        assert!(find_ffmpeg(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_av_combines_video_and_audio_without_reencoding() {
        // Needs a real ffmpeg; skip (do not fail) where none exists,
        // so the suite stays green on machines without ffmpeg.
        let ffmpeg = match find_on_path(ffmpeg_exe_name()) {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("merge");
        std::fs::create_dir_all(&dir).unwrap();
        // 1 s of silent black video + 1 s of tone, via ffmpeg itself.
        let video = dir.join("v.mp4");
        let audio = dir.join("a.m4a");
        let mk = |args: &[&str]| {
            Command::new(&ffmpeg)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "color=c=black:s=64x64:d=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p",
            video.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y", "-f", "lavfi", "-i", "sine=frequency=440:d=1",
            "-c:a", "aac", audio.to_str().unwrap()
        ]));
        let out = dir.join("merged.mp4");
        merge_av(&ffmpeg, &video, &audio, &out).unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        // Merging garbage is a clear error with no partial output.
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"not a video").unwrap();
        let out2 = dir.join("merged2.mp4");
        assert!(merge_av(&ffmpeg, &bad, &audio, &out2).is_err());
        assert!(!out2.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
