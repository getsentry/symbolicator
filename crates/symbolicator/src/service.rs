//! This provides a wrapper around the main [`SymbolicationActor`] offering a Request/Response model.
//!
//! The request/response model works like this:
//! - A Symbolication request is created using `symbolicate_stacktraces` or a similar method. This
//!   function immediate returns a [`RequestId`].
//! - This [`RequestId`] can later be polled using `get_response` and an optional timeout.
//!
//! The [`RequestService`] requires access to two separate runtimes:
//! When a request comes in on the web pool, it is handed off to the `cpu_pool` for processing, which
//! is primarily synchronous work in the best case (everything is cached).
//! When file fetching is needed, that fetching will happen on the `io_pool`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future;
use futures::{channel::oneshot, FutureExt as _};
use sentry::protocol::SessionStatus;
use sentry::SentryFutureExt;
use serde::{Deserialize, Deserializer, Serialize};
use symbolicator_js::interface::{CompletedJsSymbolicationResponse, SymbolicateJsStacktraces};
use symbolicator_js::SourceMapService;
use symbolicator_native::interface::{CompletedSymbolicationResponse, SymbolicateStacktraces};
use symbolicator_native::SymbolicationActor;
use symbolicator_proguard::interface::{
    CompletedJvmSymbolicationResponse, SymbolicateJvmStacktraces,
};
use symbolicator_proguard::ProguardService;
use symbolicator_service::caching::CacheEntry;
use symbolicator_service::config::Config;
use symbolicator_service::metric;
use symbolicator_service::objects::ObjectsActor;
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Platform;
use symbolicator_service::utils::futures::CallOnDrop;
use symbolicator_service::utils::futures::{m, measure};
use symbolicator_sources::SourceConfig;
use tempfile::TempPath;
use uuid::Uuid;

pub use symbolicator_service::objects::{
    FindObject, FindResult, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
};
pub use symbolicator_service::types::{Scope, ScrapingConfig};

/// Symbolication task identifier.
#[derive(Debug, Clone, Copy, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Creates a new symbolication task identifier.
    pub fn new(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let uuid = Uuid::deserialize(deserializer);
        Ok(Self(uuid.unwrap_or_default()))
    }
}

/// The response of a symbolication request or poll request.
///
/// This object is the main type containing the symblicated crash as returned by the
/// `/minidump`, `/symbolicate` and `/applecrashreport` endpoints.
///
/// This is primarily a wrapper around [`CompletedSymbolicationResponse`] which is publicly
/// documented at <https://getsentry.github.io/symbolicator/api/response/>.
///
/// For the actual HTTP response this is further wrapped to also allow a pending or failed state etc
/// instead of a result.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SymbolicationResponse {
    /// Symbolication is still running.
    Pending {
        /// The id with which further updates can be polled.
        request_id: RequestId,
        /// An indication when the next poll would be suitable.
        retry_after: usize,
    },
    Completed(Box<CompletedResponse>),
    Failed {
        message: String,
    },
    Timeout,
    InternalError,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
/// A completed symbolication response.
pub enum CompletedResponse {
    /// A native symbolication response.
    Native(CompletedSymbolicationResponse),
    /// A JS symbolication response.
    Js(CompletedJsSymbolicationResponse),
    /// A JVM symbolication response.
    Jvm(CompletedJvmSymbolicationResponse),
}

/// Common options for all symbolication API requests.
///
/// These options control some features which control the symbolication and general request
/// handling behaviour.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestOptions {
    /// Whether to return detailed information on DIF object candidates.
    ///
    /// Symbolication requires DIF object files and which ones selected and not selected
    /// influences the quality of symbolication.  Enabling this will return extra
    /// information in the modules list section of the response detailing all DIF objects
    /// considered, any problems with them and what they were used for.  See the
    /// [`ObjectCandidate`](symbolicator_service::objects::ObjectCandidate) struct
    /// for which extra information is returned for DIF objects.
    #[serde(default)]
    pub dif_candidates: bool,

    /// Whether to apply source context for the stack frames.
    #[serde(default = "default_apply_source_context")]
    pub apply_source_context: bool,
}

