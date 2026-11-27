use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use truongsync_db::models::{
    is_mac_metadata_path, normalize_rel_path, relative_normalized, AppSettings, CloudProfile,
    IntentionalUpdate, ManifestEntry,
};
use truongsync_db::DbHandle;
use truongsync_geo::exiftool::{compute_file_md5, compute_file_md5_with_buf, ToolAvailability};
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
    pub failed_count: usize,
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
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
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
    let mut poll_ms = 5u64;
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
            None => {
                std::thread::sleep(Duration::from_millis(poll_ms));
                poll_ms = match poll_ms {
                    5 => 15,
                    15 => 50,
                    _ => 150,
                };
            }
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
/// (guards against self-copy truncation, enforces 1 MiB size cap, validates all remote section names,
/// writes via 0600 temp file).
pub fn import_rclone_config(source_conf: &Path, managed_conf: &Path) -> Result<Vec<String>> {
    if !source_conf.is_file() {
        return Err(anyhow!(
            "rclone config file not found at {}",
            source_conf.display()
        ));
    }
    let meta = std::fs::metadata(source_conf)?;
    if meta.len() > 1024 * 1024 {
        return Err(anyhow!(
            "Selected rclone.conf exceeds maximum allowed size (1 MiB)."
        ));
    }
    let contents = std::fs::read(source_conf)?;
    let raw_txt = std::str::from_utf8(&contents)
        .map_err(|_| anyhow!("Selected rclone.conf is not valid UTF-8 INI text."))?;
    let txt = raw_txt.strip_prefix('\u{feff}').unwrap_or(raw_txt);
    let mut remotes = Vec::new();
    for line in txt.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') && t.len() > 2 {
            let raw_name = &t[1..t.len() - 1];
            let valid = validate_rclone_remote_name(raw_name).map_err(|e| {
                anyhow!("Invalid remote section '[{raw_name}]' in rclone.conf: {e}")
            })?;
            remotes.push(valid);
        }
    }
    if remotes.is_empty() {
        return Err(anyhow!(
            "Selected file does not contain any valid [remote] sections."
        ));
    }
    if let (Ok(cs), Ok(cm)) = (
        std::fs::canonicalize(source_conf),
        std::fs::canonicalize(managed_conf),
    ) {
        if cs == cm && raw_txt.len() == txt.len() {
            enforce_private_conf_perms(managed_conf);
            return Ok(remotes);
        }
    }
    let parent = managed_conf
        .parent()
        .ok_or_else(|| anyhow!("Invalid managed rclone.conf path"))?;
    std::fs::create_dir_all(parent)?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    enforce_private_conf_perms(tmp.path());
    tmp.write_all(txt.as_bytes())?;
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
    let Ok(meta) = std::fs::metadata(conf_path) else {
        return Vec::new();
    };
    if meta.len() > 1024 * 1024 {
        return Vec::new();
    }
    let Ok(raw_txt) = std::fs::read_to_string(conf_path) else {
        return Vec::new();
    };
    let txt = raw_txt.strip_prefix('\u{feff}').unwrap_or(&raw_txt);
    txt.lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.starts_with('[') && t.ends_with(']') && t.len() > 2 {
                validate_rclone_remote_name(&t[1..t.len() - 1]).ok()
            } else {
                None
            }
        })
        .collect()
}

