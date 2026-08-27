//! FORK: Rust port of the `chatgpt-pro-mcp` driver (`daemon.ts`, `tab.ts`,
//! `page-scripts.ts`, `api.ts`, `ops.ts`).
//!
//! Everything here talks to the chrome-mcp daemon (`http://127.0.0.1:8848/mcp`,
//! Streamable HTTP + bearer) and, through it, to a real chatgpt.com tab. The
//! shared error vocabulary lives in this file so every submodule agrees on how a
//! failure is classified and whether the message may already have landed.

pub(crate) mod api;
pub(crate) mod daemon;
pub(crate) mod ops;
pub(crate) mod page_scripts;
pub(crate) mod tabs;

use std::fmt;

/// Phase of a send that was in progress when an operation failed. Mirrors the
/// `phase` field of `ops.ts` errors: it tells the caller whether retrying is
/// safe (before `submit`) or must go through `confirm_submitted` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FailurePhase {
    Navigate,
    Model,
    Precheck,
    Upload,
    Compose,
    AttachmentsWait,
    Submit,
    Confirm,
}

impl fmt::Display for FailurePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Navigate => "navigate",
            Self::Model => "model",
            Self::Precheck => "precheck",
            Self::Upload => "upload",
            Self::Compose => "compose",
            Self::AttachmentsWait => "attachments-wait",
            Self::Submit => "submit",
            Self::Confirm => "confirm",
        };
        f.write_str(text)
    }
}

/// How a driver operation failed. The provider maps these onto `CodexErr`
/// (see the "Mapeamento de erros" table in the plan); the driver itself never
/// decides retry policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverErrorKind {
    /// chrome-mcp daemon unreachable, extension disconnected, connect/initialize
    /// failed, or a reconnect attempt failed.
    DaemonDown,
    /// The page shows a login wall or the session token could not be fetched.
    LoginRequired,
    /// 429 after backoff, or the "Too many requests" dialog.
    RateLimited,
    /// The composer or the edge rejected the message as too long.
    MessageTooLong,
    /// A selector the driver depends on was not found after retrying — the UI
    /// changed under us.
    UiChanged,
    /// ChatGPT itself failed ("Something went wrong", partial completion).
    Upstream,
    /// Backend API returned 404 for the conversation.
    ConversationNotFound,
    /// The chrome-mcp tool call reported `isError`.
    Tool,
    /// Eval or navigation timed out.
    Timeout,
    /// The submit click was ambiguous and could not be confirmed either way.
    SubmitAmbiguous,
    /// Anything else (JSON parse, unexpected shape, IO).
    Other,
}

/// Error type of every driver operation.
#[derive(Debug, Clone)]
pub(crate) struct DriverError {
    pub(crate) kind: DriverErrorKind,
    pub(crate) message: String,
    /// The send phase that was in progress, when the error came from `ops`.
    pub(crate) phase: Option<FailurePhase>,
    /// Whether the user message is known to have landed in the conversation
    /// (`Some(true)`), known not to (`Some(false)`), or unknown (`None`). Drives
    /// the `message_landed_unanswered` continuity flag.
    pub(crate) message_landed: Option<bool>,
}

impl DriverError {
    pub(crate) fn new(kind: DriverErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            phase: None,
            message_landed: None,
        }
    }

    pub(crate) fn with_phase(mut self, phase: FailurePhase) -> Self {
        self.phase = Some(phase);
        self
    }

    pub(crate) fn landed(mut self, landed: Option<bool>) -> Self {
        self.message_landed = landed;
        self
    }

    pub(crate) fn daemon_down(message: impl Into<String>) -> Self {
        Self::new(DriverErrorKind::DaemonDown, message)
    }

    pub(crate) fn tool(message: impl Into<String>) -> Self {
        Self::new(DriverErrorKind::Tool, message)
    }

    pub(crate) fn timeout(message: impl Into<String>) -> Self {
        Self::new(DriverErrorKind::Timeout, message)
    }

    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::new(DriverErrorKind::Other, message)
    }

    pub(crate) fn ui_changed(message: impl Into<String>) -> Self {
        Self::new(DriverErrorKind::UiChanged, message)
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.phase {
            Some(phase) => write!(f, "{} (phase: {phase})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<serde_json::Error> for DriverError {
    fn from(err: serde_json::Error) -> Self {
        Self::other(format!("invalid JSON from the page: {err}"))
    }
}

pub(crate) type DriverResult<T> = Result<T, DriverError>;
