use std::fmt;

use url::Url;

/// Defines how a request header value must be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderSensitivity {
    /// The value may appear in diagnostic output.
    Public,

    /// The value must be redacted from diagnostic output.
    Sensitive,
}

/// Header required to reproduce a remote request.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestHeader {
    name: String,
    value: String,
    sensitivity: HeaderSensitivity,
}

impl RequestHeader {
    pub fn new(
        name: impl Into<String>,
        value: impl Into<String>,
        sensitivity: HeaderSensitivity,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            sensitivity,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the original header value
    ///
    /// Callers must respect `sensitivity()` and must not log sensitive values.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn sensitivity(&self) -> HeaderSensitivity {
        self.sensitivity
    }

    #[must_use]
    pub const fn is_sensitive(&self) -> bool {
        matches!(self.sensitivity, HeaderSensitivity::Sensitive)
    }
}

impl fmt::Debug for RequestHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let displayed_value = match self.sensitivity {
            HeaderSensitivity::Public => self.value.as_str(),
            HeaderSensitivity::Sensitive => "[REDACTED]",
        };

        f.debug_struct("RequestHeader")
            .field("name", &self.name)
            .field("value", &displayed_value)
            .field("sensitivity", &self.sensitivity)
            .finish()
    }
}

/// Information required to reproduce a remote request.
///
/// This type intentionally does not implement `Debug`, because URLs may contain
/// signed query parameters or other sensitive information.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestContext {
    url: Url,
    headers: Vec<RequestHeader>,
}

impl RequestContext {
    #[must_use]
    pub fn new(url: Url, headers: Vec<RequestHeader>) -> Self {
        Self { url, headers }
    }

    #[must_use]
    pub const fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub fn headers(&self) -> &[RequestHeader] {
        &self.headers
    }

    #[must_use]
    pub fn has_sensitive_headers(&self) -> bool {
        self.headers.iter().any(RequestHeader::is_sensitive)
    }
}

#[cfg(test)]
mod tests {
    use super::{HeaderSensitivity, RequestContext, RequestHeader};
    use url::Url;

    #[test]
    fn sensitive_header_value_is_redacted_from_debug_output() {
        let header = RequestHeader::new(
            "Authorization",
            "Bearer secret-token",
            HeaderSensitivity::Sensitive,
        );

        let output = format!("{header:?}");

        assert!(output.contains("Authorization"));
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("secret-token"));
    }

    #[test]
    fn request_context_reports_sensitive_headers() {
        let request = RequestContext::new(
            Url::parse("https://example.com/file").expect("URL must be valid"),
            vec![RequestHeader::new(
                "Cookie",
                "session=secret",
                HeaderSensitivity::Sensitive,
            )],
        );

        assert!(request.has_sensitive_headers());
        assert_eq!(request.headers().len(), 1);
    }
}
