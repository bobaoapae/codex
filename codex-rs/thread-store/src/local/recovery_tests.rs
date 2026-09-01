use std::fs;

use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::super::LocalThreadStore;
use super::super::test_support::test_config;
use crate::AppendThreadItemsParams;
use crate::CreateThreadParams;
use crate::RecoveryBlockReason;
use crate::RecoveryCreateParams;
use crate::RecoveryCreateResult;
use crate::RecoveryEncryptedAgentMessageCandidate;
use crate::RecoveryLimits;
use crate::RecoveryPolicy;
use crate::RecoveryPreviewParams;
use crate::RecoveryQuiescenceParams;
use crate::RecoveryRetryTurnCandidate;
use crate::RecoveryTurnCompleteCandidate;
use crate::RecoveryTurnState;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;

const POISON_ERROR: &str = "stream disconnected before completion: Encrypted function output content could not be decrypted or decoded.";

#[tokio::test]
async fn preview_and_create_recovery_drop_only_attested_poison_and_retry_turns() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
    let original_bytes = fs::read(&rollout_path).expect("read original rollout");

    let preview = store
        .preview_recovery(recovery_preview_params(thread_id, false))
        .await
        .expect("preview recovery");
    assert!(preview.can_recover);
    assert_eq!(preview.source_thread_id, thread_id);
    assert_eq!(preview.source_rollout_id, thread_id);
    assert_eq!(preview.excluded_items.len(), 6);
    assert!(preview.token.is_some());

    let token = preview.token.expect("recovery token");
    assert_eq!(token.token_id, token.recovered_thread_id);
    let prepared = match store
        .create_recovery(RecoveryCreateParams {
            token: token.clone(),
            quiescence: None,
            has_live_descendants: false,
        })
        .await
        .expect("create recovery")
    {
        RecoveryCreateResult::Prepared(prepared) => prepared,
        RecoveryCreateResult::Existing(_) => panic!("first recovery create must prepare child"),
    };
    assert_ne!(prepared.recovered_thread_id, thread_id);
    assert_eq!(prepared.source_thread_id, thread_id);
    assert_eq!(prepared.source_rollout_id, thread_id);
    assert_eq!(prepared.excluded_items.len(), 6);
    assert!(!prepared.model_context.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(envelope)
                if envelope.item.id().is_some_and(|id| id.as_str() == "amsg_poison")
        )
    }));
    assert!(!prepared.model_context.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == "original"
        )
    }));
    assert!(prepared.model_context.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(envelope)
                if matches!(
                    &envelope.item,
                    ResponseItem::AgentMessage { id: Some(id), content, .. }
                        if id.as_str() == "amsg_valid"
                            && content.iter().any(|part| matches!(
                                part,
                                AgentMessageInputContent::EncryptedContent { .. }
                            ))
                )
        )
    }));
    assert!(prepared.model_context.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(envelope)
                if matches!(&envelope.item, ResponseItem::Message { content, .. }
                    if content == &vec![ContentItem::OutputText {
                        text: "safe after retries".to_string()
                    }])
        )
    }));
    assert!(!prepared.model_context.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                if event.turn_id.starts_with("retry-")
        )
    }));
    drop(prepared);

    let recovered_thread_id = token.recovered_thread_id;
    create_recovery_child(&store, thread_id, recovered_thread_id).await;
    let result = store
        .create_recovery(RecoveryCreateParams {
            token: token.clone(),
            quiescence: None,
            has_live_descendants: false,
        })
        .await
        .expect("second recovery create should be idempotent");
    let RecoveryCreateResult::Existing(existing) = result else {
        panic!("second recovery create must return the existing child");
    };
    assert_eq!(existing.source_thread_id, thread_id);
    assert_eq!(existing.recovered_thread_id, recovered_thread_id);

    assert_eq!(
        fs::read(&rollout_path).expect("read source after recovery"),
        original_bytes
    );
}

#[tokio::test]
async fn preview_blocks_live_descendants_without_returning_a_token() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");

    let preview = store
        .preview_recovery(recovery_preview_params(thread_id, true))
        .await
        .expect("preview recovery");
    assert!(!preview.can_recover);
    assert_eq!(
        preview.blocked_reason,
        Some(RecoveryBlockReason::LiveDescendants)
    );
    assert!(preview.token.is_none());
}

