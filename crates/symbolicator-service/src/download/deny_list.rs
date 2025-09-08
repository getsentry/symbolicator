use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::caching::CacheError;
use crate::config::Config;

/// A record of a number of download failures in a given second.
#[derive(Debug, Clone, Copy)]
struct FailureCount {
    /// The time at which the failures occurred, measured in milliseconds since the Unix Epoch.
    timestamp: u64,
    /// The number of failures.
    failures: usize,
}

type CountedFailures = Arc<Mutex<VecDeque<FailureCount>>>;

/// A structure that keeps track of download failures in a given time interval
/// and puts hosts on a block list accordingly.
///
/// The logic works like this: if a host has at least `failure_threshold` download
/// failures in a window of `time_window_millis` ms, it will be blocked for a duration of
/// `block_time`.
///
/// Hosts included in `never_block` will never be blocked regardless of download_failures.
#[derive(Clone, Debug)]
pub(crate) struct HostDenyList {
    time_window_millis: u64,
    bucket_size_millis: u64,
    failure_threshold: usize,
    block_time: Duration,
    never_block: Vec<String>,
    failures: moka::sync::Cache<String, CountedFailures>,
    blocked_hosts: moka::sync::Cache<String, CacheError>,
}

impl HostDenyList {
    /// Creates an empty [`HostDenyList`].
    pub(crate) fn from_config(config: &Config) -> Self {
        let time_window_millis = config.deny_list_time_window.as_millis() as u64;
        let bucket_size_millis = config.deny_list_bucket_size.as_millis() as u64;
        Self {
            time_window_millis,
            bucket_size_millis,
            failure_threshold: config.deny_list_threshold,
            block_time: config.deny_list_block_time,
            never_block: config.deny_list_never_block_hosts.clone(),
            failures: moka::sync::Cache::builder()
                // If a host hasn't had download failures for an entire `deny_list_time_window`
                // we can remove it from the `failures` cache.
                .time_to_idle(config.deny_list_time_window)
                .build(),
            blocked_hosts: moka::sync::Cache::builder()
                .time_to_live(config.deny_list_block_time)
                .eviction_listener(|host, _, _| tracing::info!(%host, "Unblocking host"))
                .build(),
        }
    }

    /// Rounds a duration down to a multiple of the configured `bucket_size`.
    fn round_duration(&self, duration: Duration) -> u64 {
        let duration = duration.as_millis() as u64;

        duration - (duration % self.bucket_size_millis)
    }

    /// The maximum length of the failure queue for one host.
    fn max_queue_len(&self) -> usize {
        // Add one to protect against round issues if `time_window` is not a multiple of `bucket_size`.
        (self.time_window_millis / self.bucket_size_millis) as usize + 1
    }

    /// Registers a download failure for the given `host`.
    ///
    /// If that puts the host over the threshold, it is added
    /// to the blocked servers.
    pub(crate) fn register_failure(&self, source_name: &str, host: String, error: &CacheError) {
        let now = SystemTime::now();

        // Sanity check: we don't need to count failures for hosts which are currently blocked.
        // This can happen if multiple download requests fail around the same time.
        if self.blocked_hosts.contains_key(&host) {
            return;
        }

        tracing::trace!(
            host = %host,
            time = %humantime::format_rfc3339(now),
            %error,
            "Registering download failure"
        );

        let current_ts = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        let current_ts = self.round_duration(current_ts);
        let entry = self.failures.entry_by_ref(&host).or_default();

        let mut queue = entry.value().lock().unwrap();
        match queue.back_mut() {
            Some(last) if last.timestamp == current_ts => {
                last.failures += 1;
            }
            _ => {
                queue.push_back(FailureCount {
                    timestamp: current_ts,
                    failures: 1,
                });
            }
        }

        if queue.len() > self.max_queue_len() {
            queue.pop_front();
        }

        let cutoff = current_ts - self.time_window_millis;
        let total_failures: usize = queue
            .iter()
            .skip_while(|failure_count| failure_count.timestamp < cutoff)
            .map(|failure_count| failure_count.failures)
            .sum();

        if total_failures >= self.failure_threshold {
            tracing::info!(
                %host,
                block_time = %humantime::format_duration(self.block_time),
                %error,
                "Blocking host due to too many download failures"
            );

            if !self.never_block.contains(&host) {
                self.blocked_hosts.insert(host, error.clone());
                metric!(gauge("service.download.blocked-hosts") = self.blocked_hosts.weighted_size(), "source" => source_name);
            }
        }
    }

    /// If the given host is blocked, this returns the error that caused the block.
    pub(crate) fn is_blocked(&self, host: &str) -> Option<CacheError> {
        self.blocked_hosts.get(host)
    }

    /// Creates a `CacheError` for the given `host` and `reason`.
    pub(crate) fn format_error(&self, host: &str, reason: &CacheError) -> CacheError {
        CacheError::DownloadError(format!(
            "Host {host} is temporarily blocked because there were too many download failures. It will remain blocked for a maximum of {}. The error that triggered the block was: `{reason}`.",
            humantime::format_duration(self.block_time),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::config::Config;

    use super::*;

    #[test]
    fn test_host_deny_list() {
        let config = Config {
            deny_list_time_window: Duration::from_secs(5),
            deny_list_block_time: Duration::from_millis(100),
            deny_list_bucket_size: Duration::from_secs(1),
            deny_list_threshold: 2,
            ..Default::default()
        };
        let deny_list = HostDenyList::from_config(&config);
        let host = String::from("test");

        deny_list.register_failure(
            "test",
            host.clone(),
            &CacheError::DownloadError("Test error".to_owned()),
        );

        // shouldn't be blocked after one failure
        assert!(deny_list.is_blocked(&host).is_none());

        deny_list.register_failure(
            "test",
            host.clone(),
            &CacheError::DownloadError("Test error".to_owned()),
        );

        // should be blocked after two failures
        assert!(deny_list.is_blocked(&host).is_some());

        std::thread::sleep(Duration::from_millis(100));

        // should be unblocked after 100ms have passed
        assert!(deny_list.is_blocked(&host).is_none());
    }

    #[test]
    fn test_host_deny_list_never_block() {
        let config = Config {
            deny_list_time_window: Duration::from_secs(5),
            deny_list_block_time: Duration::from_millis(100),
            deny_list_bucket_size: Duration::from_secs(1),
            deny_list_threshold: 2,
            deny_list_never_block_hosts: vec!["test".to_string()],
            ..Default::default()
        };
        let deny_list = HostDenyList::from_config(&config);
        let host = String::from("test");

        deny_list.register_failure(
            "test",
            host.clone(),
            &CacheError::DownloadError("Test error".to_owned()),
        );
        deny_list.register_failure(
            "test",
            host.clone(),
            &CacheError::DownloadError("Test error".to_owned()),
        );

        assert!(deny_list.is_blocked(&host).is_none());
    }
}
