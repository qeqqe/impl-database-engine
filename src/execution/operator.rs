use crate::{
    execution::error::{ExecError, ExecResult},
    planner::{
        schema::Schema,
        value::{Row, Value},
    },
};

pub trait Operator {
    fn schema(&self) -> &Schema;

    fn open(&mut self) -> ExecResult<()>;

    fn next(&mut self) -> ExecResult<Option<Row>>;

    fn close(&mut self) -> ExecResult<()>;
}

pub type BoxedOperator<'a> = Box<dyn Operator + 'a>;

pub fn null_row(width: usize) -> Row {
    vec![Value::Null; width]
}

pub fn concat_rows(left: &[Value], right: &[Value]) -> Row {
    let mut row = Vec::with_capacity(left.len() + right.len());
    row.extend_from_slice(left);
    row.extend_from_slice(right);
    row
}

pub fn not_open(operator: &str) -> ExecError {
    ExecError::Protocol(format!("{operator}::next called before open"))
}
