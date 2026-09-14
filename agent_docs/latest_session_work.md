# Latest session work

### Source publication — 2026-09-14

- Owner explicitly authorized committing and pushing the app-server repair and its diagnosis/release records to fork/main (bobaoapae/codex). Publication includes the five changed source/test files and the two related agent_docs records. Pre-existing untracked work, raw logs, build outputs and usage ledgers remain local.
- The source was already reviewed, tested within the documented limits, built in release and installed. Commit/push does not change the code or require repeating those builds/tests.

### Release and hot swap for the app-server repair — 2026-09-14 (completed)

- Owner explicitly requested release and hot swap, emphasizing fast compilation. This authorizes local binary build/install; no commit, push, tag, version bump, or application restart is requested.
- executor_luna `release_hotswap` owns build/install/smoke with the existing scripts under work/sync-release-20260909. Root owns Git and status/handoff records.
- Use the proven release profile: LTO=false, debug=0, debug_assertions=false, codegen-units=16, opt-level=3, incremental=false, Jobs=1, existing target/cache. Two-job builds previously hit real allocation failures; current available physical RAM was about 24.9 GiB. Verify the actual cache before estimating build time.
- Build/install the complete four-binary set: codex.exe, codex-code-mode-host.exe, codex-windows-sandbox-setup.exe, codex-command-runner.exe. Resolve the current vendor path, preserve mapped processes, retain timestamped backups, verify release source/destination hashes, then smoke fresh --version and --help invocations. Existing processes do not adopt replaced images automatically.
- Prior focused lifecycle regression passed; broader app-server tests failed and CLI test result remained inconclusive. Do not repeat suites for this release request. The release build is the compilation authority; logs go under work/app-server-crash-20260914/release-*.
- Build started at 16:23:11 BRT; live evidence is work/app-server-crash-20260914/release-build/20260914-162311/{build.log,cargo-output.log}. Cargo is rebuilding foundational release dependencies; the available release cache is not complete, so do not promise a near-instant incremental build or attribute cache removal without evidence.
- Build has progressed through codex-core to codex-app-server without a reported compilation error. Root verified the live rustc process was consuming CPU, with Normal priority, full processor affinity, opt-level=3 and codegen-units=16. The cold release rebuild is substantially longer than the owner requested; report its actual duration, not merely the recipe's "fast" label. Install/smoke remain authorized immediately after build success.
- Cargo completed the optimized release with exit 0 in 135m56s. This did not meet the owner's request for a short build. No LTO/debug or extra test suite was used, but the one-job full release rebuild was slow. Do not call the resulting elapsed time fast.
- Hot swap completed with exit 0 at 18:41:11–18:41:13 BRT in the existing npm vendor bin directory. All four source/destination SHA-256 values match; backups use suffix .pre-release-20260914-184111. Root read install.log and sha256.tsv directly. Evidence: work/app-server-crash-20260914/release-install/20260914-184111.
- Fresh source and installed wrapper --version and --help all exited 0; version remains codex-cli 0.154.0. Evidence: release-smoke/source/20260914-184252 and release-smoke/wrapper/20260914-184312 under work/app-server-crash-20260914. Existing processes were preserved, with pre/post snapshots recorded by the executor; no process termination or application restart was performed. New invocations load the replaced images; already-running app-server instances still need a later restart to adopt the patch.
- Root final diff check passed. No production edits, commit, push, tag, version bump, global config change, or extra test gate was added during release/install.

### App-server crash and silent Desktop stall — 2026-09-14 (source repair complete; later hot swap recorded above)

