//! FORK: per-account usage fetch for `codex account`.
//!
//! Hits the same backend endpoint the TUI `/status` card uses
//! (`/api/codex/usage` via `codex-backend-client`), so no model turn is
//! needed. Fetches run concurrently with a per-account timeout.

use std::time::Duration;

use codex_backend_client::Client as BackendClient;
use codex_core::config::Config;
use codex_login::CodexAuth;
use codex_protocol::protocol::RateLimitSnapshot;
use tokio::task::JoinSet;

const USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How to obtain usage for one account row.
pub(crate) enum UsagePlan {
    Fetch(Box<CodexAuth>),
    /// The stored refresh token is permanently invalid; the account must be
    /// re-added.
    ReauthNeeded,
    /// Auth material could not be prepared (I/O or transient refresh error).
    Unavailable(String),
    /// Not a ChatGPT-backed account (e.g. plain API key).
    NotApplicable,
}

#[derive(Debug, Clone)]
pub(crate) enum UsageState {
    Loaded(Vec<RateLimitSnapshot>),
    Failed(String),
    ReauthNeeded,
    NotApplicable,
}

pub(crate) async fn fetch_usage(config: &Config, plans: Vec<UsagePlan>) -> Vec<UsageState> {
    let mut states: Vec<UsageState> = plans
        .iter()
        .map(|plan| match plan {
            UsagePlan::Fetch(_) => UsageState::Failed("not fetched".to_string()),
            UsagePlan::ReauthNeeded => UsageState::ReauthNeeded,
            UsagePlan::Unavailable(reason) => UsageState::Failed(reason.clone()),
            UsagePlan::NotApplicable => UsageState::NotApplicable,
        })
        .collect();

    let mut join_set = JoinSet::new();
    for (index, plan) in plans.into_iter().enumerate() {
        let UsagePlan::Fetch(auth) = plan else {
            continue;
        };
        let client = BackendClient::from_auth(
            config.chatgpt_base_url.clone(),
            &auth,
            config.http_client_factory(),
        );
        join_set.spawn(async move {
            let state = match tokio::time::timeout(
                USAGE_FETCH_TIMEOUT,
                client.get_rate_limits_many(),
            )
            .await
            {
                Err(_) => UsageState::Failed(format!(
                    "timed out after {}s",
                    USAGE_FETCH_TIMEOUT.as_secs()
                )),
                Ok(Err(err)) => UsageState::Failed(err.to_string()),
                Ok(Ok(snapshots)) => UsageState::Loaded(snapshots),
            };
            (index, state)
        });
    }

    while let Some(joined) = join_set.join_next().await {
        if let Ok((index, state)) = joined {
            states[index] = state;
        }
    }
    states
}

/// The snapshot worth displaying: the `codex` limit when present, else the
/// first one (mirrors `codex-backend-client`'s own preference).
pub(crate) fn preferred_snapshot(snapshots: &[RateLimitSnapshot]) -> Option<&RateLimitSnapshot> {
    snapshots
        .iter()
        .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
        .or_else(|| snapshots.first())
}
