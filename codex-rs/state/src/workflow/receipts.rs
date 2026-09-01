//! Durable, bounded workflow receipts.
//!
//! Receipts are metadata-only projections.  They identify an observed fact
//! and point at canonical rollout or artifact identifiers, but deliberately do
//! not contain command output, stdout, stderr, tool arguments, or other raw
//! execution payloads.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use std::collections::HashSet;

use super::WorkflowStore;
use super::types::*;

const MAX_RECEIPT_TAGS: usize = 32;
const MAX_RECEIPT_TAG_KEY_BYTES: usize = 64;
const MAX_RECEIPT_TAG_VALUE_BYTES: usize = 256;
const MAX_RECEIPT_REFERENCES: usize = 64;
const MAX_RECEIPT_REFERENCE_KIND_BYTES: usize = 64;
const MAX_RECEIPT_REFERENCE_ID_BYTES: usize = 256;
const MAX_EXPORT_RECEIPTS: usize = 200;

/// One bounded tag attached to a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowReceiptTag {
    pub key: String,
    pub value: String,
}

/// Opaque reference to a canonical rollout item or artifact.
///
/// The workflow store does not interpret the kind or identifier.  Consumers
/// resolve references through their owning store, which keeps raw output out
/// of the receipt projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowReceiptReference {
    pub kind: String,
    pub id: String,
}

/// Immutable receipt input.  `created_at_ms` is optional so retrying the same
/// logical receipt can omit wall-clock data; retries with the same ID compare
/// all content fields and return the existing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReceiptCreate {
    pub receipt_id: String,
    pub run_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub job_id: Option<String>,
    pub plan_snapshot_id: Option<String>,
    pub schema_version: i64,
    pub kind: String,
    pub subject: String,
    pub status: String,
    pub source: String,
    pub provenance: Option<Value>,
    pub tags: Vec<WorkflowReceiptTag>,
    pub payload: Option<Value>,
    pub references: Vec<WorkflowReceiptReference>,
    pub created_at_ms: Option<i64>,
}

/// One durable receipt as read from the workflow store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReceipt {
    pub receipt_id: String,
    pub run_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub job_id: Option<String>,
    pub plan_snapshot_id: Option<String>,
    pub schema_version: i64,
    pub kind: String,
    pub subject: String,
    pub status: String,
    pub source: String,
    pub provenance: Option<Value>,
    pub tags: Vec<WorkflowReceiptTag>,
    pub payload: Option<Value>,
    pub references: Vec<WorkflowReceiptReference>,
    pub created_at_ms: i64,
}

impl WorkflowReceipt {
    fn has_same_content(&self, input: &NormalizedReceipt<'_>) -> bool {
        self.receipt_id == input.receipt_id
            && self.run_id.as_ref() == input.run_id.as_ref()
            && self.thread_id.as_ref() == input.thread_id.as_ref()
            && self.turn_id.as_ref() == input.turn_id.as_ref()
            && self.job_id.as_ref() == input.job_id.as_ref()
            && self.plan_snapshot_id.as_ref() == input.plan_snapshot_id.as_ref()
            && self.schema_version == input.schema_version
            && self.kind == input.kind
            && self.subject == input.subject
            && self.status == input.status
            && self.source == input.source
            && self.provenance.as_ref() == input.provenance.as_ref()
            && self.tags.as_slice() == input.tags
            && self.payload.as_ref() == input.payload.as_ref()
            && self.references.as_slice() == input.references
    }
}

/// Filters for receipt listing.  A cursor embeds this value, so a cursor
/// cannot accidentally be reused with a different selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkflowReceiptFilter {
    pub thread_id: Option<String>,
    pub job_id: Option<String>,
    pub plan_snapshot_id: Option<String>,
    pub status: Option<String>,
    pub kind: Option<String>,
}

/// Keyset cursor ordered by `(created_at_ms, receipt_id)` descending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowReceiptCursor {
    pub created_at_ms: i64,
    pub receipt_id: String,
    pub filter: WorkflowReceiptFilter,
}

impl WorkflowReceiptCursor {
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let encoded = serde_json::to_string(self)?;
        validate_text(&encoded, MAX_JSON_BYTES, "receipt cursor")?;
        Ok(encoded)
    }

    pub fn decode(encoded: &str) -> Result<Self> {
        validate_text(encoded, MAX_JSON_BYTES, "receipt cursor")?;
        let cursor: Self = serde_json::from_str(encoded)
            .map_err(|error| anyhow::anyhow!("invalid receipt cursor: {error}"))?;
        cursor.validate()?;
        Ok(cursor)
    }

    fn validate(&self) -> Result<()> {
        validate_nonnegative_i64(self.created_at_ms, "receipt cursor timestamp")?;
        validate_text(&self.receipt_id, MAX_ID_BYTES, "receipt cursor id")?;
        validate_receipt_filter(&self.filter)
    }
}

