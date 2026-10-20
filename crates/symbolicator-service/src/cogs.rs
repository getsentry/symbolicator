//! COGS (Cost Of Goods Sold) measurements.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// A processing phase, subdividing an [`AppFeature`] measurement.
///
/// Time spent in a categorized scope is attributed to that category instead of
/// `default`. Nested scopes attribute to the innermost category only, so the
/// per-category times of a measurement always sum to its total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Stackwalk,
    Symbolication,
    CacheUse,
    CacheBuild,
}

impl Category {
    const ALL: [Category; 4] = [
        Self::Stackwalk,
        Self::Symbolication,
        Self::CacheUse,
        Self::CacheBuild,
    ];

    /// Returns the string representation of this category.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stackwalk => "stackwalk",
            Self::Symbolication => "symbolication",
            Self::CacheUse => "cache_use",
            Self::CacheBuild => "cache_build",
        }
    }

    fn index(&self) -> usize {
        *self as usize
    }
}

/// Per-category times of one COGS measurement, in nanoseconds.
#[derive(Debug, Default)]
struct CategoryTimes {
    slots: [AtomicU64; Category::ALL.len()],
    attributed: AtomicU64,
}

impl CategoryTimes {
    fn record_slice(&self, category: Category, start: Instant, attributed_before: u64) {
        let gross = start.elapsed().as_nanos() as u64;
        let inner = self
            .attributed
            .load(Ordering::Relaxed)
            .saturating_sub(attributed_before);
        let net = gross.saturating_sub(inner);
        self.slots[category.index()].fetch_add(net, Ordering::Relaxed);
        self.attributed
            .store(attributed_before.saturating_add(gross), Ordering::Relaxed);
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<CategoryTimes>>> = const { RefCell::new(None) };
}

/// Runs a closure, attributing its runtime to `category`.
///
/// This is a no-op unless called from within a future measured by [`Cogs::timed`].
pub fn categorized_sync<T>(category: Category, f: impl FnOnce() -> T) -> T {
    let Some(times) = CURRENT.with(|c| c.borrow().clone()) else {
        return f();
    };

    let attributed_before = times.attributed.load(Ordering::Relaxed);
    let start = Instant::now();
    let result = f();
    times.record_slice(category, start, attributed_before);
    result
}

/// Wraps a future, attributing the CPU time spent in its polls to `category`.
///
/// This is a no-op unless the returned future is polled from within a future
/// measured by [`Cogs::timed`].
pub fn categorized<F>(category: Category, future: F) -> Categorized<F>
where
    F: Future,
{
    Categorized {
        inner: Box::pin(future),
        category,
    }
}

/// A future whose poll time is attributed to a [`Category`].
pub struct Categorized<F> {
    inner: Pin<Box<F>>,
    category: Category,
}

impl<F: Future> Future for Categorized<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(times) = CURRENT.with(|c| c.borrow().clone()) else {
            return this.inner.as_mut().poll(cx);
        };

        let attributed_before = times.attributed.load(Ordering::Relaxed);
        let start = Instant::now();
        let result = this.inner.as_mut().poll(cx);
        times.record_slice(this.category, start, attributed_before);
        result
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
    /// If [`configure_global`](Self::configure_global) has not been called, this returns a
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
            categories: Arc::new(CategoryTimes::default()),
            feature,
            resource_id: self.resource_id.clone(),
        }
    }
}

/// A future whose poll time is measured and emitted as a COGS measurement on drop.
pub struct CogsFuture<F> {
    inner: Pin<Box<F>>,
    poll_time: Duration,
    categories: Arc<CategoryTimes>,
    feature: AppFeature,
    resource_id: Option<Arc<str>>,
}

impl<F: Future> Future for CogsFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let prev = CURRENT.with(|c| c.replace(Some(this.categories.clone())));
        let start = Instant::now();
        let result = this.inner.as_mut().poll(cx);
        this.poll_time += start.elapsed();
        CURRENT.with(|c| *c.borrow_mut() = prev);
        result
    }
}

impl<F> Drop for CogsFuture<F> {
    fn drop(&mut self) {
        let Some(resource_id) = self.resource_id.take() else {
            return;
        };
        let resource_id = resource_id.to_string();

        let mut attributed = Duration::ZERO;
        for category in Category::ALL {
            let nanos = self.categories.slots[category.index()].load(Ordering::Relaxed);
            if nanos == 0 {
                continue;
            }
            attributed += Duration::from_nanos(nanos);
            metric!(
                counter("cogs.usage") += nanos / 1_000,
                "resource_id" => resource_id.clone(),
                "app_feature" => self.feature.as_str(),
                "category" => category.as_str()
            );
        }

        let default = self.poll_time.saturating_sub(attributed);
        let micros: u64 = default.as_micros().try_into().unwrap_or(u64::MAX);
        metric!(
            counter("cogs.usage") += micros,
            "resource_id" => resource_id,
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

    #[test]
    fn test_cogs_categories() {
        let cogs = Cogs::new(&CogsConfig {
            enabled: true,
            resource_id: "symbolicator_test".into(),
        });

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let future = cogs.timed(AppFeature::MinidumpStackwalk, async {
                std::thread::sleep(Duration::from_millis(5));
                categorized(Category::Stackwalk, async {
                    std::thread::sleep(Duration::from_millis(5));
                    categorized_sync(Category::CacheBuild, || {
                        std::thread::sleep(Duration::from_millis(5));
                    });
                })
                .await;
            });
            futures::executor::block_on(future);
        });

        let mut by_category: std::collections::BTreeMap<String, u64> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, value)| {
                let key = key.key();
                let category = key
                    .labels()
                    .find(|l| l.key() == "category")
                    .unwrap()
                    .value()
                    .to_owned();
                let value = match value {
                    DebugValue::Counter(v) => v,
                    _ => panic!("expected a counter"),
                };
                (category, value)
            })
            .collect();

        let stackwalk = by_category.remove("stackwalk").unwrap();
        let cache_build = by_category.remove("cache_build").unwrap();
        let default = by_category.remove("default").unwrap();
        assert!(by_category.is_empty());

        assert!((5_000..10_000).contains(&stackwalk));
        assert!((5_000..10_000).contains(&cache_build));
        assert!((5_000..10_000).contains(&default));
    }
}
