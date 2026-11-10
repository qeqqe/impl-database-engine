pub(crate) const HEAP_CRC_OFFSET: usize = 21;
pub(crate) const HEAP_HEADER_SIZE: usize = HEAP_CRC_OFFSET + 4;
pub(crate) const WAL_HEADER_SIZE: usize = 40;
pub(crate) const WAL_MAGIC: [u8; 4] = *b"IWAL";

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PageKind {
    Root = 0,
    Internal = 1,
    Leaf = 2,
}

impl PageKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(PageKind::Root),
            1 => Some(PageKind::Internal),
            2 => Some(PageKind::Leaf),
            _ => None,
        }
    }
}

#[repr(C)]
pub(super) struct IndexHeader {
    pub id: u64,           // 0..8
    pub free_start: u16,   // 8..10
    pub free_end: u16,     // 10..12
    pub page_ty: PageKind, // 12
    pub flags: u8,         // 13, flag_bits[0] == primary_page, flage_bits[1] == has_overflow_page
    /// NOTE: ptr will behave as a Rightmost Pointers
    /// when the page is an Internal/Root node
    /// else as a right sibling leaf pointer when a Leaf node...
    pub ptr: u64, // 14..22
} // 22, then the page checksum at 22..26

impl IndexHeader {
    pub fn deserialize(buf: &[u8]) -> Option<Self> {
        let header = IndexHeader {
            id: u64::from_le_bytes(buf.get(0..8)?.try_into().ok()?),
            free_start: u16::from_le_bytes(buf.get(8..10)?.try_into().ok()?),
            free_end: u16::from_le_bytes(buf.get(10..12)?.try_into().ok()?),
            page_ty: PageKind::from_u8(*buf.get(12)?)?,
            flags: u8::from_le(*buf.get(13)?),
            ptr: u64::from_le_bytes(buf.get(14..22)?.try_into().ok()?),
        };
        let start = header.free_start as usize;
        let end = header.free_end as usize;
        let well_formed = start >= super::HEADER_SIZE
            && start <= end
            && end <= super::PAGE_SIZE
            && (start - super::HEADER_SIZE).is_multiple_of(super::SLOT_SIZE);
        well_formed.then_some(header)
    }

    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.id.to_le_bytes());
        buf[8..10].copy_from_slice(&self.free_start.to_le_bytes());
        buf[10..12].copy_from_slice(&self.free_end.to_le_bytes());
        buf[12] = self.page_ty as u8;
        buf[13] = self.flags;
        buf[14..22].copy_from_slice(&self.ptr.to_le_bytes());
    }

    pub fn is_root_leaf(&self) -> bool {
        self.flags & 0x01 == 0x01
    }

    pub fn set_root_leaf(&mut self) {
        self.flags |= 0x01;
    }

    pub fn clear_root_leaf(&mut self) {
        self.flags &= !0x01;
    }
}

///     (same for the page header)
///
///     |----------------|
///     | file header    |   
///     |----------------|
///     | page header    |   
///     |----------------|
///     | cell pointer   |   |  4 bytes per cell pointer.  Sorted order.
///     | array          |   |  Grows downward
///     |                |   v
///     |----------------|
///     | unallocated    |
///     | space          |
///     |----------------|   ^  Grows upwards
///     | cell content   |   |  Arbitrary order interspersed with freeblocks.
///     | area           |   |  and free space fragments.
///     |----------------|
///
///     (referenced from sqlite)
pub(super) struct HeapHeader {
    pub id: u64,         // 0..8
    pub free_start: u16, // 8..10
    pub free_end: u16,   // 10..12
    pub flags: u8,       // 12
    /// `ptr` will also behave differently depending
    /// on the flags.
    /// As a convention, if the first bit of the flag is 1,
    /// that would mean that THIS page is a primary page, else it's an overflow
    /// page.
    ///
    /// If the second bit of the flag is 1 that would mean that this page HAS
    /// an overflow page.
    ///
    /// So depending on the flags we have this combination (FB = flag bit)
    ///
    /// FB\[0\] == true && FB\[1\] == false: A primary page, where `ptr`
    /// indicates nothing and shouldn't be read.
    ///
    /// FB\[0\] == true && FB\[1\] == true: A primary page that has an overflow
    /// page, where `ptr` indicated the page id of the overflow'd page.
    ///
    /// FB\[0\] == false && FB\[1\] == true: An overflow page that has an
    /// overflow page, where `ptr` indicated the page id of the overflow'd page.
    ///
    /// FB\[0\] == false && FB\[1\] == false: An overflow page that doesn't have
    /// an overflow page, where `ptr` indicates nothing.
    pub ptr: u64,
} // 21, then the page checksum at 21..25

