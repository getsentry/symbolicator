use std::io::SeekFrom;

use crate::services::bitcode::BcSymbolMapHandle;

/// This is the legacy marker that was used previously to flag a SymCache that was created
/// using a`BcSymbolMap`.
const LEGACY_SYMBOLMAP_MARKER: &[u8] = b"WITH_SYMBOLMAP";

/// This marker denotes that a SymCache has appended markers.
const MARKERS32_MARKER: &[u8] = b"WITH_MARKERS32";

/// Encapsulation of all the source artifacts that are being used to create SymCaches.
#[derive(Clone, Debug, Default)]
pub struct SecondarySymCacheSources {
    pub bcsymbolmap_handle: Option<BcSymbolMapHandle>,
}

const MARKER_BCSYMBOLMAP: u32 = 1 << 0;

/// This is the markers that are being embedded into, and read from, a SymCache file.
#[derive(Debug, Default, PartialEq)]
pub struct SymCacheMarkers {
    markers: u32,
}

impl SymCacheMarkers {
    /// Gets all the markers for the given `sources`.
    pub fn from_sources(sources: &SecondarySymCacheSources) -> Self {
        let mut markers = 0;
        if sources.bcsymbolmap_handle.is_some() {
            markers |= MARKER_BCSYMBOLMAP;
        }
        Self { markers }
    }

    /// Extracts the markers embedded in the given `data`.
    ///
    /// This is lenient and will return an empty set of markers if there is a parse error.
    pub fn parse(data: &[u8]) -> Self {
        if data.ends_with(LEGACY_SYMBOLMAP_MARKER) {
            let markers = MARKER_BCSYMBOLMAP;
            return Self { markers };
        }

        fn parse_marker(data: &[u8]) -> Option<SymCacheMarkers> {
            let data = data.strip_suffix(MARKERS32_MARKER)?;

            let marker_offset = data.len().checked_sub(std::mem::size_of::<u32>())?;
            let marker_bytes = data.get(marker_offset..)?.try_into().ok()?;

            let markers = u32::from_le_bytes(marker_bytes);

            Some(SymCacheMarkers { markers })
        }
        parse_marker(data).unwrap_or_default()
    }

    /// Writes the markers to the end of `file`.
    pub fn write_to<F>(&self, mut file: F) -> std::io::Result<()>
    where
        F: std::io::Seek + std::io::Write,
    {
        if self.markers == 0 {
            return Ok(());
        }

        file.flush()?;
        file.seek(SeekFrom::End(0))?;

        file.write_all(&self.markers.to_le_bytes())?;

        file.write_all(MARKERS32_MARKER)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symbolic::common::ByteView;

    use super::*;

    #[test]
    fn test_legacy_marker() {
        let data = b"foobarWITH_SYMBOLMAP";
        let markers = SymCacheMarkers::parse(data);

        assert_eq!(markers.markers, MARKER_BCSYMBOLMAP);
    }

    #[test]
    fn test_marker_roundtrip() {
        let bcsymbolmap = BcSymbolMapHandle {
            uuid: Default::default(),
            data: ByteView::from_vec(vec![]),
        };
        let sources = SecondarySymCacheSources {
            bcsymbolmap_handle: Some(bcsymbolmap),
        };
        let markers = SymCacheMarkers::from_sources(&sources);

        let mut buf = Cursor::new(Vec::new());
        markers.write_to(&mut buf).unwrap();

        let buf = buf.into_inner();
        let parsed_markers = SymCacheMarkers::parse(&buf);

        assert_eq!(parsed_markers, markers);
    }

    #[test]
    fn test_empty_marker() {
        let data = b"\0\0\0\0WITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());
    }

    #[test]
    fn test_valid_but_unknown_marker() {
        let data = b"\0\x01\x01\0WITH_MARKERS32";
        assert_ne!(SymCacheMarkers::parse(data), SymCacheMarkers::default());
    }

    #[test]
    fn test_corrupted_marker() {
        let data = b"ITH_SYMBOLMAP";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());

        let data = b"ITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());

        let data = b"WITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());

        let data = b"\x01\0WITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());

        let data = b"\x01\0WITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());

        let data = b"\0\x01\0WITH_MARKERS32";
        assert_eq!(SymCacheMarkers::parse(data), SymCacheMarkers::default());
    }
}
