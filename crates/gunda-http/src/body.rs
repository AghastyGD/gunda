use bytes::Bytes;
use reqwest::Response;

use crate::error::map_request_error;
use crate::{HttpError, HttpInspection};

/// Incremental HTTP response body.
///
/// Received bytes are transport observations, not durable file checkpoints.
/// This type intentionally does not implement debug.
pub struct HttpBody {
    response: Option<Response>,
    metadata: HttpInspection,
    received_bytes: u64,
    failure: Option<HttpError>,
}

impl HttpBody {
    pub(crate) fn new(response: Response, metadata: HttpInspection) -> Self {
        Self {
            response: Some(response),
            metadata,
            received_bytes: 0,
            failure: None,
        }
    }

    #[must_use]
    pub const fn metadata(&self) -> &HttpInspection {
        &self.metadata
    }

    #[must_use]
    pub const fn received_bytes(&self) -> u64 {
        self.received_bytes
    }

    /// Reports whether a successful end of body has been observed.
    ///
    /// Reaching Content-Length alone does not set this flag
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.response.is_none() && self.failure.is_none()
    }

    /// Reads the next non-empty body block.
    ///
    /// None means a successful end of body. After a failure, subsquent calls
    /// return the same error tather than presenting the as complete.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, HttpError> {
        if let Some(error) = self.failure {
            return Err(error);
        }

        loop {
            let Some(response) = self.response.as_mut() else {
                return Ok(None);
            };

            let chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                Err(error) => {
                    let error = map_request_error(error);
                    return Err(self.stop_with_error(error));
                }
            };

            let Some(chunk) = chunk else {
                if let Some(expected) = self.metadata.content_length()
                    && self.received_bytes != expected
                {
                    return Err(self.stop_with_error(HttpError::BodyLengthMismatch));
                }

                self.response = None;
                return Ok(None);
            };

            if chunk.is_empty() {
                continue;
            }

            let size = match u64::try_from(chunk.len()) {
                Ok(size) => size,
                Err(_) => {
                    return Err(self.stop_with_error(HttpError::ByteCountOverflow));
                }
            };

            let Some(received_bytes) = self.received_bytes.checked_add(size) else {
                return Err(self.stop_with_error(HttpError::ByteCountOverflow));
            };

            self.received_bytes = received_bytes;

            return Ok(Some(chunk));
        }
    }

    fn stop_with_error(&mut self, error: HttpError) -> HttpError {
        self.failure = Some(error);
        self.response = None;
        error
    }
}
