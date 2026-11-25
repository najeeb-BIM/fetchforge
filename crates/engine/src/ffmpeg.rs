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
///
/// Round 12, item 8: the whole lookup + download is serialised on
/// a process-wide lock. Two jobs starting at once used to race the
/// first-use fetch — both saw "no ffmpeg", both downloaded the zip
/// to the same path, and a job that spawned ffmpeg mid-fetch could
/// fail with "The system cannot find the file specified.
/// (os error 2)" (Windows spawn error for a not-yet-present
/// binary). The cause is timing-supported, not log-proven: no log
/// ever captured the race in the act. With the lock, the second
/// caller waits, then re-checks and finds the finished binary.
/// The lock is held for the whole call (the download is minutes
/// at worst on a slow link, but only the FIRST call in the
/// process ever downloads; every later call finds ffmpeg at once
/// and the lock is uncontended). A poisoned lock (a panicking
/// holder) is treated as free rather than failing every later
/// download.
pub fn ensure_ffmpeg(data_dir: &Path) -> Result<PathBuf, String> {
    static FFMPEG_FETCH_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    let lock = FFMPEG_FETCH_LOCK.get_or_init(|| std::sync::Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    ensure_ffmpeg_locked(data_dir)
}

fn ensure_ffmpeg_locked(data_dir: &Path) -> Result<PathBuf, String> {
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
pub fn merge_av(ffmpeg: &Path, video: &Path, audio: &Path, output: &Path) -> Result<(), String> {
    merge_tracks(ffmpeg, video, Some(audio), None, None, output)
}

/// Extract the audio track of a downloaded video into a standalone
/// audio file with ffmpeg (the popup's "Audio only" choice for
/// direct MP4s and muxed HLS variants). First attempt is a stream
/// copy (`-vn -c:a copy`: no re-encode, no quality loss); that
/// fails only when the source codec cannot live in the target
/// container, in which case a single AAC re-encode is retried
/// (only then — a codec that copies must never be re-encoded).
/// The caller picks the output container (`.m4a` for AAC-family
/// sources); a failed extraction removes any partial output and
/// returns ffmpeg's error tail, exactly like [`merge_av`].
pub fn extract_audio(ffmpeg: &Path, input: &Path, output: &Path) -> Result<(), String> {
    extract_audio_as(ffmpeg, input, output, "m4a")
}

/// [`extract_audio`] with the target format chosen (round 11
/// extension, feature 13): "mp3", "opus" or "m4a". Stream copy
/// first (lossless when the source codec already fits the target
/// container); the re-encode fallback uses the format's encoder
/// (libmp3lame for mp3, libopus for opus, aac for m4a) instead of
/// always AAC — an mp3 target re-encoded as AAC would be an mp3
/// file holding AAC audio. Any other format string falls back to
/// the m4a behaviour.
pub fn extract_audio_as(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    format: &str,
) -> Result<(), String> {
    let run = |args: &[&str]| -> Result<(), String> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-y", "-i"])
            .arg(input)
            .args(args)
            .arg(output)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_console_window(&mut cmd);
        let out = cmd
            .output()
            .map_err(|e| format!("Could not run ffmpeg to extract the audio track ({e})."))?;
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
        Err(format!("ffmpeg could not extract the audio track. {tail}"))
    };
    // Stream copy first: the common case (AAC in MP4) is lossless
    // and near-instant. Re-encode only when the copy is impossible.
    let encoder = match format {
        "mp3" => "libmp3lame",
        "opus" => "libopus",
        _ => "aac",
    };
    match run(&["-vn", "-c:a", "copy"]) {
        Ok(()) => Ok(()),
        Err(copy_err) => match run(&["-vn", "-c:a", encoder]) {
            Ok(()) => Ok(()),
            Err(_) => Err(copy_err),
        },
    }
}

/// Merge video + audio (+ optionally subtitles) with ffmpeg,
/// stream-copied (`-c copy`: no re-encode, no quality loss).
/// `audio` is None for a muxed variant (the video already has
/// sound); `subtitle` is Some((vtt_file, language)) when a
/// subtitle track was downloaded (round 11 extension, feature 2)
/// — it is muxed as mov_text with its language tag, which needs
/// an MP4 output (the caller guarantees that: subtitle downloads
/// re-containerise a muxed .ts variant to .mp4 at plan time).
/// `title` is the metadata title (round 11 extension, feature
/// 12), written when present. A failed merge removes any partial
/// output and returns ffmpeg's error tail, exactly like
/// [`merge_av`].
#[allow(clippy::too_many_arguments)]
pub fn merge_tracks(
    ffmpeg: &Path,
    video: &Path,
    audio: Option<&Path>,
    subtitle: Option<(&Path, &str)>,
    title: Option<&str>,
    output: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-i"]).arg(video);
    let mut next_input = 1;
    let audio_input = if let Some(a) = audio {
        cmd.arg("-i").arg(a);
        let idx = next_input;
        next_input += 1;
        Some(idx)
    } else {
        None
    };
    let subtitle_input = if let Some((s, _)) = subtitle {
        cmd.arg("-i").arg(s);
        let idx = next_input;
        next_input += 1;
        Some(idx)
    } else {
        None
    };
    let _ = next_input;
    cmd.args(["-map", "0:v:0"]);
    if let Some(idx) = audio_input {
        cmd.args(["-map", &format!("{idx}:a:0")]);
    } else {
        // Muxed video: keep its own audio track too (a subtitle
        // merge must not strip the sound).
        cmd.args(["-map", "0:a?"]);
    }
    if let Some(idx) = subtitle_input {
        cmd.args(["-map", &format!("{idx}:s:0")]);
    }
    cmd.args(["-c", "copy"]);
    if subtitle_input.is_some() {
        // WebVTT must be converted to the MP4 subtitle codec;
        // -c copy would fail (mov_text is the only subtitle codec
        // MP4 accepts for text).
        cmd.args(["-c:s", "mov_text"]);
        if let Some((_, lang)) = subtitle {
            if !lang.is_empty() {
                cmd.args(["-metadata:s:s:0", &format!("language={lang}")]);
            }
        }
    }
    if let Some(t) = title {
        if !t.is_empty() {
            cmd.args(["-metadata", &format!("title={t}")]);
        }
    }
    cmd.arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("Could not run ffmpeg to merge the tracks ({e})."))?;
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

/// What a delivered file actually is (round 10, item 8): its video
/// height and overall bitrate, probed from the file itself — the
/// site's label for the picked stream may be wrong (Soap2Day
/// listed 1080p/3 Mbps and delivered 720p/865 kb/s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeQuality {
    pub height: Option<u32>,
    pub bitrate_kbps: Option<u32>,
}

