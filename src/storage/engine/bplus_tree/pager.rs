use std::{
    collections::{HashMap, HashSet},
    error::Error,
    path::PathBuf,
    sync::Arc,
};

use super::{
    heap::Heap,
    index::Index,
    slotted_page::Cell,
    wal::{DirtyPageEntry, RecordType, UpdatePayload, Wal, sync_parent_dir},
};

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
        let wal_path = index_path.with_file_name("wal_file.db");
        Self::new_with_wal(index_path, heap_path, wal_path)
    }

    pub fn new_with_wal(
        index_path: PathBuf,
        heap_path: PathBuf,
        wal_path: PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        let heap_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&heap_path)?;

        let index_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&index_path)?;

        sync_parent_dir(&heap_path)?;
        sync_parent_dir(&index_path)?;

        let wal = Arc::new(Wal::new(wal_path)?);
        let heap = Heap::new(heap_file, heap_path, Arc::clone(&wal));
        let index = Index::new(index_file, index_path, Arc::clone(&wal));

        Ok(Pager { index, heap, wal })
    }

    pub fn fetch_heap_data(
        &self,
        cell: &Cell,
        data_record: &mut Vec<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(ref heap_ptr) = cell.h_ptr {
            self.heap.get_record(heap_ptr.index, data_record)?;
            Ok(())
        } else {
            Err("Couldn't find the header pointer".into())
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
        if let Some((offset, start, end)) = Self::page_diff(old_data, new_data) {
            let lsn = self.log_index_update(
                txn_id,
                page_id,
                offset,
                &old_data[start..end],
                &new_data[start..end],
            )?;
            Ok(Some(lsn))
        } else {
            Ok(None)
        }
    }

    pub fn log_heap_diff(
        &self,
        txn_id: u64,
        page_id: u64,
        old_data: &[u8],
        new_data: &[u8],
    ) -> Result<Option<u64>, Box<dyn Error>> {
        if let Some((offset, start, end)) = Self::page_diff(old_data, new_data) {
            let lsn = self.log_heap_update(
                txn_id,
                page_id,
                offset,
                &old_data[start..end],
                &new_data[start..end],
            )?;
            Ok(Some(lsn))
        } else {
            Ok(None)
        }
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

    pub fn recover(&self) -> Result<RecoveryReport, Box<dyn Error>> {
        let chk_lsn = self.wal.last_checkpoint_lsn();
        let mut dpt: HashMap<(bool, u64), u64> = HashMap::new();
        let mut att: HashMap<u64, u64> = HashMap::new();
        let mut start_lsn = 0;

        let all_records = self.wal.read_all_records()?;

        if chk_lsn > 0
            && let Some(chk_record) = all_records.iter().find(|r| r.header.lsn == chk_lsn)
            && let Some(chk_data) = chk_record.parse_checkpoint()
        {
            for entry in chk_data.dpt {
                dpt.insert((entry.is_index, entry.page_id), entry.rec_lsn);
            }
            for entry in chk_data.att {
                att.insert(entry.txn_id, entry.last_lsn);
            }
            start_lsn = chk_lsn + 1;
        }

        for record in all_records.iter().filter(|r| r.header.lsn >= start_lsn) {
            match record.header.record_type {
                RecordType::Begin => {
                    att.insert(record.header.txn_id, record.header.lsn);
                }
                RecordType::Update => {
                    if record.header.txn_id != 0 {
                        att.insert(record.header.txn_id, record.header.lsn);
                    }
                    dpt.entry((record.header.is_index, record.header.page_id))
                        .or_insert(record.header.lsn);
                }
                RecordType::Commit | RecordType::Abort => {
                    att.remove(&record.header.txn_id);
                }
                RecordType::Clr => {
                    if record.header.txn_id != 0 {
                        att.insert(record.header.txn_id, record.header.lsn);
                    }
                    dpt.entry((record.header.is_index, record.header.page_id))
                        .or_insert(record.header.lsn);
                }
                RecordType::Checkpoint => {}
            }
        }

        let min_rec_lsn = if dpt.is_empty() {
            chk_lsn
        } else {
            *dpt.values().min().unwrap()
        };

        let mut redone_records = 0;

        for record in all_records.iter().filter(|r| r.header.lsn >= min_rec_lsn) {
            if record.header.record_type != RecordType::Update
                && record.header.record_type != RecordType::Clr
            {
                continue;
            }

            let page_key = (record.header.is_index, record.header.page_id);
            if let Some(&rec_lsn) = dpt.get(&page_key)
                && record.header.lsn >= rec_lsn
                && let Some((offset, _, redo)) = record.parse_update()
            {
                let off = offset as usize;
                if record.header.is_index {
                    let page_id = record.header.page_id;
                    let req_len = (page_id + 1) * super::PAGE_SIZE as u64;
                    if self.index.index_file.metadata()?.len() < req_len {
                        self.index.index_file.set_len(req_len)?;
                    }
                    if self.index.next_id.load(std::sync::atomic::Ordering::SeqCst) <= page_id {
                        self.index
                            .next_id
                            .store(page_id + 1, std::sync::atomic::Ordering::SeqCst);
                    }
                    let mut page = self.index.fetch_mut(page_id);
                    page.data[off..off + redo.len()].copy_from_slice(redo);
                    drop(page);
                    self.index.pool.update_lsn(page_id, record.header.lsn);
                } else {
                    let page_id = record.header.page_id;
                    let req_len = (page_id + 1) * super::heap::PAGE_SIZE as u64;
                    if self.heap.heap_file.metadata()?.len() < req_len {
                        self.heap.heap_file.set_len(req_len)?;
                    }
                    if self.heap.next_id.load(std::sync::atomic::Ordering::SeqCst) <= page_id {
                        self.heap
                            .next_id
                            .store(page_id + 1, std::sync::atomic::Ordering::SeqCst);
                    }
                    let mut page = self.heap.fetch_mut(page_id)?;
                    page.data[off..off + redo.len()].copy_from_slice(redo);
                    drop(page);
                    self.heap.pool.update_lsn(page_id, record.header.lsn);
                }
                redone_records += 1;
            }
        }

        let loser_txns: HashSet<u64> = att.keys().copied().filter(|&id| id != 0).collect();
        let mut undone_records = 0;

        if !loser_txns.is_empty() {
            let backward_records = self.wal.read_records_backward()?;
            for record in backward_records {
                if loser_txns.contains(&record.header.txn_id)
                    && record.header.record_type == RecordType::Update
                    && let Some((offset, undo, _)) = record.parse_update()
                {
                    let off = offset as usize;
                    if record.header.is_index {
                        let mut page = self.index.fetch_mut(record.header.page_id);
                        page.data[off..off + undo.len()].copy_from_slice(undo);
                    } else {
                        let mut page = self.heap.fetch_mut(record.header.page_id)?;
                        page.data[off..off + undo.len()].copy_from_slice(undo);
                    }

                    self.wal.write_clr(
                        record.header.txn_id,
                        record.header.is_index,
                        record.header.page_id,
                        offset,
                        undo,
                    )?;
                    undone_records += 1;
                }
            }

            for &txn_id in &loser_txns {
                self.wal.abort_transaction(txn_id)?;
            }
        }

        self.wal.flush_all()?;

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
