use super::super::RolloutMigrationPreviewWatermark;
use super::apply_support::workflow_watermark;
use codex_protocol::ThreadId;

#[test]
fn apply_watermark_preserves_preview_rollout_identity() {
    let rollout_id = ThreadId::default();
    let preview = RolloutMigrationPreviewWatermark {
        created_at: "2025-01-03T12-00-00".to_string(),
        rollout_id: Some(rollout_id),
    };
    let watermark = workflow_watermark(Some(&preview)).expect("valid preview watermark");
    assert_eq!(watermark.rollout_id, rollout_id.to_string());
    assert!(watermark.created_at_ms > 0);
}

#[test]
fn apply_watermark_rejects_missing_identity() {
    let preview = RolloutMigrationPreviewWatermark {
        created_at: "2025-01-03T12-00-00".to_string(),
        rollout_id: None,
    };
    assert!(workflow_watermark(Some(&preview)).is_none());
}
