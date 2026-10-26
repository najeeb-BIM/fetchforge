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
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let records = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, records }
    }
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
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
}
