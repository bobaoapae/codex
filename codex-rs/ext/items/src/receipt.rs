//! Generic, bounded receipts attached by trusted host-side integrations.
//!
//! Receipts are deliberately not model-facing telemetry. They are canonical
//! history items that let a hook, test runner, or product-specific adapter
//! attach a small, versioned result to a thread while preserving metadata that
//! a newer reader does not understand.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fmt;
use ts_rs::TS;

pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_KEY_BYTES: usize = 64;
pub const MAX_TAG_VALUE_BYTES: usize = 256;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_RECEIPT_ID_BYTES: usize = 128;
pub const MAX_KIND_BYTES: usize = 128;
pub const MAX_SUBJECT_BYTES: usize = 1_024;
pub const MAX_SOURCE_BYTES: usize = 128;
pub const MAX_PROVENANCE_BYTES: usize = 8 * 1024;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_TIMESTAMP_BYTES: usize = 128;
pub const MAX_REFERENCES: usize = 64;
pub const MAX_REFERENCE_KIND_BYTES: usize = 64;
pub const MAX_REFERENCE_ID_BYTES: usize = 256;

/// Stable status vocabulary for host-attached receipts.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ReceiptStatus {
    Pass,
    Fail,
    Blocked,
    Inconclusive,
    Informational,
}

/// A bounded reference to another canonical item or artifact.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ReceiptReference {
    pub kind: String,
    pub id: String,
}

/// A generic receipt that can survive readers which do not know its `kind` or
/// `schemaVersion` yet. Unknown metadata keys remain inside `metadata` as
/// JSON values; raw tool output and ciphertext are intentionally rejected.
#[derive(Debug, Clone, Serialize, TS, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ReceiptAttachedItem {
    pub receipt_id: String,
    /// Numeric versions are intentionally not matched against a closed enum,
    /// so a newer schema version can be read and written losslessly.
    pub schema_version: u64,
    /// Extension-defined receipt kind. The host does not interpret this value.
    ///
    /// It is named `receiptKind` on the wire because the surrounding
    /// `ExtensionItem` envelope already reserves `kind` for `receipt.attached`.
    #[serde(rename = "receiptKind")]
    #[ts(rename = "receiptKind")]
    pub kind: String,
    pub subject: String,
    pub status: ReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub plan_snapshot_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub finished_at: Option<String>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub provenance: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<ReceiptReference>,
    /// Extension-defined metadata. It is preserved without interpreting
    /// unknown keys, but remains bounded and cannot contain raw payload keys.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "payloadMetadata"
    )]
    #[ts(optional)]
    pub metadata: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiptAttachedItemWire {
    receipt_id: String,
    schema_version: u64,
    #[serde(rename = "receiptKind", alias = "kind")]
    kind: String,
    subject: String,
    status: ReceiptStatus,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    plan_snapshot_id: Option<String>,
    created_at: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    source: String,
    #[serde(default)]
    provenance: Option<JsonValue>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    #[serde(default)]
    refs: Vec<ReceiptReference>,
    #[serde(default, alias = "payloadMetadata")]
    metadata: Option<JsonValue>,
}

impl<'de> Deserialize<'de> for ReceiptAttachedItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReceiptAttachedItemWire::deserialize(deserializer)?;
        let item = Self {
            receipt_id: wire.receipt_id,
            schema_version: wire.schema_version,
            kind: wire.kind,
            subject: wire.subject,
            status: wire.status,
            thread_id: wire.thread_id,
            turn_id: wire.turn_id,
            job_id: wire.job_id,
            plan_snapshot_id: wire.plan_snapshot_id,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            finished_at: wire.finished_at,
            source: wire.source,
            provenance: wire.provenance,
            tags: wire.tags,
            refs: wire.refs,
            metadata: wire.metadata,
        };
        item.validate().map_err(serde::de::Error::custom)?;
        Ok(item)
    }
}

