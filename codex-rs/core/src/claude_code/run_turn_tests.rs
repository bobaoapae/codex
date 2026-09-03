//! FORK: the turn loop driven against a scripted CLI.
//!
//! Anthropic 5xx failures are not reproducible on demand, and they cost five
//! whole turns in one afternoon, so the retry path is covered here instead: a
//! fake `claude` that serves canned JSONL — one file per invocation — and logs
//! the argv it was called with.

use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// The `system` + flagged `assistant` + error `result` sequence the CLI emits
/// for an Anthropic-side failure. The error text arrives as assistant content
/// and in `errors[]`; `result` itself is empty.
fn overloaded_response(session_id: &str) -> String {
    let frames = [
        serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
        }),
        serde_json::json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "error": "overloaded",
            "session_id": session_id,
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "API Error: 529 overloaded_error" }],
            },
        }),
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "session_id": session_id,
            "errors": ["API Error: 529 overloaded_error"],
        }),
    ];
    render(&frames)
}

fn success_response(session_id: &str, text: &str) -> String {
    let frames = [
        serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
        }),
        serde_json::json!({
            "type": "assistant",
            "session_id": session_id,
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": text }],
            },
        }),
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "session_id": session_id,
            "result": text,
        }),
    ];
    render(&frames)
}

fn render(frames: &[serde_json::Value]) -> String {
    frames
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A scripted `claude`: the nth invocation prints `resp<n>.jsonl` if it exists
/// and `respdefault.jsonl` otherwise, appending its argv to `args.log` first.
struct FakeClaude {
    home: TempDir,
    command: Vec<String>,
}

impl FakeClaude {
    fn new(responses: &[String], default_response: &str) -> Self {
        let home = TempDir::new().expect("fake claude tempdir");
        for (index, response) in responses.iter().enumerate() {
            std::fs::write(
                home.path().join(format!("resp{}.jsonl", index + 1)),
                response,
            )
            .expect("write scripted response");
        }
        std::fs::write(home.path().join("respdefault.jsonl"), default_response)
            .expect("write default response");

        #[cfg(unix)]
        let command = {
            let script = home.path().join("fake_claude.sh");
            std::fs::write(
                &script,
                r#"#!/bin/sh
dir=$(dirname "$0")
echo "$@" >> "$dir/args.log"
n=1
while [ -e "$dir/used$n" ]; do n=$((n+1)); done
: > "$dir/used$n"
if [ -f "$dir/resp$n.jsonl" ]; then
  cat "$dir/resp$n.jsonl"
else
  cat "$dir/respdefault.jsonl"
fi
"#,
            )
            .expect("write fake claude script");
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script)
                .expect("script metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).expect("make the script executable");
            vec![script.to_string_lossy().into_owned()]
        };

        #[cfg(windows)]
        let command = {
            let script = home.path().join("fake_claude.cmd");
            std::fs::write(
                &script,
                "@echo off\r\n\
                 setlocal EnableExtensions EnableDelayedExpansion\r\n\
                 set \"DIR=%~dp0\"\r\n\
                 >>\"!DIR!args.log\" echo %*\r\n\
                 set /a N=0\r\n\
                 :next\r\n\
                 set /a N+=1\r\n\
                 if exist \"!DIR!used!N!\" goto next\r\n\
                 type nul > \"!DIR!used!N!\"\r\n\
                 if exist \"!DIR!resp!N!.jsonl\" (\r\n\
                 type \"!DIR!resp!N!.jsonl\"\r\n\
                 ) else (\r\n\
                 type \"!DIR!respdefault.jsonl\"\r\n\
                 )\r\n\
                 exit /b 0\r\n",
            )
            .expect("write fake claude script");
            vec![
                "cmd.exe".to_string(),
                "/D".to_string(),
                "/Q".to_string(),
                "/C".to_string(),
                script.to_string_lossy().into_owned(),
            ]
        };

        Self { home, command }
    }

    /// The argv of each invocation so far, in order.
    fn invocations(&self) -> Vec<String> {
        let Ok(log) = std::fs::read_to_string(self.home.path().join("args.log")) else {
            return Vec::new();
        };
        log.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn workspace(
    fake: &FakeClaude,
    cwd: &Path,
    delays: &'static [std::time::Duration],
) -> ClaudeCodeWorkspace {
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(cwd)
        .expect("an absolute cwd");
    ClaudeCodeWorkspace {
        cwd_uri: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        cwd: cwd.to_path_buf(),
        extra_roots: Vec::new(),
        permission_mode: "bypassPermissions",
        account_dirs: Vec::new(),
        accounts_state_path: None,
        sessions_state_path: None,
        selection: ClaudeCodeAccountSelection::default(),
        sticky_min_headroom_pct: 0.0,
        pinned_account: None,
        idle_timeout: None,
        transient_retry_delays: delays,
        claude_command: Some(fake.command.clone()),
        developer_instructions: None,
        // The scripted CLI answers no control requests, and the turn does not
        // need one: the system prompt only matters to a real agent.
        control_protocol: false,
        stream_partial_messages: false,
        sandbox: SandboxPolicy::DangerFullAccess,
        writable_roots: Vec::new(),
        host: None,
        ownership_notice: None,
    }
}

fn user_input(text: &str) -> Vec<ResponseItem> {
    vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]
}

/// Runs one turn to completion and returns everything it emitted.
async fn drive(workspace: ClaudeCodeWorkspace) -> Vec<Result<ResponseEvent>> {
    let (tx_event, mut rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event);
        }
        events
    });
    run_turn(
        user_input("do the thing"),
        "claude-opus-5".to_string(),
        None,
        workspace,
        Arc::new(ClaudeCodeThreadState::default()),
        tx_event,
        consumer_dropped,
    )
    .await;
    collector.await.expect("collector")
}

