use std::{sync::Arc, time::Duration};

use crate::executor::{database::Engine, lock::LockMode, source::CatalogState};

pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Session {
    engine: Arc<Engine>,
    id: u64,
    busy_timeout: Duration,
    held: Option<LockMode>,
    txn: TxnState,
    catalog: Arc<CatalogState>,
}

enum TxnState {
    Idle,
    Active {
        explicit: bool,
        write: Option<WriteTxn>,
    },
}

struct WriteTxn {
    id: u64,
    working: CatalogState,
}
