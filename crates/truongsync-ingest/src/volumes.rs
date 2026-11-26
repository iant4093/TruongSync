#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedVolume {
    pub mount_path: String,
    pub label: String,
    pub volume_uuid: String,
    pub is_removable: bool,
    pub has_dcim: bool,
    pub writable: bool,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub bus_protocol: String,
}

#[cfg(target_os = "macos")]
fn is_macos_real_volume_mount(vol_mount: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(vols_meta) = std::fs::metadata("/Volumes") else {
        return false;
    };
    let Ok(m_meta) = std::fs::metadata(vol_mount) else {
        return false;
    };
    m_meta.dev() != vols_meta.dev()
}

/// Returns free bytes available on the filesystem containing `path`.
/// Refuses to silently fall back to Macintosh HD or `C:` when a target volume/drive/UNC share
/// is not mounted.
pub fn disk_free_bytes(path: &Path) -> Result<u64> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(rel) = path.strip_prefix("/Volumes") {
            if let Some(first) = rel.components().next() {
                let vol_mount = Path::new("/Volumes").join(first.as_os_str());
                let is_root_symlink = std::fs::symlink_metadata(&vol_mount)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !vol_mount.exists()
                    || (!is_root_symlink && !is_macos_real_volume_mount(&vol_mount))
                {
                    return Err(anyhow!(
                        "Destination volume '{}' is not mounted.",
                        vol_mount.display()
                    ));
                }
            }
        }
    }

    let probe = existing_ancestor(path)?;
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cstr =
            CString::new(probe.as_os_str().as_bytes()).context("invalid path bytes for statvfs")?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cstr.as_ptr(), &mut stat) };
        if rc != 0 {
            return Err(anyhow!(
                "statvfs failed for {}: {}",
                probe.display(),
                std::io::Error::last_os_error()
            ));
        }
        #[allow(clippy::unnecessary_cast)]
        let block_size = if stat.f_frsize > 0 {
            stat.f_frsize as u64
        } else {
            stat.f_bsize as u64
        };
        #[allow(clippy::unnecessary_cast)]
        Ok((stat.f_bavail as u64).saturating_mul(block_size))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut s = probe.to_string_lossy().to_string();
        if !s.ends_with('\\') && !s.ends_with('/') {
            s.push('\\');
        }
        let wide: Vec<u16> = std::ffi::OsStr::new(&s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_avail: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide.as_ptr()),
                Some(&mut free_avail),
                Some(&mut total),
                Some(&mut total_free),
            )
            .map_err(|e| anyhow!("GetDiskFreeSpaceExW failed for {}: {e}", probe.display()))?;
        }
        Ok(free_avail)
    }
}

/// Checks OS filesystem read-only mount status (`ST_RDONLY` on Unix / `FILE_READ_ONLY_VOLUME` on Windows).
pub fn is_filesystem_readonly(path: &Path) -> Option<bool> {
    let probe = existing_ancestor(path).ok()?;
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cstr = CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(cstr.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((stat.f_flag as u64 & libc::ST_RDONLY as u64) != 0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
        let probe_w: Vec<u16> = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut vol_buf = vec![0u16; 512];
        unsafe {
            GetVolumePathNameW(PCWSTR(probe_w.as_ptr()), &mut vol_buf).ok()?;
        }
        let mut flags: u32 = 0;
        unsafe {
            GetVolumeInformationW(
                PCWSTR(vol_buf.as_ptr()),
                None,
                None,
                None,
                Some(&mut flags),
                None,
            )
            .ok()?;
        }
        const FILE_READ_ONLY_VOLUME: u32 = 0x00080000;
        Some((flags & FILE_READ_ONLY_VOLUME) != 0)
    }
}

fn existing_ancestor(path: &Path) -> Result<PathBuf> {
    let has_root_or_prefix = path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::Prefix(_)));
    let mut cur = path.to_path_buf();
    while !cur.exists() {
        match cur.parent() {
            Some(p) if p != cur && !p.as_os_str().is_empty() => cur = p.to_path_buf(),
            _ => {
                if has_root_or_prefix {
                    return Err(anyhow!(
                        "Drive, volume, or share for '{}' is not mounted or available.",
                        path.display()
                    ));
                }
                return Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            }
        }
    }
    Ok(cur)
}

