/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

// TODO(msrvUpgrade): This can be removed once we upgrade the MSRV to Rust 1.69
#![allow(unknown_lints)]

use self::auth::orchestrate_auth;
use crate::client::orchestrator::endpoints::orchestrate_endpoint;
use crate::client::orchestrator::http::read_body;
use crate::client::timeout::{MaybeTimeout, MaybeTimeoutConfig, TimeoutKind};
use aws_smithy_async::rt::sleep::AsyncSleep;
use aws_smithy_http::body::SdkBody;
use aws_smithy_http::byte_stream::ByteStream;
use aws_smithy_http::result::SdkError;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::connectors::Connector;
use aws_smithy_runtime_api::client::interceptors::context::{
    Error, Input, InterceptorContext, Output, RewindResult,
};
use aws_smithy_runtime_api::client::interceptors::Interceptors;
use aws_smithy_runtime_api::client::orchestrator::{
    DynResponseDeserializer, HttpResponse, LoadedRequestBody, OrchestratorError, RequestSerializer,
    ResponseDeserializer, SharedRequestSerializer,
};
use aws_smithy_runtime_api::client::request_attempts::RequestAttempts;
use aws_smithy_runtime_api::client::retries::{RetryStrategy, ShouldAttempt};
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_runtime_api::client::runtime_plugin::RuntimePlugins;
use aws_smithy_types::config_bag::ConfigBag;
use std::mem;
use tracing::{debug, debug_span, instrument, trace, Instrument};

mod auth;
/// Defines types that implement a trait for endpoint resolution
pub mod endpoints;
mod http;
pub mod interceptors;

macro_rules! halt {
    ([$ctx:ident] => $err:expr) => {{
        debug!("encountered orchestrator error; halting");
        $ctx.fail($err.into());
        return;
    }};
}

macro_rules! halt_on_err {
    ([$ctx:ident] => $expr:expr) => {
        match $expr {
            Ok(ok) => ok,
            Err(err) => halt!([$ctx] => err),
        }
    };
}

macro_rules! continue_on_err {
    ([$ctx:ident] => $expr:expr) => {
        if let Err(err) = $expr {
            debug!(err = ?err, "encountered orchestrator error; continuing");
            $ctx.fail(err.into());
        }
    };
}

macro_rules! run_interceptors {
    (continue_on_err: { $($interceptor:ident($ctx:ident, $rc:ident, $cfg:ident);)+ }) => {
        $(run_interceptors!(continue_on_err: $interceptor($ctx, $rc, $cfg));)+
    };
    (continue_on_err: $interceptor:ident($ctx:ident, $rc:ident, $cfg:ident)) => {
        continue_on_err!([$ctx] => run_interceptors!(__private $interceptor($ctx, $rc, $cfg)))
    };
    (halt_on_err: { $($interceptor:ident($ctx:ident, $rc:ident, $cfg:ident);)+ }) => {
        $(run_interceptors!(halt_on_err: $interceptor($ctx, $rc, $cfg));)+
    };
    (halt_on_err: $interceptor:ident($ctx:ident, $rc:ident, $cfg:ident)) => {
        halt_on_err!([$ctx] => run_interceptors!(__private $interceptor($ctx, $rc, $cfg)))
    };
    (__private $interceptor:ident($ctx:ident, $rc:ident, $cfg:ident)) => {
        Interceptors::new($rc.interceptors()).$interceptor($ctx, $rc, $cfg)
    };
}

pub async fn invoke(
    service_name: &str,
    operation_name: &str,
    input: Input,
    runtime_plugins: &RuntimePlugins,
) -> Result<Output, SdkError<Error, HttpResponse>> {
    invoke_with_stop_point(
        service_name,
        operation_name,
        input,
        runtime_plugins,
        StopPoint::None,
    )
    .await?
    .finalize()
}

/// Allows for returning early at different points during orchestration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPoint {
    /// Don't stop orchestration early
    None,

    /// Stop the orchestrator before transmitting the request
    BeforeTransmit,
}

