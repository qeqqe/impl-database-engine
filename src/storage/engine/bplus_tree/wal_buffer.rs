//! Buffer pool for WAL works differently compared to buffer pools of data pages
//! Instead of relying on eviction policies like LRU/LFU/CLOCK, there are set of
//! events where we can trigger the flushing to the log file.
//!
//! - Transaction commit: On transaction commits we MUST fsync to disk, this is
//!   durability.
//!
//! - Data page flush: When a data page is about to get flushed to disk, it's
//!   an invariant of WAL to make sure any data page with LSN X that's about to
//!   be written to the disk, every log record in buffer poool with LSN <= X
//!   must already be flushed.
//!
//! - Log buffer full: Independent of any commit, this triggers when our buffer
//!   pool has grown over the max size.
//!
//! NOTE: About, checkpointing method; i'm leaning towards having fuzzy
//! checkpointing,

pub(crate) struct WalBuffer {
    pub(super) buffer: Vec<u8>,
    // when we have to flush till LSN x, we find the offset of LSN x by,
    // `x_off = offset[offset[0].0 - x].1`
    // now to find the relative offset in the buffer we do,
    // `buffer[0..x_off - offset[0].1]`
    pub(super) offsets: Vec<(u64, usize)>,
    pub(super) max_frames: usize,
}

impl WalBuffer {
    pub fn new(max_frames: usize) -> Self {
        Self {
            buffer: Vec::new(),
            offsets: Vec::new(),
            max_frames,
        }
    }

    pub fn is_full(&self) -> bool {
        self.offsets.len() >= self.max_frames
    }

    pub fn insert(&mut self, buf: &[u8], lsn: u64, rec_offset: usize) {
        self.buffer.extend_from_slice(buf);
        self.offsets.push((lsn, rec_offset));
    }

    // pub fn is_empty(&self) -> bool {
    //     self.offsets.is_empty()
    // }
    // pub fn len(&self) -> usize {
    //     self.offsets.len()
    // }
    // pub fn contains(&self, lsn: u64) -> bool {
    //     let (Some(first), Some(last)) = (self.offsets.first(), self.offsets.last()) else {
    //         return false;
    //     };
    //
    //     (first.0..=last.0).contains(&lsn)
    // }
    //
    // pub fn first_lsn(&self) -> Option<u64> {
    //     self.offsets.first().map(|&(lsn, _)| lsn)
    // }
    //
    // pub fn last_lsn(&self) -> Option<u64> {
    //     self.offsets.last().map(|&(lsn, _)| lsn)
    // }
}
