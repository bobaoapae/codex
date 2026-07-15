use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Mutex;

const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000;
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EvidenceAction {
    Read { path: PathBuf },
    List { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EvidenceKey {
    action: EvidenceAction,
    canonical_cwd: PathBuf,
    permission_identity: String,
}

impl EvidenceKey {
    pub fn new(
        action: EvidenceAction,
        canonical_cwd: PathBuf,
        permission_identity: impl Into<String>,
    ) -> Self {
        Self {
            action,
            canonical_cwd,
            permission_identity: permission_identity.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyFingerprint {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub blake3: String,
}

impl DependencyFingerprint {
    pub async fn capture(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let metadata = tokio::fs::metadata(&path).await?;
        let bytes = tokio::fs::read(&path).await?;
        Ok(Self {
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        })
    }

    async fn still_valid(&self) -> bool {
        let Ok(metadata) = tokio::fs::metadata(&self.path).await else {
            return false;
        };
        if metadata.len() != self.len || metadata.modified().ok() != self.modified {
            return false;
        }
        tokio::fs::read(&self.path)
            .await
            .map(|bytes| blake3::hash(&bytes).to_hex().as_str() == self.blake3)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
struct EvidenceEntry {
    output: Arc<Vec<u8>>,
    dependencies: Vec<DependencyFingerprint>,
    workspace_generation: u64,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<EvidenceKey, EvidenceEntry>,
    total_bytes: usize,
    workspace_generation: u64,
    clock: u64,
}

#[derive(Debug, Clone)]
pub struct SharedEvidenceCache {
    enabled: bool,
    state: Arc<Mutex<CacheState>>,
}

impl SharedEvidenceCache {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: Arc::new(Mutex::new(CacheState::default())),
        }
    }

    pub async fn lookup(&self, key: &EvidenceKey) -> Option<Arc<Vec<u8>>> {
        if !self.enabled {
            return None;
        }
        let (entry, generation) = {
            let state = self.state.lock().await;
            (state.entries.get(key).cloned(), state.workspace_generation)
        };
        let Some(entry) = entry else {
            tracing::debug!(target: "codex_local_features", feature = "evidence_cache", result = "miss");
            return None;
        };
        let mut valid = entry.workspace_generation == generation;
        if valid {
            for dependency in &entry.dependencies {
                if !dependency.still_valid().await {
                    valid = false;
                    break;
                }
            }
        }
        if !valid {
            self.remove(key).await;
            tracing::debug!(target: "codex_local_features", feature = "evidence_cache", result = "invalidated");
            return None;
        }
        let mut state = self.state.lock().await;
        state.clock = state.clock.saturating_add(1);
        let clock = state.clock;
        if let Some(current) = state.entries.get_mut(key) {
            current.last_used = clock;
        }
        tracing::debug!(target: "codex_local_features", feature = "evidence_cache", result = "hit");
        Some(entry.output)
    }

    pub async fn insert(
        &self,
        key: EvidenceKey,
        output: Vec<u8>,
        dependencies: Vec<DependencyFingerprint>,
    ) -> bool {
        if !self.enabled || output.len() > MAX_ENTRY_BYTES {
            return false;
        }
        let mut state = self.state.lock().await;
        if let Some(previous) = state.entries.remove(&key) {
            state.total_bytes = state.total_bytes.saturating_sub(previous.output.len());
        }
        state.clock = state.clock.saturating_add(1);
        let entry = EvidenceEntry {
            output: Arc::new(output),
            dependencies,
            workspace_generation: state.workspace_generation,
            last_used: state.clock,
        };
        state.total_bytes = state.total_bytes.saturating_add(entry.output.len());
        state.entries.insert(key, entry);
        evict_to_limits(&mut state);
        true
    }

    pub async fn note_write(&self) {
        if !self.enabled {
            return;
        }
        let mut state = self.state.lock().await;
        state.workspace_generation = state.workspace_generation.saturating_add(1);
        state.entries.clear();
        state.total_bytes = 0;
        tracing::debug!(target: "codex_local_features", feature = "evidence_cache", "invalidated cache after workspace write");
    }

    pub async fn len(&self) -> usize {
        self.state.lock().await.entries.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.state.lock().await.entries.is_empty()
    }

    async fn remove(&self, key: &EvidenceKey) {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.entries.remove(key) {
            state.total_bytes = state.total_bytes.saturating_sub(entry.output.len());
        }
    }
}

fn evict_to_limits(state: &mut CacheState) {
    while state.entries.len() > MAX_ENTRIES || state.total_bytes > MAX_TOTAL_BYTES {
        let Some(key) = state
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(entry) = state.entries.remove(&key) {
            state.total_bytes = state.total_bytes.saturating_sub(entry.output.len());
        }
    }
}

pub async fn canonicalize_for_key(path: &Path) -> std::io::Result<PathBuf> {
    tokio::fs::canonicalize(path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clone_shares_hits_and_write_invalidates_all_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evidence.txt");
        tokio::fs::write(&path, b"one").await.expect("write");
        let cache = SharedEvidenceCache::new(true);
        let subagent = cache.clone();
        let key = EvidenceKey::new(
            EvidenceAction::Read { path: path.clone() },
            dir.path().to_path_buf(),
            "sandbox-a",
        );
        assert!(
            cache
                .insert(
                    key.clone(),
                    b"one".to_vec(),
                    vec![DependencyFingerprint::capture(&path).await.expect("hash")],
                )
                .await
        );
        assert_eq!(
            subagent.lookup(&key).await.as_deref().map(Vec::as_slice),
            Some(b"one".as_slice())
        );
        subagent.note_write().await;
        assert!(cache.lookup(&key).await.is_none());
    }

    #[tokio::test]
    async fn external_change_invalidates_metadata_and_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evidence.txt");
        tokio::fs::write(&path, b"one").await.expect("write");
        let cache = SharedEvidenceCache::new(true);
        let key = EvidenceKey::new(
            EvidenceAction::Read { path: path.clone() },
            dir.path().to_path_buf(),
            "sandbox-a",
        );
        cache
            .insert(
                key.clone(),
                b"one".to_vec(),
                vec![DependencyFingerprint::capture(&path).await.expect("hash")],
            )
            .await;
        tokio::fs::write(&path, b"two").await.expect("rewrite");
        assert!(cache.lookup(&key).await.is_none());
    }

    #[tokio::test]
    async fn oversized_entries_are_never_cached() {
        let cache = SharedEvidenceCache::new(true);
        let key = EvidenceKey::new(
            EvidenceAction::List {
                path: PathBuf::from("."),
            },
            PathBuf::from("."),
            "sandbox-a",
        );
        assert!(
            !cache
                .insert(key, vec![0; MAX_ENTRY_BYTES + 1], Vec::new())
                .await
        );
        assert_eq!(cache.len().await, 0);
    }
}
