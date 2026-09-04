# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## `[chatgpt_web]` (fork)

Fork-only. Settings for the `chatgpt_web` model provider: ChatGPT on
chatgpt.com driven through a real Chrome tab via the chrome-mcp daemon, used as
a backend for agents (the `chatgpt-web/*` models are agent-only and never
appear in the `/model` picker). The provider uses the account logged into
Chrome and never the Codex login. See
[chatgpt_web_agents.md](./chatgpt_web_agents.md) for how it behaves.

```toml
[chatgpt_web]
tools = "none"               # none (default) | connector
idle_timeout_ms = 1200000    # no visible progress for this long stops the turn; 0 = forever
max_parallel_turns = 2       # ChatGPT turns this process runs at once
archive_on_shutdown = true   # archive the conversation when its thread shuts down
```

Every key is optional. Durations are milliseconds. The driver also honors
`CHROME_MCP_URL`, `CHROME_MCP_TOKEN` and `CHATGPT_URL` as overrides for
`daemon_url`, the token and `base_url`.

| key | default | meaning |
|---|---|---|
| `tools` | follows the tunnel setup | `none`: ChatGPT only sees the transcript Codex sends it. `connector`: Codex tools are exposed to ChatGPT through a custom MCP connector; requires the shared `codex chatgpt-web daemon`. Unset: `connector` once `tunnel_id` is configured (or `tunnel` is `cloudflared`/`manual`), else `none`. |
| `idle_timeout_ms` | `1200000` | Abandon a turn with no visible progress for this long and stop the generation. `0` waits forever. |
| `max_parallel_turns` | `2` | ChatGPT turns this process runs at the same time. |
| `max_tabs` | `3` | Pool of dedicated chatgpt.com tabs, clamped to 1..8. |
| `tab_idle_ms` | `300000` | Idle time after which a pooled tab is closed. |
| `daemon_url` | `"http://127.0.0.1:8848/mcp"` | Streamable HTTP endpoint of the chrome-mcp daemon. |
| `token_file` | `~/.chrome-mcp/token.txt` | File holding the chrome-mcp bearer token. |
| `base_url` | `"https://chatgpt.com"` | Base URL of the ChatGPT web app. |
| `poll_interval_ms` | `2500` | How often the conversation is polled while a reply streams. |
| `archive_on_shutdown` | `true` | Archive the ChatGPT conversation when its Codex thread shuts down. |
| `max_fork_turns` | `0` | Upper bound on parent turns a ChatGPT Web child may inherit through `fork_turns`. |
| `connector_name` | `"Codex Native"` | Name of the custom MCP connector registered in ChatGPT. |
| `connector_description` | `"Codex tools on this machine (exec, patch, images, harness tools)."` | Description shown for that connector (at most 200 characters). |
| `tunnel` | `"openai"` | How ChatGPT reaches the daemon: the official OpenAI Secure MCP Tunnel (`openai`), a cloudflared quick tunnel (`cloudflared`), or a public URL you provide (`manual`). |
| `tunnel_id` | unset | Tunnel id (`tunnel_<32 hex>`) from platform.openai.com → Settings → Organization → Tunnels. Required with `openai`; written by `codex chatgpt-web setup`. |
| `tunnel_key_file` | `$CODEX_HOME/chatgpt_web/tunnel.key` | File holding the restricted API key (`Tunnels: Read + Use`) the tunnel client authenticates with; `CODEX_CHATGPT_WEB_TUNNEL_KEY` in the environment also works. |
| `tunnel_client_path` | unset | Explicit `tunnel-client` binary. When unset the pinned release is downloaded into `$CODEX_HOME/chatgpt_web/bin/`, then `PATH` is tried. |
| `tunnel_client_version` | `"0.0.12"` | Pinned `tunnel-client` release version. |
| `cloudflared_path` | unset | Explicit `cloudflared` binary; auto-detected when unset. |
| `cloudflared_extra_args` | `[]` | Extra arguments for `cloudflared tunnel` (e.g. `["--protocol", "http2"]`). |
| `tunnel_port` | `0` | Loopback port of the connector MCP server (`0` = ephemeral). |
| `daemon_port` | `0` | Loopback port of the daemon control API (`0` = ephemeral). |
| `daemon_idle_shutdown_ms` | `0` | Shut the daemon down after this long with no sessions attached (`0` = never). |
| `connector_auto_approve_ui` | `true` | Click ChatGPT's "Allow" card for connector calls automatically. |
| `connector_auto_developer_mode` | `true` | Turn on ChatGPT Developer Mode automatically when a connector is needed. |
| `connector_call_timeout_ms` | `120000` | How long one connector call may take before the daemon answers with an error. |
| `connector_exec_default_yield_ms` | `10000` | Default `yield_time_ms` for `codex_exec` when ChatGPT omits it. |
| `connector_ready_timeout_ms` | `90000` | How long a turn waits for the connector to be registered and reachable. |
| `turn_ttl_ms` | `3600000` | Lifetime of a turn token in the daemon. |
| `connector_mention_strategy` | `"auto"` | How the connector is attached to the composer: `auto` (mention in the background, activate the tab only if the menu never mounts), `background_only`, or `activate`. |
| `manual_mcp_url` | unset | Public MCP URL of the daemon when `tunnel = "manual"`. |

