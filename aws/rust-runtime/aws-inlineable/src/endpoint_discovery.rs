/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

//! Maintain a cache of discovered endpoints

use aws_smithy_async::rt::sleep::{AsyncSleep, SharedAsyncSleep};
use aws_smithy_async::time::SharedTimeSource;
use aws_smithy_client::erase::boxclone::BoxFuture;
use aws_smithy_http::endpoint::{ResolveEndpoint, ResolveEndpointError};
use aws_smithy_types::endpoint::Endpoint;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::oneshot::{Receiver, Sender};

/// Endpoint reloader
#[must_use]
pub struct ReloadEndpoint {
    loader: Box<dyn Fn() -> BoxFuture<(Endpoint, SystemTime), ResolveEndpointError> + Send + Sync>,
    endpoint: Arc<Mutex<Option<ExpiringEndpoint>>>,
    error: Arc<Mutex<Option<ResolveEndpointError>>>,
    rx: Receiver<()>,
    sleep: SharedAsyncSleep,
    time: SharedTimeSource,
}

impl Debug for ReloadEndpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadEndpoint").finish()
    }
}

impl ReloadEndpoint {
    /// Reload the endpoint once
    pub async fn reload_once(&self) {
        match (self.loader)().await {
            Ok((endpoint, expiry)) => {
                tracing::debug!("caching resolved endpoint: {:?}", (&endpoint, &expiry));
                *self.endpoint.lock().unwrap() = Some(ExpiringEndpoint { endpoint, expiry })
            }
            Err(err) => *self.error.lock().unwrap() = Some(err),
        }
    }

