use crate::planner::physical::{KeyRange, ValueRange};
use crate::planner::schema::Schema;
use crate::planner::value::Row;

use super::super::error::{ExecError, ExecResult};
use super::super::operator::{Operator, not_open};
use super::super::source::{RowStream, TableSource};

pub enum ScanKind {
    Full,
    Index {
        key_column: usize,
        range: KeyRange,
    },
    Lookup {
        index: String,
        column: usize,
        ranges: Vec<ValueRange>,
    },
}

pub struct ScanExec<'a> {
    source: &'a dyn TableSource,
    table: String,
    kind: ScanKind,
    projection: Vec<usize>,
    schema: Schema,
    stream: Option<RowStream<'a>>,
}

impl<'a> ScanExec<'a> {
    pub fn new(
        source: &'a dyn TableSource,
        table: String,
        kind: ScanKind,
        projection: Vec<usize>,
        schema: Schema,
    ) -> Self {
        Self {
            source,
            table,
            kind,
            projection,
            schema,
            stream: None,
        }
    }

    fn project(&self, row: Row) -> ExecResult<Row> {
        self.projection
            .iter()
            .map(|&i| {
                row.get(i).cloned().ok_or_else(|| {
                    ExecError::Storage(format!(
                        "row of {} has {} columns, column {i} requested",
                        self.table,
                        row.len()
                    ))
                })
            })
            .collect()
    }
}

impl Operator for ScanExec<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self) -> ExecResult<()> {
        let stream = match &self.kind {
            ScanKind::Full => self.source.scan(&self.table)?,
            ScanKind::Index { key_column, range } => {
                self.source.index_scan(&self.table, *key_column, range)?
            }
            ScanKind::Lookup {
                index,
                column,
                ranges,
            } => self
                .source
                .index_lookup(&self.table, index, *column, ranges)?,
        };
        self.stream = Some(Box::new(stream.fuse()));
        Ok(())
    }

    fn next(&mut self) -> ExecResult<Option<Row>> {
        let stream = self.stream.as_mut().ok_or_else(|| not_open("Scan"))?;
        match stream.next() {
            None => Ok(None),
            Some(row) => self.project(row?).map(Some),
        }
    }

    fn close(&mut self) -> ExecResult<()> {
        self.stream = None;
        Ok(())
    }
}
