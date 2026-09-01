//! Bounded teardown for a Claude Code child and its control writer.
//!
//! A turn has two independent ownership edges into stdin: the turn-local sender
//! and the [`ControlChannel`] sender. Both must be closed before the writer can
//! observe EOF. This module owns that ordering and keeps cancellation explicit so
//! a writer timeout is visible to the caller instead of being silently treated as
//! permission to kill a provider process.

use super::control::ControlChannel;
#[cfg(windows)]
use futures::future::BoxFuture;
use serde_json::json;
use std::fmt;
#[cfg(windows)]
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Child;
#[cfg(windows)]
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum time allowed for the stdin writer to observe channel closure and
/// finish its final flush/shutdown.
pub(crate) const WRITER_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time allowed for the CLI to exit after its output stream and stdin
/// have both reached EOF. A provider that ignores EOF is reported as needing
/// attention; normal teardown never escalates to a kill on this timeout.
pub(crate) const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How the child should be handled once the stream has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminationRequest {
    /// Close stdin and wait for the CLI's normal exit.
    WaitForExit,
    /// The consumer explicitly stopped the turn; terminate the process tree and
    /// then wait so no helper child is left behind.
    ExplicitCancellation,
}

/// Process and control ownership observed while tearing down one attempt.
///
/// These fields intentionally contain only lifecycle state. No command, control
/// payload, stderr, or provider content is retained in a teardown error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessControlState {
    pub(crate) control_present: bool,
    pub(crate) control_closed: bool,
    pub(crate) stdin_sender_closed: bool,
    pub(crate) control_send_timed_out: bool,
    pub(crate) writer_finished: bool,
    pub(crate) writer_aborted: bool,
    pub(crate) writer_timed_out: bool,
    pub(crate) explicit_cancellation: bool,
    pub(crate) child_waited: bool,
    pub(crate) child_exited: bool,
    /// The child did not exit within [`CHILD_WAIT_TIMEOUT`].
    pub(crate) child_wait_timed_out: bool,
    /// A coordinator should surface this state and let an explicit cancellation
    /// decide whether the still-running child is terminated.
    pub(crate) needs_attention: bool,
}

impl ProcessControlState {
    fn new(control_present: bool) -> Self {
        Self {
            control_present,
            control_closed: false,
            stdin_sender_closed: false,
            control_send_timed_out: false,
            writer_finished: false,
            writer_aborted: false,
            writer_timed_out: false,
            explicit_cancellation: false,
            child_waited: false,
            child_exited: false,
            child_wait_timed_out: false,
            needs_attention: false,
        }
    }
}

/// The bounded teardown failure and the state in which it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeardownFailure {
    /// The optional graceful `end_session` frame could not be queued in time.
    ControlSendTimedOut,
    /// The writer did not finish within [`WRITER_TEARDOWN_TIMEOUT`].
    WriterTimedOut,
    /// The writer task terminated with a join error.
    WriterJoinFailed,
    /// Waiting for the child returned an I/O error.
    ChildWaitFailed,
    /// The child did not exit within [`CHILD_WAIT_TIMEOUT`].
    ChildWaitTimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TeardownError {
    pub(crate) failure: TeardownFailure,
    pub(crate) state: ProcessControlState,
}

impl fmt::Display for TeardownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "claude_code teardown failed: failure={:?}; control_present={}; control_closed={}; stdin_sender_closed={}; control_send_timed_out={}; writer_finished={}; writer_aborted={}; writer_timed_out={}; explicit_cancellation={}; child_waited={}; child_exited={}; child_wait_timed_out={}; needs_attention={}",
            self.failure,
            self.state.control_present,
            self.state.control_closed,
            self.state.stdin_sender_closed,
            self.state.control_send_timed_out,
            self.state.writer_finished,
            self.state.writer_aborted,
            self.state.writer_timed_out,
            self.state.explicit_cancellation,
            self.state.child_waited,
            self.state.child_exited,
            self.state.child_wait_timed_out,
            self.state.needs_attention,
        )
    }
}

/// Closes control/stdin ownership, bounds writer shutdown, and reaps the child.
///
/// The child is killed only for [`TerminationRequest::ExplicitCancellation`]. A
/// writer timeout aborts the writer task to release its stdin handle, records a
/// typed error, and still reaches `child.wait()` so the in-flight guard can leave
/// its scope without leaking a process.
pub(crate) async fn finish(
    child: &mut Child,
    control: Option<ControlChannel>,
    tx_stdin: mpsc::Sender<String>,
    writer: JoinHandle<()>,
    termination: TerminationRequest,
) -> Result<ProcessControlState, TeardownError> {
    finish_with_timeouts(
        child,
        control,
        tx_stdin,
        writer,
        termination,
        WRITER_TEARDOWN_TIMEOUT,
        CHILD_WAIT_TIMEOUT,
    )
    .await
}

