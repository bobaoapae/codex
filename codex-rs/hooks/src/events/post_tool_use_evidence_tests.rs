use std::collections::HashMap;

use codex_extension_items::receipt::is_forbidden_metadata_key;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookSource;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use serde_json::Value;
use serde_json::json;

use super::parse_completed;
use crate::PostToolUseEvidenceStatus;
use crate::engine::ConfiguredHandler;
use crate::engine::ConfiguredHandlerKind;
use crate::engine::HandlerRunResult;

#[test]
fn accepts_bounded_evidence_without_model_context_and_preserves_attribution() {
    let parsed = parse_completed(
        &handler(/*async*/ false),
        run_result(evidence_json(json!({
            "kind": "test",
            "subject": "unit suite",
            "status": "pass",
            "tags": {"owner": "qa"},
            "refs": [{"kind": "artifact", "id": "artifact-1"}],
            "metadata": {"caseCount": 3}
        }))),
        Some("turn-1".to_string()),
    );

    assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
    assert!(parsed.data.additional_contexts_for_model.is_empty());
    assert!(parsed.data.feedback_messages_for_model.is_empty());
    assert_eq!(parsed.data.evidence.len(), 1);
    let evidence = &parsed.data.evidence[0];
    assert_eq!(evidence.status, PostToolUseEvidenceStatus::Pass);
    assert_eq!(evidence.attribution.handler_id, handler(false).run_id());
    assert_eq!(evidence.attribution.source, HookSource::User);
    assert_eq!(
        evidence.attribution.execution_mode,
        codex_protocol::protocol::HookExecutionMode::Sync
    );
    assert!(
        parsed
            .completed
            .run
            .entries
            .iter()
            .all(|entry| entry.kind != HookOutputEntryKind::Context)
    );
}

#[test]
fn invalid_evidence_is_warned_and_does_not_change_tool_outcome() {
    let parsed = parse_completed(
        &handler(false),
        run_result(
            json!({
                "decision": "block",
                "reason": "pause for review",
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "evidence": {
                        "kind": "test",
                        "subject": "unit suite",
                        "status": "pass",
                        "metadata": {"stdout": "must not be copied"}
                    }
                }
            })
            .to_string(),
        ),
        Some("turn-1".to_string()),
    );

    assert_eq!(parsed.completed.run.status, HookRunStatus::Blocked);
    assert!(parsed.data.should_block);
    assert!(parsed.data.evidence.is_empty());
    assert!(parsed.completed.run.entries.iter().any(|entry| {
        entry.kind == HookOutputEntryKind::Warning
            && entry.text == "PostToolUse hook returned invalid evidence; evidence was ignored"
    }));
}

#[test]
fn schema_unknown_and_oversized_evidence_are_ignored() {
    let unknown_field = json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "evidence": {
                "kind": "test",
                "subject": "unit suite",
                "status": "pass",
                "payload": "raw"
            }
        }
    });
    let oversized_metadata = json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "evidence": {
                "kind": "test",
                "subject": "unit suite",
                "status": "pass",
                "metadata": {"future": "x".repeat(65 * 1024)}
            }
        }
    });

    for stdout in [unknown_field.to_string(), oversized_metadata.to_string()] {
        let parsed = parse_completed(
            &handler(false),
            run_result(stdout),
            Some("turn-1".to_string()),
        );
        assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
        assert!(parsed.data.evidence.is_empty());
        assert!(parsed.completed.run.entries.iter().any(|entry| {
            entry.kind == HookOutputEntryKind::Warning
                && entry.text.contains("evidence was ignored")
        }));
    }
}

