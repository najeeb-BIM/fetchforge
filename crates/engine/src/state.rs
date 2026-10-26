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
}

pub struct Store {
    path: PathBuf,
    records: BTreeMap<String, DownloadRecord>,
    /// F12: set when `load` found a corrupt state file. Callers should
    /// surface this to the user (bridge prints it, desktop app logs it).
    /// It is never part of the saved JSON.
    pub load_warning: Option<String>,
}

fn bak_path(path: &PathBuf) -> PathBuf {
    path.with_extension("json.bak")
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        // No file yet: normal first run.
        if !path.exists() {
            return Self {
                path,
                records: BTreeMap::new(),
                load_warning: None,
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
                }
            }
        }
    }
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // F12: keep the last good save as .bak, but only if the current
        // file actually parses. Copying a corrupt file to .bak would
        // destroy the only good backup.
        if self.path.exists() {
            if let Ok(s) = fs::read_to_string(&self.path) {
                if serde_json::from_str::<BTreeMap<String, DownloadRecord>>(&s).is_ok() {
                    let _ = fs::copy(&self.path, bak_path(&self.path));
                }
            }
        }
        // Atomic write: temp file + rename, so a crash cannot corrupt history.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&self.records).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn upsert(&mut self, rec: DownloadRecord) {
        self.records.insert(rec.id.clone(), rec);
    }
    pub fn get(&self, id: &str) -> Option<&DownloadRecord> {
        self.records.get(id)
    }
    pub fn all(&self) -> Vec<&DownloadRecord> {
        self.records.values().collect()
    }
    pub fn remove(&mut self, id: &str) -> bool {
        self.records.remove(id).is_some()
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
        let now = 1_700_000_000;
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
        });
        s.save().unwrap();
        let mut s2 = Store::load(path);
        s2.recover_interrupted();
        assert_eq!(s2.get("a").unwrap().status, Status::Paused);
    }

    fn rec(id: &str) -> DownloadRecord {
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
            created_unix: 1_700_000_000,
            updated_unix: 1_700_000_000,
        }
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
}
