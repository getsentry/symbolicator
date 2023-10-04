//! Metrics
//!
//! - `js.unsymbolicated_frames`: The number of unsymbolicated frames, per event.
//!   Should be `0` in the best case, as we obviously should symbolicate :-)
//!
//! - `js.missing_sourcescontent`: The number of frames, per event, that have no embedded sources.
//!   Should be `0` in the best case, as the SourceMaps we use should have embedded sources.
//!   If they don’t, we have to fall back to applying source context from elsewhere.
//!
//! - `js.api_requests`: The number of (potentially cached) API requests, per event.
//!   Should be `1` in the best case, as `prefetch_artifacts` should provide us with everything we need.
//!
//! - `js.queried_bundles` / `js.fetched_bundles`: The number of artifact bundles the API gave us,
//!   and the ones we ended up using.
//!   Should both be `1` in the best case, as a single bundle should ideally serve all our needs.
//!   Otherwise `queried` and `fetched` should be the same, as a difference between the two means
//!   that multiple API requests gave us duplicated bundles.
//!
//! - `js.queried_artifacts` / `js.fetched_artifacts`: The number of individual artifacts the API
//!   gave us, and the ones we ended up using.
//!   Should both be `0` as we should not be using individual artifacts but rather bundles.
//!   Otherwise, `queried` should be close to `fetched`. If they differ, it means the API is sending
//!   us a lot of candidate artifacts that we don’t end up using, or multiple API requests give us
//!   duplicated artifacts.
//!
//! - `js.scraped_files`: The number of files that were scraped from the Web.
//!   Should be `0`, as we should find/use files from within bundles or as individual artifacts.

use symbolic::debuginfo::sourcebundle::SourceFileType;
use symbolicator_service::metric;

use crate::interface::ResolvedWith;

/// Various metrics we want to capture *per-event* for JS events.
#[derive(Debug, Default)]
pub struct JsMetrics {
    pub needed_files: u64,
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
        metric!(time_raw("js.needed_files") = self.needed_files);
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
