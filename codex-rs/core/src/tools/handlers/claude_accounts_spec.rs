//! FORK: tool specs for inspecting and choosing Claude Code accounts.

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const CLAUDE_ACCOUNTS_TOOL_NAME: &str = "claude_accounts";
pub(crate) const CLAUDE_ACCOUNT_SELECT_TOOL_NAME: &str = "claude_account_select";

pub(crate) fn create_claude_accounts_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: CLAUDE_ACCOUNTS_TOOL_NAME.to_string(),
        description: "List the configured Claude accounts with their 5-hour and weekly usage, \
             reset times, failover cooldowns, and how many turns are running on each right now. \
             Use it before delegating heavy work to a Claude agent, then pass the account you \
             picked as `spawn_agent(account = …)`. Spreading parallel agents across accounts \
             keeps them off each other's rate limits."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "include_usage".to_string(),
                JsonSchema::boolean(Some(
                    "Fetch live usage (default true). Pass false for a fast local-only listing."
                        .to_string(),
                )),
            )]),
            /*required*/ None,
            Some(false.into()),
        ),
        output_schema: Some(claude_accounts_output_schema()),
    })
}

pub(crate) fn create_claude_account_select_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: CLAUDE_ACCOUNT_SELECT_TOOL_NAME.to_string(),
        description: "Choose which Claude account new work should try first. Affects Claude \
             agents spawned from now on; agents already running keep their account. A usage \
             limit or auth failure still fails over to the other accounts. Pass `auto` to clear \
             the preference. Prefer `spawn_agent(account = …)` when the choice concerns one \
             agent rather than the whole session."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "account".to_string(),
                JsonSchema::string(Some(
                    "Index from `claude_accounts` (1, 2, …), a config-dir path, part of the \
                     account email, or `auto` to clear the preference."
                        .to_string(),
                )),
            )]),
            Some(vec!["account".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(claude_account_select_output_schema()),
    })
}

fn claude_accounts_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "accounts": {
                "type": "array",
                "description": "Configured accounts, in the order the provider considers them.",
                "items": {
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer" },
                        "account": { "type": "string" },
                        "config_dir": { "type": "string" },
                        "logged_in": { "type": "boolean" },
                        "preferred": { "type": "boolean" },
                        "five_hour_used_pct": { "type": ["number", "null"] },
                        "weekly_used_pct": { "type": ["number", "null"] },
                        "remaining_pct": { "type": ["number", "null"] },
                        "five_hour_resets_at": { "type": ["string", "null"] },
                        "weekly_resets_at": { "type": ["string", "null"] },
                        "running_turns": { "type": "integer" },
                        "cooldown_seconds_left": { "type": ["integer", "null"] },
                        "cooldown_reason": { "type": ["string", "null"] },
                        "limit_reset_hint": { "type": ["string", "null"] }
                    },
                    "required": ["index", "account", "config_dir", "logged_in", "preferred", "running_turns"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["accounts"],
        "additionalProperties": false
    })
}

fn claude_account_select_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "preferred_account": {
                "type": ["string", "null"],
                "description": "The account new work will try first, or null when cleared."
            }
        },
        "required": ["preferred_account"],
        "additionalProperties": false
    })
}