- Owner reports an app-server crash this morning followed by Codex/ChatGPT remaining idle without an error, stop, or restart; investigate and correct what the evidence supports.
- Owner narrowed the incident to 05:00–08:00 BRT (08:00–11:00 UTC), affecting `Profiling Tool` (01a08bcb-7256-7922-b3b4-b34f707461cf) and `Refazer telas aprovadas no Figma` (01a08c5a-9c8d-79f1-99a5-6d1743297c4b). Both currently report active; the later sessions do not establish recovery behavior in the incident window.
- Task 1: correlate current Windows/desktop/runtime logs with process exit and the client supervision path. Read-only analysts own runtime evidence and source mapping separately; root owns Git and this status record.
- Task 2: delegate the smallest confirmed repair to executor_luna after the failure path is identified. Preserve all concurrent work and current running applications during collection; no speculative proxy/config changes.
- Task 3: existing focused tests for the changed component, at most one relevant broad gate, final formatting and a concise diagnosis with runtime-adoption limits. No release, commit, or full-workspace suite is currently requested.
- Starting HEAD: 59766eb35b. Initial tracked working tree is clean; existing untracked agent_docs, work, target and app-server-protocol/work artifacts are pre-existing.
- Prior 8 September evidence confirms app-server unavailability but did not establish its cause; the proxy fallback defect fixed on 9 September is separate and must not be presumed causal today. Current Headroom health is already provided by the environment.
- Root verified Desktop log lines 16927–16929: Profiling Tool reasoning event at 08:54:45.226 BRT, another task event at 08:54:48.467, then repeated `Codex app-server process is not available` from 08:58:45.174. Lines 16937–16941 show continued unavailability at 09:40:53.330. These are reasoning notifications, not the configRequirements/read success initially reported by the analyst. No app-server crash event was found in Windows Application/WER for the initial window; absence of WER does not rule out panic/abort/EOF. Exact historical exit cause remains unproven.
- Source confirms a relevant lifecycle defect: app-server ignores processor JoinError, then awaits outbound/transport tasks; the stdio reader can remain blocked on open stdin. executor_luna `fix_server_failure` owns the bounded repair and regression in app-server/stdio and only necessary CLI termination boundaries. This is a confirmed code defect, not yet the proven historical crash cause. TUI-only findings are outside the patch; Desktop restart/UI supervisor is outside this repository.
- Selective Sol evidence review identified old app-server PID 83224, process UUID `pid:83224:6f110dd6-b241-48b3-96c2-0ef86626b22a`, in read-only logs_2.sqlite. Its final normal event is 11:54:56.517Z; Profiling Tool persists an exec call without result at 11:54:58.165Z, then resumes records at 12:42:52Z. No panic/exit/signal is yet recorded. Earlier analyst claims that these rows belonged to the current investigation, or that no target rollout activity existed, were incorrect. Incident details: agent_docs/app-server-crash-20260914.md.
- A child attempted targeted Win32 CTRL_BREAK against descendant GodotBuildTool PID 62348 immediately before telemetry stopped; process chain was 83224 -> 5436 -> 62348 -> 55400. No signal return/exit code or console-group flags survive, so this is a temporal candidate, not a proven cause or a case covered by the JoinError fix.
- The single Sol review accepted production lifecycle behavior: preserve JoinError, abort side tasks without waiting, and hard-exit 1 inside the stdio binary runtime; non-stdio errors propagate, and normal EOF/Forced remain unchanged. Two small findings remain assigned to executor: create the test drop guard before spawn to avoid a scheduling race, remove a vacuous sync timeout/unused import, then focused validation and final fmt. No second review or broader test suite is planned.
- Closeout: review findings are fixed; root inspected production diff and the corrected guard ownership. Final JUnit at 15:42:12.808 BRT confirms the focused regression PASS (one test, zero failures/errors), while wrapper exit code was not captured. The broader app-server suite FAIL has incomplete counts and uninvestigated baseline/causality; CLI filter result is inconclusive. Do not describe the broad failures as pre-existing or claim full-suite/CLI success. Formatting and diff check completed; no tests after formatting. Final style/doc-only cleanup adds the required pretty_assertions import and clarifies the uncaptured wrapper exit; no new test/build/review.
- Final diagnosis deliverables: agent_docs/app-server-crash-20260914.md and work/app-server-crash-20260914/validation.md. Source fixes the internal JoinError shutdown path. Historical crash cause, external CTRL_BREAK delivery, and Desktop UI restart/notification remain unproven or outside this repository. The diagnosis turn performed no installation; the owner's subsequent release/hot-swap request was completed as recorded above. Existing running processes retain the previous image until restarted.


### Fork sync and release 0.154.0 — completed 2026-09-10

- Completed on the existing checkout, with no separate worktree. Source build commit: 2b5f73c140d4e677b6371454ea72651bb7ea63fe, containing upstream a62e98d18c and stable release rust-v0.154.0. Final closeout commit/push follows this record.
- Final release build succeeded in 30m57s with LTO disabled, debug disabled, 16 codegen units and opt-level 3. Two concurrent compiler jobs hit a real allocation failure; the successful cached retry used one job. No cache cleanup or system-memory configuration change was made.
- Installed all four binaries together: codex.exe, codex-code-mode-host.exe, codex-windows-sandbox-setup.exe and codex-command-runner.exe. Source/installed SHA-256 values matched for all four. Backups use suffix .pre-release-20260910-030829 in the existing npm vendor bin directory.
- Fresh source and installed wrapper checks reported codex-cli 0.154.0 with exit 0; installed --help also exited 0. Existing Codex processes were preserved and retain their old mapped images until restarted.
- Migration compatibility: all previously tracked state/workflow SQL files remain unchanged from 9019cd8439. New 0056 uses pinned LF and preserves thread_artifacts while creating/copying into thread_attachments. Migration checksum, preserved-data, artifact and attachment tests passed.
- Validation limits: codex-state ran 250 tests (249 passed, one pre-existing query-plan expectation failed, zero skipped). Completed fork-invariant slices passed features 1, core 28, core-plugins 1 and protocol 9; the remaining broad gate was not completed. Core compilation check and final formatting passed. No full-suite or full-clippy pass is claimed.
- Evidence: work/sync-release-20260909/release-closeout-20260910.md; release-build/20260910-023528; release-install/20260910-030829; release-smoke/20260910-030858. Work logs, schema-generation scratch files and usage ledgers remain local and untracked.

