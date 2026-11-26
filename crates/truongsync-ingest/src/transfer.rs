use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use nom_exif::{EntryValue, Exif, ExifTag, MediaParser, MediaSource};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{is_mac_metadata_path, normalize_rel_path, AppSettings};
use walkdir::WalkDir;

use crate::template::{resolve_destination_dir, resolve_shoot_root_dir, TemplateContext};
use crate::volumes::{disk_free_bytes, is_filesystem_readonly};

static PART_SEQ: AtomicU64 = AtomicU64::new(1);

/// Hardcoded video extensions that must NEVER be misclassified as photos even if settings are misconfigured.
pub const HARDCODED_VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "m4v", "crm", "mxf", "mts", "m2ts", "avi", "mkv", "3gp", "wmv", "webm", "mpeg",
    "mpg",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Raw,
    Jpeg,
    Video,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedMediaFile {
    pub path: String,
    pub filename: String,
    pub kind: MediaKind,
    pub size_bytes: u64,
    pub captured_date: String,
    pub captured_at_utc: Option<String>,
    pub suspicious_zero_bytes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateGroupSummary {
    pub date: String,
    pub raw_count: usize,
    pub jpeg_count: usize,
    pub video_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    pub source_readable: bool,
    pub source_writable: bool,
    pub dest_free_bytes: Option<u64>,
    pub required_bytes: u64,
    pub safety_buffer_bytes: u64,
    pub space_ok: bool,
    pub zero_byte_files: Vec<String>,
    pub unreadable_files: Vec<String>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardScanResult {
    pub source_root: String,
    pub files: Vec<ScannedMediaFile>,
    pub date_groups: Vec<DateGroupSummary>,
    pub raw_count: usize,
    pub jpeg_count: usize,
    pub video_count: usize,
    pub total_bytes: u64,
    pub earliest_utc: Option<String>,
    pub latest_utc: Option<String>,
    pub ignored_mac_metadata_count: usize,
    pub zero_byte_files: Vec<String>,
    pub unreadable_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShootAssignment {
    /// If `dates` is empty, matches all scanned files.
    pub dates: Vec<String>,
    pub shoot_name: String,
    pub category: String,
    /// `YYYY-MM-DD` override for folder date (defaults to first date in `dates`).
    pub folder_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportPlanItem {
    pub source_path: String,
    pub dest_path: String,
    pub shoot_root: String,
    pub kind: MediaKind,
    pub size_bytes: u64,
    pub captured_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportExecutionResult {
    pub files_total: usize,
    pub files_moved: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub shoot_roots: Vec<String>,
    pub dest_photo_paths: Vec<String>,
    /// Subset of `dest_photo_paths` that already existed at destination (`SkippedDuplicate`)
    /// so inline geotagging can record `intentional_updates` for safe Drive backfill replacement.
    pub skipped_duplicate_photo_paths: Vec<String>,
    pub failures: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgress {
    pub files_total: usize,
    pub files_done: usize,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub speed_bps: u64,
    pub current_file: String,
}

pub fn classify_extension(path: &Path, settings: &AppSettings) -> Option<MediaKind> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    // Always check video extensions FIRST so misconfigured settings can never classify a video as a photo.
    if HARDCODED_VIDEO_EXTS.contains(&ext.as_str())
        || settings
            .video_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&ext))
    {
        Some(MediaKind::Video)
    } else if settings
        .raw_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(&ext))
    {
        Some(MediaKind::Raw)
    } else if settings
        .jpeg_extensions
        .iter()
        .any(|e| e.eq_ignore_ascii_case(&ext))
    {
        Some(MediaKind::Jpeg)
    } else {
        None
    }
}

fn parse_fixed_offset(raw: &str) -> Option<FixedOffset> {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if trimmed.eq_ignore_ascii_case("Z") {
        return FixedOffset::east_opt(0);
    }
    trimmed.parse::<FixedOffset>().ok()
}

/// Converts a naive local timestamp to `DateTime<Utc>` without jumping to UTC during DST spring-forward gaps.
pub fn resolve_local_naive_to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
    if let Some(local_dt) = Local.from_local_datetime(&naive).earliest() {
        return local_dt.with_timezone(&Utc);
    }
    // Spring-forward gap: use offset 1 hour earlier (never fall back to raw UTC!)
    let earlier = naive - chrono::Duration::hours(1);
    if let Some(prev_local) = Local.from_local_datetime(&earlier).earliest() {
        let off = *prev_local.offset();
        if let Some(fixed) = naive.and_local_timezone(off).earliest() {
            return fixed.with_timezone(&Utc);
        }
    }
    let now_off = *Local::now().offset();
    naive
        .and_local_timezone(now_off)
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

fn read_capture_timestamp(path: &Path, kind: MediaKind) -> (NaiveDate, Option<DateTime<Utc>>) {
    if matches!(kind, MediaKind::Raw | MediaKind::Jpeg) {
        if let Some((camera_date, dt_utc)) = read_exif_capture_time(path) {
            return (camera_date, Some(dt_utc));
        }
    } else if matches!(kind, MediaKind::Video) {
        if let Some((video_date, dt_utc)) = read_video_capture_time(path) {
            return (video_date, Some(dt_utc));
        }
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(mtime) = meta.modified() {
            let dt_utc: DateTime<Utc> = mtime.into();
            return (dt_utc.with_timezone(&Local).date_naive(), Some(dt_utc));
        }
    }
    let now = Local::now();
    (now.date_naive(), Some(now.with_timezone(&Utc)))
}

fn read_video_capture_time(path: &Path) -> Option<(NaiveDate, DateTime<Utc>)> {
    let mut parser = MediaParser::new();
    let ms = MediaSource::open(path).ok()?;
    let info: nom_exif::TrackInfo = parser.parse_track(ms).ok()?;
    let val = info.get(nom_exif::TrackInfoTag::CreateDate)?;
    match val {
        EntryValue::DateTime(dt) => Some((dt.date_naive(), dt.with_timezone(&Utc))),
        EntryValue::NaiveDateTime(naive) => {
            Some((naive.date(), resolve_local_naive_to_utc(*naive)))
        }
        _ => None,
    }
}

fn read_exif_capture_time(path: &Path) -> Option<(NaiveDate, DateTime<Utc>)> {
    let mut parser = MediaParser::new();
    let ms = MediaSource::open(path).ok()?;
    let iter = parser.parse_exif(ms).ok()?;
    let exif: Exif = iter.into();

    let offset_for = |tag: ExifTag| -> Option<FixedOffset> {
        let candidates: &[ExifTag] = match tag {
            ExifTag::DateTimeOriginal => {
                &[ExifTag::OffsetTimeOriginal, ExifTag::OffsetTimeDigitized]
            }
            ExifTag::CreateDate => &[ExifTag::OffsetTimeDigitized, ExifTag::OffsetTimeOriginal],
            _ => &[
                ExifTag::OffsetTime,
                ExifTag::OffsetTimeOriginal,
                ExifTag::OffsetTimeDigitized,
            ],
        };
        candidates.iter().find_map(|&ot| match exif.get(ot) {
            Some(EntryValue::Text(s)) => parse_fixed_offset(s),
            _ => None,
        })
    };

    for tag in [
        ExifTag::DateTimeOriginal,
        ExifTag::CreateDate,
        ExifTag::ModifyDate,
    ] {
        if let Some(val) = exif.get(tag) {
            let explicit_offset = offset_for(tag);
            match val {
                // Preserve the camera's local date (not host timezone date!) so travel shoots never split across days
                EntryValue::DateTime(dt) => return Some((dt.date_naive(), dt.with_timezone(&Utc))),
                EntryValue::NaiveDateTime(naive) => {
                    if let Some(off) = explicit_offset {
                        if let Some(fixed) = naive.and_local_timezone(off).single() {
                            return Some((fixed.date_naive(), fixed.with_timezone(&Utc)));
                        }
                    }
                    return Some((naive.date(), resolve_local_naive_to_utc(*naive)));
                }
                _ => {
                    let s = val.to_string();
                    let clean = s
                        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
                        .trim_matches('"');
                    if let Ok(dt) = DateTime::parse_from_rfc3339(clean) {
                        return Some((dt.date_naive(), dt.with_timezone(&Utc)));
                    }
                    if let Ok(naive) = NaiveDateTime::parse_from_str(clean, "%Y:%m:%d %H:%M:%S")
                        .or_else(|_| NaiveDateTime::parse_from_str(clean, "%Y-%m-%d %H:%M:%S"))
                    {
                        if let Some(off) = explicit_offset {
                            if let Some(fixed) = naive.and_local_timezone(off).single() {
                                return Some((fixed.date_naive(), fixed.with_timezone(&Utc)));
                            }
                        }
                        return Some((naive.date(), resolve_local_naive_to_utc(naive)));
                    }
                }
            }
        }
    }
    None
}

/// Scans an SD card or source folder in parallel, discovering media across ALL camera roots
/// (`DCIM`, Sony `PRIVATE/M4ROOT`, Panasonic `PRIVATE/AVCHD`, Canon `CONTENTS`, etc.) while
/// excluding OS junk/trash directories and surfacing any unreadable directories/files.
pub fn scan_media_source(source: &Path, settings: &AppSettings) -> Result<CardScanResult> {
    if !source.exists() {
        return Err(anyhow!("Source path does not exist: {}", source.display()));
    }
    // Canonicalize ONLY the root so symlinked source directories scan cleanly while child symlinks stay ignored
    let walk_root = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());

    let mut candidate_paths = Vec::new();
    let mut ignored_mac = 0usize;
    let mut walk_errors = Vec::new();

    for entry_res in WalkDir::new(&walk_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !is_mac_metadata_path(&name)
        })
    {
        match entry_res {
            Err(err) => {
                let p = err
                    .path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| source.to_string_lossy().to_string());
                walk_errors.push(format!("{p}: {err}"));
            }
            Ok(entry) => {
                if !entry.file_type().is_file() {
                    continue;
                }
                let p = entry.path();
                let rel = p
                    .strip_prefix(&walk_root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string();
                if is_mac_metadata_path(&rel) {
                    ignored_mac += 1;
                    continue;
                }
                if let Some(kind) = classify_extension(p, settings) {
                    candidate_paths.push((p.to_path_buf(), kind));
                }
            }
        }
    }

    // Count top-level skipped macOS metadata files inside scanned directories for display transparency
    let unreadable: Mutex<Vec<String>> = Mutex::new(walk_errors);
    let scanned: Vec<ScannedMediaFile> = candidate_paths
        .par_iter()
        .filter_map(|(path, kind)| {
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => {
                    unreadable.lock().push(path.to_string_lossy().to_string());
                    return None;
                }
            };
            if File::open(path).is_err() {
                unreadable.lock().push(path.to_string_lossy().to_string());
                return None;
            }
            let size = meta.len();
            let (date, dt_utc) = read_capture_timestamp(path, *kind);
            Some(ScannedMediaFile {
                path: path.to_string_lossy().to_string(),
                filename: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                kind: *kind,
                size_bytes: size,
                captured_date: date.format("%Y-%m-%d").to_string(),
                captured_at_utc: dt_utc.map(|t| t.to_rfc3339()),
                suspicious_zero_bytes: size == 0,
            })
        })
        .collect();

    let mut groups: BTreeMap<String, DateGroupSummary> = BTreeMap::new();
    let mut raw_count = 0usize;
    let mut jpeg_count = 0usize;
    let mut video_count = 0usize;
    let mut total_bytes = 0u64;
    let mut earliest: Option<String> = None;
    let mut latest: Option<String> = None;
    let mut zero_byte_files = Vec::new();

    for f in &scanned {
        total_bytes = total_bytes.saturating_add(f.size_bytes);
        if f.suspicious_zero_bytes {
            zero_byte_files.push(f.path.clone());
        }
        match f.kind {
            MediaKind::Raw => raw_count += 1,
            MediaKind::Jpeg => jpeg_count += 1,
            MediaKind::Video => video_count += 1,
        }
        if let Some(ref ts) = f.captured_at_utc {
            if earliest.as_ref().is_none_or(|e| ts < e) {
                earliest = Some(ts.clone());
            }
            if latest.as_ref().is_none_or(|l| ts > l) {
                latest = Some(ts.clone());
            }
        }
        let g = groups
            .entry(f.captured_date.clone())
            .or_insert_with(|| DateGroupSummary {
                date: f.captured_date.clone(),
                raw_count: 0,
                jpeg_count: 0,
                video_count: 0,
                total_bytes: 0,
            });
        g.total_bytes = g.total_bytes.saturating_add(f.size_bytes);
        match f.kind {
            MediaKind::Raw => g.raw_count += 1,
            MediaKind::Jpeg => g.jpeg_count += 1,
            MediaKind::Video => g.video_count += 1,
        }
    }

    Ok(CardScanResult {
        source_root: source.to_string_lossy().to_string(),
        files: scanned,
        date_groups: groups.into_values().collect(),
        raw_count,
        jpeg_count,
        video_count,
        total_bytes,
        earliest_utc: earliest,
        latest_utc: latest,
        ignored_mac_metadata_count: ignored_mac,
        zero_byte_files,
        unreadable_files: unreadable.into_inner(),
    })
}

