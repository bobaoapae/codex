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
