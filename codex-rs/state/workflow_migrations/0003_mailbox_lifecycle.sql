-- Normalize the mailbox lifecycle and make data/control delivery queues
-- independent.  Existing rows are retained byte-for-byte except for the
-- retired state names: claimed becomes delivering and acked becomes
-- delivered.
CREATE TABLE workflow_mailbox_v3 (
    message_id TEXT PRIMARY KEY NOT NULL CHECK(length(message_id) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    sender_run_id TEXT NOT NULL CHECK(length(sender_run_id) BETWEEN 1 AND 128),
    recipient_run_id TEXT NOT NULL CHECK(length(recipient_run_id) BETWEEN 1 AND 128),
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    channel TEXT NOT NULL CHECK(channel IN ('data', 'control')),
    state TEXT NOT NULL CHECK(state IN ('pending', 'delivering', 'delivered')),
    payload_json TEXT NOT NULL CHECK(length(payload_json) <= 65536),
    created_at_ms INTEGER NOT NULL,
    claim_owner TEXT CHECK(claim_owner IS NULL OR length(claim_owner) BETWEEN 1 AND 128),
    claim_token TEXT CHECK(claim_token IS NULL OR length(claim_token) BETWEEN 1 AND 128),
    claim_expires_at_ms INTEGER,
    acked_at_ms INTEGER,
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    UNIQUE(recipient_run_id, sequence)
);

INSERT INTO workflow_mailbox_v3 (
    message_id, root_run_id, sender_run_id, recipient_run_id, sequence,
    channel, state, payload_json, created_at_ms, claim_owner, claim_token,
    claim_expires_at_ms, acked_at_ms, generation
)
SELECT
    message_id, root_run_id, sender_run_id, recipient_run_id, sequence,
    channel,
    CASE state
        WHEN 'claimed' THEN 'delivering'
        WHEN 'acked' THEN 'delivered'
        ELSE state
    END,
    payload_json, created_at_ms, claim_owner, claim_token,
    claim_expires_at_ms, acked_at_ms, 0
FROM workflow_mailbox;

DROP TABLE workflow_mailbox;
ALTER TABLE workflow_mailbox_v3 RENAME TO workflow_mailbox;

CREATE INDEX workflow_mailbox_pending_idx
    ON workflow_mailbox(recipient_run_id, channel, state, sequence);

CREATE INDEX workflow_mailbox_capacity_idx
    ON workflow_mailbox(root_run_id, channel, state);
