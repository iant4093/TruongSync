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

export const App: React.FC = () => {
  const activeNav = useSyncStore((s) => s.activeNav);
  const bootstrap = useSyncStore((s) => s.bootstrap);
  const activeProgress = useSyncStore((s) => s.activeProgress);
  const inspectorJobId = useSyncStore((s) => s.inspectorJobId);
  const jobs = useSyncStore((s) => s.jobs);
  const init = useSyncStore((s) => s.init);
  const inspectorOpen = Boolean(
    activeNav === 'activity' && inspectorJobId && jobs.some((j) => j.id === inspectorJobId),
  );
  const viewportRef = useRef<HTMLDivElement>(null);
  const scrollByTabRef = useRef<Partial<Record<NavTab, number>>>({});
  const prevTabRef = useRef<NavTab>(activeNav);

  useLayoutEffect(() => {
    const vp = viewportRef.current;
    if (!vp) return;
    if (prevTabRef.current !== activeNav) {
      scrollByTabRef.current[prevTabRef.current] = vp.scrollTop;
      prevTabRef.current = activeNav;
      vp.scrollTop = scrollByTabRef.current[activeNav] ?? 0;
    }
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
            {activeProgress && <Badge tone="info">{activeProgress.message}</Badge>}
            {bootstrap?.on_battery && (
              <Badge tone="warn">
                <BatteryCharging size={12} /> Battery
              </Badge>
            )}
          </div>
          {isTauri() && windowsCaption && <WindowsCaptionButtons />}
        </header>

        <div className={`workspace-body ${inspectorOpen ? 'with-inspector' : ''}`}>
          <div className="content-viewport" ref={viewportRef}>
            <div style={{ display: activeNav === 'import' ? undefined : 'none' }}>
              <ImportView />
            </div>
            <div style={{ display: activeNav === 'geotag' ? undefined : 'none' }}>
              <GeotagView />
            </div>
            <div style={{ display: activeNav === 'cloud' ? undefined : 'none' }}>
              <CloudSyncView />
            </div>
            <div style={{ display: activeNav === 'backup' ? undefined : 'none' }}>
              <DriveBackupView />
            </div>
            <div style={{ display: activeNav === 'activity' ? undefined : 'none' }}>
              <ActivityView />
            </div>
            <div style={{ display: activeNav === 'settings' ? undefined : 'none' }}>
              <SettingsView />
            </div>
          </div>
          <JobInspectorDrawer />
        </div>
      </main>
      <FeedbackHost />
    </div>
  );
};
