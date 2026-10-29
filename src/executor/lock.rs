use std::{
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use crate::executor::error::{DbError, DbResult};

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Default)]
pub struct LockState {
    readers: usize,
    writer: Option<u64>,
    pending: Option<u64>,
}

#[derive(Default, Debug)]
pub struct LockManager {
    state: Mutex<LockState>,
    cond: Condvar,
}

impl LockManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn acquire_shared(&self, session: u64, timeout: Duration) -> DbResult<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.writer.is_none() && state.pending.is_none() {
                state.readers += 1;
                return Ok(());
            }
            state = self
                .wait(state, deadline)
                .ok_or_else(|| busy(session, "a shared lock"))?;
        }
    }

    pub fn acquire_exclusive(&self, session: u64, timeout: Duration) -> DbResult<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.writer.is_none() && state.pending.is_none() {
                state.pending = Some(session);
                break;
            }
            state = self
                .wait(state, deadline)
                .ok_or_else(|| busy(session, "the exclusive lock"))?;
        }
        self.drain_readers(state, session, 0, deadline)
    }

    pub fn upgrade(&self, session: u64, timeout: Duration) -> DbResult<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        if state.readers == 0 || state.writer.is_some() {
            return Err(DbError::Internal(format!(
                "session {session} upgraded a shared lock it does not hold"
            )));
        }
        if state.pending.is_some() {
            return Err(busy(
                session,
                "the exclusive lock, another writer is waiting",
            ));
        }
        state.pending = Some(session);
        self.drain_readers(state, session, 1, deadline)?;
        Ok(())
    }

    pub fn release(&self, mode: LockMode) {
        let mut state = self.lock();
        match mode {
            LockMode::Shared => state.readers = state.readers.saturating_sub(1),
            LockMode::Exclusive => state.writer = None,
        }
        drop(state);
        self.cond.notify_all();
    }

    fn drain_readers(
        &self,
        mut state: MutexGuard<'_, LockState>,
        session: u64,
        own: usize,
        deadline: Instant,
    ) -> DbResult<()> {
        loop {
            if state.readers == own {
                state.readers = 0;
                state.pending = None;
                state.writer = Some(session);
                return Ok(());
            }
            match self.wait(state, deadline) {
                Some(next) => state = next,
                None => {
                    let mut state = self.lock();
                    if state.pending == Some(session) {
                        state.pending = None;
                    }
                    drop(state);
                    self.cond.notify_all();
                    return Err(busy(session, "the exclusive lock"));
                }
            }
        }
    }

    fn wait<'a>(
        &self,
        state: MutexGuard<'a, LockState>,
        deadline: Instant,
    ) -> Option<MutexGuard<'a, LockState>> {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        let (state, _) = self
            .cond
            .wait_timeout(state, left)
            .unwrap_or_else(PoisonError::into_inner);
        Some(state)
    }

    fn lock(&self) -> MutexGuard<'_, LockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn busy(session: u64, what: &str) -> DbError {
    DbError::Busy(format!("session {session} timed out waiting for {what}"))
}