/// Checks if a directory contains a camera media folder (`DCIM`, Sony `PRIVATE/M4ROOT`,
/// Panasonic `PRIVATE/AVCHD`, or Canon Cinema `CONTENTS`, case-insensitive).
pub fn has_dcim_dir(mount: &Path) -> bool {
    if mount.join("DCIM").is_dir() || mount.join("dcim").is_dir() {
        return true;
    }
    if let Ok(rd) = std::fs::read_dir(mount) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() {
                continue;
            }
            if name.eq_ignore_ascii_case("DCIM") || name.eq_ignore_ascii_case("CONTENTS") {
                return true;
            }
            if name.eq_ignore_ascii_case("PRIVATE") {
                if let Ok(sub_rd) = std::fs::read_dir(entry.path()) {
                    for sub in sub_rd.flatten() {
                        if sub.path().is_dir() {
                            let sn = sub.file_name().to_string_lossy().to_string();
                            if sn.eq_ignore_ascii_case("M4ROOT") || sn.eq_ignore_ascii_case("AVCHD")
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Enumerates mounted removable and external volumes across macOS and Windows.
pub fn list_mounted_volumes() -> Vec<DetectedVolume> {
    #[cfg(target_os = "macos")]
    {
        list_macos_volumes()
    }
    #[cfg(target_os = "windows")]
    {
        list_windows_volumes()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
fn list_macos_volumes() -> Vec<DetectedVolume> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/Volumes") else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Skip root volume symlink (`/Volumes/Macintosh HD` -> `/`)
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                continue;
            }
        }
        // Skip stale empty folders on Macintosh HD inside `/Volumes` (not actual mount points)
        if !is_macos_real_volume_mount(&path) {
            continue;
        }
        let label = entry.file_name().to_string_lossy().to_string();
        if label.starts_with('.') {
            continue;
        }
        let mut vol = DetectedVolume {
            mount_path: path.to_string_lossy().to_string(),
            label: label.clone(),
            volume_uuid: String::new(),
            is_removable: false,
            has_dcim: has_dcim_dir(&path),
            writable: !is_filesystem_readonly(&path).unwrap_or(false),
            total_bytes: 0,
            free_bytes: disk_free_bytes(&path).unwrap_or(0),
            bus_protocol: String::new(),
        };

        let mut is_disk_image = false;
        let mut is_internal_non_removable = false;
        let mut diskutil_ok = false;

        if let Ok(output) = std::process::Command::new("/usr/sbin/diskutil")
            .args(["info", "-plist"])
            .arg(&path)
            .output()
        {
            if output.status.success() {
                diskutil_ok = true;
                if let Ok(plist::Value::Dictionary(dict)) = plist::from_bytes(&output.stdout) {
                    if let Some(s) = dict
                        .get("VolumeUUID")
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.trim().is_empty())
                    {
                        vol.volume_uuid = s.to_string();
                    } else if let Some(s) = dict
                        .get("DiskUUID")
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.trim().is_empty())
                    {
                        vol.volume_uuid = s.to_string();
                    }
                    if let Some(s) = dict
                        .get("VolumeName")
                        .and_then(|v| v.as_string())
                        .filter(|s| !s.trim().is_empty())
                    {
                        vol.label = s.to_string();
                    }
                    if let Some(s) = dict.get("BusProtocol").and_then(|v| v.as_string()) {
                        vol.bus_protocol = s.to_string();
                        if s.eq_ignore_ascii_case("Disk Image") {
                            is_disk_image = true;
                        }
                    }
                    let ejectable = dict
                        .get("Ejectable")
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false);
                    let removable = dict
                        .get("RemovableMediaOrExternalDevice")
                        .or_else(|| dict.get("RemovableMedia"))
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(false);
                    let internal = dict
                        .get("Internal")
                        .and_then(|v| v.as_boolean())
                        .unwrap_or(true);
                    vol.is_removable = (ejectable || removable || !internal) && !is_disk_image;
                    is_internal_non_removable = internal && !removable && !ejectable;

                    if let Some(ro) = dict.get("WritableVolume").and_then(|v| v.as_boolean()) {
                        vol.writable = ro;
                    }
                    if let Some(total) = dict.get("TotalSize").and_then(|v| v.as_unsigned_integer())
                    {
                        vol.total_bytes = total;
                    }
                    if let Some(free) = dict
                        .get("VolumeFreeSpace")
                        .or_else(|| dict.get("APFSContainerFree"))
                        .and_then(|v| v.as_unsigned_integer())
                    {
                        vol.free_bytes = free;
                    }
                }
            }
        }
        // Skip mounted `.dmg` disk images and non-removable internal APFS partitions
        if is_disk_image || (is_internal_non_removable && !vol.has_dcim) {
            continue;
        }
        // Only fall back to label-derived ID when diskutil succeeded but reported no UUID
        // (prevents transient diskutil errors from mutating volume_uuid and triggering fake reconnects)
        if !diskutil_ok {
            continue;
        }
        if vol.volume_uuid.is_empty() {
            vol.volume_uuid = format!("label:{}", vol.label);
        }
        out.push(vol);
    }
    out.sort_by(|a, b| b.has_dcim.cmp(&a.has_dcim).then(a.label.cmp(&b.label)));
    out
}