- Owner authorized committing pending changes, pushing fork/main, syncing upstream and the latest stable release, updating the version, building release, hot swapping, then committing/pushing the closeout. All operations use this existing checkout; no separate worktree.
- Starting HEAD: 9019cd8439; live fork/main: 3a72cbc159. Latest upstream stable release resolved via GitHub API: rust-v0.154.0.
- Preserve the migration LF pins and compatibility test from 9019cd8439. Existing upstream migrations retain their established line-ending policy; do not rewrite applied SQL or database checksums.
- Initial source changes have prior focused validation recorded below. Local usage ledgers and work/ artifacts remain local. Release build must include the CLI, code-mode host and both Windows sandbox sidecars, with -j1 after the prior LLVM OOM.
- Initial commits 899c9e5063 (source) and 9653bf2659 (simulation/status) were pushed successfully to fork/main, including the prior migration-fix commit. Upstream fetched at a62e98d18c; its merge is being resolved in this checkout.
- Concrete migration collision: upstream adds 0055_thread_attachments.sql while the fork already applied 0055_threads_daybreak_enabled.sql. Keep all applied fork IDs/bytes and assign the new attachments migration 0056. No database mutation or checksum rewriting is authorized.
- Latest release has two exclusive commits: cb3a5e4202 is equivalent to upstream 3d3df0a0ca (Guardian limit), and 6b9826e3aa updates the workspace version to 0.154.0. Integrate release ancestry without duplicating the equivalent patch.
- The single selective integration review found an invalid session_source reference and an early image-capability guard that disabled delegated reading; both were corrected during integration. Its remaining blocker is the new attachments migration renaming thread_artifacts while fork artifact APIs still query that table. Fix the new migration to preserve both contracts before release; do not modify applied migrations.
- Integration review fixes are complete: migration 0056 now creates thread_attachments separately and copies existing rows while preserving thread_artifacts; its existing migration test asserts both tables. The generated schemas were replaced safely with mapped-file backups in work/schema-backup-20260910; generated scratch output stays untracked.
- Upstream merge committed as a25402e8b0; stable release/tag and workspace lock version 0.154.0 committed as 6453b494ca. Final push is pending validation and hot swap. The codex-state compilation check passed in 3m22s; focused tests/gate are still pending.
- Owner explicitly requested fast compilation. Supersede the prior mandatory -j1 recipe for future builds: release_ops is preparing a release build without costly LTO/debug data and with moderate parallelism based on available RAM, reusing the existing target directory. No global build configuration change and no extra worktree.
- State validation ran 250 tests: 249 passed, 1 failed, 0 skipped. Migration checksum pins, legacy artifact preservation and artifact/attachment APIs passed. The isolated project query-plan assertion also failed: it requires a covering index, while pre-existing fork visibility/tombstone filters require row access. The query, index migration 0052 and test are unchanged between pre-sync 9653bf2659 and upstream merge a25402e8b0; preserve and report this known failure rather than weaken the test or add an unrelated migration.
- The fork-invariants gate exposed integration compile errors in recovery writer-lock access and six core callsites; bounded fixes adapt the existing APIs. The subsequent codex-core check passed in 11m20s with 13 warnings. The final gate is resuming with CARGO_BUILD_JOBS=2; no more redundant cargo-check cycles are planned.
- Fast release recipe is work/sync-release-20260909/release-build.ps1: release, jobs=2, LTO=false, debug=0, codegen-units=16, opt-level=3, incremental=false, existing target. Build and hot swap have not started.
- Validation budget decision: the broad gate is being ended gracefully under the repository's one-third effort rule and the owner's fast-compilation instruction. Latest completed gate slices: features 1, core 28, core-plugins 1, protocol 9 passed; remaining broad-gate slices are incomplete, never PASS. Keep the known state query-plan failure. Finish scoped source cleanup/formatting and use the required release build as the next compilation authority.
- Final source cleanup: app-server ThreadStartMode delimiter and memories-write Extension filtering are fixed. `just fmt` final exit code 0; nightly-only import grouping warnings are nonfatal. Root diff check passed; old migration SQL remains unchanged. No tests were rerun after final formatting. Release build is now the next step, with remaining broad tests and full clippy not declared passed.
- First fast release build at a05b4b594c ended after 45m12s with E0061 in thread_recovery_processor.rs: the old seven-argument fork_thread_from_history call needed StartThreadOptions. No binaries were installed. The bounded fix preserves config, recovery source, parent trace, MCP extensions and reserved recovered ID; just fmt returned 0. Retry will reuse the completed release dependency/core cache with the same fast flags.
- Second fast release attempt at 769487f266 reused cache and ended after 6m00s on three TUI integration errors. Removed duplicate parent/ancestor fields in named-session lookup and converted fleet-view prompt text into UserMessage. Final formatting returned 0. No install/smoke ran after either failed build; next retry keeps the same flags/cache.

