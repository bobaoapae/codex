//! Experimental bounded reads for state-owned artifacts.
//!
//! Artifact IDs are opaque values minted by the state store. This processor
//! never resolves a caller-provided path or thread ID and never exposes the
//! rollout path stored alongside an artifact.

use codex_app_server_protocol::ArtifactMetadata;
use codex_app_server_protocol::ArtifactReadParams;
use codex_app_server_protocol::ArtifactReadResponse;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_rollout::StateDbHandle;
use codex_state::DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES;
use codex_state::MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;

const DEFAULT_READ_LIMIT: u32 = DEFAULT_THREAD_ARTIFACT_READ_CHUNK_BYTES as u32;
const MAX_READ_LIMIT: u32 = MAX_THREAD_ARTIFACT_READ_CHUNK_BYTES as u32;

/// Serves the local, state-backed `artifact/read` method.
#[derive(Clone)]
pub(crate) struct ArtifactRequestProcessor {
    state_db: Option<StateDbHandle>,
}

impl ArtifactRequestProcessor {
    pub(crate) fn new(state_db: Option<StateDbHandle>) -> Self {
        Self { state_db }
    }

    pub(crate) async fn artifact_read(
        &self,
        params: ArtifactReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let state_db = self.state_db.as_ref().ok_or_else(|| {
            invalid_request(
                "artifact/read requires local SQLite state; remote state is unsupported",
            )
        })?;
        let limit = params
            .limit
            .unwrap_or(DEFAULT_READ_LIMIT)
            .clamp(1, MAX_READ_LIMIT) as usize;
        let artifact = state_db
            .get_thread_artifact(&params.artifact_id)
            .await
            .map_err(map_state_error)?
            .ok_or_else(|| invalid_request("artifact not found"))?;
        let page = state_db
            .read_thread_artifact(&params.artifact_id, params.cursor.as_deref(), limit)
            .await
            .map_err(map_state_error)?
            .ok_or_else(|| invalid_request("artifact not found"))?;
        let total_bytes = u64::try_from(page.total_bytes)
            .map_err(|_| internal_error("artifact size exceeds the supported response range"))?;
        Ok(Some(
            ArtifactReadResponse {
                artifact: ArtifactMetadata {
                    artifact_id: artifact.id,
                    thread_id: artifact.thread_id.to_string(),
                    artifact_type: artifact.artifact_type,
                    identity_key: artifact.identity_key,
                    created_at: artifact.created_at,
                },
                chunk: page.chunk,
                next_cursor: page.next_cursor,
                total_bytes,
            }
            .into(),
        ))
    }
}

fn map_state_error(error: anyhow::Error) -> JSONRPCErrorError {
    let message = error.to_string();
    if message.contains("cursor") {
        invalid_request("artifact read cursor is invalid or stale")
    } else if message.contains("artifact id") {
        invalid_request("invalid artifact id")
    } else {
        internal_error("artifact read failed")
    }
}

#[cfg(test)]
#[path = "artifact_processor_tests.rs"]
mod tests;
