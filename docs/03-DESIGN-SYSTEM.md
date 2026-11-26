# TruongSync — UI & Design System (Truong Suite / Codex Dark)

TruongSync inherits the **exact Codex Dark visual language** used in **TruongPhoto** (originally ported 1:1 from **TruongPaper**) so every app in the Truong suite feels like part of the same native desktop ecosystem.

A ready-to-use CSS starter extracted directly from TruongPhoto lives alongside this doc at [`04-UI-STYLE-REFERENCE.css`](./04-UI-STYLE-REFERENCE.css).

---

## 1. Visual Identity & Core Tokens

### Color Palette (Dark-Only Neutral Chrome)
TruongPhoto avoids loud saturated chrome — surfaces use crisp stepped neutral grays (`#171717` → `#202020` → `#242424` → `#292929`), high-contrast white primary buttons (`#e5e5e5` on `#202020` ink), and restrained semantic accents only for status badges, alerts, and progress indicators.

| Token | Value | Usage |
| --- | --- | --- |
| `--surface-sidebar` | `#171717` | Left navigation rail, map container backdrop |
| `--surface-main` | `#202020` | Main workspace background & tray shell |
| `--surface-card` | `#242424` | Panels, cards, job cards, profile cards |
| `--surface-input` | `#292929` | Dropdown triggers, inputs, selectable chips |
| `--surface-inspector` | `#1b1b1b` | Slide-over detail/log inspector drawer |
| `--surface-omnibar` | `#262626` | Header search/filter bar & `.field-box` |
| `--border` | `#343434` | Card/panel borders, dividers |
| `--border-subtle` | `#2c2c2c` | Sidebar right edge |
| `--border-titlebar` | `#303030` | Workspace top bar bottom divider |
| `--border-button` | `#414141` | Default secondary button border |
| `--border-strong` | `#4a4a4a` | Popovers, floating dropdown menus |
| `--text-primary` | `#ededed` | Primary headings, labels, values |
| `--text-secondary` | `#acacac` | Secondary descriptions, metadata |
| `--text-muted` | `#9a9a9a` | Subtitles, placeholders, quiet captions |
| `--ok` / `--ok-bg` | `#8fd19e` / `#20302a` | Verified move, GPS matched, Drive synced, connected dot |
| `--warn-text` / `--warn-bg` | `#d0b881` / `#39352c` | Clock offset warning, partial match, skipped files, offline notice |
| `--danger-text` / `--danger-bg` | `#f0a8a8` / `#352626` | Move/upload failure, destructive confirmation, missing drive |
| `--info-text` | `#9cc4f0` | In-progress / informational badges (`rgba(120, 170, 240, 0.14)` bg) |

### Typography & Spacing
- **Sans:** `-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif` — base `13px / 1.5`, `-webkit-font-smoothing: antialiased`.
- **Mono:** `ui-monospace, SFMono-Regular, Consolas, monospace` — used for file paths, coordinates (`45.3736, -121.6960`), speeds (`142.4 MB/s`), checksums, and timestamps. Always enable `font-variant-numeric: tabular-nums` on live counters, progress percentages, and speeds so numbers never jitter!
- **Heading Scale:**
  - View title (`.view-header h2`): `21px`, `600`, `letter-spacing: -0.5px`
  - View subtitle (`.view-subtitle`): `12px`, `var(--text-muted)`, `margin-top: 6px`
  - Section title (`.section-title`): `12px`, `500`, `#c7c7c7`, `margin: 26px 0 12px`
  - App brand (`.app-brand`): `14px`, `600`, `letter-spacing: -0.2px`
- **Spacing Scale:** `--space-1: 4px`, `--space-2: 8px`, `--space-3: 12px`, `--space-4: 16px`, `--space-5: 24px`.
- **Radii:** `--radius-sm: 5px`, `--radius-md: 8px`, buttons/inputs `6px`, dropdown menus `9px`, frameless desktop window `10px`.
- **Icons:** `lucide-react` at `16px` (`strokeWidth={1.6}`) for sidebar nav, `14px` for buttons/controls.

---

## 2. Shared Shell & Reusable Components from TruongPhoto

When building TruongSync, port these proven components directly from `TruongPhoto/src/components/`:

