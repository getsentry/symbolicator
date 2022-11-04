use std::fs::File;
use std::sync::Arc;

use anyhow::Result;
use tempfile::TempPath;

use symbolicator_service::config::Config;
use symbolicator_service::services::Service as SymbolicatorService;
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

impl RequestService {
    /// Creates a new [`RequestService`].
    pub async fn create(
        config: Config,
        io_pool: tokio::runtime::Handle,
        cpu_pool: tokio::runtime::Handle,
    ) -> Result<Self> {
        let inner = SymbolicatorService::create(config, io_pool, cpu_pool).await?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Gives access to the [`Config`].
    pub fn config(&self) -> &Config {
        self.inner.config()
    }

    /// Looks up the object according to the [`FindObject`] request.
    pub async fn find_object(&self, request: FindObject) -> Result<FoundObject, ObjectError> {
        self.inner.objects().find(request).await
    }

    /// Fetches the object given by the [`ObjectMetaHandle`].
    pub async fn fetch_object(
        &self,
        handle: Arc<ObjectMetaHandle>,
    ) -> Result<Arc<ObjectHandle>, ObjectError> {
        self.inner.objects().fetch(handle).await
    }

    /// Creates a new request to symbolicate stacktraces.
    ///
    /// Returns an `Err` if the [`RequestService`] is already processing the
    /// maximum number of requests, as configured by the `max_concurrent_requests` option.
    pub fn symbolicate_stacktraces(
        &self,
        request: SymbolicateStacktraces,
    ) -> Result<RequestId, MaxRequestsError> {
        self.inner.symbolication().symbolicate_stacktraces(request)
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
        self.inner
            .symbolication()
            .process_minidump(scope, minidump_file, sources, options)
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
        self.inner.symbolication().process_apple_crash_report(
            scope,
            apple_crash_report,
            sources,
            options,
        )
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
        self.inner
            .symbolication()
            .get_response(request_id, timeout)
            .await
    }
}
