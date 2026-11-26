# TruongSync — Product Plan

> **Status:** Planning Draft (awaiting existing PowerShell scripts & workflow answers before implementation)  
> **Companion App:** [TruongPhoto](../TruongPhoto) (read-only photo/video library & server)

---

## 1. Vision & Separation of Concerns

**TruongSync** is a cross-platform desktop utility (macOS + Windows) that owns the **ingest, geotagging, cloud sync, and external backup lifecycle** for photography and video shoots.

Keeping TruongSync separate from TruongPhoto enforces a clean boundary:

| Responsibility | TruongSync | TruongPhoto |
| --- | --- | --- |
| Detect SD cards & ingest media | **Owns** | — |
| Sort into folder structure (`RAW`, `JPEG`, `Video`, etc.) | **Owns** | Watches & indexes resulting folders |
| Embed GPS coordinates into photo EXIF (RAW + JPEG) | **Owns** | Reads EXIF GPS for Map view & search |
| Sync library / shoots to Google Drive | **Owns** | — |
| Back up library to external hard drives / SSDs | **Owns** | — |
| Browse timeline, stack edits, AI search, lightbox, LAN sharing | — | **Owns** |

TruongSync replaces a collection of platform-specific PowerShell scripts with a single reliable, resumable, universal desktop app that works identically on **macOS and Windows**, while matching **TruongPhoto's Codex Dark UI** pixel-for-pixel.

---

## 2. Core Workflows

### 2.1 SD Card Import & Universal Folder Sorter
- **Auto-detect removable media** (SD/CFexpress cards, USB card readers, or manual folder selection on macOS & Windows).
- **Universal folder template system** (no hardcoded layout):
  - Default preset matching Ian's `Smart-Import.ps1` workflow:
    `{Root}/{Year}/{Category}/{ShootName} {Date}/{TypeFolder}/`
    - `{Year}` (`yyyy`): automatically initializes default category folders (`Adventures`, `Hikes`, `Portraits`, `Random`, `Sports`, `Wildlife`, fully editable) when a new year begins, and dynamically lists existing categories + inline **"Create New Category"**.
    - `{Date}`: defaults to **`M-dd`** (e.g., `Cone Peak 4-15`), customizable per preset (e.g., `yyyy-MM-dd`).
    - `{TypeFolder}`: defaults to **`RAW`**, **`JPEG`**, **`VID`** (all folder names & nesting fully customizable per preset).
  - Support alternative layouts for other users (flat date folders, `YYYY/MM/DD`, camera-model folders, single folder without type subfolders, etc.).
  - Live **destination path preview** before running.
- **Multi-Day / Multi-Shoot Card Splitting:**
  - Automatically detects capture dates on the SD card and lets the user **split a single card across multiple shoot folders** in one session (by date group or custom selection), directly solving the single-folder limitation of `Smart-Import.ps1`.
  - Default single-day flow stays 1-click fast when all files belong to one shoot.
- **File classification & Mac Metadata Exclusions:**
  - Configurable by extension; **always excludes macOS AppleDouble / Finder metadata** (`._*`, `.DS_Store`, `__MACOSX/**`) across Import, Cloud Sync, and External Backup while **preserving real editing sidecars (`.xmp`)**.
  - **RAW photos:** `.CR3`, `.CR2`, `.ARW`, `.NEF`, `.RAF`, `.DNG`, `.ORF`, `.RW2`, `.PEF`, `.SRW`, `.3FR`, `.IIQ`
  - **Processed / Camera images:** `.JPG`, `.JPEG`, `.HEIC`, `.HEIF`, `.HIF`, `.PNG`, `.TIF`, `.TIFF`, `.WEBP`
  - **Videos:** `.MP4`, `.MOV`, `.CRM`, `.MXF`, `.MTS`, `.M2TS`, `.AVI`, `.MKV`
- **Pre-Flight Disk Space & SD Card Health Checks:**
  - Automatically runs before starting any import:
    - **Destination Free Space:** Confirms target volume has enough space for the full batch plus a safety buffer (blocks transfer if insufficient; warns if remaining space would drop low).
    - **SD Card Health & Writeability:** Verifies the card is readable (and writable if **Move** is selected — catches locked SD write-protect switches up front), flags 0-byte/corrupt/unreadable files before starting, and monitors read errors/retries during transfer so suspect cards never have files deleted unsafely.