#[test]
fn evidence_uses_the_canonical_forbidden_metadata_key_union() {
    let forbidden = [
        "stdout",
        "stderr",
        "aggregatedOutput",
        "arguments",
        "args",
        "ciphertext",
        "encryptedContent",
        "payload",
        "raw",
        "rawPayload",
        "toolInput",
        "toolOutput",
        "toolResponse",
        "path",
        "paths",
        "cwd",
        "workdir",
        "command",
        "argv",
        "env",
        "environment",
        "output",
        "rawOutput",
    ];
    for key in forbidden {
        assert!(
            is_forbidden_metadata_key(key),
            "key should be forbidden: {key}"
        );
        let metadata = serde_json::Map::from_iter([(key.to_string(), json!("not persisted"))]);
        let parsed = parse_completed(
            &handler(false),
            run_result(evidence_json(json!({
                "kind": "test",
                "subject": "unit suite",
                "status": "pass",
                "metadata": metadata
            }))),
            Some("turn-1".to_string()),
        );
        assert!(
            parsed.data.evidence.is_empty(),
            "key should be rejected: {key}"
        );
    }

    for key in ["futureResult", "rawData", "pathology", "environmental"] {
        assert!(
            !is_forbidden_metadata_key(key),
            "key should remain safe: {key}"
        );
        let metadata = serde_json::Map::from_iter([(key.to_string(), json!(true))]);
        let parsed = parse_completed(
            &handler(false),
            run_result(evidence_json(json!({
                "kind": "test",
                "subject": "unit suite",
                "status": "pass",
                "metadata": metadata
            }))),
            Some("turn-1".to_string()),
        );
        assert_eq!(parsed.data.evidence.len(), 1);
    }

    for key in ["AGGREGATED_OUTPUT", "tool-output", "work_dir", "ENV"] {
        assert!(
            is_forbidden_metadata_key(key),
            "separator normalization: {key}"
        );
    }
}

#[test]
fn tags_and_references_are_bounded() {
    let tags = (0..33)
        .map(|index| (format!("tag-{index}"), Value::String("value".to_string())))
        .collect::<serde_json::Map<_, _>>();
    let refs = (0..65)
        .map(|index| json!({"kind": "artifact", "id": format!("artifact-{index}")}))
        .collect::<Vec<_>>();
    let stdout = evidence_json(json!({
        "kind": "test",
        "subject": "unit suite",
        "status": "pass",
        "tags": tags,
        "refs": refs
    }));

    let parsed = parse_completed(
        &handler(false),
        run_result(stdout),
        Some("turn-1".to_string()),
    );
    assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
    assert!(parsed.data.evidence.is_empty());
    assert!(parsed.completed.run.entries.iter().any(|entry| {
        entry.kind == HookOutputEntryKind::Warning && entry.text.contains("evidence was ignored")
    }));
}

#[test]
fn asynchronous_handlers_cannot_emit_evidence() {
    let parsed = parse_completed(
        &handler(/*async*/ true),
        run_result(evidence_json(json!({
            "kind": "test",
            "subject": "unit suite",
            "status": "pass"
        }))),
        Some("turn-1".to_string()),
    );

    assert_eq!(parsed.completed.run.status, HookRunStatus::Completed);
    assert!(parsed.data.evidence.is_empty());
    assert!(parsed.data.additional_contexts_for_model.is_empty());
    assert!(parsed.completed.run.entries.iter().any(|entry| {
        entry.kind == HookOutputEntryKind::Warning
            && entry.text
                == "PostToolUse hook evidence was ignored for an asynchronous or executor-scoped handler"
    }));
}

fn evidence_json(evidence: Value) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "evidence": evidence
        }
    })
    .to_string()
}

fn handler(r#async: bool) -> ConfiguredHandler {
    ConfiguredHandler {
        event_name: HookEventName::PostToolUse,
        matcher: Some("^Bash$".to_string()),
        timeout_sec: 5,
        status_message: Some("running post tool use hook".to_string()),
        additional_context_limit: Default::default(),
        source_path: test_path_buf("/tmp/hooks.json").abs().into(),
        source: HookSource::User,
        builtin: false,
        display_order: 0,
        kind: ConfiguredHandlerKind::Command {
            command: "python3 post_tool_use_hook.py".to_string(),
            r#async,
            env: HashMap::new(),
        },
    }
}

fn run_result(stdout: String) -> HandlerRunResult {
    HandlerRunResult {
        started_at: 1_700_000_000,
        completed_at: 1_700_000_001,
        duration_ms: 12,
        exit_code: Some(0),
        stdout,
        stderr: String::new(),
        error: None,
    }
}
