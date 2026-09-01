//! Shared row decoding and CAS helpers for fleet coordination.

use super::fleet_types::*;
use super::types::now_ms;
use anyhow::Result;
use anyhow::bail;
use sqlx::Row;

pub(super) async fn ensure_fleet_root(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    root_run_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO workflow_fleet_roots
         (root_run_id, state, generation, admissions_sealed, active_operation_id, updated_at_ms)
         VALUES (?, 'active', 0, 0, NULL, ?)
         ON CONFLICT(root_run_id) DO NOTHING",
    )
    .bind(root_run_id)
    .bind(now_ms())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(super) async fn fleet_state_by_id(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    root_run_id: &str,
) -> Result<FleetState> {
    let row = sqlx::query(
        "SELECT root_run_id, state, generation, admissions_sealed,
                active_operation_id, updated_at_ms
         FROM workflow_fleet_roots WHERE root_run_id = ?",
    )
    .bind(root_run_id)
    .fetch_one(&mut **transaction)
    .await?;
    fleet_state_from_row(&row)
}

pub(super) async fn fleet_operation_by_id(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    operation_id: &str,
) -> Result<FleetOperation> {
    let row = sqlx::query(
        "SELECT operation_id, root_run_id, kind, status, expected_generation,
                new_generation, expected_member_count, partial, created_at_ms, updated_at_ms,
                (SELECT COUNT(*) FROM workflow_fleet_member_results r
                 WHERE r.operation_id = o.operation_id) AS result_count
         FROM workflow_fleet_operations o WHERE o.operation_id = ?",
    )
    .bind(operation_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| anyhow::anyhow!("fleet operation does not exist"))?;
    fleet_operation_from_row(&row)
}

pub(super) fn fleet_state_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<FleetState> {
    Ok(FleetState {
        root_run_id: row.try_get("root_run_id")?,
        state: FleetRootState::from_str(row.try_get::<String, _>("state")?.as_str())?,
        generation: row.try_get("generation")?,
        admissions_sealed: row.try_get::<i64, _>("admissions_sealed")? != 0,
        active_operation_id: row.try_get("active_operation_id")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

pub(super) fn fleet_operation_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<FleetOperation> {
    Ok(FleetOperation {
        operation_id: row.try_get("operation_id")?,
        root_run_id: row.try_get("root_run_id")?,
        kind: FleetOperationKind::from_str(row.try_get::<String, _>("kind")?.as_str())?,
        status: FleetOperationStatus::from_str(row.try_get::<String, _>("status")?.as_str())?,
        expected_generation: row.try_get("expected_generation")?,
        new_generation: row.try_get("new_generation")?,
        expected_member_count: u32::try_from(row.try_get::<i64, _>("expected_member_count")?)?,
        result_count: u32::try_from(row.try_get::<i64, _>("result_count")?)?,
        partial: row.try_get::<i64, _>("partial")? != 0,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

pub(super) fn member_result_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<FleetMemberResult> {
    Ok(FleetMemberResult {
        operation_id: row.try_get("operation_id")?,
        member_id: row.try_get("member_id")?,
        thread_id: row.try_get("thread_id")?,
        run_id: row.try_get("run_id")?,
        requested_state: row.try_get("requested_state")?,
        previous_state: row.try_get("previous_state")?,
        final_state: row.try_get("final_state")?,
        success: row.try_get::<i64, _>("success")? != 0,
        error: row.try_get("error")?,
        depth: row.try_get("depth")?,
        order_index: row.try_get("order_index")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}

pub(super) fn same_member_result(stored: &FleetMemberResult, incoming: &FleetMemberResult) -> bool {
    stored.operation_id == incoming.operation_id
        && stored.member_id == incoming.member_id
        && stored.thread_id == incoming.thread_id
        && stored.run_id == incoming.run_id
        && stored.requested_state == incoming.requested_state
        && stored.previous_state == incoming.previous_state
        && stored.final_state == incoming.final_state
        && stored.success == incoming.success
        && stored.error == redact_error(incoming.error.as_deref())
        && stored.depth == incoming.depth
        && stored.order_index == incoming.order_index
}

pub(super) fn redact_error(error: Option<&str>) -> Option<String> {
    error.map(|error| {
        let mut redacted = error
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_FLEET_ERROR_BYTES)
            .collect::<String>();
        if redacted.is_empty() {
            redacted.push_str("redacted error");
        }
        redacted
    })
}

pub(super) fn validate_generation(generation: i64) -> Result<()> {
    if generation < 0 {
        bail!("fleet generation must be non-negative");
    }
    Ok(())
}

pub(super) fn next_generation(generation: i64) -> Result<i64> {
    generation
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("fleet generation overflow"))
}

pub(super) fn validate_operation_transition(
    state: FleetRootState,
    kind: FleetOperationKind,
) -> Result<()> {
    match (state, kind) {
        (FleetRootState::Active, FleetOperationKind::Suspend)
        | (FleetRootState::Active, FleetOperationKind::Close)
        | (FleetRootState::Suspended, FleetOperationKind::Resume)
        | (FleetRootState::Suspended, FleetOperationKind::Close)
        | (FleetRootState::Failed, FleetOperationKind::Close) => Ok(()),
        (FleetRootState::Closed, _) => bail!("closed fleet root cannot be reopened"),
        (FleetRootState::Failed, FleetOperationKind::Suspend) => {
            bail!("failed fleet root cannot be suspended")
        }
        (FleetRootState::Failed, FleetOperationKind::Resume) => {
            bail!("failed fleet root cannot be resumed")
        }
        (FleetRootState::Active, FleetOperationKind::Resume) => {
            bail!("active fleet root is not suspended")
        }
        (FleetRootState::Suspended, FleetOperationKind::Suspend) => {
            bail!("suspended fleet root is already suspended")
        }
    }
}

/// Validate the only operations that may explicitly recover a partial fleet
/// operation.  Recovery is deliberately asymmetric: a suspend/resume may be
/// followed by a resume (to restore an active fleet) or a close, while a
/// partial close may only be completed by another close.  A suspend is never
/// retried implicitly from a recoverable record.
pub(super) fn validate_recoverable_operation_transition(
    state: FleetRootState,
    previous_kind: FleetOperationKind,
    requested_kind: FleetOperationKind,
) -> Result<()> {
    if matches!(state, FleetRootState::Closed) {
        bail!("closed fleet root cannot be recovered");
    }

    match (previous_kind, requested_kind) {
        (FleetOperationKind::Suspend | FleetOperationKind::Resume, FleetOperationKind::Resume)
            if matches!(state, FleetRootState::Active | FleetRootState::Suspended) =>
        {
            Ok(())
        }
        (FleetOperationKind::Suspend | FleetOperationKind::Resume, FleetOperationKind::Close)
        | (FleetOperationKind::Close, FleetOperationKind::Close) => {
            validate_operation_transition(state, FleetOperationKind::Close)
        }
        (FleetOperationKind::Suspend | FleetOperationKind::Resume, _) => {
            bail!("recoverable fleet operation requires an explicit resume or close")
        }
        (FleetOperationKind::Close, _) => {
            bail!("recoverable close operation can only be completed by close")
        }
    }
}
