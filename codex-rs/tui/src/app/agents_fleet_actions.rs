use codex_app_server_protocol::FleetMemberState;
use ratatui::style::Stylize;
use ratatui::text::Span;

pub(super) fn fleet_state_label(state: FleetMemberState) -> &'static str {
    match state {
        FleetMemberState::Running => "running",
        FleetMemberState::WaitingForTool => "waiting for tool",
        FleetMemberState::WaitingForApproval => "waiting for approval",
        FleetMemberState::WaitingForUser => "waiting for user",
        FleetMemberState::Idle => "idle",
        FleetMemberState::Suspended => "suspended",
        FleetMemberState::Closed => "closed",
        FleetMemberState::Failed => "failed",
    }
}

pub(super) fn fleet_state_rank(state: FleetMemberState) -> u8 {
    match state {
        FleetMemberState::WaitingForApproval | FleetMemberState::WaitingForUser => 0,
        FleetMemberState::Running | FleetMemberState::WaitingForTool => 1,
        FleetMemberState::Idle => 2,
        FleetMemberState::Suspended => 3,
        FleetMemberState::Failed => 4,
        FleetMemberState::Closed => 5,
    }
}

pub(super) fn fleet_state_display(state: FleetMemberState) -> (&'static str, Span<'static>) {
    let label = fleet_state_label(state);
    let dot = match state {
        FleetMemberState::Running | FleetMemberState::WaitingForTool => "●".green(),
        FleetMemberState::WaitingForApproval | FleetMemberState::WaitingForUser => "●".cyan(),
        FleetMemberState::Idle => "○".cyan(),
        FleetMemberState::Suspended => "Ⅱ".magenta(),
        FleetMemberState::Closed => "✓".dim(),
        FleetMemberState::Failed => "×".red(),
    };
    (label, dot)
}
