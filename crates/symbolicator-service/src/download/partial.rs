use std::{fmt, str::FromStr};

use reqwest::StatusCode;

/// The maximum amount of partial requests per download.
const MAX_PARTIAL_RANGES: u64 = 4;

/// The minimum amount of bytes requested in a partial download.
const MINIMUM_PARTIAL_SIZE: u64 = 12;

/// The initial range request sent to the server.
///
/// It must start at `0`.
///
/// The smaller the total initially requested range, the more requests will
/// be streamed.
const INITIAL_RANGE: Range = Range {
    start: 0,
    end: MINIMUM_PARTIAL_SIZE,
};

/// Prepares a `request` for partially streamed downloads.
pub fn initial_request(request: &mut reqwest::Request) {
    INITIAL_RANGE.apply_to(request);
}

pub fn split(start: u64, end: u64) -> impl Iterator<Item = Range> {
    let total_size = end.saturating_sub(start);

    None.into_iter()
}

/// Represents an HTTP Range header.
///
/// The string representation can be used as a range header.
///
/// Unlike the specification, this range cannot be open ended.
#[derive(Debug, Copy, Clone)]
pub struct Range {
    /// Start of the range.
    pub start: u64,
    /// Optional end of the range.
    pub end: u64,
}

impl Range {
    /// Returns the size of the range.
    pub fn size(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Applies this range to a request.
    pub fn apply_to(&self, request: &mut reqwest::Request) {
        let header = reqwest::header::HeaderValue::from_str(&INITIAL_RANGE.to_string());
        let header = header.expect("the range header to be a valid");
        request.headers_mut().insert(reqwest::header::RANGE, header);
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

/// A parsed `Content-Range` header for `bytes`.
///
/// This implementation only supports the very basic format
/// where all values are present: `bytes <start>-<end>/<size>`.
///
/// All other variations like `bytes */<size>` are not supported.
#[derive(Debug)]
pub struct BytesContentRange {
    /// Start of the returned range, offset in bytes.
    pub start: u64,
    /// End of the returned range, offset in bytes.
    pub end: u64,
    /// The total size of the resource on the server.
    pub total_size: u64,
}

impl BytesContentRange {
    /// Parses a [`BytesContentRange`] from a [`reqwest::Response`].
    ///
    /// Returns `None` if the response is not a partial response.
    pub fn from_response(response: &reqwest::Response) -> Option<Result<Self, InvalidBytesRange>> {
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return None;
        }

        response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|hv| hv.to_str().ok())
            .map(|s| s.parse())
    }
}

/// An error which can be returned when parsing a [`BytesContentRange`].
#[derive(thiserror::Error, Debug)]
pub enum InvalidBytesRange {
    /// The header is malformed.
    #[error("content range header is malformed")]
    Malformed,
    /// The specified unit is not `bytes`.
    #[error("content range unit is not bytes")]
    NotBytes,
    /// The header indicates the requested range is not satisfiable.
    #[error("the requested range is not satisfiable")]
    RangeNotSatisfiable,
    /// The header indicates the total length of the resource is unknown.
    #[error("the total length of the resource is not known")]
    UnknownLength,
}

impl FromStr for BytesContentRange {
    type Err = InvalidBytesRange;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(("bytes", s)) = s.trim().split_once(' ') else {
            return Err(InvalidBytesRange::NotBytes);
        };

        let (range, total_size) = s
            .trim()
            .split_once('/')
            .ok_or(InvalidBytesRange::Malformed)?;
        if range.trim() == "*" {
            return Err(InvalidBytesRange::RangeNotSatisfiable);
        }

        let (start, end) = range.split_once('-').ok_or(InvalidBytesRange::Malformed)?;
        let start = start
            .trim()
            .parse()
            .map_err(|_| InvalidBytesRange::Malformed)?;
        let end = end
            .trim()
            .parse()
            .map_err(|_| InvalidBytesRange::Malformed)?;

        if end < start {
            return Err(InvalidBytesRange::Malformed);
        }

        let total_size = match total_size.trim() {
            "*" => Err(InvalidBytesRange::UnknownLength),
            size => size.parse().map_err(|_| InvalidBytesRange::Malformed),
        }?;

        Ok(Self {
            start,
            end,
            total_size,
        })
    }
}
