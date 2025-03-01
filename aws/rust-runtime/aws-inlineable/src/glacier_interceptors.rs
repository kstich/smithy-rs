/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

// This code is referenced in generated code, so the compiler doesn't realize it is used.
#![allow(dead_code)]

use aws_runtime::auth::sigv4::SigV4OperationSigningConfig;
use aws_sigv4::http_request::SignableBody;
use aws_smithy_http::body::SdkBody;
use aws_smithy_http::byte_stream;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::config_bag_accessors::ConfigBagAccessors;
use aws_smithy_runtime_api::client::interceptors::context::{
    BeforeSerializationInterceptorContextMut, BeforeTransmitInterceptorContextMut,
};
use aws_smithy_runtime_api::client::interceptors::Interceptor;
use aws_smithy_runtime_api::client::orchestrator::LoadedRequestBody;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::config_bag::ConfigBag;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::Request;
use ring::digest::{Context, Digest, SHA256};
use std::fmt;
use std::marker::PhantomData;

/// The default account ID when none is set on an input
const DEFAULT_ACCOUNT_ID: &str = "-";

const TREE_HASH_HEADER: &str = "x-amz-sha256-tree-hash";
const X_AMZ_CONTENT_SHA256: &str = "x-amz-content-sha256";
const API_VERSION_HEADER: &str = "x-amz-glacier-version";

/// Adds an account ID autofill method to generated input structs
///
/// Some Glacier operations have an account ID field that needs to get defaulted to `-` if not set.
/// This trait is implemented via codegen customization for those operation inputs so that
/// the [`GlacierAccountIdAutofillInterceptor`] can do this defaulting.
pub(crate) trait GlacierAccountId: fmt::Debug {
    /// Returns a mutable reference to the account ID field
    fn account_id_mut(&mut self) -> &mut Option<String>;

    /// Autofills the account ID with the default if not set
    fn autofill_account_id(&mut self) {
        let account_id = self.account_id_mut();
        if account_id.as_deref().unwrap_or_default().is_empty() {
            *account_id = Some(DEFAULT_ACCOUNT_ID.into());
        }
    }
}

/// Autofills account ID input fields with a default if no value is set
#[derive(Debug)]
pub(crate) struct GlacierAccountIdAutofillInterceptor<I> {
    _phantom: PhantomData<I>,
}

impl<I> GlacierAccountIdAutofillInterceptor<I> {
    /// Constructs a new [`GlacierAccountIdAutofillInterceptor`]
    pub(crate) fn new() -> Self {
        Self {
            _phantom: Default::default(),
        }
    }
}

impl<I: GlacierAccountId + Send + Sync + 'static> Interceptor
    for GlacierAccountIdAutofillInterceptor<I>
{
    fn modify_before_serialization(
        &self,
        context: &mut BeforeSerializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let erased_input = context.input_mut();
        let input: &mut I = erased_input
            .downcast_mut()
            .expect("typechecked at registration");
        input.autofill_account_id();
        Ok(())
    }
}

/// Attaches the `x-amz-glacier-version` header to the request
#[derive(Debug)]
pub(crate) struct GlacierApiVersionInterceptor {
    api_version: &'static str,
}

impl GlacierApiVersionInterceptor {
    /// Constructs a new [`GlacierApiVersionInterceptor`] with the given API version.
    pub(crate) fn new(api_version: &'static str) -> Self {
        Self { api_version }
    }
}

impl Interceptor for GlacierApiVersionInterceptor {
    fn modify_before_signing(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        context.request_mut().headers_mut().insert(
            API_VERSION_HEADER,
            HeaderValue::from_static(self.api_version),
        );
        Ok(())
    }
}

/// Adds a glacier tree hash checksum to the HTTP Request
#[derive(Debug, Default)]
pub(crate) struct GlacierTreeHashHeaderInterceptor;

impl Interceptor for GlacierTreeHashHeaderInterceptor {
    fn modify_before_serialization(
        &self,
        _context: &mut BeforeSerializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        // Request the request body to be loaded into memory immediately after serialization
        // so that it can be checksummed before signing and transmit
        cfg.interceptor_state()
            .set_loaded_request_body(LoadedRequestBody::Requested);
        Ok(())
    }

