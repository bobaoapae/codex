# Claude agents (fork)

> Fork-only. Upstream Codex has no `claude_code` provider; everything here lives
> behind `FORK:` markers in the source.

Codex can run agents backed by the locally installed **Claude Code CLI** instead
of an OpenAI model. A Claude agent runs its own tool loop against the real
workspace and reports back through the ordinary multi-agent channels, so the
parent keeps orchestration, transcripts and lifecycle.

## Setup

```toml
# ~/.codex/config.toml
[claude_code]
# One config dir per Claude account. Empty/unset = inherit CLAUDE_CONFIG_DIR
# from the environment (pre-fork behavior).
account_dirs = ['C:\Users\me\.claude-accounts\one', 'C:\Users\me\.claude-accounts\two']

# hybrid (default) | drain | config — see "Choosing an account".
selection = "hybrid"

# Headroom the thread's current account must keep for `hybrid` to stay on it.
sticky_min_headroom_pct = 20.0

# Abandon a turn whose CLI has produced nothing for this long, killing its whole
# process tree. 0 waits forever.
idle_timeout_ms = 600000

# Upper bound on parent turns a Claude child may inherit through `fork_turns`.
# 0 (the default) keeps every Claude child on task-only context, which is what a
# different harness can actually use. A full-history fork is never honored.
max_fork_turns = 0
```

The control protocol below is gated by `features.claude_code_control_protocol`
(on by default). Turning it off restores the pre-fork behavior: one line of
stdin, no approvals, no bridge, no usage reporting.

Agent roles select the provider. A role file in `~/.codex/agents/`:

```toml
name = "claude-opus"
description = "Deep review, architecture audits, hard bugs, second opinions."
model = "claude-opus-5"
model_provider = "claude_code"
model_reasoning_effort = "high"
developer_instructions = """
…
"""
```

`claude_code` is the **only** provider a role may select; every other provider
stays parent-owned, as upstream requires (`codex-rs/core/src/agent/role.rs`).
Naming a Claude model without a role also works — `spawn_agent(model =
"claude-opus-5")` pulls the child onto the provider that can serve it.

Set `CODEX_CLAUDE_CODE_BIN` if `claude` is not on `PATH`.

## What a Claude child gets

- **Task-only context.** `fork_turns` is capped by `[claude_code] max_fork_turns`
  (default 0): a Claude child never inherits the parent's Codex transcript, which
  would be full of instructions meant for a different harness. Send a
  self-contained brief. When an argument is adjusted rather than honored,
  `spawn_agent` says so in `notes` instead of failing the call.
- **`plaintext_message`, not `message`.** The encrypted form can only be read by
  the OpenAI backend; the spawn is rejected with a message saying so.
- **A system prompt, not a preamble.** The role's `developer_instructions`, the
  subagent protocol, and the workspace layout are delivered through the control
  protocol's `initialize.appendSystemPrompt`. The rendered transcript is filtered
  by content kind, so the harness scaffolding meant for Codex (AGENTS.md, plugin
  catalogs, token budgets, multi-agent mode text) never reaches the child.
- **Every workspace root** the Codex session can reach (`--add-dir`), with the
  writable ones named explicitly.
- **A small MCP surface, through the bridge.** `mcp__codex__send_message`,
  `wait_agent`, `list_agents`, `spawn_agent`, `followup_task`,
  `interrupt_agent`, `update_plan`, plus this session's own MCP servers. Claude's
  own `Bash`/`Edit`/`Read` are not duplicated: it already has them, and only its
  own run under the CLI's permission mode.
- **Approval mapping**, from the Codex sandbox *and* approval policy:

  | sandbox | approval | `--permission-mode` |
  |---|---|---|
  | read-only | any | `plan` (read-only tool set) |
  | anything writable | `never` | `bypassPermissions` |
  | anything writable | anything else | `auto` + `--permission-prompt-tool stdio` |

  In `auto` the CLI asks this session before each tool call and the ordinary
  Codex approval UI opens. `bypassPermissions` is never combined with the prompt
  tool: the CLI suppresses `can_use_tool` there.

  `acceptEdits` is deliberately absent. It auto-approves file edits and,
  headless, refuses everything else: tried against the real CLI, the agent's
  `Write` succeeded while a plain `cat` came back "This command requires
  approval". It also confines nothing — what the child may touch is decided by
  `--add-dir`.

## Choosing an account

Order of preference within one turn:

