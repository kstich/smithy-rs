/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use crate::provider_config::ProviderConfig;

use aws_credential_types::provider::{self, ProvideCredentials};
use aws_smithy_async::rt::sleep::{AsyncSleep, Sleep, TokioSleep};
use aws_smithy_client::dvr::{NetworkTraffic, RecordingConnection, ReplayingConnection};
use aws_smithy_client::erase::DynConnector;
use aws_types::os_shim_internal::{Env, Fs};

use serde::Deserialize;

use crate::connector::default_connector;
use aws_smithy_types::error::display::DisplayErrorContext;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt::Debug;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tracing::dispatcher::DefaultGuard;
use tracing::Level;
use tracing_subscriber::fmt::TestWriter;

/// Test case credentials
///
/// Credentials for use in test cases. These implement Serialize/Deserialize and have a
/// non-hidden debug implementation.
#[derive(Deserialize, Debug, Eq, PartialEq)]
struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expiry: Option<u64>,
}

/// Convert real credentials to test credentials
///
/// Comparing equality on real credentials works, but it's a pain because the Debug implementation
/// hides the actual keys
impl From<&aws_credential_types::Credentials> for Credentials {
    fn from(credentials: &aws_credential_types::Credentials) -> Self {
        Self {
            access_key_id: credentials.access_key_id().into(),
            secret_access_key: credentials.secret_access_key().into(),
            session_token: credentials.session_token().map(ToString::to_string),
            expiry: credentials
                .expiry()
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs()),
        }
    }
}

impl From<aws_credential_types::Credentials> for Credentials {
    fn from(credentials: aws_credential_types::Credentials) -> Self {
        (&credentials).into()
    }
}

/// Credentials test environment
///
/// A credentials test environment is a directory containing:
/// - an `fs` directory. This is loaded into the test as if it was mounted at `/`
/// - an `env.json` file containing environment variables
/// - an  `http-traffic.json` file containing an http traffic log from [`dvr`](aws_smithy_client::dvr)
/// - a `test-case.json` file defining the expected output of the test
pub(crate) struct TestEnvironment {
    metadata: Metadata,
    base_dir: PathBuf,
    connector: ReplayingConnection,
    provider_config: ProviderConfig,
}

/// Connector which expects no traffic
pub(crate) fn no_traffic_connector() -> DynConnector {
    DynConnector::new(ReplayingConnection::new(vec![]))
}

#[derive(Debug)]
pub(crate) struct InstantSleep;
impl AsyncSleep for InstantSleep {
    fn sleep(&self, _duration: Duration) -> Sleep {
        Sleep::new(std::future::ready(()))
    }
}

#[derive(Deserialize, Debug)]
pub(crate) enum GenericTestResult<T> {
    Ok(T),
    ErrorContains(String),
}

impl<T> GenericTestResult<T>
where
    T: PartialEq + Debug,
{
    #[track_caller]
    pub(crate) fn assert_matches(&self, result: Result<impl Into<T>, impl Error>) {
        match (result, &self) {
            (Ok(actual), GenericTestResult::Ok(expected)) => {
                assert_eq!(expected, &actual.into(), "incorrect result was returned")
            }
            (Err(err), GenericTestResult::ErrorContains(substr)) => {
                let message = format!("{}", DisplayErrorContext(&err));
                assert!(
                    message.contains(substr),
                    "`{message}` did not contain `{substr}`"
                );
            }
            (Err(actual_error), GenericTestResult::Ok(expected_creds)) => panic!(
                "expected credentials ({:?}) but an error was returned: {}",
                expected_creds,
                DisplayErrorContext(&actual_error)
            ),
            (Ok(creds), GenericTestResult::ErrorContains(substr)) => panic!(
                "expected an error containing: `{}`, but a result was returned: {:?}",
                substr,
                creds.into()
            ),
        }
    }
}

type TestResult = GenericTestResult<Credentials>;

#[derive(Deserialize)]
pub(crate) struct Metadata {
    result: TestResult,
    docs: String,
    name: String,
}

// TODO(enableNewSmithyRuntimeCleanup): Replace Tee, capture_test_logs, and Rx with
// the implementations added to aws_smithy_runtime::test_util::capture_test_logs
struct Tee<W> {
    buf: Arc<Mutex<Vec<u8>>>,
    quiet: bool,
    inner: W,
}

/// Capture logs from this test.
///
/// The logs will be captured until the `DefaultGuard` is dropped.
///
/// *Why use this instead of traced_test?*
/// This captures _all_ logs, not just logs produced by the current crate.
fn capture_test_logs() -> (DefaultGuard, Rx) {
    // it may be helpful to upstream this at some point
    let (mut writer, rx) = Tee::stdout();
    if env::var("VERBOSE_TEST_LOGS").is_ok() {
        writer.loud();
    } else {
        eprintln!("To see full logs from this test set VERBOSE_TEST_LOGS=true");
    }
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(Level::TRACE)
        .with_writer(Mutex::new(writer))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, rx)
}

