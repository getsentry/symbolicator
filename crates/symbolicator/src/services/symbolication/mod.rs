use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{channel::oneshot, future, FutureExt as _};
use parking_lot::Mutex;
use sentry::protocol::SessionStatus;
use sentry::SentryFutureExt;
use tempfile::TempPath;
use thiserror::Error;

use crate::services::cficaches::{CfiCacheActor, CfiCacheError};
use crate::services::objects::ObjectsActor;
use crate::services::symcaches::{SymCacheActor, SymCacheError};
use crate::sources::SourceConfig;
use crate::types::{
    CompletedSymbolicationResponse, ObjectFileStatus, ObjectId, RawObjectInfo, RequestId,
    RequestOptions, Scope, SymbolicationResponse,
};
use crate::utils::futures::CallOnDrop;

mod apple;
mod module_lookup;
mod process_minidump;
// we should really rename this here to the `SymbolicatorService`, as it does a lot more
// than just symbolication ;-)
#[allow(clippy::module_inception)]
mod symbolication;

pub use symbolication::{StacktraceOrigin, SymbolicateStacktraces};

// We want a shared future here because otherwise polling for a response would hold the global lock.
type ComputationChannel = future::Shared<oneshot::Receiver<(Instant, SymbolicationResponse)>>;

type ComputationMap = Arc<Mutex<BTreeMap<RequestId, ComputationChannel>>>;

#[derive(Clone)]
pub struct SymbolicationActor {
    objects: ObjectsActor,
    symcaches: SymCacheActor,
    cficaches: CfiCacheActor,
    diagnostics_cache: crate::cache::Cache,
    cpu_pool: tokio::runtime::Handle,
    requests: ComputationMap,
    max_concurrent_requests: Option<usize>,
    current_requests: Arc<AtomicUsize>,
    symbolication_taskmon: tokio_metrics::TaskMonitor,
}

impl SymbolicationActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        objects: ObjectsActor,
        symcaches: SymCacheActor,
        cficaches: CfiCacheActor,
        diagnostics_cache: crate::cache::Cache,
        cpu_pool: tokio::runtime::Handle,
        max_concurrent_requests: Option<usize>,
    ) -> Self {
        SymbolicationActor {
            objects,
            symcaches,
            cficaches,
            diagnostics_cache,
            cpu_pool,
            requests: Arc::new(Mutex::new(BTreeMap::new())),
            max_concurrent_requests,
            current_requests: Arc::new(AtomicUsize::new(0)),
            symbolication_taskmon: tokio_metrics::TaskMonitor::new(),
        }
    }

    /// Creates a new request to symbolicate stacktraces.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    pub fn symbolicate_stacktraces(
        &self,
        request: SymbolicateStacktraces,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.clone();
        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "symbolicate_stacktraces",
            "symbolicate_stacktraces",
            span,
        );
        self.create_symbolication_request(async move {
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let res = slf.do_symbolicate(request).await;
            transaction.finish();
            res
        })
    }

    /// Creates a new request to process a minidump.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    pub fn process_minidump(
        &self,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.clone();
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
                .do_process_minidump(scope, minidump_file, sources, options)
                .await;
            transaction.finish();
            res
        })
    }

    /// Creates a new request to process an Apple crash report.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    pub fn process_apple_crash_report(
        &self,
        scope: Scope,
        apple_crash_report: File,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.clone();
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

impl fmt::Debug for SymbolicationActor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("SymbolicationActor")
            .field("objects", &self.objects)
            .field("symcaches", &self.symcaches)
            .field("cficaches", &self.cficaches)
            .field("diagnostics_cache", &self.diagnostics_cache)
            .field("cpu_pool", &self.cpu_pool)
            .field("requests", &self.requests)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("current_requests", &self.current_requests)
            .field("symbolication_taskmon", &"<TaskMonitor>")
            .finish()
    }
}

/// The maximum delay we allow for polling a finished request before dropping it.
const MAX_POLL_DELAY: Duration = Duration::from_secs(90);

/// Errors during symbolication.
#[derive(Debug, Error)]
pub enum SymbolicationError {
    #[error("symbolication took too long")]
    Timeout,

