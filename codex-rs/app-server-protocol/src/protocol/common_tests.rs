use super::*;
use anyhow::Result;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn client_response_payload_serializes_without_an_intermediate_json_value() -> Result<()> {
    let payload = ClientResponsePayload::ThreadArchive(v2::ThreadArchiveResponse {});
    assert_eq!(serde_json::to_string(&payload)?, "{}");
    let Some(ClientResponse::ThreadArchive {
        request_id,
        response: _,
    }) = payload.into_client_response(RequestId::Integer(7))
    else {
        panic!("expected thread/archive client response");
    };
    assert_eq!(request_id, RequestId::Integer(7));
    Ok(())
}

#[test]
fn interrupt_conversation_payload_stays_jsonrpc_only() -> Result<()> {
    let payload = ClientResponsePayload::InterruptConversation(v1::InterruptConversationResponse {
        abort_reason: TurnAbortReason::Interrupted,
    });
    assert_eq!(
        serde_json::to_value(&payload)?,
        json!({
            "abortReason": "interrupted",
        })
    );
    assert!(
        payload
            .into_client_response(RequestId::Integer(8))
            .is_none()
    );
    Ok(())
}

#[test]
fn fork_invariant_recovery_requests_use_experimental_wire_methods_and_scopes() -> Result<()> {
    let preview = ClientRequest::ThreadRecoveryPreview {
        request_id: RequestId::Integer(11),
        params: v2::ThreadRecoveryPreviewParams {
            thread_id: "thread-1".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(&preview)?,
        json!({
            "method": "thread/recovery/preview",
            "id": 11,
            "params": { "threadId": "thread-1" },
        })
    );
    assert_eq!(
        preview.serialization_scope(),
        Some(ClientRequestSerializationScope::Thread {
            thread_id: "thread-1".to_string(),
        })
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&preview),
        Some("thread/recovery/preview")
    );

    let create = ClientRequest::ThreadRecoveryCreate {
        request_id: RequestId::Integer(12),
        params: v2::ThreadRecoveryCreateParams {
            token: "opaque-token".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(&create)?,
        json!({
            "method": "thread/recovery/create",
            "id": 12,
            "params": { "token": "opaque-token" },
        })
    );
    assert_eq!(
        create.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("thread-recovery"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&create),
        Some("thread/recovery/create")
    );

    let response = v2::ThreadRecoveryPreviewResponse {
        token: Some("opaque-token".to_string()),
        thread_id: "thread-1".to_string(),
        source_rollout_id: "rollout-1".to_string(),
        source_model_provider: Some("openai".to_string()),
        watermark: v2::ThreadRecoveryWatermark {
            rollout_id: "rollout-1".to_string(),
            end_ordinal_exclusive: 4,
            end_byte_offset: 128,
        },
        source_item_count: 4,
        source_serialized_bytes: 256,
        retained_item_count: 3,
        retained_serialized_bytes: 128,
        excluded_items: vec![v2::ThreadRecoveryExcludedItem {
            rollout_ordinal: 2,
            item_id: Some("item-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            reason: "invalid encrypted content".to_string(),
        }],
        counts: v2::ThreadRecoveryCounts {
            total_items: 4,
            retained_items: 3,
            excluded_items: 1,
            failed_turns: 1,
        },
        can_recover: true,
        reason: None,
        blocked_reason: None,
    };
    assert_eq!(
        serde_json::to_value(response)?,
        json!({
            "token": "opaque-token",
            "threadId": "thread-1",
            "sourceRolloutId": "rollout-1",
            "sourceModelProvider": "openai",
            "watermark": {
                "rolloutId": "rollout-1",
                "endOrdinalExclusive": 4,
                "endByteOffset": 128,
            },
            "sourceItemCount": 4,
            "sourceSerializedBytes": 256,
            "retainedItemCount": 3,
            "retainedSerializedBytes": 128,
            "excludedItems": [{
                "rolloutOrdinal": 2,
                "itemId": "item-1",
                "turnId": "turn-1",
                "reason": "invalid encrypted content",
            }],
            "counts": {
                "totalItems": 4,
                "retainedItems": 3,
                "excludedItems": 1,
                "failedTurns": 1,
            },
            "canRecover": true,
            "reason": null,
            "blockedReason": null,
        })
    );
    Ok(())
}

#[test]
fn fork_invariant_plan_requests_use_experimental_wire_methods() -> Result<()> {
    let list = ClientRequest::PlanList {
        request_id: RequestId::Integer(13),
        params: v2::PlanListParams {
            cursor: None,
            limit: Some(50),
        },
    };
    assert_eq!(
        serde_json::to_value(&list)?,
        json!({
            "method": "plan/list",
            "id": 13,
            "params": { "cursor": null, "limit": 50 },
        })
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&list),
        Some("plan/list")
    );

    let read = ClientRequest::PlanRead {
        request_id: RequestId::Integer(14),
        params: v2::PlanReadParams {
            id: "plan-1".to_string(),
            revision: None,
        },
    };
    assert_eq!(
        serde_json::to_value(&read)?,
        json!({
            "method": "plan/read",
            "id": 14,
            "params": { "id": "plan-1", "revision": null },
        })
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&read),
        Some("plan/read")
    );

    let approve = ClientRequest::PlanApprove {
        request_id: RequestId::Integer(15),
        params: v2::PlanApproveParams {
            id: "plan-1".to_string(),
            expected_revision: 2,
        },
    };
    assert_eq!(
        serde_json::to_value(&approve)?,
        json!({
            "method": "plan/approve",
            "id": 15,
            "params": { "id": "plan-1", "expectedRevision": 2 },
        })
    );
    assert_eq!(
        approve.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("plans"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&approve),
        Some("plan/approve")
    );
    Ok(())
}

