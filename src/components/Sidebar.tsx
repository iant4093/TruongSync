import React from 'react';
import {
  Activity,
  CloudUpload,
  HardDrive,
  HardDriveDownload,
  MapPin,
  Settings as SettingsIcon,
} from 'lucide-react';
import { isTauri } from '../lib/tauri';
import { useSyncStore } from '../store/useSyncStore';
import type { NavTab } from '../types/api';
import { StatusBanner } from './Feedback';
import { isWindowsPlatform } from './WindowControls';

const NAV_ITEMS: ReadonlyArray<
  readonly [NavTab, string, React.ComponentType<{ size?: number; strokeWidth?: number }>]
> = [
  ['import', 'Import', HardDriveDownload],
  ['geotag', 'Geotag', MapPin],
  ['cloud', 'Cloud Sync', CloudUpload],
  ['backup', 'Drive Backup', HardDrive],
  ['activity', 'Activity', Activity],
];

const nativeMacTitlebar = typeof navigator !== 'undefined' && /Mac/.test(navigator.platform);
const windowsCaption = !nativeMacTitlebar && isWindowsPlatform();

export const Sidebar: React.FC = () => {
  const activeNav = useSyncStore((s) => s.activeNav);
  const setActiveNav = useSyncStore((s) => s.setActiveNav);
  const volumes = useSyncStore((s) => s.volumes);
  const cloudProfiles = useSyncStore((s) => s.cloudProfiles);
  const activeProgress = useSyncStore((s) => s.activeProgress);
  const tools = useSyncStore((s) => s.tools);

  const sdCount = volumes.filter((v) => v.has_dcim).length;
  const driveConnected = Boolean(tools?.rclone.available && tools?.rclone_config_exists);
  const staleCloud = cloudProfiles.some(
    (p) => p.last_status === 'failed' || p.last_status === 'behind' || p.last_status === 'needs_review',
  );

  return (
    <aside className="app-sidebar">
      {isTauri() && !windowsCaption && <div className="sidebar-titlebar" data-tauri-drag-region />}
      <div className="app-brand" data-tauri-drag-region>
        <img src="/app-icon.png" width={23} height={23} alt="" />
        <span>TruongSync</span>
      </div>
      <nav aria-label="Primary navigation" className="sidebar-nav">
        {NAV_ITEMS.map(([key, label, Icon]) => (
          <button
            key={key}
            type="button"
            aria-current={activeNav === key ? 'page' : undefined}
            className={`nav-item ${activeNav === key ? 'is-active' : ''}`}
            onClick={() => setActiveNav(key)}
          >
            <Icon size={16} strokeWidth={1.6} />
            <span>{label}</span>
          </button>
        ))}

        <div className="sidebar-spacer" />
        <StatusBanner />

        <button
          type="button"
          className={`sidebar-resources ${activeNav === 'activity' ? 'is-active' : ''}`}
          onClick={() => setActiveNav('activity')}
        >
          <div className="resource-row">
            <span>SD Cards</span>
            <span>{sdCount > 0 ? `${sdCount} ready` : 'None'}</span>
          </div>
          <div className="resource-row">
            <span>Drive</span>
            <span className="row" style={{ gap: 5 }}>
              <span
                className={`ws-dot ${driveConnected ? (staleCloud ? 'is-warn' : 'is-connected') : ''}`}
                aria-hidden="true"
              />
              <span>{driveConnected ? (staleCloud ? 'Pending' : 'Ready') : 'Not linked'}</span>
            </span>
          </div>
          <div className="resource-row">
            <span>Queue</span>
            <span>{activeProgress ? 'Running…' : 'Idle'}</span>
          </div>
        </button>

        <button
          type="button"
          aria-current={activeNav === 'settings' ? 'page' : undefined}
          className={`nav-item ${activeNav === 'settings' ? 'is-active' : ''}`}
          onClick={() => setActiveNav('settings')}
        >
          <SettingsIcon size={16} strokeWidth={1.6} />
          <span>Settings</span>
        </button>
      </nav>
    </aside>
  );
};
