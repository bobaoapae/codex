//! Search-generation, bounded FTS5 projection, and live-overlay operations.

use anyhow::Result;
use anyhow::bail;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

use super::WorkflowStore;
use super::search_types::*;
use super::types::*;

const SEARCH_FETCH_LIMIT: u32 = MAX_PAGE_SIZE + 1;

impl WorkflowStore {
    /// Report whether the migrated database contains the FTS5 virtual table.
    pub async fn fts5_available(&self) -> Result<bool> {
        let available = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'workflow_search_fts')",
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        Ok(available != 0)
    }

    /// Begin a new unpublished search generation with an optional watermark.
    pub async fn begin_search_generation(&self) -> Result<SearchGeneration> {
        self.begin_search_generation_with_watermark(None).await
    }

    pub async fn begin_search_generation_with_watermark(
        &self,
        source_watermark: Option<&str>,
    ) -> Result<SearchGeneration> {
        validate_optional_text(source_watermark, MAX_SEARCH_QUERY_BYTES, "source watermark")?;
        let row = sqlx::query(
            "INSERT INTO workflow_search_generations (state, created_at_ms, source_watermark)
             VALUES ('building', ?, ?)
             RETURNING generation_id, state, created_at_ms, published_at_ms, document_count, source_watermark",
        )
        .bind(now_ms())
        .bind(source_watermark)
        .fetch_one(self.pool.as_ref())
        .await?;
        search_generation_from_row(&row)
    }

    /// Publish a complete snapshot and atomically switch the active pointer.
    pub async fn publish_search_generation(&self, generation_id: i64) -> Result<bool> {
        validate_positive_i64(generation_id, "generation id")?;
        let mut tx = self.pool.begin().await?;
        let published_at_ms = now_ms();
        let result = sqlx::query(
            "UPDATE workflow_search_generations
             SET state = 'published', published_at_ms = ?
             WHERE generation_id = ? AND state = 'building'",
        )
        .bind(published_at_ms)
        .bind(generation_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE workflow_search_generations SET state = 'retired'
             WHERE state = 'published' AND generation_id <> ?",
        )
        .bind(generation_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_search_state
             SET active_generation_id = ?, updated_at_ms = ? WHERE id = 1",
        )
        .bind(generation_id)
        .bind(published_at_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn publish_search_generation_atomic(&self, generation_id: i64) -> Result<bool> {
        self.publish_search_generation(generation_id).await
    }

    pub async fn active_search_generation(&self) -> Result<Option<SearchGeneration>> {
        let row = sqlx::query(
            "SELECT g.generation_id, g.state, g.created_at_ms, g.published_at_ms,
                    g.document_count, g.source_watermark
             FROM workflow_search_state s
             JOIN workflow_search_generations g ON g.generation_id = s.active_generation_id
             WHERE s.id = 1 AND g.state = 'published'",
        )
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(search_generation_from_row).transpose()
    }

    /// Insert a document into a building generation. Exact repeats are no-ops;
    /// a different payload for the same source identity is rejected.
    pub async fn insert_search_document(
        &self,
        input: &SearchDocumentCreate,
    ) -> Result<SearchDocument> {
        validate_search_document(input)?;
        let metadata_json = input.metadata.to_json()?;
        let mut tx = self.pool.begin().await?;
        let state = sqlx::query_scalar::<_, String>(
            "SELECT state FROM workflow_search_generations WHERE generation_id = ?",
        )
        .bind(input.generation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if state.as_deref() != Some("building") {
            tx.rollback().await?;
            bail!("search generation {} is not building", input.generation_id);
        }
        let source_kind = input.source_kind.as_str();
        let inserted = sqlx::query(
            "INSERT INTO workflow_search_documents
                (generation_id, thread_id, source_id, source_kind, ordinal, content,
                 root_thread_id, project_id, cwd, provider, thread_class, outcome,
                 archived, event_time_ms, metadata_json, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(generation_id, source_id, source_kind) DO NOTHING
             RETURNING document_id, generation_id, thread_id, source_id, source_kind,
                       ordinal, metadata_json, created_at_ms,
                       NULL AS snippet, NULL AS rank, 0 AS is_live",
        )
        .bind(input.generation_id)
        .bind(&input.thread_id)
        .bind(&input.source_id)
        .bind(source_kind)
        .bind(input.ordinal)
        .bind(&input.content)
        .bind(&input.metadata.root_thread_id)
        .bind(&input.metadata.project_id)
        .bind(&input.metadata.cwd)
        .bind(&input.metadata.provider)
        .bind(input.metadata.thread_class.map(WorkflowThreadClass::as_str))
        .bind(&input.metadata.outcome)
        .bind(bool_as_sql(input.metadata.archived))
        .bind(input.metadata.event_time_ms)
        .bind(&metadata_json)
        .bind(now_ms())
        .fetch_optional(&mut *tx)
        .await?;
        let row = if let Some(row) = inserted {
            sqlx::query(
                "UPDATE workflow_search_generations
                 SET document_count = document_count + 1 WHERE generation_id = ?",
            )
            .bind(input.generation_id)
            .execute(&mut *tx)
            .await?;
            row
        } else {
            let row = sqlx::query(
                "SELECT document_id, generation_id, thread_id, source_id, source_kind,
                        ordinal, metadata_json, created_at_ms,
                        NULL AS snippet, NULL AS rank, 0 AS is_live,
                        content, root_thread_id, project_id, cwd, provider,
                        thread_class, outcome, archived, event_time_ms
                 FROM workflow_search_documents
                 WHERE generation_id = ? AND source_id = ? AND source_kind = ?",
            )
            .bind(input.generation_id)
            .bind(&input.source_id)
            .bind(source_kind)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| anyhow::anyhow!("search document disappeared during upsert"))?;
            if !generation_document_matches(&row, input, &metadata_json)? {
                tx.rollback().await?;
                bail!(
                    "conflicting search document for generation {}, source {}",
                    input.generation_id,
                    input.source_id
                );
            }
            row
        };
        tx.commit().await?;
        search_document_from_row(&row)
    }

    /// Upsert one item in the mutable live overlay. Identical repeats do not
    /// advance the epoch, so they cannot invalidate readers unnecessarily.
    pub async fn upsert_live_search_document(
        &self,
        input: &LiveSearchDocumentCreate,
    ) -> Result<SearchDocument> {
        validate_live_search_document(input)?;
        let metadata_json = input.metadata.to_json()?;
        let source_kind = input.source_kind.as_str();
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "INSERT INTO workflow_search_live_documents
                (thread_id, source_id, source_kind, ordinal, content,
                 root_thread_id, project_id, cwd, provider, thread_class, outcome,
                 archived, event_time_ms, metadata_json, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, source_id, source_kind) DO UPDATE SET
                 ordinal = excluded.ordinal, content = excluded.content,
                 root_thread_id = excluded.root_thread_id, project_id = excluded.project_id,
                 cwd = excluded.cwd, provider = excluded.provider,
                 thread_class = excluded.thread_class, outcome = excluded.outcome,
                 archived = excluded.archived, event_time_ms = excluded.event_time_ms,
                 metadata_json = excluded.metadata_json, updated_at_ms = excluded.updated_at_ms
             WHERE workflow_search_live_documents.ordinal IS NOT excluded.ordinal
                OR workflow_search_live_documents.content IS NOT excluded.content
                OR workflow_search_live_documents.root_thread_id IS NOT excluded.root_thread_id
                OR workflow_search_live_documents.project_id IS NOT excluded.project_id
                OR workflow_search_live_documents.cwd IS NOT excluded.cwd
                OR workflow_search_live_documents.provider IS NOT excluded.provider
                OR workflow_search_live_documents.thread_class IS NOT excluded.thread_class
                OR workflow_search_live_documents.outcome IS NOT excluded.outcome
                OR workflow_search_live_documents.archived IS NOT excluded.archived
                OR workflow_search_live_documents.event_time_ms IS NOT excluded.event_time_ms
                OR workflow_search_live_documents.metadata_json IS NOT excluded.metadata_json
             RETURNING live_document_id AS document_id, 0 AS generation_id, thread_id, source_id,
                       source_kind, ordinal, metadata_json, created_at_ms,
                       NULL AS snippet, NULL AS rank, 1 AS is_live",
        )
        .bind(&input.thread_id)
        .bind(&input.source_id)
        .bind(source_kind)
        .bind(input.ordinal)
        .bind(&input.content)
        .bind(&input.metadata.root_thread_id)
        .bind(&input.metadata.project_id)
        .bind(&input.metadata.cwd)
        .bind(&input.metadata.provider)
        .bind(input.metadata.thread_class.map(WorkflowThreadClass::as_str))
        .bind(&input.metadata.outcome)
        .bind(bool_as_sql(input.metadata.archived))
        .bind(input.metadata.event_time_ms)
        .bind(&metadata_json)
        .bind(now)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let changed = row.is_some();
        let row = if let Some(row) = row {
            row
        } else {
            sqlx::query(
                "SELECT live_document_id AS document_id, 0 AS generation_id, thread_id, source_id,
                        source_kind, ordinal, metadata_json, created_at_ms,
                        NULL AS snippet, NULL AS rank, 1 AS is_live
                 FROM workflow_search_live_documents
                 WHERE thread_id = ? AND source_id = ? AND source_kind = ?",
            )
            .bind(&input.thread_id)
            .bind(&input.source_id)
            .bind(source_kind)
            .fetch_one(&mut *tx)
            .await?
        };
        if changed {
            sqlx::query(
                "UPDATE workflow_search_live_state
                 SET live_epoch = live_epoch + 1, updated_at_ms = ? WHERE id = 1",
            )
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        search_document_from_row(&row)
    }

    pub async fn remove_live_search_document(
        &self,
        thread_id: &str,
        source_id: &str,
        source_kind: SearchSourceKind,
    ) -> Result<bool> {
        validate_text(thread_id, MAX_ID_BYTES, "live thread id")?;
        validate_text(source_id, MAX_SOURCE_ID_BYTES, "live source id")?;
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "DELETE FROM workflow_search_live_documents
             WHERE thread_id = ? AND source_id = ? AND source_kind = ?",
        )
        .bind(thread_id)
        .bind(source_id)
        .bind(source_kind.as_str())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 1 {
            sqlx::query(
                "UPDATE workflow_search_live_state
                 SET live_epoch = live_epoch + 1, updated_at_ms = ? WHERE id = 1",
            )
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn live_search_epoch(&self) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT live_epoch FROM workflow_search_live_state WHERE id = 1",
        )
        .fetch_one(self.pool.as_ref())
        .await?)
    }

    /// Legacy compact search wrapper; returned documents contain snippets only.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchDocument>> {
        let request = SearchRequest::new(query.to_string(), SearchFilter::default(), None, limit)?;
        Ok(self.search_page(&request).await?.documents)
    }

    /// Search the active immutable generation and, by default, live overlay.
    pub async fn search_page(&self, request: &SearchRequest) -> Result<SearchPage> {
        request.validate()?;
        if !self.fts5_available().await? {
            return Ok(SearchPage {
                documents: Vec::new(),
                next_cursor: None,
                generation_id: None,
                live_epoch: 0,
            });
        }
        let generation_id = self
            .active_search_generation()
            .await?
            .map(|g| g.generation_id);
        let live_epoch = self.live_search_epoch().await?;
        let cursor = request
            .cursor
            .as_deref()
            .map(SearchCursor::decode)
            .transpose()?;
        if let Some(cursor) = cursor.as_ref()
            && (cursor.generation_id != generation_id
                || cursor.live_epoch != live_epoch
                || cursor.query != request.query
                || cursor.filter != request.filter)
        {
            bail!("stale or incompatible search cursor");
        }
        let fetch_limit = i64::from(request.limit.saturating_add(1).min(SEARCH_FETCH_LIMIT));
        let mut documents = Vec::new();
        if let Some(generation_id) = generation_id {
            documents.extend(
                self.fetch_search_rows(
                    generation_id,
                    false,
                    &request.query,
                    &request.filter,
                    cursor.as_ref(),
                    fetch_limit,
                )
                .await?,
            );
        }
        if request.filter.include_live {
            documents.extend(
                self.fetch_search_rows(
                    generation_id.unwrap_or_default(),
                    true,
                    &request.query,
                    &request.filter,
                    cursor.as_ref(),
                    fetch_limit,
                )
                .await?,
            );
        }
        documents.sort_by(compare_search_documents);
        let has_more = documents.len() > request.limit as usize;
        documents.truncate(request.limit as usize);
        let next_cursor = if has_more {
            documents
                .last()
                .map(|document| {
                    SearchCursor {
                        generation_id,
                        live_epoch,
                        query: request.query.clone(),
                        filter: request.filter.clone(),
                        rank: document.rank.unwrap_or(f64::MAX),
                        is_live: document.is_live,
                        document_id: document.document_id,
                    }
                    .encode()
                })
                .transpose()?
        } else {
            None
        };
        Ok(SearchPage {
            documents,
            next_cursor,
            generation_id,
            live_epoch,
        })
    }

    async fn fetch_search_rows(
        &self,
        generation_id: i64,
        is_live: bool,
        query: &str,
        filter: &SearchFilter,
        cursor: Option<&SearchCursor>,
        limit: i64,
    ) -> Result<Vec<SearchDocument>> {
        let escaped_query = escape_fts5_literal(query)?;
        let (fts_table, documents_table, id_column) = if is_live {
            (
                "workflow_search_live_fts",
                "workflow_search_live_documents",
                "live_document_id",
            )
        } else {
            (
                "workflow_search_fts",
                "workflow_search_documents",
                "document_id",
            )
        };
        let mut builder = QueryBuilder::<Sqlite>::new(format!(
            "SELECT d.{id_column} AS document_id, {} AS generation_id,
                    d.thread_id, d.source_id, d.source_kind, d.ordinal,
                    d.metadata_json, d.created_at_ms,
                    snippet({fts_table}, -1, '[', ']', '…', 16) AS snippet,
                    bm25({fts_table}) AS rank, {} AS is_live
             FROM {fts_table} f JOIN {documents_table} d
               ON d.{id_column} = f.rowid WHERE {fts_table} MATCH ",
            if is_live { "0" } else { "d.generation_id" },
            if is_live { "1" } else { "0" },
        ));
        builder.push_bind(escaped_query);
        if !is_live {
            builder
                .push(" AND d.generation_id = ")
                .push_bind(generation_id);
        }
        append_filter_conditions(&mut builder, filter);
        if let Some(cursor) = cursor {
            builder
                .push(" AND (bm25(")
                .push(fts_table)
                .push(") > ")
                .push_bind(cursor.rank);
            builder
                .push(" OR (bm25(")
                .push(fts_table)
                .push(") = ")
                .push_bind(cursor.rank)
                .push(" AND ");
            if is_live == cursor.is_live {
                builder
                    .push("d.")
                    .push(id_column)
                    .push(" > ")
                    .push_bind(cursor.document_id);
            } else if is_live {
                builder.push("1");
            } else {
                builder.push("0");
            }
            builder.push("))");
        }
        builder
            .push(" ORDER BY bm25(")
            .push(fts_table)
            .push(") ASC, d.")
            .push(id_column)
            .push(" ASC LIMIT ")
            .push_bind(limit);
        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.iter().map(search_document_from_row).collect()
    }
}
