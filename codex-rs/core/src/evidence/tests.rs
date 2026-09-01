use std::collections::BTreeMap;
use std::collections::HashMap;

use codex_extension_items::ExtensionItem;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_hooks::PostToolUseEvidence;
use codex_hooks::PostToolUseEvidenceAttribution;
use codex_hooks::PostToolUseEvidenceReference;
use codex_hooks::PostToolUseEvidenceStatus;
use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookSource;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_utils_path_uri::PathUri;
use core_test_support::PathBufExt;
use core_test_support::test_path_buf;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::receipt_event_for_event;
use super::receipt_event_for_hook_evidence;
use super::receipt_ids_from_history;

#[test]
fn fork_invariant_receipts_exclude_raw_tool_fields() {
    let thread_id = ThreadId::from_u128(1);
    let turn_id = "turn-1";
    let item = TurnItem::CommandExecution(CommandExecutionItem {
        id: "call-1".to_string(),
        plugin_id: None,
        script_path: None,
        process_id: None,
        command: vec!["secret-command-argument".to_string()],
        cwd: PathUri::from_abs_path(&test_path_buf("/repo").abs()),
        parsed_cmd: Vec::new(),
        source: ExecCommandSource::Agent,
        interaction_input: None,
        status: CommandExecutionStatus::Completed,
        stdout: Some("SECRET_STDOUT".to_string()),
        stderr: Some("SECRET_STDERR".to_string()),
        aggregated_output: Some("SECRET_AGGREGATED".to_string()),
        exit_code: Some(0),
        duration: None,
        formatted_output: Some("SECRET_FORMATTED".to_string()),
    });
    let event = EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        started_at_ms: None,
        completed_at_ms: 1_700_000_000_000,
    });

    let first = receipt_event_for_event(thread_id, &event).expect("command receipt");
    let second = receipt_event_for_event(thread_id, &event).expect("same command receipt");
    assert_eq!(first.id, second.id);
    let serialized = serde_json::to_string(&first.event).expect("serialize receipt event");
    for secret in [
        "secret-command-argument",
        "SECRET_STDOUT",
        "SECRET_STDERR",
        "SECRET_AGGREGATED",
        "SECRET_FORMATTED",
    ] {
        assert!(!serialized.contains(secret), "raw field leaked: {secret}");
    }
    assert!(serialized.contains("tool.execution"));
    assert!(serialized.contains("call-1") || serialized.contains("turn-1"));
}

#[test]
fn receipt_ids_from_history_prevent_reappending_derived_items() {
    let thread_id = ThreadId::from_u128(2);
    let event = EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent {
        thread_id,
        turn_id: "turn-2".to_string(),
        item: TurnItem::CommandExecution(CommandExecutionItem {
            id: "call-2".to_string(),
            plugin_id: None,
            script_path: None,
            process_id: None,
            command: Vec::new(),
            cwd: PathUri::from_abs_path(&test_path_buf("/repo").abs()),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            status: CommandExecutionStatus::Failed,
            stdout: None,
            stderr: None,
            aggregated_output: None,
            exit_code: Some(1),
            duration: None,
            formatted_output: None,
        }),
        started_at_ms: None,
        completed_at_ms: 1_700_000_000_000,
    });
    let receipt = receipt_event_for_event(thread_id, &event).expect("failed command receipt");
    let history = InitialHistory::Forked(vec![RolloutItem::EventMsg(receipt.event.msg)]);
    assert!(receipt_ids_from_history(&history).contains(&receipt.id));
}