#[test]
fn fork_invariant_job_requests_use_experimental_wire_methods_and_scopes() -> Result<()> {
    let run = ClientRequest::JobRun {
        request_id: RequestId::Integer(21),
        params: v2::JobRunParams {
            input: Vec::new(),
            idempotency_key: Some("job-key".to_string()),
            model_provider: Some("openai".to_string()),
            ..Default::default()
        },
    };
    let run_json = serde_json::to_value(&run)?;
    assert_eq!(run_json["method"], "job/run");
    assert_eq!(run_json["id"], 21);
    assert_eq!(run_json["params"]["input"], json!([]));
    assert_eq!(run_json["params"]["idempotencyKey"], "job-key");
    assert_eq!(run_json["params"]["modelProvider"], "openai");
    assert_eq!(
        run.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("workflow"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&run),
        Some("job/run")
    );

    let list = ClientRequest::JobList {
        request_id: RequestId::Integer(22),
        params: v2::JobListParams {
            cursor: Some("cursor-1".to_string()),
            limit: Some(50),
            ..Default::default()
        },
    };
    assert_eq!(
        serde_json::to_value(&list)?,
        json!({
            "method": "job/list",
            "id": 22,
            "params": {
                "cursor": "cursor-1",
                "limit": 50,
                "status": null,
                "outcome": null,
                "rootThreadId": null,
            },
        })
    );
    assert_eq!(
        list.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead(
            "workflow"
        ))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&list),
        Some("job/list")
    );

    let read = ClientRequest::JobRead {
        request_id: RequestId::Integer(23),
        params: v2::JobReadParams {
            job_id: "job-1".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(&read)?,
        json!({
            "method": "job/read",
            "id": 23,
            "params": { "jobId": "job-1" },
        })
    );
    assert_eq!(
        read.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead(
            "workflow"
        ))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&read),
        Some("job/read")
    );

    let cancel = ClientRequest::JobCancel {
        request_id: RequestId::Integer(24),
        params: v2::JobCancelParams {
            job_id: "job-1".to_string(),
        },
    };
    assert_eq!(
        serde_json::to_value(&cancel)?,
        json!({
            "method": "job/cancel",
            "id": 24,
            "params": { "jobId": "job-1" },
        })
    );
    assert_eq!(
        cancel.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("workflow"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&cancel),
        Some("job/cancel")
    );
    Ok(())
}

