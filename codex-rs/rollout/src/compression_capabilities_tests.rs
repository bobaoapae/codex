use super::RolloutCompressionCapabilities;

#[test]
fn default_capabilities_fail_closed_and_explain_every_reader() {
    let capabilities = RolloutCompressionCapabilities::default();

    assert!(!capabilities.all_readers_support_shared());
    assert_eq!(
        capabilities.missing_shared_readers(),
        vec!["Cargo", "Bazel", "TUI", "app-server", "Desktop"]
    );
    assert!(
        capabilities
            .shared_compression_diagnostic()
            .contains("Desktop")
    );
}

#[test]
fn shared_capability_requires_an_explicit_desktop_confirmation() {
    let mut capabilities = RolloutCompressionCapabilities::all_readers();
    assert!(capabilities.all_readers_support_shared());

    capabilities.desktop = None;
    assert!(!capabilities.all_readers_support_shared());
    assert_eq!(capabilities.missing_shared_readers(), vec!["Desktop"]);

    capabilities.desktop = Some(false);
    assert!(!capabilities.all_readers_support_shared());
    assert_eq!(capabilities.missing_shared_readers(), vec!["Desktop"]);
}
