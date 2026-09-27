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

use std::collections::HashMap;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{header::WAL_HEADER_SIZE, header::WalHeader, wal_buffer::WalBuffer};

pub const WAL_RECORD_HEADER_SIZE: usize = 8 + 4 + 8 + 1 + 1 + 8 + 4 + 2; // 36 bytes
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

pub(super) struct WalInner {
    pub log_file: File,
    pub log_path: PathBuf,
    pub wal_buffer: WalBuffer,
    pub header: WalHeader,
    pub flushed_lsn: u64,
    pub active_txns: HashMap<u64, u64>,
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

    pub fn begin_transaction(&mut self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let lsn = self.write_raw_record(txn_id, RecordType::Begin, 0, false, 0, &[])?;
        self.active_txns.insert(txn_id, lsn);
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
        self.log_file.write_all_at(&buf, 0)?;
        self.log_file.sync_all()?;
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

        let last_lsn = self.wal_buffer.offsets.last().unwrap().0;

        let (flushed_offset, flushed_len) = if target_lsn >= last_lsn {
            let last_offset = self.wal_buffer.offsets.last().unwrap().1 as u64;
            let last_len = self.header.last_wal_len;
            self.log_file
                .write_all_at(&self.wal_buffer.buffer, start_file_offset)?;
            self.log_file.sync_data()?;
            self.flushed_lsn = last_lsn;
            self.wal_buffer.buffer.clear();
            self.wal_buffer.offsets.clear();
            (last_offset, last_len)
        } else {
            let mut idx = 0;
            for (i, &(lsn, _)) in self.wal_buffer.offsets.iter().enumerate() {
                if lsn <= target_lsn {
                    idx = i;
                } else {
                    break;
                }
            }

            let flushed_offset = self.wal_buffer.offsets[idx].1 as u64;
            let next_record_offset = self.wal_buffer.offsets[idx + 1].1;
            let flushed_len = (next_record_offset - self.wal_buffer.offsets[idx].1) as u32;
            let flush_len = next_record_offset - self.wal_buffer.offsets[0].1;

            self.log_file
                .write_all_at(&self.wal_buffer.buffer[..flush_len], start_file_offset)?;
            self.log_file.sync_data()?;
            self.flushed_lsn = self.wal_buffer.offsets[idx].0;

            self.wal_buffer.buffer.drain(..flush_len);
            self.wal_buffer.offsets.drain(..=idx);
            (flushed_offset, flushed_len)
        };

        let mut disk_header = self.header;
        disk_header.last_wal_offset = flushed_offset;
        disk_header.last_wal_len = flushed_len;
        let mut buf = [0u8; WAL_HEADER_SIZE];
        disk_header.serialize(&mut buf);
        self.log_file.write_all_at(&buf, 0)?;
        self.log_file.sync_data()?;
        Ok(())
    }