## Plan mode (fork)

Fork-only additions to Plan mode.

### `$CODEX_HOME/plan_mode.md` — override the Plan-mode instructions

When this file exists and is not blank, its contents replace the built-in
Plan-mode developer instructions for every Plan-mode turn (TUI and app-server
clients alike). Delete the file to go back to the built-in template. The file is
read when the Plan mask is applied, so editing it takes effect on the next turn
— no rebuild and no restart.

Caveat: if the remote model catalog ever ships its own
`model_messages.collaboration_modes.plan`, that still takes precedence over both
the override and the built-in template. Codex logs a warning when that happens.

### Plan-mode reasoning effort

The built-in Plan preset no longer pins medium reasoning effort; Plan mode
inherits the thread's effort. Set `plan_mode_reasoning_effort` in `config.toml`
(or `/model` → "Apply to Plan mode override") only when you want Plan mode
pinned to a specific effort regardless of the session's.

### Saved plans and `/plans`

Every plan Codex emits in a `<proposed_plan>` block is written to
`$CODEX_HOME/plans/<timestamp>-<slug>.md` with YAML front matter (`title`,
`thread_id`, `turn_id`, `cwd`, `model`, `created_at`, `updated_at`, `revision`).
One file per thread: revising a plan in the same thread rewrites that file and
bumps `revision`, and an unchanged plan body is not rewritten at all.

`/plans` in the TUI lists the saved plans and offers three actions for the one
you pick: implement it in Default mode, attach it as hidden context to your next
message, or revise it in Plan mode. App-server clients read the same data
through the `plan/list` and `plan/read` methods.

Plans are kept indefinitely; delete files from `$CODEX_HOME/plans/` to remove
them.

## Plugin MCP server policy (fork)

`[plugins."<plugin>@<marketplace>".mcp_servers.<server>]` is upstream's place to
overrule a plugin's own MCP manifest. The fork adds three keys there.

| Key | Default | Meaning |
| --- | --- | --- |
| `root_only_tools` | unset | Tools only the root thread may see. They stay visible (and `Direct`) for the user's own thread and become `Hidden` for every spawned agent. `disabled_tools` is all-or-nothing, which is the wrong shape for a server like the Desktop's `codex_app`: the user genuinely wants `send_message_to_thread` from their own thread and genuinely does not want a subagent reaching for it. |
| `tool_approval_overrides` | unset | Per-tool approval decisions that outrank whatever the plugin manifest declared. Every other approval knob can only *tighten*; this one is the user's own config saying "I have decided about this tool", so it may also loosen. |
| `native_computer_surface` | unset (on) | Windows only. Set `false` to turn off the pass that adds the `computer` surface to the bundled `unified-computer-use` plugin's `cua_repl` server. |

### `native_computer_surface`

The Codex Desktop app writes that plugin's `.mcp.json` itself and, on Windows,
stamps `CUA_REPL_ENABLED_SURFACES = "browser"` in the very same file that
advertises a live Computer Use kernel (`SKY_CUA_NATIVE_PIPE = "1"` plus the
pipe directory the app owns). The plugin's `launch.mjs` only registers the `sky`
service when that list contains `computer`, so the direct `js` tool the model
reaches for has no `sky.*` at all and Computer Use reports itself as "not
configured" — even though the same kernel keeps working through the `node_repl`
code-mode path.

Codex therefore appends `computer` to that list **in memory** while loading the
plugin. Nothing on disk is touched: the app owns the file and rewrites it at
every startup, and the loader re-reads it on every load. The pass only fires
when all of these hold: the target is Windows, the server is named `cua_repl`,
its transport is stdio with an `env` table, `SKY_CUA_NATIVE_PIPE` is `"1"`,
`SKY_CUA_NATIVE_PIPE_DIRECTORY` is non-empty, and `CUA_REPL_ENABLED_SURFACES`
is present but does not already list `computer`. Existing entries are preserved
(`browser` becomes `browser,computer`).

To turn it off:

```toml
[plugins."unified-computer-use@openai-bundled".mcp_servers.cua_repl]
native_computer_surface = false
```
