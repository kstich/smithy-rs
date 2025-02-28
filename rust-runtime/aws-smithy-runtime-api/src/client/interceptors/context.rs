/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

//! Interceptor context.
//!
//! Interceptors have access to varying pieces of context during the course of an operation.
//!
//! An operation is composed of multiple phases. The initial phase is "before serialization", which
//! has the original input as context. The next phase is "before transmit", which has the serialized
//! request as context. Depending on which hook is being called with the dispatch context,
//! the serialized request may or may not be signed (which should be apparent from the hook name).
//! Following the "before transmit" phase is the "before deserialization" phase, which has
//! the raw response available as context. Finally, the "after deserialization" phase
//! has both the raw and parsed response available.
//!
//! To summarize:
//! 1. Before serialization: Only has the operation input.
//! 2. Before transmit: Only has the serialized request.
//! 3. Before deserialization: Has the raw response.
//! 3. After deserialization: Has the raw response and the parsed response.
//!
//! When implementing hooks, if information from a previous phase is required, then implement
//! an earlier hook to examine that context, and save off any necessary information into the
//! [`ConfigBag`] for later hooks to examine.  Interior mutability is **NOT**
//! recommended for storing request-specific information in your interceptor implementation.
//! Use the [`ConfigBag`] instead.

use crate::client::orchestrator::{HttpRequest, HttpResponse, OrchestratorError};
use aws_smithy_http::result::SdkError;
use aws_smithy_types::config_bag::ConfigBag;
use aws_smithy_types::type_erasure::{TypeErasedBox, TypeErasedError};
use phase::Phase;
use std::fmt::Debug;
use std::{fmt, mem};
use tracing::{debug, error, trace};

pub type Input = TypeErasedBox;
pub type Output = TypeErasedBox;
pub type Error = TypeErasedError;
pub type OutputOrError = Result<Output, OrchestratorError<Error>>;

type Request = HttpRequest;
type Response = HttpResponse;

pub use wrappers::{
    AfterDeserializationInterceptorContextRef, BeforeDeserializationInterceptorContextMut,
    BeforeDeserializationInterceptorContextRef, BeforeSerializationInterceptorContextMut,
    BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextMut,
    BeforeTransmitInterceptorContextRef, FinalizerInterceptorContextMut,
    FinalizerInterceptorContextRef,
};

mod wrappers;

/// Operation phases.
pub(crate) mod phase;

/// A container for the data currently available to an interceptor.
///
/// Different context is available based on which phase the operation is currently in. For example,
/// context in the "before serialization" phase won't have a `request` yet since the input hasn't been
/// serialized at that point. But once it gets into the "before transmit" phase, the `request` will be set.
#[derive(Debug)]
pub struct InterceptorContext<I = Input, O = Output, E = Error> {
    pub(crate) input: Option<I>,
    pub(crate) output_or_error: Option<Result<O, OrchestratorError<E>>>,
    pub(crate) request: Option<Request>,
    pub(crate) response: Option<Response>,
    phase: Phase,
    tainted: bool,
    request_checkpoint: Option<HttpRequest>,
}

impl InterceptorContext<Input, Output, Error> {
    /// Creates a new interceptor context in the "before serialization" phase.
    pub fn new(input: Input) -> InterceptorContext<Input, Output, Error> {
        InterceptorContext {
            input: Some(input),
            output_or_error: None,
            request: None,
            response: None,
            phase: Phase::BeforeSerialization,
            tainted: false,
            request_checkpoint: None,
        }
    }
}

