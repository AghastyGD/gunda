use std::time::Duration;

use gunda_core::download::RequestContext;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName,
    HeaderValue,
};
use reqwest::{Client, StatusCode};

use crate::body::HttpBody;
use crate::error::{HttpError, map_request_error};

/// Metadata reported by a successful HTTP response.
///
/// HEAD metadata is advisory. A subsequent GET validates its own response.
/// This type intentionally does not implement Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpInspection {
    content_length: Option<u64>,
    content_type: Option<String>,
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
            .redirect(reqwest::redirect::Policy::none()) // TODO: maybe add a custom redirect policy here later?
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

    /// Requests metadata without downloading the resource body.
    ///
    /// Redirects and servers that reject HEAD are reported as errors.
    pub async fn inspect(&self, request: &RequestContext) -> Result<HttpInspection, HttpError> {
        validate_url(request)?;
        let headers = request_headers(request)?;

        let response = self
            .client
            .head(request.url().clone())
            .headers(headers)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(map_request_error)?;

        inspect_response(&response)
    }

    /// Opens a full-resource GET and validates its response metadata.
    ///
    /// The body is consumed incrementally through HttpBody.
    /// A previous HEAD request is not required.
    pub async fn open(&self, request: &RequestContext) -> Result<HttpBody, HttpError> {
        validate_url(request)?;
        let headers = request_headers(request)?;

        let response = self
            .client
            .get(request.url().clone())
            .headers(headers)
            .send()
            .await
            .map_err(map_request_error)?;

        let metadata = inspect_response(&response)?;

        Ok(HttpBody::new(response, metadata))
    }
}

fn inspect_response(response: &reqwest::Response) -> Result<HttpInspection, HttpError> {
    if response.status() != StatusCode::OK {
        return Err(HttpError::UnexpectedStatus(response.status().as_u16()));
    }

    let headers = response.headers();

    // Partial responses are not supported by this full-resource request path.
    if headers.contains_key(reqwest::header::CONTENT_RANGE) {
        return Err(HttpError::InvalidMetadata);
    }

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
