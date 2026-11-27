use anyhow::{anyhow, Context, Result};
use chrono::{Local, Utc};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{
    is_mac_metadata_path, relative_normalized, AppSettings, ExternalProfile, ManifestEntry,
};
use truongsync_db::DbHandle;
use truongsync_geo::exiftool::{compute_file_md5, compute_file_md5_with_buf};
use truongsync_ingest::volumes::{disk_free_bytes, find_mounted_volume, DetectedVolume};
use walkdir::WalkDir;

use crate::cloud::{group_intentional_by_rel, join_rel_path, SyncDiffItem};

static EXT_PART_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalVerifyReport {
    pub profile_id: String,
    pub checked_at: String,
    pub volume_connected: bool,
    pub mounted_path: Option<String>,
    pub local_file_count: usize,
    pub external_file_count: usize,
    pub up_to_date_count: usize,
    pub to_push: Vec<SyncDiffItem>,
    pub to_pull: Vec<SyncDiffItem>,
    pub tombstoned_local: Vec<SyncDiffItem>,
    pub backfill_updates: Vec<SyncDiffItem>,
    pub conflicts: Vec<SyncDiffItem>,
    pub ignored_mac_metadata_count: usize,
    pub pending_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSyncResult {
    pub profile_id: String,
    pub pulled_count: usize,
    pub pushed_count: usize,
    pub backfill_replaced_count: usize,
    pub failed_count: usize,
    pub bytes_transferred: u64,
    pub duration_ms: u64,
    pub failures: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct FileEntry {
    abs_path: PathBuf,
    size: u64,
    mtime_unix: i64,
}

/// Strictly matches TruongSync / Backup-Photos timestamped marker files (never arbitrary user `.txt` notes).
fn is_timestamped_marker_filename(name: &str) -> bool {
    if !name.ends_with(".txt") || name.contains('/') || name.contains('\\') {
        return false;
    }
    let rest = name
        .strip_prefix("Last Sync - ")
        .or_else(|| name.strip_prefix("Last Backup - "))
        .or_else(|| name.strip_prefix("Last Backup "));
    let Some(r) = rest else {
        return false;
    };
    let b = r.as_bytes();
    b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn scan_dir(
    root: &Path,
    must_exist: bool,
    ignore_root_markers: bool,
    cancel: &CancellationToken,
) -> Result<(HashMap<String, FileEntry>, HashSet<String>, usize)> {
    let mut map = HashMap::new();
    let mut exact_collisions = HashSet::new();
    let mut ignored_mac = 0usize;
    if !root.exists() {
        if must_exist {
            return Err(anyhow!(
                "Directory does not exist or is disconnected: {}",
                root.display()
            ));
        }
        return Ok((map, exact_collisions, 0));
    }
    for entry_res in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || !is_mac_metadata_path(&e.file_name().to_string_lossy())
        })
    {
        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        let entry = entry_res
            .with_context(|| format!("failed reading directory inside {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        let Some(rel) = relative_normalized(root, p) else {
            continue;
        };
        let lower_rel = rel.to_ascii_lowercase();
        if is_mac_metadata_path(&rel)
            || lower_rel.ends_with(".truongsync.part")
            || lower_rel.ends_with(".partial")
            || lower_rel.ends_with("_exiftool_tmp")
            || (ignore_root_markers && is_timestamped_marker_filename(&rel))
        {
            ignored_mac += 1;
            continue;
        }
        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", p.display()))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if map.contains_key(&rel) {
            exact_collisions.insert(rel.clone());
        }
        map.insert(
            rel,
            FileEntry {
                abs_path: p.to_path_buf(),
                size: meta.len(),
                mtime_unix: mtime,
            },
        );
    }
    Ok((map, exact_collisions, ignored_mac))
}

pub fn resolve_external_target(profile: &ExternalProfile) -> Option<(DetectedVolume, PathBuf)> {
    let vol = find_mounted_volume(&profile.volume_uuid, &profile.volume_label)?;
    let rel = profile
        .relative_dest_path
        .trim()
        .trim_start_matches(['/', '\\']);
    let dest = if rel.is_empty() {
        PathBuf::from(&vol.mount_path)
    } else {
        join_rel_path(Path::new(&vol.mount_path), rel)
    };
    Some((vol, dest))
}

struct ExternalVerifyContext {
    report: ExternalVerifyReport,
    local_map: HashMap<String, FileEntry>,
    ext_map: HashMap<String, FileEntry>,
    intentional_by_rel: HashMap<String, (Vec<String>, HashSet<String>, String)>,
}

/// Runs a read-only audit comparing Local ↔ External Drive (strictly immutable on conflicts,
/// case-variant aware, per-target GPS backfill aware, and tombstone-safe).
pub fn verify_external_profile(
    profile: &ExternalProfile,
    db: &DbHandle,
    deep_md5: bool,
) -> Result<ExternalVerifyReport> {
    let cancel = CancellationToken::new();
    verify_external_profile_cancellable(profile, db, deep_md5, &cancel)
}

pub fn verify_external_profile_cancellable(
    profile: &ExternalProfile,
    db: &DbHandle,
    deep_md5: bool,
    cancel: &CancellationToken,
) -> Result<ExternalVerifyReport> {
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
    let Some((vol, ext_root)) = resolve_external_target(profile) else {
        return Ok(ExternalVerifyReport {
            profile_id: profile.id.clone(),
            checked_at: Utc::now().to_rfc3339(),
            volume_connected: false,
            mounted_path: None,
            local_file_count: 0,
            external_file_count: 0,
            up_to_date_count: 0,
            to_push: Vec::new(),
            to_pull: Vec::new(),
            tombstoned_local: Vec::new(),
            backfill_updates: Vec::new(),
            conflicts: Vec::new(),
            ignored_mac_metadata_count: 0,
            pending_bytes: 0,
        });
    };
    // Verify the resolved mount point is still present, capture device identity, and ensure target doesn't escape via symlink
    let mount_path = Path::new(&vol.mount_path);
    if !mount_path.is_dir() {
        return Err(anyhow!(
            "External drive '{}' disconnected before verification could scan it.",
            profile.volume_label
        ));
    }
    if !ext_root.starts_with(mount_path) {
        return Err(anyhow!(
            "External backup destination '{}' escapes volume mount '{}'.",
            ext_root.display(),
            mount_path.display()
        ));
    }
    crate::cloud::ensure_no_symlink_ancestors(mount_path, &ext_root)?;
    let expected_identity = capture_mount_identity(mount_path, &profile.volume_label)?;
    Ok(verify_external_roots_inner(
        profile,
        Path::new(&profile.local_root),
        &ext_root,
        Some(vol.mount_path.clone()),
        Some((mount_path, expected_identity.as_ref())),
        db,
        deep_md5,
        false,
        cancel,
    )?
    .report)
}

/// Core directory-based verification engine (callable directly with custom roots for deterministic testing).
#[allow(clippy::too_many_arguments)]
pub fn verify_external_roots(
    profile: &ExternalProfile,
    local_root: &Path,
    ext_root: &Path,
    db: &DbHandle,
    deep_md5: bool,
    restore_mode: bool,
    cancel: &CancellationToken,
) -> Result<ExternalVerifyReport> {
    Ok(verify_external_roots_inner(
        profile,
        local_root,
        ext_root,
        Some(ext_root.to_string_lossy().to_string()),
        None,
        db,
        deep_md5,
        restore_mode,
        cancel,
    )?
    .report)
}

#[allow(clippy::too_many_arguments)]
fn verify_external_roots_inner(
    profile: &ExternalProfile,
    local_root: &Path,
    ext_root: &Path,
    mounted_path: Option<String>,
    mount_guard: Option<(&Path, Option<&MountIdentity>)>,
    db: &DbHandle,
    deep_md5: bool,
    restore_mode: bool,
    cancel: &CancellationToken,
) -> Result<ExternalVerifyContext> {
    let ext_root_exists = ext_root.exists();
    let scan_cancel = cancel.child_token();
    let (local_res, ext_res) = std::thread::scope(|s| {
        let sc_local = scan_cancel.clone();
        let local_handle = s.spawn(move || {
            let r = scan_dir(local_root, !restore_mode, false, &sc_local);
            if r.is_err() {
                sc_local.cancel();
            }
            r
        });
        let ext_res = scan_dir(ext_root, false, true, &scan_cancel);
        if ext_res.is_err() {
            scan_cancel.cancel();
        }
        let local_res = local_handle
            .join()
            .unwrap_or_else(|_| Err(anyhow!("local scan thread panicked")));
        (local_res, ext_res)
    });
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
    let ((local_map, local_exact_dups, local_mac), (ext_map, ext_exact_dups, ext_mac)) =
        match (local_res, ext_res) {
            (Ok(l), Ok(e)) => (l, e),
            (Err(le), Err(ee)) => {
                if le.to_string() == "Sync cancelled." {
                    return Err(ee);
                }
                return Err(le);
            }
            (Err(le), _) => return Err(le),
            (_, Err(ee)) => return Err(ee),
        };
    // Immediately verify the external mount identity survived the directory walk so a
    // mid-scan disconnect never misclassifies the entire library as "missing on external".
    check_mount_device_identity(mount_guard)?;

    let manifest = db.load_manifest_map("external", &profile.id)?;
    let pending_updates =
        db.list_pending_intentional_updates_for_target("external", &profile.id)?;
    let intentional_by_rel = group_intentional_by_rel(pending_updates, local_root);

    // Group by lowercase on BOTH sides so same-side case variants are quarantined cleanly
    let mut local_lower_groups: HashMap<String, Vec<String>> = HashMap::new();
    for k in local_map.keys() {
        local_lower_groups
            .entry(k.to_lowercase())
            .or_default()
            .push(k.clone());
    }
    let mut ext_lower_groups: HashMap<String, Vec<String>> = HashMap::new();
    for k in ext_map.keys() {
        ext_lower_groups
            .entry(k.to_lowercase())
            .or_default()
            .push(k.clone());
    }

    // Only prune stale manifest rows when ext_root actually exists (never wipe tombstones on renamed folder!)
    if ext_root_exists {
        let stale_manifest: Vec<String> = manifest
            .keys()
            .filter(|k| !local_map.contains_key(*k) && !ext_map.contains_key(*k))
            .cloned()
            .collect();
        if !stale_manifest.is_empty() {
            let _ = db.prune_manifest_entries("external", &profile.id, &stale_manifest);
        }
    }

    let mut keys: Vec<&String> = local_map
        .keys()
        .chain(ext_map.keys())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    keys.sort();

    // Pre-compute needed MD5s (deep_md5 or GPS backfill candidates) in parallel across up to 4 threads
    // using reused per-thread 1 MiB buffers.
    let mut md5_targets: HashSet<&Path> = HashSet::new();
    for &rel in &keys {
        if local_exact_dups.contains(rel) || ext_exact_dups.contains(rel) {
            continue;
        }
        let lower = rel.to_lowercase();
        let loc_bucket_len = local_lower_groups.get(&lower).map_or(0, Vec::len);
        let ext_bucket_len = ext_lower_groups.get(&lower).map_or(0, Vec::len);
        if loc_bucket_len > 1 || ext_bucket_len > 1 {
            continue;
        }
        match (local_map.get(rel), ext_map.get(rel)) {
            (Some(l), Some(e)) => {
                if intentional_by_rel.contains_key(rel) || (deep_md5 && l.size == e.size) {
                    md5_targets.insert(l.abs_path.as_path());
                    md5_targets.insert(e.abs_path.as_path());
                }
            }
            (Some(l), None) if deep_md5 => {
                if let Some(ext_variant) = ext_lower_groups.get(&lower).and_then(|v| v.first()) {
                    let e = &ext_map[ext_variant];
                    if l.size == e.size {
                        md5_targets.insert(l.abs_path.as_path());
                        md5_targets.insert(e.abs_path.as_path());
                    }
                }
            }
            (None, Some(e)) if deep_md5 => {
                if let Some(loc_variant) = local_lower_groups.get(&lower).and_then(|v| v.first()) {
                    let l = &local_map[loc_variant];
                    if l.size == e.size {
                        md5_targets.insert(l.abs_path.as_path());
                        md5_targets.insert(e.abs_path.as_path());
                    }
                }
            }
            _ => {}
        }
    }
    let precomputed_md5s: HashMap<PathBuf, Result<String, String>> = if md5_targets.len() > 1 {
        let targets: Vec<PathBuf> = md5_targets.into_iter().map(Path::to_path_buf).collect();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 4);
        if let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
            pool.install(|| {
                targets
                    .par_iter()
                    .map_init(
                        || vec![0u8; 1024 * 1024],
                        |buf, p| {
                            if cancel.is_cancelled() {
                                return (p.clone(), Err("Sync cancelled.".to_string()));
                            }
                            let res = compute_file_md5_with_buf(p, buf).map_err(|e| e.to_string());
                            (p.clone(), res)
                        },
                    )
                    .collect()
            })
        } else {
            let mut buf = vec![0u8; 1024 * 1024];
            targets
                .into_iter()
                .map(|p| {
                    if cancel.is_cancelled() {
                        return (p, Err("Sync cancelled.".to_string()));
                    }
                    let res = compute_file_md5_with_buf(&p, &mut buf).map_err(|e| e.to_string());
                    (p, res)
                })
                .collect()
        }
    } else if md5_targets.len() == 1 {
        let p = md5_targets.into_iter().next().unwrap().to_path_buf();
        let res = if cancel.is_cancelled() {
            Err("Sync cancelled.".to_string())
        } else {
            compute_file_md5(&p).map_err(|e| e.to_string())
        };
        HashMap::from([(p, res)])
    } else {
        HashMap::new()
    };
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
    if !precomputed_md5s.is_empty() {
        check_mount_device_identity(mount_guard)?;
    }
    let lookup_md5 = |p: &Path| -> Result<String, String> {
        if let Some(res) = precomputed_md5s.get(p) {
            return res.clone();
        }
        compute_file_md5(p).map_err(|e| e.to_string())
    };

    let two_way = profile.direction == "two_way_additive";
    let mut to_push = Vec::new();
    let mut to_pull = Vec::new();
    let mut tombstoned_local = Vec::new();
    let mut backfill_updates = Vec::new();
    let mut conflicts = Vec::new();
    let mut up_to_date_count = 0usize;
    let mut pending_bytes = 0u64;
    let mut new_tombstones = Vec::new();
    let mut confirmed_both: Vec<ManifestEntry> = Vec::new();
    let mut auto_applied_update_ids: Vec<String> = Vec::new();
    let mut handled_case_buckets: HashSet<String> = HashSet::new();

    for rel in keys {
        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        let loc = local_map.get(rel);
        let ext = ext_map.get(rel);

        if local_exact_dups.contains(rel) || ext_exact_dups.contains(rel) {
            conflicts.push(SyncDiffItem {
                rel_path: rel.clone(),
                direction: "conflict".into(),
                local_bytes: loc.map(|l| l.size),
                remote_bytes: ext.map(|e| e.size),
                note: Some(
                    "Multiple files normalize to this path (NFC/NFD collision); quarantined".into(),
                ),
            });
            continue;
        }

        let lower = rel.to_lowercase();
        let loc_bucket_len = local_lower_groups.get(&lower).map_or(0, Vec::len);
        let ext_bucket_len = ext_lower_groups.get(&lower).map_or(0, Vec::len);
        if loc_bucket_len > 1 || ext_bucket_len > 1 {
            conflicts.push(SyncDiffItem {
                rel_path: rel.clone(),
                direction: "conflict".into(),
                local_bytes: loc.map(|l| l.size),
                remote_bytes: ext.map(|e| e.size),
                note: Some(
                    "Multiple files differ only by letter case; quarantined to prevent overwrite on case-insensitive drives"
                        .into(),
                ),
            });
            continue;
        }

        match (loc, ext) {
            (Some(l), None) => {
                if let Some(ext_variant) = ext_lower_groups.get(&lower).and_then(|v| v.first()) {
                    if handled_case_buckets.insert(lower) {
                        let e = &ext_map[ext_variant];
                        let same = if l.size != e.size {
                            false
                        } else if deep_md5 {
                            match (lookup_md5(&l.abs_path), lookup_md5(&e.abs_path)) {
                                (Ok(lm), Ok(em)) => lm == em,
                                _ => false,
                            }
                        } else {
                            true
                        };
                        if same {
                            up_to_date_count += 1;
                        } else {
                            conflicts.push(SyncDiffItem {
                                rel_path: rel.clone(),
                                direction: "conflict".into(),
                                local_bytes: Some(l.size),
                                remote_bytes: Some(e.size),
                                note: Some(format!(
                                    "Case-variant collision with external '{ext_variant}'; protected from silent overwrite"
                                )),
                            });
                        }
                    }
                    continue;
                }
                pending_bytes = pending_bytes.saturating_add(l.size);
                to_push.push(SyncDiffItem {
                    rel_path: rel.clone(),
                    direction: "upload".into(),
                    local_bytes: Some(l.size),
                    remote_bytes: None,
                    note: None,
                });
            }
            (None, Some(e)) => {
                if let Some(loc_variant) = local_lower_groups.get(&lower).and_then(|v| v.first()) {
                    if handled_case_buckets.insert(lower) {
                        let l = &local_map[loc_variant];
                        let same = if l.size != e.size {
                            false
                        } else if deep_md5 {
                            match (lookup_md5(&l.abs_path), lookup_md5(&e.abs_path)) {
                                (Ok(lm), Ok(em)) => lm == em,
                                _ => false,
                            }
                        } else {
                            true
                        };
                        if same {
                            up_to_date_count += 1;
                        } else {
                            conflicts.push(SyncDiffItem {
                                rel_path: rel.clone(),
                                direction: "conflict".into(),
                                local_bytes: Some(l.size),
                                remote_bytes: Some(e.size),
                                note: Some(format!(
                                    "Case-variant collision with local '{loc_variant}'; protected from silent overwrite"
                                )),
                            });
                        }
                    }
                    continue;
                }

                let was_synced = manifest.contains_key(rel);
                if was_synced && !restore_mode {
                    new_tombstones.push(rel.clone());
                    tombstoned_local.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: "tombstone_restore".into(),
                        local_bytes: None,
                        remote_bytes: Some(e.size),
                        note: Some(
                            "Deleted locally; preserved on external drive (use Restore to pull back)"
                                .into(),
                        ),
                    });
                } else if two_way || restore_mode {
                    pending_bytes = pending_bytes.saturating_add(e.size);
                    to_pull.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: if was_synced {
                            "tombstone_restore".into()
                        } else {
                            "download".into()
                        },
                        local_bytes: None,
                        remote_bytes: Some(e.size),
                        note: None,
                    });
                }
            }
            (Some(l), Some(e)) => {
                if let Some((ids, pre_set, post_md5)) = intentional_by_rel.get(rel) {
                    if let (Ok(ext_md5), Ok(loc_md5)) =
                        (lookup_md5(&e.abs_path), lookup_md5(&l.abs_path))
                    {
                        if ext_md5.eq_ignore_ascii_case(post_md5) {
                            auto_applied_update_ids.extend(ids.iter().cloned());
                        } else if pre_set.contains(&ext_md5.to_ascii_lowercase())
                            && loc_md5.eq_ignore_ascii_case(post_md5)
                        {
                            pending_bytes = pending_bytes.saturating_add(l.size);
                            backfill_updates.push(SyncDiffItem {
                                rel_path: rel.clone(),
                                direction: "backfill_replace".into(),
                                local_bytes: Some(l.size),
                                remote_bytes: Some(e.size),
                                note: Some(
                                    "Safe GPS backfill update (external matches pre-tag MD5)"
                                        .into(),
                                ),
                            });
                            continue;
                        }
                    }
                }

                let size_same = l.size == e.size;
                let mut hash_error_note: Option<String> = None;
                let content_same = if !size_same {
                    false
                } else if deep_md5 {
                    match (lookup_md5(&l.abs_path), lookup_md5(&e.abs_path)) {
                        (Ok(lm), Ok(em)) => lm == em,
                        (Err(err), _) => {
                            hash_error_note = Some(format!(
                                "Failed reading local file for MD5 verification: {err}"
                            ));
                            false
                        }
                        (_, Err(err)) => {
                            hash_error_note = Some(format!(
                                "Failed reading external file for MD5 verification: {err}"
                            ));
                            false
                        }
                    }
                } else {
                    true
                };

                if content_same {
                    up_to_date_count += 1;
                    let needs_upsert = match manifest.get(rel) {
                        Some(existing) => {
                            existing.size_bytes != l.size as i64
                                || existing.mtime_unix != l.mtime_unix
                                || existing.state != "synced"
                        }
                        None => true,
                    };
                    if needs_upsert {
                        confirmed_both.push(ManifestEntry {
                            profile_kind: "external".into(),
                            profile_id: profile.id.clone(),
                            rel_path: rel.clone(),
                            size_bytes: l.size as i64,
                            md5_hash: None,
                            mtime_unix: l.mtime_unix,
                            state: "synced".into(),
                        });
                    }
                } else {
                    conflicts.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: "conflict".into(),
                        local_bytes: Some(l.size),
                        remote_bytes: Some(e.size),
                        note: hash_error_note.or_else(|| {
                            Some(
                                "Same path differs between PC and external drive; protected from silent overwrite"
                                    .into(),
                            )
                        }),
                    });
                }
            }
            (None, None) => {}
        }
    }

    if !confirmed_both.is_empty() {
        let _ = db.upsert_manifest_entries(&confirmed_both);
    }
    if !new_tombstones.is_empty() {
        let _ = db.mark_local_tombstones("external", &profile.id, &new_tombstones);
    }
    if !auto_applied_update_ids.is_empty() {
        let _ =
            db.mark_intentional_updates_applied(&auto_applied_update_ids, "external", &profile.id);
    }

    let status = if !conflicts.is_empty() {
        "needs_review"
    } else if to_push.is_empty() && to_pull.is_empty() && backfill_updates.is_empty() {
        "up_to_date"
    } else {
        "behind"
    };

    if !restore_mode {
        let _ = db.update_external_freshness(
            &profile.id,
            false,
            true,
            status,
            None,
            (to_push.len() + backfill_updates.len()) as i64,
            to_pull.len() as i64,
            pending_bytes as i64,
        );
    }

    let report = ExternalVerifyReport {
        profile_id: profile.id.clone(),
        checked_at: Utc::now().to_rfc3339(),
        volume_connected: true,
        mounted_path,
        local_file_count: local_map.len(),
        external_file_count: ext_map.len(),
        up_to_date_count,
        to_push,
        to_pull,
        tombstoned_local,
        backfill_updates,
        conflicts,
        ignored_mac_metadata_count: local_mac + ext_mac,
        pending_bytes,
    };

    Ok(ExternalVerifyContext {
        report,
        local_map,
        ext_map,
        intentional_by_rel,
    })
}

