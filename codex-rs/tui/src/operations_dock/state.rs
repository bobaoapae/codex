use codex_local_features::OperationsDockMode;
use codex_protocol::ThreadId;
use codex_protocol::plan_tool::UpdatePlanArgs;
use crossterm::event::KeyEvent;
use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::input;
use super::layout;
use super::mouse;

pub(crate) use super::mouse::DockMouseAction;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DockTab {
    #[default]
    Tasks,
    Agents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockAgentRow {
    pub(crate) thread_id: ThreadId,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) is_main: bool,
}

#[derive(Debug, Default)]
pub(crate) struct OperationsDockState {
    pub(super) mode: OperationsDockMode,
    pub(super) appeared: bool,
    pub(super) expanded: bool,
    pub(super) focused: bool,
    pub(super) tab: DockTab,
    pub(super) scroll: usize,
    pub(super) latest_plan: Option<UpdatePlanArgs>,
    pub(super) agents: Vec<DockAgentRow>,
    pub(super) active_thread_id: Option<ThreadId>,
    pub(super) viewing_label: String,
    pub(super) hit_regions: Vec<mouse::HitRegion>,
}

impl OperationsDockState {
    pub(crate) fn new(mode: OperationsDockMode) -> Self {
        Self {
            mode,
            appeared: mode == OperationsDockMode::Always,
            expanded: mode == OperationsDockMode::Always,
            ..Default::default()
        }
    }

    pub(crate) fn update_plan(&mut self, plan: UpdatePlanArgs) {
        if self.mode == OperationsDockMode::Hidden {
            return;
        }
        self.appeared = true;
        self.expanded = true;
        self.scroll = 0;
        self.latest_plan = Some(plan);
    }

    pub(crate) fn sync_agents(
        &mut self,
        agents: Vec<DockAgentRow>,
        active_thread_id: Option<ThreadId>,
    ) {
        if self.mode == OperationsDockMode::Hidden {
            return;
        }
        if agents.iter().any(|agent| !agent.is_main) {
            self.appeared = true;
        }
        self.active_thread_id = active_thread_id;
        self.viewing_label = active_thread_id
            .and_then(|thread_id| {
                agents
                    .iter()
                    .find(|agent| agent.thread_id == thread_id)
                    .map(|agent| agent.label.clone())
            })
            .unwrap_or_else(|| "Main".to_string());
        self.agents = agents;
        self.scroll = self.scroll.min(self.agents.len().saturating_sub(1));
    }

    pub(crate) fn desired_height(&self, terminal_height: u16) -> u16 {
        layout::desired_height(self, terminal_height)
    }

    pub(crate) fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        layout::render(self, area, buffer);
    }

    pub(crate) fn focus(&mut self) -> bool {
        if !self.appeared {
            return false;
        }
        self.focused = true;
        self.expanded = true;
        true
    }

    pub(crate) fn focus_agents(&mut self) -> bool {
        if !self.focus() {
            return false;
        }
        self.tab = DockTab::Agents;
        self.scroll = self
            .active_thread_id
            .and_then(|thread_id| {
                self.agents
                    .iter()
                    .position(|agent| agent.thread_id == thread_id)
            })
            .unwrap_or(0);
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        input::handle_key(self, key)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> DockMouseAction {
        mouse::handle_mouse(self, event)
    }

    pub(crate) fn blur(&mut self) {
        self.focused = false;
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn latest_plan(&self) -> Option<&UpdatePlanArgs> {
        self.latest_plan.as_ref()
    }

    pub(crate) fn selected_agent_thread_id(&self) -> Option<ThreadId> {
        (self.focused && self.tab == DockTab::Agents)
            .then(|| self.agents.get(self.scroll).map(|row| row.thread_id))
            .flatten()
    }

    pub(crate) fn selected_interruptible_agent_thread_id(&self) -> Option<ThreadId> {
        (self.focused && self.tab == DockTab::Agents)
            .then(|| {
                self.agents
                    .get(self.scroll)
                    .filter(|row| !row.is_main)
                    .map(|row| row.thread_id)
            })
            .flatten()
    }

    pub(super) fn visible(&self) -> bool {
        self.appeared && self.mode != OperationsDockMode::Hidden
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
