use std::path::{Path, PathBuf};

use gunda_core::application::RepositoryError;

#[cfg(not(unix))]
use gunda_core::application::RepositoryErrorKind;

#[cfg(unix)]
pub(crate) fn encode(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    use std::os::unix::ffi::OsStrExt;

    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(unix)]
pub(crate) fn decode(bytes: &[u8]) -> Result<PathBuf, RepositoryError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
pub(crate) fn encode(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    use std::os::windows::ffi::OsStrExt;

    Ok(path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect())
}

#[cfg(windows)]
pub(crate) fn decode(bytes: &[u8]) -> Result<PathBuf, RepositoryError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let (pairs, remainder) = bytes.as_chunks::<2>();

    if !remainder.is_empty() {
        return Err(invalid_path(
            "stored Windows path has an invalid byte length",
        ));
    }

    let wide: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();

    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn encode(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid_path("path cannot be represented on this platform"))?;

    Ok(text.as_bytes().to_vec())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn decode(bytes: &[u8]) -> Result<PathBuf, RepositoryError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| invalid_path("stored path is not valid UTF-8"))?;

    Ok(PathBuf::from(text))
}

#[cfg(not(unix))]
fn invalid_path(message: &'static str) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{decode, encode};

    #[test]
    fn native_path_round_trips_through_binary_representation() {
        let original = PathBuf::from("downloads").join("nested").join("file.iso");

        let encoded = encode(&original).expect("path must encode");
        let decoded = decode(&encoded).expect("path must decode");

        assert_eq!(decoded, original);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unix_path_round_trips_without_data_loss() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let original = PathBuf::from(OsString::from_vec(vec![b'f', b'i', b'l', b'e', 0xff]));

        let encoded = encode(&original).expect("path must encode");
        let decoded = decode(&encoded).expect("path must decode");

        assert_eq!(decoded, original);
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_windows_code_unit_round_trips_without_data_loss() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let original = PathBuf::from(OsString::from_wide(&[
            b'f' as u16,
            b'i' as u16,
            b'l' as u16,
            b'e' as u16,
            0xd800,
        ]));

        let encoded = encode(&original).expect("path must encode");
        let decoded = decode(&encoded).expect("path must decode");

        assert_eq!(decoded, original);
    }

    #[cfg(windows)]
    #[test]
    fn odd_length_windows_path_is_rejected() {
        use gunda_core::application::RepositoryErrorKind;

        let error = decode(&[0x61]).expect_err("odd byte length must be rejected");

        assert_eq!(error.kind(), RepositoryErrorKind::InvalidData);
    }
}
