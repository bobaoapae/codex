use super::*;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
struct TestState {
    #[serde(default)]
    entries: BTreeMap<String, u64>,
}

/// FORK: the failure this replaced. Ten Claude agents in one process wrote this
/// file with a temp name derived only from the pid, so they shared one temp file
/// and each write silently replaced the last.
#[test]
fn concurrent_updates_do_not_lose_writes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");

    const WRITERS: usize = 16;
    std::thread::scope(|scope| {
        for index in 0..WRITERS {
            let path = path.clone();
            scope.spawn(move || {
                update(&path, |state: &mut TestState| {
                    state.entries.insert(format!("key-{index}"), index as u64);
                });
            });
        }
    });

    let state: TestState = read(&path);
    assert_eq!(state.entries.len(), WRITERS, "{state:?}");
    for index in 0..WRITERS {
        assert_eq!(
            state.entries.get(&format!("key-{index}")),
            Some(&(index as u64))
        );
    }
}

/// The old fallback did `remove_file` then `rename`, so a concurrent reader
/// could catch the moment where the file did not exist and conclude the state
/// was empty — losing every recorded Claude session at once.
#[test]
fn the_state_file_is_never_absent_while_being_replaced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");
    update(&path, |state: &mut TestState| {
        state.entries.insert("seed".to_string(), 1);
    });

    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut sightings = 0_usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if !path.exists() {
                    sightings += 1;
                }
            }
            sightings
        });

        for index in 0..200 {
            update(&path, |state: &mut TestState| {
                state.entries.insert(format!("key-{index}"), index);
            });
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(reader.join().expect("reader thread"), 0);
    });
}

/// A truncated or hand-edited file must not fail a turn; the state is a cache.
#[test]
fn a_corrupt_file_reads_as_the_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.json");
    std::fs::write(&path, b"{ not json").expect("write corrupt state");

    assert_eq!(read::<TestState>(&path), TestState::default());

    // And the next update rewrites it cleanly rather than compounding.
    update(&path, |state: &mut TestState| {
        state.entries.insert("fresh".to_string(), 7);
    });
    assert_eq!(read::<TestState>(&path).entries.get("fresh"), Some(&7));
}

/// The lock lives beside the file, so replacing the file cannot orphan it.
#[test]
fn the_lock_file_sits_next_to_the_state_file() {
    let path = std::path::Path::new("/tmp/claude_code_sessions.json");
    assert_eq!(
        lock_path_for(path),
        std::path::PathBuf::from("/tmp/claude_code_sessions.json.lock")
    );
}