    fn modify_before_retry_loop(
        &self,
        context: &mut BeforeTransmitInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let maybe_loaded_body = cfg.load::<LoadedRequestBody>();
        if let Some(LoadedRequestBody::Loaded(body)) = maybe_loaded_body {
            let content_sha256 = add_checksum_treehash(context.request_mut(), body)?;

            // Override the signing payload with this precomputed hash
            let mut signing_config = cfg
                .load::<SigV4OperationSigningConfig>()
                .ok_or("SigV4OperationSigningConfig not found")?
                .clone();
            signing_config.signing_options.payload_override =
                Some(SignableBody::Precomputed(content_sha256));
            cfg.interceptor_state().store_put(signing_config);
        } else {
            return Err(
                "the request body wasn't loaded into memory before the retry loop, \
                so the Glacier tree hash header can't be computed"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Adds a glacier tree hash checksum to the HTTP Request
///
/// This handles two cases:
/// 1. A body which is retryable: the body will be streamed through a digest calculator, limiting memory usage.
/// 2. A body which is not retryable: the body will be converted into `Bytes`, then streamed through a digest calculator.
///
/// The actual checksum algorithm will first compute a SHA256 checksum for each 1MB chunk. Then, a tree
/// will be assembled, recursively pairing neighboring chunks and computing their combined checksum. The 1 leftover
/// chunk (if it exists) is paired at the end.
///
/// See <https://docs.aws.amazon.com/amazonglacier/latest/dev/checksum-calculations.html> for more information.
fn add_checksum_treehash(
    request: &mut Request<SdkBody>,
    body: &Bytes,
) -> Result<String, byte_stream::error::Error> {
    let (full_body, hashes) = compute_hashes(body, MEGABYTE)?;
    let tree_hash = hex::encode(compute_hash_tree(hashes));
    let complete_hash = hex::encode(full_body);
    if !request.headers().contains_key(TREE_HASH_HEADER) {
        request.headers_mut().insert(
            HeaderName::from_static(TREE_HASH_HEADER),
            tree_hash.parse().expect("hash must be valid header"),
        );
    }
    if !request.headers().contains_key(X_AMZ_CONTENT_SHA256) {
        request.headers_mut().insert(
            HeaderName::from_static(X_AMZ_CONTENT_SHA256),
            complete_hash.parse().expect("hash must be valid header"),
        );
    }
    Ok(complete_hash)
}

const MEGABYTE: usize = 1024 * 1024;
fn compute_hashes(
    body: &Bytes,
    chunk_size: usize,
) -> Result<(Digest, Vec<Digest>), byte_stream::error::Error> {
    let mut hashes = Vec::new();
    let mut full_body = Context::new(&SHA256);
    for chunk in body.chunks(chunk_size) {
        let mut local = Context::new(&SHA256);
        local.update(chunk);
        hashes.push(local.finish());

        full_body.update(chunk);
    }
    if hashes.is_empty() {
        hashes.push(Context::new(&SHA256).finish())
    }
    Ok((full_body.finish(), hashes))
}

/// Compute the glacier tree hash for a vector of hashes.
///
/// Adjacent hashes are combined into a single hash. This process occurs recursively until only 1 hash remains.
///
/// See <https://docs.aws.amazon.com/amazonglacier/latest/dev/checksum-calculations.html> for more information.
fn compute_hash_tree(mut hashes: Vec<Digest>) -> Digest {
    assert!(
        !hashes.is_empty(),
        "even an empty file will produce a digest. this function assumes that hashes is non-empty"
    );
    while hashes.len() > 1 {
        let next = hashes.chunks(2).map(|chunk| match *chunk {
            [left, right] => {
                let mut ctx = Context::new(&SHA256);
                ctx.update(left.as_ref());
                ctx.update(right.as_ref());
                ctx.finish()
            }
            [last] => last,
            _ => unreachable!(),
        });
        hashes = next.collect();
    }
    hashes[0]
}

#[cfg(test)]
mod account_id_autofill_tests {
    use super::*;
    use aws_smithy_runtime_api::client::interceptors::context::InterceptorContext;
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
    use aws_smithy_types::type_erasure::TypedBox;

    #[test]
    fn autofill_account_id() {
        #[derive(Debug)]
        struct SomeInput {
            account_id: Option<String>,
        }
        impl GlacierAccountId for SomeInput {
            fn account_id_mut(&mut self) -> &mut Option<String> {
                &mut self.account_id
            }
        }

        let rc = RuntimeComponentsBuilder::for_tests().build().unwrap();
        let mut cfg = ConfigBag::base();
        let mut context =
            InterceptorContext::new(TypedBox::new(SomeInput { account_id: None }).erase());
        let mut context = BeforeSerializationInterceptorContextMut::from(&mut context);
        let interceptor = GlacierAccountIdAutofillInterceptor::<SomeInput>::new();
        interceptor
            .modify_before_serialization(&mut context, &rc, &mut cfg)
            .expect("success");
        assert_eq!(
            DEFAULT_ACCOUNT_ID,
            context
                .input()
                .downcast_ref::<SomeInput>()
                .unwrap()
                .account_id
                .as_ref()
                .expect("it is set now")
        );
    }
}

#[cfg(test)]
mod api_version_tests {
    use super::*;
    use aws_smithy_runtime_api::client::interceptors::context::InterceptorContext;
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
    use aws_smithy_types::type_erasure::TypedBox;

    #[test]
    fn api_version_interceptor() {
        let rc = RuntimeComponentsBuilder::for_tests().build().unwrap();
        let mut cfg = ConfigBag::base();
        let mut context = InterceptorContext::new(TypedBox::new("dontcare").erase());
        context.set_request(http::Request::builder().body(SdkBody::empty()).unwrap());
        let mut context = BeforeTransmitInterceptorContextMut::from(&mut context);

        let interceptor = GlacierApiVersionInterceptor::new("some-version");
        interceptor
            .modify_before_signing(&mut context, &rc, &mut cfg)
            .expect("success");

        assert_eq!(
            "some-version",
            context
                .request()
                .headers()
                .get(API_VERSION_HEADER)
                .expect("header set")
        );
    }
}

#[cfg(test)]
mod treehash_checksum_tests {
    use super::*;

    #[test]
    fn compute_digests() {
        {
            let body = Bytes::from_static(b"1234");
            let hashes = compute_hashes(&body, 1).expect("succeeds").1;
            assert_eq!(hashes.len(), 4);
        }
        {
            let body = Bytes::from_static(b"1234");
            let hashes = compute_hashes(&body, 2).expect("succeeds").1;
            assert_eq!(hashes.len(), 2);
        }
        {
            let body = Bytes::from_static(b"12345");
            let hashes = compute_hashes(&body, 3).expect("succeeds").1;
            assert_eq!(hashes.len(), 2);
        }
        {
            let body = Bytes::from_static(b"11221122");
            let hashes = compute_hashes(&body, 2).expect("succeeds").1;
            assert_eq!(hashes[0].as_ref(), hashes[2].as_ref());
        }
    }

    #[test]
    fn empty_body_computes_digest() {
        let body = Bytes::from_static(b"");
        let (_, hashes) = compute_hashes(&body, 2).expect("succeeds");
        assert_eq!(hashes.len(), 1);
    }

    #[test]
    fn compute_tree_digest() {
        macro_rules! hash {
            ($($inp:expr),*) => {
                {
                    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
                    $(
                        ctx.update($inp.as_ref());
                    )*
                    ctx.finish()
                }
            }
        }
        let body = Bytes::from_static(b"1234567891011");
        let (complete, hashes) = compute_hashes(&body, 3).expect("succeeds");
        assert_eq!(hashes.len(), 5);
        assert_eq!(complete.as_ref(), hash!("1234567891011").as_ref());
        let final_digest = compute_hash_tree(hashes);
        let expected_digest = hash!(
            hash!(
                hash!(hash!("123"), hash!("456")),
                hash!(hash!("789"), hash!("101"))
            ),
            hash!("1")
        );
        assert_eq!(expected_digest.as_ref(), final_digest.as_ref());
    }

    #[test]
    fn hash_value_test() {
        // the test data consists of an 11 byte sequence, repeated. Since the sequence length is
        // relatively prime with 1 megabyte, we can ensure that chunks will all have different hashes.
        let base_seq = b"01245678912";
        let total_size = MEGABYTE * 101 + 500;
        let mut test_data = vec![];
        while test_data.len() < total_size {
            test_data.extend_from_slice(base_seq)
        }
        let test_data = Bytes::from(test_data);

        let mut http_req = http::Request::builder()
            .uri("http://example.com/hello")
            .body(SdkBody::taken()) // the body isn't used by add_checksum_treehash
            .unwrap();

        add_checksum_treehash(&mut http_req, &test_data).expect("should succeed");
        // hash value verified with AWS CLI
        assert_eq!(
            http_req.headers().get(TREE_HASH_HEADER).unwrap(),
            "3d417484359fc9f5a3bafd576dc47b8b2de2bf2d4fdac5aa2aff768f2210d386"
        );
    }
}