### Unity plugin setup — 2026-09-09 (in progress)

- FINAL ACCEPTANCE: root inspected Desktop task `01a0886c-0441-7263-b78c-4dff6916e319`, title `Consultar versão e status Unity`, via `read_thread`: completed, installed `unity-codex:unity-cli` skill loaded, live `unity status --project-path C:\Users\Joao\UnityProjects\CodexUnitySmoke2 --no-banner` exit 0, ready on port 7800 / Editor 6000.3.23f1 / PID 65248. Final response reports CLI version 1.0.0-beta.9 exit 0. UI test originated in ChatGPT Windows and is backed by local Codex (`kind=codex`), not ChatGPT Web. Together with installed catalog, 31-skill validation, CLI session smoke and reversible Editor mutation, requested desktop integration is verified. No Codex/ChatGPT restart. Prior blocked UI entries below are superseded by successful full-screenshot input and completed desktop task.

- Desktop attempt used correct Windows `@oai/sky` backend and confirmed running ChatGPT app with Codex/ChatGPT mode selector and Plugins button. UI action recovery failed with `call get_window_state before using this window` even after fresh observation (earlier concurrent-input guard also triggered). Desktop ChatGPT smoke remains unverified due automation failure, not plugin absence. Evidence `work/unity-chatgpt-20260909/status.md`. No restarts performed; all Unity runtime acceptance remains passed.

- User clarified target: ChatGPT installed on Windows, NOT ChatGPT Web. `unity_chatgpt` now owns desktop inspection/test; prior web test is outside acceptance. Runtime is complete: browser OAuth reused, Personal license activated, `CodexUnitySmoke2` opened with Pipeline `0.6.0-exp.1`; live marker create/move/readback/restore/delete/save passed. Evidence `work/unity-editor-20260909/runtime-smoke.md`. Desktop ChatGPT validation is the remaining task, with no app restart.

- Latest checkpoint: `unity-codex@personal` installed/enabled at `C:\Users\Joao\plugins\unity-codex`; upstream official install removed only after replacement to avoid duplicate skills. All 31 skills validate; root reviewed the four YAML-only diffs. Fresh ephemeral read-only Codex session selected `unity-codex:unity-cli` and executed Unity CLI version successfully. Evidence: `work/unity-plugin-codex/adaptation.md` and `work/unity-plugin-codex-smoke/result.md`.
- Unity Editor `6000.3.23f1` installed at `C:\Users\Joao\UnityEditors\6000.3.23f1\Editor\Unity.exe` using official installer with native UAC; installed listing and verify pass. Root read installer success log. Project creation exits 198 because license is absent. Login requested via async question; Chrome Unity login tab `626470293`, OAuth CLI process reported PID `73976`. After user login, resume `unity_editor` to activate appropriate license and test Pipeline/scene mutation in an isolated project. Do not mark goal complete: Editor control and ChatGPT surface remain unverified. See `work/unity-editor-20260909/status.md`. No Codex/ChatGPT restart.

- User requests Unity announcement equivalent for ChatGPT/Codex, dependencies installed and tested, without restarting either app.
- Official `Unity-Technologies/unity-agent-plugin` already includes Codex manifest and marketplace; use upstream instead of a duplicate port. Source research reports version `0.1.3-beta`, 31 skills in current tree, and official Codex installation instructions at `https://docs.unity.com/ai/unity-plugin/codex`.
- `unity_plugin` executor owns installation and isolated Unity runtime test. Existing dirty Codex implementation files are outside scope. Skills pickup will be tested in a separate CLI session without restarting existing applications. ChatGPT surface availability and Editor runtime remain unverified.
- Checkpoint: official plugin installed/enabled, verified by root via CLI; Unity CLI `1.0.0-beta.9` installed. Evidence: `work/unity-plugin-evidence-20260909.md`. Upstream has four invalid skill YAML frontmatters; `unity_plugin` now owns a minimal personal compatibility package and fresh-session test. `unity_editor` separately owns official Editor installer failure diagnosis and isolated project test. No Editor currently installed, Unity auth/license inactive, previous install reports `ELEVATION_FAILED` even at a user-owned path. Goal remains incomplete.

### ChatGPT Web continuation status

