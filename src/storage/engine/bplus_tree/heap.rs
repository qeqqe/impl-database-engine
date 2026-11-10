//! This is an interface for accessing/modifying the heap organized file.
//! This implements the slotted page heap file, basically the slotted pages
//! on index file but instead of containing key/child_ptr this holds the actual
//! data record. all the actual data offsets of the page will be organized by the
//! b+ trees's disk persisted index file and will map the corresponding data
//! record in heap through the slotted page's cell Header Pointer.

// TODO: implement overflow pages cus they are bound to exist

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::buffer_pool::BufferPool;
use super::crc;
use super::header::HEAP_CRC_OFFSET;
use super::io::FileRef;
use super::slotted_page::HeapPage;
use super::sync::lock;
use super::wal::Wal;

use super::{header::HeapHeader, slotted_page::CellPointer};

pub(super) const PAGE_SIZE: usize = 8192;
pub(super) const HEADER_SIZE: usize = super::header::HEAP_HEADER_SIZE;
pub(super) const SLOT_SIZE: usize = 4;
pub(super) const MAX_RECORD_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;
pub(super) const SHARED_MAX_BLOB: usize = (PAGE_SIZE - HEADER_SIZE) / 2;
pub(super) const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;
pub(super) const CHAIN_SLOT: u16 = u16::MAX;
const MIN_TRACKED_FREE: usize = 64;

pub(super) fn pack_ptr(page: u64, slot: u16) -> u64 {
    (page << 16) | slot as u64
}

pub(super) fn unpack_ptr(ptr: u64) -> (u64, u16) {
    (ptr >> 16, (ptr & 0xFFFF) as u16)
}

pub(super) fn encoded_blob_len(records: &[Vec<u8>]) -> usize {
    4 + records.iter().map(|r| 4 + r.len()).sum::<usize>()
}

pub(super) fn encode_blob(records: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = records.iter().map(|r| 4 + r.len()).sum();
    let mut out = Vec::with_capacity(4 + total);
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for record in records {
        out.extend_from_slice(&(record.len() as u32).to_le_bytes());
        out.extend_from_slice(record);
    }
    out
}

pub(super) fn decode_blob(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    let mut rest = bytes;
    let mut take = |n: usize| -> Result<&[u8], Box<dyn Error>> {
        if rest.len() < n {
            return Err("heap blob is truncated".into());
        }
        let (head, tail) = rest.split_at(n);
        rest = tail;
        Ok(head)
    };
    let count = u32::from_le_bytes(take(4)?.try_into()?) as usize;
    let mut records = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = u32::from_le_bytes(take(4)?.try_into()?) as usize;
        records.push(take(len)?.to_vec());
    }
    if !rest.is_empty() {
        return Err("heap blob has trailing bytes".into());
    }
    Ok(records)
}

const HEAP_POOL_CAPACITY: usize = 10_000;

pub(super) struct Heap {
    pub heap_file: FileRef,
    pub path: std::path::PathBuf,
    pub next_id: AtomicU64,
    pub pool: BufferPool<HeapPage>,
    pub wal: Arc<Wal>,
    free_pages: Mutex<BTreeSet<u64>>,
    shared_space: Mutex<BTreeMap<u64, usize>>,
    replay_pages: Mutex<HashSet<u64>>,
}

pub(super) struct HeapPageGuard<'a> {
    heap: &'a Heap,
    id: u64,
    page: HeapPage,
}

impl<'a> Deref for HeapPageGuard<'a> {
    type Target = HeapPage;
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

impl<'a> DerefMut for HeapPageGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.page
    }
}

impl<'a> Drop for HeapPageGuard<'a> {
    fn drop(&mut self) {
        self.heap.pool.update_page(self.id, self.page);
        self.heap.pool.unpin(self.id);
    }
}

impl Heap {
    pub fn new(
        heap_file: FileRef,
        path: std::path::PathBuf,
        wal: Arc<Wal>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::new_with_capacity(heap_file, path, wal, HEAP_POOL_CAPACITY)
    }

    pub fn new_with_capacity(
        heap_file: FileRef,
        path: std::path::PathBuf,
        wal: Arc<Wal>,
        capacity: usize,
    ) -> Result<Self, Box<dyn Error>> {
        let next_id = heap_file.len()? / PAGE_SIZE as u64;
        Ok(Heap {
            heap_file,
            path,
            next_id: AtomicU64::new(next_id),
            pool: BufferPool::new(capacity),
            wal,
            free_pages: Mutex::new(BTreeSet::new()),
            shared_space: Mutex::new(BTreeMap::new()),
            replay_pages: Mutex::new(HashSet::new()),
        })
    }

