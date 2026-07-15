# Local fork extensions

This fork preserves `9301c03625` (`/resume` and `/btw`) and `2a7d0eb873`
(capacity recovery) as its immutable baseline. New work lands on short
`local/<feature>` branches and is merged into `feat/tui-capacity-auto-continue`.
Upstream updates are merged from `origin/main`; the local commits are not rebased.

Repository-local Git settings:

```text
rerere.enabled=true
rerere.autoupdate=false
```

## Configuration

All new features are independently selectable under `[local_extensions]`. The
disabled values are `hidden`, `off`, `legacy`, `fixed`, `direct`,
`explicit_request_only`, and `off`, respectively.

```toml
[local_extensions]
operations_dock = "auto"
mouse = "dock"
resume = "checkpointed"
context = "adaptive"
evidence_cache = "read_only"
agent_admission = "adaptive"
agent_delegation = "explicit_request_only"
goal_supervision = "in_process"
```

Derived local data is stored in `$CODEX_HOME/local-extensions.sqlite`. The file
does not contain canonical rollouts or goals, can be deleted safely, and is not
opened when every local extension is disabled.

## Integration seams

- `codex-rs/config/src/config_toml.rs`: the single public configuration table.
- `codex-rs/local-features`: storage and UI-independent policy code.
- TUI and core call narrow APIs from that crate; it never depends on either.
- Existing app-server notifications are reused; the local fork adds no protocol.

## Local features

| Feature | Runtime integration | Main seams |
| --- | --- | --- |
| Stream consolidation barrier | Complete. Command, MCP, and tool lifecycle events wait for durable assistant-message consolidation. | `tui/src/chatwidget.rs`, existing deferred-event FIFO |
| Operations Dock / Task Board | Complete for read-only plans: responsive 80–160 column layout, scrolling, persisted latest plan, resumed snapshot, keyboard focus, and dock-only mouse. | `tui/src/operations_dock`, `tui/src/app.rs` |
| Agent panel | Uses stable `AgentNavigationState` ordering. The active thread is marked in the dock; Main is always the first row. Enter opens a thread, `m` returns to Main, and uppercase `I` interrupts through the existing app-server turn flow. Opening a thread returns focus to the composer; completed agents remain visible. | `tui/src/operations_dock`, `tui/src/app/input.rs` |
| Adaptive context | Complete. The local policy computes EWMA/reserve decisions; core schedules at turn completion and compacts only at the next safe pre-turn boundary. Explicit limits and the canonical threshold still win. | `local-features/src/context.rs`, `core/src/session/context_window.rs` |
| Goal supervision | Complete in-process. Recoverable idle goals retry with persisted cooldown; a third equal blocker marks the canonical goal blocked. Usage, policy, sandbox, and cancellation errors do not retry. | `local-features/src/goal.rs`, `ext/goal`, `app-server/src/extensions.rs` |
| Runtime checkpoints | Storage and validation are complete. Full materialized reconstruction from checkpoint plus rollout suffix is not connected yet, so `resume = "checkpointed"` currently falls back to the canonical reconstruction path. | `local-features/src/checkpoints`, `local-features/src/store.rs` |
| Shared evidence cache | The bounded, shared, dependency-validated LRU policy is implemented and tested. It is not yet decorating the canonical tool executor, so runtime tool calls remain uncached. | `local-features/src/evidence` |
| Adaptive agent admission | Priority, FIFO, aging, pressure, recovery, cancellation, and hard-limit policy are implemented and tested. It is not yet wired into asynchronous spawn/resume admission; the canonical hard limiter remains authoritative. | `local-features/src/admission`, `core/src/agent/control` |

The last three rows deliberately retain canonical behavior until their runtime
integration can preserve cancellation, lifecycle events, and durable rollout
semantics. Their configuration values are accepted but do not claim an
acceleration or scheduling effect yet.

## Merge log

Record upstream merge conflicts here with the upstream commit, affected seam,
and reviewed resolution. No conflicts have been recorded yet.

## Validation

From `codex-rs`:

```text
just fmt
just write-config-schema
just test -p codex-local-features
just test -p codex-config
just test -p codex-tui
just test -p codex-core
cargo build -p codex-cli --bin codex
```

The full workspace `just test` is run only with explicit authorization.

Focused commands used by the local feature commits include:

```text
just test -p codex-local-features
just test -p codex-goal-extension
just test -p codex-app-server extensions
just test -p codex-tui operations_dock
cargo check -p codex-tui
```

On Windows, `cargo fmt --all` is the fallback when `just fmt` cannot find the
repository's external `buildifier` executable. Bazel lock regeneration also
requires Bazel to be installed locally.
