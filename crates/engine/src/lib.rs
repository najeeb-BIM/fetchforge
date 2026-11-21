//! FetchForge download engine.
//!
//! Pure download logic with no UI dependency, so the same crate backs
//! the Tauri desktop app on Windows and the localhost bridge.
//! Security invariants (see docs/SECURITY.md):
//! * only `http` / `https` URLs are accepted, never `file:`, `javascript:`, etc.
//! * filenames are sanitised for Windows before any filesystem use.
//! * downloads write to `<name>.part` and are atomically renamed on success.
//! * downloaded files are never executed.

pub mod category;
pub mod download;
pub mod extract;
pub mod ffmpeg;
pub mod filename;
pub mod id;
pub mod image;
pub mod media;
pub mod pairing;
pub mod state;
pub mod threed;
pub mod token;
pub mod validate;

pub use category::category_for_filename;
pub use download::{download, probe, stop_reason, DownloadOptions, Probe, Progress, StopReason};
pub use extract::{
    download_media, drive_direct_url, drive_file_id, ensure_ytdlp, find_ytdlp, format_selector,
    list_formats, list_formats_with, parse_media_info, plan_media_download, FormatKind,
    MediaDownloadOptions, MediaFormat, MediaInfo, MediaPlan,
};
pub use ffmpeg::{ensure_ffmpeg, find_ffmpeg, merge_av};
pub use image::{
    ensure_image_extension, image_ext_for_content_type, is_image_content_type, is_image_url,
    parse_srcset, pick_best_srcset_url, pick_srcset_url, url_path_extension, SrcsetCandidate,
};
pub use filename::{sanitize_filename, unique_path};
pub use id::generate_download_id;
pub use media::{
    download_dash, download_dash_with, download_hls, download_hls_variant, is_manifest_url,
    list_stream_formats, stream_kind_of_content_type, stream_kind_of_url, DashDownloadOptions,
    StreamKind,
};
pub use threed::{
    download_gltf_bundle, fetch_gltf_plan, gltf_dependency_uris, is_gltf_url, is_model_url,
    GltfDependency, GltfDownloadOptions, GltfPlan,
};
pub use state::{DownloadRecord, Status, Store};
pub use token::generate_secure_token;
pub use validate::validate_url;
