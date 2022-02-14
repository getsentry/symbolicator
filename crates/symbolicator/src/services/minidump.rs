//! Code for extracting our custom minidump extension for client-side stack traces.
//!
//! The extension is a minidump stream with the ID `0x53790001`.
//!
//! The format comprises:
//! - a header
//! - a list of threads
//! - a list of frames
//! - a symbol data section
//!
//! The data is currently written and read in host-native endianness. A mismatch will result in a
//! [`WrongVersion`](format::Error::WrongVersion) error.
//!
//! # Structure
//!
//! The header is 16 bytes long and structured as follows:
//!
//! ```text
//! bytes     0                     3 4                     7
//!          +-----------------------+-----------------------+
//!          | version number      1 | number of threads     |
//!          +-----------------------+-----------------------+
//!          | number of frames      | length of symbol data |
//!          +-----------------------+-----------------------+
//! ```
//!
//! The thread list as a whole is aligned to 8 bytes.
//! Each thread in the thread list is 12 bytes long, aligned to 4 bytes, and
//! structured as follows:
//!
//! The indices are 0-based and reference the n-th frame inside the global `frames` list.
//!
//! ```text
//! bytes     0                     3 4                     7
//!          +-----------------------+-----------------------+
//!          | thread id             | index of first frame  |
//!          +-----------------------+-----------------------+
//!          | number of frames      |
//!          +-----------------------+
//! ```
//!
//! Each frame in the frame list is 16 bytes long, aligned to 8 bytes, and
//! structured as follows:
//!
//! The symbol start is 0-based and references a sub-slice of the global `symbols` data.
//!
//! ```text
//! bytes     0                     3 4                     7
//!          +-----------------------+-----------------------+
//!          | instruction address                           |
//!          +-----------------------+-----------------------+
//!          | symbol start          | symbol length         |
//!          +-----------------------+-----------------------+
//! ```
//!
//! The symbol data section contains the concatenated raw symbol names of all frames.
//!
//! Symbols are not `\0`-terminated, and although the raw format does not mandate any specific
//! encoding, the symbols are being parsed as UTF-8 data.
//!
//! # Example
//!
//! The following diagram shows an example stack trace with 3 threads and 2 frames:
//!
//! ```text
//! bytes     0                     3 4                     7
//!          +-----------------------+-----------------------+
//! header   | version             1 | threads             3 |
//!          +-----------------------+-----------------------+
//!          | frames              2 | symbol bytes       10 |
//!          +-----------------------+-----------------------+
//! thread 0 | thread id         123 | first frame         0 |
//!          +-----------------------+-----------------------+
//! thread 1 | frames              2 | thread id         321 |
//!          +-----------------------+-----------------------+
//!          | first frame         2 | frames              0 |
//!          +-----------------------+-----------------------+
//! thread 2 | thread id          17 | first frame         2 |
//!          +-----------------------+-----------------------+
//!          | frames              0 | padding             0 |
//!          +-----------------------+-----------------------+
//! frame 0  | instruction address                      1337 |
//!          +-----------------------+-----------------------+
//!          | symbol start        0 | symbol length       6 |
//!          +-----------------------+-----------------------+
//! frame 1  | instruction address                0xdeadbeef |
//!          +-----------------------+-----------------------+
//!          | symbol start        6 | symbol length       4 |
//!          +-----------------------+-----------------------+
//! symbols  |  _  |  s  |  t  |  a  |  r  |  t  |  m  |  a  |
//!          +-----------------------+-----------------------+
//!          |  i  |  n  |
//!          +-----------+
//! ```

use std::convert::TryFrom;
use std::fmt;

use symbolic::minidump::processor::FrameTrust;
use thiserror::Error;

use crate::types;
use crate::utils::hex;

const MINIDUMP_EXTENSION_TYPE: u32 = u32::from_be_bytes([b'S', b'y', 0, 1]);
const MINIDUMP_FORMAT_VERSION: u32 = 1;