fn default_apply_source_context() -> bool {
    true
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            dif_candidates: false,
            apply_source_context: true,
        }
    }
}

/// Clears out all the information about the DIF object candidates in the modules list.
///
/// This will avoid this from being serialised as the DIF object candidates list is not
/// serialised when it is empty.
fn clear_dif_candidates(response: &mut CompletedSymbolicationResponse) {
    for module in response.modules.iter_mut() {
        module.candidates.clear()
    }
}
/// The underlying service for the HTTP request handlers.
#[derive(Clone)]
pub struct RequestService {
    inner: Arc<RequestServiceInner>,
}

// We want a shared future here because otherwise polling for a response would hold the global lock.
type ComputationChannel = future::Shared<oneshot::Receiver<(Instant, SymbolicationResponse)>>;

type ComputationMap = Arc<Mutex<BTreeMap<RequestId, ComputationChannel>>>;

struct RequestServiceInner {
    config: Config,

    native: SymbolicationActor,
    js: SourceMapService,
    jvm: ProguardService,
    objects: ObjectsActor,

    cpu_pool: tokio::runtime::Handle,
    requests: ComputationMap,
    max_concurrent_requests: Option<usize>,
    current_requests: Arc<AtomicUsize>,
    symbolication_taskmon: tokio_metrics::TaskMonitor,
}