- RELEASE CLOSEOUT COMPLETE (2026-09-08): isolated commit `3a72cbc159390115a7bc4d44b744e9da1f99bd38` built successfully with `-j1` in 82m26s after the initial LLVM OOM. All four release binaries were installed into the npm vendor bin directory; source/installed SHA-256 values matched. Fresh `codex.ps1 --version` returned `codex-cli 0.153.4`; help exited 0. Existing processes were preserved and retain their old mapped images. Timestamped backups end in `.pre-release-20260908-172913`. Push to `fork/main` succeeded and `git ls-remote` confirmed the exact commit. Logs: `work/release-chatgpt-web-20260908/release-build-j1.log` and `hotswap-20260908-172913.log`.
- Debug cleanup complete for removable cache: removed 49,145,855,028 bytes of dependency cache plus removable top-level debug outputs. Runtime files used by active TUI tests were preserved; the entire debug directory was not deleted. Other dirty workspace changes remain outside this scoped commit/release. All entries below describe historical progress and are superseded by this closeout.

- Release first attempt failed in LLVM with out-of-memory/Allocation failed, followed by E0786 mmap of codex_core metadata. Root verified no remaining cargo/rustc/link process. Scoped retry delegated with `-j1`, same release profile and four binaries, retaining healthy release cache and preserving `release-build.log`. Installed binaries have not been swapped; push remains pending.

- Release authorized by owner: debug cleanup, release, hot swap, commit and push. Scope question received no answer; root stated conservative scope (ChatGPT Web + required dependencies), preserving other dirty work. Local commit `3a72cbc159` contains 15 scoped files; fork/main was equal to prior HEAD. Release builds from detached clean worktree `work/release-chatgpt-web-20260908/source` at that commit, reusing original target/release cache. Package explicitly includes CLI, code-mode host, Windows sandbox setup and command runner. Hot swap/build owned by `release_build_swap`; push waits for successful installation verification. Daemon debug was stopped only while idle; two unrelated/unknown active TUI test binaries are preserved during cache cleanup.

- Final closeout: sidecar-ready probe PASS, CLI/Native exit 0 and turn.completed at 17:51:47 UTC. Root closed owned tab 626469681 after completion, preserving unrelated/unconfirmed tabs. Consent-card appearance was not exercised in this successful probe; consent script regressions passed previously. Effort/model were observed by root as 6 Pro / gpt-6-pro, alongside prior live menu/Chat checks. Final evidence `result-native-sidecars.md`. The prior BLOCKED entries below are historical; Native file-read delivery now passes with the completed isolated debug package.

- FIX VERIFIED: missing Windows sandbox sidecars in the isolated debug package were built (`codex-windows-sandbox-setup.exe`, `codex-command-runner.exe`; build 1m34). Follow-up thread `01a08222-d920-7c82-99ab-ff7ba118d12b`, log `live-native-sidecars.jsonl`, records `command_execution` Get-Content of only the fixture, exit 0, actual marker/7/11/18. Root Chrome MCP confirms `gpt-6-pro` and final MARKER=WEBPRO-20260908-ALPHA / SUM=18 / expected_sum matches, conversation `6aa04a8d-a9b8-83e9-95bf-9d8f3b81c5f9`. This is real Native read success with read-only sandbox unchanged. Earlier assistant security wording was insufficient to diagnose all failures; the missing-helper error was established through router logs and repaired. Waiting only for tester terminal closeout/owned-tab cleanup. No release build/install/hot swap.

- NEW concrete local failure found in network probe: `live-native-network.jsonl` has a real router error at 17:38:09 UTC: unified exec could not launch `codex-windows-sandbox-setup.exe` (`program not found`). Earlier assistant-only security phrases do not establish the cause of all attempts. Debug build compiled only CLI, likely missing sidecars. `web_corrigir_selecao` owns completing the isolated Windows helper build/package without relaxing sandbox; tester will rerun fixture once ready. Thread `01a08218-6eac-7cd1-a32a-fb94531054d0`, owned tab 626469681 retained after terminal completion, network capture stopped.

- DEBUG diagnostic closeout: zero public-server/call/claim-refused events in 17:23:41Z-17:27:24Z (only 3 registry events). Temporary log override removed; daemon default INFO restored, PID 64744 using isolated debug CLI, alive/ready/verified with sessions=0 and active_turns=0. Completed owned probe tab 626469663 was closed; 626469636 has unconfirmed ownership and was preserved along with unrelated tabs. Diagnostic is finished, Native acceptance remains BLOCKED, and no new product patch was justified by this run.

