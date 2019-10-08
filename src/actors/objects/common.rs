use crate::actors::objects::DownloadPath;
use crate::types::{DirectoryLayout, FileType, ObjectId, SourceFilters};
use crate::utils::paths::get_directory_path;

#[derive(Debug)]
pub(super) struct DownloadPathIter<'a> {
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
