use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const BASE_BACKOFF_SECONDS: u64 = 300;
pub const MAX_BACKOFF_SECONDS: u64 = 86400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedUnit {
    pub attempts: u32,
    pub last_error: String,
    pub retry_after: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StatePayload {
    pub device_id: Option<String>,
    #[serde(default)]
    pub failed_units: BTreeMap<String, FailedUnit>,
}

#[derive(Debug)]
pub struct StateStore {
    pub path: PathBuf,
    pub device_id: Option<String>,
    pub failed_units: BTreeMap<String, FailedUnit>,
}

pub fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl StateStore {
    pub fn load(state_path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = state_path.into();
        let mut store = Self {
            path,
            device_id: None,
            failed_units: BTreeMap::new(),
        };
        if store.path.exists() {
            let content = fs::read_to_string(&store.path)
                .map_err(|e| format!("Failed reading {}: {e}", store.path.display()))?;
            let payload: StatePayload = serde_json::from_str(&content)
                .map_err(|e| format!("Failed parsing {}: {e}", store.path.display()))?;
            store.device_id = payload.device_id.filter(|s| !s.is_empty());
            store.failed_units = payload.failed_units;
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;

        let payload = StatePayload {
            device_id: self.device_id.clone(),
            failed_units: self.failed_units.clone(),
        };
        let mut json = serde_json::to_string_pretty(&payload)
            .map_err(|e| format!("Failed serializing state: {e}"))?;
        json.push('\n');

        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_path = parent.join(format!(".state-{pid}-{nanos}.tmp"));

        let write_res = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
            fs::rename(&tmp_path, &self.path)?;
            Ok(())
        })();

        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("Failed saving {}: {err}", self.path.display()));
        }
        Ok(())
    }

    pub fn set_device_id(&mut self, device_id: Option<String>) -> Result<(), String> {
        let normalized = device_id.filter(|s| !s.is_empty());
        if normalized != self.device_id {
            self.device_id = normalized;
            self.save()?;
        }
        Ok(())
    }

    pub fn record_failure_at(&mut self, unit_id: i64, error: &str, now: f64) -> Result<f64, String> {
        let key = unit_id.to_string();
        let prev_attempts = self.failed_units.get(&key).map(|u| u.attempts).unwrap_or(0);
        let attempts = prev_attempts.saturating_add(1);
        let shift = attempts.saturating_sub(1).min(31);
        let delay = BASE_BACKOFF_SECONDS
            .saturating_mul(1u64 << shift)
            .min(MAX_BACKOFF_SECONDS);
        let retry_after = now + (delay as f64);
        let truncated_error: String = error.chars().take(500).collect();

        self.failed_units.insert(
            key,
            FailedUnit {
                attempts,
                last_error: truncated_error,
                retry_after,
            },
        );
        self.save()?;
        Ok(retry_after)
    }

    pub fn record_failure(&mut self, unit_id: i64, error: &str) -> Result<f64, String> {
        self.record_failure_at(unit_id, error, now_unix_secs())
    }

    pub fn clear_failure(&mut self, unit_id: i64) -> Result<(), String> {
        let key = unit_id.to_string();
        if self.failed_units.remove(&key).is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub fn should_skip_at(&self, unit_id: i64, now: f64) -> bool {
        match self.failed_units.get(&unit_id.to_string()) {
            Some(entry) => now < entry.retry_after,
            None => false,
        }
    }

    pub fn should_skip(&self, unit_id: i64) -> bool {
        self.should_skip_at(unit_id, now_unix_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thumbgen_state_test_{}_{}_{}",
            name,
            std::process::id(),
            now_unix_secs().to_bits()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_state_store_persistence_permissions_and_backoff() {
        let dir = temp_state_dir("basic");
        let state_path = dir.join("state.json");
        let mut store = StateStore::load(&state_path).unwrap();

        store.set_device_id(Some("dev-123".into())).unwrap();
        let mode = fs::metadata(&state_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let retry1 = store.record_failure_at(42, "decode error", 1000.0).unwrap();
        assert_eq!(retry1, 1300.0);
        assert!(store.should_skip_at(42, 1200.0));
        assert!(!store.should_skip_at(42, 1300.0));

        let retry2 = store.record_failure_at(42, "decode error", 1300.0).unwrap();
        assert_eq!(retry2, 1900.0);

        // Verify cap at 86400s
        for _ in 0..12 {
            store.record_failure_at(42, "decode error", 2000.0).unwrap();
        }
        let capped = store.record_failure_at(42, "decode error", 2000.0).unwrap();
        assert_eq!(capped, 2000.0 + 86400.0);

        let mut reloaded = StateStore::load(&state_path).unwrap();
        assert_eq!(reloaded.device_id.as_deref(), Some("dev-123"));
        assert_eq!(reloaded.failed_units["42"].attempts, 15);

        reloaded.clear_failure(42).unwrap();
        assert!(!reloaded.should_skip_at(42, 2001.0));

        let _ = fs::remove_dir_all(dir);
    }
}