#[tokio::test]
async fn deterministic_child_id_rejects_an_incompatible_existing_thread() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
    let preview = store
        .preview_recovery(recovery_preview_params(thread_id, false))
        .await
        .expect("preview recovery");
    let token = preview.token.expect("recovery token");
    let foreign_thread_id = ThreadId::new();
    create_recovery_child_with_parent(&store, foreign_thread_id, token.recovered_thread_id).await;
    let error = store
        .create_recovery(RecoveryCreateParams {
            token,
            quiescence: None,
            has_live_descendants: false,
        })
        .await
        .expect_err("incompatible deterministic child must be rejected");
    assert!(matches!(error, crate::ThreadStoreError::Conflict { .. }));
}

#[tokio::test]
async fn preview_fails_closed_for_a_candidate_that_is_not_encrypted_agent_message() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
    let (items, _, _) = codex_rollout::RolloutRecorder::load_rollout_items(&rollout_path)
        .await
        .expect("load source");
    let safe_ordinal = items
        .iter()
        .enumerate()
        .find_map(|(ordinal, item)| {
            matches!(item, RolloutItem::ResponseItem(envelope)
                if matches!(&envelope.item, ResponseItem::Message { .. }))
            .then_some(u64::try_from(ordinal).expect("ordinal"))
        })
        .expect("safe response ordinal");

    let mut params = recovery_preview_params(thread_id, false);
    params.policy.encrypted_agent_messages[0].rollout_ordinal = safe_ordinal;
    let preview = store
        .preview_recovery(params)
        .await
        .expect("preview recovery");
    assert!(!preview.can_recover);
    assert_eq!(
        preview.blocked_reason,
        Some(RecoveryBlockReason::AmbiguousCandidates)
    );
    assert!(preview.token.is_none());
}

#[tokio::test]
async fn create_recovery_rejects_a_source_that_changed_after_preview() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
    let preview = store
        .preview_recovery(recovery_preview_params(thread_id, false))
        .await
        .expect("preview recovery");
    codex_rollout::append_rollout_item_to_path(&rollout_path, &safe_response())
        .await
        .expect("append after preview");

    let error = store
        .create_recovery(RecoveryCreateParams {
            token: preview.token.expect("recovery token"),
            quiescence: None,
            has_live_descendants: false,
        })
        .await
        .expect_err("changed source must be rejected");
    assert!(matches!(error, crate::ThreadStoreError::Conflict { .. }));
}

#[tokio::test]
async fn preview_blocks_history_that_exceeds_explicit_limits() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");

    let mut params = recovery_preview_params(thread_id, false);
    params.policy.limits = RecoveryLimits {
        max_items: 1,
        max_serialized_bytes: 1,
    };
    let preview = store
        .preview_recovery(params)
        .await
        .expect("preview recovery");
    assert!(!preview.can_recover);
    assert_eq!(
        preview.blocked_reason,
        Some(RecoveryBlockReason::ContextTooLarge)
    );
    assert!(preview.token.is_none());
}

#[tokio::test]
async fn quiesced_loaded_writer_can_be_previewed_and_prepared() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;

    let attestation = store
        .attest_recovery_quiescence(RecoveryQuiescenceParams {
            thread_id,
            turn_state: RecoveryTurnState::Idle,
        })
        .await
        .expect("quiescence attestation");
    assert_eq!(attestation.thread_id, thread_id);
    assert_eq!(attestation.turn_state, RecoveryTurnState::Idle);

    let mut params = recovery_preview_params(thread_id, false);
    params.quiescence = Some(attestation);
    let preview = store
        .preview_recovery(params)
        .await
        .expect("quiesced recovery preview");
    assert!(preview.can_recover);
    let token = preview.token.expect("recovery token");
    let result = store
        .create_recovery(RecoveryCreateParams {
            token,
            quiescence: Some(attestation),
            has_live_descendants: false,
        })
        .await
        .expect("quiesced recovery create");
    let RecoveryCreateResult::Prepared(prepared) = result else {
        panic!("quiesced recovery should prepare a child");
    };
    drop(prepared);
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
}