fn validate_oauth_param(label: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('-') || trimmed.chars().any(|c| c.is_control()) {
        return Err(anyhow!(
            "Invalid {label}: cannot start with '-' or contain control characters."
        ));
    }
    Ok(trimmed.to_string())
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
    // Pre-create managed_conf with 0600 permissions BEFORE spawning rclone so tokens are never world-readable.
    if !managed_conf.exists() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(managed_conf);
    }
    enforce_private_conf_perms(managed_conf);

    let clean_name = validate_rclone_remote_name(remote_name)?;
    let mut cmd = new_rclone_cmd(&bin);
    cmd.arg("--config").arg(managed_conf).args([
        "config",
        "create",
        "--",
        &clean_name,
        "drive",
        "scope",
        "drive",
        "config_is_local",
        "true",
    ]);
    if let Some(cid) = client_id.filter(|s| !s.trim().is_empty()) {
        let clean_cid = validate_oauth_param("Google OAuth Client ID", cid)?;
        cmd.args(["client_id", &clean_cid]);
    }
    if let Some(csec) = client_secret.filter(|s| !s.trim().is_empty()) {
        let clean_csec = validate_oauth_param("Google OAuth Client Secret", csec)?;
        cmd.args(["client_secret", &clean_csec]);
    }
    let res = run_rclone_cancellable_with_timeout(cmd, cancel, Some(Duration::from_secs(300)));
    enforce_private_conf_perms(managed_conf);
    let out = res?;
    if !out.success {
        return Err(anyhow!(
            "Google Drive sign-in failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(parse_remote_names_from_conf(managed_conf))
}

fn remote_spec(profile: &CloudProfile) -> Result<String> {
    let rname = validate_rclone_remote_name(&profile.remote_name)?;
    let rpath = profile
        .remote_path
        .replace('\\', "/")
        .trim()
        .trim_matches('/')
        .to_string();
    Ok(format!("{rname}:{rpath}"))
}

/// Validates that no existing ancestor directory between `root` and `dst` (inclusive of `dst`'s parent)
/// is a symlink/reparse point, preventing symlink-traversal escapes outside sync roots.
pub(crate) fn ensure_no_symlink_ancestors(root: &Path, dst: &Path) -> Result<()> {
    let Ok(rel) = dst.strip_prefix(root) else {
        return Err(anyhow!(
            "Refusing sync target outside root: {}",
            dst.display()
        ));
    };
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        use std::path::Component;
        match comp {
            Component::Normal(seg) => {
                cur.push(seg);
                if let Ok(meta) = std::fs::symlink_metadata(&cur) {
                    if meta.file_type().is_symlink() {
                        return Err(anyhow!(
                            "Refusing to traverse symlink inside sync root: {}",
                            cur.display()
                        ));
                    }
                }
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "Invalid traversal component in sync path: {}",
                    dst.display()
                ));
            }
            Component::CurDir => {}
        }
    }
    Ok(())
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
        if seg.is_empty() || seg == "." || seg == ".." {
            continue;
        }
        // Only reject Windows drive-prefix segments (`C:`, `D:`) on Windows; on macOS,
        // Finder legitimately encodes `/` in folder names like `B/W` as `B:W` on POSIX APFS.
        #[cfg(windows)]
        {
            let b = seg.as_bytes();
            if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                continue;
            }
        }
        if matches!(
            Path::new(seg).components().next(),
            Some(std::path::Component::Prefix(_)) | Some(std::path::Component::RootDir)
        ) {
            continue;
        }
        out.push(seg);
    }
    out
}

