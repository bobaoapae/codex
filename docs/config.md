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
