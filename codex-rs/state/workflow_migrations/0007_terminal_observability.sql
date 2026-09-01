-- Interactive terminal observability is runtime coordination state, not
-- rollout/model history.  Rows are bounded and may be removed after a process
-- reaper or session shutdown has released the associated handle.

CREATE TABLE workflow_terminal_observations (
    session_id TEXT NOT NULL CHECK(length(session_id) BETWEEN 1 AND 128),
    process_id INTEGER NOT NULL CHECK(process_id > 0),
    command_summary TEXT NOT NULL CHECK(length(command_summary) <= 1024),
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    last_activity_at_ms INTEGER NOT NULL CHECK(last_activity_at_ms >= 0),
    last_output_at_ms INTEGER CHECK(last_output_at_ms IS NULL OR last_output_at_ms >= 0),
    last_output_preview TEXT CHECK(last_output_preview IS NULL OR length(last_output_preview) <= 512),
    last_output_bytes INTEGER NOT NULL CHECK(last_output_bytes >= 0),
    output_bytes INTEGER NOT NULL CHECK(output_bytes >= 0),
    state TEXT NOT NULL CHECK(state IN ('running', 'waiting', 'needsAttention', 'exited', 'failed', 'cancelled')),
    final_receipt_emitted INTEGER NOT NULL CHECK(final_receipt_emitted IN (0, 1)),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    PRIMARY KEY(session_id, process_id)
);

CREATE INDEX workflow_terminal_observations_session_idx
    ON workflow_terminal_observations(session_id, process_id);
