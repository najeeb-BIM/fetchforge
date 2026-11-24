use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Persistent download record. Stored as JSON, one file per user profile,
/// so state survives app restarts and crashes (a `.part` file + this record
/// is enough to resume).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    /// Terminal statuses: the download will never run again, so the
    /// record is history only and safe to remove from the list.
    /// Removing a record never deletes the downloaded file.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub destination: String,
    pub category: String,
    pub status: Status,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub resumable: bool,
    pub error: Option<String>,
    pub created_unix: u64,
    pub updated_unix: u64,
    /// Media downloads only: the yt-dlp format id chosen for this
    /// download (user-picked in the format picker, or the recorded
    /// auto-picked default for extension-sent URLs). None means a
    /// direct file download. `#[serde(default)]` so history files
    /// written before v0.2.0 still load.
    #[serde(default)]
    pub format_id: Option<String>,
    /// Media downloads only: true when the chosen format is video-only
    /// and the download merges it with the best audio stream (needs
    /// ffmpeg). Stored so a resume does not have to re-extract the page.
    #[serde(default)]
    pub needs_merge: bool,
    /// Media downloads only: the full yt-dlp `-f` selector computed
    /// at plan time (e.g. `160+bestaudio[ext=m4a]/160+bestaudio/160`
    /// — the audio is chosen to match the video's container, so a
    /// merge stays mp4 instead of falling back to mkv). Stored so a
    /// resume re-runs the exact same selector. None for direct
    /// downloads, and for media records written before this field
    /// existed (the worker then rebuilds a plain selector).
    #[serde(default)]
    pub format_selector: Option<String>,
    /// HLS/DASH downloads only: `"hls"` or `"dash"`. The worker
    /// dispatches on this: a set value means the download runs
    /// through media.rs (manifest parsing + segment assembly),
    /// while None with a `format_id` means a yt-dlp download.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub stream_kind: Option<String>,
    /// The page the download came from (sent by the extension when
    /// the user clicks Download on a page). Sent as the Referer
    /// header on segment, probe and yt-dlp requests: some CDNs and
    /// proxies refuse requests without the Referer they expect.
    /// None for pasted links (no page context).
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub referer: Option<String>,
    /// HLS paired-variant downloads only: the sibling audio variant
    /// playlist URL (e.g. `audio_1.m3u8`) the extension observed
    /// next to the video variant (`v0.m3u8`). When set, the worker
    /// downloads both tracks and merges them with ffmpeg instead of
    /// delivering a silent video. None for every other download.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub audio_url: Option<String>,
    /// Transient: true when this record was returned because an
    /// identical download already exists (idempotent add), NOT
    /// because a new download was queued. Set only on the clone
    /// returned to the caller by the add functions — never on the
    /// stored record, so it is never persisted to downloads.json.
    /// `skip_serializing_if` keeps it out of the JSON unless set,
    /// so old files, old clients, and the app list are unaffected;
    /// the extension reads it from the add response and reports
    /// "Already downloaded" instead of claiming a new download was
    /// sent (2026-11-22: a re-sent clip showed "Sent to
    /// FetchForge" while nothing was queued at all).
    /// `#[serde(default)]` so older history files still load.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate: bool,
    /// HLS/DASH downloads only: how many segments the manifest
    /// lists in total. Recorded at plan time (the plan parses the
    /// manifest), so the UI can show "0 / 3,321 pieces" before the
    /// first segment lands. None for direct/yt-dlp downloads and
    /// for records planned before this field existed.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub segments_total: Option<u64>,
    /// HLS/DASH downloads only: how many segments have completed
    /// so far (updated with every progress report). 0 until the
    /// first segment completes.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub segments_done: u64,
    /// HLS/DASH downloads only: the estimated total size in bytes,
    /// from the measured average segment size so far times the
    /// segment count (see [`crate::media::estimate_total_bytes`]).
    /// An estimate, never exact — the UI shows it with "~". None
    /// until at least one segment has completed.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub estimated_total_bytes: Option<u64>,
    /// True when the user picked "Audio only" in the extension
    /// popup. Direct/HLS/DASH workers then deliver an audio file
    /// instead of the video (extract the audio track with ffmpeg
    /// after the download, or fetch the audio playlist alone for
    /// HLS). For yt-dlp downloads the format choice already
    /// governs, so the flag is ignored there.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub audio_only: bool,
    /// Media downloads only (round 10, item 8): the quality the
    /// user picked, as the site DECLARED it — the variant's height
    /// in pixels and its bandwidth in kbps, recorded at plan time.
    /// Compared against the delivered values at completion: when
    /// the site mislabels its top variant (Soap2Day 2026-11-23:
    /// picked "1080p 3 Mbps", delivered 720p at 865 kb/s), the
    /// record says so instead of claiming the picked quality.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub picked_height: Option<u32>,
    #[serde(default)]
    pub picked_vbr_kbps: Option<u32>,
    /// Media downloads only (round 10, item 8): the quality the
    /// file ACTUALLY is, probed with ffmpeg at completion
    /// (best-effort: None when ffmpeg is unavailable or the probe
    /// fails — the download still completes).
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub delivered_height: Option<u32>,
    #[serde(default)]
    pub delivered_bitrate_kbps: Option<u32>,
    /// Media downloads only (round 10, item 8): a plain-English
    /// note when the delivered stream is clearly worse than the
    /// picked one (e.g. "Delivered 720p at ~865 kb/s; the site
    /// listed this stream as 1080p / 3000 kb/s"). None when the
    /// delivered quality matches the pick, or when either side
    /// is unknown.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub quality_note: Option<String>,
    /// Live download speed in bytes per second, from the most
    /// recent progress report (round 11, item 8). The engine
    /// already computed it in every progress report; the record
    /// just never kept it, so the UI's Speed column showed "-".
    /// None until the first report, and None for records written
    /// before this field existed. The UI shows it only for
    /// Downloading rows — a paused row keeps its last value in
    /// the record, but the UI must not show a stale speed.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub speed_bps: Option<f64>,
    /// How `total_bytes` was determined (round 11, item 10 — the
    /// estimate ladder). Set at plan time from the plan's kind;
    /// Exact for a direct download with a probed Content-Length.
    /// When the download's measured estimate is the only size
    /// known, `apply_progress` sets it to Measured. None when
    /// there is no total at all. The UI shows the marker.
    /// `#[serde(default)]` so older history files still load.
    #[serde(default)]
    pub size_kind: Option<crate::media::SizeKind>,
}

impl DownloadRecord {
    /// Apply one progress report, monotonically (round 10, items
    /// 2/6): progress never moves backwards. A phase change (the
    /// HLS audio phase restarting its segment counter — Plex
    /// 2026-11-23 went 645/820 back to 353/821 while bytes kept
    /// growing) or a re-plan with a smaller total must not lower
    /// a counter the UI already showed. The estimate is the one
    /// exception: it refines as segments land, so the latest
    /// value wins.
    pub fn apply_progress(&mut self, p: &crate::download::Progress) {
        self.downloaded_bytes = self.downloaded_bytes.max(p.downloaded);
        if let Some(t) = p.total {
            self.total_bytes = Some(self.total_bytes.map_or(t, |old| old.max(t)));
        }
        if let Some(d) = p.segments_done {
            self.segments_done = self.segments_done.max(d);
        }
        if let Some(t) = p.segments_total {
            self.segments_total = Some(self.segments_total.map_or(t, |old| old.max(t)));
        }
        if let Some(e) = p.estimated_total {
            self.estimated_total_bytes = Some(e);
        }
        // Round 11, item 8: keep the live speed (bytes per
        // second, averaged over the download so far by the
        // engine). Only positive values — a 0 report at the very
        // start must not overwrite a real speed.
        if p.bytes_per_sec > 0.0 {
            self.speed_bps = Some(p.bytes_per_sec);
        }
        // Round 11, item 10: the measured estimate is the only
        // size known for a stream whose manifest declared no
        // bandwidth and no ranges — label the total Measured so
        // the UI shows it with "~". A plan kind already set
        // (Exact/Average/Peak) is never downgraded. Deliberately
        // NOT promoted to Exact when all segments land: a
        // two-phase download (DASH video then audio) reports
        // done == total at the end of phase 1, and promoting
        // then would show 100% with the video phase's total
        // alone. The exact total arrives at completion, when
        // complete_with_file reconciles against the real file.
        if p.estimated_total.is_some() && self.size_kind.is_none() {
            self.size_kind = Some(crate::media::SizeKind::Measured);
        }
        self.updated_unix = now_secs();
    }