pub async fn invoke_with_stop_point(
    service_name: &str,
    operation_name: &str,
    input: Input,
    runtime_plugins: &RuntimePlugins,
    stop_point: StopPoint,
) -> Result<InterceptorContext, SdkError<Error, HttpResponse>> {
    async move {
        let mut cfg = ConfigBag::base();
        let cfg = &mut cfg;

        let mut ctx = InterceptorContext::new(input);

        let runtime_components = apply_configuration(&mut ctx, cfg, runtime_plugins)
            .map_err(SdkError::construction_failure)?;
        trace!(runtime_components = ?runtime_components);

        let operation_timeout_config =
            MaybeTimeoutConfig::new(&runtime_components, cfg, TimeoutKind::Operation);
        trace!(operation_timeout_config = ?operation_timeout_config);
        async {
            // If running the pre-execution interceptors failed, then we skip running the op and run the
            // final interceptors instead.
            if !ctx.is_failed() {
                try_op(&mut ctx, cfg, &runtime_components, stop_point).await;
            }
            finally_op(&mut ctx, cfg, &runtime_components).await;
            Ok(ctx)
        }
        .maybe_timeout(operation_timeout_config)
        .await
    }
    .instrument(debug_span!("invoke", service = %service_name, operation = %operation_name))
    .await
}

/// Apply configuration is responsible for apply runtime plugins to the config bag, as well as running
/// `read_before_execution` interceptors. If a failure occurs due to config construction, `invoke`
/// will raise it to the user. If an interceptor fails, then `invoke`
#[instrument(skip_all)]
fn apply_configuration(
    ctx: &mut InterceptorContext,
    cfg: &mut ConfigBag,
    runtime_plugins: &RuntimePlugins,
) -> Result<RuntimeComponents, BoxError> {
    let client_rc_builder = runtime_plugins.apply_client_configuration(cfg)?;
    continue_on_err!([ctx] => Interceptors::new(client_rc_builder.interceptors()).read_before_execution(false, ctx, cfg));

    let operation_rc_builder = runtime_plugins.apply_operation_configuration(cfg)?;
    continue_on_err!([ctx] => Interceptors::new(operation_rc_builder.interceptors()).read_before_execution(true, ctx, cfg));

    // The order below is important. Client interceptors must run before operation interceptors.
    Ok(RuntimeComponents::builder("merged orchestrator components")
        .merge_from(&client_rc_builder)
        .merge_from(&operation_rc_builder)
        .build()?)
}

