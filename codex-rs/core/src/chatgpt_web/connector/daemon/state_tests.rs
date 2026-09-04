use super::*;
use pretty_assertions::assert_eq;

#[test]
fn paths_live_under_the_chatgpt_web_directory() {
    let paths = DaemonPaths::new(Path::new("/home/x/.codex"));
    assert!(paths.dir.ends_with("chatgpt_web"));
    assert!(paths.lock.ends_with("daemon.lock"));
    assert!(paths.token.ends_with("daemon.token"));
    assert!(paths.connector.ends_with("connector.json"));
    assert!(paths.tunnel_key.ends_with("tunnel.key"));
}

#[test]
fn state_round_trips_through_json_and_missing_files_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("nested").join("daemon.json");
    assert_eq!(read_json::<DaemonState>(&path), DaemonState::default());

    let state = DaemonState {
        version: STATE_VERSION,
        pid: 42,
        control_port: 5000,
        started_at_ms: 1,
        codex_version: "0.0.0".into(),
        public_url: Some("tunnel:tunnel_abc".into()),
        registry_status: "unknown".into(),
    };
    write_json(&path, &state).expect("writes");
    assert_eq!(read_json::<DaemonState>(&path), state);
    assert_eq!(read_json_opt::<DaemonState>(&path), Some(state.clone()));
    assert_eq!(state.control_url(), "http://127.0.0.1:5000");

    std::fs::write(&path, b"{ not json").expect("corrupt");
    assert_eq!(read_json::<DaemonState>(&path), DaemonState::default());
    assert_eq!(read_json_opt::<DaemonState>(&path), None);
}

#[test]
fn secrets_are_trimmed_and_tokens_are_unique() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("daemon.token");
    assert_eq!(read_secret(&path), None);
    write_secret(&path, "  abc\n").expect("writes");
    assert_eq!(read_secret(&path).as_deref(), Some("abc"));
    write_secret(&path, "\n").expect("writes");
    assert_eq!(read_secret(&path), None);
    assert_ne!(new_token(), new_token());
    assert_eq!(new_token().len(), 43);
}

#[test]
fn the_instance_lock_is_exclusive_within_a_process_too() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("daemon.lock");
    let first = InstanceLock::try_acquire(&path).expect("first lock");
    assert!(matches!(
        InstanceLock::try_acquire(&path),
        Err(LockError::Held)
    ));
    drop(first);
    let _second = InstanceLock::try_acquire(&path).expect("lock after release");
}

#[test]
fn registry_status_serializes_by_tag() {
    let verified = RegistryStatus::Verified {
        connector_id: "asdk_app_1".into(),
        link_id: "link_1".into(),
        mcp_url: "tunnel:tunnel_abc".into(),
    };
    let json = serde_json::to_value(&verified).expect("json");
    assert_eq!(json["status"], "verified");
    assert_eq!(verified.label(), "verified");
    assert_eq!(RegistryStatus::default().label(), "unknown");
}

#[test]
fn our_own_pid_is_alive_and_a_dead_child_is_not() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(0));
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" });
    if cfg!(windows) {
        child.args(["/C", "exit 0"]);
    }
    let mut child = child.spawn().expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    // The pid may be recycled in theory; in practice it is dead right after wait.
    assert!(!pid_alive(pid) || cfg!(windows));
}

/// FORK: `Failed` carries the failure kind and the parked flag, and a state
/// file written by an older build still loads.
#[test]
fn a_registry_failure_round_trips_its_kind_and_park() {
    let failed = RegistryStatus::Failed {
        reason: "tunnel `tunnel_abc` is not visible".into(),
        retry_at_ms: 1_700_000_000_000,
        kind: FailureKind::TunnelNotVisible,
        parked: true,
    };
    let json = serde_json::to_value(&failed).expect("json");
    assert_eq!(json["status"], "failed");
    assert_eq!(json["kind"], "tunnel_not_visible");
    assert_eq!(json["parked"], true);
    assert_eq!(
        serde_json::from_value::<RegistryStatus>(json).expect("round trip"),
        failed
    );

    // The shape older builds wrote: no kind, no parked.
    let legacy: RegistryStatus = serde_json::from_value(serde_json::json!({
        "status": "failed",
        "reason": "boom",
        "retry_at_ms": 1_700_000_000_000_u64,
    }))
    .expect("legacy shape still loads");
    assert_eq!(
        legacy,
        RegistryStatus::Failed {
            reason: "boom".into(),
            retry_at_ms: 1_700_000_000_000,
            kind: FailureKind::Transient,
            parked: false,
        }
    );
}

#[test]
fn only_the_failures_a_user_must_fix_are_terminal() {
    for kind in [FailureKind::Transient, FailureKind::RateLimited] {
        assert!(!kind.is_terminal(), "{kind:?}");
    }
    for kind in [
        FailureKind::TunnelNotVisible,
        FailureKind::LoginRequired,
        FailureKind::SetupRequired,
    ] {
        assert!(kind.is_terminal(), "{kind:?}");
        // The label is the wire form the turn-side client parses back.
        assert_eq!(FailureKind::parse(kind.label()), Some(kind));
    }
    assert_eq!(FailureKind::parse("something else"), None);
}