/// Runs pre-flight disk space and SD card health/write-protect checks before moving/copying.
/// In Copy mode, never writes a probe file onto the SD card. On 100%-full cards, `ENOSPC`
/// still allows moving/unlinking files off the card cleanly.
pub fn run_preflight_checks(
    scan: &CardScanResult,
    dest_root: &Path,
    is_move: bool,
) -> PreflightReport {
    let source = Path::new(&scan.source_root);
    let source_readable = source.exists() && std::fs::read_dir(source).is_ok();
    let source_writable = if !source_readable {
        false
    } else if !is_move {
        !is_filesystem_readonly(source).unwrap_or(false)
    } else {
        check_source_writable_for_move(source)
    };

    let required_bytes = scan.total_bytes;
    let safety_buffer_bytes = (required_bytes / 20).clamp(1_073_741_824, 10_737_418_240);
    let dest_trimmed = dest_root.to_string_lossy().trim().to_string();
    let dest_path = PathBuf::from(&dest_trimmed);
    let dest_free_res = if !dest_trimmed.is_empty() && dest_path.is_absolute() {
        Some(disk_free_bytes(&dest_path))
    } else {
        None
    };

    let mut warnings = Vec::new();
    let mut blockers = Vec::new();

    if !source_readable {
        blockers.push("Source folder or SD card is not readable.".into());
    }
    if is_move && !source_writable {
        blockers.push(
            "SD card appears write-protected (read-only). Unlock the physical SD switch or switch to Copy mode."
                .into(),
        );
    }
    if !dest_trimmed.is_empty() && !dest_path.is_absolute() {
        blockers.push("Destination library root folder must be an absolute path.".into());
    } else if !dest_trimmed.is_empty() && is_filesystem_readonly(&dest_path) == Some(true) {
        blockers.push(
            "Destination library drive is mounted read-only (for example, an NTFS drive on macOS). Choose a writable drive."
                .into(),
        );
    }

    let (dest_free_bytes, space_ok) = match dest_free_res {
        Some(Ok(free)) => {
            if free < required_bytes {
                blockers.push(format!(
                    "Insufficient destination disk space: need {:.2} GB, only {:.2} GB free.",
                    required_bytes as f64 / 1e9,
                    free as f64 / 1e9
                ));
                (Some(free), false)
            } else if free < required_bytes.saturating_add(safety_buffer_bytes) {
                warnings.push(format!(
                    "Low destination headroom after import ({:.2} GB will remain free).",
                    free.saturating_sub(required_bytes) as f64 / 1e9
                ));
                (Some(free), true)
            } else {
                (Some(free), true)
            }
        }
        Some(Err(err)) => {
            blockers.push(format!("Destination unavailable: {err}"));
            (None, false)
        }
        None => {
            blockers.push("Choose a destination library root folder first.".into());
            (None, false)
        }
    };

    if !scan.zero_byte_files.is_empty() {
        warnings.push(format!(
            "{} file(s) on the card are 0 bytes (possible interrupted write or card issue).",
            scan.zero_byte_files.len()
        ));
    }
    if !scan.unreadable_files.is_empty() {
        blockers.push(format!(
            "{} file(s) or folders could not be read from the card (possible bad sectors).",
            scan.unreadable_files.len()
        ));
    }

    PreflightReport {
        source_readable,
        source_writable,
        dest_free_bytes,
        required_bytes,
        safety_buffer_bytes,
        space_ok,
        zero_byte_files: scan.zero_byte_files.clone(),
        unreadable_files: scan.unreadable_files.clone(),
        warnings,
        blockers,
    }
}

