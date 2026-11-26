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
use truongsync_geo::exiftool::compute_file_md5;
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
    for entry_res in WalkDir::new(root).follow_links(false) {
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
        if is_mac_metadata_path(&rel)
            || rel.ends_with(".truongsync.part")
            || rel.ends_with(".partial")
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
    Ok(verify_external_roots_inner(
        profile,
        Path::new(&profile.local_root),
        &ext_root,
        Some(vol.mount_path),
        db,
        deep_md5,
        false,
        &cancel,
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
    db: &DbHandle,
    deep_md5: bool,
    restore_mode: bool,
    cancel: &CancellationToken,
) -> Result<ExternalVerifyContext> {
    let (local_map, local_exact_dups, local_mac) =
        scan_dir(local_root, !restore_mode, false, cancel)?;
    let ext_root_exists = ext_root.exists();
    let (ext_map, ext_exact_dups, ext_mac) = scan_dir(ext_root, false, true, cancel)?;

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
                            match (compute_file_md5(&l.abs_path), compute_file_md5(&e.abs_path)) {
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
                            match (compute_file_md5(&l.abs_path), compute_file_md5(&e.abs_path)) {
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
                        (compute_file_md5(&e.abs_path), compute_file_md5(&l.abs_path))
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
                    match (compute_file_md5(&l.abs_path), compute_file_md5(&e.abs_path)) {
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
                    confirmed_both.push(ManifestEntry {
                        profile_kind: "external".into(),
                        profile_id: profile.id.clone(),
                        rel_path: rel.clone(),
                        size_bytes: l.size as i64,
                        md5_hash: None,
                        mtime_unix: l.mtime_unix,
                        state: "synced".into(),
                    });
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
            let src_w: Vec<u16> = part
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let dst_w: Vec<u16> = dst
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(src_w.as_ptr()),
                    PCWSTR(dst_w.as_ptr()),
                    MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(|e| {
                anyhow!(
                    "Refusing to overwrite or move failed for {}: {e}",
                    dst.display()
                )
            })?;
            return Ok(());
        }
    }

    // Verified GPS backfill replacement (`allow_overwrite == true`):
    // Handle Windows read-only attribute & brief Defender/Explorer locks cleanly.
    #[cfg(windows)]
    {
        if let Ok(meta) = std::fs::metadata(dst) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(dst, perms);
            }
        }
        let mut last_err = None;
        for attempt in 0..3 {
            match std::fs::rename(part, dst) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::PermissionDenied && attempt < 2 => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(120));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("rename failed {} -> {}", part.display(), dst.display())
                    })
                }
            }
        }
        return Err(anyhow!(
            "rename failed {} -> {}: {:?}",
            part.display(),
            dst.display(),
            last_err
        ));
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(part, dst)
            .with_context(|| format!("rename failed {} -> {}", part.display(), dst.display()))?;
        Ok(())
    }
}

