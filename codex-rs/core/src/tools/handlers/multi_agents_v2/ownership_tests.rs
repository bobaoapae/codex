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

/// FORK: a grant naming only scratch space used to fail with "ownership grant
/// returned no lease". Those paths need no lease at all, so the grant succeeds
/// and says why.
#[test]
fn a_grant_over_scratch_space_reports_a_note_instead_of_failing() {
    let home = tempfile::tempdir().expect("temporary home");
    let repo = home.path().join("workspace");
    let scratch = home.path().join("visualizations");
    std::fs::create_dir_all(repo.join("src")).expect("workspace source directory");
    std::fs::create_dir_all(scratch.join("thread-1")).expect("scratch directory");
    let roots = AuthorizedWorkspaceRoots::new([repo.clone(), scratch.clone()])
        .expect("authorized roots")
        .with_lease_exempt_roots([scratch.clone()]);

    assert_eq!(
        exempt_path_count(
            &roots,
            &[
                scratch.join("thread-1").join("chart.html"),
                repo.join("src")
            ]
        ),
        1
    );
    assert_eq!(exempt_path_count(&roots, &[repo.join("src")]), 0);
    // An unnormalizable path is the grant's problem to report, not a scratch path.
    assert_eq!(
        exempt_path_count(&roots, &[home.path().join("elsewhere").join("x")]),
        0
    );

    let result = OwnershipGrantResult {
        leases: Vec::new(),
        note: Some(scratch_note(1, &scratch)),
    };
    let json = serde_json::to_value(&result).expect("grant result serializes");
    assert_eq!(json["leases"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        json["note"].as_str(),
        Some(
            format!(
                "1 path(s) under {} are private scratch space and need no lease",
                scratch.display()
            )
            .as_str()
        )
    );

    // Nothing exempt, nothing to say.
    let quiet = OwnershipGrantResult {
        leases: Vec::new(),
        note: None,
    };
    let json = serde_json::to_value(&quiet).expect("grant result serializes");
    assert!(json.get("note").is_none(), "{json}");
}