    /// Mark the record Completed for the finished file (round 10,
    /// item 2): the file on disk is the truth about its size. A
    /// plan total can be badly wrong (the novahd yt-dlp record
    /// claimed 121 MB for a 3.37 GB file, because the video
    /// format had no filesize and the total was the audio track
    /// alone), so the completed record reconciles against the
    /// real file size instead of trusting the plan. When the file
    /// cannot be statted, the old behaviour stands (downloaded =
    /// planned total, or the last reported bytes).
    pub fn complete_with_file(&mut self, final_path: &std::path::Path) {
        self.status = Status::Completed;
        self.destination = final_path.to_string_lossy().to_string();
        if let Some(name) = final_path.file_name() {
            self.filename = name.to_string_lossy().to_string();
        }
        match std::fs::metadata(final_path) {
            Ok(meta) if meta.is_file() => {
                let size = meta.len();
                self.downloaded_bytes = size;
                self.total_bytes = Some(size);
                // Round 11, item 10: the file on disk is the
                // exact total — the ladder kind is settled now.
                self.size_kind = Some(crate::media::SizeKind::Exact);
            }
            _ => {
                self.downloaded_bytes = self.total_bytes.unwrap_or(self.downloaded_bytes);
            }
        }
        self.error = None;
        self.updated_unix = now_secs();
    }

