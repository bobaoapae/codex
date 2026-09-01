use super::*;
use pretty_assertions::assert_eq;

fn sample() -> PlanFrontMatter {
    PlanFrontMatter {
        title: "Final plan".to_string(),
        thread_id: Some("019c2d47-4935-7423-a190-05691f566092".to_string()),
        turn_id: Some("turn-1".to_string()),
        item_id: Some("item-1".to_string()),
        rollout_id: Some("rollout-1".to_string()),
        cwd: Some("/home/user/project".to_string()),
        model: Some("gpt-5.2".to_string()),
        created_at: "2026-08-27T10:00:00Z".to_string(),
        updated_at: "2026-08-27T11:00:00Z".to_string(),
        revision: 2,
        approved_at: Some("2026-08-27T12:00:00Z".to_string()),
        build_revision: Some("build-1".to_string()),
        config_revision: Some("config-1".to_string()),
    }
}

#[test]
fn document_round_trips() {
    let body = "# Final plan\n- first\n- second\n";
    let document = render_document(&sample(), body);

    assert!(document.starts_with("---\n"));
    let (front_matter, parsed_body) = parse_document(&document).expect("document should parse");
    assert_eq!(front_matter, sample());
    assert_eq!(parsed_body, body);
}

#[test]
fn timestamps_parse_back_to_utc() {
    let front_matter = sample();
    assert_eq!(
        front_matter
            .created_at_utc()
            .expect("created_at should parse")
            .to_rfc3339_opts(SecondsFormat::Secs, /*use_z*/ true),
        "2026-08-27T10:00:00Z"
    );
    assert_eq!(
        front_matter
            .updated_at_utc()
            .expect("updated_at should parse")
            .to_rfc3339_opts(SecondsFormat::Secs, /*use_z*/ true),
        "2026-08-27T11:00:00Z"
    );
}

#[test]
fn documents_without_front_matter_are_rejected() {
    assert!(parse_document("# Plain markdown\n").is_none());
    assert!(parse_document("---\ntitle: only\n").is_none());
    assert!(parse_document("---\nnot: valid front matter\n---\nbody\n").is_none());
}

#[test]
fn crlf_documents_parse() {
    let document = "---\r\ntitle: Final plan\r\ncreated_at: 2026-08-27T10:00:00Z\r\nupdated_at: 2026-08-27T10:00:00Z\r\nrevision: 1\r\n---\r\n\r\nbody\r\n";
    let (front_matter, body) = parse_document(document).expect("crlf document should parse");
    assert_eq!(front_matter.title, "Final plan");
    assert_eq!(front_matter.revision, 1);
    assert_eq!(body, "body\r\n");
}

#[test]
fn revision_defaults_to_one_when_missing() {
    let document = "---\ntitle: T\ncreated_at: 2026-08-27T10:00:00Z\nupdated_at: 2026-08-27T10:00:00Z\n---\n\nbody\n";
    let (front_matter, _) = parse_document(document).expect("document should parse");
    assert_eq!(front_matter.revision, 1);
}
