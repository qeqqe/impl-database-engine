use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    executor::{
        codec::encode_row,
        ddl::{CatalogEntry, parse_catalog_entry, resolve_index},
        error::{DbError, DbResult},
        keys::{
            CATALOG_TABLE, FIRST_USER_TABLE, ObjectId, TableId, encode_row_key, index_key,
            index_prefix, row_bits, table_bounds, tree_key,
        },
        lock::LockManager,
        result::StatementResult,
        session::Session,
        source::{CatalogState, column_types},
    },
    planner::{
        bind_checks,
        catalog::{Catalog, IndexDef, TableSchema},
        logical::Plan,
        physical_expr::PhysExpr,
        types::DataType,
        value::{Row, Value},
    },
    sql::Statement,
    storage::engine::bplus_tree::{
        BplusTree, Key, TreeOptions,
        io::{DirLock, OsVfs, Vfs},
    },
};

pub(super) const READ_ONLY_TXN: u64 = 0;
pub const MAX_ROW_BYTES: usize = 1024 * 1024;
const WAL_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
pub const FORMAT_TAG: &str = "impl-database-engine 4";
const DATA_FILES: [&str; 3] = ["data.idx", "data.heap", "data.wal"];

#[derive(Clone)]
pub struct DbConfig {
    pub vfs: Arc<dyn Vfs>,
    pub index_cache_pages: usize,
    pub heap_cache_pages: usize,
}

impl Default for DbConfig {
    fn default() -> Self {
        let tree = TreeOptions::default();
        Self {
            vfs: OsVfs::shared(),
            index_cache_pages: tree.index_cache_pages,
            heap_cache_pages: tree.heap_cache_pages,
        }
    }
}

pub(super) struct Engine {
    pub(super) tree: BplusTree,
    pub(super) locks: LockManager,
    catalog: RwLock<Arc<CatalogState>>,
    row_ids: Mutex<HashMap<TableId, u64>>,
    next_txn_id: AtomicU64,
    next_session_id: AtomicU64,
    closed: AtomicBool,
    _lock: DirLock,
}

pub struct Database {
    session: Session,
}

pub(super) struct Ctx<'a, S> {
    tree: &'a BplusTree,
    state: S,
}

pub(super) struct Writer<'a> {
    state: &'a mut CatalogState,
    row_ids: &'a mut HashMap<TableId, u64>,
}

impl Deref for Writer<'_> {
    type Target = CatalogState;

    fn deref(&self) -> &CatalogState {
        self.state
    }
}

impl DerefMut for Writer<'_> {
    fn deref_mut(&mut self) -> &mut CatalogState {
        self.state
    }
}

struct TableInfo {
    id: TableId,
    schema: TableSchema,
    types: Vec<DataType>,
    indexes: Vec<IndexInfo>,
    checks: Vec<(String, PhysExpr)>,
}

struct IndexInfo {
    id: ObjectId,
    def: IndexDef,
}

impl TableInfo {
    fn key_of(&self, row: &[Value]) -> DbResult<Option<Key>> {
        let Some(pk) = self.schema.integer_key_column() else {
            return Ok(None);
        };
        match &row[pk] {
            Value::Integer(k) => Ok(Some(tree_key(self.id, encode_row_key(*k)))),
            Value::Null => Err(DbError::NotNullViolation {
                table: self.schema.name.clone(),
                column: self.schema.columns[pk].name.clone(),
            }),
            other => Err(DbError::Internal(format!(
                "integer key column holds {}",
                other.data_type()
            ))),
        }
    }

    fn describe_key(&self, row: &[Value]) -> String {
        match self.schema.integer_key_column() {
            Some(pk) => row[pk].to_string(),
            None => "(hidden row id)".into(),
        }
    }

    fn check_not_null(&self, row: &[Value]) -> DbResult<()> {
        for (i, value) in row.iter().enumerate() {
            if value.is_null() && !self.schema.is_nullable(i) {
                return Err(DbError::NotNullViolation {
                    table: self.schema.name.clone(),
                    column: self.schema.columns[i].name.clone(),
                });
            }
        }
        Ok(())
    }

    fn new(
        catalog: &Catalog,
        id: TableId,
        schema: TableSchema,
        indexes: Vec<IndexInfo>,
    ) -> DbResult<Self> {
        let checks = schema
            .checks
            .iter()
            .map(ToString::to_string)
            .zip(bind_checks(catalog, &schema)?)
            .collect();
        Ok(Self {
            id,
            types: column_types(&schema),
            schema,
            indexes,
            checks,
        })
    }

    fn check_row(&self, row: &[Value]) -> DbResult<()> {
        self.check_not_null(row)?;
        for (constraint, check) in &self.checks {
            if check.eval(row)? == Value::Boolean(false) {
                return Err(DbError::CheckViolation {
                    table: self.schema.name.clone(),
                    constraint: constraint.clone(),
                });
            }
        }
        Ok(())
    }

