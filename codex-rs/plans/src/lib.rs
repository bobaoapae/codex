//! FORK: persistence for the plans produced by Plan mode.
//!
//! A plan approved in Plan mode used to live only in the session that produced it. This crate
//! writes one Markdown file per thread under `$CODEX_HOME/plans/`, with YAML front matter, so a
//! plan can be reloaded in a later session (TUI `/plans`, app-server `plan/list` + `plan/read`).

mod approval;
mod fragment;
mod front_matter;
mod locking;
mod naming;
mod store;

pub use approval::ApprovePlanError;
pub use approval::ApprovePlanRequest;
pub use approval::ApprovedPlan;
pub use approval::ApprovedPlanSummary;
pub use approval::PlanApprovalError;
pub use approval::approve_plan;
pub use approval::list_approved_plans;
pub use approval::read_approved_plan;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::path::PathBuf;

pub use fragment::MAX_APPROVED_PLAN_TOKENS;
pub use fragment::PlanFragmentError;
pub use fragment::render_plan_fragment;
pub use fragment::validate_plan_fragment;
pub use front_matter::PlanFrontMatter;
pub use front_matter::PlanOrigin;
pub use front_matter::parse_document;
pub use front_matter::render_document;
pub use naming::extract_title;
pub use naming::file_stem_for;
pub use naming::slugify;
pub use store::SavePlanMetadata;
pub use store::SavePlanRequest;
pub use store::SavedPlan;
pub use store::SavedPlanPath;
pub use store::SavedPlanSummary;
pub use store::is_valid_plan_id;
pub use store::list_plans;
pub use store::read_plan;
pub use store::save_plan;
pub use store::save_plan_at;
pub use store::save_plan_with_metadata;
pub use store::save_plan_with_metadata_at;

/// Directory that holds every saved plan for a `CODEX_HOME`.
pub fn plans_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("plans")
}

/// Convenience wrapper for the [`AbsolutePathBuf`] callers hold in practice.
pub fn plans_dir_for(codex_home: &AbsolutePathBuf) -> PathBuf {
    plans_dir(codex_home.as_path())
}