/// Bounded receipt listing request.  Callers choose the page size explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReceiptListRequest {
    pub filter: WorkflowReceiptFilter,
    pub cursor: Option<WorkflowReceiptCursor>,
    pub limit: u32,
}

impl WorkflowReceiptListRequest {
    pub fn new(
        filter: WorkflowReceiptFilter,
        cursor: Option<WorkflowReceiptCursor>,
        limit: u32,
    ) -> Result<Self> {
        let request = Self {
            filter,
            cursor,
            limit,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        validate_page_size(self.limit)?;
        validate_receipt_filter(&self.filter)?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
            if cursor.filter != self.filter {
                bail!("receipt cursor is stale or incompatible with filters");
            }
        }
        Ok(())
    }
}

/// One page of receipts and an optional keyset cursor for the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReceiptPage {
    pub receipts: Vec<WorkflowReceipt>,
    pub next_cursor: Option<WorkflowReceiptCursor>,
}

/// Explicit receipt IDs selected for a later redacted export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReceiptExportSelection {
    pub receipt_ids: Vec<String>,
}

impl WorkflowReceiptExportSelection {
    fn validate(&self) -> Result<()> {
        if self.receipt_ids.is_empty() {
            bail!("receipt export selection must not be empty");
        }
        if self.receipt_ids.len() > MAX_EXPORT_RECEIPTS {
            bail!("receipt export selection exceeds {MAX_EXPORT_RECEIPTS} receipts");
        }
        let mut unique = HashSet::with_capacity(self.receipt_ids.len());
        for receipt_id in &self.receipt_ids {
            validate_receipt_text(receipt_id, MAX_ID_BYTES, "receipt id")?;
            if !unique.insert(receipt_id) {
                bail!("receipt export selection contains duplicate receipt id");
            }
        }
        Ok(())
    }
}

struct NormalizedReceipt<'a> {
    receipt_id: &'a str,
    run_id: &'a Option<String>,
    thread_id: &'a Option<String>,
    turn_id: &'a Option<String>,
    job_id: &'a Option<String>,
    plan_snapshot_id: &'a Option<String>,
    schema_version: i64,
    kind: &'a str,
    subject: &'a str,
    status: &'a str,
    source: &'a str,
    provenance: &'a Option<Value>,
    tags: &'a [WorkflowReceiptTag],
    payload: &'a Option<Value>,
    references: &'a [WorkflowReceiptReference],
    created_at_ms: i64,
}

impl<'a> NormalizedReceipt<'a> {
    fn from_input(input: &'a WorkflowReceiptCreate) -> Result<Self> {
        validate_receipt_create(input)?;
        Ok(Self {
            receipt_id: &input.receipt_id,
            run_id: &input.run_id,
            thread_id: &input.thread_id,
            turn_id: &input.turn_id,
            job_id: &input.job_id,
            plan_snapshot_id: &input.plan_snapshot_id,
            schema_version: input.schema_version,
            kind: &input.kind,
            subject: &input.subject,
            status: &input.status,
            source: &input.source,
            provenance: &input.provenance,
            tags: &input.tags,
            payload: &input.payload,
            references: &input.references,
            created_at_ms: input.created_at_ms.unwrap_or_else(now_ms),
        })
    }
}

