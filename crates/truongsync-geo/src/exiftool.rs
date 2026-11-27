use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use truongsync_ingest::transfer::HARDCODED_VIDEO_EXTS;

use crate::matcher::{MatchStatus, PhotoGpsMatch};

/// Hardcoded allowlist of photo extensions permitted for EXIF GPS mutation.
const ALLOWED_PHOTO_EXTS: &[&str] = &[
    "cr3", "cr2", "arw", "nef", "raf", "dng", "orf", "rw2", "pef", "srw", "3fr", "iiq", "jpg",
    "jpeg", "heic", "heif", "hif", "tif", "tiff", "png", "webp",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAvailability {
    pub available: bool,
    pub bundled: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeotagWriteRecord {
    pub path: String,
    pub pre_md5: String,
    pub post_md5: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeotagExecutionResult {
    pub tagged_count: usize,
    pub skipped_count: usize,
    pub failed_count: usize,
    #[serde(default)]
    pub cancelled: bool,
    pub updated_records: Vec<GeotagWriteRecord>,
    pub failures: Vec<(String, String)>,
}

/// On Windows, assigns a spawned ExifTool launcher process to a Job Object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so dropping the guard terminates BOTH
/// `exiftool.exe` and its child `exiftool_files\perl.exe` process tree cleanly.
#[cfg(windows)]
struct ChildJobGuard(Option<windows::Win32::Foundation::HANDLE>);

#[cfg(windows)]
impl ChildJobGuard {
    fn attach(pid: u32) -> Self {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        let Ok(job) = (unsafe { CreateJobObjectW(None, PCWSTR::null()) }) else {
            return Self(None);
        };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .is_err()
        {
            let _ = unsafe { CloseHandle(job) };
            return Self(None);
        }
        let Ok(proc_handle) =
            (unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) })
        else {
            let _ = unsafe { CloseHandle(job) };
            return Self(None);
        };
        let assigned = unsafe { AssignProcessToJobObject(job, proc_handle) };
        let _ = unsafe { CloseHandle(proc_handle) };
        if assigned.is_err() {
            let _ = unsafe { CloseHandle(job) };
            return Self(None);
        }
        Self(Some(job))
    }

    fn terminate_and_close(&mut self) {
        if let Some(job) = self.0.take() {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(job) };
        }
    }
}

#[cfg(windows)]
impl Drop for ChildJobGuard {
    fn drop(&mut self) {
        self.terminate_and_close();
    }
}

#[cfg(not(windows))]
struct ChildJobGuard;

#[cfg(not(windows))]
impl ChildJobGuard {
    fn attach(_pid: u32) -> Self {
        Self
    }

    fn terminate_and_close(&mut self) {}
}

fn new_exiftool_cmd(bin: &Path) -> Command {
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

/// Strips Windows verbatim `\\?\` prefix returned by `std::fs::canonicalize()` because ExifTool's
/// `IsPC()` path loop rewrites `\` to `/`, turning `\\?\C:\...` into invalid `//?/C:/...`.
/// Paired with `-api WindowsLongPath=1` inside `-common_args`, ExifTool safely re-adds `\\?\` when needed.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn strip_windows_verbatim_str(s: &str) -> String {
    if s.as_bytes()
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(br"\\?\UNC\"))
    {
        return format!(r"\\{}", &s[8..]);
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return rest.to_string();
        }
    }
    s.to_string()
}

fn exiftool_safe_path_string(canon_path: &Path) -> String {
    let s = canon_path.to_string_lossy();
    #[cfg(windows)]
    {
        strip_windows_verbatim_str(&s)
    }
    #[cfg(not(windows))]
    {
        s.into_owned()
    }
}

/// Strictly verifies a path is a photo (never a video, even through a symlink).
pub fn is_safe_photo_target(path: &Path) -> bool {
    let check_ext = |p: &Path| -> bool {
        let Some(ext) = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
        else {
            return false;
        };
        if HARDCODED_VIDEO_EXTS.contains(&ext.as_str()) {
            return false;
        }
        ALLOWED_PHOTO_EXTS.contains(&ext.as_str())
    };
    if !check_ext(path) {
        return false;
    }
    if let Ok(canon) = std::fs::canonicalize(path) {
        if !check_ext(&canon) {
            return false;
        }
    }
    true
}

/// Locates the `exiftool` binary (user override -> bundled resource -> Homebrew/common -> PATH).
pub fn resolve_exiftool_binary(override_path: Option<&str>) -> Option<PathBuf> {
    resolve_exiftool_with_origin(override_path).map(|(p, _)| p)
}

