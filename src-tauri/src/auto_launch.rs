//! One-click OS-native auto-launch even when TruongSync is completely quit:
//! - macOS: user `launchd` LaunchAgent watching `/Volumes` (`--check-mounts`), launched via
//!   `launchctl bootstrap gui/<uid>`, delegating to `open -a <App.app> --args --import-volume <mount>`
//!   so the helper exits immediately (no warm-handoff double-consumption, no self-kill on disable).
//! - Windows: direct Win32 HKCU Registry AutoPlay handler with `%1.` quoting safety (zero PowerShell).

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "com.iantruong.truongsync.mount";

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Normalizes `--import-volume` arguments coming from Windows AutoPlay (`E:\.` or stray trailing `"`)
/// or macOS `open --args`.
pub fn normalize_import_volume_arg(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"').trim();
    if let Some(prefix) = trimmed.strip_suffix(r"\\.") {
        if prefix.len() == 2 && prefix.as_bytes()[1] == b':' {
            return format!(r"{prefix}\");
        }
        return prefix.to_string();
    }
    if let Some(prefix) = trimmed.strip_suffix(r"\.") {
        if prefix.len() == 2 && prefix.as_bytes()[1] == b':' {
            return format!(r"{prefix}\");
        }
        return prefix.to_string();
    }
    if let Some(prefix) = trimmed.strip_suffix("/.") {
        if prefix.is_empty() {
            return "/".into();
        }
        return prefix.to_string();
    }
    // Handle bare Windows drive with trailing dot from legacy `%1.` (`E:.` -> `E:\`)
    if let Some(prefix) = trimmed.strip_suffix('.') {
        let b = prefix.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return format!(r"{prefix}\");
        }
    }
    trimmed.to_string()
}

fn seen_mounts_file() -> PathBuf {
    let paths = truongsync_db::models::StoragePaths::from_data_dir(
        truongsync_db::models::StoragePaths::default_data_dir(),
    );
    let _ = paths.ensure_dirs();
    paths.data_dir.join("launch_agent_seen_mounts.json")
}

fn snapshot_current_dcim_keys() -> HashSet<String> {
    truongsync_ingest::volumes::list_mounted_volumes()
        .into_iter()
        .filter(|v| v.has_dcim)
        .map(|v| format!("{}|{}", v.mount_path, v.volume_uuid))
        .collect()
}

pub fn set_os_native_auto_launch(enabled: bool) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = launch_agent_path()?;
        let uid = unsafe { libc::geteuid() };
        let gui_domain = format!("gui/{uid}");
        let service_target = format!("{gui_domain}/{LAUNCH_AGENT_LABEL}");

        if enabled {
            if let Some(parent) = plist_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Seed currently-mounted DCIM cards so already-inserted cards don't false-trigger on next mount event
            let seen_file = seen_mounts_file();
            let current_keys = snapshot_current_dcim_keys();
            let _ = std::fs::write(
                &seen_file,
                serde_json::to_string(&current_keys).unwrap_or_default(),
            );

            let exe =
                std::env::current_exe().context("failed to resolve TruongSync executable path")?;
            let exe_escaped = xml_escape(&exe.to_string_lossy());
            let plist_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_escaped}</string>
        <string>--check-mounts</string>
    </array>
    <key>WatchPaths</key>
    <array>
        <string>/Volumes</string>
    </array>
    <key>ThrottleInterval</key>
    <integer>2</integer>
    <key>RunAtLoad</key>
    <false/>
    <key>AbandonProcessGroup</key>
    <true/>
</dict>
</plist>
"#
            );
            std::fs::write(&plist_path, plist_xml)?;
            // Bootout any prior registration before bootstrapping cleanly
            let _ = std::process::Command::new("/bin/launchctl")
                .args(["bootout", &service_target])
                .output();
            let boot_out = std::process::Command::new("/bin/launchctl")
                .args(["bootstrap", &gui_domain])
                .arg(&plist_path)
                .output()
                .context("failed to invoke launchctl bootstrap")?;
            if !boot_out.status.success() {
                let stderr = String::from_utf8_lossy(&boot_out.stderr).trim().to_string();
                anyhow::bail!("macOS LaunchAgent failed to load: {stderr}");
            }
            Ok(format!(
                "Installed macOS LaunchAgent at {}",
                plist_path.display()
            ))
        } else {
            let _ = std::process::Command::new("/bin/launchctl")
                .args(["bootout", &service_target])
                .output();
            if plist_path.exists() {
                std::fs::remove_file(&plist_path)?;
            }
            Ok("Removed macOS LaunchAgent mount trigger.".into())
        }
    }

    #[cfg(target_os = "windows")]
    {
        set_windows_autoplay_registry(enabled)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = enabled;
        Ok("Not applicable on this OS.".into())
    }
}

