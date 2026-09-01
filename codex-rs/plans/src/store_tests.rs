use super::*;
use chrono::TimeZone;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn codex_home(dir: &TempDir) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(dir.path()).expect("temp dir is absolute")
}

fn at(hour: u32, minute: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2026, 8, 27, hour, minute, 0)
        .single()
        .expect("unambiguous local time")
}

fn request(dir: &TempDir, thread_id: ThreadId, markdown: &str) -> SavePlanRequest {
    SavePlanRequest {
        codex_home: codex_home(dir),
        thread_id,
        turn_id: "turn-1".to_string(),
        cwd: None,
        model: Some("gpt-5.2".to_string()),
        markdown: markdown.to_string(),
    }
}

#[tokio::test]
async fn saving_a_plan_creates_revision_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();

    let saved = save_plan_at(
        request(&dir, thread_id, "# Final plan\n- first\n"),
        at(10, 0),
    )
    .await
    .expect("save plan");

    assert!(saved.written);
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.id, "2026-08-27T10-00-00-final-plan.md");
    assert!(saved.path.exists());

    let listed = list_plans(&codex_home(&dir)).await.expect("list plans");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Final plan");
    assert_eq!(
        listed[0].thread_id.as_deref(),
        Some(&*thread_id.to_string())
    );
    assert_eq!(listed[0].model.as_deref(), Some("gpt-5.2"));

    let read = read_plan(&codex_home(&dir), &saved.id)
        .await
        .expect("read plan")
        .expect("plan should exist");
    assert_eq!(read.markdown, "# Final plan\n- first\n");
}

#[tokio::test]
async fn revising_the_same_thread_rewrites_the_same_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();

    let first = save_plan_at(
        request(&dir, thread_id, "# Final plan\n- first\n"),
        at(10, 0),
    )
    .await
    .expect("save plan");
    let second = save_plan_at(
        request(&dir, thread_id, "# Final plan\n- first\n- second\n"),
        at(11, 30),
    )
    .await
    .expect("save revision");

    assert_eq!(second.path, first.path);
    assert_eq!(second.revision, 2);
    assert!(second.written);

    let listed = list_plans(&codex_home(&dir)).await.expect("list plans");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].revision, 2);
    assert_eq!(
        listed[0].created_at,
        at(10, 0).with_timezone(&Utc),
        "created_at is preserved across revisions"
    );
    assert_eq!(listed[0].updated_at, at(11, 30).with_timezone(&Utc));
}

#[tokio::test]
async fn identical_body_is_not_rewritten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread_id = ThreadId::new();

    let first = save_plan_at(request(&dir, thread_id, "# Plan\n- a\n"), at(10, 0))
        .await
        .expect("save plan");
    let second = save_plan_at(request(&dir, thread_id, "# Plan\n- a\n\n"), at(12, 0))
        .await
        .expect("save identical plan");

    assert!(!second.written);
    assert_eq!(second.revision, 1);
    assert_eq!(second.path, first.path);

    let listed = list_plans(&codex_home(&dir)).await.expect("list plans");
    assert_eq!(listed[0].updated_at, at(10, 0).with_timezone(&Utc));
}

#[tokio::test]
async fn different_threads_get_different_files() {
    let dir = tempfile::tempdir().expect("tempdir");

    let first = save_plan_at(request(&dir, ThreadId::new(), "# A\n"), at(10, 0))
        .await
        .expect("save first");
    let second = save_plan_at(request(&dir, ThreadId::new(), "# B\n"), at(11, 0))
        .await
        .expect("save second");

    assert_ne!(first.path, second.path);
    let listed = list_plans(&codex_home(&dir)).await.expect("list plans");
    assert_eq!(listed.len(), 2);
    // Newest first.
    assert_eq!(listed[0].title, "B");
    assert_eq!(listed[1].title, "A");
}

#[tokio::test]
async fn file_name_collisions_get_a_numeric_suffix() {
    let dir = tempfile::tempdir().expect("tempdir");

    let first = save_plan_at(request(&dir, ThreadId::new(), "# Same\n"), at(10, 0))
        .await
        .expect("save first");
    let second = save_plan_at(request(&dir, ThreadId::new(), "# Same\n"), at(10, 0))
        .await
        .expect("save second");

    assert_eq!(first.id, "2026-08-27T10-00-00-same.md");
    assert_eq!(second.id, "2026-08-27T10-00-00-same-2.md");
}

#[tokio::test]
async fn listing_ignores_non_markdown_and_invalid_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    save_plan_at(request(&dir, ThreadId::new(), "# Kept\n"), at(10, 0))
        .await
        .expect("save plan");

    let plans = plans_dir(dir.path());
    tokio::fs::write(plans.join("notes.txt"), "ignored")
        .await
        .expect("write txt");
    tokio::fs::write(plans.join("broken.md"), "# no front matter\n")
        .await
        .expect("write broken md");

    let listed = list_plans(&codex_home(&dir)).await.expect("list plans");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "Kept");
}

#[tokio::test]
async fn reading_an_unknown_or_unsafe_id_returns_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    save_plan_at(request(&dir, ThreadId::new(), "# Kept\n"), at(10, 0))
        .await
        .expect("save plan");
    let home = codex_home(&dir);

    for id in ["", "..", "a/b", "a\\b", "missing.md"] {
        assert_eq!(
            read_plan(&home, id).await.expect("read plan"),
            None,
            "id {id:?} must not resolve"
        );
    }
}

#[test]
fn plan_ids_reject_path_separators_and_dot_entries() {
    assert!(is_valid_plan_id("2026-08-27T10-00-00-plan.md"));
    assert!(!is_valid_plan_id(""));
    assert!(!is_valid_plan_id("."));
    assert!(!is_valid_plan_id(".."));
    assert!(!is_valid_plan_id("a/b"));
    assert!(!is_valid_plan_id("a\\b"));
    assert!(!is_valid_plan_id("a:b"));
    assert!(!is_valid_plan_id("a."));
}
