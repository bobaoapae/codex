//! Parent-provided context for a task-only spawned agent.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use std::error::Error;
use std::fmt;

/// Maximum UTF-8 bytes accepted for an explicit spawn brief.
///
/// The bound leaves room for the stable wrapper while keeping the complete
/// contextual response item comfortably below the 1,000-token limit.
pub(crate) const MAX_SPAWN_TASK_CONTEXT_BYTES: usize = 768;

const START_MARKER: &str = "<codex_spawn_task_context>";
const END_MARKER: &str = "</codex_spawn_task_context>";
const BODY_PREFIX: &str = "Parent-provided task context:\n";

/// A bounded, typed context brief injected before a spawned agent receives its task message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnTaskContext {
    text: String,
}

impl SpawnTaskContext {
    pub(crate) fn try_new(text: impl Into<String>) -> Result<Self, SpawnTaskContextError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(SpawnTaskContextError::Empty);
        }
        let byte_len = text.len();
        if byte_len > MAX_SPAWN_TASK_CONTEXT_BYTES {
            return Err(SpawnTaskContextError::TooLarge {
                byte_len,
                max_bytes: MAX_SPAWN_TASK_CONTEXT_BYTES,
            });
        }
        Ok(Self { text })
    }
}

/// Validation failure for an explicit spawn brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpawnTaskContextError {
    Empty,
    TooLarge { byte_len: usize, max_bytes: usize },
}

impl fmt::Display for SpawnTaskContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "`task_context` must not be empty"),
            Self::TooLarge {
                byte_len,
                max_bytes,
            } => write!(
                f,
                "`task_context` is {byte_len} UTF-8 bytes; the maximum is {max_bytes} bytes"
            ),
        }
    }
}

impl Error for SpawnTaskContextError {}

impl ContextualUserFragment for SpawnTaskContext {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.spawn_task_context".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (START_MARKER, END_MARKER)
    }

    fn body(&self) -> String {
        format!("{BODY_PREFIX}{}\n", self.text)
    }
}

#[cfg(test)]
#[path = "spawn_task_context_tests.rs"]
mod tests;
