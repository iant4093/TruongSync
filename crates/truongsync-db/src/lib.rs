pub mod models;

use anyhow::{Context, Result};
use chrono::Utc;
use models::*;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const MIGRATION_0001: &str = include_str!("../migrations/0001_initial.sql");
const MIGRATION_0002: &str = include_str!("../migrations/0002_backfill_targets.sql");

#[derive(Clone)]
pub struct DbHandle {
    conn: Arc<Mutex<Connection>>,
}

impl DbHandle {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open SQLite db at {}", path.display()))?;
        // Set busy_timeout BEFORE journal_mode=WAL so concurrent opens never fail immediately with SQLITE_BUSY.
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let handle = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        handle.migrate()?;
        handle.ensure_defaults()?;
        handle.recover_stale_running_jobs()?;
        Ok(handle)
    }

    /// Opens an existing SQLite database strictly read-only without running migrations,
    /// default seeding, or stale-job recovery (safe for short-lived LaunchAgent helpers).
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open SQLite db read-only at {}", path.display()))?;
        let _ = conn.pragma_update(None, "busy_timeout", 2000);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn recover_stale_running_jobs(&self) -> Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE jobs
             SET status = 'interrupted',
                 finished_at = COALESCE(finished_at, ?1),
                 error_message = COALESCE(error_message, 'Interrupted by application exit or crash')
             WHERE status = 'running'",
            params![now],
        )?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
        )?;
        let migrations: &[(i64, &str)] = &[(1, MIGRATION_0001), (2, MIGRATION_0002)];
        for (ver, sql) in migrations {
            let applied: Option<i64> = tx
                .query_row(
                    "SELECT version FROM schema_migrations WHERE version = ?1",
                    params![ver],
                    |r| r.get(0),
                )
                .optional()?;
            if applied.is_none() {
                tx.execute_batch(sql)?;
                tx.execute(
                    "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![ver, Utc::now().to_rfc3339()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn ensure_defaults(&self) -> Result<()> {
        let presets = self.list_import_presets()?;
        if presets.is_empty() {
            let defaults = self.load_settings().unwrap_or_default();
            self.upsert_import_preset(&ImportPreset {
                id: "default-photo-dump".into(),
                name: "Photography Default (Year / Category / Shoot M-dd / RAW·JPEG·VID)".into(),
                root_path: defaults.default_import_root,
                folder_template: defaults.folder_template,
                date_format: defaults.date_format,
                raw_folder: defaults.raw_folder_name,
                jpeg_folder: defaults.jpeg_folder_name,
                video_folder: defaults.video_folder_name,
                default_categories: defaults.default_categories,
                transfer_mode: defaults.transfer_mode,
                auto_geotag: false,
                auto_cloud_sync: true,
                auto_external_sync: false,
                eject_after: defaults.eject_after_move,
                is_default: true,
            })?;
        }
        Ok(())
    }

    // ── Settings ─────────────────────────────────────────────────────────────

    pub fn load_settings(&self) -> Result<AppSettings> {
        let conn = self.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT value_json FROM settings WHERE key = 'app_settings'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(s) => match serde_json::from_str::<AppSettings>(&s) {
                Ok(parsed) => Ok(parsed),
                Err(err) => {
                    tracing::error!(error = %err, "failed to deserialize app_settings; returning defaults");
                    Ok(AppSettings::default())
                }
            },
            None => Ok(AppSettings::default()),
        }
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json = serde_json::to_string(settings)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO settings (key, value_json, updated_at) VALUES ('app_settings', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
            params![json, now],
        )?;
        // Keep the built-in default preset aligned with Import Defaults edited in Settings.
        tx.execute(
            "UPDATE import_presets SET
                root_path = ?1,
                folder_template = ?2,
                date_format = ?3,
                raw_folder = ?4,
                jpeg_folder = ?5,
                video_folder = ?6,
                default_categories_json = ?7,
                transfer_mode = ?8,
                eject_after = ?9,
                updated_at = ?10
             WHERE id = 'default-photo-dump'",
            params![
                settings.default_import_root,
                settings.folder_template,
                settings.date_format,
                settings.raw_folder_name,
                settings.jpeg_folder_name,
                settings.video_folder_name,
                serde_json::to_string(&settings.default_categories)?,
                settings.transfer_mode,
                settings.eject_after_move as i64,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── Import Presets ───────────────────────────────────────────────────────

    pub fn list_import_presets(&self) -> Result<Vec<ImportPreset>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, root_path, folder_template, date_format, raw_folder, jpeg_folder, video_folder,
                    default_categories_json, transfer_mode, auto_geotag, auto_cloud_sync, auto_external_sync,
                    eject_after, is_default
             FROM import_presets ORDER BY is_default DESC, name ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let cats_raw: String = r.get(8)?;
            let default_categories: Vec<String> =
                serde_json::from_str(&cats_raw).unwrap_or_default();
            Ok(ImportPreset {
                id: r.get(0)?,
                name: r.get(1)?,
                root_path: r.get(2)?,
                folder_template: r.get(3)?,
                date_format: r.get(4)?,
                raw_folder: r.get(5)?,
                jpeg_folder: r.get(6)?,
                video_folder: r.get(7)?,
                default_categories,
                transfer_mode: r.get(9)?,
                auto_geotag: r.get::<_, i64>(10)? != 0,
                auto_cloud_sync: r.get::<_, i64>(11)? != 0,
                auto_external_sync: r.get::<_, i64>(12)? != 0,
                eject_after: r.get::<_, i64>(13)? != 0,
                is_default: r.get::<_, i64>(14)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn upsert_import_preset(&self, preset: &ImportPreset) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if preset.id.trim().is_empty() {
            Uuid::new_v4().to_string()
        } else {
            preset.id.clone()
        };
        if preset.is_default {
            tx.execute("UPDATE import_presets SET is_default = 0", [])?;
        }
        tx.execute(
            "INSERT INTO import_presets (
                id, name, root_path, folder_template, date_format, raw_folder, jpeg_folder, video_folder,
                default_categories_json, transfer_mode, auto_geotag, auto_cloud_sync, auto_external_sync,
                eject_after, is_default, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
            ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, root_path=excluded.root_path, folder_template=excluded.folder_template,
                date_format=excluded.date_format, raw_folder=excluded.raw_folder, jpeg_folder=excluded.jpeg_folder,
                video_folder=excluded.video_folder, default_categories_json=excluded.default_categories_json,
                transfer_mode=excluded.transfer_mode, auto_geotag=excluded.auto_geotag,
                auto_cloud_sync=excluded.auto_cloud_sync, auto_external_sync=excluded.auto_external_sync,
                eject_after=excluded.eject_after, is_default=excluded.is_default, updated_at=excluded.updated_at",
            params![
                id,
                preset.name,
                preset.root_path,
                preset.folder_template,
                preset.date_format,
                preset.raw_folder,
                preset.jpeg_folder,
                preset.video_folder,
                serde_json::to_string(&preset.default_categories)?,
                preset.transfer_mode,
                preset.auto_geotag as i64,
                preset.auto_cloud_sync as i64,
                preset.auto_external_sync as i64,
                preset.eject_after as i64,
                preset.is_default as i64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_import_preset(&self, id: &str) -> Result<()> {
        if id == "default-photo-dump" {
            anyhow::bail!("The built-in default preset cannot be deleted.");
        }
        let conn = self.conn.lock();
        conn.execute("DELETE FROM import_presets WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Cloud Profiles ───────────────────────────────────────────────────────

    pub fn list_cloud_profiles(&self) -> Result<Vec<CloudProfile>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, local_root, remote_name, remote_path, enabled, schedule_mode,
                    schedule_value, schedule_action, last_sync_at, last_verify_at, last_status,
                    last_error, pending_upload_count, pending_download_count, pending_bytes, stale_after_days
             FROM cloud_profiles ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CloudProfile {
                id: r.get(0)?,
                name: r.get(1)?,
                local_root: r.get(2)?,
                remote_name: r.get(3)?,
                remote_path: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                schedule_mode: r.get(6)?,
                schedule_value: r.get(7)?,
                schedule_action: r.get(8)?,
                last_sync_at: r.get(9)?,
                last_verify_at: r.get(10)?,
                last_status: r.get(11)?,
                last_error: r.get(12)?,
                pending_upload_count: r.get(13)?,
                pending_download_count: r.get(14)?,
                pending_bytes: r.get(15)?,
                stale_after_days: r.get(16)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_cloud_profile(&self, id: &str) -> Result<Option<CloudProfile>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT id, name, local_root, remote_name, remote_path, enabled, schedule_mode,
                        schedule_value, schedule_action, last_sync_at, last_verify_at, last_status,
                        last_error, pending_upload_count, pending_download_count, pending_bytes, stale_after_days
                 FROM cloud_profiles WHERE id = ?1",
                params![id],
                |r| {
                    Ok(CloudProfile {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        local_root: r.get(2)?,
                        remote_name: r.get(3)?,
                        remote_path: r.get(4)?,
                        enabled: r.get::<_, i64>(5)? != 0,
                        schedule_mode: r.get(6)?,
                        schedule_value: r.get(7)?,
                        schedule_action: r.get(8)?,
                        last_sync_at: r.get(9)?,
                        last_verify_at: r.get(10)?,
                        last_status: r.get(11)?,
                        last_error: r.get(12)?,
                        pending_upload_count: r.get(13)?,
                        pending_download_count: r.get(14)?,
                        pending_bytes: r.get(15)?,
                        stale_after_days: r.get(16)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_cloud_profile(&self, p: &CloudProfile) -> Result<String> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if p.id.trim().is_empty() {
            Uuid::new_v4().to_string()
        } else {
            p.id.clone()
        };
        let existing_target: Option<(String, String, String)> = tx
            .query_row(
                "SELECT local_root, remote_name, remote_path FROM cloud_profiles WHERE id = ?1",
                params![&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let target_changed = existing_target.is_some_and(|(lr, rn, rp)| {
            lr != p.local_root || rn != p.remote_name || rp != p.remote_path
        });
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO cloud_profiles (
                id, name, local_root, remote_name, remote_path, enabled, schedule_mode,
                schedule_value, schedule_action, last_sync_at, last_verify_at, last_status,
                last_error, pending_upload_count, pending_download_count, pending_bytes, stale_after_days, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
            ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, local_root=excluded.local_root, remote_name=excluded.remote_name,
                remote_path=excluded.remote_path, enabled=excluded.enabled, schedule_mode=excluded.schedule_mode,
                schedule_value=excluded.schedule_value, schedule_action=excluded.schedule_action,
                stale_after_days=excluded.stale_after_days, updated_at=excluded.updated_at",
            params![
                id,
                p.name,
                p.local_root,
                p.remote_name,
                p.remote_path,
                p.enabled as i64,
                p.schedule_mode,
                p.schedule_value,
                p.schedule_action,
                p.last_sync_at,
                p.last_verify_at,
                p.last_status,
                p.last_error,
                p.pending_upload_count,
                p.pending_download_count,
                p.pending_bytes,
                p.stale_after_days,
                now,
            ],
        )?;
        if target_changed {
            tx.execute(
                "DELETE FROM sync_manifest WHERE profile_kind = 'cloud' AND profile_id = ?1",
                params![&id],
            )?;
            tx.execute(
                "DELETE FROM intentional_update_applied WHERE profile_kind = 'cloud' AND profile_id = ?1",
                params![&id],
            )?;
            tx.execute(
                "UPDATE cloud_profiles SET
                    last_sync_at = NULL, last_verify_at = NULL, last_status = 'never',
                    last_error = NULL, pending_upload_count = 0, pending_download_count = 0, pending_bytes = 0
                 WHERE id = ?1",
                params![&id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_cloud_freshness(
        &self,
        id: &str,
        synced: bool,
        verified: bool,
        status: &str,
        error: Option<&str>,
        pending_upload: i64,
        pending_download: i64,
        pending_bytes: i64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE cloud_profiles SET
                last_sync_at = CASE WHEN ?2 THEN ?1 ELSE last_sync_at END,
                last_verify_at = CASE WHEN ?3 THEN ?1 ELSE last_verify_at END,
                last_status = ?4,
                last_error = ?5,
                pending_upload_count = ?6,
                pending_download_count = ?7,
                pending_bytes = ?8,
                updated_at = ?1
             WHERE id = ?9",
            params![
                now,
                synced as i64,
                verified as i64,
                status,
                error,
                pending_upload,
                pending_download,
                pending_bytes,
                id
            ],
        )?;
        Ok(())
    }

    pub fn delete_cloud_profile(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM cloud_profiles WHERE id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM sync_manifest WHERE profile_kind = 'cloud' AND profile_id = ?1",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM intentional_update_applied WHERE profile_kind = 'cloud' AND profile_id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── External Profiles ────────────────────────────────────────────────────

    pub fn list_external_profiles(&self) -> Result<Vec<ExternalProfile>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, local_root, volume_uuid, volume_label, relative_dest_path, direction,
                    enabled, schedule_mode, schedule_value, schedule_action, write_marker_file,
                    last_sync_at, last_verify_at, last_status, last_error, pending_push_count,
                    pending_pull_count, pending_bytes, stale_after_days
             FROM external_profiles ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ExternalProfile {
                id: r.get(0)?,
                name: r.get(1)?,
                local_root: r.get(2)?,
                volume_uuid: r.get(3)?,
                volume_label: r.get(4)?,
                relative_dest_path: r.get(5)?,
                direction: r.get(6)?,
                enabled: r.get::<_, i64>(7)? != 0,
                schedule_mode: r.get(8)?,
                schedule_value: r.get(9)?,
                schedule_action: r.get(10)?,
                write_marker_file: r.get::<_, i64>(11)? != 0,
                last_sync_at: r.get(12)?,
                last_verify_at: r.get(13)?,
                last_status: r.get(14)?,
                last_error: r.get(15)?,
                pending_push_count: r.get(16)?,
                pending_pull_count: r.get(17)?,
                pending_bytes: r.get(18)?,
                stale_after_days: r.get(19)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_external_profile(&self, id: &str) -> Result<Option<ExternalProfile>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT id, name, local_root, volume_uuid, volume_label, relative_dest_path, direction,
                        enabled, schedule_mode, schedule_value, schedule_action, write_marker_file,
                        last_sync_at, last_verify_at, last_status, last_error, pending_push_count,
                        pending_pull_count, pending_bytes, stale_after_days
                 FROM external_profiles WHERE id = ?1",
                params![id],
                |r| {
                    Ok(ExternalProfile {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        local_root: r.get(2)?,
                        volume_uuid: r.get(3)?,
                        volume_label: r.get(4)?,
                        relative_dest_path: r.get(5)?,
                        direction: r.get(6)?,
                        enabled: r.get::<_, i64>(7)? != 0,
                        schedule_mode: r.get(8)?,
                        schedule_value: r.get(9)?,
                        schedule_action: r.get(10)?,
                        write_marker_file: r.get::<_, i64>(11)? != 0,
                        last_sync_at: r.get(12)?,
                        last_verify_at: r.get(13)?,
                        last_status: r.get(14)?,
                        last_error: r.get(15)?,
                        pending_push_count: r.get(16)?,
                        pending_pull_count: r.get(17)?,
                        pending_bytes: r.get(18)?,
                        stale_after_days: r.get(19)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_external_profile(&self, p: &ExternalProfile) -> Result<String> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if p.id.trim().is_empty() {
            Uuid::new_v4().to_string()
        } else {
            p.id.clone()
        };
        let existing_target: Option<(String, String, String, String)> = tx
            .query_row(
                "SELECT local_root, volume_uuid, volume_label, relative_dest_path FROM external_profiles WHERE id = ?1",
                params![&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let target_changed = existing_target.is_some_and(|(lr, vu, vl, rd)| {
            lr != p.local_root
                || vu != p.volume_uuid
                || vl != p.volume_label
                || rd != p.relative_dest_path
        });
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO external_profiles (
                id, name, local_root, volume_uuid, volume_label, relative_dest_path, direction,
                enabled, schedule_mode, schedule_value, schedule_action, write_marker_file,
                last_sync_at, last_verify_at, last_status, last_error, pending_push_count,
                pending_pull_count, pending_bytes, stale_after_days, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
            ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, local_root=excluded.local_root, volume_uuid=excluded.volume_uuid,
                volume_label=excluded.volume_label, relative_dest_path=excluded.relative_dest_path,
                direction=excluded.direction, enabled=excluded.enabled, schedule_mode=excluded.schedule_mode,
                schedule_value=excluded.schedule_value, schedule_action=excluded.schedule_action,
                write_marker_file=excluded.write_marker_file, stale_after_days=excluded.stale_after_days,
                updated_at=excluded.updated_at",
            params![
                id,
                p.name,
                p.local_root,
                p.volume_uuid,
                p.volume_label,
                p.relative_dest_path,
                p.direction,
                p.enabled as i64,
                p.schedule_mode,
                p.schedule_value,
                p.schedule_action,
                p.write_marker_file as i64,
                p.last_sync_at,
                p.last_verify_at,
                p.last_status,
                p.last_error,
                p.pending_push_count,
                p.pending_pull_count,
                p.pending_bytes,
                p.stale_after_days,
                now,
            ],
        )?;
        if target_changed {
            tx.execute(
                "DELETE FROM sync_manifest WHERE profile_kind = 'external' AND profile_id = ?1",
                params![&id],
            )?;
            tx.execute(
                "DELETE FROM intentional_update_applied WHERE profile_kind = 'external' AND profile_id = ?1",
                params![&id],
            )?;
            tx.execute(
                "UPDATE external_profiles SET
                    last_sync_at = NULL, last_verify_at = NULL, last_status = 'never',
                    last_error = NULL, pending_push_count = 0, pending_pull_count = 0, pending_bytes = 0
                 WHERE id = ?1",
                params![&id],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_external_freshness(
        &self,
        id: &str,
        synced: bool,
        verified: bool,
        status: &str,
        error: Option<&str>,
        pending_push: i64,
        pending_pull: i64,
        pending_bytes: i64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE external_profiles SET
                last_sync_at = CASE WHEN ?2 THEN ?1 ELSE last_sync_at END,
                last_verify_at = CASE WHEN ?3 THEN ?1 ELSE last_verify_at END,
                last_status = ?4,
                last_error = ?5,
                pending_push_count = ?6,
                pending_pull_count = ?7,
                pending_bytes = ?8,
                updated_at = ?1
             WHERE id = ?9",
            params![
                now,
                synced as i64,
                verified as i64,
                status,
                error,
                pending_push,
                pending_pull,
                pending_bytes,
                id
            ],
        )?;
        Ok(())
    }

    pub fn delete_external_profile(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM external_profiles WHERE id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM sync_manifest WHERE profile_kind = 'external' AND profile_id = ?1",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM intentional_update_applied WHERE profile_kind = 'external' AND profile_id = ?1",
            params![id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ── Sync Manifest & Tombstones ───────────────────────────────────────────

    pub fn load_manifest_map(
        &self,
        profile_kind: &str,
        profile_id: &str,
    ) -> Result<HashMap<String, ManifestEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT profile_kind, profile_id, rel_path, size_bytes, md5_hash, mtime_unix, state
             FROM sync_manifest WHERE profile_kind = ?1 AND profile_id = ?2",
        )?;
        let rows = stmt.query_map(params![profile_kind, profile_id], |r| {
            Ok(ManifestEntry {
                profile_kind: r.get(0)?,
                profile_id: r.get(1)?,
                rel_path: r.get(2)?,
                size_bytes: r.get(3)?,
                md5_hash: r.get(4)?,
                mtime_unix: r.get(5)?,
                state: r.get(6)?,
            })
        })?;
        let mut map = HashMap::new();
        for entry in rows {
            let e = entry?;
            map.insert(e.rel_path.clone(), e);
        }
        Ok(map)
    }

    pub fn upsert_manifest_entries(&self, entries: &[ManifestEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO sync_manifest (profile_kind, profile_id, rel_path, size_bytes, md5_hash, mtime_unix, state, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(profile_kind, profile_id, rel_path) DO UPDATE SET
                    size_bytes=excluded.size_bytes,
                    md5_hash=CASE
                        WHEN excluded.md5_hash IS NOT NULL THEN excluded.md5_hash
                        WHEN sync_manifest.size_bytes = excluded.size_bytes AND sync_manifest.mtime_unix = excluded.mtime_unix THEN sync_manifest.md5_hash
                        ELSE NULL
                    END,
                    mtime_unix=excluded.mtime_unix,
                    state=excluded.state,
                    updated_at=excluded.updated_at",
            )?;
            for e in entries {
                let norm_md5 = e
                    .md5_hash
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_ascii_lowercase);
                stmt.execute(params![
                    e.profile_kind,
                    e.profile_id,
                    normalize_rel_path(&e.rel_path),
                    e.size_bytes,
                    norm_md5,
                    e.mtime_unix,
                    e.state,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune_manifest_entries(
        &self,
        profile_kind: &str,
        profile_id: &str,
        rel_paths: &[String],
    ) -> Result<()> {
        if rel_paths.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                "DELETE FROM sync_manifest WHERE profile_kind = ?1 AND profile_id = ?2 AND rel_path = ?3",
            )?;
            for rel in rel_paths {
                stmt.execute(params![profile_kind, profile_id, normalize_rel_path(rel)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_local_tombstones(
        &self,
        profile_kind: &str,
        profile_id: &str,
        rel_paths: &[String],
    ) -> Result<()> {
        if rel_paths.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        {
            let mut stmt = tx.prepare(
                "UPDATE sync_manifest SET state = 'local_tombstone', updated_at = ?4
                 WHERE profile_kind = ?1 AND profile_id = ?2 AND rel_path = ?3",
            )?;
            for rel in rel_paths {
                stmt.execute(params![
                    profile_kind,
                    profile_id,
                    normalize_rel_path(rel),
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Intentional Updates (GPS Backfill Safe Replacement) ──────────────────

    pub fn record_intentional_update(
        &self,
        abs_path: &str,
        pre_md5: &str,
        post_md5: &str,
        reason: &str,
    ) -> Result<()> {
        if pre_md5.eq_ignore_ascii_case(post_md5) {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO intentional_updates (id, abs_path, pre_md5, post_md5, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Uuid::new_v4().to_string(),
                abs_path,
                pre_md5.to_ascii_lowercase(),
                post_md5.to_ascii_lowercase(),
                reason,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_intentional_updates(&self) -> Result<Vec<IntentionalUpdate>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, abs_path, pre_md5, post_md5, reason, created_at
             FROM intentional_updates ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(IntentionalUpdate {
                id: r.get(0)?,
                abs_path: r.get(1)?,
                pre_md5: r.get(2)?,
                post_md5: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Lists intentional updates not yet applied to `(profile_kind, profile_id)`.
    pub fn list_pending_intentional_updates_for_target(
        &self,
        profile_kind: &str,
        profile_id: &str,
    ) -> Result<Vec<IntentionalUpdate>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.abs_path, u.pre_md5, u.post_md5, u.reason, u.created_at
             FROM intentional_updates u
             WHERE NOT EXISTS (
                 SELECT 1 FROM intentional_update_applied a
                 WHERE a.update_id = u.id AND a.profile_kind = ?1 AND a.profile_id = ?2
             )
             ORDER BY u.created_at ASC, u.rowid ASC",
        )?;
        let rows = stmt.query_map(params![profile_kind, profile_id], |r| {
            Ok(IntentionalUpdate {
                id: r.get(0)?,
                abs_path: r.get(1)?,
                pre_md5: r.get(2)?,
                post_md5: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Marks intentional update(s) as applied for a specific Cloud or External profile so
    /// other targets (e.g., an unplugged External SSD) still retain the update until they sync too.
    pub fn mark_intentional_updates_applied(
        &self,
        update_ids: &[String],
        profile_kind: &str,
        profile_id: &str,
    ) -> Result<()> {
        if update_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO intentional_update_applied (update_id, profile_kind, profile_id, applied_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for id in update_ids {
                stmt.execute(params![id, profile_kind, profile_id, now])?;
            }
        }
        // Prune updates that have been applied across every configured Cloud and External profile
        // (only when at least one profile exists so standalone tests / pre-profile backfills stay intact).
        let total_profiles: i64 = tx.query_row(
            "SELECT (SELECT COUNT(*) FROM cloud_profiles) + (SELECT COUNT(*) FROM external_profiles)",
            [],
            |r| r.get(0),
        )?;
        if total_profiles > 0 {
            tx.execute(
                "DELETE FROM intentional_updates
                 WHERE NOT EXISTS (
                     SELECT 1 FROM cloud_profiles cp
                     WHERE NOT EXISTS (
                         SELECT 1 FROM intentional_update_applied a
                         WHERE a.update_id = intentional_updates.id
                           AND a.profile_kind = 'cloud' AND a.profile_id = cp.id
                     )
                 )
                 AND NOT EXISTS (
                     SELECT 1 FROM external_profiles ep
                     WHERE NOT EXISTS (
                         SELECT 1 FROM intentional_update_applied a
                         WHERE a.update_id = intentional_updates.id
                           AND a.profile_kind = 'external' AND a.profile_id = ep.id
                     )
                 )",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_intentional_update(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM intentional_updates WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Jobs & History ───────────────────────────────────────────────────────

    pub fn create_job(
        &self,
        kind: &str,
        title: &str,
        files_total: i64,
        bytes_total: i64,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO jobs (id, kind, title, status, started_at, files_total, bytes_total)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6)",
            params![
                id,
                kind,
                title,
                Utc::now().to_rfc3339(),
                files_total,
                bytes_total
            ],
        )?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_job(
        &self,
        id: &str,
        status: &str,
        files_done: i64,
        files_skipped: i64,
        files_failed: i64,
        bytes_done: i64,
        summary_json: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE jobs SET
                status = ?2,
                finished_at = ?3,
                files_done = ?4,
                files_skipped = ?5,
                files_failed = ?6,
                bytes_done = ?7,
                summary_json = ?8,
                error_message = ?9
             WHERE id = ?1",
            params![
                id,
                status,
                Utc::now().to_rfc3339(),
                files_done,
                files_skipped,
                files_failed,
                bytes_done,
                summary_json,
                error_message
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_job_item(
        &self,
        job_id: &str,
        stage: &str,
        source_path: &str,
        dest_path: Option<&str>,
        status: &str,
        bytes: i64,
        message: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO job_items (job_id, stage, source_path, dest_path, status, bytes, message, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                job_id,
                stage,
                source_path,
                dest_path,
                status,
                bytes,
                message,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn list_jobs(&self, limit: i64) -> Result<Vec<JobRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, status, started_at, finished_at, files_total, files_done,
                    files_skipped, files_failed, bytes_total, bytes_done, summary_json, error_message
             FROM jobs ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(JobRecord {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                status: r.get(3)?,
                started_at: r.get(4)?,
                finished_at: r.get(5)?,
                files_total: r.get(6)?,
                files_done: r.get(7)?,
                files_skipped: r.get(8)?,
                files_failed: r.get(9)?,
                bytes_total: r.get(10)?,
                bytes_done: r.get(11)?,
                summary_json: r.get(12)?,
                error_message: r.get(13)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_job_items(&self, job_id: &str, limit: i64) -> Result<Vec<JobItemRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, job_id, stage, source_path, dest_path, status, bytes, message, created_at
             FROM job_items WHERE job_id = ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![job_id, limit], |r| {
            Ok(JobItemRecord {
                id: r.get(0)?,
                job_id: r.get(1)?,
                stage: r.get(2)?,
                source_path: r.get(3)?,
                dest_path: r.get(4)?,
                status: r.get(5)?,
                bytes: r.get(6)?,
                message: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn db_settings_presets_manifest_and_updates_roundtrip() {
        let dir = tempdir().unwrap();
        let db = DbHandle::open(&dir.path().join("state.db")).unwrap();

        let presets = db.list_import_presets().unwrap();
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].video_folder, "VID");
        assert_eq!(presets[0].date_format, "M-dd");

        let mut s = db.load_settings().unwrap();
        s.bandwidth_limit_mbps = Some(15.0);
        db.save_settings(&s).unwrap();
        assert_eq!(db.load_settings().unwrap().bandwidth_limit_mbps, Some(15.0));

        db.upsert_manifest_entries(&[ManifestEntry {
            profile_kind: "cloud".into(),
            profile_id: "p1".into(),
            rel_path: r"2026\Hikes\RAW\IMG_0001.CR3".into(),
            size_bytes: 1024,
            md5_hash: Some("abc".into()),
            mtime_unix: 1700000000,
            state: "synced".into(),
        }])
        .unwrap();
        db.mark_local_tombstones("cloud", "p1", &["2026/Hikes/RAW/IMG_0001.CR3".into()])
            .unwrap();
        let map = db.load_manifest_map("cloud", "p1").unwrap();
        assert_eq!(
            map.get("2026/Hikes/RAW/IMG_0001.CR3").unwrap().state,
            "local_tombstone"
        );

        assert!(is_mac_metadata_path("2026/Hikes/._IMG_0001.CR3"));
        assert!(is_mac_metadata_path("2026/.DS_Store"));
        assert!(is_mac_metadata_path("__MACOSX/foo.jpg"));
        assert!(!is_mac_metadata_path("2026/Hikes/IMG_0001.xmp"));
    }
}