- Authorized DEBUG diagnostic completed at 17:27:24 UTC (started 17:23:41), CLI exit 0 / turn.completed, thread `01a0820c-054d-7d61-a2c7-2e7a1d089409`. Exact public-server filter was applied only to the daemon child. Root Chrome API confirmed gpt-6-pro and tool discovery followed by the security message; no accepted/refused Native call appeared in the diagnostic log, and fixture was not read. Evidence points before Native dispatch but does not identify the classifier rule. No speculative code/permission changes. Root closed completed owned tab 626469663; logging restoration is underway. Evidence: `work/chatgpt-web-validation-20260908/native-debug-diagnosis.md` and `result-native-debug.md`.

- User authorized one Native diagnostic attempt with public-server DEBUG logging to determine whether the call reaches Codex. Consent executor owns the run and temporary logging; shared sessions must remain intact. Previous assistant security text is not by itself proof of an external rejection. No annotation/schema patch is authorized by evidence yet.

- Cleanup completed by root through Chrome MCP after `turn.completed`/CLI exit 0: closed only owned tab 626469584; browser list confirms unrelated ChatGPT 626469566 and Home Assistant/Headroom tabs preserved. Native remains blocked as recorded below.

- Final directed Native probe: `live-native-final.jsonl` records `turn.completed` and the literal response "Esta ferramenta foi bloqueada pelas configuracoes de seguranca da OpenAI. Verifique novamente o que esta enviando." Root authenticated Chrome MCP API read confirmed `gpt-6-pro`, discovery `api_tool.list_resources` for Codex_Native/codex_exec, and no demonstrated fixture read in conversation `6aa03ec3-9864-83e9-a4da-6e14a58fa66e`. Native live acceptance remains BLOCKED, not PASS; no speculative permission changes or further retry. Selection/Chat/effort and response delivery are verified live; long-state behavior passed focused regressions, not a >20-minute live run. Debug build only; release/hot swap not performed. Final owned-tab cleanup is assigned to tester.

- Updated debug build passed (3m56s). Readiness probe now sends and completes; root Chrome MCP observed backend `gpt-6-pro`, UI 6 Pro, conversation `6aa039cd-b94c-83e9-b5c9-e0c19c17dc71`. Fixture acceptance remains BLOCKED: answer reports file read blocked by security settings (MARKER/SUM unavailable). Consent author is attributing the exact tool error before retry. Owned tab 626469584 is retained for diagnosis; unrelated 626469566 preserved. Logs: `live-corrected-readiness.jsonl`, `result-corrected-readiness.md`.

- Composer follow-up: added bounded readiness after Chat/Work preflight and a 250 ms stable editor/form requirement. Focused driver gate passed 68/68 (4184 outside-filter skips), formatter exit 0, diff check passed. Isolated CLI rebuild and final Native fixture probe are now delegated to `web_runtime_probe`; failed `live-corrected.jsonl` is preserved.

- Latest continuation: focused tests passed 311/311; scoped fix and isolated debug CLI build passed; tester confirmed `just fmt` exit 0 (`fmt.log` is empty because formatter emitted no output). Corrected CLI live probe failed before sending: composer absent at compose, although owned tab 626469584 later exposes `#prompt-textarea` and shows 6 Pro. `web_corrigir_selecao` is investigating the readiness transition; final live Native consent/response/cleanup remains pending. No release installation or hot swap performed.

- User resumed the unfinished work after an intentional interruption. `web_lifecycle_fix` resumed generation 1 with its existing edits preserved; final tests/build/live probe are still pending.

- Chrome MCP remains the browser tool, per explicit user instruction; prepared tab 626469277 is still Chat / Extra alto.
- Package 1 is under correction: explicit selection on continuation and missing model confirmation were identified during root inspection and sent to its author.
- Package 1 remains with `web_corrigir_selecao` (driver ops/page scripts/tests only). Package 2 was reassigned to `web_lifecycle_fix` (stream/mod/tabs) to remove the sequential bottleneck. Consent scripts/tests are complete with a 300 ms stable-absence postcondition; runtime validation is pending.
- Rust tests, scoped fix/fmt, isolated CLI build and final live validation remain pending. No release build or hot swap has been performed in this task.
- Current generated selector scripts were exercised through Chrome MCP on prepared tab 626469277 without sending a message: Pro 5/5 confirmed; manual GPT-5.6 Sol selection was corrected back to Recente by the script; Extra alto 4/5 restored. The current Chat-mode script also changed Work back to Chat and confirmed it on the same URL. End-to-end CLI validation remains pending.
- Lifecycle implementation is now complete, including preservation of recoverable bindings, reclaiming the last unbound idle tab, and `AgentActivityHandle` wired from session turn into the existing agent activity registry. The single tester started `just test -p codex-core -E "test(/chatgpt_web/)"`; results, fix/fmt, build and live CLI validation are pending.
- Final focused rerun 5 passed: 311 tests run, 311 passed, 3940 outside-filter skips. Earlier compile/fixture failures and the real sweeper idle-timestamp bug were corrected. Canonical result log: `work/chatgpt-web-validation-20260908/chatgpt-web-focused-rerun-5.log`. Scoped fix/fmt, isolated CLI build and live verification follow.

