use std::error::Error;
use std::fmt;

/// Strong HTTP entity tag preserved for conditional requests.
#[derive(Clone, PartialEq, Eq)]
pub struct StrongEntityTag {
    value: Vec<u8>,
}

impl StrongEntityTag {
    pub fn parse(value: &[u8]) -> Result<Self, InvalidStrongEntityTag> {
        let opaque = value
            .strip_prefix(b"\"")
            .and_then(|value| value.strip_suffix(b"\""))
            .ok_or(InvalidStrongEntityTag)?;

        if !opaque
            .iter()
            .all(|byte| matches!(*byte, 0x21 | 0x23..=0x7e | 0x80..=0xff))
        {
            return Err(InvalidStrongEntityTag);
        }

        Ok(Self {
            value: value.to_vec(),
        })
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.value
    }
}

impl fmt::Debug for StrongEntityTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StrongEntityTag([redacted])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStrongEntityTag;

impl fmt::Display for InvalidStrongEntityTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid strong HTTP entity tag")
    }
}

impl Error for InvalidStrongEntityTag {}

#[cfg(test)]
mod tests {
    use super::StrongEntityTag;

    #[test]
    fn preserves_valid_tags_exactly() {
        for value in [
            b"\"version-81\"".as_slice(),
            b"\"\"".as_slice(),
            b"\"a,b\"".as_slice(),
            b"\"a\\b\"".as_slice(),
            b"\"\x80\xff\"".as_slice(),
        ] {
            let tag = StrongEntityTag::parse(value).expect("strong entity tag must be valid");

            assert_eq!(tag.as_bytes(), value);
        }
    }

    #[test]
    fn rejects_weak_tags_and_invalid_syntax() {
        for value in [
            b"W/\"version-81\"".as_slice(),
            b"w/\"version-81\"".as_slice(),
            b"version-81".as_slice(),
            b"*".as_slice(),
            b"\"".as_slice(),
            b"\"a\"b\"".as_slice(),
            b"\"a b\"".as_slice(),
            b"\"a\tb\"".as_slice(),
            b"\"a\r\nb\"".as_slice(),
            b"\"a\x7fb\"".as_slice(),
            b"\"one\", \"two\"".as_slice(),
        ] {
            assert!(StrongEntityTag::parse(value).is_err());
        }
    }

    #[test]
    fn comparison_is_case_sensitive() {
        let first = StrongEntityTag::parse(b"\"Version\"").expect("first tag must be valid");
        let second = StrongEntityTag::parse(b"\"version\"").expect("second tag must be valid");

        assert_ne!(first, second);
    }

    #[test]
    fn diagnostics_do_not_expose_the_tag_value() {
        let tag =
            StrongEntityTag::parse(b"\"private-validator-marker\"").expect("tag must be valid");

        assert!(!format!("{tag:?}").contains("private-validator-marker"));

        let error = StrongEntityTag::parse(b"private-validator-marker")
            .expect_err("unquoted value must be rejected");

        assert!(!format!("{error} {error:?}").contains("private-validator-marker"));
    }
}
