//! 3D asset downloading (spec §6): GLB, GLTF, OBJ, FBX, STL, USDZ.
//!
//! Single-file formats (GLB, OBJ, FBX, STL, USDZ) download through
//! the existing direct path with category "3D" — they need no special
//! handling. A `.gltf` file is different: it is JSON that references
//! dependencies (buffer `.bin` files, images/textures) by relative
//! URI, and the model only works when those files sit next to it in
//! the same folder structure. So a `.gltf` URL is planned at add time
//! (`fetch_gltf_plan`: fetch the JSON, resolve and probe every
//! dependency) and downloaded by the worker (`download_gltf_bundle`)
//! into one bundle folder that mirrors the URL path structure, so
//! every relative reference in the file still resolves.
//!
//! Protected assets are refused with a plain explanation: a
//! dependency the server will not serve (401/403/404) fails the whole
//! plan up front instead of producing a half-downloaded, broken
//! model. No DRM, auth, paywall, or encryption bypass is attempted:
//! only URLs the file itself references, fetched with a plain GET
//! and no credentials, are downloaded.

use crate::download::{download, probe, stop_reason, DownloadOptions, Progress};
use crate::filename::{filename_from_url, sanitize_filename};
use crate::validate::validate_url;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 3D model extensions from the spec (§1, §6).
pub const MODEL_EXTENSIONS: [&str; 6] = ["glb", "gltf", "obj", "fbx", "stl", "usdz"];

/// True when the URL points at a 3D model file by extension.
pub fn is_model_url(url: &str) -> bool {
    crate::image::url_path_extension(url)
        .is_some_and(|ext| MODEL_EXTENSIONS.contains(&ext.as_str()))
}

/// True when the URL points at a `.gltf` JSON model (the only format
/// with downloadable dependencies).
pub fn is_gltf_url(url: &str) -> bool {
    crate::image::url_path_extension(url).is_some_and(|ext| ext == "gltf")
}

/// One dependency of a `.gltf` file.
#[derive(Debug, Clone)]
pub struct GltfDependency {
    /// Absolute URL of the dependency, resolved against the model URL.
    pub url: String,
    /// Path inside the bundle folder, forward slashes (e.g.
    /// `models/textures/diffuse.png`). Mirrors the URL path so
    /// relative references in the model keep resolving.
    pub relative_path: String,
    /// Size from the probe, when the server reports one.
    pub size: Option<u64>,
}

