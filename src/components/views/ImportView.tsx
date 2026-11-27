import React, { useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  CheckCircle2,
  FolderOpen,
  HardDriveDownload,
  MapPin,
  Plus,
  RefreshCw,
  ShieldAlert,
  Split,
  Usb,
  X,
} from 'lucide-react';
import { api, formatBytes, formatDurationMs, pickDirectory, pickFiles } from '../../lib/tauri';
import { notifyError, notifyInfo, notifySuccess } from '../../store/feedback';
import { useSyncStore } from '../../store/useSyncStore';
import type {
  CardScanResult,
  ImportPlanItem,
  ImportPreset,
  PipelineSummary,
  PreflightReport,
  ShootAssignment,
  WatchedGpxMatch,
} from '../../types/api';
import { Dropdown } from '../Dropdown';
import { QuickSetupCard } from '../QuickSetupCard';
import { Badge, EmptyState, SectionTitle, SegmentedControl, ToggleRow, ViewHeader } from '../ui';

function todayIso(): string {
  const d = new Date();
  const mm = String(d.getMonth() + 1).padStart(2, '0');
  const dd = String(d.getDate()).padStart(2, '0');
  return `${d.getFullYear()}-${mm}-${dd}`;
}

const MONTHS_SHORT = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const MONTHS_FULL = [
  'January',
  'February',
  'March',
  'April',
  'May',
  'June',
  'July',
  'August',
  'September',
  'October',
  'November',
  'December',
];

function previewDateFormat(iso: string, fmt: string): string {
  const [y, m, d] = iso.split('-');
  if (!y || !m || !d) return iso;
  const mNum = Math.max(1, Math.min(12, Number(m) || 1));
  const dNum = Math.max(1, Math.min(31, Number(d) || 1));
  let out = '';
  for (let i = 0; i < fmt.length; ) {
    const rest = fmt.slice(i);
    if (rest.startsWith("'")) {
      const end = rest.indexOf("'", 1);
      if (end > 0) {
        out += rest.slice(1, end);
        i += end + 1;
        continue;
      }
    }
    if (rest.startsWith('yyyy') || rest.startsWith('YYYY')) {
      out += y.padStart(4, '0');
      i += 4;
    } else if (rest.startsWith('MMMM')) {
      out += MONTHS_FULL[mNum - 1];
      i += 4;
    } else if (rest.startsWith('MMM')) {
      out += MONTHS_SHORT[mNum - 1];
      i += 3;
    } else if (rest.startsWith('yy') || rest.startsWith('YY')) {
      out += y.slice(-2);
      i += 2;
    } else if (rest.startsWith('MM')) {
      out += String(mNum).padStart(2, '0');
      i += 2;
    } else if (rest.startsWith('dd') || rest.startsWith('DD')) {
      out += String(dNum).padStart(2, '0');
      i += 2;
    } else if (rest.startsWith('M')) {
      out += String(mNum);
      i += 1;
    } else if (rest.startsWith('d') || rest.startsWith('D')) {
      out += String(dNum);
      i += 1;
    } else {
      out += rest[0];
      i += 1;
    }
  }
  return out;
}

