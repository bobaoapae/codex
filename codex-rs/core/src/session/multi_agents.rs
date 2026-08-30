use crate::config::MultiAgentV2Config;
use crate::context::MultiAgentRoleInstructions;
use crate::session::turn_context::TurnContext;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::openai_models::MultiAgentRoleMessages;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

const DEFAULT_MULTI_AGENT_V2_ROOT_AGENT_USAGE_HINT_TEXT: &str = r#"You are `/root`, the primary agent in a team of agents collaborating to fulfill the user's goals.

At the start of your turn, you are the active agent.
You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents.
All agents in the team, including the agents that you can assign tasks to, are equally intelligent and capable, and have access to the same set of tools.

You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new task and trigger a turn, and `send_message` to pass a message to a running agent without triggering a turn.
Child agents can also spawn their own sub-agents.
You can decide how much context you want to propagate to your sub-agents with the `fork_turns` parameter.

You will receive messages in the analysis channel in the form:
```
Message Type: MESSAGE | FINAL_ANSWER
Task name: <recipient>
Sender: <author>
Payload:
<payload text>
```
They may be addressed as to=/root
"#;
const DEFAULT_MULTI_AGENT_V2_SUBAGENT_USAGE_HINT_TEXT: &str = r#"You are an agent in a team of agents collaborating to complete a task.

You can spawn sub-agents to handle subtasks, and those sub-agents can spawn their own sub-agents. All agents in the team, including the agents that you can assign tasks to, are equally intelligent and capable, and have access to the same set of tools.

You can use `spawn_agent` to create a new agent, `followup_task` to give an existing agent a new task and trigger a turn, and `send_message` to pass a message to a running agent.
Child agents can also spawn their own sub-agents.

When you provide a response in the final channel, that content is immediately delivered back to your parent agent.

You will receive messages in the analysis channel in the form:
```
Message Type: NEW_TASK | MESSAGE | FINAL_ANSWER
Task name: <recipient>
Sender: <author>
Payload:
<payload text>
```
You may also see them addressed as to=/root/..., which indicates your identity is /root/...
"#;
const DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT: &str = "Full-history forks (`fork_turns` omitted or `\"all\"`) inherit the parent model and reasoning effort and do not accept overrides. Only set `model` or `reasoning_effort` when explicitly requested by the user, applicable `AGENTS.md` instructions, or skill instructions; when doing so, set `fork_turns` to `\"none\"` or a positive integer string.";
const DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT: &str =
    "When calling `wait_agent`, prefer longer waits (minutes) to avoid busy polling.";
const DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT: &str = r#"Note that collaboration tools cannot be called from inside `functions.exec`. Call `spawn_agent`, `send_message`, `followup_task`, `wait_agent`, `interrupt_agent`, and `list_agents` only as direct tool calls using the recipient shown in their tool definitions, such as `to=functions.collaboration.spawn_agent`, since they are intentionally absent from the `functions.exec` `tools.*` namespace. Available tools in `functions.exec` are explicitly described with a `tools` namespace in the developer message.

All agents share the same directory. In detail:
- All agents have access to the same container and filesystem as you.
- All agents use the same current working directory.
- As a result, edits made by one agent are immediately visible to all other agents.
"#;

#[derive(Clone, Debug, Default)]
pub(crate) struct ResolvedMultiAgentV2UsageHints {
    pub(crate) root: Option<MultiAgentRoleInstructions>,
    pub(crate) subagent: Option<MultiAgentRoleInstructions>,
}

