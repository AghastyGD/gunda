//! The shared secret that guards the capture channel.
//!
//! The listener is bound to loopback, but loopback is reachable from every page
//! in the browser, so the port alone cannot be the thing that keeps other
//! people out. Every connection must present this token *and* come from an
//! extension origin before it may report anything.
//!
//! The token is generated once and stored in the application's data directory.
//! The user pastes it into the extension, which is the one manual step that
//! pairs the two halves of the feature.

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::error::CaptureError;

/// Length of a pairing token in bytes, before encoding.
const TOKEN_BYTES: usize = 32;

/// Length of an encoded pairing token, in characters.
const TOKEN_LENGTH: usize = TOKEN_BYTES * 2;

/// A shared secret the extension must present.
///
/// The value is deliberately not printable through `Debug` or `Display`: a
/// token must never reach a log through a struct that happens to contain one.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingToken(String);

impl PairingToken {
    /// Generates a token from the operating system's randomness.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Random`] if no secure randomness is available.
    pub fn generate() -> Result<Self, CaptureError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| CaptureError::Random)?;

        Ok(Self(encode_hex(&bytes)))
    }

    /// Parses a token, accepting either case and surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::InvalidToken`] unless the value is exactly
    /// [`TOKEN_BYTES`] bytes of hexadecimal.
    pub fn parse(value: &str) -> Result<Self, CaptureError> {
        let value = value.trim();

        if value.len() != TOKEN_LENGTH || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CaptureError::InvalidToken);
        }

        Ok(Self(value.to_ascii_lowercase()))
    }

    /// The token as text, for showing to the user and for storing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares a presented token.
    ///
    /// The comparison does not stop at the first difference. The length is
    /// still visible in the timing, which is unavoidable for a fixed-length
    /// secret and not useful to an attacker who cannot measure it across
    /// loopback traffic anyway.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let presented = presented.trim().to_ascii_lowercase();

        constant_time_eq(self.0.as_bytes(), presented.as_bytes())
    }
}

impl fmt::Debug for PairingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingToken(<redacted>)")
    }
}

/// The path of the pairing token inside a configuration directory.
#[must_use]
pub fn token_path(directory: &Path) -> PathBuf {
    directory.join("capture-token")
}

/// Reads a stored pairing token, if there is one.
///
/// # Errors
///
/// Returns [`CaptureError::TokenFile`] if the file exists but cannot be read,
/// and [`CaptureError::InvalidToken`] if its contents are not a token.
pub fn load_token(path: &Path) -> Result<Option<PairingToken>, CaptureError> {
    match fs::read_to_string(path) {
        Ok(contents) => PairingToken::parse(&contents).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CaptureError::TokenFile(error.kind())),
    }
}

/// Writes a pairing token, creating its directory if needed.
///
/// # Errors
///
/// Returns [`CaptureError::TokenFile`] if the file cannot be written.
pub fn save_token(path: &Path, token: &PairingToken) -> Result<(), CaptureError> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(|error| CaptureError::TokenFile(error.kind()))?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    // The token is a secret. Keep it owner-only where the platform expresses
    // that as a file mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| CaptureError::TokenFile(error.kind()))?;

    writeln!(file, "{}", token.as_str()).map_err(|error| CaptureError::TokenFile(error.kind()))
}

/// Reads the stored token, generating and storing one on first use.
///
/// # Errors
///
/// Returns [`CaptureError::TokenFile`] or [`CaptureError::Random`] when the
/// token cannot be produced or stored.
pub fn load_or_create_token(path: &Path) -> Result<PairingToken, CaptureError> {
    if let Some(token) = load_token(path)? {
        return Ok(token);
    }

    let token = PairingToken::generate()?;
    save_token(path, &token)?;

    Ok(token)
}

/// Encodes bytes as lowercase hexadecimal.
fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }

    encoded
}

/// Compares two byte strings without an early exit.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut difference = 0_u8;

    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }

    difference == 0
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{PairingToken, load_or_create_token, load_token, save_token, token_path};
    use crate::error::CaptureError;

    #[test]
    fn generates_a_hexadecimal_token() {
        let token = PairingToken::generate().expect("randomness is available");
        let text = token.as_str();

        assert_eq!(text.len(), 64);
        assert!(text.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(text.bytes().all(|byte| !byte.is_ascii_uppercase()));
    }

    #[test]
    fn generates_a_different_token_each_time() {
        let first = PairingToken::generate().expect("randomness is available");
        let second = PairingToken::generate().expect("randomness is available");

        assert_ne!(first, second);
    }

    #[test]
    fn never_prints_the_secret() {
        let token = PairingToken::generate().expect("randomness is available");

        assert_eq!(format!("{token:?}"), "PairingToken(<redacted>)");
    }

    #[test]
    fn parses_a_token_from_user_input() {
        let text = "0011223344556677889900112233445566778899001122334455667788990011";

        let token = PairingToken::parse(&format!("  {text}\n")).expect("the token is valid");

        assert_eq!(token.as_str(), text);
    }

    #[test]
    fn refuses_a_token_that_is_not_a_token() {
        assert_eq!(
            PairingToken::parse("").expect_err("an empty token is invalid"),
            CaptureError::InvalidToken
        );
        assert_eq!(
            PairingToken::parse("00").expect_err("a short token is invalid"),
            CaptureError::InvalidToken
        );
        assert_eq!(
            PairingToken::parse(&"z".repeat(64)).expect_err("a non-hex token is invalid"),
            CaptureError::InvalidToken
        );
    }

    #[test]
    fn compares_tokens_without_an_early_exit() {
        let token = PairingToken::parse(&"a1".repeat(32)).expect("the token is valid");

        assert!(token.matches(&"a1".repeat(32)));
        assert!(
            token.matches(&format!("  {}  ", "A1".repeat(32))),
            "case and spacing are forgiving"
        );
    }

    #[test]
    fn answers_a_presented_token_that_is_not_the_one() {
        let token = PairingToken::parse(&"a1".repeat(32)).expect("the token is valid");

        assert!(
            !token.matches(&"a2".repeat(32)),
            "the last character differs"
        );
        assert!(!token.matches(&"a1".repeat(31)), "the length differs");
        assert!(!token.matches(""));
    }

    #[test]
    fn stores_and_reads_a_token() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = token_path(directory.path());

        assert_eq!(
            load_token(&path).expect("a missing token is not an error"),
            None
        );

        let token = PairingToken::generate().expect("randomness is available");
        save_token(&path, &token).expect("the token is stored");

        let loaded = load_token(&path)
            .expect("the token is readable")
            .expect("the token exists");

        assert_eq!(loaded, token);
    }

    #[test]
    fn creates_a_token_on_first_use_and_reuses_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = token_path(directory.path());

        let first = load_or_create_token(&path).expect("a token is created");
        let second = load_or_create_token(&path).expect("the token is reused");

        assert_eq!(first, second);
        assert_eq!(second.as_str().len(), 64);
    }

    #[test]
    fn refuses_a_stored_file_that_is_not_a_token() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = token_path(directory.path());

        std::fs::write(&path, "not a token").expect("the file is written");

        assert_eq!(
            load_token(&path).expect_err("junk is not a token"),
            CaptureError::InvalidToken
        );
    }

    #[test]
    fn token_path_lives_in_the_given_directory() {
        assert!(token_path(Path::new("/tmp/gunda")).ends_with("capture-token"));
    }
}
