use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_core::CodexAppsToolsCache;
use codex_core::ThreadManager;
use codex_core::build_models_manager;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::ThreadStoreConfig;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::models::LocalImagePreparation;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ThreadStore;
use codex_tui::Cli as TuiCli;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use std::sync::Arc;

/// CLI arguments for read-only model-context inspection.
#[derive(Debug, Parser)]
pub(crate) struct DebugContextCommand {
    /// Inspect a stored thread without loading or resuming it.
    #[arg(
        long = "thread-id",
        value_name = "THREAD_ID",
        value_parser = ThreadId::from_string,
        conflicts_with_all = ["prompt", "images"]
    )]
    pub(crate) thread_id: Option<ThreadId>,

    /// Include bounded, redacted previews for safe text items.
    #[arg(long = "include-preview", default_value_t = false)]
    pub(crate) include_preview: bool,

    /// Optional user prompt to include in a fresh speculative context.
    #[arg(value_name = "PROMPT", conflicts_with = "thread_id")]
    pub(crate) prompt: Option<String>,

    /// Optional image(s) to include in a fresh speculative context.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1,
        conflicts_with = "thread_id"
    )]
    pub(crate) images: Vec<PathBuf>,
}

pub(crate) async fn run(
    command: DebugContextCommand,
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    let root_prompt = interactive.prompt.as_deref();
    let root_images = &interactive.shared.images;
    validate_inputs(
        command.thread_id,
        command.prompt.as_deref(),
        &command.images,
        root_prompt,
        root_images,
    )?;

    let thread_id = command.thread_id;
    let include_preview = command.include_preview;
    let (config, input) = build_config_and_input(
        command,
        root_config_overrides,
        interactive,
        &arg0_paths,
        thread_id.is_some(),
    )
    .await?;
    let thread_manager = build_thread_manager(&config).await?;

    let inspection = if let Some(thread_id) = thread_id {
        thread_manager
            .inspect_stored_context(
                thread_id,
                codex_core::context_inspection::ContextInspectionOptions {
                    mode: codex_core::context_inspection::ContextInspectionMode::Cold,
                    include_preview,
                    turn_id: None,
                },
            )
            .await?
    } else {
        inspect_speculative_context(&thread_manager, config, input, include_preview).await?
    };

    println!("{}", serde_json::to_string_pretty(&inspection)?);
    Ok(())
}

fn validate_inputs(
    thread_id: Option<ThreadId>,
    command_prompt: Option<&str>,
    command_images: &[PathBuf],
    root_prompt: Option<&str>,
    root_images: &[PathBuf],
) -> anyhow::Result<()> {
    if thread_id.is_some()
        && (command_prompt.is_some()
            || !command_images.is_empty()
            || root_prompt.is_some()
            || !root_images.is_empty())
    {
        anyhow::bail!(
            "`--thread-id` cannot be combined with a prompt or images; stored inspection is read-only"
        );
    }
    Ok(())
}

async fn build_config_and_input(
    command: DebugContextCommand,
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: &Arg0DispatchPaths,
    detached_cold: bool,
) -> anyhow::Result<(Config, Vec<UserInput>)> {
    let loader_overrides =
        super::loader_overrides_for_profile(interactive.config_profile_v2.as_ref())?;
    let shared = interactive.shared.into_inner();
    let mut cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    if interactive.web_search {
        cli_kv_overrides.push((
            "web_search".to_string(),
            toml::Value::String("live".to_string()),
        ));
    }
    if detached_cold {
        // ThreadManager construction owns a HostSkillsService which installs bundled skills as a
        // startup convenience. A detached inspection must not create or modify CODEX_HOME, and
        // cold reconstruction does not consume dynamic skill contributors, so suppress that one
        // installation side effect for this manager-only config.
        cli_kv_overrides.push((
            "skills.bundled.enabled".to_string(),
            toml::Value::Boolean(false),
        ));
    }

    let approval_policy = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(AskForApproval::Never)
    } else {
        interactive.approval_policy.map(Into::into)
    };
    let sandbox_mode = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(codex_protocol::config_types::SandboxMode::DangerFullAccess)
    } else {
        shared.sandbox_mode.map(Into::into)
    };
    let overrides = ConfigOverrides {
        model: shared.model,
        approval_policy,
        sandbox_mode,
        cwd: shared.cwd,
        codex_self_exe: arg0_paths.codex_self_exe.clone(),
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe.clone(),
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe.clone(),
        show_raw_agent_reasoning: shared.oss.then_some(true),
        ephemeral: Some(true),
        bypass_hook_trust: shared.bypass_hook_trust.then_some(true),
        additional_writable_roots: shared.add_dir,
        ..Default::default()
    };
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(overrides)
        .loader_overrides(loader_overrides)
        .build()
        .await?;

    let mut input = shared
        .images
        .into_iter()
        .chain(command.images)
        .map(|path| UserInput::LocalImage { path, detail: None })
        .collect::<Vec<_>>();
    if let Some(prompt) = command.prompt.or(interactive.prompt) {
        input.push(UserInput::Text {
            text: prompt.replace("\r\n", "\n").replace('\r', "\n"),
            text_elements: Vec::new(),
        });
    }

    Ok((config, input))
}

