//! The WAL will give every Log record a log sequence number (LSN) and update
//! the modified data page's LSN in buffer pool with the matching the log
//! record's LSN number.
//!
//! On how this will be affecting the flushing of data page in buffer pools:
//!
//! Here with STEAL policy we will follow a strict rule...
//! Before the buffer pool manager issues write() for a data page, it checks:
//!
//! `Data_Page_LSN <= Last_Flushed_LSN_WAL`
//!
//! Then only can it be flushed to the file.
//!
//! If Last_Flushed_LSN_WAL is only at 1000, and the data page thats about to
//! be evicted/flushed is 1010 the buffer pool manager pauses the data page
//! flush and then does a Forced Log Flush; The engine forces a flush/fsync of
//! the WAL log buffer up to at least LSN 1010 to disk. Last_Flushed_LSN_WAL
//! becomes 1010.
//!
//! Now that Data_Page_LSN <= Last_Flushed_LSN_WAL, the buffer pool is finally
//! allowed to write the dirty data page to disk.
//!
//! Basically the ARIES algorithm

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::crc::Crc32c;
use super::io::{FileRef, OsVfs, StorageFile, Vfs};
use super::sync::lock;
use super::{header::WAL_HEADER_SIZE, header::WalHeader, wal_buffer::WalBuffer};

pub const WAL_RECORD_HEADER_SIZE: usize = 8 + 4 + 8 + 1 + 1 + 8 + 4 + 2 + 4; // 40 bytes
pub const WAL_RECORD_CRC_OFFSET: usize = 36;
pub const POISONED_MESSAGE: &str =
    "storage is poisoned after an earlier I/O failure; reopen the database";
pub const WAL_POOL_CAPACITY: usize = 10_000;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordType {
    Begin = 1, // the transaction begining
    Update = 2,
    Commit = 3,
    Abort = 4,
    /// Compensation log records are written during the undo phase of recovery,
    /// it writes the operations being performed, this is crucial cus we can be
    /// stuck in an infinte loop if the system crashes again while recovery.
    Clr = 5,
    Checkpoint = 6,
}

