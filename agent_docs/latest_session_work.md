# Latest session work

## Objective

Implement the approved durable-workflow program in milestone order: P0 cross-provider integrity and immutable recovery; P1 workflow/jobs/search/plans/evidence; P2 fleet/ownership/migration/context inspection.

## Current milestone

- P0 is complete after one milestone review and its required fixes.
- P1.1 provenance/fork invariants and P1.2 workflow database foundation are complete.
- P1.3 transient jobs/indexed navigation, P1.4 approved-plan contracts, and P1.5 receipts/evidence are complete with focused gates.
- P2.1 durable mailbox/fleet and P2.2 ownership enforcement are implemented with focused gates; their final cross-crate gates are paused.
- An approved causal-correction amendment now has priority over further P2 expansion. The already-running migration apply slice is being stabilized, after which no new P2 front starts until the amendment closes.

## Mandatory causal-correction amendment

Analysis of 850 rollouts from 2026-08-25 through 2026-08-31 found 3,865 `wait_agent` calls, 98.3% from roots. This task accumulated 442 timed-out waits (about 13h53), including 90 waits that already contained completed subagent results. Spawn latency was not the bottleneck (p50 0.31s).

Confirmed causes:

- `wait_agent` accepted relative targets such as `p1_job_processor`, while mailbox authors are canonical paths such as `/root/p1_job_processor`; target matching did not resolve through `AgentPath`, so valid completion mail could be discarded until timeout.
- Claude local retained a `tx_stdin` clone inside `ControlChannel`; dropping the original sender and awaiting the writer while `control` remained alive prevented EOF, `child.wait()`, and `InFlightGuard` release.

Mandatory implementation order before resuming P2:

1. Canonical, deduplicated, revision-aware `wait_agent`, invalid-target fail-fast, terminal/status wakeups, typed snapshots and `afterRevision`.
2. Claude control-channel teardown before writer wait, bounded writer timeout, explicit cancellation and unconditional `InFlightGuard` release.
3. Terminal agent lifecycle/generation semantics and subtree edge reconciliation shared by `wait_agent`/`list_agents`.
4. Mailbox crash-window closure: persisted is pending, only canonical append plus ACK is delivered; restart requeues every non-delivered UUID and increments wait revision.
5. Runtime-only process heartbeat and `needsAttention`, with relevant rollout transitions/final receipt and explicit-only cancellation.
6. Fork timing/cache/context metrics and checkout/target admission for broad Rust builds.
7. Isolated smoke using the new build, without replacing or restarting the active app-server/runtime.

Immediate mitigation in this still-old runtime:

- Use only canonical wait targets (`/root/<agent>`).
- Do not repeat a wait at the same observable revision after timeout.
- Do not send repeated follow-ups to an agent busy in a tool/terminal.
- New executors for this amendment use `fork_turns="none"` or a bounded numeric history with a self-contained prompt.
- Keep at most one broad Rust build/linker active for this checkout; never auto-kill long Rust processes.

Deployment truth: this task is running Codex npm 0.146.1 started on 2026-08-29, not the modified checkout. Passing source tests cannot prove this active process is retroactively fixed. The active app-server/binary must not be replaced; proof requires a separate isolated smoke later.

E1 source result:

- Relative/canonical target resolution, deduplication, root-scoped causal revision, per-agent last-change revision, `afterRevision`, mailbox/status/terminal wakeups and typed snapshots are implemented.
- Model-facing guidance now requires reusing the canonical path and waiting only after a newer revision.
- Focused `wait_agent`/wait-state gate: 30 tests passed.
- `cargo clippy -p codex-core --lib` did not reach E1 because the paused migration slice still has an unrelated `expect_used` in `thread-store/src/local/rollout_migration/apply_support.rs`; this is recorded as blocked, not passed.

E2 source result:

- Claude teardown now drops the control channel and all stdin senders before joining the writer.
- Writer teardown is bounded to five seconds and returns structured process/control state; it does not silently auto-kill.
- Explicit cancellation still reaps the process tree, and in-flight accounting is released for normal completion, provider error, cancellation, early exit and teardown timeout.
- Focused Claude lifecycle gate: 5 tests passed; core clippy with `--no-deps` passed. Full dependency clippy remains blocked by the paused migration `expect_used` noted above.

E3 source result:

