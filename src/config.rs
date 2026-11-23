use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;

pub const DEFAULT_WATCH_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "avi", "mts", "m2ts", "wmv", "3gp", "webm", "heic", "heif",
];

#[derive(Debug, Clone)]
pub struct Settings {
    pub dsm_url: String,
    pub dsm_username: String,
    pub dsm_password: String,
    pub photos_dir: PathBuf,
    pub data_dir: PathBuf,
    pub watch_enabled: bool,
    pub poll_interval_seconds: u64,
    pub watch_debounce_seconds: u64,
    pub watch_followup_seconds: u64,
    pub watch_extensions: HashSet<String>,
    pub verify_ssl: bool,
    pub mark_broken_on_error: bool,
    pub download_fallback: bool,
    pub ffmpeg_bin: String,
    pub ffprobe_bin: String,
    pub heif_convert_bin: String,
    pub log_level: String,
    pub device_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_env_map(&HashMap::new()).expect("default settings are valid")
    }
}

fn parse_bool(value: Option<&String>, default: bool) -> bool {
    match value {
        Some(v) if !v.trim().is_empty() => {
            matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        }
        _ => default,
    }
}

fn parse_u64(value: Option<&String>, default: u64, minimum: u64, name: &str) -> Result<u64, String> {
    match value {
        Some(v) if !v.trim().is_empty() => {
            let parsed: u64 = v
                .trim()
                .parse()
                .map_err(|e| format!("Invalid integer for {name}: {e}"))?;
            if parsed < minimum {
                return Err(format!("Expected {name} >= {minimum}, got {parsed}"));
            }
            Ok(parsed)
        }
        _ => Ok(default),
    }
}

fn parse_watch_extensions(value: Option<&String>) -> HashSet<String> {
    match value {
        Some(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|part| part.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|ext| !ext.is_empty())
            .collect(),
        _ => DEFAULT_WATCH_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn get_non_empty(map: &HashMap<String, String>, key: &str, default: &str) -> String {
    match map.get(key) {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

impl Settings {
    pub fn from_env() -> Result<Self, String> {
        let map: HashMap<String, String> = env::vars().collect();
        Self::from_env_map(&map)
    }

    pub fn from_env_map(src: &HashMap<String, String>) -> Result<Self, String> {
        let dsm_url = get_non_empty(src, "DSM_URL", "http://127.0.0.1:5000")
            .trim_end_matches('/')
            .to_string();
        let watch_enabled = parse_bool(src.get("WATCH_ENABLED"), true);
        let default_poll = if watch_enabled { 1800 } else { 300 };

        Ok(Self {
            dsm_url,
            dsm_username: src.get("DSM_USERNAME").map(|s| s.trim().to_string()).unwrap_or_default(),
            dsm_password: src.get("DSM_PASSWORD").cloned().unwrap_or_default(),
            photos_dir: PathBuf::from(get_non_empty(src, "PHOTOS_DIR", "/photos")),
            data_dir: PathBuf::from(get_non_empty(src, "DATA_DIR", "/data")),
            watch_enabled,
            poll_interval_seconds: parse_u64(
                src.get("POLL_INTERVAL_SECONDS"),
                default_poll,
                1,
                "POLL_INTERVAL_SECONDS",
            )?,
            watch_debounce_seconds: parse_u64(
                src.get("WATCH_DEBOUNCE_SECONDS"),
                15,
                1,
                "WATCH_DEBOUNCE_SECONDS",
            )?,
            watch_followup_seconds: parse_u64(
                src.get("WATCH_FOLLOWUP_SECONDS"),
                45,
                0,
                "WATCH_FOLLOWUP_SECONDS",
            )?,
            watch_extensions: parse_watch_extensions(src.get("WATCH_EXTENSIONS")),
            verify_ssl: parse_bool(src.get("DSM_VERIFY_SSL"), true),
            mark_broken_on_error: parse_bool(src.get("MARK_BROKEN_ON_ERROR"), false),
            download_fallback: parse_bool(src.get("ALLOW_DOWNLOAD_FALLBACK"), true),
            ffmpeg_bin: get_non_empty(src, "FFMPEG_BIN", "ffmpeg"),
            ffprobe_bin: get_non_empty(src, "FFPROBE_BIN", "ffprobe"),
            heif_convert_bin: get_non_empty(src, "HEIF_CONVERT_BIN", "heif-convert"),
            log_level: get_non_empty(src, "LOG_LEVEL", "INFO").to_ascii_uppercase(),
            device_name: get_non_empty(src, "DSM_DEVICE_NAME", "SynologyPhotosThumbGen"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_watch_defaults_and_extensions() {
        let s_default = Settings::from_env_map(&HashMap::new()).unwrap();
        assert!(s_default.watch_enabled);
        assert_eq!(s_default.poll_interval_seconds, 1800);
        assert_eq!(s_default.watch_debounce_seconds, 15);
        assert_eq!(s_default.watch_followup_seconds, 45);
        assert!(s_default.watch_extensions.contains("mp4"));
        assert!(s_default.watch_extensions.contains("heic"));
        assert!(!s_default.watch_extensions.contains("jpg"));
        assert!(!s_default.watch_extensions.contains("png"));

        let mut env_no_watch = HashMap::new();
        env_no_watch.insert("WATCH_ENABLED".into(), "false".into());
        let s_no_watch = Settings::from_env_map(&env_no_watch).unwrap();
        assert!(!s_no_watch.watch_enabled);
        assert_eq!(s_no_watch.poll_interval_seconds, 300);

        let mut env_override = HashMap::new();
        env_override.insert("WATCH_ENABLED".into(), "true".into());
        env_override.insert("POLL_INTERVAL_SECONDS".into(), "600".into());
        env_override.insert("WATCH_EXTENSIONS".into(), ".MP4, jpg , .HEIC".into());
        let s_override = Settings::from_env_map(&env_override).unwrap();
        assert_eq!(s_override.poll_interval_seconds, 600);
        assert_eq!(s_override.watch_extensions.len(), 3);
        assert!(s_override.watch_extensions.contains("mp4"));
        assert!(s_override.watch_extensions.contains("jpg"));
        assert!(s_override.watch_extensions.contains("heic"));
    }
}
