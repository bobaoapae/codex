use super::StateRuntime;
use crate::DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES;
use crate::MAX_THREAD_ARTIFACT_ID_BYTES;
use crate::MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES;
use crate::MAX_THREAD_ARTIFACT_LIST_LIMIT;
use crate::MAX_THREAD_ARTIFACT_PAYLOAD_BYTES;
use crate::MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES;
use crate::MAX_THREAD_ARTIFACT_TYPE_BYTES;
use crate::ThreadArtifact;
use crate::ThreadArtifactAttachmentOutcome;
use crate::ThreadArtifactPage;
use crate::ThreadArtifactReadEncoding;
use crate::ThreadArtifactReadPage;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::SqliteRow;
use std::collections::BTreeSet;
use uuid::Uuid;

const CURSOR_VERSION: &str = "1";

impl StateRuntime {
    /// Attach an opaque JSON artifact to a thread.
    ///
    /// The `(thread_id, artifact_type, identity_key)` tuple is idempotent. A
    /// repeated attach returns the original row when its JSON payload is equal
    /// and fails closed when the payload differs.
    pub async fn attach_thread_artifact(
        &self,
        thread_id: ThreadId,
        artifact_type: &str,
        identity_key: &str,
        payload: Value,
    ) -> anyhow::Result<ThreadArtifactAttachmentOutcome> {
        validate_identity(
            artifact_type,
            MAX_THREAD_ARTIFACT_TYPE_BYTES,
            "artifact type",
        )?;
        validate_identity(
            identity_key,
            MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES,
            "artifact identity key",
        )?;
        let payload_json = serialize_payload(&payload)?;

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let thread_exists =
            sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?)")
                .bind(thread_id.to_string())
                .fetch_one(&mut *transaction)
                .await?
                != 0;
        if !thread_exists {
            anyhow::bail!("thread does not exist");
        }

        let existing = sqlx::query(
            "SELECT id, thread_id, artifact_type, identity_key, payload, created_at
             FROM thread_artifacts
             WHERE thread_id = ? AND artifact_type = ? AND identity_key = ?",
        )
        .bind(thread_id.to_string())
        .bind(artifact_type)
        .bind(identity_key)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = existing {
            let existing = artifact_from_row(&row)?;
            if existing.payload != payload {
                anyhow::bail!("artifact identity already exists with a different payload");
            }
            transaction.commit().await?;
            return Ok(ThreadArtifactAttachmentOutcome::Existing(existing));
        }

