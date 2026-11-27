//! Background volume arrival watcher (SD card auto-open + external backup drive connect)
//! and non-blocking user-scheduled background sync/verify runner with battery awareness,
//! per-profile RAII locks, and failure cooldowns.

use chrono::{DateTime, Local, NaiveTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use truongsync_ingest::volumes::{list_mounted_volumes, DetectedVolume};

use crate::commands::{
    run_locked_cloud_sync_background, run_locked_external_sync_background, AppState,
};
use crate::power::is_on_battery;
use crate::tray::show_main_window;

#[derive(Debug, Clone, Serialize)]
pub struct BackupDriveEvent {
    pub profile_id: String,
    pub profile_name: String,
    pub mount_path: String,
    pub volume_label: String,
}

fn volume_key(v: &DetectedVolume) -> (String, String) {
    (v.mount_path.clone(), v.volume_uuid.clone())
}

fn external_profile_matches_volume(
    p: &truongsync_db::models::ExternalProfile,
    vol: &DetectedVolume,
) -> bool {
    let p_uuid = p.volume_uuid.trim();
    if !p_uuid.is_empty() {
        return vol.volume_uuid.eq_ignore_ascii_case(p_uuid);
    }
    !p.volume_label.trim().is_empty() && p.volume_label.eq_ignore_ascii_case(&vol.label)
}

#[cfg(target_os = "macos")]
fn fast_volumes_dir_snapshot() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/Volumes")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// Starts the background volume watcher that detects newly inserted SD cards (`DCIM/`)
/// and known external backup drives across macOS and Windows.
/// On macOS, fast-checks `/Volumes` directory entries before invoking `diskutil info -plist`
/// so idle polling never wakes sleeping external HDDs or burns CPU.
pub fn start_volume_watcher(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let initial = tokio::task::spawn_blocking(list_mounted_volumes)
            .await
            .unwrap_or_default();
        let mut seen: HashSet<(String, String)> = initial.iter().map(volume_key).collect();
        #[cfg(target_os = "macos")]
        let mut last_dir_snap = fast_volumes_dir_snapshot();
        #[cfg(target_os = "macos")]
        let mut last_full_poll = Instant::now();

        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            #[cfg(target_os = "macos")]
            {
                let cur_snap = tokio::task::spawn_blocking(fast_volumes_dir_snapshot)
                    .await
                    .unwrap_or_default();
                if cur_snap == last_dir_snap && last_full_poll.elapsed() < Duration::from_secs(30) {
                    continue;
                }
                last_dir_snap = cur_snap;
                last_full_poll = Instant::now();
            }

            let current = tokio::task::spawn_blocking(list_mounted_volumes)
                .await
                .unwrap_or_default();
            let current_keys: HashSet<(String, String)> = current.iter().map(volume_key).collect();

            if current_keys != seen {
                let newly_arrived: Vec<DetectedVolume> = current
                    .iter()
                    .filter(|v| !seen.contains(&volume_key(v)))
                    .cloned()
                    .collect();

                seen = current_keys;
                let _ = app.emit("volumes-changed", &current);

                let app_snap = app.clone();
                let meta = tokio::task::spawn_blocking(move || {
                    let state = app_snap.try_state::<AppState>()?;
                    let settings = state.db.load_settings().unwrap_or_default();
                    let ext_profiles = state.db.list_external_profiles().unwrap_or_default();
                    let on_batt = is_on_battery();
                    Some((settings, ext_profiles, on_batt))
                })
                .await
                .ok()
                .flatten();

                let Some((settings, ext_profiles, on_batt)) = meta else {
                    continue;
                };
                let battery_blocked = !settings.sync_on_battery && on_batt;

                let disconnected_profiles: Vec<String> = ext_profiles
                    .iter()
                    .filter(|p| {
                        !current
                            .iter()
                            .any(|v| external_profile_matches_volume(p, v))
                    })
                    .map(|p| p.id.clone())
                    .collect();
                if !disconnected_profiles.is_empty() {
                    if let Some(st) = app.try_state::<AppState>() {
                        let mut def = st.deferred_on_connect.lock();
                        for pid in disconnected_profiles {
                            def.remove(&pid);
                        }
                    }
                }

                for vol in newly_arrived {
                    // 1. Camera SD card arrived -> auto-open Import tab!
                    if vol.has_dcim && settings.auto_open_on_sd_insert {
                        show_main_window(&app);
                        let _ = app.emit("navigate-tab", "import");
                        let _ = app.emit("auto-import-volume", &vol);
                    }

                    // 2. Match ALL registered external backup profiles on this drive (UUID-strict;
                    //    never skipped even if the backup SSD also happens to contain a DCIM folder)
                    for profile in ext_profiles
                        .iter()
                        .filter(|p| p.enabled && external_profile_matches_volume(p, &vol))
                    {
                        if settings.prompt_on_backup_drive {
                            let _ = app.emit(
                                "backup-drive-connected",
                                BackupDriveEvent {
                                    profile_id: profile.id.clone(),
                                    profile_name: profile.name.clone(),
                                    mount_path: vol.mount_path.clone(),
                                    volume_label: vol.label.clone(),
                                },
                            );
                        }
                        if profile.schedule_mode == "on_connect" {
                            if app
                                .try_state::<AppState>()
                                .is_some_and(|st| st.is_quiescing())
                            {
                                continue;
                            }
                            if battery_blocked {
                                if let Some(st) = app.try_state::<AppState>() {
                                    st.deferred_on_connect.lock().insert(profile.id.clone());
                                }
                            } else {
                                if let Some(st) = app.try_state::<AppState>() {
                                    st.deferred_on_connect.lock().remove(&profile.id);
                                }
                                let app_clone = app.clone();
                                let pid = profile.id.clone();
                                let do_sync = profile.schedule_action == "sync";
                                tauri::async_runtime::spawn(async move {
                                    let _ = run_locked_external_sync_background(
                                        &app_clone, &pid, do_sync, false,
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Triggers sync across all enabled cloud profiles (used by the system tray menu).
pub async fn trigger_all_enabled_cloud_syncs(app: AppHandle) {
    let app_snap = app.clone();
    let profiles = tokio::task::spawn_blocking(move || {
        let Some(state) = app_snap.try_state::<AppState>() else {
            return Vec::new();
        };
        state.db.list_cloud_profiles().unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    for p in profiles.into_iter().filter(|p| p.enabled) {
        let app_c = app.clone();
        let pid = p.id.clone();
        tauri::async_runtime::spawn(async move {
            let _ = run_locked_cloud_sync_background(&app_c, &pid, true, false).await;
        });
    }
}

/// Non-blocking background scheduler ticking every 60 seconds for user-configured Cloud &
/// External schedules. Runs all blocking syscalls/SQLite/diskutil checks on `spawn_blocking`,
/// respects battery policy, and only marks daily/startup complete after success (with a 15-min
/// cooldown on failures so transient offline errors retry cleanly).
pub fn start_background_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let startup_done: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let last_daily_key: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let last_failure: Arc<Mutex<HashMap<String, Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut last_battery: Option<bool> = None;

        loop {
            let app_tick = app.clone();
            let tick = tokio::task::spawn_blocking(move || {
                let on_batt = is_on_battery();
                let Some(state) = app_tick.try_state::<AppState>() else {
                    return (on_batt, None);
                };
                let settings = state.db.load_settings().unwrap_or_default();
                let cloud = state.db.list_cloud_profiles().unwrap_or_default();
                let ext = state.db.list_external_profiles().unwrap_or_default();
                (on_batt, Some((settings, cloud, ext)))
            })
            .await
            .unwrap_or((false, None));

            let (on_batt, maybe_data) = tick;
            if last_battery != Some(on_batt) {
                last_battery = Some(on_batt);
                let _ = app.emit("battery-changed", on_batt);
            }

            if let Some((settings, cloud_profiles, ext_profiles)) = maybe_data {
                let quiescing = app
                    .try_state::<AppState>()
                    .is_some_and(|st| st.is_quiescing());
                let battery_blocked = !settings.sync_on_battery && on_batt;

                if !battery_blocked && !quiescing {
                    let now_utc = Utc::now();
                    let now_local = Local::now();
                    let today_str = now_local.format("%Y-%m-%d").to_string();

                    for p in cloud_profiles.into_iter().filter(|p| p.enabled) {
                        let key = format!("cloud:{}", p.id);
                        if app
                            .try_state::<AppState>()
                            .is_some_and(|st| st.is_cloud_locked(&p.id))
                        {
                            continue;
                        }
                        if recently_attempted(&last_failure.lock(), &key, 15) {
                            continue;
                        }
                        let latest_ref = latest_timestamp(
                            p.last_sync_at.as_deref(),
                            p.last_verify_at.as_deref(),
                            p.schedule_action == "check",
                        );
                        let daily_mem = last_daily_key.lock().get(&key).cloned();
                        let due = match p.schedule_mode.as_str() {
                            "startup" => !startup_done.lock().contains(&key),
                            "interval" => {
                                let hours: i64 =
                                    p.schedule_value.trim().parse().unwrap_or(6).max(1);
                                is_interval_due(latest_ref, now_utc, hours)
                            }
                            "daily" => is_daily_due(
                                &p.schedule_value,
                                now_local,
                                daily_mem.as_deref(),
                                latest_ref,
                                &today_str,
                            ),
                            _ => false,
                        };
                        if due {
                            let app_c = app.clone();
                            let pid = p.id.clone();
                            let do_sync = p.schedule_action == "sync";
                            let mode = p.schedule_mode.clone();
                            let s_done = Arc::clone(&startup_done);
                            let d_done = Arc::clone(&last_daily_key);
                            let fail_map = Arc::clone(&last_failure);
                            let today_c = today_str.clone();
                            tauri::async_runtime::spawn(async move {
                                let ok =
                                    run_locked_cloud_sync_background(&app_c, &pid, do_sync, false)
                                        .await;
                                if ok {
                                    fail_map.lock().remove(&key);
                                    if mode == "startup" {
                                        s_done.lock().insert(key);
                                    } else if mode == "daily" {
                                        d_done.lock().insert(key, today_c);
                                    }
                                } else {
                                    fail_map.lock().insert(key, Instant::now());
                                }
                            });
                        }
                    }

                    // Determine which enabled external profiles are actually due or deferred BEFORE
                    // polling OS volumes so idle 60-second scheduler ticks never spawn `diskutil`.
                    let ext_candidates: Vec<_> = ext_profiles
                        .into_iter()
                        .filter(|p| p.enabled)
                        .filter_map(|p| {
                            let key = format!("external:{}", p.id);
                            let mut was_deferred = app
                                .try_state::<AppState>()
                                .is_some_and(|st| st.deferred_on_connect.lock().contains(&p.id));
                            if was_deferred && p.schedule_mode != "on_connect" {
                                if let Some(st) = app.try_state::<AppState>() {
                                    st.deferred_on_connect.lock().remove(&p.id);
                                }
                                was_deferred = false;
                            }
                            let locked = app
                                .try_state::<AppState>()
                                .is_some_and(|st| st.is_external_locked(&p.id));
                            let cooled = recently_attempted(&last_failure.lock(), &key, 15);
                            let latest_ref = latest_timestamp(
                                p.last_sync_at.as_deref(),
                                p.last_verify_at.as_deref(),
                                p.schedule_action == "check",
                            );
                            let daily_mem = last_daily_key.lock().get(&key).cloned();
                            let due = !locked
                                && !cooled
                                && match p.schedule_mode.as_str() {
                                    "on_connect" => was_deferred,
                                    "interval" => {
                                        let hours: i64 =
                                            p.schedule_value.trim().parse().unwrap_or(12).max(1);
                                        is_interval_due(latest_ref, now_utc, hours)
                                    }
                                    "daily" => is_daily_due(
                                        &p.schedule_value,
                                        now_local,
                                        daily_mem.as_deref(),
                                        latest_ref,
                                        &today_str,
                                    ),
                                    _ => false,
                                };
                            if due || was_deferred {
                                Some((p, key, was_deferred, due))
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !ext_candidates.is_empty() {
                        let mounted_vols = tokio::task::spawn_blocking(list_mounted_volumes)
                            .await
                            .unwrap_or_default();

                        for (p, key, was_deferred, due) in ext_candidates {
                            let connected = mounted_vols
                                .iter()
                                .any(|v| external_profile_matches_volume(&p, v));
                            if was_deferred && !connected {
                                if let Some(st) = app.try_state::<AppState>() {
                                    st.deferred_on_connect.lock().remove(&p.id);
                                }
                                continue;
                            }
                            if due && connected {
                                let app_c = app.clone();
                                let pid = p.id.clone();
                                let do_sync = p.schedule_action == "sync";
                                let mode = p.schedule_mode.clone();
                                let d_done = Arc::clone(&last_daily_key);
                                let fail_map = Arc::clone(&last_failure);
                                let today_c = today_str.clone();
                                tauri::async_runtime::spawn(async move {
                                    let ok = run_locked_external_sync_background(
                                        &app_c, &pid, do_sync, false,
                                    )
                                    .await;
                                    if ok {
                                        fail_map.lock().remove(&key);
                                        if was_deferred {
                                            if let Some(st) = app_c.try_state::<AppState>() {
                                                st.deferred_on_connect.lock().remove(&pid);
                                            }
                                        }
                                        if mode == "daily" {
                                            d_done.lock().insert(key, today_c);
                                        }
                                    } else {
                                        fail_map.lock().insert(key, Instant::now());
                                    }
                                });
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

fn recently_attempted(map: &HashMap<String, Instant>, key: &str, cooldown_mins: u64) -> bool {
    map.get(key)
        .is_some_and(|t| t.elapsed() < Duration::from_secs(cooldown_mins * 60))
}

fn latest_timestamp<'a>(
    last_sync: Option<&'a str>,
    last_verify: Option<&'a str>,
    check_only: bool,
) -> Option<&'a str> {
    if !check_only {
        return last_sync;
    }
    match (last_sync, last_verify) {
        (Some(s), Some(v)) => {
            if v >= s {
                Some(v)
            } else {
                Some(s)
            }
        }
        (Some(s), None) => Some(s),
        (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

fn is_interval_due(last_iso: Option<&str>, now: DateTime<Utc>, hours: i64) -> bool {
    let Some(raw) = last_iso else {
        return true;
    };
    let Ok(last) = DateTime::parse_from_rfc3339(raw) else {
        return true;
    };
    (now - last.with_timezone(&Utc)).num_minutes() >= hours * 60
}

fn is_daily_due(
    hhmm: &str,
    now_local: DateTime<Local>,
    last_mem_day: Option<&str>,
    last_persisted_iso: Option<&str>,
    today: &str,
) -> bool {
    let parts: Vec<&str> = hhmm.trim().split(':').collect();
    if parts.len() != 2 {
        return false;
    }
    let (Ok(h), Ok(m)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
        return false;
    };
    let Some(target) = NaiveTime::from_hms_opt(h, m, 0) else {
        return false;
    };
    if now_local.time() < target {
        return false;
    }
    if last_mem_day == Some(today) {
        return false;
    }
    if let Some(raw) = last_persisted_iso {
        if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
            let local_dt = dt.with_timezone(&Local);
            // Only skip today's daily run if a persisted sync/verify ran AT OR AFTER today's scheduled target time
            if local_dt.date_naive() == now_local.date_naive() && local_dt.time() >= target {
                return false;
            }
        }
    }
    true
}
