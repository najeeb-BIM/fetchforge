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
pub use download::{
    download, probe, stop_reason, DownloadOptions, Probe, Progress, SharedLimiter, StopReason,
};
pub use extract::{
    download_media, drive_direct_url, drive_file_id, ensure_ytdlp, find_ytdlp, format_selector,
    format_selector_for_language, format_selector_multi, list_formats, list_formats_with,
    list_formats_with_proxy, list_formats_with_referer, list_formats_with_referer_and_proxy,
    list_playlist, list_playlist_with_proxy, next_lower_format, parse_media_info,
    plan_media_download, plan_media_download_or_drive_fallback,
    plan_media_download_or_drive_fallback_with_options, plan_media_download_with_options,
    remove_partial_files, FormatKind, MediaDownloadOptions, MediaFormat, MediaInfo, MediaPlan,
    PlanOutcome, PlaylistEntry, PlaylistInfo,
};
pub use ffmpeg::{
    ensure_ffmpeg, extract_audio, extract_audio_as, find_ffmpeg, merge_av, merge_tracks,
    probe_video_quality, ProbeQuality,
};
pub use filename::{sanitize_filename, unique_path};
pub use id::generate_download_id;
pub use image::{
    ensure_image_extension, image_ext_for_content_type, is_image_content_type, is_image_url,
    parse_srcset, pick_best_srcset_url, pick_srcset_url, url_path_extension, SrcsetCandidate,
};
pub use media::{
    classify_stream_prefix, download_dash, download_dash_with, download_hls,
    download_hls_audio_only, download_hls_audio_only_opts, download_hls_pair,
    download_hls_pair_opts, download_hls_variant, download_hls_variant_opts, estimate_total_bytes,
    is_manifest_url, list_dash_formats_with_proxy, list_hls_formats_with_proxy,
    list_stream_formats, list_stream_formats_kind, list_stream_formats_kind_with_proxy,
    list_stream_formats_with_proxy, sniff_stream_kind, sniff_stream_kind_with_proxy,
    stream_kind_of_content_type, stream_kind_of_url, CapturedManifest, DashDownloadOptions,
    SizeKind, StreamDownloadOptions, StreamKind, MAX_CAPTURED_MANIFEST_BYTES,
};
pub use state::{delete_download_files, DownloadRecord, Status, Store};
pub use threed::{
    download_gltf_bundle, fetch_gltf_plan, fetch_gltf_plan_with_proxy, gltf_dependency_uris,
    is_gltf_url, is_model_url, GltfDependency, GltfDownloadOptions, GltfPlan,
};
pub use token::generate_secure_token;
pub use validate::{normalize_url_for_dedupe, validate_proxy_url, validate_url};
