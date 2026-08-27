//! FORK: read-modify-write on a JSON state file shared by many writers.
//!
//! `claude_code_accounts.json` and `claude_code_sessions.json` are written by
//! every Claude turn in every agent — up to ten in one Codex process, and more
//! across processes. The original code did load → mutate → write with a temp
//! name derived only from the process id, so two agents in the *same* process
//! shared a temp file and one write silently replaced the other. Worse, the
//! fallback path did `remove_file` then `rename`, leaving a window in which the
//! state file did not exist at all; a concurrent reader saw a fresh, empty file
//! and forgot every recorded session.
//!
//! The fix is the primitive `message-history` already uses for its own shared
//! file: an advisory lock held across the whole read-modify-write, a temp name
//! unique per writer, and `rename` without ever unlinking the destination.

use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tracing::warn;

/// Attempts to take the lock before giving up. Contention here is between a
/// handful of agents doing millisecond-long writes, so a short bounded wait
/// covers it; blocking forever would hang a turn behind a crashed writer.
const MAX_LOCK_RETRIES: usize = 8;
const LOCK_RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(25);

/// Distinguishes temp files written by different writers inside one process.
static WRITER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One mutex per state file, shared by every writer in this process.
///
/// The file lock alone is the wrong tool for in-process contention: ten agents
/// in one Codex process would spend their retry budget fighting over an OS lock
/// and then fall through to writing unsynchronized. This serializes them
/// cheaply, and the file lock is left to do the job it is good at — keeping two
/// Codex *processes* apart.
static IN_PROCESS_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn in_process_lock(path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    let mut locks = IN_PROCESS_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::sync::Arc::clone(locks.entry(path.to_path_buf()).or_default())
}

/// Reads the state at `path`, defaulting when it is missing or unreadable.
///
/// A corrupt or half-written file yields the default rather than an error: this
/// is a cache of health and session ids, and losing it costs one replay, while
/// failing the turn costs the work.
pub(crate) fn read<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
        Err(_) => T::default(),
    }
}

/// Applies `mutate` to the state at `path` and writes the result back, with the
/// whole read-modify-write serialized against other writers.
///
/// Returns whatever `mutate` returned. A failure to lock or to write is logged
/// and swallowed: the caller's turn is more important than this file.
pub(crate) fn update<T, R>(path: &Path, mutate: impl FnOnce(&mut T) -> R) -> R
where
    T: serde::de::DeserializeOwned + serde::Serialize + Default,
{
    // Two layers, because the contention has two shapes. Agents inside one
    // Codex process are serialized by a plain mutex; separate processes are
    // serialized by an advisory file lock.
    let in_process = in_process_lock(path);
    let _in_process_guard = in_process
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // The file lock lives beside the state file rather than on it: locking the
    // state file itself would race with the `rename` that replaces it, since the
    // renamed file is a different inode and carries none of the original's
    // locks.
    let lock_path = lock_path_for(path);
    let _guard = LockGuard::acquire(&lock_path);

    let mut state: T = read(path);
    let result = mutate(&mut state);
    write_atomically(path, &state);
    result
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

/// Serializes writers across processes for the length of one update.
///
/// Best-effort by construction: if the lock cannot be taken within the retry
/// budget the update proceeds anyway. Writing without the lock is still atomic
/// per-writer (temp + rename), so the worst case is a lost concurrent edit —
/// which is exactly the pre-fork behavior, not a regression.
struct LockGuard {
    file: Option<File>,
}

impl LockGuard {
    fn acquire(lock_path: &Path) -> Self {
        let Ok(file) = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
        else {
            return Self { file: None };
        };
        for _ in 0..MAX_LOCK_RETRIES {
            match file.try_lock() {
                Ok(()) => return Self { file: Some(file) },
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(LOCK_RETRY_SLEEP);
                }
                Err(_) => break,
            }
        }
        warn!("claude_code: proceeding without the state lock on {lock_path:?}");
        Self { file: None }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

/// Writes `state` to `path` through a temp file and a rename.
///
/// The temp name carries both the process id and a per-process counter, so two
/// agents in one process cannot collide. `rename` replaces the destination on
/// both platforms; the destination is never unlinked first, so a concurrent
/// reader always sees either the old file or the new one.
fn write_atomically<T: serde::Serialize>(path: &Path, state: &T) {
    let Ok(raw) = serde_json::to_vec_pretty(state) else {
        warn!("claude_code: failed to serialize state for {path:?}");
        return;
    };
    let sequence = WRITER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(".tmp.{}.{sequence}", std::process::id()));
    let temp_path = PathBuf::from(temp_name);

    if let Err(err) = std::fs::write(&temp_path, raw) {
        warn!("claude_code: failed to write {temp_path:?}: {err}");
        let _ = std::fs::remove_file(&temp_path);
        return;
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        warn!("claude_code: failed to persist {path:?}: {err}");
        let _ = std::fs::remove_file(&temp_path);
    }
}

#[cfg(test)]
#[path = "state_file_tests.rs"]
mod tests;
