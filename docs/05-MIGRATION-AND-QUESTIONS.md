# TruongSync — PowerShell Script Audit & Finalized Migration Parity

> Existing scripts reviewed from `TruongSync/Scripts/` on 2026-11-24.
>
> **Security Hygiene Note (Before `git init` / Push):** `Scripts/` currently contains live credentials (`Raw Photo Backup/client_secret_*.json`, `Raw Photo Backup/config/rclone.conf`), local logs (`logs/`), and a 78 MB Windows binary (`tools/rclone.exe`). Ensure `Scripts/` is excluded via `.gitignore` (or moved out of the repo) before committing TruongSync to GitHub.

---

## 1. Audited Script Parity Map

| Existing Script | Replaced By in TruongSync | Exact Extracted Rules & Parity Requirements |
| --- | --- | --- |
| `Smart-Import.ps1` | **Import** tab + default preset | • Auto-detects camera cards (`DCIM/*`, e.g. Canon `100EOSR7`) instead of hardcoded `H:\DCIM\100EOSR7`<br>• Year folder (`yyyy`) auto-bootstraps default categories (`Adventures`, `Hikes`, `Portraits`, `Random`, `Sports`, `Wildlife`) & dynamically reads existing folders + inline **New Category**<br>• Shoot folder format: `{ShootName} {Date}` with `M-dd` default (e.g., `Cone Peak 4-15`)<br>• Three type subfolders: **`RAW`**, **`JPEG`**, **`VID`**<br>• Multithreaded move (`/MT:8` parity) of RAW + JPEG + Video only (`*.CR3`, `*.JPG`, `*.MP4` + universal configurable extensions)<br>• Skips `._*`, `.DS_Store`, `__MACOSX`; keeps `.xmp`<br>• Triggers Google Drive sync **only** when all transfers succeed; failed files remain safely on SD card<br>• **New upgrades:** multi-day/multi-shoot splitting, pre-flight disk & card health checks, optional inline `.gpx` geotagging |
| `Sync-Raw-Photos.ps1` + `RawPhotoSync.config.psd1` | **Cloud Sync** tab + background scheduler | • Uses **`rclone copy --immutable`** in **2 passes**: Pass 1 `Drive → Local`, Pass 2 `Local → Drive`<br>• Default profile: `All Photos/Photo Dump` ↔ `My Drive/Raw Photo Backup` (supports multiple profiles so `Edited Photos` or full `All Photos` can be added too)<br>• Flags: `--immutable`, `--create-empty-src-dirs`, `--drive-skip-gdocs`, `--drive-stop-on-upload-limit`, `--transfers 4`, `--checkers 8`, retries + backoff<br>• Excludes `._*`, `.DS_Store`, `__MACOSX/**` in both directions; keeps `.xmp`<br>• Single-instance lock + coalesced follow-up pass if another trigger arrives mid-sync<br>• **New upgrades:** cross-platform bundled `rclone` (macOS + Windows), native UI progress/freshness, tombstone-aware 2-way additive sync + Restore mode, smart GPS-backfill propagation, bandwidth cap, user-defined schedule |
| `Backup-Photos.ps1` | **Drive Backup** tab | • 2-way strictly-additive sync across full library (`All Photos` ↔ `Photo Library Backup/All Photos`)<br>• Pass 1 `External → PC` first (pulls travel additions), Pass 2 `PC → External` second; never deletes, never overwrites newer files<br>• Multithreaded (`/MT:8` parity), skips `._*`, `.DS_Store`, `__MACOSX/**`, preserves `.xmp` & timestamps<br>• Writes `Last Sync - yyyy-MM-dd HH-mm-ss.txt` at backup root (kept as an optional toggle for Finder/Explorer visibility)<br>• **New upgrades:** Volume UUID matching (no broken drive letters), freshness badge, on-screen `Verify` + `Restore`, macOS + Windows parity |
| `Audit-PhotoCopies.ps1` + `Resume-PhotoContentAudit.ps1` | **Verify** action on **Cloud Sync** & **Drive Backup** (+ 3-way audit) | • Fast inventory comparison (size + Drive MD5) & resumable deep content checksum (`rclone check` / `xxHash3`)<br>• Explicitly detects **duplicate Google Drive paths** and separates macOS junk metadata counts from real photo differences<br>• Runs local mechanical-drive checks **sequentially** to avoid HDD contention |
| `Install-PhotoToolsMenu.ps1` / `Install-RawPhotoSync.ps1` / `Start-RawPhotoSyncHidden.vbs` / `*.reg` | Native Tray + Login Autostart + Auto-Open on SD Insert | • Replaced cleanly by TruongSync's cross-platform menu bar / system tray, launch-at-login, native mount listeners (`DiskArbitration` / `WM_DEVICECHANGE`), and optional OS-native auto-launch toggle |
| `Clean-MacPhotoMetadata.ps1` / `Compare-WebsitePhotoCopies.ps1` | One-off historical utilities (out of core scope) | • Ongoing prevention is built in via universal `._*` / `.DS_Store` / `__MACOSX` exclusions across all engines |

