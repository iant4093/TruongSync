import React, { useEffect, useLayoutEffect, useRef } from 'react';
import { BatteryCharging } from 'lucide-react';
import { isTauri } from './lib/tauri';
import { useSyncStore } from './store/useSyncStore';
import type { NavTab } from './types/api';
import { FeedbackHost } from './components/Feedback';
import { Sidebar } from './components/Sidebar';
import { Badge } from './components/ui';
import { WindowsCaptionButtons, ensureWindowStateWatcher, isWindowsPlatform } from './components/WindowControls';
import { ActivityView, JobInspectorDrawer } from './components/views/ActivityView';
import { CloudSyncView } from './components/views/CloudSyncView';
import { DriveBackupView } from './components/views/DriveBackupView';
import { GeotagView } from './components/views/GeotagView';
import { ImportView } from './components/views/ImportView';
import { SettingsView } from './components/views/SettingsView';

const nativeMacTitlebar = typeof navigator !== 'undefined' && /Mac/.test(navigator.platform);
const windowsCaption = !nativeMacTitlebar && isWindowsPlatform();

const NAV_TITLES: Record<NavTab, string> = {
  import: 'SD Card Import',
  geotag: 'EXIF GPS Geotagging',
  cloud: 'Google Drive Cloud Sync',
  backup: 'External Drive Backup',
  activity: 'Activity & History',
  settings: 'Settings',
};

const MemoImportView = React.memo(ImportView);
const MemoGeotagView = React.memo(GeotagView);
const MemoCloudSyncView = React.memo(CloudSyncView);
const MemoDriveBackupView = React.memo(DriveBackupView);
const MemoActivityView = React.memo(ActivityView);
const MemoSettingsView = React.memo(SettingsView);

const HeaderProgressBadge: React.FC = () => {
  const message = useSyncStore((s) => s.activeProgress?.message ?? null);
  if (!message) return null;
  return <Badge tone="info">{message}</Badge>;
};

export const App: React.FC = () => {
  const activeNav = useSyncStore((s) => s.activeNav);
  const onBattery = useSyncStore((s) => Boolean(s.bootstrap?.on_battery));
  const inspectorOpen = useSyncStore((s) =>
    Boolean(s.activeNav === 'activity' && s.inspectorJobId && s.jobs.some((j) => j.id === s.inspectorJobId)),
  );
  const init = useSyncStore((s) => s.init);
  const viewportRef = useRef<HTMLDivElement>(null);
  const scrollByTabRef = useRef<Partial<Record<NavTab, number>>>({});

  useLayoutEffect(() => {
    const vp = viewportRef.current;
    if (!vp) return;
    vp.scrollTop = scrollByTabRef.current[activeNav] ?? 0;
  }, [activeNav]);

  useEffect(() => {
    void init();
    if (isTauri()) {
      ensureWindowStateWatcher();
      if (windowsCaption) {
        document.documentElement.classList.add('tauri-frameless');
        document.body.classList.add('tauri-transparent');
      }
    }
  }, [init]);

  const frameClass = [
    'app-window-frame studio-shell',
    nativeMacTitlebar ? 'native-titlebar' : '',
    windowsCaption ? 'windows-caption' : '',
  ]
    .filter(Boolean)
    .join(' ');

  return (
    <div className={frameClass}>
      <Sidebar />
      <main className="workspace">
        <header className="workspace-header" data-tauri-drag-region>
          <span className="workspace-title" data-tauri-drag-region>
            {NAV_TITLES[activeNav]}
          </span>
          <div className="workspace-controls">
            <HeaderProgressBadge />
            {onBattery && (
              <Badge tone="warn">
                <BatteryCharging size={12} /> Battery
              </Badge>
            )}
          </div>
          {isTauri() && windowsCaption && <WindowsCaptionButtons />}
        </header>

        <div className={`workspace-body ${inspectorOpen ? 'with-inspector' : ''}`}>
          <div
            className="content-viewport"
            ref={viewportRef}
            onScroll={(e) => {
              scrollByTabRef.current[activeNav] = e.currentTarget.scrollTop;
            }}
          >
            <div style={{ display: activeNav === 'import' ? undefined : 'none' }}>
              <MemoImportView />
            </div>
            <div style={{ display: activeNav === 'geotag' ? undefined : 'none' }}>
              <MemoGeotagView />
            </div>
            <div style={{ display: activeNav === 'cloud' ? undefined : 'none' }}>
              <MemoCloudSyncView />
            </div>
            <div style={{ display: activeNav === 'backup' ? undefined : 'none' }}>
              <MemoDriveBackupView />
            </div>
            <div style={{ display: activeNav === 'activity' ? undefined : 'none' }}>
              <MemoActivityView />
            </div>
            <div style={{ display: activeNav === 'settings' ? undefined : 'none' }}>
              <MemoSettingsView />
            </div>
          </div>
          <JobInspectorDrawer />
        </div>
      </main>
      <FeedbackHost />
    </div>
  );
};
