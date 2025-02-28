/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use std::env;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::subscriber::DefaultGuard;
use tracing::Level;
use tracing_subscriber::fmt::TestWriter;

struct Tee<W> {
    buf: Arc<Mutex<Vec<u8>>>,
    quiet: bool,
    inner: W,
}

/// A guard that resets log capturing upon being dropped.
#[derive(Debug)]
pub struct LogCaptureGuard(DefaultGuard);

/// Capture logs from this test.
///
/// The logs will be captured until the `DefaultGuard` is dropped.
///
/// *Why use this instead of traced_test?*
/// This captures _all_ logs, not just logs produced by the current crate.
#[must_use] // log capturing ceases the instant the `DefaultGuard` is dropped
pub fn capture_test_logs() -> (LogCaptureGuard, Rx) {
    // it may be helpful to upstream this at some point
    let (mut writer, rx) = Tee::stdout();
    if env::var("VERBOSE_TEST_LOGS").is_ok() {
        eprintln!("Enabled verbose test logging.");
        writer.loud();
    } else {
        eprintln!("To see full logs from this test set VERBOSE_TEST_LOGS=true");
    }
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(Level::TRACE)
        .with_writer(Mutex::new(writer))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (LogCaptureGuard(guard), rx)
}

pub struct Rx(Arc<Mutex<Vec<u8>>>);
impl Rx {
    pub fn contents(&self) -> String {
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