    fn encode(&self, row: &[Value]) -> DbResult<Vec<u8>> {
        let bytes = encode_row(&self.types, row)?;
        if bytes.len() > MAX_ROW_BYTES {
            return Err(DbError::RowTooLarge {
                table: self.schema.name.clone(),
                bytes: bytes.len(),
                limit: MAX_ROW_BYTES,
            });
        }
        Ok(bytes)
    }

    fn index_entry(&self, index: &IndexInfo, key: Key, row: &[Value]) -> Option<Key> {
        let column = index.def.columns[0];
        let prefix = index_prefix(self.types[column], &row[column])?;
        Some(index_key(index.id, prefix, row_bits(key)))
    }

    fn index_types(&self, index: &IndexInfo) -> Vec<DataType> {
        index.def.columns.iter().map(|&c| self.types[c]).collect()
    }

    fn index_record(&self, index: &IndexInfo, row: &[Value]) -> DbResult<Vec<u8>> {
        let values: Row = index.def.columns.iter().map(|&c| row[c].clone()).collect();
        encode_row(&self.index_types(index), &values)
    }
}

fn unique_key(cols: &[usize], row: &[Value]) -> Option<Vec<Value>> {
    cols.iter()
        .map(|&c| (!row[c].is_null()).then(|| row[c].clone()))
        .collect()
}

impl Engine {
    fn open(dir: &Path, config: DbConfig) -> DbResult<Self> {
        let vfs = config.vfs;
        vfs.create_dir_all(dir).map_err(DbError::storage)?;
        let lock = lock_directory(vfs.as_ref(), dir)?;
        check_format(vfs.as_ref(), dir)?;
        let tree = BplusTree::open_in(
            vfs.as_ref(),
            dir.join("data.idx"),
            dir.join("data.heap"),
            dir.join("data.wal"),
            TreeOptions {
                index_cache_pages: config.index_cache_pages,
                heap_cache_pages: config.heap_cache_pages,
            },
        )
        .map_err(DbError::storage)?;
        tree.recover().map_err(DbError::storage)?;

        let mut state = CatalogState {
            next_table_id: FIRST_USER_TABLE,
            ..CatalogState::default()
        };
        let (low, high) = table_bounds(CATALOG_TABLE);
        let mut pending_indexes = Vec::new();
        for entry in tree.range(READ_ONLY_TXN, low, high) {
            let (key, records) = entry.map_err(DbError::storage)?;
            let id = TableId::try_from(row_bits(key))
                .map_err(|_| DbError::Corrupt(format!("catalog key {key} is not a table id")))?;
            let [ddl] = records.as_slice() else {
                return Err(DbError::Corrupt(format!(
                    "catalog entry {id} has {} records",
                    records.len()
                )));
            };
            let sql = std::str::from_utf8(ddl)
                .map_err(|_| DbError::Corrupt(format!("catalog entry {id} is not UTF-8")))?;
            match parse_catalog_entry(sql)? {
                CatalogEntry::Table(schema) => {
                    state.table_ids.insert(schema.name.to_lowercase(), id);
                    state.catalog.create_table(schema)?;
                }
                CatalogEntry::Index(create) => pending_indexes.push((id, create)),
            }
            state.next_table_id = state.next_table_id.max(id.saturating_add(1));
        }
        for (id, create) in pending_indexes {
            let schema = state.catalog.get_table(&create.table).map_err(|_| {
                DbError::Corrupt(format!(
                    "index {} belongs to table {}, which does not exist",
                    create.name, create.table
                ))
            })?;
            let index = resolve_index(&create, schema)?;
            state.index_ids.insert(index.name.to_lowercase(), id);
            state.catalog.create_index(index)?;
        }
        tree.checkpoint_and_truncate().map_err(DbError::storage)?;

        Ok(Self {
            tree,
            locks: super::lock::LockManager::new(),
            catalog: RwLock::new(Arc::new(state)),
            row_ids: Mutex::new(HashMap::new()),
            next_txn_id: AtomicU64::new(1),
            next_session_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            _lock: lock,
        })
    }

    pub(super) fn committed(&self) -> Arc<CatalogState> {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn publish(&self, state: CatalogState) {
        *self.catalog.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(state);
    }

    pub(super) fn row_ids(&self) -> MutexGuard<'_, HashMap<TableId, u64>> {
        self.row_ids.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn next_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(super) fn allocate_txn(&self) -> DbResult<u64> {
        if self.wal_size_bytes()? > WAL_CHECKPOINT_BYTES {
            self.checkpoint()?;
        }
        let id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);
        self.tree.begin_transaction(id).map_err(DbError::storage)?;
        Ok(id)
    }

    pub(super) fn checkpoint(&self) -> DbResult<()> {
        self.tree
            .checkpoint_and_truncate()
            .map_err(DbError::storage)
    }

    pub(super) fn wal_size_bytes(&self) -> DbResult<u64> {
        self.tree.wal_size_bytes().map_err(DbError::storage)
    }

