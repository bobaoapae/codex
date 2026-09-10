use super::SpawnAgentArgs;
use crate::agent::control::SpawnAgentForkMode;
use pretty_assertions::assert_eq;
use serde_json::json;

fn spawn_args(value: serde_json::Value) -> SpawnAgentArgs {
    serde_json::from_value(value).expect("spawn arguments should parse")
}

#[test]
fn task_context_defaults_to_task_only_context() {
    let args = spawn_args(json!({
        "message": "worker task",
        "task_name": "worker",
        "task_context": "objective: inspect the parser"
    }));

    assert_eq!(args.fork_mode(args.task_context.is_some()), Ok(None));
}

#[test]
fn explicit_fork_turns_remains_authoritative_with_task_context() {
    let full_history = spawn_args(json!({
        "message": "worker task",
        "task_name": "worker",
        "task_context": "objective: inspect the parser",
        "fork_turns": "all"
    }));
    assert_eq!(
        full_history.fork_mode(full_history.task_context.is_some()),
        Ok(Some(SpawnAgentForkMode::FullHistory))
    );

    let last_turns = spawn_args(json!({
        "message": "worker task",
        "task_name": "worker",
        "task_context": "objective: inspect the parser",
        "fork_turns": "3"
    }));
    assert_eq!(
        last_turns.fork_mode(last_turns.task_context.is_some()),
        Ok(Some(SpawnAgentForkMode::LastNTurns(3)))
    );
}

#[test]
fn omitted_task_context_preserves_full_history_compatibility() {
    let args = spawn_args(json!({
        "message": "worker task",
        "task_name": "worker"
    }));

    assert_eq!(
        args.fork_mode(args.task_context.is_some()),
        Ok(Some(SpawnAgentForkMode::FullHistory))
    );
}
