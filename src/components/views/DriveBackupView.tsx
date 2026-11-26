import React, { useState } from 'react';
import {
  CheckCircle2,
  Download,
  HardDrive,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from 'lucide-react';
import { api, formatBytes, pickDirectory } from '../../lib/tauri';
import { confirmAction, notifyError, notifySuccess } from '../../store/feedback';
import { useSyncStore } from '../../store/useSyncStore';
import type { ExternalProfile, ExternalVerifyReport } from '../../types/api';
import { Dropdown } from '../Dropdown';
import { Badge, EmptyState, FreshnessBadge, SectionTitle, ToggleRow, ViewHeader } from '../ui';

const BLANK_EXTERNAL: ExternalProfile = {
  id: '',
  name: 'Extreme SSD Backup',
  local_root: '',
  volume_uuid: '',
  volume_label: '',
  relative_dest_path: 'Photo Library Backup/All Photos',
  direction: 'two_way_additive',
  enabled: true,
  schedule_mode: 'on_connect',
  schedule_value: '12',
  schedule_action: 'check',
  write_marker_file: true,
  last_sync_at: null,
  last_verify_at: null,
  last_status: 'never',
  last_error: null,
  pending_push_count: 0,
  pending_pull_count: 0,
  pending_bytes: 0,
  stale_after_days: 14,
};

export const DriveBackupView: React.FC = () => {
  const externalProfiles = useSyncStore((s) => s.externalProfiles);
  const volumes = useSyncStore((s) => s.volumes);
  const refreshVolumes = useSyncStore((s) => s.refreshVolumes);
  const refreshProfiles = useSyncStore((s) => s.refreshProfiles);
  const refreshJobs = useSyncStore((s) => s.refreshJobs);

  const [editing, setEditing] = useState<ExternalProfile | null>(null);
  const [staleDaysText, setStaleDaysText] = useState('14');
  const [busyMap, setBusyMap] = useState<Record<string, 'verify' | 'sync'>>({});
  const [reports, setReports] = useState<Record<string, ExternalVerifyReport>>({});

  const openEditor = (profile: ExternalProfile) => {
    setEditing(profile);
    setStaleDaysText(String(profile.stale_after_days || 14));
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

  const matchVolume = (p: ExternalProfile) =>
    volumes.find((v) =>
      p.volume_uuid
        ? v.volume_uuid.toLowerCase() === p.volume_uuid.toLowerCase()
        : Boolean(p.volume_label) && v.label.toLowerCase() === p.volume_label.toLowerCase(),
    );

  const handleSave = async () => {
    if (!editing) return;
    if (!editing.name.trim() || !editing.local_root.trim() || !editing.volume_label.trim()) {
      notifyError('Name, local root, and target external drive are required.');
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
      const parsedDays = Math.max(1, Math.trunc(Number(staleDaysText.trim()) || 14));
      const savedId = editing.id;
      await api.saveExternalProfile({
        ...editing,
        schedule_value: editing.schedule_value.trim(),
        stale_after_days: parsedDays,
      });
      if (savedId) {
        setReports((prev) => {
          const next = { ...prev };
          delete next[savedId];
          return next;
        });
      }
      notifySuccess(`Saved external backup profile "${editing.name}".`);
      setEditing(null);
      await refreshProfiles();
    } catch (err) {
      notifyError(err, 'Failed to save external profile');
    }
  };

  const handleVerify = async (profileId: string, deepMd5 = false) => {
    setProfileBusy(profileId, 'verify');
    try {
      const rep = await api.verifyExternal(profileId, deepMd5);
      setReports((prev) => ({ ...prev, [profileId]: rep }));
      await refreshProfiles();
      if (!rep.volume_connected) {
        notifyError('External drive is not currently connected.');
      } else {
        notifySuccess(
          `Verified: ${rep.up_to_date_count.toLocaleString()} up to date, ↑${rep.to_push.length} to drive, ↓${rep.to_pull.length} from drive.`,
        );
      }
    } catch (err) {
      notifyError(err, 'Verify failed');
    } finally {
      setProfileBusy(profileId, null);
    }
  };

  const handleSync = async (profile: ExternalProfile, restoreMode: boolean) => {
    if (restoreMode) {
      const ok = await confirmAction(
        `Restore missing local files from "${profile.volume_label}"?`,
        'Restore Missing Files',
        'Pulls every file present on the external drive that is missing locally (bypassing local delete tombstones). No existing local files are deleted or overwritten.',
      );
      if (!ok) return;
    }
    setProfileBusy(profile.id, 'sync');
    try {
      const res = await api.syncExternal(profile.id, restoreMode);
      const summary = `${restoreMode ? 'Restored' : 'Synced'} "${profile.name}": ↑${res.pushed_count} pushed, ↓${res.pulled_count} pulled (${formatBytes(res.bytes_transferred)})`;
      if (res.failed_count > 0) {
        notifyError(`${summary} — ${res.failed_count} file(s) failed (see Activity for details).`);
      } else {
        notifySuccess(`${summary}.`);
      }
      setReports((prev) => {
        const next = { ...prev };
        delete next[profile.id];
        return next;
      });
      await refreshProfiles();
      await refreshJobs();
    } catch (err) {
      notifyError(err, 'External drive sync failed');
    } finally {
      setProfileBusy(profile.id, null);
    }
  };

  const handleDelete = async (profile: ExternalProfile) => {
    const ok = await confirmAction(
      `Remove external profile "${profile.name}"?`,
      'Remove Profile',
      'Only removes the backup profile from TruongSync. No files on your PC or external drive are touched.',
    );
    if (!ok) return;
    try {
      await api.deleteExternalProfile(profile.id);
      setEditing((cur) => (cur?.id === profile.id ? null : cur));
      setReports((prev) => {
        const next = { ...prev };
        delete next[profile.id];
        return next;
      });
      notifySuccess(`Removed external profile "${profile.name}".`);
      await refreshProfiles();
    } catch (err) {
      notifyError(err, 'Failed to remove external profile');
    }
  };

  return (
    <div className="view-content">
      <ViewHeader
        title="External Drive Backup"
        subtitle="Volume-UUID matched 2-way strictly additive sync across macOS and Windows. Pulls travel additions first, pushes your full library second, never deletes."
        actions={
          <>
            <button type="button" className="button" onClick={() => void refreshVolumes()}>
              <RefreshCw size={14} /> Refresh Drives
            </button>
            <button
              type="button"
              className="button button-primary"
              onClick={() => {
                const firstExt = volumes.find((v) => !v.has_dcim);
                openEditor({
                  ...BLANK_EXTERNAL,
                  volume_uuid: firstExt?.volume_uuid ?? '',
                  volume_label: firstExt?.label ?? '',
                });
              }}
            >
              <Plus size={14} /> Add Backup Drive
            </button>
          </>
        }
      />

      {editing && (
        <div className="card" style={{ marginBottom: 18 }}>
          <div className="card-body stack-md">
            <strong>{editing.id ? 'Edit External Drive Profile' : 'New External Drive Profile'}</strong>
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
                <span>Local Library Root (e.g., All Photos)</span>
                <div className="row">
                  <input
                    className="text-input mono"
                    value={editing.local_root}
                    onChange={(e) => setEditing({ ...editing, local_root: e.target.value })}
                  />
                  <button
                    type="button"
                    className="button"
                    onClick={async () => {
                      const d = await pickDirectory('Choose Local Library Root');
                      if (d) setEditing((prev) => (prev ? { ...prev, local_root: d } : prev));
                    }}
                  >
                    Browse
                  </button>
                </div>
              </div>
            </div>

            {volumes.length > 0 && (
              <div>
                <div className="muted" style={{ marginBottom: 6 }}>
                  Pick a currently mounted drive to lock its Volume UUID:
                </div>
                <div className="row-wrap">
                  {volumes.map((v) => (
                    <button
                      key={v.mount_path}
                      type="button"
                      className={`chip-button ${editing.volume_uuid === v.volume_uuid ? 'is-selected' : ''}`}
                      onClick={() =>
                        setEditing({
                          ...editing,
                          volume_uuid: v.volume_uuid,
                          volume_label: v.label,
                        })
                      }
                    >
                      <HardDrive size={12} /> {v.label} ({formatBytes(v.free_bytes)} free)
                    </button>
                  ))}
                </div>
              </div>
            )}

            <div className="grid-3">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span className="row-between">
                  <span>Drive Label</span>
                  <Badge tone={editing.volume_uuid ? 'ok' : 'warn'}>
                    {editing.volume_uuid ? 'UUID locked' : 'Label match'}
                  </Badge>
                </span>
                <input
                  className="text-input"
                  value={editing.volume_label}
                  onChange={(e) =>
                    setEditing({
                      ...editing,
                      volume_label: e.target.value,
                      volume_uuid: '',
                    })
                  }
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Relative Folder on Drive</span>
                <input
                  className="text-input mono"
                  value={editing.relative_dest_path}
                  onChange={(e) => setEditing({ ...editing, relative_dest_path: e.target.value })}
                />
              </div>
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Direction</span>
                <Dropdown
                  label="Direction"
                  value={editing.direction}
                  options={[
                    ['two_way_additive', '2-Way Additive (Never Deletes)'],
                    ['push_only', 'PC → Drive Only (Never Deletes)'],
                  ]}
                  onChange={(v) => setEditing({ ...editing, direction: v })}
                />
              </div>
            </div>

            <div className="grid-3">
              <div className="settings-field" style={{ marginBottom: 0 }}>
                <span>Schedule</span>
                <Dropdown
                  label="Schedule"
                  value={editing.schedule_mode}
                  options={[
                    ['on_connect', 'When Drive Connects'],
                    ['manual', 'Manual Only'],
                    ['interval', 'Every N Hours (if connected)'],
                    ['daily', 'Daily at Time (if connected)'],
                  ]}
                  onChange={(v) =>
                    setEditing({
                      ...editing,
                      schedule_mode: v,
                      schedule_value:
                        v === 'daily' && !editing.schedule_value.includes(':')
                          ? '20:00'
                          : v === 'interval' && editing.schedule_value.includes(':')
                            ? '12'
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
                    ['check', 'Verify & Notify'],
                    ['sync', 'Auto-Sync'],
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
                    placeholder={editing.schedule_mode === 'daily' ? '20:00' : '12'}
                    onChange={(e) => setEditing({ ...editing, schedule_value: e.target.value })}
                  />
                </div>
              </div>
            )}

            <ToggleRow
              label="Enable scheduled / auto-connect checks for this drive"
              description="Turn off to pause automatic triggers while keeping manual Verify and Sync available."
              checked={editing.enabled}
              onChange={(v) => setEditing({ ...editing, enabled: v })}
            />

            <ToggleRow
              label='Write "Last Sync - yyyy-MM-dd HH-mm-ss.txt" marker on backup root'
              description="Matches Backup-Photos.ps1 so you can see the last backup timestamp directly in Finder/Explorer."
              checked={editing.write_marker_file}
              onChange={(v) => setEditing({ ...editing, write_marker_file: v })}
            />

            <div className="row" style={{ justifyContent: 'flex-end' }}>
              <button type="button" className="button" onClick={() => setEditing(null)}>
                Cancel
              </button>
              <button type="button" className="button button-primary" onClick={() => void handleSave()}>
                Save Drive Profile
              </button>
            </div>
          </div>
        </div>
      )}

      <SectionTitle>Registered Backup Drives &amp; Freshness</SectionTitle>
      {externalProfiles.length === 0 ? (
        <EmptyState
          icon={<HardDrive size={28} strokeWidth={1.4} />}
          title="No external backup drives registered"
          description="Plug in your external SSD/HDD and register it by Volume UUID so TruongSync tracks backup freshness and verifies everything is up to date."
          actions={
            <button
              type="button"
              className="button button-primary"
              onClick={() => openEditor({ ...BLANK_EXTERNAL })}
            >
              <Plus size={14} /> Add Backup Drive
            </button>
          }
        />
      ) : (
        <div className="stack-md">
          {externalProfiles.map((p) => {
            const vol = matchVolume(p);
            const rep = reports[p.id];
            const busyKind = busyMap[p.id];
            const isBusy = Boolean(busyKind);
            return (
              <div key={p.id} className="card">
                <div className="folder-row">
                  <div className="stack" style={{ gap: 4 }}>
                    <div className="row-wrap">
                      <strong style={{ fontSize: 14 }}>{p.name}</strong>
                      {!p.enabled && <Badge tone="warn">Paused</Badge>}
                      <Badge tone={vol ? 'ok' : 'muted'}>
                        {vol ? `Connected · ${formatBytes(vol.free_bytes)} free` : 'Disconnected'}
                      </Badge>
                      <FreshnessBadge
                        lastSyncAt={p.last_sync_at}
                        lastStatus={p.last_status}
                        staleAfterDays={p.stale_after_days}
                      />
                      <Badge>
                        {p.direction === 'two_way_additive' ? '2-Way Additive' : 'Push Only'}
                      </Badge>
                    </div>
                    <div className="mono muted">
                      {p.local_root} ↔ [{p.volume_label}]/{p.relative_dest_path}
                    </div>
                    {(p.pending_push_count > 0 || p.pending_pull_count > 0) && (
                      <div className="row-wrap">
                        {p.pending_push_count > 0 && (
                          <Badge tone="info">↑ {p.pending_push_count} to drive</Badge>
                        )}
                        {p.pending_pull_count > 0 && (
                          <Badge tone="info">↓ {p.pending_pull_count} from drive</Badge>
                        )}
                        {p.pending_bytes > 0 && <Badge>{formatBytes(p.pending_bytes)}</Badge>}
                      </div>
                    )}
                    {p.last_error && (
                      <div style={{ color: 'var(--danger-text)', fontSize: 12 }}>{p.last_error}</div>
                    )}
                  </div>
                  <div className="row-wrap">
                    <button
                      type="button"
                      className="button"
                      disabled={!vol || isBusy}
                      onClick={() => void handleVerify(p.id, false)}
                    >
                      <ShieldCheck size={14} /> {busyKind === 'verify' ? 'Verifying…' : 'Verify'}
                    </button>
                    <button
                      type="button"
                      className="button"
                      disabled={!vol || isBusy}
                      onClick={() => void handleSync(p, true)}
                    >
                      <Download size={14} /> Restore…
                    </button>
                    <button
                      type="button"
                      className="button button-primary"
                      disabled={!vol || isBusy}
                      onClick={() => void handleSync(p, false)}
                    >
                      <HardDrive size={14} /> {busyKind === 'sync' ? 'Syncing…' : 'Sync Now'}
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

                {rep && rep.volume_connected && (
                  <div className="card-body" style={{ borderTop: '1px solid var(--border)', background: '#1e1e1e' }}>
                    <div className="row-between" style={{ marginBottom: 10 }}>
                      <div className="row-wrap">
                        <Badge tone="ok">
                          <CheckCircle2 size={12} /> {rep.up_to_date_count.toLocaleString()} up to date
                        </Badge>
                        <Badge tone={rep.to_push.length > 0 ? 'info' : 'muted'}>
                          ↑ {rep.to_push.length} to drive
                        </Badge>
                        <Badge tone={rep.to_pull.length > 0 ? 'info' : 'muted'}>
                          ↓ {rep.to_pull.length} from drive
                        </Badge>
                        {rep.backfill_updates.length > 0 && (
                          <Badge tone="ok">{rep.backfill_updates.length} GPS backfill updates</Badge>
                        )}
                        {rep.tombstoned_local.length > 0 && (
                          <Badge>{rep.tombstoned_local.length} locally deleted (Restore available)</Badge>
                        )}
                        {rep.conflicts.length > 0 && (
                          <Badge tone="danger">{rep.conflicts.length} conflicts</Badge>
                        )}
                        <Badge>{formatBytes(rep.pending_bytes)} pending</Badge>
                      </div>
                      <div className="row">
                        {(rep.to_push.length > 0 ||
                          rep.to_pull.length > 0 ||
                          rep.backfill_updates.length > 0) && (
                          <button
                            type="button"
                            className="button button-primary"
                            disabled={!vol || isBusy}
                            onClick={() => void handleSync(p, false)}
                          >
                            <HardDrive size={13} /> Sync Differences
                          </button>
                        )}
                        <button
                          type="button"
                          className="button button-quiet"
                          disabled={!vol || isBusy}
                          onClick={() => void handleVerify(p.id, true)}
                        >
                          Deep MD5 Verify
                        </button>
                      </div>
                    </div>
                    {(rep.to_push.length > 0 ||
                      rep.to_pull.length > 0 ||
                      rep.backfill_updates.length > 0 ||
                      rep.conflicts.length > 0) && (
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
                              ...rep.backfill_updates,
                              ...rep.to_pull,
                              ...rep.to_push,
                              ...rep.conflicts,
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
