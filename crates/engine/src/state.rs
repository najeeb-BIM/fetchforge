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
    pub fn reload_from_disk(&mut self) {
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
                // Round 7: never let disk overwrite a record this
                // process owns. An actively-downloading record's
                // status and progress live in memory: a stale Paused
                // on disk (written by a short-lived native-host
                // launch before round 7 moved recovery behind the
                // bind) must not flip a progressing download to
                // Paused — that is exactly what happened to the nime
                // DASH download on 2026-11-23 (it kept downloading
                // while the UI said Paused). A terminal record never
                // un-completes either: no other process can resume
                // or re-run it, so disk can only hold an older or
                // stomped view of it.
                Some(ours)
                    if ours.status == Status::Downloading || ours.status.is_terminal() => {}
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
        // An actively-downloading record is never pruned: its
        // worker lives in this process, so its absence from disk
        // can only mean the file was replaced mid-download, and
        // dropping it from memory would orphan the worker.
        let now = now_secs();
        let stale: Vec<String> = self
            .records
            .values()
            .filter(|r| {
                r.status != Status::Downloading
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
        b.reload_from_disk();
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
        b.reload_from_disk();
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
        b.reload_from_disk();
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
        b.reload_from_disk();
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
        b.reload_from_disk();
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