impl RequestService {
    /// Creates a new [`RequestService`].
    pub fn create(
        mut config: Config,
        io_pool: tokio::runtime::Handle,
        cpu_pool: tokio::runtime::Handle,
    ) -> Result<Self> {
        // FIXME(swatinem):
        // The Sentry<->Symbolicator tests currently rely on the fact that the Sentry Downloader cache
        // is deactivated depending on the file system cache directory:
        if config.cache_dir.is_none() {
            config.caches.in_memory.sentry_index_ttl = Duration::ZERO;
        }

        let shared_services = SharedServices::new(config, io_pool.clone())?;
        let native = SymbolicationActor::new(&shared_services);
        let js = SourceMapService::new(&shared_services);
        let jvm = ProguardService::new(&shared_services);
        let SharedServices {
            objects, config, ..
        } = shared_services;

        let symbolication_taskmon = tokio_metrics::TaskMonitor::new();
        {
            let symbolication_taskmon = symbolication_taskmon.clone();
            io_pool.spawn(async move {
                for interval in symbolication_taskmon.intervals() {
                    record_task_metrics("symbolication", &interval);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }

        let max_concurrent_requests = config.max_concurrent_requests;

        let inner = RequestServiceInner {
            config,

            native,
            js,
            jvm,
            objects,

            cpu_pool,
            requests: Arc::new(Mutex::new(BTreeMap::new())),
            max_concurrent_requests,
            current_requests: Arc::new(AtomicUsize::new(0)),
            symbolication_taskmon,
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Gives access to the [`Config`].
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Looks up the object according to the [`FindObject`] request.
    pub async fn find_object(&self, request: FindObject) -> FindResult {
        self.inner.objects.find(request).await
    }

    /// Fetches the object given by the [`ObjectMetaHandle`].
    pub async fn fetch_object(
        &self,
        handle: Arc<ObjectMetaHandle>,
    ) -> CacheEntry<Arc<ObjectHandle>> {
        self.inner.objects.fetch(handle).await
    }

    /// Creates a new request to symbolicate stacktraces.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn symbolicate_stacktraces(
        &self,
        request: SymbolicateStacktraces,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        self.create_symbolication_request("symbolicate", options, async move {
            slf.native
                .symbolicate(request)
                .await
                .map(CompletedResponse::Native)
        })
    }

    pub fn symbolicate_js_stacktraces(
        &self,
        request: SymbolicateJsStacktraces,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        self.create_symbolication_request("symbolicate_js", RequestOptions::default(), async move {
            Ok(CompletedResponse::Js(slf.js.symbolicate_js(request).await))
        })
    }

    pub fn symbolicate_jvm_stacktraces(
        &self,
        request: SymbolicateJvmStacktraces,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        self.create_symbolication_request(
            "symbolicate_jvm",
            RequestOptions::default(),
            async move {
                Ok(CompletedResponse::Jvm(
                    slf.jvm.symbolicate_jvm(request).await,
                ))
            },
        )
    }

    /// Creates a new request to process a minidump.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn process_minidump(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        self.create_symbolication_request("minidump_stackwalk", options, async move {
            slf.native
                .process_minidump(platform, scope, minidump_file, sources, scraping)
                .await
                .map(CompletedResponse::Native)
        })
    }

    /// Creates a new request to process an Apple crash report.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn process_apple_crash_report(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        apple_crash_report: File,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        self.create_symbolication_request("parse_apple_crash_report", options, async move {
            slf.native
                .process_apple_crash_report(platform, scope, apple_crash_report, sources, scraping)
                .await
                .map(CompletedResponse::Native)
        })
    }

    /// Polls the status for a started symbolication task.
    ///
    /// If the timeout is set and no result is ready within the given time,
    /// [`SymbolicationResponse::Pending`] is returned.
    pub async fn get_response(
        &self,
        request_id: RequestId,
        timeout: Option<u64>,
    ) -> Option<SymbolicationResponse> {
        let channel_opt = self
            .inner
            .requests
            .lock()
            .unwrap()
            .get(&request_id)
            .cloned();
        match channel_opt {
            Some(channel) => Some(wrap_response_channel(request_id, timeout, channel).await),
            None => {
                // This is okay to occur during deploys, but if it happens all the time we have a state
                // bug somewhere. Could be a misconfigured load balancer (supposed to be pinned to
                // scopes).
                metric!(counter("symbolication.request_id_unknown") += 1);
                None
            }
        }
    }

    /// Creates a new request to compute the given future.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    fn create_symbolication_request<F>(
        &self,
        task_name: &'static str,
        options: RequestOptions,
        f: F,
    ) -> Result<RequestId, MaxRequestsError>
    where
        F: Future<Output = Result<CompletedResponse>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let hub = Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));

        // Assume that there are no UUID4 collisions in practice.
        let requests = Arc::clone(&self.inner.requests);
        let current_requests = Arc::clone(&self.inner.current_requests);

        let num_requests = current_requests.load(Ordering::Relaxed);
        metric!(gauge("requests.in_flight") = num_requests as u64);

        // Reject the request if `requests` already contains `max_concurrent_requests` elements.
        if let Some(max_concurrent_requests) = self.inner.max_concurrent_requests {
            if num_requests >= max_concurrent_requests {
                metric!(counter("requests.rejected") += 1);
                return Err(MaxRequestsError);
            }
        }

        // Using `task_name` as the tag should be fine, there is only a small
        // fixed number of them.
        metric!(counter("requests.accepted") += 1, "task" => task_name);

        let request_id = RequestId::new(uuid::Uuid::new_v4());
        requests
            .lock()
            .unwrap()
            .insert(request_id, receiver.shared());
        current_requests.fetch_add(1, Ordering::Relaxed);
        let drop_hub = hub.clone();
        let token = CallOnDrop::new(move || {
            requests.lock().unwrap().remove(&request_id);
            // we consider every premature drop of the future as fatal crash, which works fine
            // since ending a session consumes it and its not possible to double-end.
            drop_hub.end_session_with_status(SessionStatus::Crashed);
        });

        let spawn_time = Instant::now();
        let request_future = async move {
            metric!(timer("symbolication.create_request.first_poll") = spawn_time.elapsed());

            let span = sentry::configure_scope(|scope| scope.get_span());
            let ctx = sentry::TransactionContext::continue_from_span(task_name, task_name, span);
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let transaction_guard = CallOnDrop::new(move || {
                transaction.finish();
            });

            // The "normal" maximum for Native and JS Symbolication is ~5 minutes,
            // and ~10 minutes for minidump processing. Going for a hard timeout of 15 minutes
            // sounds reasonable as we want to support as many events as possible. We might tighten
            // up this timeout even further in the future.
            let timeout = Duration::from_secs(15 * 60);
            let f = tokio::time::timeout(timeout, f);
            let f = measure(task_name, m::timed_result, f);
            let response = f.await;
            drop(transaction_guard);

            let response = match response {
                Ok(Ok(mut response)) => {
                    if !options.dif_candidates {
                        if let CompletedResponse::Native(ref mut res) = response {
                            clear_dif_candidates(res)
                        }
                    }
                    sentry::end_session_with_status(SessionStatus::Exited);
                    SymbolicationResponse::Completed(Box::new(response))
                }
                Ok(Err(err)) => {
                    // NOTE: We could use `capture_anyhow` here, which would correctly resolve any
                    // stack trace thats attached to the error. However these stack traces are not
                    // that useful in practice, and if these events have a stack trace, they will
                    // group the same depending on stack trace, whereas without a stack trace, they
                    // group by the chained errors, which is much better in our case.
                    let error: &dyn std::error::Error = err.as_ref();
                    tracing::error!(error, "Symbolication failed");

                    sentry::end_session_with_status(SessionStatus::Crashed);
                    SymbolicationResponse::Failed {
                        message: err.to_string(),
                    }
                }
                Err(_) => {
                    tracing::error!("Symbolication timed out after {timeout:?}");
                    // a timeout is an abnormal session exit, all other errors are considered "crashed"
                    sentry::end_session_with_status(SessionStatus::Abnormal);
                    SymbolicationResponse::Timeout
                }
            };

            sender.send((Instant::now(), response)).ok();

            // We stop counting the request as an in-flight request at this point, even though
            // it will stay in the `requests` map for another 90s.
            current_requests.fetch_sub(1, Ordering::Relaxed);

            // Using `task_name` as the tag should be fine, there is only a small
            // fixed number of them.
            metric!(counter("requests.processed") += 1, "task" => task_name);

            // Wait before removing the channel from the computation map to allow clients to
            // poll the status.
            tokio::time::sleep(MAX_POLL_DELAY).await;

            drop(token);
        }
        .bind_hub(hub);

        self.inner
            .cpu_pool
            .spawn(self.inner.symbolication_taskmon.instrument(request_future));

        Ok(request_id)
    }
}

