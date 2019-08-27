use std::fmt;
use std::path::PathBuf;

use futures::future;

use crate::types::{FileType, ObjectId, SourceConfig};
use crate::utils::futures::{RemoteThread, SendFuture};

mod common;
mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

use self::common::ObjectDownloader;
pub use self::common::{DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile};

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
        source: SourceConfig,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<DownloadPath>, DownloadError> {
        match source {
            SourceConfig::Sentry(source) => {
                let files = self.sentry.list_files(source, filetypes, object_id);
                Box::new(files)
            }
            SourceConfig::Http(source) => {
                let files = self.http.list_files(source, filetypes, object_id);
                Box::new(future::result(files))
            }
            SourceConfig::S3(source) => {
                let files = self.s3.list_files(source, filetypes, object_id);
                Box::new(future::result(files))
            }
            SourceConfig::Gcs(source) => {
                let files = self.gcs.list_files(source, filetypes, object_id);
                Box::new(future::result(files))
            }
            SourceConfig::Filesystem(source) => {
                let files = self.fs.list_files(source, filetypes, object_id);
                Box::new(future::result(files))
            }
        }
    }

    pub fn download(
        &self,
        source: SourceConfig,
        download_path: DownloadPath,
        temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, DownloadError> {
        match source {
            SourceConfig::Sentry(source) => {
                let files = self.sentry.download(source, download_path, temp_dir);
                Box::new(files)
            }
            SourceConfig::Http(source) => {
                let files = self.http.download(source, download_path, temp_dir);
                Box::new(files)
            }
            SourceConfig::S3(source) => {
                let files = self.s3.download(source, download_path, temp_dir);
                Box::new(files)
            }
            SourceConfig::Gcs(source) => {
                let files = self.gcs.download(source, download_path, temp_dir);
                Box::new(files)
            }
            SourceConfig::Filesystem(source) => {
                let files = self.fs.download(source, download_path, temp_dir);
                Box::new(future::result(files))
            }
        }
    }
}

impl fmt::Debug for Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Downloader")
    }
}