impl WorkflowStore {
    /// Insert one receipt atomically, or return the existing row for an
    /// idempotent retry with the same receipt ID and content.
    pub async fn insert_receipt(&self, input: &WorkflowReceiptCreate) -> Result<WorkflowReceipt> {
        let input = NormalizedReceipt::from_input(input)?;
        let provenance_json = serialize_receipt_json(input.provenance, "receipt provenance")?;
        let tags_json = serialize_receipt_tags(input.tags)?;
        let payload_json = serialize_receipt_json(input.payload, "receipt payload")?;
        let references_json = serialize_receipt_references(input.references)?;

        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO workflow_receipts
                (receipt_id, run_id, thread_id, turn_id, job_id, plan_snapshot_id,
                 schema_version, kind, subject, status, source, provenance_json,
                 tags_json, payload_json, references_json, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(receipt_id) DO NOTHING",
        )
        .bind(input.receipt_id)
        .bind(input.run_id)
        .bind(input.thread_id)
        .bind(input.turn_id)
        .bind(input.job_id)
        .bind(input.plan_snapshot_id)
        .bind(input.schema_version)
        .bind(input.kind)
        .bind(input.subject)
        .bind(input.status)
        .bind(input.source)
        .bind(provenance_json)
        .bind(tags_json)
        .bind(payload_json)
        .bind(references_json)
        .bind(input.created_at_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        let row = sqlx::query(receipt_select_by_id_sql())
            .bind(input.receipt_id)
            .fetch_one(&mut *tx)
            .await?;
        let receipt = receipt_from_row(&row)?;
        tx.commit().await?;
        if !inserted && !receipt.has_same_content(&input) {
            bail!(
                "receipt id {} already exists with different content",
                input.receipt_id
            );
        }
        Ok(receipt)
    }

    /// Return one receipt by its opaque ID.
    pub async fn get_receipt(&self, receipt_id: &str) -> Result<Option<WorkflowReceipt>> {
        validate_text(receipt_id, MAX_ID_BYTES, "receipt id")?;
        sqlx::query(receipt_select_by_id_sql())
            .bind(receipt_id)
            .fetch_optional(self.pool.as_ref())
            .await?
            .as_ref()
            .map(receipt_from_row)
            .transpose()
    }

    /// List receipts in stable descending `(created_at_ms, receipt_id)` order.
    pub async fn list_receipts(
        &self,
        request: &WorkflowReceiptListRequest,
    ) -> Result<WorkflowReceiptPage> {
        request.validate()?;
        let fetch_limit = i64::from(request.limit.saturating_add(1));
        let mut builder = QueryBuilder::<Sqlite>::new(receipt_select_sql());
        builder.push(" WHERE 1 = 1");
        append_receipt_filter(&mut builder, &request.filter);
        if let Some(cursor) = &request.cursor {
            builder
                .push(" AND (created_at_ms < ")
                .push_bind(cursor.created_at_ms)
                .push(" OR (created_at_ms = ")
                .push_bind(cursor.created_at_ms)
                .push(" AND receipt_id < ")
                .push_bind(&cursor.receipt_id)
                .push("))");
        }
        builder
            .push(" ORDER BY created_at_ms DESC, receipt_id DESC LIMIT ")
            .push_bind(fetch_limit);
        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let has_more = rows.len() > request.limit as usize;
        let mut receipts = rows
            .iter()
            .map(receipt_from_row)
            .collect::<Result<Vec<_>>>()?;
        if has_more {
            receipts.truncate(request.limit as usize);
        }
        let next_cursor =
            has_more
                .then(|| receipts.last())
                .flatten()
                .map(|receipt| WorkflowReceiptCursor {
                    created_at_ms: receipt.created_at_ms,
                    receipt_id: receipt.receipt_id.clone(),
                    filter: request.filter.clone(),
                });
        Ok(WorkflowReceiptPage {
            receipts,
            next_cursor,
        })
    }

    /// Select only explicitly named receipts for a later export operation.
    ///
    /// This method has no export side effects and never returns an implicit
    /// "all receipts" selection.
    pub async fn select_receipts_for_export(
        &self,
        selection: &WorkflowReceiptExportSelection,
    ) -> Result<Vec<WorkflowReceipt>> {
        selection.validate()?;
        let mut builder = QueryBuilder::<Sqlite>::new(receipt_select_sql());
        builder.push(" WHERE receipt_id IN (");
        let mut separated = builder.separated(", ");
        for receipt_id in &selection.receipt_ids {
            separated.push_bind(receipt_id);
        }
        separated.push_unseparated(") ORDER BY created_at_ms DESC, receipt_id DESC");
        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let receipts = rows
            .iter()
            .map(receipt_from_row)
            .collect::<Result<Vec<_>>>()?;
        if receipts.len() != selection.receipt_ids.len() {
            bail!("receipt export selection contains an unknown receipt id");
        }
        Ok(receipts)
    }
}

fn receipt_select_sql() -> &'static str {
    "SELECT receipt_id, run_id, thread_id, turn_id, job_id, plan_snapshot_id,
            schema_version, kind, subject, status, source, provenance_json,
            tags_json, payload_json, references_json, created_at_ms
     FROM workflow_receipts"
}

fn receipt_select_by_id_sql() -> &'static str {
    "SELECT receipt_id, run_id, thread_id, turn_id, job_id, plan_snapshot_id,
            schema_version, kind, subject, status, source, provenance_json,
            tags_json, payload_json, references_json, created_at_ms
     FROM workflow_receipts WHERE receipt_id = ?"
}