    /// An infinite loop task that will reload the endpoint
    ///
    /// This task will terminate when the corresponding [`Client`](crate::Client) is dropped.
    pub async fn reload_task(mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(_) | Err(TryRecvError::Closed) => break,
                _ => {}
            }
            self.reload_increment(self.time.now()).await;
            self.sleep.sleep(Duration::from_secs(60)).await;
        }
    }

    async fn reload_increment(&self, now: SystemTime) {
        let should_reload = self
            .endpoint
            .lock()
            .unwrap()
            .as_ref()
            .map(|e| e.is_expired(now))
            .unwrap_or(true);
        if should_reload {
            tracing::debug!("reloading endpoint, previous endpoint was expired");
            self.reload_once().await;
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EndpointCache {
    error: Arc<Mutex<Option<ResolveEndpointError>>>,
    endpoint: Arc<Mutex<Option<ExpiringEndpoint>>>,
    // When the sender is dropped, this allows the reload loop to stop
    _drop_guard: Arc<Sender<()>>,
}

impl<T> ResolveEndpoint<T> for EndpointCache {
    fn resolve_endpoint(&self, _params: &T) -> aws_smithy_http::endpoint::Result {
        self.resolve_endpoint()
    }
}

#[derive(Debug)]
struct ExpiringEndpoint {
    endpoint: Endpoint,
    expiry: SystemTime,
}

impl ExpiringEndpoint {
    fn is_expired(&self, now: SystemTime) -> bool {
        tracing::debug!(expiry = ?self.expiry, now = ?now, delta = ?self.expiry.duration_since(now), "checking expiry status of endpoint");
        match self.expiry.duration_since(now) {
            Err(_) => true,
            Ok(t) => t < Duration::from_secs(120),
        }
    }
}

pub(crate) async fn create_cache<F>(
    loader_fn: impl Fn() -> F + Send + Sync + 'static,
    sleep: SharedAsyncSleep,
    time: SharedTimeSource,
) -> Result<(EndpointCache, ReloadEndpoint), ResolveEndpointError>
where
    F: Future<Output = Result<(Endpoint, SystemTime), ResolveEndpointError>> + Send + 'static,
{
    let error_holder = Arc::new(Mutex::new(None));
    let endpoint_holder = Arc::new(Mutex::new(None));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cache = EndpointCache {
        error: error_holder.clone(),
        endpoint: endpoint_holder.clone(),
        _drop_guard: Arc::new(tx),
    };
    let reloader = ReloadEndpoint {
        loader: Box::new(move || Box::pin((loader_fn)()) as _),
        endpoint: endpoint_holder,
        error: error_holder,
        rx,
        sleep,
        time,
    };
    tracing::debug!("populating initial endpoint discovery cache");
    reloader.reload_once().await;
    // if we didn't successfully get an endpoint, bail out so the client knows
    // configuration failed to work
    cache.resolve_endpoint()?;
    Ok((cache, reloader))
}

impl EndpointCache {
    fn resolve_endpoint(&self) -> aws_smithy_http::endpoint::Result {
        tracing::trace!("resolving endpoint from endpoint discovery cache");
        self.endpoint
            .lock()
            .unwrap()
            .as_ref()
            .map(|e| e.endpoint.clone())
            .ok_or_else(|| {
                self.error
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or_else(|| ResolveEndpointError::message("no endpoint loaded"))
            })
    }
}

#[cfg(test)]
mod test {
    use crate::endpoint_discovery::create_cache;
    use aws_smithy_async::rt::sleep::{SharedAsyncSleep, TokioSleep};
    use aws_smithy_async::test_util::controlled_time_and_sleep;
    use aws_smithy_async::time::{SharedTimeSource, SystemTimeSource};
    use aws_smithy_types::endpoint::Endpoint;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::time::timeout;

    fn check_send_v<T: Send>(t: T) -> T {
        t
    }

    #[tokio::test]
    #[allow(unused_must_use)]
    async fn check_traits() {
        let (cache, reloader) = create_cache(
            || async {
                Ok((
                    Endpoint::builder().url("http://foo.com").build(),
                    SystemTime::now(),
                ))
            },
            SharedAsyncSleep::new(TokioSleep::new()),
            SharedTimeSource::new(SystemTimeSource::new()),
        )
        .await
        .unwrap();
        check_send_v(reloader.reload_task());
        check_send_v(cache);
    }

    #[tokio::test]
    async fn erroring_endpoint_always_reloaded() {
        let expiry = UNIX_EPOCH + Duration::from_secs(123456789);
        let ct = Arc::new(AtomicUsize::new(0));
        let (cache, reloader) = create_cache(
            move || {
                let shared_ct = ct.clone();
                shared_ct.fetch_add(1, Ordering::AcqRel);
                async move {
                    Ok((
                        Endpoint::builder()
                            .url(format!("http://foo.com/{shared_ct:?}"))
                            .build(),
                        expiry,
                    ))
                }
            },
            SharedAsyncSleep::new(TokioSleep::new()),
            SharedTimeSource::new(SystemTimeSource::new()),
        )
        .await
        .expect("returns an endpoint");
        assert_eq!(
            cache.resolve_endpoint().expect("ok").url(),
            "http://foo.com/1"
        );
        // 120 second buffer
        reloader
            .reload_increment(expiry - Duration::from_secs(240))
            .await;
        assert_eq!(
            cache.resolve_endpoint().expect("ok").url(),
            "http://foo.com/1"
        );

        reloader.reload_increment(expiry).await;
        assert_eq!(
            cache.resolve_endpoint().expect("ok").url(),
            "http://foo.com/2"
        );
    }

    #[tokio::test]
    async fn test_advance_of_task() {
        let expiry = UNIX_EPOCH + Duration::from_secs(123456789);
        // expires in 8 minutes
        let (time, sleep, mut gate) = controlled_time_and_sleep(expiry - Duration::from_secs(239));
        let ct = Arc::new(AtomicUsize::new(0));
        let (cache, reloader) = create_cache(
            move || {
                let shared_ct = ct.clone();
                shared_ct.fetch_add(1, Ordering::AcqRel);
                async move {
                    Ok((
                        Endpoint::builder()
                            .url(format!("http://foo.com/{shared_ct:?}"))
                            .build(),
                        expiry,
                    ))
                }
            },
            SharedAsyncSleep::new(sleep.clone()),
            SharedTimeSource::new(time.clone()),
        )
        .await
        .expect("first load success");
        let reload_task = tokio::spawn(reloader.reload_task());
        assert!(!reload_task.is_finished());
        // expiry occurs after 2 sleeps
        // t = 0
        assert_eq!(
            gate.expect_sleep().await.duration(),
            Duration::from_secs(60)
        );
        assert_eq!(cache.resolve_endpoint().unwrap().url(), "http://foo.com/1");
        // t = 60

        let sleep = gate.expect_sleep().await;
        // we're still holding the drop guard, so we haven't expired yet.
        assert_eq!(cache.resolve_endpoint().unwrap().url(), "http://foo.com/1");
        assert_eq!(sleep.duration(), Duration::from_secs(60));
        sleep.allow_progress();
        // t = 120

        let sleep = gate.expect_sleep().await;
        assert_eq!(cache.resolve_endpoint().unwrap().url(), "http://foo.com/2");
        sleep.allow_progress();

        let sleep = gate.expect_sleep().await;
        drop(cache);
        sleep.allow_progress();

        timeout(Duration::from_secs(1), reload_task)
            .await
            .expect("task finishes successfully")
            .expect("finishes");
    }
}
