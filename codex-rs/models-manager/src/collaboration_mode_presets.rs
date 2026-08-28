use codex_collaboration_mode_templates::DEFAULT as COLLABORATION_MODE_DEFAULT;
use codex_collaboration_mode_templates::PLAN as COLLABORATION_MODE_PLAN;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::TUI_VISIBLE_COLLABORATION_MODES;
use codex_utils_template::Template;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::Once;

const KNOWN_MODE_NAMES_TEMPLATE_KEY: &str = "KNOWN_MODE_NAMES";
static COLLABORATION_MODE_DEFAULT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(COLLABORATION_MODE_DEFAULT)
        .unwrap_or_else(|err| panic!("collaboration mode default template must parse: {err}"))
});

pub fn builtin_collaboration_mode_presets() -> Vec<CollaborationModeMask> {
    vec![plan_preset(), default_preset()]
}

/// FORK: file name inside `$CODEX_HOME` that overrides the built-in Plan-mode instructions.
pub const PLAN_MODE_INSTRUCTIONS_FILE: &str = "plan_mode.md";

static PLAN_MODE_OVERRIDE_LOGGED: Once = Once::new();

/// FORK: resolve the Plan-mode developer instructions.
///
/// When `$CODEX_HOME/plan_mode.md` exists and is not blank, its contents replace the built-in
/// template. Missing files and read errors fall back to the built-in template so Plan mode never
/// breaks because of a bad override.
pub fn plan_mode_instructions(codex_home: Option<&Path>) -> String {
    let Some(codex_home) = codex_home else {
        return COLLABORATION_MODE_PLAN.to_string();
    };
    let path = codex_home.join(PLAN_MODE_INSTRUCTIONS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) if !contents.trim().is_empty() => {
            PLAN_MODE_OVERRIDE_LOGGED.call_once(|| {
                tracing::info!(
                    "using Plan mode instructions override from {}",
                    path.display()
                );
            });
            contents
        }
        Ok(_) => COLLABORATION_MODE_PLAN.to_string(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            COLLABORATION_MODE_PLAN.to_string()
        }
        Err(err) => {
            tracing::warn!(
                "failed to read Plan mode instructions override {}: {err}",
                path.display()
            );
            COLLABORATION_MODE_PLAN.to_string()
        }
    }
}

fn plan_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: ModeKind::Plan.display_name().to_string(),
        mode: Some(ModeKind::Plan),
        model: None,
        // FORK: `None` means "do not pin" so Plan mode inherits the thread's reasoning effort.
        reasoning_effort: None,
        developer_instructions: Some(Some(COLLABORATION_MODE_PLAN.to_string())),
    }
}

fn default_preset() -> CollaborationModeMask {
    CollaborationModeMask {
        name: ModeKind::Default.display_name().to_string(),
        mode: Some(ModeKind::Default),
        model: None,
        reasoning_effort: None,
        developer_instructions: Some(Some(default_mode_instructions())),
    }
}

fn default_mode_instructions() -> String {
    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    COLLABORATION_MODE_DEFAULT_TEMPLATE
        .render([(KNOWN_MODE_NAMES_TEMPLATE_KEY, known_mode_names.as_str())])
        .unwrap_or_else(|err| panic!("collaboration mode default template must render: {err}"))
}

fn format_mode_names(modes: &[ModeKind]) -> String {
    let mode_names: Vec<&str> = modes.iter().map(|mode| mode.display_name()).collect();
    match mode_names.as_slice() {
        [] => "none".to_string(),
        [mode_name] => (*mode_name).to_string(),
        [first, second] => format!("{first} and {second}"),
        [..] => mode_names.join(", "),
    }
}

#[cfg(test)]
#[path = "collaboration_mode_presets_tests.rs"]
mod tests;
