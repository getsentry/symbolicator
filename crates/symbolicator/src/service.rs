use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::fs::File;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future;
use futures::{channel::oneshot, future, FutureExt as _};
use parking_lot::Mutex;
use sentry::protocol::SessionStatus;
use sentry::SentryFutureExt;
use symbolicator_service::services::objects::ObjectsActor;
use tempfile::TempPath;
use tempfile::TempPath;

use symbolicator_service::config::Config;
use symbolicator_service::services::Service as SymbolicatorService;
use symbolicator_service::utils::futures::CallOnDrop;
use symbolicator_sources::SourceConfig;

pub use symbolicator_service::services::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
};
pub use symbolicator_service::services::symbolication::{
    MaxRequestsError, StacktraceOrigin, SymbolicateStacktraces,
};
pub use symbolicator_service::types::{
    RawObjectInfo, RawStacktrace, RequestId, RequestOptions, Scope, Signal, SymbolicationResponse,
};

/// The underlying service for the HTTP request handlers.
#[derive(Debug, Clone)]
pub struct RequestService {
    inner: Arc<SymbolicatorService>,
}

// We want a shared future here because otherwise polling for a response would hold the global lock.
type ComputationChannel = future::Shared<oneshot::Receiver<(Instant, SymbolicationResponse)>>;

type ComputationMap = Arc<Mutex<BTreeMap<RequestId, ComputationChannel>>>;

struct RequestServiceInner {
    config: Config,

    symbolication: SymbolicationActor,
    objects: ObjectsActor,

    cpu_pool: tokio::runtime::Handle,
    requests: ComputationMap,
    max_concurrent_requests: Option<usize>,
    current_requests: Arc<AtomicUsize>,
    symbolication_taskmon: tokio_metrics::TaskMonitor,
}

impl RequestService {
    /// Creates a new [`RequestService`].
    pub async fn create(
        config: Config,
        io_pool: tokio::runtime::Handle,
        cpu_pool: tokio::runtime::Handle,
    ) -> Result<Self> {
        let (symbolication, objects) = create_service(&config, io_pool).await?;

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

        let inner = RequestServiceInner {
            config,

            symbolication,
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
    pub async fn find_object(&self, request: FindObject) -> Result<FoundObject, ObjectError> {
        self.inner.objects.find(request).await
    }

    /// Fetches the object given by the [`ObjectMetaHandle`].
    pub async fn fetch_object(
        &self,
        handle: Arc<ObjectMetaHandle>,
    ) -> Result<Arc<ObjectHandle>, ObjectError> {
        self.inner.objects.fetch(handle).await
    }

    /// Creates a new request to symbolicate stacktraces.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn symbolicate_stacktraces(
        &self,
        request: SymbolicateStacktraces,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "symbolicate_stacktraces",
            "symbolicate_stacktraces",
            span,
        );
        self.create_symbolication_request(async move {
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let res = slf.symbolication.do_symbolicate(request).await;
            transaction.finish();
            res
        })
    }

    /// Creates a new request to process a minidump.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn process_minidump(
        &self,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "process_minidump",
            "process_minidump",
            span,
        );
        self.create_symbolication_request(async move {
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let res = slf
                .symbolication
                .do_process_minidump(scope, minidump_file, sources, options)
                .await;
            transaction.finish();
            res
        })
    }

    /// Creates a new request to process an Apple crash report.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn process_apple_crash_report(
        &self,
        scope: Scope,
        apple_crash_report: File,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.inner.clone();
        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "process_apple_crash_report",
            "process_apple_crash_report",
            span,
        );
        self.create_symbolication_request(async move {
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let res = slf
                .symbolication
                .do_process_apple_crash_report(scope, apple_crash_report, sources, options)
                .await;
            transaction.finish();
            res
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
        let channel_opt = self.requests.lock().get(&request_id).cloned();
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

    /// Returns a clone of the task monitor for symbolication requests.
    pub fn symbolication_task_monitor(&self) -> tokio_metrics::TaskMonitor {
        self.symbolication_taskmon.clone()
    }

    /// Creates a new request to compute the given future.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    fn create_symbolication_request<F>(&self, f: F) -> Result<RequestId, MaxRequestsError>
    where
        F: Future<Output = Result<CompletedSymbolicationResponse, SymbolicationError>>
            + Send
            + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let hub = Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));

        // Assume that there are no UUID4 collisions in practice.
        let requests = Arc::clone(&self.requests);
        let current_requests = Arc::clone(&self.current_requests);

        let num_requests = current_requests.load(Ordering::Relaxed);
        metric!(gauge("requests.in_flight") = num_requests as u64);

        // Reject the request if `requests` already contains `max_concurrent_requests` elements.
        if let Some(max_concurrent_requests) = self.max_concurrent_requests {
            if num_requests >= max_concurrent_requests {
                metric!(counter("requests.rejected") += 1);
                return Err(MaxRequestsError);
            }
        }

        let request_id = RequestId::new(uuid::Uuid::new_v4());
        requests.lock().insert(request_id, receiver.shared());
        current_requests.fetch_add(1, Ordering::Relaxed);
        let drop_hub = hub.clone();
        let token = CallOnDrop::new(move || {
            requests.lock().remove(&request_id);
            // we consider every premature drop of the future as fatal crash, which works fine
            // since ending a session consumes it and its not possible to double-end.
            drop_hub.end_session_with_status(SessionStatus::Crashed);
        });

        let spawn_time = Instant::now();
        let request_future = async move {
            metric!(timer("symbolication.create_request.first_poll") = spawn_time.elapsed());
            let response = match f.await {
                Ok(response) => {
                    sentry::end_session_with_status(SessionStatus::Exited);
                    SymbolicationResponse::Completed(Box::new(response))
                }
                Err(error) => {
                    // a timeout is an abnormal session exit, all other errors are considered "crashed"
                    let status = match &error {
                        SymbolicationError::Timeout => SessionStatus::Abnormal,
                        _ => SessionStatus::Crashed,
                    };
                    sentry::end_session_with_status(status);

                    let response = error.to_symbolication_response();
                    let error = anyhow::Error::new(error);
                    tracing::error!("Symbolication error: {:?}", error);
                    response
                }
            };

            sender.send((Instant::now(), response)).ok();

            // We stop counting the request as an in-flight request at this point, even though
            // it will stay in the `requests` map for another 90s.
            current_requests.fetch_sub(1, Ordering::Relaxed);

            // Wait before removing the channel from the computation map to allow clients to
            // poll the status.
            tokio::time::sleep(MAX_POLL_DELAY).await;

            drop(token);
        }
        .bind_hub(hub);

        self.cpu_pool
            .spawn(self.symbolication_taskmon.instrument(request_future));

        Ok(request_id)
    }
}

/// The maximum delay we allow for polling a finished request before dropping it.
const MAX_POLL_DELAY: Duration = Duration::from_secs(90);

/// An error returned when symbolicator receives a request while already processing
/// the maximum number of requests.
#[derive(Debug, Clone, Error)]
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
