use std::collections::{BTreeSet, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::{error::Error, path::PathBuf};

use crate::storage::engine::bplus_tree::{PageKind, header::IndexHeader};

use super::{
    PAGE_CRC_OFFSET, PAGE_SIZE, buffer_pool::BufferPool, crc, io::FileRef, slotted_page::Page,
    sync, wal::Wal,
};

const INDEX_POOL_CAPACITY: usize = 10_000;

pub(super) struct Index {
    pub index_file: FileRef,
    pub index_path: PathBuf,
    pub pool: BufferPool<Page>,
    pub next_id: AtomicU64,
    pub wal: Arc<Wal>,
    replay_pages: Mutex<HashSet<u64>>,
    free_pages: Mutex<BTreeSet<u64>>,
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
    pub fn new(
        index_file: FileRef,
        index_path: PathBuf,
        wal: Arc<Wal>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::new_with_capacity(index_file, index_path, wal, INDEX_POOL_CAPACITY)
    }

    pub fn new_with_capacity(
        index_file: FileRef,
        index_path: PathBuf,
        wal: Arc<Wal>,
        capacity: usize,
    ) -> Result<Self, Box<dyn Error>> {
        let next_id = index_file.len()? / PAGE_SIZE as u64;
        Ok(Index {
            index_file,
            index_path,
            pool: BufferPool::new(capacity),
            next_id: AtomicU64::new(next_id),
            wal,
            replay_pages: Mutex::new(HashSet::new()),
            free_pages: Mutex::new(BTreeSet::new()),
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

    fn write_back(&self, id: u64, page: &Page, page_lsn: u64) -> Result<(), Box<dyn Error>> {
        if page_lsn > 0 {
            self.wal.flush_up_to(page_lsn)?;
        }
        self.write_sealed(id, &page.data)
    }

    fn write_sealed(&self, id: u64, data: &[u8; PAGE_SIZE]) -> Result<(), Box<dyn Error>> {
        let mut sealed = *data;
        crc::seal_page(&mut sealed, PAGE_CRC_OFFSET);
        self.poisoning(self.index_file.write_at(&sealed, Self::page_offset(id)))
    }

    pub fn expect_replay(&self, ids: impl IntoIterator<Item = u64>) {
        sync::lock(&self.replay_pages).extend(ids);
    }

    pub fn end_replay(&self) {
        sync::lock(&self.replay_pages).clear();
    }

    fn cache(&self, id: u64, page: Page) -> Result<(), Box<dyn Error>> {
        let result = self
            .pool
            .insert_with_write_back(id, page, |vid, vpage, lsn| self.write_back(vid, vpage, lsn));
        self.poisoning(result)
    }

    pub fn ensure_page(&self, id: u64) -> Result<(), Box<dyn Error>> {
        let required = Self::page_offset(id) + PAGE_SIZE as u64;
        if self.index_file.len()? < required {
            self.poisoning(self.index_file.set_len(required))?;
        }
        self.next_id.fetch_max(id + 1, Ordering::SeqCst);
        Ok(())
    }

    pub fn allocate(&self, page_ty: PageKind) -> Result<u64, Box<dyn Error>> {
        self.wal.check()?;
        let reused = sync::lock(&self.free_pages).pop_first();
        let (id, old) = match reused {
            Some(id) => (id, self.fetch(id)?.data),
            None => {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let end_offset = Self::page_offset(id) + PAGE_SIZE as u64;
                if self.index_file.len()? < end_offset {
                    self.poisoning(self.index_file.set_len(end_offset))?;
                }
                (id, [0u8; PAGE_SIZE])
            }
        };

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

        let lsn = self
            .wal
            .log_page_change(0, true, id, &old, &buf)?
            .unwrap_or(0);

        self.pool.remove(id);
        self.cache(id, Page { data: buf })?;
        self.pool.mark_dirty(id);
        self.pool.update_lsn(id, lsn);

        Ok(id)
    }

    pub fn fetch(&self, id: u64) -> Result<Page, Box<dyn Error>> {
        self.wal.check()?;
        if let Some(page) = self.pool.get(id) {
            return Ok(page);
        }

        let mut buf = [0u8; PAGE_SIZE];
        self.index_file.read_at(&mut buf, Self::page_offset(id))?;
        if !crc::page_is_intact(&buf, PAGE_CRC_OFFSET)
            && !sync::lock(&self.replay_pages).contains(&id)
        {
            return Err(format!("corrupt index page {id}: checksum mismatch").into());
        }
        let page = Page { data: buf };
        self.cache(id, page)?;
        Ok(page)
    }

    pub fn with_page<R>(&self, id: u64, f: impl FnOnce(&Page) -> R) -> Result<R, Box<dyn Error>> {
        self.wal.check()?;
        let mut f = Some(f);
        if let Some(result) = self.pool.with(id, |page| (f.take().unwrap())(page)) {
            return Ok(result);
        }
        let page = self.fetch(id)?;
        Ok((f.take().unwrap())(&page))
    }

    pub fn fetch_mut(&self, id: u64) -> Result<PageGuard<'_>, Box<dyn Error>> {
        let page = self.fetch(id)?;
        self.pool.pin(id);
        self.pool.mark_dirty(id);
        Ok(PageGuard {
            index: self,
            id,
            page,
        })
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
            self.write_sealed(id, &data)?;
            self.poisoning(self.index_file.sync())?;
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
        self.poisoning(self.index_file.sync())?;
        self.pool.clear_dirty();
        Ok(())
    }

    pub fn sync_data(&self) -> Result<(), Box<dyn Error>> {
        self.poisoning(self.index_file.sync())?;
        Ok(())
    }

    pub fn release(&self, ids: impl IntoIterator<Item = u64>) {
        sync::lock(&self.free_pages).extend(ids);
    }

    pub fn reset_free(&self, free_pages: BTreeSet<u64>) {
        *sync::lock(&self.free_pages) = free_pages;
    }

    pub fn free_page_count(&self) -> usize {
        sync::lock(&self.free_pages).len()
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
