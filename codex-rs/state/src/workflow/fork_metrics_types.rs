//! Bounded fork timing, context, and provider-usage projection values.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

use super::types::*;

/// Maximum number of context-size/provenance entries retained for one fork.
/// Aggregate byte/token counts are retained even when this cap is reached.
pub const MAX_FORK_CONTEXT_ENTRIES: usize = 2_048;
const MAX_FORK_ID_BYTES: usize = 128;
const MAX_FORK_CALL_ID_BYTES: usize = 256;

/// The history selection requested for a fork.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowForkTurns {
    FullHistory,
    LastNTurns(u32),
}

impl WorkflowForkTurns {
    pub fn mode(self) -> &'static str {
        match self {
            Self::FullHistory => "fullHistory",
            Self::LastNTurns(_) => "lastNTurns",
        }
    }

    pub fn count(self) -> Option<u32> {
        match self {
            Self::FullHistory => None,
            Self::LastNTurns(count) => Some(count),
        }
    }

    pub(crate) fn from_parts(mode: &str, count: Option<i64>) -> Result<Self> {
        match mode {
            "fullHistory" if count.is_none() => Ok(Self::FullHistory),
            "lastNTurns" => {
                let count = count.ok_or_else(|| anyhow::anyhow!("fork turn count is missing"))?;
                let count = u32::try_from(count)
                    .map_err(|_| anyhow::anyhow!("fork turn count is out of range"))?;
                if count == 0 {
                    bail!("fork turn count must be positive");
                }
                Ok(Self::LastNTurns(count))
            }
            _ => bail!("unknown fork turns mode: {mode}"),
        }
    }
}

/// Provenance exposed to future context/inspect consumers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowForkContextOrigin {
    InheritedHistory,
    NewOutput,
}

impl WorkflowForkContextOrigin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InheritedHistory => "inheritedHistory",
            Self::NewOutput => "newOutput",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        match value {
            "inheritedHistory" => Ok(Self::InheritedHistory),
            "newOutput" => Ok(Self::NewOutput),
            _ => bail!("unknown fork context origin: {value}"),
        }
    }
}

/// One size/provenance-only context projection entry.  No content is stored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowForkContextEntry {
    pub fork_id: String,
    pub sequence: i64,
    pub origin: WorkflowForkContextOrigin,
    pub byte_count: i64,
    pub token_count: i64,
}

impl WorkflowForkContextEntry {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.fork_id, "fork id")?;
        if self.sequence < 0 {
            bail!("fork context sequence must be non-negative");
        }
        validate_nonnegative_i64(self.byte_count, "fork context byte count")?;
        validate_nonnegative_i64(self.token_count, "fork context token count")
    }
}

/// Immutable input used when creating a fork projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowForkMetricsCreate {
    pub fork_id: String,
    pub spawn_call_id: String,
    pub parent_thread_id: String,
    pub fork_turns: WorkflowForkTurns,
    pub spawn_requested_at_ms: i64,
    pub projected_fork_bytes: i64,
    pub projected_fork_tokens: i64,
    pub context_entries: Vec<WorkflowForkContextEntry>,
}

impl WorkflowForkMetricsCreate {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_id(&self.fork_id, "fork id")?;
        validate_bounded_id(
            &self.spawn_call_id,
            MAX_FORK_CALL_ID_BYTES,
            "fork spawn call id",
        )?;
        validate_id(&self.parent_thread_id, "fork parent thread id")?;
        validate_nonnegative_i64(self.spawn_requested_at_ms, "fork spawn timestamp")?;
        validate_nonnegative_i64(self.projected_fork_bytes, "projected fork bytes")?;
        validate_nonnegative_i64(self.projected_fork_tokens, "projected fork tokens")?;
        if self.context_entries.len() > MAX_FORK_CONTEXT_ENTRIES {
            bail!("fork context entries exceed {MAX_FORK_CONTEXT_ENTRIES}");
        }
        let mut sequences = HashSet::with_capacity(self.context_entries.len());
        for entry in &self.context_entries {
            entry.validate()?;
            if entry.fork_id != self.fork_id {
                bail!("fork context entry has a different fork id");
            }
            if !sequences.insert(entry.sequence) {
                bail!("fork context entries must have unique sequences");
            }
        }
        Ok(())
    }
}

/// Durable projection returned by [`WorkflowStore`](super::WorkflowStore).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowForkMetrics {
    pub fork_id: String,
    pub spawn_call_id: String,
    pub parent_thread_id: String,
    pub child_thread_id: Option<String>,
    pub fork_turns: WorkflowForkTurns,
    pub spawn_requested_at_ms: i64,
    pub child_created_at_ms: Option<i64>,
    pub first_event_at_ms: Option<i64>,
    pub first_new_response_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub projected_fork_bytes: i64,
    pub projected_fork_tokens: i64,
    pub provider_input_tokens: Option<i64>,
    pub provider_cached_input_tokens: Option<i64>,
    pub provider_uncached_input_tokens: Option<i64>,
    pub provider_cache_write_input_tokens: Option<i64>,
    pub warning_emitted: bool,
    pub warning_projected_tokens: Option<i64>,
    pub warning_limit_tokens: Option<i64>,
    pub updated_at_ms: i64,
}

impl WorkflowForkMetrics {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.fork_id, "fork id")?;
        validate_bounded_id(
            &self.spawn_call_id,
            MAX_FORK_CALL_ID_BYTES,
            "fork spawn call id",
        )?;
        validate_id(&self.parent_thread_id, "fork parent thread id")?;
        validate_optional_id(self.child_thread_id.as_deref(), "fork child thread id")?;
        validate_nonnegative_i64(self.spawn_requested_at_ms, "fork spawn timestamp")?;
        for (value, name) in [
            (self.projected_fork_bytes, "projected fork bytes"),
            (self.projected_fork_tokens, "projected fork tokens"),
            (self.updated_at_ms, "fork updated timestamp"),
        ] {
            validate_nonnegative_i64(value, name)?;
        }
        for (value, name) in [
            (self.child_created_at_ms, "fork child timestamp"),
            (self.first_event_at_ms, "fork first event timestamp"),
            (
                self.first_new_response_at_ms,
                "fork first response timestamp",
            ),
            (self.completed_at_ms, "fork completion timestamp"),
            (self.provider_input_tokens, "fork provider input tokens"),
            (
                self.provider_cached_input_tokens,
                "fork provider cached input tokens",
            ),
            (
                self.provider_uncached_input_tokens,
                "fork provider uncached input tokens",
            ),
            (
                self.provider_cache_write_input_tokens,
                "fork provider cache-write input tokens",
            ),
            (
                self.warning_projected_tokens,
                "fork warning projected tokens",
            ),
            (self.warning_limit_tokens, "fork warning limit tokens"),
        ] {
            if let Some(value) = value {
                validate_nonnegative_i64(value, name)?;
            }
        }
        if self.warning_limit_tokens.is_some_and(|limit| limit == 0) {
            bail!("fork warning token limit must be positive");
        }
        Ok(())
    }
}

fn validate_id(value: &str, name: &str) -> Result<()> {
    validate_bounded_id(value, MAX_FORK_ID_BYTES, name)
}

fn validate_bounded_id(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    validate_text(value, max_bytes, name)
}

fn validate_optional_id(value: Option<&str>, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_id(value, name)?;
    }
    Ok(())
}
