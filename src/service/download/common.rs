use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use failure::{Fail, ResultExt};
use futures::{Future, Stream};
use tempfile::NamedTempFile;

use crate::types::{DirectoryLayout, FileType, ObjectId, SourceFilters};
use crate::utils::futures::ResultFuture;
use crate::utils::paths::get_directory_path;

/// A helper that renders any error as context for `DownloadError`.
#[derive(Debug, Fail)]
struct DisplayError(String);

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Fail, Clone, Copy)]
pub enum DownloadErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "download canceled due to shutdown")]
    Canceled,
}

symbolic::common::derive_failure!(
    DownloadError,
    DownloadErrorKind,
    doc = "Errors happening while downloading files"
);

impl DownloadError {
    /// Constructs an object error from any generic error that implements `Display`.
    ///
    /// This can be used for errors that do not carry stack traces and do not implement `std::Error`
    /// or `failure::Fail`.
    #[inline]
    pub fn from_error<E>(error: E, kind: DownloadErrorKind) -> Self
    where
        E: fmt::Display,
    {
        DisplayError(error.to_string()).context(kind).into()
    }

    /// Constructs an IO object error from any generic error that implements `Display`.
    #[inline]
    pub fn io<E>(error: E) -> Self
    where
        E: fmt::Display,
    {
        Self::from_error(error, DownloadErrorKind::Io)
    }
}

impl From<io::Error> for DownloadError {
    fn from(e: io::Error) -> Self {
        e.context(DownloadErrorKind::Io).into()
    }
}

/// A relative path of an object file in a source.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct DownloadPath(String);

impl From<String> for DownloadPath {
    fn from(path: String) -> Self {
        Self(path)
    }
}

impl fmt::Display for DownloadPath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for DownloadPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for DownloadPath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl std::ops::Deref for DownloadPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The result of a remote file download operation.
pub enum DownloadedFile {
    /// A locally stored file.
    Local(PathBuf, File),
    /// A downloaded file stored in a temp location.
    Temp(NamedTempFile),
}

impl DownloadedFile {
    /// Loads a locally stored file at the given location.
    pub fn local<P>(path: P) -> Result<Self, DownloadError>
    where
        P: Into<PathBuf>,
    {
        let path = path.into();
        let file = File::open(&path).context(DownloadErrorKind::Io)?;
        Ok(Self::Local(path, file))
    }

    /// Streams a remote file into a temporary location.
    pub fn streaming<S>(download_dir: &Path, stream: S) -> ResultFuture<Self, DownloadError>
    where
        S: Stream<Error = DownloadError> + 'static,
        S::Item: AsRef<[u8]>,
    {
        let named_download_file =
            tryf!(NamedTempFile::new_in(&download_dir).context(DownloadErrorKind::Io));

        let file = tryf!(named_download_file.reopen().context(DownloadErrorKind::Io));

        let future = stream
            .fold(file, |mut file, chunk| {
                file.write_all(chunk.as_ref()).map(|_| file)
            })
            .map(move |_| DownloadedFile::Temp(named_download_file));

        Box::new(future)
    }

    /// Gets the file system path of the downloaded file.
    ///
    /// Note that the path is only guaranteed to be valid until this instance of `DownloadedFile` is
    /// dropped. After that, a temporary file may get deleted in the file system.
    pub fn path(&self) -> &Path {
        match self {
            DownloadedFile::Local(path, _) => &path,
            DownloadedFile::Temp(named) => named.path(),
        }
    }

    /// Gets a file handle to the downloaded file.
    pub fn reopen(&self) -> io::Result<File> {
        match self {
            DownloadedFile::Local(_, file) => file.try_clone(),
            DownloadedFile::Temp(named) => named.reopen(),
        }
    }
}

#[derive(Debug)]
pub struct DownloadPathIter<'a> {
    filetypes: std::slice::Iter<'a, FileType>,
    filters: &'a SourceFilters,
    object_id: &'a ObjectId,
    layout: DirectoryLayout,
    next: Option<DownloadPath>,
}

impl Iterator for DownloadPathIter<'_> {
    type Item = DownloadPath;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(path) = self.next.take() {
            return Some(path);
        }

        while let Some(&filetype) = self.filetypes.next() {
            if !self.filters.is_allowed(self.object_id, filetype) {
                continue;
            }

            let path = match get_directory_path(self.layout, filetype, self.object_id) {
                Some(path) => path,
                None => continue,
            };

            if filetype == FileType::Pdb || filetype == FileType::Pe {
                let mut compressed_path = path.clone();
                compressed_path.pop();
                compressed_path.push('_');

                debug_assert!(self.next.is_none());
                self.next = Some(DownloadPath(compressed_path));
            }

            return Some(DownloadPath(path));
        }

        self.next.take()
    }
}

/// Generate a list of filepaths to try downloading from.
///
///  - `object_id`: Information about the image we want to download.
///  - `filetypes`: Limit search to these filetypes.
///  - `filters`: Filters from a `SourceConfig` to limit the amount of generated paths.
///  - `layout`: Directory from `SourceConfig` to define what kind of paths we generate.
pub(super) fn prepare_download_paths<'a>(
    object_id: &'a ObjectId,
    filetypes: &'a [FileType],
    filters: &'a SourceFilters,
    layout: DirectoryLayout,
) -> DownloadPathIter<'a> {
    DownloadPathIter {
        filetypes: filetypes.iter(),
        filters,
        object_id,
        layout,
        next: None,
    }
}
