use std::error::Error;

use super::{
    HEADER_SIZE, KEY_SIZE, Key, PAGE_SIZE, PTR_SIZE, SLOT_SIZE,
    header::{HeapHeader, IndexHeader, PageKind},
};

#[derive(Clone, Copy)]
pub(super) struct Page {
    pub data: [u8; PAGE_SIZE],
}

impl AsRef<[u8]> for Page {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
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
                let start = self.cell_start(i, &p_hdr)?;
                let key =
                    Key::from_le_bytes(self.data[start..start + KEY_SIZE].try_into().unwrap());
                let index = u64::from_le_bytes(
                    self.data[start + KEY_SIZE..start + KEY_SIZE + PTR_SIZE]
                        .try_into()
                        .unwrap(),
                );
                cells.push(Cell {
                    key,
                    c_ptr: None,
                    h_ptr: Some(HeapPointer { index }),
                });
            }
        } else {
            for i in 0..range {
                let start = self.cell_start(i, &p_hdr)?;
                let key =
                    Key::from_le_bytes(self.data[start..start + KEY_SIZE].try_into().unwrap());
                let c_ptr = u64::from_le_bytes(
                    self.data[start + KEY_SIZE..start + KEY_SIZE + PTR_SIZE]
                        .try_into()
                        .unwrap(),
                );
                cells.push(Cell {
                    key,
                    c_ptr: Some(c_ptr),
                    h_ptr: None,
                });
            }
        }
        Ok(cells)
    }

    pub fn add_cell(&mut self, key: Key, h_ptr: u64) -> Result<usize, Box<dyn Error>> {
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

        self.data[start..start + KEY_SIZE].copy_from_slice(&key.to_le_bytes());
        self.data[start + KEY_SIZE..end].clone_from_slice(&h_ptr.to_le_bytes());

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
            let mid_start = self.cell_start(mid, &hdr)?;
            let mid_key = Key::from_le_bytes(
                self.data[mid_start..mid_start + KEY_SIZE]
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
        let mut hdr = self.header()?;
        let mut all_cells = self.get_cells()?;

        if idx >= all_cells.len() {
            return Err("Cell index out of bounds".into());
        }
        let removed = all_cells.swap_remove(idx);

        let cell_size = KEY_SIZE + PTR_SIZE;
        let hole = self.cell_start(idx as u16, &hdr)?;
        let lowest = hdr.free_end as usize;
        if hole != lowest {
            let mover = (0..all_cells.len() as u16 + 1)
                .find(|&i| self.slot(i).cell_offset as usize == lowest)
                .ok_or_else(|| {
                    format!(
                        "corrupt index page {}: no cell starts at free_end {lowest}",
                        hdr.id
                    )
                })?;
            self.data.copy_within(lowest..lowest + cell_size, hole);
            let at = HEADER_SIZE + mover as usize * SLOT_SIZE;
            self.data[at..at + 2].copy_from_slice(&(hole as u16).to_le_bytes());
        }
        self.data[lowest..lowest + cell_size].fill(0);

        let slots_end = hdr.free_start as usize;
        let after = HEADER_SIZE + (idx + 1) * SLOT_SIZE;
        self.data.copy_within(after..slots_end, after - SLOT_SIZE);
        self.data[slots_end - SLOT_SIZE..slots_end].fill(0);

        hdr.free_start -= SLOT_SIZE as u16;
        hdr.free_end += cell_size as u16;
        hdr.serialize(&mut self.data[..HEADER_SIZE]);

        Ok(removed)
    }

    pub fn set_key_at(&mut self, idx: usize, key: Key) -> Result<bool, Box<dyn Error>> {
        let hdr = self.header()?;
        let count = self.num_cells()?;
        if idx >= count {
            return Err("Cell index out of bounds".into());
        }
        let key_at = |page: &Self, i: usize| -> Result<Key, Box<dyn Error>> {
            let start = page.cell_start(i as u16, &hdr)?;
            Ok(Key::from_le_bytes(
                page.data[start..start + KEY_SIZE].try_into().unwrap(),
            ))
        };
        if idx > 0 && key_at(self, idx - 1)? >= key {
            return Ok(false);
        }
        if idx + 1 < count && key_at(self, idx + 1)? <= key {
            return Ok(false);
        }
        let start = self.cell_start(idx as u16, &hdr)?;
        self.data[start..start + KEY_SIZE].copy_from_slice(&key.to_le_bytes());
        Ok(true)
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

    fn cell_start(&self, i: u16, hdr: &IndexHeader) -> Result<usize, Box<dyn Error>> {
        let start = self.slot(i).cell_offset as usize;
        if start < hdr.free_end as usize || start + KEY_SIZE + PTR_SIZE > PAGE_SIZE {
            return Err(format!(
                "corrupt index page {}: cell {i} at offset {start} lies outside the cell area",
                hdr.id
            )
            .into());
        }
        Ok(start)
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
    pub key: Key,
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

impl AsRef<[u8]> for HeapPage {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
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
        header.serialize(&mut self.data[0..heap_hdr_size]);
        Ok(())
    }

    pub fn empty(header: HeapHeader) -> HeapPage {
        let mut page = HeapPage {
            data: [0u8; super::heap::PAGE_SIZE],
        };
        header.serialize(&mut page.data[..super::heap::HEADER_SIZE]);
        page
    }

    pub fn shared_slots(&self) -> Result<Vec<Option<Vec<u8>>>, Box<dyn Error>> {
        let header = self.header()?;
        let slot_area = (header.free_start as usize)
            .checked_sub(super::heap::HEADER_SIZE)
            .ok_or("heap page slot area underflows its header")?;
        let count = slot_area / super::heap::SLOT_SIZE;
        let mut slots = Vec::with_capacity(count);
        for i in 0..count {
            let at = super::heap::HEADER_SIZE + i * super::heap::SLOT_SIZE;
            let offset = u16::from_le_bytes([self.data[at], self.data[at + 1]]) as usize;
            let size = u16::from_le_bytes([self.data[at + 2], self.data[at + 3]]) as usize;
            if offset == 0 {
                slots.push(None);
                continue;
            }
            let bytes = self
                .data
                .get(offset..offset + size)
                .ok_or("heap slot points outside its page")?;
            slots.push(Some(bytes.to_vec()));
        }
        Ok(slots)
    }

    pub fn shared_slot_count(&self) -> Result<usize, Box<dyn Error>> {
        let header = self.header()?;
        let slot_area = (header.free_start as usize)
            .checked_sub(super::heap::HEADER_SIZE)
            .ok_or("heap page slot area underflows its header")?;
        Ok(slot_area / super::heap::SLOT_SIZE)
    }

    pub fn free_shared_slot(&self) -> Result<usize, Box<dyn Error>> {
        let count = self.shared_slot_count()?;
        Ok((0..count)
            .find(|&i| self.slot_entry(i).0 == 0)
            .unwrap_or(count))
    }

    fn shared_live_bytes(&self) -> Result<usize, Box<dyn Error>> {
        let mut live = 0;
        for i in 0..self.shared_slot_count()? {
            let (offset, size) = self.slot_entry(i);
            if offset == 0 {
                continue;
            }
            if offset + size > self.data.len() {
                return Err("heap slot points outside its page".into());
            }
            live += size;
        }
        Ok(live)
    }

    pub fn shared_slot(&self, slot: usize) -> Result<Option<&[u8]>, Box<dyn Error>> {
        let header = self.header()?;
        let slot_area = (header.free_start as usize)
            .checked_sub(super::heap::HEADER_SIZE)
            .ok_or("heap page slot area underflows its header")?;
        if slot >= slot_area / super::heap::SLOT_SIZE {
            return Ok(None);
        }
        let (offset, size) = self.slot_entry(slot);
        if offset == 0 {
            return Ok(None);
        }
        let bytes = self
            .data
            .get(offset..offset + size)
            .ok_or("heap slot points outside its page")?;
        Ok(Some(bytes))
    }

    fn slot_entry(&self, i: usize) -> (usize, usize) {
        let at = super::heap::HEADER_SIZE + i * super::heap::SLOT_SIZE;
        (
            u16::from_le_bytes([self.data[at], self.data[at + 1]]) as usize,
            u16::from_le_bytes([self.data[at + 2], self.data[at + 3]]) as usize,
        )
    }

    fn set_slot_entry(&mut self, i: usize, offset: usize, size: usize) {
        let at = super::heap::HEADER_SIZE + i * super::heap::SLOT_SIZE;
        self.data[at..at + 2].copy_from_slice(&(offset as u16).to_le_bytes());
        self.data[at + 2..at + 4].copy_from_slice(&(size as u16).to_le_bytes());
    }

    pub fn shared_free_space(&self) -> Result<usize, Box<dyn Error>> {
        let header = self.header()?;
        let live = self.shared_live_bytes()?;
        Ok(super::heap::PAGE_SIZE
            .saturating_sub(header.free_start as usize + live + super::heap::SLOT_SIZE))
    }

    pub fn put_shared(&mut self, slot: usize, blob: &[u8]) -> Result<bool, Box<dyn Error>> {
        let mut header = self.header()?;
        let count =
            (header.free_start as usize - super::heap::HEADER_SIZE) / super::heap::SLOT_SIZE;
        let slot_end = if slot == count {
            header.free_start as usize + super::heap::SLOT_SIZE
        } else if slot < count && self.slot_entry(slot).0 == 0 {
            header.free_start as usize
        } else {
            return Err(format!("heap slot {slot} is not free on page {}", header.id).into());
        };
        let Some(start) = (header.free_end as usize)
            .checked_sub(blob.len())
            .filter(|&start| start >= slot_end)
        else {
            return Ok(false);
        };
        self.data[start..start + blob.len()].copy_from_slice(blob);
        self.set_slot_entry(slot, start, blob.len());
        header.free_start = slot_end as u16;
        header.free_end = start as u16;
        header.serialize(&mut self.data[..super::heap::HEADER_SIZE]);
        Ok(true)
    }

    pub fn clear_shared(&mut self, slot: usize) -> Result<(), Box<dyn Error>> {
        let mut header = self.header()?;
        let mut count =
            (header.free_start as usize - super::heap::HEADER_SIZE) / super::heap::SLOT_SIZE;
        if slot >= count || self.slot_entry(slot).0 == 0 {
            return Err(format!("heap slot {slot} is not in use on page {}", header.id).into());
        }
        self.set_slot_entry(slot, 0, 0);
        while count > 0 && self.slot_entry(count - 1).0 == 0 {
            count -= 1;
            self.set_slot_entry(count, 0, 0);
        }
        let free_end = (0..count)
            .map(|i| self.slot_entry(i).0)
            .filter(|&offset| offset != 0)
            .min()
            .unwrap_or(super::heap::PAGE_SIZE);
        header.free_start = (super::heap::HEADER_SIZE + count * super::heap::SLOT_SIZE) as u16;
        header.free_end = free_end as u16;
        header.serialize(&mut self.data[..super::heap::HEADER_SIZE]);
        Ok(())
    }

    pub fn build_shared(id: u64, slots: &[Option<Vec<u8>>]) -> Result<HeapPage, Box<dyn Error>> {
        let mut page = HeapPage::empty(HeapHeader::new_primary(id));
        let mut header = page.header()?;
        let slot_end = super::heap::HEADER_SIZE + slots.len() * super::heap::SLOT_SIZE;
        let mut data_start = super::heap::PAGE_SIZE;
        for (i, slot) in slots.iter().enumerate() {
            let at = super::heap::HEADER_SIZE + i * super::heap::SLOT_SIZE;
            let (offset, size) = match slot {
                None => (0u16, 0u16),
                Some(bytes) => {
                    data_start = data_start
                        .checked_sub(bytes.len())
                        .filter(|&start| start >= slot_end)
                        .ok_or("shared heap page overflow")?;
                    page.data[data_start..data_start + bytes.len()].copy_from_slice(bytes);
                    (data_start as u16, bytes.len() as u16)
                }
            };
            page.data[at..at + 2].copy_from_slice(&offset.to_le_bytes());
            page.data[at + 2..at + 4].copy_from_slice(&size.to_le_bytes());
        }
        header.free_start = slot_end as u16;
        header.free_end = data_start as u16;
        header.serialize(&mut page.data[..super::heap::HEADER_SIZE]);
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(page: &mut HeapPage, model: &mut Vec<Option<Vec<u8>>>, blob: Vec<u8>) -> usize {
        let slot = model
            .iter()
            .position(Option::is_none)
            .unwrap_or(model.len());
        if !page.put_shared(slot, &blob).unwrap() {
            match model.get_mut(slot) {
                Some(entry) => *entry = Some(blob.clone()),
                None => model.push(Some(blob.clone())),
            }
            *page = HeapPage::build_shared(page.header().unwrap().id, model).unwrap();
            return slot;
        }
        match model.get_mut(slot) {
            Some(entry) => *entry = Some(blob),
            None => model.push(Some(blob)),
        }
        slot
    }

    #[test]
    fn in_place_slots_match_a_model_through_reuse_and_compaction() {
        let mut page = HeapPage::build_shared(3, &[]).unwrap();
        let mut model: Vec<Option<Vec<u8>>> = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut compactions = 0;
        for step in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let live: Vec<usize> = (0..model.len()).filter(|&i| model[i].is_some()).collect();
            if !live.is_empty() && seed % 5 < 2 {
                let slot = live[(seed >> 8) as usize % live.len()];
                page.clear_shared(slot).unwrap();
                model[slot] = None;
                while matches!(model.last(), Some(None)) {
                    model.pop();
                }
            } else {
                let len = 1 + (seed >> 16) as usize % 300;
                if page.shared_free_space().unwrap() >= len + super::super::heap::SLOT_SIZE {
                    let before = page.header().unwrap().free_end;
                    store(&mut page, &mut model, vec![step as u8; len]);
                    if page.header().unwrap().free_end > before {
                        compactions += 1;
                    }
                }
            }
            assert_eq!(page.shared_slots().unwrap(), model, "step {step}");
            for slot in 0..model.len() + 2 {
                assert_eq!(
                    page.shared_slot(slot).unwrap(),
                    model.get(slot).and_then(|bytes| bytes.as_deref()),
                    "step {step} slot {slot}"
                );
            }
        }
        assert!(compactions > 0, "the compaction fallback never ran");
    }
}