    /// Record the delivered stream's probed quality and, when it
    /// is clearly worse than the picked quality, note the
    /// mismatch (round 10, item 8). A height below the picked
    /// height, or a bitrate under 60% of the picked bandwidth,
    /// counts as worse. Either side unknown means no note — an
    /// honest "unknown" beats a guessed accusation.
    pub fn apply_delivered_quality(&mut self, height: Option<u32>, bitrate_kbps: Option<u32>) {
        if let Some(h) = height {
            self.delivered_height = Some(h);
        }
        if let Some(b) = bitrate_kbps {
            self.delivered_bitrate_kbps = Some(b);
        }
        let height_worse = matches!(
            (self.picked_height, self.delivered_height),
            (Some(p), Some(d)) if d < p
        );
        let bitrate_worse = matches!(
            (self.picked_vbr_kbps, self.delivered_bitrate_kbps),
            (Some(p), Some(d)) if (d as u64) * 100 < (p as u64) * 60
        );
        if !height_worse && !bitrate_worse {
            return;
        }
        let mut delivered = String::new();
        if let Some(h) = self.delivered_height {
            delivered.push_str(&format!("{h}p"));
        }
        if let Some(b) = self.delivered_bitrate_kbps {
            if !delivered.is_empty() {
                delivered.push_str(" at ");
            }
            delivered.push_str(&format!("~{b} kb/s"));
        }
        let mut picked = String::new();
        if let Some(h) = self.picked_height {
            picked.push_str(&format!("{h}p"));
        }
        if let Some(b) = self.picked_vbr_kbps {
            if !picked.is_empty() {
                picked.push_str(" / ");
            }
            picked.push_str(&format!("{b} kb/s"));
        }
        if delivered.is_empty() || picked.is_empty() {
            return;
        }
        self.quality_note = Some(format!(
            "Delivered {delivered}; the site listed this stream as {picked}."
        ));
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

pub struct Store {
    path: PathBuf,
    records: BTreeMap<String, DownloadRecord>,
    /// F12: set when `load` found a corrupt state file. Callers should
    /// surface this to the user (bridge prints it, desktop app logs it).
    /// It is never part of the saved JSON.
    pub load_warning: Option<String>,
    /// F03: IDs removed in this process, with deletion time. `save`
    /// must not resurrect them from disk. In-memory only: a restart
    /// drops the tombstones, but the records stay deleted because the
    /// save already removed them from the file, and the other process
    /// prunes them from its own memory on reload (see
    /// `reload_from_disk`).
    deleted: BTreeMap<String, u64>,
}

/// F03: lock-file guard. The lock file is created atomically
/// (`create_new`), held only for one save, and removed on drop, so a
/// crash cannot leave a permanent lock (stale locks older than 30 s
/// are broken on acquisition).
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Maximum records the list keeps: when a new download pushes past
/// this, the oldest finished records are dropped (list only, files
/// are never touched). Active downloads are never dropped, so the
/// list can briefly exceed this while many downloads run.
pub const MAX_LIST_RECORDS: usize = 20;

/// Grace period for cross-process record removal. A record this
/// process holds in memory but that is absent from disk is only
/// pruned once it is older than this, so a record the other process
/// just created (and has not saved yet) is never dropped by mistake:
/// a just-created record is always young, while a record the other
/// process deleted is at least as old as its last update.
const REMOVAL_GRACE_SECS: u64 = 60;

fn acquire_lock(path: &std::path::Path) -> Option<LockGuard> {
    let lock_path = path.with_extension("json.lock");
    for _ in 0..50 {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "{} {}", std::process::id(), now_secs());
                return Some(LockGuard { path: lock_path });
            }
            Err(_) => {
                // Break a stale lock (older than 30 s); a save takes
                // milliseconds, so 30 s means the holder crashed.
                if let Ok(meta) = fs::metadata(&lock_path) {
                    if let Ok(mtime) = meta.modified() {
                        if let Ok(age) = std::time::SystemTime::now().duration_since(mtime) {
                            if age.as_secs() > 30 {
                                let _ = fs::remove_file(&lock_path);
                                continue;
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    None
}

fn bak_path(path: &std::path::Path) -> PathBuf {
    path.with_extension("json.bak")
}

/// Does a record's destination belong to this download folder?
/// True when it IS the folder (a not-yet-run record) or a file
/// under it (a completed record stores the final file path, which
/// may sit in a category subfolder and may carry a `(1)`-style
/// collision suffix, so only the folder — never the file name —
/// is compared here).
fn same_download_root(destination: &str, download_dir: &str) -> bool {
    if destination == download_dir {
        return true;
    }
    std::path::Path::new(destination).starts_with(std::path::Path::new(download_dir))
}

/// Delete every file belonging to a download record (round 10, item
/// 13's "delete the files too"): the final file, its `.part` /
/// `.audio.part`, its segment dir (`.part.seg`) and legacy sidecar
/// (`.part.segments`), yt-dlp partials (`<stem>.*.part` / `.ytdl` /
/// `.temp`), and the manifest sidecar (`manifests/<id>.m3u8` in the
/// data dir). The final file is found through the record's
/// destination (a completed record stores the full file path) or,
/// for a record that never completed, as `<dir>/<filename>` in the
/// download folder and one level of category subfolders (the only
/// depths the worker writes to). Returns how many files were
/// removed (a segment dir counts its files).
///
/// Safety: only paths under `download_dir` (or the exact manifest
/// sidecar path) are ever removed. A destination pointing outside
/// the download folder (the user moved folders since) is NOT
/// deleted — the record is removed from the list, but no foreign
/// file is touched. The download dir itself is never deleted.
pub fn delete_download_files(
    record: &DownloadRecord,
    download_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> usize {
    let mut removed = 0usize;
    let canonical_root = std::fs::canonicalize(download_dir).ok();
    let under_root = |p: &std::path::Path| -> bool {
        match (&canonical_root, std::fs::canonicalize(p)) {
            (Some(root), Ok(canon)) => canon.starts_with(root),
            // Unresolvable path (already gone): only a path built
            // under the download dir is ever passed here, so a
            // failed canonicalize means "nothing to delete", not
            // "unsafe".
            _ => p.starts_with(download_dir),
        }
    };
    // Plain fn (not a closure): the seg-dir branch below adds to
    // `removed` directly, which a capturing closure forbids.
    fn try_remove_file(p: &std::path::Path) -> bool {
        p.is_file() && std::fs::remove_file(p).is_ok()
    }
    // Candidate dirs: the download folder + one level of category
    // subfolders.
    let mut dirs = vec![download_dir.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(download_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            }
        }
    }
    // The final file: a completed record stores its full path;
    // anything else stores the folder, so the file is
    // <dir>/<filename>.
    let mut finals: Vec<PathBuf> = Vec::new();
    let dest = std::path::Path::new(&record.destination);
    if dest.is_file() && under_root(dest) {
        finals.push(dest.to_path_buf());
    } else {
        for d in &dirs {
            let cand = d.join(&record.filename);
            if cand.is_file() {
                finals.push(cand);
            }
        }
    }
    for final_path in &finals {
        let name = final_path.file_name().unwrap().to_string_lossy().to_string();
        let parent = final_path.parent().unwrap_or(download_dir);
        if try_remove_file(final_path) {
            removed += 1;
        }
        // .part / .audio.part and their segment dirs + sidecars.
        for suffix in [".part", ".audio.part"] {
            let part = parent.join(format!("{name}{suffix}"));
            if try_remove_file(&part) {
                removed += 1;
            }
            let seg_dir = parent.join(format!("{name}{suffix}.seg"));
            if seg_dir.is_dir() {
                let mut n = 0usize;
                if let Ok(files) = std::fs::read_dir(&seg_dir) {
                    for f in files.flatten() {
                        if f.metadata().map(|m| m.is_file()).unwrap_or(false) {
                            n += 1;
                        }
                    }
                }
                if std::fs::remove_dir_all(&seg_dir).is_ok() {
                    removed += n;
                }
            }
            let sidecar = parent.join(format!("{name}{suffix}.segments"));
            if try_remove_file(&sidecar) {
                removed += 1;
            }
        }
    }
    // yt-dlp partials: <stem>.*.part / .ytdl / .temp in each
    // candidate dir (merge parts are named <stem>.f<id>.mp4.part).
    let stem = std::path::Path::new(&record.filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !stem.is_empty() {
        let prefix = format!("{stem}.");
        for d in &dirs {
            if let Ok(entries) = std::fs::read_dir(d) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with(&prefix)
                        && (name.ends_with(".part")
                            || name.ends_with(".ytdl")
                            || name.ends_with(".temp"))
                    {
                        if try_remove_file(&e.path()) {
                            removed += 1;
                        }
                    }
                }
            }
        }
    }
    // Manifest sidecar (data dir, not the download folder).
    let sidecar = data_dir
        .join("manifests")
        .join(format!("{}.m3u8", record.id));
    if try_remove_file(&sidecar) {
        removed += 1;
    }
    removed
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        // No file yet: normal first run.
        if !path.exists() {
            return Self {
                path,
                records: BTreeMap::new(),
                load_warning: None,
                deleted: BTreeMap::new(),
            };
        }
        match fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(records) => Self {
                path,
                records,
                load_warning: None,
                deleted: BTreeMap::new(),
            },
            None => {
                // F12: corrupt file. Never overwrite it, never silently
                // wipe history. Rename it, restore from .bak if possible.
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let corrupt_name = format!("downloads.json.corrupt-{ts}-{}", std::process::id());
                let corrupt_path = path
                    .parent()
                    .map(|p| p.join(corrupt_name.clone()))
                    .unwrap_or_else(|| PathBuf::from(corrupt_name.clone()));
                let renamed = fs::rename(&path, &corrupt_path).is_ok();
                let (records, warning) =
                    match fs::read_to_string(bak_path(&path))
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                    {
                        Some(records) => (
                            records,
                            format!(
                                "Download history file was corrupt and was moved to {corrupt_name} (renamed: {renamed}). History was restored from the last good backup (downloads.json.bak). Downloaded files were not deleted."
                            ),
                        ),
                        None => (
                            BTreeMap::new(),
                            format!(
                                "Download history file was corrupt and was moved to {corrupt_name} (renamed: {renamed}). No usable backup was found, so history starts empty. Downloaded files were not deleted."
                            ),
                        ),
                    };
                Self {
                    path,
                    records,
                    load_warning: Some(warning),
                    deleted: BTreeMap::new(),
                }
            }
        }
    }
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // F03: file lock + reload-before-save. Without this, two
        // processes (app + bridge) each holding their own in-memory
        // Store would overwrite each other's records (last writer wins).
        let _guard = acquire_lock(&self.path)
            .ok_or_else(|| "Could not acquire state lock; will retry on next save".to_string())?;
        // Reload disk first. If disk is corrupt, refuse to overwrite it
        // (F12): a restart will rename it and restore from .bak.
        let disk_records: BTreeMap<String, DownloadRecord> = if self.path.exists() {
            match fs::read_to_string(&self.path) {
                Ok(s) => match serde_json::from_str(&s) {
                    Ok(recs) => recs,
                    Err(_) => {
                        return Err(
                            "State file on disk is corrupt; refusing to overwrite it".to_string()
                        );
                    }
                },
                Err(_) => BTreeMap::new(),
            }
        } else {
            BTreeMap::new()
        };
        let had_disk = self.path.exists();
        // Merge: disk records first, then in-memory records win only if
        // they are as new or newer than disk (by updated_unix). A stale
        // in-memory record must not overwrite a newer disk record.
        let mut merged = disk_records;
        let now = now_secs();
        for (id, rec) in &self.records {
            match merged.get(id) {
                // Absent from disk: either this process just created
                // it (young, keep it) or the other process deleted
                // it and saved (old, do not resurrect it). The grace
                // period tells them apart. When no disk file existed
                // yet, every in-memory record is new by definition,
                // so nothing is skipped.
                None if had_disk && now.saturating_sub(rec.updated_unix) > REMOVAL_GRACE_SECS => {}
                None => {
                    merged.insert(id.clone(), rec.clone());
                }
                Some(disk_rec) if rec.updated_unix >= disk_rec.updated_unix => {
                    merged.insert(id.clone(), rec.clone());
                }
                Some(_) => {}
            }
        }
        for (id, deleted_at) in &self.deleted {
            if let Some(disk_rec) = merged.get(id) {
                if disk_rec.updated_unix <= *deleted_at {
                    merged.remove(id);
                }
            }
        }
        // F12: keep the last good save as .bak, before overwriting.
        if had_disk {
            let _ = fs::copy(&self.path, bak_path(&self.path));
        }
        // Atomic write: temp file + rename, so a crash cannot corrupt history.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }
    /// Re-read the state file and merge any records this process does
    /// not know about (for example, downloads added by the bridge while
    /// the app was running). Same rule as `save`: a record wins only if
    /// it is newer than the one already held. A missing or corrupt file
    /// is ignored here; `save` still refuses to overwrite corrupt state.
    /// Records the other process removed (Clear finished / auto-trim)
    /// are also pruned from memory, so this process's next save does
    /// not resurrect them from stale memory.
    ///
    /// `active` is the set of download ids THIS process is currently
    /// running (its live job map). Only those records are protected
    /// from disk: a worker's status and progress live in memory, and
    /// a stale Paused on disk must not flip a progressing download
    /// (round 7: that is exactly what happened to the nime DASH
    /// download on 2026-11-23). Round 10: the round-7 rule protected
    /// ANY record whose in-memory status was Downloading, which froze
    /// the desktop UI on stale progress forever for bridge-driven
    /// downloads — the desktop never runs them, so its copy never
    /// advanced, while the bridge's newer disk copy was ignored
    /// (LookMovie showed "1 / 1,009 pieces" against 228/1009 + 2.7 GB
    /// on disk; the completed novahd record showed as "downloading"
    /// 0 B). A record this process is NOT running refreshes from a
    /// newer disk copy like any other record. A terminal record
    /// never un-completes: no process re-runs it, so a non-terminal
    /// disk copy can only be a stomp.
    pub fn reload_from_disk(&mut self, active: &std::collections::HashSet<String>) {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(disk_records): Result<BTreeMap<String, DownloadRecord>, _> =
            serde_json::from_str(&text)
        else {
            return;
        };
        // Snapshot the disk ids first: the merge loop below consumes
        // the map, and the prune step needs to know what is on disk.
        let disk_ids: std::collections::BTreeSet<String> = disk_records.keys().cloned().collect();
        for (id, disk_rec) in disk_records {
            match self.records.get(&id) {
                None => {
                    self.records.insert(id, disk_rec);
                }
                // Actively running in this process: memory wins,
                // whatever disk says.
                Some(_) if active.contains(&id) => {}
                // Terminal in memory, non-terminal on disk: a
                // terminal record never un-completes.
                Some(ours)
                    if ours.status.is_terminal() && !disk_rec.status.is_terminal() => {}
                Some(ours) if disk_rec.updated_unix > ours.updated_unix => {
                    self.records.insert(id, disk_rec);
                }
                Some(_) => {}
            }
        }
        // Prune records the other process removed: a record this
        // process holds that is absent from disk was deleted by the
        // other process and saved. The grace period protects a
        // record this process just created but has not saved yet.
        // A record this process is actively running is never
        // pruned: its worker lives here, so its absence from disk
        // can only mean the file was replaced mid-download, and
        // dropping it from memory would orphan the worker.
        let now = now_secs();
        let stale: Vec<String> = self
            .records
            .values()
            .filter(|r| {
                !active.contains(&r.id)
                    && !disk_ids.contains(&r.id)
                    && now.saturating_sub(r.updated_unix) > REMOVAL_GRACE_SECS
            })
            .map(|r| r.id.clone())
            .collect();
        for id in stale {
            self.records.remove(&id);
        }
    }
    pub fn upsert(&mut self, rec: DownloadRecord) {
        let is_new = !self.records.contains_key(&rec.id);
        self.records.insert(rec.id.clone(), rec);
        // Auto-trim on every new download, so the list stays at the
        // newest MAX_LIST_RECORDS instead of growing without bound
        // (a long browsing session can add hundreds of detections).
        // Progress updates (existing ids) never trim, and trimming
        // only ever removes finished records, never active downloads.
        if is_new {
            self.trim_finished(MAX_LIST_RECORDS);
        }
    }
    pub fn get(&self, id: &str) -> Option<&DownloadRecord> {
        self.records.get(id)
    }
    /// Find an existing record for the same download: same
    /// normalised URL, same filename, same download folder, and a
    /// status that means the file is already coming or already here
    /// (Queued, Downloading, or Completed). A duplicate add returns
    /// this record instead of queueing a second download. Failed,
    /// Cancelled and Paused records never match: a failed download
    /// must stay retryable, and a paused one is resumed from its
    /// own row, not re-added.
    ///
    /// The destination check accepts both shapes a record takes:
    /// Queued/Downloading records store the download folder itself,
    /// while Completed records store the final file path (which
    /// sits under the download folder, possibly in a category
    /// subfolder). A record whose file lives under a DIFFERENT
    /// folder (the user moved the download folder since) is not a
    /// duplicate.
    /// A download is a duplicate of another only when the URL,
    /// filename, download root AND the audio-only flag all match:
    /// a video and its audio track are different downloads of the
    /// same URL, so "Audio only" after the video must queue, not
    /// report "Already downloaded" (round 8).
    pub fn find_duplicate(
        &self,
        url: &str,
        filename: &str,
        download_dir: &str,
        audio_only: bool,
    ) -> Option<DownloadRecord> {
        self.find_duplicate_except(url, filename, download_dir, "", audio_only)
    }

    /// Same duplicate rule as [`find_duplicate`], but ignoring one
    /// record id: the worker's post-planning re-check must not
    /// match the record being planned (its own URL + filename).
    pub fn find_duplicate_except(
        &self,
        url: &str,
        filename: &str,
        download_dir: &str,
        except_id: &str,
        audio_only: bool,
    ) -> Option<DownloadRecord> {
        let want = crate::validate::normalize_url_for_dedupe(url);
        self.records
            .values()
            .find(|r| {
                r.id != except_id
                    && crate::validate::normalize_url_for_dedupe(&r.url) == want
                    && r.filename == filename
                    && r.audio_only == audio_only
                    && matches!(
                        r.status,
                        Status::Queued | Status::Downloading | Status::Completed
                    )
                    && same_download_root(&r.destination, download_dir)
            })
            .cloned()
    }
    pub fn all(&self) -> Vec<&DownloadRecord> {
        self.records.values().collect()
    }
    pub fn remove(&mut self, id: &str) -> bool {
        if self.records.remove(id).is_some() {
            self.deleted.insert(id.to_string(), now_secs());
            true
        } else {
            false
        }
    }
    /// Remove every finished record (Completed/Failed/Cancelled)
    /// from the list. Returns how many were removed. Records only:
    /// downloaded files are never touched, and active downloads
    /// (Queued/Downloading/Paused) are never removed. Removals go
    /// through `remove`, so the tombstone map keeps them deleted
    /// across this process's later saves.
    pub fn remove_finished(&mut self) -> usize {
        let ids: Vec<String> = self
            .records
            .values()
            .filter(|r| r.status.is_terminal())
            .map(|r| r.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.remove(&id);
        }
        n
    }
    /// Keep the list short: if it holds more than `keep_newest`
    /// records, remove the oldest finished ones until only the
    /// newest `keep_newest` remain. Active downloads are never
    /// removed, so the list can stay above `keep_newest` while many
    /// downloads run. Returns how many were removed. Records only:
    /// downloaded files are never touched.
    pub fn trim_finished(&mut self, keep_newest: usize) -> usize {
        if self.records.len() <= keep_newest {
            return 0;
        }
        // Newest first by creation time; the tail beyond keep_newest
        // holds the removal candidates, filtered to finished records.
        let mut ordered: Vec<&DownloadRecord> = self.records.values().collect();
        ordered.sort_by_key(|r| std::cmp::Reverse(r.created_unix));
        let ids: Vec<String> = ordered[keep_newest..]
            .iter()
            .filter(|r| r.status.is_terminal())
            .map(|r| r.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.remove(&id);
        }
        n
    }
    /// On startup, anything left `Downloading` was interrupted: mark Paused
    /// if it was resumable, Failed otherwise, so the UI never shows a
    /// phantom active download.
    pub fn recover_interrupted(&mut self) {
        for rec in self.records.values_mut() {
            if rec.status == Status::Downloading {
                rec.status = if rec.resumable {
                    Status::Paused
                } else {
                    Status::Failed
                };
                if rec.error.is_none() {
                    rec.error = Some("Interrupted by app close or crash".to_string());
                }
                rec.updated_unix = now_secs();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn store_round_trips_and_recovers() {
        let dir = std::env::temp_dir().join("fetchforge-test-store");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let mut s = Store::load(path.clone());
        // Real timestamps: a record this process just created is
        // young, and save() only skips re-inserting records that are
        // absent from disk AND older than the removal grace.
        let now = now_secs();
        s.upsert(DownloadRecord {
            id: "a".into(),
            url: "https://example.com/x.zip".into(),
            filename: "x.zip".into(),
            destination: "/tmp".into(),
            category: "Archives".into(),
            status: Status::Downloading,
            total_bytes: Some(10),
            downloaded_bytes: 4,
            resumable: true,
            error: None,
            created_unix: now,
            updated_unix: now,
            format_id: None,
            needs_merge: false,
            format_selector: None,
            stream_kind: None,
            referer: None,
            audio_url: None,
            duplicate: false,
            segments_total: None,
            segments_done: 0,
            estimated_total_bytes: None,
            audio_only: false,
            picked_height: None,
            picked_vbr_kbps: None,
            delivered_height: None,
            delivered_bitrate_kbps: None,
            quality_note: None,
            speed_bps: None,
            size_kind: None,
        });
        s.save().unwrap();
        let mut s2 = Store::load(path);
        s2.recover_interrupted();
        assert_eq!(s2.get("a").unwrap().status, Status::Paused);
    }

    fn rec(id: &str) -> DownloadRecord {
        // Real timestamps: a record this process just created is
        // young, and save() only skips re-inserting records that are
        // absent from disk AND older than the removal grace.
        let now = now_secs();
        DownloadRecord {
            id: id.into(),
            url: "https://example.com/x.zip".into(),
            filename: "x.zip".into(),
            destination: "/tmp".into(),
            category: "Archives".into(),
            status: Status::Completed,
            total_bytes: Some(10),
            downloaded_bytes: 10,
            resumable: true,
            error: None,
            created_unix: now,
            updated_unix: now,
            format_id: None,
            needs_merge: false,
            format_selector: None,
            stream_kind: None,
            referer: None,
            audio_url: None,
            duplicate: false,
            segments_total: None,
            segments_done: 0,
            estimated_total_bytes: None,
            audio_only: false,
            picked_height: None,
            picked_vbr_kbps: None,
            delivered_height: None,
            delivered_bitrate_kbps: None,
            quality_note: None,
            speed_bps: None,
            size_kind: None,
        }
    }

    fn rec_status(id: &str, status: Status, created: u64, updated: u64) -> DownloadRecord {
        let mut r = rec(id);
        r.status = status;
        r.created_unix = created;
        r.updated_unix = updated;
        r
    }

    #[test]
    fn corrupt_state_is_renamed_restored_and_warned() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-corrupt");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        // Good save first, so a .bak exists.
        let mut s = Store::load(path.clone());
        s.upsert(rec("good"));
        s.save().unwrap();
        // Second good save, so .bak holds the first save's record.
        let mut s = Store::load(path.clone());
        s.upsert(rec("good2"));
        s.save().unwrap();
        assert!(
            dir.join("downloads.json.bak").exists(),
            ".bak must exist after saves"
        );
        // Corrupt the live file.
        fs::write(&path, "this is not json {{{").unwrap();
        let corrupt_content = fs::read_to_string(&path).unwrap();
        let s2 = Store::load(path.clone());
        // Restored from .bak (which holds "good", not "good2", because
        // .bak is the previous good save).
        assert!(
            s2.get("good").is_some(),
            "history must be restored from .bak"
        );
        assert!(
            s2.load_warning.is_some(),
            "a visible warning must be recorded on corrupt load"
        );
        // Corrupt file renamed, not overwritten, with original content.
        let corrupt_files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(corrupt_files.len(), 1, "exactly one .corrupt file expected");
        assert_eq!(
            fs::read_to_string(corrupt_files[0].path()).unwrap(),
            corrupt_content
        );
        // A subsequent save must not overwrite the corrupt file.
        let mut s3 = Store::load(path.clone());
        s3.upsert(rec("after"));
        s3.save().unwrap();
        assert_eq!(
            fs::read_to_string(corrupt_files[0].path()).unwrap(),
            corrupt_content,
            "corrupt file must never be overwritten by a save"
        );
    }

    #[test]
    fn two_stores_merging_do_not_lose_each_others_records() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-merge");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let mut a = Store::load(path.clone());
        a.upsert(rec("one"));
        a.save().unwrap();
        // B loads after A's first save, so B knows "one".
        let mut b = Store::load(path.clone());
        // A adds "two" and saves. B does NOT reload, then adds "three"
        // and saves. Without reload-before-save, B's save would wipe "two".
        a.upsert(rec("two"));
        a.save().unwrap();
        b.upsert(rec("three"));
        b.save().unwrap();
        let disk: BTreeMap<String, DownloadRecord> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(disk.contains_key("one"), "one must survive");
        assert!(disk.contains_key("two"), "two must survive B's save");
        assert!(disk.contains_key("three"), "three must survive");
    }

    #[test]
    fn reload_from_disk_picks_up_other_process_records() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-reload");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let mut a = Store::load(path.clone());
        a.upsert(rec("one"));
        a.save().unwrap();
        // B loads when only "one" exists.
        let mut b = Store::load(path.clone());
        // A adds "two" and saves. B does not see it on its own...
        a.upsert(rec("two"));
        a.save().unwrap();
        assert!(b.get("two").is_none(), "B must not see two before reload");
        // ...but reload_from_disk picks it up without losing "one".
        b.reload_from_disk(&std::collections::HashSet::new());
        assert!(b.get("one").is_some(), "one must survive reload");
        assert!(b.get("two").is_some(), "two must appear after reload");
    }

    #[test]
    fn reload_from_disk_keeps_active_downloads_and_terminal_records() {
        // Round 7: a stale Paused on disk (written by a short-lived
        // native-host launch before round 7) must not flip a
        // progressing download to Paused in live memory, and a
        // terminal record must never un-complete. Other records
        // still refresh from disk by timestamp.
        let dir = std::env::temp_dir().join("fetchforge-test-store-reload-own");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let old = now_secs() - 3600;
        let mut a = Store::load(path.clone());
        let mut active = rec_status("active", Status::Downloading, old, old);
        active.downloaded_bytes = 500;
        active.segments_done = 5;
        a.upsert(active);
        a.upsert(rec_status("done", Status::Completed, old, old));
        a.upsert(rec_status("paused", Status::Paused, old, old));
        a.save().unwrap();
        // B loads the same file, so B holds all three.
        let mut b = Store::load(path.clone());
        // The other process writes a NEWER Paused over "active" (the
        // stomp), a NEWER Paused over "done", and a NEWER Failed
        // over "paused" (a genuine status change).
        let now = now_secs();
        a.upsert(rec_status("active", Status::Paused, old, now));
        a.upsert(rec_status("done", Status::Paused, old, now));
        a.upsert(rec_status("paused", Status::Failed, old, now));
        a.save().unwrap();
        // Round 10: "active" is running in THIS process (B), so it
        // is protected; "done" is terminal, so it never
        // un-completes; "paused" is neither, so it refreshes.
        let mut active = std::collections::HashSet::new();
        active.insert("active".to_string());
        b.reload_from_disk(&active);
        let kept = b.get("active").expect("active must survive reload");
        assert_eq!(
            kept.status,
            Status::Downloading,
            "an actively-downloading record must keep its in-memory status"
        );
        assert_eq!(kept.downloaded_bytes, 500, "progress must survive too");
        assert_eq!(kept.segments_done, 5);
        assert_eq!(
            b.get("done").unwrap().status,
            Status::Completed,
            "a terminal record must never un-complete"
        );
        assert_eq!(
            b.get("paused").unwrap().status,
            Status::Failed,
            "a non-owned record must still refresh from disk"
        );
    }

    #[test]
    fn reload_from_disk_never_prunes_active_downloads() {
        // An actively-downloading record absent from disk (the file
        // was replaced mid-download) must stay in memory: dropping
        // it would orphan the worker.
        let dir = std::env::temp_dir().join("fetchforge-test-store-reload-noprune");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let old = now_secs() - 3600; // past the removal grace
        let mut a = Store::load(path.clone());
        a.upsert(rec_status("active", Status::Downloading, old, old));
        a.upsert(rec_status("stale", Status::Completed, old, old));
        a.save().unwrap();
        let mut b = Store::load(path.clone());
        // The other process removes both records and saves.
        a.remove_finished();
        // "active" is Downloading, not finished: remove it directly
        // to simulate a disk file that lost it.
        a.records.remove("active");
        a.save().unwrap();
        let mut active = std::collections::HashSet::new();
        active.insert("active".to_string());
        b.reload_from_disk(&active);
        assert!(
            b.get("active").is_some(),
            "an active download must never be pruned"
        );
        assert!(
            b.get("stale").is_none(),
            "a finished record must still be pruned"
        );
    }

    #[test]
    fn corrupt_state_without_backup_starts_empty_with_warning() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-corrupt-nobak");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        fs::write(&path, "garbage").unwrap();
        let s = Store::load(path.clone());
        assert!(s.all().is_empty(), "no backup means empty history");
        assert!(s.load_warning.is_some(), "warning must be recorded");
        assert!(
            fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".corrupt-")),
            "corrupt file must be renamed, not deleted"
        );
    }