/// Extract client-side stacktraces from a minidump file.
pub fn parse_stacktraces_from_minidump(
    buf: &[u8],
) -> Result<Option<Vec<types::RawStacktrace>>, ExtractStacktraceError> {
    let dump = minidump::Minidump::read(buf)?;
    let extension_buf = match dump.get_raw_stream(MINIDUMP_EXTENSION_TYPE) {
        Ok(stream) => stream,
        Err(minidump::Error::StreamNotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let parsed = parse_stacktraces_from_raw_extension(extension_buf)?;
    let stacktraces = parsed
        .threads()
        .map(types::RawStacktrace::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(stacktraces))
}

fn parse_stacktraces_from_raw_extension(
    buf: &[u8],
) -> Result<format::Format, ExtractStacktraceError> {
    format::Format::parse(buf).map_err(ExtractStacktraceError::from)
}

impl TryFrom<format::Thread<'_>> for types::RawStacktrace {
    type Error = ExtractStacktraceError;

    fn try_from(thread: format::Thread) -> Result<Self, Self::Error> {
        let frames = thread
            .frames()?
            .map(types::RawFrame::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(types::RawStacktrace {
            thread_id: Some(thread.thread_id() as u64),
            frames,
            ..Default::default()
        })
    }
}

impl TryFrom<format::Frame<'_>> for types::RawFrame {
    type Error = ExtractStacktraceError;

    fn try_from(frame: format::Frame) -> Result<Self, Self::Error> {
        let symbol = frame.symbol()?;
        Ok(types::RawFrame {
            instruction_addr: hex::HexValue(frame.instruction_addr()),
            function: Some(String::from_utf8_lossy(symbol).into_owned()),
            trust: FrameTrust::Prewalked,
            ..Default::default()
        })
    }
}

#[derive(Debug, Error)]
pub enum ExtractStacktraceError {
    MinidumpError(minidump::Error),
    FormatError(#[from] format::Error),
}

impl From<minidump::Error> for ExtractStacktraceError {
    fn from(err: minidump::Error) -> Self {
        Self::MinidumpError(err)
    }
}

impl fmt::Display for ExtractStacktraceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::FormatError(e) => e.fmt(f),
            Self::MinidumpError(e) => e.fmt(f),
        }
    }
}

mod format {
    use super::*;
    use std::{mem, ptr};

    #[derive(Debug, Error)]
    pub enum Error {
        /// The extension version in the header is wrong/outdated.
        #[error("wrong or outdated version")]
        WrongVersion,
        /// The header's size doesn't match our expected size.
        #[error("header is too small")]
        HeaderTooSmall,
        /// The self-advertised size of the extension is not correct.
        #[error("incorrect extension length")]
        BadFormatLength,
        /// A derived index for a frame or a set of frames is out of bounds.
        /// Includes the ID of the thread the frames are associated with.
        #[error("frame index out of bounds for thread {0}")]
        FrameIndexOutOfBounds(u32),
        /// A derived index for a symbol or a set of symbols is out of bounds.
        /// Includes the instruction address of the frame the symbol is associated
        /// with.
        #[error("symbol index out of bounds for instruction address {0}")]
        SymbolIndexOutOfBounds(u64),
    }

