/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use aws_smithy_client::SdkError;
use aws_smithy_http::http::HttpHeaders;
use aws_smithy_http::operation;
use aws_smithy_types::error::metadata::{
    Builder as ErrorMetadataBuilder, ErrorMetadata, ProvideErrorMetadata,
};
use aws_smithy_types::error::Unhandled;
use http::{HeaderMap, HeaderValue};

const EXTENDED_REQUEST_ID: &str = "s3_extended_request_id";

/// Trait to retrieve the S3-specific extended request ID
///
/// Read more at <https://aws.amazon.com/premiumsupport/knowledge-center/s3-request-id-values/>.
pub trait RequestIdExt {
    /// Returns the S3 Extended Request ID necessary when contacting AWS Support.
    fn extended_request_id(&self) -> Option<&str>;
}

impl<E, R> RequestIdExt for SdkError<E, R>
where
    R: HttpHeaders,
{
    fn extended_request_id(&self) -> Option<&str> {
        match self {
            Self::ResponseError(err) => extract_extended_request_id(err.raw().http_headers()),
            Self::ServiceError(err) => extract_extended_request_id(err.raw().http_headers()),
            _ => None,
        }
    }
}

impl RequestIdExt for ErrorMetadata {
    fn extended_request_id(&self) -> Option<&str> {
        self.extra(EXTENDED_REQUEST_ID)
    }
}

impl RequestIdExt for Unhandled {
    fn extended_request_id(&self) -> Option<&str> {
        self.meta().extended_request_id()
    }
}

impl RequestIdExt for operation::Response {
    fn extended_request_id(&self) -> Option<&str> {
        extract_extended_request_id(self.http().headers())
    }
}

impl<B> RequestIdExt for http::Response<B> {
    fn extended_request_id(&self) -> Option<&str> {
        extract_extended_request_id(self.headers())
    }
}

impl RequestIdExt for HeaderMap {
    fn extended_request_id(&self) -> Option<&str> {
        extract_extended_request_id(self)
    }
}

impl<O, E> RequestIdExt for Result<O, E>
where
    O: RequestIdExt,
    E: RequestIdExt,
{
    fn extended_request_id(&self) -> Option<&str> {
        match self {
            Ok(ok) => ok.extended_request_id(),
            Err(err) => err.extended_request_id(),
        }
    }
}

/// Applies the extended request ID to a generic error builder
#[doc(hidden)]
pub fn apply_extended_request_id(
    builder: ErrorMetadataBuilder,
    headers: &HeaderMap<HeaderValue>,
) -> ErrorMetadataBuilder {
    if let Some(extended_request_id) = extract_extended_request_id(headers) {
        builder.custom(EXTENDED_REQUEST_ID, extended_request_id)
    } else {
        builder
    }
}

/// Extracts the S3 Extended Request ID from HTTP response headers
fn extract_extended_request_id(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    headers
        .get("x-amz-id-2")
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod test {
    use super::*;
    use aws_smithy_client::SdkError;
    use aws_smithy_http::body::SdkBody;
    use http::Response;

    #[test]
    fn handle_missing_header() {
        let resp = http::Response::builder().status(400).body("").unwrap();
        let mut builder = aws_smithy_types::Error::builder().message("123");
        builder = apply_extended_request_id(builder, resp.headers());
        assert_eq!(builder.build().extended_request_id(), None);
    }

    #[test]
    fn test_extended_request_id_sdk_error() {
        let without_extended_request_id =
            || operation::Response::new(Response::builder().body(SdkBody::empty()).unwrap());
        let with_extended_request_id = || {
            operation::Response::new(
                Response::builder()
                    .header("x-amz-id-2", HeaderValue::from_static("some-request-id"))
                    .body(SdkBody::empty())
                    .unwrap(),
            )
        };
        assert_eq!(
            None,
            SdkError::<(), _>::response_error("test", without_extended_request_id())
                .extended_request_id()
        );
        assert_eq!(
            Some("some-request-id"),
            SdkError::<(), _>::response_error("test", with_extended_request_id())
                .extended_request_id()
        );
        assert_eq!(
            None,
            SdkError::service_error((), without_extended_request_id()).extended_request_id()
        );
        assert_eq!(
            Some("some-request-id"),
            SdkError::service_error((), with_extended_request_id()).extended_request_id()
        );
    }

    #[test]
    fn test_extract_extended_request_id() {
        let mut headers = HeaderMap::new();
        assert_eq!(None, extract_extended_request_id(&headers));

        headers.append("x-amz-id-2", HeaderValue::from_static("some-request-id"));
        assert_eq!(
            Some("some-request-id"),
            extract_extended_request_id(&headers)
        );
    }

    #[test]
    fn test_apply_extended_request_id() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            ErrorMetadata::builder().build(),
            apply_extended_request_id(ErrorMetadata::builder(), &headers).build(),
        );

        headers.append("x-amz-id-2", HeaderValue::from_static("some-request-id"));
        assert_eq!(
            ErrorMetadata::builder()
                .custom(EXTENDED_REQUEST_ID, "some-request-id")
                .build(),
            apply_extended_request_id(ErrorMetadata::builder(), &headers).build(),
        );
    }

    #[test]
    fn test_error_metadata_extended_request_id_impl() {
        let err = ErrorMetadata::builder()
            .custom(EXTENDED_REQUEST_ID, "some-request-id")
            .build();
        assert_eq!(Some("some-request-id"), err.extended_request_id());
    }
}
