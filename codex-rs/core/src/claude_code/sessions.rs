//! FORK: durable record of the Claude session backing each Codex thread.
//!
//! Continuity lives on the `ModelClient`, which dies with the thread. Multi-agent
//! v2 evicts idle agents to make room for new ones and rebuilds them from their
//! rollout on the next message, so without this file every follow-up to an
//! evicted agent would replay its whole transcript into a brand new Claude
//! session — the most expensive thing this provider can do.
//!
//! The file is a cache, never a source of truth: a missing, stale, or corrupt
//! entry only costs one replay.

use super::history::ClaudeSessionContinuity;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// File name under `CODEX_HOME`.
pub(crate) const SESSIONS_STATE_FILE_NAME: &str = "claude_code_sessions.json";

/// Entries older than this are dropped on the next write. A Claude session that
/// has not been touched in a week is well past any useful cache anyway.
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
    session_id: Option<String>,
    #[serde(default)]
    delivered_items: usize,
    #[serde(default)]
    delivered_fingerprint: u64,
    #[serde(default)]
    account_dir: Option<PathBuf>,
    /// Account this agent was pinned to at spawn time. Kept here because a
    /// rehydrated agent is rebuilt from its parent's turn, which knows nothing
    /// about the pin.
    #[serde(default)]
    pinned_account: Option<PathBuf>,
    #[serde(default)]
    echoed: Vec<u64>,
    #[serde(default)]
    updated_at_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn load_file(path: &Path) -> SessionsFile {
    super::state_file::read(path)
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
pub(super) fn load(
    path: &Path,
    thread_key: &str,
) -> Option<(ClaudeSessionContinuity, Option<PathBuf>)> {
    let record = load_file(path).threads.remove(thread_key)?;
    let continuity = ClaudeSessionContinuity {
        session_id: record.session_id,
        delivered_items: record.delivered_items,
        delivered_fingerprint: record.delivered_fingerprint,
        account_dir: record.account_dir,
        echoed: record.echoed,
    };
    Some((continuity, record.pinned_account))
}

/// Persists the continuity for a thread, keeping its recorded pin.
pub(super) fn store(
    path: &Path,
    thread_key: &str,
    continuity: &ClaudeSessionContinuity,
    pinned_account: Option<&Path>,
) {
    let now = now_ms();
    // FORK: one locked read-modify-write. Loading, mutating and writing
    // separately meant a sibling agent's `store` landing in between was
    // overwritten, and the thread it belonged to replayed its whole transcript
    // on its next turn.
    super::state_file::update(path, |file: &mut SessionsFile| {
        file.version = 1;
        let record = file.threads.entry(thread_key.to_string()).or_default();
        record.session_id = continuity.session_id.clone();
        record.delivered_items = continuity.delivered_items;
        record.delivered_fingerprint = continuity.delivered_fingerprint;
        record.account_dir = continuity.account_dir.clone();
        record.echoed = continuity.echoed.clone();
        if let Some(pinned_account) = pinned_account {
            record.pinned_account = Some(pinned_account.to_path_buf());
        }
        record.updated_at_ms = now;
        prune(file, now);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn continuity(session_id: &str, account: &Path) -> ClaudeSessionContinuity {
        ClaudeSessionContinuity {
            session_id: Some(session_id.to_string()),
            delivered_items: 4,
            delivered_fingerprint: 99,
            account_dir: Some(account.to_path_buf()),
            echoed: vec![7, 8],
        }
    }

    /// The whole point: an agent rebuilt after eviction resumes its session
    /// instead of replaying its transcript.
    #[test]
    fn a_stored_session_comes_back_with_its_account_and_echoes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(SESSIONS_STATE_FILE_NAME);
        let account = temp.path().join("account-a");

        store(&path, "thread-1", &continuity("claude-1", &account), None);

        let (loaded, pinned) = load(&path, "thread-1").expect("record");
        assert_eq!(loaded.session_id.as_deref(), Some("claude-1"));
        assert_eq!(loaded.delivered_items, 4);
        assert_eq!(loaded.delivered_fingerprint, 99);
        assert_eq!(loaded.account_dir, Some(account));
        assert_eq!(loaded.echoed, vec![7, 8]);
        assert_eq!(pinned, None);
        assert!(load(&path, "other-thread").is_none());
    }

    /// A pin is recorded once at spawn and must survive turns that do not
    /// mention it — the rebuilt config no longer carries it.
    #[test]
    fn a_recorded_pin_outlives_later_turns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(SESSIONS_STATE_FILE_NAME);
        let account = temp.path().join("account-a");

        store(
            &path,
            "thread-1",
            &continuity("claude-1", &account),
            Some(&account),
        );
        store(&path, "thread-1", &continuity("claude-2", &account), None);

        let (loaded, pinned) = load(&path, "thread-1").expect("record");
        assert_eq!(loaded.session_id.as_deref(), Some("claude-2"));
        assert_eq!(pinned, Some(account));
    }

    #[test]
    fn expired_entries_are_dropped_on_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(SESSIONS_STATE_FILE_NAME);
        let account = temp.path().join("account-a");

        let mut file = SessionsFile {
            version: 1,
            ..SessionsFile::default()
        };
        file.threads.insert(
            "ancient".to_string(),
            ThreadRecord {
                session_id: Some("claude-old".to_string()),
                updated_at_ms: 1,
                ..ThreadRecord::default()
            },
        );
        super::super::state_file::update(&path, |on_disk: &mut SessionsFile| {
            *on_disk = file;
        });

        store(&path, "thread-1", &continuity("claude-1", &account), None);

        assert!(load(&path, "ancient").is_none());
        assert!(load(&path, "thread-1").is_some());
    }
}