#[instrument(skip_all)]
async fn try_op(
    ctx: &mut InterceptorContext,
    cfg: &mut ConfigBag,
    runtime_components: &RuntimeComponents,
    stop_point: StopPoint,
) {
    // Before serialization
    run_interceptors!(halt_on_err: {
        read_before_serialization(ctx, runtime_components, cfg);
        modify_before_serialization(ctx, runtime_components, cfg);
    });

    // Serialization
    ctx.enter_serialization_phase();
    {
        let _span = debug_span!("serialization").entered();
        let request_serializer = cfg
            .load::<SharedRequestSerializer>()
            .expect("request serializer must be in the config bag")
            .clone();
        let input = ctx.take_input().expect("input set at this point");
        let request = halt_on_err!([ctx] => request_serializer.serialize_input(input, cfg).map_err(OrchestratorError::other));
        ctx.set_request(request);
    }

    // Load the request body into memory if configured to do so
    if let Some(&LoadedRequestBody::Requested) = cfg.load::<LoadedRequestBody>() {
        debug!("loading request body into memory");
        let mut body = SdkBody::taken();
        mem::swap(&mut body, ctx.request_mut().expect("set above").body_mut());
        let loaded_body = halt_on_err!([ctx] => ByteStream::new(body).collect().await).into_bytes();
        *ctx.request_mut().as_mut().expect("set above").body_mut() =
            SdkBody::from(loaded_body.clone());
        cfg.interceptor_state()
            .store_put(LoadedRequestBody::Loaded(loaded_body));
    }

    // Before transmit
    ctx.enter_before_transmit_phase();
    run_interceptors!(halt_on_err: {
        read_after_serialization(ctx, runtime_components, cfg);
        modify_before_retry_loop(ctx, runtime_components, cfg);
    });

    // If we got a retry strategy from the bag, ask it what to do.
    // Otherwise, assume we should attempt the initial request.
    let should_attempt = runtime_components
        .retry_strategy()
        .should_attempt_initial_request(runtime_components, cfg);
    match should_attempt {
        // Yes, let's make a request
        Ok(ShouldAttempt::Yes) => debug!("retry strategy has OKed initial request"),
        // No, this request shouldn't be sent
        Ok(ShouldAttempt::No) => {
            let err: BoxError = "the retry strategy indicates that an initial request shouldn't be made, but it didn't specify why".into();
            halt!([ctx] => OrchestratorError::other(err));
        }
        // No, we shouldn't make a request because...
        Err(err) => halt!([ctx] => OrchestratorError::other(err)),
        Ok(ShouldAttempt::YesAfterDelay(delay)) => {
            let sleep_impl = halt_on_err!([ctx] => runtime_components.sleep_impl().ok_or_else(|| OrchestratorError::other(
                "the retry strategy requested a delay before sending the initial request, but no 'async sleep' implementation was set"
            )));
            debug!("retry strategy has OKed initial request after a {delay:?} delay");
            sleep_impl.sleep(delay).await;
        }
    }

    // Save a request checkpoint before we make the request. This will allow us to "rewind"
    // the request in the case of retry attempts.
    ctx.save_checkpoint();
    let mut retry_delay = None;
    for i in 1u32.. {
        // Break from the loop if we can't rewind the request's state. This will always succeed the
        // first time, but will fail on subsequent iterations if the request body wasn't retryable.
        trace!("checking if context can be rewound for attempt #{i}");
        if let RewindResult::Impossible = ctx.rewind(cfg) {
            debug!("request cannot be retried since the request body cannot be cloned");
            break;
        }
        // Track which attempt we're currently on.
        cfg.interceptor_state()
            .store_put::<RequestAttempts>(i.into());
        // Backoff time should not be included in the attempt timeout
        if let Some((delay, sleep)) = retry_delay.take() {
            debug!("delaying for {delay:?}");
            sleep.await;
        }
        let attempt_timeout_config =
            MaybeTimeoutConfig::new(runtime_components, cfg, TimeoutKind::OperationAttempt);
        trace!(attempt_timeout_config = ?attempt_timeout_config);
        let maybe_timeout = async {
            debug!("beginning attempt #{i}");
            try_attempt(ctx, cfg, runtime_components, stop_point).await;
            finally_attempt(ctx, cfg, runtime_components).await;
            Result::<_, SdkError<Error, HttpResponse>>::Ok(())
        }
        .maybe_timeout(attempt_timeout_config)
        .await
        .map_err(|err| OrchestratorError::timeout(err.into_source().unwrap()));

        // We continue when encountering a timeout error. The retry classifier will decide what to do with it.
        continue_on_err!([ctx] => maybe_timeout);

        // If we got a retry strategy from the bag, ask it what to do.
        // If no strategy was set, we won't retry.
        let should_attempt = halt_on_err!([ctx] => runtime_components
            .retry_strategy()
            .should_attempt_retry(ctx, runtime_components, cfg)
            .map_err(OrchestratorError::other));
        match should_attempt {
            // Yes, let's retry the request
            ShouldAttempt::Yes => continue,
            // No, this request shouldn't be retried
            ShouldAttempt::No => {
                debug!("a retry is either unnecessary or not possible, exiting attempt loop");
                break;
            }
            ShouldAttempt::YesAfterDelay(delay) => {
                let sleep_impl = halt_on_err!([ctx] => runtime_components.sleep_impl().ok_or_else(|| OrchestratorError::other(
                    "the retry strategy requested a delay before sending the retry request, but no 'async sleep' implementation was set"
                )));
                retry_delay = Some((delay, sleep_impl.sleep(delay)));
                continue;
            }
        }
    }
}

