//! Tauri IPC commands for TruongSync (Import, Geotag, Cloud Sync, External Backup, Activity, Settings).

use chrono::DateTime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{
    AppSettings, CloudProfile, ExternalProfile, ImportPreset, JobItemRecord, JobRecord,
    StoragePaths,
};
use truongsync_db::DbHandle;
use truongsync_geo::exiftool::{
    check_exiftool, write_geotags_batch, GeotagExecutionResult, ToolAvailability,
};
use truongsync_geo::gpx::{find_matching_gpx_files, load_gpx_files, WatchedGpxMatch};
use truongsync_geo::matcher::{build_geotag_plan, GeotagPlan, PhotoGpsMatch};
use truongsync_ingest::template::{list_or_init_year_categories, sanitize_segment};
use truongsync_ingest::transfer::{
    build_import_plan, execute_import_plan, run_preflight_checks, scan_media_source,
    CardScanResult, ImportExecutionResult, ImportPlanItem, PreflightReport, ShootAssignment,
};
use truongsync_ingest::volumes::{eject_volume, list_mounted_volumes, DetectedVolume};
use truongsync_sync::cloud::{
    check_rclone, connect_google_drive_oauth, import_rclone_config, list_rclone_remotes,
    sync_cloud_profile, validate_rclone_remote_name, verify_cloud_profile_cancellable,
    CloudSyncResult, CloudVerifyReport,
};
use truongsync_sync::external::{
    sync_external_profile, verify_external_profile, ExternalSyncResult, ExternalVerifyReport,
};

use crate::power::{is_on_battery, SleepGuard};
use crate::tray::show_main_window;

static CANCEL_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
pub struct FollowupSpec {
    pub do_sync: bool,
    pub restore_mode: bool,
}

impl FollowupSpec {
    fn merge(&mut self, other: FollowupSpec) {
        self.do_sync |= other.do_sync;
        self.restore_mode |= other.restore_mode;
    }
}

#[derive(Debug, Clone, Copy)]
enum LockKind {
    Cloud,
    External,
}

#[derive(Default)]
struct ProfileLockTable {
    active: HashSet<String>,
    followup: HashMap<String, FollowupSpec>,
}

/// RAII guard that guarantees per-profile locks are NEVER leaked on early error or panic,
/// and automatically triggers a coalesced follow-up pass preserving exact `(do_sync, restore_mode)`
/// unless the user explicitly cancelled the run.
pub struct ProfileLockGuard {
    table: Arc<Mutex<ProfileLockTable>>,
    kind: LockKind,
    key: String,
    followup_app: Option<AppHandle>,
    suppress_followup: Arc<AtomicBool>,
}

impl ProfileLockGuard {
    pub fn suppress_followup_on_cancel(&self) {
        self.suppress_followup.store(true, Ordering::SeqCst);
    }
}

impl Drop for ProfileLockGuard {
    fn drop(&mut self) {
        let maybe_spec = {
            let mut guard = self.table.lock();
            guard.active.remove(&self.key);
            guard.followup.remove(&self.key)
        };
        if self.suppress_followup.load(Ordering::SeqCst) {
            return;
        }
        if let (Some(spec), Some(app)) = (maybe_spec, self.followup_app.take()) {
            let pid = self.key.clone();
            let kind = self.kind;
            tauri::async_runtime::spawn(async move {
                match kind {
                    LockKind::Cloud => {
                        let _ = run_locked_cloud_sync_background(
                            &app,
                            &pid,
                            spec.do_sync,
                            spec.restore_mode,
                        )
                        .await;
                    }
                    LockKind::External => {
                        let _ = run_locked_external_sync_background(
                            &app,
                            &pid,
                            spec.do_sync,
                            spec.restore_mode,
                        )
                        .await;
                    }
                }
            });
        }
    }
}

/// RAII guard registering a uniquely-keyed `CancellationToken` in `AppState::active_cancels`.
pub struct CancelRegistration {
    map: Arc<Mutex<HashMap<String, CancellationToken>>>,
    id: String,
    pub token: CancellationToken,
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        self.map.lock().remove(&self.id);
    }
}

pub struct AppState {
    pub db: DbHandle,
    pub paths: StoragePaths,
    pub initial_import_volume: Mutex<Option<String>>,
    active_cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,
    cloud_locks: Arc<Mutex<ProfileLockTable>>,
    external_locks: Arc<Mutex<ProfileLockTable>>,
}

impl AppState {
    pub fn new(db: DbHandle, paths: StoragePaths, initial_import_volume: Option<String>) -> Self {
        Self {
            db,
            paths,
            initial_import_volume: Mutex::new(initial_import_volume),
            active_cancels: Arc::new(Mutex::new(HashMap::new())),
            cloud_locks: Arc::new(Mutex::new(ProfileLockTable::default())),
            external_locks: Arc::new(Mutex::new(ProfileLockTable::default())),
        }
    }

    pub fn register_cancel(&self, prefix: &str) -> CancelRegistration {
        let seq = CANCEL_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = format!("{prefix}#{seq}");
        let token = CancellationToken::new();
        self.active_cancels.lock().insert(id.clone(), token.clone());
        CancelRegistration {
            map: Arc::clone(&self.active_cancels),
            id,
            token,
        }
    }

    pub fn register_cancel_exact(
        &self,
        job_id: String,
        token: CancellationToken,
    ) -> CancelRegistration {
        self.active_cancels
            .lock()
            .insert(job_id.clone(), token.clone());
        CancelRegistration {
            map: Arc::clone(&self.active_cancels),
            id: job_id,
            token,
        }
    }

    pub fn cancel_by_id(&self, job_id: &str) -> bool {
        let guard = self.active_cancels.lock();
        if let Some(tok) = guard.get(job_id) {
            tok.cancel();
            return true;
        }
        false
    }

    pub fn cancel_all_active(&self) -> bool {
        let guard = self.active_cancels.lock();
        if guard.is_empty() {
            return false;
        }
        for tok in guard.values() {
            tok.cancel();
        }
        true
    }

