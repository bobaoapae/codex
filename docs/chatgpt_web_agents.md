# ChatGPT Web agents (fork)

> Fork-only. Upstream Codex has no `chatgpt_web` provider; everything here lives
> behind `FORK:` markers in the source.

Codex can run agents backed by **ChatGPT on chatgpt.com**, driven through a real
Chrome tab. The provider talks to the chrome-mcp daemon, which drives the
browser through its extension; ChatGPT answers inside the web app the way it
answers a person, and Codex reads the reply back through ChatGPT's own
conversation API. The models are agent-only: every `chatgpt-web/*` line is
`visibility: hide`, so it never appears in the `/model` picker and is reached
through a role or `spawn_agent`.

By default (`tools = "none"`) the ChatGPT side sees only the transcript Codex
sends it and answers from that. With `tools = "connector"` ChatGPT calls Codex
tools natively through a custom MCP connector, and Codex runs them under its
own sandbox and approval policy. The browser never touches the filesystem.

## Prerequisites

- **Chrome with the chrome-mcp extension, logged into chatgpt.com.** The account
  logged into that browser is the account the provider uses. It is independent
  of the Codex login: `codex account …` and `auth.json` are never consulted, the
  same way the `claude_code` provider ignores them. One web account per machine;
  the chrome-mcp daemon accepts a single extension at a time, so multi-account
  is out of scope.
- **The chrome-mcp daemon** at `http://127.0.0.1:8848` (`/mcp` is the Streamable
  HTTP endpoint, `/healthz` the probe) with its bearer token in
  `~/.chrome-mcp/token.txt`. A turn is refused until `/healthz` reports the
  extension connected. `CHROME_MCP_URL` and `CHROME_MCP_TOKEN` override the
  location and the token; `CHATGPT_URL` overrides `https://chatgpt.com`.
