use std::{collections::HashSet, error::Error, path::PathBuf, sync::Arc};

use super::{
    header::HeapHeader,
    heap::{
        CHAIN_SLOT, Heap, MAX_BLOB_BYTES, MAX_RECORD_SIZE, SHARED_MAX_BLOB, SLOT_SIZE, decode_blob,
        encode_blob, pack_ptr, unpack_ptr,
    },
    index::Index,
    io::{OsVfs, Vfs},
    slotted_page::{Cell, HeapPage},
    wal::{DirtyPageEntry, RecordType, UpdatePayload, Wal},
};

const MIN_CACHE_PAGES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeOptions {
    pub index_cache_pages: usize,
    pub heap_cache_pages: usize,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            index_cache_pages: 10_000,
            heap_cache_pages: 10_000,
        }
    }
}

pub struct RecoveryReport {
    pub checkpoint_lsn: u64,
    pub redone_records: usize,
    pub undone_records: usize,
    pub active_txns: Vec<u64>,
}

pub(super) struct Pager {
    pub index: Index,
    pub heap: Heap,
    pub wal: Arc<Wal>,
}

impl Pager {
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
            &OsVfs,
            index_path,
            heap_path,
            wal_path,
            TreeOptions::default(),
        )
    }

    pub fn open_in(
        vfs: &dyn Vfs,
        index_path: PathBuf,
        heap_path: PathBuf,
        wal_path: PathBuf,
        options: TreeOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let heap_file = vfs.open(&heap_path)?;
        let index_file = vfs.open(&index_path)?;

        vfs.sync_parent(&heap_path)?;
        vfs.sync_parent(&index_path)?;

        let wal = Arc::new(Wal::open_in(vfs, wal_path)?);
        let heap = Heap::new_with_capacity(
            heap_file,
            heap_path,
            Arc::clone(&wal),
            options.heap_cache_pages.max(MIN_CACHE_PAGES),
        )?;
        let index = Index::new_with_capacity(
            index_file,
            index_path,
            Arc::clone(&wal),
            options.index_cache_pages.max(MIN_CACHE_PAGES),
        )?;

        for record in wal.read_all_records()? {
            let header = &record.header;
            if record.is_full_image() {
                if header.is_index {
                    index.expect_replay([header.page_id]);
                } else {
                    heap.expect_replay([header.page_id]);
                }
            }
        }

        Ok(Pager { index, heap, wal })
    }

    pub fn fetch_heap_data(
        &self,
        cell: &Cell,
        data_record: &mut Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        match cell.h_ptr {
            Some(ref heap_ptr) => self.load_records(heap_ptr.index, data_record),
            None => Err("Couldn't find the header pointer".into()),
        }
    }

    fn write_heap_page(&self, txn_id: u64, id: u64, page: &HeapPage) -> Result<(), Box<dyn Error>> {
        let mut guard = self.heap.fetch_mut(id)?;
        let old = guard.data;
        self.log_heap_diff(txn_id, id, &old, &page.data)?;
        guard.data = page.data;
        Ok(())
    }

    pub fn check_records(records: &[Vec<u8>]) -> Result<(), Box<dyn Error>> {
        let len = super::heap::encoded_blob_len(records);
        if len > MAX_BLOB_BYTES {
            return Err(
                format!("records of {len} bytes exceed the {MAX_BLOB_BYTES} byte limit").into(),
            );
        }
        Ok(())
    }

    pub fn store_records(&self, txn_id: u64, records: Vec<Vec<u8>>) -> Result<u64, Box<dyn Error>> {
        Self::check_records(&records)?;
        let blob = encode_blob(&records);

        if blob.len() <= SHARED_MAX_BLOB {
            let needed = blob.len() + SLOT_SIZE;
            let (id, current) = match self.heap.find_shared_page(needed) {
                Some(id) => (id, Some(self.heap.fetch(id)?)),
                None => (self.heap.allocate()?, None),
            };
            let slot = match &current {
                Some(page) => page.free_shared_slot()?,
                None => 0,
            };
            let mut page = match current {
                Some(page) => page,
                None => HeapPage::build_shared(id, &[])?,
            };
            if !page.put_shared(slot, &blob)? {
                let mut slots = page.shared_slots()?;
                match slots.get_mut(slot) {
                    Some(entry) => *entry = Some(blob),
                    None => slots.push(Some(blob)),
                }
                page = HeapPage::build_shared(id, &slots)?;
            }
            self.write_heap_page(txn_id, id, &page)?;
            self.heap.note_shared_space(id, page.shared_free_space()?);
            return Ok(pack_ptr(id, slot as u16));
        }

        let chunks: Vec<&[u8]> = blob.chunks(MAX_RECORD_SIZE).collect();
        let ids = chunks
            .iter()
            .map(|_| self.heap.allocate())
            .collect::<Result<Vec<u64>, _>>()?;
        for (i, chunk) in chunks.iter().enumerate() {
            let mut header = if i == 0 {
                HeapHeader::new_primary(ids[i])
            } else {
                HeapHeader::new_overflow(ids[i])
            };
            if let Some(next) = ids.get(i + 1) {
                header.set_has_overflow(*next);
            }
            let mut page = HeapPage::empty(header);
            page.add_cell(chunk.to_vec())?;
            self.write_heap_page(txn_id, ids[i], &page)?;
        }
        Ok(pack_ptr(ids[0], CHAIN_SLOT))
    }

    pub fn load_records(&self, ptr: u64, out: &mut Vec<Vec<u8>>) -> Result<(), Box<dyn Error>> {
        let (id, slot) = unpack_ptr(ptr);
        if slot == CHAIN_SLOT {
            let mut chunks = Vec::new();
            self.heap.get_record(id, &mut chunks)?;
            out.extend(decode_blob(&chunks.concat())?);
        } else {
            let records = self
                .heap
                .with_page(id, |page| -> Result<_, Box<dyn Error>> {
                    let blob = page
                        .shared_slot(slot as usize)?
                        .ok_or_else(|| format!("dangling heap pointer to page {id} slot {slot}"))?;
                    decode_blob(blob)
                })??;
            out.extend(records);
        }
        Ok(())
    }

    pub fn remove_records(&self, txn_id: u64, ptr: u64) -> Result<Vec<u64>, Box<dyn Error>> {
        let (id, slot) = unpack_ptr(ptr);
        if slot == CHAIN_SLOT {
            return self.heap.free_chain(id);
        }
        let mut page = self.heap.fetch(id)?;
        if page.shared_slot(slot as usize)?.is_none() {
            return Err(format!("dangling heap pointer to page {id} slot {slot}").into());
        }
        page.clear_shared(slot as usize)?;
        self.write_heap_page(txn_id, id, &page)?;
        if page.shared_slot_count()? == 0 {
            self.heap.forget_shared_page(id);
            Ok(vec![id])
        } else {
            self.heap.note_shared_space(id, page.shared_free_space()?);
            Ok(Vec::new())
        }
    }

    pub fn flush_all(&self) -> Result<(), Box<dyn Error>> {
        self.wal.flush_all()?;
        self.index.flush_all()?;
        self.heap.flush_all()?;
        Ok(())
    }

    pub fn flush_index_page(&self, id: u64) -> Result<(), Box<dyn Error>> {
        let lsn = self.index.pool.page_lsn(id);
        if lsn > 0 {
            self.wal.flush_up_to(lsn)?;
        }
        self.index.flush(id)?;
        Ok(())
    }

    pub fn flush_heap_page(&self, id: u64) -> Result<(), Box<dyn Error>> {
        let lsn = self.heap.pool.page_lsn(id);
        if lsn > 0 {
            self.wal.flush_up_to(lsn)?;
        }
        self.heap.flush(id)?;
        Ok(())
    }

    pub fn discard_all_dirty(&self) {
        self.index.discard_dirty();
        self.heap.discard_dirty();
    }

    pub fn dirty_pages(&self) -> (Vec<u64>, Vec<u64>) {
        (
            self.index.pool.dirty_page_ids(),
            self.heap.pool.dirty_page_ids(),
        )
    }

    pub fn log_index_update(
        &self,
        txn_id: u64,
        page_id: u64,
        offset: u16,
        undo: &[u8],
        redo: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        let payload = UpdatePayload {
            offset_in_page: offset,
            undo_data: undo,
            redo_data: redo,
        };
        let lsn = self
            .wal
            .write_record(txn_id, RecordType::Update, page_id, true, payload)?;
        self.index.pool.update_lsn(page_id, lsn);
        Ok(lsn)
    }

    pub fn log_heap_update(
        &self,
        txn_id: u64,
        page_id: u64,
        offset: u16,
        undo: &[u8],
        redo: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        let payload = UpdatePayload {
            offset_in_page: offset,
            undo_data: undo,
            redo_data: redo,
        };
        let lsn = self
            .wal
            .write_record(txn_id, RecordType::Update, page_id, false, payload)?;
        self.heap.pool.update_lsn(page_id, lsn);
        Ok(lsn)
    }

    pub fn log_index_diff(
        &self,
        txn_id: u64,
        page_id: u64,
        old_data: &[u8],
        new_data: &[u8],
    ) -> Result<Option<u64>, Box<dyn Error>> {
        let lsn = self
            .wal
            .log_page_change(txn_id, true, page_id, old_data, new_data)?;
        if let Some(lsn) = lsn {
            self.index.pool.logged_image(page_id, lsn, new_data);
        }
        Ok(lsn)
    }

    pub fn log_heap_diff(
        &self,
        txn_id: u64,
        page_id: u64,
        old_data: &[u8],
        new_data: &[u8],
    ) -> Result<Option<u64>, Box<dyn Error>> {
        let lsn = self
            .wal
            .log_page_change(txn_id, false, page_id, old_data, new_data)?;
        if let Some(lsn) = lsn {
            self.heap.pool.logged_image(page_id, lsn, new_data);
        }
        Ok(lsn)
    }

    pub fn fuzzy_checkpoint(&self) -> Result<u64, Box<dyn Error>> {
        self.index.sync_data()?;
        self.heap.sync_data()?;

        let mut dpt = Vec::new();

        for (page_id, rec_lsn) in self.index.pool.dirty_page_table() {
            dpt.push(DirtyPageEntry {
                is_index: true,
                page_id,
                rec_lsn,
            });
        }

        for (page_id, rec_lsn) in self.heap.pool.dirty_page_table() {
            dpt.push(DirtyPageEntry {
                is_index: false,
                page_id,
                rec_lsn,
            });
        }

        let att = self.wal.active_transaction_table();
        let checkpoint_lsn = self.wal.write_checkpoint(&dpt, &att)?;
        Ok(checkpoint_lsn)
    }

    fn apply_image(
        &self,
        is_index: bool,
        page_id: u64,
        offset: u16,
        image: &[u8],
        lsn: u64,
    ) -> Result<(), Box<dyn Error>> {
        let off = offset as usize;
        let page_size = if is_index {
            super::PAGE_SIZE
        } else {
            super::heap::PAGE_SIZE
        };
        if off + image.len() > page_size {
            return Err(format!(
                "corrupt WAL: image of {} bytes at offset {off} does not fit page {page_id}",
                image.len()
            )
            .into());
        }
        if is_index {
            self.index.ensure_page(page_id)?;
            let mut page = self.index.fetch_mut(page_id)?;
            page.data[off..off + image.len()].copy_from_slice(image);
            drop(page);
            self.index.pool.update_lsn(page_id, lsn);
        } else {
            self.heap.ensure_page(page_id)?;
            let mut page = self.heap.fetch_mut(page_id)?;
            page.data[off..off + image.len()].copy_from_slice(image);
            drop(page);
            self.heap.pool.update_lsn(page_id, lsn);
        }
        Ok(())
    }

    pub fn recover(&self) -> Result<RecoveryReport, Box<dyn Error>> {
        let chk_lsn = self.wal.last_checkpoint_lsn();
        let all_records = self.wal.read_all_records()?;

        let mut loser_txns: HashSet<u64> = HashSet::new();
        let mut redone_records = 0;

        for record in &all_records {
            let header = &record.header;
            match header.record_type {
                RecordType::Begin => {
                    if header.txn_id != 0 {
                        loser_txns.insert(header.txn_id);
                    }
                }
                RecordType::Update | RecordType::Clr => {
                    if header.txn_id != 0 {
                        loser_txns.insert(header.txn_id);
                    }
                    let (offset, _, redo) = record
                        .parse_update()
                        .ok_or("corrupt WAL: malformed update payload")?;
                    self.apply_image(header.is_index, header.page_id, offset, redo, header.lsn)?;
                    redone_records += 1;
                }
                RecordType::Commit | RecordType::Abort => {
                    loser_txns.remove(&header.txn_id);
                }
                RecordType::Checkpoint => {}
            }
        }

        let mut undone_records = 0;

        if !loser_txns.is_empty() {
            for record in all_records.iter().rev() {
                let header = &record.header;
                if header.record_type != RecordType::Update || !loser_txns.contains(&header.txn_id)
                {
                    continue;
                }
                let (offset, undo, _) = record
                    .parse_update()
                    .ok_or("corrupt WAL: malformed update payload")?;
                let clr_lsn = self.wal.write_clr(
                    header.txn_id,
                    header.is_index,
                    header.page_id,
                    offset,
                    undo,
                )?;
                self.apply_image(header.is_index, header.page_id, offset, undo, clr_lsn)?;
                undone_records += 1;
            }

            let mut losers: Vec<u64> = loser_txns.iter().copied().collect();
            losers.sort_unstable();
            for txn_id in losers {
                self.wal.abort_transaction(txn_id)?;
            }
        }

        self.wal.flush_all()?;
        self.index.end_replay();
        self.heap.end_replay();

        let mut active_list: Vec<u64> = loser_txns.into_iter().collect();
        active_list.sort_unstable();

        Ok(RecoveryReport {
            checkpoint_lsn: chk_lsn,
            redone_records,
            undone_records,
            active_txns: active_list,
        })
    }
    fn page_diff(old: &[u8], new: &[u8]) -> Option<(u16, usize, usize)> {
        let mut start = None;
        for i in 0..old.len() {
            if old[i] != new[i] {
                start = Some(i);
                break;
            }
        }
        let start = start?;
        let mut end = start + 1;
        for i in (start..old.len()).rev() {
            if old[i] != new[i] {
                end = i + 1;
                break;
            }
        }
        Some((start as u16, start, end))
    }
}
