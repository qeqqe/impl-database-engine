use crate::planner::physical_expr::PhysExpr;
use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::ExecResult;
use super::super::operator::{BoxedOperator, Operator};

pub struct ProjectExec<'a> {
    input: BoxedOperator<'a>,
    exprs: Vec<PhysExpr>,
    schema: Schema,
}

impl<'a> ProjectExec<'a> {
    pub fn new(input: BoxedOperator<'a>, exprs: Vec<PhysExpr>, schema: Schema) -> Self {
        Self {
            input,
            exprs,
            schema,
        }
    }
}

impl Operator for ProjectExec<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        self.input.open()
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let Some(row) = self.input.next()? else {
            return Ok(None);
        };
        let projected = self
            .exprs
            .iter()
            .map(|e| e.eval(&row))
            .collect::<Result<Row, _>>()?;
        Ok(Some(projected))
    }

    fn close(&mut self) -> ExecResult<()> {
        self.input.close()
    }
}
