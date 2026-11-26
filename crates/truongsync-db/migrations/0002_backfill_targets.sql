-- Migration 0002: Per-target GPS backfill tracking (prevents Cloud sync from clearing
-- intentional updates before External Drives or secondary Cloud profiles sync).
CREATE TABLE IF NOT EXISTS intentional_update_applied (
    update_id TEXT NOT NULL REFERENCES intentional_updates(id) ON DELETE CASCADE,
    profile_kind TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    applied_at TEXT NOT NULL,
    PRIMARY KEY (update_id, profile_kind, profile_id)
);

CREATE INDEX IF NOT EXISTS idx_intentional_update_applied_target
    ON intentional_update_applied (profile_kind, profile_id);
