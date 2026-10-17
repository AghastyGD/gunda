use std::time::Duration;

use gunda_core::download::RequestContext;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderName, HeaderValue, IF_RANGE, LOCATION, RANGE,
};
use reqwest::{Client, Method, StatusCode};

use crate::body::HttpBody;
use crate::error::{HttpError, map_request_error};
use crate::range::{ContentRange, HttpRangeBody};
use crate::validator::StrongEntityTag;

/// Response metadata and expected body length.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpInspection {
    content_length: Option<u64>,
    content_type: Option<String>,
    strong_etag: Option<StrongEntityTag>,
}

impl HttpInspection {
    #[must_use]
    pub const fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    #[must_use]
    pub fn strong_etag(&self) -> Option<&StrongEntityTag> {
        self.strong_etag.as_ref()
    }

    pub(crate) fn resumed(
        total_bytes: u64,
        content_type: Option<String>,
        strong_etag: StrongEntityTag,
    ) -> Self {
        Self {
            content_length: Some(total_bytes),
            content_type,
            strong_etag: Some(strong_etag),
        }
    }

    /// Describes a staging file written by another engine.
    ///
    /// There is no remote validator for such a file, so the strong entity tag is
    /// absent and resumption against the origin is unavailable by construction.
    pub(crate) fn staged(total_bytes: u64, content_type: Option<String>) -> Self {
        Self {
            content_length: Some(total_bytes),
            content_type,
            strong_etag: None,
        }
    }
}