#[cfg(target_os = "windows")]
fn list_windows_volumes() -> Vec<DetectedVolume> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
        GetVolumeNameForVolumeMountPointW,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        SetThreadErrorMode, SEM_FAILCRITICALERRORS, THREAD_ERROR_MODE,
    };

    struct ErrorModeGuard(THREAD_ERROR_MODE);
    impl Drop for ErrorModeGuard {
        fn drop(&mut self) {
            let mut prev = THREAD_ERROR_MODE(0);
            let _ = unsafe { SetThreadErrorMode(self.0, Some(&mut prev)) };
        }
    }
    let mut old_mode = THREAD_ERROR_MODE(0);
    let _guard = unsafe {
        if SetThreadErrorMode(SEM_FAILCRITICALERRORS, Some(&mut old_mode)).is_ok() {
            Some(ErrorModeGuard(old_mode))
        } else {
            None
        }
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let sys_drive = std::env::var("SystemDrive")
        .unwrap_or_else(|_| "C:".into())
        .trim_end_matches('\\')
        .to_string()
        + "\\";

    let mut buf = vec![0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    if len == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for slice in buf[..len].split(|&c| c == 0) {
        if slice.is_empty() {
            continue;
        }
        let drive_str = OsString::from_wide(slice).to_string_lossy().to_string();
        if drive_str.eq_ignore_ascii_case(&sys_drive) {
            continue;
        }
        let mut wide_root: Vec<u16> = slice.to_vec();
        wide_root.push(0);
        let dtype = unsafe { GetDriveTypeW(PCWSTR(wide_root.as_ptr())) };
        if dtype != DRIVE_REMOVABLE && dtype != DRIVE_FIXED {
            continue;
        }

        let mut label_buf = [0u16; 256];
        let mut fs_buf = [0u16; 64];
        let mut serial: u32 = 0;
        let mut max_comp: u32 = 0;
        let mut flags: u32 = 0;
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut label_buf),
                Some(&mut serial),
                Some(&mut max_comp),
                Some(&mut flags),
                Some(&mut fs_buf),
            )
        }
        .is_ok();
        if !ok {
            continue;
        }
        let label_end = label_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(label_buf.len());
        let label = OsString::from_wide(&label_buf[..label_end])
            .to_string_lossy()
            .to_string();

        let mut guid_buf = [0u16; 128];
        let volume_uuid = if unsafe {
            GetVolumeNameForVolumeMountPointW(PCWSTR(wide_root.as_ptr()), &mut guid_buf)
        }
        .is_ok()
        {
            let end = guid_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(guid_buf.len());
            OsString::from_wide(&guid_buf[..end])
                .to_string_lossy()
                .trim_matches(['\\', '?'])
                .to_string()
        } else {
            format!("serial:{serial:08X}")
        };

        let mut free_avail = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let _ = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide_root.as_ptr()),
                Some(&mut free_avail),
                Some(&mut total),
                Some(&mut total_free),
            )
        };

        let mount = PathBuf::from(&drive_str);
        let has_dcim = has_dcim_dir(&mount);
        const FILE_READ_ONLY_VOLUME: u32 = 0x00080000;
        out.push(DetectedVolume {
            mount_path: drive_str.clone(),
            label: if label.is_empty() {
                drive_str.trim_end_matches('\\').to_string()
            } else {
                label
            },
            volume_uuid,
            is_removable: dtype == DRIVE_REMOVABLE || has_dcim,
            has_dcim,
            writable: (flags & FILE_READ_ONLY_VOLUME) == 0,
            total_bytes: total,
            free_bytes: free_avail,
            bus_protocol: if dtype == DRIVE_REMOVABLE {
                "Removable".into()
            } else {
                "External/Fixed".into()
            },
        });
    }
    out.sort_by(|a, b| b.has_dcim.cmp(&a.has_dcim).then(a.label.cmp(&b.label)));
    out
}

