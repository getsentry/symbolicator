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
pub fn prepare_download_paths<'a>(
    object_id: &'a ObjectId,
    filetypes: &'static [FileType],
    filters: &'a SourceFilters,
    layout: DirectoryLayout,
) -> impl Iterator<Item = DownloadPath> + 'a {
    filetypes.iter().filter_map(move |&filetype| {
        if (filters.filetypes.is_empty() || filters.filetypes.contains(&filetype))
            && matches_path_patterns(object_id, &filters.path_patterns)
        {
            Some(DownloadPath(get_directory_path(
                layout, filetype, &object_id,
            )?))
        } else {
            None
        }
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
