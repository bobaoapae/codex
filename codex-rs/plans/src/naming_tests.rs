use super::*;
use chrono::TimeZone;
use pretty_assertions::assert_eq;

fn fixed_now() -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2026, 8, 27, 14, 5, 9)
        .single()
        .expect("unambiguous local time")
}

#[test]
fn title_prefers_the_first_heading() {
    assert_eq!(
        extract_title("intro\n\n## Ship the plan mode\n- step\n", fixed_now()),
        "Ship the plan mode"
    );
}

#[test]
fn title_falls_back_to_the_first_non_empty_line_then_to_a_date() {
    assert_eq!(
        extract_title("\n\n  Do the thing  \nmore\n", fixed_now()),
        "Do the thing"
    );
    assert_eq!(extract_title("   \n\n", fixed_now()), "Plan 2026-08-27");
}

#[test]
fn title_truncates_on_char_boundaries() {
    let long = format!("# {}", "á".repeat(200));
    let title = extract_title(&long, fixed_now());
    assert_eq!(title.chars().count(), 80);
    assert!(title.chars().all(|ch| ch == 'á'));
}

#[test]
fn slugify_joins_alphanumeric_runs() {
    assert_eq!(slugify("Ship the Plan mode!"), "ship-the-plan-mode");
    assert_eq!(slugify("  --- "), "plan");
    assert_eq!(slugify("çãé"), "plan");
    assert_eq!(slugify("a".repeat(100).as_str()).chars().count(), 48);
}

#[test]
fn file_stem_is_windows_safe_and_sortable() {
    let stem = file_stem_for(fixed_now(), "ship-the-plan-mode");
    assert_eq!(stem, "2026-08-27T14-05-09-ship-the-plan-mode");
    assert!(!stem.contains(':'));
}
