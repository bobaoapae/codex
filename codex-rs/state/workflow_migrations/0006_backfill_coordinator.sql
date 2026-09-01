-- Backfill coordination gets an explicit fenced lifecycle. Preserve the
-- historical rows while normalizing the old `running` spelling to
-- `processing`; no rollout or journal row is deleted by this migration.
CREATE TABLE workflow_backfill_state_v6 (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    status TEXT NOT NULL CHECK(status IN ('pending', 'processing', 'complete', 'recoverable', 'failed')),
    watermark_created_at_ms INTEGER,
    watermark_rollout_id TEXT CHECK(watermark_rollout_id IS NULL OR length(watermark_rollout_id) BETWEEN 1 AND 128),
    last_success_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL,
    error_json TEXT CHECK(error_json IS NULL OR length(error_json) <= 65536),
    owner_id TEXT CHECK(owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 128),
    owner_token TEXT CHECK(owner_token IS NULL OR length(owner_token) BETWEEN 1 AND 128),
    lease_id TEXT CHECK(lease_id IS NULL OR length(lease_id) BETWEEN 1 AND 128),
    lease_expires_at_ms INTEGER CHECK(lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    generation_id INTEGER,
    cursor_json TEXT CHECK(cursor_json IS NULL OR length(cursor_json) <= 65536),
    source_size_bytes INTEGER CHECK(source_size_bytes IS NULL OR source_size_bytes >= 0),
    source_mtime_ms INTEGER CHECK(source_mtime_ms IS NULL OR source_mtime_ms >= 0)
);

INSERT INTO workflow_backfill_state_v6 (
    id, status, watermark_created_at_ms, watermark_rollout_id,
    last_success_at_ms, updated_at_ms, error_json, owner_id, owner_token,
    lease_id, lease_expires_at_ms, generation, generation_id, cursor_json, source_size_bytes,
    source_mtime_ms
)
SELECT
    id,
    CASE status WHEN 'running' THEN 'processing' ELSE status END,
    watermark_created_at_ms, watermark_rollout_id, last_success_at_ms,
    updated_at_ms, error_json, owner_id, owner_token, lease_id, NULL, 0,
    generation_id, cursor_json, source_size_bytes, source_mtime_ms
FROM workflow_backfill_state;

DROP TABLE workflow_backfill_state;
ALTER TABLE workflow_backfill_state_v6 RENAME TO workflow_backfill_state;

CREATE TABLE workflow_backfill_journal_v6 (
    journal_id INTEGER PRIMARY KEY AUTOINCREMENT,
    rollout_id TEXT NOT NULL CHECK(length(rollout_id) BETWEEN 1 AND 128),
    source_path TEXT NOT NULL CHECK(length(source_path) <= 4096),
    byte_offset INTEGER NOT NULL DEFAULT 0 CHECK(byte_offset >= 0),
    rollout_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(rollout_ordinal >= 0),
    status TEXT NOT NULL CHECK(status IN ('pending', 'processing', 'complete', 'skippedPermanent', 'recoverable', 'failed')),
    error_json TEXT CHECK(error_json IS NULL OR length(error_json) <= 65536),
    updated_at_ms INTEGER NOT NULL,
    owner_id TEXT CHECK(owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 128),
    owner_token TEXT CHECK(owner_token IS NULL OR length(owner_token) BETWEEN 1 AND 128),
    lease_id TEXT CHECK(lease_id IS NULL OR length(lease_id) BETWEEN 1 AND 128),
    lease_expires_at_ms INTEGER CHECK(lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    generation_id INTEGER,
    cursor_json TEXT CHECK(cursor_json IS NULL OR length(cursor_json) <= 65536),
    source_size_bytes INTEGER CHECK(source_size_bytes IS NULL OR source_size_bytes >= 0),
    source_mtime_ms INTEGER CHECK(source_mtime_ms IS NULL OR source_mtime_ms >= 0),
    UNIQUE(rollout_id)
);

INSERT INTO workflow_backfill_journal_v6 (
    journal_id, rollout_id, source_path, byte_offset, rollout_ordinal, status,
    error_json, updated_at_ms, owner_id, owner_token, lease_id, lease_expires_at_ms, generation,
    generation_id, cursor_json, source_size_bytes, source_mtime_ms
)
SELECT
    journal_id, rollout_id, source_path, byte_offset, rollout_ordinal,
    CASE status WHEN 'running' THEN 'processing' ELSE status END,
    error_json, updated_at_ms, owner_id, owner_token, lease_id, NULL, 0,
    generation_id, cursor_json, source_size_bytes, source_mtime_ms
FROM workflow_backfill_journal;

DROP TABLE workflow_backfill_journal;
ALTER TABLE workflow_backfill_journal_v6 RENAME TO workflow_backfill_journal;

CREATE INDEX workflow_backfill_journal_status_idx
    ON workflow_backfill_journal(status, updated_at_ms, rollout_id);
CREATE INDEX workflow_backfill_journal_source_idx
    ON workflow_backfill_journal(source_path, rollout_id);

-- Incremental capture is explicit state, separate from the frozen historical
-- watermark. It starts pending and is never advanced by a historical finalize.
CREATE TABLE workflow_backfill_incremental (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    status TEXT NOT NULL CHECK(status IN ('pending', 'processing', 'complete', 'recoverable', 'failed')),
    watermark_created_at_ms INTEGER,
    watermark_rollout_id TEXT CHECK(watermark_rollout_id IS NULL OR length(watermark_rollout_id) BETWEEN 1 AND 128),
    updated_at_ms INTEGER NOT NULL,
    error_json TEXT CHECK(error_json IS NULL OR length(error_json) <= 65536),
    owner_id TEXT CHECK(owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 128),
    owner_token TEXT CHECK(owner_token IS NULL OR length(owner_token) BETWEEN 1 AND 128),
    lease_id TEXT CHECK(lease_id IS NULL OR length(lease_id) BETWEEN 1 AND 128),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0)
);

INSERT INTO workflow_backfill_incremental (id, status, updated_at_ms)
VALUES (1, 'pending', 0);