#[test]
fn fork_invariant_evidence_requests_are_experimental_and_host_only() -> Result<()> {
    let list = ClientRequest::EvidenceList {
        request_id: RequestId::Integer(41),
        params: v2::EvidenceListParams {
            cursor: None,
            limit: Some(25),
            thread_id: Some("thread-1".to_string()),
            job_id: None,
            plan_snapshot_id: None,
            status: Some(v2::EvidenceStatus::Pass),
            kind: Some("physical.smoke".to_string()),
        },
    };
    let list_json = serde_json::to_value(&list)?;
    assert_eq!(list_json["method"], "evidence/list");
    assert_eq!(list_json["params"]["threadId"], "thread-1");
    assert_eq!(list_json["params"]["status"], "pass");
    assert_eq!(
        list.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead(
            "workflow"
        ))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&list),
        Some("evidence/list")
    );

    let attach = ClientRequest::EvidenceAttach {
        request_id: RequestId::Integer(42),
        params: v2::EvidenceAttachParams {
            thread_id: "thread-1".to_string(),
            receipt_id: "receipt-1".to_string(),
            schema_version: 1,
            kind: "physical.smoke".to_string(),
            subject: "smoke".to_string(),
            status: v2::EvidenceStatus::Pass,
            source: "test-hook".to_string(),
            turn_id: None,
            job_id: None,
            plan_snapshot_id: None,
            created_at: None,
            provenance: None,
            tags: None,
            refs: None,
            metadata: None,
        },
    };
    assert_eq!(serde_json::to_value(&attach)?["method"], "evidence/attach");
    assert_eq!(
        attach.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("workflow"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&attach),
        Some("evidence/attach")
    );

    let export = ClientRequest::EvidenceExport {
        request_id: RequestId::Integer(43),
        params: v2::EvidenceExportParams {
            receipt_ids: vec!["receipt-1".to_string()],
        },
    };
    assert_eq!(serde_json::to_value(&export)?["method"], "evidence/export");
    assert_eq!(
        export.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead(
            "workflow"
        ))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&export),
        Some("evidence/export")
    );
    Ok(())
}

#[test]
fn fork_invariant_artifact_read_is_opaque_bounded_and_experimental() -> Result<()> {
    let request = ClientRequest::ArtifactRead {
        request_id: RequestId::Integer(44),
        params: v2::ArtifactReadParams {
            artifact_id: "01984de2-8f74-7c91-a3b2-5c5e937cf318".to_string(),
            cursor: Some("opaque-cursor".to_string()),
            limit: Some(16 * 1024),
        },
    };
    assert_eq!(
        serde_json::to_value(&request)?,
        json!({
            "method": "artifact/read",
            "id": 44,
            "params": {
                "artifactId": "01984de2-8f74-7c91-a3b2-5c5e937cf318",
                "cursor": "opaque-cursor",
                "limit": 16384,
            },
        })
    );
    assert_eq!(
        request.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead(
            "artifacts"
        ))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&request),
        Some("artifact/read")
    );
    Ok(())
}