/// Probe a finished file's delivered resolution and bitrate with
/// `ffmpeg -i` (round 10, item 8). ffprobe is NOT in the install
/// (only ffmpeg.exe is extracted), so the info is parsed from
/// ffmpeg's stderr: `Stream #0:0: Video: ... 1280x720 ...` and
/// `Duration: ..., bitrate: 865 kb/s`. Best-effort: None when
/// ffmpeg cannot run, the file is not a video, or nothing parses
/// — the caller never fails a completed download over a probe.
/// `-i` alone exits immediately after printing the file info, so
/// no timeout machinery is needed.
pub fn probe_video_quality(ffmpeg: &Path, file: &Path) -> Option<ProbeQuality> {
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-i")
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut cmd);
    let out = cmd.output().ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    parse_ffmpeg_probe(&stderr)
}

/// Parse `ffmpeg -i` stderr into a ProbeQuality (pure function,
/// unit-tested on canned output). Height comes from the first
/// `Video:` stream line (`1280x720`); bitrate from the
/// `bitrate: N kb/s` field on the Duration line.
fn parse_ffmpeg_probe(stderr: &str) -> Option<ProbeQuality> {
    let mut height = None;
    let mut bitrate_kbps = None;
    for line in stderr.lines() {
        if height.is_none() {
            if let Some(v) = line.split("Video:").nth(1) {
                // "... h264, yuv420p, 1280x720 [SAR ...], 25 fps ..."
                for token in v.split(|c: char| c == ' ' || c == ',' || c == '[') {
                    let token = token.trim_end_matches(']');
                    if let Some((w, h)) = token.split_once('x') {
                        if let (Ok(_), Ok(hh)) = (w.parse::<u32>(), h.parse::<u32>()) {
                            if hh > 0 {
                                height = Some(hh);
                                break;
                            }
                        }
                    }
                }
            }
        }
        if bitrate_kbps.is_none() {
            if let Some(b) = line.split("bitrate:").nth(1) {
                // " 865 kb/s" — the first number wins; "N/A" parses
                // to nothing and stays None.
                let num: String = b
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if let Ok(v) = num.parse::<f64>() {
                    if v > 0.0 {
                        bitrate_kbps = Some(v as u32);
                    }
                }
            }
        }
    }
    if height.is_none() && bitrate_kbps.is_none() {
        return None;
    }
    Some(ProbeQuality {
        height,
        bitrate_kbps,
    })
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
    fn parse_ffmpeg_probe_reads_height_and_bitrate() {
        // Round 10, item 8: canned `ffmpeg -i` stderr in the
        // Soap2Day shape (720p, 865 kb/s) plus the no-video and
        // garbage cases. No ffmpeg binary needed.
        let stderr = "ffmpeg version 9.1-full_build-www.gyan.dev Copyright (c) 2000-2025\n\
             Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'Scream 7.mp4':\n\
             \x20 Duration: 01:49:23.04, start: 0.000000, bitrate: 865 kb/s\n\
             \x20 Stream #0:0(und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709), 1280x720 [SAR 1:1 DAR 16:9], 25 fps, 25 tbr, 90k tbn (default)\n\
             \x20 Stream #0:1(und): Audio: aac (LC) (mp4a / 0x6134706D), 48000 Hz, stereo, fltp, 128 kb/s (default)\n";
        assert_eq!(
            parse_ffmpeg_probe(stderr),
            Some(ProbeQuality {
                height: Some(720),
                bitrate_kbps: Some(865),
            })
        );
        // Audio-only file: no Video line, bitrate still parsed.
        let audio_only = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'song.m4a':\n\
             \x20 Duration: 00:03:12.00, start: 0.000000, bitrate: 129 kb/s\n\
             \x20 Stream #0:0(und): Audio: aac (LC), 44100 Hz, stereo, fltp, 128 kb/s\n";
        assert_eq!(
            parse_ffmpeg_probe(audio_only),
            Some(ProbeQuality {
                height: None,
                bitrate_kbps: Some(129),
            })
        );
        // Garbage / empty: None, never a panic.
        assert_eq!(parse_ffmpeg_probe(""), None);
        assert_eq!(parse_ffmpeg_probe("not a video at all\n"), None);
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
    fn ensure_ffmpeg_serializes_concurrent_first_use() {
        // Round 12, item 8: eight threads calling ensure_ffmpeg
        // at once must all return the same binary, never panic or
        // deadlock on the fetch lock. This exercises the lock
        // itself; the real first-use download race is
        // Windows-only and cannot be exercised on this VM, so the
        // download path is verified by inspection + the full
        // suite, not by this test.
        let dir = unique_temp_dir("ff-lock");
        std::fs::create_dir_all(dir.join("tools").join("ffmpeg")).unwrap();
        let tools_ff = dir.join("tools").join("ffmpeg").join(ffmpeg_exe_name());
        std::fs::write(&tools_ff, b"fake").unwrap();
        let dir = std::sync::Arc::new(dir);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let d = dir.clone();
            handles.push(std::thread::spawn(move || ensure_ffmpeg(&d)));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.join().expect("ensure_ffmpeg thread panicked"));
        }
        let first = results[0].as_ref().unwrap();
        for r in &results {
            assert_eq!(r.as_ref().unwrap(), first, "concurrent calls disagreed");
        }
        let _ = std::fs::remove_dir_all(&*dir);
    }

    #[test]
    fn ffmpeg_resolution_env_override_and_tools_folder() {
        let dir = unique_temp_dir("ff-resolve");
        std::fs::create_dir_all(dir.join("tools").join("ffmpeg")).unwrap();
        let tools_ff = dir.join("tools").join("ffmpeg").join(ffmpeg_exe_name());
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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            video.to_str().unwrap()
        ]));
        assert!(mk(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:d=1",
            "-c:a",
            "aac",
            audio.to_str().unwrap()
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

    #[test]
    fn extract_audio_copies_aac_and_reencodes_only_when_copy_fails() {
        // Needs a real ffmpeg; skip (do not fail) where none exists,
        // so the suite stays green on machines without ffmpeg.
        let ffmpeg = match find_on_path(ffmpeg_exe_name()) {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("extract-audio");
        std::fs::create_dir_all(&dir).unwrap();
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
        // Case 1: AAC in MP4 — stream copy, no re-encode. The
        // output must be audio-only (no video stream) and decode.
        let muxed = dir.join("muxed.mp4");
        assert!(mk(&[
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
            muxed.to_str().unwrap()
        ]));
        let out = dir.join("audio.m4a");
        extract_audio(&ffmpeg, &muxed, &out).unwrap();
        assert!(out.is_file());
        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        let probe = Command::new(&ffmpeg)
            .arg("-i")
            .arg(&out)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&probe.stderr);
        assert!(info.contains("Audio: aac"), "probe: {info}");
        assert!(
            !info.contains("Video:"),
            "output must be audio-only: {info}"
        );
        // Case 2: PCM in MKV cannot be copied into .m4a (the mp4
        // muxer has no tag for pcm_s16le), so the single AAC
        // re-encode fallback must produce a working .m4a.
        let pcm = dir.join("pcm.mkv");
        assert!(mk(&[
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
            "pcm_s16le",
            pcm.to_str().unwrap()
        ]));
        let out2 = dir.join("audio2.m4a");
        extract_audio(&ffmpeg, &pcm, &out2).unwrap();
        assert!(out2.is_file());
        assert!(std::fs::metadata(&out2).unwrap().len() > 0);
        // Case 3: garbage in is a clear error with no partial output.
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"not a video").unwrap();
        let out3 = dir.join("audio3.m4a");
        assert!(extract_audio(&ffmpeg, &bad, &out3).is_err());
        assert!(!out3.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_audio_as_reencodes_to_mp3_and_opus() {
        // Round 11 extension, feature 13: an mp3 target gets a
        // real mp3 file (libmp3lame re-encode when the AAC source
        // cannot be copied into mp3), an opus target a real opus
        // file. Needs a real ffmpeg; skipped otherwise.
        let ffmpeg = match find_on_path(ffmpeg_exe_name()) {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("extract-as");
        std::fs::create_dir_all(&dir).unwrap();
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
        let muxed = dir.join("muxed.mp4");
        assert!(mk(&[
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
            muxed.to_str().unwrap()
        ]));
        let probe = |path: &Path| {
            let out = Command::new(&ffmpeg)
                .arg("-i")
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr).to_string()
        };
        let mp3 = dir.join("audio.mp3");
        extract_audio_as(&ffmpeg, &muxed, &mp3, "mp3").unwrap();
        assert!(mp3.is_file());
        let info = probe(&mp3);
        assert!(info.contains("Audio: mp3"), "mp3 probe: {info}");
        assert!(!info.contains("Video:"), "mp3 must be audio-only: {info}");
        let opus = dir.join("audio.opus");
        extract_audio_as(&ffmpeg, &muxed, &opus, "opus").unwrap();
        assert!(opus.is_file());
        let info = probe(&opus);
        assert!(info.contains("Audio: opus"), "opus probe: {info}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_tracks_muxes_subtitles_and_title_metadata() {
        // Round 11 extension, features 2 + 12: a muxed video +
        // a WebVTT file merge into an mp4 carrying a mov_text
        // subtitle stream tagged with its language, plus the
        // title metadata. Needs a real ffmpeg; skipped otherwise.
        let ffmpeg = match find_on_path(ffmpeg_exe_name()) {
            Some(p) => p,
            None => return,
        };
        let dir = unique_temp_dir("merge-subs");
        std::fs::create_dir_all(&dir).unwrap();
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
        let muxed = dir.join("muxed.mp4");
        assert!(mk(&[
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
            muxed.to_str().unwrap()
        ]));
        let vtt = dir.join("subs.vtt");
        std::fs::write(&vtt, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHello\n").unwrap();
        let out = dir.join("merged.mp4");
        merge_tracks(
            &ffmpeg,
            &muxed,
            None,
            Some((&vtt, "en")),
            Some("Test Title"),
            &out,
        )
        .unwrap();
        assert!(out.is_file());
        let probe = Command::new(&ffmpeg)
            .arg("-i")
            .arg(&out)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let info = String::from_utf8_lossy(&probe.stderr);
        assert!(info.contains("Video:"), "video kept: {info}");
        assert!(info.contains("Audio:"), "muxed audio kept: {info}");
        assert!(
            info.contains("Subtitle: mov_text"),
            "subtitle muxed as mov_text: {info}"
        );
        assert!(
            info.contains("title") && info.contains("Test Title"),
            "title metadata written: {info}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
