use super::*;
use crate::PlanOrigin;
use crate::SavePlanRequest;
use crate::list_plans;
use crate::plans_dir;
use crate::read_plan;
use crate::save_plan_at;
use chrono::Local;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn codex_home(dir: &TempDir) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(dir.path()).expect("temp dir is absolute")
}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 31, hour, 0, 0)
        .single()
        .expect("unambiguous UTC time")
}

fn draft(dir: &TempDir, thread_id: ThreadId, markdown: &str) -> SavePlanRequest {
    SavePlanRequest {
        codex_home: codex_home(dir),
        thread_id,
        turn_id: "turn-1".to_string(),
        cwd: None,
        model: Some("gpt-test".to_string()),
        markdown: markdown.to_string(),
    }
}

fn approval(
    dir: &TempDir,
    id: &str,
    revision: u32,
    approved_at: DateTime<Utc>,
) -> ApprovePlanRequest {
    ApprovePlanRequest {
        codex_home: codex_home(dir),
        id: id.to_string(),
        expected_revision: revision,
        origin: PlanOrigin {
            item_id: Some("item-1".to_string()),
            rollout_id: Some("rollout-1".to_string()),
            build_revision: Some("build-1".to_string()),
            config_revision: Some("config-1".to_string()),
            ..Default::default()
        },
        approved_at: Some(approved_at),
    }
}

#[tokio::test]
async fn concurrent_saves_serialize_revisions_without_losing_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();
    let first = draft(&dir, thread_id, "# First\n");
    let second = draft(&dir, thread_id, "# Second\n");
    let (left, right) = tokio::join!(
        save_plan_at(first, Local::now()),
        save_plan_at(second, Local::now())
    );
    let left = left.expect("first save");
    let right = right.expect("second save");
    assert_eq!(
        [left.revision, right.revision]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [1, 2].into_iter().collect()
    );
    let listed = list_plans(&codex_home(&dir)).await.expect("list drafts");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].revision, 2);
}

#[tokio::test]
async fn fork_invariant_approved_plan_draft_uses_revision_cas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();
    let saved = save_plan_at(draft(&dir, thread_id, "# One\n"), Local::now())
        .await
        .expect("save revision one");
    save_plan_at(draft(&dir, thread_id, "# Two\n"), Local::now())
        .await
        .expect("save revision two");

    let error = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect_err("stale approval must fail");
    assert!(matches!(
        error,
        PlanApprovalError::StaleDraft {
            expected: 1,
            actual: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn equivalent_approval_is_idempotent_and_keeps_the_snapshot_immutable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let saved = save_plan_at(
        draft(&dir, ThreadId::new(), "# Plan\n- one\n"),
        Local::now(),
    )
    .await
    .expect("save draft");
    let first = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect("approve draft");
    let bytes_before = std::fs::read(&first.summary.path).expect("read approved snapshot");
    let second = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect("repeat approval");
    assert!(first.written);
    assert!(!second.written);
    assert_eq!(first.summary, second.summary);
    assert_eq!(first.markdown, second.markdown);
    assert_eq!(
        bytes_before,
        std::fs::read(&first.summary.path).expect("read unchanged snapshot")
    );
}

#[tokio::test]
async fn divergent_existing_snapshot_is_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let saved = save_plan_at(
        draft(&dir, ThreadId::new(), "# Plan\n- one\n"),
        Local::now(),
    )
    .await
    .expect("save draft");
    let approved = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect("approve draft");
    std::fs::write(&approved.summary.path, b"tampered").expect("tamper approved snapshot");

    let error = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect_err("divergent snapshot must fail");
    assert!(matches!(error, PlanApprovalError::Conflict(_)));
}

#[tokio::test]
async fn later_approval_requires_a_new_draft_and_derives_superseded_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();
    let first_draft = save_plan_at(draft(&dir, thread_id, "# First\n"), Local::now())
        .await
        .expect("save first draft");
    let first = approve_plan(approval(&dir, &first_draft.id, 1, at(12)))
        .await
        .expect("approve first revision");
    assert_eq!(
        first
            .summary
            .path
            .parent()
            .and_then(|path| path.file_name())
            .and_then(|value| value.to_str()),
        Some(first_draft.id.as_str())
    );
    let first_bytes = std::fs::read(&first.summary.path).expect("read first snapshot");

    save_plan_at(draft(&dir, thread_id, "# Second\n"), Local::now())
        .await
        .expect("save second draft");
    let second = approve_plan(approval(&dir, &first_draft.id, 2, at(13)))
        .await
        .expect("approve second revision");
    assert_ne!(first.summary.path, second.summary.path);
    assert_eq!(
        first_bytes,
        std::fs::read(&first.summary.path).expect("read immutable first")
    );

    let listed = list_approved_plans(&codex_home(&dir))
        .await
        .expect("list approved snapshots");
    let old = listed
        .iter()
        .find(|summary| summary.revision == 1)
        .expect("old revision");
    let current = listed
        .iter()
        .find(|summary| summary.revision == 2)
        .expect("new revision");
    assert_eq!(old.superseded_by, Some(2));
    assert_eq!(current.superseded_by, None);
}