#[cfg(target_os = "windows")]
fn set_windows_autoplay_registry(enabled: bool) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn set_reg_sz(subkey: &str, name: Option<&str>, value: &str) -> Result<()> {
        let sk = to_wide(subkey);
        let mut hkey = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sk.as_ptr()),
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hkey,
                None,
            )
            .ok()
            .with_context(|| format!("RegCreateKeyExW failed for {subkey}"))?;
            let val_w = to_wide(value);
            let bytes: &[u8] =
                std::slice::from_raw_parts(val_w.as_ptr() as *const u8, val_w.len() * 2);
            let name_w = name.map(to_wide);
            let name_ptr = name_w
                .as_ref()
                .map(|v| PCWSTR(v.as_ptr()))
                .unwrap_or(PCWSTR::null());
            let res = RegSetValueExW(hkey, name_ptr, Some(0), REG_SZ, Some(bytes)).ok();
            let _ = RegCloseKey(hkey);
            res.with_context(|| format!("RegSetValueExW failed for {subkey}"))?;
        }
        Ok(())
    }

    let base = r"Software\Microsoft\Windows\CurrentVersion\Explorer\AutoplayHandlers";
    let handler_key = format!(r"{base}\Handlers\TruongSyncImportHandler");
    let prog_key = r"Software\Classes\TruongSync.AutoPlay";
    let events = [
        "ShowPicturesOnArrival",
        "ImportVideoOnArrival",
        "MixedContentOnArrival",
    ];

    if enabled {
        let exe =
            std::env::current_exe().context("failed to resolve TruongSync executable path")?;
        let exe_str = exe.to_string_lossy().to_string();

        set_reg_sz(&handler_key, Some("Action"), "Import photos and videos")?;
        set_reg_sz(&handler_key, Some("Provider"), "TruongSync")?;
        set_reg_sz(&handler_key, Some("InvokeProgID"), "TruongSync.AutoPlay")?;
        set_reg_sz(&handler_key, Some("InvokeVerb"), "open")?;
        set_reg_sz(
            &handler_key,
            Some("DefaultIcon"),
            &format!("\"{exe_str}\",0"),
        )?;

        for evt in events {
            let ev_key = format!(r"{base}\EventHandlers\{evt}");
            set_reg_sz(&ev_key, Some("TruongSyncImportHandler"), "")?;
        }

        // `%1\\.` protects both `E:\` (becomes `E:\\.`) and `E:` / `E:\DCIM` from quote escaping under CommandLineToArgvW
        set_reg_sz(
            &format!(r"{prog_key}\shell\open\command"),
            None,
            &format!("\"{exe_str}\" --import-volume \"%1\\\\.\""),
        )?;

        Ok("Registered Windows AutoPlay handler (HKCU). Select TruongSync once in Windows AutoPlay settings for memory cards.".into())
    } else {
        unsafe {
            let hk = to_wide(&handler_key);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(hk.as_ptr()));
            let pk = to_wide(prog_key);
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(pk.as_ptr()));
            let val_name = to_wide("TruongSyncImportHandler");
            for evt in events {
                let ev_key = to_wide(&format!(r"{base}\EventHandlers\{evt}"));
                let mut h = HKEY::default();
                if RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(ev_key.as_ptr()),
                    Some(0),
                    KEY_SET_VALUE,
                    &mut h,
                )
                .is_ok()
                {
                    let _ = RegDeleteValueW(h, PCWSTR(val_name.as_ptr()));
                    let _ = RegCloseKey(h);
                }
            }
        }
        Ok("Removed Windows AutoPlay handler.".into())
    }
}

/// Fast headless check invoked by macOS LaunchAgent (`truongsync --check-mounts`).
/// Respects `settings.auto_open_on_sd_insert`, tracks `(mount_path, volume_uuid)` pairs, and
/// returns `Some(mount_path)` if a brand-new camera card arrived.
pub fn check_new_sd_card_for_launch_agent() -> Option<String> {
    let paths = truongsync_db::models::StoragePaths::from_data_dir(
        truongsync_db::models::StoragePaths::default_data_dir(),
    );
    let _ = paths.ensure_dirs();
    // Open SQLite strictly read-only so `--check-mounts` NEVER mutates or marks active GUI jobs interrupted!
    if let Ok(db) = truongsync_db::DbHandle::open_readonly(&paths.db_path) {
        if let Ok(settings) = db.load_settings() {
            if !settings.auto_open_on_sd_insert {
                return None;
            }
        }
    }

    let seen_file = seen_mounts_file();
    let prev: HashSet<String> = std::fs::read_to_string(&seen_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let vols = truongsync_ingest::volumes::list_mounted_volumes();
    let current_dcim: Vec<(String, String)> = vols
        .into_iter()
        .filter(|v| v.has_dcim)
        .map(|v| (format!("{}|{}", v.mount_path, v.volume_uuid), v.mount_path))
        .collect();
    let current_set: HashSet<String> = current_dcim.iter().map(|(k, _)| k.clone()).collect();
    let _ = std::fs::write(
        &seen_file,
        serde_json::to_string(&current_set).unwrap_or_default(),
    );

    current_dcim
        .into_iter()
        .find(|(k, _)| !prev.contains(k))
        .map(|(_, mount_path)| mount_path)
}

/// Detaches and launches the main TruongSync GUI app for `mount_path` so the LaunchAgent
/// helper process can exit immediately (preventing warm-handoff double-consumption and
/// preventing `launchctl bootout` from killing a running GUI instance).
pub fn launch_gui_for_volume(mount_path: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    #[cfg(target_os = "macos")]
    {
        // Inside `.../TruongSync.app/Contents/MacOS/truongsync-desktop`, prefer `/usr/bin/open -a <App.app>`
        if let Some(app_bundle) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .filter(|p| p.extension().is_some_and(|e| e == "app"))
        {
            // Wait for `/usr/bin/open` to finish handing off to LaunchServices before helper exits
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("-a")
                .arg(app_bundle)
                .args(["--args", "--import-volume", mount_path])
                .status();
            return;
        }
    }
    let _ = std::process::Command::new(exe)
        .args(["--import-volume", mount_path])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_special_characters_and_normalizes_volume_arg() {
        assert_eq!(
            xml_escape("/Applications/Photo & Video <Test>/TruongSync.app"),
            "/Applications/Photo &amp; Video &lt;Test&gt;/TruongSync.app"
        );
        assert_eq!(normalize_import_volume_arg(r"E:\."), r"E:\");
        assert_eq!(normalize_import_volume_arg(r#""E:\""#), r"E:\");
        assert_eq!(
            normalize_import_volume_arg("/Volumes/EOS_DIGITAL"),
            "/Volumes/EOS_DIGITAL"
        );
    }
}
