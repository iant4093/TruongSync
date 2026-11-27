use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Normalizes a relative path into OS-agnostic forward-slash NFC form so macOS and Windows
/// share identical keys in `sync_manifest` and verification diffs.
pub fn normalize_rel_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches('/')
        .nfc()
        .collect()
}

/// Returns `true` if `path` represents OS junk/trash/AppleDouble metadata that must never be
/// imported, uploaded, or mirrored across drives (while preserving `.xmp` sidecars).
pub fn is_mac_metadata_path(path: &str) -> bool {
    let norm = normalize_rel_path(path);
    for seg in norm.split('/') {
        if seg.eq_ignore_ascii_case("__MACOSX")
            || seg.eq_ignore_ascii_case(".DS_Store")
            || seg.starts_with("._")
            || seg.starts_with(".truongsync_probe_")
            || seg.eq_ignore_ascii_case(".Trash")
            || seg.eq_ignore_ascii_case(".Trashes")
            || seg.eq_ignore_ascii_case(".Spotlight-V100")
            || seg.eq_ignore_ascii_case(".DocumentRevisions-V100")
            || seg.eq_ignore_ascii_case(".fseventsd")
            || seg.eq_ignore_ascii_case(".TemporaryItems")
            || seg.eq_ignore_ascii_case(".VolumeIcon.icns")
            || seg.eq_ignore_ascii_case("$RECYCLE.BIN")
            || seg.eq_ignore_ascii_case("System Volume Information")
            || seg.eq_ignore_ascii_case("Thumbs.db")
            || seg.eq_ignore_ascii_case("desktop.ini")
        {
            return true;
        }
    }
    false
}

/// Application storage locations (SQLite DB, managed rclone config, logs).
#[derive(Debug, Clone)]
pub struct StoragePaths {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub rclone_config_path: PathBuf,
    pub logs_dir: PathBuf,
}