        let artifact = ThreadArtifact {
            id: Uuid::now_v7().to_string(),
            thread_id,
            artifact_type: artifact_type.to_string(),
            identity_key: identity_key.to_string(),
            payload,
            created_at: Utc::now().timestamp(),
        };
        validate_artifact(&artifact)?;
        sqlx::query(
            "INSERT INTO thread_artifacts
             (id, thread_id, artifact_type, identity_key, payload, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&artifact.id)
        .bind(artifact.thread_id.to_string())
        .bind(&artifact.artifact_type)
        .bind(&artifact.identity_key)
        .bind(payload_json)
        .bind(artifact.created_at)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ThreadArtifactAttachmentOutcome::Created(artifact))
    }

    /// Return one artifact by its server-assigned identity.
    pub async fn get_thread_artifact(
        &self,
        artifact_id: &str,
    ) -> anyhow::Result<Option<ThreadArtifact>> {
        validate_identity(artifact_id, MAX_THREAD_ARTIFACT_ID_BYTES, "artifact id")?;
        let row = sqlx::query(
            "SELECT id, thread_id, artifact_type, identity_key, payload, created_at
             FROM thread_artifacts WHERE id = ?",
        )
        .bind(artifact_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.as_ref().map(artifact_from_row).transpose()
    }

    /// List artifacts belonging to an explicit selection of threads.
    ///
    /// Results use stable keyset ordering `(thread_id, created_at, id)`. An
    /// empty selection is intentionally empty rather than an accidental full
    /// database scan.
    pub async fn list_thread_artifacts(
        &self,
        thread_ids: &[ThreadId],
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<ThreadArtifactPage> {
        let selected = normalized_thread_ids(thread_ids);
        let selection_digest = selection_digest(&selected);
        let anchor = cursor
            .map(|cursor| decode_list_cursor(cursor, selection_digest))
            .transpose()?;
        if selected.is_empty() {
            return Ok(ThreadArtifactPage {
                artifacts: Vec::new(),
                next_cursor: None,
            });
        }

        let page_size = limit.clamp(1, MAX_THREAD_ARTIFACT_LIST_LIMIT);
        let fetch_limit = i64::try_from(page_size.saturating_add(1))?;
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id, thread_id, artifact_type, identity_key, payload, created_at
             FROM thread_artifacts WHERE thread_id IN (",
        );
        let mut separated = query.separated(", ");
        for thread_id in &selected {
            separated.push_bind(thread_id);
        }
        separated.push_unseparated(")");
        if let Some(anchor) = anchor {
            query
                .push(" AND (thread_id > ")
                .push_bind(&anchor.thread_id)
                .push(" OR (thread_id = ")
                .push_bind(&anchor.thread_id)
                .push(" AND created_at > ")
                .push_bind(anchor.created_at)
                .push(") OR (thread_id = ")
                .push_bind(&anchor.thread_id)
                .push(" AND created_at = ")
                .push_bind(anchor.created_at)
                .push(" AND id > ")
                .push_bind(anchor.id)
                .push("))");
        }
        query
            .push(" ORDER BY thread_id ASC, created_at ASC, id ASC LIMIT ")
            .push_bind(fetch_limit);
        let rows = query.build().fetch_all(self.pool.as_ref()).await?;
        let mut artifacts = rows
            .iter()
            .map(artifact_from_row)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let next_cursor = if artifacts.len() > page_size {
            artifacts.pop();
            artifacts.last().map(|artifact| {
                encode_cursor(&format!(
                    "list|{CURSOR_VERSION}|{selection_digest:016x}|{}|{}|{}",
                    artifact.thread_id, artifact.created_at, artifact.id
                ))
            })
        } else {
            None
        };
        Ok(ThreadArtifactPage {
            artifacts,
            next_cursor,
        })
    }

    /// Read a bounded UTF-8 page from an artifact's canonical JSON payload.
    ///
    /// The cursor carries the artifact identity, payload length, digest, and
    /// byte offset. It therefore fails closed if a stale or cross-artifact
    /// cursor is supplied. `chunk_bytes` is capped at 64 KiB.
    pub async fn read_thread_artifact(
        &self,
        artifact_id: &str,
        cursor: Option<&str>,
        chunk_bytes: usize,
    ) -> anyhow::Result<Option<ThreadArtifactReadPage>> {
        validate_identity(artifact_id, MAX_THREAD_ARTIFACT_ID_BYTES, "artifact id")?;
        let Some(artifact) = self.get_thread_artifact(artifact_id).await? else {
            return Ok(None);
        };
        let payload = serde_json::to_string(&artifact.payload)?;
        let payload_bytes = payload.as_bytes();
        let payload_digest = digest(payload_bytes);
        let offset = cursor
            .map(|cursor| decode_read_cursor(cursor, artifact_id, payload_bytes))
            .transpose()?
            .unwrap_or(0);
        if !payload.is_char_boundary(offset) {
            anyhow::bail!("artifact read cursor is not on a UTF-8 boundary");
        }
        let chunk_limit = chunk_bytes.clamp(1, MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES);
        let end = chunk_end(&payload, offset, chunk_limit);
        let chunk = payload[offset..end].to_string();
        let next_cursor = (end < payload.len()).then(|| {
            encode_cursor(&format!(
                "read|{CURSOR_VERSION}|{artifact_id}|{payload_digest:016x}|{}|{end}",
                payload.len()
            ))
        });
        Ok(Some(ThreadArtifactReadPage {
            artifact_id: artifact.id,
            offset,
            chunk,
            encoding: ThreadArtifactReadEncoding::JsonUtf8,
            next_cursor,
            complete: end == payload.len(),
            total_bytes: payload.len(),
        }))
    }

    /// Read using the default bounded chunk size.
    pub async fn read_thread_artifact_default(
        &self,
        artifact_id: &str,
        cursor: Option<&str>,
    ) -> anyhow::Result<Option<ThreadArtifactReadPage>> {
        self.read_thread_artifact(
            artifact_id,
            cursor,
            DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES,
        )
        .await
    }
}

fn validate_identity(value: &str, max_bytes: usize, label: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if value.len() > max_bytes {
        anyhow::bail!("{label} exceeds {max_bytes} bytes");
    }
    Ok(())
}

fn serialize_payload(payload: &Value) -> anyhow::Result<String> {
    let encoded = serde_json::to_string(payload)?;
    if encoded.len() > MAX_THREAD_ARTIFACT_PAYLOAD_BYTES {
        anyhow::bail!("artifact payload exceeds {MAX_THREAD_ARTIFACT_PAYLOAD_BYTES} bytes");
    }
    Ok(encoded)
}

fn validate_artifact(artifact: &ThreadArtifact) -> anyhow::Result<()> {
    validate_identity(&artifact.id, MAX_THREAD_ARTIFACT_ID_BYTES, "artifact id")?;
    validate_identity(
        &artifact.artifact_type,
        MAX_THREAD_ARTIFACT_TYPE_BYTES,
        "artifact type",
    )?;
    validate_identity(
        &artifact.identity_key,
        MAX_THREAD_ARTIFACT_IDENTITY_KEY_BYTES,
        "artifact identity key",
    )?;
    serialize_payload(&artifact.payload)?;
    Ok(())
}

fn artifact_from_row(row: &SqliteRow) -> anyhow::Result<ThreadArtifact> {
    let artifact = ThreadArtifact {
        id: row.try_get("id")?,
        thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
        artifact_type: row.try_get("artifact_type")?,
        identity_key: row.try_get("identity_key")?,
        payload: serde_json::from_str(&row.try_get::<String, _>("payload")?)?,
        created_at: row.try_get("created_at")?,
    };
    validate_artifact(&artifact)?;
    Ok(artifact)
}

fn normalized_thread_ids(thread_ids: &[ThreadId]) -> Vec<String> {
    thread_ids
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn selection_digest(thread_ids: &[String]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for thread_id in thread_ids {
        for byte in thread_id.as_bytes().iter().chain([0].iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

#[derive(Debug)]
struct ListCursor {
    thread_id: String,
    created_at: i64,
    id: String,
}

fn decode_list_cursor(cursor: &str, expected_digest: u64) -> anyhow::Result<ListCursor> {
    let decoded = decode_cursor(cursor)?;
    let [kind, version, digest, thread_id, created_at, id] =
        decoded.split('|').collect::<Vec<_>>()[..]
    else {
        anyhow::bail!("invalid artifact list cursor");
    };
    if kind != "list" || version != CURSOR_VERSION {
        anyhow::bail!("invalid artifact list cursor");
    }
    let cursor_digest = u64::from_str_radix(digest, 16)
        .map_err(|_| anyhow::anyhow!("invalid artifact list cursor"))?;
    if cursor_digest != expected_digest {
        anyhow::bail!("stale artifact list cursor");
    }
    ThreadId::try_from(thread_id.to_string())
        .map_err(|_| anyhow::anyhow!("invalid artifact list cursor"))?;
    let created_at = created_at
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid artifact list cursor"))?;
    validate_identity(id, MAX_THREAD_ARTIFACT_ID_BYTES, "artifact id in cursor")?;
    Ok(ListCursor {
        thread_id: thread_id.to_string(),
        created_at,
        id: id.to_string(),
    })
}

fn decode_read_cursor(cursor: &str, artifact_id: &str, payload: &[u8]) -> anyhow::Result<usize> {
    let decoded = decode_cursor(cursor)?;
    let [
        kind,
        version,
        cursor_artifact_id,
        digest_hex,
        length,
        offset,
    ] = decoded.split('|').collect::<Vec<_>>()[..]
    else {
        anyhow::bail!("invalid artifact read cursor");
    };
    if kind != "read" || version != CURSOR_VERSION || cursor_artifact_id != artifact_id {
        anyhow::bail!("stale artifact read cursor");
    }
    let cursor_digest = u64::from_str_radix(digest_hex, 16)
        .map_err(|_| anyhow::anyhow!("invalid artifact read cursor"))?;
    if cursor_digest != digest(payload) {
        anyhow::bail!("stale artifact read cursor");
    }
    let length = length
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid artifact read cursor"))?;
    if length != payload.len() {
        anyhow::bail!("stale artifact read cursor");
    }
    let offset = offset
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid artifact read cursor"))?;
    if offset > payload.len() {
        anyhow::bail!("invalid artifact read cursor");
    }
    Ok(offset)
}

fn encode_cursor(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_cursor(cursor: &str) -> anyhow::Result<String> {
    let encoded = cursor.as_bytes();
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) || encoded.len() > 1024 {
        anyhow::bail!("invalid artifact cursor");
    }
    let bytes = (0..encoded.len())
        .step_by(2)
        .map(|index| {
            let high = hex_digit(encoded[index])?;
            let low = hex_digit(encoded[index + 1])?;
            Ok((high << 4) | low)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid artifact cursor"))
}

fn hex_digit(value: u8) -> anyhow::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(anyhow::anyhow!("invalid artifact cursor")),
    }
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn chunk_end(payload: &str, offset: usize, max_bytes: usize) -> usize {
    let mut end = offset.saturating_add(max_bytes).min(payload.len());
    while end > offset && !payload.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < payload.len() {
        end = (offset + 1..=payload.len())
            .find(|candidate| payload.is_char_boundary(*candidate))
            .unwrap_or(payload.len());
    }
    end
}

#[cfg(test)]
#[path = "artifacts_tests.rs"]
mod tests;