impl HeapHeader {
    pub fn deserialize(buf: &[u8]) -> Option<Self> {
        let header = HeapHeader {
            id: u64::from_le_bytes(buf.get(0..8)?.try_into().ok()?),
            free_start: u16::from_le_bytes(buf.get(8..10)?.try_into().ok()?),
            free_end: u16::from_le_bytes(buf.get(10..12)?.try_into().ok()?),
            flags: u8::from_le(*buf.get(12)?),
            ptr: u64::from_le_bytes(buf.get(13..21)?.try_into().ok()?),
        };
        let start = header.free_start as usize;
        let end = header.free_end as usize;
        let well_formed = start >= HEAP_HEADER_SIZE
            && start <= end
            && end <= super::heap::PAGE_SIZE
            && (start - HEAP_HEADER_SIZE).is_multiple_of(super::heap::SLOT_SIZE);
        well_formed.then_some(header)
    }

    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.id.to_le_bytes());
        buf[8..10].copy_from_slice(&self.free_start.to_le_bytes());
        buf[10..12].copy_from_slice(&self.free_end.to_le_bytes());
        buf[12] = self.flags;
        buf[13..21].copy_from_slice(&self.ptr.to_le_bytes());
    }

    pub fn remaining_space(&self) -> usize {
        (self.free_end as usize).saturating_sub(self.free_start as usize + super::heap::SLOT_SIZE)
    }

    pub fn new_primary(id: u64) -> Self {
        HeapHeader {
            id,
            free_start: HEAP_HEADER_SIZE as u16,
            free_end: super::heap::PAGE_SIZE as u16,
            flags: 0x01, // first bit high = pirmary key
            ptr: 0,
        }
    }

    pub fn new_overflow(id: u64) -> Self {
        HeapHeader {
            id,
            free_start: HEAP_HEADER_SIZE as u16,
            free_end: super::heap::PAGE_SIZE as u16,
            flags: 0x00, // first bit low = overflow key
            ptr: 0,
        }
    }
    pub fn set_has_overflow(&mut self, overflow_page_id: u64) {
        self.flags |= 0x02; // second bit high = overflow
        self.ptr = overflow_page_id;
    }

    pub fn is_overflow_page(&self) -> bool {
        self.flags & 0x01 == 0
    }

    pub fn has_overflow_page(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WalHeader {
    pub salt: u32,
    pub last_checkpoint_lsn: u64,
    pub last_wal_offset: u64, // we can use this to trace the last WAL and walk back
    pub last_wal_len: u32,
    pub next_lsn: u64, // NOTE: future me, make sure you flush this only when
                       //  the buffer pool is supposed to be flushed (forced by
                       //  the data page) or checkpointing..
}

impl WalHeader {
    pub fn deserialize(buf: &[u8]) -> Option<Self> {
        if buf.len() < WAL_HEADER_SIZE || buf[0..4] != WAL_MAGIC {
            return None;
        }
        let stored = u32::from_le_bytes(buf[36..40].try_into().ok()?);
        if super::crc::crc32c(&buf[0..36]) != stored {
            return None;
        }
        Some(Self {
            salt: u32::from_le_bytes(buf[4..8].try_into().ok()?),
            last_checkpoint_lsn: u64::from_le_bytes(buf[8..16].try_into().ok()?),
            last_wal_offset: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            last_wal_len: u32::from_le_bytes(buf[24..28].try_into().ok()?),
            next_lsn: u64::from_le_bytes(buf[28..36].try_into().ok()?),
        })
    }

    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&WAL_MAGIC);
        buf[4..8].copy_from_slice(&self.salt.to_le_bytes());
        buf[8..16].copy_from_slice(&self.last_checkpoint_lsn.to_le_bytes());
        buf[16..24].copy_from_slice(&self.last_wal_offset.to_le_bytes());
        buf[24..28].copy_from_slice(&self.last_wal_len.to_le_bytes());
        buf[28..36].copy_from_slice(&self.next_lsn.to_le_bytes());
        let crc = super::crc::crc32c(&buf[0..36]);
        buf[36..40].copy_from_slice(&crc.to_le_bytes());
    }
}
