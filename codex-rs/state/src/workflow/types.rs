//! Shared bounded values and validation helpers for workflow modules.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub(super) const MAX_ID_BYTES: usize = 128;
pub(super) const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub(super) const MAX_STATUS_BYTES: usize = 32;
pub(super) const MAX_PATH_BYTES: usize = 4_096;
pub(super) const MAX_JSON_BYTES: usize = 65_536;
pub(super) const MAX_SOURCE_ID_BYTES: usize = 256;
pub(super) const MAX_SEARCH_CONTENT_BYTES: usize = 1_000_000;
pub(super) const MAX_SEARCH_QUERY_BYTES: usize = 4_096;
pub(super) const MAX_SEARCH_SNIPPET_BYTES: usize = 512;
pub(super) const MAX_SEARCH_FILTERS: usize = 5;
pub(super) const MAX_PAGE_SIZE: u32 = 200;
pub(super) const MAX_BATCH_IDS: usize = 200;

/// The workflow class used by run and search projections.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowThreadClass {
    Interactive,
    SubAgent,
    TransientJob,
    Internal,
    LegacyExec,
}

impl WorkflowThreadClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::SubAgent => "subAgent",
            Self::TransientJob => "transientJob",
            Self::Internal => "internal",
            Self::LegacyExec => "legacyExec",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "subAgent" => Ok(Self::SubAgent),
            "transientJob" => Ok(Self::TransientJob),
            "internal" => Ok(Self::Internal),
            "legacyExec" => Ok(Self::LegacyExec),
            _ => bail!("unknown workflow thread class: {value}"),
        }
    }
}

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) fn validate_text(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{name} must not be empty");
    }
    if value.len() > max_bytes {
        bail!("{name} exceeds {max_bytes} bytes");
    }
    Ok(())
}

pub(super) fn validate_nonempty_bounded(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    validate_text(value, max_bytes, name)
}

pub(super) fn validate_optional_text(
    value: Option<&str>,
    max_bytes: usize,
    name: &str,
) -> Result<()> {
    if let Some(value) = value {
        validate_text(value, max_bytes, name)?;
    }
    Ok(())
}

pub(super) fn validate_json_bytes(value: &str, name: &str) -> Result<()> {
    if value.len() > MAX_JSON_BYTES {
        bail!("{name} exceeds {MAX_JSON_BYTES} bytes");
    }
    Ok(())
}

pub(super) fn serialize_optional_json(value: Option<&Value>, name: &str) -> Result<Option<String>> {
    value
        .map(|value| {
            let encoded = serde_json::to_string(value)?;
            validate_json_bytes(&encoded, name)?;
            Ok(encoded)
        })
        .transpose()
}

pub(super) fn parse_optional_json(value: Option<String>, name: &str) -> Result<Option<Value>> {
    value
        .map(|value| {
            validate_json_bytes(&value, name)?;
            Ok(serde_json::from_str(&value)?)
        })
        .transpose()
}

pub(super) fn validate_positive_i64(value: i64, name: &str) -> Result<()> {
    if value <= 0 {
        bail!("{name} must be positive");
    }
    Ok(())
}

pub(super) fn validate_nonnegative_i64(value: i64, name: &str) -> Result<()> {
    if value < 0 {
        bail!("{name} must be non-negative");
    }
    Ok(())
}

pub(super) fn validate_optional_nonnegative_i64(value: Option<i64>, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_nonnegative_i64(value, name)?;
    }
    Ok(())
}

pub(super) fn validate_page_size(limit: u32) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        bail!("page size must be between 1 and {MAX_PAGE_SIZE}");
    }
    Ok(())
}

pub(super) fn escape_fts5_literal(query: &str) -> Result<String> {
    validate_text(query, MAX_SEARCH_QUERY_BYTES, "search query")?;
    if query.trim().is_empty() {
        bail!("search query must contain non-whitespace text");
    }
    if query.contains('\0') {
        bail!("search query must not contain NUL");
    }
    let mut terms = Vec::new();
    let mut term = String::new();
    for character in query.chars() {
        if character.is_alphanumeric() || character == '_' {
            term.push(character);
        } else if !term.is_empty() {
            terms.push(std::mem::take(&mut term));
        }
    }
    if !term.is_empty() {
        terms.push(term);
    }
    if terms.is_empty() {
        bail!("search query has no searchable terms");
    }
    Ok(format!("\"{}\"", terms.join(" ")))
}

pub(super) fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "blocked" | "inconclusive" | "cancelled" | "aborted"
    )
}
