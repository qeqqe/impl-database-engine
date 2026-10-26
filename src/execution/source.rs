use std::collections::HashMap;

use crate::{
    execution::error::{ExecError, ExecResult},
    planner::{
        physical::{KeyRange, ValueRange},
        value::{Row, Value},
    },
};

pub type RowStream<'a> = Box<dyn Iterator<Item = ExecResult<Row>> + 'a>;

pub trait TableSource {
    fn scan(&self, table: &str) -> ExecResult<RowStream<'_>>;

    fn index_scan(
        &self,
        table: &str,
        key_column: usize,
        range: &KeyRange,
    ) -> ExecResult<RowStream<'_>> {
        let range = range.clone();
        let rows = self.scan(table)?;
        Ok(Box::new(rows.filter(move |row| match row {
            Ok(row) => matches!(row.get(key_column), Some(Value::Integer(k)) if range.contains(*k)),
            Err(_) => true,
        })))
    }

    fn index_lookup(
        &self,
        table: &str,
        _index: &str,
        column: usize,
        ranges: &[ValueRange],
    ) -> ExecResult<RowStream<'_>> {
        let ranges = ranges.to_vec();
        let rows = self.scan(table)?;
        Ok(Box::new(rows.filter(move |row| {
            match row {
                Ok(row) => row
                    .get(column)
                    .is_some_and(|v| ranges.iter().any(|r| r.contains(v))),
                Err(_) => true,
            }
        })))
    }
}

#[derive(Debug, Default, Clone)]
pub struct MemoryTableSource {
    tables: HashMap<String, Vec<Row>>,
}

impl MemoryTableSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_table(&mut self, table: &str) {
        self.tables.entry(table.to_lowercase()).or_default();
    }

    pub fn insert(&mut self, table: &str, rows: impl IntoIterator<Item = Row>) {
        self.tables
            .entry(table.to_lowercase())
            .or_default()
            .extend(rows);
    }

    pub fn rows(&self, table: &str) -> Option<&[Row]> {
        self.tables.get(&table.to_lowercase()).map(Vec::as_slice)
    }
}

impl TableSource for MemoryTableSource {
    fn scan(&self, table: &str) -> ExecResult<RowStream<'_>> {
        let rows = self
            .tables
            .get(&table.to_lowercase())
            .ok_or_else(|| ExecError::TableNotFound(table.to_string()))?;
        Ok(Box::new(rows.iter().cloned().map(Ok)))
    }
}