- **Fast Multithreaded Verified Move (default) & Optional Copy:**
  - Uses a **parallel multi-threaded transfer pool** tuned to saturate fast UHS-II / CFexpress card readers without thrashing storage.
  - Because SD imports default to **Move** (clearing moved RAW/JPEG/Video files from the SD card after transfer), every file uses a **two-phase verified move**:
    1. Copy to destination temp file (`.truongsync.part`)
    2. Flush (`fsync`) and atomically rename to final filename
    3. Verify byte size matches source (fast default; optional `xxHash3` checksum verify toggle in Settings)
    4. Only then remove the source file from the SD card
  - Collision handling: detect exact duplicates (skip safely without deleting unverified source) vs filename collisions (configurable rename or skip).
  - Optional post-import **Safe Eject** of the SD card once all verified moves finish.

### 2.2 EXIF GPS Geotagging (Photos Only)
- **Strict Media Scope:** GPS tags are written **only to photo files** — both **RAW and JPEG** (plus HEIC/DNG/TIFF if configured). **Video files are strictly skipped** and never modified.
- **Two Modes (Both Supported):**
  1. **Optional Inline Step during SD Import:** Drop/select `.gpx` file(s) right inside the Import screen so photos are geotagged **before** Google Drive sync kicks off (ensuring Drive receives geotagged files on the first upload).
  2. **Standalone Backfill (`Geotag` tab):** Point at any existing shoot folder (or multiple shoots) to backfill GPS data retroactively, inspect existing GPS, or re-tag with a corrected time offset.
- **GPS Track Format & Auto-Match:**
  - **Universal `.gpx`** as the first-class primary format (supports multiple `.gpx` files merged in one session, multi-segment `<trkseg>`, elevation `<ele>`, and UTC timestamps).
  - **Watched GPX Folder Auto-Match:** Optionally configure a watched GPX folder (e.g., `Downloads` or a synced `GPX` folder); when an SD card or shoot folder is loaded, TruongSync checks track time ranges and offers a 1-click **"Attach matching GPX"** button for overlapping tracks.
  - Clean parser trait so additional formats (`.kml`, Google Timeline JSON, `.fit`) can be added later without UI clutter.
- **Timestamp Matching & Interpolation (Strict In-Range Policy):**
  - Reads camera capture timestamp (`DateTimeOriginal` + `SubSecTimeOriginal` + `OffsetTimeOriginal` when present).
  - Supports **camera clock offset calibration** (e.g., camera left in home timezone or drifted by `+3m 12s`).
  - **Linear interpolation** between adjacent track points inside a track segment.
  - **Strictly skip out-of-range photos:** Any photo captured before the track starts, after the track ends, or inside a recording gap exceeding the configurable max-gap threshold is **left untagged** (no forced snapping outside the GPX range).
  - **Matched RAW + JPEG pairs** automatically receive identical coordinates (`GPSLatitude`, `GPSLongitude`, `GPSAltitude`, `GPSTimeStamp`, `GPSDateStamp`, and directional Ref tags).
