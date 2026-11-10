//! The buffer pool is based on STEAL NO-FORCE policy,which means that mutated
//! page buffers CAN be flushed to disk even before the transaction has
//! commited, but only  if the condition `PageLSN <= FlushedLSN` is satisfied.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Mutex;

use super::crc::crc32c;
use super::sync::lock;

type WriteBack<'a, P> = dyn FnMut(u64, &P, u64) -> Result<(), Box<dyn Error>> + 'a;

pub(super) struct BufferPool<P> {
    inner: Mutex<BufferPoolInner<P>>,
}

struct BufferPoolInner<P> {
    frames: HashMap<u64, FrameEntry<P>>,
    dirty: HashMap<u64, bool>,
    lru: BTreeMap<u64, u64>,
    clock: u64,
    max_frames: usize,
}

struct FrameEntry<P> {
    page: P,
    pin_count: u32,
    page_lsn: u64,
    rec_lsn: u64,
    tick: u64,
    logged_crc: u32,
}

pub(super) struct EvictedPage<P> {
    pub id: u64,
    pub page: P,
    pub was_dirty: bool,
    pub page_lsn: u64,
    pub rec_lsn: u64,
}

impl<P: Clone + AsRef<[u8]>> BufferPoolInner<P> {
    fn contains(&self, id: u64) -> bool {
        self.frames.contains_key(&id)
    }

    fn get(&mut self, id: u64) -> Option<P> {
        self.touch(id);
        self.frames.get(&id).map(|e| e.page.clone())
    }

    fn with<R>(&mut self, id: u64, f: impl FnOnce(&P) -> R) -> Option<R> {
        self.touch(id);
        self.frames.get(&id).map(|e| f(&e.page))
    }

    fn insert(&mut self, id: u64, page: P) -> Result<Option<EvictedPage<P>>, Box<dyn Error>> {
        if self.frames.contains_key(&id) {
            self.touch(id);
            return Ok(None);
        }

        let evicted = if self.frames.len() >= self.max_frames {
            match self.evict_one() {
                Some(e) => Some(e),
                None => return Err("Buffer pool full: all pages are pinned".into()),
            }
        } else {
            None
        };

        self.frames.insert(
            id,
            FrameEntry {
                logged_crc: crc32c(page.as_ref()),
                page,
                pin_count: 0,
                page_lsn: 0,
                rec_lsn: 0,
                tick: 0,
            },
        );
        self.touch(id);

        Ok(evicted)
    }

    fn insert_with_write_back(
        &mut self,
        id: u64,
        page: P,
        write_back: &mut WriteBack<'_, P>,
    ) -> Result<(), Box<dyn Error>> {
        if self.frames.contains_key(&id) {
            self.touch(id);
            return Ok(());
        }

        while self.frames.len() >= self.max_frames {
            let Some((tick, victim)) = self.victim() else {
                return Err("Buffer pool full: all pages are pinned".into());
            };
            if self.is_dirty(victim)
                && let Some(entry) = self.frames.get(&victim)
            {
                write_back(victim, &entry.page, entry.page_lsn)?;
            }
            self.lru.remove(&tick);
            self.dirty.remove(&victim);
            self.frames.remove(&victim);
        }

        self.insert_frame(id, page);
        Ok(())
    }

    fn insert_frame(&mut self, id: u64, page: P) {
        self.frames.insert(
            id,
            FrameEntry {
                logged_crc: crc32c(page.as_ref()),
                page,
                pin_count: 0,
                page_lsn: 0,
                rec_lsn: 0,
                tick: 0,
            },
        );
        self.touch(id);
    }

    fn victim(&self) -> Option<(u64, u64)> {
        self.lru
            .iter()
            .find(|(_, id)| {
                self.frames.get(id).is_some_and(|e| {
                    e.pin_count == 0
                        && (!self.is_dirty(**id) || crc32c(e.page.as_ref()) == e.logged_crc)
                })
            })
            .map(|(&tick, &id)| (tick, id))
    }

    fn has_unlogged_change(&self, id: u64) -> bool {
        self.frames
            .get(&id)
            .is_some_and(|e| self.is_dirty(id) && crc32c(e.page.as_ref()) != e.logged_crc)
    }

