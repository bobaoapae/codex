# App-server loss and silent Desktop stall — 2026-09-14

Status: investigation complete with historical cause unproven; bounded source repair completed and focused regression passed. The subsequent authorized release/hot swap was installed on 2026-09-14 at 18:41 BRT. Existing processes were preserved and still need a later restart to adopt the new image.

The owner observed Codex/ChatGPT remaining idle after an apparent app-server crash. The affected tasks were `Profiling Tool` (`01a08bcb-7256-7922-b3b4-b34f707461cf`) and `Refazer telas aprovadas no Figma` (`01a08c5a-9c8d-79f1-99a5-6d1743297c4b`). The initial approximate window was 05:00–08:00 BRT; actual observable backend loss is later.

## Verified Desktop evidence

Source: `C:\Users\Joao\AppData\Local\Codex\Logs\2026\09\14\codex-desktop-7e78cdbd-ace6-4f80-8c6b-3d38e7e2dda1-11972-t0-i1-000002-0.log`.

| BRT (UTC−3) | UTC | Evidence |
| --- | --- | --- |
| 08:54:45.226 | 11:54:45.226Z | Line 16927: reasoning completion notification for Profiling Tool, turn `01a09f93-5d34-76e2-8a0f-ad9f507ea783`. |
| 08:54:48.467 | 11:54:48.467Z | Line 16928: another task still emits a reasoning completion notification. |
| 08:58:45.174 | 11:58:45.174Z | Line 16929: `configRequirements/read` fails with `Codex app-server process is not available`. |
| 08:58:54.650 | 11:58:54.650Z | Lines 16933–16936: the visible primary renderer records the same failure. |
| 09:40:53.330 | 12:40:53.330Z | Lines 16937–16941: model/config reads still fail with the same unavailable-process error. |

Root read these lines directly. The last healthy evidence is a reasoning notification, not a successful config read. The log establishes loss of backend availability and continuing renderer activity; it does not identify a Rust panic, exit code, signal, or the original process PID. No historical cause is assigned to Headroom, RTK, or the unrelated MCP Chrome error.

The selective evidence review subsequently identified the old server in `C:\Users\Joao\.codex\logs_2.sqlite`, queried read-only with WAL preserved: `process_uuid=pid:83224:6f110dd6-b241-48b3-96c2-0ef86626b22a`. Its 35,164 records span 2026-09-13 17:48:21Z through 2026-09-14 11:54:56.517Z, ending in ordinary activity without a recorded panic or shutdown. These records belong to the incident server, not the investigation sessions started later. An earlier analyst attribution to current sessions was incorrect.

The Profiling Tool rollout at `C:\Users\Joao\.codex\sessions\2026\09\10\rollout-2026-09-10T11-49-21-01a08bcb-7256-7922-b3b4-b34f707461cf.jsonl` persists a final `exec` tool call at 11:54:58.165Z, without its result; the next persisted record is at 12:42:52Z after recovery. The tool-call record is evidence of activity through that timestamp, not proof of the exact time or cause of process death. The first external unavailable-process observation remains 11:58:45.174Z. The Figma task had no activity in that immediate window.

The final root call only requested an `apply_patch` to profiler status and `update_plan`. A child task (`01a0997b-2d8c-7b92-8f76-4837d6e1275b`) had a more relevant active command: PowerShell P/Invoke `FreeConsole()`, `AttachConsole(62348)`, `SetConsoleCtrlHandler(NULL, true)`, and `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 62348)`. Its exec was persisted at 11:54:54.549Z and was still observed by the server logger at 11:54:56.517Z. This is a strong temporal candidate for an external trigger. The explicit signal target is 62348, not app-server PID 83224; no exit status or verified process-group topology yet proves that the signal reached the app-server. The JoinError repair does not claim to fix externally delivered console signals.

The recorded parent chain was `83224 app-server -> 5436 pwsh -> 62348 GodotBuildTool.exe profile stop <capture> --json -> 55400 ssh`; GodotBuildTool started at 08:38:07 BRT. Thus the signal target was a descendant of the app-server. Creation flags and console process-group topology are not recorded. The group ID passed to GenerateConsoleCtrlEvent was nonzero, so this was not an intentional broadcast. No command-start/completion result or `SIGNAL_SENT`/`SIGNAL_FAILED` output survives. Delivery to either 62348 or 83224 remains unproven; no live signal reproduction was attempted against ongoing work.

## Confirmed lifecycle defect in the fork

