use bytes::Bytes;

use crate::{HttpBody, HttpError, HttpInspection};

/// Validated byte interval returned in a partial HTTP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

impl ContentRange {
    pub fn parse(value: &str) -> Result<Self, HttpError> {
        let (unit, value) = value.split_once(' ').ok_or(HttpError::InvalidMetadata)?;

        if !unit.eq_ignore_ascii_case("bytes") {
            return Err(HttpError::InvalidMetadata);
        }

        let (interval, total) = value.split_once('/').ok_or(HttpError::InvalidMetadata)?;

        let (start, end) = interval.split_once('-').ok_or(HttpError::InvalidMetadata)?;

        let start = parse_decimal(start)?;
        let end = parse_decimal(end)?;
        let total = parse_decimal(total)?;

        if start > end || end >= total {
            return Err(HttpError::InvalidMetadata);
        }

        Ok(Self { start, end, total })
    }

    #[must_use]
    pub const fn start(&self) -> u64 {
        self.start
    }

    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub const fn body_length(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// Streams the remaining bytes of a validated HTTP range response.
pub struct HttpRangeBody {
    body: HttpBody,
    range: ContentRange,
}

impl HttpRangeBody {
    pub(crate) fn new(body: HttpBody, range: ContentRange) -> Self {
        Self { body, range }
    }

    #[must_use]
    pub const fn range(&self) -> ContentRange {
        self.range
    }

    #[must_use]
    pub fn metadata(&self) -> &HttpInspection {
        self.body.metadata()
    }

    #[must_use]
    pub fn received_bytes(&self) -> u64 {
        self.body.received_bytes()
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.body.is_finished()
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, HttpError> {
        self.body.next_chunk().await
    }
}

fn parse_decimal(value: &str) -> Result<u64, HttpError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(HttpError::InvalidMetadata);
    }

    value.parse().map_err(|_| HttpError::InvalidMetadata)
}

#[cfg(test)]
mod tests {
    use super::ContentRange;
    use crate::HttpError;

    #[test]
    fn parses_inclusive_byte_intervals() {
        let range = ContentRange::parse("bytes 5-9/10").expect("range must be valid");

        assert_eq!(range.start(), 5);
        assert_eq!(range.end(), 9);
        assert_eq!(range.total(), 10);
        assert_eq!(range.body_length(), 5);
    }

    #[test]
    fn accepts_single_byte_and_large_ranges() {
        let single = ContentRange::parse("bytes 0-0/1").expect("single-byte range must be valid");

        assert_eq!(single.body_length(), 1);

        let large = ContentRange::parse("bytes 0-18446744073709551614/18446744073709551615")
            .expect("large range must be valid");

        assert_eq!(large.body_length(), u64::MAX);
    }

    #[test]
    fn rejects_invalid_and_unsupported_ranges() {
        for value in [
            "",
            "items 5-9/10",
            "bytes */10",
            "bytes 5-9/*",
            "bytes 9-5/10",
            "bytes 5-10/10",
            "bytes 0-0/0",
            "bytes -1-9/10",
            "bytes +5-9/10",
            "bytes 5-9/10/20",
            "bytes 5-9/10, bytes 20-29/30",
            "bytes 5-9/18446744073709551616",
        ] {
            assert_eq!(
                ContentRange::parse(value),
                Err(HttpError::InvalidMetadata),
                "unexpected result for {value}",
            );
        }
    }
}
