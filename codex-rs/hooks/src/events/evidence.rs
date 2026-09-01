//! Validation and attribution for evidence returned by `PostToolUse` hooks.

use std::collections::BTreeMap;
use std::fmt;

use codex_extension_items::receipt::is_forbidden_metadata_key;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookSource;
use serde_json::Value;

use crate::engine::ConfiguredHandler;
use crate::schema::PostToolUseEvidenceReferenceWire;
use crate::schema::PostToolUseEvidenceStatusWire;
use crate::schema::PostToolUseEvidenceWire;

pub(crate) const MAX_TAGS: usize = 32;
pub(crate) const MAX_TAG_KEY_BYTES: usize = 64;
pub(crate) const MAX_TAG_VALUE_BYTES: usize = 256;
pub(crate) const MAX_METADATA_BYTES: usize = 64 * 1024;
pub(crate) const MAX_KIND_BYTES: usize = 128;
pub(crate) const MAX_SUBJECT_BYTES: usize = 1_024;
pub(crate) const MAX_REFERENCES: usize = 64;
pub(crate) const MAX_REFERENCE_KIND_BYTES: usize = 64;
pub(crate) const MAX_REFERENCE_ID_BYTES: usize = 256;

/// The status vocabulary understood by the receipt projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostToolUseEvidenceStatus {
    Pass,
    Fail,
    Blocked,
    Inconclusive,
    Informational,
}

/// The handler identity retained when a hook contributes evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostToolUseEvidenceAttribution {
    pub handler_id: String,
    pub handler_type: HookHandlerType,
    pub execution_mode: HookExecutionMode,
    pub source: HookSource,
}

/// A bounded receipt-compatible contribution from a `PostToolUse` hook.
///
/// This value is deliberately separate from model-facing additional context
/// and feedback. It is safe to persist or project into `receipt.attached`
/// after the host adds its thread and turn associations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostToolUseEvidence {
    pub kind: String,
    pub subject: String,
    pub status: PostToolUseEvidenceStatus,
    pub tags: BTreeMap<String, String>,
    pub refs: Vec<PostToolUseEvidenceReference>,
    pub metadata: Option<Value>,
    pub attribution: PostToolUseEvidenceAttribution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostToolUseEvidenceReference {
    pub kind: String,
    pub id: String,
}

impl PostToolUseEvidence {
    pub(crate) fn from_wire(
        wire: PostToolUseEvidenceWire,
        handler: &ConfiguredHandler,
    ) -> Result<Self, EvidenceValidationError> {
        validate_wire(&wire)?;
        Ok(Self {
            kind: wire.kind,
            subject: wire.subject,
            status: PostToolUseEvidenceStatus::from(wire.status),
            tags: wire.tags,
            refs: wire
                .refs
                .into_iter()
                .map(PostToolUseEvidenceReference::from)
                .collect(),
            metadata: wire.metadata,
            attribution: PostToolUseEvidenceAttribution {
                handler_id: handler.run_id(),
                handler_type: handler.handler_type(),
                execution_mode: handler.execution_mode(),
                source: handler.source,
            },
        })
    }
}

impl From<PostToolUseEvidenceStatusWire> for PostToolUseEvidenceStatus {
    fn from(value: PostToolUseEvidenceStatusWire) -> Self {
        match value {
            PostToolUseEvidenceStatusWire::Pass => Self::Pass,
            PostToolUseEvidenceStatusWire::Fail => Self::Fail,
            PostToolUseEvidenceStatusWire::Blocked => Self::Blocked,
            PostToolUseEvidenceStatusWire::Inconclusive => Self::Inconclusive,
            PostToolUseEvidenceStatusWire::Informational => Self::Informational,
        }
    }
}

impl From<PostToolUseEvidenceReferenceWire> for PostToolUseEvidenceReference {
    fn from(value: PostToolUseEvidenceReferenceWire) -> Self {
        Self {
            kind: value.kind,
            id: value.id,
        }
    }
}

pub(crate) fn validate_wire(
    evidence: &PostToolUseEvidenceWire,
) -> Result<(), EvidenceValidationError> {
    validate_text(&evidence.kind, MAX_KIND_BYTES, "kind")?;
    validate_text(&evidence.subject, MAX_SUBJECT_BYTES, "subject")?;
    if evidence.tags.len() > MAX_TAGS {
        return Err(EvidenceValidationError::TooMany {
            field: "tags",
            max: MAX_TAGS,
        });
    }
    for (key, value) in &evidence.tags {
        validate_text(key, MAX_TAG_KEY_BYTES, "tag key")?;
        validate_text(value, MAX_TAG_VALUE_BYTES, "tag value")?;
    }
    if evidence.refs.len() > MAX_REFERENCES {
        return Err(EvidenceValidationError::TooMany {
            field: "refs",
            max: MAX_REFERENCES,
        });
    }
    for reference in &evidence.refs {
        validate_text(&reference.kind, MAX_REFERENCE_KIND_BYTES, "reference kind")?;
        validate_text(&reference.id, MAX_REFERENCE_ID_BYTES, "reference id")?;
    }
    if let Some(metadata) = evidence.metadata.as_ref() {
        validate_json(metadata)?;
    }
    Ok(())
}

/// Tests whether a parsed hook object advertises evidence. This is used by
/// executor-scoped dispatch, where the output is intentionally not parsed as
/// a user-visible hook run but must still produce a local diagnostic.
pub(crate) fn has_evidence(stdout: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) else {
        return false;
    };
    value
        .get("hookSpecificOutput")
        .and_then(Value::as_object)
        .is_some_and(|output| output.contains_key("evidence"))
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), EvidenceValidationError> {
    if value.trim().is_empty() {
        return Err(EvidenceValidationError::Empty { field });
    }
    if value.len() > max_bytes {
        return Err(EvidenceValidationError::TooLong {
            field,
            max: max_bytes,
        });
    }
    if value.contains('\0') {
        return Err(EvidenceValidationError::Nul { field });
    }
    Ok(())
}

fn validate_json(value: &Value) -> Result<(), EvidenceValidationError> {
    let encoded = serde_json::to_vec(value).map_err(|error| EvidenceValidationError::Json {
        message: error.to_string(),
    })?;
    if encoded.len() > MAX_METADATA_BYTES {
        return Err(EvidenceValidationError::TooLong {
            field: "metadata",
            max: MAX_METADATA_BYTES,
        });
    }
    validate_metadata_keys(value)
}

fn validate_metadata_keys(value: &Value) -> Result<(), EvidenceValidationError> {
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                if is_forbidden_metadata_key(key) {
                    return Err(EvidenceValidationError::RawPayloadKey);
                }
                validate_metadata_keys(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_metadata_keys(value)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceValidationError {
    Empty { field: &'static str },
    TooLong { field: &'static str, max: usize },
    TooMany { field: &'static str, max: usize },
    Nul { field: &'static str },
    Json { message: String },
    RawPayloadKey,
}

impl fmt::Display for EvidenceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "{field} must not be empty"),
            Self::TooLong { field, max } => write!(formatter, "{field} exceeds {max} bytes"),
            Self::TooMany { field, max } => write!(formatter, "{field} exceeds {max} entries"),
            Self::Nul { field } => write!(formatter, "{field} must not contain NUL"),
            Self::Json { .. } => formatter.write_str("metadata is not valid JSON"),
            Self::RawPayloadKey => formatter.write_str("metadata contains a forbidden raw field"),
        }
    }
}

impl std::error::Error for EvidenceValidationError {}
