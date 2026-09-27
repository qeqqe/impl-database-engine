#![allow(dead_code)]

use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{error::Error, path::PathBuf};

use header::PageKind;
use pager::Pager;
use slotted_page::Cell;

pub(super) mod buffer_pool;
pub(super) mod header;
pub(super) mod heap;
pub(super) mod index;
pub(super) mod pager;
pub(super) mod slotted_page;
pub(super) mod wal;
pub(super) mod wal_buffer;

pub(super) const PAGE_SIZE: usize = 4096;
pub(super) const HEADER_SIZE: usize = 8 + 2 + 2 + 1 + 1 + 8;
pub(super) const KEY_SIZE: usize = 8;
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
const DEGREE: usize = FANOUT / 2;

pub struct BplusTree {
    root_id: AtomicU64,
    pager: Pager,
    tree_lock: RwLock<()>,
}

impl BplusTree {
    pub fn new(index_path: PathBuf, heap_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let wal_path = index_path.with_file_name("wal_file.db");
        Self::new_with_wal(index_path, heap_path, wal_path)
    }

    pub fn new_with_wal(
        index_path: PathBuf,
        heap_path: PathBuf,
        wal_path: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let pager = Pager::new_with_wal(index_path, heap_path, wal_path)?;
        let next_id = pager.index.next_id.load(Ordering::SeqCst);
        let root_id = if next_id > 0 {
            let mut found_root = 0;
            for id in 0..next_id {
                if let Ok(hdr) = pager.index.fetch(id).header()
                    && hdr.page_ty == PageKind::Root
                {
                    found_root = id;
                    break;
                }
            }
            found_root
        } else {
            let id = pager.index.allocate(PageKind::Root);
            let mut root_page = pager.index.fetch_mut(id);
            let mut hdr = root_page.header()?;
            hdr.set_root_leaf();
            hdr.serialize(&mut root_page.data[..HEADER_SIZE]);
            let new_data = root_page.data;
            drop(root_page);
            pager.log_index_diff(0, id, &[0u8; PAGE_SIZE], &new_data)?;
            id
        };

        Ok(Self {
            root_id: AtomicU64::new(root_id),
            pager,
            tree_lock: RwLock::new(()),
        })
    }

