use super::ResponsesStreamRequest;
use super::log_retry;
use crate::session::tests::make_session_and_context;
use codex_protocol::error::CodexErr;
use codex_protocol::error::UnexpectedResponseError;
use http::StatusCode;
use std::time::Duration;
use tracing_test::internal::MockWriter;

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}

/// FORK: the sampling loop gates on `is_retryable` before it reaches
/// `handle_retryable_response_stream_error`, so a terminal 4xx costs zero
/// websocket attempts and never triggers the HTTPS transport fallback. A 404
/// used to burn five attempts on each transport before the turn died anyway.
#[test]
fn sampling_treats_404_as_terminal_and_5xx_as_retryable() {
    fn unexpected(status: StatusCode) -> CodexErr {
        CodexErr::UnexpectedStatus(UnexpectedResponseError {
            status,
            body: String::new(),
            user_message: None,
            url: None,
            cf_ray: None,
            request_id: None,
            identity_authorization_error: None,
            identity_error_code: None,
        })
    }

    assert!(!unexpected(StatusCode::NOT_FOUND).is_retryable());
    assert!(unexpected(StatusCode::SERVICE_UNAVAILABLE).is_retryable());
}