/// The maximum delay we allow for polling a finished request before dropping it.
const MAX_POLL_DELAY: Duration = Duration::from_secs(90);

/// An error returned when symbolicator receives a request while already processing
/// the maximum number of requests.
#[derive(Debug, Clone, thiserror::Error)]
#[error("maximum number of concurrent requests reached")]
pub struct MaxRequestsError;

async fn wrap_response_channel(
    request_id: RequestId,
    timeout: Option<u64>,
    channel: ComputationChannel,
) -> SymbolicationResponse {
    let channel_result = if let Some(timeout) = timeout {
        match tokio::time::timeout(Duration::from_secs(timeout), channel).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                return SymbolicationResponse::Pending {
                    request_id,
                    // We should estimate this better, but at some point the
                    // architecture will probably change to pushing results on a
                    // queue instead of polling so it's unlikely we'll ever do
                    // better here.
                    retry_after: 30,
                };
            }
        }
    } else {
        channel.await
    };

    match channel_result {
        Ok((finished_at, response)) => {
            metric!(timer("requests.response_idling") = finished_at.elapsed());
            response
        }
        // If the sender is dropped, this is likely due to a panic that is captured at the source.
        // Therefore, we do not need to capture an error at this point.
        Err(_canceled) => SymbolicationResponse::InternalError,
    }
}

trait ToMaxingI64: TryInto<i64> + Copy {
    fn to_maxing_i64(self) -> i64 {
        self.try_into().unwrap_or(i64::MAX)
    }
}

impl<T: TryInto<i64> + Copy> ToMaxingI64 for T {}

