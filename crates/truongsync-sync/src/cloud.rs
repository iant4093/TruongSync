use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{
    is_mac_metadata_path, normalize_rel_path, relative_normalized, AppSettings, CloudProfile,
    IntentionalUpdate, ManifestEntry,
};
use truongsync_db::DbHandle;
use truongsync_geo::exiftool::{compute_file_md5, ToolAvailability};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDiffItem {
    pub rel_path: String,
    pub direction: String, // "upload" | "download" | "backfill_replace" | "conflict" | "tombstone_restore"
    pub local_bytes: Option<u64>,
    pub remote_bytes: Option<u64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudVerifyReport {
    pub profile_id: String,
    pub checked_at: String,
    pub local_file_count: usize,
    pub remote_file_count: usize,
    pub up_to_date_count: usize,
    pub missing_on_remote: Vec<SyncDiffItem>,
    pub missing_on_local: Vec<SyncDiffItem>,
    pub tombstoned_local: Vec<SyncDiffItem>,
    pub backfill_updates: Vec<SyncDiffItem>,
    pub conflicts: Vec<SyncDiffItem>,
    pub duplicate_remote_paths: Vec<String>,
    pub ignored_mac_metadata_count: usize,
    pub pending_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSyncResult {
    pub profile_id: String,
    pub downloaded_count: usize,
    pub uploaded_count: usize,
    pub backfill_replaced_count: usize,
    pub conflicts_count: usize,
    pub skipped_tombstones: usize,
    pub duration_ms: u64,
    pub notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RcloneLsEntry {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "Hashes")]
    hashes: Option<HashMap<String, String>>,
}

/// Validates an rclone remote name against flag injection, INI syntax breaks, and whitespace.
pub fn validate_rclone_remote_name(raw: &str) -> Result<String> {
    let clean = raw.trim().trim_end_matches(':');
    if clean.is_empty() {
        return Err(anyhow!("Remote name cannot be empty."));
    }
    let bytes = clean.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() {
        return Err(anyhow!(
            "Remote name '{clean}' must start with a letter or digit."
        ));
    }
    if !clean
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(anyhow!(
            "Remote name '{clean}' may only contain letters, digits, '.', '-', and '_'."
        ));
    }
    Ok(clean.to_string())
}

/// Locates `rclone` binary across user override, bundled resources, Homebrew, and PATH.
pub fn resolve_rclone_with_origin(override_path: Option<&str>) -> Option<(PathBuf, bool)> {
    if let Some(p) = override_path.filter(|s| !s.trim().is_empty()) {
        let pb = PathBuf::from(p.trim());
        if pb.is_file() {
            return Some((pb, false));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            #[cfg(windows)]
            let candidates = [
                dir.join("rclone/rclone.exe"),
                dir.join("resources/rclone/rclone.exe"),
                dir.join("rclone.exe"),
            ];
            #[cfg(not(windows))]
            let candidates = [
                dir.join("../Resources/rclone/rclone"),
                dir.join("resources/rclone/rclone"),
                dir.join("rclone/rclone"),
                dir.join("rclone"),
            ];
            for c in candidates {
                if c.is_file() {
                    return Some((c, true));
                }
            }
        }
    }
    #[cfg(not(windows))]
    let common = [
        "/opt/homebrew/bin/rclone",
        "/usr/local/bin/rclone",
        "/usr/bin/rclone",
    ];
    #[cfg(windows)]
    let common = [
        r"C:\Scripts\Raw Photo Backup\tools\rclone.exe",
        r"C:\Program Files\rclone\rclone.exe",
    ];
    for c in common {
        let pb = PathBuf::from(c);
        if pb.is_file() {
            return Some((pb, false));
        }
    }
    let name = if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    };
    if new_rclone_cmd(Path::new(name))
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Some((PathBuf::from(name), false));
    }
    None
}

pub fn resolve_rclone_binary(override_path: Option<&str>) -> Option<PathBuf> {
    resolve_rclone_with_origin(override_path).map(|(p, _)| p)
}

pub fn check_rclone(override_path: Option<&str>) -> ToolAvailability {
    let Some((bin, bundled)) = resolve_rclone_with_origin(override_path) else {
        return ToolAvailability {
            available: false,
            bundled: false,
            path: None,
            version: None,
        };
    };
    let ver = new_rclone_cmd(&bin)
        .arg("version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string())
        });
    ToolAvailability {
        available: ver.is_some(),
        bundled,
        path: Some(bin.to_string_lossy().to_string()),
        version: ver,
    }
}