/// Strictly matches a mounted volume by `volume_uuid` when configured.
/// Only falls back to case-insensitive `volume_label` when `volume_uuid` is empty,
/// preventing syncing to the wrong physical drive that shares a generic label like `UNTITLED`.
pub fn find_mounted_volume(volume_uuid: &str, volume_label: &str) -> Option<DetectedVolume> {
    let vols = list_mounted_volumes();
    let uuid = volume_uuid.trim();
    if !uuid.is_empty() {
        return vols
            .into_iter()
            .find(|v| v.volume_uuid.eq_ignore_ascii_case(uuid));
    }
    let label = volume_label.trim();
    if !label.is_empty() {
        return vols
            .into_iter()
            .find(|v| v.label.eq_ignore_ascii_case(label));
    }
    None
}

/// Validates a Windows drive root (`X:`, `X:\`, `X:/`, or `\\?\X:\`) before passing to OS eject helpers.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn validate_windows_drive_root(raw: &str) -> Option<String> {
    let mut trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        trimmed = rest;
    }
    let trimmed = trimmed.trim_end_matches(['\\', '/']);
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(trimmed.to_ascii_uppercase())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn validate_macos_ejectable_mount(mount_path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    // Reject any lexical traversal (`..`) before canonicalizing
    for comp in mount_path.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(anyhow!(
                "Refusing to eject path with traversal segments: {}",
                mount_path.display()
            ));
        }
    }
    let canon = std::fs::canonicalize(mount_path).with_context(|| {
        format!(
            "Refusing to eject non-existent mount path: {}",
            mount_path.display()
        )
    })?;
    let comps: Vec<Component<'_>> = canon.components().collect();
    if comps.len() != 3
        || !matches!(comps[0], Component::RootDir)
        || comps[1].as_os_str() != "Volumes"
        || !is_macos_real_volume_mount(&canon)
    {
        return Err(anyhow!(
            "Refusing to eject path that is not a top-level mounted volume under /Volumes: {}",
            canon.display()
        ));
    }
    // Verify via diskutil info -plist that the volume is actually removable/ejectable/external
    let info = std::process::Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist"])
        .arg(&canon)
        .output()
        .context("failed to query diskutil info before eject")?;
    if !info.status.success() {
        return Err(anyhow!(
            "Could not verify volume metadata before eject: {}",
            String::from_utf8_lossy(&info.stderr).trim()
        ));
    }
    let plist::Value::Dictionary(dict) =
        plist::from_bytes(&info.stdout).context("failed to parse diskutil info plist")?
    else {
        return Err(anyhow!("Unexpected diskutil info plist format"));
    };
    let ejectable = dict
        .get("Ejectable")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    let removable = dict
        .get("RemovableMediaOrExternalDevice")
        .or_else(|| dict.get("RemovableMedia"))
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    let internal = dict
        .get("Internal")
        .and_then(|v| v.as_boolean())
        .unwrap_or(true);
    if internal && !ejectable && !removable {
        return Err(anyhow!(
            "Refusing to eject internal non-removable volume: {}",
            canon.display()
        ));
    }
    Ok(canon)
}

