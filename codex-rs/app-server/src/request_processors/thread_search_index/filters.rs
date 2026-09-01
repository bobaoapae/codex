use codex_app_server_protocol::TerminalOutcome;
use codex_app_server_protocol::ThreadClass;
use codex_app_server_protocol::ThreadSourceKind;
use codex_core::path_utils;
use codex_protocol::protocol::SessionSource;
use codex_state::WorkflowStore;
use codex_state::WorkflowThreadClass;
use codex_thread_store::StoredThread;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadClassification {
    pub(crate) class: ThreadClass,
    pub(crate) outcome: Option<TerminalOutcome>,
    pub(crate) root_thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadFilterOptions<'a> {
    pub(crate) model_providers: Option<&'a [String]>,
    pub(crate) cwd_filters: Option<&'a [PathBuf]>,
    pub(crate) archived: Option<bool>,
    pub(crate) project_id: Option<&'a Option<String>>,
    pub(crate) root_thread_id: Option<&'a str>,
    pub(crate) source_kinds: Option<&'a [ThreadSourceKind]>,
    pub(crate) thread_classes: Option<&'a [ThreadClass]>,
    pub(crate) terminal_outcomes: Option<&'a [TerminalOutcome]>,
    pub(crate) relation_ids: Option<&'a HashSet<String>>,
}

/// Load workflow classifications in one bounded batch. Historical rows that
/// predate the workflow projection only receive a safe source-based class.
pub(crate) async fn classify_threads(
    workflow: Option<&WorkflowStore>,
    threads: &[StoredThread],
) -> HashMap<String, ThreadClassification> {
    let mut classifications = threads
        .iter()
        .map(|thread| {
            (
                thread.thread_id.to_string(),
                ThreadClassification {
                    class: infer_historical_thread_class(thread),
                    outcome: None,
                    root_thread_id: None,
                },
            )
        })
        .collect::<HashMap<_, _>>();

    let Some(workflow) = workflow else {
        return classifications;
    };
    let thread_ids = classifications.keys().cloned().collect::<Vec<_>>();
    let Ok(runs) = workflow.get_runs_by_thread_ids(&thread_ids).await else {
        return classifications;
    };
    let mut latest_runs = HashMap::new();
    for run in runs {
        let replace =
            latest_runs
                .get(&run.thread_id)
                .is_none_or(|current: &codex_state::WorkflowRun| {
                    (run.updated_at_ms, run.created_at_ms, run.run_id.as_str())
                        > (
                            current.updated_at_ms,
                            current.created_at_ms,
                            current.run_id.as_str(),
                        )
                });
        if replace {
            latest_runs.insert(run.thread_id.clone(), run);
        }
    }
    for (thread_id, run) in latest_runs {
        let Some(classification) = classifications.get_mut(&thread_id) else {
            continue;
        };
        classification.class = workflow_thread_class_to_api(run.thread_class);
        classification.outcome = run.outcome.as_deref().and_then(parse_terminal_outcome);
        classification.root_thread_id = run.root_thread_id;
    }
    classifications
}

/// Infer only classes supported by old rollout metadata. In particular, a
/// missing row can never be mistaken for a transient job.
pub(crate) fn infer_historical_thread_class(thread: &StoredThread) -> ThreadClass {
    if thread.source.is_non_root_agent() || thread.parent_thread_id.is_some() {
        ThreadClass::SubAgent
    } else if matches!(thread.source, SessionSource::Exec) {
        ThreadClass::LegacyExec
    } else {
        ThreadClass::Interactive
    }
}

pub(crate) fn thread_matches_filters(
    thread: &StoredThread,
    classification: &ThreadClassification,
    filters: ThreadFilterOptions<'_>,
) -> bool {
    if filters
        .archived
        .is_some_and(|archived| thread.archived_at.is_some() != archived)
    {
        return false;
    }
    if filters.model_providers.is_some_and(|providers| {
        !providers.is_empty()
            && !providers
                .iter()
                .any(|provider| provider == &thread.model_provider)
    }) {
        return false;
    }
    if filters.cwd_filters.is_some_and(|filters| {
        !filters
            .iter()
            .any(|cwd| path_utils::paths_match_after_normalization(&thread.cwd, cwd))
    }) {
        return false;
    }
    if filters
        .project_id
        .is_some_and(|expected| thread.project_id.as_ref() != expected.as_ref())
    {
        return false;
    }
    if filters.relation_ids.is_none()
        && filters.root_thread_id.is_some_and(|root| {
            classification.root_thread_id.as_deref() != Some(root)
                && thread.thread_id.to_string() != root
        })
    {
        return false;
    }
    if filters.source_kinds.is_some_and(|filter| {
        !crate::request_processors::source_kind_matches(&thread.source, filter)
    }) {
        return false;
    }
    if filters
        .thread_classes
        .is_some_and(|classes| !classes.contains(&classification.class))
    {
        return false;
    }
    if filters.thread_classes.is_none() && classification.class == ThreadClass::TransientJob {
        return false;
    }
    if filters.terminal_outcomes.is_some_and(|outcomes| {
        classification
            .outcome
            .is_none_or(|outcome| !outcomes.contains(&outcome))
    }) {
        return false;
    }
    if filters
        .relation_ids
        .is_some_and(|ids| !ids.contains(&thread.thread_id.to_string()))
    {
        return false;
    }
    true
}