/// Testable implementation of [`finish`] with injected bounded waits.
async fn finish_with_timeouts(
    child: &mut Child,
    mut control: Option<ControlChannel>,
    tx_stdin: mpsc::Sender<String>,
    mut writer: JoinHandle<()>,
    termination: TerminationRequest,
    writer_timeout: Duration,
    child_wait_timeout: Duration,
) -> Result<ProcessControlState, TeardownError> {
    let mut state = ProcessControlState::new(control.is_some());
    let mut failure = None;

    // `end_session` is best-effort. It is deliberately bounded because a full
    // control queue must not prevent the ownership closure below.
    if matches!(termination, TerminationRequest::WaitForExit)
        && let Some(control) = control.as_ref()
        && tokio::time::timeout(
            writer_timeout,
            control.send_request("end_session", json!({})),
        )
        .await
        .is_err()
    {
        state.control_send_timed_out = true;
        failure = Some(TeardownFailure::ControlSendTimedOut);
    }

    // Dropping the control channel consumes its sender. The local sender is
    // consumed by this function as well, so the writer receiver can observe
    // `None`.
    if let Some(control) = control.take() {
        drop(control);
        state.control_closed = true;
    }
    drop(tx_stdin);
    state.stdin_sender_closed = true;

    if matches!(termination, TerminationRequest::ExplicitCancellation) {
        cancel_process_tree(child).await;
        state.explicit_cancellation = true;
    }

    match tokio::time::timeout(writer_timeout, &mut writer).await {
        Ok(Ok(())) => state.writer_finished = true,
        Ok(Err(_)) => {
            failure.get_or_insert(TeardownFailure::WriterJoinFailed);
        }
        Err(_) => {
            state.writer_timed_out = true;
            // Retain the handle while applying the timeout. Aborting the task
            // drops its ChildStdin, after which `child.wait()` can make progress.
            writer.abort();
            let _ = writer.await;
            state.writer_aborted = true;
            failure = Some(TeardownFailure::WriterTimedOut);
        }
    }

    match tokio::time::timeout(child_wait_timeout, child.wait()).await {
        Err(_) => {
            // `Child::wait` is cancel-safe. Keep the child alive for the caller
            // (or the detached reaper) so a normal result/EOF never turns into
            // an implicit process kill just because it ignored EOF.
            state.child_wait_timed_out = true;
            state.needs_attention = true;
            failure = Some(TeardownFailure::ChildWaitTimedOut);
        }
        Ok(Ok(_)) => {
            state.child_waited = true;
            state.child_exited = true;
        }
        Ok(Err(_)) => {
            failure.get_or_insert(TeardownFailure::ChildWaitFailed);
        }
    }

    match failure {
        Some(failure) => Err(TeardownError { failure, state }),
        None => Ok(state),
    }
}

/// Keep a child whose bounded teardown wait expired until it exits or the
/// caller explicitly cancels the provider stream.
///
/// The normal result path does not kill a child that ignored EOF. This task
/// reaps it when it eventually exits, while retaining the stream cancellation
/// token as the explicit process-tree cancellation path.
pub(crate) fn spawn_reaper(
    mut child: Child,
    consumer_dropped: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            _ = consumer_dropped.cancelled() => {
                cancel_process_tree(&mut child).await;
            }
            _ = child.wait() => {}
        }
        // `cancel_process_tree` only signals the process; always reap the
        // direct child before releasing this task's ownership of its handle.
        let _ = child.wait().await;
    })
}

#[cfg(windows)]
trait ProcessTreeKillBackend: Send + Sync {
    /// Terminate the complete process tree while the direct parent PID is
    /// still valid.
    fn kill_tree<'a>(&'a self, pid: u32) -> BoxFuture<'a, ()>;

    /// Terminate the direct child after the tree helper has finished.
    fn kill_direct(&self, child: &mut Child);
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, Default)]
struct WindowsProcessTreeKillBackend;

#[cfg(windows)]
impl ProcessTreeKillBackend for WindowsProcessTreeKillBackend {
    fn kill_tree<'a>(&'a self, pid: u32) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Awaiting `taskkill` is important: waiting on the direct child
            // first can return while descendants are still running.
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        })
    }

    fn kill_direct(&self, child: &mut Child) {
        let _ = child.start_kill();
    }
}

#[cfg(windows)]
async fn cancel_process_tree_with<B: ProcessTreeKillBackend>(child: &mut Child, backend: &B) {
    if let Some(pid) = child.id() {
        backend.kill_tree(pid).await;
    }
    backend.kill_direct(child);
}

/// Explicitly terminates the CLI and all descendants it started.
///
/// This is intentionally separate from writer-timeout and child-wait handling.
/// Callers should use it only when a cancellation was requested or another
/// policy explicitly authorizes process termination.
pub(crate) async fn cancel_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        // `taskkill` must run while the parent PID still exists. The backend
        // awaits the tree kill before sending the direct-child fallback.
        cancel_process_tree_with(child, &WindowsProcessTreeKillBackend).await;
    }
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = child.start_kill();
        let Some(pid) = pid else {
            return;
        };
        // Negative pid = the whole process group created with `process_group(0)`.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(test)]
#[path = "teardown_tests.rs"]
mod tests;
