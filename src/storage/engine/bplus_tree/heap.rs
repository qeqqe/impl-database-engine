//! This is an interface for accessing/modifying the heap organized file.
//! This implements the slotted page heap file, basically the slotted pages
//! on index file but instead of containing key/child_ptr this holds the actual
//! data record. all the actual data offsets of the page will be organized by the
//! b+ trees's disk persisted index file and will map the corresponding data
//! record in heap through the slotted page's cell Header Pointer.

// TODO: implement overflow pages cus they are bound to exist

use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{error::Error, os::unix::fs::FileExt};

use crate::storage::engine::bplus_tree::header::HEAP_HEADER_SIZE;

use super::buffer_pool::BufferPool;
use super::slotted_page::HeapPage;
use super::wal::Wal;

use super::{header::HeapHeader, slotted_page::CellPointer};

pub(super) const PAGE_SIZE: usize = 8192;
pub(super) const HEADER_SIZE: usize = 21;
pub(super) const SLOT_SIZE: usize = 4;

const HEAP_POOL_CAPACITY: usize = 10_000;

pub(super) struct Heap {
    pub heap_file: File,
    pub path: std::path::PathBuf,
    pub next_id: AtomicU64,
    pub pool: BufferPool<HeapPage>,
    pub wal: Arc<Wal>,
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
    pub fn new(heap_file: File, path: std::path::PathBuf, wal: Arc<Wal>) -> Self {
        Self::new_with_capacity(heap_file, path, wal, HEAP_POOL_CAPACITY)
    }

    pub fn new_with_capacity(
        heap_file: File,
        path: std::path::PathBuf,
        wal: Arc<Wal>,
        capacity: usize,
    ) -> Self {
        let next_id = heap_file
            .metadata()
            .map(|m| m.len() / PAGE_SIZE as u64)
            .unwrap_or(0);
        Heap {
            heap_file,
            path,
            next_id: AtomicU64::new(next_id),
            pool: BufferPool::new(capacity),
            wal,
        }
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
            let off = cell_ptr.cell_offset as usize;
            let size = cell_ptr.cell_size as usize;
            let mut data = vec![0u8; size];
            data.copy_from_slice(&page.data[off..off + size]);
            data_records.push(data);
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
        loop {
            let page = self.fetch(cur_id)?;
            let header = page.header()?;
            let cell_ptrs = Self::get_cell_ptr(&page.data, &header);
            for cell_ptr in cell_ptrs {
                let start = cell_ptr.cell_offset as usize;
                let end = start + cell_ptr.cell_size as usize;
                let mut data_record = vec![0u8; end - start];
                data_record.copy_from_slice(&page.data[start..end]);
                data_records.push(data_record);
            }

            if header.has_overflow_page() {
                cur_id = header.ptr;
            } else {
                break;
            }
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
        if let Some(page) = self.pool.get(id) {
            return Ok(page);
        }
        let page = self.read_page_from_disk(id)?;
        if let Some(evicted) = self.pool.insert(id, page)?
            && evicted.was_dirty
        {
            if evicted.page_lsn > 0 {
                self.wal.flush_up_to(evicted.page_lsn)?;
            }
            self.heap_file
                .write_all_at(&evicted.page.data, Self::page_offset(evicted.id))?;
        }
        Ok(page)
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

    pub fn allocate(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let end_offset = Self::page_offset(id) + PAGE_SIZE as u64;
        self.heap_file
            .set_len(end_offset)
            .expect("Failed to set length of heap file");

        let header = HeapHeader {
            id,
            ptr: 0,
            free_start: HEAP_HEADER_SIZE as u16,
            free_end: PAGE_SIZE as u16,
            flags: 0,
        };
        let mut page = HeapPage {
            data: [0u8; PAGE_SIZE],
        };
        header.serialize(&mut page.data[..HEADER_SIZE]);

        if let Some(evicted) = self
            .pool
            .insert(id, page)
            .expect("Buffer pool capacity exceeded during allocate (all pages pinned)")
            && evicted.was_dirty
        {
            if evicted.page_lsn > 0 {
                self.wal
                    .flush_up_to(evicted.page_lsn)
                    .expect("Failed to flush WAL during allocate");
            }
            self.heap_file
                .write_all_at(&evicted.page.data, Self::page_offset(evicted.id))
                .expect("Failed to write-back stolen page during allocate");
        }
        self.pool.mark_dirty(id);

        id
    }

    pub fn allocate_primary(&self) -> u64 {
        let id = self.allocate();
        let mut page = self.fetch_mut(id).unwrap();
        let header = HeapHeader::new_primary(id);
        header.serialize(&mut page.data[..HEADER_SIZE]);
        id
    }

    pub fn allocate_overflow(&self) -> u64 {
        let id = self.allocate();
        let mut page = self.fetch_mut(id).unwrap();
        let header = HeapHeader::new_overflow(id);
        header.serialize(&mut page.data[..HEADER_SIZE]);
        id
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

        if header.is_overflow_page() {
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
                let overflow_id = self.allocate_overflow();

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
        self.heap_file
            .read_exact_at(&mut buf, Self::page_offset(id))?;
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
            self.heap_file.write_all_at(&data, Self::page_offset(id))?;
            self.heap_file.sync_data()?;
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
                self.heap_file.write_all_at(&data, Self::page_offset(id))?;
            }
        }
        self.heap_file.sync_data()?;
        self.pool.clear_dirty();
        Ok(())
    }

    pub fn sync_data(&self) -> Result<(), Box<dyn Error>> {
        self.heap_file.sync_data()?;
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
        loop {
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
        loop {
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
