use crate::planner::physical_expr::PhysExpr;
use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator};

pub struct FilterExec<'a> {
    input: BoxedOperator<'a>,
    predicate: PhysExpr,
}

impl<'a> FilterExec<'a> {
    pub fn new(input: BoxedOperator<'a>, predicate: PhysExpr) -> Self {
        Self { input, predicate }
    }
}

impl Operator for FilterExec<'_> {
    fn schema(&self) -> &Schema {
        self.input.schema()
    }

    fn open(&mut self) -> ExecResult<()> {
        self.input.open()
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        while let Some(row) = self.input.next()? {
            if self.predicate.eval_predicate(&row)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn close(&mut self) -> ExecResult<()> {
        self.input.close()
    }
}
