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

use std::collections::HashMap;

use symbolic::debuginfo::sourcebundle::SourceFileType;
use symbolicator_service::{metric, metrics, types::Platform};

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
            DebugId => self.found_bundle_via_debugid += 1,
            Index => self.found_bundle_via_index += 1,
            Release => self.found_bundle_via_release += 1,
            _ => self.found_bundle_via_release_old += 1,
        }
    }

    pub fn submit_metrics(&self, artifact_bundles: u64) {
        metrics::with_client(|aggregator| self.submit_local_metrics(aggregator, artifact_bundles))
    }

    fn submit_local_metrics(
        &self,
        aggregator: &mut metrics::LocalAggregator,
        artifact_bundles: u64,
    ) {
        // per-event distribution, emitted as `time_raw`
        use symbolicator_service::metrics::IntoDistributionValue;
        aggregator.emit_timer("js.needed_files", self.needed_files.into_value(), &[]);
        aggregator.emit_timer("js.api_requests", self.api_requests.into_value(), &[]);
        aggregator.emit_timer("js.queried_bundles", self.queried_bundles.into_value(), &[]);
        aggregator.emit_timer("js.fetched_bundles", artifact_bundles.into_value(), &[]);
        aggregator.emit_timer(
            "js.queried_artifacts",
            self.queried_artifacts.into_value(),
            &[],
        );
        aggregator.emit_timer(
            "js.fetched_artifacts",
            self.fetched_artifacts.into_value(),
            &[],
        );
        aggregator.emit_timer("js.scraped_files", self.scraped_files.into_value(), &[]);

        // Sources:
        aggregator.emit_count(
            "js.found_via_bundle_debugid",
            self.found_source_via_debugid,
            &[("type", "source")],
        );
        aggregator.emit_count(
            "js.found_via_bundle_url",
            self.found_source_via_release,
            &[("type", "source"), ("lookup", "release")],
        );
        aggregator.emit_count(
            "js.found_via_bundle_url",
            self.found_source_via_release_old,
            &[("type", "source"), ("lookup", "release-old")],
        );
        aggregator.emit_count(
            "js.found_via_scraping",
            self.found_source_via_scraping,
            &[("type", "source")],
        );
        aggregator.emit_count(
            "js.file_not_found",
            self.source_not_found,
            &[("type", "source")],
        );

        // SourceMaps:
        aggregator.emit_count(
            "js.found_via_bundle_debugid",
            self.found_sourcemap_via_debugid,
            &[("type", "sourcemap")],
        );
        aggregator.emit_count(
            "js.found_via_bundle_url",
            self.found_sourcemap_via_release,
            &[("type", "sourcemap"), ("lookup", "release")],
        );
        aggregator.emit_count(
            "js.found_via_bundle_url",
            self.found_sourcemap_via_release_old,
            &[("type", "sourcemap"), ("lookup", "release-old")],
        );
        aggregator.emit_count(
            "js.found_via_scraping",
            self.found_sourcemap_via_scraping,
            &[("type", "sourcemap")],
        );
        aggregator.emit_count(
            "js.file_not_found",
            self.sourcemap_not_found,
            &[("type", "sourcemap")],
        );
        aggregator.emit_count("js.sourcemap_not_needed", self.sourcemap_not_needed, &[]);

        // Lookup Method:
        aggregator.emit_count(
            "js.bundle_lookup",
            self.found_bundle_via_debugid,
            &[("method", "debugid")],
        );
        aggregator.emit_count(
            "js.bundle_lookup",
            self.found_bundle_via_index,
            &[("method", "index")],
        );
        aggregator.emit_count(
            "js.bundle_lookup",
            self.found_bundle_via_release,
            &[("method", "release")],
        );
        aggregator.emit_count(
            "js.bundle_lookup",
            self.found_bundle_via_release_old,
            &[("method", "release-old")],
        );
    }
}

/// Record metrics about stacktraces and frames.
pub fn record_stacktrace_metrics(event_platform: Option<Platform>, stats: SymbolicationStats) {
    let event_platform = event_platform
        .as_ref()
        .map(|p| p.as_ref())
        .unwrap_or("none");

    metric!(time_raw("symbolication.num_stacktraces") = stats.num_stacktraces);

    for (p, count) in stats.symbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none");
        metric!(
            time_raw("symbolication.num_frames") =
                count,
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }

    for (p, count) in stats.unsymbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none");
        metric!(
            time_raw("symbolication.unsymbolicated_frames") =
                count,
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }

    metric!(time_raw("js.missing_sourcescontent") = stats.missing_sourcescontent);
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolicationStats {
    pub(crate) symbolicated_frames: HashMap<Option<Platform>, u64>,
    pub(crate) unsymbolicated_frames: HashMap<Option<Platform>, u64>,
    pub(crate) num_stacktraces: u64,
    pub(crate) missing_sourcescontent: u64,
}