## 2026-09-08: ChatGPT Web validation requested

- Scope: Recente/Astra model, effort, long Pro waits, tab lifecycle, Codex Native consent and Chat versus Work.
- Code inspection confirmed fail-open effort/model checking, absent Chat/Work preflight, optimistic consent clicks, independent wait/provider clocks and pooled-tab cleanup gaps. The exact live cause remains unverified.
- Chrome MCP now works after the owner's explicit request. It confirmed same-URL Chat/Work switching and a real Extra alto 4/5 -> Pro 5/5 -> Extra alto change. The user-prepared tab was restored to Chat.
- The installed CLI probe completed a real Native fixture read and delivered the answer, but selected GPT-5.6 Sol/Pro (`gpt-5-6-pro`) rather than Recente. Root closed only the owned test tab after completion; logs and the server conversation were preserved.
- One design consultation by claude-fable reviewed the scoped correction. Task 1 (selection/preflight) is being implemented by `web_corrigir_selecao`; long Pro lifecycle and consent confirmation follow. No release/hot swap is included.
- Plan/evidence: `agent_docs/chatgpt-web-fix-plan-20260908.md`, `agent_docs/chatgpt-web-validation-20260908.md` and `work/chatgpt-web-validation-20260908/`. The first probe was short (UI reported 32s), so it does not certify the old long-watchdog boundary.

## 2026-09-07: authorized usage improvements

- Owner approved the recommendations and explicitly requested removal of Lemma: "lemma não utilizo, teria que remover mesmo, o restante faça ai."
- Scope: Astra Standard with Ultra preserved and Luna Fast retained; consolidate global/config/role prompts and scope SurfTank launcher instructions; tighten personal skills and remove Lemma integration; improve wait targets/deltas and event-driven waiting; add explicit delegation context; account, bound and reuse optional image-reader output while preserving raw-image access.
- Initial Git state: only this status document and the usage-audit artifacts were dirty. No production changes were present.
- `config_prompts` owns the local configuration/instruction/skill package. `revisar_desenho` reviews the fork design read-only; `mapear_fork` maps source ownership and existing tests. Root owns coordination, Git inspection and this status document.
- Validation: existing focused repository runners, one final scoped lint/format pass; no new benchmark harness, runtime replacement, release, global suite or memory-store deletion is included. Fork implementation packages and their exact focused commands will follow the design review.
- Design review complete (Fable, one round): keep terminal/needs-attention and hard-limit escape paths in event waiting; preserve strict target matching and immediate user steering; return latest-state deltas, not a fictional event history; populate existing collaboration UI events; record reader usage independently of primary context counters; overflow falls back to raw pixels.
- Implementation packages: 1) local settings/prompts/skills; 2) wait event mode, target deltas and existing UI state; 3) additive explicit delegation brief with bounded context and legacy fork compatibility; 4) optional image-reader accounting, bounded output and deduplication. Validation uses focused existing test modules/suite infrastructure, then one scoped lint/format pass. Builds are serialized after implementation.
- Current state: all four implementation packages are complete. The 65 focused core tests are validated across focused executions (64 passed together, then the corrected delegation integration passed alone), plus two history/rollout tests. `just fix -p codex-core` and `just fmt` completed successfully; no tests were rerun afterward. Remaining lint warnings are recorded without claiming a clean baseline.
- Validacao (fork): existing `just test` runner with a single nextest filter using `+` without spaces, followed by `just fix -p codex-core` and `just fmt`. The exact filter, resolved fixture/compilation failures and per-run results are in `usage-audit-20260907/validation.md`. No full-workspace suite or replacement of the running binary is included.
- No optimization saving or active-runtime adoption has been measured.
- Package 1 complete: root remains Astra/Ultra with `service_tier = "default"`; Luna roles retain Priority/Max. Lemma skill was removed without deleting memory stores. Global/config/role instructions were consolidated, Godot launch rules remain in the SurfTank checkout, and the two personal skill triggers were narrowed. TOML parsing and skill validation passed; details are in `config-changes.md`.
- Wait package complete and focused tests passed: cursor deltas omit unchanged targets, mixed stale-completed/running targets keep waiting, and stale terminal snapshots are included only for a real all-final escape. Targeted regressions cover these cases.
- Delegation package complete and focused tests passed: optional `task_context` is a typed user fragment capped at 768 UTF-8 bytes; omission of `fork_turns` with a brief selects task-only context, while legacy calls and explicit fork choices remain unchanged. Existing whole-payload redaction covers the new field. Integration coverage is in `core/tests/suite/agent_execution.rs`; details are in `delegation-changes.md`.
- Reader final diff found cancellation and completeness defects: an aborted leader could leave cache waiters blocked, keys retained full image data, and EOF without Completed could cache partial text. The first executor's follow-up report did not apply these fixes (verified directly in source). Core/cache/test ownership moved to `corrigir_cache` (Sonnet, account 1) for this bounded correction; history/rollout remain unchanged under validation.
- `validacao_fork` is the sole Rust-runner owner. Focused history/rollout tests passed: `rollout_item_variants_preserve_existing_payload_shapes` (1/1) and `image_reader_usage_is_persisted_as_a_rollout_extension` (1/1). Logs and commands are recorded in `validation.md`.
- Reader integration passed in `core/tests/suite/image_reader_accounting.rs`: equivalent images within and across turns share one auxiliary request, its usage is recorded once, and primary-model accounting remains separate.
- Reader cache corrections and unit tests passed: leader-owned watch sender releases waiters on cancellation; completed/in-flight caches are bounded; digest keys avoid retaining full images; EOF without Completed falls back to raw and is not cached.
- Final cleanup removed formatter-only changes outside the package and preserved concurrent work. Final `git diff --check` passed. Local settings are saved; Rust changes remain in the working tree and the installed/running binary was not replaced.

