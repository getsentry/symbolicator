use minidump::Minidump;
use scroll::ctx::{SizeWith, TryFromCtx};
use symbolic::common::ByteView;

pub use scroll::Endian;

/// Unified memory access for crash dumps.
pub trait MemoryAccess: std::fmt::Debug + Send + Sync {
    /// Returns up to `max_size` bytes of memory starting at `addr`.
    ///
    /// Returns fewer bytes than `max_size` when the memory region containing `addr` ends before
    /// `addr + max_size`.
    fn get_memory_up_to(&self, addr: u64, max_size: usize) -> Option<&'_ [u8]>;

    /// The endianness of the crash.
    fn endian(&self) -> Endian;
}

impl MemoryAccess for Minidump<'static, ByteView<'static>> {
    fn get_memory_up_to(&self, addr: u64, max_size: usize) -> Option<&'_ [u8]> {
        let memory = self.get_memory()?;
        let memory = memory.memory_at_address(addr)?;

        let start = addr.checked_sub(memory.base_address())? as usize;
        let bytes = match memory {
            minidump::UnifiedMemory::Memory(region) => region.bytes,
            minidump::UnifiedMemory::Memory64(region) => region.bytes,
        };
        let end = start.saturating_add(max_size).min(bytes.len());

        bytes.get(start..end)
    }

    fn endian(&self) -> Endian {
        self.endian
    }
}

/// Slice-backed memory with addresses interpreted as byte offsets.
#[cfg(test)]
#[derive(Debug)]
pub struct SliceMemory<'a> {
    bytes: &'a [u8],
    endian: Endian,
}

#[cfg(test)]
impl<'a> SliceMemory<'a> {
    pub fn new(bytes: &'a [u8], endian: Endian) -> Self {
        Self { bytes, endian }
    }
}

#[cfg(test)]
impl MemoryAccess for SliceMemory<'_> {
    fn get_memory_up_to(&self, addr: u64, size: usize) -> Option<&[u8]> {
        let start = usize::try_from(addr).ok()?;
        let bytes = self.bytes.get(start..)?;
        bytes.get(..size.min(bytes.len()))
    }

    fn endian(&self) -> Endian {
        self.endian
    }
}

/// Extension trait for [`MemoryAccess`].
pub trait MemoryAccessExt: MemoryAccess {
    /// Attempts to lookup a memory range at the specified `addr` with the specified size.
    fn get_memory_at_address(&self, addr: u64, size: usize) -> Option<&'_ [u8]> {
        self.get_memory_up_to(addr, size)
            .filter(|bytes| bytes.len() == size)
    }

    /// Returns up to `max_count` values of type `T` starting at `addr`.
    ///
    /// Returns fewer values than `max_count` when the memory region containing `addr` ends before
    /// `max_count` values.
    fn get_values_up_to<T, const N: usize>(
        &self,
        addr: u64,
        max_count: usize,
    ) -> Option<MemoryView<'_, T>>
    where
        T: EndianBytes<Bytes = [u8; N]>,
    {
        let bytes = self.get_memory_up_to(addr, max_count.saturating_mul(N))?;
        Some(MemoryView::new(bytes, self.endian()))
    }

    /// Helper which accesses the memory of a dump and converts the memory to the specified type.
    fn get_value_at_address<T>(&self, addr: u64) -> Option<T>
    where
        T: SizeWith<Endian>,
        for<'a> T: TryFromCtx<'a, Endian>,
    {
        let endian = self.endian();
        let size = T::size_with(&endian);
        let memory = self.get_memory_at_address(addr, size)?;

        T::try_from_ctx(memory, endian).ok().map(|(value, _)| value)
    }
}

impl<T: MemoryAccess + ?Sized> MemoryAccessExt for T {}

/// An integer type that can be read from memory in a given byte order.
pub trait EndianBytes: Copy {
    /// The bytes of a value, `[u8; size_of::<Self>()]`.
    type Bytes: Copy;

    /// Converts `bytes`, stored in `endian` byte order, into a value.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used to read `wchar_t` strings")
    )]
    fn from_bytes(bytes: Self::Bytes, endian: Endian) -> Self;
}

macro_rules! impl_endian_bytes {
    ($($ty:ty => $size:literal),*) => {$(
        impl EndianBytes for $ty {
            type Bytes = [u8; $size];

            fn from_bytes(bytes: Self::Bytes, endian: Endian) -> Self {
                match endian {
                    Endian::Little => Self::from_le_bytes(bytes),
                    Endian::Big => Self::from_be_bytes(bytes),
                }
            }
        }
    )*};
}

impl_endian_bytes!(u8 => 1, u16 => 2, u32 => 4);

/// A view of memory as a sequence of `T` values.
pub struct MemoryView<'a, T: EndianBytes> {
    units: &'a [T::Bytes],
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used to read `wchar_t` strings")
    )]
    endian: Endian,
}

impl<'a, T, const N: usize> MemoryView<'a, T>
where
    T: EndianBytes<Bytes = [u8; N]>,
{
    /// Creates a view of `bytes`, which are stored in `endian` byte order.
    ///
    /// Trailing bytes that do not fill a whole `T` are not part of the view.
    fn new(bytes: &'a [u8], endian: Endian) -> Self {
        Self {
            units: bytes.as_chunks().0,
            endian,
        }
    }

    /// Returns an iterator over the values in the view.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used to read `wchar_t` strings")
    )]
    pub fn iter(&self) -> impl Iterator<Item = T> + 'a {
        let endian = self.endian;
        self.units
            .iter()
            .map(move |&bytes| T::from_bytes(bytes, endian))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_values_up_to_byte_order() {
        let bytes = [0x12, 0x34, 0x56, 0x78];

        let little = SliceMemory::new(&bytes, Endian::Little);
        let values: MemoryView<u16> = little.get_values_up_to(0, 2).unwrap();
        assert_eq!(values.iter().collect::<Vec<_>>(), [0x3412, 0x7856]);

        let big = SliceMemory::new(&bytes, Endian::Big);
        let values: MemoryView<u32> = big.get_values_up_to(0, 1).unwrap();
        assert_eq!(values.iter().collect::<Vec<_>>(), [0x12345678]);
    }

    #[test]
    fn test_get_values_up_to_counts_values() {
        let memory = SliceMemory::new(&[1, 0, 2, 0, 3, 0], Endian::Little);
        let values: MemoryView<u16> = memory.get_values_up_to(0, 2).unwrap();
        assert_eq!(values.iter().collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn test_get_values_up_to_drops_partial_value() {
        // The memory region ends one byte into the second value.
        let memory = SliceMemory::new(&[1, 0, 2], Endian::Little);
        let values: MemoryView<u16> = memory.get_values_up_to(0, 4).unwrap();
        assert_eq!(values.iter().collect::<Vec<_>>(), [1]);
    }
}
