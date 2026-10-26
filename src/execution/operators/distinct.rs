use std::collections::HashSet;

use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator};

pub struct DistinctExec<'a> {
    input: BoxedOperator<'a>,
    seen: HashSet<Row>,
}

impl<'a> DistinctExec<'a> {
    pub fn new(input: BoxedOperator<'a>) -> Self {
        Self {
            input,
            seen: HashSet::new(),
        }
    }
}

impl Operator for DistinctExec<'_> {
    fn schema(&self) -> &Schema {
        self.input.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.seen.clear();
        self.input.open()
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        while let Some(row) = self.input.next()? {
            if !self.seen.contains(&row) {
                self.seen.insert(row.clone());
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn close(&mut self) -> ExecResult<()> {
        self.seen.clear();
        self.input.close()
    }
}