    pub fn try_lock_cloud(
        &self,
        profile_id: &str,
        app_for_followup: Option<AppHandle>,
        followup_if_busy: Option<FollowupSpec>,
    ) -> Option<ProfileLockGuard> {
        let mut guard = self.cloud_locks.lock();
        if guard.active.contains(profile_id) {
            if let Some(spec) = followup_if_busy {
                guard
                    .followup
                    .entry(profile_id.to_string())
                    .and_modify(|cur| cur.merge(spec))
                    .or_insert(spec);
            }
            return None;
        }
        guard.active.insert(profile_id.to_string());
        Some(ProfileLockGuard {
            table: Arc::clone(&self.cloud_locks),
            kind: LockKind::Cloud,
            key: profile_id.to_string(),
            followup_app: app_for_followup,
            suppress_followup: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn try_lock_external(
        &self,
        profile_id: &str,
        app_for_followup: Option<AppHandle>,
        followup_if_busy: Option<FollowupSpec>,
    ) -> Option<ProfileLockGuard> {
        let mut guard = self.external_locks.lock();
        if guard.active.contains(profile_id) {
            if let Some(spec) = followup_if_busy {
                guard
                    .followup
                    .entry(profile_id.to_string())
                    .and_modify(|cur| cur.merge(spec))
                    .or_insert(spec);
            }
            return None;
        }
        guard.active.insert(profile_id.to_string());
        Some(ProfileLockGuard {
            table: Arc::clone(&self.external_locks),
            kind: LockKind::External,
            key: profile_id.to_string(),
            followup_app: app_for_followup,
            suppress_followup: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// Shared background Cloud sync/verify runner used by scheduler, tray, and follow-up queue.
/// Records real SQLite job history, emits live progress, and returns `true` on clean success.
pub async fn run_locked_cloud_sync_background(
    app: &AppHandle,
    profile_id: &str,
    do_sync: bool,
    restore_mode: bool,
) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    let followup = if do_sync {
        Some(FollowupSpec {
            do_sync: true,
            restore_mode,
        })
    } else {
        None
    };
    let Some(lock_guard) = state.try_lock_cloud(profile_id, Some(app.clone()), followup) else {
        return false;
    };
    let cancel_reg = state.register_cancel(&format!("bg-cloud:{profile_id}"));
    let cancel = cancel_reg.token.clone();
    let db = state.db.clone();
    let paths = state.paths.clone();
    let pid = profile_id.to_string();
    let app_clone = app.clone();

    let ok = tokio::task::spawn_blocking(move || -> bool {
        let Ok(Some(profile)) = db.get_cloud_profile(&pid) else {
            return false;
        };
        let settings = db.load_settings().unwrap_or_default();
        let _guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);
        if do_sync {
            let kind = if restore_mode {
                "cloud_restore"
            } else {
                "cloud_sync"
            };
            let title = if restore_mode {
                format!("Restore from Drive: {}", profile.name)
            } else {
                format!("Scheduled Drive Sync: {}", profile.name)
            };
            let job_id = db.create_job(kind, &title, 0, 0).ok();
            let _exact_cancel = job_id.as_ref().and_then(|jid| {
                app_clone
                    .try_state::<AppState>()
                    .map(|st| st.register_cancel_exact(jid.clone(), cancel.clone()))
            });
            let app_p = app_clone.clone();
            let jid_p = job_id.clone().unwrap_or_else(|| format!("bg-cloud:{pid}"));
            let res = sync_cloud_profile(
                &profile,
                &db,
                &settings,
                &paths.rclone_config_path,
                restore_mode,
                &cancel,
                move |msg, done, total| {
                    let _ = app_p.emit(
                        "job-progress",
                        JobProgressEvent {
                            job_id: jid_p.clone(),
                            stage: "cloud".into(),
                            message: msg.to_string(),
                            files_done: done,
                            files_total: total,
                            bytes_done: 0,
                            bytes_total: 0,
                            speed_bps: 0,
                        },
                    );
                },
            );
            let succeeded = match res {
                Ok(s) => {
                    if let Some(ref jid) = job_id {
                        let _ = db.finish_job(
                            jid,
                            if s.conflicts_count > 0 {
                                "partial"
                            } else {
                                "succeeded"
                            },
                            (s.uploaded_count + s.downloaded_count + s.backfill_replaced_count)
                                as i64,
                            s.skipped_tombstones as i64,
                            s.conflicts_count as i64,
                            0,
                            serde_json::to_string(&s).ok().as_deref(),
                            None,
                        );
                    }
                    true
                }
                Err(e) => {
                    if let Some(ref jid) = job_id {
                        let st = if cancel.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        };
                        let _ = db.finish_job(jid, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                    }
                    false
                }
            };
            let _ = app_clone.emit("profiles-updated", ());
            let _ = app_clone.emit("jobs-updated", job_id);
            succeeded
        } else {
            let ok = verify_cloud_profile_cancellable(
                &profile,
                &db,
                settings.rclone_path_override.as_deref(),
                &paths.rclone_config_path,
                false,
                &cancel,
            )
            .is_ok();
            let _ = app_clone.emit("profiles-updated", ());
            ok
        }
    })
    .await
    .unwrap_or(false);

    if cancel_reg.token.is_cancelled() {
        lock_guard.suppress_followup_on_cancel();
    }
    ok
}

/// Shared background External Drive sync/verify runner with RAII per-profile locking & job history.
pub async fn run_locked_external_sync_background(
    app: &AppHandle,
    profile_id: &str,
    do_sync: bool,
    restore_mode: bool,
) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    let followup = if do_sync {
        Some(FollowupSpec {
            do_sync: true,
            restore_mode,
        })
    } else {
        None
    };
    let Some(lock_guard) = state.try_lock_external(profile_id, Some(app.clone()), followup) else {
        return false;
    };
    let cancel_reg = state.register_cancel(&format!("bg-ext:{profile_id}"));
    let cancel = cancel_reg.token.clone();
    let db = state.db.clone();
    let pid = profile_id.to_string();
    let app_clone = app.clone();

    let ok = tokio::task::spawn_blocking(move || -> bool {
        let Ok(Some(profile)) = db.get_external_profile(&pid) else {
            return false;
        };
        let settings = db.load_settings().unwrap_or_default();
        let _guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);
        if do_sync {
            let kind = if restore_mode {
                "external_restore"
            } else {
                "external_sync"
            };
            let title = format!("Scheduled Drive Backup: {}", profile.name);
            let job_id = db.create_job(kind, &title, 0, 0).ok();
            let _exact_cancel = job_id.as_ref().and_then(|jid| {
                app_clone
                    .try_state::<AppState>()
                    .map(|st| st.register_cancel_exact(jid.clone(), cancel.clone()))
            });
            let app_p = app_clone.clone();
            let jid_p = job_id.clone().unwrap_or_else(|| format!("bg-ext:{pid}"));
            let res = sync_external_profile(
                &profile,
                &db,
                &settings,
                restore_mode,
                &cancel,
                move |msg, done, total, bytes| {
                    let _ = app_p.emit(
                        "job-progress",
                        JobProgressEvent {
                            job_id: jid_p.clone(),
                            stage: "external".into(),
                            message: msg.to_string(),
                            files_done: done,
                            files_total: total,
                            bytes_done: bytes,
                            bytes_total: 0,
                            speed_bps: 0,
                        },
                    );
                },
            );
            let succeeded = match res {
                Ok(s) => {
                    if let Some(ref jid) = job_id {
                        let _ = db.finish_job(
                            jid,
                            if s.failed_count > 0 {
                                "partial"
                            } else {
                                "succeeded"
                            },
                            (s.pushed_count + s.pulled_count + s.backfill_replaced_count) as i64,
                            0,
                            s.failed_count as i64,
                            s.bytes_transferred as i64,
                            serde_json::to_string(&s).ok().as_deref(),
                            None,
                        );
                    }
                    s.failed_count == 0
                }
                Err(e) => {
                    if let Some(ref jid) = job_id {
                        let st = if cancel.is_cancelled() {
                            "cancelled"
                        } else {
                            "failed"
                        };
                        let _ = db.finish_job(jid, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                    }
                    false
                }
            };
            let _ = app_clone.emit("profiles-updated", ());
            let _ = app_clone.emit("jobs-updated", job_id);
            succeeded
        } else {
            let ok = verify_external_profile(&profile, &db, false).is_ok();
            let _ = app_clone.emit("profiles-updated", ());
            ok
        }
    })
    .await
    .unwrap_or(false);

    if cancel_reg.token.is_cancelled() {
        lock_guard.suppress_followup_on_cancel();
    }
    ok
}

fn validate_schedule(mode: &str, value: &str) -> Result<(), String> {
    match mode {
        "manual" | "startup" | "on_connect" => Ok(()),
        "interval" => {
            let hrs: u32 = value.trim().parse().map_err(|_| {
                "Interval schedule must be a whole number of hours (1–168).".to_string()
            })?;
            if !(1..=168).contains(&hrs) {
                return Err("Interval schedule must be between 1 and 168 hours.".into());
            }
            Ok(())
        }
        "daily" => {
            let parts: Vec<&str> = value.trim().split(':').collect();
            if parts.len() != 2 {
                return Err(
                    "Daily schedule time must use 24-hour HH:MM format (e.g., 20:00).".into(),
                );
            }
            let h: u32 = parts[0]
                .parse()
                .map_err(|_| "Invalid hour in daily schedule.".to_string())?;
            let m: u32 = parts[1]
                .parse()
                .map_err(|_| "Invalid minute in daily schedule.".to_string())?;
            if h > 23 || m > 59 {
                return Err("Daily schedule time must be between 00:00 and 23:59.".into());
            }
            Ok(())
        }
        other => Err(format!("Unsupported schedule mode '{other}'.")),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BootstrapInfo {
    pub version: String,
    pub platform: String,
    pub data_dir: String,
    pub on_battery: bool,
    pub initial_import_volume: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsStatus {
    pub exiftool: ToolAvailability,
    pub rclone: ToolAvailability,
    pub rclone_config_exists: bool,
    pub rclone_remotes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobProgressEvent {
    pub job_id: String,
    pub stage: String,
    pub message: String,
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImportPipelineRequest {
    pub source_root: String,
    pub dest_root: String,
    pub folder_template: String,
    pub date_format: String,
    pub raw_folder: String,
    pub jpeg_folder: String,
    pub video_folder: String,
    pub assignments: Vec<ShootAssignment>,
    pub is_move: bool,
    pub eject_after: bool,
    pub geotag_enabled: bool,
    pub gpx_files: Vec<String>,
    pub clock_offset_secs: i64,
    pub max_gap_secs: i64,
    pub overwrite_existing_gps: bool,
    pub cloud_profile_ids: Vec<String>,
    pub external_profile_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineSummary {
    pub job_id: String,
    pub import: ImportExecutionResult,
    pub geotag: Option<GeotagExecutionResult>,
    pub cloud_synced_profiles: usize,
    pub external_synced_profiles: usize,
    pub ejected: bool,
    pub stage_errors: usize,
    pub notes: Vec<String>,
}

fn map_err(err: anyhow::Error) -> String {
    format!("{err:#}")
}

#[tauri::command]
pub fn get_bootstrap_info(app: AppHandle, state: State<'_, AppState>) -> BootstrapInfo {
    // Take once so React StrictMode / reload never re-triggers initial auto-import forever
    let initial_vol = state.initial_import_volume.lock().take();
    BootstrapInfo {
        version: app.package_info().version.to_string(),
        platform: std::env::consts::OS.to_string(),
        data_dir: state.paths.data_dir.to_string_lossy().to_string(),
        on_battery: is_on_battery(),
        initial_import_volume: initial_vol,
    }
}

#[tauri::command]
pub fn check_tools(state: State<'_, AppState>) -> Result<ToolsStatus, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    let exiftool = check_exiftool(settings.exiftool_path_override.as_deref());
    let rclone = check_rclone(settings.rclone_path_override.as_deref());
    let rclone_config_exists = state.paths.rclone_config_path.is_file();
    let rclone_remotes = list_rclone_remotes(&state.paths.rclone_config_path);
    Ok(ToolsStatus {
        exiftool,
        rclone,
        rclone_config_exists,
        rclone_remotes,
    })
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    state.db.load_settings().map_err(map_err)
}

#[tauri::command]
pub fn save_settings(state: State<'_, AppState>, mut settings: AppSettings) -> Result<(), String> {
    if let Some(bw) = settings.bandwidth_limit_mbps {
        if !bw.is_finite() || bw < 0.0 {
            return Err("Bandwidth limit must be a non-negative number.".into());
        }
        if bw == 0.0 {
            settings.bandwidth_limit_mbps = None;
        }
    }
    if settings.folder_template.trim().is_empty() {
        return Err("Folder template cannot be empty.".into());
    }
    if settings.date_format.trim().is_empty() {
        return Err("Date format cannot be empty.".into());
    }
    if settings.raw_folder_name.trim().is_empty()
        || settings.jpeg_folder_name.trim().is_empty()
        || settings.video_folder_name.trim().is_empty()
    {
        return Err("RAW, JPEG, and Video subfolder names cannot be empty.".into());
    }
    if !(1..=16).contains(&settings.import_threads) {
        return Err("Parallel import threads must be between 1 and 16.".into());
    }
    if settings.stale_warning_days < 1 {
        return Err("Freshness stale warning days must be at least 1.".into());
    }
    settings.watched_gpx_folder = settings
        .watched_gpx_folder
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    settings.rclone_path_override = settings
        .rclone_path_override
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    settings.exiftool_path_override = settings
        .exiftool_path_override
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    state.db.save_settings(&settings).map_err(map_err)
}

#[tauri::command]
pub fn set_os_native_auto_launch(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<String, String> {
    let msg = crate::auto_launch::set_os_native_auto_launch(enabled).map_err(map_err)?;
    let mut settings = state.db.load_settings().map_err(map_err)?;
    settings.os_native_auto_launch = enabled;
    state.db.save_settings(&settings).map_err(map_err)?;
    Ok(msg)
}

#[tauri::command]
pub async fn list_volumes() -> Result<Vec<DetectedVolume>, String> {
    tokio::task::spawn_blocking(list_mounted_volumes)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn eject_volume_cmd(mount_path: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || eject_volume(Path::new(&mount_path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(map_err)
}

#[tauri::command]
pub fn list_year_categories(
    state: State<'_, AppState>,
    base_root: String,
    year: i32,
    create_if_missing: bool,
) -> Result<Vec<String>, String> {
    if base_root.trim().is_empty() {
        return Err("Base library root path is required.".into());
    }
    if !(1970..=2100).contains(&year) {
        return Err("Year must be between 1970 and 2100.".into());
    }
    let settings = state.db.load_settings().map_err(map_err)?;
    list_or_init_year_categories(
        Path::new(base_root.trim()),
        year,
        &settings.default_categories,
        create_if_missing,
    )
    .map_err(map_err)
}

#[tauri::command]
pub fn create_category_folder(
    base_root: String,
    year: i32,
    category: String,
) -> Result<String, String> {
    let root_trim = base_root.trim();
    if root_trim.is_empty() || !Path::new(root_trim).is_absolute() {
        return Err("Base library root must be an absolute directory path.".into());
    }
    if !(1970..=2100).contains(&year) {
        return Err("Year must be between 1970 and 2100.".into());
    }
    let clean = sanitize_segment(&category);
    let target = Path::new(root_trim).join(format!("{year:04}")).join(&clean);
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    Ok(clean)
}

#[tauri::command]
pub fn list_import_presets(state: State<'_, AppState>) -> Result<Vec<ImportPreset>, String> {
    state.db.list_import_presets().map_err(map_err)
}

#[tauri::command]
pub fn save_import_preset(state: State<'_, AppState>, preset: ImportPreset) -> Result<(), String> {
    state.db.upsert_import_preset(&preset).map_err(map_err)
}

#[tauri::command]
pub fn delete_import_preset(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.delete_import_preset(&id).map_err(map_err)
}

#[tauri::command]
pub async fn scan_card(
    state: State<'_, AppState>,
    source_root: String,
) -> Result<CardScanResult, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    tokio::task::spawn_blocking(move || scan_media_source(Path::new(&source_root), &settings))
        .await
        .map_err(|e| e.to_string())?
        .map_err(map_err)
}

#[tauri::command]
pub async fn preflight_import(
    scan: CardScanResult,
    dest_root: String,
    is_move: bool,
) -> Result<PreflightReport, String> {
    tokio::task::spawn_blocking(move || run_preflight_checks(&scan, Path::new(&dest_root), is_move))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn preview_import_plan(
    scan: CardScanResult,
    dest_root: String,
    folder_template: String,
    date_format: String,
    raw_folder: String,
    jpeg_folder: String,
    video_folder: String,
    assignments: Vec<ShootAssignment>,
) -> Result<Vec<ImportPlanItem>, String> {
    build_import_plan(
        &scan,
        Path::new(&dest_root),
        &folder_template,
        &date_format,
        &raw_folder,
        &jpeg_folder,
        &video_folder,
        &assignments,
    )
    .map_err(map_err)
}

#[tauri::command]
pub fn cancel_active_job(state: State<'_, AppState>, job_id: Option<String>) -> bool {
    if let Some(id) = job_id.filter(|s| !s.trim().is_empty()) {
        if state.cancel_by_id(&id) {
            return true;
        }
    }
    state.cancel_all_active()
}

#[tauri::command]
pub async fn run_import_pipeline(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ImportPipelineRequest,
) -> Result<PipelineSummary, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    let db = state.db.clone();
    let paths = state.paths.clone();
    let cancel_reg = state.register_cancel("import-pipeline");
    let cancel = cancel_reg.token.clone();

    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<PipelineSummary> {
        let _sleep_guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);

        let scan = scan_media_source(Path::new(&req.source_root), &settings)?;
        let preflight = run_preflight_checks(&scan, Path::new(&req.dest_root), req.is_move);
        if !preflight.blockers.is_empty() {
            anyhow::bail!("Pre-flight blocked: {}", preflight.blockers.join(" | "));
        }

        let plan = build_import_plan(
            &scan,
            Path::new(&req.dest_root),
            &req.folder_template,
            &req.date_format,
            &req.raw_folder,
            &req.jpeg_folder,
            &req.video_folder,
            &req.assignments,
        )?;
        let bytes_total: u64 = plan.iter().map(|p| p.size_bytes).sum();
        let shoot_title = req
            .assignments
            .iter()
            .map(|a| a.shoot_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let job_id = db.create_job(
            "pipeline",
            &format!("Import: {shoot_title}"),
            plan.len() as i64,
            bytes_total as i64,
        )?;
        let _exact_cancel = app
            .try_state::<AppState>()
            .map(|st| st.register_cancel_exact(job_id.clone(), cancel.clone()));

        let app_prog = app.clone();
        let jid_prog = job_id.clone();
        let import_res = match execute_import_plan(
            &plan,
            req.is_move,
            settings.import_threads,
            settings.deep_verify_move,
            &cancel,
            move |p| {
                let _ = app_prog.emit(
                    "job-progress",
                    JobProgressEvent {
                        job_id: jid_prog.clone(),
                        stage: "import".into(),
                        message: format!("Importing {} / {} files…", p.files_done, p.files_total),
                        files_done: p.files_done,
                        files_total: p.files_total,
                        bytes_done: p.bytes_done,
                        bytes_total: p.bytes_total,
                        speed_bps: p.speed_bps,
                    },
                );
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                let st = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                };
                let _ = db.finish_job(&job_id, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                return Err(e);
            }
        };

        for (src, msg) in &import_res.failures {
            let _ = db.add_job_item(&job_id, "import", src, None, "failed", 0, Some(msg));
        }

        let mut notes = Vec::new();
        let mut stage_errors = 0usize;
        let mut geotag_res: Option<GeotagExecutionResult> = None;

        // Stage 2: Inline EXIF GPS Geotagging (Photos only, BEFORE Google Drive sync!)
        if req.geotag_enabled
            && !req.gpx_files.is_empty()
            && !import_res.dest_photo_paths.is_empty()
            && !cancel.is_cancelled()
        {
            let _ = app.emit(
                "job-progress",
                JobProgressEvent {
                    job_id: job_id.clone(),
                    stage: "geotag".into(),
                    message: "Matching photos to GPX track…".into(),
                    files_done: 0,
                    files_total: import_res.dest_photo_paths.len(),
                    bytes_done: bytes_total,
                    bytes_total,
                    speed_bps: 0,
                },
            );
            match load_gpx_files(&req.gpx_files).and_then(|gpx| {
                let gt_plan = build_geotag_plan(
                    &import_res.dest_photo_paths,
                    &gpx,
                    req.clock_offset_secs,
                    req.max_gap_secs,
                    req.overwrite_existing_gps,
                    &settings,
                )?;
                let app_g = app.clone();
                let jid_g = job_id.clone();
                write_geotags_batch(
                    &gt_plan.items,
                    settings.exiftool_path_override.as_deref(),
                    settings.bump_mtime_on_backfill,
                    &cancel,
                    move |done, total, name| {
                        let _ = app_g.emit(
                            "job-progress",
                            JobProgressEvent {
                                job_id: jid_g.clone(),
                                stage: "geotag".into(),
                                message: format!("Geotagging {name} ({done}/{total})…"),
                                files_done: done,
                                files_total: total,
                                bytes_done: bytes_total,
                                bytes_total,
                                speed_bps: 0,
                            },
                        );
                    },
                )
            }) {
                Ok(g) => {
                    let dup_set: HashSet<&str> = import_res
                        .skipped_duplicate_photo_paths
                        .iter()
                        .map(String::as_str)
                        .collect();
                    for rec in &g.updated_records {
                        if dup_set.contains(rec.path.as_str()) {
                            let _ = db.record_intentional_update(
                                &rec.path,
                                &rec.pre_md5,
                                &rec.post_md5,
                                "gps_backfill",
                            );
                        }
                    }
                    if g.failed_count > 0 {
                        stage_errors += g.failed_count;
                    }
                    notes.push(format!(
                        "Geotagged {} photo(s) ({} skipped, {} failed).",
                        g.tagged_count, g.skipped_count, g.failed_count
                    ));
                    geotag_res = Some(g);
                }
                Err(e) => {
                    stage_errors += 1;
                    notes.push(format!("Geotag stage failed: {e}"));
                }
            }
        }

        // Optional safe eject only if zero import failures
        let mut ejected = false;
        if req.eject_after && import_res.files_failed == 0 && !cancel.is_cancelled() {
            match eject_volume(Path::new(&req.source_root)) {
                Ok(()) => {
                    ejected = true;
                    notes.push("SD card safely ejected.".into());
                }
                Err(e) => notes.push(format!("Safe eject skipped: {e}")),
            }
        }

        // Stage 3: Google Drive Sync (acquires per-profile RAII lock; queues follow-up sync if busy!)
        let mut cloud_synced = 0usize;
        if import_res.files_failed == 0 && !cancel.is_cancelled() {
            if let Some(st) = app.try_state::<AppState>() {
                for pid in &req.cloud_profile_ids {
                    let Some(lock) = st.try_lock_cloud(
                        pid,
                        Some(app.clone()),
                        Some(FollowupSpec {
                            do_sync: true,
                            restore_mode: false,
                        }),
                    ) else {
                        notes.push(
                            "Google Drive sync for profile already running; queued automatic follow-up pass."
                                .to_string(),
                        );
                        continue;
                    };
                    if let Ok(Some(profile)) = db.get_cloud_profile(pid) {
                        let app_c = app.clone();
                        let jid_c = job_id.clone();
                        match sync_cloud_profile(
                            &profile,
                            &db,
                            &settings,
                            &paths.rclone_config_path,
                            false,
                            &cancel,
                            move |msg, done, total| {
                                let _ = app_c.emit(
                                    "job-progress",
                                    JobProgressEvent {
                                        job_id: jid_c.clone(),
                                        stage: "cloud".into(),
                                        message: msg.to_string(),
                                        files_done: done,
                                        files_total: total,
                                        bytes_done: 0,
                                        bytes_total: 0,
                                        speed_bps: 0,
                                    },
                                );
                            },
                        ) {
                            Ok(c) => {
                                cloud_synced += 1;
                                notes.push(format!(
                                    "Google Drive '{}': ↑{} uploaded, ↓{} downloaded.",
                                    profile.name, c.uploaded_count, c.downloaded_count
                                ));
                            }
                            Err(e) => {
                                if cancel.is_cancelled() {
                                    lock.suppress_followup_on_cancel();
                                } else {
                                    stage_errors += 1;
                                    notes.push(format!(
                                        "Google Drive '{}' error: {e}",
                                        profile.name
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        } else if import_res.files_failed > 0 && !req.cloud_profile_ids.is_empty() {
            notes.push(
                "Skipped Google Drive sync because one or more SD transfers failed. Failed files remain on the SD card."
                    .into(),
            );
        }

        // Stage 4: External Drive Sync (acquires per-profile RAII external lock with follow-up queue!)
        let mut external_synced = 0usize;
        if import_res.files_failed == 0 && !cancel.is_cancelled() {
            if let Some(st) = app.try_state::<AppState>() {
                for pid in &req.external_profile_ids {
                    let Some(lock) = st.try_lock_external(
                        pid,
                        Some(app.clone()),
                        Some(FollowupSpec {
                            do_sync: true,
                            restore_mode: false,
                        }),
                    ) else {
                        notes.push(
                            "External drive sync already running; queued automatic follow-up pass."
                                .to_string(),
                        );
                        continue;
                    };
                    if let Ok(Some(profile)) = db.get_external_profile(pid) {
                        if truongsync_sync::external::resolve_external_target(&profile).is_none() {
                            notes.push(format!(
                                "External drive '{}' not connected; skipped.",
                                profile.name
                            ));
                            continue;
                        }
                        let app_e = app.clone();
                        let jid_e = job_id.clone();
                        match sync_external_profile(
                            &profile,
                            &db,
                            &settings,
                            false,
                            &cancel,
                            move |msg, done, total, bytes| {
                                let _ = app_e.emit(
                                    "job-progress",
                                    JobProgressEvent {
                                        job_id: jid_e.clone(),
                                        stage: "external".into(),
                                        message: msg.to_string(),
                                        files_done: done,
                                        files_total: total,
                                        bytes_done: bytes,
                                        bytes_total: 0,
                                        speed_bps: 0,
                                    },
                                );
                            },
                        ) {
                            Ok(e) => {
                                external_synced += 1;
                                if e.failed_count > 0 {
                                    stage_errors += e.failed_count;
                                }
                                notes.push(format!(
                                    "External '{}': ↑{} copied, ↓{} pulled{}.",
                                    profile.name,
                                    e.pushed_count,
                                    e.pulled_count,
                                    if e.failed_count > 0 {
                                        format!(", {} failed", e.failed_count)
                                    } else {
                                        String::new()
                                    }
                                ));
                            }
                            Err(e) => {
                                if cancel.is_cancelled() {
                                    lock.suppress_followup_on_cancel();
                                } else {
                                    stage_errors += 1;
                                    notes.push(format!("External '{}' error: {e}", profile.name));
                                }
                            }
                        }
                    }
                }
            }
        }

        let summary = PipelineSummary {
            job_id: job_id.clone(),
            import: import_res.clone(),
            geotag: geotag_res,
            cloud_synced_profiles: cloud_synced,
            external_synced_profiles: external_synced,
            ejected,
            stage_errors,
            notes,
        };

        let status = if cancel.is_cancelled() {
            "cancelled"
        } else if import_res.files_failed > 0 || stage_errors > 0 {
            "partial"
        } else {
            "succeeded"
        };
        let _ = db.finish_job(
            &job_id,
            status,
            import_res.files_moved as i64,
            import_res.files_skipped as i64,
            (import_res.files_failed + stage_errors) as i64,
            import_res.bytes_transferred as i64,
            serde_json::to_string(&summary).ok().as_deref(),
            None,
        );
        let _ = app.emit("profiles-updated", ());
        let _ = app.emit("jobs-updated", Some(job_id.clone()));
        Ok(summary)
    })
    .await
    .map_err(|e| e.to_string())?;

    res.map_err(map_err)
}

// ── Geotag Commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn find_matching_gpx(
    state: State<'_, AppState>,
    earliest_utc: String,
    latest_utc: String,
    folder_override: Option<String>,
) -> Result<Vec<WatchedGpxMatch>, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    let folder = folder_override
        .filter(|s| !s.trim().is_empty())
        .or(settings.watched_gpx_folder);
    let Some(folder) = folder else {
        return Ok(Vec::new());
    };
    let e = DateTime::parse_from_rfc3339(&earliest_utc)
        .map_err(|err| err.to_string())?
        .with_timezone(&chrono::Utc);
    let l = DateTime::parse_from_rfc3339(&latest_utc)
        .map_err(|err| err.to_string())?
        .with_timezone(&chrono::Utc);
    tokio::task::spawn_blocking(move || find_matching_gpx_files(Path::new(&folder), e, l))
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn preview_geotag(
    state: State<'_, AppState>,
    target_paths: Vec<String>,
    gpx_files: Vec<String>,
    clock_offset_secs: i64,
    max_gap_secs: i64,
    overwrite_existing: bool,
) -> Result<GeotagPlan, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    tokio::task::spawn_blocking(move || {
        let gpx = load_gpx_files(&gpx_files)?;
        build_geotag_plan(
            &target_paths,
            &gpx,
            clock_offset_secs,
            max_gap_secs,
            overwrite_existing,
            &settings,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(map_err)
}

#[tauri::command]
pub async fn run_geotag(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<PhotoGpsMatch>,
    record_backfill_updates: bool,
) -> Result<GeotagExecutionResult, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    let db = state.db.clone();
    let cancel_reg = state.register_cancel("geotag");
    let cancel = cancel_reg.token.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<GeotagExecutionResult> {
        let _guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);
        let job_id = db.create_job("geotag", "EXIF GPS Geotag", items.len() as i64, 0)?;
        let _exact_cancel = app
            .try_state::<AppState>()
            .map(|st| st.register_cancel_exact(job_id.clone(), cancel.clone()));
        let app_p = app.clone();
        let jid_p = job_id.clone();

        let res = match write_geotags_batch(
            &items,
            settings.exiftool_path_override.as_deref(),
            settings.bump_mtime_on_backfill,
            &cancel,
            move |done, total, name| {
                let _ = app_p.emit(
                    "job-progress",
                    JobProgressEvent {
                        job_id: jid_p.clone(),
                        stage: "geotag".into(),
                        message: format!("Writing GPS EXIF to {name} ({done}/{total})…"),
                        files_done: done,
                        files_total: total,
                        bytes_done: 0,
                        bytes_total: 0,
                        speed_bps: 0,
                    },
                );
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                let st = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                };
                let _ = db.finish_job(&job_id, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                return Err(e);
            }
        };

        if record_backfill_updates {
            for rec in &res.updated_records {
                let _ = db.record_intentional_update(
                    &rec.path,
                    &rec.pre_md5,
                    &rec.post_md5,
                    "gps_backfill",
                );
            }
        }
        for (p, msg) in &res.failures {
            let _ = db.add_job_item(&job_id, "geotag", p, None, "failed", 0, Some(msg));
        }
        let status = if cancel.is_cancelled() {
            "cancelled"
        } else if res.failed_count > 0 {
            "partial"
        } else {
            "succeeded"
        };
        let _ = db.finish_job(
            &job_id,
            status,
            res.tagged_count as i64,
            res.skipped_count as i64,
            res.failed_count as i64,
            0,
            serde_json::to_string(&res).ok().as_deref(),
            None,
        );
        let _ = app.emit("jobs-updated", Some(job_id.clone()));
        Ok(res)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(map_err)
}

// ── Cloud Sync Commands ──────────────────────────────────────────────────────

#[tauri::command]
pub fn list_cloud_profiles(state: State<'_, AppState>) -> Result<Vec<CloudProfile>, String> {
    state.db.list_cloud_profiles().map_err(map_err)
}

#[tauri::command]
pub fn save_cloud_profile(
    state: State<'_, AppState>,
    mut profile: CloudProfile,
) -> Result<String, String> {
    if profile.name.trim().is_empty() || profile.local_root.trim().is_empty() {
        return Err("Profile name and local folder root are required.".into());
    }
    profile.remote_name = validate_rclone_remote_name(&profile.remote_name).map_err(map_err)?;
    validate_schedule(&profile.schedule_mode, &profile.schedule_value)?;
    profile.stale_after_days = profile.stale_after_days.max(1);
    state.db.upsert_cloud_profile(&profile).map_err(map_err)
}

#[tauri::command]
pub fn delete_cloud_profile(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.delete_cloud_profile(&id).map_err(map_err)
}

#[tauri::command]
pub fn import_rclone_conf(
    state: State<'_, AppState>,
    source_path: String,
) -> Result<Vec<String>, String> {
    import_rclone_config(Path::new(&source_path), &state.paths.rclone_config_path).map_err(map_err)
}

#[tauri::command]
pub fn list_rclone_remotes_cmd(state: State<'_, AppState>) -> Vec<String> {
    list_rclone_remotes(&state.paths.rclone_config_path)
}

#[tauri::command]
pub async fn connect_google_drive(
    state: State<'_, AppState>,
    remote_name: String,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<Vec<String>, String> {
    let settings = state.db.load_settings().map_err(map_err)?;
    let conf = state.paths.rclone_config_path.clone();
    let cancel_reg = state.register_cancel("oauth");
    let cancel = cancel_reg.token.clone();
    tokio::task::spawn_blocking(move || {
        connect_google_drive_oauth(
            settings.rclone_path_override.as_deref(),
            &conf,
            &remote_name,
            client_id.as_deref(),
            client_secret.as_deref(),
            &cancel,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(map_err)
}

#[tauri::command]
pub async fn verify_cloud_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: String,
    deep_md5: bool,
) -> Result<CloudVerifyReport, String> {
    // Acquire lock WITHOUT queueing a write follow-up so read-only Verify never races Sync
    let Some(_lock_guard) = state.try_lock_cloud(&profile_id, None, None) else {
        return Err("A Google Drive sync or verify is already running for this profile.".into());
    };
    let settings = state.db.load_settings().map_err(map_err)?;
    let profile = state
        .db
        .get_cloud_profile(&profile_id)
        .map_err(map_err)?
        .ok_or_else(|| "Cloud profile not found".to_string())?;
    let db = state.db.clone();
    let conf = state.paths.rclone_config_path.clone();
    let cancel_reg = state.register_cancel(&format!("verify-cloud:{profile_id}"));
    let cancel = cancel_reg.token.clone();

    let report = tokio::task::spawn_blocking(move || {
        verify_cloud_profile_cancellable(
            &profile,
            &db,
            settings.rclone_path_override.as_deref(),
            &conf,
            deep_md5,
            &cancel,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(map_err)?;

    let _ = app.emit("profiles-updated", ());
    Ok(report)
}

#[tauri::command]
pub async fn sync_cloud_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: String,
    restore_mode: bool,
) -> Result<CloudSyncResult, String> {
    let Some(lock_guard) = state.try_lock_cloud(
        &profile_id,
        Some(app.clone()),
        Some(FollowupSpec {
            do_sync: true,
            restore_mode,
        }),
    ) else {
        return Err(
            "A Google Drive sync is already running for this profile. A follow-up pass has been queued."
                .into(),
        );
    };

    let settings = state.db.load_settings().map_err(map_err)?;
    let profile = state
        .db
        .get_cloud_profile(&profile_id)
        .map_err(map_err)?
        .ok_or_else(|| "Cloud profile not found".to_string())?;
    let db = state.db.clone();
    let conf = state.paths.rclone_config_path.clone();
    let cancel_reg = state.register_cancel(&format!("cloud:{profile_id}"));
    let cancel = cancel_reg.token.clone();

    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<CloudSyncResult> {
        let _guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);
        let kind = if restore_mode {
            "cloud_restore"
        } else {
            "cloud_sync"
        };
        let title = if restore_mode {
            format!("Restore from Drive: {}", profile.name)
        } else {
            format!("Google Drive Sync: {}", profile.name)
        };
        let job_id = db.create_job(kind, &title, 0, 0)?;
        let _exact_cancel = app
            .try_state::<AppState>()
            .map(|st| st.register_cancel_exact(job_id.clone(), cancel.clone()));
        let app_p = app.clone();
        let jid_p = job_id.clone();

        let out = sync_cloud_profile(
            &profile,
            &db,
            &settings,
            &conf,
            restore_mode,
            &cancel,
            move |msg, done, total| {
                let _ = app_p.emit(
                    "job-progress",
                    JobProgressEvent {
                        job_id: jid_p.clone(),
                        stage: "cloud".into(),
                        message: msg.to_string(),
                        files_done: done,
                        files_total: total,
                        bytes_done: 0,
                        bytes_total: 0,
                        speed_bps: 0,
                    },
                );
            },
        );
        match out {
            Ok(s) => {
                let _ = db.finish_job(
                    &job_id,
                    if s.conflicts_count > 0 {
                        "partial"
                    } else {
                        "succeeded"
                    },
                    (s.uploaded_count + s.downloaded_count + s.backfill_replaced_count) as i64,
                    s.skipped_tombstones as i64,
                    s.conflicts_count as i64,
                    0,
                    serde_json::to_string(&s).ok().as_deref(),
                    None,
                );
                let _ = app.emit("profiles-updated", ());
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                Ok(s)
            }
            Err(e) => {
                let st = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                };
                let _ = db.finish_job(&job_id, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                let _ = app.emit("profiles-updated", ());
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    if cancel_reg.token.is_cancelled() {
        lock_guard.suppress_followup_on_cancel();
    }
    res.map_err(map_err)
}

// ── External Drive Backup Commands ───────────────────────────────────────────

#[tauri::command]
pub fn list_external_profiles(state: State<'_, AppState>) -> Result<Vec<ExternalProfile>, String> {
    state.db.list_external_profiles().map_err(map_err)
}

#[tauri::command]
pub fn save_external_profile(
    state: State<'_, AppState>,
    mut profile: ExternalProfile,
) -> Result<String, String> {
    if profile.name.trim().is_empty()
        || profile.local_root.trim().is_empty()
        || profile.volume_label.trim().is_empty()
    {
        return Err("Profile name, local folder root, and drive label are required.".into());
    }
    validate_schedule(&profile.schedule_mode, &profile.schedule_value)?;
    profile.stale_after_days = profile.stale_after_days.max(1);
    state.db.upsert_external_profile(&profile).map_err(map_err)
}

#[tauri::command]
pub fn delete_external_profile(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.delete_external_profile(&id).map_err(map_err)
}

#[tauri::command]
pub async fn verify_external_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: String,
    deep_md5: bool,
) -> Result<ExternalVerifyReport, String> {
    let Some(_lock_guard) = state.try_lock_external(&profile_id, None, None) else {
        return Err("An external drive sync or verify is already running for this profile.".into());
    };
    let profile = state
        .db
        .get_external_profile(&profile_id)
        .map_err(map_err)?
        .ok_or_else(|| "External drive profile not found".to_string())?;
    let db = state.db.clone();
    let report =
        tokio::task::spawn_blocking(move || verify_external_profile(&profile, &db, deep_md5))
            .await
            .map_err(|e| e.to_string())?
            .map_err(map_err)?;
    let _ = app.emit("profiles-updated", ());
    Ok(report)
}

#[tauri::command]
pub async fn sync_external_cmd(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: String,
    restore_mode: bool,
) -> Result<ExternalSyncResult, String> {
    let Some(lock_guard) = state.try_lock_external(
        &profile_id,
        Some(app.clone()),
        Some(FollowupSpec {
            do_sync: true,
            restore_mode,
        }),
    ) else {
        return Err(
            "An external drive sync is already running for this profile. A follow-up pass has been queued."
                .into(),
        );
    };

    let settings = state.db.load_settings().map_err(map_err)?;
    let profile = state
        .db
        .get_external_profile(&profile_id)
        .map_err(map_err)?
        .ok_or_else(|| "External drive profile not found".to_string())?;
    let db = state.db.clone();
    let cancel_reg = state.register_cancel(&format!("ext:{profile_id}"));
    let cancel = cancel_reg.token.clone();

    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<ExternalSyncResult> {
        let _guard = SleepGuard::acquire(settings.prevent_sleep_during_transfers);
        let kind = if restore_mode {
            "external_restore"
        } else {
            "external_sync"
        };
        let title = if restore_mode {
            format!("Restore from External: {}", profile.name)
        } else {
            format!("External Drive Sync: {}", profile.name)
        };
        let job_id = db.create_job(kind, &title, 0, 0)?;
        let _exact_cancel = app
            .try_state::<AppState>()
            .map(|st| st.register_cancel_exact(job_id.clone(), cancel.clone()));
        let app_p = app.clone();
        let jid_p = job_id.clone();

        let out = sync_external_profile(
            &profile,
            &db,
            &settings,
            restore_mode,
            &cancel,
            move |msg, done, total, bytes| {
                let _ = app_p.emit(
                    "job-progress",
                    JobProgressEvent {
                        job_id: jid_p.clone(),
                        stage: "external".into(),
                        message: msg.to_string(),
                        files_done: done,
                        files_total: total,
                        bytes_done: bytes,
                        bytes_total: 0,
                        speed_bps: 0,
                    },
                );
            },
        );
        match out {
            Ok(s) => {
                for (rel, msg) in &s.failures {
                    let _ = db.add_job_item(&job_id, "external", rel, None, "failed", 0, Some(msg));
                }
                let _ = db.finish_job(
                    &job_id,
                    if s.failed_count > 0 {
                        "partial"
                    } else {
                        "succeeded"
                    },
                    (s.pushed_count + s.pulled_count + s.backfill_replaced_count) as i64,
                    0,
                    s.failed_count as i64,
                    s.bytes_transferred as i64,
                    serde_json::to_string(&s).ok().as_deref(),
                    None,
                );
                let _ = app.emit("profiles-updated", ());
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                Ok(s)
            }
            Err(e) => {
                let st = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                };
                let _ = db.finish_job(&job_id, st, 0, 0, 1, 0, None, Some(&e.to_string()));
                let _ = app.emit("profiles-updated", ());
                let _ = app.emit("jobs-updated", Some(job_id.clone()));
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    if cancel_reg.token.is_cancelled() {
        lock_guard.suppress_followup_on_cancel();
    }
    res.map_err(map_err)
}

// ── Jobs & Window Controls ───────────────────────────────────────────────────

#[tauri::command]
pub fn list_jobs(state: State<'_, AppState>, limit: Option<i64>) -> Result<Vec<JobRecord>, String> {
    state.db.list_jobs(limit.unwrap_or(100)).map_err(map_err)
}

#[tauri::command]
pub fn list_job_items(
    state: State<'_, AppState>,
    job_id: String,
    limit: Option<i64>,
) -> Result<Vec<JobItemRecord>, String> {
    state
        .db
        .list_job_items(&job_id, limit.unwrap_or(500))
        .map_err(map_err)
}

#[tauri::command]
pub fn show_main_window_cmd(app: AppHandle) {
    show_main_window(&app);
}

#[tauri::command]
pub fn quit_app_cmd(app: AppHandle, state: State<'_, AppState>) {
    state.cancel_all_active();
    app.exit(0);
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateFeedStatus {
    pub available: bool,
    pub version: Option<String>,
    pub notes: Option<String>,
}

/// Checks the configured GitHub Releases Tauri updater feed and returns update availability metadata.
#[tauri::command]
pub async fn check_update_feed(app: AppHandle) -> Result<UpdateFeedStatus, String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(err) => {
            tracing::debug!("updater unavailable: {err}");
            return Ok(UpdateFeedStatus {
                available: false,
                version: None,
                notes: Some(format!("Updater unavailable: {err}")),
            });
        }
    };

    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateFeedStatus {
            available: true,
            version: Some(update.version),
            notes: update.body,
        }),
        Ok(None) => Ok(UpdateFeedStatus {
            available: false,
            version: None,
            notes: None,
        }),
        Err(err) => {
            tracing::debug!("update feed check failed: {err}");
            Ok(UpdateFeedStatus {
                available: false,
                version: None,
                notes: Some(format!("Update feed unavailable: {err}")),
            })
        }
    }
}

/// Downloads and installs a signed desktop update while cancelling active background jobs cleanly.
#[tauri::command]
pub async fn install_native_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    let exit_handle = app.clone();
    let updater = app
        .updater_builder()
        .on_before_exit(move || {
            if let Some(state) = exit_handle.try_state::<AppState>() {
                state.cancel_all_active();
            }
            exit_handle.cleanup_before_exit();
        })
        .build()
        .map_err(|e| format!("Updater unavailable: {e}"))?;

    let Some(update) = updater
        .check()
        .await
        .map_err(|e| format!("Failed to check for updates: {e}"))?
    else {
        return Err("No update available.".to_string());
    };

    let downloaded = Arc::new(AtomicU64::new(0));
    let last_emitted = Arc::new(AtomicU64::new(0));
    let progress_app = app.clone();
    let finish_app = app.clone();
    let dl_counter = Arc::clone(&downloaded);
    let emit_counter = Arc::clone(&last_emitted);

    update
        .download_and_install(
            move |chunk_len, content_length| {
                let total_dl =
                    dl_counter.fetch_add(chunk_len as u64, Ordering::Relaxed) + chunk_len as u64;
                let prev = emit_counter.load(Ordering::Relaxed);
                let should_emit = total_dl.saturating_sub(prev) >= 256 * 1024
                    || content_length.is_some_and(|total| total_dl >= total);
                if should_emit {
                    emit_counter.store(total_dl, Ordering::Relaxed);
                    let _ = progress_app.emit(
                        "desktop-update-progress",
                        serde_json::json!({
                            "downloaded": total_dl,
                            "total": content_length,
                            "finished": false,
                        }),
                    );
                }
            },
            move || {
                let _ = finish_app.emit(
                    "desktop-update-progress",
                    serde_json::json!({
                        "downloaded": downloaded.load(Ordering::Relaxed),
                        "total": serde_json::Value::Null,
                        "finished": true,
                    }),
                );
            },
        )
        .await
        .map_err(|e| format!("Failed to install update: {e}"))?;

    if let Some(state) = app.try_state::<AppState>() {
        state.cancel_all_active();
    }
    app.restart();
}
