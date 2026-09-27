use std::error::Error;

use super::{
    HEADER_SIZE, KEY_SIZE, PAGE_SIZE, PTR_SIZE, SLOT_SIZE,
    header::{HeapHeader, IndexHeader, PageKind},
};

#[derive(Clone, Copy)]
pub(super) struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn get_cells(&self) -> Result<Vec<Cell>, Box<dyn Error>> {
        let p_hdr = self.header()?;
        let range = ((p_hdr.free_start as u32).saturating_sub(HEADER_SIZE as u32) / 4) as u16; // 4 bytes per ptr

        let is_leaf_level = p_hdr.page_ty == PageKind::Leaf
            || (p_hdr.page_ty == PageKind::Root && p_hdr.is_root_leaf());

        let mut cells = Vec::with_capacity(range as usize);
        if is_leaf_level {
            for i in 0..range {
                let slot = self.slot(i);
                let start = slot.cell_offset as usize;
                let key = u64::from_le_bytes(self.data[start..start + 8].try_into().unwrap());
                let index =
                    u64::from_le_bytes(self.data[start + 8..start + 16].try_into().unwrap());
                cells.push(Cell {
                    key,
                    c_ptr: None,
                    h_ptr: Some(HeapPointer { index }),
                });
            }
        } else {
            for i in 0..range {
                let slot = self.slot(i);
                let start = slot.cell_offset as usize;
                let key = u64::from_le_bytes(self.data[start..start + 8].try_into().unwrap());
                let c_ptr =
                    u64::from_le_bytes(self.data[start + 8..start + 16].try_into().unwrap());
                cells.push(Cell {
                    key,
                    c_ptr: Some(c_ptr),
                    h_ptr: None,
                });
            }
        }
        Ok(cells)
    }

    pub fn add_cell(&mut self, key: u64, h_ptr: u64) -> Result<usize, Box<dyn Error>> {
        let cell_size = KEY_SIZE + PTR_SIZE;
        let mut hdr = self.header()?;

        let c_ptr_offset = hdr.free_start;
        let c_offset = hdr.free_end;

        let end = c_offset as usize;
        let start = end
            .checked_sub(cell_size)
            // 4 bytes because we still haven't updated the cell pointer array
            // and this may lead to cell pointers and the cell collide when the cell pointer
            // for this cell is added in (free_start + 4).
            .filter(|&s| s >= c_ptr_offset as usize + 4)
            .ok_or("Page overflow")?; // this should and will NEVER trigger as we have already
        // calculated the max fanout and the split for overfull will be triggered before anything

        self.data[start..start + 8].copy_from_slice(&key.to_le_bytes());
        self.data[start + 8..end].clone_from_slice(&h_ptr.to_le_bytes());

        // now we can't just append the cell pointer at the free_start
        // we need to keep the cell pointer ITSELF to be sorted in the
        // cell pointers slot array. that'll require us to find the
        // insertion point in the cell pointers array and SHIFT every
        // cell pointer after the inserted cell pointer BY ONE.
        // This operation will cost O(N logN), where N is the order of the
        // Btree, log N is the binary search TC for searching insertion point,
        // and in worst case you would need to shift around N-1 (only for Leaf
        // nodes though as we know the first element will almost certainly never
        // be shifted as the key wouldn't even lie in the this node).

        let cell_ptr = CellPointer {
            cell_offset: start as u16,
            cell_size: cell_size as u16,
        };

        let n_slots = (c_ptr_offset - HEADER_SIZE as u16) / SLOT_SIZE as u16;
        let mut lo = 0u16;
        let mut hi = n_slots;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_slot = self.slot(mid);
            let mid_key = u64::from_le_bytes(
                self.data[mid_slot.cell_offset as usize..mid_slot.cell_offset as usize + KEY_SIZE]
                    .try_into()
                    .unwrap(),
            );

            match mid_key.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Err("duplicate key".into()),
            }
        }

        let insert_idx = lo as usize;
        let shift_from_start = insert_idx * 4 + HEADER_SIZE;

        let shift_from_end = HEADER_SIZE + n_slots as usize * 4;
        self.data
            .copy_within(shift_from_start..shift_from_end, shift_from_start + 4);

        self.data[shift_from_start..shift_from_start + 2]
            .copy_from_slice(&cell_ptr.cell_offset.to_le_bytes());
        self.data[shift_from_start + 2..shift_from_start + 4]
            .copy_from_slice(&cell_ptr.cell_size.to_le_bytes());

        hdr.free_start += 4;
        hdr.free_end = start as u16;
        hdr.serialize(&mut self.data[..HEADER_SIZE]);

        Ok(n_slots as usize + 1)
    }

    // pub fn handle_overflow_cell(&mut self) {} // index page cell will NEVER overflow

    pub fn header(&self) -> Result<IndexHeader, Box<dyn Error>> {
        IndexHeader::deserialize(&self.data[..HEADER_SIZE])
            .ok_or("Couldn't deserialize the index header".into())
    }

    pub fn num_cells(&self) -> Result<usize, Box<dyn Error>> {
        let header = self.header()?;
        Ok((header.free_start as usize - HEADER_SIZE) / SLOT_SIZE)
    }

    pub fn init_header(&mut self, id: u64, page_ty: PageKind, ptr: u64) {
        let hdr = IndexHeader {
            id,
            free_start: HEADER_SIZE as u16,
            free_end: PAGE_SIZE as u16,
            page_ty,
            flags: 0,
            ptr,
        };

        hdr.serialize(&mut self.data[..HEADER_SIZE]);
    }

    // NOTE: This can be deemed as a performace hiccup as we are rewriting the whole page
    // will add Tombstones for deleted a cell in furture
    pub fn rebuild_from_cells(
        &mut self,
        cells: &[Cell],
        page_ty: PageKind,
        id: u64,
        ptr: u64,
    ) -> Result<(), Box<dyn Error>> {
        let old_hdr = self.header().ok();
        let is_root_leaf = old_hdr
            .as_ref()
            .map(|h| h.page_ty == PageKind::Root && h.is_root_leaf())
            .unwrap_or(false);

        self.data = [0u8; PAGE_SIZE];
        self.init_header(id, page_ty, ptr);

        if is_root_leaf && page_ty == PageKind::Root {
            let mut hdr = self.header()?;
            hdr.set_root_leaf();
            hdr.serialize(&mut self.data[..HEADER_SIZE]);
        }

        let use_heap_ptr = page_ty == PageKind::Leaf || (page_ty == PageKind::Root && is_root_leaf);

        for cell in cells {
            let value = if use_heap_ptr {
                cell.h_ptr
                    .as_ref()
                    .ok_or("Leaf cell missing heap pointer")?
                    .index
            } else {
                cell.c_ptr.ok_or("Internal cell missing child pointer")?
            };
            self.add_cell(cell.key, value)?;
        }

        Ok(())
    }

    pub fn remove_cell_at(&mut self, idx: usize) -> Result<Cell, Box<dyn Error>> {
        let hdr = self.header()?;
        let all_cells = self.get_cells()?;

        if idx >= all_cells.len() {
            return Err("Cell index out of bounds".into());
        }

        let mut remaining: Vec<Cell> = Vec::with_capacity(all_cells.len() - 1);
        let mut removed: Option<Cell> = None;

        for (i, cell) in all_cells.into_iter().enumerate() {
            if i == idx {
                removed = Some(cell);
            } else {
                remaining.push(cell);
            }
        }

        let removed = removed.ok_or("Cell not found at index")?;

        self.rebuild_from_cells(&remaining, hdr.page_ty, hdr.id, hdr.ptr)?;

        Ok(removed)
    }

    pub fn set_header_ptr(&mut self, ptr: u64) -> Result<(), Box<dyn Error>> {
        let mut header = self.header()?;
        header.ptr = ptr;
        header.serialize(&mut self.data[..HEADER_SIZE]);
        Ok(())
    }

    pub fn set_page_kind(&mut self, page_ty: PageKind) -> Result<(), Box<dyn Error>> {
        let mut header = self.header()?;
        let was_leaf = header.page_ty == PageKind::Leaf
            || (header.page_ty == PageKind::Root && header.is_root_leaf());
        header.page_ty = page_ty;
        if page_ty == PageKind::Root && was_leaf {
            header.set_root_leaf();
        } else if page_ty != PageKind::Root {
            header.clear_root_leaf();
        }
        header.serialize(&mut self.data[..HEADER_SIZE]);
        Ok(())
    }

    /// Returns the CellPointer for index i in the page
    fn slot(&self, i: u16) -> CellPointer {
        let off = HEADER_SIZE + i as usize * SLOT_SIZE;

        CellPointer {
            cell_offset: u16::from_le_bytes(self.data[off..off + 2].try_into().unwrap()),
            cell_size: u16::from_le_bytes(self.data[off + 2..off + 4].try_into().unwrap()),
        }
    }
}