pub fn record_task_metrics(name: &str, metrics: &tokio_metrics::TaskMetrics) {
    metric!(counter("tasks.instrumented_count") += metrics.instrumented_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.dropped_count") += metrics.dropped_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.first_poll_count") += metrics.first_poll_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_first_poll_delay") += metrics.total_first_poll_delay.as_millis().to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_idled_count") += metrics.total_idled_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_idle_duration") += metrics.total_idle_duration.as_millis().to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_scheduled_count") += metrics.total_scheduled_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_scheduled_duration") += metrics.total_scheduled_duration.as_millis().to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_poll_count") += metrics.total_poll_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_poll_duration") += metrics.total_poll_duration.as_millis().to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_fast_poll_count") += metrics.total_fast_poll_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_fast_poll_durations") += metrics.total_fast_poll_duration.as_millis().to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_slow_poll_count") += metrics.total_slow_poll_count.to_maxing_i64(), "taskname" => name);
    metric!(counter("tasks.total_slow_poll_duration") += metrics.total_slow_poll_duration.as_millis().to_maxing_i64(), "taskname" => name);
}

#[cfg(test)]
mod tests {
    use symbolicator_native::interface::{
        CompleteObjectInfo, RawFrame, RawStacktrace, StacktraceOrigin,
    };
    use symbolicator_service::types::RawObjectInfo;
    use symbolicator_service::utils::hex::HexValue;
    use symbolicator_sources::ObjectType;

    use crate::test;

    use super::*;

    #[tokio::test]
    async fn test_get_response_multi() {
        // Make sure we can repeatedly poll for the response
        let config = Config::default();
        let handle = tokio::runtime::Handle::current();
        let service = RequestService::create(config, handle.clone(), handle).unwrap();

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x8c",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )
        .unwrap();

        let request = SymbolicateStacktraces {
            platform: None,
            modules: Vec::new(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([]),
            scope: Default::default(),
            apply_source_context: true,
            scraping: Default::default(),
        };

        let request_id = service
            .symbolicate_stacktraces(request, RequestOptions::default())
            .unwrap();

        for _ in 0..2 {
            let response = service.get_response(request_id, None).await.unwrap();

            assert!(
                matches!(&response, SymbolicationResponse::Completed(_)),
                "Not a complete response: {response:#?}"
            );
        }
    }

    fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
        SymbolicateStacktraces {
            platform: None,
            scope: Scope::Global,
            signal: None,
            sources: Arc::from(sources),
            origin: StacktraceOrigin::Symbolicate,
            stacktraces: vec![RawStacktrace {
                frames: vec![RawFrame {
                    instruction_addr: HexValue(0x1_0000_0fa0),
                    ..RawFrame::default()
                }],
                ..RawStacktrace::default()
            }],
            modules: vec![CompleteObjectInfo::from(RawObjectInfo {
                ty: ObjectType::Macho,
                code_id: Some("502fc0a51ec13e479998684fa139dca7".to_owned().to_lowercase()),
                debug_id: Some("502fc0a5-1ec1-3e47-9998-684fa139dca7".to_owned()),
                image_addr: HexValue(0x1_0000_0000),
                image_size: Some(4096),
                code_file: None,
                debug_file: None,
                debug_checksum: None,
            })],
            apply_source_context: true,
            scraping: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_max_requests() {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            max_concurrent_requests: Some(2),
            ..Default::default()
        };

        let handle = tokio::runtime::Handle::current();
        let service = RequestService::create(config, handle.clone(), handle).unwrap();

        let hitcounter = test::Server::new();
        let source = hitcounter.source("pending", "/delay/1h/");

        // Make three requests that never get resolved. Since the server is configured to only accept a maximum of
        // two concurrent requests, the first two should succeed and the third one should fail.
        let request = get_symbolication_request(vec![source.clone()]);
        assert!(service
            .symbolicate_stacktraces(request, RequestOptions::default())
            .is_ok());

        let request = get_symbolication_request(vec![source.clone()]);
        assert!(service
            .symbolicate_stacktraces(request, RequestOptions::default())
            .is_ok());

        let request = get_symbolication_request(vec![source]);
        assert!(service
            .symbolicate_stacktraces(request, RequestOptions::default())
            .is_err());
    }
}
