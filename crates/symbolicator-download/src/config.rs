use std::time::Duration;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DownloaderConfig {
    /// Allow reserved IP addresses for requests to sources.
    pub connect_to_reserved_ips: bool,

    /// The maximum timeout for downloads.
    ///
    /// This is the upper limit the download service will take for downloading from a single
    /// source, regardless of how many retries are involved. The default is set to 315s,
    /// just above the amount of time it would take for a 4MB/s connection to download 2GB.
    #[serde(with = "humantime_serde")]
    pub max_download_timeout: Duration,

    /// The timeout for the initial HEAD request in a download.
    ///
    /// This timeout applies to each individual attempt to establish a
    /// connection with a symbol source if retries take place.
    #[serde(with = "humantime_serde")]
    pub connect_timeout: Duration,

    /// The timeout per GB for streaming downloads.
    ///
    /// For downloads with a known size, this timeout applies per individual
    /// download attempt. If the download size is not known, it is ignored and
    /// only `max_download_timeout` applies. The default is set to 250s,
    /// just above the amount of time it would take for a 4MB/s connection to
    /// download 1GB.
    #[serde(with = "humantime_serde")]
    pub streaming_timeout: Duration,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            connect_to_reserved_ips: false,
            // Allow a 4MB/s connection to download 2GB without timing out
            max_download_timeout: Duration::from_secs(315),
            connect_timeout: Duration::from_secs(15),
            // Allow a 4MB/s connection to download 1GB without timing out
            streaming_timeout: Duration::from_secs(250),
        }
    }
}