impl<I, O, E: Debug> InterceptorContext<I, O, E> {
    /// Decomposes the context into its constituent parts.
    #[doc(hidden)]
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        Option<I>,
        Option<Result<O, OrchestratorError<E>>>,
        Option<Request>,
        Option<Response>,
    ) {
        (
            self.input,
            self.output_or_error,
            self.request,
            self.response,
        )
    }

    pub fn finalize(self) -> Result<O, SdkError<E, HttpResponse>> {
        let Self {
            output_or_error,
            response,
            phase,
            ..
        } = self;
        output_or_error
            .expect("output_or_error must always be set before finalize is called.")
            .map_err(|error| OrchestratorError::into_sdk_error(error, &phase, response))
    }

    /// Retrieve the input for the operation being invoked.
    pub fn input(&self) -> Option<&I> {
        self.input.as_ref()
    }

    /// Retrieve the input for the operation being invoked.
    pub fn input_mut(&mut self) -> Option<&mut I> {
        self.input.as_mut()
    }

    /// Takes ownership of the input.
    pub fn take_input(&mut self) -> Option<I> {
        self.input.take()
    }

    /// Set the request for the operation being invoked.
    pub fn set_request(&mut self, request: Request) {
        self.request = Some(request);
    }

    /// Retrieve the transmittable request for the operation being invoked.
    /// This will only be available once request marshalling has completed.
    pub fn request(&self) -> Option<&Request> {
        self.request.as_ref()
    }

    /// Retrieve the transmittable request for the operation being invoked.
    /// This will only be available once request marshalling has completed.
    pub fn request_mut(&mut self) -> Option<&mut Request> {
        self.request.as_mut()
    }

    /// Takes ownership of the request.
    pub fn take_request(&mut self) -> Option<Request> {
        self.request.take()
    }

    /// Set the response for the operation being invoked.
    pub fn set_response(&mut self, response: Response) {
        self.response = Some(response);
    }

    /// Returns the response.
    pub fn response(&self) -> Option<&Response> {
        self.response.as_ref()
    }

    /// Returns a mutable reference to the response.
    pub fn response_mut(&mut self) -> Option<&mut Response> {
        self.response.as_mut()
    }

    /// Set the output or error for the operation being invoked.
    pub fn set_output_or_error(&mut self, output: Result<O, OrchestratorError<E>>) {
        self.output_or_error = Some(output);
    }

    /// Returns the deserialized output or error.
    pub fn output_or_error(&self) -> Option<Result<&O, &OrchestratorError<E>>> {
        self.output_or_error.as_ref().map(Result::as_ref)
    }

    /// Returns the mutable reference to the deserialized output or error.
    pub fn output_or_error_mut(&mut self) -> Option<&mut Result<O, OrchestratorError<E>>> {
        self.output_or_error.as_mut()
    }

    /// Advance to the Serialization phase.
    #[doc(hidden)]
    pub fn enter_serialization_phase(&mut self) {
        debug!("entering \'serialization\' phase");
        debug_assert!(
            self.phase.is_before_serialization(),
            "called enter_serialization_phase but phase is not before 'serialization'"
        );
        self.phase = Phase::Serialization;
    }

    /// Advance to the BeforeTransmit phase.
    #[doc(hidden)]
    pub fn enter_before_transmit_phase(&mut self) {
        debug!("entering \'before transmit\' phase");
        debug_assert!(
            self.phase.is_serialization(),
            "called enter_before_transmit_phase but phase is not 'serialization'"
        );
        debug_assert!(
            self.input.is_none(),
            "input must be taken before calling enter_before_transmit_phase"
        );
        debug_assert!(
            self.request.is_some(),
            "request must be set before calling enter_before_transmit_phase"
        );
        self.request_checkpoint = try_clone(self.request().expect("checked above"));
        self.phase = Phase::BeforeTransmit;
    }

    /// Advance to the Transmit phase.
    #[doc(hidden)]
    pub fn enter_transmit_phase(&mut self) {
        debug!("entering \'transmit\' phase");
        debug_assert!(
            self.phase.is_before_transmit(),
            "called enter_transmit_phase but phase is not before transmit"
        );
        self.phase = Phase::Transmit;
    }

    /// Advance to the BeforeDeserialization phase.
    #[doc(hidden)]
    pub fn enter_before_deserialization_phase(&mut self) {
        debug!("entering \'before deserialization\' phase");
        debug_assert!(
            self.phase.is_transmit(),
            "called enter_before_deserialization_phase but phase is not 'transmit'"
        );
        debug_assert!(
            self.request.is_none(),
            "request must be taken before entering the 'before deserialization' phase"
        );
        debug_assert!(
            self.response.is_some(),
            "response must be set to before entering the 'before deserialization' phase"
        );
        self.phase = Phase::BeforeDeserialization;
    }

    /// Advance to the Deserialization phase.
    #[doc(hidden)]
    pub fn enter_deserialization_phase(&mut self) {
        debug!("entering \'deserialization\' phase");
        debug_assert!(
            self.phase.is_before_deserialization(),
            "called enter_deserialization_phase but phase is not 'before deserialization'"
        );
        self.phase = Phase::Deserialization;
    }

    /// Advance to the AfterDeserialization phase.
    #[doc(hidden)]
    pub fn enter_after_deserialization_phase(&mut self) {
        debug!("entering \'after deserialization\' phase");
        debug_assert!(
            self.phase.is_deserialization(),
            "called enter_after_deserialization_phase but phase is not 'deserialization'"
        );
        debug_assert!(
            self.output_or_error.is_some(),
            "output must be set to before entering the 'after deserialization' phase"
        );
        self.phase = Phase::AfterDeserialization;
    }

    /// Set the request checkpoint. This should only be called once, right before entering the retry loop.
    #[doc(hidden)]
    pub fn save_checkpoint(&mut self) {
        trace!("saving request checkpoint...");
        self.request_checkpoint = self.request().and_then(try_clone);
        match self.request_checkpoint.as_ref() {
            Some(_) => trace!("successfully saved request checkpoint"),
            None => trace!("failed to save request checkpoint: request body could not be cloned"),
        }
    }

    /// Returns false if rewinding isn't possible
    #[doc(hidden)]
    pub fn rewind(&mut self, _cfg: &mut ConfigBag) -> RewindResult {
        // If request_checkpoint was never set, but we've already made one attempt,
        // then this is not a retryable request
        if self.request_checkpoint.is_none() && self.tainted {
            return RewindResult::Impossible;
        }

        if !self.tainted {
            // The first call to rewind() happens before the request is ever touched, so we don't need
            // to clone it then. However, the request must be marked as tainted so that subsequent calls
            // to rewind() properly reload the saved request checkpoint.
            self.tainted = true;
            return RewindResult::Unnecessary;
        }

        // Otherwise, rewind to the saved request checkpoint
        self.phase = Phase::BeforeTransmit;
        self.request = try_clone(self.request_checkpoint.as_ref().expect("checked above"));
        assert!(
            self.request.is_some(),
            "if the request wasn't cloneable, then we should have already return from this method."
        );
        self.response = None;
        self.output_or_error = None;
        RewindResult::Occurred
    }

    /// Mark this context as failed due to errors during the operation. Any errors already contained
    /// by the context will be replaced by the given error.
    pub fn fail(&mut self, error: OrchestratorError<E>) {
        if !self.is_failed() {
            trace!(
                "orchestrator is transitioning to the 'failure' phase from the '{:?}' phase",
                self.phase
            );
        }
        if let Some(Err(existing_err)) = mem::replace(&mut self.output_or_error, Some(Err(error))) {
            error!("orchestrator context received an error but one was already present; Throwing away previous error: {:?}", existing_err);
        }
    }

    /// Return `true` if this context's `output_or_error` is an error. Otherwise, return `false`.
    pub fn is_failed(&self) -> bool {
        self.output_or_error
            .as_ref()
            .map(Result::is_err)
            .unwrap_or_default()
    }
}