    #[test]
    fn remove_finished_keeps_active_downloads() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-remove-finished");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        let now = now_secs();
        s.upsert(rec_status("done", Status::Completed, now, now));
        s.upsert(rec_status("bad", Status::Failed, now, now));
        s.upsert(rec_status("cancelled", Status::Cancelled, now, now));
        s.upsert(rec_status("running", Status::Downloading, now, now));
        s.upsert(rec_status("waiting", Status::Queued, now, now));
        s.upsert(rec_status("held", Status::Paused, now, now));
        assert_eq!(s.remove_finished(), 3);
        assert!(s.get("done").is_none());
        assert!(s.get("bad").is_none());
        assert!(s.get("cancelled").is_none());
        assert!(s.get("running").is_some(), "active downloads must survive");
        assert!(s.get("waiting").is_some(), "queued downloads must survive");
        assert!(s.get("held").is_some(), "paused downloads must survive");
    }

    #[test]
    fn upsert_auto_trims_to_twenty_on_new_downloads() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-autotrim");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        let now = now_secs();
        for i in 0..25u64 {
            s.upsert(rec_status(
                &format!("f-{i:02}"),
                Status::Completed,
                now - 100 + i,
                now - 100 + i,
            ));
        }
        assert_eq!(s.all().len(), 20, "list must stay at 20 after 25 adds");
        assert!(s.get("f-24").is_some(), "newest must survive");
        assert!(s.get("f-05").is_some(), "20th newest must survive");
        assert!(s.get("f-04").is_none(), "21st newest must be auto-trimmed");
        assert!(s.get("f-00").is_none(), "oldest must be auto-trimmed");
        // A progress update (existing id) must not trim anything.
        let mut r = s.get("f-24").unwrap().clone();
        r.downloaded_bytes = 5;
        s.upsert(r);
        assert_eq!(s.all().len(), 20);
    }

    #[test]
    fn trim_finished_keeps_newest_and_never_active() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-trim");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        let now = now_secs();
        // 25 finished records; upsert already auto-trims to the
        // newest 20 on each add, so the store holds old-05..old-24.
        for i in 0..25u64 {
            s.upsert(rec_status(
                &format!("old-{i:02}"),
                Status::Completed,
                now - 1000 + i,
                now - 1000 + i,
            ));
        }
        // An old active download must survive every trim, even
        // though it is the oldest record in the store.
        s.upsert(rec_status(
            "active-old",
            Status::Downloading,
            now - 2000,
            now - 2000,
        ));
        assert_eq!(s.all().len(), 21, "active record must not be trimmed");
        let removed = s.trim_finished(10);
        assert_eq!(removed, 10, "10 oldest finished records must go");
        assert_eq!(s.all().len(), 11, "10 newest + the old active download");
        assert!(
            s.get("active-old").is_some(),
            "active downloads are never trimmed"
        );
        assert!(s.get("old-24").is_some(), "newest finished must survive");
        assert!(s.get("old-15").is_some(), "10th newest must survive");
        assert!(s.get("old-14").is_none(), "11th newest must be trimmed");
        assert!(
            s.get("old-05").is_none(),
            "oldest remaining must be trimmed"
        );
        // Under the limit, trim is a no-op.
        assert_eq!(s.trim_finished(20), 0);
    }

    #[test]
    fn reload_from_disk_prunes_records_removed_by_other_process() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-prune");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let old = now_secs() - 3600; // last updated an hour ago: past the grace
        let mut a = Store::load(path.clone());
        a.upsert(rec_status("gone", Status::Completed, old, old));
        a.upsert(rec_status("stays", Status::Completed, old, old));
        a.save().unwrap();
        // B loads the same file, so B holds both records.
        let mut b = Store::load(path.clone());
        // A clears finished records and saves: the file no longer
        // holds either record.
        assert_eq!(a.remove_finished(), 2);
        a.save().unwrap();
        // B still holds both in memory until it reloads...
        assert!(b.get("gone").is_some());
        // ...but reload prunes them instead of leaving them to be
        // resurrected by B's next save.
        b.reload_from_disk(&std::collections::HashSet::new());
        assert!(
            b.get("gone").is_none(),
            "removed records must be pruned on reload"
        );
        assert!(b.get("stays").is_none());
        b.save().unwrap();
        let disk: BTreeMap<String, DownloadRecord> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            disk.is_empty(),
            "B's save must not resurrect removed records"
        );
    }

    #[test]
    fn save_does_not_resurrect_records_removed_by_other_process() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-save-grace");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let old = now_secs() - 3600;
        let mut a = Store::load(path.clone());
        a.upsert(rec_status("gone", Status::Completed, old, old));
        a.save().unwrap();
        let mut b = Store::load(path.clone());
        // A removes the record and saves; B never reloads.
        a.remove_finished();
        a.save().unwrap();
        assert!(b.get("gone").is_some(), "B still holds the stale record");
        // B's save must not put it back on disk.
        b.save().unwrap();
        let disk: BTreeMap<String, DownloadRecord> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            !disk.contains_key("gone"),
            "B's save must not resurrect a removed record"
        );
    }

    #[test]
    fn reload_from_disk_keeps_just_created_records() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-prune-grace");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let now = now_secs();
        let mut a = Store::load(path.clone());
        a.upsert(rec_status("saved", Status::Completed, now, now));
        a.save().unwrap();
        let mut b = Store::load(path.clone());
        // B creates a record but has not saved it yet: it is absent
        // from disk but young, so a reload must not prune it.
        b.upsert(rec_status("fresh", Status::Downloading, now, now));
        b.reload_from_disk(&std::collections::HashSet::new());
        assert!(
            b.get("fresh").is_some(),
            "a just-created record must survive reload"
        );
        assert!(b.get("saved").is_some());
        // And B's save must still write it to disk.
        b.save().unwrap();
        let disk: BTreeMap<String, DownloadRecord> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            disk.contains_key("fresh"),
            "a just-created record must be saved"
        );
    }

    #[test]
    fn find_duplicate_matches_active_and_completed_not_failed() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-dedupe");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        let now = now_secs();
        // A completed direct download: destination is the final file
        // path (with a collision suffix), under the download folder.
        let mut done = rec_status("done", Status::Completed, now, now);
        done.url = "https://cdn.example.com/v/clip.mp4".to_string();
        done.filename = "clip.mp4".to_string();
        done.destination = "/home/user/Downloads/Video/clip (1).mp4".to_string();
        s.upsert(done);
        // A queued media download: destination is the folder itself.
        let mut queued = rec_status("queued", Status::Queued, now, now);
        queued.url = "https://www.youtube.com/watch?v=abc".to_string();
        queued.filename = "Video.mp4".to_string();
        queued.destination = "/home/user/Downloads".to_string();
        s.upsert(queued);
        // A failed download for a third URL: must stay retryable.
        let mut failed = rec_status("failed", Status::Failed, now, now);
        failed.url = "https://example.com/dead.mp4".to_string();
        failed.filename = "dead.mp4".to_string();
        failed.destination = "/home/user/Downloads".to_string();
        s.upsert(failed);

        // Completed + same URL/filename/folder: duplicate, even with
        // a trailing slash and whitespace on the incoming URL.
        let dup = s.find_duplicate(
            "  https://cdn.example.com/v/clip.mp4/ ",
            "clip.mp4",
            "/home/user/Downloads",
            false,
        );
        assert_eq!(dup.as_ref().map(|r| r.id.as_str()), Some("done"));
        // Queued: duplicate too.
        let dup = s.find_duplicate(
            "https://www.youtube.com/watch?v=abc",
            "Video.mp4",
            "/home/user/Downloads",
            false,
        );
        assert_eq!(dup.as_ref().map(|r| r.id.as_str()), Some("queued"));
        // Failed: NOT a duplicate (retry must work).
        assert!(s
            .find_duplicate(
                "https://example.com/dead.mp4",
                "dead.mp4",
                "/home/user/Downloads",
                false,
            )
            .is_none());
        // Different filename: not a duplicate.
        assert!(s
            .find_duplicate(
                "https://cdn.example.com/v/clip.mp4",
                "other.mp4",
                "/home/user/Downloads",
                false,
            )
            .is_none());
        // Different query string: a different file, not a duplicate.
        assert!(s
            .find_duplicate(
                "https://cdn.example.com/v/clip.mp4?token=zzz",
                "clip.mp4",
                "/home/user/Downloads",
                false,
            )
            .is_none());
        // Same file under a different download folder: not a duplicate.
        assert!(s
            .find_duplicate(
                "https://cdn.example.com/v/clip.mp4",
                "clip.mp4",
                "/home/user/OtherDownloads",
                false,
            )
            .is_none());
    }

    #[test]
    fn find_duplicate_treats_audio_only_as_a_different_download() {
        // Round 8: a video and its audio track are different
        // downloads of the same URL — "Audio only" after the video
        // must queue, not report "Already downloaded"; and a second
        // audio-only request must match the first audio download.
        let dir = std::env::temp_dir().join("fetchforge-test-store-dedupe-audio");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        let now = now_secs();
        let mut video = rec_status("video", Status::Completed, now, now);
        video.url = "https://cdn.example.com/v/clip.mp4".to_string();
        video.filename = "clip.mp4".to_string();
        video.destination = "/home/user/Downloads/Video/clip.mp4".to_string();
        s.upsert(video);
        // Audio-only for the same URL + filename: NOT a duplicate
        // of the completed video.
        assert!(s
            .find_duplicate(
                "https://cdn.example.com/v/clip.mp4",
                "clip.mp4",
                "/home/user/Downloads",
                true,
            )
            .is_none());
        // …but a video request still matches the video.
        assert_eq!(
            s.find_duplicate(
                "https://cdn.example.com/v/clip.mp4",
                "clip.mp4",
                "/home/user/Downloads",
                false,
            )
            .unwrap()
            .id,
            "video"
        );
        // Once the audio download exists, a second audio-only
        // request matches IT (by its .m4a filename, which the
        // post-step wrote on the record at completion).
        let mut audio = rec_status("audio", Status::Completed, now, now);
        audio.url = "https://cdn.example.com/v/clip.mp4".to_string();
        audio.filename = "clip.m4a".to_string();
        audio.destination = "/home/user/Downloads/Video/clip.m4a".to_string();
        audio.audio_only = true;
        s.upsert(audio);
        assert_eq!(
            s.find_duplicate(
                "https://cdn.example.com/v/clip.mp4",
                "clip.m4a",
                "/home/user/Downloads",
                true,
            )
            .unwrap()
            .id,
            "audio"
        );
    }

    #[test]
    fn reload_from_disk_refreshes_non_active_downloading_records() {
        // Round 10, item 6: the desktop's copy of a BRIDGE-driven
        // download is stale (the desktop never runs it), so a
        // newer disk copy must win — the round-7 rule kept the
        // stale in-memory copy over ANY disk copy and the UI
        // showed "1 / 1,009 pieces" against 228/1009 + 2.7 GB on
        // disk, and a completed novahd record as "downloading"
        // 0 B. A record the other process COMPLETED must refresh
        // to Completed too.
        let dir = std::env::temp_dir().join("fetchforge-test-store-reload-r10");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let old = now_secs() - 3600;
        let mut a = Store::load(path.clone());
        let mut stale = rec_status("movie", Status::Downloading, old, old);
        stale.downloaded_bytes = 1_000_000;
        stale.segments_done = 1;
        stale.segments_total = Some(1009);
        a.upsert(stale);
        let mut done_stale = rec_status("done", Status::Downloading, old, old);
        done_stale.downloaded_bytes = 0;
        a.upsert(done_stale);
        a.save().unwrap();
        // B loads the same file, so B holds the stale copies.
        let mut b = Store::load(path.clone());
        // The other process (the bridge) advances "movie" and
        // completes "done", and saves.
        let now = now_secs();
        let mut fresh = rec_status("movie", Status::Downloading, old, now);
        fresh.downloaded_bytes = 2_700_000_000;
        fresh.segments_done = 228;
        fresh.segments_total = Some(1009);
        a.upsert(fresh);
        let mut done_fresh = rec_status("done", Status::Completed, old, now);
        done_fresh.downloaded_bytes = 3_367_173_048;
        done_fresh.total_bytes = Some(3_367_173_048);
        a.upsert(done_fresh);
        a.save().unwrap();
        // B runs NOTHING: neither record is in its active set.
        b.reload_from_disk(&std::collections::HashSet::new());
        let got = b.get("movie").unwrap();
        assert_eq!(got.downloaded_bytes, 2_700_000_000);
        assert_eq!(got.segments_done, 228);
        assert_eq!(
            b.get("done").unwrap().status,
            Status::Completed,
            "a record the other process completed must show Completed"
        );
    }

    #[test]
    fn apply_progress_never_moves_backwards() {
        // Round 10, items 2/6: the Plex HLS record went 645/820
        // back to 353/821 when the audio phase restarted its
        // segment counter. Progress reports apply monotonically:
        // a smaller value never overwrites a larger one.
        let mut r = rec("p");
        r.status = Status::Downloading;
        r.downloaded_bytes = 500_000_000;
        r.total_bytes = Some(1_000_000_000);
        r.segments_done = 645;
        r.segments_total = Some(820);
        let backwards = crate::download::Progress {
            downloaded: 488_702_134,
            total: Some(900_000_000),
            bytes_per_sec: 1000.0,
            segments_done: Some(353),
            segments_total: Some(821),
            estimated_total: Some(950_000_000),
        };
        r.apply_progress(&backwards);
        assert_eq!(r.downloaded_bytes, 500_000_000, "bytes must not go back");
        assert_eq!(r.total_bytes, Some(1_000_000_000), "total must not shrink");
        assert_eq!(r.segments_done, 645, "segments must not go back");
        assert_eq!(
            r.segments_total,
            Some(821),
            "a larger total is still accepted"
        );
        assert_eq!(
            r.estimated_total_bytes,
            Some(950_000_000),
            "the estimate refines (latest wins)"
        );
        // A forward report still applies.
        let forward = crate::download::Progress {
            downloaded: 600_000_000,
            total: None,
            bytes_per_sec: 1000.0,
            segments_done: Some(700),
            segments_total: None,
            estimated_total: None,
        };
        r.apply_progress(&forward);
        assert_eq!(r.downloaded_bytes, 600_000_000);
        assert_eq!(r.segments_done, 700);
        assert_eq!(r.total_bytes, Some(1_000_000_000), "None never wipes a total");
    }

    #[test]
    fn apply_progress_stores_speed_and_marks_measured() {
        // Round 11, items 8/10: the live speed lands in the
        // record, and a measured estimate labels the total
        // Measured when no plan kind was set — never downgrading
        // a plan kind.
        let mut r = rec("s");
        r.status = Status::Downloading;
        r.total_bytes = None;
        let p = crate::download::Progress {
            downloaded: 100_000_000,
            total: None,
            bytes_per_sec: 2_500_000.0,
            segments_done: Some(10),
            segments_total: Some(100),
            estimated_total: Some(1_000_000_000),
        };
        r.apply_progress(&p);
        assert_eq!(r.speed_bps, Some(2_500_000.0));
        assert_eq!(r.size_kind, Some(crate::media::SizeKind::Measured));
        // A 0 speed never overwrites a real one.
        let zero = crate::download::Progress {
            bytes_per_sec: 0.0,
            ..p
        };
        r.apply_progress(&zero);
        assert_eq!(r.speed_bps, Some(2_500_000.0));
        // A plan kind is never downgraded by the estimate.
        r.size_kind = Some(crate::media::SizeKind::Average);
        r.apply_progress(&p);
        assert_eq!(r.size_kind, Some(crate::media::SizeKind::Average));
    }

    #[test]
    fn complete_with_file_marks_total_exact() {
        // Round 11, item 10: the file on disk is the exact total
        // — the ladder kind is settled at completion.
        let dir = std::env::temp_dir().join("fetchforge-test-store-complete-exact");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("Movie.mp4");
        fs::write(&file, vec![0u8; 999]).unwrap();
        let mut r = rec("e");
        r.status = Status::Downloading;
        r.total_bytes = Some(50_000_000);
        r.size_kind = Some(crate::media::SizeKind::Peak);
        r.complete_with_file(&file);
        assert_eq!(r.total_bytes, Some(999));
        assert_eq!(r.size_kind, Some(crate::media::SizeKind::Exact));
    }

    #[test]
    fn complete_with_file_reconciles_against_real_size() {
        // Round 10, item 2: the novahd record claimed 121 MB for
        // a 3.37 GB file. Completion trusts the file on disk.
        let dir = std::env::temp_dir().join("fetchforge-test-store-complete");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("Movie.mp4");
        fs::write(&file, vec![0u8; 12345]).unwrap();
        let mut r = rec("c");
        r.status = Status::Downloading;
        r.downloaded_bytes = 100;
        r.total_bytes = Some(121_000_000);
        r.complete_with_file(&file);
        assert_eq!(r.status, Status::Completed);
        assert_eq!(r.downloaded_bytes, 12345);
        assert_eq!(r.total_bytes, Some(12345));
        assert_eq!(r.filename, "Movie.mp4");
        // A missing file falls back to the old behaviour (planned
        // total, or last reported bytes) instead of erroring.
        let mut r2 = rec("c2");
        r2.status = Status::Downloading;
        r2.downloaded_bytes = 77;
        r2.total_bytes = None;
        r2.complete_with_file(&dir.join("gone.mp4"));
        assert_eq!(r2.downloaded_bytes, 77);
        assert_eq!(r2.total_bytes, None);
    }

    #[test]
    fn apply_delivered_quality_notes_mismatch_only() {
        // Round 10, item 8: Soap2Day picked 1080p / 3000 kb/s and
        // delivered 720p at 865 kb/s — the record must say so.
        let mut r = rec("q");
        r.picked_height = Some(1080);
        r.picked_vbr_kbps = Some(3000);
        r.apply_delivered_quality(Some(720), Some(865));
        assert_eq!(r.delivered_height, Some(720));
        assert_eq!(r.delivered_bitrate_kbps, Some(865));
        assert_eq!(
            r.quality_note.as_deref(),
            Some("Delivered 720p at ~865 kb/s; the site listed this stream as 1080p / 3000 kb/s.")
        );
        // A matching delivery gets no note.
        let mut ok = rec("q2");
        ok.picked_height = Some(1080);
        ok.picked_vbr_kbps = Some(3000);
        ok.apply_delivered_quality(Some(1080), Some(2900));
        assert!(ok.quality_note.is_none());
        // Unknown on either side: no note, no accusation.
        let mut unknown = rec("q3");
        unknown.apply_delivered_quality(Some(720), Some(865));
        assert!(unknown.quality_note.is_none());
    }

    #[test]
    fn delete_download_files_removes_only_its_own_files() {
        // Round 10, item 13: "delete the files too" removes the
        // final file, parts, segment dir, sidecars and yt-dlp
        // partials — and nothing else.
        let base = std::env::temp_dir().join("fetchforge-test-store-delete");
        let _ = fs::remove_dir_all(&base);
        let download_dir = base.join("Downloads");
        let cat = download_dir.join("Video");
        let data_dir = base.join("data");
        let manifests = data_dir.join("manifests");
        fs::create_dir_all(&cat).unwrap();
        fs::create_dir_all(&manifests).unwrap();
        // Another download's file: must survive.
        fs::write(download_dir.join("Other.mp4"), b"keep").unwrap();
        // This download's files (in the category subfolder).
        fs::write(cat.join("Movie.mp4"), b"film").unwrap();
        fs::write(cat.join("Movie.mp4.part"), b"part").unwrap();
        fs::create_dir_all(cat.join("Movie.mp4.part.seg")).unwrap();
        fs::write(cat.join("Movie.mp4.part.seg").join("000000.seg"), b"s").unwrap();
        fs::write(cat.join("Movie.mp4.part.seg").join("000001.seg"), b"s").unwrap();
        fs::write(cat.join("Movie.mp4.part.segments"), b"sidecar").unwrap();
        fs::write(cat.join("Movie.f140.mp4.part"), b"ytdlp").unwrap();
        fs::write(manifests.join("rec1.m3u8"), b"#EXTM3U").unwrap();
        let mut r = rec("rec1");
        r.filename = "Movie.mp4".to_string();
        r.destination = cat.join("Movie.mp4").to_string_lossy().to_string();
        let removed = delete_download_files(&r, &download_dir, &data_dir);
        // Movie.mp4 + .part + 2 seg files + .segments + ytdlp
        // partial + manifest sidecar = 7.
        assert_eq!(removed, 7);
        assert!(!cat.join("Movie.mp4").exists());
        assert!(!cat.join("Movie.mp4.part").exists());
        assert!(!cat.join("Movie.mp4.part.seg").exists());
        assert!(!cat.join("Movie.mp4.part.segments").exists());
        assert!(!cat.join("Movie.f140.mp4.part").exists());
        assert!(!manifests.join("rec1.m3u8").exists());
        assert!(
            download_dir.join("Other.mp4").exists(),
            "another download's file must survive"
        );
        // A destination outside the download folder is never
        // deleted (the user moved folders since).
        let outside = base.join("Elsewhere");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("Moved.mp4"), b"moved").unwrap();
        let mut r2 = rec("rec2");
        r2.filename = "Moved.mp4".to_string();
        r2.destination = outside.join("Moved.mp4").to_string_lossy().to_string();
        let removed2 = delete_download_files(&r2, &download_dir, &data_dir);
        assert_eq!(removed2, 0);
        assert!(outside.join("Moved.mp4").exists());
    }

    #[test]
    fn find_duplicate_except_ignores_the_named_record() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-dup-except");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut s = Store::load(dir.join("downloads.json"));
        s.upsert(rec("self"));
        // The record itself is a duplicate for the plain lookup…
        assert!(s
            .find_duplicate("https://example.com/x.zip", "x.zip", "/tmp", false,)
            .is_some());
        // …but not when it is the excluded id (the worker's
        // post-planning re-check must not match the record being
        // planned).
        assert!(s
            .find_duplicate_except("https://example.com/x.zip", "x.zip", "/tmp", "self", false,)
            .is_none());
        // …while another record for the same file still matches.
        s.upsert(rec("other"));
        assert_eq!(
            s.find_duplicate_except("https://example.com/x.zip", "x.zip", "/tmp", "self", false,)
                .unwrap()
                .id,
            "other"
        );
    }
}
