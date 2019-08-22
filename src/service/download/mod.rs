use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::types::{
    FileType, FilesystemSourceConfig, GcsSourceConfig, HttpSourceConfig, ObjectId, S3SourceConfig,
    SentrySourceConfig, SourceConfig,
};
use crate::utils::futures::{RemoteThread, SendFuture};
use crate::utils::sentry::ToSentryScope;

mod common;
mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

use self::sentry::SentryFileId;

pub use self::common::{DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile};

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub enum FileId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, DownloadPath),
    Gcs(Arc<GcsSourceConfig>, DownloadPath),
    Http(Arc<HttpSourceConfig>, DownloadPath),
    Filesystem(Arc<FilesystemSourceConfig>, DownloadPath),
}

impl FileId {
    pub fn source(&self) -> SourceConfig {
        match *self {
            FileId::Sentry(ref x, ..) => SourceConfig::Sentry(x.clone()),
            FileId::S3(ref x, ..) => SourceConfig::S3(x.clone()),
            FileId::Gcs(ref x, ..) => SourceConfig::Gcs(x.clone()),
            FileId::Http(ref x, ..) => SourceConfig::Http(x.clone()),
            FileId::Filesystem(ref x, ..) => SourceConfig::Filesystem(x.clone()),
        }
    }

    pub fn cache_key(&self) -> String {
        match self {
            FileId::Http(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::S3(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::Gcs(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::Sentry(ref source, ref file_id) => {
                format!("{}.{}.sentryinternal", source.id, file_id)
            }
            FileId::Filesystem(ref source, ref path) => format!("{}.{}", source.id, path),
        }
    }
}

impl ToSentryScope for FileId {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        self.source().to_scope(scope);
    }
}

pub struct Downloader {
    fs: self::filesystem::FilesystemDownloader,
    gcs: self::gcs::GcsDownloader,
    http: self::http::HttpDownloader,
    s3: self::s3::S3Downloader,
    sentry: self::sentry::SentryDownloader,
}

impl Downloader {
    pub fn new() -> Self {
        let thread = RemoteThread::new();

        Self {
            fs: self::filesystem::FilesystemDownloader::new(),
            gcs: self::gcs::GcsDownloader::new(thread.clone()),
            http: self::http::HttpDownloader::new(thread.clone()),
            s3: self::s3::S3Downloader::new(thread.clone()),
            sentry: self::sentry::SentryDownloader::new(thread),
        }
    }

    pub fn list_files(
        &self,
        source: &SourceConfig,
        filetypes: &'static [FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<FileId>, DownloadError> {
        match *source {
            SourceConfig::Sentry(ref source) => {
                self.sentry.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Http(ref source) => {
                self.http.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::S3(ref source) => {
                self.s3.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Gcs(ref source) => {
                self.gcs.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Filesystem(ref source) => {
                self.fs.list_files(source.clone(), filetypes, object_id)
            }
        }
    }

    pub fn download(
        &self,
        file_id: FileId,
        temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, DownloadError> {
        match file_id {
            FileId::Sentry(source, file_id) => self.sentry.download(source, file_id, temp_dir),
            FileId::Http(source, path) => self.http.download(source, path, temp_dir),
            FileId::S3(source, path) => self.s3.download(source, path, temp_dir),
            FileId::Gcs(source, path) => self.gcs.download(source, path, temp_dir),
            FileId::Filesystem(source, path) => self.fs.download(source, path, temp_dir),
        }
    }
}

impl fmt::Debug for Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Downloader")
    }
}