fn workflow_thread_class_to_api(class: WorkflowThreadClass) -> ThreadClass {
    match class {
        WorkflowThreadClass::Interactive => ThreadClass::Interactive,
        WorkflowThreadClass::SubAgent => ThreadClass::SubAgent,
        WorkflowThreadClass::TransientJob => ThreadClass::TransientJob,
        WorkflowThreadClass::Internal => ThreadClass::Internal,
        WorkflowThreadClass::LegacyExec => ThreadClass::LegacyExec,
    }
}

fn parse_terminal_outcome(value: &str) -> Option<TerminalOutcome> {
    match value {
        "succeeded" => Some(TerminalOutcome::Succeeded),
        "failed" => Some(TerminalOutcome::Failed),
        "blocked" => Some(TerminalOutcome::Blocked),
        "inconclusive" => Some(TerminalOutcome::Inconclusive),
        "cancelled" => Some(TerminalOutcome::Cancelled),
        "aborted" => Some(TerminalOutcome::Aborted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_thread_store::ExtraConfig;

    fn stored_thread(source: SessionSource, parent_thread_id: Option<ThreadId>) -> StoredThread {
        StoredThread {
            thread_id: ThreadId::new(),
            extra_config: Some(ExtraConfig {}),
            rollout_path: None,
            forked_from_id: None,
            parent_thread_id,
            preview: String::new(),
            name: None,
            model_provider: "mock".to_string(),
            model: None,
            reasoning_effort: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            recency_at: Utc::now(),
            archived_at: None,
            section: None,
            section_position: None,
            section_entered_at: None,
            project_id: None,
            cwd: PathBuf::from("."),
            cli_version: String::new(),
            source,
            history_mode: Default::default(),
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            git_info: None,
            approval_mode: Default::default(),
            permission_profile: Default::default(),
            token_usage: None,
            first_user_message: None,
            history: None,
        }
    }

    #[test]
    fn historical_classification_does_not_infer_transient_jobs() {
        let exec = stored_thread(SessionSource::Exec, None);
        assert_eq!(
            infer_historical_thread_class(&exec),
            ThreadClass::LegacyExec
        );
        let child = stored_thread(SessionSource::Cli, Some(ThreadId::new()));
        assert_eq!(infer_historical_thread_class(&child), ThreadClass::SubAgent);
        let cli = stored_thread(SessionSource::Cli, None);
        assert_eq!(
            infer_historical_thread_class(&cli),
            ThreadClass::Interactive
        );
    }

    #[test]
    fn archive_filter_uses_current_hydrated_thread_state() {
        let mut thread = stored_thread(SessionSource::Cli, None);
        let classification = ThreadClassification {
            class: ThreadClass::Interactive,
            outcome: None,
            root_thread_id: None,
        };
        let filters = ThreadFilterOptions {
            model_providers: None,
            cwd_filters: None,
            archived: Some(false),
            project_id: None,
            root_thread_id: None,
            source_kinds: None,
            thread_classes: None,
            terminal_outcomes: None,
            relation_ids: None,
        };

        assert!(thread_matches_filters(&thread, &classification, filters));
        thread.archived_at = Some(Utc::now());
        assert!(!thread_matches_filters(&thread, &classification, filters));
        assert!(thread_matches_filters(
            &thread,
            &classification,
            ThreadFilterOptions {
                archived: Some(true),
                ..filters
            }
        ));

        thread.archived_at = None;
        assert!(!thread_matches_filters(
            &thread,
            &classification,
            ThreadFilterOptions {
                archived: Some(true),
                ..filters
            }
        ));
    }
}
