//! Cross-platform battery detection and thread-safe RAII sleep prevention during active transfers.

#[cfg(target_os = "macos")]
use std::process::{Child, Command, Stdio};

pub fn is_on_battery() -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/pmset")
            .args(["-g", "batt"])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .to_ascii_lowercase()
                    .contains("'battery power'")
            })
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut status) }.is_ok() {
            return status.ACLineStatus == 0;
        }
        false
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Keeps the system awake while active SD moves, backups, or uploads are running.
/// Automatically releases the OS assertion on drop, safely across any thread.
pub struct SleepGuard {
    #[cfg(target_os = "macos")]
    child: Option<Child>,
    #[cfg(target_os = "windows")]
    handle: Option<isize>,
}

impl SleepGuard {
    pub fn acquire(enabled: bool) -> Self {
        if !enabled {
            return Self::inactive();
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id().to_string();
            let child = Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            Self { child }
        }
        #[cfg(target_os = "windows")]
        {
            use windows::core::PWSTR;
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Power::{
                PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
            };
            use windows::Win32::System::Threading::{
                POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
            };
            // Handle-based PowerCreateRequest is thread-agnostic & refcounted (safe across Tokio threads).
            let mut reason_w: Vec<u16> = "TruongSync active transfer"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let ctx = REASON_CONTEXT {
                Version: 0, // POWER_REQUEST_CONTEXT_VERSION
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: PWSTR(reason_w.as_mut_ptr()),
                },
            };
            let handle = unsafe { PowerCreateRequest(&ctx) }.ok().and_then(|h| {
                if unsafe { PowerSetRequest(h, PowerRequestSystemRequired) }.is_ok() {
                    Some(h.0 as isize)
                } else {
                    let _ = unsafe { CloseHandle(h) };
                    None
                }
            });
            Self { handle }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self {}
        }
    }

    fn inactive() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self { child: None }
        }
        #[cfg(target_os = "windows")]
        {
            Self { handle: None }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self {}
        }
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(target_os = "windows")]
        if let Some(raw) = self.handle.take() {
            use windows::Win32::Foundation::{CloseHandle, HANDLE};
            use windows::Win32::System::Power::{PowerClearRequest, PowerRequestSystemRequired};
            let h = HANDLE(raw as *mut core::ffi::c_void);
            unsafe {
                let _ = PowerClearRequest(h, PowerRequestSystemRequired);
                let _ = CloseHandle(h);
            }
        }
    }
}
