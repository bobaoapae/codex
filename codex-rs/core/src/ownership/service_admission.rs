use super::OwnershipError;
use super::service::WorkspaceOwnershipService;
use super::service_helpers::*;
use super::service_types::*;
use codex_state::WorkflowLeaseAcquireRequest;
use codex_state::WorkflowLeaseAuthority;
use codex_state::WorkflowLeaseConflict;
use codex_state::WorkflowLeaseMode;
use codex_state::WorkflowLeaseOverrideCreate;
use codex_state::WorkflowLeaseOverrideUse;
use codex_state::WorkflowLeasePath;
use codex_state::WorkflowLeaseState;
use codex_state::WorkflowPathLease;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

impl WorkspaceOwnershipService {
    /// Admit a mutation and return normalized paths plus exact lease fences.
    pub async fn authorize_mutation(
        &self,
        request: MutationAuthorizationRequest,
    ) -> Result<MutationGuard, OwnershipError> {
        let (normalized_paths, state_paths) = self.normalize_paths(&request.paths)?;
        validate_operation_digest(&request.operation.digest)?;

        match request.actor.authority() {
            OwnershipAuthority::Subagent => {
                if !request.actor.capabilities().may_request_workspace_lease() {
                    return Err(OwnershipError::ReadOnlyRole);
                }
                if !matches!(
                    request.override_authorization,
                    OwnershipOverrideAuthorization::NotRequested
                ) {
                    return Err(OwnershipError::OverrideRootOnly);
                }
                let leases = self.active_leases().await?;
                let selected =
                    select_actor_leases(&leases, &state_paths, &request.actor.run_id().to_string());
                for (path, normalized) in state_paths.iter().zip(&normalized_paths) {
                    // A lease-exempt scratch root is private to this thread, so
                    // requiring a lease over it only makes a mixed path set
                    // (one repo file, one rendered chart) fail as a whole.
                    if self.authorized_roots.is_lease_exempt(normalized) {
                        continue;
                    }
                    if !selected.iter().any(|lease| {
                        lease.mode == WorkflowLeaseMode::Write
                            && state_path_covers(&lease.path, path)
                    }) {
                        return Err(OwnershipError::LeaseRequired {
                            path: normalized.display_path().to_path_buf(),
                        });
                    }
                }
                Ok(MutationGuard::new(
                    request.actor.run_id(),
                    request.operation.digest,
                    normalized_paths,
                    selected,
                ))
            }
            OwnershipAuthority::Root => {
                self.require_root(request.actor)?;
                let conflicts = self.blocking_child_leases(&state_paths).await?;
                let leases = match request.override_authorization {
                    OwnershipOverrideAuthorization::NotRequested => {
                        if !conflicts.is_empty() {
                            if let Some(proof) = self
                                .find_prepared_override(
                                    &state_paths,
                                    &request.operation,
                                    &conflicts,
                                )
                                .await?
                            {
                                self.acquire_with_override(&state_paths, &request.operation, proof)
                                    .await?
                            } else {
                                return Err(conflict_error(
                                    conflicts,
                                    request.operation.digest,
                                    state_paths,
                                ));
                            }
                        } else {
                            Vec::new()
                        }
                    }
                    OwnershipOverrideAuthorization::Request(override_request) => {
                        if conflicts.is_empty() {
                            return Err(OwnershipError::OverrideNotNeeded);
                        }
                        validate_override_reason(&override_request.reason)?;
                        self.acquire_with_new_override(
                            &state_paths,
                            &request.operation,
                            conflicts,
                            override_request,
                        )
                        .await?
                    }
                    OwnershipOverrideAuthorization::Use(proof) => {
                        if conflicts.is_empty() {
                            return Err(OwnershipError::OverrideMismatch);
                        }
                        self.acquire_with_override(&state_paths, &request.operation, proof)
                            .await?
                    }
                };
                Ok(MutationGuard::new(
                    request.actor.run_id(),
                    request.operation.digest,
                    normalized_paths,
                    leases,
                ))
            }
        }
    }

    /// Revalidate filesystem paths and durable fences immediately before use.
    pub async fn revalidate_guard(&self, guard: &MutationGuard) -> Result<(), OwnershipError> {
        guard.revalidate()?;
        let state_paths = guard
            .paths()
            .iter()
            .map(state_path)
            .collect::<Result<Vec<_>, _>>()?;
        let leases = self.active_leases().await?;
        for expected in guard.leases() {
            let valid = leases.iter().any(|actual| {
                actual.lease_id == expected.lease_id
                    && actual.token == expected.token
                    && actual.generation == expected.generation
                    && actual.state == WorkflowLeaseState::Active
            });
            if !valid {
                return Err(OwnershipError::State {
                    message: "ownership guard lease is stale".to_string(),
                });
            }
        }
        if guard.actor_run_id() == self.root_run_id && guard.leases().is_empty() {
            let conflicts = self
                .blocking_child_leases(&state_paths)
                .await?
                .into_iter()
                .map(|lease| WorkflowLeaseConflict {
                    lease_id: lease.lease_id,
                    owner_run_id: lease.owner_run_id,
                    path: lease.path,
                    mode: lease.mode,
                })
                .collect::<Vec<_>>();
            if !conflicts.is_empty() {
                return Err(OwnershipError::Conflict {
                    conflicts,
                    operation_digest: guard.operation_digest().to_string(),
                    paths: state_paths,
                });
            }
        }
        Ok(())
    }