fn atomic_copy_preserve_mtime(
    src: &Path,
    dst: &Path,
    allow_overwrite: bool,
    cancel: &CancellationToken,
) -> Result<u64> {
    if !allow_overwrite && dst.symlink_metadata().is_ok() {
        return Err(anyhow!(
            "Destination file already exists; strictly-additive sync refused overwrite: {}",
            dst.display()
        ));
    }
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let meta = std::fs::metadata(src)?;
    let expected = meta.len();
    let mtime = meta.modified().ok();
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
    let mut buf = vec![0u8; 2 * 1024 * 1024];
    let mut copied = 0u64;
    loop {
        if cancel.is_cancelled() {
            drop(w);
            return Err(anyhow!("Sync cancelled."));
        }
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        w.write_all(&buf[..n])?;
        copied += n as u64;
    }
    // Flush data BEFORE set_modified so exFAT/FAT32/Windows flush never clobbers preserved mtime
    w.sync_all()?;
    if let Some(mt) = mtime {
        let _ = w.set_modified(mt);
        let _ = w.sync_all();
    }
    drop(w);
    if copied != expected {
        return Err(anyhow!("size mismatch copying {}", src.display()));
    }
    commit_part_file(&part, dst, allow_overwrite)?;
    guard.disarm();
    Ok(copied)
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
    let Some((vol, ext_root)) = resolve_external_target(profile) else {
        return Err(anyhow!(
            "External drive '{}' is not connected. Plug it in and try again.",
            profile.volume_label
        ));
    };
    sync_external_roots(
        profile,
        Path::new(&profile.local_root),
        &ext_root,
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
    let started = Instant::now();
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
        db,
        false,
        restore_mode,
        cancel,
    )?;
    let report = ctx.report;

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
        std::fs::create_dir_all(ext_root)?;
    }

    let total_ops = pulls.len() + pushes.len() + backfills.len();
    let done = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let pulled = Arc::new(AtomicUsize::new(0));
    let pushed = Arc::new(AtomicUsize::new(0));
    let backfilled = Arc::new(AtomicUsize::new(0));
    let pull_failures = Arc::new(AtomicUsize::new(0));
    let push_failures = Arc::new(AtomicUsize::new(0));
    let failures: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let succeeded_rels: Arc<Mutex<Vec<(String, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
    let applied_update_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

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
        pulls.par_iter().for_each(|rel| {
            if cancel.is_cancelled() {
                return;
            }
            let src = ext_map
                .get(rel)
                .map(|e| e.abs_path.clone())
                .unwrap_or_else(|| join_rel_path(ext_root, rel));
            let dst = join_rel_path(local_root, rel);
            match atomic_copy_preserve_mtime(&src, &dst, false, cancel) {
                Ok(n) => {
                    pulled.fetch_add(1, Ordering::Relaxed);
                    succeeded_rels.lock().push((rel.clone(), dst));
                    let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    on_progress(&format!("Pulling {rel}"), d, total_ops, b);
                }
                Err(e) if !cancel.is_cancelled() => {
                    pull_failures.fetch_add(1, Ordering::Relaxed);
                    failures.lock().push((rel.clone(), e.to_string()));
                }
                Err(_) => {}
            }
        });

        // Pass 2: PC -> External (strictly non-overwriting!)
        pushes.par_iter().for_each(|rel| {
            if cancel.is_cancelled() {
                return;
            }
            let src = local_map
                .get(rel)
                .map(|l| l.abs_path.clone())
                .unwrap_or_else(|| join_rel_path(local_root, rel));
            let dst = join_rel_path(ext_root, rel);
            match atomic_copy_preserve_mtime(&src, &dst, false, cancel) {
                Ok(n) => {
                    pushed.fetch_add(1, Ordering::Relaxed);
                    succeeded_rels.lock().push((rel.clone(), src));
                    let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    on_progress(&format!("Copying {rel}"), d, total_ops, b);
                }
                Err(e) if !cancel.is_cancelled() => {
                    push_failures.fetch_add(1, Ordering::Relaxed);
                    failures.lock().push((rel.clone(), e.to_string()));
                }
                Err(_) => {}
            }
        });

        // Pass 3: Safe GPS backfill updates (re-verifies BOTH local post_md5 & ext pre_md5 before overwriting!)
        backfills.par_iter().for_each(|rel| {
            if cancel.is_cancelled() {
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
            // TOCTOU re-check immediately before destructive backfill replacement
            let verified = match (compute_file_md5(&src), compute_file_md5(&dst)) {
                (Ok(lm), Ok(em)) => {
                    lm.eq_ignore_ascii_case(post_md5) && pre_set.contains(&em.to_ascii_lowercase())
                }
                _ => false,
            };
            if !verified {
                push_failures.fetch_add(1, Ordering::Relaxed);
                failures.lock().push((
                    rel.clone(),
                    "Aborted GPS backfill: file contents changed since verification".into(),
                ));
                return;
            }
            match atomic_copy_preserve_mtime(&src, &dst, true, cancel) {
                Ok(n) => {
                    backfilled.fetch_add(1, Ordering::Relaxed);
                    succeeded_rels.lock().push((rel.clone(), src));
                    applied_update_ids.lock().extend(ids.iter().cloned());
                    let b = bytes.fetch_add(n, Ordering::Relaxed) + n;
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    on_progress(&format!("Updating geotagged {rel}"), d, total_ops, b);
                }
                Err(e) if !cancel.is_cancelled() => {
                    push_failures.fetch_add(1, Ordering::Relaxed);
                    failures.lock().push((rel.clone(), e.to_string()));
                }
                Err(_) => {}
            }
        });
    });

    // Record manifest ONLY for files that actually transferred cleanly
    let ok_entries: Vec<ManifestEntry> = succeeded_rels
        .lock()
        .iter()
        .filter_map(|(rel, local_path)| {
            let meta = std::fs::metadata(local_path).ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(ManifestEntry {
                profile_kind: "external".into(),
                profile_id: profile.id.clone(),
                rel_path: rel.clone(),
                size_bytes: meta.len() as i64,
                md5_hash: None,
                mtime_unix: mtime,
                state: "synced".into(),
            })
        })
        .collect();
    let _ = db.upsert_manifest_entries(&ok_entries);

    let applied_ids = applied_update_ids.lock().clone();
    if !applied_ids.is_empty() {
        let _ = db.mark_intentional_updates_applied(&applied_ids, "external", &profile.id);
    }

    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }

    let failed_list = failures.lock().clone();
    let failed_count = failed_list.len();

    // Update timestamped marker file inside `ext_root` ONLY when writable, zero failures, and opted in
    if failed_count == 0
        && !restore_mode
        && profile.write_marker_file
        && ext_writable
        && ext_root.is_dir()
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
