use std::{sync::Arc, time::Duration};

use crate::executor::result::StatementResult;
use crate::executor::{database::Engine, lock::LockMode, source::CatalogState};

use crate::planner::catalog::Catalog;
use crate::planner::logical::Plan;
use crate::planner::plan_statement;
use crate::sql::{Parser, Statement, TransactionMode};

use super::database::READ_ONLY_TXN;
use super::error::{DbError, DbResult};

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

impl Session {
    pub(super) fn new(engine: Arc<Engine>) -> Self {
        Self {
            id: engine.next_session_id(),
            catalog: engine.committed(),
            engine,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            held: None,
            txn: TxnState::Idle,
        }
    }

    pub(super) fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    pub fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
    }

    pub fn catalog(&self) -> &Catalog {
        &self.state().catalog
    }

    pub fn in_transaction(&self) -> bool {
        matches!(self.txn, TxnState::Active { explicit: true, .. })
    }

    pub fn lock_mode(&self) -> Option<LockMode> {
        self.held
    }

    pub fn execute(&mut self, sql: &str) -> DbResult<StatementResult> {
        let statement = Parser::parse(sql)?;
        self.execute_statement(&statement)
    }

    pub fn execute_script(&mut self, sql: &str) -> DbResult<Vec<StatementResult>> {
        Parser::parse_multi(sql)?
            .iter()
            .map(|statement| self.execute_statement(statement))
            .collect()
    }

    pub fn execute_statement(&mut self, statement: &Statement) -> DbResult<StatementResult> {
        let result = self.dispatch(statement);
        if matches!(self.txn, TxnState::Idle) {
            self.release();
        }
        result
    }

    pub fn explain(&self, sql: &str) -> DbResult<String> {
        let statement = Parser::parse(sql)?;
        self.check_open()?;
        let locked = self.held.is_none();
        if locked {
            self.engine
                .locks
                .acquire_shared(self.id, self.busy_timeout)?;
        }
        let result = self.explain_locked(&statement);
        if locked {
            self.engine.locks.release(LockMode::Shared);
        }
        result
    }

    pub fn checkpoint(&mut self) -> DbResult<()> {
        if self.in_transaction() {
            return Err(DbError::Transaction(
                "cannot checkpoint inside a transaction".into(),
            ));
        }
        self.check_open()?;
        self.lock_exclusive()?;
        let result = self.engine.checkpoint();
        self.release();
        result
    }

    pub fn wal_size_bytes(&self) -> DbResult<u64> {
        self.engine.wal_size_bytes()
    }

    pub(super) fn end(&mut self) -> DbResult<()> {
        let result = self.abort_active();
        self.release();
        result
    }

    fn explain_locked(&self, statement: &Statement) -> DbResult<String> {
        let committed;
        let state = match &self.txn {
            TxnState::Active {
                write: Some(write), ..
            } => &write.working,
            _ if self.held.is_some() => self.catalog.as_ref(),
            _ => {
                committed = self.engine.committed();
                committed.as_ref()
            }
        };
        let plan = plan_statement(&state.catalog, statement)?;

        self.engine.reader(state).explain_query(plan, self.txn_id())
    }

    fn dispatch(&mut self, statement: &Statement) -> DbResult<StatementResult> {
        match statement {
            Statement::Begin(mode) => return self.begin(*mode),
            Statement::Commit => return self.commit(),
            Statement::Rollback => return self.rollback(),
            _ => {}
        }
        self.check_open()?;
        let writes = writes(statement);
        if writes {
            self.lock_exclusive()?;
        } else {
            self.lock_shared()?;
        }
        self.refresh();
        match &self.txn {
            TxnState::Idle if writes => self.autocommit(statement),
            TxnState::Active { write: Some(_), .. } if writes => {
                let plan = plan_statement(&self.state().catalog, statement)?;
                self.run_statement_in_transaction(plan)
            }
            TxnState::Active { .. } if writes => {
                let plan = plan_statement(&self.catalog.catalog, statement)?;
                self.start_write()?;
                self.run_statement_in_transaction(plan)
            }
            _ => {
                let state = self.state();
                let plan = plan_statement(&state.catalog, statement)?;
                self.engine.reader(state).run_read(plan, self.txn_id())
            }
        }
    }

    fn begin(&mut self, mode: TransactionMode) -> DbResult<StatementResult> {
        if self.in_transaction() {
            return Err(DbError::Transaction(
                "a transaction is already in progress".into(),
            ));
        }
        self.check_open()?;
        if mode == TransactionMode::Immediate {
            self.lock_exclusive()?;
        }
        self.txn = TxnState::Active {
            explicit: true,
            write: None,
        };
        Ok(StatementResult::Done("BEGIN"))
    }

    fn commit(&mut self) -> DbResult<StatementResult> {
        if !self.in_transaction() {
            return Err(DbError::Transaction("no transaction in progress".into()));
        }
        self.finish()?;
        Ok(StatementResult::Done("COMMIT"))
    }

    fn rollback(&mut self) -> DbResult<StatementResult> {
        if !self.in_transaction() {
            return Err(DbError::Transaction("no transaction in progress".into()));
        }
        self.abort_active()?;
        Ok(StatementResult::Done("ROLLBACK"))
    }

    fn autocommit(&mut self, statement: &Statement) -> DbResult<StatementResult> {
        let plan = plan_statement(&self.catalog.catalog, statement)?;
        self.txn = TxnState::Active {
            explicit: false,
            write: None,
        };
        let outcome = self.start_write().and_then(|()| self.run_write(plan));
        let outcome = outcome.and_then(|result| self.finish().map(|()| result));
        match outcome {
            Ok(result) => Ok(result),
            Err(err) => {
                self.abort_active()?;
                Err(err.root_cause().clone())
            }
        }
    }

    fn finish(&mut self) -> DbResult<()> {
        if let TxnState::Active {
            write: Some(write), ..
        } = &self.txn
            && let Err(e) = self.engine.tree.commit_transaction(write.id)
        {
            let err = DbError::storage(e);
            self.abort_active()?;
            return Err(DbError::RolledBack(Box::new(err)));
        }
        if let TxnState::Active {
            write: Some(write), ..
        } = std::mem::replace(&mut self.txn, TxnState::Idle)
        {
            self.engine.publish(write.working);
        }
        self.catalog = self.engine.committed();
        Ok(())
    }

    fn run_statement_in_transaction(&mut self, plan: Plan) -> DbResult<StatementResult> {
        let (id, snapshot) = match &self.txn {
            TxnState::Active {
                write: Some(write), ..
            } => (write.id, write.working.clone()),
            _ => return Err(no_write_txn()),
        };
        let savepoint = self.engine.tree.savepoint(id);
        let err = match self.run_write(plan) {
            Ok(result) => return Ok(result),
            Err(err) => err,
        };
        match self.engine.tree.rollback_to(id, savepoint) {
            Ok(()) => {
                if let TxnState::Active {
                    write: Some(write), ..
                } = &mut self.txn
                {
                    write.working = snapshot;
                }
                self.engine.row_ids().clear();
                Err(err)
            }
            Err(_) => {
                self.abort_active()?;
                Err(DbError::RolledBack(Box::new(err)))
            }
        }
    }

    fn run_write(&mut self, plan: Plan) -> DbResult<StatementResult> {
        let TxnState::Active {
            write: Some(write), ..
        } = &mut self.txn
        else {
            return Err(no_write_txn());
        };
        let mut row_ids = self.engine.row_ids();
        self.engine
            .writer(&mut write.working, &mut row_ids)
            .run(plan, write.id)
    }

    fn start_write(&mut self) -> DbResult<()> {
        let TxnState::Active { write, .. } = &mut self.txn else {
            return Err(no_write_txn());
        };
        if write.is_none() {
            let id = self.engine.allocate_txn()?;
            *write = Some(WriteTxn {
                id,
                working: self.catalog.as_ref().clone(),
            });
        }
        Ok(())
    }

    fn abort_active(&mut self) -> DbResult<()> {
        let TxnState::Active { write, .. } = std::mem::replace(&mut self.txn, TxnState::Idle)
        else {
            return Ok(());
        };
        let Some(write) = write else {
            return Ok(());
        };
        self.engine.row_ids().clear();
        self.catalog = self.engine.committed();
        self.engine
            .tree
            .abort_transaction(write.id)
            .map(|_| ())
            .map_err(DbError::storage)
    }

    fn state(&self) -> &CatalogState {
        match &self.txn {
            TxnState::Active {
                write: Some(write), ..
            } => &write.working,
            _ => &self.catalog,
        }
    }

    fn txn_id(&self) -> u64 {
        match &self.txn {
            TxnState::Active {
                write: Some(write), ..
            } => write.id,
            _ => READ_ONLY_TXN,
        }
    }

    fn refresh(&mut self) {
        if !matches!(self.txn, TxnState::Active { write: Some(_), .. }) {
            self.catalog = self.engine.committed();
        }
    }

    fn check_open(&self) -> DbResult<()> {
        if self.engine.is_closed() {
            return Err(DbError::Transaction("the database is closed".into()));
        }
        Ok(())
    }

    fn lock_shared(&mut self) -> DbResult<()> {
        if self.held.is_none() {
            self.engine
                .locks
                .acquire_shared(self.id, self.busy_timeout)?;
            self.held = Some(LockMode::Shared);
        }
        Ok(())
    }

    fn lock_exclusive(&mut self) -> DbResult<()> {
        match self.held {
            Some(LockMode::Exclusive) => return Ok(()),
            Some(LockMode::Shared) => self.engine.locks.upgrade(self.id, self.busy_timeout)?,
            None => self
                .engine
                .locks
                .acquire_exclusive(self.id, self.busy_timeout)?,
        }
        self.held = Some(LockMode::Exclusive);
        Ok(())
    }

    fn release(&mut self) {
        if let Some(mode) = self.held.take() {
            self.engine.locks.release(mode);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.engine.is_closed() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.abort_active()));
        }
        self.txn = TxnState::Idle;
        self.release();
    }
}

fn writes(statement: &Statement) -> bool {
    !matches!(
        statement,
        Statement::Select(_)
            | Statement::SetOperation(_)
            | Statement::ShowTables
            | Statement::Describe(_)
            | Statement::Explain { .. }
    )
}

fn no_write_txn() -> DbError {
    DbError::Internal("a write ran without a storage transaction".into())
}
