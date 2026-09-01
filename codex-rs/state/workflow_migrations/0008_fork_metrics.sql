-- Fork timing and provider accounting is a bounded local projection.  The
-- canonical rollout remains the source of truth; these rows intentionally do
-- not contain prompts, response text, or provider payloads.

CREATE TABLE workflow_fork_metrics (
    fork_id TEXT PRIMARY KEY CHECK(length(fork_id) BETWEEN 1 AND 128),
    spawn_call_id TEXT NOT NULL CHECK(length(spawn_call_id) BETWEEN 1 AND 256),
    parent_thread_id TEXT NOT NULL CHECK(length(parent_thread_id) BETWEEN 1 AND 128),
    child_thread_id TEXT CHECK(child_thread_id IS NULL OR length(child_thread_id) BETWEEN 1 AND 128),
    fork_turns_mode TEXT NOT NULL CHECK(fork_turns_mode IN ('fullHistory', 'lastNTurns')),
    fork_turns_count INTEGER CHECK(fork_turns_count IS NULL OR fork_turns_count > 0),
    spawn_requested_at_ms INTEGER NOT NULL CHECK(spawn_requested_at_ms >= 0),
    child_created_at_ms INTEGER CHECK(child_created_at_ms IS NULL OR child_created_at_ms >= 0),
    first_event_at_ms INTEGER CHECK(first_event_at_ms IS NULL OR first_event_at_ms >= 0),
    first_new_response_at_ms INTEGER CHECK(first_new_response_at_ms IS NULL OR first_new_response_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= 0),
    projected_fork_bytes INTEGER NOT NULL CHECK(projected_fork_bytes >= 0),
    projected_fork_tokens INTEGER NOT NULL CHECK(projected_fork_tokens >= 0),
    provider_input_tokens INTEGER CHECK(provider_input_tokens IS NULL OR provider_input_tokens >= 0),
    provider_cached_input_tokens INTEGER CHECK(provider_cached_input_tokens IS NULL OR provider_cached_input_tokens >= 0),
    provider_uncached_input_tokens INTEGER CHECK(provider_uncached_input_tokens IS NULL OR provider_uncached_input_tokens >= 0),
    provider_cache_write_input_tokens INTEGER CHECK(provider_cache_write_input_tokens IS NULL OR provider_cache_write_input_tokens >= 0),
    warning_emitted INTEGER NOT NULL DEFAULT 0 CHECK(warning_emitted IN (0, 1)),
    warning_projected_tokens INTEGER CHECK(warning_projected_tokens IS NULL OR warning_projected_tokens >= 0),
    warning_limit_tokens INTEGER CHECK(warning_limit_tokens IS NULL OR warning_limit_tokens > 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0)
);

CREATE INDEX workflow_fork_metrics_parent_idx
    ON workflow_fork_metrics(parent_thread_id, spawn_requested_at_ms, fork_id);

CREATE INDEX workflow_fork_metrics_child_idx
    ON workflow_fork_metrics(child_thread_id);

-- Context item sizes and provenance are sufficient for later inspect/context
-- consumers while keeping raw prompt and response content out of this DB.
CREATE TABLE workflow_fork_context (
    fork_id TEXT NOT NULL REFERENCES workflow_fork_metrics(fork_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    origin TEXT NOT NULL CHECK(origin IN ('inheritedHistory', 'newOutput')),
    byte_count INTEGER NOT NULL CHECK(byte_count >= 0),
    token_count INTEGER NOT NULL CHECK(token_count >= 0),
    PRIMARY KEY(fork_id, sequence)
);

CREATE INDEX workflow_fork_context_fork_idx
    ON workflow_fork_context(fork_id, sequence);
