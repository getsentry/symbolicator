use crate::actors::objects::DownloadPath;
use crate::types::{DirectoryLayout, FileType, Glob, ObjectId, SourceFilters};
use crate::utils::paths::get_directory_path;

const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// Generate a list of filepaths to try downloading from.
///
/// `object_id`: Information about the image we want to download.
/// `filetypes`: Limit search to these filetypes.
/// `filters`: Filters from a `SourceConfig` to limit the amount of generated paths.
/// `layout`: Directory from `SourceConfig` to define what kind of paths we generate.
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