impl StoragePaths {
    pub fn default_data_dir() -> PathBuf {
        if let Ok(env_dir) = std::env::var("TS_DATA_DIR") {
            if !env_dir.trim().is_empty() {
                return PathBuf::from(env_dir);
            }
        }
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("TruongSync")
    }

    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        let db_path = data_dir.join("state.db");
        let rclone_config_path = data_dir.join("rclone.conf");
        let logs_dir = data_dir.join("logs");
        Self {
            data_dir,
            db_path,
            rclone_config_path,
            logs_dir,
        }
    }

    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.logs_dir)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppSettings {
    pub default_import_root: String,
    pub default_categories: Vec<String>,
    pub date_format: String,
    pub raw_folder_name: String,
    pub jpeg_folder_name: String,
    pub video_folder_name: String,
    pub folder_template: String,
    pub raw_extensions: Vec<String>,
    pub jpeg_extensions: Vec<String>,
    pub video_extensions: Vec<String>,
    pub transfer_mode: String,
    pub import_threads: usize,
    pub deep_verify_move: bool,
    pub eject_after_move: bool,
    pub watched_gpx_folder: Option<String>,
    pub default_max_gps_gap_secs: i64,
    pub overwrite_existing_gps: bool,
    pub bump_mtime_on_backfill: bool,
    pub bandwidth_limit_mbps: Option<f64>,
    pub prevent_sleep_during_transfers: bool,
    pub sync_on_battery: bool,
    pub auto_open_on_sd_insert: bool,
    pub prompt_on_backup_drive: bool,
    pub os_native_auto_launch: bool,
    pub write_external_timestamp_file: bool,
    pub rclone_path_override: Option<String>,
    pub exiftool_path_override: Option<String>,
    pub stale_warning_days: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_import_root: String::new(),
            default_categories: vec![
                "Adventures".into(),
                "Hikes".into(),
                "Portraits".into(),
                "Random".into(),
                "Sports".into(),
                "Wildlife".into(),
            ],
            date_format: "M-dd".into(),
            raw_folder_name: "RAW".into(),
            jpeg_folder_name: "JPEG".into(),
            video_folder_name: "VID".into(),
            folder_template: "{Year}/{Category}/{ShootName} {Date}/{Type}".into(),
            raw_extensions: vec![
                "cr3", "cr2", "arw", "nef", "raf", "dng", "orf", "rw2", "pef", "srw", "3fr", "iiq",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            jpeg_extensions: vec![
                "jpg", "jpeg", "heic", "heif", "hif", "png", "tif", "tiff", "webp",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            video_extensions: vec!["mp4", "mov", "crm", "mxf", "mts", "m2ts", "avi", "mkv"]
                .into_iter()
                .map(String::from)
                .collect(),
            transfer_mode: "move".into(),
            import_threads: 8,
            deep_verify_move: false,
            eject_after_move: false,
            watched_gpx_folder: None,
            default_max_gps_gap_secs: 900,
            overwrite_existing_gps: false,
            bump_mtime_on_backfill: true,
            bandwidth_limit_mbps: None,
            prevent_sleep_during_transfers: true,
            sync_on_battery: false,
            auto_open_on_sd_insert: true,
            prompt_on_backup_drive: true,
            os_native_auto_launch: false,
            write_external_timestamp_file: true,
            rclone_path_override: None,
            exiftool_path_override: None,
            stale_warning_days: 7,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportPreset {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub folder_template: String,
    pub date_format: String,
    pub raw_folder: String,
    pub jpeg_folder: String,
    pub video_folder: String,
    pub default_categories: Vec<String>,
    pub transfer_mode: String,
    pub auto_geotag: bool,
    pub auto_cloud_sync: bool,
    pub auto_external_sync: bool,
    pub eject_after: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudProfile {
    pub id: String,
    pub name: String,
    pub local_root: String,
    pub remote_name: String,
    pub remote_path: String,
    pub enabled: bool,
    /// `manual` | `startup` | `interval` | `daily`
    pub schedule_mode: String,
    /// Hours for `interval` (e.g. `"6"`) or `"HH:MM"` for `daily`
    pub schedule_value: String,
    /// `check` | `sync`
    pub schedule_action: String,
    pub last_sync_at: Option<String>,
    pub last_verify_at: Option<String>,
    pub last_status: String,
    pub last_error: Option<String>,
    pub pending_upload_count: i64,
    pub pending_download_count: i64,
    pub pending_bytes: i64,
    pub stale_after_days: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalProfile {
    pub id: String,
    pub name: String,
    pub local_root: String,
    pub volume_uuid: String,
    pub volume_label: String,
    pub relative_dest_path: String,
    /// `two_way_additive` | `push_only`
    pub direction: String,
    pub enabled: bool,
    /// `manual` | `on_connect` | `interval` | `daily`
    pub schedule_mode: String,
    pub schedule_value: String,
    /// `check` | `sync`
    pub schedule_action: String,
    pub write_marker_file: bool,
    pub last_sync_at: Option<String>,
    pub last_verify_at: Option<String>,
    pub last_status: String,
    pub last_error: Option<String>,
    pub pending_push_count: i64,
    pub pending_pull_count: i64,
    pub pending_bytes: i64,
    pub stale_after_days: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub profile_kind: String,
    pub profile_id: String,
    pub rel_path: String,
    pub size_bytes: i64,
    pub md5_hash: Option<String>,
    pub mtime_unix: i64,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentionalUpdate {
    pub id: String,
    pub abs_path: String,
    pub pre_md5: String,
    pub post_md5: String,
    pub reason: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub files_total: i64,
    pub files_done: i64,
    pub files_skipped: i64,
    pub files_failed: i64,
    pub bytes_total: i64,
    pub bytes_done: i64,
    pub summary_json: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobItemRecord {
    pub id: i64,
    pub job_id: String,
    pub stage: String,
    pub source_path: String,
    pub dest_path: Option<String>,
    pub status: String,
    pub bytes: i64,
    pub message: Option<String>,
    pub created_at: String,
}

fn normalize_abs_for_prefix(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let stripped = if raw
        .as_bytes()
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(br"\\?\UNC\"))
    {
        format!(r"\\{}", &raw[8..])
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };
    let nfc: String = stripped.replace('\\', "/").nfc().collect();
    nfc.trim_end_matches('/').to_string()
}

/// Computes canonical normalized relative path from `root` to `full_path`, handling
/// macOS `/var` -> `/private/var` symlinks, Windows `\\?\` verbatim prefixes & drive-letter casing,
/// and non-ASCII UTF-8 paths without ever panicking on byte boundaries.
pub fn relative_normalized(root: &Path, full_path: &Path) -> Option<String> {
    if let Ok(rel) = full_path.strip_prefix(root) {
        return Some(normalize_rel_path(&rel.to_string_lossy()));
    }
    if let (Ok(c_root), Ok(c_full)) = (
        std::fs::canonicalize(root),
        std::fs::canonicalize(full_path),
    ) {
        if let Ok(rel) = c_full.strip_prefix(&c_root) {
            return Some(normalize_rel_path(&rel.to_string_lossy()));
        }
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        if root.is_absolute() != full_path.is_absolute() {
            return None;
        }
        let r_trim = normalize_abs_for_prefix(root);
        let f = normalize_abs_for_prefix(full_path);
        if r_trim.is_empty() {
            return None;
        }
        if f.eq_ignore_ascii_case(&r_trim) {
            return Some(String::new());
        }
        // Check ASCII '/' boundary FIRST so slicing at `r_trim.len()` is guaranteed UTF-8 safe!
        if f.len() > r_trim.len()
            && f.as_bytes().get(r_trim.len()) == Some(&b'/')
            && f.as_bytes()[..r_trim.len()].eq_ignore_ascii_case(r_trim.as_bytes())
        {
            return Some(normalize_rel_path(&f[r_trim.len() + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_abs_handles_non_ascii_at_byte_8() {
        assert_eq!(
            normalize_abs_for_prefix(Path::new("/Users/Éric/Photos/IMG_0001.CR3")),
            "/Users/Éric/Photos/IMG_0001.CR3"
        );
        assert_eq!(
            normalize_abs_for_prefix(Path::new("/Users/写真/Photos")),
            "/Users/写真/Photos"
        );
        assert_eq!(
            normalize_abs_for_prefix(Path::new(r"\\?\UNC\server\share\Été")),
            "//server/share/Été"
        );
    }
}
