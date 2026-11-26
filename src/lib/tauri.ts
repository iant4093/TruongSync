import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import type {
  AppSettings,
  BootstrapInfo,
  CardScanResult,
  CloudProfile,
  CloudSyncResult,
  CloudVerifyReport,
  DetectedVolume,
  ExternalProfile,
  ExternalSyncResult,
  ExternalVerifyReport,
  GeotagExecutionResult,
  GeotagPlan,
  ImportPipelineRequest,
  ImportPlanItem,
  ImportPreset,
  JobItemRecord,
  JobRecord,
  PhotoGpsMatch,
  PipelineSummary,
  PreflightReport,
  ShootAssignment,
  ToolsStatus,
  UpdateFeedStatus,
  WatchedGpxMatch,
} from '../types/api';

export const isTauri = (): boolean =>
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

export async function pickDirectory(title: string): Promise<string | null> {
  if (!isTauri()) return null;
  const res = await openDialog({ directory: true, multiple: false, title });
  return typeof res === 'string' ? res : null;
}

export async function pickFiles(
  title: string,
  extensions: string[],
  multiple = true,
): Promise<string[]> {
  if (!isTauri()) return [];
  const res = await openDialog({
    directory: false,
    multiple,
    title,
    filters: extensions.length > 0 ? [{ name: title, extensions }] : undefined,
  });
  if (Array.isArray(res)) return res;
  if (typeof res === 'string') return [res];
  return [];
}

export async function openExternalUrl(url: string): Promise<void> {
  if (isTauri()) {
    const { open } = await import('@tauri-apps/plugin-shell');
    await open(url);
    return;
  }
  if (typeof window !== 'undefined') {
    window.open(url, '_blank', 'noopener,noreferrer');
  }
}