fn is_interactive_windows_exiftool(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase().contains("(-k)"))
        .unwrap_or(false)
}

fn resolve_exiftool_with_origin(override_path: Option<&str>) -> Option<(PathBuf, bool)> {
    if let Some(p) = override_path.filter(|s| !s.trim().is_empty()) {
        let pb = PathBuf::from(p.trim());
        if pb.is_file() && !is_interactive_windows_exiftool(&pb) {
            return Some((pb, false));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            #[cfg(windows)]
            let bundled_candidates = [
                dir.join("exiftool/exiftool.exe"),
                dir.join("resources/exiftool/exiftool.exe"),
                dir.join("exiftool.exe"),
            ];
            #[cfg(not(windows))]
            let bundled_candidates = [
                dir.join("../Resources/exiftool/exiftool"),
                dir.join("resources/exiftool/exiftool"),
                dir.join("exiftool/exiftool"),
                dir.join("exiftool"),
            ];
            for cand in bundled_candidates {
                if cand.is_file() {
                    return Some((cand, true));
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        for p in ["/opt/homebrew/bin/exiftool", "/usr/local/bin/exiftool"] {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Some((pb, false));
            }
        }
    }

    let names: &[&str] = if cfg!(windows) {
        &["exiftool.exe"]
    } else {
        &["exiftool"]
    };
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some((cand, false));
                }
            }
        }
    }
    None
}

pub fn check_exiftool(override_path: Option<&str>) -> ToolAvailability {
    let Some((bin, bundled)) = resolve_exiftool_with_origin(override_path) else {
        return ToolAvailability {
            available: false,
            bundled: false,
            path: None,
            version: None,
        };
    };
    let version = (|| -> Option<String> {
        let mut child = new_exiftool_cmd(&bin)
            .args(["-config", "", "-ver"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut job_guard = ChildJobGuard::attach(child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    let mut out = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
                    }
                    let trimmed = out.trim().to_string();
                    return (!trimmed.is_empty()).then_some(trimmed);
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        job_guard.terminate_and_close();
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => return None,
            }
        }
    })();
    ToolAvailability {
        available: version.is_some(),
        bundled,
        path: Some(bin.to_string_lossy().to_string()),
        version,
    }
}

