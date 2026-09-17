use tokio::sync::watch;

/// Shares a cancellation request with one execution attempt.
#[derive(Clone)]
pub struct DownloadCancellation {
    requested: watch::Sender<bool>,
}

impl DownloadCancellation {
    #[must_use]
    pub fn new() -> Self {
        let (requested, _) = watch::channel(false);
        Self { requested }
    }

    pub fn request(&self) {
        self.requested.send_replace(true);
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.requested.subscribe();

        let _ = receiver.wait_for(|requested| *requested).await;
    }
}

impl Default for DownloadCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::DownloadCancellation;

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
}
