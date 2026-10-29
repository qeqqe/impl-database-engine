use std::sync::atomic::{AtomicBool, AtomicU64};

use crate::{executor::lock::LockManager, storage::engine::bplus_tree::BplusTree};

pub(super) const READ_ONLY_TXN: u64 = 0;
pub const MAX_ROW_BYTES: usize = 1024 * 1024;
const WAL_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
pub const FORMAT_TAG: &str = "impl-database-engine 4";
const DATA_FILES: [&str; 3] = ["data.idx", "data.heap", "data.wal"];

pub(super) struct Engine {
    pub(super) tree: BplusTree,
    pub(super) lock: LockManager,
    next_txn_id: AtomicU64,
    next_session_id: AtomicU64,
    closed: AtomicBool,
}