struct Rx(Arc<Mutex<Vec<u8>>>);
impl Rx {
    pub(crate) fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Tee<TestWriter> {
    fn stdout() -> (Self, Rx) {
        let buf: Arc<Mutex<Vec<u8>>> = Default::default();
        (
            Tee {
                buf: buf.clone(),
                quiet: true,
                inner: TestWriter::new(),
            },
            Rx(buf),
        )
    }
}

impl<W> Tee<W> {
    fn loud(&mut self) {
        self.quiet = false;
    }
}

impl<W> Write for Tee<W>
where
    W: Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(buf);
        if !self.quiet {
            self.inner.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl TestEnvironment {
    pub(crate) async fn from_dir(dir: impl AsRef<Path>) -> Result<TestEnvironment, Box<dyn Error>> {
        let dir = dir.as_ref();
        let env = std::fs::read_to_string(dir.join("env.json"))
            .map_err(|e| format!("failed to load env: {}", e))?;
        let env: HashMap<String, String> =
            serde_json::from_str(&env).map_err(|e| format!("failed to parse env: {}", e))?;
        let env = Env::from(env);
        let fs = Fs::from_test_dir(dir.join("fs"), "/");
        let network_traffic = std::fs::read_to_string(dir.join("http-traffic.json"))
            .map_err(|e| format!("failed to load http traffic: {}", e))?;
        let network_traffic: NetworkTraffic = serde_json::from_str(&network_traffic)?;

        let metadata: Metadata = serde_json::from_str(
            &std::fs::read_to_string(dir.join("test-case.json"))
                .map_err(|e| format!("failed to load test case: {}", e))?,
        )?;
        let connector = ReplayingConnection::new(network_traffic.events().clone());
        let provider_config = ProviderConfig::empty()
            .with_fs(fs.clone())
            .with_env(env.clone())
            .with_http_connector(DynConnector::new(connector.clone()))
            .with_sleep(TokioSleep::new())
            .load_default_region()
            .await;
        Ok(TestEnvironment {
            base_dir: dir.into(),
            metadata,
            connector,
            provider_config,
        })
    }

    pub(crate) fn with_provider_config<F>(mut self, provider_config_builder: F) -> Self
    where
        F: Fn(ProviderConfig) -> ProviderConfig,
    {
        self.provider_config = provider_config_builder(self.provider_config.clone());
        self
    }

    pub(crate) fn provider_config(&self) -> &ProviderConfig {
        &self.provider_config
    }

    #[allow(unused)]
    /// Record a test case from live (remote) HTTPS traffic
    ///
    /// The `default_connector()` from the crate will be used
    pub(crate) async fn execute_from_live_traffic<F, P>(
        &self,
        make_provider: impl Fn(ProviderConfig) -> F,
    ) where
        F: Future<Output = P>,
        P: ProvideCredentials,
    {
        // swap out the connector generated from `http-traffic.json` for a real connector:
        let live_connector =
            default_connector(&Default::default(), self.provider_config.sleep()).unwrap();
        let live_connector = RecordingConnection::new(live_connector);
        let config = self
            .provider_config
            .clone()
            .with_http_connector(DynConnector::new(live_connector.clone()));
        let provider = make_provider(config).await;
        let result = provider.provide_credentials().await;
        std::fs::write(
            self.base_dir.join("http-traffic-recorded.json"),
            serde_json::to_string(&live_connector.network_traffic()).unwrap(),
        )
        .unwrap();
        self.check_results(result);
    }

    #[allow(dead_code)]
    /// Execute the test suite & record a new traffic log
    ///
    /// A connector will be created with the factory, then request traffic will be recorded.
    /// Response are generated from the existing http-traffic.json.
    pub(crate) async fn execute_and_update<F, P>(&self, make_provider: impl Fn(ProviderConfig) -> F)
    where
        F: Future<Output = P>,
        P: ProvideCredentials,
    {
        let recording_connector = RecordingConnection::new(self.connector.clone());
        let config = self
            .provider_config
            .clone()
            .with_http_connector(DynConnector::new(recording_connector.clone()));
        let provider = make_provider(config).await;
        let result = provider.provide_credentials().await;
        std::fs::write(
            self.base_dir.join("http-traffic-recorded.json"),
            serde_json::to_string(&recording_connector.network_traffic()).unwrap(),
        )
        .unwrap();
        self.check_results(result);
    }

    fn log_info(&self) {
        eprintln!("test case: {}. {}", self.metadata.name, self.metadata.docs);
    }

    fn lines_with_secrets<'a>(&'a self, logs: &'a str) -> Vec<&'a str> {
        logs.lines().filter(|l| self.contains_secret(l)).collect()
    }

    fn contains_secret(&self, log_line: &str) -> bool {
        assert!(log_line.lines().count() <= 1);
        match &self.metadata.result {
            // NOTE: we aren't currently erroring if the session token is leaked, that is in the canonical request among other things
            TestResult::Ok(creds) => log_line.contains(&creds.secret_access_key),
            TestResult::ErrorContains(_) => false,
        }
    }

    /// Execute a test case. Failures lead to panics.
    pub(crate) async fn execute<F, P>(&self, make_provider: impl Fn(ProviderConfig) -> F)
    where
        F: Future<Output = P>,
        P: ProvideCredentials,
    {
        let (_guard, rx) = capture_test_logs();
        let provider = make_provider(self.provider_config.clone()).await;
        let result = provider.provide_credentials().await;
        tokio::time::pause();
        self.log_info();
        self.check_results(result);
        // todo: validate bodies
        match self
            .connector
            .clone()
            .validate(
                &["CONTENT-TYPE", "x-aws-ec2-metadata-token"],
                |_expected, _actual| Ok(()),
            )
            .await
        {
            Ok(()) => {}
            Err(e) => panic!("{}", e),
        }
        let contents = rx.contents();
        let leaking_lines = self.lines_with_secrets(&contents);
        assert!(
            leaking_lines.is_empty(),
            "secret was exposed\n{:?}\nSee the following log lines:\n  {}",
            self.metadata.result,
            leaking_lines.join("\n  ")
        )
    }

    #[track_caller]
    fn check_results(&self, result: provider::Result) {
        self.metadata.result.assert_matches(result);
    }
}
