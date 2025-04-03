use reqwest::{header::HeaderName, StatusCode};

/// A parsed `Content-Range` header for `bytes`.
///
/// This implementation only supports the very basic format
/// where all values are present: `bytes <start>-<end>/<size>`.
///
/// All other variations like `bytes */<size>` are not supported.
#[derive(Debug)]
pub struct BytesContentRange {
    pub start: u64,
    pub end: u64,
    pub size: u64,
}

impl BytesContentRange {
    pub fn from_response(response: &reqwest::Response) -> Option<Self> {
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return None;
        }

        response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|hv| hv.to_str().ok())
            .and_then(Self::parse)
    }

    pub fn parse(header: &str) -> Option<Self> {
        let Some(("bytes", rest)) = header.trim().split_once(' ') else {
            return None;
        };

        let (range, size) = rest.trim().split_once('/')?;

        let (start, end) = range.split_once('-')?;
        let start = start.trim().parse().ok()?;
        let end = end.trim().parse().ok()?;

        let size = size.trim().parse().ok()?;

        Some(Self { start, end, size })
    }
}