    fn update_page(&mut self, id: u64, page: P) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.page = page;
            self.dirty.insert(id, true);
            self.touch(id);
        } else {
            self.insert_frame(id, page);
            self.dirty.insert(id, true);
        }
    }

    fn mark_dirty(&mut self, id: u64) {
        self.dirty.insert(id, true);
    }

    fn is_dirty(&self, id: u64) -> bool {
        self.dirty.get(&id).copied().unwrap_or(false)
    }

    fn update_lsn(&mut self, id: u64, lsn: u64) {
        let logged = self.frames.get(&id).map(|e| crc32c(e.page.as_ref()));
        if let Some(crc) = logged {
            self.logged(id, lsn, crc);
        }
    }

    fn logged(&mut self, id: u64, lsn: u64, crc: u32) {
        if let Some(entry) = self.frames.get_mut(&id) {
            if entry.rec_lsn == 0 {
                entry.rec_lsn = lsn;
            }
            entry.page_lsn = lsn;
            entry.logged_crc = crc;
            self.dirty.insert(id, true);
        }
        self.touch(id);
    }

    fn page_lsn(&self, id: u64) -> u64 {
        self.frames.get(&id).map(|e| e.page_lsn).unwrap_or(0)
    }

    fn rec_lsn(&self, id: u64) -> u64 {
        self.frames.get(&id).map(|e| e.rec_lsn).unwrap_or(0)
    }

    fn set_lsn_explicit(&mut self, id: u64, page_lsn: u64, rec_lsn: u64) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.page_lsn = page_lsn;
            entry.rec_lsn = rec_lsn;
        }
    }

    fn dirty_page_table(&self) -> Vec<(u64, u64)> {
        let mut dpt = Vec::new();
        for &id in self.dirty.keys() {
            let rec_lsn = self.frames.get(&id).map(|e| e.rec_lsn).unwrap_or(0);
            dpt.push((id, rec_lsn));
        }
        dpt
    }

    fn drain_dirty(&mut self) -> Vec<(u64, P)> {
        let dirty_ids: Vec<u64> = self.dirty.keys().copied().collect();
        let mut result = Vec::new();
        for id in &dirty_ids {
            if let Some(entry) = self.frames.get(id) {
                result.push((*id, entry.page.clone()));
            }
        }
        result
    }

    fn clear_dirty(&mut self) {
        self.dirty.clear();
        for entry in self.frames.values_mut() {
            entry.rec_lsn = 0;
        }
    }

    fn clear_dirty_single(&mut self, id: u64) {
        self.dirty.remove(&id);
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.rec_lsn = 0;
        }
    }

    fn pin(&mut self, id: u64) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.pin_count += 1;
        }
    }

    fn unpin(&mut self, id: u64) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.pin_count = entry.pin_count.saturating_sub(1);
        }
    }

    fn remove(&mut self, id: u64) -> Option<P> {
        self.dirty.remove(&id);
        let entry = self.frames.remove(&id)?;
        self.lru.remove(&entry.tick);
        Some(entry.page)
    }

    fn touch(&mut self, id: u64) {
        let Some(entry) = self.frames.get_mut(&id) else {
            return;
        };
        self.lru.remove(&entry.tick);
        self.clock += 1;
        entry.tick = self.clock;
        self.lru.insert(self.clock, id);
    }

    fn evict_one(&mut self) -> Option<EvictedPage<P>> {
        let victim = self.victim();

        if let Some((tick, id)) = victim {
            self.lru.remove(&tick);
            let was_dirty = self.is_dirty(id);
            self.dirty.remove(&id);
            let entry = self.frames.remove(&id)?;
            Some(EvictedPage {
                id,
                page: entry.page,
                was_dirty,
                page_lsn: entry.page_lsn,
                rec_lsn: entry.rec_lsn,
            })
        } else {
            None
        }
    }

    fn dirty_page_ids(&self) -> Vec<u64> {
        self.dirty.keys().copied().collect()
    }

    fn all_page_ids(&self) -> Vec<u64> {
        self.frames.keys().copied().collect()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl<P> BufferPool<P> {
    pub fn new(max_frames: usize) -> Self {
        BufferPool {
            inner: Mutex::new(BufferPoolInner {
                frames: HashMap::new(),
                dirty: HashMap::new(),
                lru: BTreeMap::new(),
                clock: 0,
                max_frames,
            }),
        }
    }
}

impl<P: Clone + AsRef<[u8]>> BufferPool<P> {
    pub fn contains(&self, id: u64) -> bool {
        lock(&self.inner).contains(id)
    }

    pub fn is_dirty(&self, id: u64) -> bool {
        lock(&self.inner).is_dirty(id)
    }

    pub fn page_lsn(&self, id: u64) -> u64 {
        lock(&self.inner).page_lsn(id)
    }

    pub fn rec_lsn(&self, id: u64) -> u64 {
        lock(&self.inner).rec_lsn(id)
    }

    pub fn mark_dirty(&self, id: u64) {
        lock(&self.inner).mark_dirty(id);
    }

    pub fn update_lsn(&self, id: u64, lsn: u64) {
        lock(&self.inner).update_lsn(id, lsn);
    }

    pub fn logged_image(&self, id: u64, lsn: u64, image: &[u8]) {
        let crc = crc32c(image);
        lock(&self.inner).logged(id, lsn, crc);
    }

    pub fn has_unlogged_change(&self, id: u64) -> bool {
        lock(&self.inner).has_unlogged_change(id)
    }

    pub fn set_lsn_explicit(&self, id: u64, page_lsn: u64, rec_lsn: u64) {
        lock(&self.inner).set_lsn_explicit(id, page_lsn, rec_lsn);
    }

    pub fn dirty_page_table(&self) -> Vec<(u64, u64)> {
        lock(&self.inner).dirty_page_table()
    }

    pub fn clear_dirty(&self) {
        lock(&self.inner).clear_dirty();
    }

    pub fn clear_dirty_single(&self, id: u64) {
        lock(&self.inner).clear_dirty_single(id);
    }

    pub fn pin(&self, id: u64) {
        lock(&self.inner).pin(id);
    }

    pub fn unpin(&self, id: u64) {
        lock(&self.inner).unpin(id);
    }

    pub fn remove(&self, id: u64) -> Option<P> {
        lock(&self.inner).remove(id)
    }

    pub fn dirty_page_ids(&self) -> Vec<u64> {
        lock(&self.inner).dirty_page_ids()
    }

    pub fn all_page_ids(&self) -> Vec<u64> {
        lock(&self.inner).all_page_ids()
    }

    pub fn len(&self) -> usize {
        lock(&self.inner).len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.inner).is_empty()
    }
}