    #[derive(Debug)]
    pub struct Format<'data> {
        threads: &'data [RawThread],
        frames: &'data [RawFrame],
        symbol_bytes: &'data [u8],
    }

    impl<'data> Format<'data> {
        /// Parse our custom minidump extension binary format.
        ///
        /// See the [parent module documentation](super) for an explanation of the binary format.
        pub fn parse(buf: &'data [u8]) -> Result<Self, Error> {
            let mut header_size = mem::size_of::<RawHeader>();
            header_size += align_to_eight(header_size);

            if buf.len() < header_size {
                return Err(Error::HeaderTooSmall);
            }

            // SAFETY: we will check validity of the header down below
            let header = unsafe { &*(buf.as_ptr() as *const RawHeader) };
            if header.version != MINIDUMP_FORMAT_VERSION {
                return Err(Error::WrongVersion);
            }

            let mut threads_size = mem::size_of::<RawThread>() * header.num_threads as usize;
            threads_size += align_to_eight(threads_size);

            let mut frames_size = mem::size_of::<RawFrame>() * header.num_frames as usize;
            frames_size += align_to_eight(frames_size);

            let expected_buf_size =
                header_size + threads_size + frames_size + header.symbol_bytes as usize;

            if buf.len() != expected_buf_size {
                return Err(Error::BadFormatLength);
            }

            // SAFETY: we just made sure that all the pointers we are constructing via pointer
            // arithmetic are within `buf`
            let threads_start = unsafe { buf.as_ptr().add(header_size) };
            let frames_start = unsafe { threads_start.add(threads_size) };
            let symbols_start = unsafe { frames_start.add(frames_size) };

            // SAFETY: the above buffer size check also made sure we are not going out of bounds
            // here
            let threads = unsafe {
                &*(ptr::slice_from_raw_parts(threads_start, header.num_threads as usize)
                    as *const [RawThread])
            };
            let frames = unsafe {
                &*(ptr::slice_from_raw_parts(frames_start, header.num_frames as usize)
                    as *const [RawFrame])
            };
            let symbol_bytes = unsafe {
                &*(ptr::slice_from_raw_parts(symbols_start, header.symbol_bytes as usize)
                    as *const [u8])
            };

            Ok(Format {
                threads,
                frames,
                symbol_bytes,
            })
        }

        /// An [`Iterator`] of [`Thread`] objects that are part of the extension.
        pub fn threads(&self) -> impl Iterator<Item = Thread> {
            self.threads.iter().map(move |raw_thread| Thread {
                format: self,
                thread: raw_thread,
            })
        }
    }

    /// A convenience wrapper around a raw [`Thread`] contained in the minidump extension.
    pub struct Thread<'data> {
        format: &'data Format<'data>,
        thread: &'data RawThread,
    }

    impl Thread<'_> {
        /// The Thread ID
        pub fn thread_id(&self) -> u32 {
            self.thread.thread_id
        }

        /// An [`Iterator`] of [`Frame`] objects associated with this [`Thread`].
        ///
        /// Returns [`Error::FrameIndexOutOfBounds`] when the frame indices are out of bounds.
        pub fn frames(&self) -> Result<impl Iterator<Item = Frame>, Error> {
            let start_frame = self.thread.start_frame as usize;
            let end_frame = self.thread.start_frame as usize + self.thread.num_frames as usize;

            let frames = self
                .format
                .frames
                .get(start_frame..end_frame)
                .ok_or_else(|| Error::FrameIndexOutOfBounds(self.thread_id()))?;

            Ok(frames.iter().map(move |raw_frame| Frame {
                format: self.format,
                frame: raw_frame,
            }))
        }
    }

    /// A convenience wrapper around a raw [`Frame`] contained in the minidump extension.
    pub struct Frame<'data> {
        format: &'data Format<'data>,
        frame: &'data RawFrame,
    }

    impl Frame<'_> {
        /// The Instruction Address of the Frame.
        pub fn instruction_addr(&self) -> u64 {
            self.frame.instruction_addr
        }

        /// The raw symbol bytes of this [`Frame`].
        ///
        /// These bytes should be parsable as UTF-8.
        ///
        /// Returns [`Error::SymbolIndexOutOfBounds`] when the symbol indices are out of bounds.
        pub fn symbol(&self) -> Result<&[u8], Error> {
            let start_symbol = self.frame.symbol_offset as usize;
            let end_symbol = start_symbol + self.frame.symbol_len as usize;
            let bytes = self
                .format
                .symbol_bytes
                .get(start_symbol..end_symbol)
                .ok_or_else(|| Error::SymbolIndexOutOfBounds(self.instruction_addr()))?;

            Ok(bytes)
        }
    }

    #[derive(Debug)]
    #[repr(C)]
    struct RawHeader {
        version: u32,
        num_threads: u32,
        num_frames: u32,
        symbol_bytes: u32,
    }

    #[derive(Debug)]
    #[repr(C)]
    struct RawThread {
        thread_id: u32,
        start_frame: u32,
        num_frames: u32,
    }

    #[derive(Debug)]
    #[repr(C)]
    struct RawFrame {
        instruction_addr: u64,
        symbol_offset: u32,
        symbol_len: u32,
    }

    #[test]
    fn test_raw_structs() {
        assert_eq!(mem::size_of::<RawHeader>(), 16);
        assert_eq!(mem::align_of::<RawHeader>(), 4);

        assert_eq!(mem::size_of::<RawThread>(), 12);
        assert_eq!(mem::align_of::<RawThread>(), 4);

        assert_eq!(mem::size_of::<RawFrame>(), 16);
        assert_eq!(mem::align_of::<RawFrame>(), 8);
    }
}