export const api = {
  getBootstrapInfo: () => invoke<BootstrapInfo>('get_bootstrap_info'),
  checkTools: () => invoke<ToolsStatus>('check_tools'),
  checkUpdateFeed: () => invoke<UpdateFeedStatus>('check_update_feed'),
  installNativeUpdate: () => invoke<void>('install_native_update'),
  getSettings: () => invoke<AppSettings>('get_settings'),
  saveSettings: (settings: AppSettings) => invoke<void>('save_settings', { settings }),
  setOsNativeAutoLaunch: (enabled: boolean) =>
    invoke<string>('set_os_native_auto_launch', { enabled }),

  listVolumes: () => invoke<DetectedVolume[]>('list_volumes'),
  ejectVolume: (mountPath: string) => invoke<void>('eject_volume_cmd', { mountPath }),
  listYearCategories: (baseRoot: string, year: number, createIfMissing = false) =>
    invoke<string[]>('list_year_categories', { baseRoot, year, createIfMissing }),
  createCategoryFolder: (baseRoot: string, year: number, category: string) =>
    invoke<string>('create_category_folder', { baseRoot, year, category }),

  listImportPresets: () => invoke<ImportPreset[]>('list_import_presets'),
  saveImportPreset: (preset: ImportPreset) => invoke<void>('save_import_preset', { preset }),
  deleteImportPreset: (id: string) => invoke<void>('delete_import_preset', { id }),

  scanCard: (sourceRoot: string) => invoke<CardScanResult>('scan_card', { sourceRoot }),
  preflightImport: (scan: CardScanResult, destRoot: string, isMove: boolean) =>
    invoke<PreflightReport>('preflight_import', { scan, destRoot, isMove }),
  previewImportPlan: (params: {
    scan: CardScanResult;
    destRoot: string;
    folderTemplate: string;
    dateFormat: string;
    rawFolder: string;
    jpegFolder: string;
    videoFolder: string;
    assignments: ShootAssignment[];
  }) => invoke<ImportPlanItem[]>('preview_import_plan', params),
  runImportPipeline: (req: ImportPipelineRequest) =>
    invoke<PipelineSummary>('run_import_pipeline', { req }),
  cancelActiveJob: (jobId?: string) =>
    invoke<boolean>('cancel_active_job', { jobId: jobId ?? null }),
  cancelOAuth: () => invoke<boolean>('cancel_oauth'),

  findMatchingGpx: (earliestUtc: string, latestUtc: string, folderOverride?: string) =>
    invoke<WatchedGpxMatch[]>('find_matching_gpx', {
      earliestUtc,
      latestUtc,
      folderOverride: folderOverride ?? null,
    }),
  previewGeotag: (params: {
    targetPaths: string[];
    gpxFiles: string[];
    clockOffsetSecs: number;
    maxGapSecs: number;
    overwriteExisting: boolean;
  }) => invoke<GeotagPlan>('preview_geotag', params),
  runGeotag: (items: PhotoGpsMatch[], recordBackfillUpdates: boolean) =>
    invoke<GeotagExecutionResult>('run_geotag', { items, recordBackfillUpdates }),

  listCloudProfiles: () => invoke<CloudProfile[]>('list_cloud_profiles'),
  saveCloudProfile: (profile: CloudProfile) => invoke<string>('save_cloud_profile', { profile }),
  deleteCloudProfile: (id: string) => invoke<void>('delete_cloud_profile', { id }),
  importRcloneConf: (sourcePath: string) => invoke<string[]>('import_rclone_conf', { sourcePath }),
  listRcloneRemotes: () => invoke<string[]>('list_rclone_remotes_cmd'),
  connectGoogleDrive: (remoteName: string, clientId?: string, clientSecret?: string) =>
    invoke<string[]>('connect_google_drive', {
      remoteName,
      clientId: clientId || null,
      clientSecret: clientSecret || null,
    }),
  verifyCloud: (profileId: string, deepMd5 = false) =>
    invoke<CloudVerifyReport>('verify_cloud_cmd', { profileId, deepMd5 }),
  syncCloud: (profileId: string, restoreMode = false) =>
    invoke<CloudSyncResult>('sync_cloud_cmd', { profileId, restoreMode }),

  listExternalProfiles: () => invoke<ExternalProfile[]>('list_external_profiles'),
  saveExternalProfile: (profile: ExternalProfile) =>
    invoke<string>('save_external_profile', { profile }),
  deleteExternalProfile: (id: string) => invoke<void>('delete_external_profile', { id }),
  verifyExternal: (profileId: string, deepMd5 = false) =>
    invoke<ExternalVerifyReport>('verify_external_cmd', { profileId, deepMd5 }),
  syncExternal: (profileId: string, restoreMode = false) =>
    invoke<ExternalSyncResult>('sync_external_cmd', { profileId, restoreMode }),

  listJobs: (limit = 100) => invoke<JobRecord[]>('list_jobs', { limit }),
  listJobItems: (jobId: string, limit = 500) =>
    invoke<JobItemRecord[]>('list_job_items', { jobId, limit }),
};

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let val = bytes / 1024;
  let u = 0;
  while (val >= 1024 && u < units.length - 1) {
    val /= 1024;
    u++;
  }
  return `${val >= 100 ? val.toFixed(0) : val >= 10 ? val.toFixed(1) : val.toFixed(2)} ${units[u]}`;
}

export function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  const secs = Math.round(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m ${s}s`;
}

export function formatRelativeAge(iso: string | null | undefined): {
  label: string;
  days: number | null;
} {
  if (!iso) return { label: 'Never synced', days: null };
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return { label: iso, days: null };
  const diffMs = Math.max(0, Date.now() - t);
  const mins = Math.floor(diffMs / 60000);
  if (mins < 1) return { label: 'Just now', days: 0 };
  if (mins < 60) return { label: `${mins}m ago`, days: 0 };
  const hours = Math.floor(mins / 60);
  if (hours < 24) return { label: `${hours}h ago`, days: 0 };
  const days = Math.floor(hours / 24);
  return { label: `${days}d ago`, days };
}
