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
pub mod filename;
pub mod id;
pub mod media;
pub mod pairing;
pub mod state;
pub mod token;
pub mod validate;

pub use category::category_for_filename;
pub use download::{download, probe, stop_reason, DownloadOptions, Probe, Progress, StopReason};
pub use filename::{sanitize_filename, unique_path};
pub use id::generate_download_id;
pub use media::{download_dash, download_hls};
pub use state::{DownloadRecord, Status, Store};
pub use token::generate_secure_token;
pub use validate::validate_url;
