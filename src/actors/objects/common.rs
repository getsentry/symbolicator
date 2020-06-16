use crate::sources::{DirectoryLayout, FileType, SourceFilters, SourceLocation};
use crate::types::ObjectId;
use crate::utils::paths::get_directory_paths;

#[derive(Debug)]
pub(super) struct SourceLocationIter<'a> {
    filetypes: std::slice::Iter<'a, FileType>,
    filters: &'a SourceFilters,
    object_id: &'a ObjectId,
    layout: DirectoryLayout,
    next: Vec<String>,
}

impl Iterator for SourceLocationIter<'_> {
    type Item = SourceLocation;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next.is_empty() {
            if let Some(&filetype) = self.filetypes.next() {
                if !self.filters.is_allowed(self.object_id, filetype) {
                    continue;
                }
                self.next = get_directory_paths(self.layout, filetype, self.object_id);
            } else {
                return None;
            }
        }

        self.next.pop().map(SourceLocation)
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
) -> SourceLocationIter<'a> {
    SourceLocationIter {
        filetypes: filetypes.iter(),
        filters,
        object_id,
        layout,
        next: Vec::new(),
    }
}