/// Safely ejects a mounted removable volume.
pub fn eject_volume(mount_path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let validated = validate_macos_ejectable_mount(mount_path)?;
        // Note: macOS `diskutil eject` rejects `--`; validated path always begins with `/Volumes/`.
        let out = std::process::Command::new("/usr/sbin/diskutil")
            .arg("eject")
            .arg(&validated)
            .output()
            .context("failed to run diskutil eject")?;
        if !out.status.success() {
            return Err(anyhow!(
                "diskutil eject failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, GetDriveTypeW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows::Win32::System::Ioctl::{
            FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA,
            IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
        };
        use windows::Win32::System::IO::DeviceIoControl;

        let drive =
            validate_windows_drive_root(&mount_path.to_string_lossy()).ok_or_else(|| {
                anyhow!(
                    "Refusing to eject invalid Windows drive path: {}",
                    mount_path.display()
                )
            })?;
        if let Ok(sys_drive) = std::env::var("SystemDrive") {
            if sys_drive.trim().eq_ignore_ascii_case(&drive) {
                return Err(anyhow!("Refusing to eject Windows system drive {drive}."));
            }
        }
        let root_w: Vec<u16> = std::ffi::OsStr::new(&format!(r"{drive}\"))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // DRIVE_REMOVABLE = 2; refuse to forcibly dismount fixed/network/system partitions.
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
        if drive_type != 2 {
            return Err(anyhow!(
                "Refusing to eject non-removable Windows volume {drive} (drive type {drive_type})."
            ));
        }
        let dev_path = format!(r"\\.\{drive}");
        let wide: Vec<u16> = std::ffi::OsStr::new(&dev_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map_err(|e| anyhow!("Could not open volume {drive} for eject: {e}"))?;

        let mut bytes_returned = 0u32;
        let result = (|| -> Result<()> {
            unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_LOCK_VOLUME,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                )
                .map_err(|e| anyhow!("Volume {drive} is in use by another program: {e}"))?;

                DeviceIoControl(
                    handle,
                    FSCTL_DISMOUNT_VOLUME,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                )
                .map_err(|e| anyhow!("Failed to dismount volume {drive}: {e}"))?;

                let allow = PREVENT_MEDIA_REMOVAL {
                    PreventMediaRemoval: false,
                };
                let _ = DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_MEDIA_REMOVAL,
                    Some((&allow as *const PREVENT_MEDIA_REMOVAL).cast()),
                    std::mem::size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                );

                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_EJECT_MEDIA,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                )
                .map_err(|e| anyhow!("Failed to eject removable media {drive}: {e}"))?;
            }
            Ok(())
        })();

        let _ = unsafe { CloseHandle(handle) };
        result
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = mount_path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_windows_drive_roots_strictly() {
        assert_eq!(validate_windows_drive_root("H:\\"), Some("H:".into()));
        assert_eq!(validate_windows_drive_root("g:"), Some("G:".into()));
        assert_eq!(validate_windows_drive_root("H:'; calc.exe; '"), None);
        assert_eq!(validate_windows_drive_root("H:\\DCIM"), None);
    }
}
