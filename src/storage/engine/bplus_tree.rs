#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::{error::Error, path::PathBuf};

use header::PageKind;
use pager::Pager;
pub use pager::TreeOptions;
use slotted_page::Cell;

pub(super) mod buffer_pool;
pub(super) mod crc;
pub(super) mod header;
pub(super) mod heap;
pub(super) mod index;
pub mod io;

const MAX_TREE_DEPTH: usize = 64;
pub(super) mod pager;
pub(super) mod slotted_page;
pub(super) mod sync;
pub(super) mod wal;
pub(super) mod wal_buffer;

pub(super) const PAGE_SIZE: usize = 4096;
pub(super) const PAGE_CRC_OFFSET: usize = 8 + 2 + 2 + 1 + 1 + 8;
pub(super) const HEADER_SIZE: usize = PAGE_CRC_OFFSET + 4;
pub(super) const KEY_SIZE: usize = 16;

pub type Key = u128;
pub(super) const PTR_SIZE: usize = 8;

pub(super) const SLOT_SIZE: usize = 4; // cell_offset: u16 + cell_size: u16

/// max seperator keys in a page
///
/// `KEY_SIZE` + `PTR_SIZE` + `SLOT_SIZE` because for a single entry of a node
/// there will be a cell pointer (`SLOT_SIZE`, 4 bytes) and the actual cell
/// `KEY_SIZE` (the page id) `u64` 8 bytes AND `PTR_SIZE` `u64` 8 bytes  
/// (ptr to the child node for internal nodes OR heap file page index for leaf nodes).
const ORDER: usize = (PAGE_SIZE - HEADER_SIZE) / (KEY_SIZE + PTR_SIZE + SLOT_SIZE);
const FANOUT: usize = ORDER + 1;
const DEGREE: usize = (FANOUT - 2) / 2;
const _: () = assert!(2 * DEGREE < ORDER);

pub type Records = Vec<Vec<u8>>;
pub const MAX_RECORD_BYTES: usize = heap::MAX_BLOB_BYTES - 8;
pub type Entry = (Key, Records);

const AUTOCOMMIT_TXN: u64 = 0;
const NO_SIBLING: u64 = 0;
const SCAN_BATCH_ROWS: usize = 256;

pub struct BplusTree {
    root_id: AtomicU64,
    pager: Pager,
    tree_lock: RwLock<()>,
    undo: Mutex<HashMap<u64, TxnUndo>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Savepoint(usize);

enum UndoOp {
    RemoveInserted(Key),
    RestoreDeleted(Key, Records),
}

#[derive(Default)]
struct TxnUndo {
    ops: Vec<UndoOp>,
    freed_pages: Vec<u64>,
    freed_index_pages: Vec<u64>,
}

pub struct RangeIter<'a> {
    tree: &'a BplusTree,
    next_lower: Bound<Key>,
    upper: Bound<Key>,
    buffer: VecDeque<Entry>,
    exhausted: bool,
}

impl Iterator for RangeIter<'_> {
    type Item = Result<Entry, Box<dyn Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer.is_empty() && !self.exhausted {
            match self
                .tree
                .scan_batch(self.next_lower, self.upper, SCAN_BATCH_ROWS)
            {
                Ok((batch, exhausted)) => {
                    self.exhausted = exhausted;
                    if let Some((last, _)) = batch.last() {
                        self.next_lower = Bound::Excluded(*last);
                    }
                    self.buffer.extend(batch);
                }
                Err(e) => {
                    self.exhausted = true;
                    return Some(Err(e));
                }
            }
        }
        self.buffer.pop_front().map(Ok)
    }
}

impl BplusTree {
    pub fn new(index_path: PathBuf, heap_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let wal_path = index_path.with_extension("wal");
        Self::new_with_wal(index_path, heap_path, wal_path)
    }

    pub fn new_with_wal(
        index_path: PathBuf,
        heap_path: PathBuf,
        wal_path: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        Self::open_in(
            &io::OsVfs,
            index_path,
            heap_path,
            wal_path,
            TreeOptions::default(),
        )
    }

    pub fn open_in(
        vfs: &dyn io::Vfs,
        index_path: PathBuf,
        heap_path: PathBuf,
        wal_path: PathBuf,
        options: TreeOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let pager = Pager::open_in(vfs, index_path, heap_path, wal_path, options)?;
        let tree = Self {
            root_id: AtomicU64::new(0),
            pager,
            tree_lock: RwLock::new(()),
            undo: Mutex::new(HashMap::new()),
        };
        let log_is_empty = tree.pager.wal.size_bytes()? <= header::WAL_HEADER_SIZE as u64;
        tree.ensure_root(log_is_empty)?;
        Ok(tree)
    }

    fn find_root(&self) -> Result<Option<u64>, Box<dyn Error>> {
        let next_id = self.pager.index.next_id.load(Ordering::SeqCst);
        for id in 0..next_id {
            if let Ok(hdr) = self.pager.index.fetch(id)?.header()
                && hdr.page_ty == PageKind::Root
                && hdr.id == id
                && hdr.free_start as usize >= HEADER_SIZE
            {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    fn ensure_root(&self, may_create: bool) -> Result<(), Box<dyn Error>> {
        if let Some(id) = self.find_root()? {
            self.root_id.store(id, Ordering::Release);
            return Ok(());
        }
        if !may_create {
            return Ok(());
        }
        let next_id = self.pager.index.next_id.load(Ordering::SeqCst);
        for id in 0..next_id {
            if self.pager.index.fetch(id)?.data.iter().any(|&b| b != 0) {
                return Err(format!(
                    "corrupt index: {next_id} pages but no root page, page {id} is in use"
                )
                .into());
            }
        }
        let heap_pages = self.pager.heap.next_id.load(Ordering::SeqCst);
        for id in 0..heap_pages {
            if self.pager.heap.fetch(id)?.data.iter().any(|&b| b != 0) {
                return Err(
                    format!("corrupt index: no root page while heap page {id} holds data").into(),
                );
            }
        }
        let id = if self.pager.index.next_id.load(Ordering::SeqCst) > 0 {
            0
        } else {
            self.pager.index.allocate(PageKind::Root)?
        };
        let old_data = self.pager.index.fetch(id)?.data;
        let mut hdr = header::IndexHeader {
            id,
            free_start: HEADER_SIZE as u16,
            free_end: PAGE_SIZE as u16,
            page_ty: PageKind::Root,
            flags: 0,
            ptr: 0,
        };
        hdr.set_root_leaf();
        let mut new_data = [0u8; PAGE_SIZE];
        hdr.serialize(&mut new_data[..HEADER_SIZE]);
        self.pager.log_index_diff(0, id, &old_data, &new_data)?;
        self.pager.index.fetch_mut(id)?.data = new_data;
        self.root_id.store(id, Ordering::Release);
        Ok(())
    }

    pub fn begin_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.pager.wal.begin_transaction(txn_id)
    }

    pub fn commit_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let lsn = self.pager.wal.commit_transaction(txn_id)?;
        let undo = self.take_undo(txn_id);
        self.pager.heap.release(undo.freed_pages);
        self.pager.index.release(undo.freed_index_pages);
        Ok(lsn)
    }

    pub fn abort_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let undo = self.take_undo(txn_id);
        self.apply_undo(txn_id, undo.ops)?;
        let lsn = self.pager.wal.abort_transaction(txn_id)?;
        let during_undo = self.take_undo(txn_id);
        self.pager.heap.release(undo.freed_pages);
        self.pager.heap.release(during_undo.freed_pages);
        self.pager.index.release(undo.freed_index_pages);
        self.pager.index.release(during_undo.freed_index_pages);
        Ok(lsn)
    }

    pub fn savepoint(&self, txn_id: u64) -> Savepoint {
        Savepoint(
            sync::lock(&self.undo)
                .get(&txn_id)
                .map_or(0, |undo| undo.ops.len()),
        )
    }

    pub fn rollback_to(&self, txn_id: u64, savepoint: Savepoint) -> Result<(), Box<dyn Error>> {
        let ops = match sync::lock(&self.undo).get_mut(&txn_id) {
            Some(undo) if undo.ops.len() > savepoint.0 => undo.ops.split_off(savepoint.0),
            _ => Vec::new(),
        };
        self.apply_undo(txn_id, ops)
    }

    fn apply_undo(&self, txn_id: u64, ops: Vec<UndoOp>) -> Result<(), Box<dyn Error>> {
        for op in ops.into_iter().rev() {
            match op {
                UndoOp::RemoveInserted(key) => {
                    let (_, pages) = self.delete_inner(txn_id, key, false)?;
                    self.pager.heap.release(pages);
                }
                UndoOp::RestoreDeleted(key, records) => {
                    self.insert_inner(txn_id, key, records)?;
                }
            }
        }
        Ok(())
    }

    pub fn insert(
        &self,
        txn_id: u64,
        key: Key,
        data_records: Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        self.insert_inner(txn_id, key, data_records)?;
        if txn_id != AUTOCOMMIT_TXN {
            self.with_undo(txn_id, |undo| undo.ops.push(UndoOp::RemoveInserted(key)));
        }
        Ok(())
    }

    pub fn delete(&self, txn_id: u64, key: Key) -> Result<(), Box<dyn Error>> {
        let transactional = txn_id != AUTOCOMMIT_TXN;
        let (old_records, pages) = self.delete_inner(txn_id, key, transactional)?;
        if transactional {
            self.with_undo(txn_id, |undo| {
                undo.ops.push(UndoOp::RestoreDeleted(key, old_records));
                undo.freed_pages.extend(pages);
            });
        } else {
            self.pager.heap.release(pages);
        }
        Ok(())
    }

