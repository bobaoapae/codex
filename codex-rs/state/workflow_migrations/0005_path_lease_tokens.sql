-- Path leases now keep a display path and a caller-normalized comparison key,
-- plus a separate fencing token. Existing leases are retained; legacy paths
-- are copied into both path columns and receive an opaque token.
CREATE TABLE workflow_path_leases_v5 (
    lease_id TEXT PRIMARY KEY NOT NULL CHECK(length(lease_id) BETWEEN 1 AND 128),
    lease_token TEXT NOT NULL CHECK(length(lease_token) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    owner_run_id TEXT NOT NULL CHECK(length(owner_run_id) BETWEEN 1 AND 128),
    environment_id TEXT CHECK(environment_id IS NULL OR length(environment_id) BETWEEN 1 AND 128),
    path_display TEXT NOT NULL CHECK(length(path_display) BETWEEN 1 AND 4096),
    path_key TEXT NOT NULL CHECK(length(path_key) BETWEEN 1 AND 4096),
    mode TEXT NOT NULL CHECK(mode IN ('read', 'write')),
    generation INTEGER NOT NULL CHECK(generation >= 0),
    state TEXT NOT NULL CHECK(state IN ('active', 'released', 'expired', 'recoverable')),
    issued_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    released_at_ms INTEGER,
    override_receipt_id TEXT CHECK(override_receipt_id IS NULL OR length(override_receipt_id) BETWEEN 1 AND 128)
);

INSERT INTO workflow_path_leases_v5 (
    lease_id, lease_token, root_run_id, owner_run_id, environment_id,
    path_display, path_key, mode, generation, state, issued_at_ms,
    expires_at_ms, released_at_ms, override_receipt_id
)
SELECT
    lease_id, lower(hex(randomblob(16))), root_run_id, owner_run_id, NULL,
    path, path, mode, generation, state, issued_at_ms,
    expires_at_ms, released_at_ms, override_receipt_id
FROM workflow_path_leases;

DROP TABLE workflow_path_leases;
ALTER TABLE workflow_path_leases_v5 RENAME TO workflow_path_leases;

CREATE INDEX workflow_path_leases_path_idx
    ON workflow_path_leases(root_run_id, path_key, state);
CREATE INDEX workflow_path_leases_owner_idx
    ON workflow_path_leases(owner_run_id, state);

CREATE TABLE workflow_path_lease_overrides (
    override_id TEXT PRIMARY KEY NOT NULL CHECK(length(override_id) BETWEEN 1 AND 128),
    token TEXT NOT NULL CHECK(length(token) BETWEEN 1 AND 128),
    root_run_id TEXT NOT NULL CHECK(length(root_run_id) BETWEEN 1 AND 128),
    paths_json TEXT NOT NULL CHECK(length(paths_json) BETWEEN 1 AND 65536),
    conflict_owner_run_ids_json TEXT NOT NULL CHECK(length(conflict_owner_run_ids_json) BETWEEN 1 AND 65536),
    operation_digest TEXT NOT NULL CHECK(length(operation_digest) BETWEEN 1 AND 128),
    reason TEXT NOT NULL CHECK(length(reason) BETWEEN 1 AND 1024),
    receipt_id TEXT NOT NULL CHECK(length(receipt_id) BETWEEN 1 AND 128),
    generation INTEGER NOT NULL CHECK(generation >= 0),
    created_at_ms INTEGER NOT NULL,
    consumed_at_ms INTEGER
);

CREATE INDEX workflow_path_lease_overrides_root_idx
    ON workflow_path_lease_overrides(root_run_id, consumed_at_ms, created_at_ms);
