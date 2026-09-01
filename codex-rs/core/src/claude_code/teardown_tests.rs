use super::*;
use crate::claude_code::accounts;
use crate::claude_code::control::ControlChannel;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

fn helper_child() -> Child {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "more > NUL"]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null"]);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn teardown helper")
}

fn already_exited_child() -> Child {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "exit 0"]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn exited teardown helper")
}

fn child_that_ignores_eof() -> Child {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "more > NUL & ping 127.0.0.1 -n 30 > NUL"]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", "cat >/dev/null; sleep 30"]);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn non-exiting teardown helper")
}

fn writer(
    child: &mut Child,
    observed_eof: Arc<AtomicBool>,
) -> (mpsc::Sender<String>, tokio::task::JoinHandle<()>) {
    let stdin = child.stdin.take().expect("helper stdin");
    let (tx_stdin, mut rx_stdin) = mpsc::channel(8);
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(line) = rx_stdin.recv().await {
            if stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = stdin.shutdown().await;
        observed_eof.store(true, Ordering::Release);
    });
    (tx_stdin, writer)
}

async fn running_turns(account: &std::path::Path) -> usize {
    accounts::list_accounts(&[account.to_path_buf()], None, false)
        .await
        .into_iter()
        .next()
        .expect("one account")
        .running_turns
}

#[tokio::test]
async fn fork_invariant_claude_teardown_closes_senders_and_reaps_process() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;
    let guard = accounts::InFlightGuard::acquire(Some(&account));
    assert_eq!(running_turns(&account).await, baseline + 1);

    let mut child = helper_child();
    let observed_eof = Arc::new(AtomicBool::new(false));
    let (tx_stdin, writer) = writer(&mut child, Arc::clone(&observed_eof));
    let control = ControlChannel::new(tx_stdin.clone());
    let state = finish(
        &mut child,
        Some(control),
        tx_stdin,
        writer,
        TerminationRequest::WaitForExit,
    )
    .await
    .expect("graceful teardown");
    drop(guard);

    assert!(state.control_present);
    assert!(state.control_closed);
    assert!(state.stdin_sender_closed);
    assert!(state.writer_finished);
    assert!(!state.writer_timed_out);
    assert!(state.child_waited);
    assert!(state.child_exited);
    assert!(observed_eof.load(Ordering::Acquire));
    assert_eq!(running_turns(&account).await, baseline);
    assert!(child.try_wait().expect("child status").is_some());
}

#[tokio::test]
async fn explicit_cancellation_reaps_the_child_and_releases_in_flight_guard() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;
    let guard = accounts::InFlightGuard::acquire(Some(&account));

    let mut child = helper_child();
    let observed_eof = Arc::new(AtomicBool::new(false));
    let (tx_stdin, writer) = writer(&mut child, Arc::clone(&observed_eof));
    let control = ControlChannel::new(tx_stdin.clone());
    let state = finish(
        &mut child,
        Some(control),
        tx_stdin,
        writer,
        TerminationRequest::ExplicitCancellation,
    )
    .await
    .expect("explicit cancellation teardown");
    drop(guard);

    assert!(state.explicit_cancellation);
    assert!(state.writer_finished);
    assert!(state.child_waited);
    assert!(state.child_exited);
    assert_eq!(running_turns(&account).await, baseline);
    assert!(child.try_wait().expect("child status").is_some());
}

#[tokio::test]
async fn early_child_exit_still_reaps_and_releases_in_flight_guard() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;
    let guard = accounts::InFlightGuard::acquire(Some(&account));

    let mut child = already_exited_child();
    tokio::task::yield_now().await;
    let observed_eof = Arc::new(AtomicBool::new(false));
    let (tx_stdin, writer) = writer(&mut child, Arc::clone(&observed_eof));
    let control = ControlChannel::new(tx_stdin.clone());
    let state = finish(
        &mut child,
        Some(control),
        tx_stdin,
        writer,
        TerminationRequest::WaitForExit,
    )
    .await
    .expect("early-exit teardown");
    drop(guard);

    assert!(state.child_waited);
    assert!(state.child_exited);
    assert!(observed_eof.load(Ordering::Acquire));
    assert_eq!(running_turns(&account).await, baseline);
    assert!(child.try_wait().expect("child status").is_some());
}

#[tokio::test]
async fn fork_invariant_claude_provider_errors_release_in_flight_guard() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;

    for provider_outcome in ["529", "error"] {
        let guard = accounts::InFlightGuard::acquire(Some(&account));
        assert_eq!(
            running_turns(&account).await,
            baseline + 1,
            "{provider_outcome}"
        );
        drop(guard);
        assert_eq!(
            running_turns(&account).await,
            baseline,
            "{provider_outcome}"
        );
    }
}