fn is_temp_partial_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.ends_with(".truongsync.part")
        || lower.ends_with(".partial")
        || lower.ends_with("_exiftool_tmp")
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
        "--recursive",
        "--files-only",
        "--fast-list",
        "--no-modtime",
        "--hash-type",
        "MD5",
        "--no-mimetype",
        "--drive-skip-gdocs",
        "--",
        remote,
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

    let local_root = Path::new(profile.local_root.trim());
    if !local_root.is_absolute() {
        return Err(anyhow!(
            "Cloud profile local root must be an absolute path: {}",
            profile.local_root
        ));
    }
    let remote = remote_spec(profile)?;
    let scan_cancel = cancel.child_token();
    let (local_res, remote_res) = std::thread::scope(|s| {
        let sc_local = scan_cancel.clone();
        let local_handle = s.spawn(move || {
            let r = scan_local_tree(local_root, !restore_mode, &sc_local);
            if r.is_err() {
                sc_local.cancel();
            }
            r
        });
        let remote_res = list_remote_tree(&bin, managed_conf, &remote, &scan_cancel);
        if remote_res.is_err() {
            scan_cancel.cancel();
        }
        let local_res = local_handle
            .join()
            .unwrap_or_else(|_| Err(anyhow!("local scan thread panicked")));
        (local_res, remote_res)
    });
    if cancel.is_cancelled() {
        return Err(anyhow!("Sync cancelled."));
    }
    let (
        (local_map, local_exact_dups, local_mac),
        (remote_map, duplicate_remote_paths, remote_mac, remote_root_exists),
    ) = match (local_res, remote_res) {
        (Ok(l), Ok(r)) => (l, r),
        (Err(le), Err(re)) => {
            if le.to_string() == "Sync cancelled." {
                return Err(re);
            }
            return Err(le);
        }
        (Err(le), _) => return Err(le),
        (_, Err(re)) => return Err(re),
    };

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

    // Pre-compute needed local MD5s (for deep_md5 or GPS backfill candidates) in parallel with
    // per-thread reused 1 MiB buffers instead of hashing sequentially one file at a time.
    let mut md5_targets: HashSet<&Path> = HashSet::new();
    for &rel in &sorted_keys {
        if dup_remote_set.contains(rel) || local_exact_dups.contains(rel) {
            continue;
        }
        let lower = rel.to_lowercase();
        let loc_bucket_len = local_lower_groups.get(&lower).map_or(0, Vec::len);
        let rem_bucket_len = remote_lower_groups.get(&lower).map_or(0, Vec::len);
        if loc_bucket_len > 1 || rem_bucket_len > 1 {
            continue;
        }
        match (local_map.get(rel), remote_map.get(rel)) {
            (Some(l), Some(r)) => {
                let needs_bf = intentional_by_rel
                    .get(rel)
                    .is_some_and(|(_, pre_set, post_md5)| {
                        r.md5.as_deref().is_some_and(|rm| {
                            !rm.eq_ignore_ascii_case(post_md5)
                                && pre_set.contains(&rm.to_ascii_lowercase())
                        })
                    });
                if needs_bf || (deep_md5 && l.size == r.size && r.md5.is_some()) {
                    md5_targets.insert(l.abs_path.as_path());
                }
            }
            (Some(l), None) if deep_md5 => {
                if let Some(rem_variant) = remote_lower_groups.get(&lower).and_then(|v| v.first()) {
                    let r = &remote_map[rem_variant];
                    if l.size == r.size && r.md5.is_some() {
                        md5_targets.insert(l.abs_path.as_path());
                    }
                }
            }
            (None, Some(r)) if deep_md5 => {
                if let Some(loc_variant) = local_lower_groups.get(&lower).and_then(|v| v.first()) {
                    let l = &local_map[loc_variant];
                    if l.size == r.size && r.md5.is_some() {
                        md5_targets.insert(l.abs_path.as_path());
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
            .clamp(2, 6);
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
    let lookup_local_md5 = |p: &Path| -> Result<String, String> {
        if let Some(res) = precomputed_md5s.get(p) {
            return res.clone();
        }
        compute_file_md5(p).map_err(|e| e.to_string())
    };

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
                            match (&r.md5, lookup_local_md5(&l.abs_path)) {
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
                            match (&r.md5, lookup_local_md5(&l.abs_path)) {
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
                            if let Ok(cur_md5) = lookup_local_md5(&l.abs_path) {
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
                    match (&r.md5, lookup_local_md5(&l.abs_path)) {
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
                    let needs_upsert = match manifest.get(rel) {
                        Some(existing) => {
                            existing.size_bytes != l.size as i64
                                || existing.mtime_unix != l.mtime_unix
                                || existing.state != "synced"
                                || (r.md5.is_some() && existing.md5_hash != r.md5)
                        }
                        None => true,
                    };
                    if needs_upsert {
                        confirmed_both.push(ManifestEntry {
                            profile_kind: "cloud".into(),
                            profile_id: profile.id.clone(),
                            rel_path: rel.clone(),
                            size_bytes: l.size as i64,
                            md5_hash: r.md5.clone(),
                            mtime_unix: l.mtime_unix,
                            state: "synced".into(),
                        });
                    }
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

fn fetch_single_remote_md5(
    bin: &Path,
    managed_conf: &Path,
    remote_file: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    let mut cmd = new_rclone_cmd(bin);
    cmd.arg("--config").arg(managed_conf).args([
        "lsjson",
        "--stat",
        "--hash",
        "--hash-type",
        "MD5",
        "--no-mimetype",
        "--drive-skip-gdocs",
        "--",
        remote_file,
    ]);
    let out = run_rclone_cancellable(cmd, cancel)?;
    if !out.success {
        return Ok(None);
    }
    let entry: RcloneLsEntry = serde_json::from_slice(&out.stdout)?;
    Ok(entry
        .hashes
        .and_then(|h| h.get("md5").or_else(|| h.get("MD5")).cloned())
        .map(|s| s.to_ascii_lowercase()))
}

fn cleanup_cancelled_cloud_parts(local_root: &Path, paths: &[String]) {
    // Rclone `--partial-suffix .truongsync.part` creates `<filename>.<rand8>.truongsync.part`,
    // so scan each affected parent directory for matching partial files.
    let mut by_parent: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for raw in paths {
        let p = join_rel_path(local_root, raw);
        let Some(leaf) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let parent = p.parent().unwrap_or(local_root).to_path_buf();
        by_parent.entry(parent).or_default().push(leaf);
    }
    for (parent, leaves) in by_parent {
        let Ok(rd) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in rd.flatten() {
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".truongsync.part") {
                continue;
            }
            let matches_any = leaves.iter().any(|leaf| {
                name == format!("{leaf}.truongsync.part")
                    || (name.starts_with(leaf.as_str())
                        && name.as_bytes().get(leaf.len()) == Some(&b'.'))
            });
            if matches_any {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
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
    F: Fn(&str, usize, usize) + Sync,
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
    let local_root = Path::new(profile.local_root.trim());
    if !local_root.is_absolute() {
        return Err(anyhow!(
            "Cloud profile local root must be an absolute path: {}",
            profile.local_root
        ));
    }
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

    let remote = remote_spec(profile)?;
    let bwlimit_mbps = settings
        .bandwidth_limit_mbps
        .filter(|&v| v.is_finite() && v > 0.0);
    let bwlimit = bwlimit_mbps.map(|v| format!("{}K", ((v * 1024.0).round() as u64).max(1)));

    let mut notes = Vec::new();
    let mut backfill_replaced = 0usize;
    let mut backfill_failed = 0usize;

    // Step 0: Safe GPS backfill replacements (re-verifies local MD5 == post_md5 & live remote MD5 right before copyto!)
    // Parallelized up to 4 concurrent uploads when multiple geotagged photos are queued,
    // dividing any configured bandwidth cap evenly across concurrent rclone processes.
    if !restore_mode && !report.backfill_updates.is_empty() {
        let total_bf = report.backfill_updates.len();
        let bf_done = AtomicUsize::new(0);
        let bf_ok = AtomicUsize::new(0);
        let bf_fail = AtomicUsize::new(0);
        let bf_notes: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let bf_progress_gate: Mutex<(Instant, usize)> =
            Mutex::new((Instant::now() - Duration::from_secs(1), 0));
        let bf_threads = total_bf.clamp(1, 4);
        let bf_bwlimit = bwlimit_mbps.map(|v| {
            format!(
                "{}K",
                ((v * 1024.0 / bf_threads as f64).round() as u64).max(1)
            )
        });

        on_progress(
            &format!("Updating geotagged photos on Drive (0/{total_bf})…"),
            0,
            total_bf,
        );

        let report_bf_done = || {
            let done_now = bf_done.fetch_add(1, Ordering::Relaxed) + 1;
            let mut gate = bf_progress_gate.lock();
            if done_now > gate.1
                && (done_now >= total_bf || gate.0.elapsed() >= Duration::from_millis(80))
            {
                gate.0 = Instant::now();
                gate.1 = done_now;
                on_progress(
                    &format!("Updating geotagged photo on Drive ({done_now}/{total_bf})…"),
                    done_now,
                    total_bf,
                );
            }
        };

        let run_one_backfill = |item: &SyncDiffItem, md5_buf: &mut [u8]| {
            if cancel.is_cancelled() {
                return;
            }
            let Some(loc_meta) = ctx.local_map.get(&item.rel_path) else {
                bf_fail.fetch_add(1, Ordering::Relaxed);
                report_bf_done();
                return;
            };
            let Some(rem_meta) = ctx.remote_map.get(&item.rel_path) else {
                bf_fail.fetch_add(1, Ordering::Relaxed);
                report_bf_done();
                return;
            };
            let Some((ids, pre_set, post_md5)) = ctx.intentional_by_rel.get(&item.rel_path) else {
                bf_fail.fetch_add(1, Ordering::Relaxed);
                report_bf_done();
                return;
            };
            // Re-verify local file still matches expected post_md5 (local TOCTOU guard)
            let cur_md5 = match compute_file_md5_with_buf(&loc_meta.abs_path, md5_buf) {
                Ok(h) if h.eq_ignore_ascii_case(post_md5) => h,
                Ok(_) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "Skipped GPS backfill for {}: local file changed after geotagging",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
                Err(e) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "Skipped GPS backfill for {}: failed to read local file ({e})",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
            };

            let remote_file = join_remote_path(&remote, &rem_meta.raw_path);
            // Re-verify live remote MD5 immediately before replacing (remote TOCTOU guard)
            match fetch_single_remote_md5(&bin, managed_conf, &remote_file, cancel) {
                Ok(Some(live_md5)) if pre_set.contains(&live_md5) => {}
                Ok(Some(_)) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "Skipped GPS backfill for {}: remote Drive copy changed since scan",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
                Ok(None) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "Skipped GPS backfill for {}: could not re-verify remote Drive MD5",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
                Err(_) if cancel.is_cancelled() => return,
                Err(e) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "Skipped GPS backfill for {}: failed checking remote MD5 ({e})",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
            }

            let mut cmd = new_rclone_cmd(&bin);
            cmd.arg("--config").arg(managed_conf).args([
                "copyto",
                "--checksum",
                "--no-traverse",
                "--drive-chunk-size",
                "64M",
                "--drive-skip-gdocs",
                "--drive-stop-on-upload-limit",
                "--retries",
                "5",
            ]);
            if let Some(ref bw) = bf_bwlimit {
                cmd.args(["--bwlimit", bw]);
            }
            cmd.arg("--").arg(&loc_meta.abs_path).arg(&remote_file);
            let out = match run_rclone_cancellable(cmd, cancel) {
                Ok(o) => o,
                Err(_) if cancel.is_cancelled() => return,
                Err(e) => {
                    bf_fail.fetch_add(1, Ordering::Relaxed);
                    bf_notes.lock().push(format!(
                        "GPS backfill update failed for {}: {e}",
                        item.rel_path
                    ));
                    report_bf_done();
                    return;
                }
            };
            if out.success {
                bf_ok.fetch_add(1, Ordering::Relaxed);
                let _ = db.mark_intentional_updates_applied(ids, "cloud", &profile.id);
                let _ = db.upsert_manifest_entries(&[ManifestEntry {
                    profile_kind: "cloud".into(),
                    profile_id: profile.id.clone(),
                    rel_path: item.rel_path.clone(),
                    size_bytes: loc_meta.size as i64,
                    md5_hash: Some(cur_md5),
                    mtime_unix: loc_meta.mtime_unix,
                    state: "synced".into(),
                }]);
            } else {
                bf_fail.fetch_add(1, Ordering::Relaxed);
                bf_notes.lock().push(format!(
                    "GPS backfill update failed for {}: {}",
                    item.rel_path,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            report_bf_done();
        };

        if bf_threads > 1 {
            if let Ok(pool) = rayon::ThreadPoolBuilder::new()
                .num_threads(bf_threads)
                .build()
            {
                pool.install(|| {
                    report.backfill_updates.par_iter().for_each_init(
                        || vec![0u8; 1024 * 1024],
                        |buf, item| run_one_backfill(item, buf),
                    );
                });
            } else {
                let mut buf = vec![0u8; 1024 * 1024];
                for item in &report.backfill_updates {
                    run_one_backfill(item, &mut buf);
                }
            }
        } else {
            let mut buf = vec![0u8; 1024 * 1024];
            for item in &report.backfill_updates {
                run_one_backfill(item, &mut buf);
            }
        }

        if cancel.is_cancelled() {
            return Err(anyhow!("Sync cancelled."));
        }
        backfill_replaced = bf_ok.load(Ordering::Relaxed);
        backfill_failed = bf_fail.load(Ordering::Relaxed);
        notes.extend(bf_notes.into_inner());
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
        // Partition newline-containing paths (unsafe for --files-from-raw) into single copyto calls,
        // and reject any destination path traversing an existing local directory symlink!
        let mut batch_dl: Vec<(&SyncDiffItem, &str)> = Vec::new();
        let mut single_dl: Vec<(&SyncDiffItem, &str)> = Vec::new();
        let mut safe_dl_items: Vec<&SyncDiffItem> = Vec::new();
        let mut all_dl_raws: Vec<String> = Vec::new();
        for item in &download_list {
            let raw = ctx
                .remote_map
                .get(&item.rel_path)
                .map(|m| m.raw_path.as_str())
                .unwrap_or(item.rel_path.as_str());
            let target_local = join_rel_path(local_root, raw);
            if let Err(err) = ensure_no_symlink_ancestors(local_root, &target_local) {
                download_failed += 1;
                notes.push(format!("Blocked unsafe download {}: {err}", item.rel_path));
                continue;
            }
            safe_dl_items.push(item);
            all_dl_raws.push(raw.to_string());
            if raw.contains('\n') || raw.contains('\r') {
                single_dl.push((item, raw));
            } else {
                batch_dl.push((item, raw));
            }
        }

        let mut dl_had_errors = false;
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
                    "--immutable",
                    "--partial-suffix",
                    ".truongsync.part",
                    "--files-from-raw",
                ])
                .arg(manifest_file.path())
                .args([
                    if batch_dl.len() <= 500 {
                        "--no-traverse"
                    } else {
                        "--fast-list"
                    },
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
            cmd.arg("--").arg(&remote).arg(local_root);
            let out = match run_rclone_cancellable(cmd, cancel) {
                Ok(o) => o,
                Err(e) => {
                    cleanup_cancelled_cloud_parts(local_root, &all_dl_raws);
                    return Err(e);
                }
            };
            if !out.success {
                dl_had_errors = true;
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                notes.push(format!("Drive-to-PC pass reported errors: {err}"));
            }
        }

        for (item, raw) in single_dl {
            if cancel.is_cancelled() {
                cleanup_cancelled_cloud_parts(local_root, &all_dl_raws);
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
            cmd.arg("--").arg(&rfile).arg(&lfile);
            let out = match run_rclone_cancellable(cmd, cancel) {
                Ok(o) => o,
                Err(e) => {
                    cleanup_cancelled_cloud_parts(local_root, &all_dl_raws);
                    return Err(e);
                }
            };
            if !out.success {
                dl_had_errors = true;
                notes.push(format!(
                    "Download failed for {}: {}",
                    item.rel_path,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }

        // Verify which downloaded files actually exist as regular files with expected size
        // (and matching MD5 when rclone reported errors, so failed --immutable downloads never pass!).
        let mut dl_entries = Vec::new();
        for item in &safe_dl_items {
            let raw = ctx
                .remote_map
                .get(&item.rel_path)
                .map(|m| m.raw_path.as_str())
                .unwrap_or(item.rel_path.as_str());
            let p_raw = join_rel_path(local_root, raw);
            let p_norm = join_rel_path(local_root, &item.rel_path);
            let found = if p_raw.is_file() {
                let m = std::fs::metadata(&p_raw);
                Some((p_raw, m))
            } else if p_norm.is_file() {
                let m = std::fs::metadata(&p_norm);
                Some((p_norm, m))
            } else {
                None
            };
            let r_md5 = ctx
                .remote_map
                .get(&item.rel_path)
                .and_then(|rm| rm.md5.clone());
            match found {
                Some((path, Ok(m)))
                    if m.is_file() && item.remote_bytes.is_none_or(|rb| m.len() == rb) =>
                {
                    let md5_ok = if dl_had_errors {
                        match r_md5.as_deref() {
                            Some(expected) => compute_file_md5(&path)
                                .map(|actual| actual.eq_ignore_ascii_case(expected))
                                .unwrap_or(false),
                            None => false,
                        }
                    } else {
                        true
                    };
                    if !md5_ok {
                        download_failed += 1;
                        continue;
                    }
                    downloaded_count += 1;
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
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
                .args(["copy", "--immutable", "--files-from-raw"])
                .arg(ul_manifest.path())
                .args([
                    if batch_ul.len() <= 500 {
                        "--no-traverse"
                    } else {
                        "--fast-list"
                    },
                    "--drive-chunk-size",
                    "64M",
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
            cmd.arg("--").arg(local_root).arg(&remote);
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
                    .map(|(item, loc_meta)| ManifestEntry {
                        profile_kind: "cloud".into(),
                        profile_id: profile.id.clone(),
                        rel_path: item.rel_path.clone(),
                        size_bytes: loc_meta.size as i64,
                        md5_hash: None,
                        mtime_unix: loc_meta.mtime_unix,
                        state: "synced".into(),
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
                    "--drive-chunk-size",
                    "64M",
                    "--drive-skip-gdocs",
                    "--drive-stop-on-upload-limit",
                ]);
                if let Some(ref bw) = bwlimit {
                    cmd.args(["--bwlimit", bw]);
                }
                cmd.arg("--").arg(&loc_meta.abs_path).arg(&rfile);
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
        failed_count: total_failures,
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
