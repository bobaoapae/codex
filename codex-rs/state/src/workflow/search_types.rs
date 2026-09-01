//! Closed search projection types and decoding helpers.

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use super::types::*;

/// The only content classes that may enter the local full-text index.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchSourceKind {
    User,
    FinalAssistant,
    CompactionSummary,
    ApprovedPlan,
    ReceiptMetadata,
}

impl SearchSourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::FinalAssistant => "finalAssistant",
            Self::CompactionSummary => "compactionSummary",
            Self::ApprovedPlan => "approvedPlan",
            Self::ReceiptMetadata => "receiptMetadata",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "finalAssistant" => Ok(Self::FinalAssistant),
            "compactionSummary" => Ok(Self::CompactionSummary),
            "approvedPlan" => Ok(Self::ApprovedPlan),
            "receiptMetadata" => Ok(Self::ReceiptMetadata),
            _ => bail!("source kind is not allowlisted: {value}"),
        }
    }
}

impl fmt::Display for SearchSourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SearchSourceKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_str(value)
    }
}

/// Closed and bounded metadata attached to an indexed item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchDocumentMetadata {
    pub root_thread_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: Option<String>,
    pub provider: Option<String>,
    pub thread_class: Option<WorkflowThreadClass>,
    pub outcome: Option<String>,
    #[serde(default)]
    pub archived: bool,
    pub event_time_ms: Option<i64>,
}

pub type SearchMetadata = SearchDocumentMetadata;

impl SearchDocumentMetadata {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_optional_text(
            self.root_thread_id.as_deref(),
            MAX_ID_BYTES,
            "search root thread id",
        )?;
        validate_optional_text(
            self.project_id.as_deref(),
            MAX_SOURCE_ID_BYTES,
            "search project id",
        )?;
        validate_optional_text(self.cwd.as_deref(), MAX_PATH_BYTES, "search cwd")?;
        validate_optional_text(self.provider.as_deref(), MAX_ID_BYTES, "search provider")?;
        if let Some(outcome) = self.outcome.as_deref() {
            validate_nonempty_bounded(outcome, MAX_STATUS_BYTES, "search outcome")?;
        }
        validate_optional_nonnegative_i64(self.event_time_ms, "search event time")?;
        validate_json_bytes(&serde_json::to_string(self)?, "search metadata")
    }

    pub fn from_json(value: Value) -> Result<Self> {
        let metadata: Self = serde_json::from_value(value)?;
        metadata.validate()?;
        Ok(metadata)
    }

    pub(crate) fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }
}

/// One immutable search generation descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchGeneration {
    pub generation_id: i64,
    pub state: String,
    pub created_at_ms: i64,
    pub published_at_ms: Option<i64>,
    pub document_count: i64,
    pub source_watermark: Option<String>,
}

/// Input for one bounded document in a building generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDocumentCreate {
    pub generation_id: i64,
    pub thread_id: String,
    pub source_id: String,
    pub source_kind: SearchSourceKind,
    pub ordinal: i64,
    pub content: String,
    pub metadata: SearchMetadata,
}

/// Input for one bounded document in the mutable live overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSearchDocumentCreate {
    pub thread_id: String,
    pub source_id: String,
    pub source_kind: SearchSourceKind,
    pub ordinal: i64,
    pub content: String,
    pub metadata: SearchMetadata,
}

/// One indexed search document. Full content is intentionally not returned.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchDocument {
    pub document_id: i64,
    pub generation_id: i64,
    pub thread_id: String,
    pub source_id: String,
    pub source_kind: SearchSourceKind,
    pub ordinal: i64,
    pub metadata: SearchMetadata,
    pub created_at_ms: i64,
    pub snippet: Option<String>,
    pub rank: Option<f64>,
    pub is_live: bool,
}

/// Bounded filters accepted by the local search projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilter {
    pub thread_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: Option<String>,
    pub provider: Option<String>,
    pub thread_class: Option<WorkflowThreadClass>,
    pub outcome: Option<String>,
    pub archived: Option<bool>,
    #[serde(default)]
    pub source_kinds: Vec<SearchSourceKind>,
    #[serde(default = "default_include_live")]
    pub include_live: bool,
}

fn default_include_live() -> bool {
    true
}

impl Default for SearchFilter {
    fn default() -> Self {
        Self {
            thread_id: None,
            root_thread_id: None,
            project_id: None,
            cwd: None,
            provider: None,
            thread_class: None,
            outcome: None,
            archived: None,
            source_kinds: Vec::new(),
            include_live: true,
        }
    }
}