fn check_source_writable_for_move(dir: &Path) -> bool {
    if is_filesystem_readonly(dir) == Some(true) {
        return false;
    }
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let probe = dir.join(format!(".truongsync_probe_{}_{}", std::process::id(), seq));
    // Create a 0-byte file (no write_all) and treat StorageFull as writable so 100%-full SD cards work!
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) if e.kind() == ErrorKind::StorageFull => true,
        Err(_) => false,
    }
}

/// Builds the full dry-run import plan mapping each scanned file to its final destination path.
#[allow(clippy::too_many_arguments)]
pub fn build_import_plan(
    scan: &CardScanResult,
    dest_root: &Path,
    template: &str,
    date_format: &str,
    raw_folder: &str,
    jpeg_folder: &str,
    video_folder: &str,
    assignments: &[ShootAssignment],
) -> Result<Vec<ImportPlanItem>> {
    let dest_trimmed = dest_root.to_string_lossy().trim().to_string();
    if dest_trimmed.is_empty() {
        return Err(anyhow!("Destination library root folder cannot be empty."));
    }
    let dest_root = Path::new(&dest_trimmed);
    if !dest_root.is_absolute() {
        return Err(anyhow!(
            "Destination library root folder must be an absolute path."
        ));
    }
    if assignments.is_empty() {
        return Err(anyhow!("At least one shoot assignment is required."));
    }
    // Validate no duplicate/overlapping date assignments
    let has_catch_all = assignments.iter().any(|a| a.dates.is_empty());
    if has_catch_all && assignments.len() > 1 {
        return Err(anyhow!(
            "Cannot combine an all-dates assignment with date-specific split assignments."
        ));
    }
    let mut seen_dates = HashSet::new();
    for a in assignments {
        for d in &a.dates {
            let trimmed = d.trim();
            if !seen_dates.insert(trimmed) {
                return Err(anyhow!(
                    "Date {trimmed} is assigned to more than one shoot."
                ));
            }
        }
    }

    let first_scan_date = scan.date_groups.first().map(|g| g.date.as_str());
    let mut items = Vec::with_capacity(scan.files.len());
    for f in &scan.files {
        let assignment = assignments
            .iter()
            .find(|a| a.dates.is_empty() || a.dates.iter().any(|d| d.trim() == f.captured_date));
        let Some(a) = assignment else {
            continue;
        };
        let fallback_date_str = if a.dates.is_empty() {
            first_scan_date.unwrap_or(f.captured_date.as_str())
        } else {
            a.dates
                .iter()
                .map(|d| d.trim())
                .min()
                .unwrap_or(f.captured_date.as_str())
        };
        let folder_date = NaiveDate::parse_from_str(a.folder_date.trim(), "%Y-%m-%d")
            .or_else(|_| NaiveDate::parse_from_str(fallback_date_str, "%Y-%m-%d"))
            .unwrap_or_else(|_| Local::now().date_naive());
        let safe_filename = crate::template::sanitize_filename(&f.filename)
            .ok_or_else(|| anyhow!("Invalid media filename '{}'", f.filename))?;
        let type_folder = match f.kind {
            MediaKind::Raw => raw_folder,
            MediaKind::Jpeg => jpeg_folder,
            MediaKind::Video => video_folder,
        };
        let ctx = TemplateContext {
            root: dest_root,
            template,
            date: folder_date,
            date_format,
            category: &a.category,
            shoot_name: &a.shoot_name,
            type_folder,
        };
        let dest_dir = resolve_destination_dir(&ctx);
        let shoot_root = resolve_shoot_root_dir(&ctx);
        items.push(ImportPlanItem {
            source_path: f.path.clone(),
            dest_path: dest_dir.join(&safe_filename).to_string_lossy().to_string(),
            shoot_root: shoot_root.to_string_lossy().to_string(),
            kind: f.kind,
            size_bytes: f.size_bytes,
            captured_date: f.captured_date.clone(),
        });
    }
    Ok(items)
}