---

## 2. Locked Design Decisions

| Topic | Decision |
| --- | --- |
| **GPS Track Format** | Universal `.gpx` as first-class primary format |
| **Geotagging Timing** | Optional step inline during SD Import (runs *before* Google Drive upload) **+** standalone Geotag tab to backfill existing shoots |
| **Geotag Media Scope** | Strictly photos only (RAW + JPEG); videos are never modified |
| **Out-of-Range Photos** | Leave untagged if outside the GPX track time range (no forced snapping) |
| **Sync Direction & Delete Policy** | **2-Way Strictly Additive** (`rclone copy --immutable` + two-pass external sync) — uploads/pushes local changes AND downloads/pulls new files added on Drive or external drives, while **never deleting** on either side (with tombstone protection + intentional GPS-backfill update tracking) |
| **Cloud Sync Scope** | Configurable sync profiles (supports `Photo Dump` ↔ `My Drive/Raw Photo Backup` out of the box plus optional `Edited Photos` / full library profiles) |
| **Backup Freshness & Verify** | Freshness indicators (last sync age + stale warnings + `↑`/`↓` delta) and one-click **`Verify`** audit directly on both **Cloud Sync** and **Drive Backup** screens (including duplicate Drive path detection) |
| **Scheduled Background Tasks** | User-configurable schedule (startup, interval, daily time, or idle/connect) running quietly in the tray to check or auto-sync Google Drive and connected external drives |
| **Pre-Flight Checks** | Automatic destination disk space check + SD card health / write-protect / readability check before starting any import |
| **Restore / Hydrate** | Explicit one-click **Restore** mode on Cloud Sync & Drive Backup (bypasses local delete tombstones for new PCs or recovering accidentally deleted files) |
| **Watched GPX Folder** | Optional watched `.gpx` folder that auto-detects tracks overlapping an inserted SD card / shoot and offers 1-click attach |
| **Bandwidth & Power Controls** | Optional Google Drive bandwidth cap, sleep prevention during active transfers, and configurable **Sync on Battery** toggle for scheduled tasks |
| **Multi-Day SD Cards** | Support splitting one SD card across multiple shoot folders (by date or selection) |
| **SD Move Scope & Speed** | Multithreaded verified move (RAW, JPEG, Video only — excludes `._*`, `.DS_Store`, `__MACOSX`; deletes from SD only after confirmed write); default subfolders `RAW`, `JPEG`, `VID` |
| **Platform Support** | Universal **macOS + Windows** (OS-agnostic paths, Volume UUIDs, native window chrome, signed updates) |
| **Auto-Open on SD Insert** | Default lightweight **Menu Bar / System Tray** mode (native mount events, smart `DCIM/` vs backup-drive routing, minimal idle footprint) **+** one-click Settings toggle for **OS-native launch when quit** (macOS `launchd` `WatchPaths` / Windows `HKCU` AutoPlay handler) |

---

## 3. Two Quick Final Confirmations Before Building

1. **Google Drive Sync Scope (`Photo Dump` vs full `All Photos`):** Your current `RawPhotoSync.config.psd1` syncs `F:\All Photos\Photo Dump` ↔ `My Drive\Raw Photo Backup` (while `Backup-Photos.ps1` syncs all of `F:\All Photos` including `Edited Photos` to the external SSD). With multi-profile support in TruongSync, should your default Google Drive setup keep `Photo Dump` ↔ `My Drive/Raw Photo Backup` as Profile 1 and let you add `Edited Photos` (or full `All Photos`) as a second profile?
2. **GPS Backfill on Already-Synced Photos:** When you backfill GPS into an older photo that is *already* backed up on Google Drive under `--immutable`, should TruongSync automatically update that file's Drive/External backup once it verifies the backup copy still matches the pre-geotag MD5 checksum (safe auto-update for TruongSync's own GPS edits, while keeping strict conflict protection for everything else)?

