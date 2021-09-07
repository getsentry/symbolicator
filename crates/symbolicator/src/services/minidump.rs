use symbolic::minidump::processor::FrameTrust;

use crate::types;
use crate::utils::hex;

const MINIDUMP_EXTENSION_TYPE: u32 = u32::from_be_bytes([b'S', b'y', 0, 1]);
const MINIDUMP_FORMAT_VERSION: u32 = 1;

pub fn parse_stacktraces_from_minidump(
    buf: &[u8],
) -> Result<Vec<types::RawStacktrace>, WrappedError> {
    let dump = minidump::Minidump::read(buf)?;
    let extension_buf = dump.get_raw_stream(MINIDUMP_EXTENSION_TYPE)?;

    let parsed = parse_stacktraces_from_raw_extension(extension_buf)?;

    parsed
        .threads()
        .map(|thread| {
            let frames = thread
                .frames()?
                .map(|frame| {
                    let symbol = frame.symbol()?;
                    Ok(types::RawFrame {
                        instruction_addr: hex::HexValue(frame.instruction_addr()),
                        symbol: Some(symbol.to_owned()),
                        trust: FrameTrust::Prewalked,
                        ..Default::default()
                    })
                })
                .collect::<Result<Vec<_>, WrappedError>>()?;

            Ok(types::RawStacktrace {
                thread_id: Some(thread.thread_id() as u64),
                frames,
                ..Default::default()
            })
        })
        .collect()
}

fn parse_stacktraces_from_raw_extension(buf: &[u8]) -> Result<format::Format, WrappedError> {
    let parsed = format::Format::parse(buf)?;
    Ok(parsed)
}

#[derive(Debug)]
pub enum WrappedError {
    MinidumpError(minidump::Error),
    FormatError(format::Error),
}

impl From<minidump::Error> for WrappedError {
    fn from(err: minidump::Error) -> Self {
        Self::MinidumpError(err)
    }
}

impl From<format::Error> for WrappedError {
    fn from(err: format::Error) -> Self {
        Self::FormatError(err)
    }
}

// TODO: well, doc comments ;-)
mod format {
    use super::*;
    use std::{mem, ptr};

    // TODO: create more variants for:
    // - buffer/header is invalid (buffer not big enough)
    // - indexes are broken (index out of bounds from threads/frames iterator)
    // - wrapped utf-8 error, or maybe figure out how we parse symbols right now
    //   ^ or maybe we use `from_utf8_lossy` in other places?
    #[derive(Debug)]
    pub struct Error;

    #[derive(Debug)]
    pub struct Format<'data> {
        header: &'data Header,
        threads: &'data [RawThread],
        frames: &'data [RawFrame],
        symbol_bytes: &'data [u8],
    }

    impl<'data> Format<'data> {
        /// Parse our custom minidump extension binary format
        ///
        /// TODO: add a better explanation of the format ;-)
        /// ^ how everything is laid out one-after-the-other in memory, how indexing works, etc
        ///
        /// The binary format looks a bit like this:
        /// - Header
        /// - some padding for alignment
        /// - num_threads Thread
        /// - some padding for alignment
        /// - num_frames Frame
        ///   - thread0 frame0 <- RawThread.start_frame = 0
        ///   - thread0 frame1 <- RawThread.num_frames = 1
        ///   - thread1 frame0
        ///   - thread1 frame1
        /// - some padding for alignment
        /// - symbol_bytes
        pub fn parse(buf: &'data [u8]) -> Result<Self, Error> {
            let mut header_size = mem::size_of::<Header>();
            header_size += align_to_eight(header_size);

            if buf.len() < header_size {
                return Err(Error);
            }

            // SAFETY: we will check validity of the header down below
            let header = unsafe { &*(buf.as_ptr() as *const Header) };
            if header.version != MINIDUMP_FORMAT_VERSION {
                return Err(Error);
            }

            let mut threads_size = mem::size_of::<RawThread>() * header.num_threads as usize;
            threads_size += align_to_eight(threads_size);

            let mut frames_size = mem::size_of::<RawFrame>() * header.num_frames as usize;
            frames_size += align_to_eight(frames_size);

            let expected_buf_size =
                header_size + threads_size + frames_size + header.symbol_bytes as usize;

            if buf.len() != expected_buf_size {
                return Err(Error);
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
                header,
                threads,
                frames,
                symbol_bytes,
            })
        }

        pub fn threads(&self) -> impl Iterator<Item = Thread> {
            self.threads.iter().map(move |raw_thread| Thread {
                format: self,
                thread: raw_thread,
            })
        }
    }

    pub struct Thread<'data> {
        format: &'data Format<'data>,
        thread: &'data RawThread,
    }

    impl Thread<'_> {
        pub fn thread_id(&self) -> u32 {
            self.thread.thread_id
        }
        pub fn frames(&self) -> Result<impl Iterator<Item = Frame>, Error> {
            let range = self.thread.start_frame as usize
                ..self.thread.start_frame as usize + self.thread.num_frames as usize;
            let frames = self.format.frames.get(range).ok_or(Error)?;

            Ok(frames.iter().map(move |raw_frame| Frame {
                format: self.format,
                frame: raw_frame,
            }))
        }
    }

    pub struct Frame<'data> {
        format: &'data Format<'data>,
        frame: &'data RawFrame,
    }

    impl Frame<'_> {
        pub fn instruction_addr(&self) -> u64 {
            self.frame.instruction_addr
        }

        pub fn symbol(&self) -> Result<&str, Error> {
            let range = self.frame.symbol_offset as usize
                ..self.frame.symbol_offset as usize + self.frame.symbol_len as usize;
            let bytes = self.format.symbol_bytes.get(range).ok_or(Error)?;

            std::str::from_utf8(bytes).map_err(|_| Error)
        }
    }

    #[derive(Debug)]
    #[repr(C)]
    struct Header {
        version: u32,
        num_threads: u32,
        num_frames: u32,
        symbol_bytes: u32,
    }

    #[derive(Debug)]
    #[repr(C)]
    pub struct RawThread {
        thread_id: u32,
        start_frame: u32,
        num_frames: u32,
    }

    #[derive(Debug)]
    #[repr(C)]
    pub struct RawFrame {
        instruction_addr: u64,
        symbol_offset: u32,
        symbol_len: u32,
    }
}

/// Returns the remainder if the input isn't a multiple of 8.
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

    use test_assembler::*;

    #[test]
    fn test_simple_minidump() {
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
            .D32(5) // the first symbol goes from 0-5
            .D64(0xfffff7002) // first instr_addr
            .D32(0)
            .D32(5) // the first symbol goes from 0-5
            .D64(0xfffff7003) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 0-5
            .D64(0xfffff7004) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 0-5
            .D64(0xfffff7006) // first instr_addr
            .D32(5)
            .D32(6) // the first symbol goes from 0-5
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
        assert_eq!(frame.symbol().unwrap(), "uiaeo");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7002);
        assert_eq!(frame.symbol().unwrap(), "uiaeo");
        assert!(frames.next().is_none());

        // second thread with 3 frames
        let thread = threads.next().unwrap();
        assert_eq!(thread.thread_id(), 2345);
        let mut frames = thread.frames().unwrap();
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7003);
        assert_eq!(frame.symbol().unwrap(), "snrtdy");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7004);
        assert_eq!(frame.symbol().unwrap(), "snrtdy");
        let frame = frames.next().unwrap();
        assert_eq!(frame.instruction_addr(), 0xfffff7006);
        assert_eq!(frame.symbol().unwrap(), "snrtdy");
        assert!(frames.next().is_none());

        assert!(threads.next().is_none());
    }
}