    pub fn begin_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.pager.wal.begin_transaction(txn_id)
    }

    pub fn commit_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.pager.wal.commit_transaction(txn_id)
    }

    pub fn abort_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.pager.wal.abort_transaction(txn_id)
    }

    pub fn fuzzy_checkpoint(&self) -> Result<u64, Box<dyn Error>> {
        let _guard = self.tree_lock.read().unwrap();
        self.pager.fuzzy_checkpoint()
    }

    pub fn recover(&self) -> Result<pager::RecoveryReport, Box<dyn Error>> {
        let _guard = self.tree_lock.write().unwrap();
        let report = self.pager.recover()?;
        let next_id = self.pager.index.next_id.load(Ordering::SeqCst);
        for id in 0..next_id {
            if let Ok(hdr) = self.pager.index.fetch(id).header()
                && hdr.page_ty == PageKind::Root
            {
                self.root_id.store(id, Ordering::Release);
                break;
            }
        }
        Ok(report)
    }

    /// Traverses the tree, finds the cell content in page
    /// i.e. the offset & size of the actual data in the heap file
    /// returns a &Vec<u8> of the data, the callers can transmute it.
    pub fn get(&self, txn_id: u64, key: u64) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
        let _guard = self.tree_lock.read().unwrap();
        let _ = txn_id;
        let mut page_id = self.root_id.load(Ordering::Acquire);
        loop {
            let (cells, p_hdr) = {
                let page = self.pager.index.fetch(page_id);
                let p_hdr = page.header()?;
                (page.get_cells()?, p_hdr)
            };

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
                        self.pager.heap.get_record(heap_id, &mut data_records)?;
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
    }

    pub fn insert(
        &self,
        txn_id: u64,
        key: u64,
        data_records: Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        let _guard = self.tree_lock.write().unwrap();
        let breadcrumbs = self.breadcrumbs(key)?;

        if breadcrumbs.found {
            return Err("Key already exists".into());
        }

        let mut breadcrumbs = breadcrumbs.breadcrumb;

        let Some(index_page_id) = breadcrumbs.pop() else {
            return Err("No page found".into());
        };

        // we first have to insert the data record into the heap file then we
        // can have the offset of the data record that we can add to the cell's of
        // index file current page
        let heap_page_id = self.pager.heap.allocate();
        {
            let mut heap_page = self.pager.heap.fetch_mut(heap_page_id)?;
            let old_data = heap_page.data;
            heap_page.add_records(data_records)?;
            let new_data = heap_page.data;
            drop(heap_page);
            self.pager
                .log_heap_diff(txn_id, heap_page_id, &old_data, &new_data)?;
        }

        // now we can store the cell in the index page itself.
        let n_slot = {
            let mut page = self.pager.index.fetch_mut(index_page_id);
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
    }

    pub fn insert_with_txn(
        &self,
        txn_id: u64,
        key: u64,
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
            let page = self.pager.index.fetch(overflow_page_id);
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
        let right_page_id = self.pager.index.allocate(right_page_kind);

        if is_leaf_level {
            // *On leaf level, the right cell's first element is the same as the promoted key
            let right_cells_with_promoted: Vec<_> = std::iter::once(promote_cell)
                .chain(right_cells.iter())
                .collect();

            let old_data = self.pager.index.fetch(overflow_page_id).data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id);
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

            let old_data = self.pager.index.fetch(right_page_id).data;
            let new_data = {
                let mut right_page = self.pager.index.fetch_mut(right_page_id);
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

            let old_data = self.pager.index.fetch(overflow_page_id).data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id);
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

            let old_data = self.pager.index.fetch(right_page_id).data;
            let new_data = {
                let mut right_page = self.pager.index.fetch_mut(right_page_id);
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
            let old_data = self.pager.index.fetch(parent_id).data;
            let (n_slot, new_data) = {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);
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
            let new_root_id = self.pager.index.allocate(PageKind::Root);
            let old_data = self.pager.index.fetch(new_root_id).data;
            let new_data = {
                let mut root_page = self.pager.index.fetch_mut(new_root_id);
                root_page.init_header(new_root_id, PageKind::Root, right_page_id);
                root_page.add_cell(promote_cell.key, overflow_page_id)?;
                root_page.data
            };
            self.pager
                .log_index_diff(txn_id, new_root_id, &old_data, &new_data)?;

            let old_data = self.pager.index.fetch(overflow_page_id).data;
            let new_data = {
                let mut left_page = self.pager.index.fetch_mut(overflow_page_id);
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

    pub fn delete(&self, txn_id: u64, key: u64) -> Result<(), Box<dyn Error>> {
        let _guard = self.tree_lock.write().unwrap();
        let crumbs = self.breadcrumbs(key)?;

        if !crumbs.found {
            return Err("Key not found".into());
        }

        let mut trail = crumbs.breadcrumb;

        let Some(leaf_id) = trail.pop() else {
            return Err("Empty breadcrumb trail".into());
        };

        let old_leaf_data = self.pager.index.fetch(leaf_id).data;
        let remaining = {
            let mut leaf_page = self.pager.index.fetch_mut(leaf_id);
            let cells = leaf_page.get_cells()?;
            let idx = cells
                .binary_search_by(|c| c.key.cmp(&key))
                .map_err(|_| "Key not found in leaf page")?;

            leaf_page.remove_cell_at(idx)?;
            leaf_page.num_cells()?
        };
        let new_leaf_data = self.pager.index.fetch(leaf_id).data;
        self.pager
            .log_index_diff(txn_id, leaf_id, &old_leaf_data, &new_leaf_data)?;

        if leaf_id == self.root_id.load(Ordering::Acquire) {
            return Ok(());
        }

        // if there's no underfull just propogate the key up the tree
        if remaining >= DEGREE {
            self.propagate_key_update(txn_id, key, leaf_id, &trail)?;
            return Ok(());
        }

        self.handle_underfull(txn_id, leaf_id, &mut trail)?;

        Ok(())
    }

    pub fn delete_with_txn(&self, txn_id: u64, key: u64) -> Result<(), Box<dyn Error>> {
        self.delete(txn_id, key)
    }

    fn propagate_key_update(
        &self,
        txn_id: u64,
        old_key: u64,
        child_id: u64,
        ancestors: &[u64],
    ) -> Result<(), Box<dyn Error>> {
        // we know that the left most cell's key of a leaf node will always
        // be the same as the parent node's key pointing to it. so if we find
        // any of the occurence of deleted key while propogating up the tree and we can
        // replace it with `new_first_key`
        let new_first_key = {
            let leaf_page = self.pager.index.fetch(child_id);
            let leaf_cells = leaf_page.get_cells()?;
            match leaf_cells.first() {
                Some(cell) => cell.key,
                None => return Ok(()),
            }
        };

        for &ancestor_id in ancestors.iter().rev() {
            let found_idx = {
                let page = self.pager.index.fetch(ancestor_id);
                let cells = page.get_cells()?;
                cells.binary_search_by(|c| c.key.cmp(&old_key)).ok()
            };

            if let Some(idx) = found_idx {
                let old_data = self.pager.index.fetch(ancestor_id).data;
                let new_data = {
                    let mut page = self.pager.index.fetch_mut(ancestor_id);
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
            let parent_page = self.pager.index.fetch(parent_id);
            let hdr = parent_page.header()?;
            let cells = parent_page.get_cells()?;
            (hdr, cells)
        };

        let sibling = Self::find_siblings(underfull_id, &parent_cells, &parent_hdr)?;

        let child_idx = sibling.child_idx;
        let left_sibling_id = sibling.left;
        let right_sibling_id = sibling.right;

        let underfull_hdr = {
            let page = self.pager.index.fetch(underfull_id);
            page.header()?
        };

        if let Some(left_id) = left_sibling_id {
            let left_cells = {
                let page = self.pager.index.fetch(left_id);
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
                let page = self.pager.index.fetch(right_id);
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

        let old_left_data = self.pager.index.fetch(left_id).data;
        let (borrowed_cell, separator_idx) = {
            let mut left_page = self.pager.index.fetch_mut(left_id);
            let left_cells = left_page.get_cells()?;
            let last_idx = left_cells.len() - 1;
            let cell = left_page.remove_cell_at(last_idx)?;
            (cell, child_idx - 1)
        };

        if is_leaf {
            let new_left_data = self.pager.index.fetch(left_id).data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);
                let value = borrowed_cell.h_ptr.as_ref().unwrap().index;
                underfull_page.add_cell(borrowed_cell.key, value)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let new_separator = {
                let underfull_page = self.pager.index.fetch(underfull_id);
                let cells = underfull_page.get_cells()?;
                cells
                    .first()
                    .ok_or("Empty underfull after redistribute")?
                    .key
            };

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(new_separator, left_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id);
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let left_old_rightmost = {
                let left_page = self.pager.index.fetch(left_id);
                left_page.header()?.ptr
            };

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);

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
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(borrowed_cell.key, left_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;

            {
                let mut left_page = self.pager.index.fetch_mut(left_id);
                if let Some(c) = borrowed_cell.c_ptr {
                    left_page.set_header_ptr(c)?;
                }
            }
            let new_left_data = self.pager.index.fetch(left_id).data;
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

        let old_right_data = self.pager.index.fetch(right_id).data;
        let borrowed_cell = {
            let mut right_page = self.pager.index.fetch_mut(right_id);
            right_page.remove_cell_at(0)?
        };

        let separator_idx = child_idx;

        if is_leaf {
            let new_right_data = self.pager.index.fetch(right_id).data;
            self.pager
                .log_index_diff(txn_id, right_id, &old_right_data, &new_right_data)?;

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);
                let value = borrowed_cell.h_ptr.as_ref().unwrap().index;
                underfull_page.add_cell(borrowed_cell.key, value)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let new_separator = {
                let right_page = self.pager.index.fetch(right_id);
                let cells = right_page.get_cells()?;
                cells.first().ok_or("Empty right after redistribute")?.key
            };

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(new_separator, underfull_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let new_right_data = self.pager.index.fetch(right_id).data;
            self.pager
                .log_index_diff(txn_id, right_id, &old_right_data, &new_right_data)?;

            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id);
                let parent_cells = parent_page.get_cells()?;
                parent_cells[separator_idx].key
            };

            let borrowed_left_child = borrowed_cell.c_ptr.ok_or("Missing c_ptr")?;

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);
                let old_ptr = underfull_page.header()?.ptr;
                underfull_page.add_cell(parent_sep_key, old_ptr)?;
                underfull_page.set_header_ptr(borrowed_left_child)?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);
                parent_page.remove_cell_at(separator_idx)?;
                parent_page.add_cell(borrowed_cell.key, underfull_id)?;
            }
            let new_parent_data = self.pager.index.fetch(parent_id).data;
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
            let page = self.pager.index.fetch(underfull_id);
            page.header()?.page_ty == PageKind::Leaf
        };

        let separator_idx = child_idx - 1;

        let (left_cells, left_hdr) = {
            let page = self.pager.index.fetch(left_id);
            (page.get_cells()?, page.header()?)
        };
        let (underfull_cells, underfull_hdr) = {
            let page = self.pager.index.fetch(underfull_id);
            (page.get_cells()?, page.header()?)
        };

        if is_leaf {
            let mut merged: Vec<slotted_page::Cell> =
                Vec::with_capacity(left_cells.len() + underfull_cells.len());
            merged.extend(left_cells);
            merged.extend(underfull_cells);

            let old_left_data = self.pager.index.fetch(left_id).data;
            {
                let mut left_page = self.pager.index.fetch_mut(left_id);
                left_page.rebuild_from_cells(
                    &merged,
                    PageKind::Leaf,
                    left_id,
                    underfull_hdr.ptr,
                )?;
            }
            let new_left_data = self.pager.index.fetch(left_id).data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);

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
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id);
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

            let old_left_data = self.pager.index.fetch(left_id).data;
            {
                let mut left_page = self.pager.index.fetch_mut(left_id);
                left_page.rebuild_from_cells(
                    &merged,
                    PageKind::Internal,
                    left_id,
                    underfull_hdr.ptr,
                )?;
            }
            let new_left_data = self.pager.index.fetch(left_id).data;
            self.pager
                .log_index_diff(txn_id, left_id, &old_left_data, &new_left_data)?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);

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
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        }

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
            let page = self.pager.index.fetch(underfull_id);
            page.header()?.page_ty == PageKind::Leaf
        };

        let separator_idx = child_idx;

        let (underfull_cells, underfull_hdr) = {
            let page = self.pager.index.fetch(underfull_id);
            (page.get_cells()?, page.header()?)
        };
        let (right_cells, right_hdr) = {
            let page = self.pager.index.fetch(right_id);
            (page.get_cells()?, page.header()?)
        };

        if is_leaf {
            let mut merged: Vec<slotted_page::Cell> =
                Vec::with_capacity(underfull_cells.len() + right_cells.len());
            merged.extend(underfull_cells);
            merged.extend(right_cells);

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);
                underfull_page.rebuild_from_cells(
                    &merged,
                    PageKind::Leaf,
                    underfull_id,
                    right_hdr.ptr,
                )?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);

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
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        } else {
            let parent_sep_key = {
                let parent_page = self.pager.index.fetch(parent_id);
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

            let old_underfull_data = self.pager.index.fetch(underfull_id).data;
            {
                let mut underfull_page = self.pager.index.fetch_mut(underfull_id);
                underfull_page.rebuild_from_cells(
                    &merged,
                    PageKind::Internal,
                    underfull_id,
                    right_hdr.ptr,
                )?;
            }
            let new_underfull_data = self.pager.index.fetch(underfull_id).data;
            self.pager.log_index_diff(
                txn_id,
                underfull_id,
                &old_underfull_data,
                &new_underfull_data,
            )?;

            let old_parent_data = self.pager.index.fetch(parent_id).data;
            {
                let mut parent_page = self.pager.index.fetch_mut(parent_id);

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
            let new_parent_data = self.pager.index.fetch(parent_id).data;
            self.pager
                .log_index_diff(txn_id, parent_id, &old_parent_data, &new_parent_data)?;
        }

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
                let root_page = self.pager.index.fetch(parent_id);
                let root_cells = root_page.get_cells()?;
                let root_hdr = root_page.header()?;
                (root_cells.is_empty(), root_hdr.ptr)
            };

            if root_cells_empty {
                let old_root_data = self.pager.index.fetch(new_root_id).data;
                {
                    let mut new_root = self.pager.index.fetch_mut(new_root_id);
                    new_root.set_page_kind(PageKind::Root)?;
                }
                let new_root_data = self.pager.index.fetch(new_root_id).data;
                self.pager
                    .log_index_diff(txn_id, new_root_id, &old_root_data, &new_root_data)?;

                self.root_id.store(new_root_id, Ordering::Release);
            }

            return Ok(());
        }

        let remaining = {
            let page = self.pager.index.fetch(parent_id);
            page.num_cells()?
        };

        if remaining < DEGREE {
            ancestors.pop();
            self.handle_underfull(txn_id, parent_id, ancestors)?;
        }

        Ok(())
    }

    fn breadcrumbs(&self, key: u64) -> Result<BreadCrumbs, Box<dyn Error>> {
        let mut breadcrumb: Vec<u64> = Vec::new();

        let mut page_id = self.root_id.load(Ordering::Acquire);

        loop {
            let (cells, p_hdr) = {
                let page = self.pager.index.fetch(page_id);
                let p_hdr = page.header()?;
                (page.get_cells()?, p_hdr)
            };

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
    fn single_insert_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"hello".to_vec()]).unwrap();

        let result = btree.get(0, 1u64).unwrap();
        assert_eq!(result, vec![b"hello".to_vec()]);
    }

    #[test]
    fn insert_and_get_multiple_keys_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 5u64, vec![b"five".to_vec()]).unwrap();
        btree.insert(0, 3u64, vec![b"three".to_vec()]).unwrap();
        btree.insert(0, 7u64, vec![b"seven".to_vec()]).unwrap();
        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 1u64).unwrap(), vec![b"one".to_vec()]);
        assert_eq!(btree.get(0, 3u64).unwrap(), vec![b"three".to_vec()]);
        assert_eq!(btree.get(0, 5u64).unwrap(), vec![b"five".to_vec()]);
        assert_eq!(btree.get(0, 7u64).unwrap(), vec![b"seven".to_vec()]);
    }

    #[test]
    fn get_nonexistent_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();

        let result = btree.get(0, 999u64);
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_insert_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();
        let result = btree.insert(0, 1u64, vec![b"one again".to_vec()]);
        assert!(result.is_err());
    }

    #[test]
    fn insert_and_get_multiple_data_records() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let records = vec![b"field1".to_vec(), b"field2".to_vec(), b"field3".to_vec()];
        btree.insert(0, 42u64, records.clone()).unwrap();

        let result = btree.get(0, 42u64).unwrap();
        assert_eq!(result, records);
    }

    #[test]
    fn delete_from_root_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();
        btree.insert(0, 2u64, vec![b"two".to_vec()]).unwrap();
        btree.insert(0, 3u64, vec![b"three".to_vec()]).unwrap();

        btree.delete(0, 2u64).unwrap();

        assert!(btree.get(0, 2u64).is_err());
        assert_eq!(btree.get(0, 1u64).unwrap(), vec![b"one".to_vec()]);
        assert_eq!(btree.get(0, 3u64).unwrap(), vec![b"three".to_vec()]);
    }

    #[test]
    fn delete_nonexistent_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();
        let result = btree.delete(0, 999u64);
        assert!(result.is_err());
    }

    #[test]
    fn delete_all_from_root() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();
        btree.insert(0, 2u64, vec![b"two".to_vec()]).unwrap();

        btree.delete(0, 1u64).unwrap();
        btree.delete(0, 2u64).unwrap();

        assert!(btree.get(0, 1u64).is_err());
        assert!(btree.get(0, 2u64).is_err());
    }

    #[test]
    fn insert_ascending_order() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in 1..=20u64 {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=20u64 {
            let result = btree.get(0, i).unwrap();
            assert_eq!(result, vec![format!("val-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn insert_descending_order() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in (1..=20u64).rev() {
            btree
                .insert(0, i, vec![format!("val-{}", i).into_bytes()])
                .unwrap();
        }

        for i in 1..=20u64 {
            let result = btree.get(0, i).unwrap();
            assert_eq!(result, vec![format!("val-{}", i).into_bytes()]);
        }
    }

    #[test]
    fn many_inserts_cause_splits() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let count = ORDER as u64 * 4;
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

        let count = ORDER as u64 * 3;
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

        let count = ORDER as u64 * 3;
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

        let count = ORDER as u64 * 2;
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

        let count = 300u64;
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

        let count = ORDER as u64 * 2;
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

        btree.insert(0, 1u64, vec![b"first".to_vec()]).unwrap();
        btree.delete(0, 1u64).unwrap();
        btree.insert(0, 1u64, vec![b"second".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 1u64).unwrap(), vec![b"second".to_vec()]);
    }

    #[test]
    fn empty_tree_get_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        assert!(btree.get(0, 1u64).is_err());
    }

    #[test]
    fn large_data_records() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        let big_data = vec![0xABu8; 4000];
        btree.insert(0, 1u64, vec![big_data.clone()]).unwrap();

        let result = btree.get(0, 1u64).unwrap();
        assert_eq!(result, vec![big_data]);
    }

    #[test]
    fn interleaved_insert_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let btree = get_btree_in(dir.path());

        for i in 1..=10u64 {
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

        btree.insert(0, 0u64, vec![b"zero".to_vec()]).unwrap();
        btree.insert(0, u64::MAX, vec![b"max".to_vec()]).unwrap();
        btree.insert(0, 1u64, vec![b"one".to_vec()]).unwrap();

        assert_eq!(btree.get(0, 0u64).unwrap(), vec![b"zero".to_vec()]);
        assert_eq!(btree.get(0, u64::MAX).unwrap(), vec![b"max".to_vec()]);
        assert_eq!(btree.get(0, 1u64).unwrap(), vec![b"one".to_vec()]);
    }

    #[test]
    fn buffer_pool_flush_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index_file.db");
        let heap_path = dir.path().join("heap_file.db");

        {
            let btree = BplusTree::new(index_path.clone(), heap_path.clone()).unwrap();
            for i in 1..=50u64 {
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
            };
            for i in 1..=50u64 {
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
        for i in 1..=30u64 {
            btree
                .insert(t1, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
        }
        btree.commit_transaction(t1).unwrap();

        let cp_lsn = btree.fuzzy_checkpoint().unwrap();
        assert!(cp_lsn > 0);

        let t2 = 2;
        btree.begin_transaction(t2).unwrap();
        for i in 31..=60u64 {
            btree
                .insert(t2, i, vec![format!("v{}", i).into_bytes()])
                .unwrap();
        }
        btree.commit_transaction(t2).unwrap();

        let t3 = 3;
        btree.begin_transaction(t3).unwrap();
        for i in 61..=80u64 {
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

        for i in 1..=60u64 {
            assert_eq!(
                btree.get(0, i).unwrap(),
                vec![format!("v{}", i).into_bytes()]
            );
        }
        for i in 61..=80u64 {
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

        let txn_user = 1001u64;
        let txn_order = 2002u64;

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

        for i in 1..=50u64 {
            btree
                .insert(0, i, vec![format!("pre-{}", i).into_bytes()])
                .unwrap();
        }

        let mut handles = Vec::new();

        for t in 0..4 {
            let tree = Arc::clone(&btree);
            handles.push(std::thread::spawn(move || {
                for i in 1..=50u64 {
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
                    tree.insert(txn_id, i, vec![format!("thread-{}", i).into_bytes()])
                        .unwrap();
                    tree.commit_transaction(txn_id).unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 1..=50u64 {
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
                    tree.insert(txn_id, i, vec![format!("item-{}", i).into_bytes()])
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
            let res = btree.get(0, i).unwrap();
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
                    tree.insert(txn_id, i, vec![format!("val-{}", i).into_bytes()])
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

        for i in 1..=100u64 {
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
                    tree.delete(txn_id, i).unwrap();
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

        for i in 1..=10u64 {
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
                    tree.insert(txn_id, i, vec![format!("committed-{}", i).into_bytes()])
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

        for i in 1..=10u64 {
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
        let idx_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&idx_path)
            .unwrap();

        let index = index::Index::new_with_capacity(idx_file, idx_path, Arc::clone(&wal), 2);

        let p0 = index.allocate(PageKind::Leaf);
        let _p1 = index.allocate(PageKind::Leaf);

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

        let _p2 = index.allocate(PageKind::Leaf);
        let _p3 = index.allocate(PageKind::Leaf);

        assert!(wal.flushed_lsn() >= lsn);
    }

    #[test]
    fn heap_steal_eviction_forces_wal_flush_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let heap_path = dir.path().join("test_heap.db");
        let wal_path = dir.path().join("test_wal.db");

        let wal = Arc::new(wal::Wal::new(wal_path).unwrap());
        let heap_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&heap_path)
            .unwrap();

        let heap = heap::Heap::new_with_capacity(heap_file, heap_path, Arc::clone(&wal), 2);

        let p0 = heap.allocate();
        let _p1 = heap.allocate();

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

        let _p2 = heap.allocate();
        let _p3 = heap.allocate();

        assert!(wal.flushed_lsn() >= lsn);
    }
}
