//! FORK: durable record of the ChatGPT conversation backing each Codex thread.
//!
//! Mirror of `claude_code::sessions`: continuity lives on the `ModelClient`,
//! which dies with the thread, and multi-agent v2 evicts idle agents and
//! rebuilds them on the next message. Without this file every follow-up to an
//! evicted agent would replay its whole transcript into a brand new
//! conversation. The file is a cache, never a source of truth.

use super::history::ConversationContinuity;
use crate::claude_code::state_file;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Entries older than this are dropped on the next write.
const ENTRY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Hard ceiling on retained entries, oldest evicted first.
const MAX_ENTRIES: usize = 512;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct SessionsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    threads: BTreeMap<String, ThreadRecord>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct ThreadRecord {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    model_slug: Option<String>,
    #[serde(default)]
    delivered_items: usize,
    #[serde(default)]
    delivered_fingerprint: u64,
    #[serde(default)]
    echoed: Vec<u64>,
    #[serde(default)]
    message_landed_unanswered: bool,
    #[serde(default)]
    updated_at_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Drops expired and surplus entries, oldest first.
fn prune(file: &mut SessionsFile, now: u64) {
    file.threads
        .retain(|_, record| now.saturating_sub(record.updated_at_ms) < ENTRY_TTL_MS);
    if file.threads.len() <= MAX_ENTRIES {
        return;
    }
    let surplus = file.threads.len() - MAX_ENTRIES;
    let mut by_age: Vec<(String, u64)> = file
        .threads
        .iter()
        .map(|(key, record)| (key.clone(), record.updated_at_ms))
        .collect();
    by_age.sort_by_key(|(_, updated_at_ms)| *updated_at_ms);
    for (key, _) in by_age.into_iter().take(surplus) {
        file.threads.remove(&key);
    }
}

/// Reads the continuity recorded for a thread, if any.
pub(crate) fn load(path: &Path, thread_key: &str) -> Option<ConversationContinuity> {
    let record = state_file::read::<SessionsFile>(path)
        .threads
        .remove(thread_key)?;
    Some(ConversationContinuity {
        conversation_id: record.conversation_id,
        model_slug: record.model_slug,
        delivered_items: record.delivered_items,
        delivered_fingerprint: record.delivered_fingerprint,
        echoed: record.echoed,
        message_landed_unanswered: record.message_landed_unanswered,
    })
}

/// Persists the continuity for a thread (one locked read-modify-write).
pub(crate) fn store(path: &Path, thread_key: &str, continuity: &ConversationContinuity) {
    let now = now_ms();
    state_file::update(path, |file: &mut SessionsFile| {
        file.version = 1;
        let record = file.threads.entry(thread_key.to_string()).or_default();
        record.conversation_id = continuity.conversation_id.clone();
        record.model_slug = continuity.model_slug.clone();
        record.delivered_items = continuity.delivered_items;
        record.delivered_fingerprint = continuity.delivered_fingerprint;
        record.echoed = continuity.echoed.clone();
        record.message_landed_unanswered = continuity.message_landed_unanswered;
        record.updated_at_ms = now;
        prune(file, now);
    });
}

/// Forgets a thread's record (after its conversation was archived).
pub(crate) fn forget(path: &Path, thread_key: &str) {
    state_file::update(path, |file: &mut SessionsFile| {
        file.threads.remove(thread_key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn continuity(conversation_id: &str) -> ConversationContinuity {
        ConversationContinuity {
            conversation_id: Some(conversation_id.to_string()),
            model_slug: Some("chatgpt-web/thinking".to_string()),
            delivered_items: 4,
            delivered_fingerprint: 99,
            echoed: vec![7, 8],
            message_landed_unanswered: true,
        }
    }

    #[test]
    fn a_stored_conversation_comes_back_whole() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(super::super::SESSIONS_STATE_FILE_NAME);

        store(&path, "thread-1", &continuity("conv-1"));

        let loaded = load(&path, "thread-1").expect("record");
        assert_eq!(loaded, continuity("conv-1"));
        assert!(load(&path, "other").is_none());
    }

    #[test]
    fn forgetting_removes_only_that_thread() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(super::super::SESSIONS_STATE_FILE_NAME);
        store(&path, "thread-1", &continuity("conv-1"));
        store(&path, "thread-2", &continuity("conv-2"));

        forget(&path, "thread-1");

        assert!(load(&path, "thread-1").is_none());
        assert!(load(&path, "thread-2").is_some());
    }

    #[test]
    fn expired_entries_are_dropped_on_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(super::super::SESSIONS_STATE_FILE_NAME);
        let mut file = SessionsFile {
            version: 1,
            ..SessionsFile::default()
        };
        file.threads.insert(
            "ancient".to_string(),
            ThreadRecord {
                conversation_id: Some("old".to_string()),
                updated_at_ms: 1,
                ..ThreadRecord::default()
            },
        );
        state_file::update(&path, |on_disk: &mut SessionsFile| {
            *on_disk = file;
        });

        store(&path, "thread-1", &continuity("conv-1"));

        assert!(load(&path, "ancient").is_none());
        assert!(load(&path, "thread-1").is_some());
    }
}