## 2026-09-07: usage audit requested by owner

- Scope: all accessible conversations with activity from 2026-09-04 19:00 BRT (22:00 UTC) through 2026-09-07 20:48:03 BRT (23:48:03 UTC). The owner explicitly selected the Friday 19:00 start. Audit-generated work is excluded.
- Deliverables: request/session/root/model usage ledger, conversation outcomes and avoidable work, source-grounded recommendations for lower subscription consumption and faster delivery with preserved quality. Analysis only; no production/config/runtime changes.
- Collection includes active and archived rollouts; repeated counters, inherited history, synthetic context-full counters and provider differences must be handled. Subscription quota and token totals are separate measurements.
- Native agents: `contabilidade` owns accounting data/scripts; `conversas` owns activity extraction and conversation review; `codigo` inspects current source; Fable reviewed the accounting methodology once.
- Current read-only evidence: Codex quota snapshot reports 86% weekly used; installed executable reports 0.153.4 although npm package metadata still says 0.146.1. Current checkout HEAD is bba1cd73a8. Config currently selects gpt-6-astra, ultra, priority; these facts do not establish every historical request's settings or current process commit.
- Final findings are in `agent_docs/usage-audit-20260907/report-final.md`; `report.md` is the earlier draft. Supporting data: `accounting.md`, `conversation_review.md`, and response-level `ledger_requests.csv/json`. Known-model GPT rows total 6,006,271,362 tokens across 30,919 responses and 165 threads; another OpenAI row has 617,158 tokens with no model attribution. Cached input is 97.9162% of known-model GPT input. Claude and estimated ChatGPT Web usage are separate.
- Under published Standard credit weights (comparison only, not billed quota), Astra represents 92.195% of the known-model GPT total. The main opportunities are fewer non-actionable root inference cycles, bounded target/delta wait results, task-specific fork context, existing build admission reuse, and accounting/bounding of the optional image reader. No implementation or configuration change was made.
- Three quota regimes were observed (61 to 100%, 0 to 100%, 0 to 86%); the cause of the two transitions is not attributed. Auxiliary GPT records were classified as one CLI synchronization smoke and 18 CLI image probes. Conflicting duplicate records and incomplete cumulative reconciliation remain explicit accounting limits; no complete subscription invoice or measured optimization gain is claimed.
- Historical workflow status below is preserved and must not be mistaken for the runtime state verified in this audit.

## Objective

Implement the approved durable-workflow program in milestone order: P0 cross-provider integrity and immutable recovery; P1 workflow/jobs/search/plans/evidence; P2 fleet/ownership/migration/context inspection.

## Current milestone

- P0 is complete after one milestone review and its required fixes.
- P1.1 provenance/fork invariants and P1.2 workflow database foundation are complete.
- P1.3 transient jobs/indexed navigation, P1.4 approved-plan contracts, and P1.5 receipts/evidence are complete with focused gates.
- P2.1 durable mailbox/fleet and P2.2 ownership enforcement are implemented with focused gates; their final cross-crate gates are paused.
- 2026-09-03: P2.2 workspace ownership (path leases, `grant/release/override_agent_ownership`, `workspaceLease/*`, `[features.workspace_ownership]`, role mutation capabilities) was removed after a measured audit: 3,538 leases in one thread, 83% on the checkout root, 603 wait timeouts, zero real same-file collisions prevented. Kept: subagent destructive-Git denial (exec and Claude Bash) and build admission. The `workflow_path_leases` tables stay in the migrations but nothing writes them.
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