#[instrument(skip_all)]
async fn try_attempt(
    ctx: &mut InterceptorContext,
    cfg: &mut ConfigBag,
    runtime_components: &RuntimeComponents,
    stop_point: StopPoint,
) {
    run_interceptors!(halt_on_err: read_before_attempt(ctx, runtime_components, cfg));

    halt_on_err!([ctx] => orchestrate_endpoint(ctx, runtime_components, cfg).await.map_err(OrchestratorError::other));

    run_interceptors!(halt_on_err: {
        modify_before_signing(ctx, runtime_components, cfg);
        read_before_signing(ctx, runtime_components, cfg);
    });

    halt_on_err!([ctx] => orchestrate_auth(ctx, runtime_components, cfg).await.map_err(OrchestratorError::other));

    run_interceptors!(halt_on_err: {
        read_after_signing(ctx, runtime_components, cfg);
        modify_before_transmit(ctx, runtime_components, cfg);
        read_before_transmit(ctx, runtime_components, cfg);
    });

    // Return early if a stop point is set for before transmit
    if let StopPoint::BeforeTransmit = stop_point {
        debug!("ending orchestration early because the stop point is `BeforeTransmit`");
        return;
    }

    // The connection consumes the request but we need to keep a copy of it
    // within the interceptor context, so we clone it here.
    ctx.enter_transmit_phase();
    let response = halt_on_err!([ctx] => {
        let request = ctx.take_request().expect("set during serialization");
        trace!(request = ?request, "transmitting request");
        let connector = halt_on_err!([ctx] => runtime_components.connector().ok_or_else(||
            OrchestratorError::other("a connector is required to send requests")
        ));
        connector.call(request).await.map_err(|err| {
            match err.downcast() {
                Ok(connector_error) => OrchestratorError::connector(*connector_error),
                Err(box_err) => OrchestratorError::other(box_err)
            }
        })
    });
    trace!(response = ?response, "received response from service");
    ctx.set_response(response);
    ctx.enter_before_deserialization_phase();

    run_interceptors!(halt_on_err: {
        read_after_transmit(ctx, runtime_components, cfg);
        modify_before_deserialization(ctx, runtime_components, cfg);
        read_before_deserialization(ctx, runtime_components, cfg);
    });

    ctx.enter_deserialization_phase();
    let output_or_error = async {
        let response = ctx.response_mut().expect("set during transmit");
        let response_deserializer = cfg
            .load::<DynResponseDeserializer>()
            .expect("a request deserializer must be in the config bag");
        let maybe_deserialized = {
            let _span = debug_span!("deserialize_streaming").entered();
            response_deserializer.deserialize_streaming(response)
        };
        match maybe_deserialized {
            Some(output_or_error) => output_or_error,
            None => read_body(response)
                .instrument(debug_span!("read_body"))
                .await
                .map_err(OrchestratorError::response)
                .and_then(|_| {
                    let _span = debug_span!("deserialize_nonstreaming").entered();
                    response_deserializer.deserialize_nonstreaming(response)
                }),
        }
    }
    .instrument(debug_span!("deserialization"))
    .await;
    trace!(output_or_error = ?output_or_error);
    ctx.set_output_or_error(output_or_error);

    ctx.enter_after_deserialization_phase();
    run_interceptors!(halt_on_err: read_after_deserialization(ctx, runtime_components, cfg));
}

#[instrument(skip_all)]
async fn finally_attempt(
    ctx: &mut InterceptorContext,
    cfg: &mut ConfigBag,
    runtime_components: &RuntimeComponents,
) {
    run_interceptors!(continue_on_err: {
        modify_before_attempt_completion(ctx, runtime_components, cfg);
        read_after_attempt(ctx, runtime_components, cfg);
    });
}

#[instrument(skip_all)]
async fn finally_op(
    ctx: &mut InterceptorContext,
    cfg: &mut ConfigBag,
    runtime_components: &RuntimeComponents,
) {
    run_interceptors!(continue_on_err: {
        modify_before_completion(ctx, runtime_components, cfg);
        read_after_execution(ctx, runtime_components, cfg);
    });
}

#[cfg(all(test, feature = "test-util"))]
mod tests {
    use super::*;
    use crate::client::auth::no_auth::{NoAuthRuntimePlugin, NO_AUTH_SCHEME_ID};
    use crate::client::orchestrator::endpoints::StaticUriEndpointResolver;
    use crate::client::retries::strategy::NeverRetryStrategy;
    use crate::client::test_util::{
        deserializer::CannedResponseDeserializer, serializer::CannedRequestSerializer,
    };
    use ::http::{Request, Response, StatusCode};
    use aws_smithy_runtime_api::client::auth::option_resolver::StaticAuthOptionResolver;
    use aws_smithy_runtime_api::client::auth::{
        AuthOptionResolverParams, SharedAuthOptionResolver,
    };
    use aws_smithy_runtime_api::client::connectors::{Connector, SharedConnector};
    use aws_smithy_runtime_api::client::interceptors::context::{
        AfterDeserializationInterceptorContextRef, BeforeDeserializationInterceptorContextMut,
        BeforeDeserializationInterceptorContextRef, BeforeSerializationInterceptorContextMut,
        BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextMut,
        BeforeTransmitInterceptorContextRef, FinalizerInterceptorContextMut,
        FinalizerInterceptorContextRef,
    };
    use aws_smithy_runtime_api::client::interceptors::{Interceptor, SharedInterceptor};
    use aws_smithy_runtime_api::client::orchestrator::{
        BoxFuture, DynResponseDeserializer, EndpointResolverParams, Future, HttpRequest,
        SharedEndpointResolver, SharedRequestSerializer,
    };
    use aws_smithy_runtime_api::client::retries::SharedRetryStrategy;
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
    use aws_smithy_runtime_api::client::runtime_plugin::{RuntimePlugin, RuntimePlugins};
    use aws_smithy_types::config_bag::{ConfigBag, FrozenLayer, Layer};
    use aws_smithy_types::type_erasure::{TypeErasedBox, TypedBox};
    use std::borrow::Cow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tracing_test::traced_test;

