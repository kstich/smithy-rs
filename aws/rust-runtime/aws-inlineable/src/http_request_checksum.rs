/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

#![allow(dead_code)]

//! Interceptor for handling Smithy `@httpChecksum` request checksumming with AWS SigV4

use aws_http::content_encoding::{AwsChunkedBody, AwsChunkedBodyOptions};
use aws_runtime::auth::sigv4::SigV4OperationSigningConfig;
use aws_sigv4::http_request::SignableBody;
use aws_smithy_checksums::ChecksumAlgorithm;
use aws_smithy_checksums::{body::calculate, http::HttpChecksum};
use aws_smithy_http::body::{BoxBody, SdkBody};
use aws_smithy_http::operation::error::BuildError;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::context::{
    BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextMut, Input,
};
use aws_smithy_runtime_api::client::interceptors::Interceptor;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::config_bag::{ConfigBag, Layer, Storable, StoreReplace};
use http::HeaderValue;
use http_body::Body;
use std::{fmt, mem};

/// Errors related to constructing checksum-validated HTTP requests
#[derive(Debug)]
pub(crate) enum Error {
    /// Only request bodies with a known size can be checksum validated
    UnsizedRequestBody,
    ChecksumHeadersAreUnsupportedForStreamingBody,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsizedRequestBody => write!(
                f,
                "Only request bodies with a known size can be checksum validated."
            ),
            Self::ChecksumHeadersAreUnsupportedForStreamingBody => write!(
                f,
                "Checksum header insertion is only supported for non-streaming HTTP bodies. \
                   To checksum validate a streaming body, the checksums must be sent as trailers."
            ),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug)]
struct RequestChecksumInterceptorState {
    checksum_algorithm: Option<ChecksumAlgorithm>,
}
impl Storable for RequestChecksumInterceptorState {
    type Storer = StoreReplace<Self>;
}

pub(crate) struct RequestChecksumInterceptor<AP> {
    algorithm_provider: AP,
}

impl<AP> fmt::Debug for RequestChecksumInterceptor<AP> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestChecksumInterceptor").finish()
    }
}

impl<AP> RequestChecksumInterceptor<AP> {
    pub(crate) fn new(algorithm_provider: AP) -> Self {
        Self { algorithm_provider }
    }
}

impl<AP> Interceptor for RequestChecksumInterceptor<AP>
where
    AP: Fn(&Input) -> Result<Option<ChecksumAlgorithm>, BoxError>,
{
    fn read_before_serialization(
        &self,
        context: &BeforeSerializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let checksum_algorithm = (self.algorithm_provider)(context.input())?;

        let mut layer = Layer::new("RequestChecksumInterceptor");
        layer.store_put(RequestChecksumInterceptorState { checksum_algorithm });
        cfg.push_layer(layer);

        Ok(())
    }

    /// Calculate a checksum and modify the request to include the checksum as a header
    /// (for in-memory request bodies) or a trailer (for streaming request bodies).
    /// Streaming bodies must be sized or this will return an error.
    fn modify_before_retry_loop(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let state = cfg
            .load::<RequestChecksumInterceptorState>()
            .expect("set in `read_before_serialization`");

        if let Some(checksum_algorithm) = state.checksum_algorithm {
            let request = context.request_mut();
            add_checksum_for_request_body(request, checksum_algorithm, cfg)?;
        }

        Ok(())
    }
}

fn add_checksum_for_request_body(
    request: &mut http::request::Request<SdkBody>,
    checksum_algorithm: ChecksumAlgorithm,
    cfg: &mut ConfigBag,
) -> Result<(), BoxError> {
    match request.body().bytes() {
        // Body is in-memory: read it and insert the checksum as a header.
        Some(data) => {
            tracing::debug!("applying {checksum_algorithm:?} of the request body as a header");
            let mut checksum = checksum_algorithm.into_impl();
            checksum.update(data);

            request
                .headers_mut()
                .insert(checksum.header_name(), checksum.header_value());
        }
        // Body is streaming: wrap the body so it will emit a checksum as a trailer.
        None => {
            tracing::debug!("applying {checksum_algorithm:?} of the request body as a trailer");
            if let Some(mut signing_config) = cfg.load::<SigV4OperationSigningConfig>().cloned() {
                signing_config.signing_options.payload_override =
                    Some(SignableBody::StreamingUnsignedPayloadTrailer);
                cfg.interceptor_state().store_put(signing_config);
            }
            wrap_streaming_request_body_in_checksum_calculating_body(request, checksum_algorithm)?;
        }
    }
    Ok(())
}