/// starting offset (cell_offset) → \[Cell\] ← ending offset, 8 bytes
pub(super) struct CellPointer {
    pub cell_offset: u16,
    pub cell_size: u16,
}

/// Cell thats stored in a page and contains the actual data,
/// grows from back in a page, referenced by the cell pointers
/// IF an internal/root node, it references a child page
/// else a leaf node and contains the page offset in the heap
/// organized file of the data record.
///
/// In the heap file this contains the actual data record (not _records_, by design
/// just meant to store one data member of the data record).
pub(super) struct Cell {
    /// Key seperator, usually a specified PK (if not, it's not this module's concern).
    /// type u64, not a B+tree's concern for ensuring the type
    /// abstraction above are responsible for ensuring a valid u64 key
    pub key: u64,
    pub c_ptr: Option<u64>, // None for PageKind::Leaf, else page offset. 8 bytes
    pub h_ptr: Option<HeapPointer>, // only PageKind::Leaf have this.
}

pub(super) struct HeapPointer {
    pub index: u64,
}

#[derive(Clone, Copy)]
pub(super) struct HeapPage {
    pub data: [u8; super::heap::PAGE_SIZE],
}

impl HeapPage {
    pub fn header(&self) -> Result<HeapHeader, Box<dyn Error>> {
        Ok(HeapHeader::deserialize(&self.data).ok_or("Couldn't deserialze the heap header")?)
    }