- Before this repair, `codex-rs/app-server/src/lib.rs:1316` at HEAD `59766eb35b` ignored `Err(JoinError)` from the processor task, treating it like graceful termination.
- The following awaits can block on outbound/transport tasks. The stdio reader in `codex-rs/app-server-transport/src/transport/stdio.rs:43` waits on the client's still-open stdin and does not observe the transport shutdown token.
- Tokio stdin may also retain a blocking read during runtime shutdown. Merely aborting the asynchronous reader is insufficient to guarantee process exit.

That path could leave a backend process alive without a functioning request processor. It was a concrete defect matching the failure class, but the available incident log does not prove it caused this morning's disappearance.

The Desktop application owns subprocess supervision, visible error handling, and restart. That client code is outside this repository. The server repair must expose an abnormal termination correctly without replaying tools or claiming automatic Desktop recovery.

## Repair and validation

Implemented in `app-server/src/lib.rs`, the new private `app-server/src/lifecycle.rs`, and the app-server/CLI executable entrypoints. `RuntimeTaskHandles` preserves the graceful shutdown order; processor JoinError cancels the shutdown token, aborts auxiliary tasks without awaiting a blocked reader, and returns the original error. In stdio mode, both executable entrypoints print the diagnostic and exit 1 before Tokio runtime teardown can block. Non-stdio errors retain normal propagation; normal EOF and Forced remain unchanged.

The single selective Sol review accepted the production behavior. Its test-race finding was corrected by creating the drop guard before spawning each pending task. The regression in `app-server/src/lifecycle_tests.rs` verifies cancellation, task drops, and preservation of panic details with pending tasks modeling an open reader. It does not reproduce a real blocking stdin thread or certify installed process termination.

Validation:

- Focused repository command: `rtk just test -p codex-app-server -E "test(processor_failure_aborts_pending_transport_without_waiting)"`. PASS according to `codex-rs/target/nextest/local/junit.xml`, timestamp 2026-09-14T15:42:12.808-03:00: one testcase, zero failures, zero errors. The wrapper's final process exit code was not captured.
- The one broader `rtk just test -p codex-app-server` run failed. Final counts were not fully captured; failures in remote_thread_store, thread_archive and thread_rollback were observed. Their baseline/causality was not investigated, so they are not labeled pre-existing or caused by this patch.
- The focused CLI app-server run executed but its final result was not captured; inconclusive, not PASS.
- Final formatting and root diff inspection/check completed. No tests were rerun after formatting.

Details: `work/app-server-crash-20260914/validation.md`. Starting repository HEAD: `59766eb35b`. The change does not add Desktop UI notification/restart supervision or protection from external console signals.

## Subsequent release and hot swap

The owner then explicitly requested release and hot swap, emphasizing a quick build. The existing recipe used release optimization 3, LTO=false, debug=0, codegen-units=16, incremental=false and one compiler job, reusing the available target directory. Cargo rebuilt extensive release dependencies and completed with exit 0 in **135m56s**. The elapsed time did not satisfy the requested quick turnaround; disabling LTO/debug did not make this cold rebuild short.

All four binaries were installed together in the existing npm vendor bin directory at 18:41:11–18:41:13 BRT: codex.exe, codex-code-mode-host.exe, codex-windows-sandbox-setup.exe and codex-command-runner.exe. The installer returned 0, kept backups with suffix `.pre-release-20260914-184111`, and verified matching source/destination SHA-256 values for every binary.

Fresh source and installed-wrapper `--version` and `--help` invocations all returned 0; the version remains `codex-cli 0.154.0`. No application restart or process termination was performed. Existing mapped processes continue using their old image until restarted; successful new CLI invocations do not certify the original crash scenario or Desktop recovery.

Evidence under `work/app-server-crash-20260914/`:

- `release-build/20260914-162311/{build.log,cargo-output.log}`.
- `release-install/20260914-184111/{install.log,sha256.tsv}`.
- `release-smoke/source/20260914-184252/{version.txt,help.txt}`.
- `release-smoke/wrapper/20260914-184312/{version.txt,help.txt}`.

The installation phase performed no commit, push, tag, version bump, global configuration change, or additional test suite.

## Source publication

The owner subsequently authorized commit and push to `fork/main` (`bobaoapae/codex`). The publication scope is the app-server lifecycle repair, its focused regression, and the diagnosis/release records. Raw logs, binary outputs, usage ledgers and pre-existing unrelated untracked files remain local. The existing validation results above apply; no build or test suite is repeated merely to commit the unchanged source.
