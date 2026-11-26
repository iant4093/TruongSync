# TruongSync — Technical Architecture

> **Status:** Planning Draft  
> **Target Platforms:** macOS (Apple Silicon / Universal) & Windows (x86_64)

---

## 1. Technology Stack (Aligned with TruongPhoto)

Using the exact same core stack as **TruongPhoto** keeps builds, styling, tray behavior, and signed auto-updates 100% consistent across both apps:

| Layer | Choice | Rationale |
| --- | --- | --- |
| Desktop Runtime | **Tauri 2** | Small native binaries, native OS dialogs, tray popover, signed updater (`latest.json`), full Rust backend |
| Frontend | **React 19 + TypeScript + Vite 6 + Zustand** | Direct reuse of TruongPhoto's UI primitives, hooks, and Codex Dark theme |
| Icons & Maps | **Lucide React** + **MapLibre GL** | Identical iconography (`strokeWidth={1.6}`) and dark vector map preview for GPS tracks |
| Backend Language | **Rust** (workspace crates) | High-throughput parallel file I/O, safe concurrency, zero runtime dependencies |
| Local State DB | **SQLite (`rusqlite` / WAL mode)** | Persistent settings, presets, Drive file-ID cache, checksum manifest, and job history |
| EXIF GPS Writer | **Bundled ExifTool sidecar** (primary) + native EXIF reader (`nom-exif` / `kamadak-exif`) | Battle-tested safe in-place EXIF writing across `.CR3`, `.ARW`, `.NEF`, `.RAF`, `.DNG`, `.JPG`, `.HEIC` without corrupting MakerNotes or embedded RAW previews |
| Secret Storage | **OS Keychain (`keyring` crate)** | macOS Keychain & Windows Credential Manager for Google Drive OAuth refresh tokens |

---

## 2. Proposed Repository Layout

```text
TruongSync/
├── docs/                       # Planning docs & design system reference
├── legacy-scripts/             # (User's existing PowerShell scripts for parity reference)
├── crates/
│   ├── truongsync-db/          # SQLite schema, migrations, presets, sync manifest, job logs
│   ├── truongsync-ingest/      # Volume watcher, SD card scanner, folder templates, verified move/copy
│   ├── truongsync-geo/         # GPX/KML/JSON track parser, interpolation, clock-offset solver, ExifTool bridge
│   ├── truongsync-cloud/       # Google Drive v3 API client, OAuth2 PKCE, resumable chunked uploader
│   └── truongsync-backup/      # External volume UUID matcher, incremental diff & mirror/append sync
├── src-tauri/                  # Tauri 2 shell, IPC commands, event stream, tray, updater
│   └── resources/exiftool/     # Bundled ExifTool sidecar (macOS + Windows)
└── src/
    ├── styles/theme.css        # Codex Dark theme (ported from TruongPhoto)
    ├── components/             # Shared Truong UI primitives (Sidebar, WindowControls, Dropdown, Feedback, ui.tsx)
    ├── components/views/       # ImportView, GeotagView, CloudSyncView, BackupView, ActivityView, SettingsView
    └── store/                  # Zustand stores (jobs, volumes, presets, settings, feedback)
```

---

## 3. Data Safety & Integrity Architecture

TruongSync moves irreplaceable SD card originals and modifies RAW EXIF headers in place. Safety guarantees are built into the lowest layer:

### 3.1 Fast Multithreaded Two-Phase Verified Move (SD Card Ingest)
Never rely on `fs::rename` across volumes or naive copy+delete, while keeping transfers as fast as the SD reader and target SSD allow:
- **Bounded parallel worker pool** (configurable concurrency, default `4` workers for flash/NVMe targets, `2` for spinning HDDs, using large 1–4 MB streaming buffers) so multiple RAW/JPEG/MOV files transfer concurrently without disk thrashing.
- **Scoped media filter:** Only configured RAW, JPEG, and Video (`.MOV`/`.MP4`/etc.) files are moved and removed from the SD card; unrelated camera misc/system files are ignored.
- **Multi-shoot grouping:** The planner scans capture dates (EXIF `DateTimeOriginal` with `mtime` fallback) in parallel before moving, enabling users to split a multi-day card into multiple shoot targets in one job.
- **Pre-Flight Checks (Before Any Move Begins):**
  - **Destination Space Check:** Queries target filesystem free bytes (`statvfs` on macOS / `GetDiskFreeSpaceExW` on Windows) against total planned transfer size + configurable headroom buffer (e.g., 5% or 2 GB minimum).
  - **Card Health & Write-Protect Probe:** Tests volume readability, checks write permissions before attempting **Move** (catches locked physical SD tabs immediately), flags 0-byte/truncated or unreadable files during scan, and aborts deletion on any file that encounters read I/O retries/errors.