/// A planned `.gltf` download: the model plus every dependency that
/// must travel with it.
#[derive(Debug, Clone)]
pub struct GltfPlan {
    /// Final model URL (after redirects).
    pub main_url: String,
    /// File name for the model itself (from its URL).
    pub filename: String,
    /// Path of the model inside the bundle folder (from its URL path).
    pub main_relative_path: String,
    /// Size of the model file itself, when the server reports one.
    pub main_size: Option<u64>,
    pub dependencies: Vec<GltfDependency>,
    /// Sum of the known sizes (model + dependencies), when any are
    /// known. `None` when no size is known at all.
    pub total_bytes: Option<u64>,
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent("FetchForge/0.2")
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn http_error(code: u16) -> String {
    match code {
        400 => "Server rejected the request (400)".to_string(),
        401 | 403 => {
            "Access denied by the server (401/403). The model may need sign-in or may be protected.".to_string()
        }
        404 => "File not found on the server (404)".to_string(),
        429 => "Server rate-limited the request (429). Retry later.".to_string(),
        500..=599 => format!("Server error ({code}). Retry later."),
        _ => format!("HTTP error {code}"),
    }
}

/// The dependency URIs a `.gltf` JSON references: every `buffers[].uri`
/// and `images[].uri`. Entries without a `uri` are embedded in the file
/// (buffer views) and need no download.
pub fn gltf_dependency_uris(json: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| "not a valid glTF model file (the JSON could not be read)".to_string())?;
    let mut out = Vec::new();
    for key in ["buffers", "images"] {
        if let Some(items) = value.get(key).and_then(|v| v.as_array()) {
            for item in items {
                if let Some(uri) = item.get("uri").and_then(|u| u.as_str()) {
                    out.push(uri.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Percent-decode one URL path segment (`%20` -> space). Invalid
/// sequences are left as-is rather than failing the whole model.
fn decode_segment(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&seg[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The bundle path for a URL: its URL path with the leading `/`
/// stripped, each segment decoded and sanitised, joined with `/`.
/// A dependency on another host cannot live under the model's folder,
/// so it goes under `_external/<host>/...` (its reference in the
/// model is absolute, so the local copy's position does not matter).
/// Any segment that would escape the bundle folder (`.`, `..`, empty
/// after sanitising) is refused — a malicious model file must never
/// make the downloader write outside the bundle folder.
fn bundle_relative_path(base: &url::Url, dep: &url::Url) -> Result<String, String> {
    let same_origin = dep.scheme() == base.scheme()
        && dep.host_str() == base.host_str()
        && dep.port() == base.port();
    let segments: Vec<String> = if same_origin {
        dep.path_segments()
            .map(|it| it.map(decode_segment).collect())
            .unwrap_or_default()
    } else {
        let mut segs = vec!["_external".to_string()];
        if let Some(host) = dep.host_str() {
            segs.push(sanitize_filename(host));
        }
        segs.extend(
            dep.path_segments()
                .map(|it| it.map(decode_segment).collect::<Vec<String>>())
                .unwrap_or_default(),
        );
        segs
    };
    let mut clean: Vec<String> = Vec::new();
    for seg in segments {
        // Check the raw segment first: sanitize_filename turns ".."
        // into "download" (dots stripped), so a post-sanitise check
        // would never see the escape.
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!(
                "3D model dependency {dep} has an unsafe path segment ({seg}); the model was not downloaded."
            ));
        }
        let s = sanitize_filename(&seg);
        if s.is_empty() {
            return Err(format!(
                "3D model dependency {dep} has an unusable path segment ({seg}); the model was not downloaded."
            ));
        }
        clean.push(s);
    }
    if clean.is_empty() {
        return Err(format!(
            "3D model dependency {dep} has no usable file name; the model was not downloaded."
        ));
    }
    Ok(clean.join("/"))
}

/// Fetch a `.gltf` model, resolve and probe every dependency it
/// references, and return the download plan. Any dependency the
/// server will not serve fails the plan with a plain explanation, so
/// the user never gets a half-downloaded model that looks complete.
pub fn fetch_gltf_plan(url: &str) -> Result<GltfPlan, String> {
    let clean = validate_url(url)?;
    let client = http_client()?;
    let resp = client
        .get(&clean)
        .send()
        .map_err(|e| format!("Could not fetch the 3D model: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!(
            "Could not fetch the 3D model: {}",
            http_error(status)
        ));
    }
    if resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim().to_ascii_lowercase().starts_with("text/html"))
    {
        return Err(
            "That link is a web page, not a glTF model file. Open the model's download link instead."
                .to_string(),
        );
    }
    let main_size = resp.content_length();
    let main_url = resp.url().to_string();
    let text = resp
        .text()
        .map_err(|e| format!("Could not read the 3D model file: {e}"))?;
    let base = url::Url::parse(&main_url).map_err(|_| "Invalid model URL".to_string())?;
    let filename = filename_from_url(&main_url)
        .map(|f| sanitize_filename(&f))
        .unwrap_or_else(|| "model.gltf".to_string());
    let main_relative_path = bundle_relative_path(&base, &base)?;
    let mut dependencies: Vec<GltfDependency> = Vec::new();
    let mut seen: Vec<String> = vec![main_url.clone()];
    let mut total: u64 = main_size.unwrap_or(0);
    let mut any_size = main_size.is_some();
    for uri in gltf_dependency_uris(&text)? {
        if uri.is_empty() {
            continue;
        }
        // Embedded data (data: URIs) travels inside the .gltf file
        // itself, so there is nothing to download.
        if uri.starts_with("data:") {
            continue;
        }
        // A blob: URI only exists inside a browser page; a model file
        // on a server cannot reference one, so this is a broken file.
        if uri.starts_with("blob:") {
            return Err(format!(
                "3D model dependency {uri} is a browser-only (blob:) link that cannot be downloaded; the model was not downloaded."
            ));
        }
        let dep_url = base
            .join(&uri)
            .map_err(|_| format!("3D model dependency {uri} is not a valid URL; the model was not downloaded."))?
            .to_string();
        if seen.contains(&dep_url) {
            continue;
        }
        seen.push(dep_url.clone());
        let dep_parsed =
            url::Url::parse(&dep_url).map_err(|_| "Invalid dependency URL".to_string())?;
        let relative_path = bundle_relative_path(&base, &dep_parsed)?;
        // Probe first: a missing or protected dependency fails the
        // plan now, with its name in the error, instead of failing
        // halfway through the download.
        let probe_opts = DownloadOptions {
            url: dep_url.clone(),
            ..Default::default()
        };
        let size = match probe(&probe_opts) {
            Ok(p) => p.content_length,
            Err(e) => {
                return Err(format!(
                    "3D model dependency {dep_url} could not be reached ({e}); the model was not downloaded."
                ));
            }
        };
        if let Some(s) = size {
            total += s;
            any_size = true;
        }
        dependencies.push(GltfDependency {
            url: dep_url,
            relative_path,
            size,
        });
    }
    Ok(GltfPlan {
        main_url,
        filename,
        main_relative_path,
        main_size,
        dependencies,
        total_bytes: if any_size { Some(total) } else { None },
    })
}

/// Options for a `.gltf` bundle download (mirrors `DownloadOptions`
/// minus the single-file fields).
#[derive(Debug, Clone)]
pub struct GltfDownloadOptions {
    pub destination_dir: PathBuf,
    pub max_segments: usize,
    pub max_retries: u32,
    pub bandwidth_kbps: Option<u64>,
}

/// Download a planned `.gltf` bundle: the model plus every
/// dependency, into `<destination_dir>/3D/<stem>-<hash>/`, preserving
/// the URL path structure so the model's relative references resolve.
/// The folder name carries the first 8 hex digits of the SHA-256 of
/// the model URL, so two different models with the same file name
/// never share (and corrupt) one bundle folder.
///
/// `main_filename` is the record's file name for the model (the
/// caller's explicit name wins over the URL's name); it is placed in
/// the same folder the URL's name would occupy, so references are
/// unaffected by the rename.
///
/// Files already present at their target with the expected size are
/// skipped, so retrying an interrupted bundle does not re-download
/// finished files. A present file with the wrong size is removed and
/// downloaded fresh (it can only be a partial file from a previous
/// attempt of this same model, inside this model's own folder).
/// Pause/cancel sentinels from the single-file downloader pass
/// through unchanged, so the worker recognises them.
pub fn download_gltf_bundle(
    plan: &GltfPlan,
    main_filename: &str,
    opts: &GltfDownloadOptions,
    progress: &(dyn Fn(Progress) + Send + Sync),
    paused: &AtomicBool,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let stem = Path::new(main_filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "model".to_string());
    let hash = format!("{:x}", Sha256::digest(plan.main_url.as_bytes()));
    let bundle_dir = opts
        .destination_dir
        .join("3D")
        .join(format!("{}-{}", sanitize_filename(&stem), &hash[..8]));
    std::fs::create_dir_all(&bundle_dir)
        .map_err(|e| format!("Cannot create 3D model folder: {e}"))?;

    // The model itself: same folder as the URL's name, but under the
    // record's file name.
    let main_parent = Path::new(&plan.main_relative_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let main_target = bundle_dir
        .join(&main_parent)
        .join(sanitize_filename(main_filename));
    let mut files: Vec<(String, PathBuf, Option<u64>)> = vec![(
        plan.main_url.clone(),
        main_target.clone(),
        plan.main_size,
    )];
    for dep in &plan.dependencies {
        // Defence in depth: the plan already validated these paths,
        // but a path must never escape the bundle folder.
        let rel = Path::new(&dep.relative_path);
        if rel.is_absolute()
            || rel.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "3D model dependency {} has an unsafe path; the model was not downloaded.",
                dep.url
            ));
        }
        files.push((
            dep.url.clone(),
            bundle_dir.join(&dep.relative_path),
            dep.size,
        ));
    }

    let completed = Arc::new(AtomicU64::new(0));
    let mut base: u64 = 0;
    for (url, target, expected_size) in &files {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create 3D model folder: {e}"))?;
        }
        let file_name = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        // Skip a file that is already complete at its target; remove
        // a wrong-sized one so the downloader writes the real file
        // instead of renaming to "file (1)".
        if let Ok(meta) = std::fs::metadata(target) {
            match expected_size {
                Some(size) if meta.len() == *size => {
                    base += *size;
                    completed.store(base, Ordering::Relaxed);
                    progress(Progress {
                        downloaded: base,
                        total: plan.total_bytes,
                        bytes_per_sec: 0.0,
                        segments_done: None,
                        segments_total: None,
                        estimated_total: None,
                    });
                    continue;
                }
                _ => {
                    let _ = std::fs::remove_file(target);
                }
            }
        }
        let dir = target
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| bundle_dir.clone());
        let file_opts = DownloadOptions {
            url: url.clone(),
            destination_dir: dir,
            filename: Some(file_name),
            max_segments: opts.max_segments,
            max_retries: opts.max_retries,
            bandwidth_kbps: opts.bandwidth_kbps,
            referer: None,
            user_agent: None,
            organise_by_category: false,
        };
        let base_before = base;
        let completed_clone = Arc::clone(&completed);
        let total = plan.total_bytes;
        let file_progress = |p: Progress| {
            completed_clone.store(base_before + p.downloaded, Ordering::Relaxed);
            progress(Progress {
                downloaded: base_before + p.downloaded,
                total,
                bytes_per_sec: p.bytes_per_sec,
                segments_done: None,
                segments_total: None,
                estimated_total: None,
            });
        };
        let got = download(&file_opts, &file_progress, paused, cancelled).map_err(|e| {
            if stop_reason(&e).is_some() {
                e
            } else {
                format!("Failed to download 3D model file {url}: {e}")
            }
        })?;
        // `download` returns the real final path; the skip check above
        // means it is the target (no unique-rename happened).
        let size = std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0);
        base += size;
        completed.store(base, Ordering::Relaxed);
    }
    progress(Progress {
        downloaded: base,
        total: plan.total_bytes,
        bytes_per_sec: 0.0,
        segments_done: None,
        segments_total: None,
        estimated_total: None,
    });
    Ok(main_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_url_by_extension() {
        assert!(is_model_url("https://example.com/robot.GLB"));
        assert!(is_model_url("https://example.com/scene.gltf?x=1"));
        assert!(is_model_url("https://example.com/part.stl#frag"));
        assert!(is_model_url("https://example.com/a.obj"));
        assert!(is_model_url("https://example.com/a.fbx"));
        assert!(is_model_url("https://example.com/a.usdz"));
        assert!(!is_model_url("https://example.com/photo.png"));
        assert!(!is_model_url("https://example.com/model"));
        assert!(is_gltf_url("https://example.com/scene.gltf"));
        assert!(!is_gltf_url("https://example.com/scene.glb"));
    }

    #[test]
    fn gltf_dependency_uris_from_buffers_and_images() {
        let json = r#"{
            "buffers": [{"uri": "model.bin", "byteLength": 12}],
            "images": [
                {"uri": "textures/diffuse.png"},
                {"uri": "data:image/png;base64,AAAA"},
                {"mimeType": "image/png", "bufferView": 0}
            ]
        }"#;
        let uris = gltf_dependency_uris(json).unwrap();
        // data: URIs and bufferView-only images are listed here (the
        // plan step is what skips data:); the extractor itself is dumb.
        assert_eq!(
            uris,
            vec![
                "model.bin",
                "textures/diffuse.png",
                "data:image/png;base64,AAAA",
            ]
        );
        assert!(gltf_dependency_uris("not json").is_err());
        assert_eq!(
            gltf_dependency_uris("{}").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn bundle_paths_mirror_url_structure() {
        let base = url::Url::parse("https://example.com/models/robot/model.gltf").unwrap();
        // Same folder.
        let same = url::Url::parse("https://example.com/models/robot/model.bin").unwrap();
        assert_eq!(
            bundle_relative_path(&base, &same).unwrap(),
            "models/robot/model.bin"
        );
        // Subfolder.
        let sub = url::Url::parse("https://example.com/models/robot/textures/d.png").unwrap();
        assert_eq!(
            bundle_relative_path(&base, &sub).unwrap(),
            "models/robot/textures/d.png"
        );
        // Parent folder: the URL path keeps the reference correct.
        let parent = url::Url::parse("https://example.com/models/shared/tex.png").unwrap();
        assert_eq!(
            bundle_relative_path(&base, &parent).unwrap(),
            "models/shared/tex.png"
        );
        // Another host: namespaced under _external, never mixed in.
        let other = url::Url::parse("https://cdn.example.org/tex.png").unwrap();
        assert_eq!(
            bundle_relative_path(&base, &other).unwrap(),
            "_external/cdn.example.org/tex.png"
        );
        // Percent-encoded names are decoded.
        let enc = url::Url::parse("https://example.com/models/robot/my%20tex.png").unwrap();
        assert_eq!(
            bundle_relative_path(&base, &enc).unwrap(),
            "models/robot/my tex.png"
        );
    }

    #[test]
    fn bundle_paths_reject_escapes() {
        let base = url::Url::parse("https://example.com/models/model.gltf").unwrap();
        // A literal ".." segment can only arrive if the URL parser did
        // not normalise it; the guard must still refuse it.
        let evil = url::Url::parse("https://example.com/models/%2e%2e/secret.txt").unwrap();
        // url crate normalises %2e%2e away, so this resolves inside;
        // assert the function never returns a path with ".." in it.
        let rel = bundle_relative_path(&base, &evil).unwrap();
        assert!(!rel.split('/').any(|s| s == ".."), "{rel}");
        // An empty path (the model URL itself) has no file name.
        let empty = url::Url::parse("https://example.com/").unwrap();
        assert!(bundle_relative_path(&base, &empty).is_err());
    }
}