struct ExtPartGuard {
    path: PathBuf,
    armed: bool,
}

impl ExtPartGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ExtPartGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn commit_part_file(part: &Path, dst: &Path, allow_overwrite: bool) -> Result<()> {
    if !allow_overwrite {
        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let src_c = CString::new(part.as_os_str().as_bytes())?;
            let dst_c = CString::new(dst.as_os_str().as_bytes())?;
            let rc = unsafe { libc::renamex_np(src_c.as_ptr(), dst_c.as_ptr(), libc::RENAME_EXCL) };
            if rc == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::AlreadyExists {
                return Err(anyhow!(
                    "Refusing to overwrite existing destination file: {}",
                    dst.display()
                ));
            }
            let raw = err.raw_os_error().unwrap_or(0);
            if raw != libc::ENOTSUP && raw != libc::EOPNOTSUPP && raw != libc::EINVAL {
                return Err(err.into());
            }
        }
        #[cfg(unix)]
        {
            match std::fs::hard_link(part, dst) {
                Ok(()) => {
                    let _ = std::fs::remove_file(part);
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    return Err(anyhow!(
                        "Refusing to overwrite existing destination file: {}",
                        dst.display()
                    ));
                }
                Err(e) => {
                    let raw = e.raw_os_error().unwrap_or(0);
                    if raw == libc::ENOTSUP
                        || raw == libc::EOPNOTSUPP
                        || raw == libc::ENOSYS
                        || raw == libc::EINVAL
                        || raw == libc::EPERM
                    {
                        if dst.symlink_metadata().is_ok() {
                            return Err(anyhow!(
                                "Refusing to overwrite existing destination file: {}",
                                dst.display()
                            ));
                        }
                        std::fs::rename(part, dst)?;
                        return Ok(());
                    }
                    return Err(e.into());
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
            let to_wide_verbatim = |p: &Path| -> Vec<u16> {
                let norm = p.to_string_lossy().replace('/', r"\");
                let formatted = if norm.encode_utf16().count() >= 240 && !norm.starts_with(r"\\?\")
                {
                    if let Some(unc) = norm.strip_prefix(r"\\") {
                        format!(r"\\?\UNC\{unc}")
                    } else {
                        format!(r"\\?\{norm}")
                    }
                } else {
                    norm
                };
                std::ffi::OsStr::new(&formatted)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect()
            };
            let src_w = to_wide_verbatim(part);
            let dst_w = to_wide_verbatim(dst);
            let mut last_err = None;
            for attempt in 0..4 {
                match unsafe {
                    MoveFileExW(
                        PCWSTR(src_w.as_ptr()),
                        PCWSTR(dst_w.as_ptr()),
                        MOVEFILE_WRITE_THROUGH,
                    )
                } {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        // Retry briefly on Windows Defender / Explorer transient sharing/lock violations
                        // (ERROR_ACCESS_DENIED = 5, ERROR_SHARING_VIOLATION = 32, ERROR_LOCK_VIOLATION = 33).
                        let code = (e.code().0 as u32) & 0xFFFF;
                        if matches!(code, 5 | 32 | 33) && attempt < 3 {
                            last_err = Some(e);
                            std::thread::sleep(std::time::Duration::from_millis(
                                120 * (attempt + 1),
                            ));
                            continue;
                        }
                        return Err(anyhow!(
                            "Refusing to overwrite or move failed for {}: {e}",
                            dst.display()
                        ));
                    }
                }
            }
            return Err(anyhow!(
                "Refusing to overwrite or move failed for {}: {:?}",
                dst.display(),
                last_err
            ));
        }
    }

    // Verified GPS backfill replacement (`allow_overwrite == true`):
    // Handle Windows read-only attribute & brief Defender/Explorer sharing locks cleanly.
    #[cfg(windows)]
    {
        if let Ok(meta) = std::fs::metadata(dst) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                // Windows-only: clears FILE_ATTRIBUTE_READONLY (does not affect Unix permissions).
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(dst, perms);
            }
        }
        let mut last_err = None;
        for attempt in 0..4 {
            match std::fs::rename(part, dst) {
                Ok(()) => return Ok(()),
                Err(e)
                    if (e.kind() == ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(5 | 32 | 33)))
                        && attempt < 3 =>
                {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(120 * (attempt + 1)));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("rename failed {} -> {}", part.display(), dst.display())
                    })
                }
            }
        }
        Err(anyhow!(
            "rename failed {} -> {}: {:?}",
            part.display(),
            dst.display(),
            last_err
        ))
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(part, dst)
            .with_context(|| format!("rename failed {} -> {}", part.display(), dst.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum MountIdentity {
    #[cfg(unix)]
    UnixDev(u64),
    #[cfg(windows)]
    WindowsVolumeGuid(String),
    #[cfg(not(any(unix, windows)))]
    None,
}

#[cfg(windows)]
fn query_windows_volume_guid(mount_root: &Path) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

    let mut s = mount_root.to_string_lossy().to_string();
    if !s.ends_with('\\') && !s.ends_with('/') {
        s.push('\\');
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut guid_buf = [0u16; 128];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut guid_buf) }.ok()?;
    let end = guid_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(guid_buf.len());
    let guid = OsString::from_wide(&guid_buf[..end])
        .to_string_lossy()
        .trim()
        .to_string();
    (!guid.is_empty()).then_some(guid)
}

