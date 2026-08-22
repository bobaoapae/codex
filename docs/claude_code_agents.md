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
```

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

- **Task-only context.** `fork_turns` is ignored for Claude children: they never
  inherit the parent's Codex transcript, which would be full of instructions
  meant for a different harness. Send a self-contained brief.
- **`plaintext_message`, not `message`.** The encrypted form can only be read by
  the OpenAI backend; the spawn is rejected with a message saying so.
- **Every workspace root** the Codex session can reach (`--add-dir`).
- **No Codex tools and no MCP servers.** Claude uses its own tools.
- **Approval mapping**: Codex `never` → `--permission-mode bypassPermissions`,
  everything else → `auto`.

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

## Limits

- Images and audio are dropped; the models declare text-only input.
- Claude's tool calls surface as reasoning text, not as Codex exec cells: there
  is no approval UI and no diff for what it did.
- Codex's sandbox does not constrain the child beyond the permission mode.
- Context window is 200k (190k effective, auto-compacted at 180k) and compaction
  runs locally, through the CLI itself.
