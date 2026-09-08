# ChatGPT Web: validation status, 2026-09-08

User request: validate Recente/Latest (reported as GPT-6 Astra), effort selection, long Pro responses, abandoned browser tabs, Codex Native consent and Chat versus Work.

Status: corrections and focused validation completed. Chrome MCP confirmed Recente/Pro selection, Chat versus Work, and the actual `gpt-6-pro` backend. A real read-only Native fixture read returned the expected marker and sum 18, with command and CLI exit 0. Release and hot swap were subsequently authorized and are tracked separately.

## Completed correction and validation

- Named efforts select Recente and confirm the model/effort before sending, including continuation. Explicit backend model changes on an existing conversation fail before send.
- Chat/Work selection is checked, then the composer must remain mounted for 250 ms within the bounded readiness deadline.
- Long Pro waits refresh authoritative state instead of completing/cancelling solely on old time thresholds. Internal agent activity is renewed without heartbeat text in model history. Recovery preserves owned-tab affinity; idle unbound tabs can be reclaimed.
- Consent uses an effective DOM click and distinguishes attempted click from stable card disappearance. Execution approval remains separate.
- Existing focused gate: 311/311 passed (3940 outside-filter skips). Follow-up composer gate: 68/68 passed (4184 outside-filter skips). Scoped fix, fmt and debug build passed.
- The isolated debug CLI initially lacked `codex-windows-sandbox-setup.exe` and `codex-command-runner.exe`. Building those sidecars fixed the demonstrated local execution failure without relaxing sandbox permissions.
- Final real probe: thread `01a08222-d920-7c82-99ab-ff7ba118d12b`, conversation `6aa04a8d-a9b8-83e9-95bf-9d8f3b81c5f9`, completed 2026-09-08 17:51:47 UTC. Native Get-Content exit 0; MARKER=WEBPRO-20260908-ALPHA, SUM=18. Owned tab closed after completion; unrelated tabs preserved.
- Limits: the final probe did not display a consent card and did not run beyond 20 minutes. Those cases have focused code coverage, not a claimed live-duration or visual-card certification. Earlier generic assistant security messages do not independently identify their cause.

Detailed local logs: `work/chatgpt-web-validation-20260908/`, particularly `chatgpt-web-focused-rerun-5.log`, `focused-web-readiness.log`, `sandbox-helper-build.log` and `result-native-sidecars.md`.

## Initial source findings (historical)

- Effort selection is fail-open. `core/src/chatgpt_web/driver/ops.rs::prepare_and_send` records failed menu selection and model-label mismatches as notes, then continues sending. It also ignores a model request when continuing an existing conversation. The selector has five fixed ordinals and legacy labels; model-family fallback is `gpt-5-6`. Exact slugs can be resolved from the fetched model list, so absence of a Recente alias alone does not prove every GPT-6 slug is unusable. Live Recente/Astra semantics remain unverified.
- Chat/Work is absent from `ComposerState`, `SendRequest` and the pre-send assertions (`driver/page_scripts.rs`, `driver/ops.rs`). The driver cannot currently prove it is sending in Chat mode.
- Connector consent is optimistic. `connector/connector_attach.rs::approval_script` reports `clicked=true` after dispatching synthetic events without confirming that the approval card resolved. Codex execution/permission approvals are a separate layer and are not satisfied by this click.
- The connector broker defaults to a 120-second tool-call timeout (`connector/daemon/broker.rs:57`). Timeout removes the pending call; a later approval cannot complete that removed call. This is a possible contributor, not yet the reproduced cause of the user's symptom.
- Pro polling normally has a 20-minute idle watchdog, not a short total-response deadline. Progress resets idle time. `stream.rs::ReplyTracker` nevertheless has a completion path that accepts an ended interim message after 600 seconds without change even while the async flag remains active. The DOM-active check is below that cap. The actual effect on the current UI requires reproduction.
- Agent wait activity is separate from provider progress. `tools/handlers/multi_agents_v2/wait.rs::target_needs_attention` treats missing/old activity as attention-needed. A quiet Pro may therefore cause the orchestrator to return from waiting while the provider is still running; timeout is not proof the response stopped.
- Tabs are pooled process-wide. `driver/tabs.rs::Drop` stops the sweeper and releases ownership without closing tabs. The pool intentionally retains one primary tab, and `shutdown()` is not called on each agent completion. Closing every tab unconditionally would break shared ownership; cleanup needs to distinguish idle-owned tabs from active/shared generations.

## Runtime evidence and limits

The read-only inspection of the ChatGPT Web daemon log found repeated browser-unavailable/reconcile transitions, eventual verified states, interrupted-turn claims and cancellation events. It did not establish the original cause of interruption or prove a Pro timeout.

The browser inventory through CUA timed out. Computer Use then identified two Chrome windows titled ANE Security; the selected window was minimized. Its activation/inspection was blocked because the tool could not determine the current browser URL with enough confidence to enforce policy. No further browser automation was attempted, and no new chat was created.

The owner subsequently requested Chrome MCP explicitly. That interface successfully identified the tab URL and DOM, and was used for selector validation. A connector fixture must be non-sensitive, and pending consent must be distinguished from execution approvals rather than bypassed.

## Existing focused coverage to use after a scoped correction

- `driver/page_scripts_tests.rs`, `driver/ops_tests.rs`: selectors, menu labels and send behavior.
- `stream_tests.rs`: completion/progress behavior; needs a regression for long Pro activity and interim completion.
- `driver/tabs_tests.rs`: ownership, sweeping and explicit shutdown; no live tab cleanup certification.
- `connector/connector_attach_tests.rs`: static script/pure parsing coverage, not real DOM approval acceptance.
- `connector/daemon/broker_tests.rs`, `connector/client_tests.rs`: call TTL, registry and recovery.
- `tools/handlers/multi_agents_v2/wait*_tests.rs`: typed wait reasons and attention conditions, not a long quiet Web Pro generation.

No additional validation runner or harness was introduced. The initial pending findings above were addressed by the completed correction and validation at the top of this document.