impl SearchFilter {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_optional_text(self.thread_id.as_deref(), MAX_ID_BYTES, "search thread id")?;
        validate_optional_text(
            self.root_thread_id.as_deref(),
            MAX_ID_BYTES,
            "search root thread id filter",
        )?;
        validate_optional_text(
            self.project_id.as_deref(),
            MAX_SOURCE_ID_BYTES,
            "search project id filter",
        )?;
        validate_optional_text(self.cwd.as_deref(), MAX_PATH_BYTES, "search cwd filter")?;
        validate_optional_text(
            self.provider.as_deref(),
            MAX_ID_BYTES,
            "search provider filter",
        )?;
        if let Some(outcome) = self.outcome.as_deref() {
            validate_nonempty_bounded(outcome, MAX_STATUS_BYTES, "search outcome filter")?;
        }
        if self.source_kinds.len() > MAX_SEARCH_FILTERS {
            bail!("search source kind filter exceeds {MAX_SEARCH_FILTERS} values");
        }
        Ok(())
    }
}

/// A bounded search request with an opaque keyset cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub filter: SearchFilter,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

fn default_search_limit() -> u32 {
    50
}

impl SearchRequest {
    pub fn new(
        query: impl Into<String>,
        filter: SearchFilter,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<Self> {
        let request = Self {
            query: query.into(),
            filter,
            cursor,
            limit,
        };
        request.validate()?;
        Ok(request)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_text(&self.query, MAX_SEARCH_QUERY_BYTES, "search query")?;
        if self.query.trim().is_empty() {
            bail!("search query must contain non-whitespace text");
        }
        if self.query.contains('\0') {
            bail!("search query must not contain NUL");
        }
        validate_page_size(self.limit)?;
        self.filter.validate()?;
        if let Some(cursor) = self.cursor.as_deref() {
            validate_text(cursor, MAX_JSON_BYTES, "search cursor")?;
        }
        Ok(())
    }
}

/// Keyset cursor bound to query, filters, generation and live epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCursor {
    pub generation_id: Option<i64>,
    pub live_epoch: i64,
    pub query: String,
    pub filter: SearchFilter,
    pub rank: f64,
    pub is_live: bool,
    pub document_id: i64,
}

impl SearchCursor {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(generation_id) = self.generation_id {
            validate_positive_i64(generation_id, "search cursor generation id")?;
        }
        validate_nonnegative_i64(self.live_epoch, "search cursor live epoch")?;
        validate_text(&self.query, MAX_SEARCH_QUERY_BYTES, "search cursor query")?;
        self.filter.validate()?;
        if !self.rank.is_finite() {
            bail!("search cursor rank must be finite");
        }
        validate_positive_i64(self.document_id, "search cursor document id")
    }

    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let encoded = serde_json::to_string(self)?;
        validate_text(&encoded, MAX_JSON_BYTES, "search cursor")?;
        Ok(encoded)
    }

    pub fn decode(encoded: &str) -> Result<Self> {
        validate_text(encoded, MAX_JSON_BYTES, "search cursor")?;
        let cursor: Self = serde_json::from_str(encoded)
            .map_err(|error| anyhow::anyhow!("invalid search cursor: {error}"))?;
        cursor.validate()?;
        Ok(cursor)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub documents: Vec<SearchDocument>,
    pub next_cursor: Option<String>,
    pub generation_id: Option<i64>,
    pub live_epoch: i64,
}

pub(super) fn validate_search_document(input: &SearchDocumentCreate) -> Result<()> {
    validate_positive_i64(input.generation_id, "generation id")?;
    validate_text(&input.thread_id, MAX_ID_BYTES, "thread id")?;
    validate_text(&input.source_id, MAX_SOURCE_ID_BYTES, "source id")?;
    validate_nonnegative_i64(input.ordinal, "document ordinal")?;
    validate_text(&input.content, MAX_SEARCH_CONTENT_BYTES, "search content")?;
    if input.content.contains('\0') {
        bail!("search content must not contain NUL");
    }
    input.metadata.validate()
}

pub(super) fn validate_live_search_document(input: &LiveSearchDocumentCreate) -> Result<()> {
    validate_text(&input.thread_id, MAX_ID_BYTES, "live thread id")?;
    validate_text(&input.source_id, MAX_SOURCE_ID_BYTES, "live source id")?;
    validate_nonnegative_i64(input.ordinal, "live document ordinal")?;
    validate_text(
        &input.content,
        MAX_SEARCH_CONTENT_BYTES,
        "live search content",
    )?;
    if input.content.contains('\0') {
        bail!("live search content must not contain NUL");
    }
    input.metadata.validate()
}

pub(super) fn search_generation_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<SearchGeneration> {
    Ok(SearchGeneration {
        generation_id: row.try_get("generation_id")?,
        state: row.try_get("state")?,
        created_at_ms: row.try_get("created_at_ms")?,
        published_at_ms: row.try_get("published_at_ms")?,
        document_count: row.try_get("document_count")?,
        source_watermark: row.try_get("source_watermark")?,
    })
}