fn wrap_streaming_request_body_in_checksum_calculating_body(
    request: &mut http::request::Request<SdkBody>,
    checksum_algorithm: ChecksumAlgorithm,
) -> Result<(), BuildError> {
    let original_body_size = request
        .body()
        .size_hint()
        .exact()
        .ok_or_else(|| BuildError::other(Error::UnsizedRequestBody))?;

    let mut body = {
        let body = mem::replace(request.body_mut(), SdkBody::taken());

        body.map(move |body| {
            let checksum = checksum_algorithm.into_impl();
            let trailer_len = HttpChecksum::size(checksum.as_ref());
            let body = calculate::ChecksumBody::new(body, checksum);
            let aws_chunked_body_options =
                AwsChunkedBodyOptions::new(original_body_size, vec![trailer_len]);

            let body = AwsChunkedBody::new(body, aws_chunked_body_options);

            SdkBody::from_dyn(BoxBody::new(body))
        })
    };

    let encoded_content_length = body
        .size_hint()
        .exact()
        .ok_or_else(|| BuildError::other(Error::UnsizedRequestBody))?;

    let headers = request.headers_mut();

    headers.insert(
        http::header::HeaderName::from_static("x-amz-trailer"),
        // Convert into a `HeaderName` and then into a `HeaderValue`
        http::header::HeaderName::from(checksum_algorithm).into(),
    );

    headers.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from(encoded_content_length),
    );
    headers.insert(
        http::header::HeaderName::from_static("x-amz-decoded-content-length"),
        HeaderValue::from(original_body_size),
    );
    headers.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_str(aws_http::content_encoding::header_value::AWS_CHUNKED)
            .map_err(BuildError::other)
            .expect("\"aws-chunked\" will always be a valid HeaderValue"),
    );

    mem::swap(request.body_mut(), &mut body);

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::http_request_checksum::wrap_streaming_request_body_in_checksum_calculating_body;
    use aws_smithy_checksums::ChecksumAlgorithm;
    use aws_smithy_http::body::SdkBody;
    use aws_smithy_http::byte_stream::ByteStream;
    use aws_smithy_types::base64;
    use bytes::BytesMut;
    use http_body::Body;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_checksum_body_is_retryable() {
        let input_text = "Hello world";
        let chunk_len_hex = format!("{:X}", input_text.len());
        let mut request = http::Request::builder()
            .body(SdkBody::retryable(move || SdkBody::from(input_text)))
            .unwrap();

        // ensure original SdkBody is retryable
        assert!(request.body().try_clone().is_some());

        let checksum_algorithm: ChecksumAlgorithm = "crc32".parse().unwrap();
        wrap_streaming_request_body_in_checksum_calculating_body(&mut request, checksum_algorithm)
            .unwrap();

        // ensure wrapped SdkBody is retryable
        let mut body = request.body().try_clone().expect("body is retryable");

        let mut body_data = BytesMut::new();
        loop {
            match body.data().await {
                Some(data) => body_data.extend_from_slice(&data.unwrap()),
                None => break,
            }
        }
        let body = std::str::from_utf8(&body_data).unwrap();
        assert_eq!(
            format!(
                "{chunk_len_hex}\r\n{input_text}\r\n0\r\nx-amz-checksum-crc32:i9aeUg==\r\n\r\n"
            ),
            body
        );
    }

    #[tokio::test]
    async fn test_checksum_body_from_file_is_retryable() {
        use std::io::Write;
        let mut file = NamedTempFile::new().unwrap();
        let checksum_algorithm: ChecksumAlgorithm = "crc32c".parse().unwrap();

        let mut crc32c_checksum = checksum_algorithm.into_impl();
        for i in 0..10000 {
            let line = format!("This is a large file created for testing purposes {}", i);
            file.as_file_mut().write_all(line.as_bytes()).unwrap();
            crc32c_checksum.update(line.as_bytes());
        }
        let crc32c_checksum = crc32c_checksum.finalize();

        let mut request = http::Request::builder()
            .body(
                ByteStream::read_from()
                    .path(&file)
                    .buffer_size(1024)
                    .build()
                    .await
                    .unwrap()
                    .into_inner(),
            )
            .unwrap();

        // ensure original SdkBody is retryable
        assert!(request.body().try_clone().is_some());

        wrap_streaming_request_body_in_checksum_calculating_body(&mut request, checksum_algorithm)
            .unwrap();

        // ensure wrapped SdkBody is retryable
        let mut body = request.body().try_clone().expect("body is retryable");

        let mut body_data = BytesMut::new();
        loop {
            match body.data().await {
                Some(data) => body_data.extend_from_slice(&data.unwrap()),
                None => break,
            }
        }
        let body = std::str::from_utf8(&body_data).unwrap();
        let expected_checksum = base64::encode(&crc32c_checksum);
        let expected = format!("This is a large file created for testing purposes 9999\r\n0\r\nx-amz-checksum-crc32c:{expected_checksum}\r\n\r\n");
        assert!(
            body.ends_with(&expected),
            "expected {body} to end with '{expected}'"
        );
    }
}