| Component | Source in TruongPhoto | Role in TruongSync |
| --- | --- | --- |
| **App Shell** | `App.tsx` + `theme.css` (`.app-window-frame`, `.studio-shell`) | `208px` sidebar + fluid workspace, `51px` titlebar, native macOS overlay + Windows 11 `46x32` caption buttons |
| **Sidebar** | `Sidebar.tsx` | Brand icon + `TruongSync`, primary nav tabs, bottom `StatusBanner`, live status readout card (Active Jobs / Drive / Mounted Drives), `Settings` tab |
| **WindowControls** | `WindowControls.tsx` | Frameless window maximize/fullscreen tracking + Windows 11 Fluent caption glyphs |
| **ViewHeader & SectionTitle** | `ui.tsx` | Consistent view title, subtitle, and right-aligned action buttons |
| **Badge** | `ui.tsx` | Status pills (`ok`, `warn`, `danger`, `info`, `muted`) for file types (`RAW`, `JPEG`, `VIDEO`), GPS match state, and sync state |
| **ToggleRow & SegmentedControl** | `ui.tsx` | Settings switches (`Move` vs `Copy`, `Skip Videos`, `Auto-Sync to Drive`, `Eject After Import`) |
| **Dropdown** | `Dropdown.tsx` | Portal-mounted glassmorphic dropdown (`rgba(34,34,36,0.96)` + `blur(18px)`) with keyboard navigation & Escape stack |
| **FeedbackHost & StatusBanner** | `Feedback.tsx` | Bottom-center toasts, destructive `ConfirmDialog` (e.g., mirror delete / overwrite GPS), quiet sidebar status strip |
| **EmptyState** | `ui.tsx` | Dashed `#414141` empty state when no SD card or GPS track is loaded |
| **InspectorDrawer** | `InspectorDrawer.tsx` | `296px` right slide-over drawer for inspecting per-file logs, EXIF GPS preview, or active transfer details |
| **Progress Bars** | `theme.css` (`.upload-progress-bar`, `.update-progress-track`) | `5px`–`6px` pill tracks for per-stage and overall transfer progress |
| **Map Shell** | `theme.css` (`.map-view-shell`, `.map-status-pill`) | Dark MapLibre GL container + floating glass status pill for GPS track + matched photo preview |

---

## 3. TruongSync Navigation & Screen Map

All screens sit inside `.studio-shell` (`208px` sidebar + `.workspace-header` + `.content-viewport`, max width `1020px` for workflows, `740px` for Settings):

### Sidebar Structure
```text
┌─────────────────────────┐
│ [Icon] TruongSync       │
├─────────────────────────┤
│  HardDriveDownload Import│   ← SD card ingest, folder sorting, pipeline launch
│  MapPin            Geotag│   ← Standalone GPS track matcher + MapLibre preview
│  CloudUpload   Cloud Sync│   ← Google Drive sync profiles, manual Sync Now, queue
│  HardDrive   Drive Backup│   ← External HDD/SSD backup profiles & auto-detect
│  Activity        Activity│   ← Live transfer queue, throughput, job history & logs
│                         │
│        (spacer)         │
│  [Quiet StatusBanner]   │
│ ┌─────────────────────┐ │
│ │ Jobs          Idle  │ │   ← Click opens right InspectorDrawer with live queue
│ │ Drive      ● Ready  │ │
│ └─────────────────────┘ │
│  Settings      Settings │   ← Presets, file extensions, accounts, ExifTool, updates
└─────────────────────────┘
```

### Key Screen Layouts

1. **Import View (`Import`)**
   - **Source Strip (Card):** Detected removable cards/drives as selectable cards (volume name, free/used space, detected counts: `142 RAW · 142 JPEG · 8 Video`) + "Browse Folder…" fallback.
   - **Pre-Flight Check Strip:** Compact inline status row showing **Disk Space** (`ok` / `warn` / `danger` badge with free space after move) and **Card Health** (`Readable · Writable · 0 corrupt files` — immediately flags locked SD tabs or unreadable files before starting).
   - **Destination & Naming Card:** Preset dropdown (`Photography Default`, custom presets), Shoot Name, Category dropdown (dynamically loaded from existing `{Year}` folders + inline `+ New Category`), Date picker (auto-filled from earliest photo EXIF date, formatted `M-dd` by default), multi-day **Split by Date / Shoot** toggle, and live resolved path preview in monospace (`…/2026/Hikes/Cone Peak 4-15/{RAW, JPEG, VID}`).
   - **Pipeline Steps Card (Toggles):**
     - Transfer Mode: `SegmentedControl` (`Move (Remove from SD)` | `Copy (Keep on SD)`)
     - `[Toggle] Geotag photos (RAW + JPEG only)` → expands dropzone for `.gpx` files, **auto-matched GPX suggestion chips** from the watched GPX folder, and clock offset summary
     - `[Toggle] Sync shoot to Google Drive after import`
     - `[Toggle] Sync with External Drive (if connected)`
     - `[Toggle] Eject SD card after verified move`
   - **Footer / Action Bar:** `Preview Plan` (dry-run file table) + primary white CTA `Import 292 Files (11.4 GB)`.

