//! FORK: lets the orchestrating agent see Claude account headroom and choose
//! which account new work should use.
//!
//! Without this the only way to pick an account was to hand-edit
//! `claude_code_accounts.json` or run the external MCP bridge, so an agent
//! delegating three Claude tasks had no way to spread them across accounts.

use crate::claude_code::AccountAlias;
use crate::claude_code::accounts::AccountStatus;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::claude_accounts_spec::CLAUDE_ACCOUNT_SELECT_TOOL_NAME;
use crate::tools::handlers::claude_accounts_spec::CLAUDE_ACCOUNTS_TOOL_NAME;
use crate::tools::handlers::claude_accounts_spec::create_claude_account_select_tool;
use crate::tools::handlers::claude_accounts_spec::create_claude_accounts_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json::json;

/// Where the account state file lives, and which accounts are configured.
struct AccountsContext {
    account_dirs: Vec<std::path::PathBuf>,
    state_path: std::path::PathBuf,
}

fn accounts_context(invocation: &ToolInvocation) -> Result<AccountsContext, FunctionCallError> {
    let config = &invocation.turn.config;
    if config.claude_code_account_dirs.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "no Claude accounts are configured; set `[claude_code].account_dirs` in config.toml"
                .to_string(),
        ));
    }
    Ok(AccountsContext {
        account_dirs: config.claude_code_account_dirs.clone(),
        state_path: config
            .codex_home
            .to_path_buf()
            .join(crate::claude_code::accounts::ACCOUNTS_STATE_FILE_NAME),
    })
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeAccountsArgs {
    include_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ClaudeAccountSelectArgs {
    account: String,
}

fn parse_args<T: serde::de::DeserializeOwned + Default>(
    payload: &ToolPayload,
    tool: &str,
) -> Result<T, FunctionCallError> {
    let ToolPayload::Function { arguments } = payload else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool} handler received unsupported payload"
        )));
    };
    if arguments.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse {tool} arguments: {err}"))
    })
}

#[derive(Debug)]
struct ClaudeAccountsOutput {
    accounts: Vec<AccountStatus>,
}

impl ClaudeAccountsOutput {
    fn text(&self) -> String {
        let lines: Vec<String> = self
            .accounts
            .iter()
            .map(|account| {
                let usage = match (account.five_hour_used_pct, account.weekly_used_pct) {
                    (Some(five_hour), Some(weekly)) => {
                        format!("5h {five_hour:.0}% used, weekly {weekly:.0}% used")
                    }
                    _ => "usage unknown".to_string(),
                };
                let mut line = format!("{}. {} — {}", account.index, account.account, usage);
                if !account.logged_in {
                    line.push_str(" — not logged in");
                }
                if account.preferred {
                    line.push_str(" — preferred");
                }
                if account.running_turns > 0 {
                    line.push_str(&format!(" — {} turn(s) running", account.running_turns));
                }
                if let Some(seconds) = account.cooldown_seconds_left {
                    let reason = account.cooldown_reason.as_deref().unwrap_or("cooldown");
                    line.push_str(&format!(" — {reason}, {seconds}s left"));
                }
                if let Some(hint) = account.limit_reset_hint.as_deref() {
                    line.push_str(&format!(" ({hint})"));
                }
                line
            })
            .collect();
        lines.join("\n")
    }
}

impl ToolOutput for ClaudeAccountsOutput {
    fn log_output(&self) -> String {
        self.text()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.text(), Some(true)).to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        json!({ "accounts": self.accounts })
    }
}

pub struct ClaudeAccountsHandler;

impl ToolExecutor<ToolInvocation> for ClaudeAccountsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CLAUDE_ACCOUNTS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_claude_accounts_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: ClaudeAccountsArgs =
                parse_args(&invocation.payload, CLAUDE_ACCOUNTS_TOOL_NAME)?;
            let context = accounts_context(&invocation)?;
            let accounts = crate::claude_code::accounts::list_accounts(
                &context.account_dirs,
                Some(&context.state_path),
                args.include_usage.unwrap_or(true),
            )
            .await;
            Ok(boxed_tool_output(ClaudeAccountsOutput { accounts }))
        })
    }
}

impl CoreToolRuntime for ClaudeAccountsHandler {}

#[derive(Debug)]
struct ClaudeAccountSelectOutput {
    preferred_account: Option<String>,
}

impl ClaudeAccountSelectOutput {
    fn text(&self) -> String {
        match self.preferred_account.as_deref() {
            Some(account) => format!("New Claude work will use {account} first."),
            None => {
                "Cleared the preferred Claude account; selection is automatic again.".to_string()
            }
        }
    }
}

impl ToolOutput for ClaudeAccountSelectOutput {
    fn log_output(&self) -> String {
        self.text()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.text(), Some(true)).to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        json!({ "preferred_account": self.preferred_account })
    }
}

pub struct ClaudeAccountSelectHandler;

impl ToolExecutor<ToolInvocation> for ClaudeAccountSelectHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CLAUDE_ACCOUNT_SELECT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_claude_account_select_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{CLAUDE_ACCOUNT_SELECT_TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: ClaudeAccountSelectArgs = serde_json::from_str(arguments).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse {CLAUDE_ACCOUNT_SELECT_TOOL_NAME} arguments: {err}"
                ))
            })?;
            let context = accounts_context(&invocation)?;
            let alias =
                crate::claude_code::resolve_account_alias(&context.account_dirs, &args.account)
                    .map_err(FunctionCallError::RespondToModel)?;
            let preferred_account = match alias {
                AccountAlias::Auto => {
                    crate::claude_code::accounts::select_account(&context.state_path, None);
                    None
                }
                AccountAlias::Dir(dir) => {
                    crate::claude_code::accounts::select_account(&context.state_path, Some(&dir));
                    Some(crate::claude_code::accounts::account_label(Some(&dir)))
                }
            };
            Ok(boxed_tool_output(ClaudeAccountSelectOutput {
                preferred_account,
            }))
        })
    }
}

impl CoreToolRuntime for ClaudeAccountSelectHandler {}
