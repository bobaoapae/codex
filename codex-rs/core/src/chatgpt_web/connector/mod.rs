//! FORK: connector mode (`[chatgpt_web] tools = "connector"`).
//!
//! ChatGPT calls the Codex tools natively through a custom MCP connector that
//! points at the shared `codex chatgpt-web daemon`. This module has two halves:
//!
//! - the session side (this file + `client.rs` + `connector_attach.rs`): the
//!   `ConnectorBroker` seam the provider's turn loop talks to, the loopback
//!   client of the daemon's control API, and the browser-side @mention;
//! - the daemon side (`daemon/`): single shared instance owning the tunnel, the
//!   public MCP server with the fixed contract (`contract.rs`), the turn broker
//!   and the connector registry.

pub mod contract;
pub mod daemon;

/// The session-side half of the connector mode.
///
/// Filled in by M6: `begin_turn` / `prompt_contract` / `end_turn`. Until then
/// the trait only marks the object the turn loop will attach.
pub(crate) trait ConnectorBroker: Send + Sync + std::fmt::Debug {}
