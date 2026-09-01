use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[derive(Debug)]
struct FakeClock(AtomicI64);

impl FakeClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }
}

impl BuildAdmissionClock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn command(script: &str) -> Vec<String> {
    vec!["bash".to_string(), "-lc".to_string(), script.to_string()]
}

#[test]
fn broad_commands_are_classified_and_package_filters_are_focused() {
    assert_eq!(
        classify_broad_rust_command(&command("cargo build --workspace")),
        Some(BroadRustCommand::CargoBuild)
    );
    assert_eq!(
        classify_broad_rust_command(&command("cargo check")),
        Some(BroadRustCommand::CargoCheck)
    );
    assert_eq!(
        classify_broad_rust_command(&command("cargo --locked test --workspace")),
        Some(BroadRustCommand::CargoTest)
    );
    assert_eq!(
        classify_broad_rust_command(&command("cargo test --package codex-core")),
        None
    );
    assert_eq!(
        classify_broad_rust_command(&command("cargo clippy -p codex-core")),
        None
    );
    assert_eq!(
        classify_broad_rust_command(&command("just test")),
        Some(BroadRustCommand::JustTest)
    );
    assert_eq!(
        classify_broad_rust_command(&command("just clippy")),
        Some(BroadRustCommand::JustClippy)
    );
    assert_eq!(
        classify_broad_rust_command(&command("just fix -p codex-core")),
        None
    );
    assert_eq!(
        classify_broad_rust_command(&command("cargo test -p codex-core && cargo check")),
        Some(BroadRustCommand::CargoCheck)
    );
    assert_eq!(
        classify_broad_rust_command(&["cargo".to_string(), "metadata".to_string()]),
        None
    );
}

#[test]
fn broad_commands_outside_git_checkouts_are_unmanaged() {
    let directory = tempdir().expect("unmanaged directory");
    assert!(!directory.path().join(".git").exists());
    assert!(
        BuildAdmission::new()
            .try_acquire(
                &command("cargo test --workspace"),
                directory.path(),
                &[],
                "unmanaged",
            )
            .expect("unmanaged admission should not fail")
            .is_none()
    );
}

#[test]
fn admission_conflicts_are_nonblocking_and_release_on_drop() {
    let checkout = tempdir().expect("checkout tempdir");
    std::fs::create_dir(checkout.path().join(".git")).expect("git marker");
    let cwd = checkout.path().join("src");
    std::fs::create_dir(&cwd).expect("cwd");
    let clock = Arc::new(FakeClock::new(1_000));
    let admission = BuildAdmission::with_clock(clock);
    let owner = admission
        .try_acquire(
            &command("cargo test --workspace"),
            &cwd,
            &[],
            "owner-session",
        )
        .expect("first admission")
        .expect("broad command gets lease");
    assert_eq!(
        owner.key().target_dir,
        std::fs::canonicalize(checkout.path().join("target")).expect("canonical target")
    );

    let contender = BuildAdmission::new()
        .try_acquire(&command("just test"), &cwd, &[], "contender-session")
        .expect_err("contender should not wait");
    let BuildAdmissionError::Busy(busy) = contender else {
        panic!("expected typed BuildAdmissionBusy");
    };
    assert!(busy.needs_attention());
    assert_eq!(busy.owner.session_id.as_deref(), Some("owner-session"));
    assert_eq!(busy.owner.started_at_ms, Some(1_000));
    assert!(busy.owner.elapsed_ms.is_some());
    let rendered = format!("{busy:?}");
    assert!(!rendered.contains("owner-session"));
    assert!(rendered.contains("redacted"));
    drop(owner);
    assert!(
        BuildAdmission::new()
            .try_acquire(&command("cargo check"), &cwd, &[], "after-release",)
            .expect("admission after release")
            .is_some()
    );
}

#[test]
fn target_directory_is_part_of_the_lock_identity() {
    let checkout = tempdir().expect("checkout tempdir");
    std::fs::create_dir(checkout.path().join(".git")).expect("git marker");
    let cwd = checkout.path().to_path_buf();
    let env_a = vec![("CARGO_TARGET_DIR".to_string(), "target-a".to_string())];
    let env_b = vec![("CARGO_TARGET_DIR".to_string(), "target-b".to_string())];
    let admission = BuildAdmission::new();
    let first = admission
        .try_acquire(&command("cargo build"), &cwd, &env_a, "one")
        .expect("first target admission")
        .expect("first target lease");
    let second = admission
        .try_acquire(&command("cargo build"), &cwd, &env_b, "two")
        .expect("second target admission")
        .expect("second target lease");
    assert_ne!(first.key().lock_path, second.key().lock_path);
}

#[test]
fn inline_cargo_target_dir_is_resolved_before_admission() {
    let checkout = tempdir().expect("checkout tempdir");
    std::fs::create_dir(checkout.path().join(".git")).expect("git marker");
    let cwd = checkout.path().to_path_buf();
    let admission = BuildAdmission::new();
    let command = command("CARGO_TARGET_DIR=inline-target cargo test");
    let guard = admission
        .try_acquire(&command, &cwd, &[], "inline")
        .expect("inline target admission")
        .expect("broad command gets lease");
    assert_eq!(
        guard.key().target_dir,
        std::fs::canonicalize(cwd.join("inline-target")).expect("canonical inline target")
    );
}

#[test]
fn cargo_target_dir_options_join_environment_target_identity() {
    let checkout = tempdir().expect("checkout tempdir");
    std::fs::create_dir(checkout.path().join(".git")).expect("git marker");
    let forms = [
        "cargo build --target-dir cli-target",
        "cargo build --target-dir=cli-target",
        "cargo build -t cli-target",
        "cargo build -t=cli-target",
    ];
    let expected = resolve_build_admission_key(
        checkout.path(),
        &[("CARGO_TARGET_DIR".to_string(), "cli-target".to_string())],
    )
    .expect("environment target key");
    let inherited_environment = [("CARGO_TARGET_DIR".to_string(), "env-target".to_string())];
    for form in forms {
        let key = resolve_build_admission_key(
            checkout.path(),
            &effective_target_environment(&command(form), &inherited_environment),
        )
        .expect("CLI target key");
        assert_eq!(key.lock_path, expected.lock_path, "{form}");
    }
}

#[test]
fn stale_metadata_without_a_live_lock_is_recoverable_without_killing_any_process() {
    let checkout = tempdir().expect("checkout tempdir");
    std::fs::create_dir(checkout.path().join(".git")).expect("git marker");
    let key = resolve_build_admission_key(checkout.path(), &[]).expect("resolve key");
    std::fs::write(
        &key.owner_path,
        br#"{"pid":4294967294,"sessionId":"stale","startedAtMs":1}
kind=CargoBuild"#,
    )
    .expect("write stale metadata");
    let admission = BuildAdmission::new();
    let guard = admission
        .try_acquire(&command("cargo build"), checkout.path(), &[], "live")
        .expect("stale metadata should not block an unlocked file")
        .expect("lease acquired");
    let contents = std::fs::read_to_string(&key.owner_path).expect("read owner metadata");
    assert!(!contents.contains("cargo build"));
    assert!(contents.contains("live"));
    drop(guard);
}