impl TryFrom<u8> for RecordType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Begin),
            2 => Ok(Self::Update),
            3 => Ok(Self::Commit),
            4 => Ok(Self::Abort),
            5 => Ok(Self::Clr),
            6 => Ok(Self::Checkpoint),
            _ => Err(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyPageEntry {
    pub is_index: bool,
    pub page_id: u64,
    pub rec_lsn: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveTxnEntry {
    pub txn_id: u64,
    pub last_lsn: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointData {
    pub dpt: Vec<DirtyPageEntry>,
    pub att: Vec<ActiveTxnEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecordHeader {
    pub lsn: u64,

    // pub offset: u32, // we don't need offset, we can derive it
    // pub prev_lsn: u64, // we don't need prev_lsn, we can derive it by self.lsn - 1
    /// This will allow track back (end->start) in WAL,
    /// we can derive the offset of the previous wal record by simply computing
    /// `current_offset - current.prev_record_len`
    pub prev_record_len: u32,
    pub txn_id: u64,
    pub record_type: RecordType,
    pub page_id: u64,
    pub is_index: bool, // else heap
    // [wal_record_header] \
    // [undo_data_size:u16][undo_payload_bytes] \
    // [redo_data_size:u16][redo_payload_bytes] \
    // [next_wal_record_header]
    pub payload_len: u32,         // +4 size for undo & redo size
    pub payload_page_offset: u16, // offset of the modified data record in the data page
}

impl WalRecordHeader {
    pub fn serialize(&self, buf: &mut [u8]) {
        buf[0..8].copy_from_slice(&self.lsn.to_le_bytes());
        buf[8..12].copy_from_slice(&self.prev_record_len.to_le_bytes());
        buf[12..20].copy_from_slice(&self.txn_id.to_le_bytes());
        buf[20] = self.record_type as u8;
        buf[21] = self.is_index as u8;
        buf[22..30].copy_from_slice(&self.page_id.to_le_bytes());
        buf[30..34].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[34..36].copy_from_slice(&self.payload_page_offset.to_le_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Option<Self> {
        if buf.len() < WAL_RECORD_CRC_OFFSET {
            return None;
        }
        Some(Self {
            lsn: u64::from_le_bytes(buf[0..8].try_into().ok()?),
            prev_record_len: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            txn_id: u64::from_le_bytes(buf[12..20].try_into().ok()?),
            record_type: RecordType::try_from(buf[20]).ok()?,
            is_index: buf[21] == 1,
            page_id: u64::from_le_bytes(buf[22..30].try_into().ok()?),
            payload_len: u32::from_le_bytes(buf[30..34].try_into().ok()?),
            payload_page_offset: u16::from_le_bytes(buf[34..36].try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone)]
pub struct UpdatePayload<'a> {
    pub offset_in_page: u16,
    pub undo_data: &'a [u8], // Old bytes
    pub redo_data: &'a [u8], // New bytes
}

impl<'a> UpdatePayload<'a> {
    pub fn serialize(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.undo_data.len() as u16).to_le_bytes());
        buf.extend_from_slice(self.undo_data);
        buf.extend_from_slice(&(self.redo_data.len() as u16).to_le_bytes());
        buf.extend_from_slice(self.redo_data);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    pub header: WalRecordHeader,
    pub payload: Vec<u8>,
}

impl WalRecord {
    pub fn parse_update(&self) -> Option<(u16, &[u8], &[u8])> {
        if self.payload.len() < 4 {
            return None;
        }
        let undo_len = u16::from_le_bytes(self.payload[0..2].try_into().ok()?) as usize;
        if self.payload.len() < 4 + undo_len {
            return None;
        }
        let undo_data = &self.payload[2..2 + undo_len];
        let redo_len =
            u16::from_le_bytes(self.payload[2 + undo_len..4 + undo_len].try_into().ok()?) as usize;
        if self.payload.len() < 4 + undo_len + redo_len {
            return None;
        }
        let redo_data = &self.payload[4 + undo_len..4 + undo_len + redo_len];
        Some((self.header.payload_page_offset, undo_data, redo_data))
    }

    pub fn total_len(&self) -> u64 {
        WAL_RECORD_HEADER_SIZE as u64 + self.payload.len() as u64
    }

    pub fn is_full_image(&self) -> bool {
        let page_size = if self.header.is_index {
            super::PAGE_SIZE
        } else {
            super::heap::PAGE_SIZE
        };
        matches!(
            self.header.record_type,
            RecordType::Update | RecordType::Clr
        ) && self
            .parse_update()
            .is_some_and(|(offset, _, redo)| offset == 0 && redo.len() == page_size)
    }

    pub fn parse_checkpoint(&self) -> Option<CheckpointData> {
        if self.header.record_type != RecordType::Checkpoint {
            return None;
        }
        let buf = &self.payload;
        if buf.len() < 4 {
            return None;
        }
        let dpt_len = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
        let mut offset = 4;
        let mut dpt = Vec::with_capacity(dpt_len);
        for _ in 0..dpt_len {
            if offset + 17 > buf.len() {
                return None;
            }
            let is_index = buf[offset] == 1;
            let page_id = u64::from_le_bytes(buf[offset + 1..offset + 9].try_into().ok()?);
            let rec_lsn = u64::from_le_bytes(buf[offset + 9..offset + 17].try_into().ok()?);
            dpt.push(DirtyPageEntry {
                is_index,
                page_id,
                rec_lsn,
            });
            offset += 17;
        }
        if offset + 4 > buf.len() {
            return None;
        }
        let att_len = u32::from_le_bytes(buf[offset..offset + 4].try_into().ok()?) as usize;
        offset += 4;
        let mut att = Vec::with_capacity(att_len);
        for _ in 0..att_len {
            if offset + 16 > buf.len() {
                return None;
            }
            let txn_id = u64::from_le_bytes(buf[offset..offset + 8].try_into().ok()?);
            let last_lsn = u64::from_le_bytes(buf[offset + 8..offset + 16].try_into().ok()?);
            att.push(ActiveTxnEntry { txn_id, last_lsn });
            offset += 16;
        }
        Some(CheckpointData { dpt, att })
    }
}

pub(super) fn record_crc(salt: u32, header: &[u8], payload: &[u8]) -> u32 {
    Crc32c::new()
        .update(&salt.to_le_bytes())
        .update(&header[..WAL_RECORD_CRC_OFFSET])
        .update(payload)
        .finish()
}

#[derive(Debug, Default)]
pub(super) struct WalScan {
    pub records: Vec<WalRecord>,
    pub valid_end: u64,
    pub last_offset: u64,
    pub last_len: u32,
    pub last_lsn: u64,
}

pub(super) fn page_diff(old: &[u8], new: &[u8]) -> Option<(u16, usize, usize)> {
    let len = old.len().min(new.len());
    let (old, new) = (&old[..len], &new[..len]);
    let mut start = 0;
    while start + DIFF_BLOCK <= len
        && old[start..start + DIFF_BLOCK] == new[start..start + DIFF_BLOCK]
    {
        start += DIFF_BLOCK;
    }
    let start = start
        + old[start..]
            .iter()
            .zip(&new[start..])
            .position(|(a, b)| a != b)?;
    let mut end = len;
    while end - start > DIFF_BLOCK && old[end - DIFF_BLOCK..end] == new[end - DIFF_BLOCK..end] {
        end -= DIFF_BLOCK;
    }
    let end = start
        + old[start..end]
            .iter()
            .zip(&new[start..end])
            .rposition(|(a, b)| a != b)?
        + 1;
    Some((start as u16, start, end))
}

const DIFF_SPLIT_GAP: usize = (WAL_RECORD_HEADER_SIZE + 4) / 2;
const DIFF_BLOCK: usize = 64;

pub(super) fn page_diff_runs(old: &[u8], new: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    let len = old.len().min(new.len());
    while i < len {
        let block = (i + DIFF_BLOCK).min(len);
        if i.is_multiple_of(DIFF_BLOCK) && old[i..block] == new[i..block] {
            i = block;
            continue;
        }
        if old[i] == new[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && old[i] != new[i] {
            i += 1;
        }
        match runs.last_mut() {
            Some(last) if start - last.1 <= DIFF_SPLIT_GAP => last.1 = i,
            _ => runs.push((start, i)),
        }
    }
    runs
}

pub(super) fn scan_log(log_file: &dyn StorageFile, salt: u32) -> Result<WalScan, Box<dyn Error>> {
    let file_len = log_file.len()?;
    let mut scan = WalScan {
        records: Vec::new(),
        valid_end: WAL_HEADER_SIZE as u64,
        last_offset: WAL_HEADER_SIZE as u64,
        last_len: 0,
        last_lsn: 0,
    };
    let mut offset = WAL_HEADER_SIZE as u64;
    while let Some(payload_start) = offset.checked_add(WAL_RECORD_HEADER_SIZE as u64) {
        if payload_start > file_len {
            break;
        }
        let mut header_buf = [0u8; WAL_RECORD_HEADER_SIZE];
        log_file.read_at(&mut header_buf, offset)?;
        let Some(header) = WalRecordHeader::deserialize(&header_buf) else {
            break;
        };
        if header.prev_record_len != scan.last_len {
            break;
        }
        let expected_lsn = if scan.records.is_empty() {
            header.lsn.max(1)
        } else {
            scan.last_lsn + 1
        };
        if header.lsn != expected_lsn {
            break;
        }
        let total_len = WAL_RECORD_HEADER_SIZE as u64 + header.payload_len as u64;
        if total_len > u32::MAX as u64 || offset + total_len > file_len {
            break;
        }
        let mut payload = vec![0u8; header.payload_len as usize];
        if !payload.is_empty() {
            log_file.read_at(&mut payload, payload_start)?;
        }
        let stored = u32::from_le_bytes(
            header_buf[WAL_RECORD_CRC_OFFSET..WAL_RECORD_HEADER_SIZE]
                .try_into()
                .unwrap_or([0; 4]),
        );
        if record_crc(salt, &header_buf, &payload) != stored {
            break;
        }
        scan.last_offset = offset;
        scan.last_len = total_len as u32;
        scan.last_lsn = header.lsn;
        offset += total_len;
        scan.valid_end = offset;
        scan.records.push(WalRecord { header, payload });
    }
    Ok(scan)
}

pub(super) struct WalInner {
    pub log_file: FileRef,
    pub log_path: PathBuf,
    pub wal_buffer: WalBuffer,
    pub header: WalHeader,
    pub flushed_lsn: u64,
    pub active_txns: HashMap<u64, u64>,
    pub imaged_pages: HashSet<(bool, u64)>,
    pub logged_txns: HashSet<u64>,
}

impl WalInner {
    pub fn write_raw_record(
        &mut self,
        txn_id: u64,
        record_type: RecordType,
        page_id: u64,
        is_index: bool,
        payload_page_offset: u16,
        payload: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        if self.wal_buffer.is_full() {
            self.flush_all()?;
        }

        let lsn = self.header.next_lsn;
        self.header.next_lsn += 1;

        let total_len = WAL_RECORD_HEADER_SIZE + payload.len();
        let mut buf = Vec::with_capacity(total_len);

        let rec_offset = self.header.last_wal_offset + self.header.last_wal_len as u64;

        let wal_rec_header = WalRecordHeader {
            lsn,
            prev_record_len: self.header.last_wal_len,
            txn_id,
            record_type,
            page_id,
            is_index,
            payload_len: payload.len() as u32,
            payload_page_offset,
        };

        let mut header_buf = [0u8; WAL_RECORD_HEADER_SIZE];
        wal_rec_header.serialize(&mut header_buf);
        let crc = record_crc(self.header.salt, &header_buf, payload);
        header_buf[WAL_RECORD_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());

        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(payload);

        self.header.last_wal_len = total_len as u32;
        self.header.last_wal_offset = rec_offset;

        self.wal_buffer.insert(&buf, lsn, rec_offset as usize);

        Ok(lsn)
    }

    pub fn write_record(
        &mut self,
        txn_id: u64,
        record_type: RecordType,
        page_id: u64,
        is_index: bool,
        payload: UpdatePayload,
    ) -> Result<u64, Box<dyn Error>> {
        let total_payload_len = payload.undo_data.len() + payload.redo_data.len() + 4;
        let mut buf = Vec::with_capacity(total_payload_len);
        payload.serialize(&mut buf);

        let lsn = self.write_raw_record(
            txn_id,
            record_type,
            page_id,
            is_index,
            payload.offset_in_page,
            &buf,
        )?;

        if txn_id != 0 {
            self.active_txns.insert(txn_id, lsn);
        }

        Ok(lsn)
    }

    pub fn log_page_change(
        &mut self,
        txn_id: u64,
        is_index: bool,
        page_id: u64,
        old: &[u8],
        new: &[u8],
    ) -> Result<Option<u64>, Box<dyn Error>> {
        if page_diff(old, new).is_none() {
            return Ok(None);
        }
        if !self.imaged_pages.contains(&(is_index, page_id)) {
            let payload = UpdatePayload {
                offset_in_page: 0,
                undo_data: old,
                redo_data: new,
            };
            let lsn = self.write_record(txn_id, RecordType::Update, page_id, is_index, payload)?;
            self.imaged_pages.insert((is_index, page_id));
            return Ok(Some(lsn));
        }
        let mut lsn = 0;
        for (start, end) in page_diff_runs(old, new) {
            let payload = UpdatePayload {
                offset_in_page: start as u16,
                undo_data: &old[start..end],
                redo_data: &new[start..end],
            };
            lsn = self.write_record(txn_id, RecordType::Update, page_id, is_index, payload)?;
        }
        Ok(Some(lsn))
    }

    pub fn begin_transaction(&mut self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let lsn = self.write_raw_record(txn_id, RecordType::Begin, 0, false, 0, &[])?;
        self.active_txns.insert(txn_id, lsn);
        self.logged_txns.insert(txn_id);
        Ok(txn_id)
    }

    pub fn commit_transaction(&mut self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let lsn = self.write_raw_record(txn_id, RecordType::Commit, 0, false, 0, &[])?;
        self.active_txns.remove(&txn_id);
        self.flush_up_to(lsn)?;
        Ok(lsn)
    }

    pub fn abort_transaction(&mut self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let lsn = self.write_raw_record(txn_id, RecordType::Abort, 0, false, 0, &[])?;
        self.active_txns.remove(&txn_id);
        self.flush_up_to(lsn)?;
        Ok(lsn)
    }

    pub fn write_clr(
        &mut self,
        txn_id: u64,
        is_index: bool,
        page_id: u64,
        payload_page_offset: u16,
        redo_data: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        let total_payload_len = redo_data.len() + 4;
        let mut buf = Vec::with_capacity(total_payload_len);
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&(redo_data.len() as u16).to_le_bytes());
        buf.extend_from_slice(redo_data);

        let lsn = self.write_raw_record(
            txn_id,
            RecordType::Clr,
            page_id,
            is_index,
            payload_page_offset,
            &buf,
        )?;

        if txn_id != 0 {
            self.active_txns.insert(txn_id, lsn);
        }

        Ok(lsn)
    }

    pub fn write_checkpoint(
        &mut self,
        dpt: &[DirtyPageEntry],
        att: &[ActiveTxnEntry],
    ) -> Result<u64, Box<dyn Error>> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(dpt.len() as u32).to_le_bytes());
        for entry in dpt {
            payload.push(entry.is_index as u8);
            payload.extend_from_slice(&entry.page_id.to_le_bytes());
            payload.extend_from_slice(&entry.rec_lsn.to_le_bytes());
        }
        payload.extend_from_slice(&(att.len() as u32).to_le_bytes());
        for entry in att {
            payload.extend_from_slice(&entry.txn_id.to_le_bytes());
            payload.extend_from_slice(&entry.last_lsn.to_le_bytes());
        }

        let lsn = self.write_raw_record(0, RecordType::Checkpoint, 0, false, 0, &payload)?;
        self.flush_up_to(lsn)?;
        self.header.last_checkpoint_lsn = lsn;
        self.flush_header()?;
        Ok(lsn)
    }

    pub fn flush_header(&mut self) -> Result<(), Box<dyn Error>> {
        let mut buf = [0u8; WAL_HEADER_SIZE];
        self.header.serialize(&mut buf);
        self.log_file.write_at(&buf, 0)?;
        self.log_file.sync()?;
        Ok(())
    }

    pub fn flush_up_to(&mut self, target_lsn: u64) -> Result<(), Box<dyn Error>> {
        if self.flushed_lsn >= target_lsn && target_lsn != u64::MAX {
            return Ok(());
        }

        if self.wal_buffer.offsets.is_empty() {
            return Ok(());
        }

        let first_lsn = self.wal_buffer.offsets[0].0;
        let start_file_offset = self.wal_buffer.offsets[0].1 as u64;

        if target_lsn < first_lsn && target_lsn != u64::MAX {
            return Ok(());
        }

        let Some(&(last_lsn, _)) = self.wal_buffer.offsets.last() else {
            return Ok(());
        };

        if target_lsn >= last_lsn {
            self.log_file
                .write_at(&self.wal_buffer.buffer, start_file_offset)?;
            self.log_file.sync()?;
            self.flushed_lsn = last_lsn;
            self.wal_buffer.buffer.clear();
            self.wal_buffer.offsets.clear();
        } else {
            let mut idx = 0;
            for (i, &(lsn, _)) in self.wal_buffer.offsets.iter().enumerate() {
                if lsn <= target_lsn {
                    idx = i;
                } else {
                    break;
                }
            }

            let next_record_offset = self.wal_buffer.offsets[idx + 1].1;
            let flush_len = next_record_offset - self.wal_buffer.offsets[0].1;

            self.log_file
                .write_at(&self.wal_buffer.buffer[..flush_len], start_file_offset)?;
            self.log_file.sync()?;
            self.flushed_lsn = self.wal_buffer.offsets[idx].0;

            self.wal_buffer.buffer.drain(..flush_len);
            self.wal_buffer.offsets.drain(..=idx);
        }

        Ok(())
    }

    pub fn flush_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.flush_up_to(u64::MAX)
    }

    pub fn truncate(&mut self) -> Result<(), Box<dyn Error>> {
        self.flush_all()?;
        self.header.salt = self.header.salt.wrapping_add(1);
        self.header.last_checkpoint_lsn = 0;
        self.header.last_wal_offset = WAL_HEADER_SIZE as u64;
        self.header.last_wal_len = 0;
        self.flush_header()?;
        self.log_file.set_len(WAL_HEADER_SIZE as u64)?;
        self.log_file.sync()?;
        self.imaged_pages.clear();
        self.logged_txns.clear();
        Ok(())
    }

    pub fn active_transaction_table(&self) -> Vec<ActiveTxnEntry> {
        self.active_txns
            .iter()
            .map(|(&txn_id, &last_lsn)| ActiveTxnEntry { txn_id, last_lsn })
            .collect()
    }
}

pub(super) struct Wal {
    pub inner: Mutex<WalInner>,
    pub flushed_lsn: AtomicU64,
    pub poisoned: AtomicBool,
}

pub fn sync_parent_dir(path: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let dir = File::open(parent)?;
    dir.sync_all()?;
    Ok(())
}

impl Wal {
    pub fn new(log_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        Self::open_in(&OsVfs, log_path)
    }

    pub fn open_in(vfs: &dyn Vfs, log_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let log_file = vfs.open(&log_path)?;

        vfs.sync_parent(&log_path)?;

        let existing_len = log_file.len()?;

        let mut header = if existing_len == 0 {
            let h = WalHeader {
                salt: 1,
                last_checkpoint_lsn: 0,
                last_wal_offset: WAL_HEADER_SIZE as u64,
                last_wal_len: 0,
                next_lsn: 1,
            };
            let mut buf = [0u8; WAL_HEADER_SIZE];
            h.serialize(&mut buf);
            log_file.write_at(&buf, 0)?;
            log_file.sync()?;
            h
        } else if existing_len >= WAL_HEADER_SIZE as u64 {
            let mut buf = [0u8; WAL_HEADER_SIZE];
            log_file.read_at(&mut buf, 0)?;
            WalHeader::deserialize(&buf).ok_or("corrupt wal header")?
        } else {
            return Err("wal file exists but header is truncated/corrupt".into());
        };

        let scan = scan_log(log_file.as_ref(), header.salt)?;
        let durable_end = header.last_wal_offset + header.last_wal_len as u64;
        if scan.valid_end < durable_end {
            return Err(format!(
                "corrupt wal: the log ends at byte {} but a checkpoint made it durable up to byte {durable_end}",
                scan.valid_end
            )
            .into());
        }
        let imaged_pages = scan
            .records
            .iter()
            .filter(|r| r.is_full_image())
            .map(|r| (r.header.is_index, r.header.page_id))
            .collect();
        let logged_txns = scan
            .records
            .iter()
            .map(|r| r.header.txn_id)
            .filter(|&txn| txn != 0)
            .collect();
        if log_file.len()? > scan.valid_end {
            log_file.set_len(scan.valid_end)?;
            log_file.sync()?;
        }
        header.last_wal_offset = scan.last_offset;
        header.last_wal_len = scan.last_len;
        header.next_lsn = header.next_lsn.max(scan.last_lsn + 1).max(1);

        let flushed_lsn = header.next_lsn - 1;

        let active_txns = HashMap::new();
        let wal_buffer = WalBuffer::new(WAL_POOL_CAPACITY);

        Ok(Self {
            inner: Mutex::new(WalInner {
                log_file,
                log_path,
                wal_buffer,
                header,
                flushed_lsn,
                active_txns,
                imaged_pages,
                logged_txns,
            }),
            flushed_lsn: AtomicU64::new(flushed_lsn),
            poisoned: AtomicBool::new(false),
        })
    }

    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), Box<dyn Error>> {
        if self.is_poisoned() {
            return Err(POISONED_MESSAGE.into());
        }
        Ok(())
    }

    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut WalInner) -> Result<T, Box<dyn Error>>,
    ) -> Result<T, Box<dyn Error>> {
        self.check()?;
        let mut inner = lock(&self.inner);
        let result = f(&mut inner);
        if result.is_err() {
            self.poison();
        }
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        result
    }

    pub fn flushed_lsn(&self) -> u64 {
        self.flushed_lsn.load(Ordering::Acquire)
    }

    pub fn last_checkpoint_lsn(&self) -> u64 {
        lock(&self.inner).header.last_checkpoint_lsn
    }

    pub fn header(&self) -> WalHeader {
        lock(&self.inner).header
    }

    pub fn write_raw_record(
        &self,
        txn_id: u64,
        record_type: RecordType,
        page_id: u64,
        is_index: bool,
        payload_page_offset: u16,
        payload: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| {
            inner.write_raw_record(
                txn_id,
                record_type,
                page_id,
                is_index,
                payload_page_offset,
                payload,
            )
        })
    }

    pub fn write_record(
        &self,
        txn_id: u64,
        record_type: RecordType,
        page_id: u64,
        is_index: bool,
        payload: UpdatePayload,
    ) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| inner.write_record(txn_id, record_type, page_id, is_index, payload))
    }

    pub fn log_page_change(
        &self,
        txn_id: u64,
        is_index: bool,
        page_id: u64,
        old: &[u8],
        new: &[u8],
    ) -> Result<Option<u64>, Box<dyn Error>> {
        self.mutate(|inner| inner.log_page_change(txn_id, is_index, page_id, old, new))
    }

    pub fn begin_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.check()?;
        if txn_id == 0 {
            return Err("transaction id 0 is reserved for autocommit".into());
        }
        if lock(&self.inner).logged_txns.contains(&txn_id) {
            return Err(format!(
                "transaction id {txn_id} is already used in the log; ids must be unique until the next checkpoint"
            )
            .into());
        }
        self.mutate(|inner| inner.begin_transaction(txn_id))
    }

    pub fn commit_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| inner.commit_transaction(txn_id))
    }

    pub fn abort_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| inner.abort_transaction(txn_id))
    }

    pub fn write_clr(
        &self,
        txn_id: u64,
        is_index: bool,
        page_id: u64,
        payload_page_offset: u16,
        redo_data: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| {
            inner.write_clr(txn_id, is_index, page_id, payload_page_offset, redo_data)
        })
    }

    pub fn write_checkpoint(
        &self,
        dpt: &[DirtyPageEntry],
        att: &[ActiveTxnEntry],
    ) -> Result<u64, Box<dyn Error>> {
        self.mutate(|inner| inner.write_checkpoint(dpt, att))
    }

    pub fn flush_header(&self) -> Result<(), Box<dyn Error>> {
        self.mutate(|inner| inner.flush_header())
    }

    pub fn flush_up_to(&self, target_lsn: u64) -> Result<(), Box<dyn Error>> {
        if target_lsn != u64::MAX && self.flushed_lsn() >= target_lsn {
            return self.check();
        }
        self.mutate(|inner| inner.flush_up_to(target_lsn))
    }

    pub fn flush_all(&self) -> Result<(), Box<dyn Error>> {
        self.mutate(|inner| inner.flush_all())
    }

    pub fn truncate(&self) -> Result<(), Box<dyn Error>> {
        self.check()?;
        if !lock(&self.inner).active_txns.is_empty() {
            return Err("cannot truncate the WAL while transactions are active".into());
        }
        self.mutate(|inner| inner.truncate())
    }

    pub fn size_bytes(&self) -> Result<u64, Box<dyn Error>> {
        let inner = lock(&self.inner);
        Ok(inner.log_file.len()?)
    }

    pub fn active_transaction_table(&self) -> Vec<ActiveTxnEntry> {
        let inner = lock(&self.inner);
        inner.active_transaction_table()
    }

    fn read_record_at_file(
        log_file: &dyn StorageFile,
        file_offset: u64,
    ) -> Result<(WalRecord, u64), Box<dyn Error>> {
        let mut header_buf = [0u8; WAL_RECORD_HEADER_SIZE];
        log_file.read_at(&mut header_buf, file_offset)?;
        let header =
            WalRecordHeader::deserialize(&header_buf).ok_or("Corrupt WAL record header")?;
        let next_offset = file_offset + WAL_RECORD_HEADER_SIZE as u64 + header.payload_len as u64;
        if next_offset > log_file.len()? {
            return Err("WAL record runs past the end of the log".into());
        }
        let mut payload = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 {
            log_file.read_at(&mut payload, file_offset + WAL_RECORD_HEADER_SIZE as u64)?;
        }
        Ok((WalRecord { header, payload }, next_offset))
    }

    pub fn read_record_at(&self, file_offset: u64) -> Result<(WalRecord, u64), Box<dyn Error>> {
        let inner = lock(&self.inner);
        Self::read_record_at_file(inner.log_file.as_ref(), file_offset)
    }

    pub fn scan(&self) -> Result<WalScan, Box<dyn Error>> {
        let inner = lock(&self.inner);
        scan_log(inner.log_file.as_ref(), inner.header.salt)
    }

    pub fn read_all_records(&self) -> Result<Vec<WalRecord>, Box<dyn Error>> {
        Ok(self.scan()?.records)
    }

    // pub fn read_records_from(&self, start_lsn: u64) -> Result<Vec<WalRecord>, Box<dyn Error>> {
    //     let all = self.read_all_records()?;
    //     Ok(all
    //         .into_iter()
    //         .filter(|r| r.header.lsn >= start_lsn)
    //         .collect())
    // }

    pub fn read_records_backward(&self) -> Result<Vec<WalRecord>, Box<dyn Error>> {
        let mut records = self.scan()?.records;
        records.reverse();
        Ok(records)
    }
}

