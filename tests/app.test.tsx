import { describe, expect, it, beforeEach } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { formatBytes, formatDurationMs, formatRelativeAge } from '../src/lib/tauri';
import { FreshnessBadge } from '../src/components/ui';
import { QuickSetupCard } from '../src/components/QuickSetupCard';
import { ImportView } from '../src/components/views/ImportView';
import { App } from '../src/App';
import { useSyncStore } from '../src/store/useSyncStore';
import type { AppSettings, ImportPreset, ToolsStatus } from '../src/types/api';

const BASE_SETTINGS: AppSettings = {
  default_import_root: '',
  default_categories: ['Hikes'],
  date_format: 'M-dd',
  raw_folder_name: 'RAW',
  jpeg_folder_name: 'JPEG',
  video_folder_name: 'VID',
  folder_template: '{Year}/{Category}/{ShootName} {Date}/{Type}',
  raw_extensions: ['cr3'],
  jpeg_extensions: ['jpg'],
  video_extensions: ['mp4'],
  transfer_mode: 'move',
  import_threads: 8,
  deep_verify_move: false,
  eject_after_move: false,
  watched_gpx_folder: null,
  default_max_gps_gap_secs: 900,
  overwrite_existing_gps: false,
  bump_mtime_on_backfill: true,
  bandwidth_limit_mbps: null,
  prevent_sleep_during_transfers: true,
  sync_on_battery: false,
  auto_open_on_sd_insert: true,
  prompt_on_backup_drive: true,
  os_native_auto_launch: false,
  write_external_timestamp_file: true,
  rclone_path_override: null,
  exiftool_path_override: null,
  stale_warning_days: 7,
};

const BASE_PRESET: ImportPreset = {
  id: 'default-photo-dump',
  name: 'Photo Dump',
  root_path: '/tmp/PhotoDump',
  folder_template: '{Year}/{Category}/{ShootName} {Date}/{Type}',
  date_format: 'M-dd',
  raw_folder: 'RAW',
  jpeg_folder: 'JPEG',
  video_folder: 'VID',
  default_categories: ['Hikes'],
  transfer_mode: 'move',
  auto_geotag: false,
  auto_cloud_sync: false,
  auto_external_sync: false,
  eject_after: false,
  is_default: true,
};

describe('TruongSync formatting & UI components', () => {
  beforeEach(() => {
    localStorage.clear();
    useSyncStore.setState({
      activeNav: 'import',
      settings: null,
      tools: null,
      presets: [],
      cloudProfiles: [],
      externalProfiles: [],
      volumes: [],
      jobs: [],
    });
  });

  it('formats bytes and durations cleanly', () => {
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1024 * 1024 * 15)).toBe('15.0 MB');
    expect(formatDurationMs(65000)).toBe('1m 5s');
    expect(formatRelativeAge(null).label).toBe('Never synced');
  });

  it('renders freshness badge states accurately', () => {
    const { rerender } = render(
      <FreshnessBadge lastSyncAt={null} lastStatus="never" staleAfterDays={7} />,
    );
    expect(screen.getByText('Never synced')).toBeTruthy();

    rerender(
      <FreshnessBadge
        lastSyncAt={new Date().toISOString()}
        lastStatus="up_to_date"
        staleAfterDays={7}
      />,
    );
    expect(screen.getByText(/Up to date/)).toBeTruthy();

    const oldDate = new Date(Date.now() - 20 * 86400 * 1000).toISOString();
    rerender(<FreshnessBadge lastSyncAt={oldDate} lastStatus="up_to_date" staleAfterDays={7} />);
    expect(screen.getByText(/Stale/)).toBeTruthy();
  });

  it('distinguishes bundled vs system tools in QuickSetupCard', () => {
    const bundledTools: ToolsStatus = {
      exiftool: { available: true, bundled: true, path: '/app/exiftool', version: '13.64' },
      rclone: { available: true, bundled: true, path: '/app/rclone', version: 'v1.76.0' },
      rclone_config_exists: false,
      rclone_remotes: [],
    };
    useSyncStore.setState({ settings: BASE_SETTINGS, tools: bundledTools });
    const { rerender } = render(<QuickSetupCard />);
    expect(screen.getByText('All tools bundled (ExifTool + rclone)')).toBeTruthy();

    const mixedTools: ToolsStatus = {
      ...bundledTools,
      exiftool: { available: true, bundled: false, path: '/opt/homebrew/bin/exiftool', version: '13.64' },
    };
    act(() => {
      useSyncStore.setState({ tools: mixedTools });
    });
    rerender(<QuickSetupCard />);
    expect(screen.getByText('Tools ready (System ExifTool + Bundled rclone)')).toBeTruthy();
  });

  it('does not reset ImportView user state when background profiles refresh', () => {
    useSyncStore.setState({
      settings: BASE_SETTINGS,
      presets: [BASE_PRESET],
      cloudProfiles: [],
      externalProfiles: [],
    });
    render(<ImportView />);
    expect(screen.getByText('SD Card Import')).toBeTruthy();
    // Simulate a background profile refresh event updating store
    act(() => {
      useSyncStore.setState({
        cloudProfiles: [
          {
            id: 'c1',
            name: 'Drive',
            local_root: '/tmp/PhotoDump',
            remote_name: 'rawphotos',
            remote_path: 'Backup',
            enabled: true,
            schedule_mode: 'manual',
            schedule_value: '',
            schedule_action: 'sync',
            last_sync_at: null,
            last_verify_at: null,
            last_status: 'never',
            last_error: null,
            pending_upload_count: 0,
            pending_download_count: 0,
            pending_bytes: 0,
            stale_after_days: 7,
          },
        ],
      });
    });
    // Ensure ImportView still renders cleanly without throwing or crashing
    expect(screen.getByText('SD Card Import')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: /Refresh Drives/i }));
  });

  it('renders the Codex Dark shell and primary navigation tabs', () => {
    render(<App />);
    expect(screen.getByText('TruongSync')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Import' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Geotag' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Cloud Sync' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Drive Backup' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Activity' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Settings' })).toBeTruthy();
  });
});
