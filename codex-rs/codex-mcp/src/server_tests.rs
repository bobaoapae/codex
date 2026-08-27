use super::McpServerMetadata;
use super::referenced_environment_variables;
use codex_config::AppToolApproval;
use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::McpServerConfig;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[test]
fn remote_http_connections_track_host_headers_but_not_executor_bearer_tokens() {
    let mut config: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://example.com/mcp",
        "environment_id": "executor-1",
        "bearer_token_env_var": "NODE_REPL_AUTH_TOKEN",
        "env_http_headers": {"X-Api-Key": "PATH"},
    }))
    .expect("remote MCP configuration should deserialize");

    assert_eq!(
        referenced_environment_variables(&config),
        vec![("PATH".to_string(), std::env::var_os("PATH"))],
    );

    let remote_host_bearer: McpServerConfig = serde_json::from_value(serde_json::json!({
        "url": "https://example.com/mcp",
        "environment_id": "executor-1",
        "bearer_token_env_var": "PATH",
    }))
    .expect("host-resolved remote MCP configuration should deserialize");
    assert_eq!(
        referenced_environment_variables(&remote_host_bearer),
        vec![("PATH".to_string(), std::env::var_os("PATH"))],
    );

    config.environment_id = DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string();
    assert_eq!(
        referenced_environment_variables(&config),
        vec![
            (
                "NODE_REPL_AUTH_TOKEN".to_string(),
                std::env::var_os("NODE_REPL_AUTH_TOKEN"),
            ),
            ("PATH".to_string(), std::env::var_os("PATH")),
        ],
    );
}

/// FORK: every other approval knob can only tighten, which is right for a
/// server describing its own tools but leaves the user no way to settle a
/// question once. The Desktop's `codex_app` declares `prompt` for
/// `send_message_to_thread`, and `prompt` is unconditionally blocking.
#[test]
fn a_user_override_outranks_the_declared_tool_approval() {
    let metadata = McpServerMetadata {
        environment_id: "local".to_string(),
        pollutes_memory: true,
        origin: None,
        supports_parallel_tool_calls: false,
        default_tools_approval_mode: Some(AppToolApproval::Prompt),
        tool_approval_modes: HashMap::from([(
            "send_message_to_thread".to_string(),
            AppToolApproval::Prompt,
        )]),
        tool_approval_overrides: HashMap::from([(
            "send_message_to_thread".to_string(),
            AppToolApproval::Approve,
        )]),
        root_only_tools: Vec::new(),
    };

    assert_eq!(
        metadata.tool_approval_mode("send_message_to_thread"),
        AppToolApproval::Approve
    );
    // A tool with no override keeps whatever the server declared.
    assert_eq!(
        metadata.tool_approval_mode("create_thread"),
        AppToolApproval::Prompt
    );
}