/// Reusable HTTP client with explicit request policy.
///
/// This initial implementation uses direct connections without proxy support.
pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new() -> Result<Self, HttpError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|_| HttpError::Configuration)?;

        Ok(Self { client })
    }

    async fn send_redirected(
        &self,
        request: &RequestContext,
        method: Method,
        mut headers: HeaderMap,
    ) -> Result<reqwest::Response, HttpError> {
        let mut url = request.url().clone();
        for hop in 0..=10 {
            let mut builder = self
                .client
                .request(method.clone(), url.clone())
                .headers(headers.clone());
            if method == Method::HEAD {
                builder = builder.timeout(Duration::from_secs(30));
            }
            let response = builder.send().await.map_err(map_request_error)?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            if hop == 10 {
                return Err(HttpError::Redirect);
            }
            let location =
                single_header(response.headers(), LOCATION)?.ok_or(HttpError::InvalidMetadata)?;
            let next = url.join(location).map_err(|_| HttpError::InvalidMetadata)?;
            validate_redirect(&url, &next)?;
            if url.origin() != next.origin() {
                // Caller headers may contain credentials, including custom tokens.
                // Only transport-owned headers travel to a different origin.
                let mut transport_headers = HeaderMap::new();
                for name in [ACCEPT_ENCODING, RANGE, IF_RANGE] {
                    if let Some(value) = headers.get(&name) {
                        transport_headers.insert(name, value.clone());
                    }
                }
                headers = transport_headers;
            }
            url = next;
        }
        unreachable!("redirect loop always returns within its hop limit")
    }

    /// Requests metadata without downloading the resource body.
    ///
    /// Follows HTTP redirects; servers that reject HEAD are reported as errors.
    pub async fn inspect(&self, request: &RequestContext) -> Result<HttpInspection, HttpError> {
        validate_url(request)?;
        let headers = request_headers(request)?;

        let response = self.send_redirected(request, Method::HEAD, headers).await?;

        inspect_response(&response)
    }

    /// Opens a full-resource GET and validates its response metadata.
    ///
    /// The body is consumed incrementally through HttpBody.
    /// A previous HEAD request is not required.
    pub async fn open(&self, request: &RequestContext) -> Result<HttpBody, HttpError> {
        validate_url(request)?;
        let headers = request_headers(request)?;

        let response = self.send_redirected(request, Method::GET, headers).await?;

        let metadata = inspect_response(&response)?;

        Ok(HttpBody::new(response, metadata))
    }

    /// Opens the remanining bytes of a representation using a strong validator.
    pub async fn open_range(
        &self,
        request: &RequestContext,
        start: u64,
        total: u64,
        validator: &StrongEntityTag,
    ) -> Result<HttpRangeBody, HttpError> {
        validate_url(request)?;

        if start == 0 || start >= total {
            return Err(HttpError::InvalidRequest);
        }

        let mut headers = request_headers(request)?;

        let range_value = HeaderValue::from_str(&format!("bytes={start}-"))
            .map_err(|_| HttpError::InvalidRequest)?;

        let mut validator_value =
            HeaderValue::from_bytes(validator.as_bytes()).map_err(|_| HttpError::InvalidRequest)?;

        validator_value.set_sensitive(true);

        headers.insert(RANGE, range_value);
        headers.insert(IF_RANGE, validator_value);

        let response = self.send_redirected(request, Method::GET, headers).await?;

        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(HttpError::UnexpectedStatus(response.status().as_u16()));
        }

        let response_headers = response.headers();

        let range = single_header(response_headers, CONTENT_RANGE)?
            .ok_or(HttpError::InvalidMetadata)
            .and_then(ContentRange::parse)?;

        if range.start() != start || range.total() != total || range.end() != total - 1 {
            return Err(HttpError::InvalidMetadata);
        }

        let mut metadata = inspect_headers(response_headers)?;

        if metadata
            .content_length
            .is_some_and(|length| length != range.body_length())
        {
            return Err(HttpError::InvalidMetadata);
        }

        ensure_single_interval(&metadata)?;

        if response_headers.contains_key(ETAG) && metadata.strong_etag.as_ref() != Some(validator) {
            return Err(HttpError::InvalidMetadata);
        }

        // Content-Range also establishes the expected length for chunked bodies.
        metadata.content_length = Some(range.body_length());

        Ok(HttpRangeBody::new(HttpBody::new(response, metadata), range))
    }

    /// Opens an explicit byte interval of a resource.
    ///
    /// Unlike [`Self::open_range`], which resumes from an offset to the end of
    /// a representation and requires a strong validator, this requests exactly
    /// `first..=last` and needs no `If-Range`: the interval comes from the
    /// resource's own manifest, so absolute positions are meaningful, and the
    /// response is rejected unless the server returns precisely that interval.
    ///
    /// A server that ignores `Range` answers `200 OK` with the whole resource.
    /// Accepting that would append an entire file where one segment was asked
    /// for, so it is reported as an unexpected status instead.
    pub async fn open_byte_range(
        &self,
        request: &RequestContext,
        first: u64,
        last: u64,
    ) -> Result<HttpRangeBody, HttpError> {
        validate_url(request)?;

        if last < first {
            return Err(HttpError::InvalidRequest);
        }

        let mut headers = request_headers(request)?;

        let range_value = HeaderValue::from_str(&format!("bytes={first}-{last}"))
            .map_err(|_| HttpError::InvalidRequest)?;

        headers.insert(RANGE, range_value);

        let response = self.send_redirected(request, Method::GET, headers).await?;

        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(HttpError::UnexpectedStatus(response.status().as_u16()));
        }

        let response_headers = response.headers();

        let range = single_header(response_headers, CONTENT_RANGE)?
            .ok_or(HttpError::InvalidMetadata)
            .and_then(ContentRange::parse)?;

        if range.start() != first || range.end() != last {
            return Err(HttpError::InvalidMetadata);
        }

        let mut metadata = inspect_headers(response_headers)?;

        if metadata
            .content_length
            .is_some_and(|length| length != range.body_length())
        {
            return Err(HttpError::InvalidMetadata);
        }

        ensure_single_interval(&metadata)?;

        // Content-Range also establishes the expected length for chunked bodies.
        metadata.content_length = Some(range.body_length());

        Ok(HttpRangeBody::new(HttpBody::new(response, metadata), range))
    }
}

/// Rejects metadata that cannot describe one byte interval.
///
/// A `multipart/byteranges` body carries several intervals at once;
/// concatenating it as though it were a single interval corrupts the output.
fn ensure_single_interval(metadata: &HttpInspection) -> Result<(), HttpError> {
    let multipart = metadata.content_type.as_deref().is_some_and(|value| {
        value.split(';').next().is_some_and(|media_type| {
            media_type
                .trim()
                .eq_ignore_ascii_case("multipart/byteranges")
        })
    });

    if multipart {
        return Err(HttpError::InvalidMetadata);
    }

    Ok(())
}

