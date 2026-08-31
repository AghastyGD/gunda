use std::error::Error;
use std::fmt;

/// Durable execution state of a download job
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    /// Persisted and waiting to be inspected or scheduled.
    Queued,

    /// The resource is being examined and an engine is being selected.
    Inspecting,

    /// Transfer work is currently active.
    Downloading,

    /// Intentionally suspended by the user.
    Paused,

    /// Transfer is complete, but output has not been committed.
    Finalizing,

    /// Final output was committed successfully.
    Completed,

    /// Execution stopped because of an error.
    Failed,

    /// Intentionally cancelled by the user.
    Cancelled,

    /// Active execution lost its owning process.
    Interrupted,
}

impl DownloadState {
    /// Returns whether a transition from this state to `next` is allowed.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Queued,
                Self::Inspecting | Self::Paused | Self::Cancelled
            ) | (
                Self::Inspecting,
                Self::Downloading
                    | Self::Paused
                    | Self::Failed
                    | Self::Interrupted
                    | Self::Cancelled
            ) | (
                Self::Downloading,
                Self::Paused
                    | Self::Finalizing
                    | Self::Failed
                    | Self::Interrupted
                    | Self::Cancelled
            ) | (Self::Paused, Self::Queued | Self::Cancelled)
                | (
                    Self::Finalizing,
                    Self::Completed | Self::Failed | Self::Interrupted
                )
                | (
                    Self::Failed | Self::Interrupted,
                    Self::Queued | Self::Cancelled
                )
        )
    }

    /// Validates a requested state transition.
    ///
    /// This method does not mutate any state. The application must persist
    /// the new state before publishing it to clients.
    pub fn ensure_can_transition_to(self, next: Self) -> Result<(), InvalidStateTransition> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(InvalidStateTransition::new(self, next))
        }
    }

    /// Returns whether no further execution transition is allowed
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /// Returns whether this state represents work owned by the current process.
    ///
    /// Jobs left in one of these states must be reconciled after a restart.
    #[must_use]
    pub const fn requires_recovery_after_restart(self) -> bool {
        matches!(
            self,
            Self::Inspecting | Self::Downloading | Self::Finalizing
        )
    }
}

/// Error returned when a lifecycle transition is not allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStateTransition {
    from: DownloadState,
    to: DownloadState,
}

impl InvalidStateTransition {
    #[must_use]
    pub const fn new(from: DownloadState, to: DownloadState) -> Self {
        Self { from, to }
    }

    #[must_use]
    pub const fn from(&self) -> DownloadState {
        self.from
    }

    #[must_use]
    pub const fn to(&self) -> DownloadState {
        self.to
    }
}

impl fmt::Display for InvalidStateTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid download state transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl Error for InvalidStateTransition {}

#[cfg(test)]
mod tests {
    use super::{DownloadState, InvalidStateTransition};

    const ALL_STATES: [DownloadState; 9] = [
        DownloadState::Queued,
        DownloadState::Inspecting,
        DownloadState::Downloading,
        DownloadState::Paused,
        DownloadState::Finalizing,
        DownloadState::Completed,
        DownloadState::Failed,
        DownloadState::Cancelled,
        DownloadState::Interrupted,
    ];

    const VALID_TRANSITIONS: &[(DownloadState, DownloadState)] = &[
        (DownloadState::Queued, DownloadState::Inspecting),
        (DownloadState::Queued, DownloadState::Paused),
        (DownloadState::Queued, DownloadState::Cancelled),
        (DownloadState::Inspecting, DownloadState::Downloading),
        (DownloadState::Inspecting, DownloadState::Paused),
        (DownloadState::Inspecting, DownloadState::Failed),
        (DownloadState::Inspecting, DownloadState::Interrupted),
        (DownloadState::Inspecting, DownloadState::Cancelled),
        (DownloadState::Downloading, DownloadState::Paused),
        (DownloadState::Downloading, DownloadState::Finalizing),
        (DownloadState::Downloading, DownloadState::Failed),
        (DownloadState::Downloading, DownloadState::Interrupted),
        (DownloadState::Downloading, DownloadState::Cancelled),
        (DownloadState::Paused, DownloadState::Queued),
        (DownloadState::Paused, DownloadState::Cancelled),
        (DownloadState::Finalizing, DownloadState::Completed),
        (DownloadState::Finalizing, DownloadState::Failed),
        (DownloadState::Finalizing, DownloadState::Interrupted),
        (DownloadState::Failed, DownloadState::Queued),
        (DownloadState::Failed, DownloadState::Cancelled),
        (DownloadState::Interrupted, DownloadState::Queued),
        (DownloadState::Interrupted, DownloadState::Cancelled),
    ];

    #[test]
    fn transition_match_the_lifecycle_contract() {
        for from in ALL_STATES {
            for to in ALL_STATES {
                let expected = VALID_TRANSITIONS.contains(&(from, to));

                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "unexpected transition result for {from:?} -> {to:?}",
                )
            }
        }
    }

    #[test]
    fn valid_transition_passes_validation() {
        let result = DownloadState::Queued.ensure_can_transition_to(DownloadState::Inspecting);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn invalid_transition_returns_both_states() {
        let result = DownloadState::Queued.ensure_can_transition_to(DownloadState::Completed);

        let error = result.expect_err("Queued -> Completed must be rejected");

        assert_eq!(
            error,
            InvalidStateTransition::new(DownloadState::Queued, DownloadState::Completed,)
        );
        assert_eq!(error.from(), DownloadState::Queued);
        assert_eq!(error.to(), DownloadState::Completed);
        assert_eq!(
            error.to_string(),
            "invalid download state transition: Queued -> Completed"
        );
    }

    #[test]
    fn requested_state_is_not_a_transition() {
        for state in ALL_STATES {
            assert!(
                !state.can_transition_to(state),
                "{state:?} must not transition to itself",
            );
        }
    }

    #[test]
    fn only_completed_and_cancelled_are_terminal() {
        for state in ALL_STATES {
            let expected = matches!(state, DownloadState::Completed | DownloadState::Cancelled);

            assert_eq!(
                state.is_terminal(),
                expected,
                "unexpected terminal status for {state:?}",
            );
        }
    }

    #[test]
    fn active_process_states_require_restart_recovery() {
        for state in ALL_STATES {
            let expected = matches!(
                state,
                DownloadState::Inspecting | DownloadState::Downloading | DownloadState::Finalizing
            );

            assert_eq!(
                state.requires_recovery_after_restart(),
                expected,
                "unexpected recovery requirement for {state:?}",
            )
        }
    }
}
