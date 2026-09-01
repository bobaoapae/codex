use codex_agent_roles::AgentRoleCapabilities;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use rand::prelude::IndexedRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::watch;

use super::lifecycle::AgentLifecycle;
use super::lifecycle::AgentLifecycleStatus;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `AgentControl`
/// is).
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    total_count: AtomicUsize,
    /// FORK: what each agent was last seen doing, and when.
    ///
    /// The parent had no way to tell a child that was compiling from one that
    /// was wedged, so it interrupted on a hunch: 593 `interrupt_agent` calls in
    /// 30 days, 70% of them aimed at children that were still working. One line
    /// of "last ran `cargo test` 40s ago" is what makes that decision possible.
    last_activity: Mutex<HashMap<ThreadId, AgentActivity>>,
    /// Root-scoped causal revision for mailbox and lifecycle changes.
    revision: AtomicU64,
    /// Most recent causal change for each agent.
    last_changes: Mutex<HashMap<ThreadId, AgentChange>>,
    /// Last status seen for each agent, used to suppress duplicate events.
    last_status: Mutex<HashMap<ThreadId, AgentStatus>>,
    change_lock: Mutex<()>,
    revision_tx: watch::Sender<u64>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        let (revision_tx, _revision_rx) = watch::channel(0);
        Self {
            active_agents: Mutex::default(),
            total_count: AtomicUsize::default(),
            last_activity: Mutex::default(),
            revision: AtomicU64::default(),
            last_changes: Mutex::default(),
            last_status: Mutex::default(),
            change_lock: Mutex::default(),
            revision_tx,
        }
    }
}

/// FORK: the most recent thing an agent was observed doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentActivity {
    /// Unix milliseconds when it was observed.
    pub(crate) at_ms: u64,
    /// Short human label, e.g. "ran `cargo test`" or "edited src/lib.rs".
    pub(crate) label: String,
}

/// Why an agent's causal wait revision changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentChangeKind {
    Message,
    StatusChanged,
    NeedsAttention,
    Terminal,
}

/// The latest causal change and its root-scoped revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgentChange {
    pub(crate) revision: u64,
    pub(crate) kind: AgentChangeKind,
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    thread_paths: HashMap<ThreadId, RegisteredAgent>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
}

struct RegisteredAgent {
    path: String,
    evicted_environments: Option<Vec<TurnEnvironmentSelection>>,
    /// Whether this identity currently owns a logical spawn slot.  A
    /// terminal or evicted agent remains registered for lineage and follow-up
    /// resolution, but must not consume this slot.
    active: bool,
    /// Logical follow-up generation.  The first spawn is generation zero;
    /// each follow-up that starts after a terminal generation increments it.
    generation: u64,
    status: AgentStatus,
    /// Explicit close is distinct from a terminal turn.  Closed identities
    /// remain available for rollout-backed resume, but are not listable or
    /// follow-up eligible until resumed.
    closed: bool,
}