    /// Require an active write lease for every requested path in one exact
    /// execution environment. This is the durable binding required before a
    /// provider or executor may treat a verified linked worktree as its
    /// complete scope.
    pub(crate) async fn require_full_environment_lease(
        &self,
        guard: &MutationGuard,
        paths: &[PathBuf],
        environment_id: &str,
    ) -> Result<(), OwnershipError> {
        self.revalidate_guard(guard).await?;
        let (normalized_paths, state_paths) = self.normalize_paths(paths)?;
        let owner_run_id = guard.actor_run_id().to_string();
        for (normalized, requested) in normalized_paths.iter().zip(state_paths) {
            if self.authorized_roots.is_lease_exempt(normalized) {
                continue;
            }
            let covered = guard.leases().iter().any(|lease| {
                lease.mode == WorkflowLeaseMode::Write
                    && lease.state == WorkflowLeaseState::Active
                    && lease.owner_run_id == owner_run_id
                    && lease_environment_matches(lease.environment_id.as_deref(), environment_id)
                    && state_path_covers(&lease.path, &requested)
            });
            if !covered {
                return Err(OwnershipError::LeaseRequired {
                    path: normalized.display_path().to_path_buf(),
                });
            }
        }
        Ok(())
    }

    /// Prepare a one-shot root override without mutating the workspace.
    pub async fn prepare_override(
        &self,
        actor: OwnershipActor,
        paths: Vec<PathBuf>,
        operation: MutationOperation,
        reason: String,
        receipt_sink: Arc<dyn OwnershipReceiptSink>,
    ) -> Result<WorkflowLeaseOverrideUse, OwnershipError> {
        self.require_root(actor)?;
        let (_, state_paths) = self.normalize_paths(&paths)?;
        validate_operation_digest(&operation.digest)?;
        validate_override_reason(&reason)?;
        let conflicts = self.blocking_child_leases(&state_paths).await?;
        if conflicts.is_empty() {
            return Err(OwnershipError::OverrideNotNeeded);
        }
        self.prepare_override_from_conflicts(
            &state_paths,
            &operation,
            conflicts,
            OwnershipOverrideRequest {
                reason,
                receipt_sink,
            },
        )
        .await
    }