    fn new_request_serializer() -> CannedRequestSerializer {
        CannedRequestSerializer::success(
            Request::builder()
                .body(SdkBody::empty())
                .expect("request is valid"),
        )
    }

    fn new_response_deserializer() -> CannedResponseDeserializer {
        CannedResponseDeserializer::new(
            Response::builder()
                .status(StatusCode::OK)
                .body(SdkBody::empty())
                .map_err(|err| OrchestratorError::other(Box::new(err)))
                .map(|res| Output::new(Box::new(res))),
        )
    }

    #[derive(Debug, Default)]
    struct OkConnector {}

    impl OkConnector {
        fn new() -> Self {
            Self::default()
        }
    }

    impl Connector for OkConnector {
        fn call(&self, _request: HttpRequest) -> BoxFuture<HttpResponse> {
            Box::pin(Future::ready(Ok(::http::Response::builder()
                .status(200)
                .body(SdkBody::empty())
                .expect("OK response is valid"))))
        }
    }

    #[derive(Debug)]
    struct TestOperationRuntimePlugin {
        builder: RuntimeComponentsBuilder,
    }

    impl TestOperationRuntimePlugin {
        fn new() -> Self {
            Self {
                builder: RuntimeComponentsBuilder::new("TestOperationRuntimePlugin")
                    .with_retry_strategy(Some(SharedRetryStrategy::new(NeverRetryStrategy::new())))
                    .with_endpoint_resolver(Some(SharedEndpointResolver::new(
                        StaticUriEndpointResolver::http_localhost(8080),
                    )))
                    .with_connector(Some(SharedConnector::new(OkConnector::new())))
                    .with_auth_option_resolver(Some(SharedAuthOptionResolver::new(
                        StaticAuthOptionResolver::new(vec![NO_AUTH_SCHEME_ID]),
                    ))),
            }
        }
    }

    impl RuntimePlugin for TestOperationRuntimePlugin {
        fn config(&self) -> Option<FrozenLayer> {
            let mut layer = Layer::new("TestOperationRuntimePlugin");
            layer.store_put(AuthOptionResolverParams::new("idontcare"));
            layer.store_put(EndpointResolverParams::new("dontcare"));
            layer.store_put(SharedRequestSerializer::new(new_request_serializer()));
            layer.store_put(DynResponseDeserializer::new(new_response_deserializer()));
            Some(layer.freeze())
        }

        fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
            Cow::Borrowed(&self.builder)
        }
    }

    macro_rules! interceptor_error_handling_test {
        (read_before_execution, $ctx:ty, $expected:expr,) => {
            interceptor_error_handling_test!(__private read_before_execution, $ctx, $expected,);
        };
        ($interceptor:ident, $ctx:ty, $expected:expr) => {
            interceptor_error_handling_test!(__private $interceptor, $ctx, $expected, _rc: &RuntimeComponents,);
        };
        (__private $interceptor:ident, $ctx:ty, $expected:expr, $($rc_arg:tt)*) => {
            #[derive(Debug)]
            struct FailingInterceptorA;
            impl Interceptor for FailingInterceptorA {
                fn $interceptor(
                    &self,
                    _ctx: $ctx,
                    $($rc_arg)*
                    _cfg: &mut ConfigBag,
                ) -> Result<(), BoxError> {
                    tracing::debug!("FailingInterceptorA called!");
                    Err("FailingInterceptorA".into())
                }
            }

            #[derive(Debug)]
            struct FailingInterceptorB;
            impl Interceptor for FailingInterceptorB {
                fn $interceptor(
                    &self,
                    _ctx: $ctx,
                    $($rc_arg)*
                    _cfg: &mut ConfigBag,
                ) -> Result<(), BoxError> {
                    tracing::debug!("FailingInterceptorB called!");
                    Err("FailingInterceptorB".into())
                }
            }

            #[derive(Debug)]
            struct FailingInterceptorC;
            impl Interceptor for FailingInterceptorC {
                fn $interceptor(
                    &self,
                    _ctx: $ctx,
                    $($rc_arg)*
                    _cfg: &mut ConfigBag,
                ) -> Result<(), BoxError> {
                    tracing::debug!("FailingInterceptorC called!");
                    Err("FailingInterceptorC".into())
                }
            }

            #[derive(Debug)]
            struct FailingInterceptorsClientRuntimePlugin(RuntimeComponentsBuilder);
            impl FailingInterceptorsClientRuntimePlugin {
                fn new() -> Self {
                    Self(RuntimeComponentsBuilder::new("test").with_interceptor(SharedInterceptor::new(FailingInterceptorA)))
                }
            }
            impl RuntimePlugin for FailingInterceptorsClientRuntimePlugin {
                fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
                    Cow::Borrowed(&self.0)
                }
            }

            #[derive(Debug)]
            struct FailingInterceptorsOperationRuntimePlugin(RuntimeComponentsBuilder);
            impl FailingInterceptorsOperationRuntimePlugin {
                fn new() -> Self {
                    Self(
                        RuntimeComponentsBuilder::new("test")
                            .with_interceptor(SharedInterceptor::new(FailingInterceptorB))
                            .with_interceptor(SharedInterceptor::new(FailingInterceptorC))
                    )
                }
            }
            impl RuntimePlugin for FailingInterceptorsOperationRuntimePlugin {
                fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
                    Cow::Borrowed(&self.0)
                }
            }

            let input = TypeErasedBox::new(Box::new(()));
            let runtime_plugins = RuntimePlugins::new()
                .with_client_plugin(FailingInterceptorsClientRuntimePlugin::new())
                .with_operation_plugin(TestOperationRuntimePlugin::new())
                .with_operation_plugin(NoAuthRuntimePlugin::new())
                .with_operation_plugin(FailingInterceptorsOperationRuntimePlugin::new());
            let actual = invoke("test", "test", input, &runtime_plugins)
                .await
                .expect_err("should error");
            let actual = format!("{:?}", actual);
            assert_eq!($expected, format!("{:?}", actual));

            assert!(logs_contain("FailingInterceptorA called!"));
            assert!(logs_contain("FailingInterceptorB called!"));
            assert!(logs_contain("FailingInterceptorC called!"));
        };
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_execution_error_handling() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ReadBeforeExecution, source: Some(\"FailingInterceptorC\") } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_execution,
            &BeforeSerializationInterceptorContextRef<'_>,
            expected,
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_serialization_error_handling() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ModifyBeforeSerialization, source: Some(\"FailingInterceptorC\") } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_serialization,
            &mut BeforeSerializationInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_serialization_error_handling() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ReadBeforeSerialization, source: Some(\"FailingInterceptorC\") } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_serialization,
            &BeforeSerializationInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_serialization_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ReadAfterSerialization, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_serialization,
            &BeforeTransmitInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_retry_loop_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeRetryLoop, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_retry_loop,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_attempt_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ReadBeforeAttempt, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_attempt,
            &BeforeTransmitInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_signing_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeSigning, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_signing,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_signing_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ReadBeforeSigning, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_signing,
            &BeforeTransmitInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_signing_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ReadAfterSigning, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_signing,
            &BeforeTransmitInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_transmit_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeTransmit, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_transmit,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_transmit_error_handling() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ReadBeforeTransmit, source: Some(\"FailingInterceptorC\") }, connection: Unknown } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_transmit,
            &BeforeTransmitInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_transmit_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterTransmit, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_transmit,
            &BeforeDeserializationInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_deserialization_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeDeserialization, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_deserialization,
            &mut BeforeDeserializationInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_deserialization_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadBeforeDeserialization, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            read_before_deserialization,
            &BeforeDeserializationInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_deserialization_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterDeserialization, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_deserialization,
            &AfterDeserializationInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_attempt_completion_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_attempt_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterAttempt, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_attempt,
            &FinalizerInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_completion_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_execution_error_handling() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterExecution, source: Some(\"FailingInterceptorC\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_handling_test!(
            read_after_execution,
            &FinalizerInterceptorContextRef<'_>,
            expected
        );
    }

    macro_rules! interceptor_error_redirection_test {
        (read_before_execution, $origin_ctx:ty, $destination_interceptor:ident, $destination_ctx:ty, $expected:expr) => {
            interceptor_error_redirection_test!(__private read_before_execution, $origin_ctx, $destination_interceptor, $destination_ctx, $expected,);
        };
        ($origin_interceptor:ident, $origin_ctx:ty, $destination_interceptor:ident, $destination_ctx:ty, $expected:expr) => {
            interceptor_error_redirection_test!(__private $origin_interceptor, $origin_ctx, $destination_interceptor, $destination_ctx, $expected, _rc: &RuntimeComponents,);
        };
        (__private $origin_interceptor:ident, $origin_ctx:ty, $destination_interceptor:ident, $destination_ctx:ty, $expected:expr, $($rc_arg:tt)*) => {
            #[derive(Debug)]
            struct OriginInterceptor;
            impl Interceptor for OriginInterceptor {
                fn $origin_interceptor(
                    &self,
                    _ctx: $origin_ctx,
                    $($rc_arg)*
                    _cfg: &mut ConfigBag,
                ) -> Result<(), BoxError> {
                    tracing::debug!("OriginInterceptor called!");
                    Err("OriginInterceptor".into())
                }
            }

            #[derive(Debug)]
            struct DestinationInterceptor;
            impl Interceptor for DestinationInterceptor {
                fn $destination_interceptor(
                    &self,
                    _ctx: $destination_ctx,
                    _runtime_components: &RuntimeComponents,
                    _cfg: &mut ConfigBag,
                ) -> Result<(), BoxError> {
                    tracing::debug!("DestinationInterceptor called!");
                    Err("DestinationInterceptor".into())
                }
            }

            #[derive(Debug)]
            struct InterceptorsTestOperationRuntimePlugin(RuntimeComponentsBuilder);
            impl InterceptorsTestOperationRuntimePlugin {
                fn new() -> Self {
                    Self(
                        RuntimeComponentsBuilder::new("test")
                            .with_interceptor(SharedInterceptor::new(OriginInterceptor))
                            .with_interceptor(SharedInterceptor::new(DestinationInterceptor))
                    )
                }
            }
            impl RuntimePlugin for InterceptorsTestOperationRuntimePlugin {
                fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
                    Cow::Borrowed(&self.0)
                }
            }

            let input = TypeErasedBox::new(Box::new(()));
            let runtime_plugins = RuntimePlugins::new()
                .with_operation_plugin(TestOperationRuntimePlugin::new())
                .with_operation_plugin(NoAuthRuntimePlugin::new())
                .with_operation_plugin(InterceptorsTestOperationRuntimePlugin::new());
            let actual = invoke("test", "test", input, &runtime_plugins)
                .await
                .expect_err("should error");
            let actual = format!("{:?}", actual);
            assert_eq!($expected, format!("{:?}", actual));

            assert!(logs_contain("OriginInterceptor called!"));
            assert!(logs_contain("DestinationInterceptor called!"));
        };
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_execution_error_causes_jump_to_modify_before_completion() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"DestinationInterceptor\") } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_execution,
            &BeforeSerializationInterceptorContextRef<'_>,
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_serialization_error_causes_jump_to_modify_before_completion() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"DestinationInterceptor\") } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_serialization,
            &mut BeforeSerializationInterceptorContextMut<'_>,
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_serialization_error_causes_jump_to_modify_before_completion() {
        let expected = r#""ConstructionFailure(ConstructionFailure { source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"DestinationInterceptor\") } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_serialization,
            &BeforeSerializationInterceptorContextRef<'_>,
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_serialization_error_causes_jump_to_modify_before_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            read_after_serialization,
            &BeforeTransmitInterceptorContextRef<'_>,
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_retry_loop_error_causes_jump_to_modify_before_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_retry_loop,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_attempt_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_attempt,
            &BeforeTransmitInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_signing_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_signing,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_signing_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_signing,
            &BeforeTransmitInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_signing_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            read_after_signing,
            &BeforeTransmitInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_transmit_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_transmit,
            &mut BeforeTransmitInterceptorContextMut<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_transmit_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""DispatchFailure(DispatchFailure { source: ConnectorError { kind: Other(None), source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, connection: Unknown } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_transmit,
            &BeforeTransmitInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_transmit_error_causes_jump_to_modify_before_attempt_completion() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            read_after_transmit,
            &BeforeDeserializationInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_deserialization_error_causes_jump_to_modify_before_attempt_completion(
    ) {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_deserialization,
            &mut BeforeDeserializationInterceptorContextMut<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_before_deserialization_error_causes_jump_to_modify_before_attempt_completion(
    ) {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(None), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            read_before_deserialization,
            &BeforeDeserializationInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_read_after_deserialization_error_causes_jump_to_modify_before_attempt_completion()
    {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ModifyBeforeAttemptCompletion, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            read_after_deserialization,
            &AfterDeserializationInterceptorContextRef<'_>,
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_attempt_completion_error_causes_jump_to_read_after_attempt() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterAttempt, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_attempt_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            read_after_attempt,
            &FinalizerInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_modify_before_completion_error_causes_jump_to_read_after_execution() {
        let expected = r#""ResponseError(ResponseError { source: InterceptorError { kind: ReadAfterExecution, source: Some(\"DestinationInterceptor\") }, raw: Response { status: 200, version: HTTP/1.1, headers: {}, body: SdkBody { inner: Once(Some(b\"\")), retryable: true } } })""#.to_string();
        interceptor_error_redirection_test!(
            modify_before_completion,
            &mut FinalizerInterceptorContextMut<'_>,
            read_after_execution,
            &FinalizerInterceptorContextRef<'_>,
            expected
        );
    }

    #[tokio::test]
    async fn test_stop_points() {
        let runtime_plugins = || {
            RuntimePlugins::new()
                .with_operation_plugin(TestOperationRuntimePlugin::new())
                .with_operation_plugin(NoAuthRuntimePlugin::new())
        };

        // StopPoint::None should result in a response getting set since orchestration doesn't stop
        let context = invoke_with_stop_point(
            "test",
            "test",
            TypedBox::new(()).erase(),
            &runtime_plugins(),
            StopPoint::None,
        )
        .await
        .expect("success");
        assert!(context.response().is_some());

        // StopPoint::BeforeTransmit will exit right before sending the request, so there should be no response
        let context = invoke_with_stop_point(
            "test",
            "test",
            TypedBox::new(()).erase(),
            &runtime_plugins(),
            StopPoint::BeforeTransmit,
        )
        .await
        .expect("success");
        assert!(context.response().is_none());
    }

    /// The "finally" interceptors should run upon error when the StopPoint is set to BeforeTransmit
    #[tokio::test]
    async fn test_stop_points_error_handling() {
        #[derive(Debug, Default)]
        struct Inner {
            modify_before_retry_loop_called: AtomicBool,
            modify_before_completion_called: AtomicBool,
            read_after_execution_called: AtomicBool,
        }
        #[derive(Clone, Debug, Default)]
        struct TestInterceptor {
            inner: Arc<Inner>,
        }

        impl Interceptor for TestInterceptor {
            fn modify_before_retry_loop(
                &self,
                _context: &mut BeforeTransmitInterceptorContextMut<'_>,
                _rc: &RuntimeComponents,
                _cfg: &mut ConfigBag,
            ) -> Result<(), BoxError> {
                self.inner
                    .modify_before_retry_loop_called
                    .store(true, Ordering::Relaxed);
                Err("test error".into())
            }

            fn modify_before_completion(
                &self,
                _context: &mut FinalizerInterceptorContextMut<'_>,
                _rc: &RuntimeComponents,
                _cfg: &mut ConfigBag,
            ) -> Result<(), BoxError> {
                self.inner
                    .modify_before_completion_called
                    .store(true, Ordering::Relaxed);
                Ok(())
            }

            fn read_after_execution(
                &self,
                _context: &FinalizerInterceptorContextRef<'_>,
                _rc: &RuntimeComponents,
                _cfg: &mut ConfigBag,
            ) -> Result<(), BoxError> {
                self.inner
                    .read_after_execution_called
                    .store(true, Ordering::Relaxed);
                Ok(())
            }
        }

        #[derive(Debug)]
        struct TestInterceptorRuntimePlugin {
            builder: RuntimeComponentsBuilder,
        }
        impl RuntimePlugin for TestInterceptorRuntimePlugin {
            fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
                Cow::Borrowed(&self.builder)
            }
        }

        let interceptor = TestInterceptor::default();
        let runtime_plugins = || {
            RuntimePlugins::new()
                .with_operation_plugin(TestOperationRuntimePlugin::new())
                .with_operation_plugin(NoAuthRuntimePlugin::new())
                .with_operation_plugin(TestInterceptorRuntimePlugin {
                    builder: RuntimeComponentsBuilder::new("test")
                        .with_interceptor(SharedInterceptor::new(interceptor.clone())),
                })
        };

        // StopPoint::BeforeTransmit will exit right before sending the request, so there should be no response
        let context = invoke_with_stop_point(
            "test",
            "test",
            TypedBox::new(()).erase(),
            &runtime_plugins(),
            StopPoint::BeforeTransmit,
        )
        .await
        .expect("success");
        assert!(context.response().is_none());

        assert!(interceptor
            .inner
            .modify_before_retry_loop_called
            .load(Ordering::Relaxed));
        assert!(interceptor
            .inner
            .modify_before_completion_called
            .load(Ordering::Relaxed));
        assert!(interceptor
            .inner
            .read_after_execution_called
            .load(Ordering::Relaxed));
    }
}
