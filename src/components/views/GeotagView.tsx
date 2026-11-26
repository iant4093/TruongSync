import React, { useEffect, useMemo, useRef, useState } from 'react';
import { FolderOpen, MapPin, Play, Sparkles } from 'lucide-react';
import { api, pickDirectory, pickFiles } from '../../lib/tauri';
import { confirmAction, notifyError, notifySuccess } from '../../store/feedback';
import { useSyncStore } from '../../store/useSyncStore';
import type { GeotagExecutionResult, GeotagPlan, WatchedGpxMatch } from '../../types/api';
import { GpxMapPreview } from '../GpxMapPreview';
import { Badge, EmptyState, SectionTitle, SegmentedControl, ToggleRow, ViewHeader } from '../ui';

export const GeotagView: React.FC = () => {
  const settings = useSyncStore((s) => s.settings);
  const tools = useSyncStore((s) => s.tools);
  const refreshJobs = useSyncStore((s) => s.refreshJobs);

  const [targetFolder, setTargetFolder] = useState('');
  const [gpxFiles, setGpxFiles] = useState<string[]>([]);
  const [suggestions, setSuggestions] = useState<WatchedGpxMatch[]>([]);
  const [clockOffsetText, setClockOffsetText] = useState('0');
  const [maxGapText, setMaxGapText] = useState(
    String(Math.max(1, Math.round((settings?.default_max_gps_gap_secs ?? 900) / 60))),
  );
  const userEditedGapRef = useRef(false);
  const folderScanSeqRef = useRef(0);
  const [overwriteExisting, setOverwriteExisting] = useState(false);
  const [recordBackfill, setRecordBackfill] = useState(true);

  const clockOffsetSecs = Number.isFinite(Number(clockOffsetText))
    ? Math.trunc(Number(clockOffsetText))
    : 0;
  const maxGapMins = Math.max(1, Math.trunc(Number(maxGapText)) || 15);

  const [plan, setPlan] = useState<GeotagPlan | null>(null);
  const [previewSignature, setPreviewSignature] = useState<string | null>(null);
  const [filter, setFilter] = useState<'all' | 'matched' | 'out_of_range' | 'skipped_video'>('all');
  const [loading, setLoading] = useState(false);
  const [writing, setWriting] = useState(false);
  const [result, setResult] = useState<GeotagExecutionResult | null>(null);

  useEffect(() => {
    if (settings && !userEditedGapRef.current) {
      setMaxGapText(String(Math.max(1, Math.round(settings.default_max_gps_gap_secs / 60))));
    }
  }, [settings?.default_max_gps_gap_secs]);

  const currentSignature = useMemo(
    () =>
      JSON.stringify({
        targetFolder: targetFolder.trim(),
        gpxFiles,
        clockOffsetSecs,
        maxGapMins,
        overwriteExisting,
      }),
    [targetFolder, gpxFiles, clockOffsetSecs, maxGapMins, overwriteExisting],
  );
  const isPlanStale = Boolean(plan && previewSignature !== currentSignature);

  const pickTargetFolder = async () => {
    const dir = await pickDirectory('Select Shoot Folder to Geotag');
    if (!dir) return;
    const seq = ++folderScanSeqRef.current;
    setTargetFolder(dir);
    setPlan(null);
    setResult(null);
    setSuggestions([]);
    try {
      const scan = await api.scanCard(dir);
      if (folderScanSeqRef.current !== seq) return;
      if (scan.earliest_utc && scan.latest_utc) {
        const matches = await api.findMatchingGpx(scan.earliest_utc, scan.latest_utc);
        if (folderScanSeqRef.current === seq) {
          setSuggestions(matches);
        }
      }
    } catch {
      if (folderScanSeqRef.current === seq) setSuggestions([]);
    }
  };

  const handlePreview = async () => {
    if (!targetFolder || gpxFiles.length === 0) {
      notifyError('Choose both a target shoot folder and at least one .gpx track.');
      return;
    }
    const sig = currentSignature;
    setLoading(true);
    setResult(null);
    try {
      const next = await api.previewGeotag({
        targetPaths: [targetFolder],
        gpxFiles,
        clockOffsetSecs,
        maxGapSecs: Math.max(60, maxGapMins * 60),
        overwriteExisting,
      });
      setPlan(next);
      setPreviewSignature(sig);
    } catch (err) {
      notifyError(err, 'GPX preview failed');
    } finally {
      setLoading(false);
    }
  };

  const handleWrite = async () => {
    if (!plan || plan.matched_count === 0) return;
    if (isPlanStale) {
      notifyError('Geotag settings changed since the last preview. Run Match & Preview again before writing.');
      return;
    }
    const ok = await confirmAction(
      `Write GPS EXIF to ${plan.matched_count} photo(s)?`,
      `Write GPS to ${plan.matched_count} Photos`,
      `Applies to RAW and JPEG photos only (${plan.skipped_video_count} videos and ${plan.out_of_range_count} out-of-range photos will not be touched).`,
    );
    if (!ok) return;
    setWriting(true);
    try {
      const res = await api.runGeotag(plan.items, recordBackfill);
      setResult(res);
      if (res.failed_count > 0) {
        notifyError(
          `Geotagging finished with ${res.failed_count} failure(s) (${res.tagged_count} photo(s) tagged).`,
        );
      } else {
        notifySuccess(`Geotagged ${res.tagged_count} photo(s) cleanly.`);
      }
      await refreshJobs();
    } catch (err) {
      notifyError(err, 'Geotagging failed');
    } finally {
      setWriting(false);
    }
  };

  const visibleItems = (plan?.items ?? []).filter((item) => {
    if (filter === 'all') return true;
    return item.status === filter;
  });

  return (
    <div className="view-content">
      <ViewHeader
        title="EXIF GPS Geotagging"
        subtitle="Embed GPS coordinates from universal .gpx files directly into RAW & JPEG EXIF. Videos and out-of-range frames are strictly skipped."
        actions={
          <Badge tone={tools?.exiftool.available ? 'ok' : 'danger'}>
            ExifTool:{' '}
            {tools?.exiftool.available
              ? `${tools.exiftool.bundled ? 'Bundled · ' : ''}v${tools.exiftool.version}`
              : 'Not found'}
          </Badge>
        }
      />

      {!tools?.exiftool.available && (
        <div className="error-banner">
          ExifTool was not detected. Install it (e.g. <code>brew install exiftool</code> on macOS) or configure its path in Settings to write GPS into RAW &amp; JPEG EXIF.
        </div>
      )}

      <div className="card">
        <div className="card-body stack-md">
          <div className="grid-2">
            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span>Target Shoot Folder (Backfill or New)</span>
              <div className="row">
                <input
                  className="text-input mono"
                  value={targetFolder}
                  placeholder="Choose any existing shoot folder…"
                  onChange={(e) => {
                    setTargetFolder(e.target.value);
                    setSuggestions([]);
                  }}
                />
                <button type="button" className="button" onClick={() => void pickTargetFolder()}>
                  <FolderOpen size={14} /> Browse
                </button>
              </div>
            </div>

            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span>Universal .gpx Track File(s)</span>
              <div className="row">
                <button
                  type="button"
                  className="button"
                  style={{ flex: 1 }}
                  onClick={async () => {
                    const files = await pickFiles('Select GPX File(s)', ['gpx'], true);
                    if (files.length > 0) {
                      setGpxFiles((prev) => Array.from(new Set([...prev, ...files])));
                    }
                  }}
                >
                  <MapPin size={14} /> {gpxFiles.length > 0 ? `${gpxFiles.length} GPX selected` : 'Select .gpx files…'}
                </button>
                {gpxFiles.length > 0 && (
                  <button type="button" className="button button-quiet" onClick={() => setGpxFiles([])}>
                    Clear
                  </button>
                )}
              </div>
            </div>
          </div>

          {suggestions.length > 0 && (
            <div className="row-wrap">
              <span className="muted row">
                <Sparkles size={13} /> Matched from watched GPX folder:
              </span>
              {suggestions.map((m) => {
                const selected = gpxFiles.includes(m.path);
                return (
                  <button
                    key={m.path}
                    type="button"
                    className={`chip-button ${selected ? 'is-selected' : ''}`}
                    onClick={() =>
                      setGpxFiles((prev) =>
                        selected ? prev.filter((x) => x !== m.path) : [...prev, m.path],
                      )
                    }
                  >
                    {m.filename} ({m.point_count} pts)
                  </button>
                );
              })}
            </div>
          )}

          {gpxFiles.length > 0 && (
            <div className="row-wrap">
              {gpxFiles.map((f) => (
                <Badge key={f} tone="ok">
                  {f.split(/[\\/]/).pop()}
                  <button
                    type="button"
                    aria-label={`Remove ${f.split(/[\\/]/).pop()}`}
                    style={{ background: 'none', border: 0, color: 'inherit', padding: 0, marginLeft: 4 }}
                    onClick={() => setGpxFiles((prev) => prev.filter((x) => x !== f))}
                  >
                    ×
                  </button>
                </Badge>
              ))}
            </div>
          )}

          <div className="grid-2">
            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span>Camera Clock Offset (seconds, +/-)</span>
              <input
                type="text"
                inputMode="numeric"
                className="text-input"
                style={{ width: 120 }}
                value={clockOffsetText}
                onChange={(e) => setClockOffsetText(e.target.value)}
              />
            </div>
            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span>Max Interpolation Gap (minutes)</span>
              <input
                type="text"
                inputMode="numeric"
                className="text-input"
                style={{ width: 120 }}
                value={maxGapText}
                onChange={(e) => {
                  userEditedGapRef.current = true;
                  setMaxGapText(e.target.value);
                }}
              />
            </div>
          </div>

          <ToggleRow
            label="Track for safe Google Drive & External Drive backfill update"
            description="Records pre-tag & post-tag MD5 so the next sync safely replaces un-geotagged backups without tripping --immutable conflicts."
            checked={recordBackfill}
            onChange={setRecordBackfill}
          />
          <ToggleRow
            label="Overwrite existing GPS coordinates"
            description="Off by default so already-geotagged photos are preserved."
            checked={overwriteExisting}
            onChange={setOverwriteExisting}
          />

          <div className="row-between">
            <Badge>Photos only (RAW + JPEG) · Videos &amp; out-of-range skipped</Badge>
            <button
              type="button"
              className="button button-primary"
              disabled={loading || !targetFolder || gpxFiles.length === 0}
              onClick={() => void handlePreview()}
            >
              <Play size={14} /> {loading ? 'Matching…' : 'Match & Preview on Map'}
            </button>
          </div>
        </div>
      </div>

      {result && (
        <div className={result.failed_count > 0 ? 'error-banner' : 'ok-banner'} style={{ marginTop: 14 }}>
          <strong>
            Geotagging complete: {result.tagged_count} photos tagged, {result.skipped_count} skipped,{' '}
            {result.failed_count} failed.
          </strong>
          {recordBackfill && result.tagged_count > 0 && (
            <div style={{ marginTop: 4 }}>
              Safe backfill hashes saved — run Google Drive or External Drive sync whenever ready to propagate the updated GPS EXIF.
            </div>
          )}
        </div>
      )}

      {plan ? (
        <>
          <SectionTitle>Track &amp; Photo Preview</SectionTitle>
          {isPlanStale && (
            <div className="offline-notice">
              <span>
                Target folder, GPX files, clock offset, or matching options changed since this preview. Click{' '}
                <strong>Match &amp; Preview on Map</strong> to refresh before writing EXIF.
              </span>
            </div>
          )}
          <GpxMapPreview plan={plan} />

          <div className="row-between" style={{ marginTop: 16, marginBottom: 10 }}>
            <SegmentedControl
              value={filter}
              options={[
                ['all', `All (${plan.items.length})`],
                ['matched', `Matched (${plan.matched_count})`],
                ['out_of_range', `Out of Range (${plan.out_of_range_count})`],
                ['skipped_video', `Videos Skipped (${plan.skipped_video_count})`],
              ]}
              onChange={setFilter}
            />
            <button
              type="button"
              className="button button-primary"
              disabled={writing || isPlanStale || plan.matched_count === 0 || !tools?.exiftool.available}
              onClick={() => void handleWrite()}
            >
              <MapPin size={14} />
              {writing ? 'Writing EXIF…' : `Write GPS to ${plan.matched_count} Photos`}
            </button>
          </div>

          {visibleItems.length > 300 && (
            <div className="muted" style={{ marginBottom: 6 }}>
              Showing 300 of {visibleItems.length} files
            </div>
          )}

          <div className="data-table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>File</th>
                  <th>Type</th>
                  <th>Status</th>
                  <th>Coordinates</th>
                  <th>Elevation</th>
                </tr>
              </thead>
              <tbody>
                {visibleItems.slice(0, 300).map((item) => (
                  <tr key={item.path}>
                    <td className="mono">{item.filename}</td>
                    <td>
                      <Badge>{item.media_kind.toUpperCase()}</Badge>
                    </td>
                    <td>
                      <Badge
                        tone={
                          item.status === 'matched'
                            ? 'ok'
                            : item.status === 'out_of_range'
                              ? 'warn'
                              : 'muted'
                        }
                      >
                        {item.status.replace(/_/g, ' ')}
                      </Badge>
                    </td>
                    <td className="mono tabular">
                      {item.lat != null && item.lon != null
                        ? `${item.lat.toFixed(5)}, ${item.lon.toFixed(5)}`
                        : '—'}
                    </td>
                    <td className="mono tabular">
                      {item.ele != null ? `${item.ele.toFixed(0)} m` : '—'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      ) : (
        <div style={{ marginTop: 16 }}>
          <EmptyState
            icon={<MapPin size={26} strokeWidth={1.4} />}
            title="Select a shoot folder and .gpx track to preview matches"
            description="Every matched RAW and JPEG pair will be plotted on the map before a single byte of EXIF is written."
          />
        </div>
      )}
    </div>
  );
};
