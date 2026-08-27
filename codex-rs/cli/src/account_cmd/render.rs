//! FORK: table/JSON rendering for `codex account`.
//!
//! The TUI has richer rate-limit formatting helpers, but they are
//! `pub(crate)` there; the small subset needed for one-line summaries is
//! reimplemented here instead of widening the TUI's public surface.

use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;

use super::usage::UsageState;
use super::usage::preferred_snapshot;

pub(crate) struct AccountRow {
    pub label: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// `None` for the synthetic "active but not stored" row.
    pub session_id: Option<String>,
    pub is_active: bool,
    pub usage: UsageState,
}

pub(crate) fn render_table(rows: &[AccountRow]) -> String {
    let mut cells: Vec<[String; 5]> = vec![[
        String::new(),
        "LABEL".to_string(),
        "EMAIL".to_string(),
        "PLAN".to_string(),
        "USAGE".to_string(),
    ]];
    for row in rows {
        cells.push([
            if row.is_active { "●" } else { " " }.to_string(),
            row.label.clone(),
            row.email.clone().unwrap_or_else(|| "-".to_string()),
            row.plan.clone().unwrap_or_else(|| "-".to_string()),
            usage_summary(&row.usage),
        ]);
    }

    let mut widths = [0usize; 5];
    for row in &cells {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for row in &cells {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if index < row.len() - 1 {
                line.extend(std::iter::repeat_n(
                    ' ',
                    widths[index].saturating_sub(cell.chars().count()) + 2,
                ));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub(crate) fn rows_to_json(rows: &[AccountRow]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|row| {
                let usage = match &row.usage {
                    UsageState::Loaded(snapshots) => serde_json::json!({
                        "status": "loaded",
                        "rate_limits": snapshots,
                    }),
                    UsageState::Failed(reason) => serde_json::json!({
                        "status": "failed",
                        "reason": reason,
                    }),
                    UsageState::ReauthNeeded => serde_json::json!({ "status": "reauth_needed" }),
                    UsageState::NotApplicable => serde_json::json!({ "status": "not_applicable" }),
                };
                serde_json::json!({
                    "session_id": row.session_id,
                    "label": row.label,
                    "email": row.email,
                    "plan": row.plan,
                    "active": row.is_active,
                    "usage": usage,
                })
            })
            .collect(),
    )
}

pub(crate) fn usage_summary(state: &UsageState) -> String {
    match state {
        UsageState::Loaded(snapshots) => match preferred_snapshot(snapshots) {
            Some(snapshot) => snapshot_summary(snapshot),
            None => "no usage data".to_string(),
        },
        UsageState::Failed(reason) => format!("no data ({reason})"),
        UsageState::ReauthNeeded => "reauth needed (run `codex account add`)".to_string(),
        UsageState::NotApplicable => "-".to_string(),
    }
}

fn snapshot_summary(snapshot: &RateLimitSnapshot) -> String {
    let mut parts = Vec::new();
    if let Some(primary) = &snapshot.primary {
        parts.push(window_summary(primary, /* is_secondary */ false));
    }
    if let Some(secondary) = &snapshot.secondary {
        parts.push(window_summary(secondary, /* is_secondary */ true));
    }
    if parts.is_empty() {
        if let Some(credits) = &snapshot.credits {
            if credits.unlimited {
                return "credits: unlimited".to_string();
            }
            if let Some(balance) = &credits.balance {
                return format!("credits: {balance}");
            }
        }
        return "no usage data".to_string();
    }
    parts.join(" | ")
}

fn window_summary(window: &RateLimitWindow, is_secondary: bool) -> String {
    let label = window_label(window.window_minutes, is_secondary);
    let left = (100.0 - window.used_percent).clamp(0.0, 100.0).round() as i64;
    match window
        .resets_at
        .and_then(|resets_at| DateTime::<Utc>::from_timestamp(resets_at, 0))
    {
        Some(resets_at) => format!(
            "{label}: {left}% left (resets {})",
            format_reset(resets_at.with_timezone(&Local))
        ),
        None => format!("{label}: {left}% left"),
    }
}

/// Same ±5% window classification the TUI uses for "5h"/"weekly" labels.
fn window_label(window_minutes: Option<i64>, is_secondary: bool) -> String {
    const KNOWN: [(i64, &str); 5] = [
        (300, "5h"),
        (1_440, "daily"),
        (10_080, "weekly"),
        (43_200, "monthly"),
        (525_600, "annual"),
    ];
    if let Some(minutes) = window_minutes {
        for (target, label) in KNOWN {
            let tolerance = target / 20;
            if (minutes - target).abs() <= tolerance {
                return (*label).to_string();
            }
        }
        if minutes % 60 == 0 {
            return format!("{}h", minutes / 60);
        }
        return format!("{minutes}m");
    }
    if is_secondary { "secondary" } else { "usage" }.to_string()
}

fn format_reset(resets_at: DateTime<Local>) -> String {
    let now = Local::now();
    if resets_at.date_naive() == now.date_naive() {
        resets_at.format("%H:%M").to_string()
    } else {
        resets_at.format("%d %b %H:%M").to_string()
    }
}

/// FORK: one line per configured Claude account.
///
/// The columns are the ones that decide what to do next: how much of each
/// window is spent, whether the account is on a failure cooldown, and which one
/// new work will pick.
pub(crate) fn render_claude_table(
    accounts: &[codex_core::claude_accounts_api::ClaudeAccountStatus],
) -> String {
    fn window(used_pct: Option<f64>) -> String {
        match used_pct {
            Some(used) => format!("{used:.0}% used"),
            // Not "0%": an account whose usage was never fetched must not be
            // drawn as a healthy one.
            None => "unknown".to_string(),
        }
    }

    let mut out = String::new();
    for account in accounts {
        let marker = if account.preferred { "*" } else { " " };
        out.push_str(&format!(
            "{marker} {}. {}\n",
            account.index, account.account
        ));
        if !account.logged_in {
            out.push_str("      not logged in (no credentials in its config dir)\n");
            continue;
        }
        out.push_str(&format!(
            "      5h: {}   7d: {}\n",
            window(account.five_hour_used_pct),
            window(account.weekly_used_pct)
        ));
        if account.running_turns > 0 {
            out.push_str(&format!(
                "      {} turn(s) running right now\n",
                account.running_turns
            ));
        }
        if let Some(seconds) = account.cooldown_seconds_left {
            let reason = account.cooldown_reason.as_deref().unwrap_or("failure");
            out.push_str(&format!("      cooling down for {seconds}s ({reason})\n"));
        }
        if let Some(hint) = account.limit_reset_hint.as_deref() {
            out.push_str(&format!("      {hint}\n"));
        }
    }
    out
}