/// Returns the amount left to add to the remainder to get 8 if
/// `to_align` isn't a multiple of 8.
fn align_to_eight(to_align: usize) -> usize {
    let remainder = to_align % 8;
    if remainder == 0 {
        remainder
    } else {
        8 - remainder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::*;
    use test_assembler::*;

    // Exposing Raw[Header|Frame|Thread] and implementing helpers
    // that construct sections out of them would make these tests
    // significantly more readable.

    #[test]
    fn test_simple_extension() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(2) // 2 threads
            .D32(5) // with 5 frames in total
            .D32(11) // and some symbol bytes
            .D32(1234) // first thread id
            .D32(0)
            .D32(2) // first two frames belong to thread 0
            .D32(2345) // second thread id
            .D32(2)
            .D32(3) // last three frames belong to thread 1
            .D64(0xfffff7001) // first instr_addr
            .D32(0)
            .D32(5) // the first symbol goes from 0-5 (0+5)
            .D64(0xfffff7002) // first instr_addr
            .D32(0)
            .D32(5) // the first symbol goes from 0-5 (0+5)
            .D64(0xfffff7003) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 6-11 (5+6)
            .D64(0xfffff7004) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 6-11 (5+6)
            .D64(0xfffff7006) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 6-11 (5+6)
            .append_bytes(b"uiaeosnrtdy");
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();

        // first thread with 2 frames
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);
        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7001);
        assert_eq!(frame.symbol().unwrap(), b"uiaeo");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7002);
        assert_eq!(frame.symbol().unwrap(), b"uiaeo");
        assert!(frames.next().is_none());

        // second thread with 3 frames
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 2345);
        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7003);
        assert_eq!(frame.symbol().unwrap(), b"snrtdy");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7004);
        assert_eq!(frame.symbol().unwrap(), b"snrtdy");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7006);
        assert_eq!(frame.symbol().unwrap(), b"snrtdy");
        assert!(frames.next().is_none());

        assert!(threads.next().is_none());
    }

    #[test]
    fn test_padded_extension() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(1) // with 1 frame
            .D32(4) // and some symbol bytes
            .D32(1234) // first thread id
            .D32(0)
            .D32(1)
            .D32(0) // padding for alignment
            .D64(0xfffff7001) // first frame, first instr_addr
            .D32(0)
            .D32(4) // the first symbol goes from 0-4
            .append_bytes(b"honk");
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();

        // first thread with 1 frame
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);
        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7001);
        assert_eq!(frame.symbol().unwrap(), b"honk");

        assert!(frames.next().is_none());
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_empty_extension() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(0)
            .D32(0)
            .D32(0);
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();
        let mut threads = parsed.threads();
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_only_thread() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(0) // 0 frames
            .D32(0) // 0 symbol bytes
            .D32(1234) // first thread id
            .D32(0)
            .D32(0) // 0 frames
            .D32(0); // padding for alignment
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();

        // first thread with 0 frames
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);
        let mut frames = thread.frames().unwrap();
        assert!(frames.next().is_none());
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_only_frames() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(0) // 0 threads
            .D32(1) // with 1 frame
            .D32(0) // and 0 symbol bytes
            .D64(0xfffff7001) // first frame, first instr_addr
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_only_symbols() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(0) // 0 threads
            .D32(0) // 0 frames
            .D32(4) // and 4 symbol bytes
            .append_bytes(b"beep");

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_only_thread_and_frames() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(1) // with 1 frame
            .D32(0) // and 0 symbol bytes
            .D32(1234) // first thread id
            .D32(0) // start at frame 0
            .D32(1) // 1 frame
            .D32(0) // padding for alignment
            .D64(0xfffff7001) // first frame, first instr_addr
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);

        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7001);
        assert_eq!(frame.symbol().unwrap(), b"");

        assert!(frames.next().is_none());
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_only_thread_and_symbols() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(0) // with 0 frames
            .D32(4) // and 4 symbol bytes
            .D32(1234) // first thread id
            .D32(0) // start at frame 0
            .D32(0) // 0 frames
            .D32(0) // padding for alignment
            .append_bytes(b"honk");

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);

        let mut frames = thread.frames().unwrap();
        assert!(frames.next().is_none());
    }

    #[test]
    fn test_only_frames_and_symbols() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(0) // 0 threads
            .D32(1) // with 1 frame
            .D32(4) // and 4 symbol bytes
            .D64(0xfffff7001) // first frame, first instr_addr
            .D32(0)
            .D32(4) // the first symbol goes from 0-4
            .append_bytes(b"honk");

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        assert!(threads.next().is_none());
    }

    #[test]
    fn test_undersized_header() {
        let section = Section::new().D32(MINIDUMP_FORMAT_VERSION).D32(0).D32(0);
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf);
        assert!(matches!(
            parsed,
            Err(ExtractStacktraceError::FormatError(
                format::Error::HeaderTooSmall
            ))
        ));
    }

    #[test]
    fn test_mismatched_header_values() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(2) // 2 frames
            .D32(0); // 0 bytes
        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf);
        assert!(matches!(
            parsed,
            Err(ExtractStacktraceError::FormatError(
                format::Error::BadFormatLength
            ))
        ));
    }

    #[test]
    fn test_malformed_frame_boundaries() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(1) // 1 frame
            .D32(0) // and 0 symbol bytes
            .D32(1234) // first thread id
            .D32(0) // start at frame 0
            .D32(2) // 2 frames (!!!)
            .D32(0) // padding for alignment
            .D64(0xfffff7001) // frame instr_addr
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);

        let frames = thread.frames();

        assert!(matches!(
            frames,
            Err(format::Error::FrameIndexOutOfBounds(1234))
        ));
    }

    #[test]
    fn test_malformed_symbol_boundaries() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(1) // 1 frame
            .D32(4) // and 0 symbol bytes
            .D32(1234) // first thread id
            .D32(0) // start at frame 0
            .D32(1) // 1 frame
            .D32(0) // padding for alignment
            .D64(0xfffff7001) // frame instr_addr
            .D32(0)
            .D32(10) // symbol at index 0-10 (!!!)
            .append_bytes(b"honk");

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);

        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7001);
        let symbol = frame.symbol();

        assert!(matches!(
            symbol,
            Err(format::Error::SymbolIndexOutOfBounds(0xfffff7001))
        ));
    }

    #[test]
    fn test_bad_symbol_bytes() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(1) // 1 thread
            .D32(1) // 1 frame
            .D32(7) // and 7 symbol bytes
            .D32(1234) // first thread id
            .D32(0) // start at frame 0
            .D32(1) // 1 frame
            .D32(0) // padding for alignment
            .D64(0xfffff7001) // frame instr_addr
            .D32(0)
            .D32(7)
            .append_bytes(b"ho\xF0\x90\x80nk");

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf).unwrap();

        let mut threads = parsed.threads();
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 1234);

        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7001);
        let symbol = frame.symbol().unwrap();
        assert_eq!(symbol, b"ho\xF0\x90\x80nk");
    }

    //// The lying header series. The header and the contents deliberately do not match up but
    //// the contents are still parseable, causing bad threads and frames to be parsed out.

    #[test]
    fn test_lying_header() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(4) // 4 threads
            .D32(0) // 0 frames
            .D32(0) // and 0 symbol bytes
            .D64(0xfffff7001) // first instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7002) // second instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7003) // third instr_addr
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf);
        println!("{:#?}", parsed);
        // frames are parsed as threads
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_another_lying_header() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(0) // 0 threads
            .D32(3) // "3" frames
            .D32(0) // and 0 symbol bytes
            .D32(1111) // first thread id
            .D32(0)
            .D32(0)
            .D32(0)
            .D32(2222) // second thread id
            .D32(0)
            .D32(0)
            .D32(0)
            .D32(3333) // third thread id
            .D32(0)
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf);
        println!("{:#?}", parsed);
        // threads are parsed as frames
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_yet_another_lying_header() {
        let section = Section::new()
            .D32(MINIDUMP_FORMAT_VERSION)
            .D32(4) // 4 threads
            .D32(3) // "3" frames
            .D32(0) // and 0 symbol bytes
            .D64(0xfffff7001) // first instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7002) // second instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7003) // third instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7004) // fourth instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7005) // fifth instr_addr
            .D32(0)
            .D32(0)
            .D64(0xfffff7006) // sixth instr_addr
            .D32(0)
            .D32(0);

        let buf = section.get_contents().unwrap();

        let parsed = parse_stacktraces_from_raw_extension(&buf);
        println!("{:#?}", parsed);
        assert!(parsed.is_ok());
    }
}