- A plan that offers the modes you select: `chatgpt-web/pro` needs ChatGPT Pro.
- For `tools = "connector"`: the shared daemon and a tunnel, see
  [Connector mode](#tools--connector).

The Node `chatgpt-pro-mcp` server can keep running next to Codex: both share
the tab registry in `~/.chatgpt-pro-mcp/tabs.json` (same file format) and do
not steal each other's tabs.

## Setup

```toml
# ~/.codex/config.toml
[chatgpt_web]
# none | connector — see the two sections below. When unset, the mode follows
# the tunnel setup: `connector` once `codex chatgpt-web setup` wrote a
# `tunnel_id` (or `tunnel` is cloudflared/manual), `none` otherwise.
# tools = "connector"

# Abandon a turn that has shown no visible progress for this long, stopping the
# generation. 0 waits forever. ChatGPT Pro thinks for a long time, but its
# status indicators keep moving; 20 minutes of real silence means it is stuck.
idle_timeout_ms = 1200000

# ChatGPT turns this Codex process runs at the same time. Keep it small: three
# concurrent turns already trip "too many requests" on one account.
max_parallel_turns = 2

# Dedicated chatgpt.com tabs kept open (clamped to 1..8), and how long an idle
# one stays open.
max_tabs = 3
tab_idle_ms = 300000

# Archive the ChatGPT conversation when its Codex thread shuts down.
archive_on_shutdown = true

# Upper bound on parent turns a ChatGPT Web child may inherit through
# `fork_turns`. 0 (the default) keeps every child on task-only context.
max_fork_turns = 0
```

Every key is optional; the complete list is in the
[key reference](#chatgpt_web-key-reference).

Agent roles select the provider. A role file in `~/.codex/agents/` — this is
the one the end-to-end runs used:

```toml
name = "chatgpt-pro"
description = "Agente ChatGPT Pro (web), servido pelo chatgpt.com num separador do Chrome. Para analise profunda, pesquisa na web, revisao e criacao de texto/codigo a partir do contexto que lhe e enviado; nao acede ao computador local."
model = "chatgpt-web/thinking"
model_provider = "chatgpt_web"
developer_instructions = """
Voce e um agente ChatGPT rodando dentro de uma sessao Codex, sob a direcao do agente principal.

- Voce NAO tem acesso ao computador local: nao pode ler, executar nem editar ficheiros. Tudo o que sabe do workspace esta na transcricao que recebeu. Se precisar de algo que nao esta la, diga exatamente o que falta em vez de inventar.
- Use as capacidades nativas do ChatGPT (pesquisa na web, navegacao) sempre que ajudarem.
- Produza a resposta completa no proprio turno: analise, patch sugerido (em diff unificado quando aplicavel), plano de testes. Nao pare num plano.
- Termine com um resumo conciso e acionavel, com incertezas explicitas.
"""
```

`chatgpt_web` and `claude_code` are the only providers a role may select; every
other provider stays parent-owned, as upstream requires
(`codex-rs/core/src/agent/role.rs`). Naming a ChatGPT Web model without a role
also works — `spawn_agent(model = "chatgpt-web/pro")` pulls the child onto the
provider that can serve it.

## Model lines

Five lines, each pinned to exactly one ChatGPT mode. The effort *is* the line:
each one advertises a single reasoning level, so `model_reasoning_effort` has
nothing else to choose.

| slug | effort | ChatGPT mode | context / auto-compact | pick it for |
|---|---|---|---|---|
| `chatgpt-web/instant` | `low` | Instant (`?model=<base>-instant`) | 41 000 / 32 000 | quick lookups, short rewrites, cheap fan-out; the only line whose connector write tools are verified |
| `chatgpt-web/thinking` | `medium` | Thinking at its default effort | 90 000 / 80 000 | the everyday choice for analysis, review and drafting |
| `chatgpt-web/high` | `high` | Thinking with the effort picker set to *High* | 90 000 / 80 000 | harder reasoning without Pro latency |
| `chatgpt-web/extra-high` | `xhigh` | Thinking with the effort picker set to *Extra high* | 90 000 / 80 000 | the hardest problems short of Pro |
| `chatgpt-web/pro` | `max` | Pro (`?model=<base>-pro`) | 111 193 / 95 000 | audits, architecture reviews, second opinions; the slowest replies |

- `<base>` is the account's default model slug with any `-instant`/`-thinking`/
  `-pro` suffix stripped (`gpt-5-6-pro` in the recorded run).
- `high` and `extra-high` pick the level through ChatGPT's effort picker,
  which only mounts while the tab is visible: the driver activates the tab,
  selects the level, reloads and restores focus (the selection persists across
  the reload; a `?model=` URL resets it, which is why every new chat navigates
  first and selects second). The current picker is a slider (`Instantâneo`,
  `Médio`, `Alto`, `Extra alto`, `Pro` — 5 positions) driven with
  ArrowLeft/ArrowRight on its focused item; the older submenu (`Esforço` /
  `Reasoning`) remains as the fallback. The composer label is verified after
  the reload and a mismatch is reported as a note on the turn.
- Context figures are measured, not declared by ChatGPT. There is no tokenizer
  for these models, so usage is estimated as `chars / 4` over the **entire**
  rendered history plus an 8 192-token reserve. The meter therefore grows with
  the conversation and auto-compaction fires at the listed limit.
- Every line accepts text and images.

## Using it

From a Codex session, through a role:

```
spawn_agent(role = "chatgpt-pro", plaintext_message = "…a self-contained brief…")
```

- **Task-only context.** `fork_turns` is capped by `[chatgpt_web] max_fork_turns`
  (default 0): the child never inherits the parent's transcript, which is full
  of instructions meant for a different harness. When an argument is adjusted
  rather than honored, `spawn_agent` says so in `notes` instead of failing.
- **`plaintext_message`, not `message`.** The encrypted form can only be read by
  the OpenAI backend; the spawn is rejected with a message saying so.
- The child reports like every other subagent: a final answer, or
  `send_message` with `target: ".."` for a mid-task update. Verified: a
  `gpt-5.6` parent spawned the `chatgpt-pro` role, the child answered in its
  own ChatGPT conversation, and the parent relayed the reply.

From a terminal:

```bash
# one turn, new conversation
codex exec -c model_provider=chatgpt_web -m chatgpt-web/instant "Reply with the single word PONG."

# images travel as ChatGPT attachments
codex exec -c model_provider=chatgpt_web -m chatgpt-web/thinking -i diagram.png "What does this show?"

# continue the same Codex thread (and, if still open, the same ChatGPT conversation)
codex exec resume --last "Now turn that into a checklist."
```

- `codex exec` reads the prompt from stdin when stdin is not a TTY; in scripts
  add `< /dev/null`.
- Every `codex exec` archives its conversation on exit (see
  [Conversations](#conversations)), so `exec resume` replays the transcript
  into a fresh conversation. Pass `-c chatgpt_web.archive_on_shutdown=false`
  when you want the second run to extend the first.
- Subagents are not resumable through `exec resume`.

## Conversations

Each Codex thread keeps **one persistent ChatGPT conversation** and extends it
turn by turn, sending only what the conversation has not seen: the new items
joined together, or `(no new input; continue from the previous turn)` when
there are none. After an interrupted or stalled turn the extension starts with
`(the previous request was interrupted; continue from it)`. The mapping is
persisted in `$CODEX_HOME/chatgpt_web_sessions.json` (7-day TTL, 512 entries),
so an agent that multi-agent v2 evicted and rebuilt resumes its conversation
instead of replaying.

A **replay** — a fresh conversation that receives the whole transcript — happens
only when it must: first turn, a rewritten history (compaction, fork, edited
turn), a different model line, or a conversation ChatGPT no longer returns. The
replay message is a fixed header (transport contract, priority order, literal
roles), the `Environment:` block (working directory, readable and writable
roots), `<developer_instructions>`, the transcript inside `<codex_transcript>`
with tool calls and results rendered as tagged blocks, and a
`<codex_transport_resume>` tail asking for the latest active request.

**Compaction.** Codex's own auto-compaction runs for these threads (unlike
`claude_code`, which compacts inside the CLI). The provider recognizes the
summarization turn and runs it in a fresh, disposable conversation with a
checkpoint contract instead of the normal one, then archives that conversation.
The thread's own conversation is left untouched; the next turn replays anyway,
because the history prefix changed.

**Archiving.** With `archive_on_shutdown = true` (default) the conversation is
archived (`is_archived: true`, 10 s budget) and forgotten from the sessions
file when the root thread shuts down — for the root and every live agent in its
tree — and when an agent is closed with `close_agent`, for that agent and its
descendants. Eviction never archives: the agent is rebuilt later and resumes.

**Tabs.** The driver keeps a pool of dedicated chatgpt.com tabs (`max_tabs`,
closed after `tab_idle_ms` idle) with an affinity between a conversation and the
tab that last served it. Tabs work hidden; the effort menu, and the connector's
mention fallback, briefly activate a tab and restore focus afterwards.

**Interrupting** a turn clicks ChatGPT's Stop button (best effort, up to 10 s)
and records the message as landed-but-unanswered, so the next turn continues
from it instead of resending it.

## What the parent sees

- In `tools = "none"` mode, a commentary line at the start of every turn:
  `⚠️ ChatGPT Web <Level> cannot access the local computer in this turn. It
  sees the accumulated Codex context (including earlier tool results) but
  cannot read or modify local files. ChatGPT-native capabilities such as web
  search remain available.` It is authored on the Codex side and never sent to
  ChatGPT.
- Thinking and Pro thoughts arrive as reasoning summaries; the reply as the
  final message. Progress is watched in the page every `poll_interval_ms`
  (2.5 s, no backend request); the text itself is refreshed from the
  conversation API at most every 30 s while it streams and once more when the
  page shows the reply finished — there is no token stream from ChatGPT, and a
  rewrite (regenerate, retry) replaces the item rather than appending.
- ChatGPT-side tool activity that produces assets (web search, image
  generation) shows as a short note.
- The turn ends when ChatGPT marks the latest text message `end_turn` and
  finished. As a fallback, a reply that has not changed across eight
  consecutive polls (about 20 s) is taken as complete, so a missing `end_turn`
  cannot hold a turn for the full idle timeout.

## `tools = "none"`

The default. ChatGPT gets the transcript and a contract that states it: this
chat has no bridge to the user's computer, earlier tool results are
authoritative snapshots, and it must never claim a fresh inspection, command,
edit or verification that is not in the transcript — if the request needs
local access it must say so instead of inventing success. It is told to use
ChatGPT-native capabilities (web search, browsing) whenever they help, and Pro
is additionally asked to finish in a single response rather than delegate.

Use this mode for what the web app is good at with a large pasted context:
review, design critique, research with web search, drafting. Anything that
must touch the workspace is the parent's job — or the connector's.

## `tools = "connector"`

> **Status.** Connector mode works end to end over both tunnels, verified
> live with the config as written by `codex chatgpt-web setup` (no other
> `[chatgpt_web]` keys): `tunnel = "openai"` with the pinned `tunnel-client`
> v0.0.12, Developer Mode enabled automatically, the "Codex Native" connector
> created with `tunnel_id`, and `codex_apply_patch` + `codex_exec` driven on
> `instant`, `thinking` and `pro`, a follow-up turn issued as a **separate**
> `codex exec resume --last` process (same conversation, tool executed), and a
> `spawn_agent` child on the `chatgpt-pro` role creating a file through the
> connector. `tunnel = "cloudflared"` was verified earlier (same smoke plus
> tunnel reconnection). Backend reads are now driven by the page (see
> *Limits*): a short turn costs about three backend calls.
>
> Two things to know: OpenAI's own connector safety layer sometimes refuses a
> call ("Esta ferramenta foi bloqueada pelas configurações de segurança da
> OpenAI") — the call never reaches Codex, the model reports it and the turn
> ends normally; rerunning usually passes. And a resumed session keeps the
> model it was started with, so pass `-m chatgpt-web/…` on `resume` too.

In connector mode ChatGPT calls Codex tools as real function calls. Codex
exposes a custom MCP connector (Developer Mode) named `connector_name`
(`Codex Native`), ChatGPT calls it, and every call is forwarded to the Codex
session that owns the turn, which runs it through its normal tool router under
its sandbox and approval policy. The daemon and the browser never see the
filesystem.

### The shared daemon

One `codex chatgpt-web daemon` per machine serves every Codex session and agent.
It starts on its own the first time a session reaches a connector turn (a
15 s wait; a losing racer exits on the lock), is single-instance, and a session
built from a newer Codex asks an older daemon to shut down when idle and spawns
its replacement. It owns:

- the loopback **control API** sessions talk to (bearer token in
  `$CODEX_HOME/chatgpt_web/daemon.token`);
- the **turn broker**: each Codex turn registers a `turn_token` (lifetime
  `turn_ttl_ms`), the tools it announced, and which exec/patch tools it uses;
  incoming calls are matched to the owning session, batched over 15 ms and
  delivered by long-poll; results flow back the same way;
- the loopback **MCP server** ChatGPT reaches through the tunnel, at a secret
  path regenerated on every start;
- the **tunnel supervisor** and the **connector registry** (below).

State lives in `$CODEX_HOME/chatgpt_web/`: `daemon.lock`, `daemon.json`
(pid, ports, public host — never the secret path), `daemon.token`,
`connector.json`, `tunnel.key`, `bin/` (downloaded tunnel client) and
`daemon.log` (rotated at 5 MB; secrets appear only as hashes).

### The tool contract

ChatGPT caches a connector's tool set by connector identity, and one connector
serves every session, so the contract is fixed: six tools, identical for
everyone, each taking the `turn_token` from the prompt and resolved by the
daemon against the tools that particular turn announced.

| tool | arguments | runs as |
|---|---|---|
| `codex_exec` | `cmd`, `workdir?`, `yield_time_ms?` (250..30000, default `connector_exec_default_yield_ms`), `max_output_tokens?`, `tty?` | `exec_command` (or legacy `shell` when that is what the turn announces) |
| `codex_write_stdin` | `session_id`, `chars?` (empty just polls), `yield_time_ms?`, `max_output_tokens?` | `write_stdin` |
| `codex_apply_patch` | `patch` (a full `*** Begin Patch … *** End Patch` envelope) | `apply_patch` — every line declares the freeform patch tool |
| `codex_view_image` | `path`, `detail?` | `view_image` |
| `codex_tool_inventory` | `query?`, `offset?`, `limit?` (≤ 50), `include_schema?` | answered by the daemon from the turn's tool snapshot |
| `codex_tool_call` | `name`, `namespace?`, `arguments?` or `input?` | any tool the turn announced, MCP tools included (with their namespace) |

Changing the contract bumps its version and registers the connector under a
new name, so ChatGPT's cached copy is left behind.

### A connector turn

1. The session makes sure the daemon is up and the registry is `verified`
   (waiting up to `connector_ready_timeout_ms`), mints a `turn_token` and
   registers the turn with its tool list.
2. The prompt carries the token and tells ChatGPT to pass it unchanged on every
   `Codex Native` call in the response, including continuations after tool
   results, and never to expose it in the answer. The read-only warning is not
   emitted in this mode.
3. The connector is attached in the composer by `@`-mentioning its name
   (`connector_mention_strategy`). Once attached and approved, the selection and
   the approval persist for the rest of that conversation; re-mentioning is
   harmless.
4. ChatGPT's consent card ("Allow ChatGPT to use Codex Native?") is clicked
   automatically when `connector_auto_approve_ui = true`, preferring the
   *Always allow* option, in the hidden tab.
5. Each call the daemon delivers becomes a `function_call` in the Codex turn.
   Codex runs it — sandbox, approval prompts, the usual transcript cell — and
   the output goes back to ChatGPT as the tool result. Calls that miss their
   deadline (`connector_call_timeout_ms`) answer with `Codex did not finish …
   within Ns. For long commands use yield_time_ms ≤ 30000 and poll the session
   with codex_write_stdin.`
6. The turn completes when ChatGPT ends its response with every tool request
   answered. Cancelling a turn revokes its token and clicks Stop.

### Tunnels

The MCP server only listens on loopback; the tunnel decides how ChatGPT reaches
it (`tunnel`):

- **`openai` (default)** — the official OpenAI Secure MCP Tunnel
  (`tunnel-client`). Outbound connections only, no public URL, a stable
  `tunnel_id`, so the connector is created once and reused. Needs a one-time
  setup:

  1. On `platform.openai.com`, **logged in as the same account that is logged
     into chatgpt.com**, create a Tunnel under *Settings → Organization →
     Tunnels* (name and description are both required) and a restricted API
     key under *API keys*: owner *You*, any project, permissions *Restricted*
     with `Tunnels` set to both `Read` and `Use` (the row then reads "All
     selected"), everything else `None`. Creating the key is free and does not
     consume model credits; the value is shown once — save it to a file.
  2. **Share the tunnel with the ChatGPT account.** ChatGPT only lists tunnels
     that name it under the tunnel's *ChatGPT workspaces* field; a fresh
     tunnel lists nothing (`GET /backend-api/aip/connectors/mcp/tunnels` →
     `{"tunnels":[]}`) and the registry refuses with "tunnel … is not visible
     to the ChatGPT account". Edit the tunnel, and in *ChatGPT workspaces*
     search for the account's id — for a personal (non-workspace) account that
     is the `account_id` returned by `https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27`
     (a UUID) — pick it and save. The tunnel shows up in ChatGPT immediately.
  3. Run:

     ```bash
     codex chatgpt-web setup --tunnel-id tunnel_<32 hex> --api-key-file /path/to/key.txt   # or `-` for stdin
     ```

     This stores the key in `$CODEX_HOME/chatgpt_web/tunnel.key` (0600),
     writes `tunnel_id` and `tunnel = "openai"` into `config.toml`, restarts a
     running daemon and waits up to 150 s for the tunnel to come up, then
     prints the registry state. `--no-start` only writes the credentials.

  The pinned `tunnel-client` release (`tunnel_client_version`, `0.0.12`) is
  downloaded into `$CODEX_HOME/chatgpt_web/bin/` with a SHA-256 check unless
  `tunnel_client_path` points at one (then `PATH` and Homebrew locations are
  tried). Credentials reach the client only through its environment. A wrong
  key or tunnel id is terminal (`fatal`, not retried); an unreachable control
  plane is retried with backoff up to 60 s. Without a tunnel id and key
  configured, `tunnel = "openai"` fails with a pointer to `setup`.
- **`cloudflared`** — a quick tunnel (`https://<random>.trycloudflare.com`).
  The URL changes on every daemon start, so the connector is deleted and
  recreated each time. `cloudflared_path` is auto-detected;
  `cloudflared_extra_args` passes flags through (for example
  `["--protocol", "http2"]` when QUIC is blocked). This is the transport the
  spikes ran on.
- **`manual`** — you expose the loopback server yourself and set
  `manual_mcp_url` to its public MCP URL.

### Developer Mode and the connector record

The connector is registered with ChatGPT's own backend, from a chatgpt.com tab
that the daemon borrows (or creates as a dedicated tab) through chrome-mcp: it
enables Developer Mode when `connector_auto_developer_mode = true`, creates the
connector (with the `tunnel_id`, or the public URL), links it without auth,
verifies that the six actions are listed and records the result in
`connector.json`. Reconcile runs on start, whenever the tunnel comes up with a
different endpoint, and before a turn when the record is not `verified`.

If Developer Mode cannot be switched on automatically, turn it on by hand
(*Settings → Apps → Advanced settings → Developer mode*) and run
`codex chatgpt-web registry reconcile`.

## `codex chatgpt-web`

```bash
codex chatgpt-web daemon [--foreground] [--idle-shutdown-ms N]
codex chatgpt-web status
codex chatgpt-web stop
codex chatgpt-web doctor
codex chatgpt-web setup --tunnel-id <id> --api-key-file <path|-> [--no-start]
codex chatgpt-web registry reconcile | show | delete
```

| command | what it does |
|---|---|
| `daemon` | runs the shared daemon: tunnel, MCP server, broker, registry. Detached by default, logging to `daemon.log`; `--foreground` stays attached and logs to stderr. `--idle-shutdown-ms` overrides `daemon_idle_shutdown_ms` (0 = never). Sessions start it on their own; run it by hand to watch it. |
| `status` | JSON: the recorded state, whether the pid is alive, the health report (pid, version, public host, `registry_status`, `tunnel_state`, attached sessions, active turns) and the connector record. |
| `stop` | stops the running daemon. |
| `doctor` | checks the chrome-mcp daemon and its extension, the tunnel prerequisites for the configured `tunnel` (id, key file and binary for `openai`; the binary for `cloudflared`; `manual_mcp_url` for `manual`) and reports the daemon if one is running. Exits non-zero when a check fails. |
| `setup` | the one-time `openai` tunnel setup described above. |
| `registry reconcile` | asks the daemon (starting it if needed) to create or repair the connector and prints the result. |
| `registry show` | prints `connector.json` and the live registry/tunnel state. |
| `registry delete` | deletes the recorded connector — and any other connector carrying `connector_name` — on the ChatGPT side, directly through chrome-mcp. A running daemon recreates it on its next reconcile. |

## `[chatgpt_web]` key reference

Every key is optional. Durations are milliseconds.

| key | default | meaning |
|---|---|---|
| `tools` | `"none"` | `none`: ChatGPT sees only the transcript. `connector`: Codex tools through the MCP connector (needs the daemon). |
| `idle_timeout_ms` | `1200000` | Abandon a turn with no visible progress for this long and stop it. `0` waits forever. |
| `max_parallel_turns` | `2` | ChatGPT turns this process runs concurrently (`0` falls back to the default). |
| `max_tabs` | `3` | Dedicated chatgpt.com tabs kept open, clamped to 1..8. |
| `tab_idle_ms` | `300000` | Idle time after which a pooled tab is closed. |
| `daemon_url` | `"http://127.0.0.1:8848/mcp"` | chrome-mcp Streamable HTTP endpoint (`CHROME_MCP_URL` overrides). |
| `token_file` | `~/.chrome-mcp/token.txt` | chrome-mcp bearer token file (`CHROME_MCP_TOKEN` overrides the value). |
| `base_url` | `"https://chatgpt.com"` | ChatGPT web app (`CHATGPT_URL` overrides). |
| `poll_interval_ms` | `2500` | How often the conversation is polled while a reply streams. |
| `archive_on_shutdown` | `true` | Archive the conversation when its thread shuts down or the agent is closed. |
| `max_fork_turns` | `0` | Upper bound on parent turns a child may inherit through `fork_turns`. |
| `connector_name` | `"Codex Native"` | Name of the custom MCP connector in ChatGPT (also the `@`-mention). |
| `connector_description` | `"Codex tools on this machine (exec, patch, images, harness tools)."` | Its description, at most 200 characters. |
| `tunnel` | `"openai"` | `openai`, `cloudflared` or `manual`. |
| `tunnel_id` | unset | `tunnel_<32 hex>` from platform.openai.com; required with `openai`; written by `setup`. |
| `tunnel_key_file` | `$CODEX_HOME/chatgpt_web/tunnel.key` | Restricted API key (`Tunnels: Read + Use`); `CODEX_CHATGPT_WEB_TUNNEL_KEY` in the environment also works. |
| `tunnel_client_path` | unset | Explicit `tunnel-client` binary; otherwise the pinned release is downloaded, then `PATH` is tried. |
| `tunnel_client_version` | `"0.0.12"` | Pinned `tunnel-client` release. |
| `cloudflared_path` | unset | Explicit `cloudflared` binary; auto-detected otherwise. |
| `cloudflared_extra_args` | `[]` | Extra `cloudflared tunnel` arguments. |
| `tunnel_port` | `0` | Loopback port of the connector MCP server (`0` = ephemeral). |
| `daemon_port` | `0` | Loopback port of the daemon control API (`0` = ephemeral). |
| `daemon_idle_shutdown_ms` | `0` | Shut the daemon down after this long with no sessions attached (`0` = never). |
| `connector_auto_approve_ui` | `true` | Click ChatGPT's consent card for connector calls automatically. |
| `connector_auto_developer_mode` | `true` | Switch ChatGPT Developer Mode on when the connector needs it. |
| `connector_call_timeout_ms` | `120000` | Deadline for one connector call before the daemon answers with an error. |
| `connector_exec_default_yield_ms` | `10000` | `yield_time_ms` for `codex_exec` when ChatGPT omits it. |
| `connector_ready_timeout_ms` | `90000` | How long a turn waits for the connector to be registered and reachable. |
| `turn_ttl_ms` | `3600000` | Lifetime of a turn token in the daemon. |
| `connector_mention_strategy` | `"auto"` | `auto`: mention in the hidden tab, activate it only if the menu never mounts. `background_only`: never activate. `activate`: always bring the tab to the front. |
| `manual_mcp_url` | unset | Public MCP URL of the daemon with `tunnel = "manual"`. |

## Security

- **Execution never leaves Codex.** Every connector call runs in the session
  that owns the turn, under its sandbox and approval policy; the daemon only
  routes. `connector_auto_approve_ui` clicks ChatGPT's own consent card and
  never changes `approval_policy`.
- **Prompt injection is the real exposure.** In connector mode ChatGPT reads
  repository content and can ask for write tools. Codex approvals and the
  sandbox are the guard; run connector agents with the approval policy you would
  give any other agent with the same tools.
- **With `tunnel = "openai"` nothing is exposed on the internet.** The MCP
  server listens on loopback and only the `tunnel-client`, authenticated to
  OpenAI with the restricted key, can reach it. What remains sensitive is
  `tunnel.key` (0600, passed only in the child's environment) and the per-turn
  `turn_token`.
- **With `cloudflared` the endpoint is public**, and these layers apply in
  full (they also hold as defense in depth under `openai`): a 256-bit secret
  path regenerated on every start; a `turn_token` (192 bits) on every tool,
  bound to one session and valid only while its turn is active — outside a
  turn the endpoint only lists six tool names, and a finished token answers
  `already finished`; a global rate limit (30 calls per 10 s, 10 failed token
  claims per minute); an 8 MiB body cap; Codex approvals on every call.
- **Isolation between sessions.** A token maps to one session; a session can
  only complete calls that were delivered to it.
- **What ChatGPT sends.** Every `tools/call` carries `_meta` with the user's
  approximate geolocation and opaque `openai/session` / `openai/subject`
  tokens, plus an `x-request-id` header. The daemon ignores the metadata, logs
  the request id only as a hash, and none of it reaches the Codex session.
- Secrets never appear in logs or command lines; `daemon.json` records the
  public host without the secret path.

## Limits and known caveats

- **Conversation reads are rate limited per account**, so the driver reads
  the backend as little as it can: progress comes from the DOM of the tab
  (stop button, streaming markers, rendered length, the copy button that
  appears when a message finishes) every `poll_interval_ms`, and
  `GET /backend-api/conversation/<id>` is called only when the page shows the
  reply finished, every 30 s while text is still growing (to refresh the
  answer), every 60 s as a safety check when nothing moves, and every 10 s
  after the page says "done" until the API confirms `end_turn`. All backend
  calls of the process share one limiter (one call per 3 s). A short turn costs
  about three backend calls (send, completion read, archive) whatever its
  duration. After a `429 Too many requests` the loop still backs off 20 s →
  120 s and polls the API no faster than every 15 s for five minutes.

- **Calls serialize.** ChatGPT runs connector calls one after another inside a
  single response, so total response time is the sum of the calls. Keep
  `yield_time_ms` at or below 30 s (the contract clamps it) and poll long
  commands with `codex_write_stdin`; three 30 s calls already approach the
  ~100 s idle cutoff of the cloudflared path.
- **Write tools run on every line.** `codex_apply_patch` and `codex_exec`
  were driven through the connector on Instant, Thinking, Extra-high and Pro
  (the "Pro is read-only for custom connectors" claim did not hold).
- **OpenAI's connector safety layer can refuse a call.** In about half of
  the recorded connector turns one call (`codex_exec` with `type mode.txt`,
  or `codex_apply_patch` with a two-line patch) came back to ChatGPT as
  "Esta ferramenta foi bloqueada pelas configurações de segurança da OpenAI"
  without ever reaching the daemon; the other tools of the same turn ran and
  the same call passed on a rerun. This is server-side and non-deterministic;
  the model reports it honestly and the turn completes.
- **The `@`-mention can flake in a hidden tab** on a freshly opened chat (the
  menu sometimes lists recent prompts instead of the connector). `auto`
  activates the tab and retries; expect an occasional focus steal.
- **Rate limits.** Three concurrent turns on one account trigger "too many
  requests". Keep `max_parallel_turns` small; on a 429 the driver waits 30 s
  and retries once before failing the turn as retryable.
- **No streaming beyond polling.** Text and thoughts are refreshed from the
  API on the cadence above (≈30 s chunks while a long answer streams);
  `idle_timeout_ms` is measured by visible progress in the page, not by
  tokens.
- **The web UI is the API.** Composer, send button, effort menu and consent
  card are located by DOM selectors captured in August 2026 and matched with
  Portuguese and English labels. A UI change surfaces as a `UiChanged` error
  naming the phase that failed; an ambiguous submit is never resent blindly.
- **Images**: up to 10 per message (the most recent; older ones stay in the
  transcript as `(not attached; …)`), materialized under
  `$CODEX_HOME/chatgpt_web/attachments/` with a content-hash name and cleaned
  up after 24 h. ChatGPT deduplicates uploads by content across the account;
  the driver dismisses its "already uploaded" popup. Audio is dropped.
- Compaction rewrites the history prefix, so the turn after it replays the
  whole transcript into a new conversation.
- The chrome-mcp daemon serves one browser profile; everything runs as that
  single web account.

## Troubleshooting

Start with `codex chatgpt-web doctor`: it probes the chrome-mcp daemon and the
extension, the tunnel prerequisites for the configured `tunnel`, and the daemon
itself. `codex chatgpt-web status` shows the live registry and tunnel state.

| symptom | cause and fix |
|---|---|
| `chrome-mcp daemon unreachable` (retried) | The daemon at `daemon_url` is down or the extension is not connected. Start it, open Chrome, check `GET /healthz`; verify `CHROME_MCP_URL`/`CHROME_MCP_TOKEN` if set. |
| `LoginRequired` / `SessionExpired` (not retried) | Log into chatgpt.com in that Chrome profile and rerun. The turn was not sent; the conversation is kept. |
| `Too many requests` / 429 | One automatic 30 s pause and retry; reduce `max_parallel_turns`, wait, rerun. |
| `chatgpt_web: no progress for Ns; generation stopped` | The reply showed no change for `idle_timeout_ms`. Stop was clicked and the request marked interrupted; the next turn continues from it. Raise the timeout (or `0`) for long Pro runs. |
| `ChatGPT stopped before finishing the answer (partial completion)` (retried) | ChatGPT's own "something went wrong". Rerun; the request is continued, not resent. |
| `model … is not a ChatGPT Web model line` | Use one of the five `chatgpt-web/*` slugs. |
| `UiChanged` naming a phase | A selector no longer matches chatgpt.com. Look at the tab; the driver does not guess. |
| `MessageTooLong` → `ContextWindowExceeded` | The composer rejected the replay. Codex compacts and retries in a new conversation. |
| `[chatgpt_web] tools = "connector" needs the connector daemon: …` | The daemon could not be started or reached; run `codex chatgpt-web doctor` and `codex chatgpt-web daemon --foreground`. |
| `the chatgpt-web daemon did not come up within 15s` | Run `codex chatgpt-web daemon --foreground` to see why; check `$CODEX_HOME/chatgpt_web/daemon.log`. |
| registry `developer_mode_off` | Automatic enabling failed. Turn Developer Mode on in ChatGPT settings, then `codex chatgpt-web registry reconcile`. |
| registry `browser_unavailable` | The daemon could not reach chrome-mcp to register the connector. Fix the daemon, reconcile. |
| registry `reconciling` / `failed` | Turns wait up to `connector_ready_timeout_ms`; `failed` carries the reason and a retry time. `registry delete` then `reconcile` rebuilds the connector from scratch, including a stale cached tool set. |
| tunnel `fatal` | Wrong key or tunnel id, or no binary. Not retried: rerun `codex chatgpt-web setup`, or point `tunnel_client_path`/`cloudflared_path` at the binary. |
| tunnel `down` | Control plane (or Cloudflare) unreachable; retried with backoff. Under `cloudflared` a new URL triggers a connector recreation before turns resume. |
| ChatGPT answers `turn_token is invalid, expired, or revoked` or `already finished` | It reused a token from an earlier message, or the turn expired (`turn_ttl_ms`). Rerun the turn; the prompt carries a fresh token. |
| `Codex did not finish … within Ns` | A single call outran `connector_call_timeout_ms`. Use shorter `yield_time_ms` and `codex_write_stdin`. |
| `Codex session disconnected` | The owning session died mid-call (missed heartbeats); its turns were revoked. |