pub fn compute_file_md5(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Writes matched GPS coordinates to RAW & JPEG photos in a single persistent
/// `exiftool -config "" -stay_open True -@ - -common_args -charset filename=UTF8` session.
///
/// Safety guarantees:
/// - `-config ""` isolates ExifTool from user/CWD `.ExifTool_config` files (no `-q`/`-lang`/arbitrary Perl)
/// - Hardcoded photo-only allowlist + video denylist + ExifTool strict `image/*` `-if` guard
/// - Signed decimal coordinates (`lat`/`lon`) paired with `GPSLatitudeRef`/`GPSLongitudeRef` so BOTH
///   EXIF unsigned rationals and any existing XMP-exif GPS tags preserve Western/Southern hemispheres
/// - `-common_args -charset filename=UTF8` (+ `-api WindowsLongPath=1` on Windows) persisted across all files
/// - Threaded stdout & stderr framing with live cancel polling + per-file timeout + Windows Job Object cleanup
/// - ValueConv `-GPSAltitudeRef#={0|1}` so below-sea-level altitudes write accurately
/// - Invokes `on_updated` immediately per file when `post_md5 != pre_md5` so backfill hashes survive crashes
pub fn write_geotags_batch<F>(
    items: &[PhotoGpsMatch],
    exiftool_override: Option<&str>,
    bump_mtime: bool,
    cancel: &CancellationToken,
    on_progress: F,
) -> Result<GeotagExecutionResult>
where
    F: Fn(usize, usize, &str),
{
    write_geotags_batch_with_hook(
        items,
        exiftool_override,
        bump_mtime,
        cancel,
        on_progress,
        |_| {},
    )
}

pub fn write_geotags_batch_with_hook<F, U>(
    items: &[PhotoGpsMatch],
    exiftool_override: Option<&str>,
    bump_mtime: bool,
    cancel: &CancellationToken,
    on_progress: F,
    on_updated: U,
) -> Result<GeotagExecutionResult>
where
    F: Fn(usize, usize, &str),
    U: Fn(&GeotagWriteRecord),
{
    const PER_FILE_TIMEOUT: Duration = Duration::from_secs(120);

    let bin = resolve_exiftool_binary(exiftool_override).ok_or_else(|| {
        anyhow!("ExifTool was not found. Reinstall TruongSync or set ExifTool's path in Settings.")
    })?;

    let mut to_write: Vec<&PhotoGpsMatch> = Vec::new();
    let mut skipped_count = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();

    for item in items {
        if item.status != MatchStatus::Matched || item.media_kind.eq_ignore_ascii_case("video") {
            skipped_count += 1;
            continue;
        }
        let path = Path::new(&item.path);
        if !is_safe_photo_target(path) {
            // Hard refusal: never touch videos or non-photo files even if IPC payload claims otherwise
            skipped_count += 1;
            continue;
        }
        let (Some(lat), Some(lon)) = (item.lat, item.lon) else {
            skipped_count += 1;
            continue;
        };
        if !lat.is_finite()
            || !lon.is_finite()
            || !(-90.0..=90.0).contains(&lat)
            || !(-180.0..=180.0).contains(&lon)
        {
            failures.push((
                item.path.clone(),
                format!("Invalid GPS coordinates ({lat}, {lon})"),
            ));
            continue;
        }
        to_write.push(item);
    }

    if to_write.is_empty() {
        return Ok(GeotagExecutionResult {
            tagged_count: 0,
            skipped_count,
            failed_count: failures.len(),
            cancelled: cancel.is_cancelled(),
            updated_records: Vec::new(),
            failures,
        });
    }

    let mut cmd = new_exiftool_cmd(&bin);
    #[allow(unused_mut)]
    let mut spawn_args = vec![
        "-config",
        "",
        "-stay_open",
        "True",
        "-@",
        "-",
        "-common_args",
        "-charset",
        "filename=UTF8",
    ];
    #[cfg(windows)]
    spawn_args.extend(["-api", "WindowsLongPath=1"]);

    cmd.args(spawn_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn ExifTool at {}", bin.display()))?;
    let mut job_guard = ChildJobGuard::attach(child.id());
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ExifTool stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open ExifTool stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to open ExifTool stderr"))?;

    // Dedicated stdout reader thread so the main loop can poll with timeouts and live cancellation.
    let (stdout_tx, stdout_rx) = mpsc::channel::<Result<String, String>>();
    let stdout_handle = std::thread::Builder::new()
        .name("ts-exiftool-stdout".into())
        .spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut byte_buf = Vec::new();
            loop {
                byte_buf.clear();
                match r.read_until(b'\n', &mut byte_buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&byte_buf).trim().to_string();
                        if stdout_tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = stdout_tx.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        })
        .ok();

    // Race-free stderr framing: ExifTool emits `{stderr_ready{seq}}` via `-echo4` right before `{ready{seq}}`.
    let (stderr_tx, stderr_rx) = mpsc::channel::<(usize, Vec<String>)>();
    let stderr_handle = std::thread::Builder::new()
        .name("ts-exiftool-stderr".into())
        .spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut byte_buf = Vec::new();
            let mut cur_lines: Vec<String> = Vec::new();
            loop {
                byte_buf.clear();
                match r.read_until(b'\n', &mut byte_buf) {
                    Ok(0) => {
                        if !cur_lines.is_empty() {
                            let _ = stderr_tx.send((0, std::mem::take(&mut cur_lines)));
                        }
                        break;
                    }
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&byte_buf);
                        let trimmed = line.trim();
                        if let Some(rest) = trimmed
                            .strip_prefix("{stderr_ready")
                            .and_then(|s| s.strip_suffix('}'))
                        {
                            if let Ok(seq) = rest.parse::<usize>() {
                                let _ = stderr_tx.send((seq, std::mem::take(&mut cur_lines)));
                                continue;
                            }
                        }
                        if !trimmed.is_empty() && cur_lines.len() < 200 {
                            cur_lines.push(trimmed.to_string());
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();

    let mut updated_records = Vec::with_capacity(to_write.len());
    let total = to_write.len();
    let mut protocol_broken = false;
    let mut was_cancelled = false;
    let mut in_flight: Option<(String, PathBuf, String)> = None;

    for (idx, item) in to_write.iter().enumerate() {
        if cancel.is_cancelled() {
            was_cancelled = true;
            break;
        }
        let raw_path = Path::new(&item.path);
        let canon_path = match std::fs::canonicalize(raw_path) {
            Ok(p) => p,
            Err(e) => {
                failures.push((item.path.clone(), format!("File unavailable: {e}")));
                on_progress(idx + 1, total, &item.filename);
                continue;
            }
        };
        if !canon_path.is_file() || !is_safe_photo_target(&canon_path) {
            skipped_count += 1;
            on_progress(idx + 1, total, &item.filename);
            continue;
        }
        let safe_path_str = exiftool_safe_path_string(&canon_path);
        if safe_path_str.contains('\n') || safe_path_str.contains('\r') {
            failures.push((
                item.path.clone(),
                "Filename contains newline characters unsupported by ExifTool argfile".into(),
            ));
            on_progress(idx + 1, total, &item.filename);
            continue;
        }

        let (Some(lat), Some(lon)) = (item.lat, item.lon) else {
            on_progress(idx + 1, total, &item.filename);
            continue;
        };
        let pre_md5 = match compute_file_md5(&canon_path) {
            Ok(h) => h,
            Err(e) => {
                failures.push((item.path.clone(), e.to_string()));
                on_progress(idx + 1, total, &item.filename);
                continue;
            }
        };

        let seq = idx + 1;
        let lat_ref = if lat >= 0.0 { "N" } else { "S" };
        let lon_ref = if lon >= 0.0 { "E" } else { "W" };

        let mut args = vec![
            "-overwrite_original".to_string(),
            // Strict allowlist defense-in-depth: ExifTool only mutates true `image/*` containers
            // (refuses MP4/MOV/MKV/MXF/application/* even if renamed to .jpg).
            "-if".to_string(),
            "defined $MIMEType and $MIMEType =~ /^image\\//i".to_string(),
            "-GPSMapDatum=WGS-84".to_string(),
            // Pass signed coordinates + explicit Ref tags: ExifTool strips the sign automatically
            // for unsigned EXIF rationals while preserving W/S hemisphere for any existing XMP-exif tags.
            format!("-GPSLatitude={lat:.7}"),
            format!("-GPSLatitudeRef={lat_ref}"),
            format!("-GPSLongitude={lon:.7}"),
            format!("-GPSLongitudeRef={lon_ref}"),
        ];
        if let Some(ele) = item.ele.filter(|v| v.is_finite()) {
            args.push(format!("-GPSAltitude={:.2}", ele.abs()));
            // ValueConv `#` guarantees numeric 0 = Above Sea Level, 1 = Below Sea Level!
            args.push(format!(
                "-GPSAltitudeRef#={}",
                if ele >= 0.0 { 0 } else { 1 }
            ));
        } else if item.has_existing_gps {
            // Clear stale altitude when overwriting an existing GPS fix with a 2D GPX track
            args.push("-GPSAltitude=".to_string());
            args.push("-GPSAltitudeRef=".to_string());
        }
        if let Some(ref ts) = item.capture_time_utc {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
                let utc = dt.with_timezone(&Utc);
                args.push(format!("-GPSDateStamp={}", utc.format("%Y:%m:%d")));
                args.push(format!("-GPSTimeStamp={}", utc.format("%H:%M:%S")));
            }
        }
        if !bump_mtime {
            args.push("-P".to_string());
        }
        args.push("-echo4".to_string());
        args.push(format!("{{stderr_ready{seq}}}"));
        args.push(safe_path_str);
        args.push(format!("-execute{seq}"));

        let payload = format!("{}\n", args.join("\n"));
        in_flight = Some((item.path.clone(), canon_path.clone(), pre_md5.clone()));
        if let Err(e) = stdin
            .write_all(payload.as_bytes())
            .and_then(|_| stdin.flush())
        {
            in_flight = None;
            protocol_broken = true;
            failures.push((
                item.path.clone(),
                format!("ExifTool pipe write failed: {e}"),
            ));
            for rem in &to_write[idx + 1..] {
                failures.push((
                    rem.path.clone(),
                    "Skipped after ExifTool process exited unexpectedly".into(),
                ));
            }
            break;
        }

        let expected_ready = format!("{{ready{seq}}}");
        let mut output_lines = Vec::new();
        let mut reached_ready = false;
        let mut timed_out = false;
        let file_deadline = Instant::now() + PER_FILE_TIMEOUT;
        loop {
            if cancel.is_cancelled() {
                was_cancelled = true;
                protocol_broken = true;
                break;
            }
            let remaining = file_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                protocol_broken = true;
                break;
            }
            let wait_slice = remaining.min(Duration::from_millis(100));
            match stdout_rx.recv_timeout(wait_slice) {
                Ok(Ok(line)) => {
                    if line == expected_ready {
                        reached_ready = true;
                        break;
                    }
                    if !line.is_empty() {
                        output_lines.push(line);
                    }
                }
                Ok(Err(e)) => {
                    output_lines.push(format!("read error: {e}"));
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        if was_cancelled {
            break;
        }

        // Wait briefly for the matching `{stderr_ready{seq}}` sentinel so stderr diagnostics are race-free
        let mut err_lines = Vec::new();
        if reached_ready {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let rem = deadline.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    break;
                }
                match stderr_rx.recv_timeout(rem) {
                    Ok((rx_seq, lines)) => {
                        if rx_seq == seq || rx_seq == 0 {
                            err_lines = lines;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        } else {
            while let Ok((_, lines)) = stderr_rx.try_recv() {
                err_lines.extend(lines);
            }
        }

        let combined = output_lines.join(" | ");
        let errs = err_lines.join(" | ");

        if combined.contains("1 image files updated") {
            in_flight = None;
            match compute_file_md5(&canon_path) {
                Ok(post_md5) if post_md5 != pre_md5 => {
                    let rec = GeotagWriteRecord {
                        path: item.path.clone(),
                        pre_md5,
                        post_md5,
                    };
                    on_updated(&rec);
                    updated_records.push(rec);
                }
                Ok(_) => {
                    skipped_count += 1;
                }
                Err(e) => failures.push((item.path.clone(), e.to_string())),
            }
        } else if combined.contains("1 image files unchanged")
            || combined.contains("1 files failed condition")
        {
            in_flight = None;
            skipped_count += 1;
        } else {
            if reached_ready {
                in_flight = None;
            }
            let detail = if timed_out {
                format!(
                    "ExifTool timed out after {}s while writing GPS metadata",
                    PER_FILE_TIMEOUT.as_secs()
                )
            } else {
                match (combined.is_empty(), errs.is_empty()) {
                    (false, false) => format!("{combined} ({errs})"),
                    (false, true) => combined,
                    (true, false) => errs,
                    (true, true) => "ExifTool did not confirm update".into(),
                }
            };
            failures.push((item.path.clone(), detail));
        }

        on_progress(idx + 1, total, &item.filename);

        // If ExifTool did not emit `{ready{seq}}` (e.g. crashed/timed out after printing
        // "1 image files updated"), abort the batch immediately rather than writing to a
        // dead or desynchronized process.
        if !reached_ready {
            protocol_broken = true;
            for rem in &to_write[idx + 1..] {
                failures.push((
                    rem.path.clone(),
                    "Skipped after ExifTool process terminated early".into(),
                ));
            }
            break;
        }
    }

    // Drop channel receivers BEFORE waiting so ExifTool cannot block writing to a full pipe
    drop(stdout_rx);
    drop(stderr_rx);

    let mut force_killed = false;
    if protocol_broken {
        drop(stdin);
        job_guard.terminate_and_close();
        let _ = child.kill();
        let _ = child.wait();
        force_killed = true;
    } else {
        let _ = stdin.write_all(b"-stay_open\nFalse\n");
        let _ = stdin.flush();
        drop(stdin);
        let wait_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < wait_deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    job_guard.terminate_and_close();
                    let _ = child.kill();
                    let _ = child.wait();
                    force_killed = true;
                    break;
                }
            }
        }
    }

    // After force-killing an in-flight write (cancel or timeout), clean up any orphaned
    // `_exiftool_tmp` sibling and reconcile MD5 so a photo already atomically replaced on disk
    // right before termination never loses its `intentional_updates` backfill record.
    if let Some((item_path, canon_path, pre_md5)) = in_flight {
        let mut tmp_os = canon_path.as_os_str().to_os_string();
        tmp_os.push("_exiftool_tmp");
        let _ = std::fs::remove_file(PathBuf::from(tmp_os));
        if let Ok(post_md5) = compute_file_md5(&canon_path) {
            if post_md5 != pre_md5 {
                let rec = GeotagWriteRecord {
                    path: item_path,
                    pre_md5,
                    post_md5,
                };
                on_updated(&rec);
                updated_records.push(rec);
            }
        }
    }
    // Only join pipe reader threads on clean exit; after a force kill, detaching prevents
    // any chance of hanging if an OS descendant briefly retained an inherited pipe handle.
    if !force_killed {
        if let Some(handle) = stdout_handle {
            let _ = handle.join();
        }
        if let Some(handle) = stderr_handle {
            let _ = handle.join();
        }
    }

    Ok(GeotagExecutionResult {
        tagged_count: updated_records.len(),
        skipped_count,
        failed_count: failures.len(),
        cancelled: was_cancelled || cancel.is_cancelled(),
        updated_records,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_windows_verbatim_handles_non_ascii_at_byte_8() {
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\D:\Ảnh\2026\IMG_0001.CR3"),
            r"D:\Ảnh\2026\IMG_0001.CR3"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\写真\IMG_0001.JPG"),
            r"C:\写真\IMG_0001.JPG"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\server\share\Été.jpg"),
            r"\\server\share\Été.jpg"
        );
    }
}