impl<P: Clone + AsRef<[u8]>> BufferPool<P> {
    pub fn get(&self, id: u64) -> Option<P> {
        lock(&self.inner).get(id)
    }

    pub fn with<R>(&self, id: u64, f: impl FnOnce(&P) -> R) -> Option<R> {
        lock(&self.inner).with(id, f)
    }

    pub fn insert(&self, id: u64, page: P) -> Result<Option<EvictedPage<P>>, Box<dyn Error>> {
        lock(&self.inner).insert(id, page)
    }

    pub fn insert_with_write_back(
        &self,
        id: u64,
        page: P,
        mut write_back: impl FnMut(u64, &P, u64) -> Result<(), Box<dyn Error>>,
    ) -> Result<(), Box<dyn Error>> {
        lock(&self.inner).insert_with_write_back(id, page, &mut write_back)
    }

    pub fn update_page(&self, id: u64, page: P) {
        lock(&self.inner).update_page(id, page);
    }

    pub fn drain_dirty(&self) -> Vec<(u64, P)> {
        lock(&self.inner).drain_dirty()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn steal_evicts_dirty_pages_and_returns_them() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(2);

        pool.insert(1, vec![1, 1, 1]).unwrap();
        pool.mark_dirty(1);

        pool.insert(2, vec![2, 2, 2]).unwrap();
        pool.mark_dirty(2);

        // pool is full of dirty pages.. STEAL lets them be evicted
        let result = pool.insert(3, vec![3, 3, 3]);
        assert!(result.is_ok());
        let evicted = result.unwrap();
        assert!(evicted.is_some());
        let evicted = evicted.unwrap();
        assert_eq!(evicted.id, 1);
        assert!(evicted.was_dirty);
        assert_eq!(evicted.page, vec![1, 1, 1]);

        assert!(!pool.contains(1));
        assert!(pool.contains(2));
        assert!(pool.contains(3));
    }