#[test]
fn hook_receipt_preserves_unknown_kind_tags_and_references() {
    let evidence = PostToolUseEvidence {
        kind: "future.check.v2".to_string(),
        subject: "external smoke".to_string(),
        status: PostToolUseEvidenceStatus::Informational,
        tags: BTreeMap::from([(String::from("owner"), String::from("qa"))]),
        refs: vec![PostToolUseEvidenceReference {
            kind: "artifact".to_string(),
            id: "artifact-1".to_string(),
        }],
        metadata: Some(json!({"futureField": {"value": 7}})),
        attribution: PostToolUseEvidenceAttribution {
            handler_id: "post-tool-use:0:/repo/hooks.json".to_string(),
            handler_type: HookHandlerType::Command,
            execution_mode: HookExecutionMode::Sync,
            source: HookSource::User,
        },
    };
    let derived = receipt_event_for_hook_evidence(ThreadId::from_u128(3), "turn-3", &evidence)
        .expect("hook receipt");
    let EventMsg::ItemCompleted(event) = derived.event.msg else {
        panic!("expected receipt item completion");
    };
    let TurnItem::Extension(ExtensionItem::ReceiptAttached(receipt)) = event.item else {
        panic!("expected receipt extension item");
    };
    assert_eq!(receipt.kind, "future.check.v2");
    assert_eq!(receipt.tags, evidence.tags);
    assert_eq!(receipt.refs.len(), 1);
    assert_eq!(receipt.metadata, evidence.metadata);
    assert_eq!(receipt.turn_id.as_deref(), Some("turn-3"));
}

#[test]
fn fork_invariant_hook_evidence_rejects_raw_metadata() {
    let evidence = PostToolUseEvidence {
        kind: "check".to_string(),
        subject: "tool result".to_string(),
        status: PostToolUseEvidenceStatus::Pass,
        tags: BTreeMap::new(),
        refs: Vec::new(),
        metadata: Some(json!({"stdout": "must not persist"})),
        attribution: PostToolUseEvidenceAttribution {
            handler_id: "hook-1".to_string(),
            handler_type: HookHandlerType::Command,
            execution_mode: HookExecutionMode::Sync,
            source: HookSource::User,
        },
    };

    assert!(receipt_event_for_hook_evidence(ThreadId::from_u128(4), "turn-4", &evidence).is_err());

    let mut asynchronous = evidence.clone();
    asynchronous.attribution.execution_mode = HookExecutionMode::Async;
    asynchronous.metadata = None;
    assert!(
        receipt_event_for_hook_evidence(ThreadId::from_u128(4), "turn-4", &asynchronous).is_err()
    );

    let mut path_reference = evidence;
    path_reference.metadata = None;
    path_reference.refs = vec![PostToolUseEvidenceReference {
        kind: "artifact".to_string(),
        id: "C:\\secret\\output.txt".to_string(),
    }];
    assert!(
        receipt_event_for_hook_evidence(ThreadId::from_u128(4), "turn-4", &path_reference).is_err()
    );
}

#[test]
fn file_and_turn_receipts_use_explicit_status_without_payloads() {
    let thread_id = ThreadId::from_u128(5);
    let file_event = EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent {
        thread_id,
        turn_id: "turn-5".to_string(),
        item: TurnItem::FileChange(FileChangeItem {
            id: "patch-5".to_string(),
            changes: HashMap::new(),
            status: Some(PatchApplyStatus::Declined),
            auto_approved: Some(false),
            stdout: Some("SECRET_PATCH_STDOUT".to_string()),
            stderr: Some("SECRET_PATCH_STDERR".to_string()),
        }),
        started_at_ms: None,
        completed_at_ms: 1_700_000_000_000,
    });
    let receipt = receipt_event_for_event(thread_id, &file_event).expect("file receipt");
    let file_serialized = serde_json::to_string(&receipt.event).expect("serialize file receipt");
    assert!(file_serialized.contains("blocked"));
    assert!(!file_serialized.contains("SECRET_PATCH_STDOUT"));
    assert!(!file_serialized.contains("SECRET_PATCH_STDERR"));

    let turn_event = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-5".to_string(),
        last_agent_message: Some("SECRET_AGENT_MESSAGE".to_string()),
        error: None,
        started_at: Some(1_700_000_000),
        completed_at: Some(1_700_000_001),
        duration_ms: Some(1_000),
        time_to_first_token_ms: None,
    });
    let turn_receipt = receipt_event_for_event(thread_id, &turn_event).expect("turn receipt");
    let turn_serialized =
        serde_json::to_string(&turn_receipt.event).expect("serialize turn receipt");
    assert!(turn_serialized.contains("turn.outcome"));
    assert!(!turn_serialized.contains("SECRET_AGENT_MESSAGE"));
}
