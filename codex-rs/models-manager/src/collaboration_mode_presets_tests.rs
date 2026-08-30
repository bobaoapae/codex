use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preset_names_use_mode_display_names() {
    assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
    assert_eq!(default_preset().name, ModeKind::Default.display_name());
    assert_eq!(plan_preset().model, None);
    assert_eq!(plan_preset().reasoning_effort, None);
    assert_eq!(default_preset().model, None);
    assert_eq!(default_preset().reasoning_effort, None);
}

#[test]
fn default_mode_instructions_replace_mode_names_placeholder() {
    let default_instructions = default_preset()
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");

    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are {known_mode_names}.");
    assert!(default_instructions.contains(&expected_snippet));

    assert!(default_instructions.contains(
        "Use the `request_user_input` tool only when it is listed in the available tools"
    ));
    assert!(
        default_instructions.contains("Ask the user directly with one concise plain-text question")
    );
}

#[test]
fn plan_mode_instructions_falls_back_to_builtin_without_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        plan_mode_instructions(Some(dir.path())),
        COLLABORATION_MODE_PLAN
    );
    assert_eq!(plan_mode_instructions(None), COLLABORATION_MODE_PLAN);
}

#[test]
fn plan_mode_instructions_uses_override_file_when_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let contents = "# Custom plan mode\n\nAsk everything.\n";
    std::fs::write(dir.path().join(PLAN_MODE_INSTRUCTIONS_FILE), contents).expect("write override");

    assert_eq!(plan_mode_instructions(Some(dir.path())), contents);
}

#[test]
fn plan_mode_instructions_ignores_blank_override_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(PLAN_MODE_INSTRUCTIONS_FILE), "   \n\t\n").expect("write blank");

    assert_eq!(
        plan_mode_instructions(Some(dir.path())),
        COLLABORATION_MODE_PLAN
    );
}
