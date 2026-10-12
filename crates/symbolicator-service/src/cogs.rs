//! COGS (Cost Of Goods Sold) measurements.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Configuration for COGS measurements.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CogsConfig {
    /// Whether COGS measurements are emitted.
    pub enabled: bool,
    /// The COGS resource id of this Symbolicator deployment.
    pub resource_id: String,
}

impl Default for CogsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            resource_id: "symbolicator".into(),
        }
    }
}

/// App feature a COGS measurement is attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFeature {
    SymbolicateNative,
    SymbolicateJs,
    SymbolicateJvm,
    MinidumpStackwalk,
    AppleCrashReport,
    Downloads,
    CacheRefresh,
}

impl AppFeature {
    /// Returns the string representation of this app feature.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SymbolicateNative => "symbolicate_native",
            Self::SymbolicateJs => "symbolicate_js",
            Self::SymbolicateJvm => "symbolicate_jvm",
            Self::MinidumpStackwalk => "minidump_stackwalk",
            Self::AppleCrashReport => "apple_crash_report",
            Self::Downloads => "downloads",
            Self::CacheRefresh => "cache_refresh",
        }
    }
}

static GLOBAL: OnceLock<Cogs> = OnceLock::new();

/// COGS measurement collector.
#[derive(Clone, Debug)]
pub struct Cogs {
    resource_id: Option<Arc<str>>,
}

impl Cogs {
    pub fn new(config: &CogsConfig) -> Self {
        Self {
            resource_id: config.enabled.then(|| config.resource_id.as_str().into()),
        }
    }

    /// Initializes the global [`Cogs`] according to the given config.
    ///
    /// Only the first call has an effect, subsequent calls are ignored.
    pub fn configure_global(config: &CogsConfig) {
        let _ = GLOBAL.set(Self::new(config));
    }

    /// Returns the global [`Cogs`].
    ///
    /// If [`configure`](Self::configure) has not been called, this returns a
    /// disabled instance which records nothing.
    pub fn global() -> Self {
        GLOBAL.get().cloned().unwrap_or(Self { resource_id: None })
    }

    /// Wraps a future, measuring the CPU time spent in its polls.
    ///
    /// The measurement is attributed to `feature` and emitted when the
    /// returned future is dropped.
    pub fn timed<F>(&self, feature: AppFeature, future: F) -> CogsFuture<F>
    where
        F: Future,
    {
        CogsFuture {
            inner: Box::pin(future),
            poll_time: Duration::ZERO,
            feature,
            resource_id: self.resource_id.clone(),
        }
    }
}

/// A future whose poll time is measured and emitted as a COGS measurement on drop.
pub struct CogsFuture<F> {
    inner: Pin<Box<F>>,
    poll_time: Duration,
    feature: AppFeature,
    resource_id: Option<Arc<str>>,
}

impl<F: Future> Future for CogsFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let start = Instant::now();
        let result = this.inner.as_mut().poll(cx);
        this.poll_time += start.elapsed();
        result
    }
}

impl<F> Drop for CogsFuture<F> {
    fn drop(&mut self) {
        let Some(resource_id) = self.resource_id.take() else {
            return;
        };

        let micros: u64 = self.poll_time.as_micros().try_into().unwrap_or(u64::MAX);
        metric!(
            counter("cogs.usage") += micros,
            "resource_id" => resource_id.to_string(),
            "app_feature" => self.feature.as_str(),
            "category" => "default"
        );
    }
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::*;

    fn run_timed(cogs: &Cogs) -> Vec<(String, u64)> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let future = cogs.timed(AppFeature::SymbolicateJs, async {
                std::thread::sleep(Duration::from_millis(5));
            });
            futures::executor::block_on(future);
        });

        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, value)| {
                let key = key.key();
                let labels: Vec<_> = key
                    .labels()
                    .map(|l| format!("{}:{}", l.key(), l.value()))
                    .collect();
                let value = match value {
                    DebugValue::Counter(v) => v,
                    _ => panic!("expected a counter"),
                };
                (format!("{} [{}]", key.name(), labels.join(", ")), value)
            })
            .collect()
    }

    #[test]
    fn test_cogs_emits_usage() {
        let cogs = Cogs::new(&CogsConfig {
            enabled: true,
            resource_id: "symbolicator_test".into(),
        });

        let metrics = run_timed(&cogs);
        assert_eq!(metrics.len(), 1);
        let (key, value) = &metrics[0];
        assert_eq!(
            key,
            "cogs.usage [resource_id:symbolicator_test, app_feature:symbolicate_js, category:default]"
        );
        assert!(*value >= 5_000);
    }

    #[test]
    fn test_cogs_disabled_emits_nothing() {
        let cogs = Cogs::new(&CogsConfig::default());

        let metrics = run_timed(&cogs);
        assert!(metrics.is_empty());
    }
}
