# ChatGPT Web — scoped correction

Basis: user-reported selector, Pro wait, consent and abandoned-tab problems; source diagnosis; one design consultation by claude-fable; Chrome MCP observations in `work/chatgpt-web-validation-20260908/browser-observations.md`.

Live evidence: the installed `chatgpt-web/pro` probe rendered GPT-5.6 Sol checked, Recente unchecked, Pro 5/5, and assistant `data-message-model-slug=gpt-5-6-pro`. Chat/Work share the same URL. Explicit focus plus keyboard changed effort reliably; some click/press calls returned success without effect.

## 1. Model, effort and mode before sending

Keep public `chatgpt-web/*` IDs compatible. Named effort levels use the current Recente model choice rather than the hardcoded old family; explicit legacy slugs retain explicit meaning. Read the checked model radio and effort after selection in the same tab. Fail before sending when an explicit requested choice cannot be confirmed. Auto retains observed-state provenance rather than inventing identity. If the Chat/Work control is present, ensure Chat; an absent control on legacy UI is not itself evidence of Work. No unconditional guessed GPT-6 backend slug.

Validacao: existing `just test -p codex-core` filtered to ChatGPT Web driver selectors/ops and new focused regressions; final live check in task 3.

## 2. Long Pro response lifecycle

Address both the 600-second completion heuristic and idle-watchdog cancellation. Before deciding a silent generation ended or stalled, refresh the owned tab and re-read authoritative state. An active generation must not be reported as a completed answer merely because a timer elapsed. Preserve bounded recovery and the conversation identity; explicit user cancellation remains cancellation. Keep activity reporting internal, without injecting heartbeat text into model context. Close only idle tabs owned by the finished operation; preserve shared/active generations and recovery affinity.

Validacao: existing stream/tabs tests with targeted regressions for active async generation past the old threshold, watchdog recheck and ownership; no sleep-based repetition matrix.

## 3. Connector consent and final live acceptance

Use effective DOM actions and verify the consent postcondition; distinguish consent still pending from accepted or failed. Preserve Codex execution approvals as a separate authorized step. Change broker timeout/lifecycle only where the real probe identifies the failure; do not globally increase timeout as a substitute for state. Then run the focused affected tests, scoped fix/fmt and one isolated CLI build for real validation. Inspect exact resulting model/effort, Native file read and response delivery, and finish ownership cleanup without abandoning test conversations.

Validacao: existing connector tests plus necessary directed regression(s), scoped `just fix -p codex-core`, `just fmt`, one CLI build using the repository's existing build path, and one final real Chrome MCP/CLI run with the non-sensitive fixture. No workspace-wide test suite, release installation or hot swap is included.

Long logs belong under `work/chatgpt-web-validation-20260908/`. PASS, FAIL, SKIP and NotExecuted remain distinct. A short successful Pro response does not certify a response longer than the previous watchdog; the long-state behavior also needs its focused regression.

## 4. Authorized release closeout

The owner subsequently requested debug-cache cleanup, release build, hot swap, commit and push. Preserve unrelated dirty work; default release/commit scope is this ChatGPT Web correction and its necessary dependencies. Build from the committed source snapshot with CLI, code-mode host and Windows sandbox setup/command-runner sidecars. Clean only verified workspace debug outputs, preserve active unrelated runtimes, and back up installed binaries before replacement. Verify a new invocation; existing processes retain their mapped image. Push to the user's fork without force.

Validacao: existing release build command for the four binaries, version/help checks and installed-artifact provenance; previously completed focused tests are not repeated without a new code change or failure.