- Agent registry now has one typed lifecycle and explicit generation shared by `wait_agent` and `list_agents`; graph `Open` remains lineage, not liveness.
- Terminal completion, abort and error release logical active/spawn-slot accounting while preserving follow-up and rollout history.
- A follow-up to a terminal member atomically advances generation; active-generation follow-up does not.
- Residency eviction releases active accounting, subtree close reconciles descendants deepest-first, and restart reconstructs generation without reopening closed edges.
- Focused lifecycle gate: 13 tests passed; core clippy with `--no-deps` and `git diff --check` passed.

E4 source result:

- Mailbox rehydration immediately returns every non-delivered row (`pending` or `delivering`) to the recipient queue and fences the old delivery generation.
- Canonical rollout UUID presence, not an in-memory persistence cache, decides deduplication; crash after append only ACKs, while crash before append requeues.
- ACK remains after canonical append/flush and queue admission; delivered rows never redeliver, ordering/backpressure remain intact, and redelivery updates the E1 causal revision.
- Focused gates: state mailbox 10/10 and core mailbox/wait 31/31 passed; core clippy with `--no-deps` and `git diff --check` passed.

E5 source result:

- Unified-exec processes expose a bounded, redacted terminal snapshot with session/PID/timing/activity/output metadata and typed lifecycle state.
- Quiet live processes transition to `needsAttention` in runtime/SQLite and wake `wait_agent`; new output/input clears the state. Heartbeats do not append recurring rollout/model-context items.
- Cancellation remains explicit, exited processes are reaped, shutdown cleans up, ownership guards stay live through process exit, and final evidence is emitted once.
- Focused gates: unified exec 61 tests, wait-agent 28, wait-state 6 and needs-attention 2 passed; core clippy with `--no-deps`, formatting and diff checks passed.

E6 source result:

- Spawn metrics persist bounded timestamps for request, child creation, first event, first response created after the inherited-history boundary, and completion, plus projected fork size and aggregate cache-token counters.
- Full-history behavior is unchanged; a near-compaction projection emits one structured warning and never changes the global default or auto-compacts.
- Broad Rust workspace build/test/link commands acquire a cross-process checkout/target admission guard retained through process exit/cancel; focused package commands remain concurrent, and busy admission returns typed `BuildAdmissionBusy` without retry or kill.
- Focused gates: state metrics 3, core build-admission 5 and core fork-metrics 2 passed; core/state clippy with `--no-deps`, formatting and diff checks passed. No broad workspace test was run.

E7 smoke iteration 1 (not a pass):

- E1 34, E2 5, E4 18, E5 106, E6 10, build-info 5 and RuntimeBuildInfo protocol/rollout 2 focused tests passed.
- E3 had one failing legacy resume expectation: an explicitly closed subtree left its grandchild not loaded, while the old test expected the open descendant to reopen. This must be reconciled with the amended rule that close reconciles the entire subtree and closed edges never reopen.
- `cargo build -p codex-cli` failed because `app-server-test-client` had stale `ThreadListParams` literals missing `root_thread_id`, `terminal_outcomes` and `thread_classes`; no new CLI binary was launched.
- Baseline after the failed smoke: helper PIDs unchanged (28), Claude running turns `[0,0]`, no Rust process, no build-admission lock. One environment-owned writer lock independently released (62 to 61).
- Because a test failed and the binary smoke was blocked, E7 remains in progress; nothing is declared passed.

E7 smoke iteration 2 (pass):

- The stale close/resume test was aligned with the amended contract: `close_agent` closes the entire subtree and a later child resume does not reopen closed descendants. The app-server test client received the new optional thread-list fields.
- Focused smoke invocations: E1 34, E2 5, E3 22, E4 18, E5 106, E6 10, build-info 5, and RuntimeBuildInfo protocol/rollout 2; 202 total passed with zero failures, blocked tests or counted skips.
- `cargo build -p codex-cli` passed and the new checkout binary ran under a unique temporary `CODEX_HOME`; `codex.exe --version` returned `codex-cli 0.0.0` with exit 0. The installed npm runtime/app-server was not restarted or replaced.
- Final baseline matched: helper PIDs 28, Claude running turns `[0,0]`, Rust processes 0, writer locks 62, build-admission locks 0. `git diff --check` passed.
- The active task still runs the old npm 0.146.1 process; only the isolated new-build process proves the amended source behavior.

## P2.3 real preview (no apply)