async fn build_thread_manager(config: &Config) -> anyhow::Result<ThreadManager> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await?;
    let mut extensions = ExtensionRegistryBuilder::new();
    codex_git_attribution::install(
        &mut extensions,
        Arc::clone(&auth_manager),
        config.chatgpt_base_url.clone(),
        config.http_client_factory(),
    );
    codex_skills_extension::install(&mut extensions, |config: &Config| {
        codex_skills_extension::SkillsExtensionConfig {
            include_instructions: config.include_skill_instructions,
            max_context_tokens: config.skill_max_context_tokens,
            bundled_skills_enabled: config.bundled_skills_enabled(),
            orchestrator_skills_enabled: config.orchestrator_skills_enabled,
            shadow_selection_enabled: config
                .features
                .enabled(codex_features::Feature::SkillSearch),
        }
    });
    let extensions = Arc::new(extensions.build());
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(
            config.codex_home.clone(),
            Some(local_runtime_paths),
            config.http_client_factory(),
        )
        .await?,
    );
    let thread_store: Arc<dyn ThreadStore> = match &config.experimental_thread_store {
        ThreadStoreConfig::Local => Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(config),
            /*state_db*/ None,
        )),
        ThreadStoreConfig::InMemory { id } => InMemoryThreadStore::for_id(id),
    };
    let installation_id = "debug-context".to_string();
    let models_manager = build_models_manager(config, Arc::clone(&auth_manager));

    Ok(ThreadManager::new(
        config,
        auth_manager,
        models_manager,
        CodexAppsToolsCache::default(),
        SessionSource::Exec,
        environment_manager,
        extensions,
        user_instructions_provider,
        /*analytics_events_client*/ None,
        codex_core::passthrough_image_store(),
        thread_store,
        /*agent_graph_store*/ None,
        installation_id,
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    ))
}

async fn inspect_speculative_context(
    thread_manager: &ThreadManager,
    config: Config,
    input: Vec<UserInput>,
    include_preview: bool,
) -> anyhow::Result<codex_core::context_inspection::ContextInspection> {
    let initial_history = if input.is_empty() {
        InitialHistory::New
    } else {
        let response_item: ResponseItem =
            ResponseInputItem::from_user_input(input, LocalImagePreparation::Defer).into();
        InitialHistory::Forked(vec![RolloutItem::ResponseItem(response_item.into())])
    };
    let mut options = codex_core::StartThreadOptions::new(config);
    options.initial_history = initial_history;
    let thread = thread_manager.start_thread(options).await?;
    let inspection = thread
        .thread
        .inspect_context(codex_core::context_inspection::ContextInspectionOptions {
            mode: codex_core::context_inspection::ContextInspectionMode::Loaded,
            include_preview,
            turn_id: None,
        })
        .await;
    let shutdown = thread.thread.shutdown_and_wait().await;
    let _removed = thread_manager.remove_thread(&thread.thread_id).await;
    shutdown?;
    Ok(inspection?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn valid_thread_id() -> &'static str {
        "123e4567-e89b-12d3-a456-426614174000"
    }

    #[test]
    fn parses_cold_context_options() {
        let command = DebugContextCommand::try_parse_from([
            "codex",
            "--thread-id",
            valid_thread_id(),
            "--include-preview",
        ])
        .expect("context options should parse");

        assert_eq!(
            command.thread_id,
            ThreadId::from_string(valid_thread_id()).ok()
        );
        assert!(command.include_preview);
        assert!(command.prompt.is_none());
        assert!(command.images.is_empty());
    }

    #[test]
    fn invalid_thread_id_is_rejected_by_clap() {
        let error = DebugContextCommand::try_parse_from(["codex", "--thread-id", "not-a-uuid"])
            .expect_err("invalid thread id should fail");

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn stored_context_rejects_new_session_inputs() {
        let thread_id = ThreadId::from_string(valid_thread_id()).expect("valid thread id");
        let error = validate_inputs(Some(thread_id), Some("prompt"), &[], None, &[])
            .expect_err("stored inspection must reject prompt input");

        assert!(error.to_string().contains("cannot be combined"));
    }
}
