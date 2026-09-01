use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::AgentPath;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ToolSpec;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn spec_name(spec: ToolSpec) -> String {
    let ToolSpec::Function(tool) = spec else {
        panic!("ownership tools must be function specs");
    };
    tool.name
}

#[test]
fn ownership_tool_specs_are_root_only_named_functions() {
    assert_eq!(
        spec_name(super::super::ownership_spec::create_grant_agent_ownership_tool()),
        "grant_agent_ownership"
    );
    assert_eq!(
        spec_name(super::super::ownership_spec::create_release_agent_ownership_tool()),
        "release_agent_ownership"
    );
    assert_eq!(
        spec_name(super::super::ownership_spec::create_override_agent_ownership_tool()),
        "override_agent_ownership"
    );
}

#[test]
fn ownership_arguments_accept_public_camel_case_names() {
    let grant: GrantAgentOwnershipArgs = serde_json::from_value(json!({
        "agent": "worker",
        "paths": ["C:/repo"],
        "mode": "write",
        "ttlMs": 5000,
        "environment": "local"
    }))
    .expect("grant arguments");
    assert_eq!(grant.ttl_ms, Some(5000));
    assert_eq!(grant.environment.as_deref(), Some("local"));

    let release: ReleaseAgentOwnershipArgs = serde_json::from_value(json!({
        "leaseId": "lease",
        "token": "token",
        "generation": 1
    }))
    .expect("release arguments");
    assert_eq!(release.lease_id, "lease");

    let override_args: OverrideAgentOwnershipArgs = serde_json::from_value(json!({
        "operationDigest": "digest",
        "paths": ["C:/repo"],
        "reason": "explicit"
    }))
    .expect("override arguments");
    assert_eq!(override_args.operation_digest, "digest");
}

fn subagent_invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
) -> ToolInvocation {
    ToolInvocation {
        step_context: crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        session,
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::default())),
        call_id: "ownership-call".to_string(),
        tool_name: codex_tools::ToolName::plain("grant_agent_ownership"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    }
}

#[tokio::test]
async fn forged_subagent_invocation_is_rejected_before_arguments_or_state() {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: 1,
        agent_path: Some(AgentPath::root().join("worker").expect("agent path")),
        agent_nickname: Some("worker".to_string()),
        agent_role: Some("worker".to_string()),
    });
    let result = GrantAgentOwnershipHandler
        .handle(subagent_invocation(Arc::new(session), Arc::new(turn)))
        .await;
    let Err(error) = result else {
        panic!("subagents must not invoke ownership tools");
    };
    assert!(error.to_string().contains("only to the root agent"));
}