- Current-source `codex migrate-rollouts --json` ran from the checkout binary with no `--apply`/`--verbose` and exited 0; stderr was empty.
- Durable report: `agent_docs/rollout-migration-preview-20260831.json` (30,683,563 bytes, valid JSON).
- 31,035 entries: 30,120 eligible, 903 skipped, 9 busy, 3 invalid, 0 malformed, 0 pending and 0 internal migration receipts.
- Classes and per-entry details are in the report. Aggregate bytes: 27,197,518,726 plain; 0 zstd; 24,299,453,851 canonical; estimated temporary space 18,853,881,855 bytes.
- Index projection estimate: 95,951 allowlisted items and 6,355,080 excluded items. Preview duration: 1,059,062 ms (about 17m39s).
- A strict before/after no-write proof was inconclusive because the active old runtime concurrently created five session files and updated its state/history DB during the scan. Workflow DB stayed absent, archived sessions and pending markers did not change, and no migration-specific write was observed.
- The active runtime was not paused/restarted. No corpus apply has been authorized or executed; the preview counts/space/duration must be presented before any separate opt-in.

P2.3 focused closure:

- Thread-store migration/preview/backfill/tombstone gate: 74 tests passed; rollout compression 28; state tombstone/backfill 13.
- CLI migration/report tests: 5; internal migration-receipt idempotency/classification tests: 4. Receipt rollouts are excluded from future watermark/index/coordinator calculations while other `Internal` rollouts remain normal.
- TUI fleet/overview gate: 11 tests passed with no leaks and no pending snapshots. App-server thread delete: 4/4; thread-store tombstone: 2/2.
- Tombstone now rejects a paginated thread writer-owned by another process before any visibility mutation, preserves rollout/state, and succeeds after ownership release.
- The only remaining P2.3 external action is real `--apply`; the approved contract requires a separate opt-in after presenting this preview, so it remains unexecuted.

## P2 milestone review findings (fixes required)

The single read-only P2 review found three blockers and nine additional product/security defects; no second review will be run. Closure is paused until focused fixes/tests complete:

- Fleet `Recoverable` operations can retain `active_operation_id`/sealed admissions indefinitely because no production recovery caller clears/restarts them.
- Claude `bypassPermissions` can skip the `can_use_tool` ownership/destructive-Git guard.
- Mailbox crash after canonical append but before ACK can dedupe the content yet lose the durable `trigger_turn` wake-up.
- Destructive Git/classifier gaps include `switch`, destructive branch/worktree/ref verbs, executable `git -c`/`--config-env`, inverted `cp/mv/install/ln -t` paths, missing move source paths, `sort -o`, and `find -fprint*` writes.
- Claude child wait is unbounded after a result frame; Windows tree-kill orders direct-parent kill before `taskkill /T`.
- Broad-build admission wrongly blocks non-Git directories and ignores `--target-dir`.
- Linked-worktree bypass proves only worktree shape, not exclusive actor assignment.
- Apply re-runs discovery instead of consuming the exact frozen preview set approved by the user.

P2 review fixes and validation:

- Recoverable fleet operations now have an explicit generation-fenced resume/close recovery path; failed members no longer wedge admissions permanently.
- Claude writable subagents always traverse the ownership prompt/guard, child wait is bounded, and Windows tree cancellation runs before direct-parent termination.
- Mailbox trigger metadata and `wake_applied` are canonical/deduplicated, so append-before-ACK crash recovery wakes an idle recipient exactly once.
- Git/shell classification now covers destructive switch/branch/worktree/ref operations, executable config overrides, `-t` destinations/move sources, `sort -o`, and `find -fprint*` writes.
- Build admission degrades to unmanaged outside Git, keys `--target-dir`, and linked worktrees require a durable actor/environment lease rather than shape-only bypass.
- Apply now requires an explicit frozen preview report, reattests the exact ordered source set under the maintenance lock, excludes later rollouts, rejects stale sources before mutation, and binds idempotency to the preview digest.
- Serial focused validation passed across affected core/state/shell/CLI/app-server/TUI crates; schemas and pending snapshots are clean; `just fork-invariants` passed 49 tests.
- Full rollout-migration filter was migrated to the frozen contract and passed 62/62.
- `context/inspect` is complete in core (loaded/cold), experimental app-server API, `codex debug context`, and TUI `/context`/`/context preview`. Focused gates: core 5 plus reconstruction 30; app-server 3; CLI 5; TUI 10; no pending snapshots.
- App-server README now contains fork-only experimental examples for recovery, jobs/search, approved plans, evidence/artifacts, fleet/leases, context inspection, migration preview/apply, and compression gating.

Remaining user-gated actions:

- The repository contract requires explicit authorization before the single broad `just test`; it has not been run.
- Real corpus apply requires separate opt-in. Because apply now consumes a frozen report, the earlier 30.7 MB decision report cannot be used as the mutation token; after authorization and with the active runtime quiescent, generate a fresh frozen preview and pass that exact report to `--apply`. No apply has run.

## Windows release hot swap — 2026-09-01

- User explicitly authorized a release build and direct hot swap of the globally installed npm vendor executable.
- `cargo build --release -p codex-cli --bin codex` completed with 0 errors and 20 warnings.
- Release artifact: `codex-rs/target/release/codex.exe`, 341,196,800 bytes.
- Release and installed SHA-256: `7D323912C429A2A5B1648A72B6A33F0425155DEFD3D3E850F88B38CC78077985`.
- Installed npm package remains `@openai/codex` 0.146.1; the fork binary reports `codex-cli 0.0.0`.
- Installed vendor path: `C:\Users\Joao\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe`.
- Recoverable backup: `codex.exe.backup-20260901-095948-049-7392da79d464473892a36c1c6bc53fa5.exe` in the same vendor directory.
- Isolated release probes passed for `--version`, `debug context --help`, and `migrate-rollouts --help`; a new wrapper invocation used the new binary.
- Existing PID 7228 remained alive with its original start time/path; it continues executing the old mapped image until that runtime is closed. No process was killed or restarted.
- The rollout cleanup command is now a frozen two-step contract: generate a fresh `--preview-report`, then pass that exact report to `--apply`. The earlier decision report is intentionally not accepted for mutation.

## Invariants

- Preserve unrelated dirty/untracked work and never reset, clean, stash, or commit implicitly.
- The rollout for thread `01a05464-12ca-75c3-b7a8-856c95a3aaee` is immutable.
- All new app-server APIs are v2, experimental, fork-only, camelCase, and schema-exported.
- Historical rollout JSONL (or its verified compressed representation) remains canonical.
- No automatic retries, purges, or lowering of the Ultra-only proactive threshold.

## Verified P0 result

- ChatGPT Web and Claude plaintext tool calls carry an explicit plaintext marker; OpenAI-to-OpenAI ciphertext remains supported.
- Receiver-local guards run after an unloaded agent is rehydrated; sensitive multi-agent arguments are redacted in tracing, OTel, and rollout trace.
- The exact undecryptable-function-output sentinel is non-retryable, survives cold resume, and blocks new provider requests until explicit recovery.
- `thread/recovery/preview` and `thread/recovery/create` are experimental, use physical ordinals/watermarks, preserve the source rollout, support an idle loaded writer through quiescence attestation, and are idempotent across CLI processes through a deterministic recovered thread ID.
- The real preview for `01a05464-12ca-75c3-b7a8-856c95a3aaee` returned `canRecover=true`, 336 total items, 229 retained, 107 excluded, 9 failed retry turns, invalid envelope ordinal 208, contaminated terminal ordinal 221, and watermark 336/722980. No create was executed.
- Focused gates: protocol 321/321; codex-api 184/184; app-server-protocol 300/300 with 1 skipped; thread-store recovery 11/11; app-server recovery 1/1; TUI recovery 10/10; transport/core, OTel, rollout-trace, CLI recovery and clippy filters passed.
- The broad app-server crate gate ran 1340 tests: 1324 passed and 16 unrelated environment/fixture tests failed because helper binaries/code-mode host were absent or global skill counts differed.

The full workspace test remains user-gated at final closeout.

## Verified P1.1 result

- Runtime build information is initialized by the CLI, app-server, exec, TUI, and MCP-server binaries.
- Optional build/config/runtime-feature revisions are persisted in session metadata and applied-thread settings without serializing configuration values.
- `fork-invariants.toml` and `just fork-invariants` cover local providers, Plan Mode, multi-agent v2, experimental APIs, and the Ultra-only proactive threshold through existing behavioral tests.
- Focused build-info, protocol, config, rollout, core, app-server, and fork-invariant checks passed.

## Verified P1.2 result

- `workflow_1.sqlite` has independent migrations and owns live coordination for workflow runs, receipts projections, checkpoints, mailbox, path leases, backfill journals, and FTS generations.
- Run idempotency is root-scoped and parameter-bound; terminal transitions use CAS; abandoned pending/running jobs reopen as inconclusive and never retry automatically.
- FTS accepts only allowlisted user/final-assistant/compaction-summary/approved-plan/receipt-metadata documents, binds cursors to generation/query/filters, and publishes generations atomically.
- The workflow state tests passed (202/202), including database reopen, concurrent claims, stale tokens, idempotency conflicts, and search generation behavior.