2. **Geotag View (`Geotag`)**
   - Split top controls: **Target Folder / Photos** picker + **GPS Track (`.gpx`)** dropzone.
   - **Calibration Strip:** Camera timezone / clock offset (`+/- hh:mm:ss`), Max Interpolation Gap (`15 min`), `Overwrite Existing GPS` toggle, and a muted pill reminder: `Applies to RAW & JPEG only · Videos & out-of-range skipped`.
   - **Map Preview (`.map-view-shell`):** Dark MapLibre map rendering the GPX track line and matched photo markers + floating `.map-status-pill` (`142 matched · 4 out of range · 8 videos skipped`).
   - **Photo Match Table + CTA:** Per-file preview list + primary CTA `Write GPS to 142 Photos`.

3. **Cloud Sync View (`Cloud Sync`)**
   - **Freshness & Status Header Card:** Signed-in Google account badge, **Backup Freshness** badge (`Up to date · Checked 14m ago` or `Behind · ↑ 12 to upload · ↓ 3 on Drive`), next scheduled background run time, Drive storage quota bar, plus top-right **`Verify`**, **`Restore…`**, and **`Sync Now`** (primary white CTA).
   - **Schedule Strip:** Quick schedule selector (`Manual`, `Every 1h / 6h / 12h`, `Daily at HH:MM`, `Check only` vs `Auto-sync`) right on the page or synced with Settings.
   - **Sync Profiles List (`.folder-row` pattern):** Local root ↔ Remote Google Drive folder, mode badge (`2-Way Additive · Never Deletes`), last verified/synced timestamps, pending `↑` upload / `↓` download counts, and per-profile **`Verify`** and **`Sync`** buttons.
   - **Verify Result Drawer/Panel:** Expandable audit summary (`24,812 verified · ↑ 12 missing on Drive · ↓ 3 new on Drive · 0 mismatched`) with one-click **`Sync Differences`**.

4. **Drive Backup View (`Drive Backup`)**
   - **Freshness Overview & Connected Backup Drives:** Cards for every configured external drive showing Volume UUID connection status (`Connected` green / `Disconnected` muted), **Backup Freshness** badge (`Last synced 2 days ago` / `Stale · 19 days ago`), free space bar, mode badge (`2-Way Additive · Never Deletes`), and **`Verify`**, **`Restore…`**, + **`Sync Now`** buttons.
   - **Schedule & Rules:** Optional schedule for always-attached drives + auto-prompt when plugged in, source roots included, exclusion patterns, and expandable dry-run / verify diff (`↑ 412 to drive · ↓ 6 from drive · 0 mismatched`).

5. **Activity View (`Activity`)**
   - **Active Pipeline Card:** Stage stepper (`Import` → `Geotag` → `Drive Sync` → `External Backup`), overall progress bar (`.upload-progress-bar`), current file, live MB/s throughput, ETA, `Pause` / `Cancel` buttons.
   - **History List:** Filterable past runs (`All`, `Import`, `Geotag`, `Cloud`, `Backup`) with status badges (`ok`, `warn`, `danger`); clicking any run opens the `.inspector-drawer` with detailed per-file results and copyable error log.

6. **Settings View (`Settings`)**
   - `max-width: 740px` centered layout matching TruongPhoto's Settings view:
     - **Import Presets & Folder Templates** (manage path variables, RAW/JPEG/Video extension lists, verification level)
     - **Geotagging Defaults** (watched GPX folder for auto-matching, default max gap, ExifTool status/version readout, preserve `mtime` vs touch `mtime`)
     - **Google Drive Account & Sync Defaults** (OAuth connect/disconnect, concurrency, optional upload/download bandwidth cap)
     - **Desktop, Tray, Power & Auto-Open** (Launch at login, prevent sleep during transfers, allow/pause scheduled sync on battery, auto-open Import on SD card insert, prompt backup when known external drive connects, and one-click toggle for **OS-Native Auto-Open Even When Quit** [macOS LaunchAgent / Windows AutoPlay])
     - **App Updates** (identical TruongPhoto inline signed updater card with `.update-progress-track`)