fn validate_redirect(current: &url::Url, next: &url::Url) -> Result<(), HttpError> {
    validate_url(&RequestContext::new(next.clone(), Vec::new()))?;
    if current.scheme() == "https" && next.scheme() != "https" {
        return Err(HttpError::InvalidRequest);
    }
    Ok(())
}

fn inspect_response(response: &reqwest::Response) -> Result<HttpInspection, HttpError> {
    if response.status() != StatusCode::OK {
        return Err(HttpError::UnexpectedStatus(response.status().as_u16()));
    }

    let headers = response.headers();

    if headers.contains_key(CONTENT_RANGE) {
        return Err(HttpError::InvalidMetadata);
    }

    inspect_headers(headers)
}

fn inspect_headers(headers: &HeaderMap) -> Result<HttpInspection, HttpError> {
    if let Some(encoding) = single_header(headers, CONTENT_ENCODING)?
        && !encoding.trim().eq_ignore_ascii_case("identity")
    {
        return Err(HttpError::UnsupportedEncoding);
    }

    let content_length = single_header(headers, CONTENT_LENGTH)?
        .map(|value| {
            let value = value.trim();

            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpError::InvalidMetadata);
            }

            value.parse::<u64>().map_err(|_| HttpError::InvalidMetadata)
        })
        .transpose()?;

    let content_type = single_header(headers, CONTENT_TYPE)?.map(str::to_owned);

    Ok(HttpInspection {
        content_length,
        content_type,
        strong_etag: response_strong_etag(headers),
    })
}

fn validate_url(request: &RequestContext) -> Result<(), HttpError> {
    let url = request.url();

    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(HttpError::InvalidRequest);
    }

    Ok(())
}

fn request_headers(request: &RequestContext) -> Result<HeaderMap, HttpError> {
    let mut headers = HeaderMap::new();

    for header in request.headers() {
        let name = HeaderName::from_bytes(header.name().as_bytes())
            .map_err(|_| HttpError::InvalidRequest)?;

        // Framing, routing, encoding and partial/conditional requests belong
        // to the transport policy, not arbitary caller-provided headers.
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "keep-alive"
                | "proxy-connection"
                | "proxy-authorization"
                | "expect"
                | "accept-encoding"
                | "range"
                | "if-range"
                | "if-match"
                | "if-none-match"
                | "if-modified-since"
                | "if-unmodified-since"
        ) {
            return Err(HttpError::InvalidRequest);
        }

        let mut value = HeaderValue::from_bytes(header.value().as_bytes())
            .map_err(|_| HttpError::InvalidRequest)?;

        // Even a publicly persistable value need not be suitable for logs.
        value.set_sensitive(true);
        headers.append(name, value);
    }

    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    Ok(headers)
}

fn single_header(headers: &HeaderMap, name: HeaderName) -> Result<Option<&str>, HttpError> {
    let mut values = headers.get_all(name).iter();

    let Some(value) = values.next() else {
        return Ok(None);
    };

    if values.next().is_some() {
        return Err(HttpError::InvalidMetadata);
    }

    value
        .to_str()
        .map(Some)
        .map_err(|_| HttpError::InvalidMetadata)
}

fn response_strong_etag(headers: &HeaderMap) -> Option<StrongEntityTag> {
    let mut values = headers.get_all(ETAG).iter();
    let value = values.next()?;

    if values.next().is_some() {
        return None;
    }

    StrongEntityTag::parse(value.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::validate_redirect;
    use crate::HttpError;
    use url::Url;

    #[test]
    fn redirects_reject_https_downgrades() {
        let current = Url::parse("https://example.com/file.bin").unwrap();
        let downgraded = Url::parse("http://example.com/file.bin").unwrap();
        let secure = Url::parse("https://cdn.example.com/file.bin").unwrap();

        assert_eq!(
            validate_redirect(&current, &downgraded),
            Err(HttpError::InvalidRequest)
        );
        assert_eq!(validate_redirect(&current, &secure), Ok(()));
    }
}
