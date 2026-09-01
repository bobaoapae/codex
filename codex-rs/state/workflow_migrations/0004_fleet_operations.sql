-- Fleet lifecycle coordination is intentionally separate from run history.
-- An operation remains durable after a crash; readers can explicitly recover
-- it, but no migration or startup path retries the operation automatically.

CREATE TABLE workflow_fleet_roots (
    root_run_id TEXT PRIMARY KEY NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    state TEXT NOT NULL CHECK(state IN ('active', 'suspended', 'closed', 'failed')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    admissions_sealed INTEGER NOT NULL DEFAULT 0 CHECK(admissions_sealed IN (0, 1)),
    active_operation_id TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE workflow_fleet_operations (
    operation_id TEXT PRIMARY KEY NOT NULL CHECK(length(operation_id) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL REFERENCES workflow_fleet_roots(root_run_id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('suspend', 'resume', 'close')),
    status TEXT NOT NULL CHECK(status IN ('running', 'recoverable', 'complete', 'failed')),
    expected_generation INTEGER NOT NULL CHECK(expected_generation >= 0),
    new_generation INTEGER NOT NULL CHECK(new_generation > expected_generation),
    expected_member_count INTEGER NOT NULL CHECK(expected_member_count BETWEEN 0 AND 200),
    partial INTEGER NOT NULL DEFAULT 0 CHECK(partial IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE UNIQUE INDEX workflow_fleet_active_operation_idx
    ON workflow_fleet_operations(root_run_id)
    WHERE status IN ('running', 'recoverable');

CREATE INDEX workflow_fleet_operations_root_idx
    ON workflow_fleet_operations(root_run_id, created_at_ms DESC, operation_id DESC);

CREATE TABLE workflow_fleet_member_results (
    operation_id TEXT NOT NULL REFERENCES workflow_fleet_operations(operation_id) ON DELETE CASCADE,
    member_id TEXT NOT NULL CHECK(length(member_id) BETWEEN 1 AND 128),
    thread_id TEXT CHECK(thread_id IS NULL OR length(thread_id) BETWEEN 1 AND 128),
    run_id TEXT CHECK(run_id IS NULL OR length(run_id) BETWEEN 1 AND 128),
    requested_state TEXT NOT NULL CHECK(length(requested_state) BETWEEN 1 AND 32),
    previous_state TEXT CHECK(previous_state IS NULL OR length(previous_state) BETWEEN 1 AND 32),
    final_state TEXT CHECK(final_state IS NULL OR length(final_state) BETWEEN 1 AND 32),
    success INTEGER NOT NULL CHECK(success IN (0, 1)),
    error TEXT CHECK(error IS NULL OR length(error) BETWEEN 1 AND 1024),
    depth INTEGER NOT NULL CHECK(depth BETWEEN 0 AND 1024),
    order_index INTEGER NOT NULL CHECK(order_index BETWEEN 0 AND 1000000),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(operation_id, member_id)
);

CREATE INDEX workflow_fleet_member_results_order_idx
    ON workflow_fleet_member_results(operation_id, depth, order_index, member_id);
