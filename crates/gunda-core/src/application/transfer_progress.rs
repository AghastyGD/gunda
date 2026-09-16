use tokio::sync::watch;

/// Publishes the latest cumulative written-byte count.
pub struct TransferProgress {
    sender: Option<watch::Sender<u64>>,
}

impl TransferProgress {
    #[must_use]
    pub fn channel() -> (Self, watch::Receiver<u64>) {
        let (sender, receiver) = watch::channel(0);

        (
            Self {
                sender: Some(sender),
            },
            receiver,
        )
    }

    #[must_use]
    pub const fn disabled() -> Self {
        Self { sender: None }
    }

    /// Reports cumulative bytes after pending writes have been flushed.
    pub fn report_written(&self, written_bytes: u64) {
        if let Some(sender) = &self.sender {
            sender.send_replace(written_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TransferProgress;

    #[test]
    fn channel_retains_only_the_latest_count() {
        let (progress, receiver) = TransferProgress::channel();

        progress.report_written(10);
        progress.report_written(20);
        progress.report_written(30);

        assert_eq!(*receiver.borrow(), 30);
    }

    #[test]
    fn reporting_without_a_receiver_is_allowed() {
        let (progress, receiver) = TransferProgress::channel();
        drop(receiver);

        progress.report_written(10);
        TransferProgress::disabled().report_written(10);
    }
}