fn append_receipt_filter(builder: &mut QueryBuilder<Sqlite>, filter: &WorkflowReceiptFilter) {
    if let Some(thread_id) = filter.thread_id.as_deref() {
        builder.push(" AND thread_id = ").push_bind(thread_id);
    }
    if let Some(job_id) = filter.job_id.as_deref() {
        builder.push(" AND job_id = ").push_bind(job_id);
    }
    if let Some(plan_snapshot_id) = filter.plan_snapshot_id.as_deref() {
        builder
            .push(" AND plan_snapshot_id = ")
            .push_bind(plan_snapshot_id);
    }
    if let Some(status) = filter.status.as_deref() {
        builder.push(" AND status = ").push_bind(status);
    }
    if let Some(kind) = filter.kind.as_deref() {
        builder.push(" AND kind = ").push_bind(kind);
    }
}

fn validate_receipt_filter(filter: &WorkflowReceiptFilter) -> Result<()> {
    validate_receipt_optional_text(
        filter.thread_id.as_deref(),
        MAX_ID_BYTES,
        "receipt thread id",
    )?;
    validate_receipt_optional_text(filter.job_id.as_deref(), MAX_ID_BYTES, "receipt job id")?;
    validate_receipt_optional_text(
        filter.plan_snapshot_id.as_deref(),
        MAX_SOURCE_ID_BYTES,
        "receipt plan snapshot id",
    )?;
    validate_receipt_optional_text(
        filter.status.as_deref(),
        MAX_STATUS_BYTES,
        "receipt status filter",
    )?;
    validate_receipt_optional_text(filter.kind.as_deref(), MAX_ID_BYTES, "receipt kind filter")
}

fn validate_receipt_create(input: &WorkflowReceiptCreate) -> Result<()> {
    validate_receipt_text(&input.receipt_id, MAX_ID_BYTES, "receipt id")?;
    validate_receipt_optional_text(input.run_id.as_deref(), MAX_ID_BYTES, "receipt run id")?;
    validate_receipt_optional_text(
        input.thread_id.as_deref(),
        MAX_ID_BYTES,
        "receipt thread id",
    )?;
    validate_receipt_optional_text(input.turn_id.as_deref(), MAX_ID_BYTES, "receipt turn id")?;
    validate_receipt_optional_text(input.job_id.as_deref(), MAX_ID_BYTES, "receipt job id")?;
    validate_receipt_optional_text(
        input.plan_snapshot_id.as_deref(),
        MAX_SOURCE_ID_BYTES,
        "receipt plan snapshot id",
    )?;
    if !(1..=i64::from(i32::MAX)).contains(&input.schema_version) {
        bail!("receipt schema version must be between 1 and {}", i32::MAX);
    }
    validate_receipt_text(&input.kind, MAX_ID_BYTES, "receipt kind")?;
    validate_receipt_text(&input.subject, 4_096, "receipt subject")?;
    validate_receipt_text(&input.status, MAX_STATUS_BYTES, "receipt status")?;
    validate_receipt_text(&input.source, MAX_ID_BYTES, "receipt source")?;
    if input.created_at_ms.is_some_and(|value| value < 0) {
        bail!("receipt created timestamp must be non-negative");
    }
    validate_receipt_json(input.provenance.as_ref(), "receipt provenance")?;
    validate_receipt_json(input.payload.as_ref(), "receipt payload")?;
    validate_receipt_tags(&input.tags)?;
    validate_receipt_references(&input.references)
}

fn validate_receipt_json(value: Option<&Value>, name: &str) -> Result<()> {
    if let Some(value) = value {
        let encoded = serde_json::to_string(value)?;
        validate_json_bytes(&encoded, name)?;
        validate_receipt_metadata_keys(value, name)?;
    }
    Ok(())
}

fn validate_receipt_text(value: &str, max_bytes: usize, name: &str) -> Result<()> {
    validate_text(value, max_bytes, name)?;
    if value.trim().is_empty() {
        bail!("{name} must not be empty");
    }
    if value.contains('\0') {
        bail!("{name} must not contain NUL");
    }
    Ok(())
}

fn validate_receipt_optional_text(value: Option<&str>, max_bytes: usize, name: &str) -> Result<()> {
    if let Some(value) = value {
        validate_receipt_text(value, max_bytes, name)?;
    }
    Ok(())
}

fn serialize_receipt_json(value: &Option<Value>, name: &str) -> Result<Option<String>> {
    value
        .as_ref()
        .map(|value| {
            let encoded = serde_json::to_string(value)?;
            validate_json_bytes(&encoded, name)?;
            Ok(encoded)
        })
        .transpose()
}

