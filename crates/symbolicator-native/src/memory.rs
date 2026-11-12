use minidump::Minidump;
use scroll::ctx::{SizeWith, TryFromCtx};
use symbolic::common::ByteView;

pub use scroll::Endian;

/// Unified memory access for crash dumps.
pub trait MemoryAccess: std::fmt::Debug + Send + Sync {
    /// Attempts to lookup a memory range at the specified `addr` with the specified size.
    fn get_memory_at_address(&self, addr: u64, size: usize) -> Option<&'_ [u8]>;

    /// The endianness of the crash.
    fn endian(&self) -> Endian;
}

impl MemoryAccess for Minidump<'static, ByteView<'static>> {
    fn get_memory_at_address(&self, addr: u64, size: usize) -> Option<&'_ [u8]> {
        let memory = self.get_memory()?;
        let memory = memory.memory_at_address(addr)?;

        let start = addr.checked_sub(memory.base_address())? as usize;
        let end = start.checked_add(size)?;

        match memory {
            minidump::UnifiedMemory::Memory(region) => region.bytes.get(start..end),
            minidump::UnifiedMemory::Memory64(region) => region.bytes.get(start..end),
        }
    }

    fn endian(&self) -> Endian {
        self.endian
    }
}

/// Extension trait for [`MemoryAccess`].
pub trait MemoryAccessExt: MemoryAccess {
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

/// A [`MemoryAccess`] which is backed by an array.
#[derive(Debug)]
pub struct ArrayMemoryAccess<const N: usize> {
    memory: [u8; N],
    endian: Endian,
}

impl<const N: usize> ArrayMemoryAccess<N> {
    pub fn new(memory: [u8; N], endian: Endian) -> Self {
        Self { memory, endian }
    }
}

impl<const N: usize> MemoryAccess for ArrayMemoryAccess<N> {
    fn get_memory_at_address(&self, addr: u64, size: usize) -> Option<&'_ [u8]> {
        let start: usize = addr.try_into().ok()?;
        let end = start.checked_add(size)?;

        self.memory.get(start..end)
    }

    fn endian(&self) -> Endian {
        self.endian
    }
}