    pub fn add_records(&mut self, data_records: Vec<Vec<u8>>) -> Result<(), Box<dyn Error>> {
        for record in data_records {
            self.add_cell(record)?;
        }

        Ok(())
    }

    pub fn add_cell(&mut self, data: Vec<u8>) -> Result<(), Box<dyn Error>> {
        let d_len = data.len();
        let mut header = self.header()?;

        let cell_ptr_offset = header.free_start;
        let data_end_offset = header.free_end;

        let end = data_end_offset as usize;

        let start = end
            .checked_sub(d_len)
            .filter(|&s| s >= (cell_ptr_offset as usize + super::heap::SLOT_SIZE))
            .ok_or("Heap page overflow!")?;

        self.data[start..end].clone_from_slice(&data);

        let cell_ptr = CellPointer {
            cell_offset: start as u16,
            cell_size: d_len as u16,
        };
        let c_start = cell_ptr_offset as usize;
        self.data[c_start..c_start + 2].clone_from_slice(&cell_ptr.cell_offset.to_le_bytes());
        self.data[c_start + 2..c_start + 4].clone_from_slice(&cell_ptr.cell_size.to_le_bytes());

        header.free_start += super::heap::SLOT_SIZE as u16;
        header.free_end = start as u16;

        let heap_hdr_size = super::header::HEAP_HEADER_SIZE;
        let mut buf = [0u8; 21]; // HEAP_HEADER_SIZE = 21
        header.serialize(&mut buf);
        self.data[0..heap_hdr_size].clone_from_slice(&buf);
        Ok(())
    }
}
