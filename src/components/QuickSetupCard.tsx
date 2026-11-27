import React, { useState } from 'react';
import { CheckCircle2, CloudUpload, FolderOpen, HardDrive, MapPin, Sparkles, X } from 'lucide-react';
import { api, pickDirectory } from '../lib/tauri';
import { notifyError, notifySuccess } from '../store/feedback';
import { useSyncStore } from '../store/useSyncStore';
import { Badge } from './ui';

/**
 * Guided first-run Quick Setup checklist shown until the user configures their library root
 * and cloud/backup targets (or dismisses it). Makes TruongSync effortless for any new user.
 */
export const QuickSetupCard: React.FC<{
  onRootPicked?: (root: string) => void;
}> = ({ onRootPicked }) => {
  const settings = useSyncStore((s) => s.settings);
  const tools = useSyncStore((s) => s.tools);
  const toolsChecked = useSyncStore((s) => s.toolsChecked);
  const cloudProfiles = useSyncStore((s) => s.cloudProfiles);
  const externalProfiles = useSyncStore((s) => s.externalProfiles);
  const setActiveNav = useSyncStore((s) => s.setActiveNav);
  const refreshSettings = useSyncStore((s) => s.refreshSettings);
  const refreshProfiles = useSyncStore((s) => s.refreshProfiles);

  const [dismissed, setDismissed] = useState(
    () => typeof localStorage !== 'undefined' && localStorage.getItem('ts_hide_quick_setup') === '1',
  );

  // Wait until the initial tool probe settles before deciding whether to show Quick Setup,
  // avoiding a layout-shift flash on cold launch for already-configured users.
  if (dismissed || !settings || (!toolsChecked && !tools)) return null;

  const hasLibraryRoot = Boolean(settings.default_import_root.trim());
  const hasCloud = Boolean(tools?.rclone_config_exists && cloudProfiles.length > 0);
  const hasExternal = externalProfiles.length > 0;
  const hasGpxFolder = Boolean(settings.watched_gpx_folder);
  const toolsReady = Boolean(tools?.exiftool.available && tools?.rclone.available);

  // Auto-hide once core steps are done
  if (hasLibraryRoot && hasCloud) return null;

  const pickLibraryRoot = async () => {
    const dir = await pickDirectory('Choose Your Photo Library Root Folder');
    if (!dir) return;
    try {
      await api.saveSettings({ ...settings, default_import_root: dir });
      await refreshSettings();
      await refreshProfiles();
      onRootPicked?.(dir);
      notifySuccess(`Default library root saved: ${dir}`);
    } catch (err) {
      notifyError(err, 'Could not save library root');
    }
  };

  const pickGpxFolder = async () => {
    const dir = await pickDirectory('Choose Your Watched GPX Folder');
    if (!dir) return;
    try {
      await api.saveSettings({ ...settings, watched_gpx_folder: dir });
      await refreshSettings();
      notifySuccess(`Watched GPX folder saved: ${dir}`);
    } catch (err) {
      notifyError(err, 'Could not save GPX folder');
    }
  };

  const hide = () => {
    setDismissed(true);
    try {
      localStorage.setItem('ts_hide_quick_setup', '1');
    } catch {
      // ignore
    }
  };

  return (
    <div className="card" style={{ marginBottom: 18 }}>
      <div className="card-body stack-md">
        <div className="row-between">
          <div className="row-wrap">
            <Sparkles size={15} color="var(--ok)" />
            <strong style={{ fontSize: 14 }}>Quick Setup</strong>
            {toolsReady && (
              <Badge tone="ok">
                {tools?.exiftool.bundled && tools?.rclone.bundled
                  ? 'All tools bundled (ExifTool + rclone)'
                  : `Tools ready (${tools?.exiftool.bundled ? 'Bundled ExifTool' : 'System ExifTool'} + ${tools?.rclone.bundled ? 'Bundled rclone' : 'System rclone'})`}
              </Badge>
            )}
          </div>
          <button type="button" className="icon-button" title="Dismiss Quick Setup" onClick={hide}>
            <X size={14} />
          </button>
        </div>

        <div className="stack">
          <div className="row-between" style={{ padding: '6px 0', borderBottom: '1px solid var(--border-subtle)' }}>
            <div className="row">
              {hasLibraryRoot ? (
                <CheckCircle2 size={16} color="var(--ok)" />
              ) : (
                <FolderOpen size={16} color="var(--text-secondary)" />
              )}
              <div>
                <div style={{ fontWeight: 500 }}>1. Choose your photo library destination folder</div>
                <div className="muted">
                  {hasLibraryRoot ? settings.default_import_root : 'Where SD imports sort into Year / Category / Shoot / RAW · JPEG · VID'}
                </div>
              </div>
            </div>
            <button type="button" className={`button ${hasLibraryRoot ? '' : 'button-primary'}`} onClick={() => void pickLibraryRoot()}>
              {hasLibraryRoot ? 'Change' : 'Choose Folder'}
            </button>
          </div>

          <div className="row-between" style={{ padding: '6px 0', borderBottom: '1px solid var(--border-subtle)' }}>
            <div className="row">
              {hasCloud ? (
                <CheckCircle2 size={16} color="var(--ok)" />
              ) : (
                <CloudUpload size={16} color="var(--text-secondary)" />
              )}
              <div>
                <div style={{ fontWeight: 500 }}>2. Link Google Drive (Sign in or import rclone.conf)</div>
                <div className="muted">
                  {hasCloud
                    ? `${cloudProfiles.length} cloud sync profile(s) configured`
                    : '2-way strictly additive backup — uploads shoots & pulls on-the-go Drive additions'}
                </div>
              </div>
            </div>
            <button
              type="button"
              className={`button ${!hasCloud && hasLibraryRoot ? 'button-primary' : ''}`}
              onClick={() => setActiveNav('cloud')}
            >
              {hasCloud ? 'Manage' : 'Set Up Cloud'}
            </button>
          </div>

          <div className="row-between" style={{ padding: '6px 0' }}>
            <div className="row">
              {hasExternal && hasGpxFolder ? (
                <CheckCircle2 size={16} color="var(--ok)" />
              ) : (
                <HardDrive size={16} color="var(--text-secondary)" />
              )}
              <div>
                <div style={{ fontWeight: 500 }}>3. Optional: External Backup Drive &amp; Watched GPX Folder</div>
                <div className="muted">
                  Auto-detect your external SSD by Volume UUID and auto-match .gpx tracks on SD insert
                </div>
              </div>
            </div>
            <div className="row">
              <button type="button" className="button" onClick={() => void pickGpxFolder()}>
                <MapPin size={13} /> {hasGpxFolder ? 'GPX Set' : 'GPX Folder'}
              </button>
              <button type="button" className="button" onClick={() => setActiveNav('backup')}>
                {hasExternal ? 'Drives Set' : 'Add Drive'}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
