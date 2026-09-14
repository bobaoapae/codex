use std::io::Error as IoError;

use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Owns the tasks that must be stopped after the processor exits.
///
/// Graceful shutdown drains them in their existing order. A processor panic or
/// cancellation takes the abort path instead, because transport cleanup can be
/// blocked by an open stdio reader.
pub(crate) struct RuntimeTaskHandles {
    pub(crate) outbound_handle: JoinHandle<()>,
    pub(crate) otel_reloader_handle: JoinHandle<()>,
    pub(crate) transport_accept_handles: Vec<JoinHandle<()>>,
}

impl RuntimeTaskHandles {
    pub(crate) async fn finish_gracefully(self, shutdown_token: &CancellationToken) {
        let Self {
            outbound_handle,
            otel_reloader_handle,
            transport_accept_handles,
        } = self;
        let _ = outbound_handle.await;
        shutdown_token.cancel();
        let _ = otel_reloader_handle.await;
        for handle in transport_accept_handles {
            let _ = handle.await;
        }
    }

    pub(crate) fn abort_after_processor_failure(
        self,
        shutdown_token: &CancellationToken,
        join_error: JoinError,
    ) -> IoError {
        shutdown_token.cancel();
        self.outbound_handle.abort();
        self.otel_reloader_handle.abort();
        for handle in &self.transport_accept_handles {
            handle.abort();
        }
        IoError::other(join_error)
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