#[test]
fn fork_invariant_agent_fleet_methods_are_camel_case_and_generation_bound() -> Result<()> {
    assert_eq!(
        serde_json::to_value([
            v2::FleetMemberState::Running,
            v2::FleetMemberState::WaitingForTool,
            v2::FleetMemberState::WaitingForApproval,
            v2::FleetMemberState::WaitingForUser,
            v2::FleetMemberState::Idle,
            v2::FleetMemberState::Suspended,
            v2::FleetMemberState::Closed,
            v2::FleetMemberState::Failed,
        ])?,
        json!([
            "running",
            "waitingForTool",
            "waitingForApproval",
            "waitingForUser",
            "idle",
            "suspended",
            "closed",
            "failed",
        ])
    );
    assert_eq!(
        serde_json::to_value([
            v2::FleetOperationKind::Suspend,
            v2::FleetOperationKind::Resume,
            v2::FleetOperationKind::Close,
        ])?,
        json!(["suspend", "resume", "close"])
    );
    assert_eq!(
        serde_json::to_value([
            v2::FleetOperationStatus::Running,
            v2::FleetOperationStatus::Recoverable,
            v2::FleetOperationStatus::Complete,
            v2::FleetOperationStatus::Failed,
        ])?,
        json!(["running", "recoverable", "complete", "failed"])
    );

    let status = ClientRequest::AgentFleetStatus {
        request_id: RequestId::Integer(51),
        params: v2::AgentFleetStatusParams {
            root_thread_id: "root-1".to_string(),
            cursor: Some("cursor-1".to_string()),
            limit: Some(50),
        },
    };
    assert_eq!(
        serde_json::to_value(&status)?,
        json!({
            "method": "agent/fleet/status",
            "id": 51,
            "params": {
                "rootThreadId": "root-1",
                "cursor": "cursor-1",
                "limit": 50,
            },
        })
    );
    assert_eq!(
        status.serialization_scope(),
        Some(ClientRequestSerializationScope::GlobalSharedRead("fleet"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&status),
        Some("agent/fleet/status")
    );

    let suspend = ClientRequest::AgentFleetSuspend {
        request_id: RequestId::Integer(52),
        params: v2::AgentFleetSuspendParams {
            root_thread_id: "root-1".to_string(),
            expected_generation: 7,
        },
    };
    assert_eq!(
        serde_json::to_value(&suspend)?,
        json!({
            "method": "agent/fleet/suspend",
            "id": 52,
            "params": {"rootThreadId": "root-1", "expectedGeneration": 7},
        })
    );
    assert_eq!(
        suspend.serialization_scope(),
        Some(ClientRequestSerializationScope::Global("fleet"))
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&suspend),
        Some("agent/fleet/suspend")
    );

    let resume = ClientRequest::AgentFleetResume {
        request_id: RequestId::Integer(53),
        params: v2::AgentFleetResumeParams {
            root_thread_id: "root-1".to_string(),
            expected_generation: 8,
        },
    };
    assert_eq!(
        serde_json::to_value(&resume)?["method"],
        "agent/fleet/resume"
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&resume),
        Some("agent/fleet/resume")
    );

    let close = ClientRequest::AgentFleetClose {
        request_id: RequestId::Integer(54),
        params: v2::AgentFleetCloseParams {
            root_thread_id: "root-1".to_string(),
            expected_generation: 9,
        },
    };
    assert_eq!(serde_json::to_value(&close)?["method"], "agent/fleet/close");
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&close),
        Some("agent/fleet/close")
    );

    assert!(
        serde_json::from_value::<v2::AgentFleetSuspendParams>(json!({
            "rootThreadId": "root-1"
        }))
        .is_err()
    );

    let response = v2::AgentFleetSuspendResponse {
        root_thread_id: "root-1".to_string(),
        generation: 8,
        sealed: true,
        operation_id: Some("operation-1".to_string()),
        results: vec![v2::FleetResult {
            operation_id: "operation-1".to_string(),
            member_id: "member-1".to_string(),
            thread_id: Some("thread-1".to_string()),
            run_id: Some("run-1".to_string()),
            requested_state: v2::FleetMemberState::Suspended,
            previous_state: Some(v2::FleetMemberState::Running),
            final_state: Some(v2::FleetMemberState::Suspended),
            success: true,
            error: None,
            depth: 1,
            order_index: 0,
            updated_at: 1_700_000_000,
        }],
        next_cursor: None,
    };
    assert_eq!(
        serde_json::to_value(response)?,
        json!({
            "rootThreadId": "root-1",
            "generation": 8,
            "sealed": true,
            "operationId": "operation-1",
            "results": [{
                "operationId": "operation-1",
                "memberId": "member-1",
                "threadId": "thread-1",
                "runId": "run-1",
                "requestedState": "suspended",
                "previousState": "running",
                "finalState": "suspended",
                "success": true,
                "error": null,
                "depth": 1,
                "orderIndex": 0,
                "updatedAt": 1700000000,
            }],
            "nextCursor": null,
        })
    );
    Ok(())
}

#[test]
fn fork_invariant_thread_search_fts_filters_are_nullable_camel_case() -> Result<()> {
    let request = ClientRequest::ThreadSearch {
        request_id: RequestId::Integer(31),
        params: v2::ThreadSearchParams {
            cursor: None,
            limit: Some(25),
            sort_key: None,
            sort_direction: None,
            model_providers: Some(vec!["openai".to_string()]),
            cwd: Some(v2::ThreadListCwdFilter::Many(vec![
                "/workspace".to_string(),
                "/other-workspace".to_string(),
            ])),
            project_id: Some(Some("project-1".to_string())),
            root_thread_id: Some("root-1".to_string()),
            ancestor_thread_id: Some("ancestor-1".to_string()),
            source_kinds: None,
            archived: None,
            search_term: "durable".to_string(),
            thread_classes: Some(vec![v2::ThreadClass::Interactive]),
            terminal_outcomes: Some(vec![v2::TerminalOutcome::Succeeded]),
        },
    };

    let serialized = serde_json::to_value(&request)?;
    assert_eq!(serialized["method"], "thread/search");
    assert_eq!(serialized["id"], 31);
    assert_eq!(serialized["params"]["modelProviders"], json!(["openai"]));
    assert_eq!(
        serialized["params"]["cwd"],
        json!(["/workspace", "/other-workspace"])
    );
    assert_eq!(serialized["params"]["projectId"], "project-1");
    assert_eq!(serialized["params"]["rootThreadId"], "root-1");
    assert_eq!(serialized["params"]["ancestorThreadId"], "ancestor-1");
    assert_eq!(
        serialized["params"]["threadClasses"],
        json!(["interactive"])
    );
    assert_eq!(
        serialized["params"]["terminalOutcomes"],
        json!(["succeeded"])
    );
    assert_eq!(
        crate::experimental_api::ExperimentalApi::experimental_reason(&request),
        Some("thread/search")
    );
    Ok(())
}