pub(super) fn search_document_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<SearchDocument> {
    let source_kind =
        SearchSourceKind::from_str(row.try_get::<String, _>("source_kind")?.as_str())?;
    let metadata = parse_search_metadata(row.try_get("metadata_json")?)?;
    let snippet = row
        .try_get::<Option<String>, _>("snippet")
        .ok()
        .flatten()
        .map(|value| bound_snippet(&value));
    let rank = row.try_get::<Option<f64>, _>("rank").ok().flatten();
    let is_live = row
        .try_get::<i64, _>("is_live")
        .map(|value| value != 0)
        .unwrap_or(false);
    Ok(SearchDocument {
        document_id: row.try_get("document_id")?,
        generation_id: row.try_get("generation_id")?,
        thread_id: row.try_get("thread_id")?,
        source_id: row.try_get("source_id")?,
        source_kind,
        ordinal: row.try_get("ordinal")?,
        metadata,
        created_at_ms: row.try_get("created_at_ms")?,
        snippet,
        rank,
        is_live,
    })
}

pub(super) fn parse_search_metadata(value: Option<String>) -> Result<SearchDocumentMetadata> {
    let Some(value) = value else {
        return Ok(SearchDocumentMetadata::default());
    };
    validate_json_bytes(&value, "search metadata")?;
    SearchDocumentMetadata::from_json(serde_json::from_str(&value)?)
}

pub(super) fn bound_snippet(value: &str) -> String {
    let mut end = value.len().min(MAX_SEARCH_SNIPPET_BYTES);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

pub(super) fn append_filter_conditions(builder: &mut QueryBuilder<Sqlite>, filter: &SearchFilter) {
    if let Some(value) = filter.thread_id.as_deref() {
        builder.push(" AND d.thread_id = ").push_bind(value);
    }
    if let Some(value) = filter.root_thread_id.as_deref() {
        builder.push(" AND d.root_thread_id = ").push_bind(value);
    }
    if let Some(value) = filter.project_id.as_deref() {
        builder.push(" AND d.project_id = ").push_bind(value);
    }
    if let Some(value) = filter.cwd.as_deref() {
        builder.push(" AND d.cwd = ").push_bind(value);
    }
    if let Some(value) = filter.provider.as_deref() {
        builder.push(" AND d.provider = ").push_bind(value);
    }
    if let Some(value) = filter.thread_class {
        builder
            .push(" AND d.thread_class = ")
            .push_bind(value.as_str());
    }
    if let Some(value) = filter.outcome.as_deref() {
        builder.push(" AND d.outcome = ").push_bind(value);
    }
    // `archived` is a mutable thread property, not a generation property.
    // The indexed value remains metadata for inspection, while app-server
    // applies the current archive state after hydrating the thread.
    if !filter.source_kinds.is_empty() {
        builder.push(" AND d.source_kind IN (");
        let mut separated = builder.separated(", ");
        for value in &filter.source_kinds {
            separated.push_bind(value.as_str());
        }
        separated.push_unseparated(")");
    }
}

pub(super) fn compare_search_documents(left: &SearchDocument, right: &SearchDocument) -> Ordering {
    left.rank
        .unwrap_or(f64::MAX)
        .total_cmp(&right.rank.unwrap_or(f64::MAX))
        .then_with(|| left.is_live.cmp(&right.is_live))
        .then_with(|| left.document_id.cmp(&right.document_id))
}

pub(super) fn generation_document_matches(
    row: &sqlx::sqlite::SqliteRow,
    input: &SearchDocumentCreate,
    metadata_json: &str,
) -> Result<bool> {
    Ok(row.try_get::<String, _>("thread_id")? == input.thread_id
        && row.try_get::<i64, _>("ordinal")? == input.ordinal
        && row.try_get::<String, _>("content")? == input.content
        && row
            .try_get::<Option<String>, _>("metadata_json")?
            .as_deref()
            == Some(metadata_json)
        && row.try_get::<Option<String>, _>("root_thread_id")? == input.metadata.root_thread_id
        && row.try_get::<Option<String>, _>("project_id")? == input.metadata.project_id
        && row.try_get::<Option<String>, _>("cwd")? == input.metadata.cwd
        && row.try_get::<Option<String>, _>("provider")? == input.metadata.provider
        && row.try_get::<Option<String>, _>("thread_class")?
            == input
                .metadata
                .thread_class
                .map(WorkflowThreadClass::as_str)
                .map(str::to_string)
        && row.try_get::<Option<String>, _>("outcome")? == input.metadata.outcome
        && row.try_get::<i64, _>("archived")? == bool_as_sql(input.metadata.archived)
        && row.try_get::<Option<i64>, _>("event_time_ms")? == input.metadata.event_time_ms)
}

pub(super) fn bool_as_sql(value: bool) -> i64 {
    if value { 1 } else { 0 }
}
