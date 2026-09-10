CREATE TABLE thread_attachments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    attachment_type TEXT NOT NULL,
    identity_key TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (thread_id, attachment_type, identity_key)
);

INSERT INTO thread_attachments (id, thread_id, attachment_type, identity_key, payload, created_at)
SELECT id, thread_id, artifact_type, identity_key, payload, created_at
FROM thread_artifacts;

CREATE INDEX idx_thread_attachments_thread_created_id
    ON thread_attachments(thread_id, created_at, id);