- **Interactive Preview Before Writing:**
  - Dark MapLibre map showing the loaded GPX track polyline and matched photo pins (with out-of-range / untagged count highlighted).
  - Table preview showing each photo, capture time, matched lat/lon/elevation, time delta to nearest track point, and status (`Matched`, `Already Tagged`, `Out of Range (Skipped)`, `Skipped (Video)`).
  - Explicit **Overwrite existing GPS** toggle (off by default so previously tagged photos aren't clobbered accidentally).
- **Safe EXIF Writing:**
  - Embeds GPS directly into photo EXIF (preserving `DateTimeOriginal`, `SubSecTimeOriginal`, MakerNotes, lens/exposure data, orientation, and embedded RAW previews so TruongPhoto stacking and rendering remain intact).
  - Atomic temp-file write + rename so interrupted writes never corrupt a RAW file.

### 2.3 Google Drive Sync (2-Way Strictly Additive, Scheduled & Manual)
- **Flexible Profile Mapping (Matches Existing Library Layout):**
  - Supports one or multiple Local ↔ Google Drive sync pairs (e.g., `All Photos/Photo Dump` ↔ `My Drive/Raw Photo Backup`, plus optional `Edited Photos` or full `All Photos` roots) — zero re-uploading of existing Drive folders.
- **2-Way Strictly Additive Sync (Two-Pass Union Merge — Never Deletes):**
  - **Pass 1 (`Drive → Local`):** Automatically downloads new files uploaded to Google Drive on the go (e.g., from a phone or travel laptop) into the matching local library path (using atomic `.part` → rename writes so TruongPhoto never indexes partial files).
  - **Pass 2 (`Local → Drive`):** Uploads new local files (new SD imports, freshly exported Edited Photos, `.xmp` sidecars) to Google Drive.
  - **Immutable Conflict Safety:** Matches `Sync-Raw-Photos.ps1`'s `--immutable` guarantee — if the same relative path exists on both sides with different content, **neither copy is silently overwritten**; it is flagged for review in Activity.
  - **Smart GPS-Backfill Propagation:** When TruongSync itself embeds GPS EXIF into an already-synced photo during a backfill, it records `(pre_md5, post_md5)` so the next sync can verify the Drive copy still matches `pre_md5` and safely push the geotagged version without tripping a false conflict (while keeping strict immutability for all unknown mismatches).
  - **Zero Destructive Deletes & Smart Tombstones:** Deleting/culling files locally never deletes them from Google Drive, and TruongSync records previously synced files in its local manifest so locally deleted bad RAWs aren't silently re-downloaded back onto your PC — while genuinely *new* files uploaded to Drive from another device still download automatically.
  - **One-Click Restore / Hydrate Mode:** Explicit restore action (for a new PC or accidental local deletion) that intentionally bypasses local tombstones to pull selected shoots or the full library back from Google Drive.
  - **Mac Metadata Hygiene:** Automatically excludes `._*`, `.DS_Store`, and `__MACOSX/**` in both directions while keeping real `.xmp` sidecars.
  - **Coalesced Follow-Up Queue:** Matches `Sync-Raw-Photos.ps1`'s `sync.lock` + `sync-requested` behavior — if an SD import finishes while a scheduled/startup sync is already running, TruongSync automatically queues one clean follow-up pass so nothing is missed.
- **Three Entry Points:**
  1. **Auto-chained after SD Import** (only when all SD transfers succeed; toggleable per import preset — kicks off immediately after import & optional geotagging finish so the SD card can be ejected right away).
  2. **User-Scheduled Background Sync** (configurable interval/schedule — e.g., at startup/login, every X hours, daily at a set time, or on idle — running quietly in the tray).
  3. **Manual "Sync Now" & "Verify" right on the Cloud Sync screen** (one-click read-only audit comparing Local ↔ Drive by size & Drive MD5, detecting missing files, size/checksum mismatches, and duplicate Google Drive paths, plus one-click sync for any differences).
- **Backup Freshness Indicator:**
  - Shows last synced timestamp, stale-backup warnings, and pending upload/download counts & GB at a glance.
- **Google Drive Sync Engine:**
  - Powered by a cross-platform bundled **`rclone` engine** (or native Drive v3 client — see `02-ARCHITECTURE.md` for why bundled `rclone` gives 1:1 parity with `Sync-Raw-Photos.ps1` + `Audit-PhotoCopies.ps1` while hiding all CLI complexity behind TruongSync's UI!).
  - Supports both **importing an existing `rclone.conf`** and **built-in OAuth 2.0 sign-in** inside the app.
  - Resumable chunked transfers, `--drive-skip-gdocs`, `--drive-stop-on-upload-limit`, pause/resume, exponential retry backoff, and optional upload/download bandwidth cap.

### 2.4 External Drive Sync & Backup (2-Way Strictly Additive)
- Replaces `Backup-Photos.ps1` and `Audit-PhotoCopies.ps1` (External phase).
- **2-Way Strictly Additive Policy (Two-Pass Union Merge — Never Deletes):**
  - **Pass 1 (`External Drive → Local`):** Pulls trip photos or additions from the external SSD onto the PC first (never overwrites newer destination files).
  - **Pass 2 (`Local → External Drive`):** Copies the complete combined local library (e.g., `All Photos` including `Photo Dump` + `Edited Photos`) to the external drive.
  - Direction configurable per profile (`2-Way Additive` default, or `Push Only`).
  - **Never deletes** files from either side, with local delete tombstones (so locally culled RAWs stay backed up without bouncing back) **plus** an explicit **Restore / Hydrate** option for recovering deleted files or setting up a new PC.
  - **Mac Metadata Hygiene:** Skips `._*`, `.DS_Store`, `__MACOSX/**` in both directions while keeping `.xmp` editing sidecars.
  - **Optional Marker File:** Continues writing `Last Sync - yyyy-MM-dd HH-mm-ss.txt` at the external backup root (toggleable) for instant visibility when browsing the drive in Finder/Explorer.
- **Backup Freshness & Verify on Screen:**
  - Each external drive profile shows **Backup Freshness** (last sync timestamp, age badge such as `Up to date` vs `12 days ago`, and pending delta when plugged in).
  - One-click **Verify** button right on the Drive Backup screen (plus optional sequential 3-way `PC ↔ External ↔ Drive` audit so mechanical HDDs aren't thrashed by simultaneous reads!).
- **Named Backup Profiles & Scheduled Checks:**
  - Pair local library roots (e.g., `All Photos` ↔ `Photo Library Backup/All Photos`) with an external HDD/SSD.
  - Identify target drives by **Volume UUID + Label** (cross-platform macOS + Windows; resilient to drive letter changes like `G:` → `H:` or macOS mount changes).
  - Auto-detect when a known backup drive is plugged in, plus optional user-configured schedule for always-attached desktop backup drives.
- **Fast Multithreaded Incremental Sync:**
  - Dry-run diff preview showing exact counts & GB to push (`↑`) and pull (`↓`).
  - Copy-to-`.part` + atomic rename + optional hash verification (`xxHash3` / MD5).

### 2.5 Unified Pipeline, Queue & Tray
- Every operation (Import, Geotag, Google Drive Sync, External Drive Backup) runs through a **unified Job Queue**.
- Users can trigger:
  - A **chained pipeline** (e.g., `SD Import (Move)` → `Geotag Photos` → `Google Drive Sync` → `External Drive Backup` if drive connected), OR
  - Any individual step on demand.
- **Persistent SQLite History:** Every run records files moved, tagged, uploaded, skipped, bytes transferred, throughput, duration, and expandable per-file logs/errors.
- **Background Menu Bar / System Tray & Auto-Open on Insert:**
  - **Primary (Default):** Lightweight menu bar / system tray mode (launch at login; closing the window hides to tray and unloads the WebView when idle so RAM/CPU stay near zero).
    - Listens to native OS volume-arrival events (`DiskArbitration` on macOS, `WM_DEVICECHANGE` on Windows) with zero polling.
    - **Smart Volume Routing:** Automatically opens and focuses the **Import** tab with the card pre-selected when a removable camera card (`DCIM/` or camera media) is inserted — while ignoring random USB sticks. If a known **external backup drive** (matching Volume UUID) is plugged in, shows a quick **"Back Up Now"** prompt instead.
    - Keeps long Google Drive uploads and external drive backups running seamlessly in the background with live tray progress, pause/resume, and OS notifications.
  - **Power & Network Controls:**
    - Optional **upload/download bandwidth cap** (e.g., MB/s or Unlimited) for Google Drive transfers so large shoots don't saturate your connection.
    - **Sleep prevention** toggle while active SD moves, backups, or large syncs are running.
    - **Sync on battery** toggle so scheduled background tasks can either run on battery or wait until plugged into AC power (manual sync always works).
  - **Optional One-Click OS-Native Auto-Launch (Works Even When Quit):**
    - A single toggle in **Settings → Desktop & Auto-Open** installs/removes an OS-level mount trigger so TruongSync can still auto-open on SD card insert even if the user has fully quit the tray app:
      - **macOS:** User `launchd` LaunchAgent (`WatchPaths: ["/Volumes"]`) running a millisecond mount check before launching the GUI.
      - **Windows:** Per-user `HKCU` Windows **AutoPlay** handler registration (with a one-click button to open Windows AutoPlay defaults if needed).

---

## 3. Non-Goals (Explicitly Out of Scope)

- Photo library browsing, justified timeline, lightbox viewing, or RAW image editing (handled by **TruongPhoto** / Lightroom / Capture One).
- Face recognition, AI captioning, or semantic search (handled by **TruongPhoto**).
- Editing video metadata / transcoding LOG video (videos are copied/moved/backed up untouched).
- Multi-user web server / LAN streaming (TruongSync is a focused local desktop utility).

---

## 4. Phased Delivery Roadmap (Draft)

| Phase | Scope | Exit Criteria |
| --- | --- | --- |
| **Phase 0** | Review user's existing PowerShell scripts & lock answers in `05-MIGRATION-AND-QUESTIONS.md` | Exact parity checklist & finalized spec |
| **Phase 1** | Tauri 2 + React shell with Codex Dark theme, sidebar, tray, settings, SQLite job store | Empty shell runs on macOS & Windows with identical TruongPhoto look & feel |
| **Phase 2** | SD Card detector, Universal Folder Sorter, dry-run preview, two-phase verified Move/Copy, safe eject | Real test SD card imports cleanly with 0 data loss & full parity with PowerShell sorter |
| **Phase 3** | EXIF GPS Geotagger (GPX/KML/Timeline JSON parser, clock offset, MapLibre preview, safe RAW+JPEG EXIF writer, strict video skip) | Real Canon CR3 + JPEG + MP4 shoot geotags photos only; TruongPhoto reads pins & keeps stacks intact |
| **Phase 4** | Google Drive Sync (OAuth2, resumable uploads, post-import trigger, manual full-library catch-up, pause/resume) | Incremental upload & catch-up verified against real Drive test folder |
| **Phase 5** | External Drive Backup (Volume UUID matching, dry-run diff, incremental sync, auto-detect prompt) | External SSD/HDD incremental backup verified with drive-letter/mount resilience |
| **Phase 6** | Pipeline chaining, polish, signed macOS + Windows CI releases & in-app updater | GitHub Release (`latest.json`) live for both platforms |
