use symbolic::debuginfo::sourcebundle::SourceFileType;

use crate::types::ResolvedWith;

/// Various metrics we want to capture *per-event* for JS events.
#[derive(Debug, Default)]
pub struct JsMetrics {
    pub api_requests: u64,
    pub queried_artifacts: u64,
    pub fetched_artifacts: u64,
    pub queried_bundles: u64,
    pub scraped_files: u64,

    // Product managers are interested in these metrics as a "funnel":
    found_source_via_debugid: i64,
    found_source_via_release: i64,
    found_source_via_release_old: i64,
    found_source_via_scraping: i64,
    source_not_found: i64,

    found_sourcemap_via_debugid: i64,
    found_sourcemap_via_release: i64,
    found_sourcemap_via_release_old: i64,
    found_sourcemap_via_scraping: i64,
    sourcemap_not_found: i64,
    sourcemap_not_needed: i64,

    // Engineers might also be interested in these metrics:
    found_bundle_via_bundleindex: i64,
    found_bundle_via_debugid: i64,
    found_bundle_via_index: i64,
    found_bundle_via_release: i64,
    found_bundle_via_release_old: i64,
}

impl JsMetrics {
    pub fn record_file_scraped(&mut self, file_ty: SourceFileType) {
        if file_ty == SourceFileType::SourceMap {
            self.found_sourcemap_via_scraping += 1;
        } else {
            self.found_source_via_scraping += 1;
        }
    }

    pub fn record_not_found(&mut self, file_ty: SourceFileType) {
        if file_ty == SourceFileType::SourceMap {
            self.sourcemap_not_found += 1
        } else {
            self.source_not_found += 1
        }
    }

    pub fn record_sourcemap_not_needed(&mut self) {
        self.sourcemap_not_needed += 1
    }

    pub fn record_file_found_in_bundle(
        &mut self,
        file_ty: SourceFileType,
        file_identified_by: ResolvedWith,
        bundle_resolved_by: ResolvedWith,
    ) {
        use ResolvedWith::*;
        use SourceFileType::*;
        match (file_ty, file_identified_by, bundle_resolved_by) {
            (SourceMap, DebugId, _) => {
                self.found_sourcemap_via_debugid += 1;
            }
            (SourceMap, Url, ReleaseOld) => {
                self.found_sourcemap_via_release_old += 1;
            }
            (SourceMap, _, _) => {
                self.found_sourcemap_via_release += 1;
            }
            (_, DebugId, _) => self.found_source_via_debugid += 1,
            (_, Url, ReleaseOld) => self.found_source_via_release_old += 1,
            (_, _, _) => self.found_source_via_release += 1,
        };

        match bundle_resolved_by {
            BundleIndex => self.found_bundle_via_bundleindex += 1,
            DebugId => self.found_bundle_via_debugid += 1,
            Index => self.found_bundle_via_index += 1,
            Release => self.found_bundle_via_release += 1,
            _ => self.found_bundle_via_release_old += 1,
        }
    }

    pub fn submit_metrics(&self, artifact_bundles: u64) {
        metric!(time_raw("js.api_requests") = self.api_requests);
        metric!(time_raw("js.queried_bundles") = self.queried_bundles);
        metric!(time_raw("js.fetched_bundles") = artifact_bundles);
        metric!(time_raw("js.queried_artifacts") = self.queried_artifacts);
        metric!(time_raw("js.fetched_artifacts") = self.fetched_artifacts);
        metric!(time_raw("js.scraped_files") = self.scraped_files);

        // TODO: we are currently getting these as counters. maybe we want to use `time_raw` to also
        // have a per-event avg, etc.

        // Sources:
        metric!(
            counter("js.found_via_bundle_debugid") += self.found_source_via_debugid,
            "type" => "source",
        );
        metric!(
            counter("js.found_via_bundle_url") += self.found_source_via_release,
            "type" => "source",
            "lookup" => "release",
        );
        metric!(
            counter("js.found_via_bundle_url") += self.found_source_via_release_old,
            "type" => "source",
            "lookup" => "release-old",
        );
        metric!(
            counter("js.found_via_scraping") += self.found_source_via_scraping,
            "type" => "source",
        );
        metric!(
            counter("js.file_not_found") += self.source_not_found,
            "type" => "source",
        );

        // SourceMaps:
        metric!(
            counter("js.found_via_bundle_debugid") += self.found_sourcemap_via_debugid,
            "type" => "sourcemap",
        );
        metric!(
            counter("js.found_via_bundle_url") += self.found_sourcemap_via_release,
            "type" => "sourcemap",
            "lookup" => "release",
        );
        metric!(
            counter("js.found_via_bundle_url") += self.found_sourcemap_via_release_old,
            "type" => "sourcemap",
            "lookup" => "release-old",
        );
        metric!(
            counter("js.found_via_scraping") += self.found_sourcemap_via_scraping,
            "type" => "sourcemap",
        );
        metric!(
            counter("js.file_not_found") += self.sourcemap_not_found,
            "type" => "sourcemap",
        );
        metric!(counter("js.sourcemap_not_needed") += self.sourcemap_not_needed);

        // Lookup Method:
        metric!(
            counter("js.bundle_lookup") += self.found_bundle_via_bundleindex,
            "method" => "bundleindex"
        );
        metric!(
            counter("js.bundle_lookup") += self.found_bundle_via_debugid,
            "method" => "debugid"
        );
        metric!(
            counter("js.bundle_lookup") += self.found_bundle_via_index,
            "method" => "index"
        );
        metric!(
            counter("js.bundle_lookup") += self.found_bundle_via_release,
            "method" => "release"
        );
        metric!(
            counter("js.bundle_lookup") += self.found_bundle_via_release_old,
            "method" => "release-old"
        );
    }
}
