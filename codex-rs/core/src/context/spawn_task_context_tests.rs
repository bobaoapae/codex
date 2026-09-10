use super::*;
use crate::context::ContextualUserFragment;

#[test]
fn spawn_task_context_renders_as_a_typed_bounded_fragment() {
    let context = SpawnTaskContext::try_new("Objective: inspect the parser.".to_string())
        .expect("context should be accepted");
    let rendered = context.render();

    assert!(rendered.starts_with(START_MARKER));
    assert!(rendered.ends_with(END_MARKER));
    assert!(rendered.contains("Objective: inspect the parser."));
    assert_eq!(
        context.content_kind(),
        ContentItemKind("multi_agent.spawn_task_context".to_string())
    );
    assert!(rendered.len() <= MAX_SPAWN_TASK_CONTEXT_BYTES + 128);
}

#[test]
fn spawn_task_context_rejects_empty_and_oversized_briefs() {
    assert_eq!(
        SpawnTaskContext::try_new(" \n\t".to_string()),
        Err(SpawnTaskContextError::Empty)
    );

    let error = SpawnTaskContext::try_new("x".repeat(MAX_SPAWN_TASK_CONTEXT_BYTES + 1))
        .expect_err("oversized context must fail");
    assert_eq!(
        error,
        SpawnTaskContextError::TooLarge {
            byte_len: MAX_SPAWN_TASK_CONTEXT_BYTES + 1,
            max_bytes: MAX_SPAWN_TASK_CONTEXT_BYTES,
        }
    );
}