    async fn acquire_with_new_override(
        &self,
        paths: &[WorkflowLeasePath],
        operation: &MutationOperation,
        conflicts: Vec<WorkflowPathLease>,
        override_request: OwnershipOverrideRequest,
    ) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        let proof = self
            .prepare_override_from_conflicts(paths, operation, conflicts, override_request)
            .await?;
        self.acquire_with_override(paths, operation, proof).await
    }

    async fn prepare_override_from_conflicts(
        &self,
        paths: &[WorkflowLeasePath],
        operation: &MutationOperation,
        conflicts: Vec<WorkflowPathLease>,
        override_request: OwnershipOverrideRequest,
    ) -> Result<WorkflowLeaseOverrideUse, OwnershipError> {
        let mut conflict_owner_run_ids = conflicts
            .iter()
            .map(|lease| lease.owner_run_id.clone())
            .collect::<Vec<_>>();
        conflict_owner_run_ids.sort();
        conflict_owner_run_ids.dedup();
        if let Some(record) = self
            .workflow
            .find_unconsumed_path_lease_override(
                &self.root_run_id.to_string(),
                &operation.digest,
                paths,
                &conflict_owner_run_ids,
            )
            .await
            .map_err(map_state_error)?
        {
            return Ok(WorkflowLeaseOverrideUse {
                override_id: record.override_id,
                token: record.token,
                generation: record.generation,
                operation_digest: record.operation_digest,
                paths: record.paths,
                conflict_owner_run_ids: record.conflict_owner_run_ids,
            });
        }
        let receipt = OwnershipOverrideReceipt {
            receipt_id: format!("ownership-override-{}", Uuid::now_v7()),
            root_run_id: self.root_run_id,
            paths: paths.to_vec(),
            conflict_owner_run_ids,
            operation_digest: operation.digest.clone(),
            reason: override_request.reason,
        };
        override_request
            .receipt_sink
            .append_ownership_override_receipt(receipt.clone())
            .await
            .map_err(|error| OwnershipError::Receipt {
                message: error.to_string(),
            })?;
        let record = self
            .workflow
            .issue_path_lease_override(&WorkflowLeaseOverrideCreate {
                root_run_id: self.root_run_id.to_string(),
                paths: receipt.paths.clone(),
                conflict_owner_run_ids: receipt.conflict_owner_run_ids.clone(),
                operation_digest: receipt.operation_digest.clone(),
                reason: receipt.reason.clone(),
                receipt_id: receipt.receipt_id,
            })
            .await
            .map_err(map_state_error)?;
        Ok(WorkflowLeaseOverrideUse {
            override_id: record.override_id,
            token: record.token,
            generation: record.generation,
            operation_digest: record.operation_digest,
            paths: record.paths,
            conflict_owner_run_ids: record.conflict_owner_run_ids,
        })
    }

    async fn acquire_with_override(
        &self,
        paths: &[WorkflowLeasePath],
        operation: &MutationOperation,
        proof: WorkflowLeaseOverrideUse,
    ) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        if proof.operation_digest != operation.digest || proof.paths != paths {
            return Err(OwnershipError::OverrideMismatch);
        }
        let request = WorkflowLeaseAcquireRequest {
            root_run_id: self.root_run_id.to_string(),
            owner_run_id: self.root_run_id.to_string(),
            environment_id: None,
            paths: paths.to_vec(),
            mode: WorkflowLeaseMode::Write,
            lease_duration_ms: duration_millis(Duration::from_secs(15 * 60))?,
            authority: WorkflowLeaseAuthority::RootOverride(proof),
        };
        self.workflow
            .acquire_path_leases(&request)
            .await
            .map_err(map_state_error)
    }

    /// Every lease row recorded for this root, in any state.
    ///
    /// Rows are never deleted, so this includes released, expired and
    /// recoverable claims. Only diagnostics and full listings want it.
    pub(super) async fn all_leases(&self) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        self.workflow
            .expire_path_leases(chrono::Utc::now().timestamp_millis())
            .await
            .map_err(map_state_error)?;
        self.workflow
            .list_path_leases(&self.root_run_id.to_string())
            .await
            .map_err(map_state_error)
    }

    /// FORK: only the leases that still hold a claim.
    ///
    /// This used to return every row, so a child lease that had been released
    /// or had expired blocked the root on that path forever and could only be
    /// cleared with a one-shot override per operation digest.
    pub(super) async fn active_leases(&self) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        Ok(self
            .all_leases()
            .await?
            .into_iter()
            .filter(|lease| lease.state == WorkflowLeaseState::Active)
            .collect())
    }

    /// FORK: the conflict set that blocks a root mutation over `state_paths`.
    ///
    /// A read lease held by a child does not block the root, but once anything
    /// blocks, every overlapping active lease belongs to the set: the store's
    /// own conflict scan is mode-agnostic for a write acquisition, and an
    /// override proof whose owner set omits a read holder is rejected as a
    /// mismatch when it is consumed.
    async fn blocking_child_leases(
        &self,
        state_paths: &[WorkflowLeasePath],
    ) -> Result<Vec<WorkflowPathLease>, OwnershipError> {
        let overlapping = self
            .active_leases()
            .await?
            .into_iter()
            .filter(|lease| lease.owner_run_id != self.root_run_id.to_string())
            .filter(|lease| {
                state_paths
                    .iter()
                    .any(|path| state_path_overlaps(&lease.path, path))
            })
            .collect::<Vec<_>>();
        if overlapping
            .iter()
            .any(|lease| lease.mode == WorkflowLeaseMode::Write)
        {
            Ok(overlapping)
        } else {
            Ok(Vec::new())
        }
    }

    async fn find_prepared_override(
        &self,
        paths: &[WorkflowLeasePath],
        operation: &MutationOperation,
        conflicts: &[WorkflowPathLease],
    ) -> Result<Option<WorkflowLeaseOverrideUse>, OwnershipError> {
        let mut owners = conflicts
            .iter()
            .map(|lease| lease.owner_run_id.clone())
            .collect::<Vec<_>>();
        owners.sort();
        owners.dedup();
        let Some(record) = self
            .workflow
            .find_unconsumed_path_lease_override(
                &self.root_run_id.to_string(),
                &operation.digest,
                paths,
                owners.as_slice(),
            )
            .await
            .map_err(map_state_error)?
        else {
            return Ok(None);
        };
        Ok(Some(WorkflowLeaseOverrideUse {
            override_id: record.override_id,
            token: record.token,
            generation: record.generation,
            operation_digest: record.operation_digest,
            paths: record.paths,
            conflict_owner_run_ids: record.conflict_owner_run_ids,
        }))
    }
}
