//! The buffer pool is based on STEAL NO-FORCE policy,which means that mutated
//! page buffers CAN be flushed to disk even before the transaction has
//! commited, but only  if the condition `PageLSN <= FlushedLSN` is satisfied.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::Mutex;

pub(super) struct BufferPool<P> {
    inner: Mutex<BufferPoolInner<P>>,
}

struct BufferPoolInner<P> {
    frames: HashMap<u64, FrameEntry<P>>,
    dirty: HashMap<u64, bool>,
    lru_order: VecDeque<u64>,
    max_frames: usize,
}

struct FrameEntry<P> {
    page: P,
    pin_count: u32,
    page_lsn: u64,
    rec_lsn: u64,
}

pub(super) struct EvictedPage<P> {
    pub id: u64,
    pub page: P,
    pub was_dirty: bool,
    pub page_lsn: u64,
    pub rec_lsn: u64,
}

impl<P: Clone> BufferPoolInner<P> {
    fn contains(&self, id: u64) -> bool {
        self.frames.contains_key(&id)
    }

    fn get(&mut self, id: u64) -> Option<P> {
        self.touch(id);
        self.frames.get(&id).map(|e| e.page.clone())
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
                page,
                pin_count: 0,
                page_lsn: 0,
                rec_lsn: 0,
            },
        );
        self.touch(id);

        Ok(evicted)
    }

    fn update_page(&mut self, id: u64, page: P) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.page = page;
            self.dirty.insert(id, true);
            self.touch(id);
        } else {
            let _ = self.insert(id, page);
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
        if let Some(entry) = self.frames.get_mut(&id) {
            if entry.rec_lsn == 0 {
                entry.rec_lsn = lsn;
            }
            entry.page_lsn = lsn;
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
        self.lru_order.retain(|&x| x != id);
        self.frames.remove(&id).map(|e| e.page)
    }

    fn touch(&mut self, id: u64) {
        self.lru_order.retain(|&x| x != id);
        self.lru_order.push_back(id);
    }

    fn evict_one(&mut self) -> Option<EvictedPage<P>> {
        let mut evict_idx = None;
        for (i, &id) in self.lru_order.iter().enumerate() {
            let Some(entry) = self.frames.get(&id) else {
                continue;
            };
            if entry.pin_count == 0 {
                evict_idx = Some((i, id));
                break;
            }
        }

        if let Some((idx, id)) = evict_idx {
            self.lru_order.remove(idx);
            let was_dirty = self.is_dirty(id);
            self.dirty.remove(&id);
            let entry = self.frames.remove(&id).unwrap();
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
                lru_order: VecDeque::new(),
                max_frames,
            }),
        }
    }
}

impl<P: Clone> BufferPool<P> {
    pub fn contains(&self, id: u64) -> bool {
        self.inner.lock().unwrap().contains(id)
    }

    pub fn is_dirty(&self, id: u64) -> bool {
        self.inner.lock().unwrap().is_dirty(id)
    }

    pub fn page_lsn(&self, id: u64) -> u64 {
        self.inner.lock().unwrap().page_lsn(id)
    }

    pub fn rec_lsn(&self, id: u64) -> u64 {
        self.inner.lock().unwrap().rec_lsn(id)
    }

    pub fn mark_dirty(&self, id: u64) {
        self.inner.lock().unwrap().mark_dirty(id);
    }

    pub fn update_lsn(&self, id: u64, lsn: u64) {
        self.inner.lock().unwrap().update_lsn(id, lsn);
    }

    pub fn set_lsn_explicit(&self, id: u64, page_lsn: u64, rec_lsn: u64) {
        self.inner
            .lock()
            .unwrap()
            .set_lsn_explicit(id, page_lsn, rec_lsn);
    }

    pub fn dirty_page_table(&self) -> Vec<(u64, u64)> {
        self.inner.lock().unwrap().dirty_page_table()
    }

    pub fn clear_dirty(&self) {
        self.inner.lock().unwrap().clear_dirty();
    }

    pub fn clear_dirty_single(&self, id: u64) {
        self.inner.lock().unwrap().clear_dirty_single(id);
    }

    pub fn pin(&self, id: u64) {
        self.inner.lock().unwrap().pin(id);
    }

    pub fn unpin(&self, id: u64) {
        self.inner.lock().unwrap().unpin(id);
    }

    pub fn remove(&self, id: u64) -> Option<P> {
        self.inner.lock().unwrap().remove(id)
    }

    pub fn dirty_page_ids(&self) -> Vec<u64> {
        self.inner.lock().unwrap().dirty_page_ids()
    }

    pub fn all_page_ids(&self) -> Vec<u64> {
        self.inner.lock().unwrap().all_page_ids()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

impl<P: Clone> BufferPool<P> {
    pub fn get(&self, id: u64) -> Option<P> {
        self.inner.lock().unwrap().get(id)
    }

    pub fn insert(&self, id: u64, page: P) -> Result<Option<EvictedPage<P>>, Box<dyn Error>> {
        self.inner.lock().unwrap().insert(id, page)
    }

    pub fn update_page(&self, id: u64, page: P) {
        self.inner.lock().unwrap().update_page(id, page);
    }

    pub fn drain_dirty(&self) -> Vec<(u64, P)> {
        self.inner.lock().unwrap().drain_dirty()
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
}