impl RegisteredAgent {
    fn new(path: String) -> Self {
        Self {
            path,
            evicted_environments: None,
            active: false,
            generation: 0,
            status: AgentStatus::PendingInit,
            closed: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AgentMetadata {
    pub(crate) agent_id: Option<ThreadId>,
    pub(crate) agent_path: Option<AgentPath>,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
}

impl AgentMetadata {
    /// Resolve role capabilities without persisting or injecting extra context.
    ///
    /// Role names are untrusted metadata; the resolver fails closed for omitted,
    /// custom, and unknown names.
    pub(crate) fn role_capabilities(&self) -> AgentRoleCapabilities {
        crate::agent::role::role_capabilities(self.agent_role.as_deref())
    }

    /// Whether mutation is effective after the ownership layer grants a lease.
    pub(crate) fn effective_mutation_allowed(&self, workspace_lease_held: bool) -> bool {
        self.role_capabilities()
            .effective_mutation_allowed(workspace_lease_held)
    }
}

impl AgentRegistry {
    /// FORK: records what an agent was last seen doing.
    pub(crate) fn record_activity(&self, thread_id: ThreadId, label: String) {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id, AgentActivity { at_ms, label });
    }

    /// FORK: what an agent was last seen doing, if anything.
    pub(crate) fn activity(&self, thread_id: ThreadId) -> Option<AgentActivity> {
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn subscribe_revision(&self) -> watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    pub(crate) fn last_change(&self, thread_id: ThreadId) -> Option<AgentChange> {
        self.last_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .copied()
    }

    pub(crate) fn record_message(&self, thread_id: ThreadId) -> u64 {
        self.record_change(thread_id, AgentChangeKind::Message)
    }

    /// Publish a non-terminal status edge without changing the protocol
    /// status or logical lifecycle bookkeeping.
    pub(crate) fn record_status_edge(&self, thread_id: ThreadId) -> u64 {
        self.record_change(thread_id, AgentChangeKind::StatusChanged)
    }

    /// Publish a non-terminal needs-attention edge without mutating the
    /// agent's protocol status or logical lifecycle slot.
    pub(crate) fn record_needs_attention(&self, thread_id: ThreadId) -> Option<u64> {
        let _change_lock = self
            .change_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut last_changes = self
            .last_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last_changes
            .get(&thread_id)
            .is_some_and(|change| change.kind == AgentChangeKind::NeedsAttention)
        {
            return None;
        }
        let revision = self.revision.load(Ordering::Relaxed).saturating_add(1);
        last_changes.insert(
            thread_id,
            AgentChange {
                revision,
                kind: AgentChangeKind::NeedsAttention,
            },
        );
        self.revision.store(revision, Ordering::Release);
        self.revision_tx.send_replace(revision);
        Some(revision)
    }

    pub(crate) fn record_status_change(
        &self,
        thread_id: ThreadId,
        status: AgentStatus,
    ) -> Option<u64> {
        let status_key = status_kind(&status);
        let changed = {
            let mut statuses = self
                .last_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if statuses
                .get(&thread_id)
                .is_some_and(|previous| status_kind(previous) == status_key)
            {
                false
            } else {
                statuses.insert(thread_id, status.clone());
                true
            }
        };
        if !changed {
            return None;
        }

        let terminal = is_generation_terminal(&status);
        let kind = if terminal {
            AgentChangeKind::Terminal
        } else {
            AgentChangeKind::StatusChanged
        };
        {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
                agent.status = status.clone();
                if terminal && agent.active {
                    agent.active = false;
                    if agent.path != AgentPath::ROOT {
                        self.total_count.fetch_sub(1, Ordering::AcqRel);
                    }
                } else if matches!(status, AgentStatus::Running)
                    && !agent.active
                    && !agent.closed
                    && agent.path != AgentPath::ROOT
                {
                    // A resumed runtime may begin a turn from queued work
                    // without passing through one of the explicit input
                    // helpers.  Reconcile the logical slot at the event edge.
                    agent.active = true;
                    self.total_count.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
        Some(self.record_change(thread_id, kind))
    }

    fn record_change(&self, thread_id: ThreadId, kind: AgentChangeKind) -> u64 {
        let _change_lock = self
            .change_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Publish the revision only after the per-agent change is visible. A
        // waiter that samples the revision concurrently with this method must
        // never observe a new revision while its corresponding change is
        // still absent from `last_changes`.
        let revision = self.revision.load(Ordering::Relaxed).saturating_add(1);
        self.last_changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id, AgentChange { revision, kind });
        self.revision.store(revision, Ordering::Release);
        self.revision_tx.send_replace(revision);
        revision
    }
}

fn status_kind(status: &AgentStatus) -> u8 {
    match status {
        AgentStatus::PendingInit => 0,
        AgentStatus::Running => 1,
        AgentStatus::Interrupted => 2,
        AgentStatus::Completed(_) => 3,
        AgentStatus::Errored(_) => 4,
        AgentStatus::Shutdown => 5,
        AgentStatus::NotFound => 6,
    }
}

fn is_generation_terminal(status: &AgentStatus) -> bool {
    AgentLifecycleStatus::from_agent_status(status, None).is_terminal()
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn agent_matches_prefix(path: &AgentPath, prefix: &AgentPath) -> bool {
    prefix.is_root()
        || path == prefix
        || path
            .as_str()
            .strip_prefix(prefix.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

impl AgentRegistry {
    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads,
                }));
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
            reactivating_thread: None,
        })
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(agent) = active_agents.thread_paths.remove(&thread_id) {
                let counted = agent.active && agent.path != AgentPath::ROOT;
                active_agents.agent_tree.remove(agent.path.as_str());
                counted
            } else {
                false
            }
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Release a logical spawn slot while retaining the identity, lineage,
    /// generation, and rollout lookup needed for a later follow-up or resume.
    pub(crate) fn release_active_slot(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id)
            && agent.active
            && agent.path != AgentPath::ROOT
        {
            agent.active = false;
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Mark an identity as explicitly closed without deleting its lineage or
    /// persisted rollout.  A later resume may reactivate the identity while
    /// the persisted graph edge remains closed.
    pub(crate) fn mark_agent_closed(&self, thread_id: ThreadId) {
        let known = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
                if agent.active && agent.path != AgentPath::ROOT {
                    self.total_count.fetch_sub(1, Ordering::AcqRel);
                }
                agent.active = false;
                agent.closed = true;
                true
            } else {
                false
            }
        };
        if known {
            let _ = self.record_status_change(thread_id, AgentStatus::NotFound);
        }
    }

    pub(crate) fn is_agent_closed(&self, thread_id: ThreadId) -> bool {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_paths
            .get(&thread_id)
            .is_some_and(|agent| agent.closed)
    }

    pub(crate) fn is_agent_active(&self, thread_id: ThreadId) -> bool {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_paths
            .get(&thread_id)
            .is_some_and(|agent| agent.active)
    }

    /// Prepare an existing identity for a rollout-backed resume.  This
    /// reserves a slot but deliberately leaves the previous terminal status
    /// and generation intact until the caller starts a new turn.
    pub(crate) fn reserve_existing_spawn_slot(
        self: &Arc<Self>,
        thread_id: ThreadId,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        let is_reactivatable = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .thread_paths
            .get(&thread_id)
            .is_some_and(|agent| !agent.active && agent.path != AgentPath::ROOT);
        if !is_reactivatable {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent {thread_id} is already active or has no resumable identity"
            )));
        }
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads,
                }));
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
            reactivating_thread: Some(thread_id),
        })
    }

    /// Start a follow-up generation atomically with logical slot reacquisition.
    /// The status is changed to `PendingInit` by `AgentControl` after this
    /// critical section so the causal revision is published exactly once.
    pub(crate) fn begin_followup_generation(
        &self,
        thread_id: ThreadId,
        status_hint: AgentStatus,
        max_threads: Option<usize>,
    ) -> Result<u64> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent = active_agents
            .thread_paths
            .get_mut(&thread_id)
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        if agent.closed {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent {thread_id} is closed; resume it before sending a follow-up"
            )));
        }

        let status = if agent.active && !is_generation_terminal(&agent.status) {
            agent.status.clone()
        } else if is_generation_terminal(&status_hint) {
            status_hint
        } else if is_generation_terminal(&agent.status) && !agent.active {
            agent.status.clone()
        } else {
            status_hint
        };
        if !agent.active && agent.path != AgentPath::ROOT {
            match max_threads {
                Some(max_threads) if !self.try_increment_spawned(max_threads) => {
                    return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                        max_threads,
                    }));
                }
                Some(_) | None => {
                    if max_threads.is_none() {
                        self.total_count.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            agent.active = true;
        }
        if is_generation_terminal(&status) {
            agent.generation = agent.generation.saturating_add(1);
            agent.status = AgentStatus::PendingInit;
        }
        Ok(agent.generation)
    }

    /// Reacquire a logical slot for an input that starts a turn on an idle or
    /// resumed identity without advancing its generation.
    pub(crate) fn ensure_active_slot(
        &self,
        thread_id: ThreadId,
        max_threads: Option<usize>,
    ) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent = active_agents
            .thread_paths
            .get_mut(&thread_id)
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        if agent.closed {
            return Err(CodexErr::UnsupportedOperation(format!(
                "agent {thread_id} is closed; resume it before sending input"
            )));
        }
        if agent.active || agent.path == AgentPath::ROOT {
            return Ok(());
        }
        match max_threads {
            Some(max_threads) if !self.try_increment_spawned(max_threads) => {
                Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads,
                }))
            }
            Some(_) | None => {
                if max_threads.is_none() {
                    self.total_count.fetch_add(1, Ordering::AcqRel);
                }
                agent.active = true;
                Ok(())
            }
        }
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root_path = AgentPath::ROOT.to_string();
        let root_thread_id = active_agents
            .agent_tree
            .entry(root_path.clone())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            })
            .agent_id;
        if let Some(root_thread_id) = root_thread_id {
            active_agents
                .thread_paths
                .entry(root_thread_id)
                .or_insert_with(|| RegisteredAgent::new(root_path));
        }
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
    }

    pub(crate) fn agent_entries_for_prefix(
        &self,
        prefix: Option<&AgentPath>,
    ) -> Vec<(ThreadId, AgentPath)> {
        self.agent_entries_for_prefix_with_filter(prefix, true)
    }

    pub(crate) fn all_agent_entries_for_prefix(
        &self,
        prefix: Option<&AgentPath>,
    ) -> Vec<(ThreadId, AgentPath)> {
        self.agent_entries_for_prefix_with_filter(prefix, false)
    }

    fn agent_entries_for_prefix_with_filter(
        &self,
        prefix: Option<&AgentPath>,
        exclude_closed: bool,
    ) -> Vec<(ThreadId, AgentPath)> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = active_agents
            .thread_paths
            .values()
            .filter_map(|agent| {
                if exclude_closed && agent.closed {
                    return None;
                }
                let metadata = active_agents.agent_tree.get(&agent.path)?;
                let metadata_thread_id = metadata.agent_id?;
                let path = metadata.agent_path.clone()?;
                if prefix.is_some_and(|prefix| !agent_matches_prefix(&path, prefix)) {
                    return None;
                }
                Some((metadata_thread_id, path))
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
        });
        entries
    }

    pub(crate) fn status_for_thread(
        &self,
        thread_id: ThreadId,
        observed: AgentStatus,
    ) -> AgentStatus {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(agent) = active_agents.thread_paths.get(&thread_id) else {
            return observed;
        };
        if agent.closed {
            AgentStatus::NotFound
        } else if matches!(observed, AgentStatus::NotFound) {
            agent.status.clone()
        } else {
            observed
        }
    }

    pub(crate) fn lifecycle(
        &self,
        thread_id: ThreadId,
        observed: AgentStatus,
        activity_label: Option<&str>,
    ) -> AgentLifecycle {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (status, generation) = match active_agents.thread_paths.get(&thread_id) {
            Some(agent) if agent.closed => (AgentStatus::NotFound, agent.generation),
            Some(agent) if matches!(observed, AgentStatus::NotFound) => {
                if !agent.active
                    && matches!(
                        agent.status,
                        AgentStatus::PendingInit | AgentStatus::Running
                    )
                {
                    (AgentStatus::NotFound, agent.generation)
                } else {
                    (agent.status.clone(), agent.generation)
                }
            }
            Some(agent) => (observed, agent.generation),
            None => (observed, 0),
        };
        AgentLifecycle::from_agent_status(&status, generation, activity_label)
    }

    /// Seed or repair the persisted identity's status during process restart.
    /// `active` is intentionally explicit because an open graph edge does not
    /// imply that a runtime is currently executing.
    pub(crate) fn restore_agent_lifecycle(
        &self,
        thread_id: ThreadId,
        status: AgentStatus,
        generation: u64,
        active: bool,
    ) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) else {
            return;
        };
        if agent.active && !active && agent.path != AgentPath::ROOT {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        } else if !agent.active && active && agent.path != AgentPath::ROOT {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        agent.active = active;
        agent.generation = generation;
        agent.status = status.clone();
        self.last_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id, status);
    }

    pub(crate) fn save_evicted_environments(
        &self,
        thread_id: ThreadId,
        environments: Vec<TurnEnvironmentSelection>,
    ) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = Some(environments);
        }
    }

    pub(crate) fn evicted_environments(
        &self,
        thread_id: ThreadId,
    ) -> Option<Vec<TurnEnvironmentSelection>> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| agent.evicted_environments.clone())
    }

    pub(crate) fn clear_evicted_environments(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .values()
            .filter(|agent| !agent.closed)
            .filter_map(|agent| active_agents.agent_tree.get(&agent.path))
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    fn register_spawned_thread(&self, agent_metadata: AgentMetadata) {
        self.register_spawned_thread_with_state(agent_metadata, false, false);
    }

    fn register_spawned_thread_with_state(
        &self,
        agent_metadata: AgentMetadata,
        active: bool,
        reactivating: bool,
    ) {
        let Some(thread_id) = agent_metadata.agent_id else {
            return;
        };
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        let previous_state = active_agents.thread_paths.get(&thread_id).map(|agent| {
            (
                agent.path.clone(),
                agent.active,
                agent.generation,
                agent.status.clone(),
            )
        });
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        if let Some((previous_path, _, _, _)) = previous_state.as_ref()
            && previous_path != &key
        {
            active_agents.agent_tree.remove(previous_path.as_str());
        }
        if let Some(previous_metadata) =
            active_agents.agent_tree.insert(key.clone(), agent_metadata)
            && let Some(previous_thread_id) = previous_metadata.agent_id
            && previous_thread_id != thread_id
            && let Some(previous_agent) = active_agents.thread_paths.remove(&previous_thread_id)
            && previous_agent.active
            && previous_agent.path != AgentPath::ROOT
        {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
        let mut registered = RegisteredAgent::new(key);
        registered.active = active;
        if let Some((_, was_active, generation, status)) = previous_state {
            if reactivating || was_active {
                registered.generation = generation;
                registered.status = status;
            }
            if reactivating {
                registered.closed = false;
            }
        } else if let Some(status) = self
            .last_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
        {
            registered.status = status;
        }
        active_agents.thread_paths.insert(thread_id, registered);
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_names: Vec<String> = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .collect();
            if let Some(name) = available_names.choose(&mut rand::rng()) {
                name.clone()
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn agent_path_belongs_to(&self, thread_id: ThreadId, agent_path: &AgentPath) -> bool {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
            == Some(thread_id)
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
    reactivating_thread: Option<ThreadId>,
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
            })?;
        self.reserved_agent_nickname = Some(agent_nickname.clone());
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        if let Some(thread_id) = self.reactivating_thread
            && self.state.agent_path_belongs_to(thread_id, agent_path)
        {
            return Ok(());
        }
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) {
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.state.register_spawned_thread_with_state(
            agent_metadata,
            true,
            self.reactivating_thread.is_some(),
        );
        self.active = false;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
