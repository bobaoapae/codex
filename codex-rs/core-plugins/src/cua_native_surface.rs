//! FORK: teaches the bundled `unified-computer-use` plugin's `cua_repl` server
//! that the native computer surface is available on Windows.
//!
//! The Codex Desktop app writes the plugin's `.mcp.json` itself and, on
//! Windows, stamps `CUA_REPL_ENABLED_SURFACES="browser"` even though the very
//! same file advertises a live Computer Use kernel (`SKY_CUA_NATIVE_PIPE=1`
//! plus the pipe directory the app owns). `launch.mjs` only registers the
//! `sky` service when that list contains `computer`, so the direct `js` tool
//! the model reaches for has no `sky.*` at all and Computer Use reports itself
//! as "not configured" — while the same kernel keeps working through the
//! `node_repl` code-mode path.
//!
//! The app owns that file and rewrites it at every startup, so the fix lives
//! here: the loader re-reads `.mcp.json` on every load, and this pass
//! normalizes the surface list **in memory** on the way through. Nothing on
//! disk is touched.

use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;

/// The plugin MCP server this pass is about.
const CUA_REPL_SERVER: &str = "cua_repl";
/// Set to `"1"` by the Desktop when a Computer Use kernel is reachable.
const NATIVE_PIPE_ENV: &str = "SKY_CUA_NATIVE_PIPE";
/// Named pipe the kernel listens on.
const NATIVE_PIPE_DIRECTORY_ENV: &str = "SKY_CUA_NATIVE_PIPE_DIRECTORY";
/// Comma-separated surface list `launch.mjs` reads.
const ENABLED_SURFACES_ENV: &str = "CUA_REPL_ENABLED_SURFACES";
/// The surface that registers the `sky` service.
const COMPUTER_SURFACE: &str = "computer";

/// What [`enable_cua_native_surface`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CuaNativeSurfaceOutcome {
    /// The surface list gained `computer`.
    Applied { before: String, after: String },
    /// The list already advertised `computer`; nothing to do.
    AlreadyEnabled,
    /// A guard refused; the reason is stable enough to log.
    Skipped(&'static str),
}

/// Adds the `computer` surface to a Windows `cua_repl` server whose own
/// environment already says a native Computer Use kernel is listening.
///
/// `target_is_windows` is a parameter rather than `cfg!(windows)` so the guard
/// is testable on every host. `escape_hatch` is the user's
/// `native_computer_surface` policy: `Some(false)` turns the pass off.
///
/// Every guard must hold; anything unexpected leaves `config` byte-identical.
pub(crate) fn enable_cua_native_surface(
    server_name: &str,
    config: &mut McpServerConfig,
    target_is_windows: bool,
    escape_hatch: Option<bool>,
) -> CuaNativeSurfaceOutcome {
    if server_name != CUA_REPL_SERVER {
        return CuaNativeSurfaceOutcome::Skipped("not the cua_repl server");
    }
    if !target_is_windows {
        // Only Windows ships the list without `computer`; macOS already has it.
        return CuaNativeSurfaceOutcome::Skipped("not a Windows target");
    }
    if escape_hatch == Some(false) {
        return CuaNativeSurfaceOutcome::Skipped("disabled by native_computer_surface = false");
    }
    let McpServerTransportConfig::Stdio { env, .. } = &mut config.transport else {
        return CuaNativeSurfaceOutcome::Skipped("not a stdio transport");
    };
    let Some(env) = env.as_mut() else {
        return CuaNativeSurfaceOutcome::Skipped("stdio transport declares no env");
    };
    if env.get(NATIVE_PIPE_ENV).map(String::as_str) != Some("1") {
        return CuaNativeSurfaceOutcome::Skipped("no native Computer Use pipe advertised");
    }
    if env
        .get(NATIVE_PIPE_DIRECTORY_ENV)
        .is_none_or(|directory| directory.trim().is_empty())
    {
        return CuaNativeSurfaceOutcome::Skipped("native Computer Use pipe has no directory");
    }
    // An absent list is `launch.mjs`'s own default (`browser,computer`):
    // writing one would only be a way to get it wrong.
    let Some(surfaces) = env.get(ENABLED_SURFACES_ENV) else {
        return CuaNativeSurfaceOutcome::Skipped("surface list is not pinned");
    };
    if surfaces
        .split(',')
        .any(|surface| surface.trim() == COMPUTER_SURFACE)
    {
        return CuaNativeSurfaceOutcome::AlreadyEnabled;
    }
    let before = surfaces.clone();
    // Keep whatever the app listed; only append.
    let after = if before.trim().is_empty() {
        COMPUTER_SURFACE.to_string()
    } else {
        format!("{before},{COMPUTER_SURFACE}")
    };
    env.insert(ENABLED_SURFACES_ENV.to_string(), after.clone());
    CuaNativeSurfaceOutcome::Applied { before, after }
}

#[cfg(test)]
#[path = "cua_native_surface_tests.rs"]
mod tests;