    #[test]
    fn steal_refuses_to_evict_pinned_pages() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(2);

        pool.insert(1, vec![1, 1, 1]).unwrap();
        pool.pin(1);

        pool.insert(2, vec![2, 2, 2]).unwrap();
        pool.pin(2);

        // here both the pages are pinned... so they can't evict even under STEAL
        let err = pool.insert(3, vec![3, 3, 3]);
        assert!(err.is_err());

        // Unpin page 1; now it can be evicted
        pool.unpin(1);
        assert!(pool.insert(3, vec![3, 3, 3]).is_ok());
        assert!(!pool.contains(1));
        assert!(pool.contains(2));
        assert!(pool.contains(3));
    }

    #[test]
    fn steal_evicts_clean_page_without_dirty_flag() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(2);

        pool.insert(1, vec![1, 1, 1]).unwrap();
        pool.insert(2, vec![2, 2, 2]).unwrap();

        let result = pool.insert(3, vec![3, 3, 3]).unwrap();
        assert!(result.is_some());
        let evicted = result.unwrap();
        assert_eq!(evicted.id, 1);
        assert!(!evicted.was_dirty);
    }

    #[test]
    fn page_with_unlogged_change_is_never_evicted() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(2);
        pool.insert(1, vec![1; 8]).unwrap();
        pool.insert(2, vec![2; 8]).unwrap();
        pool.update_page(1, vec![9; 8]);
        pool.update_page(2, vec![8; 8]);
        assert!(pool.has_unlogged_change(1));
        assert!(pool.insert(3, vec![3; 8]).is_err());
        pool.logged_image(1, 7, &[9; 8]);
        assert!(!pool.has_unlogged_change(1));
        let evicted = pool.insert(3, vec![3; 8]).unwrap().unwrap();
        assert_eq!(evicted.id, 1);
        assert_eq!(evicted.page_lsn, 7);
    }

    #[test]
    fn failed_write_back_keeps_the_victim_cached_and_dirty() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(1);
        pool.insert(1, vec![1; 8]).unwrap();
        pool.mark_dirty(1);
        let result = pool.insert_with_write_back(2, vec![2; 8], |_, _, _| Err("disk full".into()));
        assert!(result.is_err());
        assert!(pool.contains(1));
        assert!(!pool.contains(2));
        assert_eq!(pool.dirty_page_ids(), vec![1]);

        let mut written = Vec::new();
        pool.insert_with_write_back(2, vec![2; 8], |id, page, _| {
            written.push((id, page.clone()));
            Ok(())
        })
        .unwrap();
        assert_eq!(written, vec![(1, vec![1; 8])]);
        assert!(!pool.contains(1));
        assert!(pool.contains(2));
    }

    #[test]
    fn evicts_least_recently_used_unpinned_page() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(3);
        pool.insert(1, vec![1]).unwrap();
        pool.insert(2, vec![2]).unwrap();
        pool.insert(3, vec![3]).unwrap();
        pool.get(1);
        pool.pin(2);
        let evicted = pool.insert(4, vec![4]).unwrap().unwrap();
        assert_eq!(evicted.id, 3);
        pool.unpin(2);
        let evicted = pool.insert(5, vec![5]).unwrap().unwrap();
        assert_eq!(evicted.id, 2);
        assert!(pool.contains(1));
        assert_eq!(pool.remove(1), Some(vec![1]));
        assert!(pool.insert(6, vec![6]).unwrap().is_none());
    }

    #[test]
    fn all_pinned_pool_reports_full() {
        let pool: BufferPool<Vec<u8>> = BufferPool::new(1);
        pool.insert(1, vec![1]).unwrap();
        pool.pin(1);
        assert!(pool.insert(2, vec![2]).is_err());
    }
}
