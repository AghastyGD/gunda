use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use gunda_core::application::{RepositoryError, RepositoryErrorKind};

/// Holds exclusive ownership of a Gunda runtime's data directory.
pub struct RuntimeLease {
    _file: File,
}

impl RuntimeLease {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| {
                RepositoryError::new(
                    RepositoryErrorKind::Unavailable,
                    "could not open the runtime lock file",
                )
            })?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(RepositoryError::new(
                RepositoryErrorKind::Unavailable,
                "another Gunda process is already using this data directory",
            )),
            Err(TryLockError::Error(_)) => Err(RepositoryError::new(
                RepositoryErrorKind::Unavailable,
                "could not acquire the runtime lock",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use gunda_core::application::RepositoryErrorKind;
    use tempfile::tempdir;

    use super::RuntimeLease;

    #[test]
    fn rejects_another_owner_while_the_lease_is_held() {
        let directory = tempdir().expect("temporary directory must exist");
        let path = directory.path().join("gunda.runtime.lock");

        let first = RuntimeLease::acquire(&path).expect("first lease must succeed");

        let error = match RuntimeLease::acquire(&path) {
            Ok(_) => panic!("another owner must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::Unavailable);
        assert_eq!(
            error.message(),
            "another Gunda process is already using this data directory",
        );

        drop(first);
    }

    #[test]
    fn allows_another_owner_after_the_lease_is_dropped() {
        let directory = tempdir().expect("temporary directory must exist");
        let path = directory.path().join("gunda.runtime.lock");

        let first = RuntimeLease::acquire(&path).expect("first lease must succeed");
        drop(first);

        assert!(path.exists());

        let second = RuntimeLease::acquire(&path).expect("released lease must be available");

        drop(second);
    }

    #[test]
    fn independent_directories_can_have_independent_owners() {
        let first_directory = tempdir().expect("first directory must exist");
        let second_directory = tempdir().expect("second directory must exist");

        let first = RuntimeLease::acquire(first_directory.path().join("gunda.runtime.lock"))
            .expect("first directory must be available");

        let second = RuntimeLease::acquire(second_directory.path().join("gunda.runtime.lock"))
            .expect("second directory must be available");

        drop(second);
        drop(first);
    }

    #[test]
    fn missing_parent_directory_returns_a_safe_error() {
        let directory = tempdir().expect("temporary directory must exist");
        let path = directory.path().join("missing").join("gunda.runtime.lock");

        let error = match RuntimeLease::acquire(&path) {
            Ok(_) => panic!("missing parent directory must fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), RepositoryErrorKind::Unavailable);
        assert_eq!(error.message(), "could not open the runtime lock file");
    }
}
