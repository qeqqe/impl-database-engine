use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{error::Error, fs::File, os::unix::fs::FileExt, path::PathBuf};

use crate::storage::engine::bplus_tree::{PageKind, header::IndexHeader};

use super::{PAGE_SIZE, buffer_pool::BufferPool, slotted_page::Page, wal::Wal};

const INDEX_POOL_CAPACITY: usize = 10_000;

pub(super) struct Index {
    pub index_file: File,
    pub index_path: PathBuf,
    pub pool: BufferPool<Page>,
    pub next_id: AtomicU64,
    pub wal: Arc<Wal>,
}

pub(super) struct PageGuard<'a> {
    index: &'a Index,
    id: u64,
    page: Page,
}

impl<'a> Deref for PageGuard<'a> {
    type Target = Page;
    fn deref(&self) -> &Self::Target {
        &self.page
    }
}

impl<'a> DerefMut for PageGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.page
    }
}

impl<'a> Drop for PageGuard<'a> {
    fn drop(&mut self) {
        self.index.pool.update_page(self.id, self.page);
        self.index.pool.unpin(self.id);
    }
}

impl Index {
    pub fn new(index_file: File, index_path: PathBuf, wal: Arc<Wal>) -> Self {
        Self::new_with_capacity(index_file, index_path, wal, INDEX_POOL_CAPACITY)
    }

    pub fn new_with_capacity(
        index_file: File,
        index_path: PathBuf,
        wal: Arc<Wal>,
        capacity: usize,
    ) -> Self {
        let next_id = index_file
            .metadata()
            .map(|m| m.len() / PAGE_SIZE as u64)
            .unwrap_or(0);
        Index {
            index_file,
            index_path,
            pool: BufferPool::new(capacity),
            next_id: AtomicU64::new(next_id),
            wal,
        }
    }

    pub fn allocate(&self, page_ty: PageKind) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let end_offset = Self::page_offset(id) + PAGE_SIZE as u64;
        self.index_file
            .set_len(end_offset)
            .expect("Failed to set length of heap file");

        let header = IndexHeader {
            id,
            free_start: super::HEADER_SIZE as u16,
            free_end: PAGE_SIZE as u16,
            flags: 0,
            ptr: 0,
            page_ty,
        };

        let mut buf = [0u8; PAGE_SIZE];
        header.serialize(&mut buf);

        if let Some(evicted) = self
            .pool
            .insert(id, Page { data: buf })
            .expect("buf pool capacity exceeded during allocate... all pages pinned...)")
            && evicted.was_dirty
        {
            if evicted.page_lsn > 0 {
                self.wal
                    .flush_up_to(evicted.page_lsn)
                    .expect("Failed to flush WAL during allocate");
            }
            self.index_file
                .write_all_at(&evicted.page.data, Self::page_offset(evicted.id))
                .expect("Failed to write-back stolen page during allocate");
        }
        self.pool.mark_dirty(id);

        id
    }

    pub fn fetch(&self, id: u64) -> Page {
        if let Some(page) = self.pool.get(id) {
            return page;
        }

        let mut buf = [0u8; PAGE_SIZE];
        self.index_file
            .read_exact_at(&mut buf, Self::page_offset(id))
            .unwrap();
        let page = Page { data: buf };
        if let Some(evicted) = self
            .pool
            .insert(id, page)
            .expect("buf pool capacity exceeded during index fetch... all pages pinned...")
            && evicted.was_dirty
        {
            if evicted.page_lsn > 0 {
                self.wal
                    .flush_up_to(evicted.page_lsn)
                    .expect("Failed to flush WAL during fetch");
            }
            self.index_file
                .write_all_at(&evicted.page.data, Self::page_offset(evicted.id))
                .expect("Failed to write-back stolen page during fetch");
        }
        page
    }

    pub fn fetch_mut(&self, id: u64) -> PageGuard<'_> {
        let page = self.fetch(id);
        self.pool.pin(id);
        self.pool.mark_dirty(id);
        PageGuard {
            index: self,
            id,
            page,
        }
    }

    pub fn write_page(&self, id: u64, page: Page) {
        self.pool.update_page(id, page);
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
            self.index_file.write_all_at(&data, Self::page_offset(id))?;
            self.index_file.sync_data()?;
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
                self.index_file.write_all_at(&data, Self::page_offset(id))?;
            }
        }
        self.index_file.sync_data()?;
        self.pool.clear_dirty();
        Ok(())
    }

    pub fn sync_data(&self) -> Result<(), Box<dyn Error>> {
        self.index_file.sync_data()?;
        Ok(())
    }

    pub fn discard_dirty(&self) {
        let dirty_ids = self.pool.dirty_page_ids();
        for id in dirty_ids {
            self.pool.remove(id);
        }
    }

    fn page_offset(id: u64) -> u64 {
        id * PAGE_SIZE as u64
    }
}
