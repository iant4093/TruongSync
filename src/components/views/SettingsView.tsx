import React, { useEffect, useRef, useState } from 'react';
import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from '@tauri-apps/plugin-autostart';
import { Download, ExternalLink, FolderOpen, RefreshCw, Save } from 'lucide-react';
import { api, formatBytes, isTauri, openExternalUrl, pickDirectory } from '../../lib/tauri';
import { notifyError, notifySuccess } from '../../store/feedback';
import { useSyncStore } from '../../store/useSyncStore';
import type { AppSettings } from '../../types/api';
import { Badge, SectionTitle, ToggleRow, ViewHeader } from '../ui';

const RELEASES_URL = 'https://github.com/iant4093/TruongSync/releases';

export const SettingsView: React.FC = () => {
  const settings = useSyncStore((s) => s.settings);
  const bootstrap = useSyncStore((s) => s.bootstrap);
  const tools = useSyncStore((s) => s.tools);
  const refreshSettings = useSyncStore((s) => s.refreshSettings);
  const refreshTools = useSyncStore((s) => s.refreshTools);
  const refreshProfiles = useSyncStore((s) => s.refreshProfiles);
  const checkingUpdates = useSyncStore((s) => s.checkingUpdates);
  const updateStatus = useSyncStore((s) => s.updateStatus);
  const installingUpdate = useSyncStore((s) => s.installingUpdate);
  const updateProgress = useSyncStore((s) => s.updateProgress);
  const checkForUpdates = useSyncStore((s) => s.checkForUpdates);
  const installUpdate = useSyncStore((s) => s.installUpdate);

  const [draft, setDraft] = useState<AppSettings | null>(settings);
  const [categoriesText, setCategoriesText] = useState('');
  const [bandwidthText, setBandwidthText] = useState('');
  const [threadsText, setThreadsText] = useState('8');
  const [staleDaysText, setStaleDaysText] = useState('7');
  const [autostart, setAutostart] = useState(false);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [osAutoLaunchBusy, setOsAutoLaunchBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const dirtyRef = useRef(false);

  const updateDraft = (patch: Partial<AppSettings>) => {
    dirtyRef.current = true;
    setDraft((prev) => (prev ? { ...prev, ...patch } : prev));
  };

  useEffect(() => {
    if (!settings) return;
    if (!dirtyRef.current || !draft) {
      setDraft(settings);
      setCategoriesText(settings.default_categories.join(', '));
      setBandwidthText(settings.bandwidth_limit_mbps != null ? String(settings.bandwidth_limit_mbps) : '');
      setThreadsText(String(settings.import_threads));
      setStaleDaysText(String(settings.stale_warning_days));
    }
  }, [settings]);

  useEffect(() => {
    if (!isTauri()) return;
    void isAutostartEnabled().then(setAutostart).catch(() => {});
  }, []);

  if (!draft) {
    return <div className="settings-view muted">Loading settings…</div>;
  }

  const handleSave = async () => {
    if (!draft.folder_template.trim()) {
      notifyError('Folder template cannot be empty.');
      return;
    }
    if (!draft.date_format.trim()) {
      notifyError('Date format cannot be empty.');
      return;
    }
    if (!draft.raw_folder_name.trim() || !draft.jpeg_folder_name.trim() || !draft.video_folder_name.trim()) {
      notifyError('RAW, JPEG, and Video subfolder names cannot be empty.');
      return;
    }

    let bw: number | null = null;
    if (bandwidthText.trim() !== '') {
      const parsed = Number(bandwidthText.trim());
      if (!Number.isFinite(parsed) || parsed < 0) {
        notifyError('Bandwidth limit must be a non-negative number (or empty for unlimited).');
        return;
      }
      bw = parsed === 0 ? null : parsed;
    }

    const threads = Number(threadsText.trim());
    if (!Number.isInteger(threads) || threads < 1 || threads > 16) {
      notifyError('Parallel import threads must be a whole number between 1 and 16.');
      return;
    }

    const staleDays = Number(staleDaysText.trim());
    if (!Number.isInteger(staleDays) || staleDays < 1) {
      notifyError('Freshness stale warning days must be a whole number of at least 1.');
      return;
    }

    setSaving(true);
    try {
      const next: AppSettings = {
        ...draft,
        folder_template: draft.folder_template.trim(),
        date_format: draft.date_format.trim(),
        raw_folder_name: draft.raw_folder_name.trim(),
        jpeg_folder_name: draft.jpeg_folder_name.trim(),
        video_folder_name: draft.video_folder_name.trim(),
        bandwidth_limit_mbps: bw,
        import_threads: threads,
        stale_warning_days: staleDays,
        watched_gpx_folder: draft.watched_gpx_folder?.trim() || null,
        exiftool_path_override: draft.exiftool_path_override?.trim() || null,
        rclone_path_override: draft.rclone_path_override?.trim() || null,
        default_categories: categoriesText
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean),
      };
      await api.saveSettings(next);
      dirtyRef.current = false;
      await refreshSettings();
      await refreshProfiles();
      await refreshTools();
      notifySuccess('Settings saved.');
    } catch (err) {
      notifyError(err, 'Failed to save settings');
    } finally {
      setSaving(false);
    }
  };

  const handleAutostartToggle = async (next: boolean) => {
    if (autostartBusy) return;
    setAutostartBusy(true);
    try {
      if (next) await enableAutostart();
      else await disableAutostart();
      setAutostart(next);
      notifySuccess(next ? 'TruongSync will start in the tray at login.' : 'Launch at login disabled.');
    } catch (err) {
      notifyError(err, 'Could not update launch at login');
    } finally {
      setAutostartBusy(false);
    }
  };

  const handleOsNativeAutoLaunch = async (next: boolean) => {
    if (osAutoLaunchBusy) return;
    setOsAutoLaunchBusy(true);
    try {
      const msg = await api.setOsNativeAutoLaunch(next);
      setDraft((prev) => (prev ? { ...prev, os_native_auto_launch: next } : prev));
      await refreshSettings();
      notifySuccess(msg);
    } catch (err) {
      notifyError(err, 'Could not toggle OS-native auto-launch');
    } finally {
      setOsAutoLaunchBusy(false);
    }
  };

  return (
    <div className="settings-view">
      <ViewHeader
        title="Settings"
        subtitle={`TruongSync v${bootstrap?.version ?? '0.1.0'} · ${bootstrap?.platform ?? 'desktop'} · Data stored in ${bootstrap?.data_dir ?? ''}`}
        actions={
          <button type="button" className="button button-primary" disabled={saving} onClick={() => void handleSave()}>
            <Save size={14} /> {saving ? 'Saving…' : 'Save Changes'}
          </button>
        }
      />

      {/* Desktop, Tray & Auto-Open */}
      <SectionTitle>Desktop, Tray &amp; Auto-Open on SD Insert</SectionTitle>
      <div className="card">
        <div className="card-body">
          <ToggleRow
            label="Launch TruongSync in menu bar / tray at login"
            description="Keeps background volume detection and scheduled syncs running quietly with minimal idle footprint."
            checked={autostart}
            onChange={(v) => void handleAutostartToggle(v)}
          />
          <ToggleRow
            label="Auto-open Import tab when an SD card (DCIM/) is inserted"
            description="Listens for removable camera cards and ignores unrelated USB drives."
            checked={draft.auto_open_on_sd_insert}
            onChange={(v) => updateDraft({ auto_open_on_sd_insert: v })}
          />
          <ToggleRow
            label="Prompt when a registered External Backup Drive connects"
            description="Shows an instant notification when a known backup drive (matched by Volume UUID) is plugged in."
            checked={draft.prompt_on_backup_drive}
            onChange={(v) => updateDraft({ prompt_on_backup_drive: v })}
          />
          <ToggleRow
            label="OS-Native Auto-Open Even When Quit (One-Click)"
            description={
              bootstrap?.platform === 'macos'
                ? 'Installs a user launchd LaunchAgent watching /Volumes that checks for new SD cards in milliseconds.'
                : 'Registers a per-user HKCU Windows AutoPlay handler for memory cards (no admin required).'
            }
            checked={draft.os_native_auto_launch}
            onChange={(v) => void handleOsNativeAutoLaunch(v)}
          />
        </div>
      </div>

      {/* Bandwidth & Power */}
      <SectionTitle>Bandwidth, Sleep &amp; Battery Controls</SectionTitle>
      <div className="card">
        <div className="card-body">
          <div className="settings-field">
            <span>Google Drive Bandwidth Limit (MB/s — leave empty for Unlimited)</span>
            <input
              type="number"
              className="text-input"
              placeholder="Unlimited"
              min={0}
              step={1}
              value={bandwidthText}
              onChange={(e) => {
                dirtyRef.current = true;
                setBandwidthText(e.target.value);
              }}
            />
          </div>
          <ToggleRow
            label="Prevent computer from sleeping during active transfers"
            description="Holds an OS sleep assertion while SD moves, Google Drive uploads, or external backups are running."
            checked={draft.prevent_sleep_during_transfers}
            onChange={(v) => updateDraft({ prevent_sleep_during_transfers: v })}
          />
          <ToggleRow
            label="Allow scheduled background syncs while on battery"
            description="When off, scheduled cloud/external tasks pause on battery power and resume on AC (manual sync always runs)."
            checked={draft.sync_on_battery}
            onChange={(v) => updateDraft({ sync_on_battery: v })}
          />
        </div>
      </div>

      {/* Import Defaults */}
      <SectionTitle>SD Import &amp; Universal Folder Template Defaults</SectionTitle>
      <div className="card">
        <div className="card-body">
          <div className="settings-field">
            <span>Default Library Import Root</span>
            <div className="row">
              <input
                className="text-input mono"
                value={draft.default_import_root}
                onChange={(e) => updateDraft({ default_import_root: e.target.value })}
              />
              <button
                type="button"
                className="button"
                onClick={async () => {
                  const d = await pickDirectory('Default Library Import Root');
                  if (d) updateDraft({ default_import_root: d });
                }}
              >
                <FolderOpen size={14} /> Browse
              </button>
            </div>
          </div>

          <div className="grid-2">
            <div className="settings-field">
              <span>Folder Template</span>
              <input
                className="text-input mono"
                value={draft.folder_template}
                onChange={(e) => updateDraft({ folder_template: e.target.value })}
              />
            </div>
            <div className="settings-field">
              <span>Date Format (e.g., M-dd or yyyy-MM-dd)</span>
              <input
                className="text-input mono"
                value={draft.date_format}
                onChange={(e) => updateDraft({ date_format: e.target.value })}
              />
            </div>
          </div>

          <div className="grid-3">
            <div className="settings-field">
              <span>RAW Subfolder</span>
              <input
                className="text-input mono"
                value={draft.raw_folder_name}
                onChange={(e) => updateDraft({ raw_folder_name: e.target.value })}
              />
            </div>
            <div className="settings-field">
              <span>JPEG Subfolder</span>
              <input
                className="text-input mono"
                value={draft.jpeg_folder_name}
                onChange={(e) => updateDraft({ jpeg_folder_name: e.target.value })}
              />
            </div>
            <div className="settings-field">
              <span>Video Subfolder</span>
              <input
                className="text-input mono"
                value={draft.video_folder_name}
                onChange={(e) => updateDraft({ video_folder_name: e.target.value })}
              />
            </div>
          </div>

          <div className="settings-field">
            <span>Default New-Year Categories (comma-separated)</span>
            <input
              className="text-input"
              value={categoriesText}
              onChange={(e) => {
                dirtyRef.current = true;
                setCategoriesText(e.target.value);
              }}
            />
          </div>

          <div className="grid-2">
            <div className="settings-field">
              <span>Parallel Import Threads (1–16)</span>
              <input
                type="number"
                className="text-input"
                min={1}
                max={16}
                value={threadsText}
                onChange={(e) => {
                  dirtyRef.current = true;
                  setThreadsText(e.target.value);
                }}
              />
            </div>
            <div className="settings-field">
              <span>Freshness Stale Warning (Days)</span>
              <input
                type="number"
                className="text-input"
                min={1}
                value={staleDaysText}
                onChange={(e) => {
                  dirtyRef.current = true;
                  setStaleDaysText(e.target.value);
                }}
              />
            </div>
          </div>

          <ToggleRow
            label="Deep BLAKE3 checksum verification before removing SD files"
            description="Off uses fast byte-size + fsync verification; on reads back every copied file to compare cryptographic hashes."
            checked={draft.deep_verify_move}
            onChange={(v) => updateDraft({ deep_verify_move: v })}
          />
        </div>
      </div>

      {/* Geotagging & Tool Paths */}
      <SectionTitle>Geotagging, Watched GPX Folder &amp; Bundled Tools</SectionTitle>
      <div className="card">
        <div className="card-body">
          <div className="settings-field">
            <span>Watched GPX Folder (Auto-matches overlapping .gpx tracks on SD insert)</span>
            <div className="row">
              <input
                className="text-input mono"
                value={draft.watched_gpx_folder ?? ''}
                placeholder="e.g., ~/Downloads or ~/iCloud/GPX Tracks"
                onChange={(e) =>
                  updateDraft({ watched_gpx_folder: e.target.value || null })
                }
              />
              <button
                type="button"
                className="button"
                onClick={async () => {
                  const d = await pickDirectory('Select Watched GPX Folder');
                  if (d) updateDraft({ watched_gpx_folder: d });
                }}
              >
                <FolderOpen size={14} /> Browse
              </button>
            </div>
          </div>

          <ToggleRow
            label="Update file modification time on GPS backfill"
            description="Recommended so TruongPhoto automatically re-indexes newly embedded GPS coordinates."
            checked={draft.bump_mtime_on_backfill}
            onChange={(v) => updateDraft({ bump_mtime_on_backfill: v })}
          />

          <div className="grid-2" style={{ marginTop: 12 }}>
            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span className="row-between">
                <span>ExifTool Path Override</span>
                <Badge tone={tools?.exiftool.available ? 'ok' : 'danger'}>
                  {tools?.exiftool.available
                    ? `${tools.exiftool.bundled ? 'Bundled · ' : ''}v${tools.exiftool.version}`
                    : 'Missing'}
                </Badge>
              </span>
              <input
                className="text-input mono"
                placeholder={tools?.exiftool.path ?? 'Auto-detected'}
                value={draft.exiftool_path_override ?? ''}
                onChange={(e) =>
                  updateDraft({ exiftool_path_override: e.target.value || null })
                }
              />
            </div>

            <div className="settings-field" style={{ marginBottom: 0 }}>
              <span className="row-between">
                <span>rclone Path Override</span>
                <Badge tone={tools?.rclone.available ? 'ok' : 'danger'}>
                  {tools?.rclone.available
                    ? tools.rclone.bundled
                      ? 'Bundled'
                      : 'Found'
                    : 'Missing'}
                </Badge>
              </span>
              <input
                className="text-input mono"
                placeholder={tools?.rclone.path ?? 'Auto-detected'}
                value={draft.rclone_path_override ?? ''}
                onChange={(e) =>
                  updateDraft({ rclone_path_override: e.target.value || null })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Updates & Releases */}
      <SectionTitle>Updates &amp; GitHub Releases</SectionTitle>
      <div className="card">
        <div className="card-body">
          <div className="row-between" style={{ flexWrap: 'wrap', gap: 12 }}>
            <div>
              <div style={{ fontWeight: 500, fontSize: 13 }}>
                TruongSync v{bootstrap?.version ?? '0.1.0'}
              </div>
              <div className="muted" style={{ fontSize: 12, marginTop: 2 }}>
                Checks signed releases from GitHub Releases and verifies signatures before installing.
              </div>
            </div>
            <div className="row" style={{ gap: 8 }}>
              <button
                type="button"
                className="button"
                onClick={() =>
                  void openExternalUrl(RELEASES_URL).catch((err) =>
                    notifyError(err, 'Could not open GitHub Releases'),
                  )
                }
              >
                <ExternalLink size={14} /> GitHub Releases
              </button>
              <button
                type="button"
                className="button button-primary"
                disabled={checkingUpdates || installingUpdate}
                onClick={() => void checkForUpdates()}
              >
                <RefreshCw size={14} /> {checkingUpdates ? 'Checking…' : 'Check for Updates'}
              </button>
            </div>
          </div>

          {updateStatus && (
            <div style={{ marginTop: 12, paddingTop: 12, borderTop: '1px solid var(--border)' }}>
              {updateStatus.available && updateStatus.version ? (
                <div className="stack" style={{ gap: 10 }}>
                  <div className="row-between" style={{ flexWrap: 'wrap', gap: 8 }}>
                    <div className="row" style={{ gap: 8 }}>
                      <Badge tone="ok">Update Available</Badge>
                      <strong style={{ fontSize: 13 }}>v{updateStatus.version}</strong>
                    </div>
                    <button
                      type="button"
                      className="button button-primary"
                      disabled={installingUpdate}
                      onClick={() => void installUpdate()}
                    >
                      <Download size={14} />{' '}
                      {installingUpdate
                        ? updateProgress?.finished
                          ? 'Installing & Restarting…'
                          : updateProgress?.total
                            ? `Downloading ${Math.min(100, Math.round((updateProgress.downloaded / updateProgress.total) * 100))}%`
                            : `Downloading (${formatBytes(updateProgress?.downloaded ?? 0)})…`
                        : 'Download & Install Update'}
                    </button>
                  </div>
                  {updateStatus.notes && (
                    <pre
                      className="mono muted"
                      style={{
                        fontSize: 12,
                        whiteSpace: 'pre-wrap',
                        margin: 0,
                        padding: 10,
                        borderRadius: 8,
                        background: 'var(--bg-subtle, rgba(255,255,255,0.03))',
                      }}
                    >
                      {updateStatus.notes}
                    </pre>
                  )}
                </div>
              ) : (
                <div className="muted" style={{ fontSize: 12 }}>
                  {updateStatus.notes ?? `You are running the latest version (v${bootstrap?.version ?? '0.1.0'}).`}
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
