-- TruongSync initial schema (WAL mode)
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS import_presets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_path TEXT NOT NULL,
    folder_template TEXT NOT NULL,
    date_format TEXT NOT NULL,
    raw_folder TEXT NOT NULL,
    jpeg_folder TEXT NOT NULL,
    video_folder TEXT NOT NULL,
    default_categories_json TEXT NOT NULL,
    transfer_mode TEXT NOT NULL DEFAULT 'move',
    auto_geotag INTEGER NOT NULL DEFAULT 0,
    auto_cloud_sync INTEGER NOT NULL DEFAULT 1,
    auto_external_sync INTEGER NOT NULL DEFAULT 0,
    eject_after INTEGER NOT NULL DEFAULT 0,
    is_default INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cloud_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    local_root TEXT NOT NULL,
    remote_name TEXT NOT NULL,
    remote_path TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    schedule_mode TEXT NOT NULL DEFAULT 'manual',
    schedule_value TEXT NOT NULL DEFAULT '',
    schedule_action TEXT NOT NULL DEFAULT 'sync',
    last_sync_at TEXT,
    last_verify_at TEXT,
    last_status TEXT NOT NULL DEFAULT 'never',
    last_error TEXT,
    pending_upload_count INTEGER NOT NULL DEFAULT 0,
    pending_download_count INTEGER NOT NULL DEFAULT 0,
    pending_bytes INTEGER NOT NULL DEFAULT 0,
    stale_after_days INTEGER NOT NULL DEFAULT 7,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS external_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    local_root TEXT NOT NULL,
    volume_uuid TEXT NOT NULL,
    volume_label TEXT NOT NULL,
    relative_dest_path TEXT NOT NULL,
    direction TEXT NOT NULL DEFAULT 'two_way_additive',
    enabled INTEGER NOT NULL DEFAULT 1,
    schedule_mode TEXT NOT NULL DEFAULT 'on_connect',
    schedule_value TEXT NOT NULL DEFAULT '',
    schedule_action TEXT NOT NULL DEFAULT 'check',
    write_marker_file INTEGER NOT NULL DEFAULT 1,
    last_sync_at TEXT,
    last_verify_at TEXT,
    last_status TEXT NOT NULL DEFAULT 'never',
    last_error TEXT,
    pending_push_count INTEGER NOT NULL DEFAULT 0,
    pending_pull_count INTEGER NOT NULL DEFAULT 0,
    pending_bytes INTEGER NOT NULL DEFAULT 0,
    stale_after_days INTEGER NOT NULL DEFAULT 14,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_manifest (
    profile_kind TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    md5_hash TEXT,
    mtime_unix INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'synced',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (profile_kind, profile_id, rel_path)
);

CREATE INDEX IF NOT EXISTS idx_sync_manifest_profile
    ON sync_manifest (profile_kind, profile_id, state);

CREATE TABLE IF NOT EXISTS intentional_updates (
    id TEXT PRIMARY KEY,
    abs_path TEXT NOT NULL,
    pre_md5 TEXT NOT NULL,
    post_md5 TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT 'gps_backfill',
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_intentional_updates_path
    ON intentional_updates (abs_path);

CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    files_total INTEGER NOT NULL DEFAULT 0,
    files_done INTEGER NOT NULL DEFAULT 0,
    files_skipped INTEGER NOT NULL DEFAULT 0,
    files_failed INTEGER NOT NULL DEFAULT 0,
    bytes_total INTEGER NOT NULL DEFAULT 0,
    bytes_done INTEGER NOT NULL DEFAULT 0,
    summary_json TEXT,
    error_message TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_started_at ON jobs (started_at DESC);

CREATE TABLE IF NOT EXISTS job_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    stage TEXT NOT NULL,
    source_path TEXT NOT NULL,
    dest_path TEXT,
    status TEXT NOT NULL,
    bytes INTEGER NOT NULL DEFAULT 0,
    message TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_job_items_job_id ON job_items (job_id);