    pub fn flush_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.flush_up_to(u64::MAX)
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
        let log_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&log_path)?;

        sync_parent_dir(&log_path)?;

        let existing_len = log_file.metadata()?.len();

        let header = if existing_len == 0 {
            let h = WalHeader {
                last_checkpoint_lsn: 0,
                last_wal_offset: WAL_HEADER_SIZE as u64,
                last_wal_len: 0,
                next_lsn: 1,
            };
            let mut buf = [0u8; WAL_HEADER_SIZE];
            h.serialize(&mut buf);
            log_file.write_all_at(&buf, 0)?;
            log_file.sync_all()?;
            h
        } else if existing_len >= WAL_HEADER_SIZE as u64 {
            let mut buf = [0u8; WAL_HEADER_SIZE];
            log_file.read_exact_at(&mut buf, 0)?;
            WalHeader::deserialize(&buf).ok_or("corrupt wal header")?
        } else {
            return Err("wal file exists but header is truncated/corrupt".into());
        };

        let flushed_lsn = header.next_lsn.saturating_sub(1);

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
            }),
            flushed_lsn: AtomicU64::new(flushed_lsn),
        })
    }

    pub fn flushed_lsn(&self) -> u64 {
        self.flushed_lsn.load(Ordering::Acquire)
    }

    pub fn last_checkpoint_lsn(&self) -> u64 {
        self.inner.lock().unwrap().header.last_checkpoint_lsn
    }

    pub fn header(&self) -> WalHeader {
        self.inner.lock().unwrap().header
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
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.write_raw_record(
            txn_id,
            record_type,
            page_id,
            is_index,
            payload_page_offset,
            payload,
        )?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn write_record(
        &self,
        txn_id: u64,
        record_type: RecordType,
        page_id: u64,
        is_index: bool,
        payload: UpdatePayload,
    ) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.write_record(txn_id, record_type, page_id, is_index, payload)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn begin_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let tid = inner.begin_transaction(txn_id)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(tid)
    }

    pub fn commit_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.commit_transaction(txn_id)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn abort_transaction(&self, txn_id: u64) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.abort_transaction(txn_id)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn write_clr(
        &self,
        txn_id: u64,
        is_index: bool,
        page_id: u64,
        payload_page_offset: u16,
        redo_data: &[u8],
    ) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.write_clr(txn_id, is_index, page_id, payload_page_offset, redo_data)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn write_checkpoint(
        &self,
        dpt: &[DirtyPageEntry],
        att: &[ActiveTxnEntry],
    ) -> Result<u64, Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        let lsn = inner.write_checkpoint(dpt, att)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(lsn)
    }

    pub fn flush_header(&self) -> Result<(), Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush_header()
    }

    pub fn flush_up_to(&self, target_lsn: u64) -> Result<(), Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush_up_to(target_lsn)?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(())
    }

    pub fn flush_all(&self) -> Result<(), Box<dyn Error>> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush_all()?;
        self.flushed_lsn.store(inner.flushed_lsn, Ordering::Release);
        Ok(())
    }

    pub fn active_transaction_table(&self) -> Vec<ActiveTxnEntry> {
        let inner = self.inner.lock().unwrap();
        inner.active_transaction_table()
    }

    fn read_record_at_file(
        log_file: &File,
        file_offset: u64,
    ) -> Result<(WalRecord, u64), Box<dyn Error>> {
        let mut header_buf = [0u8; WAL_RECORD_HEADER_SIZE];
        log_file.read_exact_at(&mut header_buf, file_offset)?;
        let header =
            WalRecordHeader::deserialize(&header_buf).ok_or("Corrupt WAL record header")?;
        let mut payload = vec![0u8; header.payload_len as usize];
        if header.payload_len > 0 {
            log_file.read_exact_at(&mut payload, file_offset + WAL_RECORD_HEADER_SIZE as u64)?;
        }
        let next_offset = file_offset + WAL_RECORD_HEADER_SIZE as u64 + header.payload_len as u64;
        Ok((WalRecord { header, payload }, next_offset))
    }

    pub fn read_record_at(&self, file_offset: u64) -> Result<(WalRecord, u64), Box<dyn Error>> {
        let inner = self.inner.lock().unwrap();
        Self::read_record_at_file(&inner.log_file, file_offset)
    }

    pub fn read_all_records(&self) -> Result<Vec<WalRecord>, Box<dyn Error>> {
        let inner = self.inner.lock().unwrap();
        let file_len = inner.log_file.metadata()?.len();
        let mut offset = WAL_HEADER_SIZE as u64;
        let mut records = Vec::new();

        while offset + WAL_RECORD_HEADER_SIZE as u64 <= file_len {
            let (record, next_offset) = match Self::read_record_at_file(&inner.log_file, offset) {
                Ok(r) => r,
                Err(_) => break,
            };
            records.push(record);
            offset = next_offset;
        }

        Ok(records)
    }

    // pub fn read_records_from(&self, start_lsn: u64) -> Result<Vec<WalRecord>, Box<dyn Error>> {
    //     let all = self.read_all_records()?;
    //     Ok(all
    //         .into_iter()
    //         .filter(|r| r.header.lsn >= start_lsn)
    //         .collect())
    // }

    pub fn read_records_backward(&self) -> Result<Vec<WalRecord>, Box<dyn Error>> {
        let inner = self.inner.lock().unwrap();
        let mut records = Vec::new();
        if inner.header.last_wal_len == 0 {
            return Ok(records);
        }

        let mut offset = inner.header.last_wal_offset;
        while offset >= WAL_HEADER_SIZE as u64 {
            let (record, _) = Self::read_record_at_file(&inner.log_file, offset)?;
            let prev_len = record.header.prev_record_len;
            records.push(record);
            if prev_len == 0 {
                break;
            }
            if offset < WAL_HEADER_SIZE as u64 + prev_len as u64 {
                break;
            }
            offset -= prev_len as u64;
        }

        Ok(records)
    }
}
