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
        use std::path::Component;
        let mut comps = path.components();
        if matches!(comps.next(), Some(Component::RootDir))
            && matches!(
                comps.next(),
                Some(Component::Normal(seg)) if seg.to_string_lossy().eq_ignore_ascii_case("Volumes")
            )
        {
            if let Some(Component::Normal(first)) = comps.next() {
                let vol_mount = Path::new("/Volumes").join(first);
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
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows::core::PCWSTR;
        use windows::Win32::Devices::DeviceAndDriverInstallation::{
            CM_Get_Parent, CM_Request_Device_EjectW, PNP_VetoTypeUnknown,
            SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
            SetupDiGetDeviceInterfaceDetailW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
            HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        };
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Storage::FileSystem::{
            BusType1394, BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, GetDriveTypeW,
            FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows::Win32::System::Ioctl::{
            PropertyStandardQuery, StorageDeviceProperty, GUID_DEVINTERFACE_DISK,
            IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
            STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
        };
        use windows::Win32::System::IO::DeviceIoControl;

        struct HandleGuard(HANDLE);
        impl Drop for HandleGuard {
            fn drop(&mut self) {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
        struct DevSetGuard(HDEVINFO);
        impl Drop for DevSetGuard {
            fn drop(&mut self) {
                let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
            }
        }

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
        const DRIVE_REMOVABLE: u32 = 2;
        const DRIVE_FIXED: u32 = 3;
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
        if drive_type != DRIVE_REMOVABLE && drive_type != DRIVE_FIXED {
            return Err(anyhow!(
                "Refusing to eject unsupported Windows volume {drive} (drive type {drive_type})."
            ));
        }

        // Open with desired_access = 0 (metadata query only): works for standard non-admin
        // users AND physically write-protected SD cards.
        let dev_path = format!(r"\\.\{drive}");
        let wide: Vec<u16> = std::ffi::OsStr::new(&dev_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let vol_num: STORAGE_DEVICE_NUMBER = {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            }
            .map_err(|e| anyhow!("Could not open volume {drive} for safe eject: {e}"))?;
            let _h_guard = HandleGuard(handle);

            // Verify the underlying bus is removable or external (USB / SD / MMC / 1394)
            // so we allow USB CFexpress/SSD readers (`DRIVE_FIXED`) while blocking internal SATA/NVMe drives.
            let mut is_external_bus = drive_type == DRIVE_REMOVABLE;
            let query = STORAGE_PROPERTY_QUERY {
                PropertyId: StorageDeviceProperty,
                QueryType: PropertyStandardQuery,
                AdditionalParameters: [0],
            };
            let mut desc_buf = [0u64; 64];
            let mut desc_bytes = 0u32;
            if unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_QUERY_PROPERTY,
                    Some((&query as *const STORAGE_PROPERTY_QUERY).cast()),
                    std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                    Some(desc_buf.as_mut_ptr().cast()),
                    std::mem::size_of_val(&desc_buf) as u32,
                    Some(&mut desc_bytes),
                    None,
                )
            }
            .is_ok()
                && (desc_bytes as usize) >= std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>()
            {
                let desc = unsafe { &*(desc_buf.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>()) };
                if desc.RemovableMedia
                    || [BusTypeUsb, BusTypeSd, BusTypeMmc, BusType1394].contains(&desc.BusType)
                {
                    is_external_bus = true;
                }
            }
            if !is_external_bus {
                return Err(anyhow!(
                    "Refusing to eject internal fixed Windows volume {drive}."
                ));
            }

            let mut sdn = STORAGE_DEVICE_NUMBER {
                DeviceType: 0,
                DeviceNumber: 0,
                PartitionNumber: 0,
            };
            let mut returned = 0u32;
            unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    None,
                    0,
                    Some((&mut sdn as *mut STORAGE_DEVICE_NUMBER).cast()),
                    std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                    Some(&mut returned),
                    None,
                )
            }
            .map_err(|e| anyhow!("Could not identify storage device for {drive}: {e}"))?;
            sdn
        };

        // Locate the matching disk device instance via SetupAPI, then invoke PnP safe-remove
        // (`CM_Request_Device_EjectW`), which flushes/dismounts cleanly without Admin elevation.
        let dev_set = unsafe {
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_DISK),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(|e| anyhow!("Could not enumerate Windows disk devices for eject: {e}"))?;
        let _set_guard = DevSetGuard(dev_set);

        let mut matched_devinst: Option<u32> = None;
        let mut idx = 0u32;
        loop {
            let mut iface_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if unsafe {
                SetupDiEnumDeviceInterfaces(
                    dev_set,
                    None,
                    &GUID_DEVINTERFACE_DISK,
                    idx,
                    &mut iface_data,
                )
            }
            .is_err()
            {
                break;
            }
            idx += 1;

            let mut req_size = 0u32;
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    dev_set,
                    &iface_data,
                    None,
                    0,
                    Some(&mut req_size),
                    None,
                )
            };
            if req_size == 0 {
                continue;
            }
            let mut detail_words = vec![0u64; (req_size as usize).div_ceil(8)];
            let detail_ptr = detail_words
                .as_mut_ptr()
                .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            unsafe {
                (*detail_ptr).cbSize =
                    std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            }
            let mut dev_info = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(
                    dev_set,
                    &iface_data,
                    Some(detail_ptr),
                    req_size,
                    None,
                    Some(&mut dev_info),
                )
            }
            .is_err()
            {
                continue;
            }
            let dev_path_ptr =
                unsafe { std::ptr::addr_of!((*detail_ptr).DevicePath).cast::<u16>() };
            let Ok(disk_handle) = (unsafe {
                CreateFileW(
                    PCWSTR(dev_path_ptr),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
            }) else {
                continue;
            };
            let _disk_guard = HandleGuard(disk_handle);
            let mut disk_sdn = STORAGE_DEVICE_NUMBER {
                DeviceType: 0,
                DeviceNumber: 0,
                PartitionNumber: 0,
            };
            let mut ret = 0u32;
            if unsafe {
                DeviceIoControl(
                    disk_handle,
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    None,
                    0,
                    Some((&mut disk_sdn as *mut STORAGE_DEVICE_NUMBER).cast()),
                    std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                    Some(&mut ret),
                    None,
                )
            }
            .is_ok()
                && disk_sdn.DeviceType == vol_num.DeviceType
                && disk_sdn.DeviceNumber == vol_num.DeviceNumber
            {
                matched_devinst = Some(dev_info.DevInst);
                break;
            }
        }

        let dev_inst = matched_devinst
            .ok_or_else(|| anyhow!("Could not locate removable device node for {drive}."))?;

        let try_pnp_eject = |node: u32| -> Result<(), String> {
            let mut veto_type = PNP_VetoTypeUnknown;
            let mut veto_buf = [0u16; 256];
            let cr = unsafe {
                CM_Request_Device_EjectW(node, Some(&mut veto_type), Some(&mut veto_buf), 0)
            };
            if cr == CR_SUCCESS && veto_type == PNP_VetoTypeUnknown {
                return Ok(());
            }
            let end = veto_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(veto_buf.len());
            let veto_name = OsString::from_wide(&veto_buf[..end])
                .to_string_lossy()
                .trim()
                .to_string();
            if veto_name.is_empty() {
                Err(format!(
                    "PnP eject returned code {} (veto {})",
                    cr.0, veto_type.0
                ))
            } else {
                Err(format!("in use by {veto_name}"))
            }
        };

        // On USB readers/SSDs the ejectable PnP node is the parent USB Mass Storage device;
        // on built-in SD controllers it can be the disk node itself. Try parent first, then disk.
        let mut parent_inst = 0u32;
        if unsafe { CM_Get_Parent(&mut parent_inst, dev_inst, 0) } == CR_SUCCESS
            && try_pnp_eject(parent_inst).is_ok()
        {
            return Ok(());
        }
        try_pnp_eject(dev_inst)
            .map_err(|reason| anyhow!("Could not safely eject {drive}: {reason}"))
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
