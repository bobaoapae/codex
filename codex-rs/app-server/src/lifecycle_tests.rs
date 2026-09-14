use super::RuntimeTaskHandles;
use pretty_assertions::assert_eq;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn processor_failure_aborts_pending_transport_without_waiting() {
    let shutdown_token = CancellationToken::new();
    let reader_dropped = Arc::new(AtomicBool::new(false));
    let outbound_dropped = Arc::new(AtomicBool::new(false));
    let otel_dropped = Arc::new(AtomicBool::new(false));
    let transport_accept_dropped = Arc::new(AtomicBool::new(false));

    let reader_handle = pending_task(Arc::clone(&reader_dropped));
    let outbound_handle = pending_task(Arc::clone(&outbound_dropped));
    let otel_reloader_handle = pending_task(Arc::clone(&otel_dropped));
    let transport_accept_handle = pending_task(Arc::clone(&transport_accept_dropped));

    let join_error = tokio::spawn(async {
        panic!("processor panic marker");
    })
    .await
    .expect_err("processor panic should produce a join error");

    let tasks = RuntimeTaskHandles {
        outbound_handle,
        otel_reloader_handle,
        transport_accept_handles: vec![reader_handle, transport_accept_handle],
    };
    let error = tasks.abort_after_processor_failure(&shutdown_token, join_error);

    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(error.to_string().contains("processor panic marker"));
    assert!(
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<JoinError>())
            .is_some_and(|join_error| join_error.is_panic())
    );
    assert!(shutdown_token.is_cancelled());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if reader_dropped.load(Ordering::Acquire)
                && outbound_dropped.load(Ordering::Acquire)
                && otel_dropped.load(Ordering::Acquire)
                && transport_accept_dropped.load(Ordering::Acquire)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted tasks should be dropped");
}

fn pending_task(dropped: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    let guard = DropFlag(dropped);
    tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    })
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
