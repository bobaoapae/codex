use std::collections::HashMap;

use codex_app_server_protocol::JobRunParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::immutable_job_metadata;

#[test]
fn job_metadata_is_allowlisted_and_bounded_for_large_input() {
    let secret_prompt = "JOB_PROMPT_SECRET";
    let mut params = JobRunParams {
        input: vec![UserInput::Text {
            text: format!("{secret_prompt}{}", "x".repeat(70 * 1024)),
            text_elements: Vec::new(),
        }],
        config: Some(HashMap::from([(
            "secret.config".to_string(),
            json!("CONFIG_SECRET"),
        )])),
        ..Default::default()
    };

    let metadata = immutable_job_metadata(&params).expect("metadata should serialize");
    let metadata_json = serde_json::to_string(&metadata).expect("metadata JSON");
    assert!(metadata_json.len() < 64 * 1024);
    assert!(!metadata_json.contains(secret_prompt));
    assert!(!metadata_json.contains("secret.config"));
    assert!(!metadata_json.contains("CONFIG_SECRET"));
    assert_eq!(metadata["hasInput"], json!(true));
    assert_eq!(metadata["inputItemCount"], json!(1));
    assert_eq!(
        metadata["inputCharCount"],
        json!(70 * 1024 + secret_prompt.len())
    );
    assert_eq!(metadata["requestedThreadClass"], json!("transientJob"));
    assert_eq!(metadata["requestedSource"], json!("appServer.jobRun"));
    assert!(metadata["paramsDigest"].as_str().is_some_and(|value| {
        value.starts_with("sha256:") && value.len() == "sha256:".len() + 64
    }));

    let first_digest = metadata["paramsDigest"].clone();
    params.input[0] = UserInput::Text {
        text: format!("changed-{secret_prompt}{}", "x".repeat(70 * 1024)),
        text_elements: Vec::new(),
    };
    let changed_metadata = immutable_job_metadata(&params).expect("changed metadata");
    assert_ne!(first_digest, changed_metadata["paramsDigest"]);
}