impl ReceiptAttachedItem {
    /// Construct a receipt with the required identity and provenance fields.
    /// Optional associations, tags, references and metadata start empty.
    pub fn new(
        receipt_id: impl Into<String>,
        schema_version: u64,
        kind: impl Into<String>,
        subject: impl Into<String>,
        status: ReceiptStatus,
        created_at: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Self, ReceiptValidationError> {
        let item = Self {
            receipt_id: receipt_id.into(),
            schema_version,
            kind: kind.into(),
            subject: subject.into(),
            status,
            thread_id: None,
            turn_id: None,
            job_id: None,
            plan_snapshot_id: None,
            created_at: created_at.into(),
            updated_at: None,
            finished_at: None,
            source: source.into(),
            provenance: None,
            tags: BTreeMap::new(),
            refs: Vec::new(),
            metadata: None,
        };
        item.validate()?;
        Ok(item)
    }

    /// Validate all bounded fields before construction or persistence.
    pub fn validate(&self) -> Result<(), ReceiptValidationError> {
        validate_text(&self.receipt_id, MAX_RECEIPT_ID_BYTES, "receiptId")?;
        validate_text(&self.kind, MAX_KIND_BYTES, "kind")?;
        validate_text(&self.subject, MAX_SUBJECT_BYTES, "subject")?;
        validate_text(&self.created_at, MAX_TIMESTAMP_BYTES, "createdAt")?;
        validate_text(&self.source, MAX_SOURCE_BYTES, "source")?;
        validate_optional_text(self.thread_id.as_deref(), MAX_ID_BYTES, "threadId")?;
        validate_optional_text(self.turn_id.as_deref(), MAX_ID_BYTES, "turnId")?;
        validate_optional_text(self.job_id.as_deref(), MAX_ID_BYTES, "jobId")?;
        validate_optional_text(
            self.plan_snapshot_id.as_deref(),
            MAX_ID_BYTES,
            "planSnapshotId",
        )?;
        validate_optional_text(self.updated_at.as_deref(), MAX_TIMESTAMP_BYTES, "updatedAt")?;
        validate_optional_text(
            self.finished_at.as_deref(),
            MAX_TIMESTAMP_BYTES,
            "finishedAt",
        )?;

        if let Some(provenance) = self.provenance.as_ref() {
            validate_json(provenance, MAX_PROVENANCE_BYTES, "provenance")?;
        }
        if self.tags.len() > MAX_TAGS {
            return Err(ReceiptValidationError::TooMany {
                field: "tags",
                max: MAX_TAGS,
            });
        }
        for (key, value) in &self.tags {
            validate_text(key, MAX_TAG_KEY_BYTES, "tag key")?;
            validate_text(value, MAX_TAG_VALUE_BYTES, "tag value")?;
        }
        if self.refs.len() > MAX_REFERENCES {
            return Err(ReceiptValidationError::TooMany {
                field: "refs",
                max: MAX_REFERENCES,
            });
        }
        for reference in &self.refs {
            validate_text(&reference.kind, MAX_REFERENCE_KIND_BYTES, "reference kind")?;
            validate_text(&reference.id, MAX_REFERENCE_ID_BYTES, "reference id")?;
        }
        if let Some(metadata) = self.metadata.as_ref() {
            validate_json(metadata, MAX_METADATA_BYTES, "metadata")?;
        }
        Ok(())
    }
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), ReceiptValidationError> {
    if value.trim().is_empty() {
        return Err(ReceiptValidationError::Empty { field });
    }
    if value.len() > max_bytes {
        return Err(ReceiptValidationError::TooLong {
            field,
            max: max_bytes,
        });
    }
    if value.contains('\0') {
        return Err(ReceiptValidationError::Nul { field });
    }
    Ok(())
}

fn validate_optional_text(
    value: Option<&str>,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), ReceiptValidationError> {
    if let Some(value) = value {
        validate_text(value, max_bytes, field)?;
    }
    Ok(())
}

fn validate_json(
    value: &JsonValue,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), ReceiptValidationError> {
    let encoded = serde_json::to_vec(value).map_err(|error| ReceiptValidationError::Json {
        field,
        message: error.to_string(),
    })?;
    if encoded.len() > max_bytes {
        return Err(ReceiptValidationError::TooLong {
            field,
            max: max_bytes,
        });
    }
    validate_metadata_keys(value, field)
}

fn validate_metadata_keys(
    value: &JsonValue,
    field: &'static str,
) -> Result<(), ReceiptValidationError> {
    match value {
        JsonValue::Object(values) => {
            for (key, value) in values {
                if is_forbidden_metadata_key(key) {
                    return Err(ReceiptValidationError::RawPayloadKey {
                        field,
                        key: key.clone(),
                    });
                }
                validate_metadata_keys(value, field)?;
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                validate_metadata_keys(value, field)?;
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
    Ok(())
}

/// Returns whether a metadata key is reserved for raw tool/provider data.
///
/// The predicate is shared with hook evidence validation. Comparison ignores
/// case and separators, so `tool_output`, `Tool-Output`, and `toolOutput` are
/// treated identically. Unknown keys remain valid and can be carried by newer
/// receipt schemas.
pub fn is_forbidden_metadata_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "stdout"
            | "stderr"
            | "args"
            | "arguments"
            | "ciphertext"
            | "encryptedcontent"
            | "payload"
            | "raw"
            | "rawpayload"
            | "toolinput"
            | "tooloutput"
            | "toolresponse"
            | "path"
            | "paths"
            | "cwd"
            | "workdir"
            | "command"
            | "argv"
            | "env"
            | "environment"
            | "output"
            | "aggregatedoutput"
            | "rawoutput"
    )
}

/// Validation failures are stable enough for callers to show a bounded error,
/// but do not include metadata contents or raw payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptValidationError {
    Empty {
        field: &'static str,
    },
    TooLong {
        field: &'static str,
        max: usize,
    },
    TooMany {
        field: &'static str,
        max: usize,
    },
    Nul {
        field: &'static str,
    },
    Json {
        field: &'static str,
        message: String,
    },
    RawPayloadKey {
        field: &'static str,
        key: String,
    },
}

impl fmt::Display for ReceiptValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "{field} must not be empty"),
            Self::TooLong { field, max } => write!(formatter, "{field} exceeds {max} bytes"),
            Self::TooMany { field, max } => write!(formatter, "{field} exceeds {max} entries"),
            Self::Nul { field } => write!(formatter, "{field} must not contain NUL"),
            Self::Json { field, message } => write!(formatter, "invalid {field}: {message}"),
            Self::RawPayloadKey { field, key } => {
                write!(formatter, "{field} contains forbidden raw key `{key}`")
            }
        }
    }
}

impl std::error::Error for ReceiptValidationError {}
