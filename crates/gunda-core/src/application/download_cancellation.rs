use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::watch;

/// Requested cooperative stop for one execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadControlRequest {
    Pause,
    Cancel,
}

/// Shares pause and cancellation requests with one execution attempt.
#[derive(Clone)]
pub struct DownloadCancellation {
    requested: watch::Sender<Option<DownloadControlRequest>>,
    pause_supported: Arc<AtomicBool>,
}

impl DownloadCancellation {
    #[must_use]
    pub fn new() -> Self {
        let (requested, _) = watch::channel(None);
        Self {
            requested,
            pause_supported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Requests cancellation, overriding a pending pause request.
    pub fn request(&self) {
        self.requested
            .send_replace(Some(DownloadControlRequest::Cancel));
    }

    /// Requests a pause unless cancellation was already requested.
    pub fn request_pause(&self) -> bool {
        if !self.pause_supported() {
            return false;
        }
        self.requested.send_if_modified(|requested| {
            if requested.is_none() {
                *requested = Some(DownloadControlRequest::Pause);
                true
            } else {
                false
            }
        })
    }

    pub fn set_pause_supported(&self, supported: bool) {
        self.pause_supported.store(supported, Ordering::Release);
    }

    #[must_use]
    pub fn pause_supported(&self) -> bool {
        self.pause_supported.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.request_kind().is_some()
    }

    #[must_use]
    pub fn request_kind(&self) -> Option<DownloadControlRequest> {
        *self.requested.borrow()
    }

    pub async fn stopped(&self) {
        let mut receiver = self.requested.subscribe();

        let _ = receiver.wait_for(Option::is_some).await;
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.requested.subscribe();

        let _ = receiver
            .wait_for(|requested| *requested == Some(DownloadControlRequest::Cancel))
            .await;
    }
}

impl Default for DownloadCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{DownloadCancellation, DownloadControlRequest};

    #[tokio::test]
    async fn requests_are_shared_and_remain_visible_to_late_waiters() {
        let cancellation = DownloadCancellation::new();
        let handle = cancellation.clone();

        handle.request();
        handle.request();

        assert!(cancellation.is_requested());

        cancellation.cancelled().await;
        cancellation.cancelled().await;
    }

    #[tokio::test]
    async fn cancellation_overrides_pause_but_pause_does_not_override_cancellation() {
        let control = DownloadCancellation::new();
        assert!(!control.request_pause());
        control.set_pause_supported(true);
        assert!(control.request_pause());
        assert!(!control.request_pause());
        assert_eq!(control.request_kind(), Some(DownloadControlRequest::Pause));
        control.stopped().await;

        control.request();
        assert!(!control.request_pause());
        assert_eq!(control.request_kind(), Some(DownloadControlRequest::Cancel));
        control.cancelled().await;
    }
}
