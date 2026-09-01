-- Recoverable fleet operations remain durable history after an explicit
-- recovery.  Only an operation still running owns the per-root uniqueness
-- slot; the root row's active_operation_id remains the compare-and-swap
-- fence for the one operation currently being acted on.
DROP INDEX IF EXISTS workflow_fleet_active_operation_idx;

CREATE UNIQUE INDEX workflow_fleet_active_operation_idx
    ON workflow_fleet_operations(root_run_id)
    WHERE status = 'running';