/// The result of attempting to rewind a request.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[doc(hidden)]
pub enum RewindResult {
    /// The request couldn't be rewound because it wasn't cloneable.
    Impossible,
    /// The request wasn't rewound because it was unnecessary.
    Unnecessary,
    /// The request was rewound successfully.
    Occurred,
}

impl fmt::Display for RewindResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RewindResult::Impossible => write!(
                f,
                "The request couldn't be rewound because it wasn't cloneable."
            ),
            RewindResult::Unnecessary => {
                write!(f, "The request wasn't rewound because it was unnecessary.")
            }
            RewindResult::Occurred => write!(f, "The request was rewound successfully."),
        }
    }
}

fn try_clone(request: &HttpRequest) -> Option<HttpRequest> {
    let cloned_body = request.body().try_clone()?;
    let mut cloned_request = ::http::Request::builder()
        .uri(request.uri().clone())
        .method(request.method());
    *cloned_request
        .headers_mut()
        .expect("builder has not been modified, headers must be valid") = request.headers().clone();
    Some(
        cloned_request
            .body(cloned_body)
            .expect("a clone of a valid request should be a valid request"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_http::body::SdkBody;
    use aws_smithy_types::type_erasure::TypedBox;
    use http::header::{AUTHORIZATION, CONTENT_LENGTH};
    use http::{HeaderValue, Uri};

    #[test]
    fn test_success_transitions() {
        let input = TypedBox::new("input".to_string()).erase();
        let output = TypedBox::new("output".to_string()).erase();

        let mut context = InterceptorContext::new(input);
        assert_eq!(
            "input",
            context
                .input()
                .and_then(|i| i.downcast_ref::<String>())
                .unwrap()
        );
        context.input_mut();

        context.enter_serialization_phase();
        let _ = context.take_input();
        context.set_request(http::Request::builder().body(SdkBody::empty()).unwrap());

        context.enter_before_transmit_phase();
        context.request();
        context.request_mut();

        context.enter_transmit_phase();
        let _ = context.take_request();
        context.set_response(http::Response::builder().body(SdkBody::empty()).unwrap());

        context.enter_before_deserialization_phase();
        context.response();
        context.response_mut();

        context.enter_deserialization_phase();
        context.response();
        context.response_mut();
        context.set_output_or_error(Ok(output));

        context.enter_after_deserialization_phase();
        context.response();
        context.response_mut();
        let _ = context.output_or_error();
        let _ = context.output_or_error_mut();

        let output = context.output_or_error.unwrap().expect("success");
        assert_eq!("output", output.downcast_ref::<String>().unwrap());
    }

    #[test]
    fn test_rewind_for_retry() {
        use std::fmt;
        #[derive(Debug)]
        struct Error;
        impl fmt::Display for Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("don't care")
            }
        }
        impl std::error::Error for Error {}

        let mut cfg = ConfigBag::base();
        let input = TypedBox::new("input".to_string()).erase();
        let output = TypedBox::new("output".to_string()).erase();
        let error = TypedBox::new(Error).erase_error();

        let mut context = InterceptorContext::new(input);
        assert_eq!(
            "input",
            context
                .input()
                .and_then(|i| i.downcast_ref::<String>())
                .unwrap()
        );

        context.enter_serialization_phase();
        let _ = context.take_input();
        context.set_request(
            http::Request::builder()
                .header("test", "the-original-un-mutated-request")
                .body(SdkBody::empty())
                .unwrap(),
        );
        context.enter_before_transmit_phase();
        context.save_checkpoint();
        assert_eq!(context.rewind(&mut cfg), RewindResult::Unnecessary);
        // Modify the test header post-checkpoint to simulate modifying the request for signing or a mutating interceptor
        context.request_mut().unwrap().headers_mut().remove("test");
        context.request_mut().unwrap().headers_mut().insert(
            "test",
            HeaderValue::from_static("request-modified-after-signing"),
        );

        context.enter_transmit_phase();
        let request = context.take_request().unwrap();
        assert_eq!(
            "request-modified-after-signing",
            request.headers().get("test").unwrap()
        );
        context.set_response(http::Response::builder().body(SdkBody::empty()).unwrap());

        context.enter_before_deserialization_phase();
        context.enter_deserialization_phase();
        context.set_output_or_error(Err(OrchestratorError::operation(error)));

        assert_eq!(context.rewind(&mut cfg), RewindResult::Occurred);

        // Now after rewinding, the test header should be its original value
        assert_eq!(
            "the-original-un-mutated-request",
            context.request().unwrap().headers().get("test").unwrap()
        );

        context.enter_transmit_phase();
        let _ = context.take_request();
        context.set_response(http::Response::builder().body(SdkBody::empty()).unwrap());

        context.enter_before_deserialization_phase();
        context.enter_deserialization_phase();
        context.set_output_or_error(Ok(output));

        context.enter_after_deserialization_phase();

        let output = context.output_or_error.unwrap().expect("success");
        assert_eq!("output", output.downcast_ref::<String>().unwrap());
    }

    #[test]
    fn try_clone_clones_all_data() {
        let request = ::http::Request::builder()
            .uri(Uri::from_static("https://www.amazon.com"))
            .method("POST")
            .header(CONTENT_LENGTH, 456)
            .header(AUTHORIZATION, "Token: hello")
            .body(SdkBody::from("hello world!"))
            .expect("valid request");
        let cloned = try_clone(&request).expect("request is cloneable");

        assert_eq!(&Uri::from_static("https://www.amazon.com"), cloned.uri());
        assert_eq!("POST", cloned.method());
        assert_eq!(2, cloned.headers().len());
        assert_eq!("Token: hello", cloned.headers().get(AUTHORIZATION).unwrap(),);
        assert_eq!("456", cloned.headers().get(CONTENT_LENGTH).unwrap());
        assert_eq!("hello world!".as_bytes(), cloned.body().bytes().unwrap());
    }
}
