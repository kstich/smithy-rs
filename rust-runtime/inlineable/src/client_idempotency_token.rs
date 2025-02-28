/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use crate::idempotency_token::IdempotencyTokenProvider;
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::context::{
    BeforeSerializationInterceptorContextMut, Input,
};
use aws_smithy_runtime_api::client::interceptors::{Interceptor, SharedInterceptor};
use aws_smithy_runtime_api::client::runtime_components::{
    RuntimeComponents, RuntimeComponentsBuilder,
};
use aws_smithy_runtime_api::client::runtime_plugin::RuntimePlugin;
use aws_smithy_types::config_bag::ConfigBag;
use std::borrow::Cow;
use std::fmt;

#[derive(Debug)]
pub(crate) struct IdempotencyTokenRuntimePlugin {
    runtime_components: RuntimeComponentsBuilder,
}

impl IdempotencyTokenRuntimePlugin {
    pub(crate) fn new<S>(set_token: S) -> Self
    where
        S: Fn(IdempotencyTokenProvider, &mut Input) + Send + Sync + 'static,
    {
        Self {
            runtime_components: RuntimeComponentsBuilder::new("IdempotencyTokenRuntimePlugin")
                .with_interceptor(SharedInterceptor::new(IdempotencyTokenInterceptor {
                    set_token,
                })),
        }
    }
}

impl RuntimePlugin for IdempotencyTokenRuntimePlugin {
    fn runtime_components(&self) -> Cow<'_, RuntimeComponentsBuilder> {
        Cow::Borrowed(&self.runtime_components)
    }
}

struct IdempotencyTokenInterceptor<S> {
    set_token: S,
}

impl<S> fmt::Debug for IdempotencyTokenInterceptor<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdempotencyTokenInterceptor").finish()
    }
}

impl<S> Interceptor for IdempotencyTokenInterceptor<S>
where
    S: Fn(IdempotencyTokenProvider, &mut Input) + Send + Sync,
{
    fn modify_before_serialization(
        &self,
        context: &mut BeforeSerializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let token_provider = cfg
            .load::<IdempotencyTokenProvider>()
            .expect("the idempotency provider must be set")
            .clone();
        (self.set_token)(token_provider, context.input_mut());
        Ok(())
    }
}
