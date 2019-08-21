use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use failure::{Fail, ResultExt};
use futures::{Future, Stream};
use tempfile::NamedTempFile;

use crate::types::{DirectoryLayout, FileType, Glob, ObjectId, SourceFilters};
use crate::utils::futures::ResultFuture;
use crate::utils::paths::get_directory_path;

const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// A helper that renders any error as context for `ObjectError`.
#[derive(Debug, Fail)]
struct DisplayError(String);

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Fail, Clone, Copy)]
pub enum ObjectErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "unable to get directory for tempfiles")]
    NoTempDir,

    #[fail(display = "failed to parse object")]
    Parsing,

    #[fail(display = "failed to look into cache")]
    Caching,

    #[fail(display = "object download took too long")]
    Timeout,

    #[fail(display = "object download canceled due to shutdown")]
    Canceled,
}

symbolic::common::derive_failure!(
    ObjectError,
    ObjectErrorKind,
    doc = "Errors happening while fetching objects"
);

impl ObjectError {
    /// Constructs an object error from any generic error that implements `Display`.
    ///
    /// This can be used for errors that do not carry stack traces and do not implement `std::Error`
    /// or `failure::Fail`.
    #[inline]
    pub fn from_error<E>(error: E, kind: ObjectErrorKind) -> Self
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
        Self::from_error(error, ObjectErrorKind::Io)
    }
}

impl From<io::Error> for ObjectError {
    fn from(e: io::Error) -> Self {
        e.context(ObjectErrorKind::Io).into()
    }
}

/// A relative path of an object file in a source.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub(super) struct DownloadPath(String);

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
    pub fn local<P>(path: P) -> Result<Self, ObjectError>
    where
        P: Into<PathBuf>,
    {
        let path = path.into();
        let file = File::open(&path).context(ObjectErrorKind::Io)?;
        Ok(Self::Local(path, file))
    }

    /// Streams a remote file into a temporary location.
    pub fn streaming<S>(download_dir: &Path, stream: S) -> ResultFuture<Self, ObjectError>
    where
        S: Stream<Error = ObjectError> + 'static,
        S::Item: AsRef<[u8]>,
    {
        let named_download_file =
            tryf!(NamedTempFile::new_in(&download_dir).context(ObjectErrorKind::Io));

        let file = tryf!(named_download_file.reopen().context(ObjectErrorKind::Io));

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

/// Generate a list of filepaths to try downloading from.
///
///  - `object_id`: Information about the image we want to download.
///  - `filetypes`: Limit search to these filetypes.
///  - `filters`: Filters from a `SourceConfig` to limit the amount of generated paths.
///  - `layout`: Directory from `SourceConfig` to define what kind of paths we generate.
pub(super) fn prepare_download_paths<'a>(
    object_id: &'a ObjectId,
    filetypes: &'static [FileType],
    filters: &'a SourceFilters,
    layout: DirectoryLayout,
) -> impl Iterator<Item = DownloadPath> + 'a {
    filetypes.iter().flat_map(move |&filetype| {
        if (filters.filetypes.is_empty() || filters.filetypes.contains(&filetype))
            && matches_path_patterns(object_id, &filters.path_patterns)
        {
            if let Some(path) = get_directory_path(layout, filetype, &object_id) {
                return if filetype == FileType::Pdb || filetype == FileType::Pe {
                    let mut compressed_path = path.clone();
                    compressed_path.pop();
                    compressed_path.push('_');
                    itertools::Either::Left(
                        Some(DownloadPath(path))
                            .into_iter()
                            .chain(Some(DownloadPath(compressed_path))),
                    )
                } else {
                    itertools::Either::Right(Some(DownloadPath(path)).into_iter())
                };
            }
        }
        itertools::Either::Right(None.into_iter())
    })
}

fn matches_path_patterns(object_id: &ObjectId, patterns: &[Glob]) -> bool {
    fn canonicalize_path(s: &str) -> String {
        s.replace(r"\", "/")
    }

    if patterns.is_empty() {
        return true;
    }

    for pattern in patterns {
        for path in &[&object_id.code_file, &object_id.debug_file] {
            if let Some(ref path) = path {
                if pattern.matches_with(&canonicalize_path(path), GLOB_OPTIONS) {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
fn pattern(x: &str) -> Glob {
    Glob(x.parse().unwrap())
}

#[test]
fn test_matches_path_patterns_empty() {
    assert!(matches_path_patterns(
        &ObjectId {
            code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
            ..Default::default()
        },
        &[]
    ));
}

#[test]
fn test_matches_path_patterns_single_star() {
    assert!(matches_path_patterns(
        &ObjectId {
            code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
            ..Default::default()
        },
        &[pattern("c:/windows/*")]
    ));
}

#[test]
fn test_matches_path_patterns_drive_letter_wildcard() {
    assert!(matches_path_patterns(
        &ObjectId {
            code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
            ..Default::default()
        },
        &[pattern("?:/windows/*")]
    ));
}

#[test]
fn test_matches_path_patterns_drive_letter() {
    assert!(!matches_path_patterns(
        &ObjectId {
            code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
            ..Default::default()
        },
        &[pattern("d:/windows/**")]
    ));
}