#[tokio::test]
async fn quiescence_attestation_rejects_active_or_unknown_turn_state() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;

    for turn_state in [RecoveryTurnState::Active, RecoveryTurnState::Unknown] {
        let error = store
            .attest_recovery_quiescence(RecoveryQuiescenceParams {
                thread_id,
                turn_state,
            })
            .await
            .expect_err("non-idle turn must not be attested");
        assert!(matches!(
            error,
            crate::ThreadStoreError::InvalidRequest { .. }
        ));
    }
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");
}

#[tokio::test]
async fn recovery_excludes_a_decrypt_failure_without_a_turn_start_boundary() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_orphan_poisoned_thread(&store, thread_id).await;
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");

    let mut params = recovery_preview_params(thread_id, false);
    params.policy.encrypted_agent_messages[0].rollout_ordinal = 1;
    params.policy.contaminated_turn_completions[0].rollout_ordinal = 2;
    params.policy.contaminated_turn_completions[0].turn_id = "orphan".to_string();
    params.policy.retry_turns.clear();
    let preview = store
        .preview_recovery(params)
        .await
        .expect("orphan recovery preview");
    assert!(preview.can_recover);
    assert_eq!(preview.excluded_items.len(), 2);
    let original = fs::read(&rollout_path).expect("read source");
    let result = store
        .create_recovery(RecoveryCreateParams {
            token: preview.token.expect("recovery token"),
            quiescence: None,
            has_live_descendants: false,
        })
        .await
        .expect("orphan recovery create");
    let RecoveryCreateResult::Prepared(prepared) = result else {
        panic!("orphan recovery should prepare a child");
    };
    assert!(prepared.model_context.iter().all(|item| {
        !matches!(item, RolloutItem::EventMsg(EventMsg::TurnComplete(event))
            if event.turn_id == "orphan")
    }));
    drop(prepared);
    assert_eq!(
        fs::read(&rollout_path).expect("read source after create"),
        original
    );
}

#[tokio::test]
async fn reusable_recovery_scanner_reports_bounded_offsets() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = ThreadId::new();
    create_poisoned_thread(&store, thread_id).await;
    let rollout_path = store
        .live_rollout_path(thread_id)
        .await
        .expect("rollout path");
    store
        .shutdown_thread(thread_id)
        .await
        .expect("shutdown source");

    let scan = crate::scan_recovery_rollout(
        &rollout_path,
        thread_id,
        RecoveryLimits {
            max_items: 1_000,
            max_serialized_bytes: 1_000_000,
        },
    )
    .expect("bounded recovery scan");
    assert_eq!(scan.meta.as_ref().map(|meta| meta.meta.id), Some(thread_id));
    assert_eq!(scan.item_count, scan.next_ordinal as usize);
    assert!(!scan.buffer_limit_exceeded);
    assert_eq!(
        scan.records.last().map(|record| record.end_byte_offset),
        Some(scan.end_byte_offset)
    );
}

fn recovery_preview_params(
    thread_id: ThreadId,
    has_live_descendants: bool,
) -> RecoveryPreviewParams {
    RecoveryPreviewParams {
        thread_id,
        include_archived: false,
        policy: RecoveryPolicy {
            encrypted_agent_messages: vec![RecoveryEncryptedAgentMessageCandidate {
                rollout_ordinal: 2,
                item_id: Some("amsg_poison".to_string()),
                provider_id: "chatgpt_web".to_string(),
            }],
            contaminated_turn_completions: vec![RecoveryTurnCompleteCandidate {
                rollout_ordinal: 4,
                turn_id: "original".to_string(),
                error_message: POISON_ERROR.to_string(),
            }],
            retry_turns: vec![
                RecoveryRetryTurnCandidate {
                    turn_id: "retry-1".to_string(),
                    error_message: POISON_ERROR.to_string(),
                },
                RecoveryRetryTurnCandidate {
                    turn_id: "retry-2".to_string(),
                    error_message: POISON_ERROR.to_string(),
                },
            ],
            limits: RecoveryLimits {
                max_items: 1_000,
                max_serialized_bytes: 1_000_000,
            },
        },
        quiescence: None,
        has_live_descendants,
    }
}

