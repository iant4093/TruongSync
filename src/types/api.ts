export type NavTab = 'import' | 'geotag' | 'cloud' | 'backup' | 'activity' | 'settings';

export interface BootstrapInfo {
  version: string;
  platform: string;
  data_dir: string;
  on_battery: boolean;
  initial_import_volume: string | null;
}

export interface UpdateFeedStatus {
  available: boolean;
  version: string | null;
  notes: string | null;
}

export interface UpdateDownloadProgress {
  downloaded: number;
  total: number | null;
  finished: boolean;
}

export interface ToolAvailability {
  available: boolean;
  bundled: boolean;
  path: string | null;
  version: string | null;
}

export interface ToolsStatus {
  exiftool: ToolAvailability;
  rclone: ToolAvailability;
  rclone_config_exists: boolean;
  rclone_remotes: string[];
}

export interface AppSettings {
  default_import_root: string;
  default_categories: string[];
  date_format: string;
  raw_folder_name: string;
  jpeg_folder_name: string;
  video_folder_name: string;
  folder_template: string;
  raw_extensions: string[];
  jpeg_extensions: string[];
  video_extensions: string[];
  transfer_mode: string;
  import_threads: number;
  deep_verify_move: boolean;
  eject_after_move: boolean;
  watched_gpx_folder: string | null;
  default_max_gps_gap_secs: number;
  overwrite_existing_gps: boolean;
  bump_mtime_on_backfill: boolean;
  bandwidth_limit_mbps: number | null;
  prevent_sleep_during_transfers: boolean;
  sync_on_battery: boolean;
  auto_open_on_sd_insert: boolean;
  prompt_on_backup_drive: boolean;
  os_native_auto_launch: boolean;
  write_external_timestamp_file: boolean;
  rclone_path_override: string | null;
  exiftool_path_override: string | null;
  stale_warning_days: number;
}

export interface ImportPreset {
  id: string;
  name: string;
  root_path: string;
  folder_template: string;
  date_format: string;
  raw_folder: string;
  jpeg_folder: string;
  video_folder: string;
  default_categories: string[];
  transfer_mode: string;
  auto_geotag: boolean;
  auto_cloud_sync: boolean;
  auto_external_sync: boolean;
  eject_after: boolean;
  is_default: boolean;
}

export interface DetectedVolume {
  mount_path: string;
  label: string;
  volume_uuid: string;
  is_removable: boolean;
  has_dcim: boolean;
  writable: boolean;
  total_bytes: number;
  free_bytes: number;
  bus_protocol: string;
}

export interface ScannedMediaFile {
  path: string;
  filename: string;
  kind: 'raw' | 'jpeg' | 'video';
  size_bytes: number;
  captured_date: string;
  captured_at_utc: string | null;
  suspicious_zero_bytes: boolean;
}

export interface DateGroupSummary {
  date: string;
  raw_count: number;
  jpeg_count: number;
  video_count: number;
  total_bytes: number;
}

export interface CardScanResult {
  source_root: string;
  files: ScannedMediaFile[];
  date_groups: DateGroupSummary[];
  raw_count: number;
  jpeg_count: number;
  video_count: number;
  total_bytes: number;
  earliest_utc: string | null;
  latest_utc: string | null;
  ignored_mac_metadata_count: number;
  zero_byte_files: string[];
  unreadable_files: string[];
}

export interface PreflightReport {
  source_readable: boolean;
  source_writable: boolean;
  dest_free_bytes: number | null;
  required_bytes: number;
  safety_buffer_bytes: number;
  space_ok: boolean;
  zero_byte_files: string[];
  unreadable_files: string[];
  warnings: string[];
  blockers: string[];
}

export interface ShootAssignment {
  dates: string[];
  shoot_name: string;
  category: string;
  folder_date: string;
}

export interface ImportPlanItem {
  source_path: string;
  dest_path: string;
  shoot_root: string;
  kind: 'raw' | 'jpeg' | 'video';
  size_bytes: number;
  captured_date: string;
}

export interface ImportPipelineRequest {
  source_root: string;
  dest_root: string;
  folder_template: string;
  date_format: string;
  raw_folder: string;
  jpeg_folder: string;
  video_folder: string;
  assignments: ShootAssignment[];
  is_move: boolean;
  eject_after: boolean;
  geotag_enabled: boolean;
  gpx_files: string[];
  clock_offset_secs: number;
  max_gap_secs: number;
  overwrite_existing_gps: boolean;
  cloud_profile_ids: string[];
  external_profile_ids: string[];
}

export interface PipelineSummary {
  job_id: string;
  import: {
    files_total: number;
    files_moved: number;
    files_skipped: number;
    files_failed: number;
    bytes_transferred: number;
    duration_ms: number;
    shoot_roots: string[];
    dest_photo_paths: string[];
    failures: [string, string][];
  };
  geotag: GeotagExecutionResult | null;
  cloud_synced_profiles: number;
  external_synced_profiles: number;
  ejected: boolean;
  stage_errors?: number;
  notes: string[];
}

