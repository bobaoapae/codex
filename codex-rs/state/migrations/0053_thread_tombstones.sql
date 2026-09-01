ALTER TABLE threads ADD COLUMN tombstoned_at INTEGER;
ALTER TABLE threads ADD COLUMN visible INTEGER NOT NULL DEFAULT 1;

CREATE INDEX idx_threads_visible_created_at
    ON threads(visible, created_at DESC, id DESC);
CREATE INDEX idx_threads_visible_updated_at
    ON threads(visible, updated_at DESC, id DESC);