## Verified P1.3 result

- `codex exec --transient` is distinct from `--ephemeral`; transient threads use the normal persisted thread/turn pipeline and are classified as `transientJob`.
- Experimental `job/run`, `job/list`, `job/read`, and `job/cancel` use `workflow_1.sqlite`, explicit idempotency, durable terminal outcomes, and no hidden approval prompts.
- Terminal job state is derived exclusively from canonical turn events. The former thread-status watcher was removed so an idle observation cannot race ahead of a failed `TurnComplete`.
- The rollout projector indexes each physical source once, supports plain and zstd rollouts plus live overlay, and excludes tools, ciphertext, inter-agent content, stdout, and payloads.
- `thread/search` uses the active FTS generation when available, supports the approved filters and cursor binding, and falls back with an explicit partial/index state. TUI `/resume` uses the backend search path.
- Focused gates: transient lifecycle 5/5, app-server job integration 2/2, thread-store search index 8/8, workflow state 202/202, central app-server P1 package 31/31, exec transient/ephemeral 5/5, and TUI search/jobs/plans 31/31 with no pending snapshots.

## Verified P1.4 result

- The plan store uses an interprocess lock, bounded metadata, no-follow path validation, and immutable approved snapshots under `plans/approved/<opaque-id>/<revision>.md`.
- `plan/approve` uses CAS against the current draft revision; snapshots are pinned, idempotent only for identical content, and previous approved revisions derive as superseded.
- `thread/start` and `turn/start` accept experimental `approvedPlan`; the exact snapshot is resolved before admission, a non-complete Goal conflicts, and the typed `plan.loaded` fragment is admitted atomically before the user input.
- Cold resume, fork, rollback, and compaction reconstruct the surviving checklist and approved-plan reference without a parallel task ledger.
- Focused gates: codex-plans 25/25, core plan/context 15 focused tests, Goal claims 3/3, app-server plan/Goal coverage in the 31-test central package, and TUI approved-plan coverage in the 31-test TUI package.

## Verified P1.5 result

- Canonical `receipt.attached` extension items are bounded, version tolerant, persisted in Legacy/Paginated rollouts, and projected idempotently into `workflow_receipts` from live/plain/zstd sources.
- Trusted synchronous `PostToolUse` hooks can contribute bounded evidence through a channel that never becomes model context; automatic receipts reference canonical items and never copy stdout, arguments, ciphertext, or raw payloads.
- Experimental `evidence/list`, `evidence/attach`, and `evidence/export` append before acknowledgement, use explicit selection and redaction, and are not exposed as model tools.
- Experimental `artifact/read` accepts only opaque artifact IDs, enforces UTF-8/keyset cursor binding and a 64 KiB maximum, and never accepts a filesystem path as authority.
- Focused gates: extension items 11/11, hooks 175/175, core evidence 10/10, receipt state 4/4, receipt projection 3/3, artifact state 6/6, app-server protocol 313 passed with 1 skipped, and evidence/artifact coverage in the central 31-test package.

## P1 milestone review and fixes

- The single read-only P1 review found one trust-boundary blocker: config fingerprints redacted hook `env`/header values before hashing, allowing a changed MCP-hook input to retain a trusted hash. `version_for_toml` now hashes the complete canonical TOML while exposing only the digest; config and hook-trust regressions pass.
- Workflow job metadata no longer serializes prompt/config payloads. It stores bounded counts/source/class plus a digest, so valid inputs above 64 KiB do not hit the metadata limit and secrets remain only in canonical paths.
- Receipt metadata validation now uses one shared denylist across extension items, hooks, and app-server. Evidence export reports whether redaction occurred.
- Archive filtering is evaluated from current hydrated thread metadata rather than an immutable FTS snapshot.
- `evidence/attach` now performs idempotent Created/Existing/Conflict decisions under the canonical rollout lifecycle/writer lock; SQLite is no longer an existence authority.
- Post-review focused gates: config fingerprint 1/1, hook trust 1/1, extension items 12/12, hook evidence 6/6, state 211/211, jobs 4/4, canonical receipt append 3/3, app-server P1 package 30/30, and app-server protocol 313 passed with 1 skipped.
- The review's observation that hard `thread/delete` leaves workflow projections is intentionally resolved by P2 tombstoning and indefinite retention; this program must not add a physical purge.