    fn poisoning<T, E: Into<Box<dyn Error>>>(
        &self,
        result: Result<T, E>,
    ) -> Result<T, Box<dyn Error>> {
        result.map_err(|e| {
            self.wal.poison();
            e.into()
        })
    }

    fn write_back(&self, id: u64, page: &HeapPage, page_lsn: u64) -> Result<(), Box<dyn Error>> {
        if page_lsn > 0 {
            self.wal.flush_up_to(page_lsn)?;
        }
        self.write_sealed(id, &page.data)
    }

    fn write_sealed(&self, id: u64, data: &[u8; PAGE_SIZE]) -> Result<(), Box<dyn Error>> {
        let mut sealed = *data;
        crc::seal_page(&mut sealed, HEAP_CRC_OFFSET);
        self.poisoning(self.heap_file.write_at(&sealed, Self::page_offset(id)))
    }

    pub fn expect_replay(&self, ids: impl IntoIterator<Item = u64>) {
        lock(&self.replay_pages).extend(ids);
    }

    pub fn end_replay(&self) {
        lock(&self.replay_pages).clear();
    }

    fn cache(&self, id: u64, page: HeapPage) -> Result<(), Box<dyn Error>> {
        let result = self
            .pool
            .insert_with_write_back(id, page, |vid, vpage, lsn| self.write_back(vid, vpage, lsn));
        self.poisoning(result)
    }

    pub fn ensure_page(&self, id: u64) -> Result<(), Box<dyn Error>> {
        let required = Self::page_offset(id) + PAGE_SIZE as u64;
        if self.heap_file.len()? < required {
            self.poisoning(self.heap_file.set_len(required))?;
        }
        self.next_id.fetch_max(id + 1, Ordering::SeqCst);
        Ok(())
    }

    pub fn get_record(
        &self,
        id: u64,
        data_records: &mut Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        let page = self.fetch(id)?;
        let header =
            HeapHeader::deserialize(&page.data).ok_or("Couldn't deserialize the heap header")?;

        let cell_ptrs = Self::get_cell_ptr(&page.data, &header);
        for cell_ptr in &cell_ptrs {
            data_records.push(Self::cell_bytes(&page.data, &header, cell_ptr)?.to_vec());
        }

        let has_overflow = header.has_overflow_page();
        let overflow_ptr = header.ptr;

        if has_overflow {
            self.collect_overflow_records(overflow_ptr, data_records)?;
        }

        Ok(())
    }

    pub fn collect_overflow_records(
        &self,
        overflow_id: u64,
        data_records: &mut Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        let mut cur_id = overflow_id;
        let mut steps = 0;
        loop {
            self.check_chain_step(&mut steps, cur_id)?;
            let page = self.fetch(cur_id)?;
            let header = page.header()?;
            let cell_ptrs = Self::get_cell_ptr(&page.data, &header);
            for cell_ptr in &cell_ptrs {
                data_records.push(Self::cell_bytes(&page.data, &header, cell_ptr)?.to_vec());
            }

            if header.has_overflow_page() {
                cur_id = header.ptr;
            } else {
                break;
            }
        }

        Ok(())
    }

