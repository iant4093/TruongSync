import React, { useState } from 'react';
import {
  CheckCircle2,
  CloudDownload,
  CloudUpload,
  FolderOpen,
  Link2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from 'lucide-react';
import { api, formatBytes, pickDirectory, pickFiles } from '../../lib/tauri';
import { confirmAction, notifyError, notifySuccess } from '../../store/feedback';
import { useSyncStore } from '../../store/useSyncStore';
import type { CloudProfile, CloudVerifyReport } from '../../types/api';
import { Dropdown } from '../Dropdown';
import { Badge, EmptyState, FreshnessBadge, SectionTitle, ToggleRow, ViewHeader } from '../ui';

const BLANK_PROFILE: CloudProfile = {
  id: '',
  name: 'Raw Photo Backup',
  local_root: '',
  remote_name: 'rawphotos',
  remote_path: 'Raw Photo Backup',
  enabled: true,
  schedule_mode: 'startup',
  schedule_value: '6',
  schedule_action: 'sync',
  last_sync_at: null,
  last_verify_at: null,
  last_status: 'never',
  last_error: null,
  pending_upload_count: 0,
  pending_download_count: 0,
  pending_bytes: 0,
  stale_after_days: 7,
};

export const CloudSyncView: React.FC = () => {
  const cloudProfiles = useSyncStore((s) => s.cloudProfiles);
  const tools = useSyncStore((s) => s.tools);
  const refreshProfiles = useSyncStore((s) => s.refreshProfiles);
  const refreshTools = useSyncStore((s) => s.refreshTools);
  const refreshJobs = useSyncStore((s) => s.refreshJobs);

  const [editing, setEditing] = useState<CloudProfile | null>(null);
  const [staleDaysText, setStaleDaysText] = useState('7');
  const [oauthOpen, setOauthOpen] = useState(false);
  const [oauthRemote, setOauthRemote] = useState('rawphotos');
  const [oauthClientId, setOauthClientId] = useState('');
  const [oauthClientSecret, setOauthClientSecret] = useState('');
  const [oauthBusy, setOauthBusy] = useState(false);
  const [busyMap, setBusyMap] = useState<Record<string, 'verify' | 'sync'>>({});
  const [verifyReports, setVerifyReports] = useState<Record<string, CloudVerifyReport>>({});

  const openEditor = (profile: CloudProfile) => {
    setEditing(profile);
    setStaleDaysText(String(profile.stale_after_days || 7));
  };

  const setProfileBusy = (id: string, kind: 'verify' | 'sync' | null) => {
    setBusyMap((prev) => {
      if (!kind) {
        const next = { ...prev };
        delete next[id];
        return next;
      }
      return { ...prev, [id]: kind };
    });
  };

  const handleImportConf = async () => {
    const files = await pickFiles('Select existing rclone.conf', ['conf'], false);
    if (files.length === 0) return;
    try {
      const remotes = await api.importRcloneConf(files[0]);
      notifySuccess(`Imported rclone config (${remotes.join(', ') || 'ready'}).`);
      await refreshTools();
    } catch (err) {
      notifyError(err, 'Failed to import rclone.conf');
    }
  };

  const handleConnectOauth = async () => {
    setOauthBusy(true);
    try {
      const remotes = await api.connectGoogleDrive(oauthRemote, oauthClientId, oauthClientSecret);
      notifySuccess(`Connected Google Drive remote "${oauthRemote}" (${remotes.length} total).`);
      setOauthOpen(false);
      await refreshTools();
    } catch (err) {
      notifyError(err, 'Google Drive OAuth failed');
    } finally {
      setOauthBusy(false);
    }
  };

  const handleSaveProfile = async () => {
    if (!editing) return;
    if (!editing.name.trim() || !editing.local_root.trim() || !editing.remote_name.trim()) {
      notifyError('Name, local folder, and remote name are required.');
      return;
    }
    if (editing.schedule_mode === 'daily') {
      const m = /^([01]\d|2[0-3]):([0-5]\d)$/.exec(editing.schedule_value.trim());
      if (!m) {
        notifyError('Daily schedule time must use 24-hour HH:MM format (e.g., 20:00).');
        return;
      }
    } else if (editing.schedule_mode === 'interval') {
      const hrs = Number(editing.schedule_value.trim());
      if (!Number.isInteger(hrs) || hrs < 1 || hrs > 168) {
        notifyError('Interval schedule must be a whole number of hours between 1 and 168.');
        return;
      }
    }
    try {
      const parsedDays = Math.max(1, Math.trunc(Number(staleDaysText.trim()) || 7));
      const savedId = editing.id;
      await api.saveCloudProfile({
        ...editing,
        schedule_value: editing.schedule_value.trim(),
        stale_after_days: parsedDays,
      });
      if (savedId) {
        setVerifyReports((prev) => {
          const next = { ...prev };
          delete next[savedId];
          return next;
        });
      }
      notifySuccess(`Saved cloud profile "${editing.name}".`);
      setEditing(null);
      await refreshProfiles();
    } catch (err) {
      notifyError(err, 'Failed to save profile');
    }
  };

  const handleVerify = async (profileId: string, deepMd5 = false) => {
    setProfileBusy(profileId, 'verify');
    try {
      const rep = await api.verifyCloud(profileId, deepMd5);
      setVerifyReports((prev) => ({ ...prev, [profileId]: rep }));
      await refreshProfiles();
      notifySuccess(
        `Verified: ${rep.up_to_date_count.toLocaleString()} up to date, ↑${rep.missing_on_remote.length} upload, ↓${rep.missing_on_local.length} download.`,
      );
    } catch (err) {
      notifyError(err, 'Verify failed');
    } finally {
      setProfileBusy(profileId, null);
    }
  };

  const handleSync = async (profile: CloudProfile, restoreMode: boolean) => {
    if (restoreMode) {
      const ok = await confirmAction(
        `Restore missing local files from "${profile.name}"?`,
        'Restore Missing Files',
        'Restore mode bypasses local delete tombstones and pulls every missing file from Google Drive back into your local folder without overwriting existing local files.',
      );
      if (!ok) return;
    }
    setProfileBusy(profile.id, 'sync');
    try {
      const res = await api.syncCloud(profile.id, restoreMode);
      const summary = `${restoreMode ? 'Restored' : 'Synced'} "${profile.name}": ↑${res.uploaded_count} uploaded, ↓${res.downloaded_count} downloaded${
        res.backfill_replaced_count > 0 ? `, ${res.backfill_replaced_count} GPS backfills updated` : ''
      }`;
      if (res.failed_count > 0 || res.conflicts_count > 0) {
        notifyError(
          `${summary} (${res.failed_count} failed, ${res.conflicts_count} conflict(s) skipped safely).`,
        );
      } else {
        notifySuccess(`${summary}.`);
      }
      setVerifyReports((prev) => {
        const next = { ...prev };
        delete next[profile.id];
        return next;
      });
      await refreshProfiles();
      await refreshJobs();
    } catch (err) {
      notifyError(err, restoreMode ? 'Restore failed' : 'Google Drive sync failed');
    } finally {
      setProfileBusy(profile.id, null);
    }
  };

  const handleDelete = async (profile: CloudProfile) => {
    const ok = await confirmAction(
      `Remove cloud profile "${profile.name}"?`,
      'Remove Profile',
      'Only removes the sync configuration from TruongSync. No local or Google Drive files are deleted.',
    );
    if (!ok) return;
    try {
      await api.deleteCloudProfile(profile.id);
      setEditing((cur) => (cur?.id === profile.id ? null : cur));
      setVerifyReports((prev) => {
        const next = { ...prev };
        delete next[profile.id];
        return next;
      });
      notifySuccess(`Removed cloud profile "${profile.name}".`);
      await refreshProfiles();
    } catch (err) {
      notifyError(err, 'Failed to remove cloud profile');
    }
  };

  return (
    <div className="view-content">
      <ViewHeader
        title="Google Drive Cloud Sync"
        subtitle="2-way strictly additive sync (--immutable): uploads local shoots & edits, downloads on-the-go Drive additions, never deletes on either side."
        actions={
          <>
            <button type="button" className="button" onClick={() => void handleImportConf()}>
              <FolderOpen size={14} /> Import rclone.conf
            </button>
            <button
              type="button"
              className="button"
              onClick={() => setOauthOpen((v) => (oauthBusy ? true : !v))}
            >
              <Link2 size={14} /> Connect Google Drive
            </button>
            <button
              type="button"
              className="button button-primary"
              onClick={() =>
                openEditor({
                  ...BLANK_PROFILE,
                  remote_name: tools?.rclone_remotes[0] ?? 'rawphotos',
                })
              }
            >
              <Plus size={14} /> Add Profile
            </button>
          </>
        }
      />

      {/* Connection Status Card */}
      <div className="card" style={{ marginBottom: 16 }}>
        <div className="card-body row-between">
          <div className="row-wrap">
            <Badge tone={tools?.rclone.available ? 'ok' : 'danger'}>
              rclone:{' '}
              {tools?.rclone.available
                ? `${tools.rclone.bundled ? 'Bundled · ' : ''}${tools.rclone.version}`
                : 'Not installed'}
            </Badge>
            <Badge tone={tools?.rclone_config_exists ? 'ok' : 'warn'}>
              {tools?.rclone_config_exists
                ? `Remotes: ${tools.rclone_remotes.join(', ') || 'Configured'}`
                : 'No Google Drive remote linked'}
            </Badge>
            <Badge>Policy: 2-Way Additive · Never Deletes</Badge>
          </div>
          <button type="button" className="button button-quiet" onClick={() => void refreshTools()}>
            <RefreshCw size={13} /> Recheck
          </button>
        </div>
      </div>

      {/* Built-in OAuth Sign-in Card */}
      {oauthOpen && (
        <div className="card" style={{ marginBottom: 16 }}>
          <div className="card-body stack-md">
            <strong>Connect Google Drive (Browser OAuth)</strong>
            <div className="grid-3">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Remote Name</span>
                <input
                  className="text-input mono"
                  value={oauthRemote}
                  onChange={(e) => setOauthRemote(e.target.value)}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>OAuth Client ID (optional)</span>
                <input
                  className="text-input mono"
                  value={oauthClientId}
                  placeholder="Recommended for faster Drive quota"
                  onChange={(e) => setOauthClientId(e.target.value)}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>OAuth Client Secret (optional)</span>
                <input
                  type="password"
                  className="text-input mono"
                  value={oauthClientSecret}
                  onChange={(e) => setOauthClientSecret(e.target.value)}
                />
              </div>
            </div>
            <div className="row-between">
              <span className="muted">
                Opens your browser for Google sign-in and stores the token in TruongSync&apos;s private config.
              </span>
              <div className="row">
                {oauthBusy && (
                  <button
                    type="button"
                    className="button button-danger"
                    onClick={() => void api.cancelOAuth()}
                  >
                    Cancel Sign-In
                  </button>
                )}
                <button
                  type="button"
                  className="button button-primary"
                  disabled={oauthBusy}
                  onClick={() => void handleConnectOauth()}
                >
                  {oauthBusy ? 'Waiting for browser…' : 'Sign in with Google'}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Profile Editor */}
      {editing && (
        <div className="card" style={{ marginBottom: 18 }}>
          <div className="card-body stack-md">
            <strong>{editing.id ? 'Edit Cloud Profile' : 'New Cloud Sync Profile'}</strong>
            <div className="grid-2">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Profile Name</span>
                <input
                  className="text-input"
                  value={editing.name}
                  onChange={(e) => setEditing({ ...editing, name: e.target.value })}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Local Folder Root</span>
                <div className="row">
                  <input
                    className="text-input mono"
                    value={editing.local_root}
                    placeholder="e.g., /Volumes/Photos/Photo Dump"
                    onChange={(e) => setEditing({ ...editing, local_root: e.target.value })}
                  />
                  <button
                    type="button"
                    className="button"
                    onClick={async () => {
                      const d = await pickDirectory('Choose Local Folder to Sync');
                      if (d) setEditing((prev) => (prev ? { ...prev, local_root: d } : prev));
                    }}
                  >
                    Browse
                  </button>
                </div>
              </div>
            </div>
            <div className="grid-2">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>rclone Remote Name</span>
                <input
                  className="text-input mono"
                  value={editing.remote_name}
                  onChange={(e) => setEditing({ ...editing, remote_name: e.target.value })}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Google Drive Folder Path</span>
                <input
                  className="text-input mono"
                  value={editing.remote_path}
                  placeholder="Raw Photo Backup"
                  onChange={(e) => setEditing({ ...editing, remote_path: e.target.value })}
                />
              </div>
            </div>
            <div className="grid-3">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Background Schedule</span>
                <Dropdown
                  label="Schedule"
                  value={editing.schedule_mode}
                  options={[
                    ['manual', 'Manual Only'],
                    ['startup', 'On App Startup'],
                    ['interval', 'Every N Hours'],
                    ['daily', 'Daily at Time'],
                  ]}
                  onChange={(v) =>
                    setEditing({
                      ...editing,
                      schedule_mode: v,
                      schedule_value:
                        v === 'daily' && !editing.schedule_value.includes(':')
                          ? '20:00'
                          : v === 'interval' && editing.schedule_value.includes(':')
                            ? '6'
                            : editing.schedule_value,
                    })
                  }
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Scheduled Action</span>
                <Dropdown
                  label="Action"
                  value={editing.schedule_action}
                  options={[
                    ['sync', 'Auto-Sync (2-Way Additive)'],
                    ['check', 'Verify Only (Update Freshness)'],
                  ]}
                  onChange={(v) => setEditing({ ...editing, schedule_action: v })}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Freshness Warning (Days)</span>
                <input
                  type="text"
                  inputMode="numeric"
                  className="text-input"
                  value={staleDaysText}
                  onChange={(e) => setStaleDaysText(e.target.value)}
                />
              </div>
            </div>
            {(editing.schedule_mode === 'daily' || editing.schedule_mode === 'interval') && (
              <div className="grid-2">
                <div className="settings-field" style={{ marginBottom: 0 }}>
                  <span>
                    {editing.schedule_mode === 'daily' ? 'Daily Run Time (HH:MM, 24h)' : 'Run Interval (Hours)'}
                  </span>
                  <input
                    className="text-input mono"
                    value={editing.schedule_value}
                    placeholder={editing.schedule_mode === 'daily' ? '20:00' : '6'}
                    onChange={(e) => setEditing({ ...editing, schedule_value: e.target.value })}
                  />
                </div>
              </div>
            )}
            <ToggleRow
              label="Enable scheduled background runs for this profile"
              description="Turn off to pause automatic schedules while keeping manual Verify and Sync available."
              checked={editing.enabled}
              onChange={(v) => setEditing({ ...editing, enabled: v })}
            />
            <div className="row" style={{ justifyContent: 'flex-end' }}>
              <button type="button" className="button" onClick={() => setEditing(null)}>
                Cancel
              </button>
              <button type="button" className="button button-primary" onClick={() => void handleSaveProfile()}>
                Save Profile
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Profile List & Freshness */}
      <SectionTitle>Cloud Sync Profiles &amp; Freshness</SectionTitle>
      {cloudProfiles.length === 0 ? (
        <EmptyState
          icon={<CloudUpload size={28} strokeWidth={1.4} />}
          title="No Google Drive sync profiles yet"
          description="Add a profile mapping any local library folder (e.g., Photo Dump or Edited Photos) to a Google Drive folder. Your existing Drive hierarchy works out of the box."
          actions={
            <button
              type="button"
              className="button button-primary"
              onClick={() => openEditor({ ...BLANK_PROFILE })}
            >
              <Plus size={14} /> Add Cloud Profile
            </button>
          }
        />
      ) : (
        <div className="stack-md">
          {cloudProfiles.map((p) => {
            const report = verifyReports[p.id];
            const busyKind = busyMap[p.id];
            const isBusy = Boolean(busyKind);
            return (
              <div key={p.id} className="card">
                <div className="folder-row">
                  <div className="stack" style={{ gap: 4 }}>
                    <div className="row-wrap">
                      <strong style={{ fontSize: 14 }}>{p.name}</strong>
                      {!p.enabled && <Badge tone="warn">Paused</Badge>}
                      <FreshnessBadge
                        lastSyncAt={p.last_sync_at}
                        lastStatus={p.last_status}
                        staleAfterDays={p.stale_after_days}
                      />
                      <Badge>
                        {p.schedule_mode === 'interval'
                          ? `Every ${p.schedule_value || 6}h`
                          : p.schedule_mode === 'daily'
                            ? `Daily ${p.schedule_value}`
                            : p.schedule_mode}
                      </Badge>
                    </div>
                    <div className="mono muted">
                      {p.local_root} ↔ {p.remote_name}:{p.remote_path}
                    </div>
                    {(p.pending_upload_count > 0 || p.pending_download_count > 0) && (
                      <div className="row-wrap">
                        {p.pending_upload_count > 0 && (
                          <Badge tone="info">↑ {p.pending_upload_count} to upload</Badge>
                        )}
                        {p.pending_download_count > 0 && (
                          <Badge tone="info">↓ {p.pending_download_count} on Drive</Badge>
                        )}
                        <Badge>{formatBytes(p.pending_bytes)}</Badge>
                      </div>
                    )}
                    {p.last_error && <div style={{ color: 'var(--danger-text)', fontSize: 12 }}>{p.last_error}</div>}
                  </div>
                  <div className="row-wrap">
                    <button
                      type="button"
                      className="button"
                      disabled={isBusy}
                      onClick={() => void handleVerify(p.id, false)}
                    >
                      <ShieldCheck size={14} /> {busyKind === 'verify' ? 'Verifying…' : 'Verify'}
                    </button>
                    <button
                      type="button"
                      className="button"
                      disabled={isBusy}
                      onClick={() => void handleSync(p, true)}
                    >
                      <CloudDownload size={14} /> Restore…
                    </button>
                    <button
                      type="button"
                      className="button button-primary"
                      disabled={isBusy}
                      onClick={() => void handleSync(p, false)}
                    >
                      <CloudUpload size={14} /> {busyKind === 'sync' ? 'Syncing…' : 'Sync Now'}
                    </button>
                    <button
                      type="button"
                      className="button button-quiet"
                      disabled={isBusy}
                      onClick={() => openEditor(p)}
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      className="icon-button button-danger-quiet"
                      title="Remove profile"
                      disabled={isBusy}
                      onClick={() => void handleDelete(p)}
                    >
                      <Trash2 size={14} />
                    </button>
                  </div>
                </div>

                {report && (
                  <div className="card-body" style={{ borderTop: '1px solid var(--border)', background: '#1e1e1e' }}>
                    <div className="row-between" style={{ marginBottom: 10 }}>
                      <div className="row-wrap">
                        <Badge tone="ok">
                          <CheckCircle2 size={12} /> {report.up_to_date_count.toLocaleString()} verified up to date
                        </Badge>
                        <Badge tone={report.missing_on_remote.length > 0 ? 'info' : 'muted'}>
                          ↑ {report.missing_on_remote.length} to upload
                        </Badge>
                        <Badge tone={report.missing_on_local.length > 0 ? 'info' : 'muted'}>
                          ↓ {report.missing_on_local.length} new on Drive
                        </Badge>
                        {report.backfill_updates.length > 0 && (
                          <Badge tone="ok">{report.backfill_updates.length} GPS backfill updates</Badge>
                        )}
                        {report.tombstoned_local.length > 0 && (
                          <Badge>{report.tombstoned_local.length} locally deleted (available in Restore)</Badge>
                        )}
                        {report.conflicts.length > 0 && (
                          <Badge tone="danger">{report.conflicts.length} conflicts</Badge>
                        )}
                        {report.duplicate_remote_paths.length > 0 && (
                          <Badge tone="warn">{report.duplicate_remote_paths.length} duplicate Drive paths</Badge>
                        )}
                      </div>
                      <div className="row">
                        {(report.missing_on_remote.length > 0 ||
                          report.missing_on_local.length > 0 ||
                          report.backfill_updates.length > 0) && (
                          <button
                            type="button"
                            className="button button-primary"
                            disabled={isBusy}
                            onClick={() => void handleSync(p, false)}
                          >
                            <CloudUpload size={13} /> Sync Differences
                          </button>
                        )}
                        <button
                          type="button"
                          className="button button-quiet"
                          disabled={isBusy}
                          onClick={() => void handleVerify(p.id, true)}
                        >
                          Deep MD5 Verify
                        </button>
                      </div>
                    </div>
                    {(report.missing_on_remote.length > 0 ||
                      report.missing_on_local.length > 0 ||
                      report.backfill_updates.length > 0 ||
                      report.conflicts.length > 0) && (
                      <div className="data-table-wrap">
                        <table className="data-table">
                          <thead>
                            <tr>
                              <th>Action</th>
                              <th>Path</th>
                              <th>Size</th>
                              <th>Notes</th>
                            </tr>
                          </thead>
                          <tbody>
                            {[
                              ...report.backfill_updates,
                              ...report.missing_on_local,
                              ...report.missing_on_remote,
                              ...report.conflicts,
                            ]
                              .slice(0, 150)
                              .map((d) => (
                                <tr key={`${d.direction}:${d.rel_path}`}>
                                  <td>
                                    <Badge
                                      tone={
                                        d.direction === 'conflict'
                                          ? 'danger'
                                          : d.direction === 'backfill_replace'
                                            ? 'ok'
                                            : 'info'
                                      }
                                    >
                                      {d.direction}
                                    </Badge>
                                  </td>
                                  <td className="mono">{d.rel_path}</td>
                                  <td className="mono tabular">
                                    {formatBytes(d.local_bytes ?? d.remote_bytes)}
                                  </td>
                                  <td className="muted">{d.note ?? '—'}</td>
                                </tr>
                              ))}
                          </tbody>
                        </table>
                      </div>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};
