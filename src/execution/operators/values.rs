use crate::planner::physical_expr::PhysExpr;
use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::ExecResult;
use super::super::operator::{Operator, not_open};

pub struct ValuesExec {
    rows: Vec<Vec<PhysExpr>>,
    schema: Schema,
    cursor: Option<usize>,
}

impl ValuesExec {
    pub fn new(rows: Vec<Vec<PhysExpr>>, schema: Schema) -> Self {
        Self {
            rows,
            schema,
            cursor: None,
        }
    }
}

impl Operator for ValuesExec {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        self.cursor = Some(0);
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let cursor = self.cursor.as_mut().ok_or_else(|| not_open("Values"))?;
        let Some(exprs) = self.rows.get(*cursor) else {
            return Ok(None);
        };
        *cursor += 1;
        let row = exprs
            .iter()
            .map(|e| e.eval(&[]))
            .collect::<Result<Row, _>>()?;
        Ok(Some(row))
    }

    fn close(&mut self) -> ExecResult<()> {
        self.cursor = None;
        Ok(())
    }
}