    pub fn range(&self, _txn_id: u64, lower: Bound<Key>, upper: Bound<Key>) -> RangeIter<'_> {
        RangeIter {
            tree: self,
            next_lower: lower,
            upper,
            buffer: VecDeque::new(),
            exhausted: false,
        }
    }

    pub fn last_key_in(
        &self,
        _txn_id: u64,
        lower: Bound<Key>,
        upper: Bound<Key>,
    ) -> Result<Option<Key>, Box<dyn Error>> {
        let _guard = self.shared()?;
        let low = match lower {
            Bound::Included(l) => l,
            Bound::Excluded(l) => match l.checked_add(1) {
                Some(l) => l,
                None => return Ok(None),
            },
            Bound::Unbounded => 0,
        };
        let high = match upper {
            Bound::Included(u) => u,
            Bound::Excluded(u) => match u.checked_sub(1) {
                Some(u) => u,
                None => return Ok(None),
            },
            Bound::Unbounded => Key::MAX,
        };
        if low > high {
            return Ok(None);
        }
        self.last_key_below(self.root_id.load(Ordering::Acquire), low, high, 0)
    }

    fn last_key_below(
        &self,
        page_id: u64,
        low: Key,
        high: Key,
        depth: usize,
    ) -> Result<Option<Key>, Box<dyn Error>> {
        if depth >= MAX_TREE_DEPTH {
            return Err("corrupt index: runaway depth looking for the last key".into());
        }
        let (cells, p_hdr) =
            self.pager
                .index
                .with_page(page_id, |page| -> Result<_, Box<dyn Error>> {
                    let p_hdr = page.header()?;
                    Ok((page.get_cells()?, p_hdr))
                })??;
        let is_leaf_level = p_hdr.page_ty == PageKind::Leaf
            || (p_hdr.page_ty == PageKind::Root && p_hdr.is_root_leaf());
        if is_leaf_level {
            return Ok(cells
                .iter()
                .rev()
                .map(|c| c.key)
                .find(|&k| k <= high)
                .filter(|&k| k >= low));
        }
        let start = cells.partition_point(|c| c.key <= high);
        for child in (0..=start).rev() {
            let child_id = match cells.get(child) {
                Some(cell) => cell
                    .c_ptr
                    .ok_or("corrupt index: internal cell without child")?,
                None => p_hdr.ptr,
            };
            if let Some(key) = self.last_key_below(child_id, low, high, depth + 1)? {
                return Ok(Some(key));
            }
            if child == 0 || cells[child - 1].key <= low {
                break;
            }
        }
        Ok(None)
    }

    pub fn contains(&self, _txn_id: u64, key: Key) -> Result<bool, Box<dyn Error>> {
        let _guard = self.shared()?;
        Ok(self.breadcrumbs(key)?.found)
    }

    pub fn flush(&self) -> Result<(), Box<dyn Error>> {
        let _guard = self.shared()?;
        self.pager.flush_all()
    }

    pub fn checkpoint_and_truncate(&self) -> Result<(), Box<dyn Error>> {
        let _guard = self.exclusive()?;
        if !sync::lock(&self.undo).is_empty() {
            return Err("cannot checkpoint while transactions are active".into());
        }
        self.pager.flush_all()?;
        self.pager.wal.truncate()
    }

    pub fn wal_size_bytes(&self) -> Result<u64, Box<dyn Error>> {
        self.pager.wal.size_bytes()
    }

    pub fn scan(&self, txn_id: u64) -> RangeIter<'_> {
        self.range(txn_id, Bound::Unbounded, Bound::Unbounded)
    }

    fn poison_on_err<T>(&self, result: Result<T, Box<dyn Error>>) -> Result<T, Box<dyn Error>> {
        if result.is_err() {
            self.pager.wal.poison();
        }
        result
    }

    fn shared(&self) -> Result<std::sync::RwLockReadGuard<'_, ()>, Box<dyn Error>> {
        self.pager.wal.check()?;
        self.tree_lock.read().map_err(|_| {
            self.pager.wal.poison();
            wal::POISONED_MESSAGE.into()
        })
    }

    fn exclusive(&self) -> Result<std::sync::RwLockWriteGuard<'_, ()>, Box<dyn Error>> {
        self.pager.wal.check()?;
        self.tree_lock.write().map_err(|_| {
            self.pager.wal.poison();
            wal::POISONED_MESSAGE.into()
        })
    }

    fn take_undo(&self, txn_id: u64) -> TxnUndo {
        sync::lock(&self.undo).remove(&txn_id).unwrap_or_default()
    }

    fn with_undo(&self, txn_id: u64, f: impl FnOnce(&mut TxnUndo)) {
        f(sync::lock(&self.undo).entry(txn_id).or_default());
    }

    fn free_index_page(&self, txn_id: u64, id: u64) {
        if txn_id == AUTOCOMMIT_TXN {
            self.pager.index.release([id]);
        } else {
            self.with_undo(txn_id, |undo| undo.freed_index_pages.push(id));
        }
    }

    fn scan_batch(
        &self,
        lower: Bound<Key>,
        upper: Bound<Key>,
        max_rows: usize,
    ) -> Result<(Vec<Entry>, bool), Box<dyn Error>> {
        let _guard = self.shared()?;
        let start = match lower {
            Bound::Included(k) | Bound::Excluded(k) => k,
            Bound::Unbounded => 0,
        };
        let mut page_id = *self
            .breadcrumbs(start)?
            .breadcrumb
            .last()
            .ok_or("Empty breadcrumb trail")?;

        let page_count = self.pager.index.next_id.load(Ordering::SeqCst);
        let mut steps = 0u64;
        let mut out = Vec::new();
        loop {
            steps += 1;
            if steps > page_count {
                return Err("corrupt index: the leaf chain loops".into());
            }
            let (cells, hdr) = self
                .pager
                .index
                .with_page(page_id, |page| -> Result<_, Box<dyn Error>> {
                    Ok((page.get_cells()?, page.header()?))
                })??;
            for cell in cells {
                let below = match lower {
                    Bound::Included(k) => cell.key < k,
                    Bound::Excluded(k) => cell.key <= k,
                    Bound::Unbounded => false,
                };
                if below {
                    continue;
                }
                let above = match upper {
                    Bound::Included(k) => cell.key > k,
                    Bound::Excluded(k) => cell.key >= k,
                    Bound::Unbounded => false,
                };
                if above {
                    return Ok((out, true));
                }
                if out.last().is_some_and(|(prev, _)| cell.key <= *prev) {
                    return Err(format!(
                        "corrupt index: leaf page {page_id} has key {} out of order",
                        cell.key
                    )
                    .into());
                }
                let mut records = Vec::new();
                self.pager.fetch_heap_data(&cell, &mut records)?;
                out.push((cell.key, records));
                if out.len() >= max_rows {
                    return Ok((out, false));
                }
            }
            let next = hdr.ptr;
            if next == NO_SIBLING || next == page_id {
                return Ok((out, true));
            }
            page_id = next;
        }
    }

    pub fn fuzzy_checkpoint(&self) -> Result<u64, Box<dyn Error>> {
        let _guard = self.shared()?;
        self.pager.fuzzy_checkpoint()
    }

    pub fn recover(&self) -> Result<pager::RecoveryReport, Box<dyn Error>> {
        let _guard = self.exclusive()?;
        let report = self.pager.recover()?;
        self.ensure_root(true)?;
        self.rebuild_heap_space()?;
        self.rebuild_index_free_list()?;
        Ok(report)
    }

    fn rebuild_index_free_list(&self) -> Result<(), Box<dyn Error>> {
        let page_count = self.pager.index.next_id.load(Ordering::SeqCst);
        let mut reachable = std::collections::BTreeSet::new();
        let mut pending = vec![self.root_id.load(Ordering::Acquire)];
        while let Some(id) = pending.pop() {
            if id >= page_count {
                return Err(format!("corrupt index: child pointer {id} past the last page").into());
            }
            if !reachable.insert(id) {
                return Err(format!("corrupt index: page {id} is reachable twice").into());
            }
            let page = self.pager.index.fetch(id)?;
            let hdr = page.header()?;
            let is_leaf = hdr.page_ty == PageKind::Leaf
                || (hdr.page_ty == PageKind::Root && hdr.is_root_leaf());
            if is_leaf {
                continue;
            }
            for cell in page.get_cells()? {
                pending.push(cell.c_ptr.ok_or("Internal cell without child pointer")?);
            }
            pending.push(hdr.ptr);
        }
        let free = (0..page_count)
            .filter(|id| !reachable.contains(id))
            .collect();
        self.pager.index.reset_free(free);
        Ok(())
    }

    pub fn free_index_page_count(&self) -> usize {
        self.pager.index.free_page_count()
    }

    fn rebuild_heap_space(&self) -> Result<(), Box<dyn Error>> {
        let mut used = std::collections::BTreeSet::new();
        let mut shared = std::collections::BTreeSet::new();
        let mut visited = std::collections::BTreeSet::new();
        let mut page_id = *self
            .breadcrumbs(0)?
            .breadcrumb
            .last()
            .ok_or("Empty breadcrumb trail")?;
        loop {
            let (cells, hdr) = self
                .pager
                .index
                .with_page(page_id, |page| -> Result<_, Box<dyn Error>> {
                    Ok((page.get_cells()?, page.header()?))
                })??;
            for cell in cells {
                let ptr = cell
                    .h_ptr
                    .as_ref()
                    .ok_or("Leaf cell without heap pointer")?
                    .index;
                let (heap_page, slot) = heap::unpack_ptr(ptr);
                if slot == heap::CHAIN_SLOT {
                    used.extend(self.pager.heap.free_chain(heap_page)?);
                } else {
                    used.insert(heap_page);
                    shared.insert(heap_page);
                }
            }
            visited.insert(page_id);
            let next = hdr.ptr;
            if next == NO_SIBLING || visited.contains(&next) {
                break;
            }
            page_id = next;
        }
        let free = (0..self.pager.heap.page_count())
            .filter(|id| !used.contains(id))
            .collect();
        let shared = shared
            .into_iter()
            .map(|id| Ok((id, self.pager.heap.fetch(id)?.shared_free_space()?)))
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        self.pager.heap.reset_space(free, shared);
        Ok(())
    }

    /// Traverses the tree, finds the cell content in page
    /// i.e. the offset & size of the actual data in the heap file
    /// returns a &Vec<u8> of the data, the callers can transmute it.
    pub fn get(&self, txn_id: u64, key: Key) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
        let _guard = self.shared()?;
        let _ = txn_id;
        let mut page_id = self.root_id.load(Ordering::Acquire);
        for _ in 0..MAX_TREE_DEPTH {
            let (cells, p_hdr) =
                self.pager
                    .index
                    .with_page(page_id, |page| -> Result<_, Box<dyn Error>> {
                        let p_hdr = page.header()?;
                        Ok((page.get_cells()?, p_hdr))
                    })??;

            let is_leaf_level = p_hdr.page_ty == PageKind::Leaf
                || (p_hdr.page_ty == PageKind::Root && p_hdr.is_root_leaf());

            match cells.binary_search_by(|cell| cell.key.cmp(&key)) {
                Ok(i) => {
                    if is_leaf_level {
                        let cell = cells.get(i).unwrap();
                        let heap_id = cell
                            .h_ptr
                            .as_ref()
                            .ok_or("Couldn't find the header pointer")?
                            .index;
                        let mut data_records: Vec<Vec<u8>> = vec![];
                        self.pager.load_records(heap_id, &mut data_records)?;
                        return Ok(data_records);
                    } else {
                        let child_page_id = if i + 1 < cells.len() {
                            cells.get(i + 1).unwrap().c_ptr.unwrap()
                        } else {
                            p_hdr.ptr
                        };
                        page_id = child_page_id;
                    }
                }
                Err(i) => {
                    if is_leaf_level {
                        return Err("Entry not found".into());
                    }

                    let child_page_id = if i < cells.len() {
                        cells.get(i).unwrap().c_ptr.unwrap()
                    } else {
                        p_hdr.ptr
                    };
                    page_id = child_page_id;
                }
            }
        }
        Err(format!("corrupt index: runaway depth looking up key {key}").into())
    }

    fn insert_inner(
        &self,
        txn_id: u64,
        key: Key,
        data_records: Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        let _guard = self.exclusive()?;
        let breadcrumbs = self.breadcrumbs(key)?;

        if breadcrumbs.found {
            return Err("Key already exists".into());
        }
        Pager::check_records(&data_records)?;

        let result = (|| -> Result<(), Box<dyn Error>> {
            let mut breadcrumbs = breadcrumbs.breadcrumb;

            let Some(index_page_id) = breadcrumbs.pop() else {
                return Err("No page found".into());
            };

            // we first have to insert the data record into the heap file then we
            // can have the offset of the data record that we can add to the cell's of
            // index file current page
            let heap_page_id = self.pager.store_records(txn_id, data_records)?;

            // now we can store the cell in the index page itself.
            let n_slot = {
                let mut page = self.pager.index.fetch_mut(index_page_id)?;
                let old_data = page.data;
                let n_slot = page.add_cell(key, heap_page_id)?;
                let new_data = page.data;
                drop(page);
                self.pager
                    .log_index_diff(txn_id, index_page_id, &old_data, &new_data)?;
                n_slot
            };

            if n_slot >= ORDER {
                // index page was popped off so we need to re-insert
                breadcrumbs.push(index_page_id);
                match self.handle_overfull(txn_id, &mut breadcrumbs)? {
                    Some(new_id) => self.root_id.store(new_id, Ordering::Release),
                    None => return Ok(()),
                }
            }

            Ok(())
        })();
        self.poison_on_err(result)
    }

    pub fn insert_with_txn(
        &self,
        txn_id: u64,
        key: Key,
        data_records: Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        self.insert(txn_id, key, data_records)
    }

    fn handle_overfull(
        &self,
        txn_id: u64,
        breadcrumbs: &mut Vec<u64>,
    ) -> Result<Option<u64>, Box<dyn Error>> {
        let Some(overflow_page_id) = breadcrumbs.pop() else {
            return Err("Overflow page not found!?".into());
        };

        // collecting all the cells in the logical page, no matter if the page is
        // overfull or not
        let (overflow_hdr, overflow_cells) = {
            let page = self.pager.index.fetch(overflow_page_id)?;
            let hdr = page.header()?;
            let cells = page.get_cells()?;
            (hdr, cells)
        };

        let n = overflow_cells.len();
        // keep these many elements remanining in the current page,
        // (split + 1)th element will be promoted to the parent node and further
        // will have the rest of the nodes as childs.
        let split = n.div_ceil(2);

        // Left cells are just remaning cells in the current overflow page
        // The promote cell is prompted to the parent, further pointing to right cells*
        // and left cells
        // Right cells are the split from original and form a new page which is pointed by the
        // promoted cell
        let left_cells = &overflow_cells[..split];
        let promote_cell = &overflow_cells[split];
        let right_cells = &overflow_cells[split + 1..];

        let is_leaf_level = overflow_hdr.page_ty == PageKind::Leaf
            || (overflow_hdr.page_ty == PageKind::Root && overflow_hdr.is_root_leaf());

        let right_page_kind = if is_leaf_level {
            PageKind::Leaf
        } else {
            PageKind::Internal
        };
        let right_page_id = self.pager.index.allocate(right_page_kind)?;

        if is_leaf_level {
            // *On leaf level, the right cell's first element is the same as the promoted key
            let right_cells_with_promoted: Vec<_> = std::iter::once(promote_cell)
                .chain(right_cells.iter())
                .collect();

            let old_data = self.pager.index.fetch(overflow_page_id)?.data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id)?;
                left_page.rebuild_from_cells(
                    left_cells,
                    PageKind::Leaf,
                    overflow_page_id,
                    right_page_id,
                )?;
                left_page.data
            };
            self.pager
                .log_index_diff(txn_id, overflow_page_id, &old_data, &new_data)?;

            let old_data = self.pager.index.fetch(right_page_id)?.data;
            let new_data = {
                let mut right_page = self.pager.index.fetch_mut(right_page_id)?;
                let right_data: Vec<Cell> = right_cells_with_promoted
                    .into_iter()
                    .map(|c| Cell {
                        key: c.key,
                        c_ptr: c.c_ptr,
                        h_ptr: c
                            .h_ptr
                            .as_ref()
                            .map(|h| slotted_page::HeapPointer { index: h.index }),
                    })
                    .collect();
                right_page.rebuild_from_cells(
                    &right_data,
                    PageKind::Leaf,
                    right_page_id,
                    overflow_hdr.ptr,
                )?;
                right_page.data
            };
            self.pager
                .log_index_diff(txn_id, right_page_id, &old_data, &new_data)?;
        } else {
            let promote_c_ptr = promote_cell
                .c_ptr
                .ok_or("Internal promote cell missing c_ptr")?;

            let old_data = self.pager.index.fetch(overflow_page_id)?.data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id)?;
                left_page.rebuild_from_cells(
                    left_cells,
                    PageKind::Internal,
                    overflow_page_id,
                    promote_c_ptr,
                )?;
                left_page.data
            };
            self.pager
                .log_index_diff(txn_id, overflow_page_id, &old_data, &new_data)?;

            let old_data = self.pager.index.fetch(right_page_id)?.data;
            let new_data = {
                let mut right_page = self.pager.index.fetch_mut(right_page_id)?;
                let right_data: Vec<Cell> = right_cells
                    .iter()
                    .map(|c| Cell {
                        key: c.key,
                        c_ptr: c.c_ptr,
                        h_ptr: None,
                    })
                    .collect();

                right_page.rebuild_from_cells(
                    &right_data,
                    PageKind::Internal,
                    right_page_id,
                    overflow_hdr.ptr,
                )?;
                right_page.data
            };
            self.pager
                .log_index_diff(txn_id, right_page_id, &old_data, &new_data)?;
        };

        if let Some(&parent_id) = breadcrumbs.last() {
            let old_data = self.pager.index.fetch(parent_id)?.data;
            let (n_slot, new_data) = {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;
                let parent_hdr = parent_page.header()?;

                let parent_cells = parent_page.get_cells()?;
                let fixed_cells: Vec<Cell> = parent_cells
                    .into_iter()
                    .map(|c| Cell {
                        key: c.key,
                        c_ptr: c.c_ptr.map(|p| {
                            if p == overflow_page_id {
                                right_page_id
                            } else {
                                p
                            }
                        }),
                        h_ptr: c.h_ptr,
                    })
                    .collect();

                let fixed_ptr = if parent_hdr.ptr == overflow_page_id {
                    right_page_id
                } else {
                    parent_hdr.ptr
                };

                parent_page.rebuild_from_cells(
                    &fixed_cells,
                    parent_hdr.page_ty,
                    parent_id,
                    fixed_ptr,
                )?;

                let n_slot = parent_page.add_cell(promote_cell.key, overflow_page_id)?;
                (n_slot, parent_page.data)
            };
            self.pager
                .log_index_diff(txn_id, parent_id, &old_data, &new_data)?;

            if n_slot >= ORDER {
                self.handle_overfull(txn_id, breadcrumbs)
            } else {
                Ok(None)
            }
        } else {
            let new_root_id = self.pager.index.allocate(PageKind::Root)?;
            let old_data = self.pager.index.fetch(new_root_id)?.data;
            let new_data = {
                let mut root_page = self.pager.index.fetch_mut(new_root_id)?;
                root_page.init_header(new_root_id, PageKind::Root, right_page_id);
                root_page.add_cell(promote_cell.key, overflow_page_id)?;
                root_page.data
            };
            self.pager
                .log_index_diff(txn_id, new_root_id, &old_data, &new_data)?;

            let old_data = self.pager.index.fetch(overflow_page_id)?.data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id)?;
                let hdr = left_page.header()?;
                if hdr.page_ty == PageKind::Root {
                    left_page.set_page_kind(PageKind::Internal)?;
                }
                left_page.data
            };
            self.pager
                .log_index_diff(txn_id, overflow_page_id, &old_data, &new_data)?;

            Ok(Some(new_root_id))
        }
    }

    fn delete_inner(
        &self,
        txn_id: u64,
        key: Key,
        capture: bool,
    ) -> Result<(Records, Vec<u64>), Box<dyn Error>> {
        let _guard = self.exclusive()?;
        let crumbs = self.breadcrumbs(key)?;

        if !crumbs.found {
            return Err("Key not found".into());
        }

        let result = (|| -> Result<(Records, Vec<u64>), Box<dyn Error>> {
            let mut trail = crumbs.breadcrumb;

            let Some(leaf_id) = trail.pop() else {
                return Err("Empty breadcrumb trail".into());
            };

            let old_leaf_data = self.pager.index.fetch(leaf_id)?.data;
            let (remaining, removed) = {
                let mut leaf_page = self.pager.index.fetch_mut(leaf_id)?;
                let cells = leaf_page.get_cells()?;
                let idx = cells
                    .binary_search_by(|c| c.key.cmp(&key))
                    .map_err(|_| "Key not found in leaf page")?;
                let primary = cells[idx]
                    .h_ptr
                    .as_ref()
                    .ok_or("Leaf cell without heap pointer")?
                    .index;

                leaf_page.remove_cell_at(idx)?;
                (leaf_page.num_cells()?, primary)
            };

            let new_leaf_data = self.pager.index.fetch(leaf_id)?.data;
            self.pager
                .log_index_diff(txn_id, leaf_id, &old_leaf_data, &new_leaf_data)?;
            let mut old_records = Vec::new();
            if capture {
                self.pager.load_records(removed, &mut old_records)?;
            }
            let heap_chain = self.pager.remove_records(txn_id, removed)?;

            if leaf_id == self.root_id.load(Ordering::Acquire) {
                return Ok((old_records, heap_chain));
            }

            // if there's no underfull just propogate the key up the tree
            if remaining >= DEGREE {
                self.propagate_key_update(txn_id, key, leaf_id, &trail)?;
                return Ok((old_records, heap_chain));
            }

            self.handle_underfull(txn_id, leaf_id, &mut trail)?;

            Ok((old_records, heap_chain))
        })();
        self.poison_on_err(result)
    }

    pub fn delete_with_txn(&self, txn_id: u64, key: Key) -> Result<(), Box<dyn Error>> {
        self.delete(txn_id, key)
    }

    fn propagate_key_update(
        &self,
        txn_id: u64,
        old_key: Key,
        child_id: u64,
        ancestors: &[u64],
    ) -> Result<(), Box<dyn Error>> {
        // we know that the left most cell's key of a leaf node will always
        // be the same as the parent node's key pointing to it. so if we find
        // any of the occurence of deleted key while propogating up the tree and we can
        // replace it with `new_first_key`
        let new_first_key = {
            let leaf_page = self.pager.index.fetch(child_id)?;
            let leaf_cells = leaf_page.get_cells()?;
            match leaf_cells.first() {
                Some(cell) => cell.key,
                None => return Ok(()),
            }
        };

        for &ancestor_id in ancestors.iter().rev() {
            let found_idx = {
                let page = self.pager.index.fetch(ancestor_id)?;
                let cells = page.get_cells()?;
                cells.binary_search_by(|c| c.key.cmp(&old_key)).ok()
            };

            if let Some(idx) = found_idx {
                let old_data = self.pager.index.fetch(ancestor_id)?.data;
                let new_data = {
                    let mut page = self.pager.index.fetch_mut(ancestor_id)?;
                    if page.set_key_at(idx, new_first_key)? {
                        page.data
                    } else {
                        let hdr = page.header()?;
                        let cells = page.get_cells()?;

                        let updated_cells: Vec<Cell> = cells
                            .into_iter()
                            .enumerate()
                            .map(|(i, c)| {
                                if i == idx {
                                    Cell {
                                        key: new_first_key,
                                        c_ptr: c.c_ptr,
                                        h_ptr: c.h_ptr,
                                    }
                                } else {
                                    c
                                }
                            })
                            .collect();

                        page.rebuild_from_cells(&updated_cells, hdr.page_ty, hdr.id, hdr.ptr)?;
                        page.data
                    }
                };
                self.pager
                    .log_index_diff(txn_id, ancestor_id, &old_data, &new_data)?;

                // there will only be ONE occurence at MAX so just
                // exit when you find one.
                return Ok(());
            }
        }

        Ok(())
    }

    fn handle_underfull(
        &self,
        txn_id: u64,
        underfull_id: u64,
        ancestors: &mut Vec<u64>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(&parent_id) = ancestors.last() else {
            return Ok(());
        };

        let (parent_hdr, parent_cells) = {
            let parent_page = self.pager.index.fetch(parent_id)?;
            let hdr = parent_page.header()?;
            let cells = parent_page.get_cells()?;
            (hdr, cells)
        };

        let sibling = Self::find_siblings(underfull_id, &parent_cells, &parent_hdr)?;

        let child_idx = sibling.child_idx;
        let left_sibling_id = sibling.left;
        let right_sibling_id = sibling.right;

        let underfull_hdr = {
            let page = self.pager.index.fetch(underfull_id)?;
            page.header()?
        };

        if let Some(left_id) = left_sibling_id {
            let left_cells = {
                let page = self.pager.index.fetch(left_id)?;
                page.get_cells()?
            };

            if left_cells.len() > DEGREE {
                return self.redistribute_from_left(
                    txn_id,
                    left_id,
                    underfull_id,
                    parent_id,
                    child_idx,
                    &underfull_hdr,
                );
            }
        }

        if let Some(right_id) = right_sibling_id {
            let right_cells = {
                let page = self.pager.index.fetch(right_id)?;
                page.get_cells()?
            };

            if right_cells.len() > DEGREE {
                return self.redistribute_from_right(
                    txn_id,
                    underfull_id,
                    right_id,
                    parent_id,
                    child_idx,
                    &underfull_hdr,
                );
            }
        }

        if let Some(left_id) = left_sibling_id {
            self.merge_with_left(
                txn_id,
                left_id,
                underfull_id,
                parent_id,
                child_idx,
                ancestors,
            )?;
        } else if let Some(right_id) = right_sibling_id {
            self.merge_with_right(
                txn_id,
                underfull_id,
                right_id,
                parent_id,
                child_idx,
                ancestors,
            )?;
        }

        Ok(())
    }

    /// this method finds the left and right siblings of a given child page in the parent page's cells.
    /// It returns the index of the child in the parent's cells, and the IDs of the left and right
    /// siblings if they exist...
    fn find_siblings(
        child_id: u64,
        parent_cells: &[slotted_page::Cell],
        parent_hdr: &header::IndexHeader,
    ) -> Result<Sibling, Box<dyn Error>> {
        let mut child_idx = parent_cells.len();

        for (i, cell) in parent_cells.iter().enumerate() {
            if cell.c_ptr == Some(child_id) {
                child_idx = i;
                break;
            }
        }

        if child_idx == parent_cells.len() && parent_hdr.ptr == child_id {
            let left = if parent_cells.is_empty() {
                None
            } else {
                let last = parent_cells.len() - 1;
                parent_cells[last].c_ptr
            };
            return Ok(Sibling {
                child_idx,
                left,
                right: None,
            });
        }

        let left = if child_idx > 0 {
            parent_cells[child_idx - 1].c_ptr
        } else {
            None
        };

        let right = if child_idx + 1 < parent_cells.len() {
            parent_cells[child_idx + 1].c_ptr
        } else if child_idx < parent_cells.len() {
            Some(parent_hdr.ptr)
        } else {
            None
        };

        Ok(Sibling {
            child_idx,
            left,
            right,
        })
    }

    fn redistribute_from_left(
        &self,
        txn_id: u64,
        left_id: u64,
        underfull_id: u64,
        parent_id: u64,
        child_idx: usize,
        underfull_hdr: &header::IndexHeader,
    ) -> Result<(), Box<dyn Error>> {
        let is_leaf = underfull_hdr.page_ty == PageKind::Leaf;

        let old_left_data = self.pager.index.fetch(left_id)?.data;
        let (borrowed_cell, separator_idx) = {
            let mut left_page = self.pager.index.fetch_mut(left_id)?;
            let left_cells = left_page.get_cells()?;
            let last_idx = left_cells.len() - 1;
            let cell = left_page.remove_cell_at(last_idx)?;
            (cell, child_idx - 1)
        };

        if is_leaf {
            let new_left_data = self.pager.index.fetch(left_id)?.data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;
                let value = borrowed_cell.h_ptr.as_ref().unwrap().index;
                underfull_page.add_cell(borrowed_cell.key, value)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let new_separator = {
                let underfull_page = self.pager.index.fetch(underfull_id)?;
                let cells = underfull_page.get_cells()?;
                cells
                    .first()
                    .ok_or("Empty underfull after redistribute")?
                    .key
            };

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(new_separator, left_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id)?;
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let left_old_rightmost = {
                let left_page = self.pager.index.fetch(left_id)?;
                left_page.header()?.ptr
            };

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;

                let underfull_cells = underfull_page.get_cells()?;
                let all_cells: Vec<slotted_page::Cell> = std::iter::once(slotted_page::Cell {
                    key: parent_sep_key,
                    c_ptr: Some(left_old_rightmost),
                    h_ptr: None,
                })
                .chain(underfull_cells)
                .collect();

                underfull_page.rebuild_from_cells(
                    &all_cells,
                    underfull_hdr.page_ty,
                    underfull_id,
                    underfull_hdr.ptr,
                )?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(borrowed_cell.key, left_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;

            {
                let mut left_page = self.pager.index.fetch_mut(left_id)?;
                if let Some(c) = borrowed_cell.c_ptr {
                    left_page.set_header_ptr(c)?;
                }
            }
            let new_left_data = self.pager.index.fetch(left_id)?.data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;
        }

        Ok(())
    }

    fn redistribute_from_right(
        &self,
        txn_id: u64,
        underfull_id: u64,
        right_id: u64,
        parent_id: u64,
        child_idx: usize,
        underfull_hdr: &header::IndexHeader,
    ) -> Result<(), Box<dyn Error>> {
        let is_leaf = underfull_hdr.page_ty == PageKind::Leaf;

        let old_right_data = self.pager.index.fetch(right_id)?.data;
        let borrowed_cell = {
            let mut right_page = self.pager.index.fetch_mut(right_id)?;
            right_page.remove_cell_at(0)?
        };

        let separator_idx = child_idx;

        if is_leaf {
            let new_right_data = self.pager.index.fetch(right_id)?.data;
            self.pager
                .log_index_diff(txn_id, right_id, &old_right_data, &new_right_data)?;

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;
                let value = borrowed_cell.h_ptr.as_ref().unwrap().index;
                underfull_page.add_cell(borrowed_cell.key, value)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let new_separator = {
                let right_page = self.pager.index.fetch(right_id)?;
                let cells = right_page.get_cells()?;
                cells.first().ok_or("Empty right after redistribute")?.key
            };

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(new_separator, underfull_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let new_right_data = self.pager.index.fetch(right_id)?.data;
            self.pager
                .log_index_diff(txn_id, right_id, &old_right_data, &new_right_data)?;

            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id)?;
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let borrowed_left_child = borrowed_cell.c_ptr.ok_or("Missing c_ptr")?;

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;
                let old_ptr = underfull_page.header()?.ptr;
                underfull_page.add_cell(parent_sep_key, old_ptr)?;
                underfull_page.set_header_ptr(borrowed_left_child)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(borrowed_cell.key, underfull_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        }

        Ok(())
    }

    fn merge_with_left(
        &self,
        txn_id: u64,
        left_id: u64,
        underfull_id: u64,
        parent_id: u64,
        child_idx: usize,
        ancestors: &mut Vec<u64>,
    ) -> Result<(), Box<dyn Error>> {
        let is_leaf = {
            let page = self.pager.index.fetch(underfull_id)?;
            page.header()?.page_ty == PageKind::Leaf
        };

        let separator_idx = child_idx - 1;

        let (left_cells, left_hdr) = {
            let page = self.pager.index.fetch(left_id)?;
            (page.get_cells()?, page.header()?)
        };
        let (underfull_cells, underfull_hdr) = {
            let page = self.pager.index.fetch(underfull_id)?;
            (page.get_cells()?, page.header()?)
        };

        if is_leaf {
            let mut merged: Vec<slotted_page::Cell> =
                Vec::with_capacity(left_cells.len() + underfull_cells.len());
            merged.extend(left_cells);
            merged.extend(underfull_cells);

            let old_left_data = self.pager.index.fetch(left_id)?.data;
            {
                let mut left_page = self.pager.index.fetch_mut(left_id)?;
                left_page.rebuild_from_cells(
                    &merged,
                    PageKind::Leaf,
                    left_id,
                    underfull_hdr.ptr,
                )?;
            }
            let new_left_data = self.pager.index.fetch(left_id)?.data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;

                parent_page.remove_cell_at(separator_idx)?;

                let parent_hdr_after = parent_page.header()?;
                let parent_cells_after = parent_page.get_cells()?;

                let fixed_cells: Vec<slotted_page::Cell> = parent_cells_after
                    .into_iter()
                    .map(|c| slotted_page::Cell {
                        key: c.key,
                        c_ptr: c.c_ptr.map(|p| if p == underfull_id { left_id } else { p }),
                        h_ptr: c.h_ptr,
                    })
                    .collect();

                let fixed_ptr = if parent_hdr_after.ptr == underfull_id {
                    left_id
                } else {
                    parent_hdr_after.ptr
                };

                parent_page.rebuild_from_cells(
                    &fixed_cells,
                    parent_hdr_after.page_ty,
                    parent_id,
                    fixed_ptr,
                )?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id)?;
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let mut merged: Vec<slotted_page::Cell> = Vec::new();
            merged.extend(left_cells);
            merged.push(slotted_page::Cell {
                key: parent_sep_key,
                c_ptr: Some(left_hdr.ptr),
                h_ptr: None,
            });
            merged.extend(underfull_cells);

            let old_left_data = self.pager.index.fetch(left_id)?.data;
            {
                let mut left_page = self.pager.index.fetch_mut(left_id)?;
                left_page.rebuild_from_cells(
                    &merged,
                    PageKind::Internal,
                    left_id,
                    underfull_hdr.ptr,
                )?;
            }
            let new_left_data = self.pager.index.fetch(left_id)?.data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;

                parent_page.remove_cell_at(separator_idx)?;

                let parent_hdr_after = parent_page.header()?;
                let parent_cells_after = parent_page.get_cells()?;

                let fixed_cells: Vec<slotted_page::Cell> = parent_cells_after
                    .into_iter()
                    .map(|c| slotted_page::Cell {
                        key: c.key,
                        c_ptr: c.c_ptr.map(|p| if p == underfull_id { left_id } else { p }),
                        h_ptr: None,
                    })
                    .collect();

                let fixed_ptr = if parent_hdr_after.ptr == underfull_id {
                    left_id
                } else {
                    parent_hdr_after.ptr
                };

                parent_page.rebuild_from_cells(
                    &fixed_cells,
                    parent_hdr_after.page_ty,
                    parent_id,
                    fixed_ptr,
                )?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        }

        self.free_index_page(txn_id, underfull_id);

        self.check_parent_underfull(txn_id, parent_id, ancestors)
    }

    fn merge_with_right(
        &self,
        txn_id: u64,
        underfull_id: u64,
        right_id: u64,
        parent_id: u64,
        child_idx: usize,
        ancestors: &mut Vec<u64>,
    ) -> Result<(), Box<dyn Error>> {
        let is_leaf = {
            let page = self.pager.index.fetch(underfull_id)?;
            page.header()?.page_ty == PageKind::Leaf
        };

        let separator_idx = child_idx;

        let (underfull_cells, underfull_hdr) = {
            let page = self.pager.index.fetch(underfull_id)?;
            (page.get_cells()?, page.header()?)
        };
        let (right_cells, right_hdr) = {
            let page = self.pager.index.fetch(right_id)?;
            (page.get_cells()?, page.header()?)
        };

        if is_leaf {
            let mut merged: Vec<slotted_page::Cell> =
                Vec::with_capacity(underfull_cells.len() + right_cells.len());
            merged.extend(underfull_cells);
            merged.extend(right_cells);

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;
                underfull_page.rebuild_from_cells(
                    &merged,
                    PageKind::Leaf,
                    underfull_id,
                    right_hdr.ptr,
                )?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;

                parent_page.remove_cell_at(separator_idx)?;

                let parent_hdr_after = parent_page.header()?;
                let parent_cells_after = parent_page.get_cells()?;

                let fixed_cells: Vec<slotted_page::Cell> = parent_cells_after
                    .into_iter()
                    .map(|c| slotted_page::Cell {
                        key: c.key,
                        c_ptr: c
                            .c_ptr
                            .map(|p| if p == right_id { underfull_id } else { p }),
                        h_ptr: c.h_ptr,
                    })
                    .collect();

                let fixed_ptr = if parent_hdr_after.ptr == right_id {
                    underfull_id
                } else {
                    parent_hdr_after.ptr
                };

                parent_page.rebuild_from_cells(
                    &fixed_cells,
                    parent_hdr_after.page_ty,
                    parent_id,
                    fixed_ptr,
                )?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id)?;
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let mut merged: Vec<slotted_page::Cell> = Vec::new();
            merged.extend(underfull_cells);
            merged.push(slotted_page::Cell {
                key: parent_sep_key,
                c_ptr: Some(underfull_hdr.ptr),
                h_ptr: None,
            });
            merged.extend(right_cells);

            let old_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id)?;
                underfull_page.rebuild_from_cells(
                    &merged,
                    PageKind::Internal,
                    underfull_id,
                    right_hdr.ptr,
                )?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id)?.data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id)?.data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id)?;

                parent_page.remove_cell_at(separator_idx)?;

                let parent_hdr_after = parent_page.header()?;
                let parent_cells_after = parent_page.get_cells()?;

                let fixed_cells: Vec<slotted_page::Cell> = parent_cells_after
                    .into_iter()
                    .map(|c| slotted_page::Cell {
                        key: c.key,
                        c_ptr: c
                            .c_ptr
                            .map(|p| if p == right_id { underfull_id } else { p }),
                        h_ptr: None,
                    })
                    .collect();

                let fixed_ptr = if parent_hdr_after.ptr == right_id {
                    underfull_id
                } else {
                    parent_hdr_after.ptr
                };

                parent_page.rebuild_from_cells(
                    &fixed_cells,
                    parent_hdr_after.page_ty,
                    parent_id,
                    fixed_ptr,
                )?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id)?.data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        }

        self.free_index_page(txn_id, right_id);

        self.check_parent_underfull(txn_id, parent_id, ancestors)
    }

    fn check_parent_underfull(
        &self,
        txn_id: u64,
        parent_id: u64,
        ancestors: &mut Vec<u64>,
    ) -> Result<(), Box<dyn Error>> {
        if parent_id == self.root_id.load(Ordering::Acquire) {
            let (root_cells_empty, new_root_id) = {
                let root_page = self.pager.index.fetch(parent_id)?;
                let root_cells = root_page.get_cells()?;
                let root_hdr = root_page.header()?;
                (root_cells.is_empty(), root_hdr.ptr)
            };

            if root_cells_empty {
                let old_root_data = self.pager.index.fetch(new_root_id)?.data;
                {
                    let mut new_root = self.pager.index.fetch_mut(new_root_id)?;
                    new_root.set_page_kind(PageKind::Root)?;
                }
                let new_root_data = self.pager.index.fetch(new_root_id)?.data;
                self.pager
                    .log_index_diff(txn_id, new_root_id, &old_root_data, &new_root_data)?;

                let stale_before = self.pager.index.fetch(parent_id)?.data;
                {
                    let mut stale = self.pager.index.fetch_mut(parent_id)?;
                    stale.set_page_kind(PageKind::Internal)?;
                }
                let stale_after = self.pager.index.fetch(parent_id)?.data;
                self.pager
                    .log_index_diff(txn_id, parent_id, &stale_before, &stale_after)?;

                self.root_id.store(new_root_id, Ordering::Release);
                self.free_index_page(txn_id, parent_id);
            }

            return Ok(());
        }

        let remaining = {
            let page = self.pager.index.fetch(parent_id)?;
            page.num_cells()?
        };

        if remaining < DEGREE {
            ancestors.pop();
            self.handle_underfull(txn_id, parent_id, ancestors)?;
        }

        Ok(())
    }

    fn breadcrumbs(&self, key: Key) -> Result<BreadCrumbs, Box<dyn Error>> {
        let mut breadcrumb: Vec<u64> = Vec::new();

        let mut page_id = self.root_id.load(Ordering::Acquire);

        loop {
            let (cells, p_hdr) =
                self.pager
                    .index
                    .with_page(page_id, |page| -> Result<_, Box<dyn Error>> {
                        let p_hdr = page.header()?;
                        Ok((page.get_cells()?, p_hdr))
                    })??;

            if breadcrumb.contains(&page_id) || breadcrumb.len() >= MAX_TREE_DEPTH {
                return Err(
                    format!("corrupt index: cycle or runaway depth at page {page_id}").into(),
                );
            }
            breadcrumb.push(page_id);

            let is_leaf_level = p_hdr.page_ty == PageKind::Leaf
                || (p_hdr.page_ty == PageKind::Root && p_hdr.is_root_leaf());

            match cells.binary_search_by(|c| c.key.cmp(&key)) {
                Ok(_i) => {
                    if is_leaf_level {
                        return Ok(BreadCrumbs {
                            breadcrumb,
                            found: true,
                        });
                    }
                    let child_page_id = if _i + 1 < cells.len() {
                        cells.get(_i + 1).unwrap().c_ptr.unwrap()
                    } else {
                        p_hdr.ptr
                    };
                    page_id = child_page_id;
                }
                Err(i) => {
                    if is_leaf_level {
                        return Ok(BreadCrumbs {
                            breadcrumb,
                            found: false,
                        });
                    }

                    let child_page_id = if i < cells.len() {
                        cells.get(i).unwrap().c_ptr.unwrap()
                    } else {
                        p_hdr.ptr
                    };
                    page_id = child_page_id;
                }
            }
        }
    }
}
/// Stack that stores the page ids while traversing to leaf
struct BreadCrumbs {
    breadcrumb: Vec<u64>,
    found: bool,
}