#[tokio::test]
async fn partial_and_legacy_files_are_safe_to_read_and_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = codex_home(&dir);
    let plans = plans_dir(dir.path());
    std::fs::create_dir_all(&plans).expect("create plans directory");
    std::fs::write(plans.join("partial.md"), "---\ntitle: partial\n").expect("write partial");
    std::fs::write(
        plans.join("legacy.md"),
        "---\ntitle: Legacy\nthread_id: old-thread\nturn_id: old-turn\ncreated_at: 2026-08-31T10:00:00Z\nupdated_at: 2026-08-31T10:00:00Z\n---\n\nlegacy body\n",
    )
    .expect("write legacy plan");

    let listed = list_plans(&home).await.expect("list plans");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "legacy.md");
    assert_eq!(listed[0].revision, 1);
    let read = read_plan(&home, "legacy.md")
        .await
        .expect("read legacy plan")
        .expect("legacy plan exists");
    assert_eq!(read.markdown, "legacy body\n");
}

#[tokio::test]
async fn metadata_bounds_and_approved_token_budget_are_enforced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut oversized = draft(&dir, ThreadId::new(), "# Plan\n");
    oversized.turn_id = "t".repeat(257);
    assert_eq!(
        save_plan_at(oversized, Local::now())
            .await
            .expect_err("oversized metadata must fail")
            .kind(),
        std::io::ErrorKind::InvalidInput
    );

    let saved = save_plan_at(
        draft(
            &dir,
            ThreadId::new(),
            &format!("# Huge\n{}", "x".repeat(40_001)),
        ),
        Local::now(),
    )
    .await
    .expect("save large draft");
    let error = approve_plan(approval(&dir, &saved.id, 1, at(12)))
        .await
        .expect_err("large approved fragment must fail");
    assert!(matches!(error, PlanApprovalError::TooLarge { .. }));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_draft_is_not_followed_outside_the_plans_directory() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let outside_file = outside.path().join("outside.md");
    std::fs::write(&outside_file, "---\ntitle: Outside\ncreated_at: 2026-08-31T10:00:00Z\nupdated_at: 2026-08-31T10:00:00Z\n---\n\nsecret\n")
        .expect("write outside file");
    let plans = plans_dir(dir.path());
    std::fs::create_dir_all(&plans).expect("create plans directory");
    symlink(&outside_file, plans.join("escape.md")).expect("create symlink");

    assert!(
        read_plan(&codex_home(&dir), "escape.md")
            .await
            .expect("read symlink")
            .is_none()
    );
    assert!(
        list_plans(&codex_home(&dir))
            .await
            .expect("list symlink")
            .is_empty()
    );
}