export const ImportView: React.FC = () => {
  const volumes = useSyncStore((s) => s.volumes);
  const presets = useSyncStore((s) => s.presets);
  const settings = useSyncStore((s) => s.settings);
  const cloudProfiles = useSyncStore((s) => s.cloudProfiles);
  const externalProfiles = useSyncStore((s) => s.externalProfiles);
  const importProgress = useSyncStore(
    (s) =>
      Object.values(s.activeJobs).find(
        (j) => j.job_kind === 'pipeline' || (!j.job_kind && j.stage === 'import'),
      ) ?? null,
  );
  const pendingAutoImportPath = useSyncStore((s) => s.pendingAutoImportPath);
  const setPendingAutoImportPath = useSyncStore((s) => s.setPendingAutoImportPath);
  const refreshVolumes = useSyncStore((s) => s.refreshVolumes);
  const refreshProfiles = useSyncStore((s) => s.refreshProfiles);
  const refreshJobs = useSyncStore((s) => s.refreshJobs);
  const refreshSettings = useSyncStore((s) => s.refreshSettings);

  const hydratedPresetIdRef = useRef<string | null>(null);
  const prevPresetRef = useRef<ImportPreset | null>(null);
  const scanSeqRef = useRef(0);

  const defaultPreset = presets[0];
  const [selectedPresetId, setSelectedPresetId] = useState(defaultPreset?.id ?? '');
  const [sourceRoot, setSourceRoot] = useState('');
  const [destRoot, setDestRoot] = useState('');
  const [folderTemplate, setFolderTemplate] = useState('{Year}/{Category}/{ShootName} {Date}/{Type}');
  const [dateFormat, setDateFormat] = useState('M-dd');
  const [rawFolder, setRawFolder] = useState('RAW');
  const [jpegFolder, setJpegFolder] = useState('JPEG');
  const [videoFolder, setVideoFolder] = useState('VID');
  const [transferMode, setTransferMode] = useState<'move' | 'copy'>('move');

  const [scan, setScan] = useState<CardScanResult | null>(null);
  const [scanning, setScanning] = useState(false);
  const [preflight, setPreflight] = useState<PreflightReport | null>(null);
  const [planPreview, setPlanPreview] = useState<ImportPlanItem[] | null>(null);

  // Single-shoot & Multi-day split state
  const [splitByDate, setSplitByDate] = useState(false);
  const [shootName, setShootName] = useState('');
  const [category, setCategory] = useState('Hikes');
  const [folderDate, setFolderDate] = useState(todayIso());
  const [categories, setCategories] = useState<string[]>([
    'Adventures',
    'Hikes',
    'Portraits',
    'Random',
    'Sports',
    'Wildlife',
  ]);
  const [newCatOpen, setNewCatOpen] = useState(false);
  const [newCatInput, setNewCatInput] = useState('');
  const [dateAssignments, setDateAssignments] = useState<
    Record<string, { shoot_name: string; category: string; folder_date: string; included: boolean }>
  >({});

  // Pipeline steps
  const [geotagEnabled, setGeotagEnabled] = useState(false);
  const [gpxFiles, setGpxFiles] = useState<string[]>([]);
  const [gpxSuggestions, setGpxSuggestions] = useState<WatchedGpxMatch[]>([]);
  const [clockOffsetText, setClockOffsetText] = useState('0');
  const clockOffsetSecs = Number.isFinite(Number(clockOffsetText)) ? Math.trunc(Number(clockOffsetText)) : 0;
  const [ejectAfter, setEjectAfter] = useState(false);
  const [selectedCloudIds, setSelectedCloudIds] = useState<string[]>([]);
  const [selectedExternalIds, setSelectedExternalIds] = useState<string[]>([]);

  const [running, setRunning] = useState(false);
  const [lastSummary, setLastSummary] = useState<PipelineSummary | null>(null);

  // Hydrate form fields when preset selection changes OR apply per-field diffs when the
  // selected preset's saved defaults change (e.g. edited in SettingsView), without clobbering
  // unrelated in-form user overrides like Copy vs Move or Geotag toggles.
  useEffect(() => {
    const p = presets.find((x) => x.id === selectedPresetId) ?? presets[0];
    if (!p) return;
    if (!selectedPresetId) setSelectedPresetId(p.id);
    const presetSwitched = hydratedPresetIdRef.current !== p.id;
    const prev = prevPresetRef.current;

    if (presetSwitched) {
      hydratedPresetIdRef.current = p.id;
      prevPresetRef.current = p;
      setDestRoot((cur) => cur || p.root_path || settings?.default_import_root || '');
      setFolderTemplate(p.folder_template);
      setDateFormat(p.date_format);
      setRawFolder(p.raw_folder);
      setJpegFolder(p.jpeg_folder);
      setVideoFolder(p.video_folder);
      setTransferMode(p.transfer_mode === 'copy' ? 'copy' : 'move');
      setGeotagEnabled(p.auto_geotag);
      setEjectAfter(p.eject_after);
      if (p.auto_cloud_sync) {
        setSelectedCloudIds(cloudProfiles.filter((c) => c.enabled).map((c) => c.id));
      }
      if (p.auto_external_sync) {
        setSelectedExternalIds(externalProfiles.filter((e) => e.enabled).map((e) => e.id));
      }
    } else if (prev && !running) {
      prevPresetRef.current = p;
      if (prev.root_path !== p.root_path) {
        setDestRoot((cur) => (!cur || cur === prev.root_path ? p.root_path : cur));
      }
      if (prev.folder_template !== p.folder_template) setFolderTemplate(p.folder_template);
      if (prev.date_format !== p.date_format) setDateFormat(p.date_format);
      if (prev.raw_folder !== p.raw_folder) setRawFolder(p.raw_folder);
      if (prev.jpeg_folder !== p.jpeg_folder) setJpegFolder(p.jpeg_folder);
      if (prev.video_folder !== p.video_folder) setVideoFolder(p.video_folder);
      if (prev.transfer_mode !== p.transfer_mode) {
        setTransferMode(p.transfer_mode === 'copy' ? 'copy' : 'move');
      }
      if (prev.auto_geotag !== p.auto_geotag) setGeotagEnabled(p.auto_geotag);
      if (prev.eject_after !== p.eject_after) setEjectAfter(p.eject_after);
    }
  }, [presets, selectedPresetId, settings?.default_import_root, running]);

  // Prune deleted cloud/external profile IDs without overriding user selections or transferMode
  useEffect(() => {
    const validCloud = new Set(cloudProfiles.map((c) => c.id));
    setSelectedCloudIds((prev) => prev.filter((id) => validCloud.has(id)));
  }, [cloudProfiles]);

  useEffect(() => {
    const validExt = new Set(externalProfiles.map((e) => e.id));
    setSelectedExternalIds((prev) => prev.filter((id) => validExt.has(id)));
  }, [externalProfiles]);

  // Handle auto-open from SD card arrival (defer while an import pipeline is actively running)
  useEffect(() => {
    if (!pendingAutoImportPath || running) return;
    const path = pendingAutoImportPath;
    setPendingAutoImportPath(null);
    void handleScanSource(path);
  }, [pendingAutoImportPath, running]);

  // Load dynamic year categories whenever destRoot, folderDate, or default_categories changes
  useEffect(() => {
    const applyCategories = (cats: string[]) => {
      if (cats.length === 0) return;
      setCategories(cats);
      setCategory((prev) => (cats.includes(prev) ? prev : cats[0]));
      setDateAssignments((prev) => {
        let changed = false;
        const next: typeof prev = {};
        for (const [k, row] of Object.entries(prev)) {
          if (!cats.includes(row.category)) {
            changed = true;
            next[k] = { ...row, category: cats[0] };
          } else {
            next[k] = row;
          }
        }
        return changed ? next : prev;
      });
    };

    if (!destRoot.trim()) {
      if (settings?.default_categories && settings.default_categories.length > 0) {
        applyCategories(settings.default_categories);
      }
      return;
    }
    let cancelled = false;
    const year = Number(folderDate.slice(0, 4)) || new Date().getFullYear();
    const timer = window.setTimeout(() => {
      void api
        .listYearCategories(destRoot, year, false)
        .then((cats) => {
          if (!cancelled) applyCategories(cats);
        })
        .catch(() => {});
    }, 250);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [destRoot, folderDate, settings?.default_categories]);

  // Re-run preflight automatically when scan, destRoot, or transferMode changes (debounced)
  useEffect(() => {
    if (!scan) {
      setPreflight(null);
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void api
        .preflightImport(scan, destRoot, transferMode === 'move')
        .then((rep) => {
          if (!cancelled) setPreflight(rep);
        })
        .catch(() => {});
    }, 200);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [scan, destRoot, transferMode]);

  // Invalidate stale dry-run plan preview whenever destination/naming inputs change
  useEffect(() => {
    setPlanPreview(null);
  }, [destRoot, folderTemplate, dateFormat, rawFolder, jpegFolder, videoFolder, splitByDate, shootName, category, folderDate, dateAssignments]);

  const handleScanSource = async (
    path: string,
    opts?: { preserveSummary?: boolean; force?: boolean; silentIfMissing?: boolean },
  ) => {
    if (running && !opts?.force) return;
    const seq = ++scanSeqRef.current;
    setSourceRoot(path);
    setScan(null);
    setScanning(true);
    setPlanPreview(null);
    if (!opts?.preserveSummary) {
      setLastSummary(null);
    }
    try {
      const res = await api.scanCard(path);
      if (scanSeqRef.current !== seq) return;
      setScan(res);
      if (res.date_groups.length > 0) {
        const firstDate = res.date_groups[0].date;
        setFolderDate(firstDate);
        const initSplit: typeof dateAssignments = {};
        for (const g of res.date_groups) {
          initSplit[g.date] = {
            shoot_name: shootName || '',
            category,
            folder_date: g.date,
            included: true,
          };
        }
        setDateAssignments(initSplit);
        setSplitByDate(res.date_groups.length > 1);
      }
      // Check watched GPX folder for overlapping tracks!
      if (res.earliest_utc && res.latest_utc) {
        const matches = await api.findMatchingGpx(res.earliest_utc, res.latest_utc).catch(() => []);
        if (scanSeqRef.current === seq) {
          setGpxSuggestions(matches);
        }
      } else {
        setGpxSuggestions([]);
      }
    } catch (err) {
      if (scanSeqRef.current === seq) {
        if (opts?.silentIfMissing) {
          setSourceRoot('');
          setScan(null);
          setPreflight(null);
          setPlanPreview(null);
        } else {
          notifyError(err, 'Card scan failed');
        }
      }
    } finally {
      if (scanSeqRef.current === seq) {
        setScanning(false);
      }
    }
  };

  const buildAssignments = (): ShootAssignment[] => {
    if (!splitByDate || !scan || scan.date_groups.length <= 1) {
      return [
        {
          dates: [],
          shoot_name: shootName.trim() || 'Untitled Shoot',
          category: category.trim() || 'Random',
          folder_date: folderDate,
        },
      ];
    }
    return Object.entries(dateAssignments)
      .filter(([, cfg]) => cfg.included)
      .map(([date, cfg]) => ({
        dates: [date],
        shoot_name: cfg.shoot_name.trim() || 'Untitled Shoot',
        category: cfg.category.trim() || 'Random',
        folder_date: cfg.folder_date || date,
      }));
  };

  const formatPreviewPath = (dateStr: string, catStr: string, shootStr: string) => {
    const year = dateStr.slice(0, 4) || String(new Date().getFullYear());
    const dStr = previewDateFormat(dateStr, dateFormat);
    const rootClean = (destRoot || '<Destination Root>').replace(/[\\/]+$/, '');
    const tokens: Record<string, string> = {
      year,
      category: catStr || 'Category',
      shootname: shootStr.trim() || 'Cone Peak',
      date: dStr,
      type: `{${rawFolder}, ${jpegFolder}, ${videoFolder}}`,
    };
    const filled = folderTemplate.replace(/\{([A-Za-z]+)\}/g, (match, key: string) => {
      const val = tokens[key.toLowerCase()];
      return val !== undefined ? val : match;
    });
    return `${rootClean}/${filled}`;
  };

  const resolvedPathPreviews = useMemo(() => {
    if (splitByDate && scan && scan.date_groups.length > 1) {
      const included = scan.date_groups
        .map((g) => {
          const row = dateAssignments[g.date];
          if (!row?.included) return null;
          return formatPreviewPath(row.folder_date || g.date, row.category, row.shoot_name);
        })
        .filter((x): x is string => Boolean(x));
      return included.length > 0 ? included : ['Select at least one capture date above'];
    }
    return [formatPreviewPath(folderDate, category, shootName)];
  }, [
    destRoot,
    folderTemplate,
    folderDate,
    dateFormat,
    category,
    shootName,
    rawFolder,
    jpegFolder,
    videoFolder,
    splitByDate,
    scan,
    dateAssignments,
  ]);

  const handleCreateCategory = async () => {
    const trimmed = newCatInput.trim();
    if (!trimmed) return;
    if (destRoot.trim()) {
      const year = Number(folderDate.slice(0, 4)) || new Date().getFullYear();
      try {
        const created = await api.createCategoryFolder(destRoot, year, trimmed);
        setCategories((prev) => Array.from(new Set([...prev, created])).sort());
        setCategory(created);
        notifySuccess(`Created category "${created}"`);
      } catch (err) {
        notifyError(err, 'Failed to create category');
        return;
      }
    } else {
      setCategories((prev) => Array.from(new Set([...prev, trimmed])).sort());
      setCategory(trimmed);
    }
    setNewCatInput('');
    setNewCatOpen(false);
  };

  const handlePreviewPlan = async () => {
    if (!scan) return;
    try {
      const items = await api.previewImportPlan({
        scan,
        destRoot,
        folderTemplate,
        dateFormat,
        rawFolder,
        jpegFolder,
        videoFolder,
        assignments: buildAssignments(),
      });
      setPlanPreview(items);
    } catch (err) {
      notifyError(err, 'Preview failed');
    }
  };

  const handleRunPipeline = async () => {
    if (!scan) return;
    if (!destRoot.trim()) {
      notifyError('Choose a destination folder root first.');
      return;
    }
    const isMultiDay = splitByDate && scan.date_groups.length > 1;
    if (isMultiDay) {
      const includedEntries = Object.entries(dateAssignments).filter(([, cfg]) => cfg.included);
      if (includedEntries.length === 0) {
        notifyError('Select at least one capture date to import.');
        return;
      }
      const missingName = includedEntries.find(([, cfg]) => !cfg.shoot_name.trim());
      if (missingName) {
        notifyError(`Enter a shoot name for ${missingName[0]} before importing.`);
        return;
      }
    } else if (!shootName.trim()) {
      notifyError('Enter a shoot name before importing.');
      return;
    }
    if (geotagEnabled && gpxFiles.length === 0) {
      notifyError('Attach at least one .gpx file or turn off inline geotagging before importing.');
      return;
    }
    setRunning(true);
    setLastSummary(null);
    try {
      const summary = await api.runImportPipeline({
        source_root: sourceRoot,
        dest_root: destRoot,
        folder_template: folderTemplate,
        date_format: dateFormat,
        raw_folder: rawFolder,
        jpeg_folder: jpegFolder,
        video_folder: videoFolder,
        assignments: buildAssignments(),
        is_move: transferMode === 'move',
        eject_after: ejectAfter,
        geotag_enabled: geotagEnabled,
        gpx_files: gpxFiles,
        clock_offset_secs: clockOffsetSecs,
        max_gap_secs: settings?.default_max_gps_gap_secs ?? 900,
        overwrite_existing_gps: settings?.overwrite_existing_gps ?? false,
        cloud_profile_ids: selectedCloudIds,
        external_profile_ids: selectedExternalIds,
      });
      setLastSummary(summary);
      const stageErrs = summary.stage_errors ?? 0;
      if (summary.cancelled) {
        notifyInfo(
          `Import cancelled (${summary.import.files_moved} transferred before cancellation).`,
        );
      } else if (summary.import.files_failed === 0 && stageErrs === 0) {
        notifySuccess(
          `Imported ${summary.import.files_moved} files in ${formatDurationMs(summary.import.duration_ms)}`,
        );
      } else if (summary.import.files_failed > 0) {
        notifyError(
          `${summary.import.files_failed} file(s) failed and remain safely on the SD card.`,
        );
      } else {
        notifyError(
          `Imported ${summary.import.files_moved} files, but ${stageErrs} post-import stage error(s) occurred. Review notes below.`,
        );
      }
      await refreshVolumes();
      await refreshJobs();
      if (summary.ejected) {
        setScan(null);
        setSourceRoot('');
        setPreflight(null);
        setPlanPreview(null);
      } else if (transferMode === 'move') {
        await handleScanSource(sourceRoot, {
          preserveSummary: true,
          force: true,
          silentIfMissing: true,
        });
      }
    } catch (err) {
      notifyError(err, 'Import failed');
    } finally {
      setRunning(false);
    }
  };

  const isMultiDaySplit = Boolean(splitByDate && scan && scan.date_groups.length > 1);
  const selectedTotals = useMemo(() => {
    if (!scan) return { count: 0, bytes: 0 };
    if (!isMultiDaySplit) {
      return {
        count: scan.raw_count + scan.jpeg_count + scan.video_count,
        bytes: scan.total_bytes,
      };
    }
    let count = 0;
    let bytes = 0;
    for (const g of scan.date_groups) {
      if (dateAssignments[g.date]?.included) {
        count += g.raw_count + g.jpeg_count + g.video_count;
        bytes += g.total_bytes;
      }
    }
    return { count, bytes };
  }, [scan, isMultiDaySplit, dateAssignments]);
  const totalCount = selectedTotals.count;
  const hasBlockers = (preflight?.blockers.length ?? 0) > 0;

  return (
    <div className="view-content">
      <ViewHeader
        title="SD Card Import"
        subtitle="Fast multithreaded verified move into your universal folder structure, with optional GPX geotagging and post-import cloud/external sync."
        actions={
          <>
            <button type="button" className="button" onClick={() => void refreshVolumes()}>
              <RefreshCw size={14} /> Refresh Drives
            </button>
            <button
              type="button"
              className="button"
              onClick={async () => {
                const picked = await pickDirectory('Choose SD Card or Source Folder');
                if (picked) void handleScanSource(picked);
              }}
            >
              <FolderOpen size={14} /> Browse Folder…
            </button>
          </>
        }
      />

      {/* First-Run Quick Setup */}
      <QuickSetupCard onRootPicked={(dir) => setDestRoot(dir)} />

      {/* Detected Cards / Drives */}
      <SectionTitle>Detected Cards &amp; Removable Volumes</SectionTitle>
      {volumes.length === 0 ? (
        <EmptyState
          icon={<Usb size={28} strokeWidth={1.4} />}
          title="No SD card or external drive detected"
          description="Insert an SD card (TruongSync will auto-open automatically) or click Browse Folder to test with any local directory."
          actions={
            <button
              type="button"
              className="button button-primary"
              onClick={async () => {
                const picked = await pickDirectory('Choose Source Media Folder');
                if (picked) void handleScanSource(picked);
              }}
            >
              <FolderOpen size={14} /> Browse Folder…
            </button>
          }
        />
      ) : (
        <div className="volume-grid">
          {volumes.map((v) => {
            const active = sourceRoot === v.mount_path;
            return (
              <button
                key={v.mount_path}
                type="button"
                className={`volume-card ${active ? 'is-selected' : ''}`}
                onClick={() => void handleScanSource(v.mount_path)}
              >
                <div className="row-between">
                  <strong style={{ fontSize: 13 }}>{v.label}</strong>
                  <div className="row" style={{ gap: 4 }}>
                    {v.has_dcim && <Badge tone="ok">DCIM</Badge>}
                    {!v.writable && <Badge tone="danger">Read-only</Badge>}
                  </div>
                </div>
                <div className="mono muted">{v.mount_path}</div>
                <div className="row-between muted tabular">
                  <span>{v.bus_protocol || 'Volume'}</span>
                  <span>{formatBytes(v.free_bytes)} free</span>
                </div>
              </button>
            );
          })}
        </div>
      )}

      {scanning && (
        <div className="card" style={{ marginTop: 14 }}>
          <div className="card-body row">
            <RefreshCw size={14} />
            <span>Scanning media and reading EXIF capture timestamps…</span>
          </div>
        </div>
      )}

      {scan && !scanning && (
        <>
          {/* Summary + Pre-Flight Checks */}
          <SectionTitle>Source Summary &amp; Pre-Flight Checks</SectionTitle>
          <div className="stack-md">
            <div className="card">
              <div className="card-body row-between">
                <div className="row-wrap">
                  <Badge tone="info">{scan.raw_count} RAW</Badge>
                  <Badge tone="info">{scan.jpeg_count} JPEG</Badge>
                  <Badge tone="info">{scan.video_count} VID</Badge>
                  <Badge>{formatBytes(scan.total_bytes)}</Badge>
                  {scan.date_groups.length > 1 && (
                    <Badge tone="warn">{scan.date_groups.length} shoot dates detected</Badge>
                  )}
                  {scan.ignored_mac_metadata_count > 0 && (
                    <Badge>{scan.ignored_mac_metadata_count} macOS metadata skipped</Badge>
                  )}
                </div>
                <span className="mono muted">{scan.source_root}</span>
              </div>
            </div>

            {preflight && (
              <div
                className={`preflight-strip ${
                  preflight.blockers.length > 0
                    ? 'has-blockers'
                    : preflight.warnings.length > 0
                      ? 'has-warnings'
                      : ''
                }`}
              >
                {preflight.blockers.length > 0 ? (
                  <ShieldAlert size={16} color="var(--danger-text)" />
                ) : preflight.warnings.length > 0 ? (
                  <AlertTriangle size={16} color="var(--warn-text)" />
                ) : (
                  <CheckCircle2 size={16} color="var(--ok)" />
                )}
                <Badge tone={preflight.space_ok ? 'ok' : 'danger'}>
                  Disk Space: {preflight.dest_free_bytes != null ? `${formatBytes(preflight.dest_free_bytes)} free` : 'Set destination'}
                </Badge>
                <Badge tone={preflight.source_readable ? 'ok' : 'danger'}>
                  Card: {preflight.source_readable ? 'Readable' : 'Unreadable'}
                </Badge>
                <Badge tone={preflight.source_writable || transferMode === 'copy' ? 'ok' : 'danger'}>
                  {preflight.source_writable ? 'Writable' : 'Write-Protected'}
                </Badge>
                <Badge tone={preflight.zero_byte_files.length === 0 ? 'ok' : 'warn'}>
                  Health: {preflight.zero_byte_files.length === 0 ? '0 corrupt/0-byte' : `${preflight.zero_byte_files.length} 0-byte`}
                </Badge>
                {preflight.blockers.map((b, i) => (
                  <div key={i} style={{ flexBasis: '100%', color: 'var(--danger-text)' }}>
                    {b}
                  </div>
                ))}
                {preflight.warnings.map((w, i) => (
                  <div key={i} style={{ flexBasis: '100%', color: 'var(--warn-text)' }}>
                    {w}
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* Destination & Naming */}
          <SectionTitle>Destination &amp; Folder Structure</SectionTitle>
          <div className="card">
            <div className="card-body stack-md">
              <div className="grid-2">
                <div className="settings-field" style={{ marginBottom: 0 }}>
                  <span>Library Root Folder</span>
                  <div className="row">
                    <input
                      className="text-input mono"
                      value={destRoot}
                      placeholder="/Volumes/Photos/Photo Dump  or  F:\All Photos\Photo Dump"
                      onChange={(e) => setDestRoot(e.target.value)}
                    />
                    <button
                      type="button"
                      className="button"
                      onClick={async () => {
                        const d = await pickDirectory('Choose Destination Library Root');
                        if (d) {
                          setDestRoot(d);
                          if (settings && !settings.default_import_root.trim()) {
                            void api
                              .saveSettings({ ...settings, default_import_root: d })
                              .then(() => Promise.all([refreshSettings(), refreshProfiles()]))
                              .catch(() => {});
                          }
                        }
                      }}
                    >
                      Browse
                    </button>
                  </div>
                </div>
                <div className="settings-field" style={{ marginBottom: 0 }}>
                  <span>Folder Template</span>
                  <input
                    className="text-input mono"
                    value={folderTemplate}
                    onChange={(e) => setFolderTemplate(e.target.value)}
                  />
                </div>
              </div>

              {scan.date_groups.length > 1 && (
                <ToggleRow
                  label="Split multi-day card into separate shoot folders"
                  description={`Detected ${scan.date_groups.length} capture dates on this card. Assign a separate shoot name and category per day.`}
                  checked={splitByDate}
                  onChange={setSplitByDate}
                />
              )}

              {!splitByDate || scan.date_groups.length <= 1 ? (
                <div className="grid-3">
                  <div className="settings-field" style={{ marginBottom: 0 }}>
                    <span>Shoot Name</span>
                    <input
                      className="text-input"
                      placeholder="e.g., Cone Peak"
                      value={shootName}
                      onChange={(e) => setShootName(e.target.value)}
                    />
                  </div>
                  <div className="settings-field" style={{ marginBottom: 0 }}>
                    <span>Category</span>
                    <div className="row">
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <Dropdown
                          label="Category"
                          value={category}
                          options={categories.map((c) => [c, c] as const)}
                          onChange={setCategory}
                        />
                      </div>
                      <button
                        type="button"
                        className="icon-button"
                        title="Create new category"
                        onClick={() => setNewCatOpen((v) => !v)}
                      >
                        <Plus size={15} />
                      </button>
                    </div>
                  </div>
                  <div className="settings-field" style={{ marginBottom: 0 }}>
                    <span>Shoot Date ({dateFormat})</span>
                    <input
                      type="date"
                      className="text-input"
                      value={folderDate}
                      onChange={(e) => setFolderDate(e.target.value)}
                    />
                  </div>
                </div>
              ) : (
                <div className="stack">
                  <div className="row-between muted">
                    <div className="row">
                      <Split size={14} />
                      <span>Assign each shoot date to its own destination folder:</span>
                    </div>
                    <button
                      type="button"
                      className="button button-quiet"
                      onClick={() => setNewCatOpen((v) => !v)}
                    >
                      <Plus size={13} /> New Category
                    </button>
                  </div>
                  {scan.date_groups.map((g) => {
                    const row = dateAssignments[g.date] ?? {
                      shoot_name: 'Shoot',
                      category,
                      folder_date: g.date,
                      included: true,
                    };
                    return (
                      <div key={g.date} className="panel" style={{ padding: 10 }}>
                        <div className="row-between" style={{ marginBottom: 8 }}>
                          <label className="row" style={{ cursor: 'pointer' }}>
                            <input
                              type="checkbox"
                              checked={row.included}
                              onChange={(e) =>
                                setDateAssignments((prev) => ({
                                  ...prev,
                                  [g.date]: { ...row, included: e.target.checked },
                                }))
                              }
                            />
                            <strong className="mono">{g.date}</strong>
                          </label>
                          <div className="row" style={{ gap: 6 }}>
                            <Badge>{g.raw_count} RAW</Badge>
                            <Badge>{g.jpeg_count} JPG</Badge>
                            <Badge>{g.video_count} VID</Badge>
                            <Badge>{formatBytes(g.total_bytes)}</Badge>
                          </div>
                        </div>
                        {row.included && (
                          <div className="grid-2">
                            <input
                              className="text-input"
                              placeholder="Shoot name for this date"
                              value={row.shoot_name}
                              onChange={(e) =>
                                setDateAssignments((prev) => ({
                                  ...prev,
                                  [g.date]: { ...row, shoot_name: e.target.value },
                                }))
                              }
                            />
                            <Dropdown
                              label={`Category for ${g.date}`}
                              value={row.category}
                              options={categories.map((c) => [c, c] as const)}
                              onChange={(val) =>
                                setDateAssignments((prev) => ({
                                  ...prev,
                                  [g.date]: { ...row, category: val },
                                }))
                              }
                            />
                          </div>
                        )}
                      </div>
                    );
                  })}
                </div>
              )}

              {newCatOpen && (
                <div className="row">
                  <input
                    className="text-input"
                    placeholder="New category name (e.g., Concerts)"
                    value={newCatInput}
                    onChange={(e) => setNewCatInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') void handleCreateCategory();
                    }}
                  />
                  <button type="button" className="button button-primary" onClick={() => void handleCreateCategory()}>
                    Create
                  </button>
                  <button type="button" className="icon-button" onClick={() => setNewCatOpen(false)}>
                    <X size={14} />
                  </button>
                </div>
              )}

              <div className="path-preview-box">
                {resolvedPathPreviews.map((line, idx) => (
                  <div key={idx}>{line}</div>
                ))}
              </div>
            </div>
          </div>

          {/* Pipeline Options */}
          <SectionTitle>Pipeline Steps</SectionTitle>
          <div className="card">
            <div className="card-body stack-md">
              <div className="row-between">
                <div>
                  <div style={{ fontWeight: 500 }}>Transfer Mode</div>
                  <div className="muted">
                    Move verifies each written file before deleting from the SD card.
                  </div>
                </div>
                <SegmentedControl
                  value={transferMode}
                  options={[
                    ['move', 'Move (Clear SD)'],
                    ['copy', 'Copy (Keep SD)'],
                  ]}
                  onChange={setTransferMode}
                />
              </div>

              <ToggleRow
                label="Geotag photos with GPX track before syncing (RAW + JPEG only)"
                description="Embeds GPS coordinates directly into photo EXIF; videos and out-of-range frames are skipped."
                checked={geotagEnabled}
                onChange={setGeotagEnabled}
              />

              {geotagEnabled && (
                <div className="panel" style={{ padding: 12 }}>
                  <div className="row-between" style={{ marginBottom: 8 }}>
                    <span className="row">
                      <MapPin size={14} />
                      <strong>GPX Tracks ({gpxFiles.length} attached)</strong>
                    </span>
                    <button
                      type="button"
                      className="button"
                      onClick={async () => {
                        const files = await pickFiles('Select GPX Track(s)', ['gpx'], true);
                        if (files.length > 0) {
                          setGpxFiles((prev) => Array.from(new Set([...prev, ...files])));
                        }
                      }}
                    >
                      Attach .gpx…
                    </button>
                  </div>
                  {gpxSuggestions.length > 0 && (
                    <div className="row-wrap" style={{ marginBottom: 8 }}>
                      <span className="muted">Auto-matched from watched GPX folder:</span>
                      {gpxSuggestions.map((m) => {
                        const attached = gpxFiles.includes(m.path);
                        return (
                          <button
                            key={m.path}
                            type="button"
                            className={`chip-button ${attached ? 'is-selected' : ''}`}
                            onClick={() =>
                              setGpxFiles((prev) =>
                                attached ? prev.filter((p) => p !== m.path) : [...prev, m.path],
                              )
                            }
                          >
                            <MapPin size={12} /> {m.filename} ({m.point_count} pts)
                          </button>
                        );
                      })}
                    </div>
                  )}
                  {gpxFiles.length > 0 && (
                    <div className="row-wrap" style={{ marginBottom: 8 }}>
                      {gpxFiles.map((f) => (
                        <Badge key={f} tone="ok">
                          {f.split(/[\\/]/).pop()}
                          <button
                            type="button"
                            style={{ background: 'none', border: 0, color: 'inherit', padding: 0, marginLeft: 4 }}
                            onClick={() => setGpxFiles((prev) => prev.filter((x) => x !== f))}
                          >
                            ×
                          </button>
                        </Badge>
                      ))}
                    </div>
                  )}
                  <div className="row">
                    <span className="muted">Camera clock offset (seconds):</span>
                    <input
                      type="text"
                      inputMode="numeric"
                      className="text-input"
                      style={{ width: 110 }}
                      value={clockOffsetText}
                      onChange={(e) => setClockOffsetText(e.target.value)}
                    />
                  </div>
                </div>
              )}

              {cloudProfiles.length > 0 && (
                <div>
                  <div style={{ fontWeight: 500, marginBottom: 6 }}>
                    Auto-sync to Google Drive after successful import
                  </div>
                  <div className="row-wrap">
                    {cloudProfiles.map((cp) => {
                      const on = selectedCloudIds.includes(cp.id);
                      return (
                        <button
                          key={cp.id}
                          type="button"
                          className={`chip-button ${on ? 'is-selected' : ''}`}
                          onClick={() =>
                            setSelectedCloudIds((prev) =>
                              on ? prev.filter((id) => id !== cp.id) : [...prev, cp.id],
                            )
                          }
                        >
                          {cp.name}
                        </button>
                      );
                    })}
                  </div>
                </div>
              )}

              {externalProfiles.length > 0 && (
                <div>
                  <div style={{ fontWeight: 500, marginBottom: 6 }}>
                    Auto-sync to External Drive (if connected)
                  </div>
                  <div className="row-wrap">
                    {externalProfiles.map((ep) => {
                      const on = selectedExternalIds.includes(ep.id);
                      return (
                        <button
                          key={ep.id}
                          type="button"
                          className={`chip-button ${on ? 'is-selected' : ''}`}
                          onClick={() =>
                            setSelectedExternalIds((prev) =>
                              on ? prev.filter((id) => id !== ep.id) : [...prev, ep.id],
                            )
                          }
                        >
                          {ep.name} ({ep.volume_label})
                        </button>
                      );
                    })}
                  </div>
                </div>
              )}

              <ToggleRow
                label="Safely eject SD card after verified import"
                checked={ejectAfter}
                onChange={setEjectAfter}
              />
            </div>
          </div>

          {/* Live Progress Bar */}
          {running && importProgress && (
            <div className="card" style={{ marginTop: 14 }}>
              <div className="card-body stack">
                <div className="row-between">
                  <strong>{importProgress.message}</strong>
                  <span className="mono muted tabular">
                    {importProgress.speed_bps > 0 ? `${formatBytes(importProgress.speed_bps)}/s` : importProgress.stage}
                  </span>
                </div>
                <div className="upload-progress-bar">
                  <div
                    style={{
                      width: `${
                        importProgress.files_total > 0
                          ? Math.min(100, (importProgress.files_done / importProgress.files_total) * 100)
                          : 15
                      }%`,
                    }}
                  />
                </div>
                <div className="row-between">
                  <span className="muted tabular">
                    {importProgress.files_done} / {importProgress.files_total} files
                  </span>
                  <button
                    type="button"
                    className="button button-danger"
                    onClick={() => void api.cancelActiveJob(importProgress.job_id)}
                  >
                    Cancel
                  </button>
                </div>
              </div>
            </div>
          )}

          {/* Action Bar */}
          <div className="row-between" style={{ marginTop: 18 }}>
            <button type="button" className="button" disabled={running || totalCount === 0} onClick={() => void handlePreviewPlan()}>
              Preview Plan ({totalCount} files)
            </button>
            <button
              type="button"
              className="button button-primary"
              disabled={running || totalCount === 0 || hasBlockers}
              onClick={() => void handleRunPipeline()}
            >
              <HardDriveDownload size={14} />
              {running
                ? 'Importing…'
                : `${transferMode === 'move' ? 'Import & Move' : 'Copy'} ${totalCount} Files (${formatBytes(selectedTotals.bytes)})`}
            </button>
          </div>

          {/* Dry-Run Preview Table */}
          {planPreview && (
            <div style={{ marginTop: 16 }}>
              <div className="row-between">
                <SectionTitle>Dry-Run Plan ({planPreview.length} files)</SectionTitle>
                {planPreview.length > 200 && (
                  <span className="muted">Showing 200 of {planPreview.length} files</span>
                )}
              </div>
              <div className="data-table-wrap">
                <table className="data-table">
                  <thead>
                    <tr>
                      <th>Type</th>
                      <th>Date</th>
                      <th>Destination Path</th>
                      <th>Size</th>
                    </tr>
                  </thead>
                  <tbody>
                    {planPreview.slice(0, 200).map((item) => (
                      <tr key={item.source_path}>
                        <td>
                          <Badge tone="info">{item.kind.toUpperCase()}</Badge>
                        </td>
                        <td className="mono">{item.captured_date}</td>
                        <td className="mono">{item.dest_path}</td>
                        <td className="mono tabular">{formatBytes(item.size_bytes)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          )}
        </>
      )}

      {/* Last Run Summary (rendered outside scan block so it stays visible even after auto-eject clears the card) */}
      {lastSummary && (
        <div
          className={
            lastSummary.import.files_failed > 0 || (lastSummary.stage_errors ?? 0) > 0
              ? 'error-banner'
              : lastSummary.cancelled
                ? 'offline-notice'
                : 'ok-banner'
          }
          style={{ marginTop: 14 }}
        >
          <div>
            <strong>
              {lastSummary.cancelled ? 'Import cancelled' : 'Import finished'}:{' '}
              {lastSummary.import.files_moved} transferred, {lastSummary.import.files_skipped}{' '}
              skipped, {lastSummary.import.files_failed} failed (
              {formatBytes(lastSummary.import.bytes_transferred)} in{' '}
              {formatDurationMs(lastSummary.import.duration_ms)})
            </strong>
            {lastSummary.notes.map((n, i) => (
              <div key={i} style={{ marginTop: 4 }}>
                • {n}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
};