#[cfg(test)]
mod test {
    use crate::storage::engine::bplus_tree::PAGE_SIZE;
    use crate::storage::engine::bplus_tree::io::SimVfs;

    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    const LOG: &str = "/db/t.wal";

    fn open(vfs: &Arc<SimVfs>) -> Result<Wal, Box<dyn Error>> {
        Wal::open_in(vfs.as_ref(), PathBuf::from(LOG))
    }

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; PAGE_SIZE]
    }

    fn write_pages(wal: &Wal, txn: u64, pages: u64) {
        wal.begin_transaction(txn).unwrap();
        for id in 0..pages {
            wal.log_page_change(txn, true, id, &page(0), &page(id as u8 + 1))
                .unwrap();
        }
        wal.commit_transaction(txn).unwrap();
    }

    fn record_offsets(records: &[WalRecord]) -> Vec<u64> {
        let mut offset = WAL_HEADER_SIZE as u64;
        records
            .iter()
            .map(|r| {
                let at = offset;
                offset += r.total_len();
                at
            })
            .collect()
    }

    fn damage(vfs: &Arc<SimVfs>, f: impl FnOnce(&mut Vec<u8>)) {
        let mut bytes = vfs.read_file(Path::new(LOG)).unwrap();
        f(&mut bytes);
        vfs.write_file(Path::new(LOG), bytes);
    }

    #[test]
    fn bad_record_crc_ends_the_log_and_open_cuts_the_tail() {
        let vfs = SimVfs::new();
        let offsets = {
            let wal = open(&vfs).unwrap();
            write_pages(&wal, 1, 4);
            record_offsets(&wal.read_all_records().unwrap())
        };
        assert_eq!(offsets.len(), 6);
        let victim = offsets[3];
        damage(&vfs, |b| {
            b[victim as usize + WAL_RECORD_HEADER_SIZE + 100] ^= 0x10
        });
        let wal = open(&vfs).unwrap();
        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(wal.size_bytes().unwrap(), victim);
        let lsn = wal.begin_transaction(2).unwrap();
        assert_eq!(lsn, 2);
        wal.flush_all().unwrap();
        drop(wal);
        let wal = open(&vfs).unwrap();
        let records = wal.read_all_records().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[3].header.lsn, records[2].header.lsn + 1);
    }

    #[test]
    fn torn_last_record_is_truncated_on_open() {
        let vfs = SimVfs::new();
        let offsets = {
            let wal = open(&vfs).unwrap();
            write_pages(&wal, 1, 2);
            record_offsets(&wal.read_all_records().unwrap())
        };
        let last = *offsets.last().unwrap();
        damage(&vfs, |b| b.truncate(last as usize + 7));
        let wal = open(&vfs).unwrap();
        assert_eq!(wal.read_all_records().unwrap().len(), offsets.len() - 1);
        assert_eq!(wal.size_bytes().unwrap(), last);
    }

    #[test]
    fn records_from_before_a_truncate_are_never_replayed() {
        let vfs = SimVfs::new();
        let stale = {
            let wal = open(&vfs).unwrap();
            write_pages(&wal, 1, 3);
            let stale = vfs.read_file(Path::new(LOG)).unwrap()[WAL_HEADER_SIZE..].to_vec();
            wal.truncate().unwrap();
            stale
        };
        damage(&vfs, |b| b.extend_from_slice(&stale));
        let wal = open(&vfs).unwrap();
        assert!(wal.read_all_records().unwrap().is_empty());
        assert_eq!(wal.size_bytes().unwrap(), WAL_HEADER_SIZE as u64);
    }
}