/// Executes a multithreaded two-phase verified move (or copy) across `plan`.
/// Uses per-transfer unique `.truongsync.part` files with RAII cleanup, shared destination
/// reservation across parallel workers, non-overwriting atomic placement (`hard_link` + unlink
/// on Unix, `MoveFileExW` without replace on Windows), parent-dir `fsync` before SD deletion,
/// and strict `same_file` protection.
pub fn execute_import_plan<F>(
    plan: &[ImportPlanItem],
    is_move: bool,
    threads: usize,
    deep_verify: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<ImportExecutionResult>
where
    F: Fn(TransferProgress) + Send + Sync,
{
    let started = Instant::now();
    let files_total = plan.len();
    let bytes_total: u64 = plan.iter().map(|p| p.size_bytes).sum();

    let unique_dirs: HashSet<PathBuf> = plan
        .iter()
        .filter_map(|p| Path::new(&p.dest_path).parent().map(Path::to_path_buf))
        .collect();
    for d in unique_dirs {
        std::fs::create_dir_all(&d)
            .with_context(|| format!("failed to create destination folder {}", d.display()))?;
    }

    let files_done = Arc::new(AtomicUsize::new(0));
    let files_moved = Arc::new(AtomicUsize::new(0));
    let files_skipped = Arc::new(AtomicUsize::new(0));
    let files_failed = Arc::new(AtomicUsize::new(0));
    let bytes_done = Arc::new(AtomicU64::new(0));
    let failures: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let dest_photos: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let dup_photos: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let shoot_roots_set: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let reserved_dests: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let last_emit = Arc::new(Mutex::new(Instant::now()));

    let worker_count = threads.clamp(1, 16);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .thread_name(|i| format!("ts-ingest-{i}"))
        .build()
        .context("failed to build ingest thread pool")?;

    pool.install(|| {
        plan.par_iter().for_each(|item| {
            if cancel.is_cancelled() {
                return;
            }
            let src = Path::new(&item.source_path);
            let dst = Path::new(&item.dest_path);

            match transfer_single_file(src, dst, is_move, deep_verify, cancel, &reserved_dests) {
                Ok(SingleOutcome::Transferred(final_dst, nbytes)) => {
                    shoot_roots_set.lock().insert(item.shoot_root.clone());
                    files_moved.fetch_add(1, Ordering::Relaxed);
                    let bd = bytes_done.fetch_add(nbytes, Ordering::Relaxed) + nbytes;
                    let fd = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                    if matches!(item.kind, MediaKind::Raw | MediaKind::Jpeg) {
                        dest_photos
                            .lock()
                            .push(final_dst.to_string_lossy().to_string());
                    }
                    emit_throttled(
                        &last_emit,
                        &on_progress,
                        started,
                        files_total,
                        fd,
                        bytes_total,
                        bd,
                        &item.source_path,
                    );
                }
                Ok(SingleOutcome::TransferredSourceKept {
                    dest: final_dst,
                    bytes: nbytes,
                    reason,
                }) => {
                    shoot_roots_set.lock().insert(item.shoot_root.clone());
                    files_moved.fetch_add(1, Ordering::Relaxed);
                    files_failed.fetch_add(1, Ordering::Relaxed);
                    let bd = bytes_done.fetch_add(nbytes, Ordering::Relaxed) + nbytes;
                    let fd = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                    if matches!(item.kind, MediaKind::Raw | MediaKind::Jpeg) {
                        dest_photos
                            .lock()
                            .push(final_dst.to_string_lossy().to_string());
                    }
                    failures.lock().push((item.source_path.clone(), reason));
                    emit_throttled(
                        &last_emit,
                        &on_progress,
                        started,
                        files_total,
                        fd,
                        bytes_total,
                        bd,
                        &item.source_path,
                    );
                }
                Ok(SingleOutcome::SkippedDuplicate(final_dst)) => {
                    shoot_roots_set.lock().insert(item.shoot_root.clone());
                    files_skipped.fetch_add(1, Ordering::Relaxed);
                    let bd = bytes_done.load(Ordering::Relaxed);
                    let fd = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                    if matches!(item.kind, MediaKind::Raw | MediaKind::Jpeg) {
                        let p = final_dst.to_string_lossy().to_string();
                        dest_photos.lock().push(p.clone());
                        dup_photos.lock().push(p);
                    }
                    emit_throttled(
                        &last_emit,
                        &on_progress,
                        started,
                        files_total,
                        fd,
                        bytes_total,
                        bd,
                        &item.source_path,
                    );
                }
                Ok(SingleOutcome::Cancelled) => {}
                Err(err) => {
                    files_failed.fetch_add(1, Ordering::Relaxed);
                    let fd = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                    failures
                        .lock()
                        .push((item.source_path.clone(), err.to_string()));
                    emit_throttled(
                        &last_emit,
                        &on_progress,
                        started,
                        files_total,
                        fd,
                        bytes_total,
                        bytes_done.load(Ordering::Relaxed),
                        &item.source_path,
                    );
                }
            }
        });
    });

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let total_bd = bytes_done.load(Ordering::Relaxed);
    on_progress(TransferProgress {
        files_total,
        files_done: files_done.load(Ordering::Relaxed),
        bytes_total,
        bytes_done: total_bd,
        speed_bps: if elapsed_ms > 0 {
            total_bd.saturating_mul(1000) / elapsed_ms
        } else {
            0
        },
        current_file: String::new(),
    });

    let mut shoot_roots: Vec<String> = shoot_roots_set.lock().iter().cloned().collect();
    shoot_roots.sort();
    let mut dest_seen = HashSet::new();
    let dest_photo_paths: Vec<String> = dest_photos
        .lock()
        .iter()
        .filter(|p| dest_seen.insert((*p).clone()))
        .cloned()
        .collect();
    let mut dup_seen = HashSet::new();
    let skipped_duplicate_photo_paths: Vec<String> = dup_photos
        .lock()
        .iter()
        .filter(|p| dup_seen.insert((*p).clone()))
        .cloned()
        .collect();
    let final_failures = failures.lock().clone();

    Ok(ImportExecutionResult {
        files_total,
        files_moved: files_moved.load(Ordering::Relaxed),
        files_skipped: files_skipped.load(Ordering::Relaxed),
        files_failed: files_failed.load(Ordering::Relaxed),
        bytes_transferred: total_bd,
        duration_ms: elapsed_ms,
        shoot_roots,
        dest_photo_paths,
        skipped_duplicate_photo_paths,
        failures: final_failures,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_throttled<F>(
    last_emit: &Mutex<Instant>,
    on_progress: &F,
    started: Instant,
    files_total: usize,
    files_done: usize,
    bytes_total: u64,
    bytes_done: u64,
    current_file: &str,
) where
    F: Fn(TransferProgress),
{
    let mut guard = last_emit.lock();
    if files_done < files_total && guard.elapsed().as_millis() < 80 {
        return;
    }
    *guard = Instant::now();
    let elapsed_ms = started.elapsed().as_millis().max(1) as u64;
    on_progress(TransferProgress {
        files_total,
        files_done,
        bytes_total,
        bytes_done,
        speed_bps: bytes_done.saturating_mul(1000) / elapsed_ms,
        current_file: current_file.to_string(),
    });
}

enum SingleOutcome {
    Transferred(PathBuf, u64),
    /// File was verified and placed safely at destination, but source could not be removed (e.g. camera-locked photo).
    TransferredSourceKept {
        dest: PathBuf,
        bytes: u64,
        reason: String,
    },
    SkippedDuplicate(PathBuf),
    Cancelled,
}

struct PartCleanup {
    path: PathBuf,
    armed: bool,
}

impl PartCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn is_same_file_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        if ca == cb {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    false
}

#[cfg(unix)]
static EXFAT_RENAME_LOCK: Mutex<()> = Mutex::new(());

fn hash_file_blake3(
    path: &Path,
    cancel: &CancellationToken,
    bypass_cache: bool,
) -> Result<Option<blake3::Hash>> {
    let mut file = File::open(path)?;
    #[cfg(target_os = "macos")]
    if bypass_cache {
        use std::os::unix::io::AsRawFd;
        unsafe {
            let _ = libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = bypass_cache;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(hasher.finalize()))
}

fn normalize_reservation_key(p: &Path) -> String {
    normalize_rel_path(&p.to_string_lossy()).to_lowercase()
}

fn reserve_unique_dest(desired: &Path, reserved: &Mutex<HashSet<String>>) -> Result<PathBuf> {
    let mut guard = reserved.lock();
    let desired_key = normalize_reservation_key(desired);
    if !desired.exists() && !guard.contains(&desired_key) {
        guard.insert(desired_key);
        return Ok(desired.to_path_buf());
    }
    let stem = desired
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = desired.extension().map(|e| e.to_string_lossy().to_string());
    let parent = desired.parent().unwrap_or_else(|| Path::new("."));
    for idx in 1..100_000u32 {
        let name = match &ext {
            Some(e) if !e.is_empty() => format!("{stem}_{idx}.{e}"),
            _ => format!("{stem}_{idx}"),
        };
        let cand = parent.join(name);
        let key = normalize_reservation_key(&cand);
        if !cand.exists() && !guard.contains(&key) {
            guard.insert(key);
            return Ok(cand);
        }
    }
    Err(anyhow!(
        "Too many filename collisions (>99,999) for {}",
        desired.display()
    ))
}

fn make_unique_part_path(final_dst: &Path) -> PathBuf {
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let parent = final_dst.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".ts.{}.{}.truongsync.part",
        std::process::id(),
        seq
    ))
}

/// Atomically places `part_path` at `final_dst` WITHOUT overwriting an existing file.
/// Returns `Ok(true)` if placed, `Ok(false)` if `final_dst` already existed (caller should retry with a new name).
fn atomic_place_no_overwrite(part_path: &Path, final_dst: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let src_c =
            CString::new(part_path.as_os_str().as_bytes()).context("invalid NUL in source path")?;
        let dst_c = CString::new(final_dst.as_os_str().as_bytes())
            .context("invalid NUL in destination path")?;
        let rc = unsafe { libc::renamex_np(src_c.as_ptr(), dst_c.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::AlreadyExists {
            return Ok(false);
        }
        let raw = err.raw_os_error().unwrap_or(0);
        if raw != libc::ENOTSUP && raw != libc::EOPNOTSUPP && raw != libc::EINVAL {
            return Err(err.into());
        }
    }
    #[cfg(unix)]
    {
        match std::fs::hard_link(part_path, final_dst) {
            Ok(()) => {
                let _ = std::fs::remove_file(part_path);
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(e) => {
                let raw = e.raw_os_error().unwrap_or(0);
                // Only fall back to checked rename when the filesystem lacks hardlink support (exFAT/FAT32/SMB)
                if raw == libc::ENOTSUP
                    || raw == libc::EOPNOTSUPP
                    || raw == libc::ENOSYS
                    || raw == libc::EINVAL
                    || raw == libc::EPERM
                {
                    let _rename_guard = EXFAT_RENAME_LOCK.lock();
                    if final_dst.symlink_metadata().is_ok() {
                        return Ok(false);
                    }
                    std::fs::rename(part_path, final_dst)?;
                    Ok(true)
                } else {
                    Err(e.into())
                }
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let src_w: Vec<u16> = part_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dst_w: Vec<u16> = final_dst
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // Omit MOVEFILE_REPLACE_EXISTING so existing files are NEVER overwritten; include WRITE_THROUGH for durability.
        match unsafe {
            MoveFileExW(
                PCWSTR(src_w.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        } {
            Ok(()) => Ok(true),
            Err(e)
                if e.code() == ERROR_ALREADY_EXISTS.to_hresult()
                    || e.code() == ERROR_FILE_EXISTS.to_hresult() =>
            {
                Ok(false)
            }
            Err(e) => Err(anyhow!(
                "MoveFileExW failed {} -> {}: {e}",
                part_path.display(),
                final_dst.display()
            )),
        }
    }
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = File::open(parent).with_context(|| {
            format!(
                "failed to open destination directory {} for durability sync",
                parent.display()
            )
        })?;
        if let Err(err) = dir.sync_all() {
            let raw = err.raw_os_error().unwrap_or(0);
            if raw != libc::EINVAL
                && raw != libc::ENOTSUP
                && raw != libc::EOPNOTSUPP
                && raw != libc::EBADF
            {
                return Err(err).with_context(|| {
                    format!(
                        "failed to flush destination directory {} before removing source",
                        parent.display()
                    )
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn transfer_single_file(
    src: &Path,
    dst: &Path,
    is_move: bool,
    deep_verify: bool,
    cancel: &CancellationToken,
    reserved_dests: &Mutex<HashSet<String>>,
) -> Result<SingleOutcome> {
    if is_same_file_path(src, dst) {
        return Ok(SingleOutcome::SkippedDuplicate(dst.to_path_buf()));
    }

    let src_meta = std::fs::metadata(src)
        .with_context(|| format!("failed to stat source {}", src.display()))?;
    let src_len = src_meta.len();
    let src_mtime = src_meta.modified().ok();

    // Check existing destination (duplicate vs collision)
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        if dst_meta.is_file() && dst_meta.len() == src_len {
            let same = if deep_verify || src_len > 0 {
                let Some(src_h) = hash_file_blake3(src, cancel, false)? else {
                    return Ok(SingleOutcome::Cancelled);
                };
                let Some(dst_h) = hash_file_blake3(dst, cancel, false)? else {
                    return Ok(SingleOutcome::Cancelled);
                };
                src_h == dst_h
            } else {
                true
            };
            if same {
                return Ok(SingleOutcome::SkippedDuplicate(dst.to_path_buf()));
            }
        }
    }

    let mut final_dst = reserve_unique_dest(dst, reserved_dests)?;
    let part_path = make_unique_part_path(&final_dst);
    let mut cleanup = PartCleanup::new(part_path.clone());

    let mut reader =
        File::open(src).with_context(|| format!("failed to open source {}", src.display()))?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part_path)
        .with_context(|| format!("failed to create part file {}", part_path.display()))?;

    let mut buf = vec![0u8; 2 * 1024 * 1024];
    let mut copied = 0u64;
    let mut src_hasher = if deep_verify {
        Some(blake3::Hasher::new())
    } else {
        None
    };

    loop {
        if cancel.is_cancelled() {
            drop(writer);
            return Ok(SingleOutcome::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        writer.write_all(&buf[..n])?;
        if let Some(ref mut h) = src_hasher {
            h.update(&buf[..n]);
        }
        copied += n as u64;
    }

    // Flush data BEFORE set_modified so Windows/exFAT/SMB writeback does not clobber preserved mtime
    writer.sync_all()?;
    if let Some(mtime) = src_mtime {
        let _ = writer.set_modified(mtime);
        let _ = writer.sync_all();
    }
    drop(writer);
    drop(reader);

    if copied != src_len {
        return Err(anyhow!(
            "byte count mismatch for {}: copied {copied}, expected {src_len}",
            src.display()
        ));
    }

    if let Some(expected_hash) = src_hasher.map(|h| h.finalize()) {
        let Some(actual_hash) = hash_file_blake3(&part_path, cancel, true)? else {
            return Ok(SingleOutcome::Cancelled);
        };
        if actual_hash != expected_hash {
            return Err(anyhow!(
                "blake3 checksum mismatch on destination for {}",
                src.display()
            ));
        }
    }

    // Non-overwriting atomic placement (retries with a fresh unique name if another process raced)
    for _ in 0..32 {
        if atomic_place_no_overwrite(&part_path, &final_dst)? {
            cleanup.disarm();
            break;
        }
        final_dst = reserve_unique_dest(dst, reserved_dests)?;
    }
    if cleanup.armed {
        return Err(anyhow!(
            "could not place {} without colliding with existing destination files",
            src.display()
        ));
    }

    // Verify final destination exists with exact expected size before ever removing source!
    let verified_len = std::fs::metadata(&final_dst)?.len();
    if verified_len != src_len {
        return Err(anyhow!(
            "post-rename verification failed for {}",
            final_dst.display()
        ));
    }

    if is_move {
        sync_parent_dir(&final_dst)?;
        if is_same_file_path(src, &final_dst) {
            return Err(anyhow!(
                "refusing to delete source because source and destination resolve to the same file"
            ));
        }
        if let Err(err) = std::fs::remove_file(src) {
            return Ok(SingleOutcome::TransferredSourceKept {
                dest: final_dst.clone(),
                bytes: copied,
                reason: format!(
                    "File safely copied to {}, but source could not be removed (possibly camera-protected/locked): {err}",
                    final_dst.display()
                ),
            });
        }
    }

    Ok(SingleOutcome::Transferred(final_dst, copied))
}

pub fn set_file_mtime(path: &Path, mtime: SystemTime) -> Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_modified(mtime)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn multithreaded_verified_move_handles_same_filename_across_subfolders_and_sony_clips() {
        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();
        let dcim1 = src_dir.path().join("DCIM/100EOSR7");
        let dcim2 = src_dir.path().join("DCIM/101EOSR7");
        let sony_clips = src_dir.path().join("PRIVATE/M4ROOT/CLIP");
        std::fs::create_dir_all(&dcim1).unwrap();
        std::fs::create_dir_all(&dcim2).unwrap();
        std::fs::create_dir_all(&sony_clips).unwrap();

        // Duplicate filename across two camera folders with DIFFERENT content!
        std::fs::write(dcim1.join("IMG_0001.CR3"), b"raw-folder-1-data-aaa").unwrap();
        std::fs::write(dcim2.join("IMG_0001.CR3"), b"raw-folder-2-data-bbbbbb").unwrap();
        std::fs::write(dcim1.join("IMG_0001.JPG"), b"jpg-data-1234").unwrap();
        // Sony video outside DCIM/
        std::fs::write(sony_clips.join("C0001.MP4"), b"sony-xavc-video-payload").unwrap();
        // OS junk must be ignored
        std::fs::write(dcim1.join("._IMG_0001.CR3"), b"appledouble").unwrap();
        std::fs::write(dcim1.join(".DS_Store"), b"finder").unwrap();
        std::fs::write(dcim1.join("MISC.CTG"), b"canon-ctg").unwrap();

        let settings = AppSettings::default();
        let scan = scan_media_source(src_dir.path(), &settings).unwrap();
        assert_eq!(scan.raw_count, 2);
        assert_eq!(scan.jpeg_count, 1);
        assert_eq!(scan.video_count, 1); // Found Sony clip outside DCIM!

        let preflight = run_preflight_checks(&scan, dst_dir.path(), true);
        assert!(preflight.source_readable);
        assert!(preflight.source_writable);
        assert!(preflight.blockers.is_empty());

        let plan = build_import_plan(
            &scan,
            dst_dir.path(),
            "{Year}/{Category}/{ShootName} {Date}/{Type}",
            "M-dd",
            "RAW",
            "JPEG",
            "VID",
            &[ShootAssignment {
                dates: vec![],
                shoot_name: "Cone Peak".into(),
                category: "Hikes".into(),
                folder_date: "2026-04-15".into(),
            }],
        )
        .unwrap();
        assert_eq!(plan.len(), 4);

        let cancel = CancellationToken::new();
        let res = execute_import_plan(&plan, true, 4, true, &cancel, |_| {}).unwrap();
        assert_eq!(res.files_moved, 4);
        assert_eq!(res.files_failed, 0);

        // BOTH colliding IMG_0001.CR3 files must survive with distinct names (zero overwrite!)
        let raw_dir = dst_dir.path().join("2026/Hikes/Cone Peak 4-15/RAW");
        assert!(raw_dir.join("IMG_0001.CR3").exists());
        assert!(raw_dir.join("IMG_0001_1.CR3").exists());
        assert!(dst_dir
            .path()
            .join("2026/Hikes/Cone Peak 4-15/VID/C0001.MP4")
            .exists());
    }
}