pub(super) fn usage_hint_text(
    turn_context: &TurnContext,
    session_source: &SessionSource,
) -> Option<MultiAgentRoleInstructions> {
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let catalog = turn_context
        .model_info()
        .model_messages
        .as_ref()
        .and_then(|messages| messages.multi_agent.as_ref())
        .and_then(|messages| messages.role.as_ref());
    let snapshot = resolve_usage_hints(&turn_context.config.multi_agent_v2, catalog);
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => snapshot.subagent,
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => snapshot.root,
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

/// FORK: how agents report, for both roles.
///
/// Measured problem: subagents were reaching for the Desktop's
/// `send_message_to_thread` to report progress, which lands in the *user's* own
/// thread as a "sent from another task" card and prompts for permission on every
/// call. The inter-agent channel already exists; it just was not named here.
const FORK_MULTI_AGENT_V2_REPORTING_HINT_TEXT: &str = r#"## Reporting

Report through the agent channel only: your final answer for the turn, or `send_message` with `target: ".."` when you must say something before you are done.

Never use the Desktop thread tools (`send_message_to_thread`, `create_thread`, `fork_thread`, `handoff_thread`, `automation_update`) to report. They post into the user's own thread and ask for permission on every call. Creating and forking threads is the user's business, not yours.

## Git state

The working tree is shared with the other agents and is dirty by design. Never run `git reset`, `checkout`, `clean`, `stash`, or `commit` unless the user asked for it by name: those discard uncommitted work that is not yours and cannot be recovered."#;

/// FORK: how the root should wait, for the root only.
///
/// Measured problem: 28,986 `wait`/`wait_agent` calls in 30 days, 13,942 of them
/// timing out, and 593 `interrupt_agent` calls — 70% of them aimed at children
/// that were still working. Impatience, not deadlock.
const FORK_MULTI_AGENT_V2_PATIENCE_HINT_TEXT: &str = r#"## Waiting on agents

Call `wait_agent` once per round with a generous timeout and treat a timeout as "still working", not as "stuck". Tight polling burns your context and tells you nothing new.

Before `interrupt_agent`, check `list_agents`: an agent with recent activity is working. Interrupting loses everything it has not yet reported."#;

/// FORK: what "done" means, for the root only.
///
/// Measured problem: the same corrections pasted into 47 prompts in 30 days —
/// stop inventing audit/certification/fingerprint gates, follow the plan, do not
/// re-ask what the plan already answers.
const FORK_MULTI_AGENT_V2_DELIVERY_HINT_TEXT: &str = r#"## Delivery discipline

The approved plan is the contract. Execute its tasks in order; do not insert audit, certification, re-baseline, or publication-gate tasks it does not contain. If verification is genuinely missing, propose one bounded item instead of starting it.

Validation per task is the existing focused tests plus at most one broad gate, using the repository's own runner. Do not build a new harness, a result parser, or a repetition matrix for routine work. Hash or fingerprint only for release/signature/provenance or an explicit acceptance criterion.

Task IDs stay at most two levels deep. If a third level seems necessary, the task is mis-scoped: re-plan instead.

Do not ask what the plan, the request, or the code already answers. Ask only when two readings would lead to materially different work."#;

pub(crate) fn resolve_usage_hints(
    config: &MultiAgentV2Config,
    catalog: Option<&MultiAgentRoleMessages>,
) -> ResolvedMultiAgentV2UsageHints {
    let resolve_role = |configured: Option<&str>, catalog: Option<&str>, bundled: &str| {
        // Configured roles take precedence; empty configured or catalog roles suppress fallback.
        if let Some(configured) = configured {
            return (!configured.is_empty())
                .then(|| MultiAgentRoleInstructions::unmarked(configured));
        }

        let base = catalog.unwrap_or(bundled);
        if base.is_empty() {
            return None;
        }

        let max_concurrency = config.max_concurrent_threads_per_session;
        let wait_agent_guidance = if config.wait_agent_enabled {
            format!("{DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT}\n\n")
        } else {
            String::new()
        };
        let mut text = format!(
            "{base}\n{DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT}\n{wait_agent_guidance}There are {max_concurrency} available concurrency slots, meaning that up to {max_concurrency} agents can be active at once, including you."
        );
        if config.expose_spawn_agent_model_overrides {
            text.push_str("\n\n");
            text.push_str(DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT);
        }

        Some(if catalog.is_some() {
            MultiAgentRoleInstructions::catalog(text)
        } else {
            MultiAgentRoleInstructions::unmarked(text)
        })
    };

    // FORK: these sections are appended after whatever `resolve_role` produced,
    // including a fully configured hint text. Configuring a hint replaces the
    // bundled one, and losing this guidance with it is exactly the failure the
    // sections describe.
    let mut root_suffix = vec![FORK_MULTI_AGENT_V2_REPORTING_HINT_TEXT.to_string()];
    root_suffix.push(FORK_MULTI_AGENT_V2_PATIENCE_HINT_TEXT.to_string());
    if config.delivery_discipline_hint {
        root_suffix.push(FORK_MULTI_AGENT_V2_DELIVERY_HINT_TEXT.to_string());
    }
    root_suffix.extend(config.root_agent_usage_hint_suffix.clone());

    let mut subagent_suffix = vec![FORK_MULTI_AGENT_V2_REPORTING_HINT_TEXT.to_string()];
    subagent_suffix.extend(config.subagent_usage_hint_suffix.clone());

    ResolvedMultiAgentV2UsageHints {
        root: append_sections(
            resolve_role(
                config.root_agent_usage_hint_text.as_deref(),
                catalog.and_then(|messages| messages.root.as_deref()),
                DEFAULT_MULTI_AGENT_V2_ROOT_AGENT_USAGE_HINT_TEXT,
            ),
            &root_suffix,
        ),
        subagent: append_sections(
            resolve_role(
                config.subagent_usage_hint_text.as_deref(),
                catalog.and_then(|messages| messages.subagent.as_deref()),
                DEFAULT_MULTI_AGENT_V2_SUBAGENT_USAGE_HINT_TEXT,
            ),
            &subagent_suffix,
        ),
    }
}

/// FORK: where the fork-owned sections begin inside a resolved usage hint.
///
/// A forked child strips its parent's stale hints by comparing text exactly, so
/// a hint recorded by a build whose sections differ would otherwise survive the
/// fork and reach the child as someone else's instructions. Splitting here lets
/// that comparison also try the text without them.
const FORK_HINT_SECTIONS_MARKER: &str = "\n\n## Reporting\n";

/// FORK: the portion of a usage hint before the fork-owned sections.
pub(crate) fn without_fork_hint_sections(hint: &str) -> &str {
    match hint.find(FORK_HINT_SECTIONS_MARKER) {
        Some(index) => &hint[..index],
        None => hint,
    }
}

/// FORK: appends fork-owned sections to a resolved role hint.
///
/// An explicitly emptied hint stays empty: that is the user saying "say nothing
/// here", and this is not the place to overrule it.
fn append_sections(
    hint: Option<MultiAgentRoleInstructions>,
    sections: &[String],
) -> Option<MultiAgentRoleInstructions> {
    let sections: Vec<&str> = sections
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .collect();
    if sections.is_empty() {
        return hint;
    }
    let hint = hint?;
    let combined = format!("{}\n\n{}", hint.text().trim_end(), sections.join("\n\n"));
    Some(hint.with_text(combined))
}

pub(crate) fn effective_multi_agent_mode(turn_context: &TurnContext) -> Option<MultiAgentMode> {
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let catalog_mode = turn_context
        .model_info()
        .model_messages
        .as_ref()
        .and_then(|messages| messages.multi_agent.as_ref())
        .and_then(|messages| messages.mode.as_ref());
    let mode_hint_text = turn_context
        .config
        .multi_agent_v2
        .multi_agent_mode_hint_text
        .as_deref()
        .or_else(|| catalog_mode.and_then(|mode| mode.hint_text.as_deref()));

    // A configured or catalog hint, including an empty string, defines a custom policy instead
    // of an effort-derived built-in policy.
    let multi_agent_mode = match mode_hint_text {
        Some(hint_text) => MultiAgentMode::Custom(hint_text.to_string()),
        None => match turn_context.effective_reasoning_effort() {
            Some(ReasoningEffort::Ultra) => catalog_mode
                .and_then(|messages| messages.proactive.clone())
                .map(MultiAgentMode::Custom)
                .unwrap_or(MultiAgentMode::Proactive),
            _ => catalog_mode
                .and_then(|messages| messages.explicit.clone())
                .map(MultiAgentMode::Custom)
                .unwrap_or(MultiAgentMode::ExplicitRequestOnly),
        },
    };

    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(multi_agent_mode),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

#[cfg(test)]
mod fork_hint_tests {
    use super::*;
    use crate::config::MultiAgentV2Config;

    fn config() -> MultiAgentV2Config {
        MultiAgentV2Config::default()
    }

    fn body(hint: Option<MultiAgentRoleInstructions>) -> String {
        use crate::context::ContextualUserFragment;
        hint.expect("a usage hint should resolve").body()
    }

    /// FORK: the `*_usage_hint_text` keys *replace* the bundled text, so a user
    /// who configures one silently loses everything the harness needed to say.
    /// The fork's own sections are appended instead.
    #[test]
    fn fork_reporting_hint_survives_a_configured_usage_hint_text() {
        let mut config = config();
        config.root_agent_usage_hint_text = Some("Only this.".to_string());
        config.subagent_usage_hint_text = Some("And this.".to_string());

        let hints = resolve_usage_hints(&config, /*catalog*/ None);
        let root = body(hints.root);
        let subagent = body(hints.subagent);

        assert!(root.starts_with("Only this."), "{root}");
        assert!(root.contains("send_message_to_thread"), "{root}");
        assert!(subagent.starts_with("And this."), "{subagent}");
        assert!(subagent.contains("send_message_to_thread"), "{subagent}");
    }

    /// Waiting and delivery discipline are the orchestrator's problem; a worker
    /// reading them would only spend context on advice it cannot act on.
    #[test]
    fn patience_hint_is_root_only() {
        let hints = resolve_usage_hints(&config(), /*catalog*/ None);
        let root = body(hints.root);
        let subagent = body(hints.subagent);

        assert!(root.contains("## Waiting on agents"), "{root}");
        assert!(root.contains("## Delivery discipline"), "{root}");
        assert!(!subagent.contains("## Waiting on agents"), "{subagent}");
        assert!(!subagent.contains("## Delivery discipline"), "{subagent}");
    }

    /// The delivery hint is the one piece of this a user may not want.
    #[test]
    fn delivery_discipline_hint_can_be_turned_off() {
        let mut config = config();
        config.delivery_discipline_hint = false;

        let root = body(resolve_usage_hints(&config, /*catalog*/ None).root);

        assert!(!root.contains("## Delivery discipline"), "{root}");
        // Turning it off must not take the rest with it.
        assert!(root.contains("## Reporting"), "{root}");
        assert!(root.contains("## Waiting on agents"), "{root}");
    }

    /// A configured suffix is appended after the fork's own sections.
    #[test]
    fn a_configured_suffix_is_appended_last() {
        let mut config = config();
        config.root_agent_usage_hint_suffix = Some("House rule.".to_string());
        config.subagent_usage_hint_suffix = Some("Worker rule.".to_string());

        let hints = resolve_usage_hints(&config, /*catalog*/ None);
        assert!(body(hints.root).ends_with("House rule."));
        assert!(body(hints.subagent).ends_with("Worker rule."));
    }

    /// An explicitly emptied hint means "say nothing here"; that is the user's
    /// call, not something to overrule.
    #[test]
    fn an_emptied_hint_stays_empty() {
        let mut config = config();
        config.root_agent_usage_hint_text = Some(String::new());

        assert!(
            resolve_usage_hints(&config, /*catalog*/ None)
                .root
                .is_none()
        );
    }

    /// The fork sections are recognizable, so a forked child can strip a hint
    /// recorded by a build whose sections differ.
    #[test]
    fn fork_sections_can_be_split_back_off() {
        let mut config = config();
        config.root_agent_usage_hint_text = Some("Only this.".to_string());

        let root = body(resolve_usage_hints(&config, /*catalog*/ None).root);

        assert_eq!(without_fork_hint_sections(&root), "Only this.");
        // A hint that never carried them is returned unchanged.
        assert_eq!(without_fork_hint_sections("plain"), "plain");
    }
}