fn validate_receipt_tags(tags: &[WorkflowReceiptTag]) -> Result<()> {
    if tags.len() > MAX_RECEIPT_TAGS {
        bail!("receipt tags exceed {MAX_RECEIPT_TAGS} entries");
    }
    let mut keys = HashSet::with_capacity(tags.len());
    for tag in tags {
        validate_receipt_text(&tag.key, MAX_RECEIPT_TAG_KEY_BYTES, "receipt tag key")?;
        validate_receipt_text(&tag.value, MAX_RECEIPT_TAG_VALUE_BYTES, "receipt tag value")?;
        if !keys.insert(&tag.key) {
            bail!("receipt tags contain duplicate key");
        }
    }
    Ok(())
}

fn serialize_receipt_tags(tags: &[WorkflowReceiptTag]) -> Result<Option<String>> {
    if tags.is_empty() {
        return Ok(None);
    }
    validate_receipt_tags(tags)?;
    let encoded = serde_json::to_string(tags)?;
    validate_json_bytes(&encoded, "receipt tags")?;
    Ok(Some(encoded))
}

fn validate_receipt_references(references: &[WorkflowReceiptReference]) -> Result<()> {
    if references.len() > MAX_RECEIPT_REFERENCES {
        bail!("receipt references exceed {MAX_RECEIPT_REFERENCES} entries");
    }
    for reference in references {
        validate_receipt_text(
            &reference.kind,
            MAX_RECEIPT_REFERENCE_KIND_BYTES,
            "receipt reference kind",
        )?;
        validate_receipt_text(
            &reference.id,
            MAX_RECEIPT_REFERENCE_ID_BYTES,
            "receipt reference id",
        )?;
    }
    Ok(())
}

fn serialize_receipt_references(references: &[WorkflowReceiptReference]) -> Result<Option<String>> {
    if references.is_empty() {
        return Ok(None);
    }
    validate_receipt_references(references)?;
    let encoded = serde_json::to_string(references)?;
    validate_json_bytes(&encoded, "receipt references")?;
    Ok(Some(encoded))
}

fn receipt_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<WorkflowReceipt> {
    Ok(WorkflowReceipt {
        receipt_id: row.try_get("receipt_id")?,
        run_id: row.try_get("run_id")?,
        thread_id: row.try_get("thread_id")?,
        turn_id: row.try_get("turn_id")?,
        job_id: row.try_get("job_id")?,
        plan_snapshot_id: row.try_get("plan_snapshot_id")?,
        schema_version: row.try_get("schema_version")?,
        kind: row.try_get("kind")?,
        subject: row.try_get("subject")?,
        status: row.try_get("status")?,
        source: row.try_get("source")?,
        provenance: parse_receipt_json(row.try_get("provenance_json")?, "receipt provenance")?,
        tags: parse_receipt_tags(row.try_get("tags_json")?)?,
        payload: parse_receipt_json(row.try_get("payload_json")?, "receipt payload")?,
        references: parse_receipt_references(row.try_get("references_json")?)?,
        created_at_ms: row.try_get("created_at_ms")?,
    })
}

fn parse_receipt_json(value: Option<String>, name: &str) -> Result<Option<Value>> {
    value
        .map(|value| {
            validate_json_bytes(&value, name)?;
            Ok(serde_json::from_str(&value)?)
        })
        .transpose()
}

fn parse_receipt_tags(value: Option<String>) -> Result<Vec<WorkflowReceiptTag>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    validate_json_bytes(&value, "receipt tags")?;
    let tags: Vec<WorkflowReceiptTag> = serde_json::from_str(&value)?;
    validate_receipt_tags(&tags)?;
    Ok(tags)
}

fn parse_receipt_references(value: Option<String>) -> Result<Vec<WorkflowReceiptReference>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    validate_json_bytes(&value, "receipt references")?;
    let references: Vec<WorkflowReceiptReference> = serde_json::from_str(&value)?;
    validate_receipt_references(&references)?;
    Ok(references)
}

fn validate_receipt_metadata_keys(value: &Value, field: &str) -> Result<()> {
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                if is_raw_receipt_key(key) {
                    bail!("{field} contains forbidden raw metadata key `{key}`");
                }
                validate_receipt_metadata_keys(value, field)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_receipt_metadata_keys(value, field)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

fn is_raw_receipt_key(key: &str) -> bool {
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
            | "output"
            | "aggregatedoutput"
            | "rawoutput"
    )
}

#[cfg(test)]
#[path = "receipts_tests.rs"]
mod tests;