1. the account the agent was pinned to at spawn (`spawn_agent(account = …)`);
2. the account chosen with `claude_account_select`;
3. the account already serving this thread, while it has headroom;
4. the remaining accounts, per `selection`;
5. accounts with unknown usage, then spent ones, then accounts on cooldown —
   attempted last rather than skipped, so stale local state cannot dead-lock the
   provider.

`selection` policies:

| policy | behavior | when |
|---|---|---|
| `hybrid` (default) | keep the thread's account while it has more than `sticky_min_headroom_pct` left, then move to the account with the most headroom; among healthy accounts prefer the least busy | mixed interactive + fan-out work |
| `drain` | spend the account closest to its limit first | squeezing the most out of a quota before a reset |
| `config` | plain configured order, never fetches usage | deterministic setups |

Keeping an account matters beyond fairness: `--resume` only works inside the
config dir that created the session, so changing account replays the whole
conversation and loses the prompt cache.

Usage limits and auth failures fail over to the next account automatically and
record a short cooldown in `$CODEX_HOME/claude_code_accounts.json`. Re-logging in
lifts an auth cooldown immediately.

### Tools

- `claude_accounts` — per account: 5-hour and weekly usage, reset times,
  cooldowns, whether it is preferred, and how many turns are running on it right
  now. `include_usage: false` skips the network.
- `claude_account_select` — sets (or clears, with `auto`) the account new work
  tries first. Running agents keep theirs.
- `spawn_agent(account = …)` — pins one agent. Accepts an index from
  `claude_accounts`, a config-dir path, or part of the account email.

Both tools appear only when `account_dirs` is configured, and only for the root
agent — the one doing the delegating.

From a terminal, the same state is reachable without an agent turn:

```bash
codex account claude list            # accounts, 5h/7d usage, cooldowns, preference
codex account claude list --json     # same, machine-readable
codex account claude use 2           # prefer account 2 for new agents
codex account claude use auto        # back to automatic selection
```

During a Claude turn the account's 5-hour and weekly windows are reported to the
status line like any other rate limit, labeled with the account's email. A window
that has never been fetched is reported as unknown rather than as zero.

## Reporting and orchestration

A Claude child reports the way every other Codex subagent does:

- **Final answer**, or `send_message` with `target: ".."` for a mid-task update.
- Never through the Desktop's `codex_app` tools (`send_message_to_thread`,
  `create_thread`, `fork_thread`, `handoff_thread`, `automation_update`). Those
  render as "sent from another task" cards in the user's own thread and prompt
  for permission on every call; `create_thread`/`fork_thread`/`handoff_thread`
  belong to the user, not to an agent.
- The parent should check `list_agents` before `interrupt_agent`: a child with
  recent activity is working, not stuck. `wait_agent` is a long poll — call it
  once per round with a generous timeout instead of tight polling.

## Sessions and cost

Each Codex thread keeps one Claude session and extends it turn by turn, sending
only what the session has not seen. The mapping is persisted in
`$CODEX_HOME/claude_code_sessions.json`, so an agent that multi-agent v2 evicted
and rebuilt resumes its session instead of replaying its transcript.

A replay happens only when it must: first turn, a rewritten history (compaction,
fork, edited turn), an account change, or a session the CLI can no longer find.

Turn accounting comes from the CLI's own `usage` object. Each completed turn logs
the account, whether it resumed, and the cache hit/write token counts —
`claude_code turn completed` at `info` level.

## What the parent sees

Claude runs its own tool loop, and each call surfaces as the same transcript item
Codex emits for its own tools: an exec cell with the command and its output for
`Bash`, a per-file diff for `Edit`/`Write`/`MultiEdit`, an MCP call cell for
`mcp__*`, and a generic tool cell for the rest. File changes feed the turn diff.
These items are recorded under a namespace of their own, so they are never
dispatched (the work is already done) and never replayed back to Claude as if
they were new transcript.

`list_agents` reports each agent's role, model, Claude account, and what it was
last observed doing; `wait_agent` repeats that on a timeout, so a parent can tell
a child running `cargo test` from one that has genuinely stopped before reaching
for `interrupt_agent`.

## Limits

- Images and audio are dropped; the models declare text-only input.
- Codex's sandbox does not constrain the child beyond the permission mode; the
  approval surface does.
- Context window is 200k (190k effective) and compaction runs locally, through
  the CLI itself — Codex's own auto-compaction is skipped for these threads,
  because rewriting the history prefix would invalidate the session fingerprint
  and force a full replay.
- The control protocol is undocumented CLI surface. If the installed `claude`
  does not answer `initialize`, the adapter logs it and runs the pre-fork path:
  the turn still happens, without approvals, bridge, or usage.