    fn cell_bytes<'p>(
        buf: &'p [u8],
        header: &HeapHeader,
        cell_ptr: &CellPointer,
    ) -> Result<&'p [u8], Box<dyn Error>> {
        let start = cell_ptr.cell_offset as usize;
        let end = start + cell_ptr.cell_size as usize;
        if start < header.free_end as usize || end > PAGE_SIZE {
            return Err(format!(
                "corrupt heap page {}: record at {start}..{end} lies outside the record area",
                header.id
            )
            .into());
        }
        Ok(&buf[start..end])
    }

    fn check_chain_step(&self, steps: &mut u64, id: u64) -> Result<(), Box<dyn Error>> {
        *steps += 1;
        if *steps > self.page_count() || id >= self.page_count() {
            return Err(format!(
                "corrupt heap chain: page {id} is out of range or part of a cycle"
            )
            .into());
        }
        Ok(())
    }

    fn get_cell_ptr(buf: &[u8], header: &HeapHeader) -> Vec<CellPointer> {
        let range = (header.free_start - HEADER_SIZE as u16) / 4;
        // NOTE: here we can derive that a single cellpointer is a
        // data member's pointer of a row.
        let mut cell_ptr: Vec<CellPointer> = Vec::with_capacity(range as usize);

        for i in 0..range {
            cell_ptr.push(Self::slot(i, buf));
        }

        cell_ptr
    }

    pub fn fetch(&self, id: u64) -> Result<HeapPage, Box<dyn Error>> {
        self.wal.check()?;
        if let Some(page) = self.pool.get(id) {
            return Ok(page);
        }
        let page = self.read_page_from_disk(id)?;
        self.cache(id, page)?;
        Ok(page)
    }

    pub fn with_page<R>(
        &self,
        id: u64,
        f: impl FnOnce(&HeapPage) -> R,
    ) -> Result<R, Box<dyn Error>> {
        self.wal.check()?;
        let mut f = Some(f);
        if let Some(result) = self.pool.with(id, |page| (f.take().unwrap())(page)) {
            return Ok(result);
        }
        let page = self.read_page_from_disk(id)?;
        self.cache(id, page)?;
        Ok((f.take().unwrap())(&page))
    }

    pub fn fetch_mut(&self, id: u64) -> Result<HeapPageGuard<'_>, Box<dyn Error>> {
        let page = self.fetch(id)?;
        self.pool.pin(id);
        self.pool.mark_dirty(id);
        Ok(HeapPageGuard {
            heap: self,
            id,
            page,
        })
    }

    pub fn write_page(&self, id: u64, page: HeapPage) {
        self.pool.update_page(id, page);
    }

    pub fn allocate(&self) -> Result<u64, Box<dyn Error>> {
        self.wal.check()?;
        if let Some(id) = lock(&self.free_pages).pop_first() {
            return Ok(id);
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let end_offset = Self::page_offset(id) + PAGE_SIZE as u64;
        if self.heap_file.len()? < end_offset {
            self.poisoning(self.heap_file.set_len(end_offset))?;
        }

        let page = HeapPage {
            data: [0u8; PAGE_SIZE],
        };

        self.pool.remove(id);
        self.cache(id, page)?;
        self.pool.mark_dirty(id);

        Ok(id)
    }

    pub fn release(&self, ids: impl IntoIterator<Item = u64>) {
        lock(&self.free_pages).extend(ids);
    }

    pub fn find_shared_page(&self, needed: usize) -> Option<u64> {
        lock(&self.shared_space)
            .iter()
            .find(|(_, free)| **free >= needed)
            .map(|(id, _)| *id)
    }

    pub fn note_shared_space(&self, id: u64, free: usize) {
        let mut space = lock(&self.shared_space);
        if free >= MIN_TRACKED_FREE {
            space.insert(id, free);
        } else {
            space.remove(&id);
        }
    }

    pub fn forget_shared_page(&self, id: u64) {
        lock(&self.shared_space).remove(&id);
    }

    pub fn reset_space(&self, free_pages: BTreeSet<u64>, shared: Vec<(u64, usize)>) {
        *lock(&self.free_pages) = free_pages;
        let mut space = lock(&self.shared_space);
        space.clear();
        for (id, free) in shared {
            if free >= MIN_TRACKED_FREE {
                space.insert(id, free);
            }
        }
    }

    pub fn page_count(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst)
    }

    pub fn free_page_count(&self) -> usize {
        lock(&self.free_pages).len()
    }

    pub fn allocate_primary(&self) -> Result<u64, Box<dyn Error>> {
        let id = self.allocate()?;
        let mut page = self.fetch_mut(id)?;
        let header = HeapHeader::new_primary(id);
        header.serialize(&mut page.data[..HEADER_SIZE]);
        Ok(id)
    }

    pub fn allocate_overflow(&self) -> Result<u64, Box<dyn Error>> {
        let id = self.allocate()?;
        let mut page = self.fetch_mut(id)?;
        let header = HeapHeader::new_overflow(id);
        header.serialize(&mut page.data[..HEADER_SIZE]);
        Ok(id)
    }

    pub fn insert_records(
        &self,
        primary_page_id: u64,
        data_record: Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        let header = {
            let page = self.fetch(primary_page_id)?;
            page.header()?
        };

        let mut current_id = primary_page_id;

        if header.has_overflow_page() {
            current_id = self.find_tail_id(header.ptr)?;
        }

        for record in data_record {
            let d_len = record.len();
            let needed = d_len + SLOT_SIZE;

            let remaining = {
                let page = self.fetch(current_id)?;
                page.header()?.remaining_space()
            };

            if remaining >= needed {
                let mut page = self.fetch_mut(current_id)?;
                page.add_cell(record)?;
            } else {
                let overflow_id = self.allocate_overflow()?;

                {
                    let mut current_page = self.fetch_mut(current_id)?;
                    let mut hdr = current_page.header()?;
                    hdr.set_has_overflow(overflow_id);
                    hdr.serialize(&mut current_page.data[..HEADER_SIZE]);
                }

                {
                    let mut overflow_page = self.fetch_mut(overflow_id)?;
                    overflow_page.add_cell(record)?;
                }

                current_id = overflow_id;
            }
        }

        Ok(())
    }

    fn read_page_from_disk(&self, id: u64) -> Result<HeapPage, Box<dyn Error>> {
        let mut buf = [0u8; PAGE_SIZE];
        self.heap_file.read_at(&mut buf, Self::page_offset(id))?;
        if !crc::page_is_intact(&buf, HEAP_CRC_OFFSET) && !lock(&self.replay_pages).contains(&id) {
            return Err(format!("corrupt heap page {id}: checksum mismatch").into());
        }
        Ok(HeapPage { data: buf })
    }

    pub fn flush(&self, id: u64) -> Result<(), Box<dyn Error>> {
        if self.pool.is_dirty(id)
            && let Some(page) = self.pool.get(id)
        {
            let lsn = self.pool.page_lsn(id);
            if lsn > 0 {
                self.wal.flush_up_to(lsn)?;
            }
            let data = page.data;
            self.write_sealed(id, &data)?;
            self.poisoning(self.heap_file.sync())?;
            self.pool.clear_dirty_single(id);
        }
        Ok(())
    }

    pub fn flush_all(&self) -> Result<(), Box<dyn Error>> {
        let dirty_ids = self.pool.dirty_page_ids();
        for id in dirty_ids {
            if let Some(page) = self.pool.get(id) {
                let lsn = self.pool.page_lsn(id);
                if lsn > 0 {
                    self.wal.flush_up_to(lsn)?;
                }
                let data = page.data;
                self.write_sealed(id, &data)?;
            }
        }
        self.poisoning(self.heap_file.sync())?;
        self.pool.clear_dirty();
        Ok(())
    }

    pub fn sync_data(&self) -> Result<(), Box<dyn Error>> {
        self.poisoning(self.heap_file.sync())?;
        Ok(())
    }

    pub fn discard_dirty(&self) {
        let dirty_ids = self.pool.dirty_page_ids();
        for id in dirty_ids {
            self.pool.remove(id);
        }
    }

    fn find_tail_id(&self, id: u64) -> Result<u64, Box<dyn Error>> {
        let mut cur_id = id;
        let mut steps = 0;
        loop {
            self.check_chain_step(&mut steps, cur_id)?;
            let hdr = {
                let page = self.fetch(cur_id)?;
                page.header()?
            };
            if hdr.has_overflow_page() {
                cur_id = hdr.ptr;
            } else {
                return Ok(cur_id);
            }
        }
    }

    /// returns the chain of ptr from primary to the last overflow page
    pub fn free_chain(&self, primary_id: u64) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut chain = vec![primary_id];
        let mut cur_id = primary_id;
        let mut steps = 0;
        loop {
            self.check_chain_step(&mut steps, cur_id)?;
            let hdr = {
                let page = self.fetch(cur_id)?;
                page.header()?
            };
            if hdr.has_overflow_page() {
                cur_id = hdr.ptr;
                chain.push(cur_id);
            } else {
                break;
            }
        }
        Ok(chain)
    }

    fn slot(i: u16, buf: &[u8]) -> CellPointer {
        let off = HEADER_SIZE + i as usize * SLOT_SIZE;

        CellPointer {
            cell_offset: u16::from_le_bytes(buf[off..off + 2].try_into().unwrap()),
            cell_size: u16::from_le_bytes(buf[off + 2..off + 4].try_into().unwrap()),
        }
    }

    fn page_offset(id: u64) -> u64 {
        PAGE_SIZE as u64 * id
    }
}