fn new_rclone_cmd(bin: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

struct CmdOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Spawns `rclone` with non-blocking stdout/stderr drainer threads and polls `cancel` (+ optional
/// timeout) every 150ms so clicking Cancel immediately kills the child process even mid-transfer.
fn run_rclone_cancellable_with_timeout(
    mut cmd: Command,
    cancel: &CancellationToken,
    timeout: Option<Duration>,
) -> Result<CmdOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("failed to spawn rclone")?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();

    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = out_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = err_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let started = Instant::now();
    let status = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_handle.join();
            let _ = err_handle.join();
            return Err(anyhow!("Sync cancelled."));
        }
        if let Some(limit) = timeout {
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(anyhow!("Operation timed out waiting for browser/rclone."));
            }
        }
        match child.try_wait()? {
            Some(st) => break st,
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(CmdOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn run_rclone_cancellable(cmd: Command, cancel: &CancellationToken) -> Result<CmdOutput> {
    run_rclone_cancellable_with_timeout(cmd, cancel, None)
}

fn enforce_private_conf_perms(conf_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = conf_path;
    }
}

/// Safely copies an existing `rclone.conf` into TruongSync's managed config path
/// (guards against self-copy truncation, validates at least one remote section, writes via 0600 temp file).
pub fn import_rclone_config(source_conf: &Path, managed_conf: &Path) -> Result<Vec<String>> {
    if !source_conf.is_file() {
        return Err(anyhow!(
            "rclone config file not found at {}",
            source_conf.display()
        ));
    }
    let remotes = parse_remote_names_from_conf(source_conf);
    if remotes.is_empty() {
        return Err(anyhow!(
            "Selected file does not contain any valid [remote] sections."
        ));
    }
    if let (Ok(cs), Ok(cm)) = (
        std::fs::canonicalize(source_conf),
        std::fs::canonicalize(managed_conf),
    ) {
        if cs == cm {
            enforce_private_conf_perms(managed_conf);
            return Ok(remotes);
        }
    }
    let parent = managed_conf
        .parent()
        .ok_or_else(|| anyhow!("Invalid managed rclone.conf path"))?;
    std::fs::create_dir_all(parent)?;
    let contents = std::fs::read(source_conf)?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    enforce_private_conf_perms(tmp.path());
    tmp.write_all(&contents)?;
    tmp.flush()?;
    tmp.persist(managed_conf)
        .map_err(|e| anyhow!("Failed to install managed rclone.conf: {}", e.error))?;
    enforce_private_conf_perms(managed_conf);
    Ok(remotes)
}

pub fn list_rclone_remotes(managed_conf: &Path) -> Vec<String> {
    parse_remote_names_from_conf(managed_conf)
}

fn parse_remote_names_from_conf(conf_path: &Path) -> Vec<String> {
    let Ok(txt) = std::fs::read_to_string(conf_path) else {
        return Vec::new();
    };
    txt.lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') && t.len() > 2 {
                Some(t[1..t.len() - 1].to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Launches `rclone config create <remote_name> drive scope drive config_is_local=true` with a
/// 5-minute timeout & cancellation support so closing the browser tab never leaks port 53682.
pub fn connect_google_drive_oauth(
    rclone_override: Option<&str>,
    managed_conf: &Path,
    remote_name: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let bin = resolve_rclone_binary(rclone_override).ok_or_else(|| {
        anyhow!("rclone was not found. Reinstall TruongSync or set its path in Settings.")
    })?;
    if let Some(parent) = managed_conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let clean_name = validate_rclone_remote_name(remote_name)?;
    let mut cmd = new_rclone_cmd(&bin);
    cmd.arg("--config").arg(managed_conf).args([
        "config",
        "create",
        &clean_name,
        "drive",
        "scope",
        "drive",
        "config_is_local",
        "true",
    ]);
    if let Some(cid) = client_id.filter(|s| !s.trim().is_empty()) {
        cmd.args(["client_id", cid.trim()]);
    }
    if let Some(csec) = client_secret.filter(|s| !s.trim().is_empty()) {
        cmd.args(["client_secret", csec.trim()]);
    }
    let out = run_rclone_cancellable_with_timeout(cmd, cancel, Some(Duration::from_secs(300)))?;
    enforce_private_conf_perms(managed_conf);
    if !out.success {
        return Err(anyhow!(
            "Google Drive sign-in failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_remote_names_from_conf(managed_conf))
}

fn remote_spec(profile: &CloudProfile) -> String {
    let rname = profile.remote_name.trim().trim_end_matches(':');
    let rpath = profile
        .remote_path
        .replace('\\', "/")
        .trim()
        .trim_matches('/')
        .to_string();
    format!("{rname}:{rpath}")
}

fn join_remote_path(remote_root: &str, rel_raw: &str) -> String {
    let clean_rel = rel_raw.trim_start_matches('/');
    if remote_root.ends_with(':') {
        format!("{remote_root}{clean_rel}")
    } else {
        format!("{}/{}", remote_root.trim_end_matches('/'), clean_rel)
    }
}

pub(crate) fn join_rel_path(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in rel.split(['/', '\\']) {
        if !seg.is_empty() && seg != "." && seg != ".." {
            out.push(seg);
        }
    }
    out
}

fn is_temp_partial_file(rel: &str) -> bool {
    rel.ends_with(".truongsync.part") || rel.ends_with(".partial")
}

#[derive(Debug, Clone)]
struct LocalFileMeta {
    abs_path: PathBuf,
    /// Exact relative path on disk (preserving NFD/raw bytes for `--files-from-raw` accuracy).
    raw_rel: String,
    size: u64,
    mtime_unix: i64,
}

fn raw_rel_for_rclone(root: &Path, full: &Path) -> Option<String> {
    let rel = full.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        if let std::path::Component::Normal(os) = comp {
            parts.push(os.to_string_lossy().into_owned());
        }
    }
    Some(parts.join("/"))
}

fn scan_local_tree(
    root: &Path,
    must_exist: bool,
    cancel: &CancellationToken,
) -> Result<(HashMap<String, LocalFileMeta>, HashSet<String>, usize)> {
    let mut map = HashMap::new();
    let mut exact_collisions = HashSet::new();
    let mut ignored_mac = 0usize;
    if !root.exists() {
        if must_exist {
            return Err(anyhow!(
                "Local sync root does not exist or is disconnected: {}",
                root.display()
            ));
        }
        return Ok((map, exact_collisions, 0));
    }
    for entry_res in WalkDir::new(root).follow_links(false) {
        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        let entry = entry_res.with_context(|| {
            format!(
                "failed reading directory inside local root {}",
                root.display()
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        let Some(rel) = relative_normalized(root, p) else {
            continue;
        };
        if is_mac_metadata_path(&rel) || is_temp_partial_file(&rel) {
            ignored_mac += 1;
            continue;
        }
        let raw_rel = raw_rel_for_rclone(root, p).unwrap_or_else(|| rel.clone());
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
            LocalFileMeta {
                abs_path: p.to_path_buf(),
                raw_rel,
                size: meta.len(),
                mtime_unix: mtime,
            },
        );
    }
    Ok((map, exact_collisions, ignored_mac))
}

#[derive(Debug, Clone)]
struct RemoteFileMeta {
    raw_path: String,
    size: u64,
    md5: Option<String>,
}

#[allow(clippy::type_complexity)]
fn list_remote_tree(
    bin: &Path,
    managed_conf: &Path,
    remote: &str,
    cancel: &CancellationToken,
) -> Result<(HashMap<String, RemoteFileMeta>, Vec<String>, usize, bool)> {
    let mut cmd = new_rclone_cmd(bin);
    cmd.arg("--config").arg(managed_conf).args([
        "lsjson",
        remote,
        "--recursive",
        "--files-only",
        "--hash-type",
        "MD5",
        "--no-mimetype",
        "--drive-skip-gdocs",
    ]);
    let out = run_rclone_cancellable(cmd, cancel)?;

    if !out.success {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("directory not found") {
            return Ok((HashMap::new(), Vec::new(), 0, false));
        }
        return Err(anyhow!("rclone lsjson failed: {}", stderr.trim()));
    }

    let entries: Vec<RcloneLsEntry> =
        serde_json::from_slice(&out.stdout).context("failed to parse rclone lsjson output")?;
    let mut map = HashMap::new();
    let mut duplicates = Vec::new();
    let mut ignored_mac = 0usize;

    for e in entries {
        let rel = normalize_rel_path(&e.path);
        if is_mac_metadata_path(&rel) || is_temp_partial_file(&rel) {
            ignored_mac += 1;
            continue;
        }
        if map.contains_key(&rel) {
            duplicates.push(rel.clone());
        }
        let md5 = e
            .hashes
            .and_then(|h| h.get("md5").or_else(|| h.get("MD5")).cloned())
            .map(|s| s.to_ascii_lowercase());
        map.insert(
            rel,
            RemoteFileMeta {
                raw_path: e.path,
                size: e.size.max(0) as u64,
                md5,
            },
        );
    }

    Ok((map, duplicates, ignored_mac, true))
}

/// Groups contiguous chained intentional updates (e.g., `A -> B` then `B -> C`) per relative path
/// so any remote matching a reachable `pre_md5` safely upgrades to the latest `post_md5`.
/// If an unrecorded external edit (`B -> X`) broke the chain before a later geotag (`X -> D`),
/// only `X` is reachable from `D` (preventing `A` from silently being replaced by `D`).
pub(crate) fn group_intentional_by_rel(
    updates: Vec<IntentionalUpdate>,
    local_root: &Path,
) -> HashMap<String, (Vec<String>, HashSet<String>, String)> {
    let mut per_rel: HashMap<String, Vec<IntentionalUpdate>> = HashMap::new();
    for u in updates {
        let Some(rel) = relative_normalized(local_root, Path::new(&u.abs_path)) else {
            continue;
        };
        per_rel.entry(rel).or_default().push(u);
    }

    let mut grouped = HashMap::new();
    for (rel, list) in per_rel {
        let Some(last) = list.last() else {
            continue;
        };
        let final_post = last.post_md5.to_ascii_lowercase();
        let mut expected_post = final_post.clone();
        let mut ids = Vec::new();
        let mut pre_set = HashSet::new();

        // Walk backwards from latest update; stop as soon as chain continuity breaks!
        for u in list.iter().rev() {
            let u_post = u.post_md5.to_ascii_lowercase();
            let u_pre = u.pre_md5.to_ascii_lowercase();
            if u_post == expected_post {
                ids.push(u.id.clone());
                pre_set.insert(u_pre.clone());
                expected_post = u_pre;
            } else {
                break;
            }
        }
        ids.reverse();
        if !ids.is_empty() {
            grouped.insert(rel, (ids, pre_set, final_post));
        }
    }
    grouped
}

struct CloudVerifyContext {
    report: CloudVerifyReport,
    local_map: HashMap<String, LocalFileMeta>,
    remote_map: HashMap<String, RemoteFileMeta>,
    intentional_by_rel: HashMap<String, (Vec<String>, HashSet<String>, String)>,
}

/// Runs a read-only audit comparing Local ↔ Google Drive.
pub fn verify_cloud_profile(
    profile: &CloudProfile,
    db: &DbHandle,
    rclone_override: Option<&str>,
    managed_conf: &Path,
    deep_md5: bool,
) -> Result<CloudVerifyReport> {
    let cancel = CancellationToken::new();
    verify_cloud_profile_cancellable(
        profile,
        db,
        rclone_override,
        managed_conf,
        deep_md5,
        &cancel,
    )
}

pub fn verify_cloud_profile_cancellable(
    profile: &CloudProfile,
    db: &DbHandle,
    rclone_override: Option<&str>,
    managed_conf: &Path,
    deep_md5: bool,
    cancel: &CancellationToken,
) -> Result<CloudVerifyReport> {
    Ok(verify_cloud_inner(
        profile,
        db,
        rclone_override,
        managed_conf,
        deep_md5,
        false,
        cancel,
    )?
    .report)
}

fn verify_cloud_inner(
    profile: &CloudProfile,
    db: &DbHandle,
    rclone_override: Option<&str>,
    managed_conf: &Path,
    deep_md5: bool,
    restore_mode: bool,
    cancel: &CancellationToken,
) -> Result<CloudVerifyContext> {
    let bin = resolve_rclone_binary(rclone_override).ok_or_else(|| {
        anyhow!("rclone was not found. Reinstall TruongSync or set its path in Settings.")
    })?;
    if !managed_conf.is_file() {
        return Err(anyhow!(
            "Google Drive is not connected yet. Connect Google Drive or import rclone.conf first."
        ));
    }

    let local_root = Path::new(&profile.local_root);
    let (local_map, local_exact_dups, local_mac) =
        scan_local_tree(local_root, !restore_mode, cancel)?;
    let remote = remote_spec(profile);
    let (remote_map, duplicate_remote_paths, remote_mac, remote_root_exists) =
        list_remote_tree(&bin, managed_conf, &remote, cancel)?;

    let manifest = db.load_manifest_map("cloud", &profile.id)?;
    let pending_updates = db.list_pending_intentional_updates_for_target("cloud", &profile.id)?;
    let intentional_by_rel = group_intentional_by_rel(pending_updates, local_root);

    let dup_remote_set: HashSet<&String> = duplicate_remote_paths.iter().collect();

    // Group by lowercase on BOTH sides so same-side case variants are quarantined cleanly
    let mut local_lower_groups: HashMap<String, Vec<String>> = HashMap::new();
    for k in local_map.keys() {
        local_lower_groups
            .entry(k.to_lowercase())
            .or_default()
            .push(k.clone());
    }
    let mut remote_lower_groups: HashMap<String, Vec<String>> = HashMap::new();
    for k in remote_map.keys() {
        remote_lower_groups
            .entry(k.to_lowercase())
            .or_default()
            .push(k.clone());
    }

    let all_keys: HashSet<&String> = local_map.keys().chain(remote_map.keys()).collect();
    let mut sorted_keys: Vec<&String> = all_keys.into_iter().collect();
    sorted_keys.sort();

    // Only prune stale manifest entries when the remote root actually exists (never wipe tombstones on typo/missing folder!)
    if remote_root_exists {
        let stale_manifest: Vec<String> = manifest
            .keys()
            .filter(|k| !local_map.contains_key(*k) && !remote_map.contains_key(*k))
            .cloned()
            .collect();
        if !stale_manifest.is_empty() {
            let _ = db.prune_manifest_entries("cloud", &profile.id, &stale_manifest);
        }
    }

    let mut missing_on_remote = Vec::new();
    let mut missing_on_local = Vec::new();
    let mut tombstoned_local = Vec::new();
    let mut backfill_updates = Vec::new();
    let mut conflicts = Vec::new();
    let mut up_to_date_count = 0usize;
    let mut pending_bytes = 0u64;
    let mut new_tombstones = Vec::new();
    let mut confirmed_both: Vec<ManifestEntry> = Vec::new();
    let mut auto_applied_update_ids: Vec<String> = Vec::new();
    let mut handled_case_buckets: HashSet<String> = HashSet::new();

    for rel in sorted_keys {
        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        let loc = local_map.get(rel);
        let rem = remote_map.get(rel);

        // Quarantine duplicate Google Drive paths and local exact normalization collisions
        if dup_remote_set.contains(rel) || local_exact_dups.contains(rel) {
            conflicts.push(SyncDiffItem {
                rel_path: rel.clone(),
                direction: "conflict".into(),
                local_bytes: loc.map(|l| l.size),
                remote_bytes: rem.map(|r| r.size),
                note: Some(if dup_remote_set.contains(rel) {
                    "Duplicate files share this exact path on Google Drive; quarantined for safety"
                        .into()
                } else {
                    "Multiple local files normalize to this path (NFC/NFD collision); quarantined"
                        .into()
                }),
            });
            continue;
        }

        // Quarantine same-side case-variant collisions (e.g., both IMG.JPG and img.jpg on Drive or PC)
        let lower = rel.to_lowercase();
        let loc_bucket_len = local_lower_groups.get(&lower).map_or(0, Vec::len);
        let rem_bucket_len = remote_lower_groups.get(&lower).map_or(0, Vec::len);
        if loc_bucket_len > 1 || rem_bucket_len > 1 {
            conflicts.push(SyncDiffItem {
                rel_path: rel.clone(),
                direction: "conflict".into(),
                local_bytes: loc.map(|l| l.size),
                remote_bytes: rem.map(|r| r.size),
                note: Some(
                    "Multiple files differ only by letter case; quarantined to prevent overwrite on case-insensitive filesystems"
                        .into(),
                ),
            });
            continue;
        }

        match (loc, rem) {
            (Some(l), None) => {
                // Symmetric cross-side case-variant check
                if let Some(rem_variant) = remote_lower_groups.get(&lower).and_then(|v| v.first()) {
                    if handled_case_buckets.insert(lower) {
                        let r = &remote_map[rem_variant];
                        let same = if l.size != r.size {
                            false
                        } else if deep_md5 {
                            match (&r.md5, compute_file_md5(&l.abs_path)) {
                                (Some(rm), Ok(lm)) => rm.eq_ignore_ascii_case(&lm),
                                (_, Err(_)) => false,
                                (None, Ok(_)) => true,
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
                                remote_bytes: Some(r.size),
                                note: Some(format!(
                                    "Case-variant collision with Drive '{rem_variant}'; protected by --immutable"
                                )),
                            });
                        }
                    }
                    continue;
                }
                pending_bytes = pending_bytes.saturating_add(l.size);
                missing_on_remote.push(SyncDiffItem {
                    rel_path: rel.clone(),
                    direction: "upload".into(),
                    local_bytes: Some(l.size),
                    remote_bytes: None,
                    note: None,
                });
            }
            (None, Some(r)) => {
                if let Some(local_variant) = local_lower_groups.get(&lower).and_then(|v| v.first())
                {
                    if handled_case_buckets.insert(lower) {
                        let l = &local_map[local_variant];
                        let same = if l.size != r.size {
                            false
                        } else if deep_md5 {
                            match (&r.md5, compute_file_md5(&l.abs_path)) {
                                (Some(rm), Ok(lm)) => rm.eq_ignore_ascii_case(&lm),
                                (_, Err(_)) => false,
                                (None, Ok(_)) => true,
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
                                remote_bytes: Some(r.size),
                                note: Some(format!(
                                    "Case-variant collision with local '{local_variant}'; protected by --immutable"
                                )),
                            });
                        }
                    }
                    continue;
                }

                let was_synced = manifest.contains_key(rel);
                if was_synced {
                    new_tombstones.push(rel.clone());
                    tombstoned_local.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: "tombstone_restore".into(),
                        local_bytes: None,
                        remote_bytes: Some(r.size),
                        note: Some(
                            "Deleted locally; preserved on Drive (use Restore to pull back)".into(),
                        ),
                    });
                } else {
                    pending_bytes = pending_bytes.saturating_add(r.size);
                    missing_on_local.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: "download".into(),
                        local_bytes: None,
                        remote_bytes: Some(r.size),
                        note: None,
                    });
                }
            }
            (Some(l), Some(r)) => {
                if let Some((ids, pre_set, post_md5)) = intentional_by_rel.get(rel) {
                    if let Some(rm) = r.md5.as_deref() {
                        if rm.eq_ignore_ascii_case(post_md5) {
                            auto_applied_update_ids.extend(ids.iter().cloned());
                        } else if pre_set.contains(&rm.to_ascii_lowercase()) {
                            if let Ok(cur_md5) = compute_file_md5(&l.abs_path) {
                                if cur_md5.eq_ignore_ascii_case(post_md5) {
                                    pending_bytes = pending_bytes.saturating_add(l.size);
                                    backfill_updates.push(SyncDiffItem {
                                        rel_path: rel.clone(),
                                        direction: "backfill_replace".into(),
                                        local_bytes: Some(l.size),
                                        remote_bytes: Some(r.size),
                                        note: Some(
                                            "Safe GPS backfill replacement (remote matches pre-tag MD5)"
                                                .into(),
                                        ),
                                    });
                                    continue;
                                }
                            }
                        }
                    }
                }

                let size_same = l.size == r.size;
                let mut hash_error_note: Option<String> = None;
                let content_same = if !size_same {
                    false
                } else if deep_md5 {
                    match (&r.md5, compute_file_md5(&l.abs_path)) {
                        (Some(rm), Ok(lm)) => rm.eq_ignore_ascii_case(&lm),
                        (_, Err(e)) => {
                            hash_error_note = Some(format!(
                                "Failed reading local file for MD5 verification: {e}"
                            ));
                            false
                        }
                        (None, Ok(_)) => true,
                    }
                } else {
                    true
                };

                if content_same {
                    up_to_date_count += 1;
                    confirmed_both.push(ManifestEntry {
                        profile_kind: "cloud".into(),
                        profile_id: profile.id.clone(),
                        rel_path: rel.clone(),
                        size_bytes: l.size as i64,
                        md5_hash: r.md5.clone(),
                        mtime_unix: l.mtime_unix,
                        state: "synced".into(),
                    });
                } else {
                    conflicts.push(SyncDiffItem {
                        rel_path: rel.clone(),
                        direction: "conflict".into(),
                        local_bytes: Some(l.size),
                        remote_bytes: Some(r.size),
                        note: hash_error_note.or_else(|| {
                            Some(
                                "Same path differs between PC and Drive; protected by --immutable"
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
        let _ = db.mark_local_tombstones("cloud", &profile.id, &new_tombstones);
    }
    if !auto_applied_update_ids.is_empty() {
        let _ = db.mark_intentional_updates_applied(&auto_applied_update_ids, "cloud", &profile.id);
    }

    let status = if !conflicts.is_empty() || !duplicate_remote_paths.is_empty() {
        "needs_review"
    } else if missing_on_remote.is_empty()
        && missing_on_local.is_empty()
        && backfill_updates.is_empty()
    {
        "up_to_date"
    } else {
        "behind"
    };

    if !restore_mode {
        let _ = db.update_cloud_freshness(
            &profile.id,
            false,
            true,
            status,
            None,
            (missing_on_remote.len() + backfill_updates.len()) as i64,
            missing_on_local.len() as i64,
            pending_bytes as i64,
        );
    }

    let report = CloudVerifyReport {
        profile_id: profile.id.clone(),
        checked_at: Utc::now().to_rfc3339(),
        local_file_count: local_map.len(),
        remote_file_count: remote_map.len(),
        up_to_date_count,
        missing_on_remote,
        missing_on_local,
        tombstoned_local,
        backfill_updates,
        conflicts,
        duplicate_remote_paths,
        ignored_mac_metadata_count: local_mac + remote_mac,
        pending_bytes,
    };

    Ok(CloudVerifyContext {
        report,
        local_map,
        remote_map,
        intentional_by_rel,
    })
}

fn ensure_local_root_for_sync(local_root: &Path, restore_mode: bool) -> Result<()> {
    if local_root.is_dir() {
        return Ok(());
    }
    if !restore_mode {
        return Err(anyhow!(
            "Local sync root does not exist or is disconnected: {} (use Restore if hydrating a new folder)",
            local_root.display()
        ));
    }
    // Even in restore mode, verify the parent volume/drive is actually mounted (never create on /Volumes on macOS!)
    truongsync_ingest::volumes::disk_free_bytes(local_root)?;
    std::fs::create_dir_all(local_root)?;
    Ok(())
}

/// Runs 2-way strictly-additive Google Drive sync (or Restore/Hydrate when `restore_mode` is true):
/// 0. Safely replaces verified GPS-backfilled photos (`--checksum`, TOCTOU MD5 re-check, per-target applied)
/// 1. Pass 1 (`Drive → Local`): downloads brand-new Drive files (or all missing files in restore mode)
/// 2. Pass 2 (`Local → Drive`): uploads ONLY `report.missing_on_remote` via raw source paths + `--immutable`
#[allow(clippy::too_many_arguments)]
pub fn sync_cloud_profile<F>(
    profile: &CloudProfile,
    db: &DbHandle,
    settings: &AppSettings,
    managed_conf: &Path,
    restore_mode: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<CloudSyncResult>
where
    F: Fn(&str, usize, usize),
{
    let started = Instant::now();
    let bin = resolve_rclone_binary(settings.rclone_path_override.as_deref()).ok_or_else(|| {
        anyhow!("rclone was not found. Reinstall TruongSync or set its path in Settings.")
    })?;
    if !managed_conf.is_file() {
        return Err(anyhow!(
            "Google Drive is not connected yet. Connect Google Drive or import rclone.conf first."
        ));
    }
    let local_root = Path::new(&profile.local_root);
    ensure_local_root_for_sync(local_root, restore_mode)?;

    on_progress("Auditing Local ↔ Google Drive…", 0, 100);
    let ctx = verify_cloud_inner(
        profile,
        db,
        settings.rclone_path_override.as_deref(),
        managed_conf,
        false,
        restore_mode,
        cancel,
    )?;
    let report = ctx.report;

    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }

    let remote = remote_spec(profile);
    let bwlimit = settings
        .bandwidth_limit_mbps
        .filter(|&v| v.is_finite() && v > 0.0)
        .map(|v| format!("{v:.2}M"));

    let mut notes = Vec::new();
    let mut backfill_replaced = 0usize;
    let mut backfill_failed = 0usize;

    // Step 0: Safe GPS backfill replacements (re-verifies local MD5 == post_md5 right before copyto!)
    if !restore_mode && !report.backfill_updates.is_empty() {
        let total_bf = report.backfill_updates.len();

        for (idx, item) in report.backfill_updates.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(anyhow!("Sync cancelled."));
            }
            on_progress(
                &format!(
                    "Updating geotagged photo on Drive ({}/{total_bf})…",
                    idx + 1
                ),
                idx + 1,
                total_bf,
            );
            let Some(loc_meta) = ctx.local_map.get(&item.rel_path) else {
                backfill_failed += 1;
                continue;
            };
            let Some(rem_meta) = ctx.remote_map.get(&item.rel_path) else {
                backfill_failed += 1;
                continue;
            };
            let Some((ids, _, post_md5)) = ctx.intentional_by_rel.get(&item.rel_path) else {
                backfill_failed += 1;
                continue;
            };
            // Re-verify local file still matches expected post_md5 (TOCTOU guard)
            let cur_md5 = match compute_file_md5(&loc_meta.abs_path) {
                Ok(h) if h.eq_ignore_ascii_case(post_md5) => h,
                Ok(_) => {
                    backfill_failed += 1;
                    notes.push(format!(
                        "Skipped GPS backfill for {}: local file changed after geotagging",
                        item.rel_path
                    ));
                    continue;
                }
                Err(e) => {
                    backfill_failed += 1;
                    notes.push(format!(
                        "Skipped GPS backfill for {}: failed to read local file ({e})",
                        item.rel_path
                    ));
                    continue;
                }
            };

            let remote_file = join_remote_path(&remote, &rem_meta.raw_path);
            let mut cmd = new_rclone_cmd(&bin);
            cmd.arg("--config").arg(managed_conf).args([
                "copyto",
                "--checksum",
                "--no-traverse",
                "--drive-skip-gdocs",
                "--drive-stop-on-upload-limit",
                "--retries",
                "5",
            ]);
            if let Some(ref bw) = bwlimit {
                cmd.args(["--bwlimit", bw]);
            }
            cmd.arg(&loc_meta.abs_path).arg(&remote_file);
            let out = run_rclone_cancellable(cmd, cancel)?;
            if out.success {
                backfill_replaced += 1;
                let _ = db.mark_intentional_updates_applied(ids, "cloud", &profile.id);
                if let Ok(meta) = std::fs::metadata(&loc_meta.abs_path) {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let _ = db.upsert_manifest_entries(&[ManifestEntry {
                        profile_kind: "cloud".into(),
                        profile_id: profile.id.clone(),
                        rel_path: item.rel_path.clone(),
                        size_bytes: meta.len() as i64,
                        md5_hash: Some(cur_md5),
                        mtime_unix: mtime,
                        state: "synced".into(),
                    }]);
                }
            } else {
                backfill_failed += 1;
                notes.push(format!(
                    "GPS backfill update failed for {}: {}",
                    item.rel_path,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
    }

    // Step 1: Pass 1 (Drive -> Local) using exact raw remote paths + --partial-suffix .truongsync.part
    let download_list: Vec<&SyncDiffItem> = if restore_mode {
        report
            .missing_on_local
            .iter()
            .chain(report.tombstoned_local.iter())
            .collect()
    } else {
        report.missing_on_local.iter().collect()
    };

    let mut downloaded_count = 0usize;
    let mut download_failed = 0usize;
    if !download_list.is_empty() {
        on_progress(
            &format!(
                "Downloading {} file(s) from Google Drive…",
                download_list.len()
            ),
            1,
            2,
        );
        // Partition newline-containing paths (unsafe for --files-from-raw) into single copyto calls
        let mut batch_dl: Vec<(&SyncDiffItem, &str)> = Vec::new();
        let mut single_dl: Vec<(&SyncDiffItem, &str)> = Vec::new();
        for item in &download_list {
            let raw = ctx
                .remote_map
                .get(&item.rel_path)
                .map(|m| m.raw_path.as_str())
                .unwrap_or(item.rel_path.as_str());
            if raw.contains('\n') || raw.contains('\r') {
                single_dl.push((item, raw));
            } else {
                batch_dl.push((item, raw));
            }
        }

        if !batch_dl.is_empty() {
            let mut manifest_file = NamedTempFile::new()?;
            for (_, raw) in &batch_dl {
                writeln!(manifest_file, "{raw}")?;
            }
            manifest_file.flush()?;

            let mut cmd = new_rclone_cmd(&bin);
            cmd.arg("--config")
                .arg(managed_conf)
                .args([
                    "copy",
                    &remote,
                    &profile.local_root,
                    "--immutable",
                    "--partial-suffix",
                    ".truongsync.part",
                    "--files-from-raw",
                ])
                .arg(manifest_file.path())
                .args([
                    "--drive-skip-gdocs",
                    "--transfers",
                    "4",
                    "--checkers",
                    "8",
                    "--retries",
                    "5",
                    "--retries-sleep",
                    "15s",
                    "--low-level-retries",
                    "10",
                ]);
            if let Some(ref bw) = bwlimit {
                cmd.args(["--bwlimit", bw]);
            }
            let out = run_rclone_cancellable(cmd, cancel)?;
            if !out.success {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                notes.push(format!("Drive-to-PC pass reported errors: {err}"));
            }
        }

        for (item, raw) in single_dl {
            if cancel.is_cancelled() {
                return Err(anyhow!("Sync cancelled."));
            }
            let rfile = join_remote_path(&remote, raw);
            let lfile = join_rel_path(local_root, raw);
            let mut cmd = new_rclone_cmd(&bin);
            cmd.arg("--config").arg(managed_conf).args([
                "copyto",
                "--immutable",
                "--partial-suffix",
                ".truongsync.part",
                "--drive-skip-gdocs",
            ]);
            if let Some(ref bw) = bwlimit {
                cmd.args(["--bwlimit", bw]);
            }
            cmd.arg(&rfile).arg(&lfile);
            let out = run_rclone_cancellable(cmd, cancel)?;
            if !out.success {
                notes.push(format!(
                    "Download failed for {}: {}",
                    item.rel_path,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }

        // Verify which downloaded files actually exist on disk with expected size before recording manifest
        let mut dl_entries = Vec::new();
        for item in &download_list {
            let raw = ctx
                .remote_map
                .get(&item.rel_path)
                .map(|m| m.raw_path.as_str())
                .unwrap_or(item.rel_path.as_str());
            let p_raw = join_rel_path(local_root, raw);
            let p_norm = join_rel_path(local_root, &item.rel_path);
            let meta = std::fs::metadata(&p_raw).or_else(|_| std::fs::metadata(&p_norm));
            match meta {
                Ok(m) if item.remote_bytes.is_none_or(|rb| m.len() == rb) => {
                    downloaded_count += 1;
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let r_md5 = ctx
                        .remote_map
                        .get(&item.rel_path)
                        .and_then(|rm| rm.md5.clone());
                    dl_entries.push(ManifestEntry {
                        profile_kind: "cloud".into(),
                        profile_id: profile.id.clone(),
                        rel_path: item.rel_path.clone(),
                        size_bytes: m.len() as i64,
                        md5_hash: r_md5,
                        mtime_unix: mtime,
                        state: "synced".into(),
                    });
                }
                _ => {
                    download_failed += 1;
                }
            }
        }
        let _ = db.upsert_manifest_entries(&dl_entries);
    }

    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }

    // Step 2: Pass 2 (Local -> Drive) scoped strictly to `missing_on_remote` using raw local paths
    let mut uploaded_count = 0usize;
    let mut upload_failed = 0usize;
    if !restore_mode && !report.missing_on_remote.is_empty() {
        on_progress(
            &format!(
                "Uploading {} file(s) to Google Drive…",
                report.missing_on_remote.len()
            ),
            2,
            2,
        );
        let mut batch_ul: Vec<(&SyncDiffItem, &LocalFileMeta)> = Vec::new();
        let mut single_ul: Vec<(&SyncDiffItem, &LocalFileMeta)> = Vec::new();
        for item in &report.missing_on_remote {
            let Some(loc_meta) = ctx.local_map.get(&item.rel_path) else {
                upload_failed += 1;
                continue;
            };
            if loc_meta.raw_rel.contains('\n') || loc_meta.raw_rel.contains('\r') {
                single_ul.push((item, loc_meta));
            } else {
                batch_ul.push((item, loc_meta));
            }
        }

        let mut batch_ok = true;
        if !batch_ul.is_empty() {
            let mut ul_manifest = NamedTempFile::new()?;
            for (_, loc_meta) in &batch_ul {
                writeln!(ul_manifest, "{}", loc_meta.raw_rel)?;
            }
            ul_manifest.flush()?;

            let mut cmd = new_rclone_cmd(&bin);
            cmd.arg("--config")
                .arg(managed_conf)
                .args([
                    "copy",
                    &profile.local_root,
                    &remote,
                    "--immutable",
                    "--files-from-raw",
                ])
                .arg(ul_manifest.path())
                .args([
                    "--drive-skip-gdocs",
                    "--drive-stop-on-upload-limit",
                    "--transfers",
                    "4",
                    "--checkers",
                    "8",
                    "--retries",
                    "5",
                    "--retries-sleep",
                    "15s",
                    "--low-level-retries",
                    "10",
                ]);
            if let Some(ref bw) = bwlimit {
                cmd.args(["--bwlimit", bw]);
            }
            let out = run_rclone_cancellable(cmd, cancel)?;
            if !out.success {
                batch_ok = false;
                upload_failed += batch_ul.len();
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                notes.push(format!("PC-to-Drive pass failed safely: {err}"));
            } else {
                uploaded_count += batch_ul.len();
                let ul_entries: Vec<ManifestEntry> = batch_ul
                    .iter()
                    .filter_map(|(item, loc_meta)| {
                        let meta = std::fs::metadata(&loc_meta.abs_path).ok()?;
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        Some(ManifestEntry {
                            profile_kind: "cloud".into(),
                            profile_id: profile.id.clone(),
                            rel_path: item.rel_path.clone(),
                            size_bytes: meta.len() as i64,
                            md5_hash: None,
                            mtime_unix: mtime,
                            state: "synced".into(),
                        })
                    })
                    .collect();
                let _ = db.upsert_manifest_entries(&ul_entries);
            }
        }

        if batch_ok || !single_ul.is_empty() {
            for (item, loc_meta) in single_ul {
                if cancel.is_cancelled() {
                    return Err(anyhow!("Sync cancelled."));
                }
                let rfile = join_remote_path(&remote, &loc_meta.raw_rel);
                let mut cmd = new_rclone_cmd(&bin);
                cmd.arg("--config").arg(managed_conf).args([
                    "copyto",
                    "--immutable",
                    "--drive-skip-gdocs",
                    "--drive-stop-on-upload-limit",
                ]);
                if let Some(ref bw) = bwlimit {
                    cmd.args(["--bwlimit", bw]);
                }
                cmd.arg(&loc_meta.abs_path).arg(&rfile);
                let out = run_rclone_cancellable(cmd, cancel)?;
                if out.success {
                    uploaded_count += 1;
                    let _ = db.upsert_manifest_entries(&[ManifestEntry {
                        profile_kind: "cloud".into(),
                        profile_id: profile.id.clone(),
                        rel_path: item.rel_path.clone(),
                        size_bytes: loc_meta.size as i64,
                        md5_hash: None,
                        mtime_unix: loc_meta.mtime_unix,
                        state: "synced".into(),
                    }]);
                } else {
                    upload_failed += 1;
                    notes.push(format!(
                        "Upload failed for {}: {}",
                        item.rel_path,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
            }
        }
    }

    let total_failures = backfill_failed + download_failed + upload_failed;
    let remaining_uploads = if restore_mode {
        (report.missing_on_remote.len() + report.backfill_updates.len()) as i64
    } else {
        (backfill_failed + upload_failed) as i64
    };
    let remaining_downloads = download_failed as i64;
    let remaining_bytes: u64 = if restore_mode {
        report
            .missing_on_remote
            .iter()
            .chain(report.backfill_updates.iter())
            .filter_map(|i| i.local_bytes)
            .sum()
    } else if total_failures > 0 {
        report.pending_bytes
    } else {
        0
    };

    let final_status = if total_failures > 0 {
        "partial"
    } else if !report.conflicts.is_empty() || !report.duplicate_remote_paths.is_empty() {
        "needs_review"
    } else if remaining_uploads > 0 || remaining_downloads > 0 {
        "behind"
    } else {
        "up_to_date"
    };
    let err_note = if notes.is_empty() {
        None
    } else {
        Some(notes.join(" | "))
    };
    db.update_cloud_freshness(
        &profile.id,
        !restore_mode && total_failures == 0,
        true,
        final_status,
        err_note.as_deref(),
        remaining_uploads,
        remaining_downloads,
        remaining_bytes as i64,
    )?;

    if total_failures > 0 && downloaded_count == 0 && uploaded_count == 0 && backfill_replaced == 0
    {
        return Err(anyhow!(
            "{}",
            err_note.unwrap_or_else(|| format!("{total_failures} cloud transfer(s) failed"))
        ));
    }

    Ok(CloudSyncResult {
        profile_id: profile.id.clone(),
        downloaded_count,
        uploaded_count,
        backfill_replaced_count: backfill_replaced,
        conflicts_count: report.conflicts.len(),
        skipped_tombstones: if restore_mode {
            0
        } else {
            report.tombstoned_local.len()
        },
        duration_ms: started.elapsed().as_millis() as u64,
        notes,
    })
}