async fn create_poisoned_thread(store: &LocalThreadStore, thread_id: ThreadId) {
    store
        .create_thread(CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Cli,
            thread_source: None,
            originator: "test-originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Paginated,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: "window-1".to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(std::env::current_dir().expect("cwd")),
                model_provider: "openai".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await
        .expect("create source");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![
                turn_started("original"),
                poisoned_agent_message(),
                valid_encrypted_agent_message(),
                turn_completed_with_error("original"),
                turn_started("retry-1"),
                turn_completed_with_error("retry-1"),
                turn_started("retry-2"),
                turn_completed_with_error("retry-2"),
                safe_response(),
            ],
        })
        .await
        .expect("append source");
    store.flush_thread(thread_id).await.expect("flush source");
}

async fn create_orphan_poisoned_thread(store: &LocalThreadStore, thread_id: ThreadId) {
    store
        .create_thread(CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Cli,
            thread_source: None,
            originator: "test-originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Paginated,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: "window-1".to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(std::env::current_dir().expect("cwd")),
                model_provider: "openai".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await
        .expect("create orphan source");
    store
        .append_items(AppendThreadItemsParams {
            thread_id,
            items: vec![
                poisoned_agent_message(),
                turn_completed_with_error("orphan"),
                safe_response(),
            ],
        })
        .await
        .expect("append orphan source");
    store
        .flush_thread(thread_id)
        .await
        .expect("flush orphan source");
}

async fn create_recovery_child(
    store: &LocalThreadStore,
    source_thread_id: ThreadId,
    recovered_thread_id: ThreadId,
) {
    create_recovery_child_with_parent(store, source_thread_id, recovered_thread_id).await;
}

async fn create_recovery_child_with_parent(
    store: &LocalThreadStore,
    parent_thread_id: ThreadId,
    recovered_thread_id: ThreadId,
) {
    store
        .create_thread(CreateThreadParams {
            session_id: recovered_thread_id.into(),
            thread_id: recovered_thread_id,
            extra_config: None,
            forked_from_id: Some(parent_thread_id),
            parent_thread_id: None,
            source: SessionSource::Custom("recovery".to_string()),
            thread_source: Some(ThreadSource::Feature("recovery".to_string())),
            originator: "test-originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Paginated,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: "window-recovery".to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(std::env::current_dir().expect("cwd")),
                model_provider: "openai".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        })
        .await
        .expect("create deterministic recovery child");
    store
        .persist_thread(recovered_thread_id, crate::PersistContext::Standard)
        .await
        .expect("persist deterministic recovery child");
    store
        .shutdown_thread(recovered_thread_id)
        .await
        .expect("shutdown deterministic recovery child");
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        trace_id: None,
        started_at: Some(10),
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn turn_completed_with_error(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: Some(ErrorEvent {
            message: POISON_ERROR.to_string(),
            codex_error_info: Some(CodexErrorInfo::Other),
            misalignment: None,
        }),
        started_at: Some(10),
        completed_at: Some(20),
        duration_ms: Some(10),
        time_to_first_token_ms: None,
    }))
}

fn poisoned_agent_message() -> RolloutItem {
    RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
        ResponseItem::AgentMessage {
            id: Some(ResponseItemId::from_server("amsg_poison".to_string())),
            author: "local-agent".to_string(),
            recipient: "root".to_string(),
            content: vec![
                AgentMessageInputContent::InputText {
                    text: "Message Type: MESSAGE".to_string(),
                },
                AgentMessageInputContent::EncryptedContent {
                    encrypted_content: "plaintext emitted by local provider".to_string(),
                },
            ],
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}

fn valid_encrypted_agent_message() -> RolloutItem {
    RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
        ResponseItem::AgentMessage {
            id: Some(ResponseItemId::from_server("amsg_valid".to_string())),
            author: "openai-agent".to_string(),
            recipient: "root".to_string(),
            content: vec![AgentMessageInputContent::EncryptedContent {
                encrypted_content: "valid ciphertext".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}

fn safe_response() -> RolloutItem {
    RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
        ResponseItem::Message {
            id: Some(ResponseItemId::from_server("msg_safe".to_string())),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "safe after retries".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}