- **Per-file verified move steps:**
  1. Compute destination path from template and verify free space on target volume before starting.
  2. Stream source bytes into `<dest>.truongsync.part` (computing incremental `xxHash3` only when deep checksum verification is enabled).
  3. `sync_all()` (`fsync`) the `.part` file and verify byte size matches source (fast default; optional read-back hash check in Settings).
  4. Atomically rename `<dest>.truongsync.part` → `<dest>` and preserve original file timestamps (`mtime`).
  5. **Only after step 4 succeeds** does TruongSync unlink the file from the SD card.
  6. If any step fails or is cancelled: `.part` is cleaned up, the SD card copy remains untouched, and the file is flagged in the job report.

> **TruongPhoto Compatibility:** Because files appear at their final destination atomically via rename (never written directly with the final extension), TruongPhoto's folder watcher will never index a half-copied file or lock a file mid-transfer.

### 3.2 Safe EXIF GPS Writing (Photos Only)
1. **Strict media filter:** Only files matching configured photo extensions (RAW + JPEG/HEIC/TIFF) can enter the EXIF writer queue. Video extensions (`.MP4`, `.MOV`, `.CRM`, `.MXF`, etc.) are hard-blocked at both the planner and writer layers.
2. **Why ExifTool for writing:** Proprietary RAW containers — especially Canon **`.CR3`** (ISO BMFF/CRX boxes), Sony `.ARW`, Nikon `.NEF`, and Fujifilm `.RAF` — have delicate MakerNotes, preview offsets, and IFD structures. ExifTool is the industry standard used by Lightroom/Photo Mechanic/GeoSetter and performs atomic temp-file writes (`file.CR3_exiftool_tmp` → rename) while preserving all MakerNotes, `SubSecTimeOriginal`, and embedded previews.
3. **Batch execution (`-stay_open True`):** TruongSync spawns a single persistent ExifTool worker process per job and streams commands via stdin/stdout, tagging hundreds of RAW + JPEG files in seconds without per-file process spawn overhead.
4. **Metadata preservation:** Pass `-P` (preserve file modification date/time) unless the user prefers updating `mtime` so TruongPhoto detects the EXIF change automatically (see Question #4 in `05-MIGRATION-AND-QUESTIONS.md` — updating `mtime` or nudging TruongPhoto ensures TruongPhoto re-reads the new GPS coordinates!).
5. **Non-destructive default:** Skip photos that already have valid GPS tags unless "Overwrite existing GPS" is explicitly checked.

### 3.3 Google Drive 2-Way Additive Sync & Verification Engine
- **Recommended Engine: Bundled Cross-Platform `rclone` Sidecar (with Native Rust Orchestration):**
  - Auditing `Sync-Raw-Photos.ps1`, `Audit-PhotoCopies.ps1`, and `Resume-PhotoContentAudit.ps1` confirms the existing library already runs on `rclone` (`rclone copy --immutable`, `rclone lsjson --hash-type MD5`, `rclone check`).
  - Bundling the official `rclone` binary inside `src-tauri/resources/rclone/` (macOS arm64 + Windows x64) and driving it via structured JSON logs (`--use-json-log --stats 1s`) gives:
    1. **100% seamless migration** with existing `My Drive/Raw Photo Backup` and optional one-click import of an existing `rclone.conf` (plus built-in OAuth 2.0 browser sign-in for new setups).
    2. **Battle-tested Google Drive quirks handled natively:** `--immutable`, `--drive-skip-gdocs`, `--drive-stop-on-upload-limit`, resumable chunked transfers, `--bwlimit` bandwidth caps, MD5 verification, and future multi-cloud targets (S3 / Backblaze B2 / OneDrive) for free.
    3. **Strict Mac Metadata Exclusions:** Always passes `--exclude "._*" --exclude ".DS_Store" --exclude "__MACOSX/**"` while retaining `.xmp` sidecars.
- **2-Way Union Merge (Zero Deletes, Two-Pass Order):**
  - Matches `Sync-Raw-Photos.ps1`'s proven order: **Pass 1 (`Drive → Local`)** then **Pass 2 (`Local → Drive`)**, using `--immutable` (never deletes, never silently overwrites mismatched files).
  - **Tombstone-Aware Download Filter:** Before Pass 1 (`Drive → Local`), TruongSync compares the remote listing (`rclone lsjson --recursive --files-only --hash-type MD5`) against SQLite `sync_manifest`:
    - Brand-new files on Drive (never seen in `sync_manifest`) → **Download** to local via atomic `.part` staging.
    - Previously synced files intentionally deleted locally (present in `sync_manifest` as `synced` / `local_tombstone`) → **Skip** during normal sync so culled RAWs never bounce back onto the PC.
    - **Restore / Hydrate Mode** sets `ignore_tombstones = true` to intentionally pull missing local files back.
  - **Intentional GPS-Backfill Updates (Avoiding `--immutable` False Conflicts):**
    - Whenever TruongSync's Geotagger modifies an already-synced photo in place, it records `(relative_path, pre_exif_md5, post_exif_md5)` in SQLite `intentional_updates`.
    - Before the `--immutable` pass, TruongSync verifies that the remote Drive file's current MD5 still equals `pre_exif_md5` (confirming no external edit occurred on Drive), safely updates **only** those explicitly geotagged files, and updates `sync_manifest` to `post_exif_md5`. Every unknown mismatch remains strictly protected as an immutable conflict!
  - **Read-Only Verify Mode (`Audit-PhotoCopies` / `Resume-PhotoContentAudit` Parity):**
    - Runs `rclone lsjson --hash-type MD5` (fast size + Drive MD5 audit) and optional full `rclone check` checksum audit, explicitly detecting:
      - Missing on Drive (`↑`)
      - Missing on Local (`↓`)
      - Size / MD5 Mismatches
      - **Duplicate Google Drive paths** (same path appearing twice in Drive's ID-based filesystem)
      - Separated count of ignored macOS metadata (`._*`, `.DS_Store`, `__MACOSX`)
    - **Sequential Mechanical-Disk Guard:** When running a 3-way audit (`PC ↔ External` + `PC ↔ Drive`), runs the two local-disk passes **sequentially** (matching `Resume-PhotoContentAudit.ps1`) to avoid thrashing mechanical HDDs.
  - **Single-Instance Coalesced Follow-Up Queue:** If a second sync trigger arrives (e.g., SD Import completes while a Scheduled/Startup sync is in flight), the job runner coalesces it into one clean follow-up pass upon completion (matching `sync.lock` + `sync-requested`).

### 3.4 External Drive 2-Way Additive Sync & Verification Engine
- **Stable Volume Identification:**
  - **macOS:** Query `diskutil` / `DADiskCopyDescription` for `VolumeUUID` + mount path + filesystem type (APFS / exFAT / HFS+).
  - **Windows:** Query `GetVolumeNameForVolumeMountPointW` (GUID volume path `\\?\Volume{...}\`) + volume label + drive letter.
- **2-Way Additive Diff & Verify:**
  - Computes push (`↑ Local → External`), pull (`↓ External → Local`), and mismatch sets using `(relative_path, file_size, mtime_tolerance_2s)` for fast scans, with an optional **Deep Verify (`xxHash3`)** mode directly on the Drive Backup screen.
  - Both pushes and pulls use `.truongsync.part` → `fsync` → atomic rename so neither the external drive nor TruongPhoto's local folders ever see partial files.
  - **Never deletes** files from either the PC or the external drive.
- **OS-Agnostic Sync State & Freshness Tracking:**
  - All relative paths in SQLite (`sync_manifest`) are normalized with forward slashes (`/`) and NFC Unicode normalization so the same Google Drive tree and external backup drives work seamlessly across **both macOS and Windows**.
  - `backup_targets` table records `last_sync_at`, `last_verify_at`, `last_status`, `pending_push_count`, `pending_pull_count`, and `stale_after_days` to power the **Backup Freshness** indicators in the UI and tray.

### 3.5 Background Scheduler
- Lightweight Tokio timer/scheduler running inside the tray backend:
  - Evaluates user-defined schedules per profile (e.g., `Manual Only`, `Every N Hours`, `Daily at HH:MM`, or `On Idle / Drive Connect`).
  - Supports **"Check Only (Notify if behind)"** vs **"Auto-Sync (2-Way Additive)"** per profile.
  - Skips or defers when offline, on battery (optional), or while an active SD import is already running.

---

## 4. Pipeline & Event Architecture

```text
User / Auto-Trigger
        │
        ▼
  Job Planner (Dry-Run Manifest)
        │  - Shows file counts, GB, folder destinations, GPS matches, sync diffs
        ▼
  Job Runner (Rust tokio tasks + bounded worker pools)
        │
        ├── Stage 1: SD Import (Verified Move / Copy)
        ├── Stage 2: EXIF GPS Tagging (Photos only: RAW + JPEG)
        ├── Stage 3: Google Drive Sync (Shoot folder or full library)
        └── Stage 4: External Drive Backup (If target volume mounted)
        │
        ▼
  Tauri Event Stream ──► React UI (Live progress bars, speed, ETA, per-file log)
        │
        ▼
  SQLite Job History (`jobs`, `job_items`, `sync_manifest`)
```

- **Pause / Cancel / Resume:** Every stage checks a cooperative `CancellationToken` between chunks/files so pausing or cancelling leaves zero corrupt files and allows resuming from the exact next file.
- **Watched GPX Folder Indexing:** Lightweight header/time-bounds scan of `.gpx` files inside the user's configured GPX folder (`min_time` / `max_time` cached by file `mtime`) so overlapping tracks surface instantly when an SD card or shoot folder is opened.
- **Sync Tombstones & Restore Mode:**
  - `sync_manifest` tracks every synced `(profile_id, relative_path)` so normal 2-way additive sync skips re-downloading files that were previously synced and later intentionally deleted locally, while still downloading newly added remote files.
  - **Restore / Hydrate Mode** runs the diff with `ignore_tombstones = true` (scoped to a folder or full profile) to pull missing local files back from Google Drive or an external drive cleanly.
- **Token-Bucket Bandwidth Limiter:** Optional configurable bytes/sec cap shared across concurrent Google Drive transfer workers so cloud sync stays polite during normal PC use.
- **Power & Sleep Handling:** Hold OS sleep-prevention assertion (macOS `IOPMAssertion` / Windows `SetThreadExecutionState`) while an active SD move, backup, or large upload is running (toggleable in Settings), and inspect OS AC/battery power state before running scheduled background tasks according to the user's **Sync on Battery** setting.

---

## 5. Cross-Platform & Release Parity with TruongPhoto

- **Window Chrome:**
  - macOS: native overlay titlebar (`titleBarStyle: "Overlay"`, hidden title, native traffic lights).
  - Windows: frameless window with TruongPhoto's custom `46x32` Windows 11 `.caption-buttons` and rounded frame border when unmaximized.
- **System Tray & Auto-Open on SD Card Insert:**
  - **Primary (Tray Resident):** Menu bar / system tray icon showing active sync progress, quick "Sync Google Drive Now", detected SD cards / external drives, and Show/Quit. Closing the window hides to tray (with optional WebView teardown while idle to keep background memory minimal).
  - **Native Mount Listeners (Zero Polling):**
    - **macOS:** `DiskArbitration` (`DARegisterDiskAppearedCallback`) / `NSWorkspace.didMountNotification` inspecting removable volumes for `DCIM/` (or known backup `VolumeUUID`).
    - **Windows:** Hidden Win32 message window listening to `WM_DEVICECHANGE` (`DBT_DEVICEARRIVAL` / `DBT_DEVTYP_VOLUME`), filtering for removable media with `DCIM/` (or known backup volume GUID).
  - **One-Click OS-Native Launch-When-Quit Toggle (Settings):**
    - **macOS:** Writes/removes `~/Library/LaunchAgents/com.iantruong.truongsync.mount.plist` (`WatchPaths: ["/Volumes"]`) invoking `truongsync --check-mounts` (debounced against already-seen mounted volumes so unmounts/DMGs exit in <10 ms without launching the GUI).
    - **Windows:** Registers/unregisters per-user `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\AutoplayHandlers` entries (`ShowPicturesOnArrival` / `ImportVideoOnArrival` / `MixedContentOnArrival`) invoking `truongsync --import-volume "%1"` with zero admin privileges required.
- **CI/CD & Updates:** Same GitHub Actions matrix (`macos-latest` Apple Silicon + `windows-latest`), Tauri updater signing keypair, and `latest.json` manifest for one-click in-app updates.
