use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator};

pub struct LimitExec<'a> {
    input: BoxedOperator<'a>,
    limit: Option<usize>,
    offset: usize,
    skipped: usize,
    emitted: usize,
}

impl<'a> LimitExec<'a> {
    pub fn new(input: BoxedOperator<'a>, limit: Option<usize>, offset: usize) -> Self {
        Self {
            input,
            limit,
            offset,
            skipped: 0,
            emitted: 0,
        }
    }
}

impl Operator for LimitExec<'_> {
    fn schema(&self) -> &Schema {
        self.input.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.skipped = 0;
        self.emitted = 0;
        self.input.open()
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        if self.limit.is_some_and(|l| self.emitted >= l) {
            return Ok(None);
        }
        while self.skipped < self.offset {
            if self.input.next()?.is_none() {
                return Ok(None);
            }
            self.skipped += 1;
        }
        let row = self.input.next()?;
        if row.is_some() {
            self.emitted += 1;
        }
        Ok(row)
    }

    fn close(&mut self) -> ExecResult<()> {
        self.input.close()
    }
}