struct Sibling {
    child_idx: usize,
    left: Option<u64>,
    right: Option<u64>,
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use wal::RecordType;

    fn get_btree_in(dir: &std::path::Path) -> BplusTree {
        let heap_path = dir.join("heap_file.db");
        let index_path = dir.join("index_file.db");
        BplusTree::new(index_path, heap_path).unwrap()
    }

    #[test]
    fn merged_index_pages_are_reused_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let index_pages = |t: &BplusTree| t.pager.index.next_id.load(Ordering::SeqCst);
        let peak;
        {
            let btree = get_btree_in(dir.path());
            for key in 0..4000 {
                btree.insert(0, key, vec![vec![7; 16]]).unwrap();
            }
            peak = index_pages(&btree);
            for key in 0..4000 {
                btree.delete(0, key).unwrap();
            }
            assert!(btree.free_index_page_count() > 0);
            for key in 0..4000 {
                btree.insert(0, key, vec![vec![7; 16]]).unwrap();
            }
            assert_eq!(index_pages(&btree), peak);

            btree.begin_transaction(9).unwrap();
            let before = btree.free_index_page_count();
            for key in (0..4000).filter(|k| k % 4 != 0) {
                btree.delete(9, key).unwrap();
            }
            assert_eq!(btree.free_index_page_count(), before);
            btree.commit_transaction(9).unwrap();
            assert!(btree.free_index_page_count() > before);

            btree.begin_transaction(10).unwrap();
            for key in (0..4000).filter(|k| k % 4 == 0) {
                btree.delete(10, key).unwrap();
            }
            btree.abort_transaction(10).unwrap();
            btree.checkpoint_and_truncate().unwrap();
        }
        let btree = get_btree_in(dir.path());
        btree.recover().unwrap();
        let free_after_reopen = btree.free_index_page_count();
        assert!(free_after_reopen > 0);
        let keys: Vec<Key> = btree
            .range(0, Bound::Unbounded, Bound::Unbounded)
            .map(|e| e.unwrap().0)
            .collect();
        assert_eq!(keys, (0..4000).step_by(4).collect::<Vec<_>>());
        for key in (0..4000).filter(|k| k % 4 != 0) {
            btree.insert(0, key, vec![vec![7; 16]]).unwrap();
        }
        assert_eq!(index_pages(&btree), peak);
        assert_eq!(btree.get(0, 3999).unwrap(), vec![vec![7; 16]]);
    }

    #[test]
    fn flipped_bit_in_a_page_is_reported_as_corruption() {
        for (file, offset) in [("index_file.db", 4000), ("heap_file.db", 4090)] {
            let dir = tempfile::tempdir().unwrap();
            {
                let btree = get_btree_in(dir.path());
                for key in 0..20 {
                    btree.insert(0, key, vec![vec![key as u8; 200]]).unwrap();
                }
                btree.checkpoint_and_truncate().unwrap();
            }
            let path = dir.path().join(file);
            let mut bytes = std::fs::read(&path).unwrap();
            bytes[offset as usize] ^= 0x04;
            std::fs::write(&path, bytes).unwrap();
            let outcome = get_btree_result_in(dir.path()).and_then(|t| {
                t.recover()?;
                (0..20).try_for_each(|k| t.get(0, k).map(drop))
            });
            let err = outcome.expect_err(file).to_string();
            assert!(err.contains("checksum mismatch"), "{file}: {err}");
        }
    }

    #[test]
    fn zeroed_page_never_serves_a_wrong_answer() {
        let keys = 0..2000;
        let value = |k: Key| vec![(k % 251) as u8; 40];
        let build = |dir: &std::path::Path| {
            let btree = get_btree_in(dir);
            for key in keys.clone() {
                btree.insert(0, key, vec![value(key)]).unwrap();
            }
            btree.checkpoint_and_truncate().unwrap();
        };
        for (file, page_size) in [
            ("index_file.db", PAGE_SIZE),
            ("heap_file.db", heap::PAGE_SIZE),
        ] {
            let pages = {
                let dir = tempfile::tempdir().unwrap();
                build(dir.path());
                std::fs::metadata(dir.path().join(file)).unwrap().len() / page_size as u64
            };
            assert!(pages > 3, "{file}");
            let mut errors = 0;
            for zeroed in 0..pages {
                let dir = tempfile::tempdir().unwrap();
                build(dir.path());
                let path = dir.path().join(file);
                let mut bytes = std::fs::read(&path).unwrap();
                let start = zeroed as usize * page_size;
                bytes[start..start + page_size].fill(0);
                std::fs::write(&path, bytes).unwrap();
                let outcome = get_btree_result_in(dir.path()).and_then(|t| {
                    t.recover()?;
                    for key in keys.clone() {
                        assert_eq!(
                            t.get(0, key)?,
                            vec![value(key)],
                            "{file} page {zeroed} key {key}"
                        );
                    }
                    let scanned = t.scan(0).collect::<Result<Vec<_>, _>>()?;
                    assert_eq!(scanned.len(), keys.clone().count(), "{file} page {zeroed}");
                    Ok(())
                });
                if outcome.is_err() {
                    errors += 1;
                }
            }
            assert_eq!(errors, pages, "{file}: a zeroed page went unnoticed");
        }
    }

    fn get_btree_result_in(dir: &std::path::Path) -> Result<BplusTree, Box<dyn Error>> {
        BplusTree::new(dir.join("index_file.db"), dir.join("heap_file.db"))
    }

    #[test]
    fn last_key_in_matches_a_sorted_model() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        let mut model = std::collections::BTreeSet::new();
        let mut rng = io::Rng::new(7);
        for _ in 0..3000 {
            let key = (rng.below(20_000) * 3) as Key;
            if model.insert(key) {
                btree.insert(0, key, vec![vec![1]]).unwrap();
            }
        }
        for key in model.clone().into_iter().filter(|k| k % 2 == 0).take(1200) {
            btree.delete(0, key).unwrap();
            model.remove(&key);
        }
        for _ in 0..2000 {
            let a = rng.below(61_000) as Key;
            let b = a + 1 + rng.below(3_000) as Key;
            let bounds = match rng.below(3) {
                0 => (Bound::Included(a), Bound::Included(b)),
                1 => (Bound::Excluded(a), Bound::Excluded(b)),
                _ => (Bound::Included(a), Bound::Unbounded),
            };
            let expected = model.range(bounds).next_back().copied();
            assert_eq!(
                btree.last_key_in(0, bounds.0, bounds.1).unwrap(),
                expected,
                "{bounds:?}"
            );
        }
        assert_eq!(
            btree
                .last_key_in(0, Bound::Unbounded, Bound::Excluded(0))
                .unwrap(),
            None
        );
    }

    #[test]
    fn random_inserts_and_deletes_match_a_model() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        let mut model = std::collections::BTreeSet::new();
        let mut rng = io::Rng::new(0xB7);
        for step in 0..40_000 {
            let key = rng.below(1_500) as Key;
            let delete_bias = if (step / 5_000) % 2 == 0 { 3 } else { 6 };
            if rng.below(10) < delete_bias {
                assert_eq!(
                    btree.delete(0, key).is_ok(),
                    model.remove(&key),
                    "step {step}: delete {key}"
                );
            } else {
                let inserted = btree.insert(0, key, vec![vec![step as u8]]);
                assert_eq!(
                    inserted.is_ok(),
                    model.insert(key),
                    "step {step}: insert {key}: {inserted:?}"
                );
            }
        }
        let keys: Vec<Key> = btree.scan(0).map(|entry| entry.unwrap().0).collect();
        assert_eq!(keys, model.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn single_insert_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"hello".to_vec()]).unwrap();

        let result = btree.get(0, 1).unwrap();
        assert_eq!(result, vec![b"hello".to_vec()]);
    }

    #[test]
    fn insert_and_get_multiple_keys_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 5, vec![b"five".to_vec()]).unwrap();
        btree.insert(0, 3, vec![b"three".to_vec()]).unwrap();
        btree.insert(0, 7, vec![b"seven".to_vec()]).unwrap();
        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 1).unwrap(), vec![b"one".to_vec()]);
        assert_eq!(btree.get(0, 3).unwrap(), vec![b"three".to_vec()]);
        assert_eq!(btree.get(0, 5).unwrap(), vec![b"five".to_vec()]);
        assert_eq!(btree.get(0, 7).unwrap(), vec![b"seven".to_vec()]);
    }

    #[test]
    fn get_nonexistent_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();

        let result = btree.get(0, 999);
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_insert_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();
        let result = btree.insert(0, 1, vec![b"one again".to_vec()]);
        assert!(result.is_err());
    }

    #[test]
    fn insert_and_get_multiple_data_records() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let records = vec![b"field1".to_vec(), b"field2".to_vec(), b"field3".to_vec()];
        btree.insert(0, 42, records.clone()).unwrap();

        let result = btree.get(0, 42).unwrap();
        assert_eq!(result, records);
    }

    #[test]
    fn delete_from_root_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();
        btree.insert(0, 2, vec![b"two".to_vec()]).unwrap();
        btree.insert(0, 3, vec![b"three".to_vec()]).unwrap();

        btree.delete(0, 2).unwrap();

        assert!(btree.get(0, 2).is_err());
        assert_eq!(btree.get(0, 1).unwrap(), vec![b"one".to_vec()]);
        assert_eq!(btree.get(0, 3).unwrap(), vec![b"three".to_vec()]);
    }

    #[test]
    fn delete_nonexistent_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();
        let result = btree.delete(0, 999);
        assert!(result.is_err());
    }

    #[test]
    fn delete_all_from_root() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();
        btree.insert(0, 2, vec![b"two".to_vec()]).unwrap();

        btree.delete(0, 1).unwrap();
        btree.delete(0, 2).unwrap();

        assert!(btree.get(0, 1).is_err());
        assert!(btree.get(0, 2).is_err());
    }

    #[test]
    fn insert_ascending_order() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in 1..=20 {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=20 {
            let result = btree.get(0, i).unwrap();
            assert_eq!(result, vec![format!("val-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn insert_descending_order() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in (1..=20).rev() {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=20 {
            let result = btree.get(0, i).unwrap();
            assert_eq!(result, vec![format!("val-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn many_inserts_cause_splits() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as Key * 4;
        for i in 1..=count {
            btree
                .insert(0, i, vec![format!("data-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=count {
            let result = btree.get(0, i).unwrap();
            assert_eq!(
                result,
                vec![format!("data-{}", i).into_bytes()],
                "Failed to get key {}",
                i
            );
        }

        assert!(btree.get(0, 0).is_err());
        assert!(btree.get(0, count + 1).is_err());
    }

    #[test]
    fn internal_node_exact_separator_lookups() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as Key * 3;
        for i in (1..=count).step_by(2) {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in (1..=count).step_by(2) {
            let result = btree.get(0, i);
            assert!(result.is_ok(), "Key {} should exist", i);
            assert_eq!(result.unwrap(), vec![format!("val-{}", i).into_bytes()]);
        }

        for i in (2..=count).step_by(2) {
            let result = btree.get(0, i);
            assert!(result.is_err(), "Key {} should not exist", i);
        }
    }

    #[test]
    fn internal_node_deletion_and_merge() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as Key * 3;
        for i in 1..=count {
            btree
                .insert(0, i, vec![format!("data-{}", i).into_bytes()])
                .unwrap();
        }

        // delete odd keys to trigger leaf/internal redistributions and merges
        for i in (1..=count).step_by(2) {
            btree.delete(0, i).unwrap();
        }

        // check odd keys are gone and even keys remain
        for i in 1..=count {
            if i % 2 == 1 {
                assert!(btree.get(0, i).is_err(), "Key {} should be deleted", i);
            } else {
                let result = btree.get(0, i);
                assert!(result.is_ok(), "Key {} should still exist", i);
                assert_eq!(result.unwrap(), vec![format!("data-{}", i).into_bytes()]);
            }
        }

        // reinsert deleted keys and check
        for i in (1..=count).step_by(2) {
            btree
                .insert(0, i, vec![format!("re-data-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=count {
            let result = btree.get(0, i).unwrap();
            if i % 2 == 1 {
                assert_eq!(result, vec![format!("re-data-{}", i).into_bytes()]);
            } else {
                assert_eq!(result, vec![format!("data-{}", i).into_bytes()]);
            }
        }
    }

    #[test]
    fn internal_node_root_collapse_on_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as Key * 2;
        for i in 1..=count {
            btree
                .insert(0, i, vec![format!("v-{}", i).into_bytes()])
                .unwrap();
        }

        // delete almost all keys except a few, triggering multiple merges and root collapse
        for i in 4..=count {
            btree.delete(0, i).unwrap();
        }

        assert_eq!(btree.get(0, 1).unwrap(), vec![b"v-1".to_vec()]);
        assert_eq!(btree.get(0, 2).unwrap(), vec![b"v-2".to_vec()]);
        assert_eq!(btree.get(0, 3).unwrap(), vec![b"v-3".to_vec()]);
        assert!(btree.get(0, 4).is_err());

        // insert new keys into the collapsed root
        btree.insert(0, 100, vec![b"v-100".to_vec()]).unwrap();
        assert_eq!(btree.get(0, 100).unwrap(), vec![b"v-100".to_vec()]);
    }

    #[test]
    fn multi_level_mixed_workload() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = 300;
        for i in 1..=count {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 100..=200 {
            btree.delete(0, i).unwrap();
        }

        for i in 1..=count {
            if (100..=200).contains(&i) {
                assert!(btree.get(0, i).is_err(), "Key {} should be deleted", i);
            } else {
                assert_eq!(
                    btree.get(0, i).unwrap(),
                    vec![format!("val-{}", i).into_bytes()]
                );
            }
        }

        for i in 500..=600 {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 500..=600 {
            assert_eq!(
                btree.get(0, i).unwrap(),
                vec![format!("val-{}", i).into_bytes()]
            );
        }
    }

    #[test]
    fn delete_all_from_multi_level_tree() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as Key * 2;
        for i in 1..=count {
            btree
                .insert(0, i, vec![format!("data-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=count {
            btree.delete(0, i).unwrap();
            assert!(btree.get(0, i).is_err());
        }

        // tree is empty... getting any key should return error
        for i in 1..=count {
            assert!(btree.get(0, i).is_err());
        }

        // can insert again into empty root
        btree.insert(0, 42, vec![b"forty-two".to_vec()]).unwrap();
        assert_eq!(btree.get(0, 42).unwrap(), vec![b"forty-two".to_vec()]);
    }

    #[test]
    fn insert_delete_reinsert() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"first".to_vec()]).unwrap();
        btree.delete(0, 1).unwrap();
        btree.insert(0, 1, vec![b"second".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 1).unwrap(), vec![b"second".to_vec()]);
    }

    #[test]
    fn empty_tree_get_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        assert!(btree.get(0, 1).is_err());
    }

    #[test]
    fn large_data_records() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let big_data = vec![0xABu8; 4000];
        btree.insert(0, 1, vec![big_data.clone()]).unwrap();

        let result = btree.get(0, 1).unwrap();
        assert_eq!(result, vec![big_data]);
    }

    #[test]
    fn interleaved_insert_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in 1..=10 {
            btree
                .insert(0, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
            let result = btree.get(0, i).unwrap();
            assert_eq!(result, vec![format!("v{}", i).into_bytes()]);
        }
    }

    #[test]
    fn boundary_keys() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 0, vec![b"zero".to_vec()]).unwrap();
        btree.insert(0, Key::MAX, vec![b"max".to_vec()]).unwrap();
        btree.insert(0, 1, vec![b"one".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 0).unwrap(), vec![b"zero".to_vec()]);
        assert_eq!(btree.get(0, Key::MAX).unwrap(), vec![b"max".to_vec()]);
        assert_eq!(btree.get(0, 1).unwrap(), vec![b"one".to_vec()]);
    }

    #[test]
    fn buffer_pool_flush_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");

        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            for i in 1..=50 {
                btree
                    .insert(0, i, vec![format!("value-{}", i).into_bytes()])
                    .unwrap();
            }
            btree.pager.flush_all().unwrap();
        }

        {
            let reopened = Pager::new(index_path, heap_path).unwrap();
            let btree = BplusTree {
                root_id: AtomicU64::new(0),
                pager: reopened,
                tree_lock: RwLock::new(()),
                undo: Mutex::new(HashMap::new()),
            };
            for i in 1..=50 {
                let res = btree.get(0, i).unwrap();
                assert_eq!(res, vec![format!("value-{}", i).into_bytes()]);
            }
        }
    }

    #[test]
    fn buffer_pool_discard_dirty_simulation() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1, vec![b"first".to_vec()]).unwrap();
        btree.pager.flush_all().unwrap();

        btree
            .insert(0, 2, vec![b"uncommitted shit".to_vec()])
            .unwrap();
        btree.pager.discard_all_dirty();

        // 1 was flushed... so refetching from disk should wokr..
        assert_eq!(btree.get(0, 1).unwrap(), vec![b"first".to_vec()]);
    }

    #[test]
    fn wal_transaction_commit_and_abort() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 1, vec![b"val1".to_vec()]).unwrap();
        btree.commit_transaction(t1).unwrap();

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        btree.insert(t2, 2, vec![b"val2".to_vec()]).unwrap();
        btree.abort_transaction(t2).unwrap();

        let records = btree.pager.wal.read_all_records().unwrap();
        assert!(!records.is_empty());
        assert!(
            records
                .iter()
                .any(|r| r.header.record_type == RecordType::Commit && r.header.txn_id == t1)
        );
        assert!(
            records
                .iter()
                .any(|r| r.header.record_type == RecordType::Abort && r.header.txn_id == t2)
        );
    }

    #[test]
    fn wal_steal_flush_lsn_invariant() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 42, vec![b"val42".to_vec()]).unwrap();

        let (idx_dirty, _) = btree.pager.dirty_pages();
        assert!(!idx_dirty.is_empty());

        for page_id in idx_dirty {
            let page_lsn = btree.pager.index.pool.page_lsn(page_id);
            assert!(page_lsn > 0);
            btree.pager.flush_index_page(page_id).unwrap();
            assert!(btree.pager.wal.flushed_lsn() >= page_lsn);
        }
    }

    #[test]
    fn fuzzy_checkpointing_preserves_dirty_pages_and_records_dpt_att() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 100, vec![b"hello".to_vec()]).unwrap();

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        btree.insert(t2, 200, vec![b"world".to_vec()]).unwrap();

        let cp_lsn = btree.fuzzy_checkpoint().unwrap();
        assert!(cp_lsn > 0);
        assert_eq!(btree.pager.wal.last_checkpoint_lsn(), cp_lsn);

        let all = btree.pager.wal.read_all_records().unwrap();
        let cp_rec = all.iter().find(|r| r.header.lsn == cp_lsn).unwrap();
        assert_eq!(cp_rec.header.record_type, RecordType::Checkpoint);

        let cp_data = cp_rec.parse_checkpoint().unwrap();
        assert!(cp_data.att.iter().any(|a| a.txn_id == t1));
        assert!(cp_data.att.iter().any(|a| a.txn_id == t2));
        assert!(!cp_data.dpt.is_empty());
        for d in &cp_data.dpt {
            assert!(d.rec_lsn > 0);
        }

        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        assert!(!idx_dirty.is_empty());
        assert!(!heap_dirty.is_empty());
    }

    #[test]
    fn crash_recovery_redo_and_undo_with_fuzzy_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 10, vec![b"val10".to_vec()]).unwrap();
        btree.commit_transaction(t1).unwrap();

        let cp_lsn = btree.fuzzy_checkpoint().unwrap();
        assert!(cp_lsn > 0);

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        btree.insert(t2, 20, vec![b"val20".to_vec()]).unwrap();
        btree.commit_transaction(t2).unwrap();

        let t3 = 3;
        btree.begin_transaction(t3).unwrap();
        btree.insert(t3, 30, vec![b"val30".to_vec()]).unwrap();

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }

        btree.pager.discard_all_dirty();

        btree.recover().unwrap();

        assert_eq!(btree.get(0, 10).unwrap(), vec![b"val10".to_vec()]);
        assert_eq!(btree.get(0, 20).unwrap(), vec![b"val20".to_vec()]);
        assert!(btree.get(0, 30).is_err());
    }

    #[test]
    fn crash_recovery_undo_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 10, vec![b"val10".to_vec()]).unwrap();
        btree.commit_transaction(t1).unwrap();

        let t1b = 2;
        btree.begin_transaction(t1b).unwrap();
        btree.insert(t1b, 20, vec![b"val20".to_vec()]).unwrap();
        btree.commit_transaction(t1b).unwrap();

        btree.fuzzy_checkpoint().unwrap();

        let t2 = 3;
        btree.begin_transaction(t2).unwrap();
        btree.delete(t2, 10).unwrap();

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }

        btree.pager.discard_all_dirty();

        btree.recover().unwrap();

        assert_eq!(btree.get(0, 10).unwrap(), vec![b"val10".to_vec()]);
        assert_eq!(btree.get(0, 20).unwrap(), vec![b"val20".to_vec()]);
    }

    #[test]
    fn fuzzy_checkpoint_recovery_idempotence() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        btree.insert(t1, 5, vec![b"val5".to_vec()]).unwrap();
        btree.commit_transaction(t1).unwrap();

        btree.fuzzy_checkpoint().unwrap();

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        btree.insert(t2, 6, vec![b"val6".to_vec()]).unwrap();
        btree.commit_transaction(t2).unwrap();

        let t3 = 3;
        btree.begin_transaction(t3).unwrap();
        btree.insert(t3, 7, vec![b"val7".to_vec()]).unwrap();

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }

        btree.pager.discard_all_dirty();

        btree.recover().unwrap();
        assert_eq!(btree.get(0, 5).unwrap(), vec![b"val5".to_vec()]);
        assert_eq!(btree.get(0, 6).unwrap(), vec![b"val6".to_vec()]);
        assert!(btree.get(0, 7).is_err());

        btree.recover().unwrap();
        assert_eq!(btree.get(0, 5).unwrap(), vec![b"val5".to_vec()]);
        assert_eq!(btree.get(0, 6).unwrap(), vec![b"val6".to_vec()]);
        assert!(btree.get(0, 7).is_err());
    }

    #[test]
    fn crash_recovery_multi_level_tree_splits() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let t1 = 1;
        btree.begin_transaction(t1).unwrap();
        for i in 1..=30 {
            btree
                .insert(t1, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
        }
        btree.commit_transaction(t1).unwrap();

        let cp_lsn = btree.fuzzy_checkpoint().unwrap();
        assert!(cp_lsn > 0);

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        for i in 31..=60 {
            btree
                .insert(t2, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
        }
        btree.commit_transaction(t2).unwrap();

        let t3 = 3;
        btree.begin_transaction(t3).unwrap();
        for i in 61..=80 {
            btree
                .insert(t3, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
        }

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }

        btree.pager.discard_all_dirty();

        btree.recover().unwrap();

        for i in 1..=60 {
            assert_eq!(
                btree.get(0, i).unwrap(),
                vec![format!("v{}", i).into_bytes()]
            );
        }
        for i in 61..=80 {
            assert!(btree.get(0, i).is_err());
        }
    }

    #[test]
    fn crash_recovery_reopen_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");

        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            let t1 = 1;
            btree.begin_transaction(t1).unwrap();
            btree.insert(t1, 100, vec![b"first_val".to_vec()]).unwrap();
            btree.commit_transaction(t1).unwrap();

            btree.fuzzy_checkpoint().unwrap();

            let t2 = 2;
            btree.begin_transaction(t2).unwrap();
            btree.insert(t2, 200, vec![b"second_val".to_vec()]).unwrap();
            btree.commit_transaction(t2).unwrap();

            let t3 = 3;
            btree.begin_transaction(t3).unwrap();
            btree.insert(t3, 300, vec![b"third_val".to_vec()]).unwrap();

            btree.pager.wal.flush_all().unwrap();
            let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
            for p in idx_dirty {
                btree.pager.flush_index_page(p).unwrap();
            }
            for p in heap_dirty {
                btree.pager.flush_heap_page(p).unwrap();
            }
        }

        {
            let btree = BplusTree::new(index_path, heap_path).unwrap();
            btree.recover().unwrap();

            assert_eq!(btree.get(0, 100).unwrap(), vec![b"first_val".to_vec()]);
            assert_eq!(btree.get(0, 200).unwrap(), vec![b"second_val".to_vec()]);
            assert!(btree.get(0, 300).is_err());
        }
    }

    #[test]
    fn caller_provided_transaction_id_flow() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 10, vec![b"autocommit".to_vec()]).unwrap();
        assert_eq!(btree.get(0, 10).unwrap(), vec![b"autocommit".to_vec()]);

        let txn_user = 1001;
        let txn_order = 2002;

        btree.begin_transaction(txn_user).unwrap();
        btree
            .insert(txn_user, 20, vec![b"user_data".to_vec()])
            .unwrap();
        assert_eq!(
            btree.get(txn_user, 20).unwrap(),
            vec![b"user_data".to_vec()]
        );

        btree.begin_transaction(txn_order).unwrap();
        btree
            .insert(txn_order, 30, vec![b"order_data".to_vec()])
            .unwrap();
        assert_eq!(
            btree.get(txn_order, 30).unwrap(),
            vec![b"order_data".to_vec()]
        );

        let cp_lsn = btree.fuzzy_checkpoint().unwrap();
        assert!(cp_lsn > 0);

        let records = btree.pager.wal.read_all_records().unwrap();
        let cp_rec = records.iter().find(|r| r.header.lsn == cp_lsn).unwrap();
        let cp_data = cp_rec.parse_checkpoint().unwrap();
        assert!(cp_data.att.iter().any(|a| a.txn_id == txn_user));
        assert!(cp_data.att.iter().any(|a| a.txn_id == txn_order));

        btree.commit_transaction(txn_user).unwrap();

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }
        btree.pager.discard_all_dirty();

        btree.recover().unwrap();

        assert_eq!(btree.get(0, 10).unwrap(), vec![b"autocommit".to_vec()]);
        assert_eq!(btree.get(0, 20).unwrap(), vec![b"user_data".to_vec()]);
        assert!(btree.get(0, 30).is_err());
    }

    #[test]
    fn concurrent_readers_and_writers() {
        let dir = tempfile::tempdir().unwrap();
        let btree = Arc::new(get_btree_in(dir.path()));

        for i in 1..=50 {
            btree
                .insert(0, i, vec![format!("pre-{}", i).into_bytes()])
                .unwrap();
        }

        let mut handles = Vec::new();

        for t in 0..4 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                for i in 1..=50 {
                    let res = tree.get(0, i).unwrap();
                    assert_eq!(res, vec![format!("pre-{}", i).into_bytes()]);
                }
            }));

            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                let start = 100 + t * 25;
                for i in start..start + 25 {
                    let txn_id = 1000 + i;
                    tree.begin_transaction(txn_id).unwrap();
                    tree.insert(txn_id, i as Key, vec![format!("thread-{}", i).into_bytes()])
                        .unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 1..=50 {
            let res = btree.get(0, i).unwrap();
            assert_eq!(res, vec![format!("pre-{}", i).into_bytes()]);
        }

        for t in 0..4 {
            let start = 100 + t * 25;
            for i in start..start + 25 {
                let res = btree.get(0, i).unwrap();
                assert_eq!(res, vec![format!("thread-{}", i).into_bytes()]);
            }
        }
    }

    #[test]
    fn concurrent_insert_splits() {
        let dir = tempfile::tempdir().unwrap();
        let btree = Arc::new(get_btree_in(dir.path()));

        let num_threads = 6;
        let items_per_thread = 40;
        let mut handles = Vec::new();

        for t in 0..num_threads {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                let start = t * items_per_thread + 1;
                let end = start + items_per_thread;
                for i in start..end {
                    let txn_id = 5000 + i;
                    tree.begin_transaction(txn_id).unwrap();
                    tree.insert(txn_id, i as Key, vec![format!("item-{}", i).into_bytes()])
                        .unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let total = num_threads * items_per_thread;
        for i in 1..=total {
            let res = btree.get(0, i as Key).unwrap();
            assert_eq!(res, vec![format!("item-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn concurrent_mutations_and_fuzzy_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let btree = Arc::new(get_btree_in(dir.path()));

        let mut handles = Vec::new();

        for t in 0..3 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                let start = t * 30 + 1;
                for i in start..start + 30 {
                    let txn_id = 10000 + i;
                    tree.begin_transaction(txn_id).unwrap();
                    tree.insert(txn_id, i as Key, vec![format!("val-{}", i).into_bytes()])
                        .unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        let tree = Arc::clone(&btree);
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                let _ = tree.fuzzy_checkpoint();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }));

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 1..=90 {
            let res = btree.get(0, i).unwrap();
            assert_eq!(res, vec![format!("val-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn concurrent_deletions_and_lookups() {
        let dir = tempfile::tempdir().unwrap();
        let btree = Arc::new(get_btree_in(dir.path()));

        for i in 1..=100 {
            btree
                .insert(0, i, vec![format!("data-{}", i).into_bytes()])
                .unwrap();
        }

        let mut handles = Vec::new();

        for t in 0..2 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                let start = t * 25 + 1;
                for i in start..start + 25 {
                    let txn_id = 20000 + i;
                    tree.begin_transaction(txn_id).unwrap();
                    tree.delete(txn_id, i as Key).unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        for _ in 0..2 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                for i in 51..=100 {
                    let res = tree.get(0, i);
                    if let Ok(val) = res {
                        assert_eq!(val, vec![format!("data-{}", i).into_bytes()]);
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 1..=50 {
            assert!(btree.get(0, i).is_err());
        }
        for i in 51..=100 {
            let res = btree.get(0, i).unwrap();
            assert_eq!(res, vec![format!("data-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn concurrent_transactions_commit_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let btree = Arc::new(get_btree_in(dir.path()));

        for i in 1..=10 {
            btree
                .insert(0, i, vec![format!("base-{}", i).into_bytes()])
                .unwrap();
        }

        let mut handles = Vec::new();

        for t in 0..4 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                let start = 100 + t * 25;
                for i in start..start + 25 {
                    let txn_id = 30000 + i;
                    tree.begin_transaction(txn_id).unwrap();
                    tree.insert(
                        txn_id,
                        i as Key,
                        vec![format!("committed-{}", i).into_bytes()],
                    )
                    .unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        let tree = Arc::clone(&btree);
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                let _ = tree.fuzzy_checkpoint();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }));

        for handle in handles {
            handle.join().unwrap();
        }

        btree.pager.wal.flush_all().unwrap();
        let (idx_dirty, heap_dirty) = btree.pager.dirty_pages();
        for p in idx_dirty {
            btree.pager.flush_index_page(p).unwrap();
        }
        for p in heap_dirty {
            btree.pager.flush_heap_page(p).unwrap();
        }

        btree.pager.discard_all_dirty();

        btree.recover().unwrap();

        for i in 1..=10 {
            let res = btree.get(0, i).unwrap();
            assert_eq!(res, vec![format!("base-{}", i).into_bytes()]);
        }

        for t in 0..4 {
            let start = 100 + t * 25;
            for i in start..start + 25 {
                let res = btree.get(0, i).unwrap();
                assert_eq!(res, vec![format!("committed-{}", i).into_bytes()]);
            }
        }
    }

    #[test]
    fn wal_steal_eviction_forces_wal_flush_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("test_index.db");
        let wal_path = dir.path().join("test_wal.db");

        let wal = Arc::new(wal::Wal::new(wal_path).unwrap());
        let idx_file = io::Vfs::open(&io::OsVfs, &idx_path).unwrap();

        let index =
            index::Index::new_with_capacity(idx_file, idx_path, Arc::clone(&wal), 2).unwrap();

        let p0 = index.allocate(PageKind::Leaf).unwrap();
        let _p1 = index.allocate(PageKind::Leaf).unwrap();

        let payload = wal::UpdatePayload {
            offset_in_page: 0,
            undo_data: &[0u8; 4],
            redo_data: &[1u8; 4],
        };
        let lsn = wal
            .write_record(1, wal::RecordType::Update, p0, true, payload)
            .unwrap();
        index.pool.update_lsn(p0, lsn);

        assert!(wal.flushed_lsn() < lsn);

        let _p2 = index.allocate(PageKind::Leaf).unwrap();
        let _p3 = index.allocate(PageKind::Leaf).unwrap();

        assert!(wal.flushed_lsn() >= lsn);
    }

    #[test]
    fn heap_steal_eviction_forces_wal_flush_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let heap_path = dir.path().join("test_heap.db");
        let wal_path = dir.path().join("test_wal.db");

        let wal = Arc::new(wal::Wal::new(wal_path).unwrap());
        let heap_file = io::Vfs::open(&io::OsVfs, &heap_path).unwrap();

        let heap =
            heap::Heap::new_with_capacity(heap_file, heap_path, Arc::clone(&wal), 2).unwrap();

        let p0 = heap.allocate().unwrap();
        let _p1 = heap.allocate().unwrap();

        let payload = wal::UpdatePayload {
            offset_in_page: 0,
            undo_data: &[0u8; 4],
            redo_data: &[1u8; 4],
        };
        let lsn = wal
            .write_record(1, wal::RecordType::Update, p0, false, payload)
            .unwrap();
        heap.pool.update_lsn(p0, lsn);

        assert!(wal.flushed_lsn() < lsn);

        let _p2 = heap.allocate().unwrap();
        let _p3 = heap.allocate().unwrap();

        assert!(wal.flushed_lsn() >= lsn);
    }
    fn keys(iter: RangeIter<'_>) -> Vec<Key> {
        iter.map(|r| r.unwrap().0).collect()
    }

    #[test]
    fn abort_undoes_inserts_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        btree.insert(0, 1, vec![b"kept".to_vec()]).unwrap();

        btree.begin_transaction(7).unwrap();
        btree.insert(7, 2, vec![b"new".to_vec()]).unwrap();
        btree.delete(7, 1).unwrap();
        assert!(btree.get(0, 1).is_err());
        btree.abort_transaction(7).unwrap();

        assert_eq!(btree.get(0, 1).unwrap(), vec![b"kept".to_vec()]);
        assert!(btree.get(0, 2).is_err());
    }

    #[test]
    fn abort_across_splits_keeps_other_transactions_keys() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.begin_transaction(1).unwrap();
        btree.begin_transaction(2).unwrap();
        for i in 0..2000 {
            let txn = if i % 2 == 0 { 1 } else { 2 };
            btree
                .insert(txn, i, vec![i.to_le_bytes().to_vec()])
                .unwrap();
        }
        btree.commit_transaction(1).unwrap();
        btree.abort_transaction(2).unwrap();

        let remaining = keys(btree.scan(0));
        let expected: Vec<Key> = (0..2000).filter(|i| i % 2 == 0).collect();
        assert_eq!(remaining, expected);
    }

    #[test]
    fn aborted_transaction_stays_aborted_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            btree.begin_transaction(1).unwrap();
            btree.insert(1, 10, vec![b"committed".to_vec()]).unwrap();
            btree.commit_transaction(1).unwrap();

            btree.begin_transaction(2).unwrap();
            btree.insert(2, 20, vec![b"aborted".to_vec()]).unwrap();
            btree.delete(2, 10).unwrap();
            btree.abort_transaction(2).unwrap();
            btree.pager.wal.flush_all().unwrap();
        }
        {
            let btree = BplusTree::new(index_path, heap_path).unwrap();
            btree.recover().unwrap();
            assert_eq!(btree.get(0, 10).unwrap(), vec![b"committed".to_vec()]);
            assert!(btree.get(0, 20).is_err());
        }
    }

    #[test]
    fn autocommit_delete_releases_heap_pages_for_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for round in 0..50u64 {
            btree
                .insert(0, 1, vec![round.to_le_bytes().to_vec()])
                .unwrap();
            btree.delete(0, 1).unwrap();
        }
        assert!(btree.pager.heap.next_id.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn transactional_delete_frees_pages_only_on_commit() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        btree.insert(0, 1, vec![vec![b'a'; 5000]]).unwrap();
        btree.insert(0, 2, vec![vec![b'b'; 5000]]).unwrap();

        btree.begin_transaction(3).unwrap();
        btree.delete(3, 1).unwrap();
        assert_eq!(btree.pager.heap.free_page_count(), 0);
        btree.abort_transaction(3).unwrap();
        assert_eq!(btree.get(0, 1).unwrap(), vec![vec![b'a'; 5000]]);

        btree.begin_transaction(4).unwrap();
        btree.delete(4, 2).unwrap();
        btree.commit_transaction(4).unwrap();
        assert!(btree.pager.heap.free_page_count() >= 1);
    }

    #[test]
    fn reused_heap_page_survives_crash_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            btree
                .insert(0, 1, vec![vec![0xAA; 3000], vec![0xBB; 3000]])
                .unwrap();
            btree.pager.flush_all().unwrap();
            btree.delete(0, 1).unwrap();

            btree.begin_transaction(9).unwrap();
            btree.insert(9, 2, vec![b"tiny".to_vec()]).unwrap();
            btree.commit_transaction(9).unwrap();
            btree.pager.wal.flush_all().unwrap();
        }
        {
            let btree = BplusTree::new(index_path, heap_path).unwrap();
            btree.recover().unwrap();
            assert_eq!(btree.get(0, 2).unwrap(), vec![b"tiny".to_vec()]);
            assert!(btree.get(0, 1).is_err());
        }
    }

    #[test]
    fn records_spanning_multiple_heap_pages() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        let records: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 5000]).collect();
        btree.insert(0, 1, records.clone()).unwrap();
        assert_eq!(btree.get(0, 1).unwrap(), records);

        btree.delete(0, 1).unwrap();
        assert!(btree.pager.heap.free_page_count() >= 3);
    }

    #[test]
    fn oversized_record_is_rejected_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        let result = btree.insert(0, 1, vec![vec![0u8; MAX_RECORD_BYTES + 1]]);
        assert!(result.is_err());
        assert!(btree.get(0, 1).is_err());
        assert_eq!(btree.pager.heap.page_count(), 0);
        btree
            .insert(0, 1, vec![vec![7u8; MAX_RECORD_BYTES]])
            .unwrap();
        assert_eq!(btree.get(0, 1).unwrap()[0].len(), MAX_RECORD_BYTES);
    }

    #[test]
    fn large_row_round_trips_and_frees_its_chain() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        let big: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        btree
            .insert(0, 1, vec![big.clone(), b"tail".to_vec()])
            .unwrap();
        assert_eq!(btree.get(0, 1).unwrap(), vec![big, b"tail".to_vec()]);
        let pages = btree.pager.heap.page_count();
        assert!(pages >= 25);
        btree.delete(0, 1).unwrap();
        assert_eq!(btree.pager.heap.free_page_count() as u64, pages);
    }

    #[test]
    fn small_rows_share_heap_pages() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for i in 0..10_000 {
            btree.insert(0, i, vec![i.to_le_bytes().to_vec()]).unwrap();
        }
        assert!(btree.pager.heap.page_count() < 100);
        for i in (0..10_000).step_by(997) {
            assert_eq!(btree.get(0, i).unwrap(), vec![i.to_le_bytes().to_vec()]);
        }
    }

    #[test]
    fn deleted_space_is_reused_by_later_inserts() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for i in 0..2000 {
            btree.insert(0, i, vec![vec![1u8; 100]]).unwrap();
        }
        let pages = btree.pager.heap.page_count();
        for round in 0..5 {
            for i in 0..2000 {
                btree.delete(0, i).unwrap();
            }
            for i in 0..2000 {
                btree.insert(0, i, vec![vec![round as u8; 100]]).unwrap();
            }
        }
        assert!(btree.pager.heap.page_count() <= pages + 1);
        assert_eq!(btree.get(0, 1999).unwrap(), vec![vec![4u8; 100]]);
    }

    #[test]
    fn abort_restores_rows_on_pages_shared_with_committed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for i in 1..=3 {
            btree.insert(0, i, vec![vec![i as u8; 40]]).unwrap();
        }
        assert_eq!(btree.pager.heap.page_count(), 1);
        btree.begin_transaction(5).unwrap();
        btree.delete(5, 2).unwrap();
        btree.insert(5, 4, vec![vec![4u8; 40]]).unwrap();
        btree.insert(5, 5, vec![vec![5u8; 40]]).unwrap();
        btree.abort_transaction(5).unwrap();
        for i in 1..=3 {
            assert_eq!(btree.get(0, i).unwrap(), vec![vec![i as u8; 40]]);
        }
        assert!(btree.get(0, 4).is_err());
        assert!(btree.get(0, 5).is_err());
    }

    #[test]
    fn shared_heap_pages_survive_crash_and_space_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            for i in 0..600 {
                btree.insert(0, i, vec![vec![(i % 256) as u8; 50]]).unwrap();
            }
            btree.begin_transaction(1).unwrap();
            for i in 0..300 {
                btree.delete(1, i).unwrap();
            }
            btree.commit_transaction(1).unwrap();
            btree.begin_transaction(2).unwrap();
            for i in 300..400 {
                btree.delete(2, i).unwrap();
            }
            btree.insert(2, 10_000, vec![vec![9u8; 50]]).unwrap();
            btree.pager.wal.flush_all().unwrap();
            std::mem::forget(btree);
        }
        let btree = BplusTree::new(index_path, heap_path).unwrap();
        btree.recover().unwrap();
        assert_eq!(keys(btree.scan(0)), (300..600).collect::<Vec<_>>());
        for i in 300..600 {
            assert_eq!(btree.get(0, i).unwrap(), vec![vec![(i % 256) as u8; 50]]);
        }
        let pages = btree.pager.heap.page_count();
        assert!(btree.pager.heap.free_page_count() > 0);
        for i in 0..300 {
            btree.insert(0, i, vec![vec![1u8; 50]]).unwrap();
        }
        assert_eq!(btree.pager.heap.page_count(), pages);
    }

    #[test]
    fn range_scan_bounds_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        assert!(keys(btree.scan(0)).is_empty());

        for i in (0..3000).rev() {
            btree
                .insert(0, i * 2, vec![i.to_le_bytes().to_vec()])
                .unwrap();
        }

        let all = keys(btree.scan(0));
        assert_eq!(all, (0..3000).map(|i| i * 2).collect::<Vec<_>>());

        assert_eq!(
            keys(btree.range(0, Bound::Included(10), Bound::Excluded(20))),
            vec![10, 12, 14, 16, 18]
        );
        assert_eq!(
            keys(btree.range(0, Bound::Excluded(10), Bound::Included(20))),
            vec![12, 14, 16, 18, 20]
        );
        assert_eq!(
            keys(btree.range(0, Bound::Included(11), Bound::Included(11))),
            Vec::<Key>::new()
        );
        assert_eq!(
            keys(btree.range(0, Bound::Excluded(5990), Bound::Unbounded)),
            vec![5992, 5994, 5996, 5998]
        );

        let (first_key, first_records) = btree.scan(0).next().unwrap().unwrap();
        assert_eq!(first_key, 0);
        assert_eq!(first_records, vec![0u128.to_le_bytes().to_vec()]);
    }

    #[test]
    fn looping_leaf_chain_is_reported_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for i in 0..1000 {
            btree.insert(0, i, vec![vec![1]]).unwrap();
        }
        let next = |id: u64| btree.pager.index.fetch(id).unwrap().header().unwrap().ptr;
        let keys_of = |id: u64| -> Vec<Key> {
            let page = btree.pager.index.fetch(id).unwrap();
            page.get_cells().unwrap().iter().map(|c| c.key).collect()
        };
        let first = *btree.breadcrumbs(0).unwrap().breadcrumb.last().unwrap();
        let second = next(first);
        let third = next(second);
        assert!(second != NO_SIBLING && third != NO_SIBLING);
        btree
            .pager
            .index
            .fetch_mut(third)
            .unwrap()
            .set_header_ptr(second)
            .unwrap();
        let collect = |lower: Bound<Key>| {
            btree
                .range(0, lower, Bound::Unbounded)
                .collect::<Result<Vec<_>, _>>()
                .map(|rows| rows.len())
                .unwrap_err()
                .to_string()
        };
        let err = collect(Bound::Unbounded);
        assert!(err.contains("out of order"), "{err}");
        let past_third = *keys_of(third).last().unwrap();
        let err = collect(Bound::Excluded(past_third));
        assert!(err.contains("loops"), "{err}");
    }

    #[test]
    fn range_scan_after_merges() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        for i in 0..2000 {
            btree.insert(0, i, vec![vec![1]]).unwrap();
        }
        for i in (0..2000).filter(|i| i % 3 != 0) {
            btree.delete(0, i).unwrap();
        }
        let expected: Vec<Key> = (0..2000).filter(|i| i % 3 == 0).collect();
        assert_eq!(keys(btree.scan(0)), expected);
    }

    #[test]
    fn trees_in_one_directory_use_separate_wal_files() {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            BplusTree::new(
                dir.path().join(format!("{name}.idx")),
                dir.path().join(format!("{name}.heap")),
            )
            .unwrap()
        };
        {
            let a = open("a");
            let b = open("b");
            a.begin_transaction(1).unwrap();
            a.insert(1, 1, vec![b"from a".to_vec()]).unwrap();
            a.commit_transaction(1).unwrap();
            b.begin_transaction(1).unwrap();
            b.insert(1, 1, vec![b"from b".to_vec()]).unwrap();
            b.commit_transaction(1).unwrap();
        }
        assert!(dir.path().join("a.wal").exists());
        assert!(dir.path().join("b.wal").exists());
        let a = open("a");
        a.recover().unwrap();
        let b = open("b");
        b.recover().unwrap();
        assert_eq!(a.get(0, 1).unwrap(), vec![b"from a".to_vec()]);
        assert_eq!(b.get(0, 1).unwrap(), vec![b"from b".to_vec()]);
    }
    fn root_pages(btree: &BplusTree) -> Vec<u64> {
        let next = btree.pager.index.next_id.load(Ordering::SeqCst);
        (0..next)
            .filter(|&id| {
                btree
                    .pager
                    .index
                    .fetch(id)
                    .unwrap()
                    .header()
                    .is_ok_and(|h| h.page_ty == PageKind::Root)
            })
            .collect()
    }

    #[test]
    fn root_collapse_leaves_exactly_one_root_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            for round in 0..3 {
                for i in 0..3000 {
                    btree.insert(0, round * 100_000 + i, vec![vec![1]]).unwrap();
                }
                for i in 0..2990 {
                    btree.delete(0, round * 100_000 + i).unwrap();
                }
                assert_eq!(root_pages(&btree).len(), 1, "round {round}");
            }
            btree.flush().unwrap();
        }
        let btree = BplusTree::new(index_path, heap_path).unwrap();
        btree.recover().unwrap();
        let expected: Vec<Key> = (0..3)
            .flat_map(|round| (2990..3000).map(move |i| round * 100_000 + i))
            .collect();
        assert_eq!(keys(btree.scan(0)), expected);
        for key in expected {
            assert_eq!(btree.get(0, key).unwrap(), vec![vec![1]]);
        }
    }
    #[test]
    fn fresh_index_pages_survive_crash_before_flush() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            btree.begin_transaction(1).unwrap();
            for i in 0..3000 {
                btree.insert(1, i, vec![i.to_le_bytes().to_vec()]).unwrap();
            }
            btree.commit_transaction(1).unwrap();
            std::mem::forget(btree);
        }
        let btree = BplusTree::new(index_path, heap_path).unwrap();
        btree.recover().unwrap();
        let next = btree.pager.index.next_id.load(Ordering::SeqCst);
        assert!(next > 3, "expected splits");
        for id in 0..next {
            let hdr = btree.pager.index.fetch(id).unwrap().header().unwrap();
            assert_eq!(hdr.id, id, "header id of page {id}");
        }
        assert_eq!(keys(btree.scan(0)), (0..3000).collect::<Vec<_>>());
    }
    #[test]
    fn wal_truncation_keeps_recovery_working() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");
        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            btree.begin_transaction(1).unwrap();
            for i in 0..500 {
                btree.insert(1, i, vec![vec![1]]).unwrap();
            }
            assert!(btree.checkpoint_and_truncate().is_err());
            btree.commit_transaction(1).unwrap();

            let before = btree.wal_size_bytes().unwrap();
            btree.checkpoint_and_truncate().unwrap();
            let after = btree.wal_size_bytes().unwrap();
            assert!(after < before);
            assert_eq!(after, header::WAL_HEADER_SIZE as u64);

            btree.begin_transaction(2).unwrap();
            for i in 500..800 {
                btree.insert(2, i, vec![vec![2]]).unwrap();
            }
            btree.delete(2, 0).unwrap();
            btree.commit_transaction(2).unwrap();
            btree.begin_transaction(3).unwrap();
            btree.insert(3, 10_000, vec![vec![3]]).unwrap();
            btree.pager.wal.flush_all().unwrap();
            std::mem::forget(btree);
        }
        let btree = BplusTree::new(index_path, heap_path).unwrap();
        btree.recover().unwrap();
        assert_eq!(keys(btree.scan(0)), (1..800).collect::<Vec<_>>());
    }
    #[test]
    fn rollback_to_savepoint_undoes_only_later_work() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        btree.begin_transaction(1).unwrap();
        for i in 0..1000 {
            btree.insert(1, i, vec![vec![1]]).unwrap();
        }
        let savepoint = btree.savepoint(1);
        for i in 1000..3000 {
            btree.insert(1, i, vec![vec![2]]).unwrap();
        }
        for i in 0..500 {
            btree.delete(1, i).unwrap();
        }
        btree.rollback_to(1, savepoint).unwrap();
        assert_eq!(keys(btree.scan(0)), (0..1000).collect::<Vec<_>>());

        btree.delete(1, 999).unwrap();
        btree.commit_transaction(1).unwrap();
        assert_eq!(keys(btree.scan(0)), (0..999).collect::<Vec<_>>());
    }

    #[test]
    fn rollback_to_then_abort_undoes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        btree.insert(0, 7, vec![b"kept".to_vec()]).unwrap();
        btree.begin_transaction(2).unwrap();
        btree.insert(2, 1, vec![vec![1]]).unwrap();
        let savepoint = btree.savepoint(2);
        btree.delete(2, 7).unwrap();
        btree.rollback_to(2, savepoint).unwrap();
        btree.insert(2, 2, vec![vec![2]]).unwrap();
        btree.abort_transaction(2).unwrap();
        assert_eq!(keys(btree.scan(0)), vec![7]);
        assert_eq!(btree.get(0, 7).unwrap(), vec![b"kept".to_vec()]);
    }

    #[test]
    fn rewriting_one_small_row_logs_only_the_changed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());
        btree.begin_transaction(1).unwrap();
        for i in 0..200 {
            btree.insert(1, i, vec![vec![i as u8; 40]]).unwrap();
        }
        btree.commit_transaction(1).unwrap();
        btree.checkpoint_and_truncate().unwrap();
        let mut grew = Vec::new();
        for txn in 2..6 {
            let before = btree.wal_size_bytes().unwrap();
            btree.begin_transaction(txn).unwrap();
            btree.delete(txn, 50).unwrap();
            btree.insert(txn, 50, vec![vec![txn as u8; 40]]).unwrap();
            btree.commit_transaction(txn).unwrap();
            grew.push(btree.wal_size_bytes().unwrap() - before);
        }
        assert!(grew[0] > grew[3], "first touch logs page images: {grew:?}");
        assert!(grew[3] < 1536, "steady-state rewrite of one row: {grew:?}");
        assert_eq!(grew[1], grew[3], "steady state is stable: {grew:?}");
        assert_eq!(btree.get(0, 50).unwrap(), vec![vec![5u8; 40]]);
        assert_eq!(keys(btree.scan(0)), (0..200).collect::<Vec<_>>());
    }
}