    #[error(transparent)]
    Failed(#[from] anyhow::Error),

    #[error("failed to parse apple crash report")]
    InvalidAppleCrashReport(#[from] apple_crash_report_parser::ParseError),
}

impl SymbolicationError {
    fn to_symbolication_response(&self) -> SymbolicationResponse {
        match self {
            SymbolicationError::Timeout => SymbolicationResponse::Timeout,
            SymbolicationError::Failed(_) | SymbolicationError::InvalidAppleCrashReport(_) => {
                SymbolicationResponse::Failed {
                    message: self.to_string(),
                }
            }
        }
    }
}

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

impl From<&CfiCacheError> for ObjectFileStatus {
    fn from(e: &CfiCacheError) -> ObjectFileStatus {
        match e {
            CfiCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            CfiCacheError::Timeout => ObjectFileStatus::Timeout,
            CfiCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,

            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&SymCacheError> for ObjectFileStatus {
    fn from(e: &SymCacheError) -> ObjectFileStatus {
        match e {
            SymCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            SymCacheError::Timeout => ObjectFileStatus::Timeout,
            SymCacheError::Malformed => ObjectFileStatus::Malformed,
            SymCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

fn object_id_from_object_info(object_info: &RawObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: match object_info.debug_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        code_id: match object_info.code_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        debug_file: object_info.debug_file.clone(),
        code_file: object_info.code_file.clone(),
        object_type: object_info.ty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Config;
    use crate::services::symbolication::module_lookup::ModuleLookup;
    use crate::services::Service;
    use crate::test::{self, fixture};
    use crate::types::{CompleteObjectInfo, ObjectType, RawFrame, RawStacktrace};
    use crate::utils::addr::AddrMode;
    use crate::utils::hex::HexValue;

    /// Setup tests and create a test service.
    ///
    /// This function returns a tuple containing the service to test, and a temporary cache
    /// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
    /// guard until the test has finished.
    ///
    /// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
    /// symbol server to test object file downloads.
    pub(crate) async fn setup_service() -> (Service, test::TempDir) {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            ..Default::default()
        };
        let handle = tokio::runtime::Handle::current();
        let service = Service::create(config, handle.clone(), handle)
            .await
            .unwrap();

        (service, cache_dir)
    }

    fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
        SymbolicateStacktraces {
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
                checksum: None,
            })],
            options: RequestOptions {
                dif_candidates: true,
            },
        }
    }

    /// Helper to redact the port number from localhost URIs in insta snapshots.
    ///
    /// Since we use a localhost source on a random port during tests we get random port
    /// numbers in URI of the dif object file candidates.  This redaction masks this out.
    pub(crate) fn redact_localhost_port(
        value: insta::internals::Content,
        _path: insta::internals::ContentPath<'_>,
    ) -> impl Into<insta::internals::Content> {
        let re = regex::Regex::new(r"^http://localhost:[0-9]+").unwrap();
        re.replace(value.as_str().unwrap(), "http://localhost:<port>")
            .into_owned()
    }

    macro_rules! assert_snapshot {
        ($e:expr) => {
            ::insta::assert_yaml_snapshot!($e, {
                ".**.location" => ::insta::dynamic_redaction(
                    $crate::services::symbolication::tests::redact_localhost_port
                )
            });
        }
    }

    #[tokio::test]
    async fn test_remove_bucket() -> Result<(), SymbolicationError> {
        // Test with sources first, and then without. This test should verify that we do not leak
        // cached debug files to requests that no longer specify a source.

        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![source]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_add_bucket() -> anyhow::Result<()> {
        // Test without sources first, then with. This test should verify that we apply a new source
        // to requests immediately.

        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let request = get_symbolication_request(vec![]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        let request = get_symbolication_request(vec![source]);
        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_response_multi() {
        // Make sure we can repeatedly poll for the response
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();

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
            modules: Vec::new(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([]),
            scope: Default::default(),
            options: Default::default(),
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();

        for _ in 0..2 {
            let response = symbolication.get_response(request_id, None).await.unwrap();

            assert!(
                matches!(&response, SymbolicationResponse::Completed(_)),
                "Not a complete response: {:#?}",
                response
            );
        }
    }

    #[tokio::test]
    async fn test_apple_crash_report() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let report_file = std::fs::File::open(fixture("apple_crash_report.txt"))?;
        let request_id = symbolication
            .process_apple_crash_report(
                Scope::Global,
                report_file,
                Arc::new([source]),
                RequestOptions {
                    dif_candidates: true,
                },
            )
            .unwrap();

        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_wasm_payload() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"wasm",
                "debug_id":"bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
                "code_id":"bda18fd85d4a4eb893022d6bfad846b1",
                "debug_file":"file://foo.invalid/demo.wasm"
              }
            ]"#,
        )?;

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
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: Default::default(),
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_source_candidates() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service().await;
        let symbolication = service.symbolication();
        let (_symsrv, source) = test::symbol_server();

        // its not wasm, but that is the easiest to write tests because of relative
        // addressing ;-)
        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"wasm",
                "debug_id":"7f883fcd-c553-36d0-a809-b0150f09500b",
                "code_id":"7f883fcdc55336d0a809b0150f09500b"
              }
            ]"#,
        )?;

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x3880",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            origin: StacktraceOrigin::Symbolicate,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: RequestOptions {
                dif_candidates: true,
            },
        };

        let request_id = symbolication.symbolicate_stacktraces(request).unwrap();
        let response = symbolication.get_response(request_id, None).await;

        assert_snapshot!(response.unwrap());
        Ok(())
    }

    #[test]
    fn test_symcache_lookup_open_end_addr() {
        test::setup();

        // The Rust SDK and some other clients sometimes send zero-sized images when no end addr
        // could be determined. Symbolicator should still resolve such images.
        let info = CompleteObjectInfo::from(RawObjectInfo {
            ty: ObjectType::Unknown,
            code_id: None,
            debug_id: None,
            code_file: None,
            debug_file: None,
            image_addr: HexValue(42),
            image_size: Some(0),
            checksum: None,
        });

        let lookup = ModuleLookup::new(Scope::Global, Arc::new([]), std::iter::once(info.clone()));

        let lookup_result = lookup.lookup_cache(43, AddrMode::Abs).unwrap();
        assert_eq!(lookup_result.module_index, 0);
        assert_eq!(lookup_result.object_info, &info);
        assert!(lookup_result.cache.is_none());
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
        let service = Service::create(config, handle.clone(), handle)
            .await
            .unwrap();

        let symbolication = service.symbolication();
        let symbol_server = test::FailingSymbolServer::new();

        // Make three requests that never get resolved. Since the server is configured to only accept a maximum of
        // two concurrent requests, the first two should succeed and the third one should fail.
        let request = get_symbolication_request(vec![symbol_server.pending_source.clone()]);
        assert!(symbolication.symbolicate_stacktraces(request).is_ok());

        let request = get_symbolication_request(vec![symbol_server.pending_source.clone()]);
        assert!(symbolication.symbolicate_stacktraces(request).is_ok());

        let request = get_symbolication_request(vec![symbol_server.pending_source]);
        assert!(symbolication.symbolicate_stacktraces(request).is_err());
    }
}
