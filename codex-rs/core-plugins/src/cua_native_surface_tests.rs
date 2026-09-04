use super::*;
use codex_config::McpServerTransportConfig;
use std::collections::HashMap;

/// A `cua_repl` entry shaped like the one the Desktop writes on Windows.
fn desktop_cua_repl(env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::Stdio {
            command: "node.exe".to_string(),
            args: vec!["launch.mjs".to_string()],
            env: Some(
                env.iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                    .collect::<HashMap<String, String>>(),
            ),
            env_vars: Vec::new(),
            cwd: None,
        },
        environment_id: "local".to_string(),
        enabled: true,
        required: false,
        supports_parallel_tool_calls: false,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        root_only_tools: None,
        tool_approval_overrides: Default::default(),
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: Default::default(),
    }
}

/// The full env the Desktop writes, with the surface list left to the caller.
fn native_env(surfaces: &str) -> Vec<(&'static str, String)> {
    vec![
        ("SKY_CUA_NATIVE_PIPE", "1".to_string()),
        (
            "SKY_CUA_NATIVE_PIPE_DIRECTORY",
            r"\\.\pipe\codex-computer-use-fd249c73".to_string(),
        ),
        ("CUA_REPL_ENABLED_SURFACES", surfaces.to_string()),
    ]
}

fn native_cua_repl(surfaces: &str) -> McpServerConfig {
    let owned = native_env(surfaces);
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    desktop_cua_repl(&borrowed)
}

fn surfaces_of(config: &McpServerConfig) -> Option<String> {
    let McpServerTransportConfig::Stdio { env, .. } = &config.transport else {
        return None;
    };
    env.as_ref()?.get("CUA_REPL_ENABLED_SURFACES").cloned()
}

#[test]
fn the_windows_browser_only_surface_list_gains_computer() {
    let mut config = native_cua_repl("browser");

    let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Applied {
            before: "browser".to_string(),
            after: "browser,computer".to_string(),
        }
    );
    assert_eq!(surfaces_of(&config).as_deref(), Some("browser,computer"));
}

#[test]
fn an_absent_surface_list_is_left_alone() {
    let mut config = desktop_cua_repl(&[
        ("SKY_CUA_NATIVE_PIPE", "1"),
        ("SKY_CUA_NATIVE_PIPE_DIRECTORY", r"\\.\pipe\codex"),
    ]);
    let before = config.clone();

    let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Skipped("surface list is not pinned")
    );
    assert_eq!(config, before);
}

#[test]
fn a_list_that_already_has_computer_is_reported_as_such() {
    let mut config = native_cua_repl("browser,computer");
    let before = config.clone();

    let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

    assert_eq!(outcome, CuaNativeSurfaceOutcome::AlreadyEnabled);
    assert_eq!(config, before);
}

#[test]
fn a_non_windows_target_is_left_alone() {
    let mut config = native_cua_repl("browser");
    let before = config.clone();

    let outcome = enable_cua_native_surface("cua_repl", &mut config, false, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Skipped("not a Windows target")
    );
    assert_eq!(config, before);
}

#[test]
fn the_escape_hatch_turns_the_pass_off_and_on() {
    let mut disabled = native_cua_repl("browser");
    let before = disabled.clone();
    assert_eq!(
        enable_cua_native_surface("cua_repl", &mut disabled, true, Some(false)),
        CuaNativeSurfaceOutcome::Skipped("disabled by native_computer_surface = false")
    );
    assert_eq!(disabled, before);

    for hatch in [Some(true), None] {
        let mut config = native_cua_repl("browser");
        assert!(matches!(
            enable_cua_native_surface("cua_repl", &mut config, true, hatch),
            CuaNativeSurfaceOutcome::Applied { .. }
        ));
        assert_eq!(surfaces_of(&config).as_deref(), Some("browser,computer"));
    }
}

#[test]
fn a_server_without_a_live_native_pipe_is_left_alone() {
    for env in [
        vec![
            ("SKY_CUA_NATIVE_PIPE", "0"),
            ("SKY_CUA_NATIVE_PIPE_DIRECTORY", r"\\.\pipe\codex"),
            ("CUA_REPL_ENABLED_SURFACES", "browser"),
        ],
        vec![
            ("SKY_CUA_NATIVE_PIPE", "1"),
            ("SKY_CUA_NATIVE_PIPE_DIRECTORY", "   "),
            ("CUA_REPL_ENABLED_SURFACES", "browser"),
        ],
        vec![
            ("SKY_CUA_NATIVE_PIPE", "1"),
            ("CUA_REPL_ENABLED_SURFACES", "browser"),
        ],
    ] {
        let mut config = desktop_cua_repl(&env);
        let before = config.clone();

        let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

        assert!(
            matches!(outcome, CuaNativeSurfaceOutcome::Skipped(_)),
            "expected a skip for {env:?}, got {outcome:?}"
        );
        assert_eq!(config, before);
    }
}

#[test]
fn another_server_with_the_same_env_is_left_alone() {
    let mut config = native_cua_repl("browser");
    let before = config.clone();

    let outcome = enable_cua_native_surface("node_repl", &mut config, true, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Skipped("not the cua_repl server")
    );
    assert_eq!(config, before);
}

#[test]
fn a_non_stdio_transport_is_left_alone() {
    let mut config = native_cua_repl("browser");
    config.transport = McpServerTransportConfig::StreamableHttp {
        url: "https://example.invalid/mcp".to_string(),
        bearer_token_env_var: None,
        http_headers: None,
        env_http_headers: None,
        http_headers_helper: None,
    };
    let before = config.clone();

    let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Skipped("not a stdio transport")
    );
    assert_eq!(config, before);
}

#[test]
fn existing_surfaces_survive_the_append() {
    let mut config = native_cua_repl(" browser , iab ");

    let outcome = enable_cua_native_surface("cua_repl", &mut config, true, None);

    assert_eq!(
        outcome,
        CuaNativeSurfaceOutcome::Applied {
            before: " browser , iab ".to_string(),
            after: " browser , iab ,computer".to_string(),
        }
    );
    assert_eq!(
        surfaces_of(&config).as_deref(),
        Some(" browser , iab ,computer")
    );
}

/// FORK invariant: on Windows, a `cua_repl` that advertises a live Computer Use
/// kernel must end up with the `computer` surface. Without it the direct `js`
/// tool has no `sky.*` at all and Computer Use reports itself as
/// "not configured", while the same kernel still answers through `node_repl`.
#[test]
fn fork_invariant_cua_repl_native_surface_is_enabled_on_windows() {
    let mut config = native_cua_repl("browser");

    enable_cua_native_surface("cua_repl", &mut config, cfg!(windows), None);

    let expected = if cfg!(windows) {
        "browser,computer"
    } else {
        "browser"
    };
    assert_eq!(surfaces_of(&config).as_deref(), Some(expected));
}