    pub(super) fn reader<'a>(&'a self, state: &'a CatalogState) -> Ctx<'a, &'a CatalogState> {
        Ctx {
            tree: &self.tree,
            state,
        }
    }

    pub(super) fn writer<'a>(
        &'a self,
        state: &'a mut CatalogState,
        row_ids: &'a mut HashMap<TableId, u64>,
    ) -> Ctx<'a, Writer<'a>> {
        Ctx {
            tree: &self.tree,
            state: Writer { state, row_ids },
        }
    }

    fn shutdown(&self) -> DbResult<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.checkpoint()
    }

    fn crash(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

pub(super) fn is_read_only(plan: &Plan) -> bool {
    matches!(
        plan,
        Plan::Query(_) | Plan::ShowTables | Plan::Describe(_) | Plan::Explain { .. }
    )
}

fn lock_directory(vfs: &dyn Vfs, dir: &Path) -> DbResult<DirLock> {
    vfs.lock(&dir.join("LOCK")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            DbError::Locked(dir.display().to_string())
        } else {
            DbError::storage(e)
        }
    })
}

fn check_format(vfs: &dyn Vfs, dir: &Path) -> DbResult<()> {
    let path = dir.join("FORMAT");
    if vfs.exists(&path).map_err(DbError::storage)? {
        let file = vfs.open(&path).map_err(DbError::storage)?;
        let len = file.len().map_err(DbError::storage)?;
        let mut buf = vec![0u8; len.min(4096) as usize];
        file.read_at(&mut buf, 0).map_err(DbError::storage)?;
        let found = String::from_utf8_lossy(&buf);
        if found.trim() == FORMAT_TAG {
            return Ok(());
        }
        if !found.trim().is_empty() || has_data(vfs, dir)? {
            return Err(DbError::IncompatibleFormat {
                found: found.trim().to_string(),
                expected: FORMAT_TAG,
            });
        }
    } else if has_data(vfs, dir)? {
        return Err(DbError::IncompatibleFormat {
            found: "no FORMAT file".into(),
            expected: FORMAT_TAG,
        });
    }
    let file = vfs.open(&path).map_err(DbError::storage)?;
    let tag = format!("{FORMAT_TAG}\n");
    file.set_len(0).map_err(DbError::storage)?;
    file.write_at(tag.as_bytes(), 0).map_err(DbError::storage)?;
    file.sync().map_err(DbError::storage)?;
    vfs.sync_dir(dir).map_err(DbError::storage)
}

fn has_data(vfs: &dyn Vfs, dir: &Path) -> DbResult<bool> {
    for name in DATA_FILES {
        let path = dir.join(name);
        if vfs.exists(&path).map_err(DbError::storage)?
            && !vfs
                .open(&path)
                .and_then(|f| f.is_empty())
                .map_err(DbError::storage)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Database {
    pub fn open(dir: impl AsRef<Path>) -> DbResult<Self> {
        Self::open_with_vfs(OsVfs::shared(), dir)
    }

    pub fn open_with_vfs(vfs: Arc<dyn Vfs>, dir: impl AsRef<Path>) -> DbResult<Self> {
        Self::open_with(
            dir,
            DbConfig {
                vfs,
                ..DbConfig::default()
            },
        )
    }

    pub fn open_with(dir: impl AsRef<Path>, config: DbConfig) -> DbResult<Self> {
        let engine = Arc::new(Engine::open(dir.as_ref(), config)?);
        Ok(Self {
            session: Session::new(engine),
        })
    }

    pub fn session(&self) -> Session {
        Session::new(self.session.engine().clone())
    }

    pub fn default_session(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn catalog(&self) -> &Catalog {
        self.session.catalog()
    }

    pub fn in_transaction(&self) -> bool {
        self.session.in_transaction()
    }

    pub fn execute(&mut self, sql: &str) -> DbResult<StatementResult> {
        self.session.execute(sql)
    }

    pub fn explain(&self, sql: &str) -> DbResult<String> {
        self.session.explain(sql)
    }

    pub fn execute_script(&mut self, sql: &str) -> DbResult<Vec<StatementResult>> {
        self.session.execute_script(sql)
    }

    pub fn execute_statement(&mut self, statement: &Statement) -> DbResult<StatementResult> {
        self.session.execute_statement(statement)
    }

    pub fn checkpoint(&mut self) -> DbResult<()> {
        self.session.checkpoint()
    }

    pub fn wal_size_bytes(&self) -> DbResult<u64> {
        self.session.wal_size_bytes()
    }

    pub fn close(mut self) -> DbResult<()> {
        self.session.end()?;
        let engine = self.session.engine();
        let others = Arc::strong_count(engine) - 1;
        if others > 0 {
            return Err(DbError::SessionsOpen(others));
        }
        engine.shutdown()
    }

    pub fn crash(self) {
        self.session.engine().crash();
    }
}

impl<S: Deref<Target = CatalogState>> Ctx<'_, S> {
    pub(super) fn run_read(&self, plan: Plan, txn: u64) -> DbResult<StatementResult> {
        todo!()
    }

    pub(super) fn explain_query(&self, plan: Plan, txn: u64) -> DbResult<String> {
        todo!()
    }
}

impl Ctx<'_, Writer<'_>> {
    pub(super) fn run(&mut self, plan: Plan, txn: u64) -> DbResult<StatementResult> {
        todo!()
    }
}
