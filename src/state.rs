use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10 MB
pub const QUOTA_BYTES: u64 = 30 * 1024 * 1024; // 30 MB / hour / IP
pub const TTL: Duration = Duration::from_secs(60 * 60);

pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub created: Instant,
}

pub struct AppState {
    pub dir: PathBuf,
    pub base_url: Option<String>,
    files: Mutex<HashMap<String, Entry>>,
    quota: Mutex<HashMap<String, Vec<(Instant, u64)>>>,
}

impl AppState {
    pub fn new(dir: PathBuf, base_url: Option<String>) -> Self {
        Self {
            dir,
            base_url,
            files: Mutex::new(HashMap::new()),
            quota: Mutex::new(HashMap::new()),
        }
    }

    /// Reserve `size` bytes of quota for `ip`. Returns remaining bytes on failure.
    pub fn try_reserve(&self, ip: &str, size: u64) -> Result<(), u64> {
        let mut q = self.quota.lock().unwrap();
        let now = Instant::now();
        let entries = q.entry(ip.to_string()).or_default();
        entries.retain(|(t, _)| now.duration_since(*t) < TTL);
        let used: u64 = entries.iter().map(|(_, b)| b).sum();
        if used + size > QUOTA_BYTES {
            return Err(QUOTA_BYTES.saturating_sub(used));
        }
        entries.push((now, size));
        Ok(())
    }

    /// Bytes this IP may still upload in the current window.
    pub fn remaining(&self, ip: &str) -> u64 {
        let mut q = self.quota.lock().unwrap();
        let now = Instant::now();
        let entries = q.entry(ip.to_string()).or_default();
        entries.retain(|(t, _)| now.duration_since(*t) < TTL);
        let used: u64 = entries.iter().map(|(_, b)| b).sum();
        QUOTA_BYTES.saturating_sub(used)
    }

    pub fn insert(&self, id: String, entry: Entry) {
        self.files.lock().unwrap().insert(id, entry);
    }

    /// Take an entry out of the map: a file can only be downloaded once.
    pub fn take(&self, id: &str) -> Option<Entry> {
        self.files.lock().unwrap().remove(id)
    }

    pub fn expired(&self) -> Vec<Entry> {
        let mut files = self.files.lock().unwrap();
        let now = Instant::now();
        let ids: Vec<String> = files
            .iter()
            .filter(|(_, e)| now.duration_since(e.created) >= TTL)
            .map(|(k, _)| k.clone())
            .collect();
        ids.into_iter().filter_map(|id| files.remove(&id)).collect()
    }

    pub fn gc_quota(&self) {
        let mut q = self.quota.lock().unwrap();
        let now = Instant::now();
        for entries in q.values_mut() {
            entries.retain(|(t, _)| now.duration_since(*t) < TTL);
        }
        q.retain(|_, v| !v.is_empty());
    }

    pub fn stats(&self) -> (usize, u64) {
        let files = self.files.lock().unwrap();
        (files.len(), files.values().map(|e| e.size).sum())
    }
}