#[tokio::test]
async fn writer_timeout_is_bounded_and_returns_process_control_state_without_kill() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;
    let guard = accounts::InFlightGuard::acquire(Some(&account));

    let mut child = helper_child();
    let stdin = child.stdin.take().expect("helper stdin");
    let (tx_stdin, _rx_stdin) = mpsc::channel(8);
    let control = ControlChannel::new(tx_stdin.clone());
    let writer = tokio::spawn(async move {
        let _stdin = stdin;
        std::future::pending::<()>().await;
    });

    let started = tokio::time::Instant::now();
    let error = finish(
        &mut child,
        Some(control),
        tx_stdin,
        writer,
        TerminationRequest::WaitForExit,
    )
    .await
    .expect_err("writer timeout must be visible");
    drop(guard);

    assert_eq!(error.failure, TeardownFailure::WriterTimedOut);
    assert!(error.state.control_present);
    assert!(error.state.control_closed);
    assert!(error.state.stdin_sender_closed);
    assert!(error.state.writer_timed_out);
    assert!(error.state.writer_aborted);
    assert!(!error.state.explicit_cancellation);
    assert!(error.state.child_waited);
    assert!(error.state.child_exited);
    assert!(started.elapsed() >= WRITER_TEARDOWN_TIMEOUT);
    assert!(child.try_wait().expect("child status").is_some());
    assert_eq!(running_turns(&account).await, baseline);
    let rendered = error.to_string();
    assert!(rendered.contains("writer_timed_out=true"));
    assert!(rendered.contains("child_waited=true"));
}

#[tokio::test]
async fn child_wait_timeout_is_bounded_and_marks_needs_attention() {
    let temp = tempfile::tempdir().expect("tempdir");
    let account = temp.path().join("account");
    std::fs::create_dir_all(&account).expect("account dir");
    let baseline = running_turns(&account).await;
    let guard = accounts::InFlightGuard::acquire(Some(&account));

    let mut child = child_that_ignores_eof();
    let observed_eof = Arc::new(AtomicBool::new(false));
    let (tx_stdin, writer) = writer(&mut child, Arc::clone(&observed_eof));
    let control = ControlChannel::new(tx_stdin.clone());
    let started = tokio::time::Instant::now();
    let error = finish_with_timeouts(
        &mut child,
        Some(control),
        tx_stdin,
        writer,
        TerminationRequest::WaitForExit,
        Duration::from_millis(100),
        Duration::from_millis(100),
    )
    .await
    .expect_err("a child that ignores EOF must be reported");
    drop(guard);

    assert_eq!(error.failure, TeardownFailure::ChildWaitTimedOut);
    assert!(error.state.control_closed);
    assert!(error.state.stdin_sender_closed);
    assert!(error.state.writer_finished);
    assert!(error.state.child_wait_timed_out);
    assert!(error.state.needs_attention);
    assert!(!error.state.child_waited);
    assert!(!error.state.child_exited);
    assert!(observed_eof.load(Ordering::Acquire));
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(child.try_wait().expect("child status").is_none());
    assert_eq!(running_turns(&account).await, baseline);

    // The timeout does not kill implicitly. Cancellation remains the explicit
    // path and is still responsible for terminating and reaping the helper.
    cancel_process_tree(&mut child).await;
    let _ = child.wait().await;
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct RecordingTreeKillBackend {
    steps: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

#[cfg(windows)]
impl ProcessTreeKillBackend for RecordingTreeKillBackend {
    fn kill_tree<'a>(&'a self, _pid: u32) -> futures::future::BoxFuture<'a, ()> {
        let steps = Arc::clone(&self.steps);
        Box::pin(async move {
            steps.lock().expect("steps lock").push("taskkill-tree");
        })
    }

    fn kill_direct(&self, child: &mut Child) {
        self.steps.lock().expect("steps lock").push("direct-child");
        let _ = child.start_kill();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_tree_kill_runs_before_direct_child_kill() {
    let mut child = helper_child();
    let steps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let backend = RecordingTreeKillBackend {
        steps: Arc::clone(&steps),
    };

    cancel_process_tree_with(&mut child, &backend).await;

    assert_eq!(
        *steps.lock().expect("steps lock"),
        vec!["taskkill-tree", "direct-child"]
    );
    let _ = child.wait().await;
}
