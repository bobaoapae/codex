-- The workflow database is intentionally separate from state_5.sqlite.  It
-- contains coordination and rebuildable search projections, so a corrupt or
-- busy workflow database must not prevent the core thread metadata store from
-- being opened.

CREATE TABLE workflow_runs (
    run_id TEXT PRIMARY KEY NOT NULL CHECK(length(run_id) BETWEEN 1 AND 128),
    thread_id TEXT NOT NULL CHECK(length(thread_id) BETWEEN 1 AND 128),
    root_thread_id TEXT CHECK(root_thread_id IS NULL OR length(root_thread_id) BETWEEN 1 AND 128),
    parent_run_id TEXT CHECK(parent_run_id IS NULL OR length(parent_run_id) BETWEEN 1 AND 128),
    thread_class TEXT NOT NULL CHECK(thread_class IN ('interactive', 'subAgent', 'transientJob', 'internal', 'legacyExec')),
    status TEXT NOT NULL CHECK(length(status) BETWEEN 1 AND 32),
    outcome TEXT CHECK(outcome IS NULL OR length(outcome) BETWEEN 1 AND 32),
    idempotency_key TEXT CHECK(idempotency_key IS NULL OR length(idempotency_key) BETWEEN 1 AND 256),
    provider TEXT CHECK(provider IS NULL OR length(provider) BETWEEN 1 AND 128),
    model TEXT CHECK(model IS NULL OR length(model) BETWEEN 1 AND 256),
    cwd TEXT CHECK(cwd IS NULL OR length(cwd) BETWEEN 1 AND 4096),
    metadata_json TEXT CHECK(metadata_json IS NULL OR length(metadata_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    version INTEGER NOT NULL DEFAULT 0 CHECK(version >= 0)
);

CREATE UNIQUE INDEX workflow_runs_idempotency_idx
    ON workflow_runs(root_thread_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX workflow_runs_thread_idx
    ON workflow_runs(thread_id, created_at_ms DESC);

CREATE INDEX workflow_runs_root_idx
    ON workflow_runs(root_thread_id, created_at_ms DESC);

CREATE TABLE workflow_receipts (
    receipt_id TEXT PRIMARY KEY NOT NULL CHECK(length(receipt_id) BETWEEN 1 AND 128),
    run_id TEXT REFERENCES workflow_runs(run_id) ON DELETE SET NULL,
    thread_id TEXT CHECK(thread_id IS NULL OR length(thread_id) BETWEEN 1 AND 128),
    turn_id TEXT CHECK(turn_id IS NULL OR length(turn_id) BETWEEN 1 AND 128),
    job_id TEXT CHECK(job_id IS NULL OR length(job_id) BETWEEN 1 AND 128),
    plan_snapshot_id TEXT CHECK(plan_snapshot_id IS NULL OR length(plan_snapshot_id) BETWEEN 1 AND 256),
    schema_version INTEGER NOT NULL CHECK(schema_version BETWEEN 1 AND 2147483647),
    kind TEXT NOT NULL CHECK(length(kind) BETWEEN 1 AND 128),
    subject TEXT NOT NULL CHECK(length(subject) BETWEEN 1 AND 4096),
    status TEXT NOT NULL CHECK(length(status) BETWEEN 1 AND 32),
    source TEXT NOT NULL CHECK(length(source) BETWEEN 1 AND 128),
    provenance_json TEXT CHECK(provenance_json IS NULL OR length(provenance_json) <= 65536),
    tags_json TEXT CHECK(tags_json IS NULL OR length(tags_json) <= 16384),
    payload_json TEXT CHECK(payload_json IS NULL OR length(payload_json) <= 65536),
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX workflow_receipts_run_idx ON workflow_receipts(run_id, created_at_ms DESC);
CREATE INDEX workflow_receipts_thread_idx ON workflow_receipts(thread_id, created_at_ms DESC);

CREATE TABLE workflow_checkpoints (
    run_id TEXT NOT NULL REFERENCES workflow_runs(run_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    checkpoint_kind TEXT NOT NULL CHECK(length(checkpoint_kind) BETWEEN 1 AND 128),
    rollout_ordinal INTEGER CHECK(rollout_ordinal IS NULL OR rollout_ordinal >= 0),
    rollout_byte_offset INTEGER CHECK(rollout_byte_offset IS NULL OR rollout_byte_offset >= 0),
    payload_json TEXT NOT NULL CHECK(length(payload_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY(run_id, sequence)
);

CREATE INDEX workflow_checkpoints_run_created_idx
    ON workflow_checkpoints(run_id, created_at_ms, sequence);

CREATE TABLE workflow_mailbox (
    message_id TEXT PRIMARY KEY NOT NULL CHECK(length(message_id) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    sender_run_id TEXT NOT NULL CHECK(length(sender_run_id) BETWEEN 1 AND 128),
    recipient_run_id TEXT NOT NULL CHECK(length(recipient_run_id) BETWEEN 1 AND 128),
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    channel TEXT NOT NULL CHECK(channel IN ('data', 'control')),
    state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'acked')),
    payload_json TEXT NOT NULL CHECK(length(payload_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    claim_owner TEXT CHECK(claim_owner IS NULL OR length(claim_owner) BETWEEN 1 AND 128),
    claim_token TEXT CHECK(claim_token IS NULL OR length(claim_token) BETWEEN 1 AND 128),
    claim_expires_at_ms INTEGER,
    acked_at_ms INTEGER,
    UNIQUE(recipient_run_id, sequence)
);

CREATE INDEX workflow_mailbox_pending_idx
    ON workflow_mailbox(recipient_run_id, channel, state, sequence);

CREATE TABLE workflow_path_leases (
    lease_id TEXT PRIMARY KEY NOT NULL CHECK(length(lease_id) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    owner_run_id TEXT NOT NULL CHECK(length(owner_run_id) BETWEEN 1 AND 128),
    path TEXT NOT NULL CHECK(length(path) BETWEEN 1 AND 4096),
    mode TEXT NOT NULL CHECK(mode IN ('read', 'write')),
    generation INTEGER NOT NULL CHECK(generation >= 0),
    state TEXT NOT NULL CHECK(state IN ('active', 'released', 'expired')),
    issued_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    released_at_ms INTEGER,
    override_receipt_id TEXT CHECK(override_receipt_id IS NULL OR length(override_receipt_id) BETWEEN 1 AND 128)
);

CREATE INDEX workflow_path_leases_path_idx ON workflow_path_leases(path, mode, state);
CREATE INDEX workflow_path_leases_owner_idx ON workflow_path_leases(owner_run_id, state);

CREATE TABLE workflow_backfill_state (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    status TEXT NOT NULL CHECK(length(status) BETWEEN 1 AND 32),
    watermark_created_at_ms INTEGER,
    watermark_rollout_id TEXT CHECK(watermark_rollout_id IS NULL OR length(watermark_rollout_id) BETWEEN 1 AND 128),
    last_success_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL,
    error_json TEXT CHECK(error_json IS NULL OR length(error_json) <= 65536),
    owner_id TEXT CHECK(owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 128),
    owner_token TEXT CHECK(owner_token IS NULL OR length(owner_token) BETWEEN 1 AND 128),
    lease_id TEXT CHECK(lease_id IS NULL OR length(lease_id) BETWEEN 1 AND 128),
    generation_id INTEGER,
    cursor_json TEXT CHECK(cursor_json IS NULL OR length(cursor_json) <= 65536),
    source_size_bytes INTEGER CHECK(source_size_bytes IS NULL OR source_size_bytes >= 0),
    source_mtime_ms INTEGER CHECK(source_mtime_ms IS NULL OR source_mtime_ms >= 0)
);

INSERT INTO workflow_backfill_state (id, status, updated_at_ms)
VALUES (1, 'pending', 0);

CREATE TABLE workflow_backfill_journal (
    journal_id INTEGER PRIMARY KEY AUTOINCREMENT,
    rollout_id TEXT NOT NULL CHECK(length(rollout_id) BETWEEN 1 AND 128),
    source_path TEXT NOT NULL CHECK(length(source_path) <= 4096),
    byte_offset INTEGER NOT NULL DEFAULT 0 CHECK(byte_offset >= 0),
    rollout_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(rollout_ordinal >= 0),
    status TEXT NOT NULL CHECK(length(status) BETWEEN 1 AND 32),
    error_json TEXT CHECK(error_json IS NULL OR length(error_json) <= 65536),
    updated_at_ms INTEGER NOT NULL,
    owner_id TEXT CHECK(owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 128),
    owner_token TEXT CHECK(owner_token IS NULL OR length(owner_token) BETWEEN 1 AND 128),
    lease_id TEXT CHECK(lease_id IS NULL OR length(lease_id) BETWEEN 1 AND 128),
    generation_id INTEGER,
    cursor_json TEXT CHECK(cursor_json IS NULL OR length(cursor_json) <= 65536),
    source_size_bytes INTEGER CHECK(source_size_bytes IS NULL OR source_size_bytes >= 0),
    source_mtime_ms INTEGER CHECK(source_mtime_ms IS NULL OR source_mtime_ms >= 0),
    UNIQUE(rollout_id)
);

CREATE TABLE workflow_search_generations (
    generation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    state TEXT NOT NULL CHECK(state IN ('building', 'published', 'retired', 'failed')),
    created_at_ms INTEGER NOT NULL,
    published_at_ms INTEGER,
    document_count INTEGER NOT NULL DEFAULT 0 CHECK(document_count >= 0),
    source_watermark TEXT CHECK(source_watermark IS NULL OR length(source_watermark) <= 4096)
);

CREATE INDEX workflow_search_generations_state_idx
    ON workflow_search_generations(state, created_at_ms DESC);

-- A published generation is a read-only snapshot.  The only allowed
-- transition after publication is published -> retired; this keeps the
-- active pointer swap transactional without permitting a published snapshot
-- to be rewritten.
CREATE TRIGGER workflow_search_generations_immutable_update
BEFORE UPDATE ON workflow_search_generations
WHEN OLD.state = 'published'
 AND NOT (
     NEW.state = 'retired'
     AND NEW.generation_id = OLD.generation_id
     AND NEW.created_at_ms = OLD.created_at_ms
     AND NEW.published_at_ms = OLD.published_at_ms
     AND NEW.document_count = OLD.document_count
     AND NEW.source_watermark IS OLD.source_watermark
 )
BEGIN
    SELECT RAISE(ABORT, 'published search generation is immutable');
END;

CREATE TRIGGER workflow_search_generations_immutable_delete
BEFORE DELETE ON workflow_search_generations
WHEN OLD.state IN ('published', 'retired')
BEGIN
    SELECT RAISE(ABORT, 'published search generation is immutable');
END;

CREATE TABLE workflow_search_state (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    active_generation_id INTEGER REFERENCES workflow_search_generations(generation_id),
    updated_at_ms INTEGER NOT NULL
);

INSERT INTO workflow_search_state (id, active_generation_id, updated_at_ms)
VALUES (1, NULL, 0);

CREATE TABLE workflow_search_documents (
    document_id INTEGER PRIMARY KEY AUTOINCREMENT,
    generation_id INTEGER NOT NULL REFERENCES workflow_search_generations(generation_id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL CHECK(length(thread_id) BETWEEN 1 AND 128),
    source_id TEXT NOT NULL CHECK(length(source_id) BETWEEN 1 AND 256),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('user', 'finalAssistant', 'compactionSummary', 'approvedPlan', 'receiptMetadata')),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    content TEXT NOT NULL CHECK(length(content) BETWEEN 1 AND 1000000),
    root_thread_id TEXT CHECK(root_thread_id IS NULL OR length(root_thread_id) BETWEEN 1 AND 128),
    project_id TEXT CHECK(project_id IS NULL OR length(project_id) BETWEEN 1 AND 256),
    cwd TEXT CHECK(cwd IS NULL OR length(cwd) BETWEEN 1 AND 4096),
    provider TEXT CHECK(provider IS NULL OR length(provider) BETWEEN 1 AND 128),
    thread_class TEXT CHECK(thread_class IS NULL OR length(thread_class) BETWEEN 1 AND 32),
    outcome TEXT CHECK(outcome IS NULL OR length(outcome) BETWEEN 1 AND 32),
    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
    event_time_ms INTEGER CHECK(event_time_ms IS NULL OR event_time_ms >= 0),
    metadata_json TEXT CHECK(metadata_json IS NULL OR length(metadata_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    UNIQUE(generation_id, source_id, source_kind)
);

CREATE INDEX workflow_search_documents_generation_idx
    ON workflow_search_documents(generation_id, document_id);

CREATE INDEX workflow_search_documents_thread_idx
    ON workflow_search_documents(thread_id, generation_id, document_id);

CREATE INDEX workflow_search_documents_filter_idx
    ON workflow_search_documents(
        generation_id, root_thread_id, project_id, cwd, provider,
        thread_class, outcome, archived, document_id
    );

CREATE TRIGGER workflow_search_documents_only_building
BEFORE INSERT ON workflow_search_documents
WHEN (SELECT state FROM workflow_search_generations
      WHERE generation_id = NEW.generation_id) <> 'building'
BEGIN
    SELECT RAISE(ABORT, 'search documents can only be added to a building generation');
END;

CREATE TRIGGER workflow_search_documents_immutable_update
BEFORE UPDATE ON workflow_search_documents
WHEN (SELECT state FROM workflow_search_generations
      WHERE generation_id = OLD.generation_id) IN ('published', 'retired')
BEGIN
    SELECT RAISE(ABORT, 'published search documents are immutable');
END;

CREATE TRIGGER workflow_search_documents_immutable_delete
BEFORE DELETE ON workflow_search_documents
WHEN (SELECT state FROM workflow_search_generations
      WHERE generation_id = OLD.generation_id) IN ('published', 'retired')
BEGIN
    SELECT RAISE(ABORT, 'published search documents are immutable');
END;

CREATE VIRTUAL TABLE workflow_search_fts USING fts5(
    content,
    content='workflow_search_documents',
    content_rowid='document_id',
    tokenize='unicode61'
);

CREATE TRIGGER workflow_search_documents_ai AFTER INSERT ON workflow_search_documents BEGIN
    INSERT INTO workflow_search_fts(rowid, content) VALUES (new.document_id, new.content);
END;

CREATE TRIGGER workflow_search_documents_ad AFTER DELETE ON workflow_search_documents BEGIN
    INSERT INTO workflow_search_fts(workflow_search_fts, rowid, content)
    VALUES ('delete', old.document_id, old.content);
END;

CREATE TRIGGER workflow_search_documents_au AFTER UPDATE OF content ON workflow_search_documents BEGIN
    INSERT INTO workflow_search_fts(workflow_search_fts, rowid, content)
    VALUES ('delete', old.document_id, old.content);
    INSERT INTO workflow_search_fts(rowid, content) VALUES (new.document_id, new.content);
END;

-- Live documents are deliberately separate from an immutable generation.  A
-- write changes the epoch only when it changes a document; readers bind their
-- cursor to that epoch and reject a cursor after an overlay mutation.
CREATE TABLE workflow_search_live_state (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    live_epoch INTEGER NOT NULL DEFAULT 0 CHECK(live_epoch >= 0),
    updated_at_ms INTEGER NOT NULL
);

INSERT INTO workflow_search_live_state (id, live_epoch, updated_at_ms)
VALUES (1, 0, 0);

CREATE TABLE workflow_search_live_documents (
    live_document_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL CHECK(length(thread_id) BETWEEN 1 AND 128),
    source_id TEXT NOT NULL CHECK(length(source_id) BETWEEN 1 AND 256),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('user', 'finalAssistant', 'compactionSummary', 'approvedPlan', 'receiptMetadata')),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    content TEXT NOT NULL CHECK(length(content) BETWEEN 1 AND 1000000),
    root_thread_id TEXT CHECK(root_thread_id IS NULL OR length(root_thread_id) BETWEEN 1 AND 128),
    project_id TEXT CHECK(project_id IS NULL OR length(project_id) BETWEEN 1 AND 256),
    cwd TEXT CHECK(cwd IS NULL OR length(cwd) BETWEEN 1 AND 4096),
    provider TEXT CHECK(provider IS NULL OR length(provider) BETWEEN 1 AND 128),
    thread_class TEXT CHECK(thread_class IS NULL OR length(thread_class) BETWEEN 1 AND 32),
    outcome TEXT CHECK(outcome IS NULL OR length(outcome) BETWEEN 1 AND 32),
    archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
    event_time_ms INTEGER CHECK(event_time_ms IS NULL OR event_time_ms >= 0),
    metadata_json TEXT CHECK(metadata_json IS NULL OR length(metadata_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(thread_id, source_id, source_kind)
);

CREATE INDEX workflow_search_live_documents_filter_idx
    ON workflow_search_live_documents(
        thread_id, root_thread_id, project_id, cwd, provider,
        thread_class, outcome, archived, live_document_id
    );

CREATE VIRTUAL TABLE workflow_search_live_fts USING fts5(
    content,
    content='workflow_search_live_documents',
    content_rowid='live_document_id',
    tokenize='unicode61'
);

CREATE TRIGGER workflow_search_live_documents_ai AFTER INSERT ON workflow_search_live_documents BEGIN
    INSERT INTO workflow_search_live_fts(rowid, content)
    VALUES (new.live_document_id, new.content);
END;

CREATE TRIGGER workflow_search_live_documents_ad AFTER DELETE ON workflow_search_live_documents BEGIN
    INSERT INTO workflow_search_live_fts(workflow_search_live_fts, rowid, content)
    VALUES ('delete', old.live_document_id, old.content);
END;

CREATE TRIGGER workflow_search_live_documents_au AFTER UPDATE OF content ON workflow_search_live_documents BEGIN
    INSERT INTO workflow_search_live_fts(workflow_search_live_fts, rowid, content)
    VALUES ('delete', old.live_document_id, old.content);
    INSERT INTO workflow_search_live_fts(rowid, content)
    VALUES (new.live_document_id, new.content);
END;