export interface WatchedGpxMatch {
  path: string;
  filename: string;
  point_count: number;
  start_utc: string;
  end_utc: string;
  overlap_seconds: number;
}

export type MatchStatus =
  | 'matched'
  | 'already_tagged'
  | 'out_of_range'
  | 'no_timestamp'
  | 'skipped_video';

export interface PhotoGpsMatch {
  path: string;
  filename: string;
  media_kind: string;
  capture_time_utc: string | null;
  has_existing_gps: boolean;
  status: MatchStatus;
  lat: number | null;
  lon: number | null;
  ele: number | null;
  nearest_point_delta_secs: number | null;
}

export interface GeotagPlan {
  gpx_files: string[];
  track_point_count: number;
  track_start_utc: string | null;
  track_end_utc: string | null;
  preview_lines: [number, number][][];
  items: PhotoGpsMatch[];
  matched_count: number;
  already_tagged_count: number;
  out_of_range_count: number;
  skipped_video_count: number;
}

export interface GeotagExecutionResult {
  tagged_count: number;
  skipped_count: number;
  failed_count: number;
  updated_records: { path: string; pre_md5: string; post_md5: string }[];
  failures: [string, string][];
}

export interface CloudProfile {
  id: string;
  name: string;
  local_root: string;
  remote_name: string;
  remote_path: string;
  enabled: boolean;
  schedule_mode: string;
  schedule_value: string;
  schedule_action: string;
  last_sync_at: string | null;
  last_verify_at: string | null;
  last_status: string;
  last_error: string | null;
  pending_upload_count: number;
  pending_download_count: number;
  pending_bytes: number;
  stale_after_days: number;
}

export interface ExternalProfile {
  id: string;
  name: string;
  local_root: string;
  volume_uuid: string;
  volume_label: string;
  relative_dest_path: string;
  direction: string;
  enabled: boolean;
  schedule_mode: string;
  schedule_value: string;
  schedule_action: string;
  write_marker_file: boolean;
  last_sync_at: string | null;
  last_verify_at: string | null;
  last_status: string;
  last_error: string | null;
  pending_push_count: number;
  pending_pull_count: number;
  pending_bytes: number;
  stale_after_days: number;
}

export interface SyncDiffItem {
  rel_path: string;
  direction: string;
  local_bytes: number | null;
  remote_bytes: number | null;
  note: string | null;
}

export interface CloudVerifyReport {
  profile_id: string;
  checked_at: string;
  local_file_count: number;
  remote_file_count: number;
  up_to_date_count: number;
  missing_on_remote: SyncDiffItem[];
  missing_on_local: SyncDiffItem[];
  tombstoned_local: SyncDiffItem[];
  backfill_updates: SyncDiffItem[];
  conflicts: SyncDiffItem[];
  duplicate_remote_paths: string[];
  ignored_mac_metadata_count: number;
  pending_bytes: number;
}

export interface CloudSyncResult {
  profile_id: string;
  downloaded_count: number;
  uploaded_count: number;
  backfill_replaced_count: number;
  failed_count: number;
  conflicts_count: number;
  skipped_tombstones: number;
  duration_ms: number;
  notes: string[];
}

export interface ExternalVerifyReport {
  profile_id: string;
  checked_at: string;
  volume_connected: boolean;
  mounted_path: string | null;
  local_file_count: number;
  external_file_count: number;
  up_to_date_count: number;
  to_push: SyncDiffItem[];
  to_pull: SyncDiffItem[];
  tombstoned_local: SyncDiffItem[];
  backfill_updates: SyncDiffItem[];
  conflicts: SyncDiffItem[];
  ignored_mac_metadata_count: number;
  pending_bytes: number;
}

export interface ExternalSyncResult {
  profile_id: string;
  pulled_count: number;
  pushed_count: number;
  backfill_replaced_count: number;
  failed_count: number;
  bytes_transferred: number;
  duration_ms: number;
  failures: [string, string][];
}

export interface JobRecord {
  id: string;
  kind: string;
  title: string;
  status: string;
  started_at: string;
  finished_at: string | null;
  files_total: number;
  files_done: number;
  files_skipped: number;
  files_failed: number;
  bytes_total: number;
  bytes_done: number;
  summary_json: string | null;
  error_message: string | null;
}

export interface JobItemRecord {
  id: number;
  job_id: string;
  stage: string;
  source_path: string;
  dest_path: string | null;
  status: string;
  bytes: number;
  message: string | null;
  created_at: string;
}

export interface JobProgressEvent {
  job_id: string;
  stage: string;
  message: string;
  files_done: number;
  files_total: number;
  bytes_done: number;
  bytes_total: number;
  speed_bps: number;
}
