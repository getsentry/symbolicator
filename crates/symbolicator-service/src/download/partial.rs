use std::{fmt, str::FromStr};

use reqwest::StatusCode;

/// The maximum amount of partial requests per download.
///
/// Note: this does not include the original request.
const MAX_PARTIAL_RANGES: u64 = 4;

/// The amount of bytes when a download should split into multiple parts,
/// this essentially means the minimum split size is `PARTIAL_SPLIT_SIZE / 2`.
#[cfg(not(test))]
const PARTIAL_SPLIT_SIZE: u64 = 128 * 1024 * 1024;
#[cfg(test)]
const PARTIAL_SPLIT_SIZE: u64 = 100;

/// The initial range request sent to the server.
///
/// It must start at `0`.
///
/// The smaller the total initially requested range, the more requests will
/// be streamed.
const INITIAL_RANGE: Range = Range {
    start: 0,
    end: PARTIAL_SPLIT_SIZE - 1, // Ranges are inclusive.
};

/// Prepares a `request` for partially streamed downloads.
pub const fn initial_range() -> Range {
    INITIAL_RANGE
}

/// Splits the remainder of the remote file into multiple smaller ranges.
pub fn split(r: BytesContentRange) -> impl Iterator<Item = Range> {
    // Ranges are inclusive. This range represents the correct
    // bounds of the remainder of the supplied content range.
    let remainder = Range {
        start: r.end + 1,
        end: r.total_size - 1,
    };

    if remainder.size() == 0 {
        return None.into_iter().flatten();
    }

    // Infer the amount of partial requests we want to make,
    // making sure each request is split at `PARTIAL_SPLIT_SIZE` bytes,
    // but there are not more partial requests than `MAX_PARTIAL_RANGES`.
    let ranges = remainder
        .size()
        .div_ceil(PARTIAL_SPLIT_SIZE)
        .min(MAX_PARTIAL_RANGES);

    // Divide the remaining bytes into individual ranges.
    let range_size = remainder.size() / ranges;

    // Emit the ranges.
    Some((0..ranges).map(move |i| {
        let is_last = i + 1 == ranges;
        let start = remainder.start + i * range_size;
        let end = match is_last {
            false => start + range_size,
            true => remainder.end,
        };
        Range { start, end }
    }))
    .into_iter()
    .flatten()
}

/// Represents an HTTP Range header.
///
/// The string representation can be used as a range header.
///
/// Unlike the specification, this range cannot be open ended.
#[derive(Copy, Clone)]
pub struct Range {
    /// Start of the range, inclusive.
    pub start: u64,
    /// End of the range, inclusive.
    pub end: u64,
}

impl Range {
    /// Returns the amount of bytes the range contains.
    pub fn size(&self) -> u64 {
        if self.start > self.end {
            return 0;
        }
        // +1 because the end of the range is inclusive,
        // A 0-0 range is 1 byte in size.
        self.end - self.start + 1
    }
}

impl fmt::Debug for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bytes={}-{}", self.start, self.end)
    }
}

/// A parsed `Content-Range` header for `bytes`.
///
/// This implementation only supports the very basic format
/// where all values are present: `bytes <start>-<end>/<size>`.
///
/// All other variations like `bytes */<size>` are not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytesContentRange {
    /// Start of the returned range, offset in bytes.
    pub start: u64,
    /// End of the returned range, offset in bytes.
    pub end: u64,
    /// The total size of the resource on the server.
    pub total_size: u64,
}

impl BytesContentRange {
    /// Extracts the contained range into a [`Range`].
    pub fn range(self) -> Range {
        Range {
            start: self.start,
            end: self.end,
        }
    }

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
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_split {
        ($r:expr, @$expected:literal) => {{
            let s = split($r).collect::<Vec<_>>();
            insta::assert_debug_snapshot!(s, @$expected);

            if let Some(x) = s.last() {
                assert_eq!($r.total_size, x.end + 1);
            }

            s
        }};
    }

    #[test]
    fn test_split_nothing_to_do() {
        let r = BytesContentRange {
            start: 0,
            end: 11,
            total_size: 12,
        };

        assert_split!(r, @"[]");
    }

    #[test]
    fn test_split_too_small() {
        let r = BytesContentRange {
            start: 0,
            end: 9,
            total_size: 10 + PARTIAL_SPLIT_SIZE,
        };

        let s = split(r).collect::<Vec<_>>();
        insta::assert_debug_snapshot!(s, @r###"
        [
            bytes=10-109,
        ]
        "###);
    }

    #[test]
    fn test_split_two_parts() {
        let r = BytesContentRange {
            start: 0,
            end: 9,
            // 10 bytes for the original range, in order to split, we need to fit at least
            // two partial ranges into the remainder.
            total_size: 10 + PARTIAL_SPLIT_SIZE * 2,
        };

        assert_split!(r, @r###"
        [
            bytes=10-110,
            bytes=110-209,
        ]
        "###);
    }

    #[test]
    fn test_split_two_parts_not_exact() {
        let r = BytesContentRange {
            start: 0,
            end: 9,
            total_size: 10 + PARTIAL_SPLIT_SIZE * 2 - 1,
        };

        assert_split!(r, @r###"
        [
            bytes=10-109,
            bytes=109-208,
        ]
        "###);
    }

    #[test]
    fn test_split_too_many_parts() {
        let r = BytesContentRange {
            start: 0,
            end: 9,
            total_size: 10 + PARTIAL_SPLIT_SIZE * (MAX_PARTIAL_RANGES + 10),
        };

        assert_split!(r, @r###"
        [
            bytes=10-360,
            bytes=360-710,
            bytes=710-1060,
            bytes=1060-1409,
        ]
        "###);
    }

    #[test]
    fn test_byte_content_range_valid() {
        macro_rules! assert_bcr {
            ($s:literal = $start:literal-$end:literal/$size:literal) => {{
                assert_eq!(
                    BytesContentRange::from_str($s),
                    Ok(BytesContentRange {
                        start: $start,
                        end: $end,
                        total_size: $size,
                    })
                );
            }};
        }

        assert_bcr!("bytes 0-0/0" = 0 - 0 / 0);
        assert_bcr!("bytes 0-12/123" = 0 - 12 / 123);
        assert_bcr!("bytes 1-23/123" = 1 - 23 / 123);
        assert_bcr!("bytes 1-122/123" = 1 - 122 / 123);
        assert_bcr!("bytes     0-12/123" = 0 - 12 / 123);
        assert_bcr!("   bytes 0-12/123   " = 0 - 12 / 123);
        assert_bcr!("bytes 0- 12/ 123" = 0 - 12 / 123);
    }

    #[test]
    fn test_byte_content_range_invalid() {
        macro_rules! assert_bcr {
            ($s:literal, $v:ident) => {{
                assert_eq!(BytesContentRange::from_str($s), Err(InvalidBytesRange::$v));
            }};
        }

        assert_bcr!("bits 0-12/123", NotBytes);
        assert_bcr!("bytes */123", RangeNotSatisfiable);
        assert_bcr!("bytes 0-12/*", UnknownLength);
        assert_bcr!("bytes 0-12", Malformed);
        assert_bcr!("bytes a-12/123", Malformed);
        assert_bcr!("bytes 0-a/123", Malformed);
        assert_bcr!("bytes 23/123", Malformed);
        assert_bcr!("0-12/123", NotBytes);
    }
}