fn messages(events: &[Result<ResponseEvent>]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })) => Some(
                content
                    .iter()
                    .filter_map(|item| match item {
                        ContentItem::OutputText { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

fn failure(events: &[Result<ResponseEvent>]) -> Option<String> {
    events.iter().find_map(|event| match event {
        Err(err) => Some(err.to_string()),
        _ => None,
    })
}

/// Retries with no wait: the pause is what the delays are for, not the logic.
const NO_PAUSE: &[std::time::Duration] = &[std::time::Duration::ZERO, std::time::Duration::ZERO];

/// FORK: five turns died in twelve hours because a 529 was reported as an
/// assistant message and then dropped the turn. It is retried on the same
/// account now, resuming the session the failed attempt opened, and the error
/// text never reaches the Codex transcript as the agent's own words.
#[tokio::test]
async fn an_anthropic_server_error_is_retried_in_place_on_the_same_session() {
    let fake = FakeClaude::new(
        &[
            overloaded_response("sess-1"),
            success_response("sess-1", "ok"),
        ],
        &success_response("sess-1", "ok"),
    );
    let cwd = TempDir::new().expect("cwd tempdir");
    let events = drive(workspace(&fake, cwd.path(), NO_PAUSE)).await;

    assert_eq!(failure(&events), None, "{events:?}");
    assert_eq!(messages(&events), vec!["ok".to_string()], "{events:?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ResponseEvent::Completed { .. }))),
        "{events:?}"
    );

    let invocations = fake.invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    assert!(
        !invocations[0].contains("--resume"),
        "the first attempt starts a session: {invocations:?}"
    );
    assert!(
        invocations[1].contains("--resume sess-1"),
        "the retry resumes the failed attempt's session: {invocations:?}"
    );
}

/// FORK: the retry budget is two extra attempts, and a server error is not an
/// account problem -- there is nothing to fail over to and nothing to blame the
/// account for.
#[tokio::test]
async fn a_persistent_server_error_fails_after_three_attempts_without_failing_over() {
    let overloaded = overloaded_response("sess-1");
    let fake = FakeClaude::new(&[], &overloaded);
    let cwd = TempDir::new().expect("cwd tempdir");
    let events = drive(workspace(&fake, cwd.path(), NO_PAUSE)).await;

    let message = failure(&events).expect("the turn must fail");
    assert!(message.contains("after 3 attempts"), "{message}");
    assert!(message.contains("Anthropic server error"), "{message}");
    assert_eq!(fake.invocations().len(), 3);
    // The error text is the CLI's, not the agent's.
    assert_eq!(messages(&events), Vec::<String>::new(), "{events:?}");
}

/// A pause no test should ever wait out.
const LONG_PAUSE: &[std::time::Duration] = &[std::time::Duration::from_secs(120)];

/// FORK: the pause between attempts must not outlive the consumer. A cancelled
/// turn stops there instead of spawning the CLI again minutes later.
#[tokio::test]
async fn cancelling_during_the_retry_pause_does_not_spawn_the_cli_again() {
    let overloaded = overloaded_response("sess-1");
    let fake = FakeClaude::new(&[], &overloaded);
    let cwd = TempDir::new().expect("cwd tempdir");
    let (tx_event, _rx_event) = mpsc::channel(EVENT_CHANNEL_SIZE);
    let consumer_dropped = CancellationToken::new();
    let cancel = consumer_dropped.clone();

    let turn = tokio::spawn(run_turn(
        user_input("do the thing"),
        "claude-opus-5".to_string(),
        None,
        workspace(&fake, cwd.path(), LONG_PAUSE),
        Arc::new(ClaudeCodeThreadState::default()),
        tx_event,
        consumer_dropped,
    ));

    // Wait for the first attempt to fail and the pause to begin.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while fake.invocations().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        fake.invocations().len(),
        1,
        "the first attempt must have run"
    );
    cancel.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("a cancelled turn must not wait out the retry pause")
        .expect("turn task");
    assert_eq!(fake.invocations().len(), 1);
}