fn check_mount_device_identity(mount_check: Option<(&Path, Option<&MountIdentity>)>) -> Result<()> {
    let Some((mount_root, expected_id)) = mount_check else {
        return Ok(());
    };
    let meta = std::fs::metadata(mount_root).map_err(|_| {
        anyhow!(
            "External drive disconnected during sync ({}).",
            mount_root.display()
        )
    })?;
    #[cfg(unix)]
    if let Some(MountIdentity::UnixDev(exp)) = expected_id {
        use std::os::unix::fs::MetadataExt;
        if meta.dev() != *exp {
            return Err(anyhow!(
                "External drive device changed unexpectedly during sync ({}).",
                mount_root.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        let _ = meta;
        if let Some(MountIdentity::WindowsVolumeGuid(exp)) = expected_id {
            match query_windows_volume_guid(mount_root) {
                Some(cur) if cur.eq_ignore_ascii_case(exp) => {}
                Some(_) => {
                    return Err(anyhow!(
                        "External drive volume identity changed unexpectedly during sync ({}).",
                        mount_root.display()
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "External drive disconnected during sync ({}).",
                        mount_root.display()
                    ));
                }
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (meta, expected_id);
    Ok(())
}

fn capture_mount_identity(mount_path: &Path, volume_label: &str) -> Result<Option<MountIdentity>> {
    let mount_meta = std::fs::metadata(mount_path).map_err(|_| {
        anyhow!(
            "External drive '{}' disconnected before operation could start.",
            volume_label
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(MountIdentity::UnixDev(mount_meta.dev())))
    }
    #[cfg(windows)]
    {
        let _ = mount_meta;
        Ok(query_windows_volume_guid(mount_path).map(MountIdentity::WindowsVolumeGuid))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = mount_meta;
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn atomic_copy_preserve_mtime(
    dst_root: &Path,
    src: &Path,
    dst: &Path,
    allow_overwrite: bool,
    expected_src_md5: Option<&str>,
    expected_dst_pre_md5s: Option<&HashSet<String>>,
    skip_parent_setup: bool,
    buf: &mut [u8],
    cancel: &CancellationToken,
) -> Result<(u64, i64)> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    if !skip_parent_setup {
        crate::cloud::ensure_no_symlink_ancestors(dst_root, dst)?;
        std::fs::create_dir_all(parent)?;
    } else if let Ok(meta) = dst.symlink_metadata() {
        if meta.file_type().is_symlink() {
            return Err(anyhow!(
                "Refusing to traverse symlink inside sync root: {}",
                dst.display()
            ));
        }
        if !allow_overwrite {
            return Err(anyhow!(
                "Destination file already exists; strictly-additive sync refused overwrite: {}",
                dst.display()
            ));
        }
    }
    if !skip_parent_setup && !allow_overwrite && dst.symlink_metadata().is_ok() {
        return Err(anyhow!(
            "Destination file already exists; strictly-additive sync refused overwrite: {}",
            dst.display()
        ));
    }

    let meta = std::fs::metadata(src)?;
    let expected = meta.len();
    let mtime = meta.modified().ok();
    let mtime_unix = mtime
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let seq = EXT_PART_SEQ.fetch_add(1, Ordering::Relaxed);
    // Short fixed-length sibling temp file prevents ENAMETOOLONG / Windows MAX_PATH overflow
    let part = parent.join(format!(
        ".ts-ext.{}.{}.truongsync.part",
        std::process::id(),
        seq
    ));
    let mut guard = ExtPartGuard::new(part.clone());

    let mut r = File::open(src)?;
    let mut w = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)?;
    use md5::Digest;
    let mut copied = 0u64;
    let mut hasher = expected_src_md5.map(|_| md5::Md5::new());
    loop {
        if cancel.is_cancelled() {
            drop(w);
            return Err(anyhow!("Sync cancelled."));
        }
        let n = match r.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        w.write_all(&buf[..n])?;
        if let Some(ref mut h) = hasher {
            use md5::Digest;
            h.update(&buf[..n]);
        }
        copied += n as u64;
    }
    // Flush data BEFORE set_modified so exFAT/FAT32/Windows flush never clobbers preserved mtime,
    // then issue a single full sync_all() durability barrier.
    if let Some(mt) = mtime {
        w.sync_data()?;
        let _ = w.set_modified(mt);
    }
    w.sync_all()?;
    drop(w);
    if copied != expected {
        return Err(anyhow!("size mismatch copying {}", src.display()));
    }
    if let (Some(exp_md5), Some(h)) = (expected_src_md5, hasher) {
        use md5::Digest;
        let streamed = hex::encode(h.finalize());
        if !streamed.eq_ignore_ascii_case(exp_md5) {
            return Err(anyhow!(
                "Aborted GPS backfill: source bytes changed mid-copy for {}",
                src.display()
            ));
        }
    }
    if let Some(pre_set) = expected_dst_pre_md5s {
        let live_dst_md5 = compute_file_md5_with_buf(dst, buf)?;
        if !pre_set.contains(&live_dst_md5.to_ascii_lowercase()) {
            return Err(anyhow!(
                "Aborted GPS backfill: destination changed before atomic replace for {}",
                dst.display()
            ));
        }
    }
    commit_part_file(&part, dst, allow_overwrite)?;
    guard.disarm();
    Ok((copied, mtime_unix))
}

/// Runs 2-way strictly-additive sync with an external drive (matching `Backup-Photos.ps1`).
#[allow(clippy::too_many_arguments)]
pub fn sync_external_profile<F>(
    profile: &ExternalProfile,
    db: &DbHandle,
    settings: &AppSettings,
    restore_mode: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<ExternalSyncResult>
where
    F: Fn(&str, usize, usize, u64) + Send + Sync,
{
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
    let Some((vol, ext_root)) = resolve_external_target(profile) else {
        return Err(anyhow!(
            "External drive '{}' is not connected. Plug it in and try again.",
            profile.volume_label
        ));
    };
    // Verify the resolved mount point is still present, does not escape via symlinks,
    // and record expected device/volume identity (Unix dev_t / Windows volume GUID).
    let mount_path = PathBuf::from(&vol.mount_path);
    if !ext_root.starts_with(&mount_path) {
        return Err(anyhow!(
            "External backup destination '{}' escapes volume mount '{}'.",
            ext_root.display(),
            mount_path.display()
        ));
    }
    crate::cloud::ensure_no_symlink_ancestors(&mount_path, &ext_root)?;
    let expected_identity = capture_mount_identity(&mount_path, &profile.volume_label)?;

    sync_external_roots_with_mount_guard(
        profile,
        Path::new(&profile.local_root),
        &ext_root,
        Some((&mount_path, expected_identity.as_ref())),
        vol.writable,
        Some(vol.free_bytes),
        db,
        settings,
        restore_mode,
        cancel,
        on_progress,
    )
}

/// Core directory-based sync engine (callable directly with custom roots for deterministic testing).
#[allow(clippy::too_many_arguments)]
pub fn sync_external_roots<F>(
    profile: &ExternalProfile,
    local_root: &Path,
    ext_root: &Path,
    ext_writable: bool,
    ext_free_bytes: Option<u64>,
    db: &DbHandle,
    settings: &AppSettings,
    restore_mode: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<ExternalSyncResult>
where
    F: Fn(&str, usize, usize, u64) + Send + Sync,
{
    sync_external_roots_with_mount_guard(
        profile,
        local_root,
        ext_root,
        None,
        ext_writable,
        ext_free_bytes,
        db,
        settings,
        restore_mode,
        cancel,
        on_progress,
    )
}

#[allow(clippy::too_many_arguments)]
fn sync_external_roots_with_mount_guard<F>(
    profile: &ExternalProfile,
    local_root: &Path,
    ext_root: &Path,
    mount_guard: Option<(&Path, Option<&MountIdentity>)>,
    ext_writable: bool,
    ext_free_bytes: Option<u64>,
    db: &DbHandle,
    settings: &AppSettings,
    restore_mode: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<ExternalSyncResult>
where
    F: Fn(&str, usize, usize, u64) + Send + Sync,
{
    let started = Instant::now();
    check_mount_device_identity(mount_guard)?;
    if !local_root.is_dir() {
        if !restore_mode {
            return Err(anyhow!(
                "Local sync root does not exist or is disconnected: {} (use Restore if hydrating a new folder)",
                local_root.display()
            ));
        }
        disk_free_bytes(local_root)?;
        std::fs::create_dir_all(local_root)?;
    }

    let ctx = verify_external_roots_inner(
        profile,
        local_root,
        ext_root,
        Some(ext_root.to_string_lossy().to_string()),
        mount_guard,
        db,
        false,
        restore_mode,
        cancel,
    )?;
    let report = ctx.report;

    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }

    let pulls: Vec<String> = report.to_pull.iter().map(|i| i.rel_path.clone()).collect();
    let pushes: Vec<String> = if restore_mode {
        Vec::new()
    } else {
        report.to_push.iter().map(|i| i.rel_path.clone()).collect()
    };
    let backfills: Vec<String> = if restore_mode {
        Vec::new()
    } else {
        report
            .backfill_updates
            .iter()
            .map(|i| i.rel_path.clone())
            .collect()
    };

    if (!pushes.is_empty() || !backfills.is_empty()) && !ext_writable {
        return Err(anyhow!(
            "External drive '{}' is mounted read-only (e.g., NTFS on macOS) and cannot accept backups.",
            profile.volume_label
        ));
    }

    // Free-space preflight checks before starting transfers
    let pull_bytes: u64 = report.to_pull.iter().filter_map(|i| i.remote_bytes).sum();
    if pull_bytes > 0 {
        if let Ok(local_free) = disk_free_bytes(local_root) {
            if pull_bytes > local_free {
                return Err(anyhow!(
                    "Insufficient free space on local disk ({local_free} bytes free, {pull_bytes} bytes needed to pull)."
                ));
            }
        }
    }
    let push_bytes: u64 = if restore_mode {
        0
    } else {
        report
            .to_push
            .iter()
            .filter_map(|i| i.local_bytes)
            .sum::<u64>()
    };
    if push_bytes > 0 {
        if let Some(ext_free) = ext_free_bytes {
            if push_bytes > ext_free {
                return Err(anyhow!(
                    "Insufficient free space on external drive '{}' ({ext_free} bytes free, {push_bytes} bytes needed).",
                    profile.volume_label
                ));
            }
        }
    }

    if !pushes.is_empty() || !backfills.is_empty() {
        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        check_mount_device_identity(mount_guard)?;
        std::fs::create_dir_all(ext_root)?;
    }

    // Lazily validate symlink safety and create destination parent directories at most once
    // per unique parent ONLY when a file in that directory is actively about to be copied
    // (avoiding orphaned empty directory trees on early cancel or drive disconnect).
    let validated_local_parents: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    let validated_ext_parents: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    let ensure_parent_cached = |root: &Path, dst: &Path, cache: &Mutex<HashSet<PathBuf>>| -> bool {
        let parent = dst.parent().unwrap_or(root);
        if cache.lock().contains(parent) {
            return true;
        }
        if crate::cloud::ensure_no_symlink_ancestors(root, parent).is_ok()
            && std::fs::create_dir_all(parent).is_ok()
        {
            cache.lock().insert(parent.to_path_buf());
            return true;
        }
        false
    };

    let total_ops = pulls.len() + pushes.len() + backfills.len();
    let done = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let pulled = Arc::new(AtomicUsize::new(0));
    let pushed = Arc::new(AtomicUsize::new(0));
    let backfilled = Arc::new(AtomicUsize::new(0));
    let pull_failures = Arc::new(AtomicUsize::new(0));
    let push_failures = Arc::new(AtomicUsize::new(0));
    let failures: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let succeeded_rels: Arc<Mutex<Vec<(String, u64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let applied_update_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let last_mount_check_ms = AtomicU64::new(0);
    let last_progress_emit = Mutex::new(Instant::now() - std::time::Duration::from_secs(1));
    // Use a child cancellation token for drive-disconnect aborts so we stop in-flight copies
    // immediately WITHOUT poisoning the caller's `cancel` token (which would misreport
    // disconnects as user cancellation and abort multi-drive pipelines).
    let worker_cancel = cancel.child_token();
    let disconnect_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let maybe_check_mount = |force: bool| -> Result<()> {
        if mount_guard.is_none() {
            return Ok(());
        }
        let now_ms = started.elapsed().as_millis() as u64;
        if !force {
            let prev = last_mount_check_ms.load(Ordering::Relaxed);
            if now_ms.saturating_sub(prev) < 200 {
                return Ok(());
            }
            if last_mount_check_ms
                .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return Ok(());
            }
        }
        check_mount_device_identity(mount_guard)
    };

    let emit_progress = |msg: &str, d: usize, b: u64| {
        let should = if d >= total_ops {
            true
        } else {
            let mut g = last_progress_emit.lock();
            if g.elapsed().as_millis() >= 80 {
                *g = Instant::now();
                true
            } else {
                false
            }
        };
        if should {
            on_progress(msg, d, total_ops, b);
        }
    };

    let workers = settings.import_threads.clamp(1, 8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("ts-ext-{i}"))
        .build()?;

    let local_map = &ctx.local_map;
    let ext_map = &ctx.ext_map;
    let grouped_updates = &ctx.intentional_by_rel;

    pool.install(|| {
        // Pass 1: External -> PC (strictly non-overwriting!)
        pulls.par_iter().for_each_init(
            || vec![0u8; 2 * 1024 * 1024],
            |io_buf, rel| {
                if worker_cancel.is_cancelled() {
                    return;
                }
                if let Err(e) = maybe_check_mount(false) {
                    pull_failures.fetch_add(1, Ordering::Relaxed);
                    let msg = e.to_string();
                    let mut d_err = disconnect_err.lock();
                    if d_err.is_none() {
                        *d_err = Some(msg.clone());
                    }
                    drop(d_err);
                    failures.lock().push((rel.clone(), msg));
                    worker_cancel.cancel();
                    return;
                }
                let src = ext_map
                    .get(rel)
                    .map(|e| e.abs_path.clone())
                    .unwrap_or_else(|| join_rel_path(ext_root, rel));
                let dst = join_rel_path(local_root, rel);
                let skip_parent = ensure_parent_cached(local_root, &dst, &validated_local_parents);
                match atomic_copy_preserve_mtime(
                    local_root,
                    &src,
                    &dst,
                    false,
                    None,
                    None,
                    skip_parent,
                    io_buf,
                    &worker_cancel,
                ) {
                    Ok((n, mtime_unix)) => {
                        pulled.fetch_add(1, Ordering::Relaxed);
                        succeeded_rels.lock().push((rel.clone(), n, mtime_unix));
                        let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        emit_progress(&format!("Pulling {rel}"), d, b);
                    }
                    Err(e) if !worker_cancel.is_cancelled() => {
                        if let Err(me) = maybe_check_mount(true) {
                            let msg = me.to_string();
                            let mut d_err = disconnect_err.lock();
                            if d_err.is_none() {
                                *d_err = Some(msg.clone());
                            }
                            drop(d_err);
                            pull_failures.fetch_add(1, Ordering::Relaxed);
                            failures.lock().push((rel.clone(), msg));
                            worker_cancel.cancel();
                            return;
                        }
                        pull_failures.fetch_add(1, Ordering::Relaxed);
                        failures.lock().push((rel.clone(), e.to_string()));
                    }
                    Err(_) => {}
                }
            },
        );

        // Pass 2: PC -> External (strictly non-overwriting!)
        pushes.par_iter().for_each_init(
            || vec![0u8; 2 * 1024 * 1024],
            |io_buf, rel| {
                if worker_cancel.is_cancelled() {
                    return;
                }
                if let Err(e) = maybe_check_mount(false) {
                    push_failures.fetch_add(1, Ordering::Relaxed);
                    let msg = e.to_string();
                    let mut d_err = disconnect_err.lock();
                    if d_err.is_none() {
                        *d_err = Some(msg.clone());
                    }
                    drop(d_err);
                    failures.lock().push((rel.clone(), msg));
                    worker_cancel.cancel();
                    return;
                }
                let src = local_map
                    .get(rel)
                    .map(|l| l.abs_path.clone())
                    .unwrap_or_else(|| join_rel_path(local_root, rel));
                let dst = join_rel_path(ext_root, rel);
                let skip_parent = ensure_parent_cached(ext_root, &dst, &validated_ext_parents);
                match atomic_copy_preserve_mtime(
                    ext_root,
                    &src,
                    &dst,
                    false,
                    None,
                    None,
                    skip_parent,
                    io_buf,
                    &worker_cancel,
                ) {
                    Ok((n, mtime_unix)) => {
                        pushed.fetch_add(1, Ordering::Relaxed);
                        succeeded_rels.lock().push((rel.clone(), n, mtime_unix));
                        let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        emit_progress(&format!("Copying {rel}"), d, b);
                    }
                    Err(e) if !worker_cancel.is_cancelled() => {
                        if let Err(me) = maybe_check_mount(true) {
                            let msg = me.to_string();
                            let mut d_err = disconnect_err.lock();
                            if d_err.is_none() {
                                *d_err = Some(msg.clone());
                            }
                            drop(d_err);
                            push_failures.fetch_add(1, Ordering::Relaxed);
                            failures.lock().push((rel.clone(), msg));
                            worker_cancel.cancel();
                            return;
                        }
                        push_failures.fetch_add(1, Ordering::Relaxed);
                        failures.lock().push((rel.clone(), e.to_string()));
                    }
                    Err(_) => {}
                }
            },
        );

        // Pass 3: Safe GPS backfill updates (stream-hashes copied bytes == post_md5 & re-verifies ext pre_md5!)
        backfills.par_iter().for_each_init(
            || vec![0u8; 2 * 1024 * 1024],
            |io_buf, rel| {
                if worker_cancel.is_cancelled() {
                    return;
                }
                if let Err(e) = maybe_check_mount(false) {
                    push_failures.fetch_add(1, Ordering::Relaxed);
                    let msg = e.to_string();
                    let mut d_err = disconnect_err.lock();
                    if d_err.is_none() {
                        *d_err = Some(msg.clone());
                    }
                    drop(d_err);
                    failures.lock().push((rel.clone(), msg));
                    worker_cancel.cancel();
                    return;
                }
                let src = local_map
                    .get(rel)
                    .map(|l| l.abs_path.clone())
                    .unwrap_or_else(|| join_rel_path(local_root, rel));
                let dst = ext_map
                    .get(rel)
                    .map(|e| e.abs_path.clone())
                    .unwrap_or_else(|| join_rel_path(ext_root, rel));
                let Some((ids, pre_set, post_md5)) = grouped_updates.get(rel) else {
                    push_failures.fetch_add(1, Ordering::Relaxed);
                    failures
                        .lock()
                        .push((rel.clone(), "Missing intentional update metadata".into()));
                    return;
                };
                let skip_parent = ensure_parent_cached(ext_root, &dst, &validated_ext_parents);
                // Stream-verify copied bytes == post_md5 AND re-verify dst == pre_set right before commit!
                match atomic_copy_preserve_mtime(
                    ext_root,
                    &src,
                    &dst,
                    true,
                    Some(post_md5.as_str()),
                    Some(pre_set),
                    skip_parent,
                    io_buf,
                    &worker_cancel,
                ) {
                    Ok((n, mtime_unix)) => {
                        backfilled.fetch_add(1, Ordering::Relaxed);
                        succeeded_rels.lock().push((rel.clone(), n, mtime_unix));
                        applied_update_ids.lock().extend(ids.iter().cloned());
                        let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        emit_progress(&format!("Updating geotagged {rel}"), d, b);
                    }
                    Err(e) if !worker_cancel.is_cancelled() => {
                        if let Err(me) = maybe_check_mount(true) {
                            let msg = me.to_string();
                            let mut d_err = disconnect_err.lock();
                            if d_err.is_none() {
                                *d_err = Some(msg.clone());
                            }
                            drop(d_err);
                            push_failures.fetch_add(1, Ordering::Relaxed);
                            failures.lock().push((rel.clone(), msg));
                            worker_cancel.cancel();
                            return;
                        }
                        push_failures.fetch_add(1, Ordering::Relaxed);
                        failures.lock().push((rel.clone(), e.to_string()));
                    }
                    Err(_) => {}
                }
            },
        );
    });

    // Record manifest ONLY for files that actually transferred cleanly (reusing copied size & mtime)
    let ok_entries: Vec<ManifestEntry> = succeeded_rels
        .lock()
        .iter()
        .map(|(rel, size, mtime_unix)| ManifestEntry {
            profile_kind: "external".into(),
            profile_id: profile.id.clone(),
            rel_path: rel.clone(),
            size_bytes: *size as i64,
            md5_hash: None,
            mtime_unix: *mtime_unix,
            state: "synced".into(),
        })
        .collect();
    let _ = db.upsert_manifest_entries(&ok_entries);

    let applied_ids = applied_update_ids.lock().clone();
    if !applied_ids.is_empty() {
        let _ = db.mark_intentional_updates_applied(&applied_ids, "external", &profile.id);
    }

    if let Some(err_msg) = disconnect_err.lock().take() {
        return Err(anyhow!(err_msg));
    }

    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }

    // Mandatory final unthrottled mount identity check before declaring sync complete
    check_mount_device_identity(mount_guard)?;

    let failed_list = failures.lock().clone();
    let failed_count = failed_list.len();

    // Update timestamped marker file inside `ext_root` ONLY when writable, zero failures, and opted in
    if failed_count == 0
        && !restore_mode
        && profile.write_marker_file
        && ext_writable
        && ext_root.is_dir()
        && check_mount_device_identity(mount_guard).is_ok()
    {
        let _ = update_last_sync_marker_file(ext_root);
    }

    let rem_push = if restore_mode {
        (report.to_push.len() + report.backfill_updates.len()) as i64
    } else {
        push_failures.load(Ordering::Relaxed) as i64
    };
    let rem_pull = pull_failures.load(Ordering::Relaxed) as i64;
    let rem_bytes: u64 = if restore_mode {
        report
            .to_push
            .iter()
            .chain(report.backfill_updates.iter())
            .filter_map(|i| i.local_bytes)
            .sum()
    } else if failed_count > 0 {
        report.pending_bytes
    } else {
        0
    };

    let status = if failed_count > 0 {
        "partial"
    } else if !report.conflicts.is_empty() {
        "needs_review"
    } else if rem_push > 0 || rem_pull > 0 {
        "behind"
    } else {
        "up_to_date"
    };
    let err_msg = if failed_count > 0 {
        Some(format!("{failed_count} file(s) failed to copy"))
    } else {
        None
    };
    db.update_external_freshness(
        &profile.id,
        failed_count == 0 && !restore_mode,
        true,
        status,
        err_msg.as_deref(),
        rem_push,
        rem_pull,
        rem_bytes as i64,
    )?;

    Ok(ExternalSyncResult {
        profile_id: profile.id.clone(),
        pulled_count: pulled.load(Ordering::Relaxed),
        pushed_count: pushed.load(Ordering::Relaxed),
        backfill_replaced_count: backfilled.load(Ordering::Relaxed),
        failed_count,
        bytes_transferred: bytes.load(Ordering::Relaxed),
        duration_ms: started.elapsed().as_millis() as u64,
        failures: failed_list,
    })
}

fn update_last_sync_marker_file(backup_root: &Path) -> Result<()> {
    if let Ok(rd) = std::fs::read_dir(backup_root) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_timestamped_marker_filename(&name) && entry.path().is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let stamp = Local::now().format("%Y-%m-%d %H-%M-%S");
    let marker = backup_root.join(format!("Last Sync - {stamp}.txt"));
    File::create(marker)?;
    Ok(())
}
